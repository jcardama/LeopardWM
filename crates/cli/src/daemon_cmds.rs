//! Daemon lifecycle and recovery handlers: run, stop, panic-revert, status, subscribe, autostart.

use crate::args::AutostartAction;
use crate::ipc_client::{
    error_chain_has_command_timeout, error_chain_has_connect_timeout,
    error_chain_has_disconnected_before_response, error_chain_has_pipe_not_found,
    error_chain_indicates_pipe_not_found_timeout, is_non_success_response, open_pipe_with_retry,
    probe_daemon_running, send_command, wait_for_daemon, wait_for_daemon_shutdown,
    IPC_CONNECT_TIMEOUT, IPC_DEFAULT_RESPONSE_TIMEOUT, IPC_NOT_FOUND_FAST_FAIL_AFTER,
    SHUTDOWN_CONFIRM_TIMEOUT,
};
use crate::output::print_response;
use anyhow::{Context, Result};
use leopardwm_ipc::{
    is_protocol_version_supported, EventKind, IpcCommand, IpcEvent, IpcResponse,
    MAX_IPC_MESSAGE_SIZE,
};
use leopardwm_platform_win32::uncloak_all_visible_windows;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn watchdog_binary_name() -> &'static str {
    if cfg!(windows) {
        "leopardwm-watchdog.exe"
    } else {
        "leopardwm-watchdog"
    }
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "leopardwm.exe"
    } else {
        "leopardwm"
    }
}

pub(crate) fn find_daemon_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let candidate = exe_dir.join(daemon_binary_name());
    if candidate.exists() {
        return Some(candidate);
    }

    let cwd = std::env::current_dir().ok()?;
    let debug = cwd.join("target").join("debug").join(daemon_binary_name());
    if debug.exists() {
        return Some(debug);
    }
    let release = cwd
        .join("target")
        .join("release")
        .join(daemon_binary_name());
    if release.exists() {
        return Some(release);
    }

    None
}

fn ensure_daemon_binary() -> Result<PathBuf> {
    if let Some(path) = find_daemon_binary() {
        return Ok(path);
    }

    println!("Daemon binary not found. Building leopardwm-daemon...");
    let status = Command::new("cargo")
        .args(["build", "-p", "leopardwm-daemon"])
        .status()
        .context("Failed to run cargo build for leopardwm-daemon")?;
    if !status.success() {
        anyhow::bail!("cargo build failed for leopardwm-daemon");
    }

    find_daemon_binary().context("Daemon binary still not found after build")
}

#[cfg(windows)]
fn apply_detach_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn apply_detach_flags(_cmd: &mut Command) {}

fn spawn_daemon(safe_mode: bool) -> Result<u32> {
    let daemon_path = ensure_daemon_binary()?;
    let log_dir = leopardwm_ipc::log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    // The daemon writes its own leopardwm-daemon.log; send its stdout to null
    // so we don't open a second handle to the same file. Keep stderr for
    // panics that fire before the tracing subscriber initializes.
    let stderr_path = log_dir.join("leopardwm-daemon.err.log");
    let stderr = File::create(&stderr_path).context("Failed to create daemon stderr log")?;

    let mut cmd = Command::new(daemon_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    if safe_mode {
        cmd.arg("--safe-mode");
    }
    apply_detach_flags(&mut cmd);

    let child = cmd.spawn().context("Failed to start leopardwm daemon")?;
    if safe_mode {
        println!(
            "Started leopardwm daemon in SAFE MODE (PID {}).",
            child.id()
        );
    } else {
        println!("Started leopardwm daemon (PID {}).", child.id());
    }
    println!(
        "Logs: {} / {}",
        log_dir.join("leopardwm-daemon.log").display(),
        stderr_path.display()
    );
    Ok(child.id())
}

fn find_watchdog_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let candidate = exe_dir.join(watchdog_binary_name());
    if candidate.exists() {
        return Some(candidate);
    }

    let cwd = std::env::current_dir().ok()?;
    let debug = cwd
        .join("target")
        .join("debug")
        .join(watchdog_binary_name());
    if debug.exists() {
        return Some(debug);
    }
    let release = cwd
        .join("target")
        .join("release")
        .join(watchdog_binary_name());
    if release.exists() {
        return Some(release);
    }
    None
}

