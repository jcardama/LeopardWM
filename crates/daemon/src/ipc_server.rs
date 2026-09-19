//! Named-pipe IPC server and client handling.

use super::DaemonEvent;
use crate::events::SubscribeStartup;
use anyhow::Result;
use leopardwm_ipc::{
    preferred_pipe_name, EventKind, IpcCommand, IpcEvent, IpcResponse, MAX_IPC_MESSAGE_SIZE,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, warn};

/// IPC read timeout - clients must send within this period.
pub(crate) const IPC_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// IPC responder timeout - daemon must answer within this period.
pub(crate) const IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound stalled clients so disconnected consumers cannot retain stream tasks.
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Heartbeat interval for stream-mode subscribers. Subscribers receive a
/// `IpcEvent::Heartbeat` after this much silence so they can detect a
/// dead daemon pipe by missing keepalives.
pub(crate) const STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Poll interval for cooperative timed thread joins.
const JOIN_WITH_TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) fn response_for_ipc_wait_failure(cmd: &IpcCommand, timed_out: bool) -> IpcResponse {
    if matches!(cmd, IpcCommand::Stop | IpcCommand::PanicRevert) {
        // Stop/panic_revert semantics are "shutdown initiated"; don't report as a hard failure
        // if the responder channel closes or cleanup outlives the client timeout.
        IpcResponse::Ok
    } else if timed_out {
        IpcResponse::error("Timed out waiting for daemon response")
    } else {
        IpcResponse::error("Failed to get response from daemon")
    }
}

/// Run the IPC server, accepting connections and dispatching commands.
pub(crate) async fn run_ipc_server(event_tx: mpsc::Sender<DaemonEvent>) {
    let mut is_first_instance = true;
    let pipe_name = preferred_pipe_name();
    // Held as a usize and leaked for the daemon's lifetime so the async server
    // future stays Send without an `unsafe impl Send`.
    let pipe_security_ptr: Option<usize> =
        match leopardwm_platform_win32::ipc_security::PipeSecurityAttributes::new() {
            Some(sec) => {
                let ptr = sec.as_ptr() as usize;
                std::mem::forget(sec);
                Some(ptr)
            }
            None => {
                warn!("Could not build IPC pipe security attributes; using defaults (a non-elevated client may not reach an elevated daemon)");
                None
            }
        };
    // Bound concurrent IPC handlers to avoid local task-exhaustion DoS.
    // Stream-mode (Subscribe) connections drop their permit on entry so
    // long-lived subscribers don't starve normal command handlers.
    let connection_limit = Arc::new(Semaphore::new(32));

    loop {
        let permit = match connection_limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!("IPC connection limiter closed while accepting client");
                return;
            }
        };

        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(is_first_instance)
            .pipe_mode(PipeMode::Byte);
        let create_result = match pipe_security_ptr {
            Some(ptr) => unsafe {
                opts.create_with_security_attributes_raw(&pipe_name, ptr as *mut std::ffi::c_void)
            },
            None => opts.create(&pipe_name),
        };
        let server = match create_result {
            Ok(s) => {
                is_first_instance = false; // Subsequent instances don't need this flag
                s
            }
            Err(e) => {
                error!("Failed to create named pipe server: {}", e);
                if is_first_instance {
                    // If we can't create the first instance, maybe another daemon is running
                    error!("Is another leopardwm daemon already running?");
                }
                drop(permit);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        debug!("Waiting for client connection on {}", pipe_name);

        if let Err(e) = server.connect().await {
            error!("Failed to accept client connection: {}", e);
            drop(permit);
            continue;
        }

        debug!("Client connected");

        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(server, event_tx, permit).await {
                warn!("Client handler error: {}", e);
            }
        });
    }
}

