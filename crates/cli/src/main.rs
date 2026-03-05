//! LeopardWM CLI
//!
//! Command-line interface for controlling the LeopardWM window manager.
//!
//! Commands are sent to the daemon via IPC (named pipe).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use leopardwm_ipc::{pipe_name_candidates, IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE};
use leopardwm_platform_win32::uncloak_all_visible_windows;
use std::fs;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{sleep, timeout};

/// Timeout budget for establishing an IPC connection to the daemon.
const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Extended connect timeout budget for recovery commands that can race shutdown/startup.
const IPC_RECOVERY_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// Default timeout budget for daemon responses after request send.
const IPC_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Apply can involve heavier work and should allow a longer response window.
const IPC_APPLY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Extended timeout for recovery commands that can race shutdown.
const IPC_RECOVERY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for daemon process/pipe teardown after stop-style commands.
const SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll cadence for daemon shutdown confirmation.
const SHUTDOWN_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Fast-fail threshold for pure "pipe not found" states on command sends.
const IPC_NOT_FOUND_FAST_FAIL_AFTER: Duration = Duration::from_millis(800);
const RUN_WAIT_DEFAULT_MS: u64 = 5000;
const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
const MAX_SET_WIDTH_FRACTION: f64 = 1.0;
const PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE: &str =
    "Daemon disconnected before sending a response";

fn validate_set_width_fraction(fraction: f64) -> std::result::Result<(), String> {
    if !fraction.is_finite() {
        return Err("set-width fraction must be a finite number".to_string());
    }
    if !(MIN_SET_WIDTH_FRACTION..=MAX_SET_WIDTH_FRACTION).contains(&fraction) {
        return Err(format!(
            "set-width fraction must be in [{:.1}, {:.1}]",
            MIN_SET_WIDTH_FRACTION, MAX_SET_WIDTH_FRACTION
        ));
    }
    Ok(())
}

fn parse_set_width_fraction(raw: &str) -> std::result::Result<f64, String> {
    let fraction = raw
        .parse::<f64>()
        .map_err(|_| format!("Invalid set-width fraction '{}': expected a number", raw))?;
    validate_set_width_fraction(fraction)?;
    Ok(fraction)
}

fn is_non_success_response(response: &IpcResponse) -> bool {
    matches!(response, IpcResponse::Error { .. } | IpcResponse::Unknown)
}

fn command_connect_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_CONNECT_TIMEOUT,
        _ => IPC_CONNECT_TIMEOUT,
    }
}

fn command_response_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Apply => IPC_APPLY_RESPONSE_TIMEOUT,
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_RESPONSE_TIMEOUT,
        _ => IPC_DEFAULT_RESPONSE_TIMEOUT,
    }
}

#[derive(Parser)]
#[command(name = "leopardwm-cli")]
#[command(author, version, about = "Control the LeopardWM window manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Focus commands
    Focus {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Scroll the viewport
    Scroll {
        #[command(subcommand)]
        direction: ScrollDirection,
    },
    /// Move the focused column
    Move {
        #[command(subcommand)]
        direction: MoveDirection,
    },
    /// Resize the focused column
    Resize {
        /// Width delta in pixels (positive to grow, negative to shrink)
        #[arg(short, long)]
        delta: i32,
    },
    /// Focus a different monitor
    FocusMonitor {
        #[command(subcommand)]
        direction: MonitorDirection,
    },
    /// Move the focused window to a different monitor
    MoveToMonitor {
        #[command(subcommand)]
        direction: MonitorDirection,
    },
    /// Query workspace state
    Query {
        #[command(subcommand)]
        what: QueryType,
    },
    /// Re-enumerate windows
    Refresh,
    /// Apply current layout to windows
    Apply,
    /// Reload configuration from file
    Reload,
    /// Start daemon (if needed) and apply layout once
    Run {
        /// Skip applying layout after the daemon is ready
        #[arg(long)]
        no_apply: bool,
        /// How long to wait for the daemon to become ready (milliseconds)
        #[arg(long, default_value_t = RUN_WAIT_DEFAULT_MS)]
        wait_ms: u64,
        /// Start daemon in safe mode (no hotkeys, no cloaking)
        #[arg(long)]
        safe_mode: bool,
    },
    /// Generate default configuration file
    Init {
        /// Output path (default: %APPDATA%/leopardwm/config/config.toml)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite existing config file
        #[arg(short, long)]
        force: bool,
        /// Use a preset profile: developer, laptop, ultrawide
        #[arg(short, long)]
        profile: Option<String>,
    },
    /// Stop the daemon
    Stop,
    /// Toggle pause/resume of tiling operations
    #[command(visible_alias = "pause")]
    TogglePause,
    /// Emergency recovery command to revert managed windows
    #[command(visible_alias = "recover")]
    PanicRevert,
    /// Local emergency restore that bypasses daemon IPC
    #[command(visible_alias = "restore-windows")]
    EmergencyUncloak,
    /// Close the focused window
    CloseWindow,
    /// Toggle floating for the focused window
    ToggleFloating,
    /// Toggle fullscreen for the focused window
    ToggleFullscreen,
    /// Set the focused column width
    SetWidth {
        /// Width as fraction of viewport (e.g., 0.333, 0.5, 0.667)
        #[arg(short, long, value_parser = parse_set_width_fraction)]
        fraction: f64,
    },
    /// Equalize all column widths
    EqualizeWidths,
    /// Query daemon status
    Status,
    /// Run diagnostic checks
    Doctor,
    /// Manage auto-start on login
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Collect diagnostic logs for bug reports
    CollectLogs,
    /// Quick health check (is daemon alive and responding?)
    Health,
    /// First-run setup assistant
    Setup,
    /// Manage configuration files
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum FocusDirection {
    /// Focus the column to the left
    Left,
    /// Focus the column to the right
    Right,
    /// Focus the window above (in stacked columns)
    Up,
    /// Focus the window below (in stacked columns)
    Down,
}

#[derive(Subcommand)]
enum ScrollDirection {
    /// Scroll viewport left
    Left {
        /// Pixels to scroll (default: 100)
        #[arg(short, long, default_value = "100")]
        pixels: i32,
    },
    /// Scroll viewport right
    Right {
        /// Pixels to scroll (default: 100)
        #[arg(short, long, default_value = "100")]
        pixels: i32,
    },
}

#[derive(Subcommand)]
enum MoveDirection {
    /// Move focused column left
    Left,
    /// Move focused column right
    Right,
}

#[derive(Subcommand)]
enum MonitorDirection {
    /// Focus/move to the monitor on the left
    Left,
    /// Focus/move to the monitor on the right
    Right,
}

#[derive(Subcommand)]
enum QueryType {
    /// Get current workspace state
    Workspace,
    /// Get focused window info
    Focused,
    /// List all managed windows
    All,
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Enable auto-start on login
    Enable,
    /// Disable auto-start on login
    Disable,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Reset config to defaults (backs up current to config.toml.bak)
    Reset,
    /// Create a backup of the current config
    Backup,
    /// Restore config from backup
    Restore,
}

/// Convert CLI command to IPC command.
fn to_ipc_command(cmd: &Commands) -> IpcCommand {
    match cmd {
        Commands::Focus { direction } => match direction {
            FocusDirection::Left => IpcCommand::FocusLeft,
            FocusDirection::Right => IpcCommand::FocusRight,
            FocusDirection::Up => IpcCommand::FocusUp,
            FocusDirection::Down => IpcCommand::FocusDown,
        },
        Commands::Scroll { direction } => match direction {
            ScrollDirection::Left { pixels } => IpcCommand::Scroll {
                delta: -(*pixels as f64),
            },
            ScrollDirection::Right { pixels } => IpcCommand::Scroll {
                delta: *pixels as f64,
            },
        },
        Commands::Move { direction } => match direction {
            MoveDirection::Left => IpcCommand::MoveColumnLeft,
            MoveDirection::Right => IpcCommand::MoveColumnRight,
        },
        Commands::Resize { delta } => IpcCommand::Resize { delta: *delta },
        Commands::FocusMonitor { direction } => match direction {
            MonitorDirection::Left => IpcCommand::FocusMonitorLeft,
            MonitorDirection::Right => IpcCommand::FocusMonitorRight,
        },
        Commands::MoveToMonitor { direction } => match direction {
            MonitorDirection::Left => IpcCommand::MoveWindowToMonitorLeft,
            MonitorDirection::Right => IpcCommand::MoveWindowToMonitorRight,
        },
        Commands::Query { what } => match what {
            QueryType::Workspace => IpcCommand::QueryWorkspace,
            QueryType::Focused => IpcCommand::QueryFocused,
            QueryType::All => IpcCommand::QueryAllWindows,
        },
        Commands::Refresh => IpcCommand::Refresh,
        Commands::Apply => IpcCommand::Apply,
        Commands::Reload => IpcCommand::Reload,
        Commands::CloseWindow => IpcCommand::CloseWindow,
        Commands::ToggleFloating => IpcCommand::ToggleFloating,
        Commands::ToggleFullscreen => IpcCommand::ToggleFullscreen,
        Commands::SetWidth { fraction } => IpcCommand::SetColumnWidth {
            fraction: *fraction,
        },
        Commands::EqualizeWidths => IpcCommand::EqualizeColumnWidths,
        Commands::Status => IpcCommand::QueryStatus,
        Commands::Health => IpcCommand::HealthCheck,
        Commands::PanicRevert => IpcCommand::PanicRevert,
        Commands::Run { .. } => unreachable!("Run handled separately"),
        Commands::Init { .. } => unreachable!("Init handled separately"),
        Commands::Doctor => unreachable!("Doctor handled separately"),
        Commands::Autostart { .. } => unreachable!("Autostart handled separately"),
        Commands::CollectLogs => unreachable!("CollectLogs handled separately"),
        Commands::Setup => unreachable!("Setup handled separately"),
        Commands::Config { .. } => unreachable!("Config handled separately"),
        Commands::EmergencyUncloak => unreachable!("EmergencyUncloak handled separately"),
        Commands::Stop => IpcCommand::Stop,
        Commands::TogglePause => IpcCommand::TogglePause,
    }
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "leopardwm.exe"
    } else {
        "leopardwm"
    }
}