fn spawn_watchdog(safe_mode: bool) -> Result<u32> {
    let Some(watchdog_path) = find_watchdog_binary() else {
        // Watchdog not bundled (e.g. dev build that didn't `cargo build` it).
        // Fall back to direct daemon spawn rather than failing — preserves
        // backwards-compatible behavior for users who build a partial workspace.
        eprintln!(
            "leopardwm-watchdog binary not found alongside this CLI; \
             falling back to direct daemon spawn (no crash recovery)."
        );
        return spawn_daemon(safe_mode);
    };

    // Make sure the daemon binary is buildable / present too — the watchdog
    // looks for it next to itself, so resolve it via the same search the
    // direct-spawn path uses (covers the "ran from cargo target/" case).
    ensure_daemon_binary()?;

    let log_dir = leopardwm_ipc::log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    let watchdog_log_path = log_dir.join("leopardwm-watchdog.log");
    let stderr_path = log_dir.join("leopardwm-watchdog.err.log");
    let stderr = File::create(&stderr_path).context("Failed to create watchdog stderr log")?;

    let mut cmd = Command::new(watchdog_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    if safe_mode {
        cmd.arg("--safe-mode");
    }
    apply_detach_flags(&mut cmd);

    let child = cmd.spawn().context("Failed to start leopardwm-watchdog")?;
    if safe_mode {
        println!(
            "Started leopardwm-watchdog supervising daemon in SAFE MODE (PID {}).",
            child.id()
        );
    } else {
        println!(
            "Started leopardwm-watchdog supervising daemon (PID {}).",
            child.id()
        );
    }
    println!(
        "Logs: {} / {}",
        watchdog_log_path.display(),
        stderr_path.display()
    );
    Ok(child.id())
}

pub(crate) fn safe_mode_existing_daemon_message() -> &'static str {
    "Daemon is already running. '--safe-mode' only applies when starting a new daemon. Stop it with 'leopardwm-cli stop', then run 'leopardwm-cli run --safe-mode'."
}

pub(crate) fn panic_revert_not_running_message() -> &'static str {
    "Daemon is not running. Local emergency visibility restore was executed (same action as `leopardwm-cli emergency-uncloak`)."
}

pub(crate) fn panic_revert_unconfirmed_message() -> &'static str {
    "Daemon disconnected before confirming panic-revert completion. Local emergency visibility restore was executed. Verify windows are visible, run 'leopardwm-cli status' (it should fail if daemon exited), and run 'leopardwm-cli stop' if the daemon still responds."
}

pub(crate) fn panic_revert_timeout_recovery_message() -> &'static str {
    "Timed out waiting for panic-revert response. Local emergency visibility restore was executed. Run 'leopardwm-cli status' to confirm daemon shutdown."
}

pub(crate) fn stop_timeout_recovery_message() -> &'static str {
    "Timed out waiting for daemon stop confirmation. Run 'leopardwm-cli status' to verify shutdown; if windows remain hidden, run 'leopardwm-cli panic-revert' or `leopardwm-cli emergency-uncloak`."
}

pub(crate) fn apply_not_running_message() -> &'static str {
    "Daemon is not running. Start it with `leopardwm-cli run` (or `leopardwm-cli run --safe-mode`) before applying layout."
}

pub(crate) fn apply_timeout_recovery_message() -> &'static str {
    "Timed out waiting for `apply` response. If desktop control degrades, run `leopardwm-cli panic-revert` first, or run `leopardwm-cli emergency-uncloak` from any reachable terminal."
}

pub(crate) fn apply_unconfirmed_recovery_message() -> &'static str {
    "Apply completion was not confirmed. Local emergency visibility restore was executed. Verify windows are visible, then run `leopardwm-cli status` before retrying."
}

pub(crate) fn apply_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success apply response. Local emergency visibility restore was executed. Verify windows are visible, then run `leopardwm-cli status` before retrying."
}

pub(crate) fn apply_pending_response_message() -> &'static str {
    "Layout application remains pending. Retry after recovery finishes."
}

pub(crate) fn stop_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success stop response. Local emergency visibility restore was executed. Treat shutdown as unconfirmed and run `leopardwm-cli status`."
}

pub(crate) fn panic_revert_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success panic-revert response. Local emergency visibility restore was executed. Verify windows are visible and run `leopardwm-cli status`."
}

pub(crate) fn apply_non_success_recovery_reason() -> &'static str {
    "apply daemon returned non-success response"
}

pub(crate) fn stop_non_success_recovery_reason() -> &'static str {
    "stop daemon returned non-success response"
}

pub(crate) fn panic_revert_non_success_recovery_reason() -> &'static str {
    "panic-revert daemon returned non-success response"
}

