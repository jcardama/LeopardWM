//! Diagnostic checks (doctor) and log collection for bug reports.

use crate::daemon_cmds::find_daemon_binary;
use crate::ipc_client::{probe_daemon_running, send_command};
use anyhow::Result;
use directories::ProjectDirs;
use leopardwm_ipc::{ElevationBlockReason, ElevationBlockedWindow, IpcCommand, IpcResponse};
use std::fs;
use std::path::{Path, PathBuf};

/// Result of a single diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String),
}

impl CheckResult {
    pub(crate) fn print(&self) {
        match self {
            CheckResult::Pass(msg) => println!("[PASS] {}", msg),
            CheckResult::Warn(msg) => println!("[WARN] {}", msg),
            CheckResult::Fail(msg) => println!("[FAIL] {}", msg),
        }
    }
}

/// Get the config file path (first one that exists, or the primary default).
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

/// Validate that a file contains valid TOML.
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

/// Get the Windows version string.
pub(crate) fn get_windows_version() -> String {
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
    let mut blocked_windows_result = None;
    if matches!(probe_daemon_running(), Ok(true)) {
        match send_command(IpcCommand::HealthCheck).await {
            Ok(IpcResponse::HealthInfo {
                thumbnail_register_balance,
                elevation_blocked_windows,
                daemon_integrity,
                elevation_blocked_records,
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

/// Collect diagnostic logs into a text report for bug reports.
pub(crate) fn handle_collect_logs() -> Result<()> {
    println!("LeopardWM Log Collection");
    println!("=======================\n");

    println!("## Environment");
    println!("OS: {}", get_windows_version());
    println!("CLI Version: {}", env!("CARGO_PKG_VERSION"));
    println!();

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

    let log_dir = leopardwm_ipc::log_dir();
    print!(
        "{}",
        format_file_section(
            "Daemon Log",
            &log_dir.join("leopardwm-daemon.log"),
            Some(100),
        )
    );
    println!();

    let err_log_path = log_dir.join("leopardwm-daemon.err.log");
    println!("## Daemon Error Log ({}):", err_log_path.display());
    match fs::read_to_string(&err_log_path) {
        Ok(content) if !content.trim().is_empty() => println!("{}", content),
        Ok(_) => println!("  (empty)"),
        Err(e) => println!("  (not found or unreadable: {})", e),
    }
    println!();

    // Watchdog tracing log. Daemon bootstrap stderr and early panics are kept
    // separately in the daemon error log above.
    let watchdog_log_path = log_dir.join("leopardwm-watchdog.log");
    print!(
        "{}",
        format_file_section("Watchdog Log", &watchdog_log_path, Some(100))
    );
    println!();

    let watchdog_err_log_path = log_dir.join("leopardwm-watchdog.err.log");
    println!(
        "## Watchdog Error Log ({}):",
        watchdog_err_log_path.display()
    );
    match fs::read_to_string(&watchdog_err_log_path) {
        Ok(content) if !content.trim().is_empty() => println!("{}", content),
        Ok(_) => println!("  (empty)"),
        Err(e) => println!("  (not found or unreadable: {})", e),
    }
    println!();

    let capture_path = log_dir.join(leopardwm_ipc::GESTURE_CAPTURE_LOG_FILE);
    print!(
        "{}",
        format_file_section("Gesture Capture", &capture_path, None)
    );
    println!("  Note: this is the full capture file, not a last-100 tail.");
    println!(
        "  Presence of this file does not mean a capture is running now. Capture is startup-only and default off."
    );
    println!(
        "  For touchpad gesture reports, share this section or the capture file itself rather than the tailed daemon log."
    );
    println!();

    println!("## Daemon Binary:");
    match find_daemon_binary() {
        Some(path) => println!("  Found: {}", path.display()),
        None => println!("  Not found"),
    }

    println!("\n---");
    println!("Copy the above output and attach it to your bug report.");
    Ok(())
}

/// Format a log file as a collect-logs section. `tail_lines = None` dumps the
/// whole file so dedicated capture reports are not truncated to last-100.
pub(crate) fn format_file_section(heading: &str, path: &Path, tail_lines: Option<usize>) -> String {
    let mut out = format!("## {heading} ({}):\n", path.display());
    match fs::read_to_string(path) {
        Ok(content) => {
            if let Some(n) = tail_lines {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(n);
                for line in &lines[start..] {
                    out.push_str(line);
                    out.push('\n');
                }
                if start > 0 {
                    out.push_str(&format!("  ... ({} earlier lines omitted)\n", start));
                }
            } else if content.is_empty() {
                out.push_str("  (empty)\n");
            } else {
                out.push_str(&content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        Err(e) => out.push_str(&format!("  (not found or unreadable: {e})\n")),
    }
    out
}
