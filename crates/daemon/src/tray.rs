//! The tray icon and its hidden notification window live on a dedicated thread
//! that runs a Win32 message pump. The `tray-icon` crate needs
//! `WM_RBUTTONUP` and related shell notifications dispatched on the owning
//! thread for the context menu to appear.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering},
    mpsc, Arc, Mutex,
};
use thiserror::Error;
use tracing::{debug, info};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu},
    TrayIconBuilder,
};

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
    pub const WM_APP_UPDATE_TOOLTIP: u32 = 0x8000; // WM_APP
    pub const WM_APP_UPDATE_PAUSE: u32 = 0x8001; // WM_APP + 1
    pub const WM_APP_UPDATE_TOGGLES: u32 = 0x8002; // WM_APP + 2
    pub const WM_APP_UPDATE_RELEASE_INFO: u32 = 0x8003; // WM_APP + 3

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
    pub const TASKBAR_HIDE_OFFSCREEN: &str = "taskbar_hide_offscreen";
    pub const TASKBAR_HIDE_INACTIVE_WORKSPACES: &str = "taskbar_hide_inactive_workspaces";
    pub const TASKBAR_SHOW_ALL: &str = "taskbar_show_all";
    pub const TOGGLE_AUTO_START: &str = "toggle_auto_start";
    pub const CENTERING_CENTER: &str = "centering_center";
    pub const CENTERING_JUST_IN_VIEW: &str = "centering_just_in_view";
    pub const CENTERING_ON_OVERFLOW: &str = "centering_on_overflow";
    pub const CENTER_COLUMNS_OFF: &str = "center_columns_off";
    pub const CENTER_COLUMNS_SINGLE: &str = "center_columns_single";
    pub const CENTER_COLUMNS_FIT: &str = "center_columns_fit";
    pub const PLACEMENT_NEW_COLUMN: &str = "placement_new_column";
    pub const PLACEMENT_IN_COLUMN: &str = "placement_in_column";
    pub const CHECK_UPDATES: &str = "check_updates";
}

#[derive(Debug, Clone)]
pub enum TrayEvent {
    Refresh,
    Reload,
    Exit,
    TogglePause,
    OpenConfig,
    OpenAbout,
    EditConfig,
    ViewLogs,
    ReleaseAllWindows,
    ToggleActiveBorder,
    ToggleFocusNewWindows,
    ToggleFocusFollowsMouse,
    SetTaskbarButtons(crate::config::TaskbarButtons),
    ToggleAutoStart,
    SetCenteringCenter,
    SetCenteringJustInView,
    SetCenteringOnOverflow,
    SetCenterColumnsOff,
    SetCenterColumnsSingle,
    SetCenterColumnsFit,
    SetPlacementNewColumn,
    SetPlacementInColumn,
    OpenReleasesPage,
}

pub const CENTERING_CENTER: u8 = 0;
pub const CENTERING_JUST_IN_VIEW: u8 = 1;
pub const CENTERING_ON_OVERFLOW: u8 = 2;
pub const CENTER_COLUMNS_OFF: u8 = 0;
pub const CENTER_COLUMNS_SINGLE: u8 = 1;
pub const CENTER_COLUMNS_FIT: u8 = 2;
pub const PLACEMENT_NEW_COLUMN: u8 = 0;
pub const PLACEMENT_IN_COLUMN: u8 = 1;
pub const TASKBAR_HIDE_OFFSCREEN: u8 = 0;
pub const TASKBAR_HIDE_INACTIVE_WORKSPACES: u8 = 1;
pub const TASKBAR_SHOW_ALL: u8 = 2;

/// Shared state between the caller and the message-loop thread.
///
/// `MenuItem` and `TrayIcon` are `!Send`, so they must stay on the
/// message-loop thread. Updates are communicated via these shared atomics
/// and mutexes, with `PostThreadMessageW` to wake the thread.
struct SharedState {
    language: Mutex<String>,
    paused: AtomicBool,
    tooltip_text: Mutex<String>,
    active_border: AtomicBool,
    focus_new_windows: AtomicBool,
    focus_follows_mouse: AtomicBool,
    taskbar_buttons: AtomicU8,
    auto_start: AtomicBool,
    centering_mode: AtomicU8,
    center_columns: AtomicU8,
    placement_mode: AtomicU8,
    available_update: Mutex<Option<String>>,
}

