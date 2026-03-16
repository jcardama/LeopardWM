//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::types::{PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetWindowRect, IsIconic, IsWindow,
    SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

/// Undocumented but well-known DWM attribute for cloaking windows.
/// Cloaked windows remain composed by DWM (surface stays alive) but are
/// invisible to the user. Used by the Windows shell for virtual desktops.
const DWMWA_CLOAK: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(13i32);

/// Set or clear the DWM cloak on a window.
unsafe fn dwm_set_cloak(hwnd: HWND, cloaked: bool) {
    let value = BOOL::from(cloaked);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CLOAK,
        &value as *const _ as _,
        std::mem::size_of::<BOOL>() as u32,
    );
}

/// Lock GLOBAL_CLOAKED, recovering from poison (a prior panic while holding
/// the lock). All access to the cloaked set goes through this helper so that
/// shutdown/panic cleanup paths never silently give up.
fn lock_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    GLOBAL_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Uncloak a window by its WindowId. Public for use in shutdown/panic cleanup.
pub fn dwm_uncloak_window(window_id: WindowId) {
    if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe { dwm_set_cloak(hwnd, false) };
    }
    let mut guard = lock_cloaked();
    if let Some(ref mut set) = *guard {
        set.remove(&window_id);
    }
}

/// Uncloak all windows tracked as cloaked by the placement system.
/// Called during shutdown and panic recovery.
pub fn dwm_uncloak_all() {
    let ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => return,
        }
    };
    for wid in ids {
        if let Ok(hwnd) = window_id_to_hwnd(wid) {
            unsafe { dwm_set_cloak(hwnd, false) };
        }
    }
}

/// Check if a window is currently cloaked by the placement system.
///
/// Used by the event hook to suppress spurious SHOW/LOCATIONCHANGE events
/// fired by DWM when we cloak/uncloak windows during placement.
pub fn is_placement_cloaked(window_id: WindowId) -> bool {
    let guard = lock_cloaked();
    guard
        .as_ref()
        .is_some_and(|set| set.contains(&window_id))
}

/// Drain and uncloak all tracked windows. Called when the placement list
/// becomes empty (e.g., switching to an empty workspace) so that windows
/// from the previous call are not left permanently invisible.
fn uncloak_all_tracked() {
    let ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => return,
        }
    };
    for wid in ids {
        if let Ok(hwnd) = window_id_to_hwnd(wid) {
            unsafe { dwm_set_cloak(hwnd, false) };
        }
    }
}

/// Global set of window IDs currently cloaked by the placement system.
static GLOBAL_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

/// Cache of last-applied window placements and border insets.
///
/// The position cache skips redundant SetWindowPos calls during animations.
/// The inset cache preserves known-good invisible border insets so that windows
/// returning from off-screen (where DWM may lose track of extended frame bounds)
/// are positioned correctly.
pub struct PlacementCache {
    positions: HashMap<WindowId, (Rect, Visibility)>,
    insets: HashMap<WindowId, (i32, i32, i32, i32)>,
}

impl Default for PlacementCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PlacementCache {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            insets: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.positions.clear();
        // Keep inset cache — insets are a window property, not position-dependent
    }

    /// Clear the cached border insets. Call when system theme or DWM metrics
    /// change (e.g., high contrast toggle) so that stale invisible-border
    /// values don't cause incorrect window sizing.
    pub fn clear_insets(&mut self) {
        self.insets.clear();
    }
}

/// A window whose actual width exceeds the requested placement width,
/// indicating it enforces a minimum size. The `min_width` is in layout
/// pixels (excludes invisible border insets).
#[derive(Debug, Clone)]
pub struct WidthViolation {
    pub window_id: WindowId,
    /// Minimum width in layout coordinates (border insets subtracted).
    pub min_width: i32,
}

/// Result of apply_placements, including any detected width violations.
pub struct ApplyPlacementsResult {
    /// Width violations detected after positioning (windows wider than requested).
    pub width_violations: Vec<WidthViolation>,
}

