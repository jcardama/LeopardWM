//! System tray icon management for LeopardWM daemon.
//!
//! Provides a system tray icon with a context menu for common operations:
//! - Refresh windows
//! - Reload configuration
//! - Exit daemon
//!
//! The tray icon and its hidden notification window live on a dedicated thread
//! that runs a Win32 message pump. This is required for the right-click context
//! menu to appear — the `tray-icon` crate needs `WM_RBUTTONUP` and related
//! shell notification messages to be dispatched on the owning thread.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use thiserror::Error;
use tracing::{debug, info};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    TrayIconBuilder,
};

/// Minimal Win32 FFI for the tray icon message pump.
///
/// Only the functions needed to run a message loop and signal the thread.
mod win32_msg {
    use std::ffi::c_void;

    #[repr(C)]
    #[allow(clippy::upper_case_acronyms)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    #[allow(clippy::upper_case_acronyms)]
    pub struct MSG {
        pub hwnd: *mut c_void,
        pub message: u32,
        pub wparam: usize,
        pub lparam: isize,
        pub time: u32,
        pub pt: POINT,
    }

    pub const WM_QUIT: u32 = 0x0012;
    pub const PM_NOREMOVE: u32 = 0x0000;
    /// Application-private message: signal the thread to apply a tooltip update.
    pub const WM_APP_UPDATE_TOOLTIP: u32 = 0x8000; // WM_APP
    /// Application-private message: signal the thread to update pause text.
    pub const WM_APP_UPDATE_PAUSE: u32 = 0x8001; // WM_APP + 1

    extern "system" {
        pub fn GetCurrentThreadId() -> u32;
        pub fn GetMessageW(msg: *mut MSG, hwnd: *mut c_void, min: u32, max: u32) -> i32;
        pub fn TranslateMessage(msg: *const MSG) -> i32;
        pub fn DispatchMessageW(msg: *const MSG) -> isize;
        pub fn PostThreadMessageW(id: u32, msg: u32, wp: usize, lp: isize) -> i32;
        pub fn PeekMessageW(
            msg: *mut MSG,
            hwnd: *mut c_void,
            min: u32,
            max: u32,
            remove: u32,
        ) -> i32;
    }
}

/// Menu item IDs for tray context menu.
mod menu_ids {
    pub const REFRESH: &str = "refresh";
    pub const RELOAD: &str = "reload";
    pub const EXIT: &str = "exit";
    pub const TOGGLE_PAUSE: &str = "toggle_pause";
    pub const OPEN_CONFIG: &str = "open_config";
    pub const EDIT_CONFIG: &str = "edit_config";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const RELEASE_ALL_WINDOWS: &str = "release_all_windows";
}

/// Events emitted by the tray icon.
#[derive(Debug, Clone)]
pub enum TrayEvent {
    /// User clicked "Refresh Windows" menu item.
    Refresh,
    /// User clicked "Reload Config" menu item.
    Reload,
    /// User clicked "Exit" menu item.
    Exit,
    /// User clicked "Pause/Resume Tiling" menu item.
    TogglePause,
    /// User clicked "Settings" menu item.
    OpenConfig,
    /// User clicked "Edit Config" menu item.
    EditConfig,
    /// User clicked "View Logs" menu item.
    ViewLogs,
    /// User clicked "Release All Windows" menu item.
    ReleaseAllWindows,
}

/// Shared state between the caller and the message-loop thread.
///
/// `MenuItem` and `TrayIcon` are `!Send`, so they must stay on the
/// message-loop thread. Updates are communicated via these shared atomics
/// and mutexes, with `PostThreadMessageW` to wake the thread.
struct SharedState {
    paused: AtomicBool,
    tooltip_text: Mutex<String>,
}

/// Manages the system tray icon and context menu.
///
/// The tray icon and its hidden window live on a dedicated thread that runs a
/// Win32 message pump, which is required for the context menu to appear on
/// right-click.
pub struct TrayManager {
    shared: Arc<SharedState>,
    /// Win32 thread ID of the message-loop thread (for `PostThreadMessageW`).
    msg_thread_id: u32,
    /// Join handle for the message-loop thread.
    msg_thread: Option<std::thread::JoinHandle<()>>,
}

/// Init handshake sent from the message-loop thread back to the caller.
type InitResult = Result<u32, TrayError>;

