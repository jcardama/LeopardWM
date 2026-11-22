use crate::daemon_cmds::find_daemon_binary;
use crate::ipc_client::{probe_daemon_running, send_command};
use anyhow::Result;
use directories::ProjectDirs;
use leopardwm_ipc::{
    DaemonLogStatus, ElevationBlockReason, ElevationBlockedWindow, IpcCommand, IpcResponse,
    NativeSwipeStatus,
};
pub(crate) use leopardwm_platform_win32::get_windows_version;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String),
}

pub(crate) fn native_swipes_check(status: Option<&NativeSwipeStatus>) -> CheckResult {
    match status {
        Some(NativeSwipeStatus::Off) => {
            CheckResult::Pass("Native three-finger swipes: off (default)".to_string())
        }
        Some(NativeSwipeStatus::Active) => CheckResult::Pass(
            "Native three-finger swipes: active (Raw Input registered)".to_string(),
        ),
        Some(NativeSwipeStatus::Inactive { reason }) => CheckResult::Warn(format!(
            "Native three-finger swipes are enabled but inactive: {reason}"
        )),
        Some(NativeSwipeStatus::Unknown) => CheckResult::Warn(
            "Native three-finger swipe status not recognized by this CLI".to_string(),
        ),
        None => CheckResult::Warn(
            "Native three-finger swipe status unavailable (daemon did not report it; older daemon)"
                .to_string(),
        ),
    }
}

pub(crate) fn daemon_log_path(
    status: Option<&DaemonLogStatus>,
    running: Option<bool>,
    default: &Path,
) -> (PathBuf, &'static str) {
    match status {
        Some(
            DaemonLogStatus::Writing { path }
            | DaemonLogStatus::OpenFailed { path, .. }
            | DaemonLogStatus::WriteFailed { path, .. },
        ) => (PathBuf::from(path), "reported by the running daemon"),
        _ if running == Some(false) => (
            default.to_path_buf(),
            "default path because the daemon isn't running",
        ),
        _ if running.is_none() => (
            default.to_path_buf(),
            "default path because the daemon state could not be checked",
        ),
        _ => (
            default.to_path_buf(),
            "default path because the daemon didn't report one",
        ),
    }
}

/// Configured `behavior.log_level`, not the running daemon's; a missing file, key, or unknown
/// value means info, and an existing but unreadable file is unknown.
pub(crate) fn configured_log_level(config: Option<(PathBuf, io::Result<String>)>) -> String {
    const LEVELS: [(&str, &str); 5] = [
        ("trace", "everything is written"),
        ("debug", "debug, info, warnings and errors are written"),
        ("info", "info, warnings and errors are written"),
        ("warn", "only warnings and errors are written"),
        ("error", "only errors are written"),
    ];
    let text = match config {
        Some((path, Err(error))) => {
            return format!("unknown (could not read {}: {error})", path.display());
        }
        Some((_, Ok(text))) => Some(text),
        None => None,
    };
    let configured = text
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            let level = table.get("behavior")?.get("log_level")?.as_str()?;
            LEVELS
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(level))
        });
    match configured {
        Some((name, writes)) => format!("{name} ({writes})"),
        None => "info (default; no recognized behavior.log_level in the config file)".to_string(),
    }
}