fn find_daemon_binary() -> Option<PathBuf> {
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
    let log_dir = std::env::temp_dir();
    let stdout_path = log_dir.join("leopardwm-daemon.log");
    let stderr_path = log_dir.join("leopardwm-daemon.err.log");

    let stdout = File::create(&stdout_path).context("Failed to create daemon stdout log")?;
    let stderr = File::create(&stderr_path).context("Failed to create daemon stderr log")?;

    let mut cmd = Command::new(daemon_path);
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
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
        stdout_path.display(),
        stderr_path.display()
    );
    Ok(child.id())
}

async fn wait_for_daemon(timeout: Duration) -> Result<()> {
    let _ = open_pipe_with_retry(timeout, None).await?;
    Ok(())
}

async fn wait_for_daemon_shutdown(timeout: Duration) -> Result<bool> {
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

fn classify_pipe_probe_error(err: &std::io::Error) -> Option<bool> {
    if is_pipe_busy(err) {
        Some(true)
    } else if is_pipe_not_found(err) {
        Some(false)
    } else {
        None
    }
}

fn pipe_connect_retry_timeout_message(
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

fn pipe_connect_not_found_fast_fail_message(cutoff: Duration) -> String {
    format!(
        "Daemon IPC pipe was not found after {}ms. Daemon is likely not running. Start it with `leopardwm-cli run`.",
        cutoff.as_millis()
    )
}

fn safe_mode_existing_daemon_message() -> &'static str {
    "Daemon is already running. '--safe-mode' only applies when starting a new daemon. Stop it with 'leopardwm-cli stop', then run 'leopardwm-cli run --safe-mode'."
}

fn panic_revert_not_running_message() -> &'static str {
    "Daemon is not running. Local emergency visibility restore was executed (same action as `leopardwm-cli emergency-uncloak`)."
}

fn panic_revert_unconfirmed_message() -> &'static str {
    "Daemon disconnected before confirming panic-revert completion. Local emergency visibility restore was executed. Verify windows are visible, run 'leopardwm-cli status' (it should fail if daemon exited), and run 'leopardwm-cli stop' if the daemon still responds."
}

fn panic_revert_timeout_recovery_message() -> &'static str {
    "Timed out waiting for panic-revert response. Local emergency visibility restore was executed. Run 'leopardwm-cli status' to confirm daemon shutdown."
}

fn stop_timeout_recovery_message() -> &'static str {
    "Timed out waiting for daemon stop confirmation. Run 'leopardwm-cli status' to verify shutdown; if windows remain hidden, run 'leopardwm-cli panic-revert' or `leopardwm-cli emergency-uncloak`."
}

fn apply_not_running_message() -> &'static str {
    "Daemon is not running. Start it with `leopardwm-cli run` (or `leopardwm-cli run --safe-mode`) before applying layout."
}

fn apply_timeout_recovery_message() -> &'static str {
    "Timed out waiting for `apply` response. If desktop control degrades, run `leopardwm-cli panic-revert` first, or run `leopardwm-cli emergency-uncloak` from any reachable terminal."
}

fn apply_unconfirmed_recovery_message() -> &'static str {
    "Apply completion was not confirmed. Local emergency visibility restore was executed. Verify windows are visible, then run `leopardwm-cli status` before retrying."
}

fn apply_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success apply response. Local emergency visibility restore was executed. Verify windows are visible, then run `leopardwm-cli status` before retrying."
}

fn stop_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success stop response. Local emergency visibility restore was executed. Treat shutdown as unconfirmed and run `leopardwm-cli status`."
}

fn panic_revert_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success panic-revert response. Local emergency visibility restore was executed. Verify windows are visible and run `leopardwm-cli status`."
}

fn apply_non_success_recovery_reason() -> &'static str {
    "apply daemon returned non-success response"
}

fn stop_non_success_recovery_reason() -> &'static str {
    "stop daemon returned non-success response"
}

fn panic_revert_non_success_recovery_reason() -> &'static str {
    "panic-revert daemon returned non-success response"
}

fn error_chain_has_pipe_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(is_pipe_not_found)
            .unwrap_or(false)
    })
}

fn error_chain_has_disconnected_before_response(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string() == PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE)
}

fn error_chain_has_command_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
            || cause
                .to_string()
                .contains("Timed out waiting for daemon response")
    })
}

fn error_chain_indicates_pipe_not_found_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("pipe was not found (daemon is likely not running)")
            || text.contains("IPC pipe was not found")
    })
}

fn error_chain_has_connect_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("Timed out after") && text.contains("connecting to daemon IPC pipe")
    })
}

fn stop_race_shutdown_message() -> &'static str {
    "Daemon is already stopping or stopped. Run 'leopardwm-cli status' to confirm it no longer responds."
}

fn stop_unconfirmed_message() -> &'static str {
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

