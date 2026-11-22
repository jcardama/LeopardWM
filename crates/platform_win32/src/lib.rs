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
pub mod focus_placeholder;
pub mod gestures;
pub mod hotkeys;
pub mod ipc_security;
pub mod keyboard_hook;
pub mod mouse_hook;
pub mod overlay;
pub mod overview;
mod raw_touchpad;
pub mod shell;
pub mod snapshot;
pub mod tab_strip;
pub mod taskbar;
pub mod thumbnail;
pub mod toast;

pub use tab_strip::{TabAction, TabActionEvent, TabCloseAction};

#[cfg(test)]
mod clipping_proof;
mod elevation;
mod enumeration;
mod event_hooks;
mod focus;
#[doc(hidden)]
pub mod inspect;
mod placement;
mod system;
mod types;
mod user_press;
mod virtual_desktop;
mod visibility;
mod window_identity;
mod window_query;
mod window_style;

pub use gestures::*;
pub use hotkeys::*;
pub use keyboard_hook::*;
pub use mouse_hook::*;
pub use user_press::{
    arm_drag_float, disarm_drag_float, drag_float_requested, install_mouse_press_hook,
    take_drag_float_request, user_press_count, MousePressHookHandle,
};

pub use elevation::{
    current_process_integrity, manage_block, window_manage_block, ManageBlock, INTEGRITY_HIGH,
    INTEGRITY_MEDIUM,
};
pub use enumeration::{
    collect_all_top_level_window_ids, enumerate_monitors, enumerate_windows, find_monitor_by_id,
    find_monitor_for_rect, get_primary_monitor, get_process_executable, get_window_info,
    get_window_title, is_excluded_tool_window_hwnd, is_excluded_topmost_popup_hwnd,
    is_excluded_window_class_hwnd, monitor_above, monitor_below, monitor_left_of,
    monitor_right_of, monitor_to_left, monitor_to_right, monitors_by_position,
    TopLevelWindowIdCollection,
};
pub use event_hooks::{install_event_hooks, EventHookHandle, MoveStartCapture, WindowEvent};
pub use focus::{
    can_raise_normal_window, close_window, current_event_time_ms, get_foreground_window,
    ms_since_last_user_input, raise_normal_window_no_activate, raise_window_no_activate,
    restore_window_no_activate, set_foreground_window, visible_window_z_order,
    warp_cursor_to_window,
};
pub use placement::apply_cloak_state;
pub use placement::clear_suspected_oversize;
pub use placement::{
    apply_display_change_placements, apply_placements, clear_inset_cache, drain_ghost_cloaked,
    dwm_cloak_window, dwm_uncloak_all, dwm_uncloak_window, forget_offscreen_placement,
    get_window_frame_insets, get_window_invisible_insets, get_window_style_bits,
    is_placement_cloaked, is_placement_parked, mark_ghost_cloaked, offscreen_placement_matches,
    park_window_for_placement, pending_async_position, set_dwm_transitions_disabled,
    stop_placement_mover, unmark_ghost_cloaked, visible_rect_to_frame_rect,
    wait_for_animation_positions, ApplyPlacementsResult, HeightViolation, PlacementCache,
    PlacementLanding, WidthViolation,
};
pub use system::{
    are_animations_enabled, get_system_highlight_color_bgr, get_windows_version,
    is_high_contrast_enabled, is_on_battery_or_power_saver, scale_px, set_dpi_awareness,
};
pub use types::{MonitorId, MonitorInfo, PlatformConfig, Win32Error, WindowInfo};
pub use virtual_desktop::{is_window_on_current_virtual_desktop, warm_virtual_desktop_queries};
pub use visibility::{
    cascade_windows, emergency_restore_windows, is_move_offscreen_sentinel_position,
    is_move_offscreen_sentinel_rect, move_window_offscreen, position_window,
    queue_window_offscreen, restore_all_windows_moved_offscreen_best_effort,
    restore_window_moved_offscreen, restore_windows_moved_offscreen,
    restore_windows_moved_offscreen_and_uncloak_managed, uncloak_all_managed_windows,
    uncloak_all_visible_windows,
};
pub use window_identity::{
    clear_window_lifetime_token, read_managed_lifetime_token, read_window_lifetime_token,
    stamp_managed_lifetime_token, stamp_window_lifetime_token,
};
pub use window_query::{
    cursor_is_over_window, get_cursor_pos, get_window_chrome_rect, get_window_corner_radius,
    get_window_icon, get_window_visible_rect, is_cursor_on_resize_border, is_dialog_like_window,
    is_fixed_caption_window, is_frameless_fixed_size_window, is_frameless_popup,
    is_shift_key_pressed, is_valid_window, is_window_maximized, is_window_shell_cloaked,
    is_window_valid, is_window_visible, window_minimized_state, window_presence, WindowPresence,
};
pub use window_style::{
    queue_maximized_window_restore, remove_maximizebox, reset_window_border_color,
    restore_maximizebox, restore_maximizebox_all, restore_maximizebox_panic_recovery,
    set_window_border_color, wait_for_window_style_requests,
};

use leopardwm_core_layout::WindowId;
use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::WM_USER;

/// Sentinel coordinate used by MoveOffScreen strategy.
/// USER32 clamps smaller positions to the signed 16-bit minimum; recovery must
/// recognize the coordinates that GetWindowRect actually returns.
pub const MOVE_OFFSCREEN_SENTINEL_COORD: i32 = i16::MIN as i32;

/// Custom message to signal the gesture/mouse-hook thread to stop.
pub(crate) const WM_QUIT_LLHOOK_THREAD: u32 = WM_USER + 2;

pub(crate) fn recover_poisoned_mutex<T>(
    err: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>,
) -> std::sync::MutexGuard<'_, T> {
    eprintln!("[leopardwm] WARNING: Mutex poisoned, recovering");
    err.into_inner()
}

// Zero would become a null HWND, which most window operations reject.
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

// A vanished window is a benign race; a null ID is a caller error.
pub(crate) fn is_benign_side_effect_error(error: &Win32Error) -> bool {
    matches!(
        error,
        Win32Error::WindowNotFound(window_id) if *window_id != 0
    )
}

pub(crate) use enumeration::normalize_to_root_window;
pub(crate) use enumeration::should_emit_window_event;