impl TrayManager {
    /// Create a new tray manager with icon and context menu.
    ///
    /// The provided sender will receive tray events when menu items are clicked.
    /// Internally spawns a dedicated thread with a Win32 message pump so that
    /// right-click context menus work correctly.
    pub fn new(event_sender: mpsc::Sender<TrayEvent>) -> Result<Self, TrayError> {
        let shared = Arc::new(SharedState {
            paused: AtomicBool::new(false),
            tooltip_text: Mutex::new(String::from(
                "LeopardWM - Tiling Window Manager",
            )),
        });
        let shared_for_thread = shared.clone();
        let (init_tx, init_rx) = mpsc::channel::<InitResult>();

        let thread = std::thread::Builder::new()
            .name("tray-msg-loop".into())
            .spawn(move || {
                run_tray_thread(init_tx, shared_for_thread);
            })
            .map_err(|e| TrayError::Build(format!("Failed to spawn tray thread: {e}")))?;

        // Wait for the message-loop thread to finish building the tray icon.
        let thread_id = init_rx
            .recv()
            .map_err(|_| TrayError::Build("Tray thread exited during init".into()))??;

        // Spawn thread to listen for menu events and forward them.
        std::thread::Builder::new()
            .name("tray-menu-events".into())
            .spawn(move || {
                let rx = MenuEvent::receiver();
                while let Ok(event) = rx.recv() {
                    let Some(tray_event) = map_menu_id_to_event(event.id.0.as_str()) else {
                        debug!("Unknown menu item clicked: {}", event.id.0);
                        continue;
                    };
                    if event_sender.send(tray_event).is_err() {
                        break;
                    }
                }
            })
            .ok();

        info!("System tray icon created");

        Ok(Self {
            shared,
            msg_thread_id: thread_id,
            msg_thread: Some(thread),
        })
    }