fn probe_daemon_running() -> Result<bool> {
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

async fn open_pipe_with_retry(
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

async fn handle_run(no_apply: bool, wait_ms: u64, safe_mode: bool) -> Result<()> {
    let already_running = probe_daemon_running()?;

    if already_running && safe_mode {
        anyhow::bail!(safe_mode_existing_daemon_message());
    }

    if !already_running {
        spawn_daemon(safe_mode)?;
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

async fn handle_apply() -> Result<()> {
    if !probe_daemon_running()? {
        anyhow::bail!(apply_not_running_message());
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

async fn handle_stop() -> Result<()> {
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

async fn handle_panic_revert() -> Result<()> {
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

async fn handle_status() -> Result<()> {
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

fn handle_emergency_uncloak() -> Result<()> {
    run_local_emergency_visibility_restore("explicit emergency-uncloak request")
        .context("Failed to execute local emergency visibility restore")
}

fn parse_ipc_response_line(raw: &str) -> Result<IpcResponse> {
    serde_json::from_str(raw.trim()).context("Failed to parse response")
}

fn parse_ipc_response_frame(frame: &[u8], max_bytes: usize) -> Result<IpcResponse> {
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
async fn send_command(cmd: IpcCommand) -> Result<IpcResponse> {
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

/// Print a response in a human-readable format.
fn print_response(response: &IpcResponse) {
    match response {
        IpcResponse::Ok => {
            println!("OK");
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
        }
        IpcResponse::WorkspaceState {
            columns,
            windows,
            focused_column,
            focused_window,
            scroll_offset,
            total_width,
        } => {
            println!("Workspace State:");
            println!("  Columns: {}", columns);
            println!("  Windows: {}", windows);
            println!("  Focused column: {}", focused_column);
            println!("  Focused window in column: {}", focused_window);
            println!("  Scroll offset: {:.1}", scroll_offset);
            println!("  Total width: {}", total_width);
        }
        IpcResponse::FocusedWindow {
            window_id,
            column_index,
            window_index,
        } => {
            println!("Focused Window:");
            match window_id {
                Some(id) => println!("  Window ID: {}", id),
                None => println!("  No window focused"),
            }
            println!("  Column index: {}", column_index);
            println!("  Window index: {}", window_index);
        }
        IpcResponse::WindowList { windows } => {
            println!("Managed Windows ({} total):", windows.len());
            for win in windows {
                let location = if win.is_floating {
                    "floating".to_string()
                } else {
                    format!(
                        "col {} win {}",
                        win.column_index.unwrap_or(0),
                        win.window_index.unwrap_or(0)
                    )
                };
                let focus_marker = if win.is_focused { " [FOCUSED]" } else { "" };
                println!(
                    "  {} - {} ({}) [{}]{}",
                    win.window_id, win.title, win.executable, location, focus_marker
                );
            }
        }
        IpcResponse::FocusedWindowInfo { window } => match window {
            Some(win) => {
                println!("Focused Window Info:");
                println!("  Window ID: {}", win.window_id);
                println!("  Title: {}", win.title);
                println!("  Class: {}", win.class_name);
                println!("  Executable: {}", win.executable);
                println!("  Position: ({}, {})", win.rect.x, win.rect.y);
                println!("  Size: {}x{}", win.rect.width, win.rect.height);
                println!("  Monitor: {}", win.monitor_id);
                if win.is_floating {
                    println!("  Layout: floating");
                } else {
                    println!(
                        "  Layout: tiled (col {}, win {})",
                        win.column_index.unwrap_or(0),
                        win.window_index.unwrap_or(0)
                    );
                }
            }
            None => {
                println!("No window is currently focused");
            }
        },
        IpcResponse::StatusInfo {
            version,
            monitors,
            total_windows,
            uptime_seconds,
        } => {
            println!("LeopardWM Daemon Status:");
            println!("  Version: {}", version);
            println!("  Monitors: {}", monitors);
            println!("  Total windows: {}", total_windows);
            let hours = uptime_seconds / 3600;
            let mins = (uptime_seconds % 3600) / 60;
            let secs = uptime_seconds % 60;
            println!("  Uptime: {}h {}m {}s", hours, mins, secs);
        }
        IpcResponse::HealthInfo {
            healthy,
            uptime_seconds,
            total_windows,
            monitors,
            paused,
        } => {
            if *healthy {
                println!("HEALTHY");
            } else {
                println!("UNHEALTHY");
            }
            println!("  Uptime: {}s", uptime_seconds);
            println!("  Windows: {}", total_windows);
            println!("  Monitors: {}", monitors);
            if *paused {
                println!("  Status: PAUSED");
            }
        }
        IpcResponse::Unknown => {
            println!("Daemon returned an unknown response status (client/daemon version mismatch)");
        }
    }
}

/// Generate default configuration content.
fn generate_default_config() -> String {
    r#"# LeopardWM Configuration
# https://github.com/jcardama/LeopardWM

[layout]
# Gap between columns in pixels
gap = 10

# Gap at the edges of the viewport in pixels
outer_gap = 10

# Default width for new columns in pixels
default_column_width = 800

# Minimum column width in pixels
min_column_width = 400

# Maximum column width in pixels
max_column_width = 1600

# Centering mode: "center" or "just_in_view"
# - center: Always center the focused column
# - just_in_view: Only scroll if focused column would be outside viewport
centering_mode = "center"

[appearance]

[behavior]
# Automatically focus new windows when they appear
focus_new_windows = true

# Track focus changes from Windows (sync with Alt-Tab, etc.)
track_focus_changes = true

# Log level: trace, debug, info, warn, error
log_level = "info"

# Focus follows mouse (hover to focus)
focus_follows_mouse = false

[hotkeys]
# Vim-style navigation with Win key
"Win+H" = "focus_left"
"Win+L" = "focus_right"
"Win+J" = "focus_down"
"Win+K" = "focus_up"

# Move columns with Win+Shift
"Win+Shift+H" = "move_column_left"
"Win+Shift+L" = "move_column_right"

# Resize with Win+Ctrl
"Win+Ctrl+H" = "resize_shrink"
"Win+Ctrl+L" = "resize_grow"

# Emergency restore + stop daemon
"Win+Ctrl+Escape" = "panic_revert"
# Optional: quick pause/resume kill switch
#"Win+Ctrl+P" = "toggle_pause"

# Close focused window
"Win+Shift+Q" = "close_window"

# Toggle floating / fullscreen
"Win+F" = "toggle_floating"
"Win+Shift+F" = "toggle_fullscreen"

# Column width presets
"Win+1" = "width_third"
"Win+2" = "width_half"
"Win+3" = "width_two_thirds"
"Win+0" = "equalize_widths"

[gestures]
# Touchpad gesture support
enabled = true
swipe_left = "focus_left"
swipe_right = "focus_right"
swipe_up = "focus_up"
swipe_down = "focus_down"

[snap_hints]
# Visual snap hint overlays during resize
enabled = true
duration_ms = 200
opacity = 128

# [[window_rules]]
# match_class = "Chrome_WidgetWin_1"
# match_title = ".*DevTools.*"
# action = "float"
"#
    .to_string()
}

/// Get the default config file path.
fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "leopardwm").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// Handle the init command (generate default config).
fn handle_init(output: Option<PathBuf>, force: bool, profile: Option<String>) -> Result<()> {
    let path = output
        .or_else(default_config_path)
        .context("Could not determine config path. Use --output to specify a path.")?;

    // Check if file exists
    if path.exists() && !force {
        anyhow::bail!(
            "Config file already exists at: {}\nUse --force to overwrite.",
            path.display()
        );
    }

    // Create parent directories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    // Write config file (apply profile overrides if specified)
    let config_content = match profile.as_deref() {
        Some("laptop") => generate_profile_config("laptop"),
        Some("ultrawide") => generate_profile_config("ultrawide"),
        Some("developer") => generate_profile_config("developer"),
        Some(other) => anyhow::bail!(
            "Unknown profile '{}'. Available: developer, laptop, ultrawide",
            other
        ),
        None => generate_default_config(),
    };
    fs::write(&path, &config_content)
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;

    if let Some(name) = &profile {
        println!("Created config file ({} profile): {}", name, path.display());
    } else {
        println!("Created config file: {}", path.display());
    }
    println!("\nEdit this file to customize LeopardWM settings.");
    println!("Run 'leopardwm-cli reload' to apply changes while daemon is running.");

    Ok(())
}

/// Generate config content for a named profile.
fn generate_profile_config(profile: &str) -> String {
    let (gap, outer_gap, default_width, min_width, max_width, centering) = match profile {
        "laptop" => (6, 6, 700, 350, 1400, "center"),
        "ultrawide" => (12, 16, 1000, 500, 2000, "just_in_view"),
        "developer" => (10, 10, 900, 400, 1600, "center"),
        _ => (10, 10, 800, 400, 1600, "center"),
    };

    format!(
        r#"# LeopardWM Configuration — {profile} profile
# https://github.com/jcardama/LeopardWM

[layout]
gap = {gap}
outer_gap = {outer_gap}
default_column_width = {default_width}
min_column_width = {min_width}
max_column_width = {max_width}
centering_mode = "{centering}"

[appearance]

[behavior]
focus_new_windows = true
track_focus_changes = true
log_level = "info"
focus_follows_mouse = false

[hotkeys]
"Win+H" = "focus_left"
"Win+L" = "focus_right"
"Win+J" = "focus_down"
"Win+K" = "focus_up"
"Win+Shift+H" = "move_column_left"
"Win+Shift+L" = "move_column_right"
"Win+Ctrl+H" = "resize_shrink"
"Win+Ctrl+L" = "resize_grow"
"Win+Ctrl+Escape" = "panic_revert"
"Win+Shift+Q" = "close_window"
"Win+F" = "toggle_floating"
"Win+Shift+F" = "toggle_fullscreen"
"Win+1" = "width_third"
"Win+2" = "width_half"
"Win+3" = "width_two_thirds"
"Win+0" = "equalize_widths"

[gestures]
enabled = true
swipe_left = "focus_left"
swipe_right = "focus_right"
swipe_up = "focus_up"
swipe_down = "focus_down"

[snap_hints]
enabled = true
duration_ms = 200
opacity = 128
"#
    )
}

/// Result of a single diagnostic check.
enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String),
}

impl CheckResult {
    fn print(&self) {
        match self {
            CheckResult::Pass(msg) => println!("[PASS] {}", msg),
            CheckResult::Warn(msg) => println!("[WARN] {}", msg),
            CheckResult::Fail(msg) => println!("[FAIL] {}", msg),
        }
    }
}

/// Get the config file path (first one that exists, or the primary default).
fn doctor_config_path() -> (Option<PathBuf>, PathBuf) {
    let primary = ProjectDirs::from("", "", "leopardwm")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    // Check all possible locations
    let mut candidates = vec![primary.clone()];
    if let Some(base) = directories::BaseDirs::new() {
        candidates.push(
            base.home_dir()
                .join(".config")
                .join("leopardwm")
                .join("config.toml"),
        );
    }
    candidates.push(PathBuf::from("config.toml"));

    for path in candidates {
        if path.exists() {
            return (Some(path.clone()), path);
        }
    }

    (None, primary)
}

/// Validate that a file contains valid TOML.
fn validate_toml_file(path: &std::path::Path) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|e| format!("Cannot read file: {}", e))?;
    content
        .parse::<toml::Table>()
        .map_err(|e| format!("Invalid TOML: {}", e))?;
    Ok(())
}

/// Check if the current process is running as administrator.
fn is_running_as_admin() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::IsUserAnAdmin;
        unsafe { IsUserAnAdmin().as_bool() }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Get the Windows version string.
fn get_windows_version() -> String {
    #[cfg(windows)]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(key) = hklm.open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion") {
            let build: String = key.get_value("CurrentBuildNumber").unwrap_or_default();
            let display: String = key.get_value("DisplayVersion").unwrap_or_default();
            let product: String = key.get_value("ProductName").unwrap_or_default();
            if !build.is_empty() {
                return format!("{} ({}, Build {})", product, display, build);
            }
        }
        "Unknown".to_string()
    }
    #[cfg(not(windows))]
    {
        "Not Windows".to_string()
    }
}

/// Handle the doctor command (run diagnostic checks).
async fn handle_doctor() -> Result<()> {
    println!("LeopardWM Doctor");
    println!("===============");

    // 1. Daemon binary check
    match find_daemon_binary() {
        Some(path) => CheckResult::Pass(format!("Daemon binary found: {}", path.display())),
        None => CheckResult::Fail(
            "Daemon binary not found. Run 'cargo build --release' to build.".to_string(),
        ),
    }
    .print();

    // 2. Config file exists
    let (found_path, display_path) = doctor_config_path();
    match &found_path {
        Some(path) => CheckResult::Pass(format!("Config file exists: {}", path.display())),
        None => CheckResult::Warn(format!(
            "No config file found. Run 'leopardwm-cli init' to create one at: {}",
            display_path.display()
        )),
    }
    .print();

    // 3. Config file is valid TOML
    if let Some(ref path) = found_path {
        match validate_toml_file(path) {
            Ok(()) => CheckResult::Pass("Config file is valid TOML".to_string()),
            Err(e) => CheckResult::Fail(format!("Config file has errors: {}", e)),
        }
        .print();
    }

    // 4. Daemon running check
    match probe_daemon_running() {
        Ok(true) => {
            // Try to get status
            match send_command(IpcCommand::QueryStatus).await {
                Ok(IpcResponse::StatusInfo {
                    version,
                    monitors,
                    total_windows,
                    uptime_seconds,
                    ..
                }) => {
                    let hours = uptime_seconds / 3600;
                    let mins = (uptime_seconds % 3600) / 60;
                    CheckResult::Pass(format!(
                        "Daemon is running (v{}, {} monitors, {} windows, uptime {}h{}m)",
                        version, monitors, total_windows, hours, mins
                    ))
                }
                Ok(other) => CheckResult::Fail(format!(
                    "Daemon IPC is reachable but returned unexpected status payload: {:?}",
                    other
                )),
                Err(e) => CheckResult::Fail(format!(
                    "Daemon IPC is reachable but status query failed: {}. Run `leopardwm-cli panic-revert` (or `leopardwm-cli emergency-uncloak`) before retrying.",
                    e
                )),
            }
            .print();
        }
        Ok(false) => {
            CheckResult::Warn(
                "Daemon is not running. Use 'leopardwm-cli run' to start.".to_string(),
            )
            .print();
        }
        Err(e) => {
            CheckResult::Warn(format!(
                "Unable to probe daemon state: {}. If the daemon may be running, try 'leopardwm-cli status'.",
                e
            ))
            .print();
        }
    }

    // 5. Admin check
    if is_running_as_admin() {
        CheckResult::Warn(
            "Running as administrator (may cause issues with non-elevated windows)".to_string(),
        )
    } else {
        CheckResult::Pass("Running as standard user".to_string())
    }
    .print();

    // 6. Windows version
    let version = get_windows_version();
    CheckResult::Pass(format!("Windows version: {}", version)).print();

    println!();
    Ok(())
}

/// Handle the autostart command (enable/disable Registry run key).
fn handle_autostart(action: AutostartAction) -> Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = hkcu
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            KEY_READ | KEY_WRITE,
        )
        .context("Failed to open Registry Run key")?;

    const REG_VALUE: &str = "LeopardWM";

    match action {
        AutostartAction::Enable => {
            let daemon_path = ensure_daemon_binary()?;
            let path_str = daemon_path.to_string_lossy().to_string();
            let quoted = format!("\"{}\"", path_str);
            run_key
                .set_value(REG_VALUE, &quoted)
                .context("Failed to set Registry value")?;
            println!("Auto-start enabled: {}", quoted);
        }
        AutostartAction::Disable => match run_key.delete_value(REG_VALUE) {
            Ok(()) => println!("Auto-start disabled."),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    println!("Auto-start was not enabled.");
                } else {
                    return Err(e).context("Failed to remove Registry value");
                }
            }
        },
    }

    Ok(())
}

