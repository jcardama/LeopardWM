//! GitHub Releases version checker.
//!
//! Polls `api.github.com` once on startup and once per day. Notifies via the
//! provided callback when a newer version is detected. No telemetry beyond
//! the bare HTTPS GET. Disabled entirely when `check_for_updates = false`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const RELEASES_API: &str = "https://api.github.com/repos/jcardama/LeopardWM/releases/latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

/// Public release URL for the GUI / tray click action.
pub const RELEASES_PAGE_URL: &str = "https://github.com/jcardama/LeopardWM/releases";

/// Spawn the background update-checker thread.
///
/// `on_update_found` runs on the worker thread when a newer release tag is
/// observed. `cancel` lets shutdown abort sleeps early.
pub fn spawn_update_checker<F>(cancel: Arc<AtomicBool>, on_update_found: F)
where
    F: Fn(String) + Send + 'static,
{
    std::thread::Builder::new()
        .name("leopardwm-update-check".to_string())
        .spawn(move || {
            interruptible_sleep(STARTUP_DELAY, &cancel);
            while !cancel.load(Ordering::SeqCst) {
                run_check_once(&on_update_found);
                interruptible_sleep(POLL_INTERVAL, &cancel);
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| warn!("Failed to spawn update checker: {}", e));
}

fn run_check_once(on_update_found: &impl Fn(String)) {
    match fetch_latest_release_tag() {
        Some(tag) => {
            let current = env!("CARGO_PKG_VERSION");
            if is_newer(&tag, current) {
                info!("Update available: {} (current: {})", tag, current);
                on_update_found(tag);
            } else {
                debug!("Up to date (latest: {}, current: {})", tag, current);
            }
        }
        None => debug!("Update check failed (network/parse error)"),
    }
}

/// Fetch the `tag_name` of the latest GitHub release. Returns `None` on any
/// network or parse failure — the caller treats this as "unknown, try again
/// tomorrow."
fn fetch_latest_release_tag() -> Option<String> {
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let body = agent
        .get(RELEASES_API)
        .set(
            "User-Agent",
            concat!("LeopardWM/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json.get("tag_name")?.as_str().map(String::from)
}

/// Compare `latest` (e.g. `v0.1.11`) against `current` (e.g. `0.1.10`).
/// Strips a leading `v` from either side. Fails closed (`false`) when either
/// side cannot be parsed as semver.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let l = semver::Version::parse(latest.trim_start_matches('v'));
    let c = semver::Version::parse(current.trim_start_matches('v'));
    match (l, c) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

fn interruptible_sleep(total: Duration, cancel: &AtomicBool) {
    let chunk = Duration::from_secs(1);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let step = remaining.min(chunk);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_basic() {
        assert!(is_newer("v0.1.11", "0.1.10"));
        assert!(is_newer("0.2.0", "v0.1.10"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn not_newer_when_same() {
        assert!(!is_newer("v0.1.10", "0.1.10"));
        assert!(!is_newer("0.1.10", "v0.1.10"));
    }

    #[test]
    fn not_newer_when_older() {
        assert!(!is_newer("v0.1.9", "0.1.10"));
        assert!(!is_newer("0.0.1", "1.0.0"));
    }

    #[test]
    fn fail_closed_on_unparseable() {
        assert!(!is_newer("garbage", "0.1.10"));
        assert!(!is_newer("v0.1.10", "not-a-version"));
        assert!(!is_newer("", ""));
    }
}
