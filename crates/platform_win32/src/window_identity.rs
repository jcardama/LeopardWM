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
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{GetPropW, IsWindow, RemovePropW, SetPropW};

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
pub fn clear_window_lifetime_token(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = require_live_hwnd(window_id)?;
    let _ = unsafe { RemovePropW(hwnd, TOKEN_PROPERTY) };
    Ok(())
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
}
