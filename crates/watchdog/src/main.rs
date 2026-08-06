#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! LeopardWM Watchdog
//!
//! Spawns the daemon (`leopardwm.exe`) as a child process, monitors its
//! exit code, runs the panic-revert recovery path on abnormal exit, and
//! optionally auto-restarts within a crash-loop budget. Designed to be
//! invoked transparently by `lwm run`; can also be run directly by users
//! who want the supervision layer.

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
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
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

/// Crash-loop detection: if the daemon exits abnormally
/// `MAX_CRASHES_PER_WINDOW` or more times within `CRASH_WINDOW`, the
/// watchdog gives up and exits — the user has to intervene (run
/// `lwm doctor`, check the crash log, etc.). This prevents an infinite
/// restart loop that masks a deterministic startup bug.
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_CRASHES_PER_WINDOW: usize = 3;

fn init_tracing(log_dir: &Path) -> Result<()> {
    use tracing_subscriber::prelude::*;

    let file_appender = tracing_appender::rolling::never(log_dir, "leopardwm-watchdog.log");
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

    // Put ourselves in a Job Object with KILL_ON_JOB_CLOSE so any daemon
    // we spawn dies with us if the watchdog itself is killed (`taskkill
    // /IM leopardwm-watchdog.exe`, parent process death, etc). Without
    // this, the daemon child is orphaned and continues unsupervised.
    // Non-fatal on failure — supervision still works without this safety
    // net, just leaks orphan processes on watchdog death.
    if let Err(err) = install_kill_on_close_job() {
        warn!(%err, "Job Object kill-on-close not installed — daemon may orphan if watchdog dies");
    }

    // Register AUMID + bind it to this process so toast recovery
    // notifications can render. Non-fatal on failure — we still want to
    // supervise the daemon even if toasts can't be set up.
    if let Err(err) = leopardwm_platform_win32::toast::init(TOAST_AUMID, TOAST_APP_NAME) {
        warn!(%err, "Toast notifications disabled (AUMID setup failed)");
    }

    let daemon_path = find_daemon_binary()?;
    info!(path = %daemon_path.display(), "Resolved daemon binary");

    // Forward our argv (minus argv[0]) to the daemon every restart so
    // flags like --safe-mode propagate through.
    let daemon_args: Vec<String> = std::env::args().skip(1).collect();

    let mut crashes: Vec<Instant> = Vec::new();

    loop {
        let status = spawn_daemon(&daemon_path, &daemon_args, &log_paths.daemon_error)?;

        // Console-control path: do not recover/restart. A mid-cleanup kill
        // would otherwise spawn a fresh daemon that re-tiles and re-parks
        // windows at the sentinel right as the session is ending.
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

/// Adds a crash event, evicts stale events outside the window, and decides
/// whether to keep restarting. Pulled out for testability.
///
/// The retain check uses strict `<`: a crash exactly `window` seconds old
/// is treated as outside the window and evicted. So crashes at t=0, t=30,
/// t=60 with a 60s window count as 2 (the t=0 entry is evicted at t=60).
fn record_crash_and_decide(
    crashes: &mut Vec<Instant>,
    now: Instant,
    window: Duration,
    max_per_window: usize,
) -> CrashDecision {
    crashes.retain(|t| now.duration_since(*t) < window);
    crashes.push(now);
    if crashes.len() >= max_per_window {
        CrashDecision::GiveUp { count: crashes.len() }
    } else {
        CrashDecision::Restart { attempt: crashes.len() }
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
    // Only call uncloak — `restore_maximizebox_panic_recovery()` reads
    // a process-local set populated by the daemon, so calling it from
    // the watchdog process is a no-op. The daemon's own panic hook
    // handles maximizebox restore in-process when a Rust panic fires;
    // a hard kill (taskkill /F) skips that and leaves maximizebox state
    // unrestored — the user can run `lwm panic-revert` if needed.
    info!("Running emergency uncloak");
    leopardwm_platform_win32::uncloak_all_visible_windows();
}

fn install_kill_on_close_job() -> Result<()> {
    unsafe {
        let job =
            CreateJobObjectW(None, None).context("CreateJobObjectW failed")?;
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
        // The HANDLE is Copy (no Drop), so simply letting it go out of
        // scope here does not call CloseHandle. The OS keeps the handle
        // in our process table until the watchdog exits, which is exactly
        // when we want the KILL_ON_JOB_CLOSE cascade to fire.
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

/// Fallback handler for console-attached debug or manual launches.
///
/// For CTRL_CLOSE / CTRL_LOGOFF / CTRL_SHUTDOWN, returning from the handler
/// terminates the process immediately. Park forever instead (mirroring tokio's
/// windows signal driver) so the process stays alive for the OS grace period
/// and the supervised daemon can finish cleanup before the Job Object handle
/// closes and KILL_ON_JOB_CLOSE fires.
///
/// For CTRL_C / CTRL_BREAK, return TRUE so the default ExitProcess handler does
/// not run — the watchdog must keep supervising while the daemon restores.
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
        // Anchor test instants to a single base so durations are exact.
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
            // Each crash is a full window apart — none should ever accumulate.
            let decision = record_crash_and_decide(
                &mut crashes,
                t(i * 120),
                Duration::from_secs(60),
                3,
            );
            assert_eq!(decision, CrashDecision::Restart { attempt: 1 });
        }
    }
}