/// Apply window placements from the layout engine.
///
/// Visible windows are positioned immediately via SetWindowPos.
/// Off-screen windows are moved to sentinel coordinates far off-screen.
///
/// When `cache` is provided, placements whose rect and visibility match the
/// cached values are skipped, avoiding redundant Win32 calls during animations
/// where most windows haven't moved.
pub fn apply_placements(
    placements: &[WindowPlacement],
    _config: &PlatformConfig,
    mut cache: Option<&mut PlacementCache>,
) -> Result<ApplyPlacementsResult, Win32Error> {
    let empty_result = ApplyPlacementsResult { width_violations: Vec::new() };
    if placements.is_empty() {
        if let Some(cache) = cache {
            cache.clear();
        }
        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        uncloak_all_tracked();
        return Ok(empty_result);
    }

    let mut applied = 0u32;
    let mut skipped = 0u32;

    // Collect all (hwnd, adjusted_rect, flags) entries for deferred positioning.
    // Pre-compute border insets and cache checks before the batch to minimize
    // time between BeginDeferWindowPos and EndDeferWindowPos.
    struct DeferEntry {
        hwnd: HWND,
        window_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        visibility: Visibility,
        flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
        column_index: usize,
    }
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());

    // Prepare all window entries — visible and off-screen alike.
    // All windows get full position + size with border inset adjustment.
    // Off-screen windows are later clamped to just outside the visible area
    // to prevent DWM from releasing composition surfaces.
    let offscreen_count = placements.iter().filter(|p| p.visibility != Visibility::Visible).count();

    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area.  If we expand by the usual insets, adjacent windows' visible borders
    // overlap and the layout gaps disappear.  Zero the insets to keep correct spacing.
    let high_contrast = crate::is_high_contrast_enabled();

    for placement in placements {
        if let Some(ref cache) = cache {
            if cache.positions.get(&placement.window_id) == Some(&(placement.rect, placement.visibility)) {
                skipped += 1;
                continue;
            }
        }
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else { continue };
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
        }

        let (inset_l, inset_t, inset_r, inset_b) = if high_contrast {
            (0, 0, 0, 0)
        } else {
            cached_border_insets(hwnd, placement.window_id, cache.as_deref_mut())
        };
        let frame_w = placement.rect.width + inset_l + inset_r;
        let frame_h = placement.rect.height + inset_t + inset_b;

        if placement.visibility == Visibility::Visible {
            let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
            // Only send SWP_FRAMECHANGED (expensive WM_NCCALCSIZE) on first
            // frame or landing pass — not every animation frame.
            let needs_frame_changed = if let Some(ref cache) = cache {
                !cache.positions.contains_key(&placement.window_id)
            } else {
                true
            };
            if needs_frame_changed {
                flags |= SWP_FRAMECHANGED;
            }
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x - inset_l,
                y: placement.rect.y - inset_t,
                w: frame_w,
                h: frame_h,
                visibility: placement.visibility,
                flags,
                column_index: placement.column_index,
            });
        } else {
            // Off-screen: SWP_NOSIZE keeps current size (no resize side-effects).
            // w stores estimated frame width for clamping only — SetWindowPos
            // ignores it due to SWP_NOSIZE.
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: placement.rect.x - inset_l,
                y: placement.rect.y - inset_t,
                w: frame_w,
                h: 0,
                visibility: placement.visibility,
                flags: SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                column_index: placement.column_index,
            });
        }
    }

    // Uncloak windows that are becoming visible BEFORE positioning,
    // so DWM starts compositing them at the correct location on this frame.
    // Also remove from the tracking set — the post-positioning block will
    // re-add if the window ends up off-screen on this frame.
    // Win32 calls happen after releasing the lock to avoid stalling the
    // event hook callback (which reads GLOBAL_CLOAKED via is_placement_cloaked).
    {
        let to_uncloak: Vec<HWND> = {
            let mut cloaked = lock_cloaked();
            if let Some(ref mut set) = *cloaked {
                entries.iter()
                    .filter(|e| e.visibility == Visibility::Visible && set.remove(&e.window_id))
                    .map(|e| e.hwnd)
                    .collect()
            } else {
                Vec::new()
            }
        };
        for hwnd in to_uncloak {
            unsafe { dwm_set_cloak(hwnd, false) };
        }
    }

    // Track windows that failed positioning (excluded from cache).
    let mut failed_window_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Batch all SetWindowPos calls via DeferWindowPos for atomic repositioning.
    if !entries.is_empty() {
        unsafe {
            match BeginDeferWindowPos(entries.len() as i32) {
            Err(_) => {
                // Fallback: apply individually if batching fails
                for entry in &entries {
                    if SetWindowPos(
                        entry.hwnd, None,
                        entry.x, entry.y, entry.w, entry.h,
                        entry.flags,
                    ).is_err() {
                        failed_window_ids.insert(entry.window_id);
                    }
                }
                applied = (entries.len() - failed_window_ids.len()) as u32;
            }
            Ok(initial_hdwp) => {
                let mut hdwp = initial_hdwp;
                let mut batch_ok = true;
                for entry in &entries {
                    match DeferWindowPos(
                        hdwp, entry.hwnd, None,
                        entry.x, entry.y, entry.w, entry.h,
                        entry.flags,
                    ) {
                        Ok(new_hdwp) => hdwp = new_hdwp,
                        Err(_) => {
                            batch_ok = false;
                            break;
                        }
                    }
                }
                if batch_ok {
                    if EndDeferWindowPos(hdwp).is_err() {
                        // EndDeferWindowPos failed — fall back to individual calls
                        for entry in &entries {
                            if SetWindowPos(
                                entry.hwnd, None,
                                entry.x, entry.y, entry.w, entry.h,
                                entry.flags,
                            ).is_err() {
                                failed_window_ids.insert(entry.window_id);
                            }
                        }
                        applied = (entries.len() - failed_window_ids.len()) as u32;
                    } else {
                        applied = entries.len() as u32;
                    }
                } else {
                    // DeferWindowPos failed — HDWP is already freed by Win32.
                    // Fall back to individual SetWindowPos calls.
                    for entry in &entries {
                        if SetWindowPos(
                            entry.hwnd, None,
                            entry.x, entry.y, entry.w, entry.h,
                            entry.flags,
                        ).is_err() {
                            failed_window_ids.insert(entry.window_id);
                        }
                    }
                    applied = (entries.len() - failed_window_ids.len()) as u32;
                }
            }
            }
        }
    }

    // Fix-up pass: some windows enforce minimum sizes and Windows silently
    // resizes them, causing overlaps in stacked columns. Detect violations
    // and re-layout affected columns.
    {
        use std::collections::HashMap;
        // Group visible entries by column (only multi-window columns matter).
        let mut col_indices: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            if entry.column_index != usize::MAX
                && entry.visibility == Visibility::Visible
                && !failed_window_ids.contains(&entry.window_id)
            {
                col_indices.entry(entry.column_index).or_default().push(i);
            }
        }
        for indices in col_indices.values() {
            if indices.len() < 2 {
                continue;
            }
            // Sort by y position (top to bottom).
            let mut sorted: Vec<usize> = indices.clone();
            sorted.sort_by_key(|&i| entries[i].y);

            // Check each window's actual height against requested.
            let mut needs_fixup = false;
            for &idx in &sorted {
                let entry = &entries[idx];
                let actual_h = unsafe {
                    let mut r = RECT::default();
                    if GetWindowRect(entry.hwnd, &mut r).is_ok() {
                        r.bottom - r.top
                    } else {
                        entry.h
                    }
                };
                if actual_h > entry.h + 2 {
                    needs_fixup = true;
                    break;
                }
            }
            if !needs_fixup {
                continue;
            }

            // Compute column bottom boundary from the last entry's original position.
            let last = &entries[*sorted.last().unwrap()];
            let column_bottom = last.y + last.h;

            // Re-layout: walk top-to-bottom, query actual heights, push
            // subsequent windows down. Last window absorbs remaining space.
            let mut current_y = entries[sorted[0]].y;
            for (pos, &idx) in sorted.iter().enumerate() {
                let entry = &entries[idx];
                let actual_h = unsafe {
                    let mut r = RECT::default();
                    if GetWindowRect(entry.hwnd, &mut r).is_ok() {
                        r.bottom - r.top
                    } else {
                        entry.h
                    }
                };
                let gap = if pos > 0 {
                    // Infer gap from original layout spacing.
                    let prev = &entries[sorted[pos - 1]];
                    (entry.y - (prev.y + prev.h)).max(0)
                } else {
                    0
                };
                let new_y = current_y + gap;
                let new_h = if pos == sorted.len() - 1 {
                    // Last window: fill remaining space.
                    (column_bottom - new_y).max(1)
                } else if actual_h > entry.h + 2 {
                    // This window enforces a minimum — use its actual height.
                    actual_h
                } else {
                    entry.h
                };
                if new_y != entry.y || new_h != entry.h {
                    tracing::debug!(
                        "Min-size fixup: {:?} y {} → {}, h {} → {}",
                        entry.hwnd, entry.y, new_y, entry.h, new_h,
                    );
                    unsafe {
                        let _ = SetWindowPos(
                            entry.hwnd, None,
                            entry.x, new_y, entry.w, new_h,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                current_y = new_y + new_h;
            }
        }
    }

    // Detect width violations: windows wider than requested (min-width enforcement).
    // Report them so the layout engine can account for them on subsequent frames.
    let mut width_violations = Vec::new();
    for entry in &entries {
        if entry.column_index == usize::MAX
            || entry.visibility != Visibility::Visible
            || failed_window_ids.contains(&entry.window_id)
        {
            continue;
        }
        let actual_w = unsafe {
            let mut r = RECT::default();
            if GetWindowRect(entry.hwnd, &mut r).is_ok() {
                r.right - r.left
            } else {
                continue;
            }
        };
        if actual_w > entry.w + 2 {
            // Convert back to layout pixels by subtracting border insets.
            let (inset_l, _, inset_r, _) = if high_contrast {
                (0, 0, 0, 0)
            } else {
                invisible_border_insets(entry.hwnd)
            };
            let layout_min = actual_w - inset_l - inset_r;
            tracing::debug!(
                "Width violation: {:?} requested {}px, actual {}px (layout min {}px)",
                entry.hwnd, entry.w, actual_w, layout_min,
            );
            width_violations.push(WidthViolation {
                window_id: entry.window_id,
                min_width: layout_min,
            });
        }
    }

    // Update cache: remove stale entries (windows no longer in placements),
    // update positioned entries, and keep skipped-unchanged entries intact.
    if let Some(cache) = cache {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();
        // Remove windows that are no longer in the layout
        cache.positions.retain(|id, _| current_ids.contains(id));
        cache.insets.retain(|id, _| current_ids.contains(id));
        // Update entries for windows that were actually positioned
        let positioned: std::collections::HashSet<u64> =
            entries.iter()
                .filter(|e| !failed_window_ids.contains(&e.window_id))
                .map(|e| e.window_id)
                .collect();
        for p in placements {
            if positioned.contains(&p.window_id) {
                cache.positions.insert(p.window_id, (p.rect, p.visibility));
            }
        }
    }

    // Cloak off-screen windows AFTER positioning. DWM cloaking keeps the
    // composition surface alive (preventing content shift on return) while
    // hiding the window from view (preventing peeking through outer gaps).
    // Events from cloaking are filtered by is_placement_cloaked() in event_hooks.
    // Win32 calls happen after releasing the lock to minimize contention.
    {
        let (to_cloak, to_uncloak) = {
            let mut cloaked = lock_cloaked();
            let set = cloaked.get_or_insert_with(HashSet::new);

            let cloak: Vec<HWND> = entries.iter()
                .filter(|e| !failed_window_ids.contains(&e.window_id)
                    && e.visibility != Visibility::Visible
                    && set.insert(e.window_id))
                .map(|e| e.hwnd)
                .collect();

            // Prune windows no longer in the layout (e.g., workspace switch).
            let current_ids: HashSet<u64> = placements.iter().map(|p| p.window_id).collect();
            let uncloak: Vec<HWND> = set.iter()
                .filter(|id| !current_ids.contains(id))
                .filter_map(|&wid| window_id_to_hwnd(wid).ok())
                .collect();
            set.retain(|id| current_ids.contains(id));

            (cloak, uncloak)
        };
        for hwnd in to_cloak {
            unsafe { dwm_set_cloak(hwnd, true) };
        }
        for hwnd in to_uncloak {
            unsafe { dwm_set_cloak(hwnd, false) };
        }
    }

    tracing::debug!(
        "Applied {} placements ({} skipped unchanged), {} off-screen total",
        applied,
        skipped,
        offscreen_count,
    );

    Ok(ApplyPlacementsResult { width_violations })
}

type InsetMap = HashMap<WindowId, (i32, i32, i32, i32)>;

/// Global inset cache for the `apply_layout` path (which passes `cache: None`).
/// Ensures windows returning from off-screen get correct insets even without
/// a per-worker PlacementCache.
static GLOBAL_INSET_CACHE: Mutex<Option<InsetMap>> = Mutex::new(None);

/// Clear the global inset cache. Must be called when system theme or DWM
/// metrics change (e.g., high contrast toggle, display change) so that stale
/// invisible-border values don't cause incorrect window sizing.
pub fn clear_inset_cache() {
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        *global = None;
    }
}

