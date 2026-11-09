#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

use super::{focus_command, monitors_dto, parse, peer, response, Action};
use crate::config::GlazewmCompatConfig;
use crate::events::DaemonEvent;
use futures_util::{SinkExt, StreamExt};
use leopardwm_ipc::{EventKind, IpcCommand, IpcEvent, IpcResponse, WorkspaceStateSnapshot};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    protocol::{Message, WebSocketConfig},
};
use windows::Win32::Networking::WinSock::{setsockopt, SOCKET, SOL_SOCKET, SO_EXCLUSIVEADDRUSE};

const LIMIT: usize = 4096;
type Snapshot = Arc<WorkspaceStateSnapshot>;

#[derive(Default)]
pub(crate) struct Server {
    config: Option<GlazewmCompatConfig>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    pub(crate) async fn configure(
        &mut self,
        config: &GlazewmCompatConfig,
        events: mpsc::Sender<DaemonEvent>,
    ) {
        if self.config.as_ref() == Some(config)
            && !self.task.as_ref().is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        self.stop().await;
        self.config = Some(config.clone());
        if !config.enabled {
            return;
        }
        match bind(config.port) {
            Ok(listener) => self.task = Some(tokio::spawn(run(listener, events))),
            Err(error) => tracing::warn!("GlazeWM compatibility port {} could not be bound (GlazeWM or another program may be running): {}", config.port, error),
        }
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.config = None;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn bind(port: u16) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let result = unsafe {
        setsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            Some(&1i32.to_ne_bytes()),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())?;
    socket.listen(8)?;
    TcpListener::from_std(socket.into())
}

async fn run(listener: TcpListener, events: mpsc::Sender<DaemonEvent>) {
    let mut clients = JoinSet::new();
    let mut feed = JoinSet::new();
    let mut channels = None;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote) = match accepted {
                    Ok(connection) => connection,
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::Interrupted) => {
                        tracing::debug!(%error, "GlazeWM compatibility connection accept failed");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "GlazeWM compatibility listener stopped");
                        break;
                    }
                };
                if clients.len() >= 8 { continue; }
                let (SocketAddr::V4(peer_addr), Ok(SocketAddr::V4(local_addr))) = (remote, stream.local_addr()) else { continue; };
                if !peer::same_user_peer(peer_addr, local_addr) { continue; }
                if channels.is_none() {
                    let (snapshot_tx, snapshot_rx) = watch::channel(None);
                    let (change_tx, _) = broadcast::channel(256);
                    feed.spawn(snapshot_feed(events.clone(), snapshot_tx, change_tx.clone()));
                    channels = Some((snapshot_rx, change_tx));
                }
                if let Some((snapshot, changes)) = &channels {
                    clients.spawn(client(stream, events.clone(), snapshot.clone(), changes.subscribe()));
                }
            }
            _ = feed.join_next(), if !feed.is_empty() => {
                clients.abort_all();
                while clients.join_next().await.is_some() {}
                channels = None;
            }
            _ = clients.join_next(), if !clients.is_empty() => {
                if clients.is_empty() {
                    feed.abort_all();
                    while feed.join_next().await.is_some() {}
                    channels = None;
                }
            }
        }
    }
}

fn update_snapshot(
    partial: &mut Option<WorkspaceStateSnapshot>,
    event: IpcEvent,
) -> Result<Option<WorkspaceStateSnapshot>, ()> {
    match event {
        IpcEvent::WorkspaceSnapshotBegin {
            session_id,
            revision,
            focused_monitor_device_name,
            ..
        } if partial.is_none() => {
            *partial = Some(WorkspaceStateSnapshot {
                session_id,
                revision,
                focused_monitor_device_name,
                records: Vec::new(),
            });
        }
        IpcEvent::WorkspaceSnapshotChunk { revision, records } => {
            let Some(snapshot) = partial.as_mut().filter(|s| s.revision == revision) else {
                return Err(());
            };
            snapshot.records.extend(records);
        }
        IpcEvent::WorkspaceSnapshotEnd { revision } => {
            if partial.as_ref().is_none_or(|s| s.revision != revision) {
                return Err(());
            }
            return Ok(partial.take());
        }
        _ => return Err(()),
    }
    Ok(None)
}

