//! Daemon lifecycle and recovery handlers: run, stop, panic-revert, status, subscribe, autostart.

use crate::args::AutostartAction;
use crate::ipc_client::{
    error_chain_has_command_timeout, error_chain_has_connect_timeout,
    error_chain_has_disconnected_before_response, error_chain_has_pipe_not_found,
    error_chain_indicates_pipe_not_found_timeout, is_non_success_response, open_pipe_with_retry,
    probe_daemon_running, send_command, wait_for_daemon, wait_for_daemon_shutdown,
    IPC_CONNECT_TIMEOUT, IPC_NOT_FOUND_FAST_FAIL_AFTER, SHUTDOWN_CONFIRM_TIMEOUT,
};
use crate::output::print_response;
use anyhow::{Context, Result};
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::uncloak_all_visible_windows;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
    if safe_mode {
        cmd.arg("--safe-mode");
    }
    apply_detach_flags(&mut cmd);

    let child = cmd.spawn().context("Failed to start leopardwm daemon")?;
    if safe_mode {
        println!("Started leopardwm daemon in SAFE MODE (PID {}).", child.id());
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
    let stdout_path = log_dir.join("leopardwm-watchdog.log");
    let stderr_path = log_dir.join("leopardwm-watchdog.err.log");

    let stdout = File::create(&stdout_path).context("Failed to create watchdog stdout log")?;
    let stderr = File::create(&stderr_path).context("Failed to create watchdog stderr log")?;

    let mut cmd = Command::new(watchdog_path);
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
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
        stdout_path.display(),
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
    if is_non_success_response(&response) {
        run_local_emergency_visibility_restore(apply_non_success_recovery_reason())
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

/// Subscribe to daemon events and stream them as newline-delimited JSON
/// to stdout. After the daemon answers `Subscribed`, the connection
/// stays open and every subsequent line is an `IpcEvent` frame. This is
/// the documented client-state-machine mode-switch — the response parser
/// is `IpcResponse` for the first frame, `IpcEvent` for all subsequent
/// frames.
pub(crate) async fn handle_subscribe(events: Option<Vec<String>>) -> Result<()> {
    use leopardwm_ipc::EventKind;
    use std::collections::BTreeSet;

    // Parse the requested kinds. Empty/missing means "all".
    let requested: BTreeSet<EventKind> = match events {
        None => BTreeSet::new(), // server interprets empty as all
        Some(list) => {
            let mut out = BTreeSet::new();
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
                    other => anyhow::bail!(
                        "Unknown event kind '{}'. Valid: workspace, focused_window, \
                         layout, config, heartbeat",
                        other
                    ),
                };
                out.insert(kind);
            }
            out
        }
    };

    if !probe_daemon_running()? {
        anyhow::bail!("Daemon is not running. Start it with `leopardwm-cli run`.");
    }

    // Open the pipe and send Subscribe.
    let client =
        open_pipe_with_retry(IPC_CONNECT_TIMEOUT, Some(IPC_NOT_FOUND_FAST_FAIL_AFTER)).await?;
    let (reader, mut writer) = tokio::io::split(client);
    let cmd = IpcCommand::Subscribe { events: requested };
    let cmd_json = serde_json::to_string(&cmd)? + "\n";
    writer
        .write_all(cmd_json.as_bytes())
        .await
        .context("Failed to send Subscribe command")?;

    // Read the Subscribed ack as IpcResponse — last frame parsed via
    // that type. After this, the parser switches to IpcEvent. We
    // intentionally do NOT use `reader.take(MAX_IPC_MESSAGE_SIZE)` here
    // (that would cap *total* bytes, killing long-lived subscribers
    // after ~64 KiB of events). Per-frame size guarding is the daemon's
    // responsibility (write_event_frame caps each frame at 64 KiB).
    let mut buf = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    let bytes = buf
        .read_line(&mut line)
        .await
        .context("Failed to read Subscribed ack")?;
    if bytes == 0 {
        anyhow::bail!("Daemon disconnected before sending Subscribed ack");
    }
    let ack: IpcResponse = serde_json::from_str(line.trim())
        .with_context(|| format!("Failed to parse Subscribed ack: {}", line.trim()))?;
    match ack {
        IpcResponse::Subscribed { .. } => {}
        IpcResponse::Error { message } => anyhow::bail!("Subscribe rejected: {}", message),
        other => anyhow::bail!("Unexpected response to Subscribe: {:?}", other),
    }

    // Stream loop. Each frame is a single line of JSON; raw passthrough
    // to stdout so users can pipe into `jq` etc.
    let mut stdout = tokio::io::stdout();
    let mut event_line = Vec::new();
    loop {
        event_line.clear();
        let bytes = buf
            .read_until(b'\n', &mut event_line)
            .await
            .context("Failed to read event frame")?;
        if bytes == 0 {
            // Daemon closed the pipe (shutdown, restart, etc.)
            break;
        }
        // Validate as IpcEvent so we surface daemon-side bugs noisily,
        // but pass the raw bytes through to stdout to preserve any
        // formatting subtleties for jq consumers.
        if let Err(e) = serde_json::from_slice::<leopardwm_ipc::IpcEvent>(&event_line) {
            eprintln!(
                "Warning: failed to parse event frame ({}): {}",
                e,
                String::from_utf8_lossy(&event_line).trim_end()
            );
            continue;
        }
        stdout
            .write_all(&event_line)
            .await
            .context("Failed to write event to stdout")?;
        stdout.flush().await.ok();
    }
    Ok(())
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