    /// Update the pause menu item text based on the current paused state.
    pub fn update_pause_text(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_PAUSE,
                0,
                0,
            );
        }
    }

    /// Update the tray tooltip to reflect current state.
    ///
    /// If `hotkey_mismatch` is provided as `Some((registered, requested))` and
    /// registered < requested, a warning is appended to the tooltip.
    pub fn update_tooltip(
        &self,
        window_count: usize,
        monitor_count: usize,
        paused: bool,
        hotkey_mismatch: Option<(usize, usize)>,
    ) {
        let tooltip = format_tooltip_text(window_count, monitor_count, paused, hotkey_mismatch);
        if let Ok(mut text) = self.shared.tooltip_text.lock() {
            *text = tooltip;
        }
        // Wake the message-loop thread to apply the new tooltip.
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_TOOLTIP,
                0,
                0,
            );
        }
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        // Signal the message loop to exit. WM_QUIT causes GetMessageW to return 0,
        // which breaks the loop and lets TrayIcon drop on its creating thread.
        unsafe {
            win32_msg::PostThreadMessageW(self.msg_thread_id, win32_msg::WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.msg_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Runs on the dedicated tray thread: builds the tray icon and pumps messages.
fn run_tray_thread(init_tx: mpsc::Sender<InitResult>, shared: Arc<SharedState>) {
    let thread_id = unsafe { win32_msg::GetCurrentThreadId() };

    let (tray, pause_item) = match build_tray() {
        Ok(v) => v,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    // Ensure the thread has a message queue before signaling init complete.
    // PeekMessageW creates the queue as a side effect, so subsequent
    // PostThreadMessageW calls from the caller won't be lost.
    unsafe {
        let mut msg = std::mem::zeroed::<win32_msg::MSG>();
        win32_msg::PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            0,
            0,
            win32_msg::PM_NOREMOVE,
        );
    }

    if init_tx.send(Ok(thread_id)).is_err() {
        return; // Caller dropped the receiver.
    }

    // Win32 message loop — pumps messages for the hidden tray-icon window.
    unsafe {
        let mut msg = std::mem::zeroed::<win32_msg::MSG>();
        loop {
            let ret = win32_msg::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if ret <= 0 {
                break; // WM_QUIT (0) or error (-1).
            }

            // Thread messages (hwnd == NULL) carry our custom update signals.
            if msg.hwnd.is_null() {
                match msg.message {
                    win32_msg::WM_APP_UPDATE_TOOLTIP => {
                        if let Ok(text) = shared.tooltip_text.lock() {
                            let _ = tray.set_tooltip(Some(text.as_str()));
                        }
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_PAUSE => {
                        let paused = shared.paused.load(Ordering::Relaxed);
                        let label = if paused {
                            "Resume Tiling\tCtrl+Alt+P"
                        } else {
                            "Pause Tiling\tCtrl+Alt+P"
                        };
                        pause_item.set_text(label);
                        continue;
                    }
                    _ => {}
                }
            }

            win32_msg::TranslateMessage(&msg);
            win32_msg::DispatchMessageW(&msg);
        }
    }
    // `tray` and `pause_item` are dropped here — on the same thread that created them.
}

/// Build the tray icon with its context menu. Called on the message-loop
/// thread so the hidden notification window belongs to that thread.
fn build_tray() -> Result<(tray_icon::TrayIcon, MenuItem), TrayError> {
    let menu = Menu::new();
    let append = |item: &dyn tray_icon::menu::IsMenuItem| -> Result<(), TrayError> {
        menu.append(item)
            .map_err(|e| TrayError::Menu(e.to_string()))
    };

    // Title item (disabled, shows version)
    let version = env!("CARGO_PKG_VERSION");
    append(&MenuItem::new(format!("LeopardWM v{version}"), false, None))?;
    append(&PredefinedMenuItem::separator())?;

    // Toggle Pause (first — most time-sensitive action)
    let toggle_pause = MenuItem::with_id(
        menu_ids::TOGGLE_PAUSE,
        "Pause Tiling\tCtrl+Alt+P",
        true,
        None,
    );
    append(&toggle_pause)?;
    append(&PredefinedMenuItem::separator())?;

    // Configuration group
    append(&MenuItem::with_id(menu_ids::OPEN_CONFIG, "Settings...", true, None))?;
    append(&MenuItem::with_id(menu_ids::EDIT_CONFIG, "Edit Config", true, None))?;
    append(&MenuItem::with_id(
        menu_ids::RELOAD,
        "Reload Config\tCtrl+Alt+Shift+R",
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;

    // Troubleshooting submenu
    let troubleshoot = Submenu::new("Troubleshooting", true);
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::REFRESH,
            "Refresh Windows\tCtrl+Alt+R",
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(menu_ids::VIEW_LOGS, "View Logs", true, None))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::RELEASE_ALL_WINDOWS,
            "Release All Windows",
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&troubleshoot)?;
    append(&PredefinedMenuItem::separator())?;

    // Exit
    append(&MenuItem::with_id(menu_ids::EXIT, "Exit", true, None))?;

    // Create the tray icon with a simple embedded icon
    let icon = create_default_icon()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("LeopardWM - Tiling Window Manager")
        .with_icon(icon)
        .build()
        .map_err(|e| TrayError::Build(e.to_string()))?;

    Ok((tray, toggle_pause))
}

fn map_menu_id_to_event(menu_id: &str) -> Option<TrayEvent> {
    match menu_id {
        menu_ids::REFRESH => Some(TrayEvent::Refresh),
        menu_ids::RELOAD => Some(TrayEvent::Reload),
        menu_ids::EXIT => Some(TrayEvent::Exit),
        menu_ids::TOGGLE_PAUSE => Some(TrayEvent::TogglePause),
        menu_ids::OPEN_CONFIG => Some(TrayEvent::OpenConfig),
        menu_ids::EDIT_CONFIG => Some(TrayEvent::EditConfig),
        menu_ids::VIEW_LOGS => Some(TrayEvent::ViewLogs),
        menu_ids::RELEASE_ALL_WINDOWS => Some(TrayEvent::ReleaseAllWindows),
        _ => None,
    }
}

/// Format the tray tooltip text (testable without requiring a real tray icon).
pub fn format_tooltip_text(
    window_count: usize,
    monitor_count: usize,
    paused: bool,
    hotkey_mismatch: Option<(usize, usize)>,
) -> String {
    let status = if paused { "Paused" } else { "Active" };
    let mut tooltip = format!(
        "LeopardWM - {} ({} windows, {} monitors)",
        status, window_count, monitor_count
    );
    if let Some((registered, requested)) = hotkey_mismatch {
        if registered < requested {
            tooltip.push_str(&format!(
                "\nHotkeys: {}/{} ({} failed)",
                registered,
                requested,
                requested - registered
            ));
        }
    }
    tooltip
}

/// Create the tray icon from the embedded 32x32 PNG.
fn create_default_icon() -> Result<tray_icon::Icon, TrayError> {
    let png_bytes = include_bytes!("../../../assets/icon-32.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| TrayError::Icon(format!("PNG decode error: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| TrayError::Icon(format!("PNG frame error: {e}")))?;
    buf.truncate(info.buffer_size());

    // Convert RGB to RGBA if needed
    let rgba = if info.color_type == png::ColorType::Rgb {
        let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
        for chunk in buf.chunks(3) {
            out.extend_from_slice(chunk);
            out.push(255);
        }
        out
    } else {
        buf
    };

    tray_icon::Icon::from_rgba(rgba, info.width, info.height)
        .map_err(|e| TrayError::Icon(e.to_string()))
}

/// Errors that can occur during tray operations.
#[derive(Debug, Error)]
pub enum TrayError {
    #[error("Failed to create menu: {0}")]
    Menu(String),

    #[error("Failed to build tray icon: {0}")]
    Build(String),

    #[error("Failed to create icon: {0}")]
    Icon(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_default_icon() {
        let icon = create_default_icon();
        assert!(icon.is_ok(), "Should create default icon successfully");
    }

    #[test]
    fn test_tray_event_toggle_pause_variant() {
        let event = TrayEvent::TogglePause;
        assert!(matches!(event, TrayEvent::TogglePause));
    }

    #[test]
    fn test_tray_event_release_all_windows_variant() {
        let event = TrayEvent::ReleaseAllWindows;
        assert!(matches!(event, TrayEvent::ReleaseAllWindows));
    }

    #[test]
    fn test_map_menu_id_to_event() {
        assert!(matches!(
            map_menu_id_to_event(menu_ids::REFRESH),
            Some(TrayEvent::Refresh)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::RELOAD),
            Some(TrayEvent::Reload)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::EXIT),
            Some(TrayEvent::Exit)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_PAUSE),
            Some(TrayEvent::TogglePause)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::OPEN_CONFIG),
            Some(TrayEvent::OpenConfig)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::EDIT_CONFIG),
            Some(TrayEvent::EditConfig)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::VIEW_LOGS),
            Some(TrayEvent::ViewLogs)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::RELEASE_ALL_WINDOWS),
            Some(TrayEvent::ReleaseAllWindows)
        ));
        assert!(map_menu_id_to_event("unknown").is_none());
    }

    #[test]
    fn test_tooltip_format() {
        let active = format_tooltip_text(14, 2, false, None);
        assert_eq!(active, "LeopardWM - Active (14 windows, 2 monitors)");

        let paused = format_tooltip_text(3, 1, true, None);
        assert_eq!(paused, "LeopardWM - Paused (3 windows, 1 monitors)");
    }

    #[test]
    fn test_tooltip_format_with_hotkey_mismatch() {
        let tooltip = format_tooltip_text(10, 2, false, Some((7, 10)));
        assert_eq!(
            tooltip,
            "LeopardWM - Active (10 windows, 2 monitors)\nHotkeys: 7/10 (3 failed)"
        );
    }

    #[test]
    fn test_tooltip_format_no_hotkey_mismatch() {
        // When registered == requested, no mismatch line
        let tooltip = format_tooltip_text(10, 2, false, Some((10, 10)));
        assert_eq!(tooltip, "LeopardWM - Active (10 windows, 2 monitors)");
    }

    #[test]
    fn test_tooltip_format_paused_with_mismatch() {
        let tooltip = format_tooltip_text(5, 1, true, Some((3, 8)));
        assert!(tooltip.contains("Paused"));
        assert!(tooltip.contains("3/8 (5 failed)"));
    }

    #[test]
    fn test_menu_ids_constants() {
        // Ensure menu IDs are distinct
        let ids = [
            menu_ids::REFRESH,
            menu_ids::RELOAD,
            menu_ids::EXIT,
            menu_ids::TOGGLE_PAUSE,
            menu_ids::OPEN_CONFIG,
            menu_ids::EDIT_CONFIG,
            menu_ids::VIEW_LOGS,
            menu_ids::RELEASE_ALL_WINDOWS,
        ];
        for (i, a) in ids.iter().enumerate() {
            for (j, b) in ids.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "Menu IDs must be distinct");
                }
            }
        }
    }
}
