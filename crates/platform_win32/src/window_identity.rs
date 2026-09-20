//! HWND lifetime identity via a daemon-owned window property.
//!
//! Windows removes window properties when the HWND is destroyed, so a stored
//! token can distinguish a live stamped lifetime from a recycled handle that
//! reused the same numeric HWND (even with the same PID and class) before a
//! delayed Destroyed event is processed.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ACCESS_DENIED, HANDLE, HWND, WIN32_ERROR,
};
use windows::Win32::UI::WindowsAndMessaging::{GetPropW, IsWindow, SetPropW};

const TOKEN_PROPERTY: windows::core::PCWSTR = w!("LeopardWMIgnoreToken");

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn mint_token() -> u64 {
    loop {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

fn require_live_hwnd(window_id: WindowId) -> Result<HWND, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }
    }
    Ok(hwnd)
}

fn handle_from_token(token: u64) -> HANDLE {
    HANDLE(token as *mut c_void)
}

fn token_from_handle(handle: HANDLE) -> Option<u64> {
    if handle.0.is_null() {
        None
    } else {
        Some(handle.0 as usize as u64)
    }
}

/// Documented [`RemovePropW`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-removepropw)
/// outcomes after last error was zeroed.
///
/// The return value is the stored data handle, or NULL if the data cannot be
/// found. UIPI blocks set `GetLastError` to 5. The API does not say that every
/// failure sets last error, so NULL with last error 0 cannot be distinguished
/// from an undetectable failure. Other last-error codes after NULL are not
/// documented; they are classified only because this call produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovePropClassification {
    Removed,
    Missing,
    AccessDenied,
    UndocumentedFailure(u32),
}

fn classify_remove_prop(returned: HANDLE, last_error: u32) -> RemovePropClassification {
    if !returned.0.is_null() {
        RemovePropClassification::Removed
    } else if last_error == 0 {
        RemovePropClassification::Missing
    } else if last_error == ERROR_ACCESS_DENIED.0 {
        RemovePropClassification::AccessDenied
    } else {
        RemovePropClassification::UndocumentedFailure(last_error)
    }
}

fn result_for_remove_prop(
    window_id: WindowId,
    classification: RemovePropClassification,
) -> Result<(), Win32Error> {
    match classification {
        RemovePropClassification::Removed | RemovePropClassification::Missing => Ok(()),
        RemovePropClassification::AccessDenied => Err(Win32Error::SetPositionFailed(format!(
            "RemovePropW failed for window {window_id}: access denied (UIPI)"
        ))),
        RemovePropClassification::UndocumentedFailure(last_error) => {
            Err(Win32Error::SetPositionFailed(format!(
                "RemovePropW failed for window {window_id}: GetLastError={last_error}"
            )))
        }
    }
}

/// Call `RemovePropW` without the windows-rs `Result` wrapper, which treats NULL
/// as `Error::from_thread` and can inherit a stale last error.
fn remove_lifetime_token_prop(hwnd: HWND) -> (HANDLE, u32) {
    windows::core::link!("user32.dll" "system" fn RemovePropW(hwnd: HWND, lpstring: windows::core::PCWSTR) -> HANDLE);
    unsafe {
        let previous_error = GetLastError();
        SetLastError(WIN32_ERROR(0));
        let handle = RemovePropW(hwnd, TOKEN_PROPERTY);
        let last_error = GetLastError().0;
        SetLastError(previous_error);
        (handle, last_error)
    }
}

/// Stamp a unique lifetime token on `window_id`. The OS clears the property
/// when that window is destroyed.
pub fn stamp_window_lifetime_token(window_id: WindowId) -> Result<u64, Win32Error> {
    let hwnd = require_live_hwnd(window_id)?;
    let token = mint_token();
    unsafe {
        SetPropW(hwnd, TOKEN_PROPERTY, Some(handle_from_token(token))).map_err(|error| {
            Win32Error::SetPositionFailed(format!(
                "SetPropW failed for window {window_id}: {error}"
            ))
        })?;
    }
    Ok(token)
}

