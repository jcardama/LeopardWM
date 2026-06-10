//! Read-only window state queries: validity, visibility, rects, icons, cursor.

use leopardwm_core_layout::{Rect, WindowId};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsIconic, IsWindow, IsWindowVisible,
};

/// Query DWM for the window's corner-rounding preference and map to a pixel
/// radius matching what Windows itself draws.
///
/// Returns `Some(radius_px)` only when the app has *explicitly* opted into
/// a non-default corner preference (`DONOTROUND` → 0, `ROUNDSMALL` → 4,
/// `ROUND` → 8). `DWMWCP_DEFAULT` (the value every app gets unless it
/// overrides it) returns `None`, because it tells you nothing about what
/// the app actually paints — Mozilla's PiP, Chromium custom-frame popups,
/// and similar apps report DEFAULT while drawing their own square edges.
/// Callers should fall back to a sensible default (or a class-based match
/// list) when `None` is returned.
pub fn get_window_corner_radius(hwnd: WindowId) -> Option<f32> {
    if hwnd == 0 {
        return None;
    }
    const DWMWA_WINDOW_CORNER_PREFERENCE: i32 = 33;
    const DWMWCP_DONOTROUND: u32 = 1;
    const DWMWCP_ROUND: u32 = 2;
    const DWMWCP_ROUNDSMALL: u32 = 3;
    unsafe {
        let target = HWND(hwnd as *mut c_void);
        if !IsWindow(Some(target)).as_bool() {
            return None;
        }
        let mut pref: u32 = 0;
        let result = DwmGetWindowAttribute(
            target,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE),
            &mut pref as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if result.is_err() {
            return None;
        }
        match pref {
            DWMWCP_DONOTROUND => Some(0.0),
            DWMWCP_ROUND => Some(8.0),
            DWMWCP_ROUNDSMALL => Some(4.0),
            _ => None,
        }
    }
}

/// Fetch the window's small icon as a raw `HICON` handle. Resolution order:
/// 1. `SendMessageW(WM_GETICON, ICON_SMALL2)` (taskbar size — preferred)
/// 2. `SendMessageW(WM_GETICON, ICON_SMALL)` (legacy)
/// 3. `GetClassLongPtrW(GCLP_HICONSM)` (class-registered small)
/// 4. `SendMessageW(WM_GETICON, ICON_BIG)` (big icon, scaled down by caller)
/// 5. `GetClassLongPtrW(GCLP_HICON)` (class-registered big)
///
/// The returned handle is owned by the app (not by us); callers must NOT
/// `DestroyIcon` it. Returned as `isize` to keep the value `Send`/`Sync`
/// across thread boundaries — convert back to `HICON` at the call site.
/// Returns `None` if every probe fails (e.g. iconless tool windows).
pub fn get_window_icon(hwnd: WindowId) -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassLongPtrW, SendMessageTimeoutW, GCLP_HICON, GCLP_HICONSM, ICON_BIG, ICON_SMALL,
        ICON_SMALL2, SEND_MESSAGE_TIMEOUT_FLAGS, SMTO_ABORTIFHUNG, WM_GETICON,
    };
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let target = HWND(hwnd as *mut c_void);
        if !IsWindow(Some(target)).as_bool() {
            return None;
        }
        // Bounded `SendMessage` so a frozen app can't stall the daemon's
        // per-frame tab strip update. `SMTO_ABORTIFHUNG` returns early
        // (with 0 in the out-param) if the target window is unresponsive;
        // 50ms is comfortably above normal Win32 response time and well
        // below the human-perceptible animation frame budget.
        let smto: SEND_MESSAGE_TIMEOUT_FLAGS = SMTO_ABORTIFHUNG;
        let timeout_ms: u32 = 50;
        let probe = |wparam: usize| -> Option<isize> {
            let mut out: usize = 0;
            let result = SendMessageTimeoutW(
                target,
                WM_GETICON,
                WPARAM(wparam),
                LPARAM(0),
                smto,
                timeout_ms,
                Some(&mut out as *mut usize),
            );
            if result.0 == 0 || out == 0 {
                None
            } else {
                Some(out as isize)
            }
        };
        for kind in [ICON_SMALL2, ICON_SMALL] {
            if let Some(h) = probe(kind as usize) {
                return Some(h);
            }
        }
        let class_small = GetClassLongPtrW(target, GCLP_HICONSM);
        if class_small != 0 {
            return Some(class_small as isize);
        }
        if let Some(h) = probe(ICON_BIG as usize) {
            return Some(h);
        }
        let class_big = GetClassLongPtrW(target, GCLP_HICON);
        if class_big != 0 {
            return Some(class_big as isize);
        }
        None
    }
}

/// Check if a window is in the maximized (zoomed) state.
pub fn is_window_maximized(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        IsWindow(Some(hwnd)).as_bool() && !IsIconic(hwnd).as_bool() && {
            use windows::Win32::UI::WindowsAndMessaging::IsZoomed;
            IsZoomed(hwnd).as_bool()
        }
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

/// Check if a window is shell-cloaked (hidden by DWM, e.g. on another virtual desktop
/// or a suspended UWP app frame).
pub fn is_window_shell_cloaked(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    let hwnd = HWND(hwnd as *mut c_void);
    crate::enumeration::is_window_cloaked(hwnd)
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

/// Get the chrome (GetWindowRect) rect of a window in screen coordinates.
///
/// Unlike `get_window_visible_rect`, this returns the OS-tracked frame rect
/// (the rect set by SetWindowPos), not the DWM compositor's visible content
/// rect. Use this when you need an authoritative position that is immune to
/// DirectComposition swap-chain desync — e.g. Chromium-family apps can have
/// their visible content composited at a stale position after a rapid burst
/// of async SetWindowPos calls, but `GetWindowRect` always reports where the
/// chrome window actually lives.
pub fn get_window_chrome_rect(hwnd: WindowId) -> Option<Rect> {
    let hwnd_win = HWND(hwnd as *mut c_void);
    unsafe {
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

/// Cheap existence check for a window handle. Returns `false` if the
/// HWND has been recycled / the window no longer exists. Does NOT
/// require visibility or non-minimized state (use
/// `is_window_alive_and_visible` for that). Suitable for guarding
/// async deferred operations whose target may have been destroyed
/// between request and apply time.
pub fn is_window_valid(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))).as_bool() }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Win32Error;
    use crate::window_id_to_hwnd;

    #[test]
    fn test_is_valid_window_zero_returns_false() {
        assert!(!is_valid_window(0));
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
}
