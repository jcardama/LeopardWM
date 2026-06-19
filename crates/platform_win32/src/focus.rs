//! Foreground/focus management and graceful window close.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
    PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// The current OS foreground window as a `WindowId`, if any. This is
/// authoritative at the moment of the call, unlike the daemon's cached
/// focus, so callers that must know the truly-focused window at a precise
/// instant (e.g. recording which window was focused on a workspace before
/// leaving it) can query it directly.
pub fn get_foreground_window() -> Option<WindowId> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd.0 as WindowId)
}

/// Milliseconds since the user last produced a real input event
/// (keyboard or mouse). Used to distinguish user-initiated focus changes
/// from spurious `EVENT_SYSTEM_FOREGROUND` events fired by background
/// apps that steal focus on their own (notifications, app-internal focus
/// shuffles, etc.). Returns `None` if the API call fails.
pub fn ms_since_last_user_input() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::Win32::System::SystemInformation::GetTickCount;
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut lii).as_bool() {
            return None;
        }
        Some(GetTickCount().wrapping_sub(lii.dwTime))
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

        // Attach our input queue to BOTH the current foreground window's thread
        // and the target window's thread so Windows permits the foreground
        // change. Attaching to the foreground thread is what lets us steal focus
        // from an active holder (e.g. a borderless fullscreen game): hotkeys now
        // arrive via a low-level keyboard hook, which does not grant this process
        // the "last input event" foreground right that RegisterHotKey conferred.
        // Without it the window scrolls into place but stays behind the
        // foreground app.
        let foreground_thread = {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                0
            } else {
                GetWindowThreadProcessId(fg, None)
            }
        };
        let mut attached: Vec<u32> = Vec::new();
        for candidate in [foreground_thread, target_thread] {
            if candidate != 0 && candidate != current_thread && !attached.contains(&candidate) {
                if windows::Win32::System::Threading::AttachThreadInput(
                    current_thread,
                    candidate,
                    true,
                )
                .as_bool()
                {
                    attached.push(candidate);
                } else {
                    diagnostics.push(format!(
                        "AttachThreadInput attach failed (current_thread={}, other_thread={})",
                        current_thread, candidate
                    ));
                }
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

        // Detach every input queue we attached to.
        for thread in &attached {
            if !windows::Win32::System::Threading::AttachThreadInput(
                current_thread,
                *thread,
                false,
            )
            .as_bool()
            {
                diagnostics.push(format!(
                    "AttachThreadInput detach failed (current_thread={}, other_thread={})",
                    current_thread, thread
                ));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
