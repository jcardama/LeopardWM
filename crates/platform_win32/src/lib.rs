//! LeopardWM Platform Win32
//!
//! Windows-specific window manipulation using Win32 APIs.
//!
//! This crate handles:
//! - Window enumeration and filtering
//! - Window positioning via SetWindowPos
//! - WinEvent hooks for window lifecycle events
//! - Visual overlay for snap hints

pub mod border;
pub mod gestures;
pub mod hotkeys;
pub mod mouse_hook;
pub mod overlay;

pub use gestures::*;
pub use hotkeys::*;
pub use mouse_hook::*;

use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc;
use thiserror::Error;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DispatchMessageW, EnumWindows, GetAncestor, GetClassNameW, GetMessageW,
    GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, PeekMessageW, PostMessageW,
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, PostThreadMessageW,
    SetForegroundWindow, SetWindowPos, ShowWindow, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GW_OWNER,
    MSG, PM_NOREMOVE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, WM_USER,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_VISIBLE,
};

// WinEvent constants (not all are exposed by windows-rs)
const EVENT_OBJECT_CREATE: u32 = 0x8000;
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
const EVENT_OBJECT_SHOW: u32 = 0x8002;
const EVENT_OBJECT_HIDE: u32 = 0x8003;
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000A;
const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
const OBJID_WINDOW: i32 = 0;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

/// Sentinel coordinate used by MoveOffScreen strategy.
pub const MOVE_OFFSCREEN_SENTINEL_COORD: i32 = -100_000;

/// Custom message to signal the gesture/mouse-hook thread to stop.
pub(crate) const WM_QUIT_LLHOOK_THREAD: u32 = WM_USER + 2;

/// Recover from a poisoned mutex, logging a warning.
///
/// When a thread panics while holding a mutex, the mutex becomes "poisoned".
/// This helper logs the event and recovers the inner data so the application
/// can continue operating.
fn recover_poisoned_mutex<T>(
    err: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>,
) -> std::sync::MutexGuard<'_, T> {
    eprintln!("[leopardwm] WARNING: Mutex poisoned, recovering");
    err.into_inner()
}

/// Convert a WindowId to an HWND safely, returning an error for null (zero) IDs.
///
/// A WindowId of 0 would produce a null HWND pointer, which is invalid for
/// most Win32 window operations.
fn window_id_to_hwnd(id: WindowId) -> Result<HWND, Win32Error> {
    if id == 0 {
        return Err(Win32Error::WindowNotFound(id));
    }
    Ok(HWND(id as *mut c_void))
}

fn combine_operation_failures(context: &str, failures: Vec<String>) -> Win32Error {
    debug_assert!(!failures.is_empty());
    Win32Error::SetPositionFailed(format!(
        "{} ({} failures): {}",
        context,
        failures.len(),
        failures.join("; ")
    ))
}

/// Whether an operation failure is benign and should not fail the entire
/// placement batch.
///
/// Benign failures include:
/// - Window-not-found races (window vanished between enumeration and operation)
fn is_benign_side_effect_error(error: &Win32Error) -> bool {
    matches!(
        error,
        Win32Error::WindowNotFound(window_id) if *window_id != 0
    )
}

/// Errors that can occur during Win32 operations.
#[derive(Debug, Error)]
pub enum Win32Error {
    #[error("Failed to enumerate windows: {0}")]
    EnumerationFailed(String),

    #[error("Failed to enumerate monitors: {0}")]
    MonitorEnumerationFailed(String),

    #[error("Failed to set window position: {0}")]
    SetPositionFailed(String),

    #[error("Failed to install event hook: {0}")]
    HookInstallFailed(String),

    #[error("Failed to register hotkey: {0}")]
    HotkeyRegistrationFailed(String),

    #[error("Window not found: {0}")]
    WindowNotFound(WindowId),
}

/// Information about a managed window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// The window handle (HWND) as u64.
    pub hwnd: WindowId,
    /// Window title.
    pub title: String,
    /// Window class name.
    pub class_name: String,
    /// Process ID.
    pub process_id: u32,
    /// Current window rectangle.
    pub rect: Rect,
    /// Whether the window is visible.
    pub visible: bool,
}

/// Unique identifier for a monitor (derived from HMONITOR handle).
pub type MonitorId = isize;

/// Information about a display monitor.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Unique monitor identifier.
    pub id: MonitorId,
    /// Full monitor rectangle (entire display area).
    pub rect: Rect,
    /// Work area (excludes taskbar and other docked windows).
    pub work_area: Rect,
    /// Whether this is the primary monitor.
    pub is_primary: bool,
    /// Device name (e.g., `\\.\DISPLAY1`).
    pub device_name: String,
}

impl MonitorInfo {
    /// Check if a point is within this monitor's bounds.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.rect.x
            && x < self.rect.x + self.rect.width
            && y >= self.rect.y
            && y < self.rect.y + self.rect.height
    }

    /// Check if a rectangle's center is within this monitor's bounds.
    pub fn contains_rect_center(&self, rect: &Rect) -> bool {
        let center_x = rect.x + rect.width / 2;
        let center_y = rect.y + rect.height / 2;
        self.contains_point(center_x, center_y)
    }
}

/// Configuration for the Win32 platform layer.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig;

/// Enumerate all top-level windows that should be managed.
///
/// Filters out:
/// Get info for a single window handle with relaxed filters.
///
/// Unlike `enumerate_windows`, this does not filter out cloaked windows
/// or windows with empty titles, making it suitable for handling window
/// creation events where UWP apps may still be transitioning.
pub fn get_window_info(hwnd_id: WindowId) -> Option<WindowInfo> {
    unsafe {
        let hwnd = HWND(hwnd_id as *mut c_void);

        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }

        let _style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

        // Skip tool windows (unless they have WS_EX_APPWINDOW)
        let is_tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
        let is_app_window = ex_style & WS_EX_APPWINDOW.0 != 0;
        if is_tool_window && !is_app_window {
            return None;
        }

        if ex_style & WS_EX_NOACTIVATE.0 != 0 {
            return None;
        }

        // Skip owned windows
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.is_invalid() {
                return None;
            }
        }

        // Get title (allow empty for UWP apps still loading)
        let title_len = GetWindowTextLengthW(hwnd);
        let title = if title_len > 0 {
            let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
            let actual_len = GetWindowTextW(hwnd, &mut title_buf);
            String::from_utf16_lossy(&title_buf[..actual_len as usize])
        } else {
            String::new()
        };

        // Get class name
        let mut class_buf: Vec<u16> = vec![0; 256];
        let class_len = GetClassNameW(hwnd, &mut class_buf);
        let class_name = if class_len > 0 {
            String::from_utf16_lossy(&class_buf[..class_len as usize])
        } else {
            String::new()
        };

        if should_skip_window_by_class(&class_name) {
            return None;
        }

        // Get process ID
        let mut process_id: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));

        // Get window rect
        let mut win_rect = RECT::default();
        if GetWindowRect(hwnd, &mut win_rect).is_err() {
            return None;
        }

        let rect = Rect::new(
            win_rect.left,
            win_rect.top,
            win_rect.right - win_rect.left,
            win_rect.bottom - win_rect.top,
        );

        if rect.width == 0 || rect.height == 0 {
            return None;
        }

        Some(WindowInfo {
            hwnd: hwnd_id,
            title,
            class_name,
            process_id,
            rect,
            visible: true,
        })
    }
}

/// - Invisible windows
/// - Tool windows (WS_EX_TOOLWINDOW without WS_EX_APPWINDOW)
/// - Windows with empty titles
/// - Cloaked windows
/// - Windows with WS_EX_NOACTIVATE
pub fn enumerate_windows() -> Result<Vec<WindowInfo>, Win32Error> {
    let mut windows: Vec<WindowInfo> = Vec::new();

    unsafe {
        // EnumWindows callback receives a raw pointer to our Vec
        let windows_ptr = &mut windows as *mut Vec<WindowInfo>;

        let result = EnumWindows(Some(enum_windows_callback), LPARAM(windows_ptr as isize));

        if result.is_err() {
            return Err(Win32Error::EnumerationFailed(
                "EnumWindows failed".to_string(),
            ));
        }
    }

    tracing::debug!("Enumerated {} manageable windows", windows.len());
    Ok(windows)
}

