//! Diagnostic checks (doctor) and log collection for bug reports.

use crate::daemon_cmds::find_daemon_binary;
use crate::ipc_client::{probe_daemon_running, send_command};
use anyhow::Result;
use directories::ProjectDirs;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use std::fs;
use std::path::PathBuf;

/// Result of a single diagnostic check.
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
pub(crate) fn validate_toml_file(path: &std::path::Path) -> Result<(), String> {
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
            "No config file found. Config will be auto-created on next daemon start at: {}",
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

    // 5. Ghost-animation handle balance (only meaningful while running).
    // A non-zero balance at rest means a DWM thumbnail leaked.
    if matches!(probe_daemon_running(), Ok(true)) {
        match send_command(IpcCommand::HealthCheck).await {
            Ok(IpcResponse::HealthInfo {
                thumbnail_register_balance,
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
            }
            Ok(_) | Err(_) => {
                // Older daemon without the field, or transient IPC issue;
                // not worth failing the doctor over.
            }
        }
    }

    // 6. Admin check
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

/// Collect diagnostic logs into a text report for bug reports.
pub(crate) fn handle_collect_logs() -> Result<()> {
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
    let log_dir = leopardwm_ipc::log_dir();
    let log_path = log_dir.join("leopardwm-daemon.log");
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
    let err_log_path = log_dir.join("leopardwm-daemon.err.log");
    println!("## Daemon Error Log ({}):", err_log_path.display());
    match fs::read_to_string(&err_log_path) {
        Ok(content) if !content.trim().is_empty() => println!("{}", content),
        Ok(_) => println!("  (empty)"),
        Err(e) => println!("  (not found or unreadable: {})", e),
    }
    println!();

    // Watchdog log — captures the daemon's stdout/stderr when launched via the
    // watchdog (e.g. autostart), including panics that never reach the file log.
    let watchdog_log_path = log_dir.join("leopardwm-watchdog.log");
    println!("## Watchdog Log ({}):", watchdog_log_path.display());
    match fs::read_to_string(&watchdog_log_path) {
        Ok(content) => {
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