async fn snapshot_feed(
    events: mpsc::Sender<DaemonEvent>,
    snapshots: watch::Sender<Option<Snapshot>>,
    changes: broadcast::Sender<Snapshot>,
) {
    let mut previous: Option<(String, u64)> = None;
    loop {
        let (tx, rx) = oneshot::channel();
        if events
            .send(DaemonEvent::IpcSubscribe {
                events: [EventKind::WorkspaceState].into_iter().collect(),
                responder: tx,
            })
            .await
            .is_err()
        {
            return;
        }
        let Ok(Ok(startup)) = tokio::time::timeout(Duration::from_secs(10), rx).await else {
            return;
        };
        if !matches!(startup.ack, IpcResponse::Subscribed { .. }) {
            return;
        }
        let mut partial = None;
        let mut receiver = startup.receiver;
        for event in startup.snapshot {
            match update_snapshot(&mut partial, event) {
                Ok(Some(snapshot)) => publish(snapshot, &mut previous, &snapshots, &changes),
                Ok(None) => {}
                Err(()) => {
                    snapshots.send_replace(None);
                    return;
                }
            }
        }
        loop {
            match receiver.recv().await {
                Ok(event) if event.kind() != EventKind::WorkspaceState => continue,
                Ok(event) => match update_snapshot(&mut partial, event) {
                    Ok(Some(snapshot)) => publish(snapshot, &mut previous, &snapshots, &changes),
                    Ok(None) => {}
                    Err(()) => break,
                },
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => {
                    snapshots.send_replace(None);
                    return;
                }
            }
        }
        snapshots.send_replace(None);
    }
}

fn publish(
    snapshot: WorkspaceStateSnapshot,
    previous: &mut Option<(String, u64)>,
    snapshots: &watch::Sender<Option<Snapshot>>,
    changes: &broadcast::Sender<Snapshot>,
) {
    let key = (snapshot.session_id.clone(), snapshot.revision);
    let changed = previous.as_ref().is_some_and(|old| *old != key);
    *previous = Some(key);
    let snapshot = Arc::new(snapshot);
    snapshots.send_replace(Some(snapshot.clone()));
    if changed {
        let _ = changes.send(snapshot);
    }
}

fn positions() -> Result<HashMap<i64, (i32, i32)>, &'static str> {
    leopardwm_platform_win32::enumerate_monitors()
        .map(|monitors| {
            monitors
                .into_iter()
                .map(|monitor| (monitor.id as i64, (monitor.rect.x, monitor.rect.y)))
                .collect()
        })
        .map_err(|_| "monitor positions unavailable")
}

async fn get_snapshot(
    snapshots: &mut watch::Receiver<Option<Snapshot>>,
) -> Result<Snapshot, &'static str> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(snapshot) = snapshots.borrow_and_update().clone() {
                return Ok(snapshot);
            }
            if snapshots.changed().await.is_err() {
                return Err("workspace state unavailable");
            }
        }
    })
    .await
    .unwrap_or(Err("workspace state unavailable"))
}

async fn command(events: &mpsc::Sender<DaemonEvent>, cmd: IpcCommand) -> Result<Value, String> {
    let (tx, rx) = oneshot::channel();
    events
        .send(DaemonEvent::IpcCommand { cmd, responder: tx })
        .await
        .map_err(|_| "daemon is shutting down".to_string())?;
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(IpcResponse::Ok)) => {
            Ok(json!({"subjectContainerId":uuid::Uuid::new_v4().to_string()}))
        }
        Ok(Ok(IpcResponse::Error { message })) | Ok(Ok(IpcResponse::ApplyPending { message })) => {
            Err(message)
        }
        _ => Err("daemon response unavailable".into()),
    }
}