/// Serialize an `IpcResponse` to a newline-terminated frame and write it.
async fn write_response_frame<W>(writer: &mut W, response: &IpcResponse) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut response_json = match serde_json::to_string(response) {
        Ok(json) => json + "\n",
        Err(e) => {
            warn!("Failed to serialize IPC response: {}", e);
            "{\"status\":\"error\",\"message\":\"Internal serialization error\"}\n".to_string()
        }
    };

    if response_json.len() > MAX_IPC_MESSAGE_SIZE {
        warn!(
            "IPC response exceeded {} bytes; returning bounded error response instead",
            MAX_IPC_MESSAGE_SIZE
        );
        response_json = serde_json::to_string(&IpcResponse::error(
            "IPC response exceeded maximum size; narrow query scope and retry",
        ))
        .unwrap_or_else(|_| {
            "{\"status\":\"error\",\"message\":\"Internal serialization error\"}".to_string()
        });
        response_json.push('\n');
    }

    tokio::time::timeout(
        IPC_WRITE_TIMEOUT,
        writer.write_all(response_json.as_bytes()),
    )
    .await??;
    Ok(())
}

/// Serialize an `IpcEvent` to a newline-terminated frame and write it.
async fn write_event_frame<W>(writer: &mut W, event: &IpcEvent) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut json = serde_json::to_string(event)? + "\n";
    if json.len() > MAX_IPC_MESSAGE_SIZE {
        if event.kind() == EventKind::WorkspaceState {
            let error = IpcEvent::WorkspaceSnapshotError {
                message: "Workspace snapshot frame exceeded maximum size".into(),
            };
            let frame = serde_json::to_string(&error)? + "\n";
            tokio::time::timeout(IPC_WRITE_TIMEOUT, writer.write_all(frame.as_bytes())).await??;
            anyhow::bail!("oversize workspace snapshot frame");
        }
        // Oversize events shouldn't happen given the small variant
        // payloads, but if a future LayoutChanged ever blows the cap,
        // surface a Lagged-style hint instead of a corrupt frame.
        json = serde_json::to_string(&IpcEvent::Lagged { skipped: 0 })? + "\n";
    }
    tokio::time::timeout(IPC_WRITE_TIMEOUT, writer.write_all(json.as_bytes())).await??;
    Ok(())
}

/// Handle a single client connection.
async fn handle_client(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    event_tx: mpsc::Sender<DaemonEvent>,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(pipe);
    let limited_reader = reader.take(MAX_IPC_MESSAGE_SIZE as u64);
    let mut reader = BufReader::new(limited_reader);
    let mut line = String::new();

    // Read command (single line of JSON) with timeout and size bound
    let read_result = tokio::time::timeout(IPC_READ_TIMEOUT, reader.read_line(&mut line)).await;
    let bytes_read = match read_result {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            // Timeout: client did not send in time, silently close
            return Ok(());
        }
    };
    if bytes_read == 0 {
        return Ok(()); // Client disconnected
    }

    if !line.ends_with('\n') {
        let msg = if bytes_read >= MAX_IPC_MESSAGE_SIZE {
            "Command too large or missing newline terminator"
        } else {
            "IPC command must be newline-terminated"
        };
        write_response_frame(&mut writer, &IpcResponse::error(msg)).await?;
        return Ok(());
    }

    let line = line.trim_end_matches(['\r', '\n']);
    debug!("Received command: {}", line);

    // Parse the command
    let cmd: IpcCommand = match serde_json::from_str(line) {
        Ok(cmd) => cmd,
        Err(e) => {
            let response = IpcResponse::error(format!("Invalid command: {}", e));
            write_response_frame(&mut writer, &response).await?;
            return Ok(());
        }
    };

    // Subscribe is routed through a dedicated DaemonEvent variant whose
    // responder carries (ack, snapshot, broadcast::Receiver). The main
    // daemon loop processes it under the AppState mutex so the receiver
    // creation + snapshot read happen in one atomic critical section —
    // no event between handoff and receiver-creation can be lost.
    if let IpcCommand::Subscribe { events } = cmd {
        return handle_subscribe(writer, event_tx, events, permit, false).await;
    }

    if matches!(cmd, IpcCommand::QueryWorkspaceState) {
        return handle_subscribe(
            writer,
            event_tx,
            [EventKind::WorkspaceState].into_iter().collect(),
            permit,
            true,
        )
        .await;
    }

    // Everything else: existing oneshot path through the daemon main loop.
    handle_command_oneshot(writer, event_tx, cmd, permit).await
}

