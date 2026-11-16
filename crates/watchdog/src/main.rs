#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Supervises the daemon, restores hidden windows after abnormal exits, and bounds crash restarts.

use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use windows::core::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// AppUserModelID for the watchdog's recovery toasts (distinct from the
/// daemon's so the two register independently).
const TOAST_AUMID: &str = "jcardama.LeopardWM.Watchdog";
const TOAST_APP_NAME: &str = "LeopardWM";

const DAEMON_BIN_NAME: &str = "leopardwm.exe";
const DAEMON_ERROR_LOG_NAME: &str = "leopardwm-daemon.err.log";
const WATCHDOG_ERROR_LOG_NAME: &str = "leopardwm-watchdog.err.log";

struct WatchdogLogPaths {
    dir: PathBuf,
    daemon_error: PathBuf,
    watchdog_error: PathBuf,
}

impl WatchdogLogPaths {
    fn new() -> Self {
        let dir = leopardwm_ipc::log_dir();
        Self {
            daemon_error: dir.join(DAEMON_ERROR_LOG_NAME),
            watchdog_error: dir.join(WATCHDOG_ERROR_LOG_NAME),
            dir,
        }
    }
}

/// Set by the console control handler so the supervise loop exits instead of
/// restarting the daemon after a console-close / Ctrl+C kill mid-cleanup.
static CONSOLE_CTRL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Bound crash restarts so deterministic startup failures cannot loop indefinitely.
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_CRASHES_PER_WINDOW: usize = 3;

fn init_tracing(log_dir: &Path) -> Result<()> {
    use tracing_subscriber::prelude::*;

    let file_appender = std::sync::Mutex::new(
        leopardwm_ipc::bounded_log::BoundedLogFile::open(
            log_dir,
            "leopardwm-watchdog.log",
            leopardwm_ipc::bounded_log::LOG_FILE_MAX_BYTES,
        )
        .context("Failed to open watchdog log")?,
    );
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_appender),
        )
        .try_init()
        .map_err(|err| anyhow::anyhow!("Failed to set tracing subscriber: {err}"))
}

fn append_error(path: &Path, message: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{message}")
}

fn install_panic_hook(error_log_path: PathBuf) {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = append_error(&error_log_path, &format!("watchdog panic: {panic_info}"));
        previous_hook(panic_info);
    }));
}

fn main() -> ExitCode {
    let log_paths = WatchdogLogPaths::new();
    if let Err(err) = fs::create_dir_all(&log_paths.dir).context("Failed to create log directory") {
        let message = format!("watchdog fatal error: {err:#}");
        eprintln!("{message}");
        let _ = append_error(&log_paths.watchdog_error, &message);
        return ExitCode::FAILURE;
    }

    install_panic_hook(log_paths.watchdog_error.clone());

    match run(&log_paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "Watchdog fatal error");
            let message = format!("watchdog fatal error: {err:#}");
            eprintln!("{message}");
            let _ = append_error(&log_paths.watchdog_error, &message);
            ExitCode::FAILURE
        }
    }
}