pub(crate) fn daemon_log_check(
    status: Option<&DaemonLogStatus>,
    modified: Option<SystemTime>,
    now: SystemTime,
    uptime_seconds: Option<u64>,
    log_level: &str,
) -> CheckResult {
    match status {
        Some(DaemonLogStatus::OpenFailed { path, error }) => {
            let message = format!("Daemon log: cannot open {path}: {error}");
            CheckResult::Fail(message)
        }
        Some(DaemonLogStatus::WriteFailed { path, error }) => {
            let message = format!("Daemon log: cannot write {path}: {error}");
            CheckResult::Fail(message)
        }
        Some(DaemonLogStatus::Writing { path }) => {
            // Allow 60 seconds for logging before AppState.start_time and uptime rounding.
            let start =
                uptime_seconds.and_then(|uptime| now.checked_sub(Duration::from_secs(uptime)));
            let stale = start
                .zip(modified)
                .and_then(|(start, modified)| start.duration_since(modified).ok())
                .is_some_and(|age| age > Duration::from_secs(60));
            if stale {
                let message = format!(
                    "Daemon log: {path}; the daemon has not written to it since it started"
                );
                CheckResult::Warn(message)
            } else if modified.is_none() {
                let message = format!("Daemon log: writing {path}; file modified time unavailable");
                CheckResult::Warn(message)
            } else {
                CheckResult::Pass(format!(
                    "Daemon log: writing {path}; configured log level: {log_level}"
                ))
            }
        }
        Some(DaemonLogStatus::Unknown) => {
            CheckResult::Warn("Daemon log status not recognized by this CLI".into())
        }
        None => CheckResult::Warn(
            concat!(
                "Daemon log status unavailable (daemon not running, ",
                "health unavailable, or older daemon)"
            )
            .into(),
        ),
    }
}

fn print_daemon_log_check(status: Option<&DaemonLogStatus>, uptime_seconds: Option<u64>) {
    let (path, _) = daemon_log_path(
        status,
        Some(uptime_seconds.is_some()),
        &leopardwm_ipc::log_dir().join("leopardwm-daemon.log"),
    );
    daemon_log_check_from_paths(
        status,
        uptime_seconds,
        &path,
        doctor_config_path().0.as_deref(),
    )
    .print();
}

fn daemon_log_check_from_paths(
    status: Option<&DaemonLogStatus>,
    uptime_seconds: Option<u64>,
    log_path: &Path,
    config_path: Option<&Path>,
) -> CheckResult {
    let modified = fs::metadata(log_path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let config = config_path.map(|path| {
        let text = fs::read_to_string(path);
        (path.to_path_buf(), text)
    });
    daemon_log_check(
        status,
        modified,
        SystemTime::now(),
        uptime_seconds,
        &configured_log_level(config),
    )
}

impl CheckResult {
    pub(crate) fn print(&self) {
        self.write_to(&mut io::stdout())
            .expect("failed printing to stdout");
    }

    fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        match self {
            CheckResult::Pass(msg) => writeln!(out, "[PASS] {}", msg),
            CheckResult::Warn(msg) => writeln!(out, "[WARN] {}", msg),
            CheckResult::Fail(msg) => writeln!(out, "[FAIL] {}", msg),
        }
    }
}

/// Returns the first existing config path, if any, and a display path that falls back to the
/// primary default.
pub(crate) fn doctor_config_path() -> (Option<PathBuf>, PathBuf) {
    let primary = ProjectDirs::from("", "", "leopardwm")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

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

pub(crate) fn validate_toml_file(path: &std::path::Path) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|e| format!("Cannot read file: {}", e))?;
    content
        .parse::<toml::Table>()
        .map_err(|e| format!("Invalid TOML: {}", e))?;
    Ok(())
}

pub(crate) fn format_integrity_rid(rid: Option<u32>) -> String {
    match rid {
        Some(leopardwm_platform_win32::INTEGRITY_MEDIUM) => "Medium".to_string(),
        Some(leopardwm_platform_win32::INTEGRITY_HIGH) => "High".to_string(),
        Some(rid) => format!("0x{rid:X}"),
        None => "unavailable".to_string(),
    }
}

pub(crate) fn format_integrity_line(label: &str, rid: Option<u32>) -> String {
    format!("{label} integrity: {}", format_integrity_rid(rid))
}

pub(crate) fn integrity_check(label: &str, rid: Option<u32>) -> CheckResult {
    let line = format_integrity_line(label, rid);
    if rid.is_some() {
        CheckResult::Pass(line)
    } else {
        CheckResult::Warn(line)
    }
}