pub(crate) fn stop_race_shutdown_message() -> &'static str {
    "Daemon is already stopping or stopped. Run 'leopardwm-cli status' to confirm it no longer responds."
}

pub(crate) fn stop_unconfirmed_message() -> &'static str {
    "Daemon stop was not confirmed. Treat this as unconfirmed shutdown: run 'leopardwm-cli status', and if windows remain hidden run 'leopardwm-cli panic-revert' or `leopardwm-cli emergency-uncloak`."
}

fn local_emergency_restore_success_message() -> &'static str {
    "Executed local emergency visibility restore (best-effort)."
}

fn run_local_emergency_visibility_restore(reason: &str) -> Result<()> {
    uncloak_all_visible_windows();
    println!("{}", local_emergency_restore_success_message());
    println!("Recovery trigger: {}", reason);
    Ok(())
}

pub(crate) async fn handle_run(
    no_apply: bool,
    wait_ms: u64,
    safe_mode: bool,
    no_watchdog: bool,
) -> Result<()> {
    let already_running = probe_daemon_running()?;

    if already_running && safe_mode {
        anyhow::bail!(safe_mode_existing_daemon_message());
    }

    if !already_running {
        if no_watchdog {
            spawn_daemon(safe_mode)?;
        } else {
            spawn_watchdog(safe_mode)?;
        }
    } else {
        println!("Daemon already running.");
    }

    wait_for_daemon(Duration::from_millis(wait_ms)).await?;

    if no_apply {
        println!("Daemon is ready.");
        return Ok(());
    }

    let response = send_apply_with_recovery().await?;
    print_response(&response);
    conclude_apply_command_response(&response, run_local_emergency_visibility_restore)?;

    Ok(())
}

pub(crate) fn conclude_apply_command_response(
    response: &IpcResponse,
    mut restore: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    if matches!(response, IpcResponse::ApplyPending { .. }) {
        anyhow::bail!(apply_pending_response_message());
    }
    if is_non_success_response(response) {
        restore(apply_non_success_recovery_reason())
            .context("Failed to execute local emergency visibility restore")?;
        anyhow::bail!(apply_error_response_recovery_message());
    }
    Ok(())
}

async fn send_apply_with_recovery() -> Result<IpcResponse> {
    match send_command(IpcCommand::Apply).await {
        Ok(response) => Ok(response),
        Err(err) if error_chain_indicates_pipe_not_found_timeout(&err) => {
            anyhow::bail!(apply_not_running_message());
        }
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            anyhow::bail!(apply_not_running_message());
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            run_local_emergency_visibility_restore("apply response timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(apply_timeout_recovery_message());
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            run_local_emergency_visibility_restore("apply daemon disconnected before response")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(apply_unconfirmed_recovery_message());
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            run_local_emergency_visibility_restore("apply IPC connect timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(apply_unconfirmed_recovery_message());
        }
        Err(err) => {
            run_local_emergency_visibility_restore("apply unexpected IPC failure")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                apply_unconfirmed_recovery_message(),
                err
            );
        }
    }
}

pub(crate) async fn handle_stop() -> Result<()> {
    let daemon_running = probe_daemon_running()?;

    if !daemon_running {
        run_local_emergency_visibility_restore("stop requested while daemon not running")
            .context("Failed to execute local emergency visibility restore")?;
        println!("Daemon not running.");
        return Ok(());
    }

    let response = match send_command(IpcCommand::Stop).await {
        Ok(response) => response,
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            run_local_emergency_visibility_restore("stop lost daemon connection before response")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\n{}",
                stop_race_shutdown_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            run_local_emergency_visibility_restore("stop daemon disconnected before response")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\n{}",
                stop_race_shutdown_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            run_local_emergency_visibility_restore("stop response timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            run_local_emergency_visibility_restore("stop IPC connect timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) => {
            run_local_emergency_visibility_restore("stop unexpected IPC failure")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                stop_unconfirmed_message(),
                err
            );
        }
    };

    print_response(&response);
    if is_non_success_response(&response) {
        run_local_emergency_visibility_restore(stop_non_success_recovery_reason())
            .context("Failed to execute local emergency visibility restore")?;
        anyhow::bail!(
            "{}\n{}",
            stop_error_response_recovery_message(),
            stop_unconfirmed_message()
        );
    }

    match wait_for_daemon_shutdown(SHUTDOWN_CONFIRM_TIMEOUT).await {
        Ok(true) => {}
        Ok(false) => {
            run_local_emergency_visibility_restore("stop shutdown confirmation timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) => {
            run_local_emergency_visibility_restore("stop shutdown confirmation probe failed")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "Failed to confirm daemon shutdown after stop: {}.\n{}",
                err,
                stop_unconfirmed_message()
            );
        }
    }
    Ok(())
}