fn run(log_paths: &WatchdogLogPaths) -> Result<()> {
    fs::File::create(&log_paths.daemon_error).context("Failed to reset daemon error log")?;
    init_tracing(&log_paths.dir)?;

    info!("leopardwm-watchdog starting");

    // Release builds use the Windows GUI subsystem and allocate no console.
    // Keep this fallback for console-attached debug or manual launches so the
    // watchdog survives long enough for the daemon's restore-first cleanup.
    if let Err(err) = install_console_ctrl_handler() {
        warn!(%err, "Console control fallback not installed — console close may kill daemon before cleanup");
    }

    // Kill daemon children with the watchdog to avoid leaving an unsupervised daemon.
    if let Err(err) = install_kill_on_close_job() {
        warn!(%err, "Job Object kill-on-close not installed — daemon may orphan if watchdog dies");
    }

    // Toast setup must not prevent daemon supervision.
    if let Err(err) = leopardwm_platform_win32::toast::init(TOAST_AUMID, TOAST_APP_NAME) {
        warn!(%err, "Toast notifications disabled (AUMID setup failed)");
    }

    let daemon_path = find_daemon_binary()?;
    info!(path = %daemon_path.display(), "Resolved daemon binary");

    let daemon_args: Vec<String> = std::env::args().skip(1).collect();

    let mut crashes: Vec<Instant> = Vec::new();

    loop {
        let status = spawn_daemon(&daemon_path, &daemon_args, &log_paths.daemon_error)?;

        // Restarting during console shutdown would re-tile and re-park windows during cleanup.
        if CONSOLE_CTRL_RECEIVED.load(Ordering::SeqCst) {
            info!(
                ?status,
                "Daemon exited after console control — watchdog stopping without restart"
            );
            return Ok(());
        }

        if status.success() {
            info!(?status, "Daemon exited cleanly — watchdog stopping");
            return Ok(());
        }

        warn!(?status, "Daemon exited abnormally — running recovery");
        recover_from_crash();

        match record_crash_and_decide(
            &mut crashes,
            Instant::now(),
            CRASH_WINDOW,
            MAX_CRASHES_PER_WINDOW,
        ) {
            CrashDecision::Restart { attempt } => {
                // Fire-and-forget: the toast renders on the shared worker
                // thread so we don't delay the daemon restart.
                leopardwm_platform_win32::toast::show_toast(
                    "LeopardWM recovered",
                    "The daemon crashed and was restarted automatically. Your windows are visible again.",
                );
                info!(attempt, "Restarting daemon after crash");
            }
            CrashDecision::GiveUp { count } => {
                // Render synchronously so the user actually sees the "disabled"
                // message before the watchdog process exits.
                leopardwm_platform_win32::toast::show_toast_blocking(
                    "LeopardWM disabled",
                    "Repeated crashes detected. Run `lwm collect-logs` for details.",
                );
                return Err(anyhow::anyhow!(
                    "{} daemon crashes within {}s — watchdog refusing to restart further",
                    count,
                    CRASH_WINDOW.as_secs()
                ));
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CrashDecision {
    Restart { attempt: usize },
    GiveUp { count: usize },
}

/// A crash exactly one window old is excluded from the restart budget.
fn record_crash_and_decide(
    crashes: &mut Vec<Instant>,
    now: Instant,
    window: Duration,
    max_per_window: usize,
) -> CrashDecision {
    crashes.retain(|t| now.duration_since(*t) < window);
    crashes.push(now);
    if crashes.len() >= max_per_window {
        CrashDecision::GiveUp {
            count: crashes.len(),
        }
    } else {
        CrashDecision::Restart {
            attempt: crashes.len(),
        }
    }
}

fn find_daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let dir = exe
        .parent()
        .context("watchdog binary has no parent directory")?;
    let candidate = dir.join(DAEMON_BIN_NAME);
    if !candidate.exists() {
        anyhow::bail!(
            "daemon binary not found alongside watchdog at {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn spawn_daemon(path: &Path, args: &[String], error_log_path: &Path) -> Result<ExitStatus> {
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(error_log_path)
        .context("failed to open daemon error log")?;
    let mut child = Command::new(path)
        .args(args)
        // The daemon writes its own tracing log. Keep stdout nulled and retain
        // bootstrap stderr and early panics across every supervised restart.
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("failed to spawn {}", path.display()))?;
    child.wait().context("failed to wait on daemon child")
}

fn recover_from_crash() {
    // Maximize-box recovery uses daemon-local state unavailable to the watchdog.
    info!("Running emergency uncloak");
    leopardwm_platform_win32::uncloak_all_visible_windows();
}

fn install_kill_on_close_job() -> Result<()> {
    unsafe {
        let job = CreateJobObjectW(None, None).context("CreateJobObjectW failed")?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .context("SetInformationJobObject failed")?;
        AssignProcessToJobObject(job, GetCurrentProcess())
            .context("AssignProcessToJobObject failed")?;
        // HANDLE has no Drop, so the job stays open until process exit,
        // when kill-on-close should fire.
        let _ = job;
    }
    Ok(())
}

fn install_console_ctrl_handler() -> Result<()> {
    unsafe {
        SetConsoleCtrlHandler(Some(console_ctrl_handler), true)
            .context("SetConsoleCtrlHandler failed")?;
    }
    Ok(())
}

/// Park on console close/logoff/shutdown to preserve the OS cleanup grace period.
/// Swallow Ctrl+C/Ctrl+Break so the default handler cannot terminate supervision early.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            CONSOLE_CTRL_RECEIVED.store(true, Ordering::SeqCst);
            loop {
                std::thread::park();
            }
        }
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            CONSOLE_CTRL_RECEIVED.store(true, Ordering::SeqCst);
            BOOL(1)
        }
        _ => BOOL(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        thread_local! {
            static BASE: Instant = Instant::now();
        }
        BASE.with(|b| *b + Duration::from_secs(secs))
    }

    #[test]
    fn first_crash_returns_restart() {
        let mut crashes = Vec::new();
        let decision = record_crash_and_decide(&mut crashes, t(0), Duration::from_secs(60), 3);
        assert_eq!(decision, CrashDecision::Restart { attempt: 1 });
    }

    #[test]
    fn third_crash_in_window_gives_up() {
        let mut crashes = Vec::new();
        record_crash_and_decide(&mut crashes, t(0), Duration::from_secs(60), 3);
        record_crash_and_decide(&mut crashes, t(10), Duration::from_secs(60), 3);
        let decision = record_crash_and_decide(&mut crashes, t(20), Duration::from_secs(60), 3);
        assert_eq!(decision, CrashDecision::GiveUp { count: 3 });
    }

    #[test]
    fn crashes_outside_window_dont_count() {
        let mut crashes = Vec::new();
        record_crash_and_decide(&mut crashes, t(0), Duration::from_secs(60), 3);
        record_crash_and_decide(&mut crashes, t(30), Duration::from_secs(60), 3);
        // At t=85: t=0 is 85s old (evicted, > 60s window); t=30 is 55s old (kept).
        let decision = record_crash_and_decide(&mut crashes, t(85), Duration::from_secs(60), 3);
        assert_eq!(decision, CrashDecision::Restart { attempt: 2 });
    }

    #[test]
    fn well_separated_crashes_always_restart() {
        let mut crashes = Vec::new();
        for i in 0..10 {
            let decision =
                record_crash_and_decide(&mut crashes, t(i * 120), Duration::from_secs(60), 3);
            assert_eq!(decision, CrashDecision::Restart { attempt: 1 });
        }
    }
}
