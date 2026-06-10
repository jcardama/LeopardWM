//! Off-screen sentinel parking, restore/uncloak recovery, and window positioning.

use crate::enumeration::{collect_all_top_level_window_ids, get_primary_monitor};
use crate::placement::apply_placements;
use crate::types::{PlatformConfig, Win32Error};
use crate::window_style::reset_window_border_color;
use crate::{combine_operation_failures, is_benign_side_effect_error, window_id_to_hwnd};
use crate::MOVE_OFFSCREEN_SENTINEL_COORD;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsIconic, IsWindow, SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};

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

/// Synchronously move and resize a window to `rect` AND raise it to the
/// top of the normal (non-topmost) window band. No activation, no async.
///
/// Raising matters for a freshly-summoned scratchpad: the focus border
/// tracks the window at its own z-level, so the window must be above the
/// previously-focused window for the border to be visible. The move is
/// synchronous so the window's rect is correct immediately (the async
/// layout pass would otherwise leave it stale).
pub fn position_window(window_id: WindowId, rect: Rect) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOACTIVATE,
        )
        .map_err(|e| {
            Win32Error::SetPositionFailed(format!("Failed to position window {}: {}", window_id, e))
        })?;
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

    let _ = apply_placements(&placements, &PlatformConfig::default(), None, false);
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
    fn test_uncloak_all_managed_empty_list() {
        // Should not panic with an empty list
        uncloak_all_managed_windows(&[]);
    }

    #[test]
    #[ignore = "Calls real Win32 APIs against literal HWND values (999_999, 1_234_567) \
                that may collide with a live window on a running daemon and move it if \
                parked at MoveOffScreen sentinel coords. Run with: cargo test -- --ignored"]
    fn test_uncloak_all_managed_with_invalid_ids() {
        // Should not panic even with invalid window IDs (best-effort)
        uncloak_all_managed_windows(&[0, 999_999, 1_234_567]);
    }

    #[test]
    #[ignore = "Enumerates all system windows and moves any parked at MoveOffScreen sentinel \
                coords back to the primary monitor work area. Safe to run in isolation but \
                disrupts a concurrently-running daemon (mass retile + Chromium swap-chain \
                desync). Run with: cargo test -- --ignored"]
    fn test_uncloak_all_visible_windows_no_panic() {
        // EnumWindows should succeed; uncloaking random windows is best-effort
        uncloak_all_visible_windows();
    }
}
