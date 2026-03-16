//! Settings window for LeopardWM daemon.
//!
//! Native Win32 settings window using the `windows` crate.
//! Tabbed form with one tab per config section. Runs on a dedicated thread
//! with its own message loop. An `AtomicBool` singleton guard prevents
//! multiple windows.

mod html;
mod win32;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use tracing::{info, warn};

use crate::config::Config;

/// Events emitted by the settings window back to the daemon.
#[derive(Debug, Clone)]
pub enum SettingsEvent {
    /// The user saved the config (already written to disk).
    Saved,
}

/// Singleton guard — only one settings window at a time.
static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

/// Handle to the settings window thread.
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
        event_tx: mpsc::Sender<SettingsEvent>,
        initial_section: Option<&str>,
        high_contrast: bool,
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
                if let Err(e) =
                    win32::run_settings_window(config, event_tx, section.as_deref(), high_contrast)
                {
                    warn!("Settings window error: {}", e);
                }
                SETTINGS_OPEN.store(false, Ordering::SeqCst);
            })
            .ok()?;

        Some(SettingsWindowHandle { _thread: handle })
    }
}