/// Existing single-command request/response path.
async fn handle_command_oneshot<W>(
    mut writer: W,
    event_tx: mpsc::Sender<DaemonEvent>,
    cmd: IpcCommand,
    permit: OwnedSemaphorePermit,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let _permit = permit; // released when this task returns

    let (resp_tx, resp_rx) = oneshot::channel();
    let response_cmd = cmd.clone();

    if event_tx
        .send(DaemonEvent::IpcCommand {
            cmd,
            responder: resp_tx,
        })
        .await
        .is_err()
    {
        let response = IpcResponse::error("Daemon is shutting down");
        write_response_frame(&mut writer, &response).await?;
        return Ok(());
    }

    let response = match tokio::time::timeout(IPC_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => response_for_ipc_wait_failure(&response_cmd, false),
        Err(_) => response_for_ipc_wait_failure(&response_cmd, true),
    };

    write_response_frame(&mut writer, &response).await?;
    Ok(())
}

/// Stream-mode entry: route Subscribe through a dedicated DaemonEvent
/// variant so the daemon main loop can subscribe + snapshot atomically
/// under the AppState mutex, then drive an event loop that writes
/// `IpcEvent` frames until the pipe closes or the broadcaster is dropped.
async fn handle_subscribe<W>(
    mut writer: W,
    event_tx: mpsc::Sender<DaemonEvent>,
    requested_raw: BTreeSet<EventKind>,
    permit: OwnedSemaphorePermit,
    one_shot: bool,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // Preserve the legacy default for clients whose event enum is closed.
    // New snapshot events require an explicit workspace_state filter.
    let requested = if requested_raw.is_empty() {
        EventKind::legacy_default()
    } else {
        requested_raw
    };

    // Send Subscribe to the daemon main loop, which builds the bundle
    // (ack + snapshot + broadcast::Receiver) under the AppState mutex.
    let (resp_tx, resp_rx) = oneshot::channel();
    if event_tx
        .send(DaemonEvent::IpcSubscribe {
            events: requested.clone(),
            responder: resp_tx,
        })
        .await
        .is_err()
    {
        let response = IpcResponse::error("Daemon is shutting down");
        let _ = write_response_frame(&mut writer, &response).await;
        return Ok(());
    }

    let startup = match tokio::time::timeout(IPC_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            let _ = write_response_frame(
                &mut writer,
                &IpcResponse::error("Failed to get subscribe response from daemon"),
            )
            .await;
            return Ok(());
        }
        Err(_) => {
            let _ = write_response_frame(
                &mut writer,
                &IpcResponse::error("Timed out waiting for subscribe response"),
            )
            .await;
            return Ok(());
        }
    };

    let SubscribeStartup {
        ack,
        snapshot,
        mut receiver,
    } = startup;

    // Stream-mode connections release the connection-limiter permit
    // before entering the long-lived loop. Otherwise 32 long-lived
    // subscribers would starve all other IPC commands.
    let _query_permit = if one_shot {
        Some(permit)
    } else {
        drop(permit);
        None
    };

    let ack = if one_shot {
        IpcResponse::WorkspaceStateReady {
            protocol_version: leopardwm_ipc::IPC_PROTOCOL_VERSION,
        }
    } else {
        ack
    };

    // Write ack
    if write_response_frame(&mut writer, &ack).await.is_err() {
        return Ok(());
    }

    // Write snapshot frames
    for ev in &snapshot {
        if write_event_frame(&mut writer, ev).await.is_err()
            || matches!(ev, IpcEvent::WorkspaceSnapshotError { .. })
        {
            return Ok(());
        }
    }

    if one_shot {
        return Ok(());
    }

    let mut in_snapshot = false;

    // Stream loop: events + heartbeat. The heartbeat's uptime field is
    // per-subscriber connection time (since we entered stream mode),
    // NOT the daemon process uptime — that's intentional, callers can
    // detect "did we just reconnect" vs "are we still on the original
    // connection". Computing daemon-wide uptime would require an extra
    // mutex acquisition per heartbeat for negligible signal.
    let stream_started = std::time::Instant::now();
    let mut heartbeat = tokio::time::interval(STREAM_HEARTBEAT_INTERVAL);
    // Skip the immediate first tick so we don't send a heartbeat right
    // after the snapshot.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            recv = receiver.recv() => match recv {
                Ok(ev) => {
                    if !requested.contains(&ev.kind()) {
                        continue;
                    }
                    match &ev {
                        IpcEvent::WorkspaceSnapshotBegin { .. } => in_snapshot = true,
                        IpcEvent::WorkspaceSnapshotEnd { .. } => in_snapshot = false,
                        _ => {}
                    }
                    if write_event_frame(&mut writer, &ev).await.is_err()
                        || matches!(ev, IpcEvent::WorkspaceSnapshotError { .. }) {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let lagged = IpcEvent::Lagged { skipped };
                    if write_event_frame(&mut writer, &lagged).await.is_err()
                        || requested.contains(&EventKind::WorkspaceState) {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = heartbeat.tick(), if !in_snapshot => {
                let uptime = stream_started.elapsed().as_secs();
                let hb = IpcEvent::Heartbeat { uptime_seconds: uptime };
                if write_event_frame(&mut writer, &hb).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Spawn a named forwarding thread that receives events from a std::sync::mpsc channel
/// and forwards them to a tokio mpsc sender. Returns the JoinHandle for graceful shutdown.
pub(crate) fn spawn_forwarding_thread<T: Send + 'static>(
    name: &str,
    receiver: std::sync::mpsc::Receiver<T>,
    sender: mpsc::Sender<DaemonEvent>,
    map_fn: impl Fn(T) -> DaemonEvent + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    let thread_name = name.to_string();
    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                if sender.blocking_send(map_fn(event)).is_err() {
                    break; // Channel closed, daemon shutting down
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn {} thread: {}", thread_name, e))
}

/// Join a thread with a timeout. Returns true if the thread joined within the deadline,
/// false if it timed out. The join handle remains available on timeout so callers can retry
/// later without losing ownership.
pub(crate) fn join_with_timeout(
    handle: &mut Option<std::thread::JoinHandle<()>>,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let Some(join_handle) = handle.as_ref() else {
            return true;
        };
        if join_handle.is_finished() {
            let join_handle = handle
                .take()
                .expect("join handle must exist when finishing timed join");
            let _ = join_handle.join();
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(JOIN_WITH_TIMEOUT_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod workspace_stream_tests {
    use super::*;
    use leopardwm_ipc::WorkspaceStateSnapshot;

    async fn start_stream(
        requested: BTreeSet<EventKind>,
        one_shot: bool,
        backlog: usize,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<()>>,
        broadcast::Sender<IpcEvent>,
    ) {
        let (server, client) = tokio::io::duplex(512 * 1024);
        let (tx, mut rx) = mpsc::channel(1);
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let task = tokio::spawn(handle_subscribe(server, tx, requested, permit, one_shot));
        let DaemonEvent::IpcSubscribe { events, responder } = rx.recv().await.unwrap() else {
            panic!("wrong dispatch")
        };
        let broadcaster = broadcast::channel(256).0;
        let receiver = broadcaster.subscribe();
        for _ in 0..backlog {
            broadcaster
                .send(IpcEvent::Heartbeat { uptime_seconds: 1 })
                .unwrap();
        }
        let snapshot = WorkspaceStateSnapshot {
            session_id: "test".into(),
            revision: 42,
            focused_monitor_device_name: None,
            records: Vec::new(),
        }
        .events()
        .unwrap();
        responder
            .send(SubscribeStartup {
                ack: IpcResponse::Subscribed {
                    events: events.clone(),
                },
                snapshot: if events.contains(&EventKind::WorkspaceState) {
                    snapshot
                } else {
                    Vec::new()
                },
                receiver,
            })
            .unwrap_or_else(|_| panic!("startup dropped"));
        (client, task, broadcaster)
    }

    async fn read_frames(client: tokio::io::DuplexStream) -> Vec<serde_json::Value> {
        let mut text = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(client).read_to_string(&mut text),
        )
        .await
        .unwrap()
        .unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn one_shot_ends_after_complete_snapshot() {
        let (client, task, _sender) =
            start_stream([EventKind::WorkspaceState].into_iter().collect(), true, 0).await;
        let frames = read_frames(client).await;
        task.await.unwrap().unwrap();
        assert_eq!(frames[0]["status"], "workspace_state_ready");
        assert_eq!(frames[1]["type"], "workspace_snapshot_begin");
        assert_eq!(frames.last().unwrap()["type"], "workspace_snapshot_end");
    }

    #[tokio::test]
    async fn lag_closes_workspace_stream_after_initial_snapshot() {
        let (client, task, _sender) = start_stream(
            [EventKind::WorkspaceState].into_iter().collect(),
            false,
            300,
        )
        .await;
        let frames = read_frames(client).await;
        task.await.unwrap().unwrap();
        assert_eq!(frames[0]["events"], serde_json::json!(["workspace_state"]));
        assert_eq!(frames[frames.len() - 2]["type"], "workspace_snapshot_end");
        assert_eq!(frames.last().unwrap()["type"], "lagged");
    }

    #[tokio::test]
    async fn empty_filter_preserves_legacy_defaults() {
        let (client, task, sender) = start_stream(BTreeSet::new(), false, 0).await;
        sender
            .send(IpcEvent::WorkspaceSnapshotEnd { revision: 43 })
            .unwrap();
        sender.send(IpcEvent::ConfigReloaded).unwrap();
        drop(sender);
        let frames = read_frames(client).await;
        task.await.unwrap().unwrap();
        let kinds: BTreeSet<EventKind> =
            serde_json::from_value(frames[0]["events"].clone()).unwrap();
        assert_eq!(kinds, EventKind::legacy_default());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["type"], "config_reloaded");
    }

    #[tokio::test]
    async fn mixed_filters_keep_legacy_and_workspace_events() {
        let (client, task, sender) = start_stream(
            [EventKind::WorkspaceState, EventKind::Config]
                .into_iter()
                .collect(),
            false,
            0,
        )
        .await;
        sender.send(IpcEvent::ConfigReloaded).unwrap();
        drop(sender);
        let frames = read_frames(client).await;
        task.await.unwrap().unwrap();
        assert_eq!(frames[1]["type"], "workspace_snapshot_begin");
        assert_eq!(frames.last().unwrap()["type"], "config_reloaded");
    }

    #[tokio::test]
    async fn disconnected_writer_returns_promptly() {
        let (mut server, client) = tokio::io::duplex(16);
        drop(client);
        assert!(
            write_event_frame(&mut server, &IpcEvent::Heartbeat { uptime_seconds: 0 })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stalled_writer_is_bounded() {
        let (mut server, _client) = tokio::io::duplex(1);
        let result = tokio::time::timeout(
            IPC_WRITE_TIMEOUT + Duration::from_secs(2),
            write_event_frame(&mut server, &IpcEvent::Heartbeat { uptime_seconds: 0 }),
        )
        .await;
        assert!(result
            .expect("transport must enforce its own write deadline")
            .is_err());
    }

    #[tokio::test]
    async fn lag_during_transaction_never_writes_a_successful_end() {
        let (client, task, sender) =
            start_stream([EventKind::WorkspaceState].into_iter().collect(), false, 0).await;
        let mut reader = BufReader::new(client);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"]
                == "workspace_snapshot_end"
            {
                break;
            }
        }
        sender
            .send(IpcEvent::WorkspaceSnapshotBegin {
                protocol_version: 4,
                session_id: "test".into(),
                revision: 43,
                focused_monitor_device_name: None,
            })
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"],
            "workspace_snapshot_begin"
        );
        // No await in this burst: the receiver deterministically overruns.
        for _ in 0..300 {
            sender
                .send(IpcEvent::WorkspaceSnapshotChunk {
                    revision: 43,
                    records: Vec::new(),
                })
                .unwrap();
        }
        sender
            .send(IpcEvent::WorkspaceSnapshotEnd { revision: 43 })
            .unwrap();
        let mut tail = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut tail))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(tail.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(tail.trim()).unwrap()["type"],
            "lagged"
        );
    }

    #[tokio::test]
    async fn snapshot_error_closes_stream() {
        let (client, task, sender) =
            start_stream([EventKind::WorkspaceState].into_iter().collect(), false, 0).await;
        sender
            .send(IpcEvent::WorkspaceSnapshotError {
                message: "unencodable record".into(),
            })
            .unwrap();
        let frames = read_frames(client).await;
        task.await.unwrap().unwrap();
        assert_eq!(frames.last().unwrap()["type"], "workspace_snapshot_error");
    }

    #[tokio::test]
    async fn initial_snapshot_can_exceed_broadcast_capacity() {
        let (server, client) = tokio::io::duplex(4096);
        let (tx, mut rx) = mpsc::channel(1);
        let limiter = Arc::new(Semaphore::new(1));
        let permit = limiter.clone().acquire_owned().await.unwrap();
        let task = tokio::spawn(handle_subscribe(
            server,
            tx,
            [EventKind::WorkspaceState].into_iter().collect(),
            permit,
            true,
        ));
        let DaemonEvent::IpcSubscribe { events, responder } = rx.recv().await.unwrap() else {
            panic!("wrong dispatch")
        };
        let broadcaster = broadcast::channel(256).0;
        let receiver = broadcaster.subscribe();
        let mut snapshot = vec![IpcEvent::WorkspaceSnapshotBegin {
            protocol_version: 4,
            session_id: "test".into(),
            revision: 7,
            focused_monitor_device_name: None,
        }];
        snapshot.extend((0..300).map(|_| IpcEvent::WorkspaceSnapshotChunk {
            revision: 7,
            records: Vec::new(),
        }));
        snapshot.push(IpcEvent::WorkspaceSnapshotEnd { revision: 7 });
        responder
            .send(SubscribeStartup {
                ack: IpcResponse::Subscribed { events },
                snapshot,
                receiver,
            })
            .unwrap_or_else(|_| panic!("startup dropped"));
        let mut reader = BufReader::new(client);
        let mut ack = String::new();
        reader.read_line(&mut ack).await.unwrap();
        assert_eq!(
            limiter.available_permits(),
            0,
            "finite queries retain their permit while writing"
        );
        let mut rest = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut rest))
            .await
            .unwrap()
            .unwrap();
        let text = ack + &rest;
        let frames: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        task.await.unwrap().unwrap();
        assert_eq!(limiter.available_permits(), 1);
        assert_eq!(frames.len(), 303);
        assert_eq!(frames.last().unwrap()["type"], "workspace_snapshot_end");
        assert!(!frames.iter().any(|f| f["type"] == "lagged"));
    }
}
