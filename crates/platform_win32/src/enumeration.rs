//! Window and monitor enumeration, filtering, and queries.

use crate::event_hooks::{
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_FOCUS, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND,
    EVENT_SYSTEM_MOVESIZESTART,
};
use crate::types::{MonitorId, MonitorInfo, Win32Error, WindowInfo};
use leopardwm_core_layout::{Rect, WindowId};
use std::ffi::c_void;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, WS_CAPTION, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_THICKFRAME, WS_VISIBLE,
};

/// Whether a tool window (`WS_EX_TOOLWINDOW`) should be excluded from tiling.
///
/// Tool windows are auxiliary by design. We make an exception for ones that also
/// set `WS_EX_APPWINDOW` (they ask to be treated as primary windows), but only
/// when they are also resizable (`WS_THICKFRAME`). A fixed-size tool window is an
/// overlay or launcher, like Raycast's 2x2 helper window, which must never be
/// tiled into a column.
pub(crate) fn is_excluded_tool_window(style: u32, ex_style: u32) -> bool {
    let is_tool = ex_style & WS_EX_TOOLWINDOW.0 != 0;
    let is_app = ex_style & WS_EX_APPWINDOW.0 != 0;
    let is_resizable = style & WS_THICKFRAME.0 != 0;
    is_tool && (!is_app || !is_resizable)
}

/// Excludes always-on-top notification popups without app-window chrome.
/// Chromium/Electron main windows keep `WS_THICKFRAME` or `WS_CAPTION`.
pub(crate) fn is_excluded_topmost_popup(style: u32, ex_style: u32) -> bool {
    ex_style & WS_EX_TOPMOST.0 != 0
        && ex_style & WS_EX_APPWINDOW.0 == 0
        && style & WS_CAPTION.0 == 0
        && style & WS_THICKFRAME.0 == 0
}

/// Reads a live window's current styles to test [`is_excluded_tool_window`].
/// Style-only, so it's safe to call on already-managed windows without
/// false-positiving on transient title/cloak states.
pub fn is_excluded_tool_window_hwnd(hwnd: WindowId) -> bool {
    let hwnd = HWND(hwnd as *mut c_void);
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };
    is_excluded_tool_window(style, ex_style)
}

/// Reads current styles for the admission-only topmost popup exclusion.
/// Already-managed windows may acquire this shape without losing management.
pub fn is_excluded_topmost_popup_hwnd(hwnd: WindowId) -> bool {
    let hwnd = HWND(hwnd as *mut c_void);
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };
    is_excluded_topmost_popup(style, ex_style)
}

/// Checks built-in class exclusions without rejecting hidden or cloaked managed windows.
pub fn is_excluded_window_class_hwnd(hwnd: WindowId) -> bool {
    let mut class_buf = [0u16; 256];
    let class_len = unsafe { GetClassNameW(HWND(hwnd as *mut c_void), &mut class_buf) };
    should_skip_window_by_class(&String::from_utf16_lossy(&class_buf[..class_len as usize]))
}

/// Get info for a single window handle with relaxed filters.
///
/// Unlike `enumerate_windows`, this does not filter out cloaked windows
/// or windows with empty titles, making it suitable for handling window
/// creation events where UWP apps may still be transitioning.
///
/// Used for both admission and already-managed window lookups. The daemon
/// applies the admission-only topmost popup exclusion after this read succeeds.
/// Changes to these filters must be reflected in `inspect.rs`'s live-create
/// diagnostics, which also model that daemon admission check. Live-create omits
/// cloak, empty title, and skip-title relative to startup.
pub fn get_window_info(hwnd_id: WindowId) -> Option<WindowInfo> {
    unsafe {
        let hwnd = HWND(hwnd_id as *mut c_void);

        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }

        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

        if is_excluded_tool_window(style, ex_style) {
            return None;
        }

        if ex_style & WS_EX_NOACTIVATE.0 != 0 {
            return None;
        }

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

        let mut process_id: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));

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

