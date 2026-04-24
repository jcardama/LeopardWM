//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::types::{PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClassNameW, GetWindowRect, IsIconic,
    IsWindow, IsZoomed, SetWindowPos, ShowWindow, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
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

/// A window whose actual visible width exceeds the requested placement width,
/// indicating it enforces a minimum size. The `min_width` is in layout
/// pixels (matches what the layout engine would allocate).
#[derive(Debug, Clone)]
pub struct WidthViolation {
    pub window_id: WindowId,
    /// Minimum width in layout coordinates.
    pub min_width: i32,
}

/// A window whose actual visible height exceeds the requested placement height.
/// Symmetric to `WidthViolation`. The `min_height` is in layout pixels.
#[derive(Debug, Clone)]
pub struct HeightViolation {
    pub window_id: WindowId,
    /// Minimum height in layout coordinates.
    pub min_height: i32,
}

/// Result of apply_placements, including any detected size violations.
pub struct ApplyPlacementsResult {
    /// Width violations detected after positioning (windows wider than requested).
    pub width_violations: Vec<WidthViolation>,
    /// Height violations detected after positioning (windows taller than requested).
    pub height_violations: Vec<HeightViolation>,
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
    let empty_result = ApplyPlacementsResult {
        width_violations: Vec::new(),
        height_violations: Vec::new(),
    };
    if placements.is_empty() {
        if let Some(cache) = cache {
            cache.clear();
        }
        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        uncloak_all_tracked();
        return Ok(empty_result);
    }