struct TrayItems {
    menu: Menu,
    pause_item: MenuItem,
    active_border_item: CheckMenuItem,
    focus_new_windows_item: CheckMenuItem,
    focus_follows_mouse_item: CheckMenuItem,
    taskbar_hide_offscreen_item: CheckMenuItem,
    taskbar_hide_inactive_workspaces_item: CheckMenuItem,
    taskbar_show_all_item: CheckMenuItem,
    auto_start_item: CheckMenuItem,
    centering_center_item: CheckMenuItem,
    centering_just_in_view_item: CheckMenuItem,
    centering_on_overflow_item: CheckMenuItem,
    center_columns_off_item: CheckMenuItem,
    center_columns_single_item: CheckMenuItem,
    center_columns_fit_item: CheckMenuItem,
    placement_new_column_item: CheckMenuItem,
    placement_in_column_item: CheckMenuItem,
    update_item: MenuItem,
}

pub struct QuickToggleState {
    pub language: String,
    pub active_border: bool,
    pub focus_new_windows: bool,
    pub focus_follows_mouse: bool,
    pub taskbar_buttons: u8,
    pub auto_start: bool,
    /// 0 = Center, 1 = JustInView, 2 = OnOverflow
    pub centering_mode: u8,
    /// 0 = Off, 1 = Single, 2 = Fit
    pub center_columns: u8,
    /// 0 = NewColumn, 1 = InColumn
    pub placement_mode: u8,
}

/// Manages the system tray icon and context menu.
///
/// The tray icon and its hidden window live on a dedicated thread that runs a
/// Win32 message pump, which is required for the context menu to appear on
/// right-click.
pub struct TrayManager {
    shared: Arc<SharedState>,
    _event_sender: Arc<mpsc::Sender<TrayEvent>>,
    msg_thread_id: u32,
    msg_thread: Option<std::thread::JoinHandle<()>>,
}

type InitResult = Result<u32, TrayError>;