/// Enumerate all top-level windows that should be managed.
///
/// Filters out:
/// - Invisible windows
/// - Tool windows, unless they are a resizable WS_EX_APPWINDOW
/// - Topmost popups without caption, sizing frame, or WS_EX_APPWINDOW
/// - Windows with empty titles
/// - Cloaked windows
/// - Windows with WS_EX_NOACTIVATE
pub fn enumerate_windows() -> Result<Vec<WindowInfo>, Win32Error> {
    let mut windows: Vec<WindowInfo> = Vec::new();

    unsafe {
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

/// Get the primary monitor's information, including its work area excluding the taskbar.
pub fn get_primary_monitor() -> Result<MonitorInfo, Win32Error> {
    let monitors = enumerate_monitors()?;

    monitors
        .into_iter()
        .find(|m| m.is_primary)
        .ok_or_else(|| Win32Error::MonitorEnumerationFailed("No primary monitor found".to_string()))
}

/// Find the monitor containing the rectangle's center, falling back to the primary.
/// Returns `None` when neither is available.
pub fn find_monitor_for_rect<'a>(
    monitors: &'a [MonitorInfo],
    rect: &Rect,
) -> Option<&'a MonitorInfo> {
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

fn monitor_center(m: &MonitorInfo) -> (i32, i32) {
    (m.rect.x + m.rect.width / 2, m.rect.y + m.rect.height / 2)
}

/// Find the monitor above the given one: of those whose center sits higher
/// (smaller y), the nearest vertically, tie-broken by horizontal proximity.
/// Geometry-based so it works for stacked layouts regardless of enumeration
/// order.
pub fn monitor_above(monitors: &[MonitorInfo], current_id: MonitorId) -> Option<&MonitorInfo> {
    let (cx, cy) = monitor_center(monitors.iter().find(|m| m.id == current_id)?);
    monitors
        .iter()
        .filter(|m| m.id != current_id && monitor_center(m).1 < cy)
        .min_by_key(|m| {
            let (mx, my) = monitor_center(m);
            (cy - my, (mx - cx).abs())
        })
}

/// Find the monitor below the given one. Mirror of [`monitor_above`].
pub fn monitor_below(monitors: &[MonitorInfo], current_id: MonitorId) -> Option<&MonitorInfo> {
    let (cx, cy) = monitor_center(monitors.iter().find(|m| m.id == current_id)?);
    monitors
        .iter()
        .filter(|m| m.id != current_id && monitor_center(m).1 > cy)
        .min_by_key(|m| {
            let (mx, my) = monitor_center(m);
            (my - cy, (mx - cx).abs())
        })
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

unsafe extern "system" fn enum_monitors_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprc_clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    if GetMonitorInfoW(hmonitor, &mut info as *mut MONITORINFOEXW as *mut _).as_bool() {
        let mon_rect = info.monitorInfo.rcMonitor;
        let work_rect = info.monitorInfo.rcWork;

        let device_name_len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let device_name = String::from_utf16_lossy(&info.szDevice[..device_name_len]);

        // Bound gap/border scaling and fall back to 1.0 for an invalid DPI reading.
        let scale_factor = {
            use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
            let mut dpi_x: u32 = 96;
            let mut dpi_y: u32 = 96;
            let _ = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let raw = dpi_x as f64 / 96.0;
            if raw.is_finite() && raw > 0.0 {
                raw.clamp(1.0, 8.0)
            } else {
                1.0
            }
        };

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
            scale_factor,
        });

        TRUE
    } else {
        TRUE
    }
}

