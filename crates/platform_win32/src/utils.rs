//! Utility functions for window manipulation, focus, border colors, etc.

use crate::enumeration::{collect_all_top_level_window_ids, get_primary_monitor};

/// Check if Windows "Show animations" accessibility setting is enabled.
/// Returns `false` when the user has disabled client-area animations
/// (Settings > Accessibility > Visual effects > Animation effects).
pub fn are_animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;
    use windows::Win32::UI::WindowsAndMessaging::SPI_GETCLIENTAREAANIMATION;
    use windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS;

    let mut enabled: i32 = 1;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut i32 as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled != 0
}
use crate::placement::apply_placements;
use crate::types::{PlatformConfig, Win32Error};
use crate::{combine_operation_failures, is_benign_side_effect_error, window_id_to_hwnd};
use crate::MOVE_OFFSCREEN_SENTINEL_COORD;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindow,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};

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

/// Get the visible (DWM extended frame) rect of a window in screen coordinates.
///
/// Returns the rect corresponding to layout coordinates — the visible area
/// excluding invisible window borders. Falls back to GetWindowRect if DWM
/// attributes are unavailable.
pub fn get_window_visible_rect(hwnd: WindowId) -> Option<Rect> {
    let hwnd_win = HWND(hwnd as *mut c_void);
    unsafe {
        let mut extended_rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd_win,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut extended_rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
        {
            return Some(Rect::new(
                extended_rect.left,
                extended_rect.top,
                extended_rect.right - extended_rect.left,
                extended_rect.bottom - extended_rect.top,
            ));
        }
        // Fall back to GetWindowRect
        let mut win_rect = RECT::default();
        if GetWindowRect(hwnd_win, &mut win_rect).is_ok() {
            Some(Rect::new(
                win_rect.left,
                win_rect.top,
                win_rect.right - win_rect.left,
                win_rect.bottom - win_rect.top,
            ))
        } else {
            None
        }
    }
}

/// Check if the cursor is on a window's resize border (not the title bar/interior).
///
/// Returns `true` if the cursor position at `MoveSizeStart` time suggests a resize
/// operation rather than a move. Uses system metrics for border thickness.
pub fn is_cursor_on_resize_border(hwnd: WindowId) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYSIZEFRAME,
    };

    let (cx, cy) = match get_cursor_pos() {
        Some(pos) => pos,
        None => return false,
    };

    let mut win_rect = RECT::default();
    unsafe {
        if GetWindowRect(HWND(hwnd as *mut c_void), &mut win_rect).is_err() {
            return false;
        }

        let border_x = GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
        let border_y = GetSystemMetrics(SM_CYSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);

        // If cursor is within the border thickness of any edge, it's a resize
        cx <= win_rect.left + border_x
            || cx >= win_rect.right - border_x
            || cy <= win_rect.top + border_y
            || cy >= win_rect.bottom - border_y
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

// ============================================================================
// Offscreen sentinel helpers
// ============================================================================

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

#[allow(dead_code)]
pub(crate) fn move_offscreen_rect_for(rect: &Rect) -> Rect {
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

// ============================================================================
// Focus and window operations
// ============================================================================

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

// ============================================================================
// Border color
// ============================================================================

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

// ============================================================================
// Restore / uncloak
// ============================================================================

/// Restore one window from MoveOffScreen sentinel coordinates to the primary monitor.
///
/// Returns `Ok(true)` if the window was restored, `Ok(false)` if it was not at
/// sentinel coordinates, and `Err` if restore operations failed.
pub fn restore_window_moved_offscreen(window_id: WindowId) -> Result<bool, Win32Error> {
    let primary = get_primary_monitor()?;
    restore_window_if_offscreen_to_work_area(window_id, &primary.work_area)
}

pub(crate) fn restore_windows_moved_offscreen_with_work_area<F>(
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
    crate::dwm_uncloak_all();

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
    crate::dwm_uncloak_all();
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

    let _ = apply_placements(&placements, &PlatformConfig::default(), None);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_set_dpi_awareness_no_panic() {
        // On CI/test environments this may return false (already set), but must not panic
        let _result = set_dpi_awareness();
    }
}