/// Look up border insets for a window, using a sticky cache to protect against
/// stale DWM data for windows that were parked off-screen.
///
/// Border insets are determined by window style and DPI, not position, so they
/// should be stable for a window's lifetime. Once cached, we never re-query DWM.
fn cached_border_insets(
    hwnd: HWND,
    window_id: WindowId,
    local_cache: Option<&mut PlacementCache>,
) -> (i32, i32, i32, i32) {
    // Check local (per-worker) cache first
    if let Some(cached) = local_cache
        .as_ref()
        .and_then(|c| c.insets.get(&window_id).copied())
    {
        return cached;
    }
    // Check global cache (shared across apply_layout threads)
    if let Ok(global) = GLOBAL_INSET_CACHE.lock() {
        if let Some(cached) = global.as_ref().and_then(|m| m.get(&window_id).copied()) {
            // Promote to local cache for fast subsequent lookups
            if let Some(cache) = local_cache {
                cache.insets.insert(window_id, cached);
            }
            return cached;
        }
    }
    // No cache — query DWM and cache if non-zero
    let fresh = invisible_border_insets(hwnd);
    if fresh != (0, 0, 0, 0) {
        if let Some(cache) = local_cache {
            cache.insets.insert(window_id, fresh);
        }
        if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
            global.get_or_insert_with(HashMap::new).insert(window_id, fresh);
        }
    }
    fresh
}

