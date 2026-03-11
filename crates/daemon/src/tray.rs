//! System tray icon management for LeopardWM daemon.
//!
//! Provides a system tray icon with a context menu for common operations:
//! - Pause/resume tiling
//! - Quick toggles for common settings
//! - Configuration access (Settings GUI, Edit Config, Reload)
//! - Troubleshooting (Refresh, View Logs, Release All Windows)
//! - Exit
//!
//! The tray icon and its hidden notification window live on a dedicated thread
//! that runs a Win32 message pump. This is required for the right-click context
//! menu to appear — the `tray-icon` crate needs `WM_RBUTTONUP` and related
//! shell notification messages to be dispatched on the owning thread.

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc, Arc, Mutex,
};
use thiserror::Error;
use tracing::{debug, info};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
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
    /// Application-private message: signal the thread to sync quick-toggle check marks.
    pub const WM_APP_UPDATE_TOGGLES: u32 = 0x8002; // WM_APP + 2

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
    pub const OPEN_ABOUT: &str = "open_about";
    pub const EDIT_CONFIG: &str = "edit_config";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const RELEASE_ALL_WINDOWS: &str = "release_all_windows";
    pub const TOGGLE_ACTIVE_BORDER: &str = "toggle_active_border";
    pub const TOGGLE_FOCUS_NEW_WINDOWS: &str = "toggle_focus_new_windows";
    pub const TOGGLE_FOCUS_FOLLOWS_MOUSE: &str = "toggle_focus_follows_mouse";
    pub const CENTERING_CENTER: &str = "centering_center";
    pub const CENTERING_JUST_IN_VIEW: &str = "centering_just_in_view";
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
    /// User clicked the title / "About" menu item.
    OpenAbout,
    /// User clicked "Edit Config" menu item.
    EditConfig,
    /// User clicked "View Logs" menu item.
    ViewLogs,
    /// User clicked "Release All Windows" menu item.
    ReleaseAllWindows,
    /// User toggled "Active Border" check item.
    ToggleActiveBorder,
    /// User toggled "Focus New Windows" check item.
    ToggleFocusNewWindows,
    /// User toggled "Focus Follows Mouse" check item.
    ToggleFocusFollowsMouse,
    /// User selected "Center" centering mode.
    SetCenteringCenter,
    /// User selected "Just in View" centering mode.
    SetCenteringJustInView,
}

/// Centering mode values for atomic storage.
pub const CENTERING_CENTER: u8 = 0;
pub const CENTERING_JUST_IN_VIEW: u8 = 1;

/// Shared state between the caller and the message-loop thread.
///
/// `MenuItem` and `TrayIcon` are `!Send`, so they must stay on the
/// message-loop thread. Updates are communicated via these shared atomics
/// and mutexes, with `PostThreadMessageW` to wake the thread.
struct SharedState {
    paused: AtomicBool,
    tooltip_text: Mutex<String>,
    active_border: AtomicBool,
    focus_new_windows: AtomicBool,
    focus_follows_mouse: AtomicBool,
    centering_mode: AtomicU8,
}

/// Items returned by `build_tray` that the message-loop thread needs to update.
struct TrayItems {
    pause_item: MenuItem,
    active_border_item: CheckMenuItem,
    focus_new_windows_item: CheckMenuItem,
    focus_follows_mouse_item: CheckMenuItem,
    centering_center_item: CheckMenuItem,
    centering_just_in_view_item: CheckMenuItem,
}

