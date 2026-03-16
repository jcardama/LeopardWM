//! Startup utilities: banner, crash report, and duplicate-instance detection.

use leopardwm_ipc::pipe_name_candidates;
use tracing::warn;

/// Startup banner info for display after initialization.
pub struct StartupInfo {
    pub version: String,
    pub monitor_names: Vec<String>,
    pub window_count: usize,
    pub hotkeys_registered: usize,
    pub hotkeys_requested: usize,
    pub config_path: Option<String>,
    pub config_warnings: Vec<String>,
    pub log_path: String,
    pub safe_mode: bool,
    pub no_hotkeys: bool,
    pub reduce_motion: bool,
    pub high_contrast: bool,
}

/// Format the startup banner into a string (testable without capturing stderr).
pub fn format_startup_banner(info: &StartupInfo) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(out, "LeopardWM v{}", info.version).unwrap();
    if info.monitor_names.is_empty() {
        writeln!(out, "  Monitors: 0 (fallback mode)").unwrap();
    } else {
        writeln!(
            out,
            "  Monitors: {} ({})",
            info.monitor_names.len(),
            info.monitor_names.join(", ")
        )
        .unwrap();
    }
    writeln!(out, "  Windows:  {} managed", info.window_count).unwrap();
    if info.hotkeys_registered < info.hotkeys_requested {
        writeln!(
            out,
            "  Hotkeys:  {}/{} registered ({} failed)",
            info.hotkeys_registered,
            info.hotkeys_requested,
            info.hotkeys_requested - info.hotkeys_registered
        )
        .unwrap();
    } else {
        writeln!(out, "  Hotkeys:  {} registered", info.hotkeys_registered).unwrap();
    }
    if let Some(ref path) = info.config_path {
        writeln!(out, "  Config:   {}", path).unwrap();
    } else {
        writeln!(out, "  Config:   (default — no config file found)").unwrap();
    }
    for w in &info.config_warnings {
        writeln!(out, "  Warning:  {}", w).unwrap();
    }
    writeln!(out, "  Logs:     {}", info.log_path).unwrap();
    if info.reduce_motion {
        writeln!(out, "  Motion:   reduced (animations disabled)").unwrap();
    }
    if info.high_contrast {
        writeln!(out, "  Display:  high contrast").unwrap();
    }
    if info.safe_mode {
        writeln!(out, "  Mode:     SAFE MODE (hotkeys disabled)").unwrap();
    } else if info.no_hotkeys {
        writeln!(out, "  Mode:     hotkeys disabled").unwrap();
    } else {
        writeln!(out, "  Status:   Active").unwrap();
    }
    writeln!(out).unwrap();
    out
}

/// Print the startup banner to stderr.
pub fn print_startup_banner(info: &StartupInfo) {
    eprint!("{}", format_startup_banner(info));
}

/// Format a crash report from a panic.
pub(crate) fn format_crash_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    use std::fmt::Write;
    let mut report = String::new();
    writeln!(report, "LeopardWM Crash Report").unwrap();
    writeln!(report, "=====================").unwrap();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(report, "Timestamp (unix): {}", timestamp).unwrap();
    writeln!(report, "Version: {}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(report).unwrap();

    // Panic message
    writeln!(report, "## Panic Info").unwrap();
    if let Some(msg) = info.payload().downcast_ref::<&str>() {
        writeln!(report, "Message: {}", msg).unwrap();
    } else if let Some(msg) = info.payload().downcast_ref::<String>() {
        writeln!(report, "Message: {}", msg).unwrap();
    } else {
        writeln!(report, "Message: (unknown payload type)").unwrap();
    }
    if let Some(location) = info.location() {
        writeln!(
            report,
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )
        .unwrap();
    }
    writeln!(report).unwrap();

    // Backtrace
    writeln!(report, "## Backtrace").unwrap();
    writeln!(report, "{}", std::backtrace::Backtrace::force_capture()).unwrap();

    report
}

pub(crate) const ERROR_PIPE_BUSY: i32 = 231;
pub(crate) const ERROR_FILE_NOT_FOUND: i32 = 2;

pub(crate) fn pipe_probe_error_indicates_running(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(ERROR_PIPE_BUSY) => true,
        Some(ERROR_FILE_NOT_FOUND) => false,
        _ if error.kind() == std::io::ErrorKind::NotFound => false,
        _ => true,
    }
}

pub(crate) fn pipe_probe_result_indicates_running<T>(probe_result: std::io::Result<T>) -> bool {
    match probe_result {
        Ok(_) => true,
        Err(error) => pipe_probe_error_indicates_running(&error),
    }
}

/// Check if another daemon instance is already running by probing the named pipe.
///
/// Returns `true` if the pipe exists (connected or busy). ERROR_PIPE_BUSY (231)
/// means another client is already connected — the daemon is still running.
pub(crate) async fn check_already_running() -> bool {
    for pipe_name in pipe_name_candidates() {
        let probe_result = tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe_name);
        if let Err(error) = &probe_result {
            if pipe_probe_error_indicates_running(error)
                && error.raw_os_error() != Some(ERROR_PIPE_BUSY)
            {
                warn!(
                    "Named pipe probe for {} failed with non-NotFound error ({}); assuming daemon is already running to avoid duplicate instances",
                    pipe_name,
                    error
                );
            }
        }
        if pipe_probe_result_indicates_running(probe_result.map(|_| ())) {
            return true;
        }
    }
    false
}