async fn handle_message(
    message: &str,
    events: &mpsc::Sender<DaemonEvent>,
    snapshots: &mut watch::Receiver<Option<Snapshot>>,
    subscription: &mut Option<(String, bool)>,
    changes: &mut broadcast::Receiver<Snapshot>,
) -> Value {
    let result: Result<Value, String> = async {
        match parse(message).map_err(str::to_string)? {
            Action::QueryMonitors => {
                let snapshot = get_snapshot(snapshots).await.map_err(str::to_string)?;
                let positions = positions().map_err(str::to_string)?;
                Ok(json!({"monitors":monitors_dto(&snapshot, &positions).map_err(str::to_string)?}))
            }
            Action::Subscribe(kinds) => {
                let id = uuid::Uuid::new_v4().to_string();
                *subscription = Some((
                    id.clone(),
                    kinds
                        .iter()
                        .any(|kind| *kind == "all" || *kind == "workspace_updated"),
                ));
                *changes = changes.resubscribe();
                Ok(json!({"subscriptionId":id}))
            }
            Action::Unsubscribe(id) => {
                if subscription
                    .as_ref()
                    .is_some_and(|(current, _)| current == id)
                {
                    *subscription = None;
                }
                Ok(Value::Null)
            }
            Action::FocusWorkspace(name) => {
                let snapshot = get_snapshot(snapshots).await.map_err(str::to_string)?;
                let positions = positions().map_err(str::to_string)?;
                command(
                    events,
                    focus_command(name, &snapshot, &positions).map_err(str::to_string)?,
                )
                .await
            }
            Action::Next => command(events, IpcCommand::WorkspaceNext).await,
            Action::Previous => command(events, IpcCommand::WorkspacePrev).await,
        }
    }
    .await;
    match result {
        Ok(data) => response(message, Ok(data)),
        Err(error) => response(message, Err(&error)),
    }
}

fn event_frame(id: &str, snapshot: &WorkspaceStateSnapshot) -> Value {
    let workspace = positions()
        .and_then(|positions| monitors_dto(snapshot, &positions))
        .ok()
        .and_then(|monitors| {
            monitors
                .into_iter()
                .flat_map(|monitor| monitor["children"].as_array().cloned().unwrap_or_default())
                .find(|workspace| workspace["hasFocus"] == true)
        });
    json!({"messageType":"event_subscription", "subscriptionId":id, "success":true, "error":null, "data":{"eventType":"workspace_updated", "updatedWorkspace":workspace}})
}

// Tungstenite's handshake callback requires its unboxed ErrorResponse type.
#[allow(clippy::result_large_err)]
async fn client(
    stream: TcpStream,
    events: mpsc::Sender<DaemonEvent>,
    mut snapshots: watch::Receiver<Option<Snapshot>>,
    mut changes: broadcast::Receiver<Snapshot>,
) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(LIMIT))
        .max_frame_size(Some(LIMIT));
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if request.headers().contains_key("origin") {
            return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Some("Origin not allowed".into()))
                .unwrap());
        }
        Ok(response)
    };
    let Ok(Ok(mut socket)) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)),
    )
    .await
    else {
        return;
    };
    let mut subscription = None;
    loop {
        tokio::select! {
            message = socket.next() => match message {
                Some(Ok(Message::Text(message))) => {
                    let reply = handle_message(&message, &events, &mut snapshots, &mut subscription, &mut changes).await;
                    if socket.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            change = changes.recv() => match change {
                Ok(snapshot) => if let Some((id, true)) = &subscription {
                    let frame = event_frame(id, &snapshot);
                    if socket.send(Message::Text(frame.to_string().into())).await.is_err() { break; }
                },
                Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