/// Initial state for quick-toggle menu items.
pub struct QuickToggleState {
    pub active_border: bool,
    pub focus_new_windows: bool,
    pub focus_follows_mouse: bool,
    /// 0 = Center, 1 = JustInView
    pub centering_mode: u8,
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
    /// `initial` sets the starting check state for quick-toggle items.
    pub fn new(
        event_sender: mpsc::Sender<TrayEvent>,
        initial: QuickToggleState,
    ) -> Result<Self, TrayError> {
        let shared = Arc::new(SharedState {
            paused: AtomicBool::new(false),
            tooltip_text: Mutex::new(String::from("LeopardWM - Tiling Window Manager")),
            active_border: AtomicBool::new(initial.active_border),
            focus_new_windows: AtomicBool::new(initial.focus_new_windows),
            focus_follows_mouse: AtomicBool::new(initial.focus_follows_mouse),
            centering_mode: AtomicU8::new(initial.centering_mode),
        });
        let shared_for_thread = shared.clone();
        let (init_tx, init_rx) = mpsc::channel::<InitResult>();

        let thread = std::thread::Builder::new()
            .name("tray-msg-loop".into())
            .spawn(move || {
                run_tray_thread(init_tx, shared_for_thread, initial);
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

    /// Sync quick-toggle check marks with the current config state.
    pub fn update_quick_toggles(
        &self,
        active_border: bool,
        focus_new_windows: bool,
        focus_follows_mouse: bool,
        centering_mode: u8,
    ) {
        self.shared
            .active_border
            .store(active_border, Ordering::Relaxed);
        self.shared
            .focus_new_windows
            .store(focus_new_windows, Ordering::Relaxed);
        self.shared
            .focus_follows_mouse
            .store(focus_follows_mouse, Ordering::Relaxed);
        self.shared
            .centering_mode
            .store(centering_mode, Ordering::Relaxed);
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_TOGGLES,
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
        active_workspace: u8,
    ) {
        let tooltip = format_tooltip_text(
            window_count,
            monitor_count,
            paused,
            hotkey_mismatch,
            active_workspace,
        );
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

/// Enable dark mode for native Win32 context menus.
///
/// Calls undocumented but stable `uxtheme.dll` ordinals (135, 136) used by
/// Windows Terminal, VS Code, Notepad, etc. Requires Windows 10 1903+.
/// Silently no-ops on older Windows versions.
fn enable_dark_mode_menus() {
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> isize;
        fn GetProcAddress(
            module: isize,
            name: *const u8,
        ) -> Option<unsafe extern "system" fn() -> isize>;
        fn FreeLibrary(module: isize) -> i32;
    }

    const ALLOW_DARK: i32 = 1; // PreferredAppMode::AllowDark — follows system theme

    unsafe {
        let lib: Vec<u16> = "uxtheme.dll\0".encode_utf16().collect();
        let hmodule = LoadLibraryW(lib.as_ptr());
        if hmodule == 0 {
            return;
        }

        // Ordinal 135: SetPreferredAppMode(AllowDark)
        // Tells Windows to use dark theme for native controls when the system is in dark mode.
        if let Some(f) = GetProcAddress(hmodule, 135usize as *const u8) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(f);
            set_preferred_app_mode(ALLOW_DARK);
        }

        // Ordinal 136: FlushMenuThemes()
        // Discards cached menu theme so the new preference takes effect immediately.
        if let Some(f) = GetProcAddress(hmodule, 136usize as *const u8) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }

        FreeLibrary(hmodule);
    }
}

/// Runs on the dedicated tray thread: builds the tray icon and pumps messages.
fn run_tray_thread(
    init_tx: mpsc::Sender<InitResult>,
    shared: Arc<SharedState>,
    initial: QuickToggleState,
) {
    let thread_id = unsafe { win32_msg::GetCurrentThreadId() };

    // Enable dark mode for native context menus before any menu is created.
    enable_dark_mode_menus();

    let (tray, items) = match build_tray(&initial) {
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
                        items.pause_item.set_text(label);
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_TOGGLES => {
                        items
                            .active_border_item
                            .set_checked(shared.active_border.load(Ordering::Relaxed));
                        items
                            .focus_new_windows_item
                            .set_checked(shared.focus_new_windows.load(Ordering::Relaxed));
                        items
                            .focus_follows_mouse_item
                            .set_checked(shared.focus_follows_mouse.load(Ordering::Relaxed));
                        let cm = shared.centering_mode.load(Ordering::Relaxed);
                        items
                            .centering_center_item
                            .set_checked(cm == CENTERING_CENTER);
                        items
                            .centering_just_in_view_item
                            .set_checked(cm == CENTERING_JUST_IN_VIEW);
                        continue;
                    }
                    _ => {}
                }
            }

            win32_msg::TranslateMessage(&msg);
            win32_msg::DispatchMessageW(&msg);
        }
    }
    // `tray` and items are dropped here — on the same thread that created them.
}

