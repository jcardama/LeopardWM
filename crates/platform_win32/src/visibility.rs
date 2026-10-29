//! Off-screen sentinel parking, restore/uncloak recovery, and window positioning.

use crate::enumeration::{collect_all_top_level_window_ids, get_primary_monitor};
use crate::placement::apply_placements;
use crate::types::{PlatformConfig, Win32Error};
use crate::window_identity::{capture_window_identity, window_identity_matches, WindowIdentity};
use crate::window_style::reset_window_border_color;
use crate::MOVE_OFFSCREEN_SENTINEL_COORD;
use crate::{combine_operation_failures, is_benign_side_effect_error, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsIconic, IsWindow, SetWindowPos, ShowWindow, HWND_TOP, SET_WINDOW_POS_FLAGS,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};

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
    move_window_offscreen_with_flags(window_id, SET_WINDOW_POS_FLAGS(0)).map(|_| ())
}

pub fn queue_window_offscreen(window_id: WindowId) -> Result<(), Win32Error> {
    let origin = crate::placement::observed_async_position(window_id_to_hwnd(window_id)?);
    if !move_window_offscreen_with_flags(window_id, SWP_ASYNCWINDOWPOS)? {
        crate::placement::record_queued_owner_position(
            window_id,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            origin,
        );
    }
    Ok(())
}

fn move_window_offscreen_with_flags(
    window_id: WindowId,
    extra_flags: SET_WINDOW_POS_FLAGS,
) -> Result<bool, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    if crate::placement::seal_marked_position(
        hwnd,
        window_id,
        MOVE_OFFSCREEN_SENTINEL_COORD,
        MOVE_OFFSCREEN_SENTINEL_COORD,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    ) {
        return Ok(true);
    }
    unsafe {
        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            MOVE_OFFSCREEN_SENTINEL_COORD,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | extra_flags,
        ) {
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to move window {} offscreen: {}",
                window_id, e
            )));
        }
    }
    Ok(false)
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
    crate::placement::note_synchronous_move(window_id);
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

fn compute_restore_rect_from_offscreen(current_rect: &Rect, work_area: &Rect) -> Rect {
    let max_width = work_area.width.max(1);
    let max_height = work_area.height.max(1);
    let width = current_rect.width.max(1).min(max_width);
    let height = current_rect.height.max(1).min(max_height);
    Rect::new(work_area.x, work_area.y, width, height)
}

fn recovery_work_areas() -> Option<Vec<Rect>> {
    match crate::enumeration::enumerate_monitors() {
        Ok(monitors) if !monitors.is_empty() => Some(
            monitors
                .into_iter()
                .map(|monitor| monitor.work_area)
                .collect(),
        ),
        Ok(_) => {
            tracing::warn!(
                "Skipping non-sentinel queued owner recovery: monitor enumeration returned no monitors"
            );
            None
        }
        Err(error) => {
            tracing::warn!(%error, "Skipping non-sentinel queued owner recovery: monitor enumeration failed");
            None
        }
    }
}

fn queued_target_is_offscreen(target: &Rect, work_areas: Option<&[Rect]>) -> bool {
    is_move_offscreen_sentinel_rect(target)
        || work_areas.is_some_and(|areas| areas.iter().all(|area| !target.intersects(area)))
}

fn restore_window_if_offscreen_to_work_area(
    window_id: WindowId,
    work_area: &Rect,
    work_areas: Option<&[Rect]>,
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

        let owner_target = crate::placement::owner_wait_target(window_id, &current_rect);
        let owner_wait = owner_target.is_some();
        let restore_needed = owner_target.as_ref().map_or_else(
            || is_move_offscreen_sentinel_rect(&current_rect),
            |target| queued_target_is_offscreen(target, work_areas),
        );
        if !restore_needed {
            return Ok(false);
        }

        let restore_rect = compute_restore_rect_from_offscreen(&current_rect, work_area);
        let async_flag = if owner_wait {
            SWP_ASYNCWINDOWPOS
        } else {
            SET_WINDOW_POS_FLAGS(0)
        };

        if !crate::placement::can_recover_placement_owner(window_id, hwnd, None) {
            return Ok(false);
        }
        if !async_flag.contains(SWP_ASYNCWINDOWPOS) {
            crate::placement::note_synchronous_move(window_id);
        }
        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            restore_rect.x,
            restore_rect.y,
            restore_rect.width,
            restore_rect.height,
            SWP_NOZORDER | SWP_NOACTIVATE | async_flag,
        ) {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(Win32Error::WindowNotFound(window_id));
            }
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to restore off-screen window {}: {}",
                window_id, e
            )));
        }
        if owner_wait {
            crate::placement::record_queued_owner_rect(
                window_id,
                &restore_rect,
                Some((current_rect.x, current_rect.y)),
            );
        }
    }

    Ok(true)
}