/// Collect diagnostic logs into a text report for bug reports.
fn handle_collect_logs() -> Result<()> {
    println!("LeopardWM Log Collection");
    println!("=======================\n");

    // OS version
    println!("## Environment");
    println!("OS: {}", get_windows_version());
    println!("CLI Version: {}", env!("CARGO_PKG_VERSION"));
    println!();

    // Config file
    let (found_path, display_path) = doctor_config_path();
    match &found_path {
        Some(path) => {
            println!("## Config ({}):", path.display());
            match fs::read_to_string(path) {
                Ok(content) => println!("{}", content),
                Err(e) => println!("  (error reading: {})", e),
            }
        }
        None => println!(
            "## Config: not found (expected at {})",
            display_path.display()
        ),
    }
    println!();

    // Daemon log
    let log_path = std::env::temp_dir().join("leopardwm-daemon.log");
    println!("## Daemon Log ({}):", log_path.display());
    match fs::read_to_string(&log_path) {
        Ok(content) => {
            // Print last 100 lines
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(100);
            for line in &lines[start..] {
                println!("{}", line);
            }
            if start > 0 {
                println!("  ... ({} earlier lines omitted)", start);
            }
        }
        Err(e) => println!("  (not found or unreadable: {})", e),
    }
    println!();

    // Error log
    let err_log_path = std::env::temp_dir().join("leopardwm-daemon.err.log");
    println!("## Daemon Error Log ({}):", err_log_path.display());
    match fs::read_to_string(&err_log_path) {
        Ok(content) if !content.trim().is_empty() => println!("{}", content),
        Ok(_) => println!("  (empty)"),
        Err(e) => println!("  (not found or unreadable: {})", e),
    }
    println!();

    // Daemon binary
    println!("## Daemon Binary:");
    match find_daemon_binary() {
        Some(path) => println!("  Found: {}", path.display()),
        None => println!("  Not found"),
    }

    println!("\n---");
    println!("Copy the above output and attach it to your bug report.");
    Ok(())
}