/// Read the lifetime token currently stored on `window_id`, if any.
pub fn read_window_lifetime_token(window_id: WindowId) -> Result<Option<u64>, Win32Error> {
    let hwnd = require_live_hwnd(window_id)?;
    let handle = unsafe { GetPropW(hwnd, TOKEN_PROPERTY) };
    Ok(token_from_handle(handle))
}

/// Remove the lifetime token from `window_id`. Missing properties succeed.
///
/// [`RemovePropW`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-removepropw)
/// returns the stored handle, or NULL if the property is not found. A UIPI
/// block sets `GetLastError` to 5. Last error is zeroed before the call so a
/// leftover error 5 is not treated as UIPI. NULL with last error 0 is missing;
/// failures that return NULL without setting last error are indistinguishable
/// from missing.
pub fn clear_window_lifetime_token(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = require_live_hwnd(window_id)?;
    let (handle, last_error) = remove_lifetime_token_prop(hwnd);
    result_for_remove_prop(window_id, classify_remove_prop(handle, last_error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_rejects_null_hwnd_without_foreign_window() {
        let error = stamp_window_lifetime_token(0).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn read_rejects_null_hwnd_without_foreign_window() {
        let error = read_window_lifetime_token(0).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn clear_rejects_null_hwnd_without_foreign_window() {
        let error = clear_window_lifetime_token(0).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn stamp_rejects_invalid_hwnd_without_foreign_window() {
        let error = stamp_window_lifetime_token(u64::MAX).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(id) if id == u64::MAX));
    }

    #[test]
    fn read_rejects_invalid_hwnd_without_foreign_window() {
        let error = read_window_lifetime_token(u64::MAX).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(id) if id == u64::MAX));
    }

    #[test]
    fn clear_rejects_invalid_hwnd_without_foreign_window() {
        let error = clear_window_lifetime_token(u64::MAX).unwrap_err();
        assert!(matches!(error, Win32Error::WindowNotFound(id) if id == u64::MAX));
    }

    fn non_null_handle() -> HANDLE {
        handle_from_token(1)
    }

    fn invalid_handle_value() -> HANDLE {
        handle_from_token(u64::MAX)
    }

    #[test]
    fn remove_prop_non_null_is_removed_even_with_stale_access_denied() {
        assert_eq!(ERROR_ACCESS_DENIED.0, 5);
        assert_eq!(
            classify_remove_prop(non_null_handle(), ERROR_ACCESS_DENIED.0),
            RemovePropClassification::Removed
        );
        assert!(result_for_remove_prop(1, RemovePropClassification::Removed).is_ok());
    }

    #[test]
    fn remove_prop_null_with_zeroed_last_error_is_missing() {
        assert_eq!(
            classify_remove_prop(HANDLE::default(), 0),
            RemovePropClassification::Missing
        );
        assert!(result_for_remove_prop(1, RemovePropClassification::Missing).is_ok());
    }

    #[test]
    fn remove_prop_null_with_access_denied_is_documented_uipi_failure() {
        let classification = classify_remove_prop(HANDLE::default(), ERROR_ACCESS_DENIED.0);
        assert_eq!(classification, RemovePropClassification::AccessDenied);
        let error = result_for_remove_prop(1, classification).unwrap_err();
        assert!(
            matches!(error, Win32Error::SetPositionFailed(message) if message.contains("UIPI"))
        );
    }

    #[test]
    fn remove_prop_null_with_undocumented_last_error_is_not_missing() {
        let classification = classify_remove_prop(HANDLE::default(), 87);
        assert_eq!(
            classification,
            RemovePropClassification::UndocumentedFailure(87)
        );
        let error = result_for_remove_prop(1, classification).unwrap_err();
        assert!(
            matches!(error, Win32Error::SetPositionFailed(message) if message.contains("GetLastError=87"))
        );
    }

    #[test]
    fn remove_prop_invalid_handle_value_is_removed_not_missing() {
        assert!(invalid_handle_value().is_invalid());
        assert!(!invalid_handle_value().0.is_null());
        assert_eq!(
            classify_remove_prop(invalid_handle_value(), 0),
            RemovePropClassification::Removed
        );
    }
}