    // Animation frames (cache present) use async positioning so hung windows
    // don't stall the vsync-driven animation loop. Landing passes (no cache)
    // stay synchronous for precise final placement.
    let async_flag = if cache.is_some() {
        SWP_ASYNCWINDOWPOS
    } else {
        SET_WINDOW_POS_FLAGS(0)
    };

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
        /// Layout-coordinate width requested by the layout engine (pre-insets).
        /// Used for size-violation detection, which compares DWM visible bounds
        /// directly and is immune to stale cached border insets.
        layout_w: i32,
        /// Layout-coordinate height requested by the layout engine (pre-insets).
        layout_h: i32,
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
            // Restore maximized tiled windows before positioning — WS_MAXIMIZE
            // causes some windows to ignore SetWindowPos size changes.
            // Only for tiled windows (column_index != MAX); floating windows
            // may be intentionally maximized by the user.
            if placement.visibility == Visibility::Visible
                && placement.column_index != usize::MAX
                && IsZoomed(hwnd).as_bool()
            {
                let _ = ShowWindow(hwnd, SW_RESTORE);
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
            let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | async_flag;
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
                layout_w: placement.rect.width,
                layout_h: placement.rect.height,
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
                layout_w: placement.rect.width,
                layout_h: placement.rect.height,
                visibility: placement.visibility,
                flags: SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | async_flag,
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

    // Detect size violations by comparing the DWM extended frame bounds
    // (the window's actual visible content area) against the layout rect the
    // layout engine asked for. This deliberately bypasses the cached-inset
    // math used for SetWindowPos: if the cached insets go stale (e.g. apps
    // like Slack/Spotify toggle custom client frames at runtime) the frame-
    // vs-frame comparison silently cancels out and violations are missed.
    //
    // Visible-bounds-vs-layout-rect is the honest comparison: the layout
    // engine allocates `placement.rect.width × placement.rect.height` of
    // visible real estate, and we check whether the window actually fits.
    //
    // Skipped during async animation frames — DWM returns stale (pre-resize)
    // bounds which would create false constraints that prevent columns from
    // shrinking. The synchronous landing pass detects real violations
    // authoritatively.
    let mut width_violations = Vec::new();
    let mut height_violations = Vec::new();
    if async_flag == SET_WINDOW_POS_FLAGS(0) {
        // Wait for the compositor to composite a frame before reading DWM
        // bounds. Sync SetWindowPos only guarantees the target thread received
        // WM_WINDOWPOSCHANGED — it does NOT wait for the target to process and
        // re-render. Under CPU pressure (e.g. a background `cargo test` build),
        // the target thread can lag behind: we'd read PRE-shrink bounds,
        // interpret the oversized rect as a min-size violation, and record a
        // bogus constraint that breaks subsequent layouts (e.g. a 50/50 column
        // turning into 75/50 because one window's min_height got inflated).
        //
        // DwmFlush blocks for ~one vsync (~16ms) until the compositor has
        // presented a frame incorporating our just-applied positions. Cheap
        // on the landing pass (runs once per settle, not per frame).
        unsafe {
            let _ = DwmFlush();
        }
        for entry in &entries {
            if entry.column_index == usize::MAX
                || entry.visibility != Visibility::Visible
                || failed_window_ids.contains(&entry.window_id)
            {
                continue;
            }
            // Query DWM for the current visible bounds. This ignores any
            // invisible-border metrics and reports what the user actually sees.
            let (visible_w, visible_h) = unsafe {
                let mut ext = RECT::default();
                if DwmGetWindowAttribute(
                    entry.hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    &mut ext as *mut RECT as *mut _,
                    std::mem::size_of::<RECT>() as u32,
                )
                .is_err()
                {
                    continue;
                }
                (ext.right - ext.left, ext.bottom - ext.top)
            };

            // Sanity cap — a genuine min-size violation has the window just
            // barely larger than requested (tens of pixels at most). If DWM
            // reports bounds >1.5x the requested size, the target thread is
            // almost certainly lagging behind our just-applied resize under
            // CPU pressure (despite the DwmFlush above this can still happen
            // for extremely unresponsive apps). Recording these as real
            // constraints would permanently inflate future layouts. Skip them
            // and let the next landing pass re-measure authoritatively.
            const STALE_BOUNDS_RATIO: i32 = 3; // visible > requested * 3/2 → skip
            let looks_stale_w = entry.layout_w > 0
                && visible_w * 2 > entry.layout_w * STALE_BOUNDS_RATIO;
            let looks_stale_h = entry.layout_h > 0
                && visible_h * 2 > entry.layout_h * STALE_BOUNDS_RATIO;

            let mut mismatched = false;
            if visible_w > entry.layout_w + 2 && !looks_stale_w {
                tracing::debug!(
                    "Width violation: {:?} requested {}px, visible {}px",
                    entry.hwnd, entry.layout_w, visible_w,
                );
                width_violations.push(WidthViolation {
                    window_id: entry.window_id,
                    min_width: visible_w,
                });
                mismatched = true;
            } else if visible_w > entry.layout_w + 2 && looks_stale_w {
                tracing::warn!(
                    "Skipping suspect width violation (stale DWM bounds?): {:?} \
                     requested {}px, visible {}px ({}x reported)",
                    entry.hwnd,
                    entry.layout_w,
                    visible_w,
                    visible_w as f32 / entry.layout_w.max(1) as f32,
                );
            }
            if visible_h > entry.layout_h + 2 && !looks_stale_h {
                tracing::debug!(
                    "Height violation: {:?} requested {}px, visible {}px",
                    entry.hwnd, entry.layout_h, visible_h,
                );
                height_violations.push(HeightViolation {
                    window_id: entry.window_id,
                    min_height: visible_h,
                });
                mismatched = true;
            } else if visible_h > entry.layout_h + 2 && looks_stale_h {
                tracing::warn!(
                    "Skipping suspect height violation (stale DWM bounds?): {:?} \
                     requested {}px, visible {}px ({}x reported)",
                    entry.hwnd,
                    entry.layout_h,
                    visible_h,
                    visible_h as f32 / entry.layout_h.max(1) as f32,
                );
            }

            // On any mismatch, invalidate the cached border insets for this
            // window. Stale insets are the most likely reason a prior frame
            // sized the frame incorrectly, and the next SetWindowPos should
            // re-query DWM for fresh values.
            if mismatched {
                if let Some(ref mut cache) = cache {
                    cache.insets.remove(&entry.window_id);
                }
                if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
                    if let Some(ref mut m) = *global {
                        m.remove(&entry.window_id);
                    }
                }
            }
        }
    } // end: skip size violation detection during async frames

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