/// Get the primary monitor's information.
///
/// Returns the work area (excluding taskbar) which is suitable for window positioning.
pub fn get_primary_monitor() -> Result<MonitorInfo, Win32Error> {
    let monitors = enumerate_monitors()?;

    monitors
        .into_iter()
        .find(|m| m.is_primary)
        .ok_or_else(|| Win32Error::MonitorEnumerationFailed("No primary monitor found".to_string()))
}

/// Find which monitor contains the center of a given rectangle.
///
/// Returns the monitor info if found, or None if no monitor contains the point.
/// Falls back to primary monitor if no exact match.
pub fn find_monitor_for_rect<'a>(
    monitors: &'a [MonitorInfo],
    rect: &Rect,
) -> Option<&'a MonitorInfo> {
    // First, try to find a monitor that contains the rect's center
    let center_x = rect.x + rect.width / 2;
    let center_y = rect.y + rect.height / 2;

    monitors
        .iter()
        .find(|m| m.contains_point(center_x, center_y))
        .or_else(|| monitors.iter().find(|m| m.is_primary))
}

/// Find a monitor by its ID.
pub fn find_monitor_by_id(monitors: &[MonitorInfo], id: MonitorId) -> Option<&MonitorInfo> {
    monitors.iter().find(|m| m.id == id)
}

/// Get monitors sorted by position (left to right, then top to bottom).
pub fn monitors_by_position(monitors: &[MonitorInfo]) -> Vec<&MonitorInfo> {
    let mut sorted: Vec<_> = monitors.iter().collect();
    sorted.sort_by(|a, b| {
        // Sort by x first, then by y
        a.rect.x.cmp(&b.rect.x).then(a.rect.y.cmp(&b.rect.y))
    });
    sorted
}

/// Find the monitor to the left of the given monitor.
pub fn monitor_to_left(monitors: &[MonitorInfo], current_id: MonitorId) -> Option<&MonitorInfo> {
    let sorted = monitors_by_position(monitors);
    let current_idx = sorted.iter().position(|m| m.id == current_id)?;
    if current_idx > 0 {
        Some(sorted[current_idx - 1])
    } else {
        None
    }
}

/// Find the monitor to the right of the given monitor.
pub fn monitor_to_right(monitors: &[MonitorInfo], current_id: MonitorId) -> Option<&MonitorInfo> {
    let sorted = monitors_by_position(monitors);
    let current_idx = sorted.iter().position(|m| m.id == current_id)?;
    if current_idx + 1 < sorted.len() {
        Some(sorted[current_idx + 1])
    } else {
        None
    }
}

/// Enumerate all connected monitors.
///
/// Returns information about each display including work area (usable space
/// excluding taskbar and docked windows).
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, Win32Error> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();

    unsafe {
        let monitors_ptr = &mut monitors as *mut Vec<MonitorInfo>;

        let result = EnumDisplayMonitors(
            None, // HDC - None to enumerate all monitors
            None, // lprcClip - None to not clip
            Some(enum_monitors_callback),
            LPARAM(monitors_ptr as isize),
        );

        if !result.as_bool() {
            return Err(Win32Error::MonitorEnumerationFailed(
                "EnumDisplayMonitors failed".to_string(),
            ));
        }
    }

    if monitors.is_empty() {
        return Err(Win32Error::MonitorEnumerationFailed(
            "No monitors found".to_string(),
        ));
    }

    tracing::debug!("Enumerated {} monitors", monitors.len());
    Ok(monitors)
}

/// Callback for EnumDisplayMonitors that collects monitor info.
unsafe extern "system" fn enum_monitors_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprc_clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);

    // Initialize MONITORINFOEXW with correct size
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    if GetMonitorInfoW(hmonitor, &mut info as *mut MONITORINFOEXW as *mut _).as_bool() {
        let mon_rect = info.monitorInfo.rcMonitor;
        let work_rect = info.monitorInfo.rcWork;

        // Convert device name from wide string
        let device_name_len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let device_name = String::from_utf16_lossy(&info.szDevice[..device_name_len]);

        monitors.push(MonitorInfo {
            id: hmonitor.0 as MonitorId,
            rect: Rect::new(
                mon_rect.left,
                mon_rect.top,
                mon_rect.right - mon_rect.left,
                mon_rect.bottom - mon_rect.top,
            ),
            work_area: Rect::new(
                work_rect.left,
                work_rect.top,
                work_rect.right - work_rect.left,
                work_rect.bottom - work_rect.top,
            ),
            // MONITORINFOF_PRIMARY = 1
            is_primary: info.monitorInfo.dwFlags & 1 != 0,
            device_name,
        });

        TRUE
    } else {
        // Continue enumeration even if one monitor fails
        TRUE
    }
}

/// Callback for EnumWindows that filters and collects window info.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);

    // Skip invisible windows
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }

    // Skip minimized windows (e.g., tray apps like Raycast)
    if IsIconic(hwnd).as_bool() {
        return TRUE;
    }

    // Get window styles
    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

    // Skip if not visible style
    if style & WS_VISIBLE.0 == 0 {
        return TRUE;
    }

    // Skip tool windows (unless they have WS_EX_APPWINDOW)
    let is_tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
    let is_app_window = ex_style & WS_EX_APPWINDOW.0 != 0;
    if is_tool_window && !is_app_window {
        return TRUE;
    }

    // Skip windows with WS_EX_NOACTIVATE (tooltips, popups, etc.)
    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return TRUE;
    }

    // Skip owned windows (dialogs, secondary windows)
    if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
        if !owner.is_invalid() {
            return TRUE;
        }
    }

    // Skip cloaked windows (e.g., on other virtual desktops)
    if is_window_cloaked(hwnd) {
        return TRUE;
    }

    // Get window title
    let title_len = GetWindowTextLengthW(hwnd);
    if title_len == 0 {
        return TRUE; // Skip windows with no title
    }

    let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
    let actual_len = GetWindowTextW(hwnd, &mut title_buf);
    if actual_len == 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&title_buf[..actual_len as usize]);

    // Skip known system windows by title
    if should_skip_window_by_title(&title) {
        return TRUE;
    }

    // Get class name
    let mut class_buf: Vec<u16> = vec![0; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    let class_name = if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize])
    } else {
        String::new()
    };

    // Skip known system classes
    if should_skip_window_by_class(&class_name) {
        return TRUE;
    }

    // Get process ID
    let mut process_id: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));

    // Get window rect
    let mut win_rect = RECT::default();
    if GetWindowRect(hwnd, &mut win_rect).is_err() {
        return TRUE;
    }

    let rect = Rect::new(
        win_rect.left,
        win_rect.top,
        win_rect.right - win_rect.left,
        win_rect.bottom - win_rect.top,
    );

    // Skip zero-size windows
    if rect.width == 0 || rect.height == 0 {
        return TRUE;
    }

    windows.push(WindowInfo {
        hwnd: hwnd.0 as WindowId,
        title,
        class_name,
        process_id,
        rect,
        visible: true,
    });

    TRUE
}

/// Apply manageability filters to a window handle for WinEvent callback emission.
///
/// This mirrors enumeration filters so event callbacks don't emit churn for
/// windows we would never manage.
fn should_emit_window_event_with_policy(
    hwnd: HWND,
    require_visible: bool,
    require_title: bool,
    filter_cloaked: bool,
) -> bool {
    // Keep callback work best-effort and non-panicking.
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };

    if require_visible && !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return false;
    }

    if require_visible && style & WS_VISIBLE.0 == 0 {
        return false;
    }

    // Skip minimized windows (e.g., tray apps) unless we're handling
    // restore/minimize transitions where the window state is transient.
    if require_visible && unsafe { IsIconic(hwnd) }.as_bool() {
        return false;
    }

    let is_tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
    let is_app_window = ex_style & WS_EX_APPWINDOW.0 != 0;
    if is_tool_window && !is_app_window {
        return false;
    }

    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return false;
    }

    if let Ok(owner) = unsafe { GetWindow(hwnd, GW_OWNER) } {
        if !owner.is_invalid() {
            return false;
        }
    }

    if filter_cloaked && is_window_cloaked(hwnd) {
        return false;
    }

    if require_title {
        let title_len = unsafe { GetWindowTextLengthW(hwnd) };
        if title_len == 0 {
            return false;
        }

        let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
        let actual_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
        if actual_len == 0 {
            return false;
        }
        let title = String::from_utf16_lossy(&title_buf[..actual_len as usize]);
        if should_skip_window_by_title(&title) {
            return false;
        }
    }

    let mut class_buf: Vec<u16> = vec![0; 256];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    let class_name = if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize])
    } else {
        String::new()
    };

    if should_skip_window_by_class(&class_name) {
        return false;
    }

    true
}