/// First-run setup assistant.
fn handle_setup() -> Result<()> {
    println!("LeopardWM Setup");
    println!("==============\n");

    // Step 1: Check if config exists
    let config_path = default_config_path().context("Could not determine config path.")?;

    if config_path.exists() {
        println!("[OK] Config file already exists: {}", config_path.display());
    } else {
        println!("Creating default config file...");
        handle_init(None, false, None)?;
    }

    // Step 2: Check daemon binary
    match find_daemon_binary() {
        Some(path) => println!("[OK] Daemon binary found: {}", path.display()),
        None => {
            println!("[!!] Daemon binary not found. Building...");
            ensure_daemon_binary()?;
            println!("[OK] Daemon built successfully.");
        }
    }

    // Step 3: Print summary
    println!("\n## Quick Start");
    println!("  1. Edit config: {}", config_path.display());
    println!("  2. Start daemon: leopardwm-cli run");
    println!("  3. Check health: leopardwm-cli doctor");
    println!();
    println!("## Default Hotkeys");
    println!("  Win+H/L    Focus left/right");
    println!("  Win+J/K    Focus down/up");
    println!("  Win+Shift+H/L   Move column");
    println!("  Win+Ctrl+H/L    Resize column");
    println!("  Win+Ctrl+Escape Emergency restore + stop daemon");
    println!("  Win+F      Toggle floating");
    println!("  Win+Shift+F     Toggle fullscreen");
    println!("  Win+1/2/3/0     Column width presets");
    println!("  Win+Shift+Q     Close focused window");
    println!();
    println!("## Optional: Enable Auto-Start");
    println!("  leopardwm-cli autostart enable");

    Ok(())
}

/// Get the backup path for a config file.
fn config_backup_path(config_path: &std::path::Path) -> PathBuf {
    config_path.with_extension("toml.bak")
}

