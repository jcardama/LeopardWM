//! Settings runs on a dedicated thread with its own Win32 message loop.
//! An AtomicBool singleton guard prevents multiple settings windows.

mod html;
mod issue_report;
pub(crate) mod troubleshooting;
mod win32;

pub use win32::{
    push_failed_binds, push_locale, push_recorded_chord, push_reload_config, push_report_status,
    push_tray_config,
};
pub(crate) use win32::{push_health, push_snapshot_result};

use leopardwm_ipc::log_text;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use tracing::{info, warn};

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum SettingsEvent {
    /// The user saved the config (already written to disk).
    Saved,
    RequestHealth,
    CreateSnapshot(u64),
    LogFailure,
    /// The hotkey recorder started (`true`) or stopped (`false`). While
    /// recording, the keyboard hook captures the combo before it can fire its
    /// configured action.
    SetRecording(bool),
    /// The settings window closed. Used as a safety net to resume hotkeys if
    /// the window was closed mid-recording.
    Closed,
    ReportIssue {
        url: String,
        truncated: bool,
        copied: bool,
    },
}

static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

pub struct SettingsWindowHandle {
    _thread: std::thread::JoinHandle<()>,
}

impl SettingsWindowHandle {
    /// Open the settings window on a dedicated thread.
    ///
    /// `initial_section` optionally navigates to a specific tab (e.g., `"about"`).
    /// Returns `None` if a settings window is already open.
    pub fn open(
        config: Config,
        event_tx: std::sync::Weak<mpsc::Sender<SettingsEvent>>,
        initial_section: Option<&str>,
        high_contrast: bool,
        failed_binds: Vec<String>,
    ) -> Option<Self> {
        if SETTINGS_OPEN
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            info!("Settings window already open — focusing existing");
            return None;
        }

        let section = initial_section.map(String::from);
        let handle = std::thread::Builder::new()
            .name("settings-window".into())
            .spawn(move || {
                if let Err(e) = win32::run_settings_window(
                    config,
                    event_tx,
                    section.as_deref(),
                    high_contrast,
                    failed_binds,
                ) {
                    warn!("Settings window error: {}", log_text::text(e));
                }
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
            })
            .ok()?;

        Some(SettingsWindowHandle { _thread: handle })
    }
}