fn format_blocked_record(record: &ElevationBlockedWindow) -> String {
    let hwnd = format!("{:#x}", record.hwnd);
    match record.reason {
        ElevationBlockReason::HigherIntegrity => format!(
            "\"{}\" (hwnd {hwnd}, higher integrity; running LeopardWM elevated can help, but is not guaranteed if the target is System)",
            record.title
        ),
        ElevationBlockReason::Protected => format!(
            "\"{}\" (hwnd {hwnd}, protected/access-denied/unreadable token; elevation may not help)",
            record.title
        ),
        ElevationBlockReason::Unknown => {
            format!("\"{}\" (hwnd {hwnd}, unknown admission-time reason)", record.title)
        }
    }
}

fn format_nonempty_blocked_records(records: &[ElevationBlockedWindow]) -> String {
    format!(
        "{} window(s) currently recorded as privilege-blocked at admission (snapshot, not a live reclassification): {}",
        records.len(),
        records
            .iter()
            .map(format_blocked_record)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn format_legacy_blocked_windows(legacy: &[(u64, String)]) -> String {
    format!(
        "{} window(s) currently recorded as privilege-blocked at admission (snapshot, not a live reclassification; admission-time reason unavailable): {}",
        legacy.len(),
        legacy
            .iter()
            .map(|(hwnd, title)| format!("\"{title}\" (hwnd {hwnd:#x})"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(crate) fn blocked_windows_check(
    records: Option<&[ElevationBlockedWindow]>,
    legacy: &[(u64, String)],
) -> CheckResult {
    match records {
        Some([]) => CheckResult::Pass(
            "No privilege-blocked windows currently recorded by the daemon".to_string(),
        ),
        Some(records) => CheckResult::Warn(format_nonempty_blocked_records(records)),
        None if legacy.is_empty() => CheckResult::Pass(
            "No privilege-blocked windows currently recorded by the daemon".to_string(),
        ),
        None => CheckResult::Warn(format_legacy_blocked_windows(legacy)),
    }
}

pub(crate) async fn handle_doctor() -> Result<()> {
    println!("LeopardWM Doctor");
    println!("===============");

    match find_daemon_binary() {
        Some(path) => CheckResult::Pass(format!("Daemon binary found: {}", path.display())),
        None => CheckResult::Fail(
            "Daemon binary not found. Run 'cargo build --release' to build.".to_string(),
        ),
    }
    .print();

    let (found_path, display_path) = doctor_config_path();
    match &found_path {
        Some(path) => CheckResult::Pass(format!("Config file exists: {}", path.display())),
        None => CheckResult::Warn(format!(
            "No config file found. Config will be auto-created on next daemon start at: {}",
            display_path.display()
        )),
    }
    .print();

    if let Some(ref path) = found_path {
        match validate_toml_file(path) {
            Ok(()) => CheckResult::Pass("Config file is valid TOML".to_string()),
            Err(e) => CheckResult::Fail(format!("Config file has errors: {}", e)),
        }
        .print();
    }

    match probe_daemon_running() {
        Ok(true) => {
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

    // A non-zero thumbnail balance at rest means a DWM thumbnail leaked.
    let mut printed_daemon_integrity = false;
    let mut printed_daemon_log = false;
    let mut blocked_windows_result = None;
    if matches!(probe_daemon_running(), Ok(true)) {
        match send_command(IpcCommand::HealthCheck).await {
            Ok(IpcResponse::HealthInfo {
                thumbnail_register_balance,
                elevation_blocked_windows,
                daemon_integrity,
                elevation_blocked_records,
                native_swipes,
                daemon_log,
                uptime_seconds,
                ..
            }) => {
                if thumbnail_register_balance == 0 {
                    CheckResult::Pass(
                        "Ghost-animation thumbnail balance is 0 (no leak)".to_string(),
                    )
                } else {
                    CheckResult::Warn(format!(
                        "Ghost-animation thumbnail balance is {} (expected 0 at rest; possible leak if no animation is running)",
                        thumbnail_register_balance
                    ))
                }
                .print();

                native_swipes_check(native_swipes.as_ref()).print();
                print_daemon_log_check(daemon_log.as_ref(), Some(uptime_seconds));
                printed_daemon_log = true;
                integrity_check("Daemon", daemon_integrity).print();
                printed_daemon_integrity = true;
                blocked_windows_result = Some(blocked_windows_check(
                    elevation_blocked_records.as_deref(),
                    &elevation_blocked_windows,
                ));
            }
            Ok(other) => {
                CheckResult::Warn(format!(
                    "Daemon health payload is unavailable (unexpected response: {:?}); daemon integrity and blocked-window diagnostics are incomplete",
                    other
                ))
                .print();
            }
            Err(e) => {
                CheckResult::Warn(format!(
                    "Daemon health query failed: {e}; daemon integrity and blocked-window diagnostics are incomplete"
                ))
                .print();
            }
        }
    }
    if !printed_daemon_log {
        print_daemon_log_check(None, None);
    }
    if !printed_daemon_integrity {
        integrity_check("Daemon", None).print();
    }
    integrity_check("CLI", leopardwm_platform_win32::current_process_integrity()).print();
    if let Some(blocked) = blocked_windows_result {
        blocked.print();
    }

    let version = get_windows_version();
    CheckResult::Pass(format!("Windows version: {}", version)).print();

    println!();
    Ok(())
}

pub(crate) async fn handle_collect_logs() -> Result<()> {
    use leopardwm_ipc::diagnostic_snapshot::{self, ConfigLoadStatus, Monitor, SnapshotInput};
    let version = env!("CARGO_PKG_VERSION").to_string();
    let integrity = leopardwm_platform_win32::current_process_integrity();
    let reason = match probe_daemon_running() {
        Ok(true) => match send_command(IpcCommand::WriteDiagnosticSnapshot {
            requester_version: version.clone(),
            requester_integrity: integrity,
        })
        .await
        {
            Ok(IpcResponse::DiagnosticSnapshotWritten { path }) => {
                println!("{path}");
                return Ok(());
            }
            _ => "daemon does not support snapshot capture or the request failed",
        },
        Ok(false) => "daemon not running",
        Err(_) => "daemon state could not be checked",
    };
    let (found, config_path) = doctor_config_path();
    let (config_status, config_text) = match found {
        None => (ConfigLoadStatus::NotFound, None),
        Some(path) => match fs::read_to_string(path) {
            Ok(text) => (ConfigLoadStatus::Loaded, Some(text)),
            Err(error) => (ConfigLoadStatus::ReadFailed(error.to_string()), None),
        },
    };
    let input = SnapshotInput {
        requester: "CLI".into(),
        requester_version: version,
        requester_integrity: integrity,
        daemon_version: None,
        daemon_integrity: None,
        daemon_uptime: None,
        unavailable_reason: reason.into(),
        windows_version: get_windows_version(),
        monitors: leopardwm_platform_win32::enumerate_monitors()
            .ok()
            .map(|monitors| {
                monitors
                    .into_iter()
                    .map(|m| Monitor {
                        device: m.device_name,
                        x: m.rect.x,
                        y: m.rect.y,
                        width: m.rect.width,
                        height: m.rect.height,
                        scale: m.scale_factor,
                    })
                    .collect()
            }),
        health: None,
        config_path,
        config_status,
        config_text,
        workspaces: None,
        known_titles: Vec::new(),
        userprofile: std::env::var("USERPROFILE").ok(),
        log_dir: leopardwm_ipc::log_dir(),
        timestamp: SystemTime::now(),
    };
    let path = diagnostic_snapshot::write(&input, &leopardwm_ipc::diagnostics_dir())?;
    println!("{}", path.display());
    println!("Daemon data unavailable: {reason}.");
    Ok(())
}
