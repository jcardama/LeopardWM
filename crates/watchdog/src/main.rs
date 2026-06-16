//! LeopardWM Watchdog
//!
//! Spawns the daemon (`leopardwm.exe`) as a child process, monitors its
//! exit code, runs the panic-revert recovery path on abnormal exit, and
//! optionally auto-restarts within a crash-loop budget. Designed to be
//! invoked transparently by `lwm run`; can also be run directly by users
//! who want the supervision layer.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::GetCurrentProcess;

mod notification;

const DAEMON_BIN_NAME: &str = "leopardwm.exe";

/// Crash-loop detection: if the daemon exits abnormally
/// `MAX_CRASHES_PER_WINDOW` or more times within `CRASH_WINDOW`, the
/// watchdog gives up and exits — the user has to intervene (run
/// `lwm doctor`, check the crash log, etc.). This prevents an infinite
/// restart loop that masks a deterministic startup bug.
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_CRASHES_PER_WINDOW: usize = 3;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("leopardwm-watchdog starting");

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
    if let Err(err) = notification::init() {
        warn!(%err, "Toast notifications disabled (AUMID setup failed)");
    }

    let daemon_path = find_daemon_binary()?;
    info!(path = %daemon_path.display(), "Resolved daemon binary");

    // Forward our argv (minus argv[0]) to the daemon every restart so
    // flags like --safe-mode propagate through.
    let daemon_args: Vec<String> = std::env::args().skip(1).collect();

    let mut crashes: Vec<Instant> = Vec::new();

    loop {
        let status = spawn_daemon(&daemon_path, &daemon_args)?;

        if status.success() {
            info!(?status, "Daemon exited cleanly — watchdog stopping");
            return Ok(());
        }

        warn!(?status, "Daemon exited abnormally — running recovery");
        recover_from_crash();

        match record_crash_and_decide(&mut crashes, Instant::now(), CRASH_WINDOW, MAX_CRASHES_PER_WINDOW) {
            CrashDecision::Restart { attempt } => {
                // Fire-and-forget: the toast renders on a worker thread
                // so we don't delay the daemon restart.
                let _ = notification::show_toast(
                    "LeopardWM recovered",
                    "The daemon crashed and was restarted automatically. Your windows are visible again.",
                    notification::Severity::Info,
                );
                info!(attempt, "Restarting daemon after crash");
            }
            CrashDecision::GiveUp { count } => {
                error!(
                    count,
                    window_secs = CRASH_WINDOW.as_secs(),
                    "Crash loop detected — exiting watchdog without restart"
                );
                // Block on the toast worker so the user actually sees the
                // "disabled" message before the watchdog process exits.
                let handle = notification::show_toast(
                    "LeopardWM disabled",
                    "Repeated crashes detected. Run `lwm collect-logs` for details.",
                    notification::Severity::Warning,
                );
                let _ = handle.join();
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

fn spawn_daemon(path: &Path, args: &[String]) -> Result<ExitStatus> {
    let mut child = Command::new(path)
        .args(args)
        // The daemon writes its own log file, so null its stdout rather than
        // duplicating it into the watchdog log. stderr stays inherited so a
        // panic before the tracing subscriber inits still reaches the watchdog
        // error log.
        .stdout(std::process::Stdio::null())
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
