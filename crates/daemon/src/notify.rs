//! Daemon-side toast notifications.
//!
//! Thin wrapper over the shared `platform_win32::toast` worker, fixing the
//! daemon's AppUserModelID. Used to tell the user when a window can't be managed
//! (e.g. an elevated window the non-elevated daemon is blocked from tiling).

const AUMID: &str = "jcardama.LeopardWM";
const APP_NAME: &str = "LeopardWM";

/// Register the toast identity and start the worker. Call once during startup.
pub(crate) fn init() -> anyhow::Result<()> {
    leopardwm_platform_win32::toast::init(AUMID, APP_NAME)
}

/// Queue a fire-and-forget toast (bounded worker; drops on overflow).
// Only called from the `cfg(not(test))` elevation-skip path, so it reads as
// dead code when the crate is compiled for tests.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn show_toast(title: &str, body: &str) {
    leopardwm_platform_win32::toast::show_toast(title, body);
}