/// Build the tray icon with its context menu. Called on the message-loop
/// thread so the hidden notification window belongs to that thread.
fn build_tray(
    initial: &QuickToggleState,
) -> Result<(tray_icon::TrayIcon, TrayItems), TrayError> {
    let menu = Menu::new();
    let append = |item: &dyn tray_icon::menu::IsMenuItem| -> Result<(), TrayError> {
        menu.append(item)
            .map_err(|e| TrayError::Menu(e.to_string()))
    };

    // Title item (clickable — opens About section in Settings)
    let version = env!("CARGO_PKG_VERSION");
    append(&MenuItem::with_id(
        menu_ids::OPEN_ABOUT,
        format!("LeopardWM v{version}"),
        true,
        None,
    ))?;
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

    // Quick toggles
    let active_border_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_ACTIVE_BORDER,
        "Active Border",
        true,
        initial.active_border,
        None,
    );
    append(&active_border_item)?;

    let focus_new_windows_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_NEW_WINDOWS,
        "Focus New Windows",
        true,
        initial.focus_new_windows,
        None,
    );
    append(&focus_new_windows_item)?;

    let focus_follows_mouse_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE,
        "Focus Follows Mouse",
        true,
        initial.focus_follows_mouse,
        None,
    );
    append(&focus_follows_mouse_item)?;

    // Centering Mode submenu
    let centering_sub = Submenu::new("Centering Mode", true);
    let centering_center_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_CENTER,
        "Center",
        true,
        initial.centering_mode == CENTERING_CENTER,
        None,
    );
    let centering_just_in_view_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_JUST_IN_VIEW,
        "Just in View",
        true,
        initial.centering_mode == CENTERING_JUST_IN_VIEW,
        None,
    );
    centering_sub
        .append(&centering_center_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    centering_sub
        .append(&centering_just_in_view_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&centering_sub)?;
    append(&PredefinedMenuItem::separator())?;

    // Configuration group
    append(&MenuItem::with_id(
        menu_ids::OPEN_CONFIG,
        "Settings...",
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        menu_ids::EDIT_CONFIG,
        "Edit Config",
        true,
        None,
    ))?;
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
        .append(&MenuItem::with_id(
            menu_ids::VIEW_LOGS,
            "View Logs",
            true,
            None,
        ))
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

    let items = TrayItems {
        pause_item: toggle_pause,
        active_border_item,
        focus_new_windows_item,
        focus_follows_mouse_item,
        centering_center_item,
        centering_just_in_view_item,
    };

    Ok((tray, items))
}

fn map_menu_id_to_event(menu_id: &str) -> Option<TrayEvent> {
    match menu_id {
        menu_ids::REFRESH => Some(TrayEvent::Refresh),
        menu_ids::RELOAD => Some(TrayEvent::Reload),
        menu_ids::EXIT => Some(TrayEvent::Exit),
        menu_ids::TOGGLE_PAUSE => Some(TrayEvent::TogglePause),
        menu_ids::OPEN_CONFIG => Some(TrayEvent::OpenConfig),
        menu_ids::OPEN_ABOUT => Some(TrayEvent::OpenAbout),
        menu_ids::EDIT_CONFIG => Some(TrayEvent::EditConfig),
        menu_ids::VIEW_LOGS => Some(TrayEvent::ViewLogs),
        menu_ids::RELEASE_ALL_WINDOWS => Some(TrayEvent::ReleaseAllWindows),
        menu_ids::TOGGLE_ACTIVE_BORDER => Some(TrayEvent::ToggleActiveBorder),
        menu_ids::TOGGLE_FOCUS_NEW_WINDOWS => Some(TrayEvent::ToggleFocusNewWindows),
        menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE => Some(TrayEvent::ToggleFocusFollowsMouse),
        menu_ids::CENTERING_CENTER => Some(TrayEvent::SetCenteringCenter),
        menu_ids::CENTERING_JUST_IN_VIEW => Some(TrayEvent::SetCenteringJustInView),
        _ => None,
    }
}

