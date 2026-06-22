//! Window style tweaks: DWM border color and WS_MAXIMIZEBOX (snap layout) management.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

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
// Snap layout suppression (WS_MAXIMIZEBOX removal)
// ============================================================================

/// Global set of window IDs whose WS_MAXIMIZEBOX style has been removed.
/// Used for panic recovery when AppState may be poisoned/unavailable.
static SNAP_DISABLED_HWNDS: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_snap_disabled() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    SNAP_DISABLED_HWNDS
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Remove `WS_MAXIMIZEBOX` from a window to disable Windows 11 Snap Layouts.
///
/// Returns `Ok(true)` if the style was changed, `Ok(false)` if already absent.
/// Registers the window in the global tracking set for panic recovery.
///
/// Uses `GetWindowLongW`/`SetWindowLongW` (32-bit) intentionally: on 64-bit
/// Windows this disables the DWM snap layout flyout while preserving the
/// maximize button and its click-to-maximize behavior.
pub fn remove_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos,
        GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE,
    };

    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        let style = GetWindowLongW(hwnd, GWL_STYLE);
        const WS_MAXIMIZEBOX: i32 = 0x0001_0000;
        if (style & WS_MAXIMIZEBOX) == 0 {
            return Ok(false); // Already absent
        }

        let new_style = style & !WS_MAXIMIZEBOX;
        SetWindowLongW(hwnd, GWL_STYLE, new_style);

        let _ = SetWindowPos(
            hwnd, None, 0, 0, 0, 0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        let mut guard = lock_snap_disabled();
        guard.get_or_insert_with(HashSet::new).insert(window_id);
    }
    Ok(true)
}

/// Restore `WS_MAXIMIZEBOX` on a window, re-enabling Windows 11 Snap Layouts.
///
/// Returns `Ok(true)` if the style was restored, `Ok(false)` if already present.
/// Removes the window from the global tracking set.
pub fn restore_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos,
        GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE,
    };

    // Always remove from tracking set, even if the Win32 call fails
    {
        let mut guard = lock_snap_disabled();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }

    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        let style = GetWindowLongW(hwnd, GWL_STYLE);
        const WS_MAXIMIZEBOX: i32 = 0x0001_0000;
        if (style & WS_MAXIMIZEBOX) != 0 {
            return Ok(false); // Already present
        }

        let new_style = style | WS_MAXIMIZEBOX;
        SetWindowLongW(hwnd, GWL_STYLE, new_style);

        let _ = SetWindowPos(
            hwnd, None, 0, 0, 0, 0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    Ok(true)
}

/// Best-effort bulk restore of `WS_MAXIMIZEBOX` for multiple windows.
/// Never panics — logs failures and continues.
pub fn restore_maximizebox_all(window_ids: &[WindowId]) {
    for &wid in window_ids {
        match restore_maximizebox(wid) {
            Ok(_) => {}
            Err(Win32Error::WindowNotFound(_)) => {
                // Window already destroyed — tracking set already cleaned up
            }
            Err(e) => {
                tracing::warn!("Failed to restore WS_MAXIMIZEBOX for window {}: {}", wid, e);
            }
        }
    }
}

/// Emergency restore of `WS_MAXIMIZEBOX` for all tracked windows.
/// Drains the global tracking set and restores styles best-effort.
/// Safe to call from panic hooks (no AppState needed).
pub fn restore_maximizebox_panic_recovery() {
    let window_ids: Vec<WindowId> = {
        let mut guard = lock_snap_disabled();
        guard
            .as_mut()
            .map(|set| set.drain().collect())
            .unwrap_or_default()
    };

    if window_ids.is_empty() {
        return;
    }

    eprintln!(
        "[leopardwm] Restoring WS_MAXIMIZEBOX for {} window(s) in panic recovery",
        window_ids.len()
    );

    for wid in &window_ids {
        // Direct Win32 call — don't use restore_maximizebox since tracking set is already drained
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetWindowLongW, SetWindowPos,
            GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE,
        };
        let Ok(hwnd) = window_id_to_hwnd(*wid) else { continue };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                continue;
            }
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            const WS_MAXIMIZEBOX_VAL: i32 = 0x0001_0000;
            if (style & WS_MAXIMIZEBOX_VAL) == 0 {
                let new_style = style | WS_MAXIMIZEBOX_VAL;
                SetWindowLongW(hwnd, GWL_STYLE, new_style);
                let _ = SetWindowPos(
                    hwnd, None, 0, 0, 0, 0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    eprintln!(
        "[leopardwm] WS_MAXIMIZEBOX panic recovery complete ({} windows processed)",
        window_ids.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_remove_maximizebox_zero_fails() {
        let result = remove_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_remove_maximizebox_invalid_hwnd_fails() {
        let result = remove_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_restore_maximizebox_zero_fails() {
        let result = restore_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_restore_maximizebox_invalid_hwnd_fails() {
        let result = restore_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_restore_maximizebox_all_empty_is_noop() {
        restore_maximizebox_all(&[]);
    }

    #[test]
    fn test_restore_maximizebox_panic_recovery_no_panic() {
        // Should not panic even with empty tracking set
        restore_maximizebox_panic_recovery();
    }
}