pub(crate) fn should_emit_window_event(hwnd: HWND) -> bool {
    should_emit_window_event_with_policy(
        hwnd, true, // visible only
        true, // non-empty title
        true, // filter cloaked windows
    )
}

fn should_emit_window_event_for(event: u32, hwnd: HWND) -> bool {
    match event {
        // Creation can happen before title is set; keep manageability checks but
        // allow empty title and cloaked transitional states.
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => {
            should_emit_window_event_with_policy(hwnd, true, false, false)
        }
        // Focus must not be blocked by cloaked-state checks or empty titles.
        EVENT_SYSTEM_FOREGROUND | EVENT_OBJECT_FOCUS => {
            should_emit_window_event_with_policy(hwnd, true, false, false)
        }
        // Restore/minimize/destroy/hide should still pass basic top-level filtering,
        // but visibility/title can be transient during these transitions.
        EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND | EVENT_OBJECT_DESTROY
        | EVENT_OBJECT_HIDE => {
            should_emit_window_event_with_policy(hwnd, false, false, false)
        }
        EVENT_OBJECT_LOCATIONCHANGE => should_emit_window_event_with_policy(hwnd, true, true, true),
        EVENT_SYSTEM_MOVESIZESTART | EVENT_SYSTEM_MOVESIZEEND => {
            should_emit_window_event_with_policy(hwnd, false, false, false)
        }
        _ => false,
    }
}

fn should_filter_window_event_by_manageability(event: u32) -> bool {
    matches!(
        event,
        EVENT_OBJECT_CREATE
            | EVENT_OBJECT_DESTROY
            | EVENT_OBJECT_SHOW
            | EVENT_OBJECT_HIDE
            | EVENT_SYSTEM_FOREGROUND
            | EVENT_SYSTEM_MINIMIZESTART
            | EVENT_SYSTEM_MINIMIZEEND
            | EVENT_SYSTEM_MOVESIZESTART
            | EVENT_SYSTEM_MOVESIZEEND
            | EVENT_OBJECT_LOCATIONCHANGE
            | EVENT_OBJECT_FOCUS
    )
}

pub(crate) fn normalize_to_root_window(hwnd: HWND) -> HWND {
    let root_hwnd = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root_hwnd.0.is_null() {
        hwnd
    } else {
        root_hwnd
    }
}

/// Check if a window should be skipped based on its title.
fn should_skip_window_by_title(title: &str) -> bool {
    const SKIP_TITLES: &[&str] = &[
        "Program Manager",
        "Windows Input Experience",
        "Microsoft Text Input Application",
    ];

    SKIP_TITLES.contains(&title)
}

/// Check if a window is cloaked (hidden by DWM).
///
/// Only treats shell-cloaked windows (on other virtual desktops) as cloaked.
/// App-cloaked windows (UWP transitioning) are allowed through so that
/// ApplicationFrameWindow hosts like Settings are not filtered out.
fn is_window_cloaked(hwnd: HWND) -> bool {
    const DWM_CLOAKED_SHELL: u32 = 0x2;
    unsafe {
        let mut cloaked: u32 = 0;
        let result = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        match result {
            Ok(()) => cloaked & DWM_CLOAKED_SHELL != 0,
            Err(e) => {
                let window_is_valid = IsWindow(Some(hwnd)).as_bool();
                let treat_as_cloaked = should_treat_cloak_query_failure_as_cloaked(window_is_valid);
                tracing::debug!(
                    "DwmGetWindowAttribute(DWMWA_CLOAKED) failed for {:?}: {}. window_is_valid={} -> treat_as_cloaked={}",
                    hwnd,
                    e,
                    window_is_valid,
                    treat_as_cloaked
                );
                treat_as_cloaked
            }
        }
    }
}

fn should_treat_cloak_query_failure_as_cloaked(window_is_valid: bool) -> bool {
    !window_is_valid
}

/// Check whether coordinates indicate MoveOffScreen sentinel placement.
pub fn is_move_offscreen_sentinel_position(x: i32, y: i32) -> bool {
    x <= MOVE_OFFSCREEN_SENTINEL_COORD && y <= MOVE_OFFSCREEN_SENTINEL_COORD
}

/// Check whether a rectangle indicates MoveOffScreen sentinel placement.
pub fn is_move_offscreen_sentinel_rect(rect: &Rect) -> bool {
    is_move_offscreen_sentinel_position(rect.x, rect.y)
}

/// Move a single window to the off-screen sentinel position.
/// Used by workspace switching to hide inactive workspace windows.
pub fn move_window_offscreen(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        ) {
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to move window {} offscreen: {}",
                window_id, e
            )));
        }
    }
    Ok(())
}

fn move_offscreen_rect_for(rect: &Rect) -> Rect {
    Rect::new(
        MOVE_OFFSCREEN_SENTINEL_COORD,
        MOVE_OFFSCREEN_SENTINEL_COORD,
        rect.width,
        rect.height,
    )
}

fn compute_restore_rect_from_offscreen(current_rect: &Rect, work_area: &Rect) -> Rect {
    let max_width = work_area.width.max(1);
    let max_height = work_area.height.max(1);
    let width = current_rect.width.max(1).min(max_width);
    let height = current_rect.height.max(1).min(max_height);
    Rect::new(work_area.x, work_area.y, width, height)
}

fn restore_window_if_offscreen_to_work_area(
    window_id: WindowId,
    work_area: &Rect,
) -> Result<bool, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;

    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        let mut current_rect = RECT::default();
        GetWindowRect(hwnd, &mut current_rect).map_err(|e| {
            Win32Error::SetPositionFailed(format!(
                "GetWindowRect failed for window {}: {}",
                window_id, e
            ))
        })?;

        let current_rect = Rect::new(
            current_rect.left,
            current_rect.top,
            current_rect.right - current_rect.left,
            current_rect.bottom - current_rect.top,
        );

        if !is_move_offscreen_sentinel_rect(&current_rect) {
            return Ok(false);
        }

        let restore_rect = compute_restore_rect_from_offscreen(&current_rect, work_area);

        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            restore_rect.x,
            restore_rect.y,
            restore_rect.width,
            restore_rect.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(Win32Error::WindowNotFound(window_id));
            }
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to restore off-screen window {}: {}",
                window_id, e
            )));
        }
    }

    Ok(true)
}

/// Check if a window should be skipped based on its class name.
fn should_skip_window_by_class(class_name: &str) -> bool {
    const SKIP_CLASSES: &[&str] = &[
        "Progman",                    // Program Manager
        "Shell_TrayWnd",              // Taskbar
        "Shell_SecondaryTrayWnd",     // Secondary taskbar
        "WorkerW",                    // Desktop worker
        "Windows.UI.Core.CoreWindow", // UWP system windows
        // ApplicationFrameWindow removed: allows tiling UWP apps (Calculator, Photos, etc.)
        // Empty/cloaked UWP frames are already filtered by the cloaked window check.
        "XamlExplorerHostIslandWindow", // XAML islands
        "TopLevelWindowForOverflowXamlIsland", // Overflow islands
        "LeopardWMSettings",          // Our own settings window
        "LeopardWMBorderFrame",       // Our own border overlay
    ];

    SKIP_CLASSES.contains(&class_name)
}

// ============================================================================
// Process Information
// ============================================================================

/// Get the executable name for a process by PID.
///
/// Returns just the filename (e.g., "notepad.exe"), not the full path.
/// Returns None if the process cannot be accessed or doesn't exist.
pub fn get_process_executable(pid: u32) -> Option<String> {
    unsafe {
        // Open the process with limited query rights
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return None,
        };

        // Get the executable path — use extended-length buffer for long paths
        let mut buffer: Vec<u16> = vec![0; 1024];
        let len = K32GetModuleFileNameExW(Some(handle), None, &mut buffer);

        // Close the handle
        let _ = CloseHandle(handle);

        if len == 0 {
            return None;
        }

        // Convert to string and extract filename
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().map(|s| s.to_string())
    }
}