    // DirectComposition swap-chain repair.
    //
    // On the synchronous landing pass, nudge windows whose compositor rebuilds
    // its swap chain only on observed size deltas. During rapid scroll the
    // intermediate async frames coalesce on the app's UI thread, leaving the
    // internal render target stuck at an interim size; the landing SetWindowPos
    // arrives with the same rect as the last async frame, so the compositor
    // sees "no size change" and never rebuilds. A brief (w-1 -> w) resize pair
    // forces a real delta through. Scoped to known-affected classes to avoid a
    // universal flicker tax.
    if async_flag == SET_WINDOW_POS_FLAGS(0) {
        let nudge_targets: Vec<NudgeTarget> = entries
            .iter()
            .filter(|e| {
                e.visibility == Visibility::Visible
                    && e.w > 1
                    && !failed_window_ids.contains(&e.window_id)
            })
            .map(|e| NudgeTarget {
                hwnd: e.hwnd,
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
            })
            .collect();
        nudge_sticky_compositor_windows(&nudge_targets);
    }

    tracing::debug!(
        "Applied {} placements ({} skipped unchanged), {} off-screen total",
        applied,
        skipped,
        offscreen_count,
    );

    Ok(ApplyPlacementsResult {
        width_violations,
        height_violations,
    })
}

/// Window classes whose compositor (DirectComposition / swap-chain based)
/// fails to rebuild after rapid async SetWindowPos during animation. A real
/// size delta must reach the window for the render target to re-sync.
const STICKY_COMPOSITOR_CLASSES: &[&str] = &[
    "Chrome_WidgetWin_1",           // Electron / Chromium (Slack, Beeper, Spotify, TradingView)
    "MozillaWindowClass",           // Firefox / Zen
    "CASCADIA_HOSTING_WINDOW_CLASS", // Windows Terminal
];

/// Read the class name of a window. Returns empty string on failure.
fn window_class_name(hwnd: HWND) -> String {
    let mut buf: [u16; 256] = [0; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Position data passed to the nudge helper.
struct NudgeTarget {
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Send a (w-1 -> w) synchronous SetWindowPos pair to each entry whose window
/// class matches a known sticky-compositor class. The 1px shrink forces a real
/// size delta through the message pump; the immediate restore returns the rect
/// to the layout-requested size. The compositor sees two size-changes and
/// rebuilds the swap chain, resolving the stuck-interim-size bug.
fn nudge_sticky_compositor_windows(targets: &[NudgeTarget]) {
    for t in targets {
        unsafe {
            if !IsWindow(Some(t.hwnd)).as_bool() {
                continue;
            }
        }
        let class = window_class_name(t.hwnd);
        if !STICKY_COMPOSITOR_CLASSES.iter().any(|c| *c == class) {
            continue;
        }
        let flags = SWP_NOZORDER | SWP_NOACTIVATE;
        unsafe {
            if SetWindowPos(t.hwnd, None, t.x, t.y, t.w - 1, t.h, flags).is_err() {
                continue;
            }
            // Re-validate the HWND between the pair: the first SetWindowPos
            // pumps messages on the target thread and can cause the window to
            // be destroyed; the handle could be recycled for an unrelated
            // window before the restore call lands. Re-checking both the
            // handle validity and the class name catches recycling. If either
            // fails the target is left at w-1 rather than risk resizing the
            // wrong window — next apply pass will correct it.
            if !IsWindow(Some(t.hwnd)).as_bool() {
                continue;
            }
            if window_class_name(t.hwnd) != class {
                continue;
            }
            if let Err(e) = SetWindowPos(t.hwnd, None, t.x, t.y, t.w, t.h, flags) {
                // Restore failed — window is stranded at w-1 (1px narrower)
                // until the next apply_layout re-places it. Log so the state
                // is diagnosable; the next apply will correct geometry.
                tracing::warn!(
                    "Nudge restore SetWindowPos failed for hwnd={:?} class={} — window left at w-1 until next apply: {:?}",
                    t.hwnd, class, e
                );
                continue;
            }
        }
        tracing::debug!(
            "Nudged sticky-compositor window (class={}, hwnd={:?})",
            class,
            t.hwnd
        );
    }
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