/// Handle config subcommands (reset, backup, restore).
fn handle_config(action: ConfigAction) -> Result<()> {
    let config_path = default_config_path().context("Could not determine config path.")?;
    let backup_path = config_backup_path(&config_path);

    match action {
        ConfigAction::Reset => {
            // Back up current config if it exists
            if config_path.exists() {
                fs::copy(&config_path, &backup_path)
                    .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
                println!("Backed up current config to: {}", backup_path.display());
            }
            // Write fresh defaults
            let config_content = generate_default_config();
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&config_path, config_content)
                .with_context(|| format!("Failed to write: {}", config_path.display()))?;
            println!("Config reset to defaults: {}", config_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
        ConfigAction::Backup => {
            if !config_path.exists() {
                anyhow::bail!("No config file found at: {}", config_path.display());
            }
            fs::copy(&config_path, &backup_path)
                .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
            println!("Config backed up to: {}", backup_path.display());
        }
        ConfigAction::Restore => {
            if !backup_path.exists() {
                anyhow::bail!("No backup found at: {}", backup_path.display());
            }
            fs::copy(&backup_path, &config_path)
                .with_context(|| format!("Failed to restore from {}", backup_path.display()))?;
            println!("Config restored from: {}", backup_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle locally-executed commands (do not use IPC command mapping)
    match cli.command {
        Commands::Init {
            output,
            force,
            profile,
        } => return handle_init(output, force, profile),
        Commands::Run {
            no_apply,
            wait_ms,
            safe_mode,
        } => return handle_run(no_apply, wait_ms, safe_mode).await,
        Commands::Apply => return handle_apply().await,
        Commands::Stop => return handle_stop().await,
        Commands::PanicRevert => return handle_panic_revert().await,
        Commands::EmergencyUncloak => return handle_emergency_uncloak(),
        Commands::Status => return handle_status().await,
        Commands::Doctor => return handle_doctor().await,
        Commands::Autostart { action } => return handle_autostart(action),
        Commands::CollectLogs => return handle_collect_logs(),
        Commands::Setup => return handle_setup(),
        Commands::Config { action } => return handle_config(action),
        _ => {}
    }

    if let Commands::SetWidth { fraction } = &cli.command {
        if let Err(message) = validate_set_width_fraction(*fraction) {
            anyhow::bail!(message);
        }
    }

    let ipc_cmd = to_ipc_command(&cli.command);
    let response = send_command(ipc_cmd).await?;
    print_response(&response);

    // Exit with error code if response was an error
    if is_non_success_response(&response) {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // to_ipc_command tests
    // =========================================================================

    #[test]
    fn test_to_ipc_command_focus_left() {
        let cmd = Commands::Focus {
            direction: FocusDirection::Left,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusLeft));
    }

    #[test]
    fn test_to_ipc_command_focus_right() {
        let cmd = Commands::Focus {
            direction: FocusDirection::Right,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusRight));
    }

    #[test]
    fn test_to_ipc_command_focus_up() {
        let cmd = Commands::Focus {
            direction: FocusDirection::Up,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusUp));
    }

    #[test]
    fn test_to_ipc_command_focus_down() {
        let cmd = Commands::Focus {
            direction: FocusDirection::Down,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusDown));
    }

    #[test]
    fn test_to_ipc_command_scroll_left() {
        let cmd = Commands::Scroll {
            direction: ScrollDirection::Left { pixels: 100 },
        };
        match to_ipc_command(&cmd) {
            IpcCommand::Scroll { delta } => assert_eq!(delta, -100.0),
            other => panic!("Expected Scroll command, got {:?}", other),
        }
    }

    #[test]
    fn test_to_ipc_command_scroll_right() {
        let cmd = Commands::Scroll {
            direction: ScrollDirection::Right { pixels: 150 },
        };
        match to_ipc_command(&cmd) {
            IpcCommand::Scroll { delta } => assert_eq!(delta, 150.0),
            other => panic!("Expected Scroll command, got {:?}", other),
        }
    }

    #[test]
    fn test_to_ipc_command_move_left() {
        let cmd = Commands::Move {
            direction: MoveDirection::Left,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveColumnLeft));
    }

    #[test]
    fn test_to_ipc_command_move_right() {
        let cmd = Commands::Move {
            direction: MoveDirection::Right,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveColumnRight));
    }

    #[test]
    fn test_to_ipc_command_resize() {
        let cmd = Commands::Resize { delta: 50 };
        match to_ipc_command(&cmd) {
            IpcCommand::Resize { delta } => assert_eq!(delta, 50),
            other => panic!("Expected Resize command, got {:?}", other),
        }
    }

    #[test]
    fn test_to_ipc_command_resize_negative() {
        let cmd = Commands::Resize { delta: -30 };
        match to_ipc_command(&cmd) {
            IpcCommand::Resize { delta } => assert_eq!(delta, -30),
            other => panic!("Expected Resize command, got {:?}", other),
        }
    }

    #[test]
    fn test_to_ipc_command_focus_monitor_left() {
        let cmd = Commands::FocusMonitor {
            direction: MonitorDirection::Left,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusMonitorLeft));
    }

    #[test]
    fn test_to_ipc_command_focus_monitor_right() {
        let cmd = Commands::FocusMonitor {
            direction: MonitorDirection::Right,
        };
        assert!(matches!(
            to_ipc_command(&cmd),
            IpcCommand::FocusMonitorRight
        ));
    }

    #[test]
    fn test_to_ipc_command_move_to_monitor_left() {
        let cmd = Commands::MoveToMonitor {
            direction: MonitorDirection::Left,
        };
        assert!(matches!(
            to_ipc_command(&cmd),
            IpcCommand::MoveWindowToMonitorLeft
        ));
    }

    #[test]
    fn test_to_ipc_command_move_to_monitor_right() {
        let cmd = Commands::MoveToMonitor {
            direction: MonitorDirection::Right,
        };
        assert!(matches!(
            to_ipc_command(&cmd),
            IpcCommand::MoveWindowToMonitorRight
        ));
    }

    #[test]
    fn test_to_ipc_command_query_workspace() {
        let cmd = Commands::Query {
            what: QueryType::Workspace,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryWorkspace));
    }

    #[test]
    fn test_to_ipc_command_query_focused() {
        let cmd = Commands::Query {
            what: QueryType::Focused,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryFocused));
    }

    #[test]
    fn test_to_ipc_command_query_all() {
        let cmd = Commands::Query {
            what: QueryType::All,
        };
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryAllWindows));
    }

    #[test]
    fn test_to_ipc_command_refresh() {
        let cmd = Commands::Refresh;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::Refresh));
    }

    #[test]
    fn test_to_ipc_command_apply() {
        let cmd = Commands::Apply;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::Apply));
    }

    #[test]
    fn test_to_ipc_command_reload() {
        let cmd = Commands::Reload;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::Reload));
    }

    #[test]
    fn test_to_ipc_command_stop() {
        let cmd = Commands::Stop;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::Stop));
    }

    #[test]
    fn test_to_ipc_command_toggle_pause() {
        let cmd = Commands::TogglePause;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::TogglePause));
    }

    #[test]
    fn test_to_ipc_command_panic_revert() {
        let cmd = Commands::PanicRevert;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::PanicRevert));
    }

    #[test]
    fn test_cli_alias_recover_parses_to_panic_revert() {
        let cli = Cli::try_parse_from(["leopardwm-cli", "recover"]).expect("alias should parse");
        assert!(matches!(cli.command, Commands::PanicRevert));
    }

    #[test]
    fn test_cli_alias_pause_parses_to_toggle_pause() {
        let cli = Cli::try_parse_from(["leopardwm-cli", "pause"]).expect("alias should parse");
        assert!(matches!(cli.command, Commands::TogglePause));
    }

    #[test]
    fn test_cli_alias_restore_windows_parses_to_emergency_uncloak() {
        let cli =
            Cli::try_parse_from(["leopardwm-cli", "restore-windows"]).expect("alias should parse");
        assert!(matches!(cli.command, Commands::EmergencyUncloak));
    }

    // =========================================================================
    // generate_default_config tests
    // =========================================================================

    #[test]
    fn test_generate_default_config_contains_layout_section() {
        let config = generate_default_config();
        assert!(config.contains("[layout]"));
        assert!(config.contains("gap"));
        assert!(config.contains("outer_gap"));
        assert!(config.contains("default_column_width"));
    }

    #[test]
    fn test_generate_default_config_contains_appearance_section() {
        let config = generate_default_config();
        assert!(config.contains("[appearance]"));
    }

    #[test]
    fn test_generate_default_config_contains_behavior_section() {
        let config = generate_default_config();
        assert!(config.contains("[behavior]"));
        assert!(config.contains("focus_new_windows"));
        assert!(config.contains("track_focus_changes"));
        assert!(config.contains("log_level"));
    }

    #[test]
    fn test_generate_default_config_contains_centering_mode() {
        let config = generate_default_config();
        assert!(config.contains("centering_mode"));
        assert!(config.contains("center") || config.contains("just_in_view"));
    }

    // =========================================================================
    // default_config_path tests
    // =========================================================================

    #[test]
    fn test_default_config_path_returns_some() {
        // This may return None in certain CI environments without home dirs
        // but on most systems it should return Some
        let path = default_config_path();
        // Just verify the function runs without panicking
        if let Some(p) = path {
            assert!(p.ends_with("config.toml"));
        }
    }

    #[test]
    fn test_default_config_path_contains_leopardwm() {
        if let Some(path) = default_config_path() {
            let path_str = path.to_string_lossy();
            assert!(
                path_str.contains("leopardwm"),
                "Path should contain 'leopardwm': {}",
                path_str
            );
        }
    }

    // =========================================================================
    // IPC timeout and framing tests
    // =========================================================================

    #[test]
    fn test_ipc_connect_timeout_is_reasonable() {
        // Timeout should be between 1 and 30 seconds
        assert!(IPC_CONNECT_TIMEOUT >= Duration::from_secs(1));
        assert!(IPC_CONNECT_TIMEOUT <= Duration::from_secs(30));
    }

    #[test]
    fn test_recovery_connect_timeout_is_longer_than_default_connect() {
        assert!(IPC_RECOVERY_CONNECT_TIMEOUT > IPC_CONNECT_TIMEOUT);
        assert!(IPC_RECOVERY_CONNECT_TIMEOUT <= Duration::from_secs(30));
    }

    #[test]
    fn test_default_response_timeout_is_reasonable() {
        assert!(IPC_DEFAULT_RESPONSE_TIMEOUT >= Duration::from_secs(1));
        assert!(IPC_DEFAULT_RESPONSE_TIMEOUT <= Duration::from_secs(30));
    }

    #[test]
    fn test_apply_response_timeout_is_longer_than_default() {
        assert!(IPC_APPLY_RESPONSE_TIMEOUT > IPC_DEFAULT_RESPONSE_TIMEOUT);
        assert!(IPC_APPLY_RESPONSE_TIMEOUT <= Duration::from_secs(60));
    }

    #[test]
    fn test_recovery_response_timeout_is_longer_than_default() {
        assert!(IPC_RECOVERY_RESPONSE_TIMEOUT > IPC_DEFAULT_RESPONSE_TIMEOUT);
        assert!(IPC_RECOVERY_RESPONSE_TIMEOUT <= Duration::from_secs(60));
    }

    #[test]
    fn test_shutdown_confirm_timeout_is_reasonable() {
        assert!(SHUTDOWN_CONFIRM_TIMEOUT >= Duration::from_secs(1));
        assert!(SHUTDOWN_CONFIRM_TIMEOUT <= Duration::from_secs(60));
    }

    #[test]
    fn test_shutdown_confirm_poll_interval_is_reasonable() {
        assert!(SHUTDOWN_CONFIRM_POLL_INTERVAL >= Duration::from_millis(50));
        assert!(SHUTDOWN_CONFIRM_POLL_INTERVAL <= Duration::from_secs(1));
    }

    #[test]
    fn test_command_connect_timeout_for_apply_uses_default_connect_budget() {
        assert_eq!(
            command_connect_timeout(&IpcCommand::Apply),
            IPC_CONNECT_TIMEOUT
        );
    }

    #[test]
    fn test_command_connect_timeout_for_stop_and_panic_revert_uses_recovery_budget() {
        assert_eq!(
            command_connect_timeout(&IpcCommand::Stop),
            IPC_RECOVERY_CONNECT_TIMEOUT
        );
        assert_eq!(
            command_connect_timeout(&IpcCommand::PanicRevert),
            IPC_RECOVERY_CONNECT_TIMEOUT
        );
    }

    #[test]
    fn test_command_response_timeout_for_stop_and_panic_revert() {
        assert_eq!(
            command_response_timeout(&IpcCommand::Stop),
            IPC_RECOVERY_RESPONSE_TIMEOUT
        );
        assert_eq!(
            command_response_timeout(&IpcCommand::PanicRevert),
            IPC_RECOVERY_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn test_command_response_timeout_for_apply_uses_extended_budget() {
        assert_eq!(
            command_response_timeout(&IpcCommand::Apply),
            IPC_APPLY_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn test_command_response_timeout_for_regular_command_uses_default() {
        assert_eq!(
            command_response_timeout(&IpcCommand::FocusLeft),
            IPC_DEFAULT_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn test_empty_response_parse_fails() {
        // Verify that an empty string cannot be parsed as a valid IPC response
        let result: Result<IpcResponse, _> = serde_json::from_str("");
        assert!(
            result.is_err(),
            "Empty string should not parse as IpcResponse"
        );
    }

    #[test]
    fn test_unknown_response_parse_maps_to_unknown() {
        let result: Result<IpcResponse, _> =
            serde_json::from_str(r#"{"status":"future_response","data":{"x":1}}"#);
        assert!(matches!(result, Ok(IpcResponse::Unknown)));
    }

    #[test]
    fn test_is_non_success_response_for_unknown() {
        assert!(is_non_success_response(&IpcResponse::Unknown));
        assert!(!is_non_success_response(&IpcResponse::Ok));
    }

    #[test]
    fn test_classify_pipe_probe_error_busy() {
        let err = std::io::Error::from_raw_os_error(231);
        assert_eq!(classify_pipe_probe_error(&err), Some(true));
    }

    #[test]
    fn test_classify_pipe_probe_error_not_found() {
        let err = std::io::Error::from_raw_os_error(2);
        assert_eq!(classify_pipe_probe_error(&err), Some(false));
    }

    #[test]
    fn test_classify_pipe_probe_error_unknown() {
        let err = std::io::Error::from_raw_os_error(5);
        assert_eq!(classify_pipe_probe_error(&err), None);
    }

    #[test]
    fn test_pipe_connect_retry_timeout_message_busy_only() {
        let message = pipe_connect_retry_timeout_message(Duration::from_millis(750), true, false);
        assert!(message.contains("750ms"));
        assert!(message.contains("busy"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_pipe_connect_retry_timeout_message_not_found_only() {
        let message = pipe_connect_retry_timeout_message(Duration::from_millis(500), false, true);
        assert!(message.contains("500ms"));
        assert!(message.contains("not found"));
        assert!(message.contains("leopardwm-cli run"));
    }

    #[test]
    fn test_pipe_connect_retry_timeout_message_mixed_states() {
        let message = pipe_connect_retry_timeout_message(Duration::from_millis(1000), true, true);
        assert!(message.contains("1000ms"));
        assert!(message.contains("busy"));
        assert!(message.contains("not-found"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_pipe_connect_not_found_fast_fail_message_is_actionable() {
        let message = pipe_connect_not_found_fast_fail_message(Duration::from_millis(800));
        assert!(message.contains("800ms"));
        assert!(message.contains("not found"));
        assert!(message.contains("leopardwm-cli run"));
    }

    #[test]
    fn test_safe_mode_existing_daemon_message_is_actionable() {
        let message = safe_mode_existing_daemon_message();
        assert!(message.contains("leopardwm-cli stop"));
        assert!(message.contains("leopardwm-cli run --safe-mode"));
    }

    #[test]
    fn test_error_chain_has_pipe_not_found_true() {
        let err = Err::<(), _>(std::io::Error::from_raw_os_error(2))
            .context("wrapped")
            .unwrap_err();
        assert!(error_chain_has_pipe_not_found(&err));
    }

    #[test]
    fn test_error_chain_has_pipe_not_found_false() {
        let err = Err::<(), _>(std::io::Error::from_raw_os_error(5))
            .context("wrapped")
            .unwrap_err();
        assert!(!error_chain_has_pipe_not_found(&err));
    }

    #[test]
    fn test_error_chain_has_disconnected_before_response_true() {
        let err = anyhow::anyhow!(PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE).context("wrapped");
        assert!(error_chain_has_disconnected_before_response(&err));
    }

    #[test]
    fn test_error_chain_has_disconnected_before_response_false() {
        let err = anyhow::anyhow!("some other message").context("wrapped");
        assert!(!error_chain_has_disconnected_before_response(&err));
    }

    #[test]
    fn test_error_chain_has_command_timeout_true_for_timeout_message() {
        let err = anyhow::anyhow!("Timed out waiting for daemon response after 15000ms");
        assert!(error_chain_has_command_timeout(&err));
    }

    #[test]
    fn test_error_chain_has_command_timeout_false_for_non_timeout_error() {
        let err = anyhow::anyhow!("some other failure");
        assert!(!error_chain_has_command_timeout(&err));
    }

    #[test]
    fn test_error_chain_indicates_pipe_not_found_timeout_true() {
        let err = anyhow::anyhow!(
            "Timed out after 1000ms connecting to daemon IPC pipe: the pipe was not found (daemon is likely not running). Start it with `leopardwm-cli run`."
        );
        assert!(error_chain_indicates_pipe_not_found_timeout(&err));
    }

    #[test]
    fn test_error_chain_indicates_pipe_not_found_timeout_true_for_fast_fail_message() {
        let err = anyhow::anyhow!(pipe_connect_not_found_fast_fail_message(
            Duration::from_millis(800)
        ));
        assert!(error_chain_indicates_pipe_not_found_timeout(&err));
    }

    #[test]
    fn test_error_chain_indicates_pipe_not_found_timeout_false() {
        let err = anyhow::anyhow!(
            "Timed out after 1000ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown)."
        );
        assert!(!error_chain_indicates_pipe_not_found_timeout(&err));
    }

    #[test]
    fn test_error_chain_has_connect_timeout_true() {
        let err = anyhow::anyhow!(
            "Timed out after 1000ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown)."
        );
        assert!(error_chain_has_connect_timeout(&err));
    }

    #[test]
    fn test_error_chain_has_connect_timeout_false() {
        let err = anyhow::anyhow!("Timed out waiting for daemon response after 15000ms");
        assert!(!error_chain_has_connect_timeout(&err));
    }

    #[test]
    fn test_stop_race_shutdown_message_is_actionable() {
        assert!(stop_race_shutdown_message().contains("stopping or stopped"));
        assert!(stop_race_shutdown_message().contains("leopardwm-cli status"));
    }

    #[test]
    fn test_panic_revert_not_running_message_is_actionable() {
        let message = panic_revert_not_running_message();
        assert!(message.contains("not running"));
        assert!(message.contains("leopardwm-cli emergency-uncloak"));
    }

    #[test]
    fn test_panic_revert_unconfirmed_message_is_actionable() {
        let message = panic_revert_unconfirmed_message();
        assert!(message.contains("before confirming"));
        assert!(message.contains("leopardwm-cli status"));
        assert!(message.contains("Local emergency visibility restore"));
    }

    #[test]
    fn test_panic_revert_timeout_recovery_message_is_actionable() {
        let message = panic_revert_timeout_recovery_message();
        assert!(message.contains("Timed out"));
        assert!(message.contains("Local emergency visibility restore"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_stop_timeout_recovery_message_is_actionable() {
        let message = stop_timeout_recovery_message();
        assert!(message.contains("Timed out"));
        assert!(message.contains("leopardwm-cli status"));
        assert!(message.contains("leopardwm-cli panic-revert"));
        assert!(message.contains("leopardwm-cli emergency-uncloak"));
    }

    #[test]
    fn test_apply_not_running_message_is_actionable() {
        let message = apply_not_running_message();
        assert!(message.contains("not running"));
        assert!(message.contains("leopardwm-cli run"));
    }

    #[test]
    fn test_apply_timeout_recovery_message_is_actionable() {
        let message = apply_timeout_recovery_message();
        assert!(message.contains("Timed out"));
        assert!(message.contains("leopardwm-cli panic-revert"));
        assert!(message.contains("leopardwm-cli emergency-uncloak"));
    }

    #[test]
    fn test_apply_unconfirmed_recovery_message_is_actionable() {
        let message = apply_unconfirmed_recovery_message();
        assert!(message.contains("not confirmed"));
        assert!(message.contains("Local emergency visibility restore"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_apply_error_response_recovery_message_is_actionable() {
        let message = apply_error_response_recovery_message();
        assert!(message.contains("non-success apply response"));
        assert!(message.contains("Local emergency visibility restore"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_stop_error_response_recovery_message_is_actionable() {
        let message = stop_error_response_recovery_message();
        assert!(message.contains("non-success stop response"));
        assert!(message.contains("Local emergency visibility restore"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_panic_revert_error_response_recovery_message_is_actionable() {
        let message = panic_revert_error_response_recovery_message();
        assert!(message.contains("non-success panic-revert response"));
        assert!(message.contains("Local emergency visibility restore"));
        assert!(message.contains("leopardwm-cli status"));
    }

    #[test]
    fn test_non_success_recovery_reasons_are_command_specific() {
        assert!(apply_non_success_recovery_reason().contains("apply"));
        assert!(stop_non_success_recovery_reason().contains("stop"));
        assert!(panic_revert_non_success_recovery_reason().contains("panic-revert"));
    }

    #[test]
    fn test_parse_ipc_response_line_parses_ok_response() {
        let raw = serde_json::to_string(&IpcResponse::Ok).unwrap();
        let response = parse_ipc_response_line(&raw).unwrap();
        assert!(matches!(response, IpcResponse::Ok));
    }

    #[test]
    fn test_parse_ipc_response_frame_accepts_valid_newline_terminated_response() {
        let frame = format!("{}\n", serde_json::to_string(&IpcResponse::Ok).unwrap());
        let response = parse_ipc_response_frame(frame.as_bytes(), MAX_IPC_MESSAGE_SIZE).unwrap();
        assert!(matches!(response, IpcResponse::Ok));
    }

    #[test]
    fn test_parse_ipc_response_frame_rejects_oversized_payload() {
        let oversized = vec![b'x'; MAX_IPC_MESSAGE_SIZE + 1];
        let err = parse_ipc_response_frame(&oversized, MAX_IPC_MESSAGE_SIZE).unwrap_err();
        assert!(err.to_string().contains("exceeded"));
    }

    #[test]
    fn test_parse_ipc_response_frame_rejects_non_newline_terminated_payload() {
        let frame = serde_json::to_string(&IpcResponse::Ok).unwrap();
        let err = parse_ipc_response_frame(frame.as_bytes(), MAX_IPC_MESSAGE_SIZE).unwrap_err();
        assert!(err.to_string().contains("newline-terminated"));
    }

    #[test]
    fn test_to_ipc_command_close_window() {
        let cmd = Commands::CloseWindow;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::CloseWindow));
    }

    #[test]
    fn test_to_ipc_command_toggle_floating() {
        let cmd = Commands::ToggleFloating;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::ToggleFloating));
    }

    #[test]
    fn test_to_ipc_command_toggle_fullscreen() {
        let cmd = Commands::ToggleFullscreen;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::ToggleFullscreen));
    }

    #[test]
    fn test_to_ipc_command_set_width() {
        let cmd = Commands::SetWidth { fraction: 0.5 };
        match to_ipc_command(&cmd) {
            IpcCommand::SetColumnWidth { fraction } => {
                assert!((fraction - 0.5).abs() < f64::EPSILON)
            }
            other => panic!("Expected SetColumnWidth, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_set_width_fraction_accepts_bounds() {
        assert!(validate_set_width_fraction(0.1).is_ok());
        assert!(validate_set_width_fraction(1.0).is_ok());
    }

    #[test]
    fn test_validate_set_width_fraction_rejects_out_of_range() {
        assert!(validate_set_width_fraction(0.09).is_err());
        assert!(validate_set_width_fraction(1.01).is_err());
    }

    #[test]
    fn test_validate_set_width_fraction_rejects_non_finite() {
        assert!(validate_set_width_fraction(f64::NAN).is_err());
        assert!(validate_set_width_fraction(f64::INFINITY).is_err());
    }

    #[test]
    fn test_parse_set_width_fraction_rejects_non_numeric() {
        assert!(parse_set_width_fraction("not-a-number").is_err());
    }

    #[test]
    fn test_to_ipc_command_equalize_widths() {
        let cmd = Commands::EqualizeWidths;
        assert!(matches!(
            to_ipc_command(&cmd),
            IpcCommand::EqualizeColumnWidths
        ));
    }

    #[test]
    fn test_to_ipc_command_status() {
        let cmd = Commands::Status;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryStatus));
    }

    #[test]
    fn test_generate_default_config_contains_hotkeys() {
        let config = generate_default_config();
        assert!(config.contains("[hotkeys]"));
        assert!(config.contains("close_window"));
        assert!(config.contains("toggle_floating"));
        assert!(config.contains("\"Win+Ctrl+Escape\" = \"panic_revert\""));
        assert!(config.contains("toggle_pause"));
    }

    #[test]
    fn test_generate_default_config_contains_gestures() {
        let config = generate_default_config();
        assert!(config.contains("[gestures]"));
        assert!(config.contains("enabled = true"));
    }

    #[test]
    fn test_generate_default_config_contains_snap_hints() {
        let config = generate_default_config();
        assert!(config.contains("[snap_hints]"));
    }

    // =========================================================================
    // Doctor helper tests
    // =========================================================================

    #[test]
    fn test_doctor_config_path_returns_primary() {
        let (_found, display) = doctor_config_path();
        let display_str = display.to_string_lossy();
        assert!(
            display_str.contains("leopardwm"),
            "Display path should contain leopardwm: {}",
            display_str
        );
        assert!(
            display_str.ends_with("config.toml"),
            "Display path should end with config.toml: {}",
            display_str
        );
    }

    #[test]
    fn test_validate_toml_valid() {
        let dir = std::env::temp_dir();
        let path = dir.join("leopardwm-test-valid.toml");
        fs::write(&path, "[layout]\ngap = 10\n").unwrap();
        assert!(validate_toml_file(&path).is_ok());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_validate_toml_invalid() {
        let dir = std::env::temp_dir();
        let path = dir.join("leopardwm-test-invalid.toml");
        fs::write(&path, "[layout\ngap = !!!").unwrap();
        assert!(validate_toml_file(&path).is_err());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_check_result_variants() {
        // Verify that all CheckResult variants can be constructed and printed
        let pass = CheckResult::Pass("test pass".to_string());
        let warn = CheckResult::Warn("test warn".to_string());
        let fail = CheckResult::Fail("test fail".to_string());
        // Just verify they don't panic when printed
        pass.print();
        warn.print();
        fail.print();
    }

    #[test]
    fn test_get_windows_version_does_not_panic() {
        let version = get_windows_version();
        assert!(!version.is_empty());
    }

    // =========================================================================
    // Phase 2: CLI completeness tests (Iteration 42)
    // =========================================================================

    #[test]
    fn test_config_backup_path() {
        let config = PathBuf::from("/some/path/config.toml");
        let backup = config_backup_path(&config);
        assert!(
            backup.to_string_lossy().ends_with("config.toml.bak"),
            "backup path should end with .toml.bak: {}",
            backup.display()
        );
    }

    #[test]
    fn test_config_backup_and_restore_roundtrip() {
        let dir = std::env::temp_dir().join("leopardwm-test-config-roundtrip");
        let _ = fs::create_dir_all(&dir);
        let config_path = dir.join("config.toml");
        let backup_path = config_backup_path(&config_path);

        // Write initial config
        fs::write(&config_path, "gap = 10\n").unwrap();

        // Backup
        fs::copy(&config_path, &backup_path).unwrap();
        assert!(backup_path.exists());

        // Modify config
        fs::write(&config_path, "gap = 20\n").unwrap();

        // Restore
        fs::copy(&backup_path, &config_path).unwrap();
        let restored = fs::read_to_string(&config_path).unwrap();
        assert_eq!(restored, "gap = 10\n");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_collect_logs_does_not_panic() {
        // Just verify the function runs without panicking
        // It prints to stdout, which is fine in tests
        let result = handle_collect_logs();
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_action_variants_parse() {
        // Verify ConfigAction variants can be constructed
        let _reset = ConfigAction::Reset;
        let _backup = ConfigAction::Backup;
        let _restore = ConfigAction::Restore;
    }

    // =========================================================================
    // Phase 3: Health command test (Iteration 43)
    // =========================================================================

    #[test]
    fn test_to_ipc_command_health() {
        let cmd = Commands::Health;
        assert!(matches!(to_ipc_command(&cmd), IpcCommand::HealthCheck));
    }

    // =========================================================================
    // Phase 4: Profile config tests (Iteration 44)
    // =========================================================================

    #[test]
    fn test_generate_profile_config_laptop() {
        let config = generate_profile_config("laptop");
        assert!(config.contains("laptop profile"));
        assert!(config.contains("gap = 6"));
        assert!(config.contains("default_column_width = 700"));
    }

    #[test]
    fn test_generate_profile_config_ultrawide() {
        let config = generate_profile_config("ultrawide");
        assert!(config.contains("ultrawide profile"));
        assert!(config.contains("default_column_width = 1000"));
        assert!(config.contains("just_in_view"));
    }

    #[test]
    fn test_generate_profile_config_developer() {
        let config = generate_profile_config("developer");
        assert!(config.contains("developer profile"));
        assert!(config.contains("default_column_width = 900"));
        assert!(config.contains("\"Win+Ctrl+Escape\" = \"panic_revert\""));
    }

    #[test]
    fn test_all_profile_configs_are_valid_toml() {
        for profile in &["developer", "laptop", "ultrawide"] {
            let content = generate_profile_config(profile);
            let result = content.parse::<toml::Table>();
            assert!(
                result.is_ok(),
                "Profile '{}' generates invalid TOML: {:?}",
                profile,
                result.err()
            );
        }
    }
}