/// Callback for EnumWindows that filters and collects window info.
///
/// Adding or reordering a check here requires the same edit in
/// `platform_win32/src/inspect.rs` (`classify_startup` only). The two
/// admission chains intentionally diverge: live-create omits cloak, empty
/// title, and skip-title relative to startup so transient popups can admit.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);

    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }

    // Skip minimized windows (e.g., tray apps like Raycast)
    if IsIconic(hwnd).as_bool() {
        return TRUE;
    }

    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

    if style & WS_VISIBLE.0 == 0 {
        return TRUE;
    }

    if is_excluded_tool_window(style, ex_style) {
        return TRUE;
    }

    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return TRUE;
    }

    if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
        if !owner.is_invalid() {
            return TRUE;
        }
    }

    if is_excluded_topmost_popup(style, ex_style) {
        return TRUE;
    }

    // Skip cloaked windows (e.g., on other virtual desktops)
    if is_window_cloaked(hwnd) {
        return TRUE;
    }

    let title_len = GetWindowTextLengthW(hwnd);
    if title_len == 0 {
        return TRUE;
    }

    let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
    let actual_len = GetWindowTextW(hwnd, &mut title_buf);
    if actual_len == 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&title_buf[..actual_len as usize]);

    if should_skip_window_by_title(&title) {
        return TRUE;
    }

    let mut class_buf: Vec<u16> = vec![0; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    let class_name = if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize])
    } else {
        String::new()
    };

    if should_skip_window_by_class(&class_name) {
        return TRUE;
    }

    let mut process_id: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));

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
///
/// Adding or reordering a check here requires the same edit in
/// `platform_win32/src/inspect.rs` (`classify_live_create` Stage 1 only). The two
/// admission chains intentionally diverge: live-create omits cloak, empty
/// title, and skip-title relative to startup so transient popups can admit.
pub(crate) fn should_emit_window_event_with_policy(
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

    if is_excluded_tool_window(style, ex_style) {
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

    !should_skip_window_by_class(&class_name)
}

pub(crate) fn should_emit_window_event(hwnd: HWND) -> bool {
    should_emit_window_event_with_policy(
        hwnd, true, // visible only
        true, // non-empty title
        true, // filter cloaked windows
    )
}

pub(crate) fn should_emit_window_event_for(event: u32, hwnd: HWND) -> bool {
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
        EVENT_SYSTEM_MINIMIZESTART
        | EVENT_SYSTEM_MINIMIZEEND
        | EVENT_OBJECT_DESTROY
        | EVENT_OBJECT_HIDE => should_emit_window_event_with_policy(hwnd, false, false, false),
        EVENT_OBJECT_LOCATIONCHANGE => should_emit_window_event_with_policy(hwnd, true, true, true),
        EVENT_SYSTEM_MOVESIZESTART | EVENT_SYSTEM_MOVESIZEEND => {
            should_emit_window_event_with_policy(hwnd, false, false, false)
        }
        _ => false,
    }
}