// Recovery also runs in DPI-unaware CLI/watchdog threads. Keep monitor and window
// coordinates physical for the entire operation, then restore the caller's context.
// Per-monitor v1 is sufficient here and also works on Windows 10 before v2 support.
struct RecoveryDpiContext(DPI_AWARENESS_CONTEXT);

impl RecoveryDpiContext {
    fn enter() -> Result<Self, Win32Error> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) };
        if previous.0.is_null() {
            return Err(Win32Error::SetPositionFailed(format!(
                "Failed to establish physical recovery coordinates: {}",
                windows::core::Error::from_thread()
            )));
        }
        Ok(Self(previous))
    }
}

impl Drop for RecoveryDpiContext {
    fn drop(&mut self) {
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

// A timed-out native move stays in flight across recovery passes until its owner returns.
static RECOVERY_MOVES: LazyLock<Mutex<HashSet<WindowId>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct RecoveryMove(WindowId);

impl Drop for RecoveryMove {
    fn drop(&mut self) {
        RECOVERY_MOVES
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .remove(&self.0);
    }
}

fn emergency_marked_move(
    id: WindowId,
    restored: Rect,
    origin: Option<(i32, i32)>,
    deadline: Instant,
) {
    let Some(identity) = window_id_to_hwnd(id).ok().and_then(capture_window_identity) else {
        return;
    };
    emergency_marked_move_with_identity(id, restored, origin, deadline, identity);
}

fn emergency_marked_move_with_identity(
    id: WindowId,
    restored: Rect,
    origin: Option<(i32, i32)>,
    deadline: Instant,
    identity: WindowIdentity,
) {
    if Instant::now() >= deadline
        || !RECOVERY_MOVES
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex)
            .insert(id)
    {
        return;
    }
    let in_flight = RecoveryMove(id);
    let (done_tx, done_rx) = mpsc::channel();
    let helper = std::thread::Builder::new()
        .name("window-recovery-move".into())
        .spawn(move || {
            let _in_flight = in_flight;
            let Ok(_dpi) = RecoveryDpiContext::enter() else {
                return;
            };
            let Ok(hwnd) = window_id_to_hwnd(id) else {
                return;
            };
            if Instant::now() < deadline
                && window_identity_matches(hwnd, identity)
                && unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        restored.x,
                        restored.y,
                        restored.width,
                        restored.height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                }
                .is_ok()
                && origin.is_some()
            {
                crate::placement::record_queued_owner_rect(id, &restored, origin);
            }
            let _ = done_tx.send(());
        });
    if helper.is_ok() {
        let budget =
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now()));
        let _ = done_rx.recv_timeout(budget);
    }
}

fn emergency_sentinel_pass(
    window_ids: &[WindowId],
    work_area: &Rect,
    work_areas: Option<&[Rect]>,
    deadline: std::time::Instant,
) {
    for &id in window_ids {
        if crate::placement::is_marked_placement_owner(id) && std::time::Instant::now() >= deadline
        {
            break;
        }
        let Ok(hwnd) = window_id_to_hwnd(id) else {
            continue;
        };
        unsafe {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                continue;
            }
            let current = Rect::new(
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            );
            let owner_target = crate::placement::owner_wait_target(id, &current);
            let owner_wait = owner_target.is_some();
            let restore_needed = owner_target.as_ref().map_or_else(
                || is_move_offscreen_sentinel_rect(&current),
                |target| queued_target_is_offscreen(target, work_areas),
            );
            if !restore_needed {
                continue;
            }
            let restored = compute_restore_rect_from_offscreen(&current, work_area);
            if crate::placement::is_marked_placement_owner(id)
                && RECOVERY_MOVES
                    .lock()
                    .unwrap_or_else(crate::recover_poisoned_mutex)
                    .contains(&id)
            {
                continue;
            }
            if !crate::placement::can_recover_placement_owner(id, hwnd, Some(deadline)) {
                continue;
            }
            if crate::placement::is_marked_placement_owner(id) {
                emergency_marked_move(
                    id,
                    restored,
                    owner_wait.then_some((current.x, current.y)),
                    deadline,
                );
                continue;
            }
            if SetWindowPos(
                hwnd,
                None,
                restored.x,
                restored.y,
                restored.width,
                restored.height,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            )
            .is_ok()
                && owner_wait
            {
                crate::placement::record_queued_owner_rect(
                    id,
                    &restored,
                    Some((current.x, current.y)),
                );
            }
        }
    }
}

fn emergency_primary_work_area() -> Option<Rect> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let work = info.rcWork;
        Some(Rect::new(
            work.left,
            work.top,
            work.right - work.left,
            work.bottom - work.top,
        ))
    }
}

