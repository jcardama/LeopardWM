//! LeopardWM Platform Win32
//!
//! Windows-specific window manipulation using Win32 APIs.
//!
//! This crate handles:
//! - Window enumeration and filtering
//! - Window positioning via SetWindowPos
//! - WinEvent hooks for window lifecycle events
//! - Visual overlay for snap hints

pub mod autostart;
pub mod border;
pub mod dialog;
pub mod gestures;
pub mod hotkeys;
pub mod ipc_security;
pub mod keyboard_hook;
pub mod mouse_hook;
pub mod overlay;
pub mod overview;
pub mod snapshot;
pub mod tab_strip;
pub mod thumbnail;

pub use tab_strip::{TabAction, TabActionEvent, TabCloseAction};

mod enumeration;
mod event_hooks;
mod focus;
mod placement;
mod system;
mod types;
mod visibility;
mod window_query;
mod window_style;

pub use gestures::*;
pub use hotkeys::*;
pub use keyboard_hook::*;
pub use mouse_hook::*;

// Re-export public API from submodules
pub use enumeration::{
    enumerate_monitors, enumerate_windows, find_monitor_by_id, find_monitor_for_rect,
    get_primary_monitor, get_process_executable, get_window_info, monitor_to_left,
    monitor_to_right, monitors_by_position,
};
pub use event_hooks::{install_event_hooks, EventHookHandle, WindowEvent};
pub use placement::{
    apply_placements, clear_inset_cache, drain_ghost_cloaked, dwm_cloak_window, dwm_uncloak_all,
    dwm_uncloak_window, get_window_invisible_insets, is_placement_cloaked,
    mark_ghost_cloaked, set_dwm_transitions_disabled, unmark_ghost_cloaked,
    ApplyPlacementsResult, HeightViolation, PlacementCache, WidthViolation,
};
pub use placement::apply_cloak_state;
pub use types::{MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo};
pub use focus::{
    close_window, get_foreground_window, ms_since_last_user_input, set_foreground_window,
};
pub use system::{
    are_animations_enabled, get_system_highlight_color_bgr, is_high_contrast_enabled,
    is_on_battery_or_power_saver, scale_px, set_dpi_awareness,
};
pub use visibility::{
    cascade_windows, is_move_offscreen_sentinel_position, is_move_offscreen_sentinel_rect,
    move_window_offscreen, position_window, restore_all_windows_moved_offscreen_best_effort,
    restore_window_moved_offscreen, restore_windows_moved_offscreen, uncloak_all_managed_windows,
    uncloak_all_visible_windows,
};
pub use window_query::{
    get_cursor_pos, get_window_chrome_rect, get_window_corner_radius, get_window_icon,
    get_window_visible_rect, is_cursor_on_resize_border, is_shift_key_pressed, is_valid_window,
    is_window_alive_and_visible, is_window_maximized, is_window_shell_cloaked, is_window_valid,
    is_window_visible,
};
pub use window_style::{
    remove_maximizebox, reset_window_border_color, restore_maximizebox, restore_maximizebox_all,
    restore_maximizebox_panic_recovery, set_window_border_color,
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