/// Check if a window handle is still valid.
///
/// This helps prevent race conditions where a window is destroyed
/// between receiving an event and processing it.
pub fn is_valid_window(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        IsWindow(Some(hwnd)).as_bool()
    }
}

/// Check if a window handle is valid and has `WS_VISIBLE` style.
///
/// Used to distinguish spurious `EVENT_OBJECT_HIDE` from Electron apps
/// (which fire hide on still-visible main windows) from genuine hide events.
pub fn is_window_visible(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool()
    }
}

/// Check if the Shift key is currently held down.
///
/// Uses `GetKeyState` to poll the keyboard state. Returns `true`
/// if the high bit is set (key is down).
pub fn is_shift_key_pressed() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

/// Get the current mouse cursor position in screen coordinates.
pub fn get_cursor_pos() -> Option<(i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = windows::Win32::Foundation::POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_ok() {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

/// Check if a managed window is still valid and visible.
///
/// Returns `false` if the window no longer exists, is not visible,
/// or is minimized (e.g., close-to-tray apps). Used to prune stale
/// windows from the layout that disappeared without firing events.
pub fn is_window_alive_and_visible(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        IsWindow(Some(hwnd)).as_bool()
            && IsWindowVisible(hwnd).as_bool()
            && !IsIconic(hwnd).as_bool()
    }
}

/// Cache of last-applied window placements, used to skip redundant SetWindowPos calls.
pub type PlacementCache = HashMap<WindowId, (Rect, Visibility)>;

/// Apply window placements from the layout engine.
///
/// Visible windows are positioned immediately via SetWindowPos.
/// Off-screen windows are moved to sentinel coordinates far off-screen.
///
/// When `cache` is provided, placements whose rect and visibility match the
/// cached values are skipped, avoiding redundant Win32 calls during animations
/// where most windows haven't moved.
pub fn apply_placements(
    placements: &[WindowPlacement],
    _config: &PlatformConfig,
    cache: Option<&mut PlacementCache>,
) -> Result<(), Win32Error> {
    if placements.is_empty() {
        return Ok(());
    }

    let mut applied = 0u32;
    let mut skipped = 0u32;

    // Collect all (hwnd, adjusted_rect, flags) entries for deferred positioning.
    // Pre-compute border insets and cache checks before the batch to minimize
    // time between BeginDeferWindowPos and EndDeferWindowPos.
    struct DeferEntry {
        hwnd: HWND,
        window_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
        column_index: usize,
    }
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());

    // Separate visible and off-screen windows
    let (visible, offscreen): (Vec<_>, Vec<_>) = placements
        .iter()
        .partition(|p| p.visibility == Visibility::Visible);

    // Prepare visible window entries
    for placement in &visible {
        if let Some(ref cache) = cache {
            if cache.get(&placement.window_id) == Some(&(placement.rect, placement.visibility)) {
                skipped += 1;
                continue;
            }
        }
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else { continue };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
        }
        let (inset_l, inset_t, inset_r, inset_b) = invisible_border_insets(hwnd);
        entries.push(DeferEntry {
            hwnd,
            window_id: placement.window_id,
            x: placement.rect.x - inset_l,
            y: placement.rect.y - inset_t,
            w: placement.rect.width + inset_l + inset_r,
            h: placement.rect.height + inset_t + inset_b,
            flags: SWP_NOZORDER | SWP_NOACTIVATE,
            column_index: placement.column_index,
        });
    }

    // Prepare off-screen window entries
    for placement in &offscreen {
        let offscreen_rect = move_offscreen_rect_for(&placement.rect);
        if let Some(ref cache) = cache {
            if cache.get(&placement.window_id)
                == Some(&(offscreen_rect, Visibility::OffScreenLeft))
            {
                skipped += 1;
                continue;
            }
        }
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else { continue };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
        }
        entries.push(DeferEntry {
            hwnd,
            window_id: placement.window_id,
            x: MOVE_OFFSCREEN_SENTINEL_COORD,
            y: MOVE_OFFSCREEN_SENTINEL_COORD,
            w: 0,
            h: 0,
            flags: SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            column_index: usize::MAX,
        });
    }

    // Track windows that failed positioning (excluded from cache).
    let mut failed_window_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Batch all SetWindowPos calls via DeferWindowPos for atomic repositioning.
    if !entries.is_empty() {
        unsafe {
            match BeginDeferWindowPos(entries.len() as i32) {
            Err(_) => {
                // Fallback: apply individually if batching fails
                for entry in &entries {
                    if SetWindowPos(
                        entry.hwnd, None,
                        entry.x, entry.y, entry.w, entry.h,
                        entry.flags,
                    ).is_err() {
                        failed_window_ids.insert(entry.window_id);
                    }
                }
                applied = (entries.len() - failed_window_ids.len()) as u32;
            }
            Ok(initial_hdwp) => {
                let mut hdwp = initial_hdwp;
                let mut batch_ok = true;
                for entry in &entries {
                    match DeferWindowPos(
                        hdwp, entry.hwnd, None,
                        entry.x, entry.y, entry.w, entry.h,
                        entry.flags,
                    ) {
                        Ok(new_hdwp) => hdwp = new_hdwp,
                        Err(_) => {
                            batch_ok = false;
                            break;
                        }
                    }
                }
                if batch_ok {
                    let _ = EndDeferWindowPos(hdwp);
                    applied = entries.len() as u32;
                } else {
                    // Release the HDWP handle before falling back
                    let _ = EndDeferWindowPos(hdwp);
                    // Batch was interrupted — fall back to individual calls
                    for entry in &entries {
                        if SetWindowPos(
                            entry.hwnd, None,
                            entry.x, entry.y, entry.w, entry.h,
                            entry.flags,
                        ).is_err() {
                            failed_window_ids.insert(entry.window_id);
                        }
                    }
                    applied = (entries.len() - failed_window_ids.len()) as u32;
                }
            }
            }
        }
    }

    // Fix-up pass: some windows enforce minimum sizes and Windows silently
    // resizes them, causing overlaps in stacked columns. Detect violations
    // and re-layout affected columns.
    {
        use std::collections::HashMap;
        // Group visible entries by column (only multi-window columns matter).
        let mut col_indices: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            if entry.column_index != usize::MAX && entry.x != MOVE_OFFSCREEN_SENTINEL_COORD {
                col_indices.entry(entry.column_index).or_default().push(i);
            }
        }
        for indices in col_indices.values() {
            if indices.len() < 2 {
                continue;
            }
            // Sort by y position (top to bottom).
            let mut sorted: Vec<usize> = indices.clone();
            sorted.sort_by_key(|&i| entries[i].y);

            // Check each window's actual height against requested.
            let mut needs_fixup = false;
            for &idx in &sorted {
                let entry = &entries[idx];
                let actual_h = unsafe {
                    let mut r = RECT::default();
                    if GetWindowRect(entry.hwnd, &mut r).is_ok() {
                        r.bottom - r.top
                    } else {
                        entry.h
                    }
                };
                if actual_h > entry.h + 2 {
                    needs_fixup = true;
                    break;
                }
            }
            if !needs_fixup {
                continue;
            }

            // Compute column bottom boundary from the last entry's original position.
            let last = &entries[*sorted.last().unwrap()];
            let column_bottom = last.y + last.h;

            // Re-layout: walk top-to-bottom, query actual heights, push
            // subsequent windows down. Last window absorbs remaining space.
            let mut current_y = entries[sorted[0]].y;
            for (pos, &idx) in sorted.iter().enumerate() {
                let entry = &entries[idx];
                let actual_h = unsafe {
                    let mut r = RECT::default();
                    if GetWindowRect(entry.hwnd, &mut r).is_ok() {
                        r.bottom - r.top
                    } else {
                        entry.h
                    }
                };
                let gap = if pos > 0 {
                    // Infer gap from original layout spacing.
                    let prev = &entries[sorted[pos - 1]];
                    (entry.y - (prev.y + prev.h)).max(0)
                } else {
                    0
                };
                let new_y = current_y + gap;
                let new_h = if pos == sorted.len() - 1 {
                    // Last window: fill remaining space.
                    (column_bottom - new_y).max(1)
                } else if actual_h > entry.h + 2 {
                    // This window enforces a minimum — use its actual height.
                    actual_h
                } else {
                    entry.h
                };
                if new_y != entry.y || new_h != entry.h {
                    tracing::debug!(
                        "Min-size fixup: {:?} y {} → {}, h {} → {}",
                        entry.hwnd, entry.y, new_y, entry.h, new_h,
                    );
                    unsafe {
                        let _ = SetWindowPos(
                            entry.hwnd, None,
                            entry.x, new_y, entry.w, new_h,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                current_y = new_y + new_h;
            }
        }
    }

    // Update the cache with only windows that were actually positioned
    // (skip windows that failed IsWindow/IsIconic checks or SetWindowPos failures)
    if let Some(cache) = cache {
        cache.clear();
        let positioned: std::collections::HashSet<u64> =
            entries.iter()
                .filter(|e| !failed_window_ids.contains(&e.window_id))
                .map(|e| e.window_id)
                .collect();
        for p in &visible {
            if positioned.contains(&p.window_id) {
                cache.insert(p.window_id, (p.rect, p.visibility));
            }
        }
        for p in &offscreen {
            if positioned.contains(&p.window_id) {
                let offscreen_rect = move_offscreen_rect_for(&p.rect);
                cache.insert(p.window_id, (offscreen_rect, Visibility::OffScreenLeft));
            }
        }
    }

    tracing::debug!(
        "Applied {} placements ({} skipped unchanged), {} off-screen total",
        applied,
        skipped,
        offscreen.len()
    );

    Ok(())
}

/// Set window position immediately using SetWindowPos.
/// Compute invisible border insets for a window.
///
/// Windows 10/11 windows have invisible borders (typically ~7px on left, right,
/// bottom and 0px on top). `SetWindowPos` operates on the full frame rect
/// including these borders. To make the *visible* area fill our target rect,
/// we expand the frame rect by the invisible border amount.
///
/// Returns (left, top, right, bottom) insets to subtract/add to the target rect.
fn invisible_border_insets(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut frame_rect = RECT::default();
        if GetWindowRect(hwnd, &mut frame_rect).is_err() {
            return (0, 0, 0, 0);
        }

        let mut extended_rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut extended_rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            return (0, 0, 0, 0);
        }

        // Insets = how much the frame rect extends beyond the visible area
        let left = extended_rect.left - frame_rect.left;
        let top = extended_rect.top - frame_rect.top;
        let right = frame_rect.right - extended_rect.right;
        let bottom = frame_rect.bottom - extended_rect.bottom;

        (left.max(0), top.max(0), right.max(0), bottom.max(0))
    }
}

