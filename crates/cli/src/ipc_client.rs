//! Named-pipe IPC client: connect/retry, send/receive, and error classification.

use anyhow::{Context, Result};
use leopardwm_ipc::{pipe_name_candidates, IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{sleep, timeout};

/// Timeout budget for establishing an IPC connection to the daemon.
pub(crate) const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Extended connect timeout budget for recovery commands that can race shutdown/startup.
pub(crate) const IPC_RECOVERY_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// Default timeout budget for daemon responses after request send.
pub(crate) const IPC_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Extended timeout for recovery commands that can race shutdown.
pub(crate) const IPC_RECOVERY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for daemon process/pipe teardown after stop-style commands.
pub(crate) const SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll cadence for daemon shutdown confirmation.
pub(crate) const SHUTDOWN_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Fast-fail threshold for pure "pipe not found" states on command sends.
pub(crate) const IPC_NOT_FOUND_FAST_FAIL_AFTER: Duration = Duration::from_millis(800);
pub(crate) const PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE: &str =
    "Daemon disconnected before sending a response";

pub(crate) fn is_non_success_response(response: &IpcResponse) -> bool {
    matches!(response, IpcResponse::Error { .. } | IpcResponse::Unknown)
}

pub(crate) fn command_connect_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_CONNECT_TIMEOUT,
        _ => IPC_CONNECT_TIMEOUT,
    }
}

pub(crate) fn command_response_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_RESPONSE_TIMEOUT,
        _ => IPC_DEFAULT_RESPONSE_TIMEOUT,
    }
}

pub(crate) async fn wait_for_daemon(timeout: Duration) -> Result<()> {
    let _ = open_pipe_with_retry(timeout, None).await?;
    Ok(())
}

pub(crate) async fn wait_for_daemon_shutdown(timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    loop {
        if !probe_daemon_running()? {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        sleep(SHUTDOWN_CONFIRM_POLL_INTERVAL).await;
    }
}

fn is_pipe_busy(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(231)
}

fn is_pipe_not_found(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(2)
}

pub(crate) fn classify_pipe_probe_error(err: &std::io::Error) -> Option<bool> {
    if is_pipe_busy(err) {
        Some(true)
    } else if is_pipe_not_found(err) {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn pipe_connect_retry_timeout_message(
    timeout: Duration,
    saw_busy: bool,
    saw_not_found: bool,
) -> String {
    let timeout_ms = timeout.as_millis();
    match (saw_busy, saw_not_found) {
        (true, false) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: the pipe remained busy (daemon may be starting or shutting down). Run `leopardwm-cli status` and retry once the daemon is stable."
        ),
        (false, true) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: the pipe was not found (daemon is likely not running). Start it with `leopardwm-cli run`."
        ),
        (true, true) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown). Run `leopardwm-cli status`, then retry."
        ),
        (false, false) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe. Run `leopardwm-cli status` to check daemon health."
        ),
    }
}

pub(crate) fn pipe_connect_not_found_fast_fail_message(cutoff: Duration) -> String {
    format!(
        "Daemon IPC pipe was not found after {}ms. Daemon is likely not running. Start it with `leopardwm-cli run`.",
        cutoff.as_millis()
    )
}

pub(crate) fn error_chain_has_pipe_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(is_pipe_not_found)
            .unwrap_or(false)
    })
}

pub(crate) fn error_chain_has_disconnected_before_response(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string() == PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE)
}

pub(crate) fn error_chain_has_command_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
            || cause
                .to_string()
                .contains("Timed out waiting for daemon response")
    })
}

pub(crate) fn error_chain_indicates_pipe_not_found_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("pipe was not found (daemon is likely not running)")
            || text.contains("IPC pipe was not found")
    })
}

pub(crate) fn error_chain_has_connect_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("Timed out after") && text.contains("connecting to daemon IPC pipe")
    })
}

pub(crate) fn probe_daemon_running() -> Result<bool> {
    for pipe_name in pipe_name_candidates() {
        match ClientOptions::new().open(&pipe_name) {
            Ok(_) => return Ok(true),
            Err(e) => match classify_pipe_probe_error(&e) {
                Some(true) => return Ok(true),
                Some(false) => continue,
                None => {
                    return Err(e).context(format!(
                        "Failed to check daemon state via IPC pipe '{}'",
                        pipe_name
                    ))
                }
            },
        }
    }
    Ok(false)
}