/// Best-effort tray Quit recovery. The caller must independently enforce `deadline`:
/// shell, DWM, tracing, and style calls can block.
pub fn emergency_restore_windows(window_ids: &[WindowId], deadline: std::time::Instant) {
    crate::placement::stop_placement_mover();
    let Ok(_dpi) = RecoveryDpiContext::enter() else {
        return;
    };
    let Some(work_area) = emergency_primary_work_area() else {
        return;
    };
    let work_areas = recovery_work_areas();
    let (marked, unmarked): (Vec<_>, Vec<_>) = window_ids
        .iter()
        .copied()
        .partition(|&id| crate::placement::is_marked_placement_owner(id));
    emergency_sentinel_pass(&unmarked, &work_area, work_areas.as_deref(), deadline);
    tracing::warn!("Tray Quit fallback fired: shutdown did not complete before its deadline");
    crate::placement::emergency_uncloak_tracked(window_ids);
    if let Some(stopped) = crate::taskbar::emergency_disconnect() {
        while !stopped.load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    crate::window_style::emergency_restore_maximizebox(&unmarked, deadline);
    emergency_sentinel_pass(&unmarked, &work_area, work_areas.as_deref(), deadline);
    emergency_sentinel_pass(&marked, &work_area, work_areas.as_deref(), deadline);
    crate::window_style::emergency_restore_maximizebox(&marked, deadline);
    emergency_sentinel_pass(&marked, &work_area, work_areas.as_deref(), deadline);
    while Instant::now() < deadline {
        let (ready, moving) = {
            let moves = RECOVERY_MOVES
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex);
            let ready: Vec<_> = marked
                .iter()
                .copied()
                .filter(|id| !moves.contains(id))
                .collect();
            (ready, !moves.is_empty())
        };
        crate::window_style::emergency_restore_maximizebox(&ready, deadline);
        if !moving {
            break;
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(crate) fn recover_after_placement_mover(
    window_id: WindowId,
    before: Option<RECT>,
    is_current: impl Fn() -> bool,
) {
    let Ok(_dpi) = RecoveryDpiContext::enter() else {
        return;
    };
    let Ok(primary) = get_primary_monitor() else {
        return;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return;
    };
    let work_areas = recovery_work_areas().unwrap_or_else(|| vec![primary.work_area]);
    let before = before.map(|r| Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top));
    let restored =
        if let Some(before) = before.filter(|r| work_areas.iter().any(|area| r.intersects(area))) {
            before
        } else {
            let mut current = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut current) }.is_err() {
                return;
            }
            compute_restore_rect_from_offscreen(
                &Rect::new(
                    current.left,
                    current.top,
                    current.right - current.left,
                    current.bottom - current.top,
                ),
                &primary.work_area,
            )
        };
    if !is_current() {
        return;
    }
    crate::placement::dwm_uncloak_window(window_id);
    if !is_current() {
        return;
    }
    if let Err(error) = unsafe {
        SetWindowPos(
            hwnd,
            None,
            restored.x,
            restored.y,
            restored.width,
            restored.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    } {
        tracing::warn!(window_id, %error, "Could not recover a late placement mover call");
    }
}

/// Restore a window parked at sentinel coordinates or queued off-screen to the
/// primary monitor.
///
/// Returns `Ok(true)` for a completed or queued restore, `Ok(false)` if recovery
/// is unnecessary or the placement owner cannot be recovered, and `Err` on failure.
pub fn restore_window_moved_offscreen(window_id: WindowId) -> Result<bool, Win32Error> {
    let _dpi = RecoveryDpiContext::enter()?;
    let primary = get_primary_monitor()?;
    let work_areas = recovery_work_areas();
    restore_window_if_offscreen_to_work_area(window_id, &primary.work_area, work_areas.as_deref())
}

pub(crate) fn restore_windows_moved_offscreen_with_work_area<F>(
    window_ids: &[WindowId],
    work_area: &Rect,
    work_areas: Option<&[Rect]>,
    mut restore_one: F,
) -> (usize, Vec<String>)
where
    F: FnMut(WindowId, &Rect, Option<&[Rect]>) -> Result<bool, Win32Error>,
{
    let mut restored_count: usize = 0;
    let mut failures: Vec<String> = Vec::new();

    let mut window_ids = window_ids.to_vec();
    window_ids.sort_by_key(|&id| crate::placement::is_marked_placement_owner(id));
    for window_id in window_ids {
        match restore_one(window_id, work_area, work_areas) {
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

/// Restore windows parked at MoveOffScreen sentinel coordinates or queued off-screen.
///
/// Returns the number of restored windows or queued restores. If any window restore
/// fails, this returns an aggregated error after attempting all windows.
pub fn restore_windows_moved_offscreen(window_ids: &[WindowId]) -> Result<usize, Win32Error> {
    restore_offscreen_windows(window_ids, None)
}

/// Restore unmarked windows, uncloak managed state, then restore marked windows.
/// Counts and aggregated errors follow `restore_windows_moved_offscreen`.
pub fn restore_windows_moved_offscreen_and_uncloak_managed(
    window_ids: &[WindowId],
    managed_window_ids: &[WindowId],
) -> Result<usize, Win32Error> {
    restore_offscreen_windows(window_ids, Some(managed_window_ids))
}

fn restore_offscreen_windows(
    window_ids: &[WindowId],
    mut managed_window_ids: Option<&[WindowId]>,
) -> Result<usize, Win32Error> {
    crate::placement::stop_placement_mover();
    if window_ids.is_empty() {
        return Ok(0);
    }

    let _dpi = RecoveryDpiContext::enter()?;
    let primary = get_primary_monitor()?;
    let work_areas = recovery_work_areas();
    let (restored_count, failures) = restore_windows_moved_offscreen_with_work_area(
        window_ids,
        &primary.work_area,
        work_areas.as_deref(),
        |window_id, work_area, work_areas| {
            if crate::placement::is_marked_placement_owner(window_id) {
                if let Some(managed_window_ids) = managed_window_ids.take() {
                    uncloak_managed_window_state(managed_window_ids);
                }
            }
            restore_window_if_offscreen_to_work_area(window_id, work_area, work_areas)
        },
    );
    if let Some(managed_window_ids) = managed_window_ids {
        uncloak_managed_window_state(managed_window_ids);
    }

    if !failures.is_empty() {
        return Err(combine_operation_failures(
            "Failed to restore one or more MoveOffScreen windows",
            failures,
        ));
    }

    Ok(restored_count)
}

fn uncloak_managed_window_state(window_ids: &[WindowId]) {
    crate::dwm_uncloak_all();

    for &wid in window_ids {
        if wid == 0 {
            continue;
        }
        let _ = reset_window_border_color(wid);
    }
}

/// Uncloak managed windows, reset border colors, and recover sentinel or queued
/// off-screen placements to the primary monitor, best-effort.
///
/// Restore errors are logged rather than returned.
pub fn uncloak_all_managed_windows(window_ids: &[WindowId]) {
    uncloak_managed_window_state(window_ids);
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

struct OffscreenRecovery {
    restored_count: usize,
    error: Option<Win32Error>,
}

fn restore_all_windows_moved_offscreen(include_marked: bool) -> OffscreenRecovery {
    let _dpi = match RecoveryDpiContext::enter() {
        Ok(context) => context,
        Err(error) => {
            return OffscreenRecovery {
                restored_count: 0,
                error: Some(error),
            }
        }
    };
    let primary = match get_primary_monitor() {
        Ok(primary) => primary,
        Err(error) => {
            return OffscreenRecovery {
                restored_count: 0,
                error: Some(error),
            }
        }
    };
    let mut collection = collect_all_top_level_window_ids();
    if !include_marked {
        collection
            .window_ids
            .retain(|&id| !crate::placement::is_marked_placement_owner(id));
    }
    let work_areas = recovery_work_areas();
    let (restored_count, mut failures) = restore_windows_moved_offscreen_with_work_area(
        &collection.window_ids,
        &primary.work_area,
        work_areas.as_deref(),
        restore_window_if_offscreen_to_work_area,
    );
    if let Some(error) = collection.error {
        failures.push(format!("top-level window enumeration: {error}"));
    }
    let error = (!failures.is_empty()).then(|| {
        combine_operation_failures(
            "Failed to restore one or more MoveOffScreen windows",
            failures,
        )
    });
    OffscreenRecovery {
        restored_count,
        error,
    }
}

/// Recover top-level windows parked at sentinel coordinates or queued off-screen,
/// best-effort. Returns the number of completed or queued restores.
///
/// Does not require daemon state, so it can run from panic hooks when that state
/// is unavailable or poisoned.
pub fn restore_all_windows_moved_offscreen_best_effort() -> usize {
    crate::placement::stop_placement_mover();
    let recovery = restore_all_windows_moved_offscreen(true);
    if let Some(error) = recovery.error {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore had a failure: {}",
            error
        );
    }
    if recovery.restored_count > 0 {
        eprintln!(
            "[leopardwm] Emergency MoveOffScreen restore recovered {} window(s)",
            recovery.restored_count
        );
    }
    recovery.restored_count
}

pub(crate) fn uncloak_unmarked_windows_for_recovery() {
    crate::placement::stop_placement_mover();
    crate::dwm_uncloak_all();
    let recovery = restore_all_windows_moved_offscreen(false);
    if let Some(error) = recovery.error {
        eprintln!("[leopardwm] Unmarked window recovery had a failure: {error}");
    }
}

/// Uncloak tracked windows and recover top-level sentinel or queued off-screen
/// placements, best-effort.
///
/// Does not require daemon state, so it can run from panic hooks when that state
/// is unavailable or poisoned.
pub fn uncloak_all_visible_windows() {
    crate::dwm_uncloak_all();
    let _ = restore_all_windows_moved_offscreen_best_effort();
    // eprintln because tracing may not work in a panic hook
    eprintln!("[leopardwm] Emergency window restore complete");
}

/// Cascade windows from the primary monitor work area's origin.
///
/// Each window uses 50% of the work-area height with approximately 4:3 width
/// and 30px offsets. Off-screen windows are first restored, then cascaded.
pub fn cascade_windows(window_ids: &[WindowId]) -> Result<(), Win32Error> {
    crate::placement::cancel_window_placement_moves(window_ids);
    let work_area =
        get_primary_monitor().map_or(Rect::new(0, 0, 1920, 1080), |monitor| monitor.work_area);
    cascade_windows_with(
        window_ids,
        work_area,
        || {
            for &window_id in window_ids {
                crate::placement::dwm_uncloak_window(window_id);
            }
            let recovery = restore_all_windows_moved_offscreen(true);
            tracing::info!(
                restored_count = recovery.restored_count,
                partial = recovery.error.is_some(),
                "Release MoveOffScreen recovery completed"
            );
            recovery.error.map_or(Ok(()), Err)
        },
        restore_minimized_window,
        |placements| apply_placements(placements, &PlatformConfig::default(), None, true),
        crate::is_window_valid,
    )
}

fn restore_minimized_window(window_id: WindowId) -> Result<(), Win32Error> {
    restore_minimized_window_with(
        window_id,
        |window_id| unsafe { IsIconic(HWND(window_id as *mut c_void)).as_bool() },
        |window_id| unsafe {
            let _ = ShowWindow(HWND(window_id as *mut c_void), SW_RESTORE);
        },
        crate::is_window_valid,
    )
}

fn restore_minimized_window_with<I, R, L>(
    window_id: WindowId,
    mut is_iconic: I,
    mut restore: R,
    mut is_live: L,
) -> Result<(), Win32Error>
where
    I: FnMut(WindowId) -> bool,
    R: FnMut(WindowId),
    L: FnMut(WindowId) -> bool,
{
    if !is_iconic(window_id) {
        return Ok(());
    }

    restore(window_id);
    if !is_iconic(window_id) || !is_live(window_id) {
        return Ok(());
    }

    Err(Win32Error::SetPositionFailed(format!(
        "Window {window_id} remained minimized after restore"
    )))
}

fn cascade_windows_with<R, M, A, L>(
    window_ids: &[WindowId],
    work_area: Rect,
    mut recover: R,
    mut restore_minimized: M,
    mut apply: A,
    mut is_live: L,
) -> Result<(), Win32Error>
where
    R: FnMut() -> Result<(), Win32Error>,
    M: FnMut(WindowId) -> Result<(), Win32Error>,
    A: FnMut(&[WindowPlacement]) -> Result<crate::placement::ApplyPlacementsResult, Win32Error>,
    L: FnMut(WindowId) -> bool,
{
    let recovery_error = recover().err();
    // Use height as the base so windows look reasonable on ultrawide monitors.
    let cascade_h = (work_area.height as f64 * 0.5) as i32;
    let cascade_w = (cascade_h as f64 * 1.33) as i32; // Approximately 4:3.
    let placements: Vec<WindowPlacement> = window_ids
        .iter()
        .enumerate()
        .map(|(index, &window_id)| {
            let offset = index as i32 * 30;
            WindowPlacement {
                window_id,
                rect: Rect::new(
                    work_area.x + offset,
                    work_area.y + offset,
                    cascade_w,
                    cascade_h,
                ),
                visibility: Visibility::Visible,
                column_index: 0,
                row_index: 0,
            }
        })
        .collect();

    let mut failures = Vec::new();
    if let Some(error) = recovery_error {
        failures.push(format!("MoveOffScreen recovery: {error}"));
    }
    for &window_id in window_ids {
        if let Err(error) = restore_minimized(window_id) {
            if is_benign_side_effect_error(&error) || !is_live(window_id) {
                tracing::debug!(
                    "Ignoring vanished window {} during cascade restore: {}",
                    window_id,
                    error
                );
            } else {
                failures.push(format!("restore minimized window {window_id}: {error}"));
            }
        }
    }
    match apply(&placements) {
        Ok(result) => {
            for window_id in result.maximized_skipped_window_ids {
                if is_live(window_id) {
                    failures.push(format!(
                        "window {window_id} was skipped because it remained maximized"
                    ));
                }
            }
            for landing in result.landings {
                if (landing.failed || (landing.unreadable && !landing.measurement_deferred))
                    && is_live(landing.window_id)
                {
                    failures.push(format!(
                        "window {} could not be {} after cascade",
                        landing.window_id,
                        if landing.failed {
                            "positioned"
                        } else {
                            "verified"
                        }
                    ));
                }
            }
        }
        Err(error) => failures.push(format!("apply cascade placements: {error}")),
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(combine_operation_failures(
            "Failed to cascade one or more windows",
            failures,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_UNAWARE,
    };

    #[test]
    fn cascade_continues_after_enumeration_failure_and_reports_it() {
        let window_ids = [10, 20];
        let mut restored = Vec::new();
        let mut applied = Vec::new();
        let error = cascade_windows_with(
            &window_ids,
            Rect::new(0, 0, 1920, 1080),
            || {
                Err(Win32Error::EnumerationFailed(
                    "EnumWindows failed".to_string(),
                ))
            },
            |window_id| {
                restored.push(window_id);
                Ok(())
            },
            |placements| {
                applied.extend(placements.iter().map(|placement| placement.window_id));
                Ok(Default::default())
            },
            |_| true,
        )
        .unwrap_err();

        assert_eq!(restored, window_ids);
        assert_eq!(applied, window_ids);
        assert!(error.to_string().contains("EnumWindows failed"));
    }

    #[test]
    fn cascade_reports_live_failed_and_unreadable_landings_without_size_mismatch() {
        let result = cascade_windows_with(
            &[10, 20, 30],
            Rect::new(0, 0, 1920, 1080),
            || Ok(()),
            |_| Ok(()),
            |placements| {
                Ok(crate::placement::ApplyPlacementsResult {
                    landings: vec![
                        crate::placement::PlacementLanding {
                            window_id: 10,
                            requested_rect: placements[0].rect,
                            requested_visibility: Visibility::Visible,
                            actual_visible_rect: None,
                            actual_outer_rect: None,
                            failed: true,
                            unreadable: false,
                            measurement_deferred: false,
                        },
                        crate::placement::PlacementLanding {
                            window_id: 20,
                            requested_rect: placements[1].rect,
                            requested_visibility: Visibility::Visible,
                            actual_visible_rect: None,
                            actual_outer_rect: None,
                            failed: false,
                            unreadable: true,
                            measurement_deferred: false,
                        },
                    ],
                    ..Default::default()
                })
            },
            |_| true,
        )
        .unwrap_err();
        assert!(result.to_string().contains("10"));
        assert!(result.to_string().contains("20"));
    }

    #[test]
    fn cascade_accepts_successful_landings_with_size_mismatches() {
        assert!(cascade_windows_with(
            &[10],
            Rect::new(0, 0, 1920, 1080),
            || Ok(()),
            |_| Ok(()),
            |placements| {
                Ok(crate::placement::ApplyPlacementsResult {
                    landings: vec![crate::placement::PlacementLanding {
                        window_id: 10,
                        requested_rect: placements[0].rect,
                        requested_visibility: Visibility::Visible,
                        actual_visible_rect: Some(Rect::new(0, 0, 800, 600)),
                        actual_outer_rect: Some(Rect::new(0, 0, 804, 604)),
                        failed: false,
                        unreadable: false,
                        measurement_deferred: false,
                    }],
                    ..Default::default()
                })
            },
            |_| true,
        )
        .is_ok());
    }

    #[test]
    fn cascade_reports_outer_apply_error_after_attempting_every_restore() {
        let mut restored = Vec::new();
        let error = cascade_windows_with(
            &[10, 20],
            Rect::new(0, 0, 1920, 1080),
            || Ok(()),
            |window_id| {
                restored.push(window_id);
                Ok(())
            },
            |_| Err(Win32Error::SetPositionFailed("apply failed".to_string())),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(restored, [10, 20]);
        assert!(error.to_string().contains("apply failed"));
    }

    #[test]
    fn cascade_ignores_vanished_windows_but_reports_live_skipped_outcomes() {
        let error = cascade_windows_with(
            &[10, 20, 30, 40, 50],
            Rect::new(0, 0, 1920, 1080),
            || Ok(()),
            |window_id| {
                if window_id == 10 {
                    Err(Win32Error::SetPositionFailed(
                        "destroyed during restore".to_string(),
                    ))
                } else {
                    Ok(())
                }
            },
            |placements| {
                Ok(crate::placement::ApplyPlacementsResult {
                    maximized_skipped_window_ids: vec![20, 30],
                    landings: vec![
                        crate::placement::PlacementLanding {
                            window_id: 40,
                            requested_rect: placements[3].rect,
                            requested_visibility: Visibility::Visible,
                            actual_visible_rect: None,
                            actual_outer_rect: None,
                            failed: true,
                            unreadable: false,
                            measurement_deferred: false,
                        },
                        crate::placement::PlacementLanding {
                            window_id: 50,
                            requested_rect: placements[4].rect,
                            requested_visibility: Visibility::Visible,
                            actual_visible_rect: None,
                            actual_outer_rect: None,
                            failed: false,
                            unreadable: true,
                            measurement_deferred: false,
                        },
                    ],
                    ..Default::default()
                })
            },
            |window_id| !matches!(window_id, 10 | 30 | 40),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("20"));
        assert!(message.contains("50"));
        assert!(!message.contains("destroyed during restore"));
        assert!(!message.contains("window 30"));
        assert!(!message.contains("window 40"));
    }

    #[test]
    fn cascade_rechecks_liveness_after_real_minimized_restore_failure() {
        for live_at_caller in [false, true] {
            let liveness_reads = std::cell::Cell::new(0);
            let is_live = |_| {
                let read = liveness_reads.get();
                liveness_reads.set(read + 1);
                read == 0 || live_at_caller
            };
            let result = cascade_windows_with(
                &[10],
                Rect::new(0, 0, 1920, 1080),
                || Ok(()),
                |window_id| restore_minimized_window_with(window_id, |_| true, |_| {}, is_live),
                |_| Ok(Default::default()),
                is_live,
            );
            assert_eq!(liveness_reads.get(), 2);
            if live_at_caller {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("remained minimized"));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn restore_minimized_window_reports_a_live_window_that_stays_iconic() {
        let mut iconic_states = [true, true].into_iter();
        let error = restore_minimized_window_with(
            10,
            |_| iconic_states.next().unwrap_or(true),
            |_| {},
            |_| true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("remained minimized"));
    }

    #[test]
    fn restore_minimized_window_ignores_a_window_that_vanishes_during_restore() {
        let mut iconic_states = [true, true].into_iter();
        assert!(restore_minimized_window_with(
            10,
            |_| iconic_states.next().unwrap_or(true),
            |_| {},
            |_| false,
        )
        .is_ok());
    }

    #[test]
    fn cascade_empty_input_applies_empty_placements() {
        let mut applied = false;
        assert!(cascade_windows_with(
            &[],
            Rect::new(0, 0, 1920, 1080),
            || Ok(()),
            |_| unreachable!("no windows should be restored"),
            |placements| {
                assert!(placements.is_empty());
                applied = true;
                Ok(Default::default())
            },
            |_| true,
        )
        .is_ok());
        assert!(applied);
    }

    fn set_test_dpi_context(context: DPI_AWARENESS_CONTEXT) -> RecoveryDpiContext {
        let previous = unsafe { SetThreadDpiAwarenessContext(context) };
        assert!(!previous.0.is_null());
        RecoveryDpiContext(previous)
    }

    fn assert_dpi_context(context: DPI_AWARENESS_CONTEXT) {
        assert!(unsafe {
            AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), context).as_bool()
        });
    }

    #[test]
    fn test_recovery_dpi_context_restores_caller_on_all_exits() {
        let _caller = set_test_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
        {
            let _dpi = RecoveryDpiContext::enter().unwrap();
            assert_dpi_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        }
        assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);

        let result = (|| -> Result<(), Win32Error> {
            let _dpi = RecoveryDpiContext::enter()?;
            assert_dpi_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            Err(Win32Error::WindowNotFound(0))
        })();
        assert!(result.is_err());
        assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);

        let result = std::panic::catch_unwind(|| {
            let _dpi = RecoveryDpiContext::enter().unwrap();
            assert_dpi_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            panic!("test recovery context unwinding");
        });
        assert!(result.is_err());
        assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
    }

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
            None,
            |window_id, _, _| {
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
            None,
            |window_id, _, _| match window_id {
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
    fn test_move_offscreen_native_parking_is_recoverable() {
        let _serialize = crate::placement::lock_cloak_set_tests();
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, IsWindowVisible, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_POPUP,
        };

        struct OwnedWindow(HWND);
        impl Drop for OwnedWindow {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }

        unsafe {
            let _dpi = set_test_dpi_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            let window = OwnedWindow(
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("LeopardWM hidden parking test"),
                    WS_POPUP,
                    100,
                    100,
                    640,
                    480,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
            let window_id = window.0 .0 as usize as WindowId;
            move_window_offscreen(window_id).unwrap();

            let mut parked = RECT::default();
            GetWindowRect(window.0, &mut parked).unwrap();
            assert!(
                is_move_offscreen_sentinel_position(parked.left, parked.top),
                "native parking ({}, {}) must be recognized for recovery",
                parked.left,
                parked.top
            );
            assert_eq!(
                (parked.right - parked.left, parked.bottom - parked.top),
                (640, 480)
            );
            assert!(!IsWindowVisible(window.0).as_bool());

            let work_area = Rect::new(100, 100, 1920, 1080);
            assert!(restore_window_if_offscreen_to_work_area(window_id, &work_area, None).unwrap());
            let mut restored = RECT::default();
            GetWindowRect(window.0, &mut restored).unwrap();
            assert_eq!(
                (restored.left, restored.top, restored.right, restored.bottom),
                (100, 100, 740, 580)
            );
            assert!(!IsWindowVisible(window.0).as_bool());
            assert!(
                !restore_window_if_offscreen_to_work_area(window_id, &work_area, None).unwrap()
            );

            let primary = get_primary_monitor().unwrap();
            let work = primary.work_area;
            let restore_functions: [fn(WindowId) -> Result<bool, Win32Error>; 2] =
                [restore_window_moved_offscreen, |id| {
                    restore_windows_moved_offscreen(&[id]).map(|count| count == 1)
                }];
            for restore in restore_functions {
                move_window_offscreen(window_id).unwrap();
                {
                    let _caller = set_test_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
                    assert!(restore(window_id).unwrap());
                    assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
                    assert!(!restore(window_id).unwrap());
                    assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
                    assert!(restore(0).is_err());
                    assert_dpi_context(DPI_AWARENESS_CONTEXT_UNAWARE);
                }
                GetWindowRect(window.0, &mut restored).unwrap();
                assert_eq!(
                    (restored.left, restored.top, restored.right, restored.bottom),
                    (
                        work.x,
                        work.y,
                        work.x + 640.min(work.width.max(1)),
                        work.y + 480.min(work.height.max(1)),
                    )
                );
                assert!(!IsWindowVisible(window.0).as_bool());
                assert_dpi_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            }
        }
    }

    #[test]
    fn emergency_marked_move_rejects_changed_lifetime_and_moves_matching_identity() {
        let _owner_sweep = crate::placement::tests::OWNER_SWEEP
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        use crate::window_identity::stamp_managed_lifetime_token;
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        struct OwnedWindow(HWND);
        impl Drop for OwnedWindow {
            fn drop(&mut self) {
                crate::placement::cancel_window_placement_moves(&[self.0 .0 as usize as WindowId]);
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }

        let _dpi = RecoveryDpiContext::enter().unwrap();
        for changed_lifetime in [true, false] {
            let window = OwnedWindow(unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    None,
                    WS_POPUP,
                    100,
                    100,
                    640,
                    480,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            });
            let id = window.0 .0 as usize as WindowId;
            let token = stamp_managed_lifetime_token(id).unwrap();
            let identity = capture_window_identity(window.0).unwrap();
            let mut before = RECT::default();
            unsafe { GetWindowRect(window.0, &mut before) }.unwrap();
            let current = Rect::new(
                before.left,
                before.top,
                before.right - before.left,
                before.bottom - before.top,
            );
            let restored = Rect::new(200, 220, 320, 240);
            assert_ne!(current, restored);
            assert_eq!(crate::placement::owner_wait_target(id, &current), None);
            if changed_lifetime {
                assert_ne!(stamp_managed_lifetime_token(id).unwrap(), token);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let caller = std::thread::spawn(move || {
                if changed_lifetime {
                    emergency_marked_move_with_identity(
                        id,
                        restored,
                        Some((current.x, current.y)),
                        deadline,
                        identity,
                    );
                } else {
                    emergency_marked_move(id, restored, Some((current.x, current.y)), deadline);
                }
            });
            while !caller.is_finished() || RECOVERY_MOVES.lock().unwrap().contains(&id) {
                assert!(Instant::now() < deadline, "recovery helper did not finish");
                unsafe {
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&message);
                    }
                }
                std::thread::yield_now();
            }
            caller.join().unwrap();
            let mut after = RECT::default();
            unsafe { GetWindowRect(window.0, &mut after) }.unwrap();
            let actual = Rect::new(
                after.left,
                after.top,
                after.right - after.left,
                after.bottom - after.top,
            );
            if changed_lifetime {
                assert_eq!(actual, current, "stale identity must not move the window");
                assert_eq!(crate::placement::owner_wait_target(id, &actual), None);
            } else {
                assert_eq!(actual, restored, "matching identity must move the window");
                assert_eq!(
                    crate::placement::owner_wait_target(id, &actual),
                    Some(restored)
                );
            }
        }
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
}
