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
    IsWindowVisible, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_VISIBLE,
};

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

/// Enumerate all top-level windows that should be managed.
///
/// Filters out:
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

        // Query per-monitor DPI for scaling gap/border values.
        // Clamp to [1.0, 8.0] to guard against API failure returning 0 or
        // non-finite values. Windows supports up to 500% (5.0) scaling.
        let scale_factor = {
            use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
            let mut dpi_x: u32 = 96;
            let mut dpi_y: u32 = 96;
            let _ = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let raw = dpi_x as f64 / 96.0;
            if raw.is_finite() && raw > 0.0 { raw.clamp(1.0, 8.0) } else { 1.0 }
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

/// Check if a window should be skipped based on its title.
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

/// Check if a window should be skipped based on its class name.
pub(crate) fn should_skip_window_by_class(class_name: &str) -> bool {
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
        "RAIL_WINDOW",                // WSLg RemoteApp (msrdc.exe) — RDP-projected from Linux;
                                      // tiling breaks them because the remote session controls sizing
        "Ghost",                      // DWM hung-window replacement — tiling duplicates the original
        "#32770",                     // Standard Win32 dialog (Open/Save/Print/Properties)
        "Chrome_RenderWidgetHostHWND", // Internal Electron/Chrome render widget, not a real window
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

        if len == 0 || len as usize >= buffer.len() {
            // len == 0: call failed; len >= buffer size: path was truncated
            return None;
        }

        // Convert to string and extract filename
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().map(|s| s.to_string())
    }
}

/// Collect all top-level window IDs (used by emergency restore).
pub(crate) fn collect_all_top_level_window_ids() -> Vec<WindowId> {
    let mut window_ids: Vec<WindowId> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_all_window_ids_callback),
            LPARAM((&mut window_ids as *mut Vec<WindowId>) as isize),
        );
    }
    window_ids
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
    fn test_skip_classes_does_not_contain_application_frame_window() {
        let skip = should_skip_window_by_class("ApplicationFrameWindow");
        assert!(
            !skip,
            "ApplicationFrameWindow should NOT be in skip list (UWP apps should be tiled)"
        );
    }
}