pub(crate) async fn open_pipe_with_retry(
    timeout: Duration,
    not_found_fast_fail_after: Option<Duration>,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let start = Instant::now();
    let mut saw_busy = false;
    let mut saw_not_found = false;
    let pipe_names = pipe_name_candidates();
    loop {
        let mut hard_error: Option<anyhow::Error> = None;
        for pipe_name in &pipe_names {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => return Ok(client),
                Err(e) if is_pipe_busy(&e) || is_pipe_not_found(&e) => {
                    saw_busy |= is_pipe_busy(&e);
                    saw_not_found |= is_pipe_not_found(&e);
                }
                Err(e) => {
                    hard_error = Some(anyhow::Error::new(e).context(format!(
                        "Failed to connect to daemon IPC pipe '{}'",
                        pipe_name
                    )));
                    break;
                }
            }
        }
        if let Some(err) = hard_error {
            return Err(err);
        }
        if let Some(cutoff) = not_found_fast_fail_after {
            if saw_not_found && !saw_busy && start.elapsed() >= cutoff {
                return Err(anyhow::anyhow!(pipe_connect_not_found_fast_fail_message(
                    cutoff
                )));
            }
        }
        if start.elapsed() >= timeout {
            return Err(anyhow::anyhow!(pipe_connect_retry_timeout_message(
                timeout,
                saw_busy,
                saw_not_found,
            )));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn parse_ipc_response_line(raw: &str) -> Result<IpcResponse> {
    serde_json::from_str(raw.trim()).context("Failed to parse response")
}

pub(crate) fn parse_ipc_response_frame(frame: &[u8], max_bytes: usize) -> Result<IpcResponse> {
    if frame.len() > max_bytes {
        anyhow::bail!(
            "Daemon response exceeded {} bytes; refusing to parse oversized payload",
            max_bytes
        );
    }
    if !frame.ends_with(b"\n") {
        anyhow::bail!(
            "Daemon response exceeded {} bytes or was not newline-terminated",
            max_bytes
        );
    }
    let raw =
        std::str::from_utf8(frame).context("Daemon response contained invalid UTF-8 bytes")?;
    parse_ipc_response_line(raw)
}

async fn read_ipc_response_bounded<R>(reader: R, max_bytes: usize) -> Result<IpcResponse>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader).take((max_bytes + 1) as u64);
    let mut frame = Vec::new();
    let bytes_read = reader
        .read_until(b'\n', &mut frame)
        .await
        .context("Failed to read response")?;

    if bytes_read == 0 {
        anyhow::bail!(PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE);
    }

    parse_ipc_response_frame(&frame, max_bytes)
}

/// Send a command to the daemon and return the response (with timeout).
pub(crate) async fn send_command(cmd: IpcCommand) -> Result<IpcResponse> {
    let connect_timeout = command_connect_timeout(&cmd);
    let response_timeout = command_response_timeout(&cmd);
    send_command_with_timeouts(cmd, connect_timeout, response_timeout).await
}

/// Send command with explicit connect/response timeout budgets.
async fn send_command_with_timeouts(
    cmd: IpcCommand,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<IpcResponse> {
    send_command_inner(cmd, connect_timeout, response_timeout).await
}

/// Inner implementation with separate connect/response timeout control.
async fn send_command_inner(
    cmd: IpcCommand,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<IpcResponse> {
    // Connect to the named pipe (retry if busy/starting)
    let client = open_pipe_with_retry(connect_timeout, Some(IPC_NOT_FOUND_FAST_FAIL_AFTER)).await?;

    let (reader, mut writer) = tokio::io::split(client);

    // Send command as JSON line
    let json = serde_json::to_string(&cmd)? + "\n";
    writer
        .write_all(json.as_bytes())
        .await
        .context("Failed to send command")?;

    timeout(
        response_timeout,
        read_ipc_response_bounded(reader, MAX_IPC_MESSAGE_SIZE),
    )
    .await
    .with_context(|| {
        format!(
            "Timed out waiting for daemon response after {}ms",
            response_timeout.as_millis()
        )
    })?
}