impl TrayManager {
    pub fn new(
        event_sender: mpsc::Sender<TrayEvent>,
        initial: QuickToggleState,
        quit_fallback: Arc<crate::quit_fallback::QuitFallback>,
        quit_thread_id: Arc<AtomicU32>,
    ) -> Result<Self, TrayError> {
        let shared = Arc::new(SharedState {
            language: Mutex::new(initial.language.clone()),
            paused: AtomicBool::new(false),
            tooltip_text: Mutex::new(crate::locale::text(
                &initial.language,
                "tray.tooltip.initial",
            )),
            active_border: AtomicBool::new(initial.active_border),
            focus_new_windows: AtomicBool::new(initial.focus_new_windows),
            focus_follows_mouse: AtomicBool::new(initial.focus_follows_mouse),
            taskbar_buttons: AtomicU8::new(initial.taskbar_buttons),
            auto_start: AtomicBool::new(initial.auto_start),
            centering_mode: AtomicU8::new(initial.centering_mode),
            center_columns: AtomicU8::new(initial.center_columns),
            placement_mode: AtomicU8::new(initial.placement_mode),
            available_update: Mutex::new(None),
        });
        let shared_for_thread = shared.clone();
        let (init_tx, init_rx) = mpsc::channel::<InitResult>();

        let thread = std::thread::Builder::new()
            .name("tray-msg-loop".into())
            .spawn(move || {
                run_tray_thread(init_tx, shared_for_thread, initial);
            })
            .map_err(|e| TrayError::Build(format!("Failed to spawn tray thread: {e}")))?;

        let thread_id = init_rx
            .recv()
            .map_err(|_| TrayError::Build("Tray thread exited during init".into()))??;

        quit_thread_id.store(thread_id, Ordering::SeqCst);

        let event_sender = Arc::new(event_sender);
        let menu_sender = Arc::downgrade(&event_sender);
        std::thread::Builder::new()
            .name("tray-menu-events".into())
            .spawn(move || {
                let rx = MenuEvent::receiver();
                while let Ok(event) = rx.recv() {
                    let Some(menu_sender) = menu_sender.upgrade() else {
                        break;
                    };
                    if dispatch_menu_event(event.id.0.as_str(), &menu_sender, &quit_fallback)
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();

        // TrayIconEvent::receiver() is one process-global stream; this must
        // stay the only consumer or readers would compete for events.
        let click_sender = Arc::downgrade(&event_sender);
        std::thread::Builder::new()
            .name("tray-icon-events".into())
            .spawn(move || {
                let rx = tray_icon::TrayIconEvent::receiver();
                while let Ok(event) = rx.recv() {
                    let Some(event_sender) = click_sender.upgrade() else {
                        break;
                    };
                    if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. })
                        && event_sender.send(TrayEvent::OpenConfig).is_err()
                    {
                        break;
                    }
                }
            })
            .ok();

        info!("System tray icon created");

        Ok(Self {
            shared,
            _event_sender: event_sender,
            msg_thread_id: thread_id,
            msg_thread: Some(thread),
        })
    }

    pub fn update_pause_text(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
        unsafe {
            win32_msg::PostThreadMessageW(self.msg_thread_id, win32_msg::WM_APP_UPDATE_PAUSE, 0, 0);
        }
    }

    pub fn update_quick_toggles(&self, toggles: &QuickToggleState) {
        if let Ok(mut language) = self.shared.language.lock() {
            *language = toggles.language.clone();
        }
        self.shared
            .active_border
            .store(toggles.active_border, Ordering::Relaxed);
        self.shared
            .focus_new_windows
            .store(toggles.focus_new_windows, Ordering::Relaxed);
        self.shared
            .focus_follows_mouse
            .store(toggles.focus_follows_mouse, Ordering::Relaxed);
        self.shared
            .taskbar_buttons
            .store(toggles.taskbar_buttons, Ordering::Relaxed);
        self.shared
            .auto_start
            .store(toggles.auto_start, Ordering::Relaxed);
        self.shared
            .centering_mode
            .store(toggles.centering_mode, Ordering::Relaxed);
        self.shared
            .center_columns
            .store(toggles.center_columns, Ordering::Relaxed);
        self.shared
            .placement_mode
            .store(toggles.placement_mode, Ordering::Relaxed);
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
        let language = self
            .shared
            .language
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| "en".into());
        let tooltip = format_tooltip_text(
            &language,
            window_count,
            monitor_count,
            paused,
            hotkey_mismatch,
            active_workspace,
        );
        if let Ok(mut text) = self.shared.tooltip_text.lock() {
            if *text == tooltip {
                return;
            }
            *text = tooltip;
        }
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_TOOLTIP,
                0,
                0,
            );
        }
    }

    /// Record the latest available release tag (e.g. `v0.1.11`) and refresh
    /// the tray's update menu item label. Pass `None` to reset to the default
    /// "Check for Updates" label.
    pub fn set_available_update(&self, tag: Option<String>) {
        if let Ok(mut g) = self.shared.available_update.lock() {
            *g = tag;
        }
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_RELEASE_INFO,
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
        win32_msg::PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, win32_msg::PM_NOREMOVE);
    }

    if init_tx.send(Ok(thread_id)).is_err() {
        return;
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
                        let text = shared.tooltip_text.lock().ok().map(|text| text.clone());
                        if let Some(text) = text {
                            let _ = tray.set_tooltip(Some(text.as_str()));
                        }
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_PAUSE => {
                        let paused = shared.paused.load(Ordering::Relaxed);
                        let language = shared
                            .language
                            .lock()
                            .map(|value| value.clone())
                            .unwrap_or_else(|_| "en".into());
                        let label = pause_label(&language, paused);
                        items.pause_item.set_text(label);
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_TOGGLES => {
                        let language = shared
                            .language
                            .lock()
                            .map(|value| value.clone())
                            .unwrap_or_else(|_| "en".into());
                        relabel_menu(items.menu.items(), &language);
                        items.pause_item.set_text(pause_label(
                            &language,
                            shared.paused.load(Ordering::Relaxed),
                        ));
                        let tag = shared
                            .available_update
                            .lock()
                            .ok()
                            .and_then(|tag| tag.clone());
                        items
                            .update_item
                            .set_text(update_label(&language, tag.as_deref()));
                        items
                            .active_border_item
                            .set_checked(shared.active_border.load(Ordering::Relaxed));
                        items
                            .focus_new_windows_item
                            .set_checked(shared.focus_new_windows.load(Ordering::Relaxed));
                        items
                            .focus_follows_mouse_item
                            .set_checked(shared.focus_follows_mouse.load(Ordering::Relaxed));
                        let tb = shared.taskbar_buttons.load(Ordering::Relaxed);
                        items
                            .taskbar_hide_offscreen_item
                            .set_checked(tb == TASKBAR_HIDE_OFFSCREEN);
                        items
                            .taskbar_hide_inactive_workspaces_item
                            .set_checked(tb == TASKBAR_HIDE_INACTIVE_WORKSPACES);
                        items
                            .taskbar_show_all_item
                            .set_checked(tb == TASKBAR_SHOW_ALL);
                        items
                            .auto_start_item
                            .set_checked(shared.auto_start.load(Ordering::Relaxed));
                        let cm = shared.centering_mode.load(Ordering::Relaxed);
                        items
                            .centering_center_item
                            .set_checked(cm == CENTERING_CENTER);
                        items
                            .centering_just_in_view_item
                            .set_checked(cm == CENTERING_JUST_IN_VIEW);
                        items
                            .centering_on_overflow_item
                            .set_checked(cm == CENTERING_ON_OVERFLOW);
                        let cc = shared.center_columns.load(Ordering::Relaxed);
                        items
                            .center_columns_off_item
                            .set_checked(cc == CENTER_COLUMNS_OFF);
                        items
                            .center_columns_single_item
                            .set_checked(cc == CENTER_COLUMNS_SINGLE);
                        items
                            .center_columns_fit_item
                            .set_checked(cc == CENTER_COLUMNS_FIT);
                        let pm = shared.placement_mode.load(Ordering::Relaxed);
                        items
                            .placement_new_column_item
                            .set_checked(pm == PLACEMENT_NEW_COLUMN);
                        items
                            .placement_in_column_item
                            .set_checked(pm == PLACEMENT_IN_COLUMN);
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_RELEASE_INFO => {
                        let language = shared
                            .language
                            .lock()
                            .map(|value| value.clone())
                            .unwrap_or_else(|_| "en".into());
                        let tag = shared
                            .available_update
                            .lock()
                            .ok()
                            .and_then(|tag| tag.clone());
                        let label = update_label(&language, tag.as_deref());
                        items.update_item.set_text(label);
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

fn menu_label(language: &str, id: &str) -> String {
    match id {
        "toggle_pause" => crate::locale::text(language, "tray.toggle_pause"),
        "toggle_active_border" => crate::locale::text(language, "tray.toggle_active_border"),
        "toggle_focus_new_windows" => {
            crate::locale::text(language, "tray.toggle_focus_new_windows")
        }
        "toggle_focus_follows_mouse" => {
            crate::locale::text(language, "tray.toggle_focus_follows_mouse")
        }
        "taskbar_hide_offscreen" => crate::locale::text(language, "tray.taskbar_hide_offscreen"),
        "taskbar_hide_inactive_workspaces" => {
            crate::locale::text(language, "tray.taskbar_hide_inactive_workspaces")
        }
        "taskbar_show_all" => crate::locale::text(language, "tray.taskbar_show_all"),
        "toggle_auto_start" => crate::locale::text(language, "tray.toggle_auto_start"),
        "centering_center" => crate::locale::text(language, "tray.centering_center"),
        "centering_just_in_view" => crate::locale::text(language, "tray.centering_just_in_view"),
        "centering_on_overflow" => crate::locale::text(language, "tray.centering_on_overflow"),
        "center_columns_off" => crate::locale::text(language, "tray.center_columns_off"),
        "center_columns_single" => crate::locale::text(language, "tray.center_columns_single"),
        "center_columns_fit" => crate::locale::text(language, "tray.center_columns_fit"),
        "placement_new_column" => crate::locale::text(language, "tray.placement_new_column"),
        "placement_in_column" => crate::locale::text(language, "tray.placement_in_column"),
        "check_updates" => crate::locale::text(language, "tray.check_updates"),
        "open_config" => crate::locale::text(language, "tray.open_config"),
        "edit_config" => crate::locale::text(language, "tray.edit_config"),
        "reload" => crate::locale::text(language, "tray.reload"),
        "refresh" => crate::locale::text(language, "tray.refresh"),
        "view_logs" => crate::locale::text(language, "tray.view_logs"),
        "release_all_windows" => crate::locale::text(language, "tray.release_all_windows"),
        "exit" => crate::locale::text(language, "tray.exit"),
        "taskbar" => crate::locale::text(language, "tray.taskbar"),
        "centering" => crate::locale::text(language, "tray.centering"),
        "center_columns" => crate::locale::text(language, "tray.center_columns"),
        "placement" => crate::locale::text(language, "tray.placement"),
        "troubleshooting" => crate::locale::text(language, "tray.troubleshooting"),
        "open_about" => crate::locale::format(
            language,
            "tray.about",
            &[("version", env!("CARGO_PKG_VERSION"))],
        ),
        _ => id.to_string(),
    }
}

fn relabel_menu(items: Vec<MenuItemKind>, language: &str) {
    for item in items {
        let label = menu_label(language, item.id().0.as_str());
        match item {
            MenuItemKind::MenuItem(item) => item.set_text(label),
            MenuItemKind::Check(item) => item.set_text(label),
            MenuItemKind::Submenu(item) => {
                item.set_text(label);
                relabel_menu(item.items(), language);
            }
            _ => {}
        }
    }
}

fn pause_label(language: &str, paused: bool) -> String {
    if paused {
        crate::locale::text(language, "tray.resume")
    } else {
        crate::locale::text(language, "tray.toggle_pause")
    }
}

fn update_label(language: &str, tag: Option<&str>) -> String {
    match tag {
        Some(tag) => crate::locale::format(language, "tray.update_available", &[("tag", tag)]),
        None => crate::locale::text(language, "tray.check_updates"),
    }
}

/// Build the tray icon with its context menu. Called on the message-loop
/// thread so the hidden notification window belongs to that thread.
fn build_tray(initial: &QuickToggleState) -> Result<(tray_icon::TrayIcon, TrayItems), TrayError> {
    let menu = Menu::new();
    let append = |item: &dyn tray_icon::menu::IsMenuItem| -> Result<(), TrayError> {
        menu.append(item)
            .map_err(|e| TrayError::Menu(e.to_string()))
    };

    let version = env!("CARGO_PKG_VERSION");
    append(&MenuItem::with_id(
        menu_ids::OPEN_ABOUT,
        crate::locale::format(&initial.language, "tray.about", &[("version", version)]),
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;

    // Toggle Pause (first — most time-sensitive action)
    let toggle_pause = MenuItem::with_id(
        menu_ids::TOGGLE_PAUSE,
        menu_label(&initial.language, "toggle_pause"),
        true,
        None,
    );
    append(&toggle_pause)?;
    append(&PredefinedMenuItem::separator())?;

    let active_border_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_ACTIVE_BORDER,
        menu_label(&initial.language, "toggle_active_border"),
        true,
        initial.active_border,
        None,
    );
    append(&active_border_item)?;

    let focus_new_windows_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_NEW_WINDOWS,
        menu_label(&initial.language, "toggle_focus_new_windows"),
        true,
        initial.focus_new_windows,
        None,
    );
    append(&focus_new_windows_item)?;

    let focus_follows_mouse_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE,
        menu_label(&initial.language, "toggle_focus_follows_mouse"),
        true,
        initial.focus_follows_mouse,
        None,
    );
    append(&focus_follows_mouse_item)?;

    let taskbar_sub = Submenu::with_id("taskbar", menu_label(&initial.language, "taskbar"), true);
    let taskbar_hide_offscreen_item = CheckMenuItem::with_id(
        menu_ids::TASKBAR_HIDE_OFFSCREEN,
        menu_label(&initial.language, "taskbar_hide_offscreen"),
        true,
        initial.taskbar_buttons == TASKBAR_HIDE_OFFSCREEN,
        None,
    );
    taskbar_sub
        .append(&taskbar_hide_offscreen_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    let taskbar_hide_inactive_workspaces_item = CheckMenuItem::with_id(
        menu_ids::TASKBAR_HIDE_INACTIVE_WORKSPACES,
        menu_label(&initial.language, "taskbar_hide_inactive_workspaces"),
        true,
        initial.taskbar_buttons == TASKBAR_HIDE_INACTIVE_WORKSPACES,
        None,
    );
    taskbar_sub
        .append(&taskbar_hide_inactive_workspaces_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    let taskbar_show_all_item = CheckMenuItem::with_id(
        menu_ids::TASKBAR_SHOW_ALL,
        menu_label(&initial.language, "taskbar_show_all"),
        true,
        initial.taskbar_buttons == TASKBAR_SHOW_ALL,
        None,
    );
    taskbar_sub
        .append(&taskbar_show_all_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&taskbar_sub)?;

    let auto_start_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_AUTO_START,
        menu_label(&initial.language, "toggle_auto_start"),
        true,
        initial.auto_start,
        None,
    );
    append(&auto_start_item)?;

    let centering_sub = Submenu::with_id(
        "centering",
        menu_label(&initial.language, "centering"),
        true,
    );
    let centering_center_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_CENTER,
        menu_label(&initial.language, "centering_center"),
        true,
        initial.centering_mode == CENTERING_CENTER,
        None,
    );
    let centering_just_in_view_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_JUST_IN_VIEW,
        menu_label(&initial.language, "centering_just_in_view"),
        true,
        initial.centering_mode == CENTERING_JUST_IN_VIEW,
        None,
    );
    let centering_on_overflow_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_ON_OVERFLOW,
        menu_label(&initial.language, "centering_on_overflow"),
        true,
        initial.centering_mode == CENTERING_ON_OVERFLOW,
        None,
    );
    centering_sub
        .append(&centering_center_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    centering_sub
        .append(&centering_just_in_view_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    centering_sub
        .append(&centering_on_overflow_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&centering_sub)?;

    let center_columns_sub = Submenu::with_id(
        "center_columns",
        menu_label(&initial.language, "center_columns"),
        true,
    );
    let center_columns_off_item = CheckMenuItem::with_id(
        menu_ids::CENTER_COLUMNS_OFF,
        menu_label(&initial.language, "center_columns_off"),
        true,
        initial.center_columns == CENTER_COLUMNS_OFF,
        None,
    );
    let center_columns_single_item = CheckMenuItem::with_id(
        menu_ids::CENTER_COLUMNS_SINGLE,
        menu_label(&initial.language, "center_columns_single"),
        true,
        initial.center_columns == CENTER_COLUMNS_SINGLE,
        None,
    );
    let center_columns_fit_item = CheckMenuItem::with_id(
        menu_ids::CENTER_COLUMNS_FIT,
        menu_label(&initial.language, "center_columns_fit"),
        true,
        initial.center_columns == CENTER_COLUMNS_FIT,
        None,
    );
    center_columns_sub
        .append(&center_columns_off_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    center_columns_sub
        .append(&center_columns_single_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    center_columns_sub
        .append(&center_columns_fit_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&center_columns_sub)?;

    let placement_sub = Submenu::with_id(
        "placement",
        menu_label(&initial.language, "placement"),
        true,
    );
    let placement_new_column_item = CheckMenuItem::with_id(
        menu_ids::PLACEMENT_NEW_COLUMN,
        menu_label(&initial.language, "placement_new_column"),
        true,
        initial.placement_mode == PLACEMENT_NEW_COLUMN,
        None,
    );
    let placement_in_column_item = CheckMenuItem::with_id(
        menu_ids::PLACEMENT_IN_COLUMN,
        menu_label(&initial.language, "placement_in_column"),
        true,
        initial.placement_mode == PLACEMENT_IN_COLUMN,
        None,
    );
    placement_sub
        .append(&placement_new_column_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    placement_sub
        .append(&placement_in_column_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&placement_sub)?;
    append(&PredefinedMenuItem::separator())?;

    let update_item = MenuItem::with_id(
        menu_ids::CHECK_UPDATES,
        menu_label(&initial.language, "check_updates"),
        true,
        None,
    );
    append(&update_item)?;
    append(&PredefinedMenuItem::separator())?;

    append(&MenuItem::with_id(
        menu_ids::OPEN_CONFIG,
        menu_label(&initial.language, "open_config"),
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        menu_ids::EDIT_CONFIG,
        menu_label(&initial.language, "edit_config"),
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        menu_ids::RELOAD,
        menu_label(&initial.language, "reload"),
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;

    let troubleshoot = Submenu::with_id(
        "troubleshooting",
        menu_label(&initial.language, "troubleshooting"),
        true,
    );
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::REFRESH,
            menu_label(&initial.language, "refresh"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::VIEW_LOGS,
            menu_label(&initial.language, "view_logs"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::RELEASE_ALL_WINDOWS,
            menu_label(&initial.language, "release_all_windows"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&troubleshoot)?;
    append(&PredefinedMenuItem::separator())?;

    append(&MenuItem::with_id(
        menu_ids::EXIT,
        menu_label(&initial.language, "exit"),
        true,
        None,
    ))?;

    let icon = create_default_icon()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip(crate::locale::text(
            &initial.language,
            "tray.tooltip.initial",
        ))
        .with_icon(icon)
        .build()
        .map_err(|e| TrayError::Build(e.to_string()))?;

    let items = TrayItems {
        menu,
        pause_item: toggle_pause,
        active_border_item,
        focus_new_windows_item,
        focus_follows_mouse_item,
        taskbar_hide_offscreen_item,
        taskbar_hide_inactive_workspaces_item,
        taskbar_show_all_item,
        auto_start_item,
        centering_center_item,
        centering_just_in_view_item,
        centering_on_overflow_item,
        center_columns_off_item,
        center_columns_single_item,
        center_columns_fit_item,
        placement_new_column_item,
        placement_in_column_item,
        update_item,
    };

    Ok((tray, items))
}

pub(crate) fn post_quit(thread_id: u32) {
    if thread_id != 0 {
        unsafe {
            win32_msg::PostThreadMessageW(thread_id, win32_msg::WM_QUIT, 0, 0);
        }
    }
}

pub(crate) fn dispatch_menu_event(
    menu_id: &str,
    sender: &mpsc::Sender<TrayEvent>,
    quit_fallback: &crate::quit_fallback::QuitFallback,
) -> Result<(), mpsc::SendError<TrayEvent>> {
    let Some(event) = map_menu_id_to_event(menu_id) else {
        debug!("Unknown menu item clicked: {}", menu_id);
        return Ok(());
    };
    if matches!(event, TrayEvent::Exit) {
        quit_fallback.arm();
    }
    sender.send(event)
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
        menu_ids::TASKBAR_HIDE_OFFSCREEN => Some(TrayEvent::SetTaskbarButtons(
            crate::config::TaskbarButtons::HideOffscreen,
        )),
        menu_ids::TASKBAR_HIDE_INACTIVE_WORKSPACES => Some(TrayEvent::SetTaskbarButtons(
            crate::config::TaskbarButtons::HideInactiveWorkspaces,
        )),
        menu_ids::TASKBAR_SHOW_ALL => Some(TrayEvent::SetTaskbarButtons(
            crate::config::TaskbarButtons::ShowAll,
        )),
        menu_ids::TOGGLE_AUTO_START => Some(TrayEvent::ToggleAutoStart),
        menu_ids::CENTERING_CENTER => Some(TrayEvent::SetCenteringCenter),
        menu_ids::CENTERING_JUST_IN_VIEW => Some(TrayEvent::SetCenteringJustInView),
        menu_ids::CENTERING_ON_OVERFLOW => Some(TrayEvent::SetCenteringOnOverflow),
        menu_ids::CENTER_COLUMNS_OFF => Some(TrayEvent::SetCenterColumnsOff),
        menu_ids::CENTER_COLUMNS_SINGLE => Some(TrayEvent::SetCenterColumnsSingle),
        menu_ids::CENTER_COLUMNS_FIT => Some(TrayEvent::SetCenterColumnsFit),
        menu_ids::PLACEMENT_NEW_COLUMN => Some(TrayEvent::SetPlacementNewColumn),
        menu_ids::PLACEMENT_IN_COLUMN => Some(TrayEvent::SetPlacementInColumn),
        menu_ids::CHECK_UPDATES => Some(TrayEvent::OpenReleasesPage),
        _ => None,
    }
}

pub fn format_tooltip_text(
    language: &str,
    window_count: usize,
    monitor_count: usize,
    paused: bool,
    hotkey_mismatch: Option<(usize, usize)>,
    active_workspace: u8,
) -> String {
    let status = if paused {
        crate::locale::text(language, "tray.status.paused")
    } else {
        crate::locale::text(language, "tray.status.active")
    };
    let mut tooltip = crate::locale::format(
        language,
        "tray.tooltip.status",
        &[
            ("status", &status),
            ("workspace", &active_workspace.to_string()),
            ("windows", &window_count.to_string()),
            ("monitors", &monitor_count.to_string()),
        ],
    );
    if let Some((registered, requested)) = hotkey_mismatch {
        if registered < requested {
            tooltip.push_str(&crate::locale::format(
                language,
                "tray.tooltip.hotkeys",
                &[
                    ("registered", &registered.to_string()),
                    ("requested", &requested.to_string()),
                    ("failed", &(requested - registered).to_string()),
                ],
            ));
        }
    }
    tooltip
}

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
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTER_COLUMNS_OFF),
            Some(TrayEvent::SetCenterColumnsOff)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTER_COLUMNS_SINGLE),
            Some(TrayEvent::SetCenterColumnsSingle)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTER_COLUMNS_FIT),
            Some(TrayEvent::SetCenterColumnsFit)
        ));
        for (id, expected) in [
            (
                menu_ids::TASKBAR_HIDE_OFFSCREEN,
                crate::config::TaskbarButtons::HideOffscreen,
            ),
            (
                menu_ids::TASKBAR_HIDE_INACTIVE_WORKSPACES,
                crate::config::TaskbarButtons::HideInactiveWorkspaces,
            ),
            (
                menu_ids::TASKBAR_SHOW_ALL,
                crate::config::TaskbarButtons::ShowAll,
            ),
        ] {
            assert!(
                matches!(map_menu_id_to_event(id), Some(TrayEvent::SetTaskbarButtons(mode)) if mode == expected)
            );
        }
        assert!(map_menu_id_to_event("unknown").is_none());
    }

    #[test]
    fn test_tooltip_format() {
        let active = format_tooltip_text("en", 14, 2, false, None, 1);
        assert_eq!(active, "LeopardWM - Active (WS 1, 14 windows, 2 monitors)");

        let paused = format_tooltip_text("en", 3, 1, true, None, 1);
        assert_eq!(paused, "LeopardWM - Paused (WS 1, 3 windows, 1 monitors)");
    }

    #[test]
    fn test_tooltip_format_with_hotkey_mismatch() {
        let tooltip = format_tooltip_text("en", 10, 2, false, Some((7, 10)), 1);
        assert_eq!(
            tooltip,
            "LeopardWM - Active (WS 1, 10 windows, 2 monitors)\nHotkeys: 7/10 (3 failed)"
        );
    }

    #[test]
    fn test_tooltip_format_no_hotkey_mismatch() {
        let tooltip = format_tooltip_text("en", 10, 2, false, Some((10, 10)), 1);
        assert_eq!(tooltip, "LeopardWM - Active (WS 1, 10 windows, 2 monitors)");
    }

    #[test]
    fn test_tooltip_format_paused_with_mismatch() {
        let tooltip = format_tooltip_text("en", 5, 1, true, Some((3, 8)), 1);
        assert!(tooltip.contains("Paused"));
        assert!(tooltip.contains("3/8 (5 failed)"));
    }

    #[test]
    fn test_menu_ids_constants() {
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