/// Set the foreground window using Win32 SetForegroundWindow.
///
/// Uses AttachThreadInput trick to reliably set foreground even when
/// the calling process is not the foreground process.
pub fn set_foreground_window(hwnd: WindowId) -> Result<bool, Win32Error> {
    let window_id = hwnd;
    let hwnd = window_id_to_hwnd(window_id)?;

    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            if IsIconic(hwnd).as_bool() {
                return Err(Win32Error::SetPositionFailed(format!(
                    "Failed to restore minimized window {} before setting foreground",
                    window_id
                )));
            }
        }

        let target_thread = GetWindowThreadProcessId(hwnd, None);
        if target_thread == 0 {
            return Err(Win32Error::SetPositionFailed(format!(
                "GetWindowThreadProcessId returned 0 for window {}",
                window_id
            )));
        }
        let current_thread = GetCurrentThreadId();
        let mut diagnostics: Vec<String> = Vec::new();

        // Attach to the target thread's input queue to allow SetForegroundWindow
        let mut attached = false;
        if target_thread != current_thread {
            if windows::Win32::System::Threading::AttachThreadInput(
                current_thread,
                target_thread,
                true,
            )
            .as_bool()
            {
                attached = true;
            } else {
                diagnostics.push(format!(
                    "AttachThreadInput attach failed (current_thread={}, target_thread={})",
                    current_thread, target_thread
                ));
            }
        }

        let mut foreground_set = SetForegroundWindow(hwnd).as_bool();

        // If SetForegroundWindow failed, try BringWindowToTop as fallback
        if !foreground_set {
            match BringWindowToTop(hwnd) {
                Ok(()) => {
                    foreground_set = SetForegroundWindow(hwnd).as_bool();
                    if !foreground_set {
                        diagnostics.push(
                            "SetForegroundWindow returned FALSE after BringWindowToTop fallback"
                                .to_string(),
                        );
                    }
                }
                Err(e) => diagnostics.push(format!("BringWindowToTop failed: {}", e)),
            }
        }

        // Detach thread input
        if attached
            && !windows::Win32::System::Threading::AttachThreadInput(
                current_thread,
                target_thread,
                false,
            )
            .as_bool()
        {
            diagnostics.push(format!(
                "AttachThreadInput detach failed (current_thread={}, target_thread={})",
                current_thread, target_thread
            ));
        }

        if foreground_set {
            if !diagnostics.is_empty() {
                tracing::warn!(
                    "Foreground set for window {} with warnings: {}",
                    window_id,
                    diagnostics.join("; ")
                );
            }
            return Ok(true);
        }

        if diagnostics.is_empty() {
            // No explicit API error, but Windows denied foreground change.
            return Ok(false);
        }

        Err(Win32Error::SetPositionFailed(format!(
            "Failed to set foreground window {}: {}",
            window_id,
            diagnostics.join("; ")
        )))
    }
}

/// Close a window by posting WM_CLOSE.
///
/// This is a graceful close that allows the application to handle cleanup.
pub fn close_window(hwnd: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(hwnd)?;
    unsafe {
        const WM_CLOSE: u32 = 0x0010;
        PostMessageW(
            Some(hwnd),
            WM_CLOSE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .map_err(|e| {
            Win32Error::SetPositionFailed(format!("PostMessageW(WM_CLOSE) failed: {}", e))
        })?;
    }
    Ok(())
}

/// Set the DWM border color for a window (Windows 11+).
///
/// Returns Ok(true) if the border was set, Ok(false) if the API is unsupported.
pub fn set_window_border_color(hwnd: WindowId, color: u32) -> Result<bool, Win32Error> {
    let window_id = hwnd;
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        // DWMWA_BORDER_COLOR = 34
        const DWMWA_BORDER_COLOR: u32 = 34;
        let colorref = color;
        let result = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(DWMWA_BORDER_COLOR as i32),
            &colorref as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        match result {
            Ok(()) => Ok(true),
            Err(e) => {
                if !IsWindow(Some(hwnd)).as_bool() {
                    return Err(Win32Error::WindowNotFound(window_id));
                }

                if is_border_color_unsupported_hresult(e.code()) {
                    return Ok(false);
                }

                Err(Win32Error::SetPositionFailed(format!(
                    "DwmSetWindowAttribute(DWMWA_BORDER_COLOR) failed for {:?}: {}",
                    hwnd, e
                )))
            }
        }
    }
}

/// Reset the DWM border color for a window to the default.
///
/// Returns Ok(true) if the border was reset, Ok(false) if the API is unsupported.
pub fn reset_window_border_color(hwnd: WindowId) -> Result<bool, Win32Error> {
    // DWMWA_COLOR_DEFAULT = 0xFFFFFFFF
    set_window_border_color(hwnd, 0xFFFFFFFF)
}

fn is_border_color_unsupported_hresult(code: windows::core::HRESULT) -> bool {
    const E_INVALIDARG_HRESULT: i32 = 0x8007_0057u32 as i32;
    const E_NOTIMPL_HRESULT: i32 = 0x8000_4001u32 as i32;
    code.0 == E_INVALIDARG_HRESULT || code.0 == E_NOTIMPL_HRESULT
}

/// Restore one window from MoveOffScreen sentinel coordinates to the primary monitor.
///
/// Returns `Ok(true)` if the window was restored, `Ok(false)` if it was not at
/// sentinel coordinates, and `Err` if restore operations failed.
pub fn restore_window_moved_offscreen(window_id: WindowId) -> Result<bool, Win32Error> {
    let primary = get_primary_monitor()?;
    restore_window_if_offscreen_to_work_area(window_id, &primary.work_area)
}