pub(crate) fn should_filter_window_event_by_manageability(event: u32) -> bool {
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

pub(crate) fn should_skip_window_by_title(title: &str) -> bool {
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
pub fn is_window_cloaked(hwnd: HWND) -> bool {
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

pub(crate) fn should_skip_window_by_class(class_name: &str) -> bool {
    const SKIP_CLASSES: &[&str] = &[
        "Progman",                    // Program Manager
        "Shell_TrayWnd",              // Taskbar
        "Shell_SecondaryTrayWnd",     // Secondary taskbar
        "WorkerW",                    // Desktop worker
        "Windows.UI.Core.CoreWindow", // UWP system windows
        "IME",                        // Legacy input-method helper window
        "MSCTFIME UI",                // Text Services Framework IME helper window
        // ApplicationFrameWindow must remain eligible so UWP application hosts can tile.
        "XamlExplorerHostIslandWindow",        // XAML islands
        "TopLevelWindowForOverflowXamlIsland", // Overflow islands
        "RAIL_WINDOW", // WSLg RemoteApp (msrdc.exe) — RDP-projected from Linux;
        // tiling breaks them because the remote session controls sizing
        "Ghost",  // DWM hung-window replacement — tiling duplicates the original
        "#32770", // Standard Win32 dialog (Open/Save/Print/Properties)
        "C2RCustomWindowDialog", // Office Click-to-Run dialogs have no native dialog styles or owner.
        "OperationStatusWindow", // Shell copy/move/delete progress dialog. Style-based
        // dialog detection misses it: it keeps WS_MINIMIZEBOX,
        // so it fails the "no minimize *and* no maximize" test.
        "Chrome_RenderWidgetHostHWND", // Internal Electron/Chrome render widget, not a real window
        "LeopardWMSettings",           // Our own settings window
        "LeopardWMFocusPlaceholder",   // Invisible foreground target on empty workspaces
        "LeopardWMBorderFrame",        // Our own border overlay
        "LeopardWMThumbnailHost",      // Our own DWM thumbnail host
        "LeopardWMOverview",           // Our own overview overlay
        "LeopardWMOverviewMask",       // Our own overview corner-cap mask layer
    ];

    SKIP_CLASSES.contains(&class_name)
}

/// Get the executable name for a process by PID.
///
/// Returns just the filename (e.g., "notepad.exe"), not the full path.
/// Returns None if the process cannot be accessed or doesn't exist.
pub fn get_process_executable(pid: u32) -> Option<String> {
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return None,
        };

        // Extended-length buffer for long paths.
        let mut buffer: Vec<u16> = vec![0; 1024];
        let len = K32GetModuleFileNameExW(Some(handle), None, &mut buffer);

        let _ = CloseHandle(handle);

        if len == 0 || len as usize >= buffer.len() {
            // len == 0: call failed; len >= buffer size: path was truncated
            return None;
        }

        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().map(|s| s.to_string())
    }
}

pub struct TopLevelWindowIdCollection {
    pub window_ids: Vec<WindowId>,
    pub error: Option<Win32Error>,
}

/// Collect all top-level window IDs (used by emergency restore).
pub fn collect_all_top_level_window_ids() -> TopLevelWindowIdCollection {
    collect_all_top_level_window_ids_with(|window_ids| unsafe {
        EnumWindows(
            Some(collect_all_window_ids_callback),
            LPARAM((window_ids as *mut Vec<WindowId>) as isize),
        )
        .map_err(|error| Win32Error::EnumerationFailed(format!("EnumWindows failed: {error}")))
    })
}

fn collect_all_top_level_window_ids_with<F>(enumerate: F) -> TopLevelWindowIdCollection
where
    F: FnOnce(&mut Vec<WindowId>) -> Result<(), Win32Error>,
{
    let mut window_ids = Vec::new();
    let error = enumerate(&mut window_ids).err();
    TopLevelWindowIdCollection { window_ids, error }
}

unsafe extern "system" fn collect_all_window_ids_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let window_ids = &mut *(lparam.0 as *mut Vec<WindowId>);
    let window_id = hwnd.0 as WindowId;
    if window_id != 0 {
        window_ids.push(window_id);
    }
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_all_top_level_window_ids_surfaces_error_without_losing_partial_ids() {
        let collection = collect_all_top_level_window_ids_with(|window_ids| {
            window_ids.extend([10, 20]);
            Err(Win32Error::EnumerationFailed(
                "injected EnumWindows failure".to_string(),
            ))
        });
        assert_eq!(collection.window_ids, [10, 20]);
        assert!(collection
            .error
            .unwrap()
            .to_string()
            .contains("injected EnumWindows failure"));
    }

    #[test]
    fn test_is_excluded_tool_window() {
        let thickframe = WS_THICKFRAME.0;
        let tool = WS_EX_TOOLWINDOW.0;
        let app = WS_EX_APPWINDOW.0;

        // Raycast's helper: tool + appwindow but fixed-size — excluded.
        assert!(is_excluded_tool_window(0, tool | app));
        // Plain tool window (no appwindow) — excluded regardless of resizability.
        assert!(is_excluded_tool_window(thickframe, tool));
        assert!(is_excluded_tool_window(0, tool));
        // Tool + appwindow + resizable — a genuine primary window, kept.
        assert!(!is_excluded_tool_window(thickframe, tool | app));
        // Not a tool window — never excluded here (resizable or not).
        assert!(!is_excluded_tool_window(thickframe, app));
        assert!(!is_excluded_tool_window(0, 0));
    }

    #[test]
    fn topmost_popup_excludes_captured_notion_calendar_styles() {
        assert!(is_excluded_topmost_popup(0x14020000, 0x00200008));
        assert!(is_excluded_topmost_popup(0x04020000, 0x00200008));
    }

    #[test]
    fn topmost_popup_preserves_app_window_shapes() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_SYSMENU,
        };

        let frameless_resizable =
            (WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU | WS_VISIBLE).0;
        assert!(!is_excluded_topmost_popup(
            frameless_resizable,
            WS_EX_TOPMOST.0
        ));
        assert!(!is_excluded_topmost_popup(
            (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0,
            WS_EX_TOPMOST.0
        ));
        assert!(!is_excluded_topmost_popup(
            (WS_CAPTION | WS_SYSMENU | WS_VISIBLE).0,
            WS_EX_TOPMOST.0
        ));
        assert!(!is_excluded_topmost_popup(
            0x14020000,
            0x00200008 | WS_EX_APPWINDOW.0
        ));
        assert!(!is_excluded_topmost_popup(0x14020000, 0x00200000));
    }

    #[test]
    fn test_enumerate_monitors() {
        let monitors = enumerate_monitors().expect("enumerate connected displays");
        assert!(!monitors.is_empty(), "at least one display must exist");
        assert_eq!(monitors.iter().filter(|m| m.is_primary).count(), 1);
        for monitor in &monitors {
            assert!(monitor.rect.width > 0 && monitor.rect.height > 0);
            assert!(monitor.work_area.width > 0 && monitor.work_area.height > 0);
            assert!(monitor.work_area.x >= monitor.rect.x);
            assert!(monitor.work_area.y >= monitor.rect.y);
            assert!(
                i64::from(monitor.work_area.x) + i64::from(monitor.work_area.width)
                    <= i64::from(monitor.rect.x) + i64::from(monitor.rect.width)
            );
            assert!(
                i64::from(monitor.work_area.y) + i64::from(monitor.work_area.height)
                    <= i64::from(monitor.rect.y) + i64::from(monitor.rect.height)
            );
        }
    }

    #[test]
    fn test_get_primary_monitor() {
        let primary = get_primary_monitor().expect("select the primary display");
        assert!(primary.is_primary, "selected display must be primary");
        assert_eq!((primary.rect.x, primary.rect.y), (0, 0));
        assert!(primary.rect.width > 0 && primary.rect.height > 0);
        assert!(primary.work_area.width > 0 && primary.work_area.height > 0);
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
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
                scale_factor: 1.0,
            },
        ];

        let window1 = Rect::new(100, 100, 800, 600);
        let found = find_monitor_for_rect(&monitors, &window1);
        assert_eq!(found.unwrap().id, 1);

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
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            },
        ];

        let sorted = monitors_by_position(&monitors);
        assert_eq!(sorted[0].id, 1);
        assert_eq!(sorted[1].id, 2);
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
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
                scale_factor: 1.0,
            },
        ];

        let right = monitor_to_right(&monitors, 1);
        assert_eq!(right.unwrap().id, 2);

        let left = monitor_to_left(&monitors, 2);
        assert_eq!(left.unwrap().id, 1);

        let no_left = monitor_to_left(&monitors, 1);
        assert!(no_left.is_none());

        let no_right = monitor_to_right(&monitors, 2);
        assert!(no_right.is_none());
    }

    #[test]
    fn test_monitor_above_below() {
        // Monitor 1 stacked directly above monitor 2.
        let monitors = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(0, 1080, 1920, 1080),
                work_area: Rect::new(0, 1080, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
                scale_factor: 1.0,
            },
        ];

        assert_eq!(monitor_below(&monitors, 1).unwrap().id, 2);
        assert_eq!(monitor_above(&monitors, 2).unwrap().id, 1);
        assert!(monitor_above(&monitors, 1).is_none());
        assert!(monitor_below(&monitors, 2).is_none());

        // Side-by-side monitors have nothing above or below each other.
        let side = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1080),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
                scale_factor: 1.0,
            },
        ];
        assert!(monitor_above(&side, 1).is_none());
        assert!(monitor_below(&side, 1).is_none());
    }

    #[test]
    fn test_should_filter_window_event_by_manageability_covers_hooked_events() {
        for event in [
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_HIDE,
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            EVENT_SYSTEM_MOVESIZESTART,
            EVENT_SYSTEM_MOVESIZEEND,
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_FOCUS,
        ] {
            assert!(
                should_filter_window_event_by_manageability(event),
                "event {event}"
            );
        }
        assert!(!should_filter_window_event_by_manageability(0));
    }

    #[test]
    fn test_should_treat_cloak_query_failure_as_cloaked_only_for_invalid_windows() {
        assert!(!should_treat_cloak_query_failure_as_cloaked(true));
        assert!(should_treat_cloak_query_failure_as_cloaked(false));
    }

    #[test]
    fn test_skip_classes_preserve_uwp_and_exclude_focus_placeholder() {
        let skip = should_skip_window_by_class("ApplicationFrameWindow");
        assert!(
            !skip,
            "ApplicationFrameWindow should NOT be in skip list (UWP apps should be tiled)"
        );
        assert!(should_skip_window_by_class("LeopardWMFocusPlaceholder"));
    }

    #[test]
    fn test_shell_progress_dialog_is_skipped() {
        assert!(
            should_skip_window_by_class("OperationStatusWindow"),
            "shell copy/move/delete progress dialogs should not be tiled"
        );
    }

    #[test]
    fn test_office_click_to_run_dialog_is_skipped_exactly() {
        assert!(should_skip_window_by_class("C2RCustomWindowDialog"));
        assert!(!should_skip_window_by_class("C2RCustomWindow"));
        assert!(!should_skip_window_by_class("OMain"));
        assert!(!should_skip_window_by_class("OpusApp"));
        assert!(!should_skip_window_by_class("XLMAIN"));
    }

    #[test]
    fn test_input_method_helper_classes_are_skipped_exactly() {
        assert!(should_skip_window_by_class("IME"));
        assert!(should_skip_window_by_class("MSCTFIME UI"));
        assert!(!should_skip_window_by_class("IMEWindow"));
    }
}
