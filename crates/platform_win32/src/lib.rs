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

mod enumeration;
mod event_hooks;
mod placement;
mod types;
mod utils;

pub use gestures::*;
pub use hotkeys::*;
pub use mouse_hook::*;

// Re-export public API from submodules
pub use enumeration::{
    enumerate_monitors, enumerate_windows, find_monitor_by_id, find_monitor_for_rect,
    get_primary_monitor, get_process_executable, get_window_info, monitor_to_left,
    monitor_to_right, monitors_by_position,
};
pub use event_hooks::{install_event_hooks, EventHookHandle, WindowEvent};
pub use placement::{
    apply_placements, clear_inset_cache, dwm_uncloak_all, dwm_uncloak_window,
    is_placement_cloaked, ApplyPlacementsResult, PlacementCache, WidthViolation,
};
pub use types::{MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo};
pub use utils::{
    are_animations_enabled, cascade_windows, close_window, get_cursor_pos, is_on_battery_or_power_saver,
    get_system_highlight_color_bgr, get_window_visible_rect, is_high_contrast_enabled,
    is_cursor_on_resize_border, is_move_offscreen_sentinel_position,
    is_move_offscreen_sentinel_rect, is_shift_key_pressed, is_valid_window,
    is_window_alive_and_visible, is_window_visible, move_window_offscreen,
    reset_window_border_color, restore_all_windows_moved_offscreen_best_effort,
    restore_window_moved_offscreen, restore_windows_moved_offscreen, set_dpi_awareness,
    set_foreground_window, set_window_border_color, uncloak_all_managed_windows,
    uncloak_all_visible_windows,
};

use leopardwm_core_layout::WindowId;
use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::WM_USER;

/// Sentinel coordinate used by MoveOffScreen strategy.
pub const MOVE_OFFSCREEN_SENTINEL_COORD: i32 = -100_000;

/// Custom message to signal the gesture/mouse-hook thread to stop.
pub(crate) const WM_QUIT_LLHOOK_THREAD: u32 = WM_USER + 2;

/// Recover from a poisoned mutex, logging a warning.
///
/// When a thread panics while holding a mutex, the mutex becomes "poisoned".
/// This helper logs the event and recovers the inner data so the application
/// can continue operating.
pub(crate) fn recover_poisoned_mutex<T>(
    err: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>,
) -> std::sync::MutexGuard<'_, T> {
    eprintln!("[leopardwm] WARNING: Mutex poisoned, recovering");
    err.into_inner()
}

/// Convert a WindowId to an HWND safely, returning an error for null (zero) IDs.
///
/// A WindowId of 0 would produce a null HWND pointer, which is invalid for
/// most Win32 window operations.
pub(crate) fn window_id_to_hwnd(id: WindowId) -> Result<HWND, Win32Error> {
    if id == 0 {
        return Err(Win32Error::WindowNotFound(id));
    }
    Ok(HWND(id as *mut c_void))
}

pub(crate) fn combine_operation_failures(context: &str, failures: Vec<String>) -> Win32Error {
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
pub(crate) fn is_benign_side_effect_error(error: &Win32Error) -> bool {
    matches!(
        error,
        Win32Error::WindowNotFound(window_id) if *window_id != 0
    )
}

// Re-export pub(crate) items needed by sibling modules (mouse_hook, etc.)
pub(crate) use enumeration::normalize_to_root_window;
pub(crate) use enumeration::should_emit_window_event;