/// Compute invisible border insets for a window.
///
/// Windows 10/11 windows have invisible borders (typically ~7px on left, right,
/// bottom and 0px on top). `SetWindowPos` operates on the full frame rect
/// including these borders. To make the *visible* area fill our target rect,
/// we expand the frame rect by the invisible border amount.
///
/// Returns (left, top, right, bottom) insets to subtract/add to the target rect.
pub(crate) fn invisible_border_insets(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut frame_rect = RECT::default();
        if GetWindowRect(hwnd, &mut frame_rect).is_err() {
            return (0, 0, 0, 0);
        }

        let mut extended_rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut extended_rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            return (0, 0, 0, 0);
        }

        // Insets = how much the frame rect extends beyond the visible area
        let left = extended_rect.left - frame_rect.left;
        let top = extended_rect.top - frame_rect.top;
        let right = frame_rect.right - extended_rect.right;
        let bottom = frame_rect.bottom - extended_rect.bottom;

        (left.max(0), top.max(0), right.max(0), bottom.max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_placements_empty() {
        // Verify empty placements succeed without error
        let config = PlatformConfig::default();
        let result = apply_placements(&[], &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apply_placements_skips_invalid_windows() {
        let config = PlatformConfig::default();
        let placements = vec![WindowPlacement {
            window_id: 0,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::OffScreenLeft,
            column_index: 0,
        }];

        // Invalid windows (hwnd 0) are silently skipped in the deferred batch
        let result = apply_placements(&placements, &config, None);
        assert!(result.is_ok());
    }
}