/// Format the tray tooltip text (testable without requiring a real tray icon).
pub fn format_tooltip_text(
    window_count: usize,
    monitor_count: usize,
    paused: bool,
    hotkey_mismatch: Option<(usize, usize)>,
    active_workspace: u8,
) -> String {
    let status = if paused { "Paused" } else { "Active" };
    let mut tooltip = format!(
        "LeopardWM - {} (WS {}, {} windows, {} monitors)",
        status, active_workspace, window_count, monitor_count
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
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
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
    fn test_tray_event_quick_toggle_variants() {
        assert!(matches!(
            TrayEvent::ToggleActiveBorder,
            TrayEvent::ToggleActiveBorder
        ));
        assert!(matches!(
            TrayEvent::ToggleFocusNewWindows,
            TrayEvent::ToggleFocusNewWindows
        ));
        assert!(matches!(
            TrayEvent::ToggleFocusFollowsMouse,
            TrayEvent::ToggleFocusFollowsMouse
        ));
        assert!(matches!(
            TrayEvent::SetCenteringCenter,
            TrayEvent::SetCenteringCenter
        ));
        assert!(matches!(
            TrayEvent::SetCenteringJustInView,
            TrayEvent::SetCenteringJustInView
        ));
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
            map_menu_id_to_event(menu_ids::OPEN_ABOUT),
            Some(TrayEvent::OpenAbout)
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
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_ACTIVE_BORDER),
            Some(TrayEvent::ToggleActiveBorder)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_FOCUS_NEW_WINDOWS),
            Some(TrayEvent::ToggleFocusNewWindows)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE),
            Some(TrayEvent::ToggleFocusFollowsMouse)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTERING_CENTER),
            Some(TrayEvent::SetCenteringCenter)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTERING_JUST_IN_VIEW),
            Some(TrayEvent::SetCenteringJustInView)
        ));
        assert!(map_menu_id_to_event("unknown").is_none());
    }

    #[test]
    fn test_tooltip_format() {
        let active = format_tooltip_text(14, 2, false, None, 1);
        assert_eq!(
            active,
            "LeopardWM - Active (WS 1, 14 windows, 2 monitors)"
        );

        let paused = format_tooltip_text(3, 1, true, None, 1);
        assert_eq!(
            paused,
            "LeopardWM - Paused (WS 1, 3 windows, 1 monitors)"
        );
    }

    #[test]
    fn test_tooltip_format_with_hotkey_mismatch() {
        let tooltip = format_tooltip_text(10, 2, false, Some((7, 10)), 1);
        assert_eq!(
            tooltip,
            "LeopardWM - Active (WS 1, 10 windows, 2 monitors)\nHotkeys: 7/10 (3 failed)"
        );
    }

    #[test]
    fn test_tooltip_format_no_hotkey_mismatch() {
        // When registered == requested, no mismatch line
        let tooltip = format_tooltip_text(10, 2, false, Some((10, 10)), 1);
        assert_eq!(
            tooltip,
            "LeopardWM - Active (WS 1, 10 windows, 2 monitors)"
        );
    }

    #[test]
    fn test_tooltip_format_paused_with_mismatch() {
        let tooltip = format_tooltip_text(5, 1, true, Some((3, 8)), 1);
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
            menu_ids::OPEN_ABOUT,
            menu_ids::EDIT_CONFIG,
            menu_ids::VIEW_LOGS,
            menu_ids::RELEASE_ALL_WINDOWS,
            menu_ids::TOGGLE_ACTIVE_BORDER,
            menu_ids::TOGGLE_FOCUS_NEW_WINDOWS,
            menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE,
            menu_ids::CENTERING_CENTER,
            menu_ids::CENTERING_JUST_IN_VIEW,
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