fn restore_windows_moved_offscreen_with_work_area<F>(
    window_ids: &[WindowId],
    work_area: &Rect,
    mut restore_one: F,
) -> (usize, Vec<String>)
where
    F: FnMut(WindowId, &Rect) -> Result<bool, Win32Error>,
{
    let mut restored_count: usize = 0;
    let mut failures: Vec<String> = Vec::new();

    for &window_id in window_ids {
        match restore_one(window_id, work_area) {
            Ok(true) => restored_count += 1,
            Ok(false) => {}
            Err(e) if is_benign_side_effect_error(&e) => {
                tracing::debug!(
                    "Ignoring benign race during MoveOffScreen restore for {}: {}",
                    window_id,
                    e
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to restore off-screen window {} during shutdown recovery: {}",
                    window_id,
                    e
                );
                failures.push(format!("window {}: {}", window_id, e));
            }
        }
    }

    (restored_count, failures)
}

/// Restore all windows currently parked at MoveOffScreen sentinel coordinates.
///
/// Returns the number of restored windows. If any window restore fails, this
/// returns an aggregated error after attempting all windows.
pub fn restore_windows_moved_offscreen(window_ids: &[WindowId]) -> Result<usize, Win32Error> {
    if window_ids.is_empty() {
        return Ok(0);
    }

    let primary = get_primary_monitor()?;
    let (restored_count, failures) = restore_windows_moved_offscreen_with_work_area(
        window_ids,
        &primary.work_area,
        restore_window_if_offscreen_to_work_area,
    );

    if !failures.is_empty() {
        return Err(combine_operation_failures(
            "Failed to restore one or more MoveOffScreen windows",
            failures,
        ));
    }

    Ok(restored_count)
}

/// Restore managed windows to their visible positions, best-effort.
///
/// Resets border colors and restores windows parked at MoveOffScreen
/// sentinel coordinates. Logs warnings for failures but never panics.
pub fn uncloak_all_managed_windows(window_ids: &[WindowId]) {
    for &wid in window_ids {
        if wid == 0 {
            continue;
        }
        let _ = reset_window_border_color(wid);
    }

    if let Err(e) = restore_windows_moved_offscreen(window_ids) {
        tracing::warn!(
            "MoveOffScreen shutdown recovery had one or more failures: {}",
            e
        );
    }

    tracing::info!(
        "Restored {} managed windows during shutdown",
        window_ids.len()
    );
}

unsafe extern "system" fn collect_all_window_ids_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let window_ids = &mut *(lparam.0 as *mut Vec<WindowId>);
    let window_id = hwnd.0 as WindowId;
    if window_id != 0 {
        window_ids.push(window_id);
    }
    TRUE
}

fn collect_all_top_level_window_ids() -> Vec<WindowId> {
    let mut window_ids: Vec<WindowId> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_all_window_ids_callback),
            LPARAM((&mut window_ids as *mut Vec<WindowId>) as isize),
        );
    }
    window_ids
}

/// Restore any top-level window parked at MoveOffScreen sentinel coordinates.
///
/// This helper is panic-safe and best-effort, making it suitable for panic
/// hooks where daemon state may be unavailable or poisoned.
pub fn restore_all_windows_moved_offscreen_best_effort() -> usize {
    let primary = match get_primary_monitor() {
        Ok(primary) => primary,
        Err(e) => {
            eprintln!(
                "[leopardwm] Emergency MoveOffScreen restore skipped: no primary monitor ({})",
                e
            );
            return 0;
        }
    };

    let window_ids = collect_all_top_level_window_ids();
    let (restored_count, failures) = restore_windows_moved_offscreen_with_work_area(
        &window_ids,
        &primary.work_area,
        restore_window_if_offscreen_to_work_area,
    );

    if !failures.is_empty() {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore had {} hard failure(s)",
            failures.len()
        );
    }

    if restored_count > 0 {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore recovered {} window(s)",
            restored_count
        );
    }

    restored_count
}

/// Restore all visible windows on the system, best-effort.
///
/// Restores any windows parked at MoveOffScreen sentinel coordinates.
/// This does not require AppState and works even if state is poisoned,
/// making it suitable for use in panic hooks.
pub fn uncloak_all_visible_windows() {
    let _ = restore_all_windows_moved_offscreen_best_effort();
    // eprintln because tracing may not work in a panic hook
    eprintln!("[leopardwm] Emergency window restore complete");
}

/// Cascade windows starting at (0, 0) on the primary monitor work area.
///
/// Each window is sized to 60% of the work area and offset by 30px from the
/// previous one. Off-screen windows are first restored, then cascaded.
pub fn cascade_windows(window_ids: &[WindowId]) {
    let work_area = match get_primary_monitor() {
        Ok(m) => m.work_area,
        Err(_) => Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
    };

    // First restore any windows that are off-screen
    let _ = restore_all_windows_moved_offscreen_best_effort();

    // Use height as the base so windows look reasonable on ultrawide monitors
    let cascade_h = (work_area.height as f64 * 0.5) as i32;
    let cascade_w = (cascade_h as f64 * 1.33) as i32; // 4:3 aspect ratio
    let step = 30;

    let placements: Vec<WindowPlacement> = window_ids
        .iter()
        .enumerate()
        .map(|(i, &wid)| {
            let offset = (i as i32) * step;
            WindowPlacement {
                window_id: wid,
                rect: Rect {
                    x: work_area.x + offset,
                    y: work_area.y + offset,
                    width: cascade_w,
                    height: cascade_h,
                },
                visibility: Visibility::Visible,
                column_index: 0,
            }
        })
        .collect();

    // Restore minimized windows first
    for &wid in window_ids {
        let hwnd = HWND(wid as *mut c_void);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }
    }

    let _ = apply_placements(&placements, &PlatformConfig, None);
}

/// Set the process DPI awareness to Per-Monitor Aware V2.
///
/// This must be called as early as possible in `main()`, before any
/// window or GDI operations. Returns `true` if the call succeeded.
pub fn set_dpi_awareness() -> bool {
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok()
    }
}

/// Window event types that the daemon needs to handle.
#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// A new window was created.
    Created(WindowId),
    /// A window was destroyed.
    Destroyed(WindowId),
    /// A window was hidden (e.g., close-to-tray apps using ShowWindow(SW_HIDE)).
    Hidden(WindowId),
    /// A window received focus.
    Focused(WindowId),
    /// A window was minimized.
    Minimized(WindowId),
    /// A window was restored from minimized state.
    Restored(WindowId),
    /// A window was moved or resized by the user.
    MovedOrResized(WindowId),
    /// User started dragging/resizing a window.
    MoveSizeStart(WindowId),
    /// User finished dragging/resizing a window.
    MoveSizeEnd(WindowId),
    /// Display configuration changed (monitors added/removed/rearranged).
    DisplayChange,
    /// Mouse cursor entered a window (for focus-follows-mouse).
    MouseEnterWindow(WindowId),
}

/// Global sender for window events from WinEvent callbacks.
///
/// This uses a thread-safe channel because WinEvent callbacks run on Windows'
/// internal thread pool and we need to forward events to the async runtime.
static EVENT_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);

fn set_event_sender(sender: mpsc::Sender<WindowEvent>) -> Result<(), Win32Error> {
    let mut guard = EVENT_SENDER
        .lock()
        .map_err(|_| Win32Error::HookInstallFailed("Event sender mutex poisoned".to_string()))?;
    if guard.is_some() {
        return Err(Win32Error::HookInstallFailed(
            "Event sender already initialized - drop existing EventHookHandle first".to_string(),
        ));
    }
    *guard = Some(sender);
    Ok(())
}

fn clear_event_sender() {
    let mut guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    *guard = None;
}

fn clone_event_sender() -> Option<mpsc::Sender<WindowEvent>> {
    let guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    guard.as_ref().cloned()
}

/// Handle for installed event hooks.
///
/// Dropping this handle will unhook all installed event hooks.
/// Custom message ID used to signal the WinEvent hook thread to exit.
const WM_QUIT_WINEVENT_THREAD: u32 = WM_USER + 3;