pub(crate) async fn handle_panic_revert() -> Result<()> {
    let daemon_running = probe_daemon_running()?;
    if !daemon_running {
        run_local_emergency_visibility_restore("panic-revert requested while daemon not running")
            .context("Failed to execute local emergency visibility restore")?;
        println!("{}", panic_revert_not_running_message());
        return Ok(());
    }

    let response = match send_command(IpcCommand::PanicRevert).await {
        Ok(response) => response,
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            run_local_emergency_visibility_restore(
                "panic-revert lost daemon connection before response",
            )
            .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            run_local_emergency_visibility_restore(
                "panic-revert daemon disconnected before response",
            )
            .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            run_local_emergency_visibility_restore("panic-revert response timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_timeout_recovery_message());
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            run_local_emergency_visibility_restore("panic-revert IPC connect timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_timeout_recovery_message());
        }
        Err(err) => {
            run_local_emergency_visibility_restore("panic-revert unexpected IPC failure")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                panic_revert_unconfirmed_message(),
                err
            );
        }
    };

    print_response(&response);
    if is_non_success_response(&response) {
        run_local_emergency_visibility_restore(panic_revert_non_success_recovery_reason())
            .context("Failed to execute local emergency visibility restore")?;
        anyhow::bail!(
            "{}\n{}",
            panic_revert_error_response_recovery_message(),
            panic_revert_unconfirmed_message()
        );
    }

    match wait_for_daemon_shutdown(SHUTDOWN_CONFIRM_TIMEOUT).await {
        Ok(true) => {}
        Ok(false) => {
            run_local_emergency_visibility_restore("panic-revert shutdown confirmation timeout")
                .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(_) => {
            run_local_emergency_visibility_restore(
                "panic-revert shutdown confirmation probe failed",
            )
            .context("Failed to execute local emergency visibility restore")?;
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
    }
    Ok(())
}

pub(crate) async fn handle_status() -> Result<()> {
    if !probe_daemon_running()? {
        anyhow::bail!("Daemon is not running. Start it with `leopardwm-cli run`.");
    }

    let response = send_command(IpcCommand::QueryStatus)
        .await
        .context("Daemon appears reachable but did not return status")?;
    print_response(&response);
    if is_non_success_response(&response) {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn handle_emergency_uncloak() -> Result<()> {
    run_local_emergency_visibility_restore("explicit emergency-uncloak request")
        .context("Failed to execute local emergency visibility restore")
}

/// Parse user-facing event filter names into the shared IPC filter set.
pub(crate) fn parse_event_kinds(
    events: Option<Vec<String>>,
) -> Result<std::collections::BTreeSet<EventKind>> {
    let Some(list) = events else {
        return Ok(std::collections::BTreeSet::new());
    };

    let mut requested = std::collections::BTreeSet::new();
    for raw in list {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let kind = match trimmed {
            "workspace" => EventKind::Workspace,
            "focused_window" => EventKind::FocusedWindow,
            "layout" => EventKind::Layout,
            "config" => EventKind::Config,
            "heartbeat" => EventKind::Heartbeat,
            "workspace_state" => EventKind::WorkspaceState,
            other => anyhow::bail!(
                "Unknown event kind '{}'. Valid: workspace, focused_window, layout, config, heartbeat, workspace_state",
                other
            ),
        };
        requested.insert(kind);
    }
    Ok(requested)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamAckKind {
    Subscribe,
    WorkspaceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventReadMode {
    Subscribe { workspace_state: bool },
    WorkspaceQuery,
}

async fn read_bounded_frame<R>(reader: &mut R, description: &str) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    let bytes = reader
        .take((MAX_IPC_MESSAGE_SIZE + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await
        .with_context(|| format!("Failed to read {description}"))?;
    if bytes == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_IPC_MESSAGE_SIZE {
        anyhow::bail!(
            "Daemon {description} exceeded {} bytes; refusing oversized frame",
            MAX_IPC_MESSAGE_SIZE
        );
    }
    if !frame.ends_with(b"\n") {
        anyhow::bail!("Daemon {description} was not newline-terminated");
    }
    Ok(Some(frame))
}

pub(crate) async fn read_stream_ack<R>(
    reader: &mut R,
    expected: StreamAckKind,
    required_events: Option<&std::collections::BTreeSet<EventKind>>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let read = read_bounded_frame(reader, "stream acknowledgment");
    let frame = if expected == StreamAckKind::WorkspaceState {
        tokio::time::timeout(IPC_DEFAULT_RESPONSE_TIMEOUT, read)
            .await
            .context("Timed out waiting for workspace-state query acknowledgment")??
    } else {
        read.await?
    }
    .context("Daemon disconnected before sending stream acknowledgment")?;
    let ack: IpcResponse = serde_json::from_slice(&frame).with_context(|| {
        format!(
            "Failed to parse stream acknowledgment: {}",
            String::from_utf8_lossy(&frame).trim()
        )
    })?;

    match (expected, ack) {
        (StreamAckKind::Subscribe, IpcResponse::Subscribed { events }) => {
            if let Some(required) = required_events {
                let missing: Vec<_> = required.difference(&events).copied().collect();
                if !missing.is_empty() {
                    anyhow::bail!(
                        "Daemon subscription acknowledgment omitted requested event kinds: {missing:?}"
                    );
                }
            }
            Ok(())
        }
        (StreamAckKind::WorkspaceState, IpcResponse::WorkspaceStateReady { protocol_version })
            if is_protocol_version_supported(protocol_version) =>
        {
            Ok(())
        }
        (StreamAckKind::WorkspaceState, IpcResponse::WorkspaceStateReady { protocol_version }) => {
            anyhow::bail!(
                "Daemon selected unsupported workspace-state protocol version {}",
                protocol_version
            )
        }
        (StreamAckKind::Subscribe, IpcResponse::Error { message }) => {
            anyhow::bail!("Subscribe rejected: {message}")
        }
        (StreamAckKind::WorkspaceState, IpcResponse::Error { message }) => {
            anyhow::bail!("Workspace-state query rejected: {message}")
        }
        (expected, other) => anyhow::bail!("Unexpected response to {expected:?}: {other:?}"),
    }
}

/// Read validated event frames and forward their original NDJSON bytes.
pub(crate) async fn forward_event_frames<R, W>(
    reader: &mut R,
    output: &mut W,
    mode: EventReadMode,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut snapshot_revision = None;

    loop {
        let read = read_bounded_frame(reader, "event frame");
        let frame = if mode == EventReadMode::WorkspaceQuery {
            tokio::time::timeout(IPC_DEFAULT_RESPONSE_TIMEOUT, read)
                .await
                .context("Timed out waiting for workspace-state query frame")??
        } else {
            read.await?
        };
        let Some(frame) = frame else {
            return match mode {
                EventReadMode::WorkspaceQuery => anyhow::bail!(
                    "Daemon disconnected before completing the workspace-state snapshot"
                ),
                EventReadMode::Subscribe {
                    workspace_state: true,
                } if snapshot_revision.is_some() => {
                    anyhow::bail!("Daemon disconnected during a workspace-state snapshot")
                }
                EventReadMode::Subscribe { .. } => Ok(()),
            };
        };

        let event = match serde_json::from_slice::<IpcEvent>(&frame) {
            Ok(event) => event,
            Err(error)
                if matches!(
                    mode,
                    EventReadMode::Subscribe {
                        workspace_state: false
                    }
                ) =>
            {
                eprintln!(
                    "Warning: failed to parse event frame ({}): {}",
                    error,
                    String::from_utf8_lossy(&frame).trim_end()
                );
                continue;
            }
            Err(error) => return Err(error).context("Failed to parse workspace-state event frame"),
        };

        let transaction_result = match mode {
            EventReadMode::WorkspaceQuery => match &event {
                IpcEvent::WorkspaceSnapshotBegin { revision, .. }
                    if snapshot_revision.is_none() =>
                {
                    snapshot_revision = Some(*revision);
                    None
                }
                IpcEvent::WorkspaceSnapshotChunk { revision, .. }
                    if snapshot_revision == Some(*revision) =>
                {
                    None
                }
                IpcEvent::WorkspaceSnapshotEnd { revision }
                    if snapshot_revision == Some(*revision) =>
                {
                    Some(Ok(()))
                }
                IpcEvent::WorkspaceSnapshotError { message } => Some(Err(anyhow::anyhow!(
                    "Workspace-state snapshot failed: {message}"
                ))),
                other => {
                    return Err(anyhow::anyhow!(
                        "Unexpected or mismatched workspace-state event: {other:?}"
                    ));
                }
            },
            EventReadMode::Subscribe {
                workspace_state: true,
            } => match &event {
                IpcEvent::WorkspaceSnapshotBegin { revision, .. }
                    if snapshot_revision.is_none() =>
                {
                    snapshot_revision = Some(*revision);
                    None
                }
                IpcEvent::WorkspaceSnapshotChunk { revision, .. }
                    if snapshot_revision == Some(*revision) =>
                {
                    None
                }
                IpcEvent::WorkspaceSnapshotEnd { revision }
                    if snapshot_revision == Some(*revision) =>
                {
                    snapshot_revision = None;
                    None
                }
                IpcEvent::WorkspaceSnapshotBegin { .. }
                | IpcEvent::WorkspaceSnapshotChunk { .. }
                | IpcEvent::WorkspaceSnapshotEnd { .. } => {
                    return Err(anyhow::anyhow!(
                        "Unexpected or mismatched workspace-state snapshot frame: {event:?}"
                    ));
                }
                IpcEvent::WorkspaceSnapshotError { message } => Some(Err(anyhow::anyhow!(
                    "Workspace-state snapshot failed: {message}"
                ))),
                IpcEvent::Lagged { skipped } => Some(Err(anyhow::anyhow!(
                    "Workspace-state subscription lagged by {skipped} events; reconnect for a fresh snapshot"
                ))),
                other if snapshot_revision.is_some() => {
                    return Err(anyhow::anyhow!(
                        "Event interleaved within workspace-state snapshot: {other:?}"
                    ));
                }
                _ => None,
            },
            EventReadMode::Subscribe {
                workspace_state: false,
            } => None,
        };

        output
            .write_all(&frame)
            .await
            .context("Failed to write event to stdout")?;
        output
            .flush()
            .await
            .context("Failed to flush event output")?;

        if let Some(result) = transaction_result {
            return result;
        }
    }
}

async fn open_stream_command(
    command: IpcCommand,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    if !probe_daemon_running()? {
        anyhow::bail!("Daemon is not running. Start it with `leopardwm-cli run`.");
    }

    let mut client =
        open_pipe_with_retry(IPC_CONNECT_TIMEOUT, Some(IPC_NOT_FOUND_FAST_FAIL_AFTER)).await?;
    let command_json = serde_json::to_string(&command)? + "\n";
    client
        .write_all(command_json.as_bytes())
        .await
        .context("Failed to send stream command")?;
    Ok(client)
}

/// Subscribe to daemon events and stream them as newline-delimited JSON.
pub(crate) async fn handle_subscribe(events: Option<Vec<String>>) -> Result<()> {
    let requested = parse_event_kinds(events)?;
    let workspace_state = requested.contains(&EventKind::WorkspaceState);
    let client = open_stream_command(IpcCommand::Subscribe {
        events: requested.clone(),
    })
    .await?;
    let (reader, _writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader);

    read_stream_ack(&mut reader, StreamAckKind::Subscribe, Some(&requested)).await?;
    let mut stdout = tokio::io::stdout();
    forward_event_frames(
        &mut reader,
        &mut stdout,
        EventReadMode::Subscribe { workspace_state },
    )
    .await
}

/// Query and print one complete workspace-state snapshot as NDJSON.
pub(crate) async fn handle_query_workspaces() -> Result<()> {
    let client = open_stream_command(IpcCommand::QueryWorkspaceState).await?;
    let (reader, _writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader);

    read_stream_ack(&mut reader, StreamAckKind::WorkspaceState, None).await?;
    let mut stdout = tokio::io::stdout();
    forward_event_frames(&mut reader, &mut stdout, EventReadMode::WorkspaceQuery).await
}
/// Handle the autostart command (enable/disable Registry run key).
pub(crate) fn handle_autostart(action: AutostartAction) -> Result<()> {
    use leopardwm_platform_win32::autostart;

    match action {
        AutostartAction::Enable => {
            let daemon_path = ensure_daemon_binary()?;
            autostart::enable_autostart(&daemon_path)?;
            println!("Auto-start enabled: \"{}\"", daemon_path.display());
        }
        AutostartAction::Disable => {
            let was_enabled = autostart::get_autostart().unwrap_or(false);
            autostart::disable_autostart()?;
            if was_enabled {
                println!("Auto-start disabled.");
            } else {
                println!("Auto-start was not enabled.");
            }
        }
    }

    Ok(())
}