pub struct EventHookHandle {
    thread_id: u32,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EventHookHandle {
    fn drop(&mut self) {
        // Signal the dedicated thread to exit
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT_WINEVENT_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(handle) = self._thread.take() {
            let _ = handle.join();
        }
        clear_event_sender();
        tracing::debug!("WinEvent hook thread stopped");
    }
}

/// Install WinEvent hooks to receive window lifecycle events.
///
/// Spawns a dedicated thread with a Win32 message pump so that
/// `WINEVENT_OUTOFCONTEXT` callbacks are dispatched reliably.
///
/// Returns a handle that must be kept alive to receive events.
/// Also returns a receiver channel for the events.
///
/// # Events Hooked
/// - Window creation (EVENT_OBJECT_CREATE)
/// - Window destruction (EVENT_OBJECT_DESTROY)
/// - Foreground change (EVENT_SYSTEM_FOREGROUND)
/// - Minimize/restore (EVENT_SYSTEM_MINIMIZESTART/END)
/// - Drag start/end (EVENT_SYSTEM_MOVESIZESTART/END)
/// - Move/resize (EVENT_OBJECT_LOCATIONCHANGE)
/// - Focus within app (EVENT_OBJECT_FOCUS)
pub fn install_event_hooks() -> Result<(EventHookHandle, mpsc::Receiver<WindowEvent>), Win32Error> {
    // Create channel for events
    let (tx, rx) = mpsc::channel();

    // Store sender globally for callback access
    set_event_sender(tx)?;

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("winevent-pump".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();

                // Ensure message queue exists before installing hooks
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Define events to hook: (min_event, max_event)
                let event_ranges = [
                    (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
                    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                    (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
                    (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
                    (EVENT_OBJECT_FOCUS, EVENT_OBJECT_FOCUS),
                ];

                let mut hooks = Vec::new();

                for (min_event, max_event) in event_ranges {
                    let hook = SetWinEventHook(
                        min_event,
                        max_event,
                        None,
                        Some(win_event_callback),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    );

                    if hook.is_invalid() {
                        for h in &hooks {
                            let _ = UnhookWinEvent(*h);
                        }
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "SetWinEventHook failed for events {}-{}",
                            min_event, max_event
                        ))));
                        return;
                    }

                    hooks.push(hook);
                }

                tracing::info!("Installed {} WinEvent hooks", hooks.len());
                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WINEVENT_OUTOFCONTEXT callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_WINEVENT_THREAD {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                // Clean up hooks
                for hook in &hooks {
                    if !UnhookWinEvent(*hook).as_bool() {
                        tracing::warn!("Failed to unhook WinEvent: {:?}", hook);
                    }
                }
            }
        })
        .map_err(|e| {
            Win32Error::HookInstallFailed(format!("Failed to spawn winevent-pump thread: {}", e))
        })?;

    // Wait for init result
    match init_rx.recv() {
        Ok(Ok(thread_id)) => Ok((
            EventHookHandle {
                thread_id,
                _thread: Some(thread),
            },
            rx,
        )),
        Ok(Err(e)) => {
            let _ = thread.join();
            clear_event_sender();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            clear_event_sender();
            Err(Win32Error::HookInstallFailed(
                "WinEvent hook thread exited during init".to_string(),
            ))
        }
    }
}

/// Callback function for WinEvent hooks.
///
/// This runs on Windows' thread pool, so we forward events to the channel.
/// Wrapped with catch_unwind to prevent panics from crashing the application.
unsafe extern "system" fn win_event_callback(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    id_event_thread: u32,
    dwms_event_time: u32,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        win_event_callback_inner(
            hook,
            event,
            hwnd,
            id_object,
            id_child,
            id_event_thread,
            dwms_event_time,
        )
    }));

    if let Err(e) = result {
        // Can't use tracing here safely in all contexts, use eprintln
        eprintln!("Panic in win_event_callback: {:?}", e);
    }
}

/// Inner implementation of WinEvent callback.
fn win_event_callback_inner(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Only handle window-level events (not child objects like menus).
    // Exception: EVENT_OBJECT_FOCUS fires with OBJID_CLIENT, and
    // EVENT_SYSTEM_* events always have id_object == 0, so we allow
    // focus/foreground events regardless of id_object.
    // EVENT_SYSTEM_FOREGROUND and EVENT_OBJECT_FOCUS fire with id_object == 0
    // or OBJID_CLIENT, so allow them regardless. But EVENT_OBJECT_SHOW/HIDE
    // must be OBJID_WINDOW only — child control visibility changes should not
    // be emitted as top-level window lifecycle events.
    let is_focus_event = matches!(
        event,
        EVENT_SYSTEM_FOREGROUND | EVENT_OBJECT_FOCUS
    );
    if id_object != OBJID_WINDOW && !is_focus_event {
        return;
    }

    // Ignore invalid HWNDs
    if hwnd.0.is_null() {
        return;
    }

    // For destroy/hide events, skip normalization — the window may already be gone,
    // and GetAncestor would return null. Use the HWND as-is.
    let hwnd = if matches!(event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
        hwnd
    } else {
        normalize_to_root_window(hwnd)
    };

    if should_filter_window_event_by_manageability(event)
        && !should_emit_window_event_for(event, hwnd)
    {
        return;
    }

    let window_id = hwnd.0 as WindowId;

    // Map event to our WindowEvent type
    let window_event = match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => WindowEvent::Created(window_id),
        EVENT_OBJECT_DESTROY => WindowEvent::Destroyed(window_id),
        EVENT_OBJECT_HIDE => WindowEvent::Hidden(window_id),
        EVENT_SYSTEM_FOREGROUND | EVENT_OBJECT_FOCUS => WindowEvent::Focused(window_id),
        EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized(window_id),
        EVENT_SYSTEM_MINIMIZEEND => WindowEvent::Restored(window_id),
        EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MoveSizeStart(window_id),
        EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MoveSizeEnd(window_id),
        EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::MovedOrResized(window_id),
        _ => return,
    };

    // Send event through channel
    if let Some(sender) = clone_event_sender() {
        let _ = sender.send(window_event);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    static GLOBAL_SENDER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_event_sender_can_be_reinstalled_after_clear() {
        let _guard = GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        clear_event_sender();

        let (first_tx, _first_rx) = mpsc::channel::<WindowEvent>();
        assert!(set_event_sender(first_tx).is_ok());

        let (second_tx, _second_rx) = mpsc::channel::<WindowEvent>();
        let err = set_event_sender(second_tx).unwrap_err();
        assert!(matches!(err, Win32Error::HookInstallFailed(_)));

        clear_event_sender();

        let (third_tx, _third_rx) = mpsc::channel::<WindowEvent>();
        assert!(set_event_sender(third_tx).is_ok());
        clear_event_sender();
    }

    #[test]
    fn test_platform_config_default() {
        let _config = PlatformConfig::default();
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_enumerate_monitors() {
        let result = enumerate_monitors();
        if let Ok(monitors) = result {
            assert!(!monitors.is_empty(), "At least one monitor should exist");
            for monitor in &monitors {
                assert!(monitor.rect.width > 0, "Monitor width should be positive");
                assert!(monitor.rect.height > 0, "Monitor height should be positive");
                assert!(
                    monitor.work_area.width > 0,
                    "Work area width should be positive"
                );
                assert!(
                    monitor.work_area.height > 0,
                    "Work area height should be positive"
                );
            }
        }
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_get_primary_monitor() {
        let result = get_primary_monitor();
        if let Ok(primary) = result {
            assert!(
                primary.is_primary,
                "Primary monitor should be marked as primary"
            );
            assert!(primary.rect.width > 0);
            assert!(primary.work_area.width > 0);
        }
    }

    #[test]
    fn test_monitor_contains_point() {
        let monitor = MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
        };

        // Point inside monitor
        assert!(monitor.contains_point(960, 540));
        // Point at origin
        assert!(monitor.contains_point(0, 0));
        // Point just inside right edge
        assert!(monitor.contains_point(1919, 540));
        // Point outside (right edge)
        assert!(!monitor.contains_point(1920, 540));
        // Point outside (negative)
        assert!(!monitor.contains_point(-1, 0));
    }

    #[test]
    fn test_monitor_contains_rect_center() {
        let monitor = MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
        };

        // Window centered in monitor
        let window = Rect::new(100, 100, 800, 600);
        assert!(monitor.contains_rect_center(&window));

        // Window mostly outside but center inside
        let window2 = Rect::new(-300, 100, 800, 600);
        assert!(monitor.contains_rect_center(&window2)); // Center at 100, 400

        // Window with center outside
        let window3 = Rect::new(1800, 100, 800, 600);
        assert!(!monitor.contains_rect_center(&window3)); // Center at 2200, 400
    }

    #[test]
    fn test_find_monitor_for_rect() {
        let monitors = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
        ];

        // Window on first monitor
        let window1 = Rect::new(100, 100, 800, 600);
        let found = find_monitor_for_rect(&monitors, &window1);
        assert_eq!(found.unwrap().id, 1);

        // Window on second monitor
        let window2 = Rect::new(2000, 100, 800, 600);
        let found = find_monitor_for_rect(&monitors, &window2);
        assert_eq!(found.unwrap().id, 2);
    }

    #[test]
    fn test_monitors_by_position() {
        let monitors = vec![
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
        ];

        let sorted = monitors_by_position(&monitors);
        assert_eq!(sorted[0].id, 1); // Left monitor first
        assert_eq!(sorted[1].id, 2); // Right monitor second
    }

    #[test]
    fn test_monitor_to_left_right() {
        let monitors = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
        ];

        // From monitor 1, go right
        let right = monitor_to_right(&monitors, 1);
        assert_eq!(right.unwrap().id, 2);

        // From monitor 2, go left
        let left = monitor_to_left(&monitors, 2);
        assert_eq!(left.unwrap().id, 1);

        // From monitor 1, can't go left (edge)
        let no_left = monitor_to_left(&monitors, 1);
        assert!(no_left.is_none());

        // From monitor 2, can't go right (edge)
        let no_right = monitor_to_right(&monitors, 2);
        assert!(no_right.is_none());
    }


    #[test]
    fn test_win32_error_display() {
        // Verify error types have proper Display implementations
        let set_pos_err = Win32Error::SetPositionFailed("test error".to_string());
        let display = format!("{}", set_pos_err);
        assert!(display.contains("test error"));
        assert!(display.contains("position"));

        let window_not_found = Win32Error::WindowNotFound(12345);
        let display = format!("{}", window_not_found);
        assert!(display.contains("12345"));
    }

    #[test]
    fn test_is_benign_side_effect_error_only_for_nonzero_not_found() {
        assert!(is_benign_side_effect_error(&Win32Error::WindowNotFound(
            123
        )));
        assert!(!is_benign_side_effect_error(&Win32Error::WindowNotFound(0)));
        assert!(!is_benign_side_effect_error(
            &Win32Error::SetPositionFailed("hard failure".to_string())
        ));
    }

    #[test]
    fn test_should_filter_window_event_by_manageability_covers_hooked_events() {
        assert!(should_filter_window_event_by_manageability(
            EVENT_OBJECT_CREATE
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_OBJECT_LOCATIONCHANGE
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_SYSTEM_FOREGROUND
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_OBJECT_FOCUS
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_OBJECT_DESTROY
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_SYSTEM_MINIMIZESTART
        ));
        assert!(should_filter_window_event_by_manageability(
            EVENT_SYSTEM_MINIMIZEEND
        ));
    }

    #[test]
    fn test_should_treat_cloak_query_failure_as_cloaked_only_for_invalid_windows() {
        assert!(!should_treat_cloak_query_failure_as_cloaked(true));
        assert!(should_treat_cloak_query_failure_as_cloaked(false));
    }

    #[test]
    fn test_is_border_color_unsupported_hresult_mapping() {
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8007_0057u32 as i32
        )));
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8000_4001u32 as i32
        )));
        assert!(!is_border_color_unsupported_hresult(
            windows::core::HRESULT(0x8000_4005u32 as i32)
        ));
    }

    #[test]
    fn test_restore_windows_moved_offscreen_with_work_area_ignores_benign_races() {
        let window_ids = [10, 20, 30];
        let work_area = Rect::new(0, 0, 1920, 1080);
        let mut seen: Vec<WindowId> = Vec::new();
        let (restored, failures) = restore_windows_moved_offscreen_with_work_area(
            &window_ids,
            &work_area,
            |window_id, _| {
                seen.push(window_id);
                match window_id {
                    10 => Ok(true),
                    20 => Err(Win32Error::WindowNotFound(20)),
                    30 => Ok(false),
                    _ => unreachable!(),
                }
            },
        );

        assert_eq!(seen, window_ids);
        assert_eq!(restored, 1);
        assert!(failures.is_empty());
    }

    #[test]
    fn test_restore_windows_moved_offscreen_with_work_area_reports_hard_failures() {
        let window_ids = [7, 8];
        let work_area = Rect::new(0, 0, 1920, 1080);
        let (restored, failures) = restore_windows_moved_offscreen_with_work_area(
            &window_ids,
            &work_area,
            |window_id, _| match window_id {
                7 => Ok(true),
                8 => Err(Win32Error::SetPositionFailed("boom".to_string())),
                _ => unreachable!(),
            },
        );

        assert_eq!(restored, 1);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("window 8"));
        assert!(failures[0].contains("boom"));
    }

    #[test]
    fn test_apply_placements_empty() {
        // Verify empty placements succeed without error
        let config = PlatformConfig::default();
        let result = apply_placements(&[], &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apply_placements_skips_invalid_windows() {
        let config = PlatformConfig;
        let placements = vec![WindowPlacement {
            window_id: 0,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        }];

        // Invalid windows (hwnd 0) are silently skipped in the deferred batch
        let result = apply_placements(&placements, &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_move_offscreen_sentinel_detection() {
        assert!(is_move_offscreen_sentinel_position(
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD
        ));
        assert!(is_move_offscreen_sentinel_position(
            MOVE_OFFSCREEN_SENTINEL_COORD - 1,
            MOVE_OFFSCREEN_SENTINEL_COORD - 500
        ));
        assert!(!is_move_offscreen_sentinel_position(
            MOVE_OFFSCREEN_SENTINEL_COORD + 1,
            MOVE_OFFSCREEN_SENTINEL_COORD
        ));
        assert!(!is_move_offscreen_sentinel_position(
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD + 1
        ));
    }

    #[test]
    fn test_move_offscreen_sentinel_does_not_match_minimized_coordinates() {
        // Windows commonly reports minimized windows around (-32000, -32000).
        assert!(!is_move_offscreen_sentinel_position(-32_000, -32_000));
    }

    #[test]
    fn test_move_offscreen_restore_rect_clamps_size() {
        let offscreen = Rect::new(
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            5000,
            0,
        );
        let work_area = Rect::new(100, 200, 1920, 1080);
        let restored = compute_restore_rect_from_offscreen(&offscreen, &work_area);

        assert_eq!(restored.x, 100);
        assert_eq!(restored.y, 200);
        assert_eq!(restored.width, 1920);
        assert_eq!(restored.height, 1);
        assert!(is_move_offscreen_sentinel_rect(&offscreen));
        assert!(!is_move_offscreen_sentinel_rect(&restored));
    }

    #[test]
    fn test_restore_windows_moved_offscreen_empty_list() {
        let result = restore_windows_moved_offscreen(&[]);
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_window_id_to_hwnd_zero_returns_error() {
        let result = window_id_to_hwnd(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_window_id_to_hwnd_nonzero_succeeds() {
        let result = window_id_to_hwnd(12345);
        assert!(result.is_ok());
    }

    #[test]
    fn test_is_valid_window_zero_returns_false() {
        assert!(!is_valid_window(0));
    }

    #[test]
    fn test_set_foreground_window_zero_fails() {
        let result = set_foreground_window(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_foreground_window_invalid_hwnd_fails() {
        let result = set_foreground_window(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_close_window_zero_fails() {
        let result = close_window(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_window_border_color_zero_fails() {
        let result = set_window_border_color(0, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_window_border_color_invalid_hwnd_fails() {
        let result = set_window_border_color(u64::MAX, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_reset_window_border_color_zero_fails() {
        let result = reset_window_border_color(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_skip_classes_does_not_contain_application_frame_window() {
        let skip = should_skip_window_by_class("ApplicationFrameWindow");
        assert!(
            !skip,
            "ApplicationFrameWindow should NOT be in skip list (UWP apps should be tiled)"
        );
    }

    #[test]
    fn test_uncloak_all_managed_empty_list() {
        // Should not panic with an empty list
        uncloak_all_managed_windows(&[]);
    }

    #[test]
    fn test_uncloak_all_managed_with_invalid_ids() {
        // Should not panic even with invalid window IDs (best-effort)
        uncloak_all_managed_windows(&[0, 999_999, 1_234_567]);
    }

    #[test]
    fn test_uncloak_all_visible_windows_no_panic() {
        // EnumWindows should succeed; uncloaking random windows is best-effort
        uncloak_all_visible_windows();
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_set_dpi_awareness_no_panic() {
        // On CI/test environments this may return false (already set), but must not panic
        let _result = set_dpi_awareness();
    }
}
