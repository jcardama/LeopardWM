//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::types::{PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClassNameW, GetWindowRect, IsIconic,
    IsWindow, IsZoomed, SetWindowPos, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

/// Undocumented but well-known DWM attribute for cloaking windows.
/// Cloaked windows remain composed by DWM (surface stays alive) but are
/// invisible to the user. Used by the Windows shell for virtual desktops.
const DWMWA_CLOAK: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(13i32);

/// Disable DWM-managed visual transitions (minimize/maximize fade,
/// position interpolation between SetWindowPos calls, etc.) on a
/// specific window. Tiling WMs want instant snap behavior, not DWM
/// smoothing — without this, dragging a window into a tabbed column
/// makes the dropped window visibly "slide" from the drop point to
/// its layout slot.
const DWMWA_TRANSITIONS_FORCEDISABLED: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(3i32);

/// Set or clear the DWM cloak on a window. Bypasses both `GLOBAL_CLOAKED`
/// and `GHOST_CLOAKED` — only callers that have already evaluated the
/// OR-cloak invariant (or recovery paths that want to force-uncloak
/// regardless) should call this directly.
unsafe fn dwm_set_cloak(hwnd: HWND, cloaked: bool) {
    // NOTE: DWMWA_CLOAK only succeeds on windows owned by the calling
    // process; cloaking another process's window returns E_ACCESSDENIED
    // (0x80070005). LeopardWM manages external windows, so this is a no-op
    // for them and hiding relies on physically moving windows off-screen
    // (see the off-screen sentinel positioning). Kept as belt-and-suspenders
    // for the rare same-process window.
    let value = BOOL::from(cloaked);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CLOAK,
        &value as *const _ as _,
        std::mem::size_of::<BOOL>() as u32,
    );
}

/// OR-cloak helper. Applies the logical OR of `GLOBAL_CLOAKED` and
/// `GHOST_CLOAKED` membership for `wid` to the underlying DWM cloak
/// state. Callers mutate one of the two sets, then call this to commit
/// the resulting effective state.
///
/// Validates that the HWND is still live (`IsWindow`) before calling
/// `dwm_set_cloak`. `WindowId → HWND` is a raw cast (`lib.rs:89-93`),
/// so without this guard we could cloak/uncloak a recycled HWND.
pub fn apply_cloak_state(wid: WindowId) {
    let should_cloak = ghost_cloaked_contains(wid) || global_cloaked_contains(wid);
    let Ok(hwnd) = window_id_to_hwnd(wid) else {
        return;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return;
    }
    unsafe { dwm_set_cloak(hwnd, should_cloak) };
}

fn global_cloaked_contains(wid: WindowId) -> bool {
    let guard = lock_cloaked();
    guard.as_ref().is_some_and(|set| set.contains(&wid))
}

// ---------------------------------------------------------------------
// GHOST_CLOAKED — distinct cloak set populated only by the ghost-animation
// path. Logical-OR'd with GLOBAL_CLOAKED to determine the effective cloak
// state (see `apply_cloak_state`).
// ---------------------------------------------------------------------

static GHOST_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_ghost_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    GHOST_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn ghost_cloaked_contains(wid: WindowId) -> bool {
    let guard = lock_ghost_cloaked();
    guard.as_ref().is_some_and(|set| set.contains(&wid))
}

/// Mark a window as cloaked by the ghost-animation system. Caller must
/// follow with `apply_cloak_state(wid)` to commit the DWM state.
pub fn mark_ghost_cloaked(wid: WindowId) {
    let mut guard = lock_ghost_cloaked();
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert(wid);
}

/// Remove a window from the ghost-cloak set. Caller must follow with
/// `apply_cloak_state(wid)` to commit the DWM state (which will uncloak
/// the window unless it's still in `GLOBAL_CLOAKED`).
pub fn unmark_ghost_cloaked(wid: WindowId) {
    let mut guard = lock_ghost_cloaked();
    if let Some(ref mut set) = *guard {
        set.remove(&wid);
    }
}

/// Drain the entire `GHOST_CLOAKED` set, returning the wids that were
/// being held. Recovery paths (panic hook, abort_active_ghost_transition)
/// use this to clear all ghost cloaks at once. Caller is responsible for
/// calling `apply_cloak_state(wid)` (or `dwm_set_cloak` directly for
/// force-uncloak) on each returned wid.
pub fn drain_ghost_cloaked() -> Vec<WindowId> {
    let mut guard = lock_ghost_cloaked();
    match guard.as_mut() {
        Some(set) => set.drain().collect(),
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------
// DIRECT_CLOAKED — windows cloaked outside the placement system (e.g. a
// stashed scratchpad window removed from all workspaces). NOT consulted by
// `apply_cloak_state` or `uncloak_all_tracked`, so normal placement never
// touches them — but `dwm_uncloak_all` drains it, so shutdown / panic /
// emergency-uncloak recovery always restores them. Without this set such a
// window would be cloaked with no recovery path = permanently invisible.
// ---------------------------------------------------------------------

static DIRECT_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

fn lock_direct_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    DIRECT_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Disable (or re-enable) DWM-managed visual transitions on a window.
/// Pass `true` to make subsequent `SetWindowPos` calls land instantly
/// without DWM's automatic position-interpolation smoothing.
pub fn set_dwm_transitions_disabled(window_id: WindowId, disabled: bool) {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return;
    };
    unsafe {
        let value = BOOL::from(disabled);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &value as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Lock GLOBAL_CLOAKED, recovering from poison (a prior panic while holding
/// the lock). All access to the cloaked set goes through this helper so that
/// shutdown/panic cleanup paths never silently give up.
fn lock_cloaked() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    GLOBAL_CLOAKED
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Force-cloak a single window directly, without touching either tracking
/// set. For windows held OUTSIDE normal layout management (e.g. a stashed
/// scratchpad window that has been removed from its workspace) — nothing
/// in the placement path will reposition or uncloak it, so a direct cloak
/// is safe and stays put until the owner uncloaks it.
pub fn dwm_cloak_window(window_id: WindowId) {
    {
        let mut guard = lock_direct_cloaked();
        guard.get_or_insert_with(HashSet::new).insert(window_id);
    }
    if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe { dwm_set_cloak(hwnd, true) };
    }
}

/// Force-uncloak a window by its WindowId regardless of either tracking
/// set's membership. Removes from both `GLOBAL_CLOAKED` and
/// `GHOST_CLOAKED`. Used by shutdown / panic cleanup.
///
/// Bypasses `apply_cloak_state`'s OR-check: the intent here is "force
/// visible" regardless of why the window was originally cloaked.
pub fn dwm_uncloak_window(window_id: WindowId) {
    {
        let mut guard = lock_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    {
        let mut guard = lock_ghost_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    {
        let mut guard = lock_direct_cloaked();
        if let Some(ref mut set) = *guard {
            set.remove(&window_id);
        }
    }
    if let Ok(hwnd) = window_id_to_hwnd(window_id) {
        unsafe { dwm_set_cloak(hwnd, false) };
    }
}

/// Force-uncloak every tracked window from both sets. Called during
/// shutdown and panic recovery. Bypasses `apply_cloak_state`.
pub fn dwm_uncloak_all() {
    let global_ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    let ghost_ids: Vec<WindowId> = {
        let mut guard = lock_ghost_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    let direct_ids: Vec<WindowId> = {
        let mut guard = lock_direct_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => Vec::new(),
        }
    };
    // Use a set union so we don't issue redundant DWM calls for windows
    // present in more than one set. dwm_set_cloak is idempotent.
    let mut seen: HashSet<WindowId> =
        HashSet::with_capacity(global_ids.len() + ghost_ids.len() + direct_ids.len());
    for wid in global_ids
        .into_iter()
        .chain(ghost_ids)
        .chain(direct_ids)
    {
        if seen.insert(wid) {
            if let Ok(hwnd) = window_id_to_hwnd(wid) {
                unsafe { dwm_set_cloak(hwnd, false) };
            }
        }
    }
}

fn mark_placement_parked(window_id: WindowId) {
    let mut cloaked = lock_cloaked();
    cloaked.get_or_insert_with(HashSet::new).insert(window_id);
}

/// Park a window at the placement sentinel and record that placement owns its
/// return-to-layout recovery. The logical park is rolled back if the physical
/// move fails, so visible maximized recovery never infers ownership from raw
/// coordinates.
pub fn park_window_for_placement(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    mark_placement_parked(window_id);
    apply_cloak_state(window_id);
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            None,
            crate::MOVE_OFFSCREEN_SENTINEL_COORD,
            crate::MOVE_OFFSCREEN_SENTINEL_COORD,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    if let Err(error) = moved {
        {
            let mut cloaked = lock_cloaked();
            if let Some(set) = cloaked.as_mut() {
                set.remove(&window_id);
            }
        }
        apply_cloak_state(window_id);
        return Err(Win32Error::SetPositionFailed(format!(
            "Failed to park window {} for placement: {}",
            window_id, error
        )));
    }
    Ok(())
}

/// Whether placement, rather than ghost animation, owns an off-screen park.
pub fn is_placement_parked(window_id: WindowId) -> bool {
    global_cloaked_contains(window_id)
}

/// Check if a window is currently cloaked by the placement system OR the
/// ghost-animation system. Used by the event hook to suppress spurious
/// SHOW/LOCATIONCHANGE events fired by DWM when we cloak/uncloak windows
/// during placement or ghost transitions.
///
/// Returns the logical OR of `GLOBAL_CLOAKED` (off-screen parking) and
/// `GHOST_CLOAKED` (ghost-animation in flight) membership.
pub fn is_placement_cloaked(window_id: WindowId) -> bool {
    global_cloaked_contains(window_id) || ghost_cloaked_contains(window_id)
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
    /// Visible tiled windows omitted because they were maximized at execution time.
    pub maximized_skipped_window_ids: Vec<WindowId>,
}

// Collect all (hwnd, adjusted_rect, flags) entries for deferred positioning.
// Pre-compute border insets and cache checks before the batch to minimize
// time between BeginDeferWindowPos and EndDeferWindowPos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsetSource {
    LocalCache,
    GlobalCache,
    Fresh,
}

/// Border insets resolved for one placement, tagged with their provenance and
/// with the global-inset-cache generation they were resolved under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolvedInsets {
    insets: (i32, i32, i32, i32),
    source: InsetSource,
    generation: u64,
}

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
    /// Frame insets used to turn the layout request into this SetWindowPos request.
    insets: (i32, i32, i32, i32),
    inset_source: InsetSource,
    /// Global-inset-cache generation `insets` was resolved under; publication
    /// is skipped when it no longer matches (see `INSET_CACHE_GENERATION`).
    inset_generation: u64,
    visibility: Visibility,
    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    column_index: usize,
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
    config: &PlatformConfig,
    mut cache: Option<&mut PlacementCache>,
    nudge_sticky_compositors: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    apply_placements_inner(
        placements,
        config,
        &mut cache,
        nudge_sticky_compositors,
        true,
    )
}

fn apply_placements_inner(
    placements: &[WindowPlacement],
    _config: &PlatformConfig,
    cache: &mut Option<&mut PlacementCache>,
    nudge_sticky_compositors: bool,
    allow_inset_retry: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    let empty_result = ApplyPlacementsResult {
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
    };
    if placements.is_empty() {
        if let Some(cache) = cache.as_deref_mut() {
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

    // Prepare all window entries — visible and off-screen alike.
    // All windows get full position + size with border inset adjustment.
    // Off-screen windows are kept at their layout-flow position; DWM cloaking
    // makes them invisible.
    let offscreen_count = placements
        .iter()
        .filter(|p| p.visibility != Visibility::Visible)
        .count();

    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area. If we expand by the usual insets, adjacent windows' visible borders
    // overlap and the layout gaps disappear. Zero the insets to keep correct spacing.
    let high_contrast = crate::is_high_contrast_enabled();
    let force_positioning = !allow_inset_retry;
    let (entries, skipped, maximized_skipped_window_ids) = build_defer_entries(
        placements,
        cache,
        async_flag,
        high_contrast,
        force_positioning,
    );
    // Uncloak before positioning so DWM composites returning windows at their
    // new rect before the landing measurement. The retry can repeat this safely.
    uncloak_becoming_visible(&entries);
    let (applied, failed_window_ids) = position_entries(&entries);

    // On the synchronous landing pass, compare the DWM visible measurement to
    // both the layout request and the expanded SetWindowPos frame request. A
    // visible rect between them is a stale-inset artifact, not a window minimum:
    // Slack/Spotify can change their client frame at runtime, and Chromium can
    // briefly become frameless after app fullscreen. Retry the complete batch
    // once after evicting the affected tuple; no first-pass cache, compositor
    // nudge, or suspect-state finalization is allowed to escape.
    let detection = if async_flag == SET_WINDOW_POS_FLAGS(0) {
        detect_size_violations(&entries, &failed_window_ids, allow_inset_retry)
    } else {
        SizeViolationDetection::default()
    };
    if should_retry_insets(allow_inset_retry, &detection.inset_artifact_windows) {
        tracing::debug!(
            "Retrying placement batch after stale inset measurement for {} window(s)",
            detection.inset_artifact_windows.len(),
        );
        evict_cached_border_insets(&detection.inset_artifact_windows, cache);
        return apply_placements_inner(placements, _config, cache, nudge_sticky_compositors, false);
    }

    finalize_cached_border_insets(&entries, &detection.inset_artifact_windows, cache);
    evict_cached_border_insets(&detection.violating_windows, cache);

    // Update cache: remove stale entries (windows no longer in placements),
    // update positioned entries, and keep skipped-unchanged entries intact.
    if let Some(cache) = cache.as_deref_mut() {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();
        // Remove windows that are no longer in the layout
        cache.positions.retain(|id, _| current_ids.contains(id));
        cache.insets.retain(|id, _| current_ids.contains(id));
        // Update entries for windows that were actually positioned
        let positioned: std::collections::HashSet<u64> = entries
            .iter()
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
    //
    // Routed through `apply_cloak_state` so a window that's also in
    // `GHOST_CLOAKED` stays cloaked even if we remove it from
    // `GLOBAL_CLOAKED` during pruning.
    sync_cloak_state(&entries, placements, &failed_window_ids);

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
    if async_flag == SET_WINDOW_POS_FLAGS(0) && nudge_sticky_compositors {
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
        width_violations: detection.width_violations,
        height_violations: detection.height_violations,
        maximized_skipped_window_ids,
    })
}

fn recover_placement_parked<F>(
    window_id: WindowId,
    async_flag: SET_WINDOW_POS_FLAGS,
    position: F,
) -> bool
where
    F: FnOnce(SET_WINDOW_POS_FLAGS) -> bool,
{
    if !is_placement_parked(window_id) {
        return false;
    }
    let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | async_flag;
    if !position(flags) {
        return false;
    }
    let released = {
        let mut cloaked = lock_cloaked();
        cloaked
            .as_mut()
            .is_some_and(|set| set.remove(&window_id))
    };
    if released {
        apply_cloak_state(window_id);
    }
    released
}

fn skip_visible_tiled_maximized(
    placement: &WindowPlacement,
    is_zoomed: bool,
    cache: Option<&mut PlacementCache>,
    async_flag: SET_WINDOW_POS_FLAGS,
) -> bool {
    let skip = placement.visibility == Visibility::Visible
        && placement.column_index != usize::MAX
        && is_zoomed;
    if skip {
        if let Some(cache) = cache {
            cache.positions.remove(&placement.window_id);
        }
        // Skipped entries never reach `uncloak_becoming_visible` or
        // `sync_cloak_state`, so a window held in `GLOBAL_CLOAKED` from an
        // earlier off-screen placement would stay cloaked while visible in
        // the layout. Return a placement-parked maximized HWND before
        // releasing that cloak. SWP_NOSIZE deliberately preserves maximized
        // dimensions and does not restore ordinary visible maximized windows.
        let _ = recover_placement_parked(
            placement.window_id,
            async_flag,
            |flags| {
                let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {
                    return false;
                };
                unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        placement.rect.x,
                        placement.rect.y,
                        0,
                        0,
                        flags,
                    )
                    .is_ok()
                }
            },
        );
    }
    skip
}

/// Build the defer-entry list for all placements, skipping cache-unchanged windows.
fn build_defer_entries(
    placements: &[WindowPlacement],
    cache: &mut Option<&mut PlacementCache>,
    async_flag: SET_WINDOW_POS_FLAGS,
    high_contrast: bool,
    force_positioning: bool,
) -> (Vec<DeferEntry>, u32, Vec<WindowId>) {
    let mut skipped = 0u32;
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut maximized_skipped_window_ids = Vec::new();

    for placement in placements {
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else { continue };
        let is_zoomed = unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
            IsZoomed(hwnd).as_bool()
        };
        if skip_visible_tiled_maximized(placement, is_zoomed, cache.as_deref_mut(), async_flag) {
            maximized_skipped_window_ids.push(placement.window_id);
            continue;
        }
        if !force_positioning
            && cache.as_deref().is_some_and(|cache| {
                cache.positions.get(&placement.window_id)
                    == Some(&(placement.rect, placement.visibility))
            })
        {
            skipped += 1;
            continue;
        }

        let resolved = if high_contrast {
            ResolvedInsets {
                insets: (0, 0, 0, 0),
                source: InsetSource::Fresh,
                generation: inset_cache_generation(),
            }
        } else {
            cached_border_insets(hwnd, placement.window_id, cache.as_deref())
        };
        let ResolvedInsets {
            insets,
            source: inset_source,
            generation: inset_generation,
        } = resolved;
        let frame_rect = visible_rect_to_frame_rect(placement.rect, insets, high_contrast);

        if placement.visibility == Visibility::Visible {
            let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | async_flag;
            // Only send SWP_FRAMECHANGED (expensive WM_NCCALCSIZE) on first
            // frame or landing pass — not every animation frame.
            let needs_frame_changed = cache
                .as_ref()
                .is_none_or(|cache| !cache.positions.contains_key(&placement.window_id));
            if needs_frame_changed {
                flags |= SWP_FRAMECHANGED;
            }
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x: frame_rect.x,
                y: frame_rect.y,
                w: frame_rect.width,
                h: frame_rect.height,
                layout_w: placement.rect.width,
                layout_h: placement.rect.height,
                insets,
                inset_source,
                inset_generation,
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
                x: frame_rect.x,
                y: frame_rect.y,
                w: frame_rect.width,
                h: 0,
                layout_w: placement.rect.width,
                layout_h: placement.rect.height,
                insets,
                inset_source,
                inset_generation,
                visibility: placement.visibility,
                flags: SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | async_flag,
                column_index: placement.column_index,
            });
        }
    }

    (entries, skipped, maximized_skipped_window_ids)
}

/// Uncloak entries becoming visible and drop them from the tracking set.
fn uncloak_becoming_visible(entries: &[DeferEntry]) {
    let to_consider: Vec<WindowId> = {
        let mut cloaked = lock_cloaked();
        if let Some(ref mut set) = *cloaked {
            entries
                .iter()
                .filter(|e| e.visibility == Visibility::Visible && set.remove(&e.window_id))
                .map(|e| e.window_id)
                .collect()
        } else {
            Vec::new()
        }
    };
    for wid in to_consider {
        apply_cloak_state(wid);
    }
}

/// Position all entries in one DeferWindowPos batch; returns (applied, failed ids).
fn position_entries(entries: &[DeferEntry]) -> (u32, HashSet<u64>) {
    let mut applied = 0u32;

    // Track windows that failed positioning (excluded from cache).
    let mut failed_window_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Batch all SetWindowPos calls via DeferWindowPos for atomic repositioning.
    if !entries.is_empty() {
        unsafe {
            match BeginDeferWindowPos(entries.len() as i32) {
            Err(_) => {
                // Fallback: apply individually if batching fails
                for entry in entries {
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
                for entry in entries {
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
                        for entry in entries {
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
                    for entry in entries {
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

    (applied, failed_window_ids)
}

/// Per-window suspect state for the size-violation two-pass confirmation:
/// `(width_suspect, height_suspect)` — whether that axis's oversize looked stale
/// (beyond the stale-bounds ratio) on the window's previous landing pass. A
/// genuine min-size reproduces and is promoted on the second sighting; a one-off
/// stale DWM read does not reproduce and is dropped. Module-global because
/// `detect_size_violations` is a free function called once per settle across all
/// workspaces, so a window is only re-measured when its workspace next lands.
/// Entries are evicted on window destroy (`clear_suspected_oversize`) so the map
/// stays bounded and a recycled HWND never inherits a stale suspect bit.
static SUSPECTED_OVERSIZE: Mutex<Option<HashMap<u64, (bool, bool)>>> = Mutex::new(None);

fn lock_suspected_oversize() -> std::sync::MutexGuard<'static, Option<HashMap<u64, (bool, bool)>>> {
    SUSPECTED_OVERSIZE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Drop a window's suspect state. Called when a window is destroyed/unmanaged so
/// the set stays bounded and a recycled HWND starts fresh.
pub fn clear_suspected_oversize(window_id: WindowId) {
    let mut guard = lock_suspected_oversize();
    if let Some(map) = guard.as_mut() {
        map.remove(&window_id);
    }
}

const VISIBLE_SIZE_TOLERANCE: i32 = 2;
const STALE_BOUNDS_RATIO: i32 = 3;
const ABSURD_BOUNDS_RATIO: i32 = 4;

#[derive(Debug, PartialEq, Eq)]
enum AxisSizeClassification {
    Fits,
    InsetArtifact,
    Violation { record: bool, suspect: bool },
}

#[derive(Clone, Copy)]
struct SizeMeasurement {
    hwnd: HWND,
    window_id: WindowId,
    layout_size: i32,
    frame_size: i32,
    visible_size: i32,
}

#[derive(Clone, Copy)]
struct WindowSizeMeasurement {
    hwnd: HWND,
    window_id: WindowId,
    layout_w: i32,
    layout_h: i32,
    frame_w: i32,
    frame_h: i32,
    visible_w: i32,
    visible_h: i32,
}

struct ClassifiedSizeMeasurement {
    measurement: WindowSizeMeasurement,
    width: AxisSizeClassification,
    height: AxisSizeClassification,
}

#[derive(Default)]
struct SizeViolationDetection {
    width_violations: Vec<WidthViolation>,
    height_violations: Vec<HeightViolation>,
    inset_artifact_windows: HashSet<WindowId>,
    violating_windows: HashSet<WindowId>,
}

/// Decide whether one visible measurement fits, proves the cached insets stale,
/// or is a real minimum-size candidate. The retry-eligible first pass treats a
/// measurement inside the frame request as an inset artifact. The authoritative
/// retry-disabled pass instead records that stable residual as a real minimum.
fn classify_size_axis(
    layout_size: i32,
    frame_size: i32,
    visible_size: i32,
    allow_inset_retry: bool,
    was_suspected: bool,
) -> AxisSizeClassification {
    if visible_size <= layout_size + VISIBLE_SIZE_TOLERANCE {
        return AxisSizeClassification::Fits;
    }
    if allow_inset_retry && visible_size <= frame_size + VISIBLE_SIZE_TOLERANCE {
        return AxisSizeClassification::InsetArtifact;
    }

    // Confirmation and absurdity are judged against the layout request, as the
    // pre-existing behavior did. The frame request stays relevant only to the
    // inset-artifact test above: insets are a few pixels, so folding them into
    // these ratios would silently widen both thresholds.
    let looks_stale = layout_size > 0 && visible_size * 2 > layout_size * STALE_BOUNDS_RATIO;
    let absurd = layout_size > 0 && visible_size > layout_size * ABSURD_BOUNDS_RATIO;
    let (record, suspect) = classify_oversize(true, looks_stale, was_suspected, absurd);
    AxisSizeClassification::Violation { record, suspect }
}

fn is_inset_artifact(layout_size: i32, frame_size: i32, visible_size: i32) -> bool {
    visible_size > layout_size + VISIBLE_SIZE_TOLERANCE
        && visible_size <= frame_size + VISIBLE_SIZE_TOLERANCE
}

/// Decide how to treat a confirmed oversize measurement after it has exceeded
/// the frame request. A small excess records immediately; a >1.5x excess needs
/// a second landing pass; a >4x excess is never trusted.
fn classify_oversize(
    over: bool,
    looks_stale: bool,
    was_suspected: bool,
    absurd: bool,
) -> (bool, bool) {
    if !over {
        (false, false)
    } else if !looks_stale {
        (true, false)
    } else if absurd {
        (false, false)
    } else if was_suspected {
        (true, false)
    } else {
        (false, true)
    }
}

fn should_retry_insets(
    allow_inset_retry: bool,
    inset_artifact_windows: &HashSet<WindowId>,
) -> bool {
    allow_inset_retry && !inset_artifact_windows.is_empty()
}

fn classification_outcome(classification: &AxisSizeClassification) -> (bool, bool) {
    match classification {
        AxisSizeClassification::Violation { record, suspect } => (*record, *suspect),
        AxisSizeClassification::Fits | AxisSizeClassification::InsetArtifact => (false, false),
    }
}

fn classify_measurements_and_update_suspects(
    measurements: &[WindowSizeMeasurement],
    allow_inset_retry: bool,
    suspects: &mut HashMap<WindowId, (bool, bool)>,
) -> Vec<ClassifiedSizeMeasurement> {
    measurements
        .iter()
        .map(|measurement| {
            let (was_w, was_h) = suspects
                .get(&measurement.window_id)
                .copied()
                .unwrap_or((false, false));
            let width = classify_size_axis(
                measurement.layout_w,
                measurement.frame_w,
                measurement.visible_w,
                allow_inset_retry,
                was_w,
            );
            let height = classify_size_axis(
                measurement.layout_h,
                measurement.frame_h,
                measurement.visible_h,
                allow_inset_retry,
                was_h,
            );
            let (_, suspect_w) = classification_outcome(&width);
            let (_, suspect_h) = classification_outcome(&height);
            if suspect_w || suspect_h {
                suspects.insert(measurement.window_id, (suspect_w, suspect_h));
            } else {
                suspects.remove(&measurement.window_id);
            }
            ClassifiedSizeMeasurement {
                measurement: *measurement,
                width,
                height,
            }
        })
        .collect()
}

/// Detect min-size violations on the landing pass via DWM visible bounds.
fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    allow_inset_retry: bool,
) -> SizeViolationDetection {
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

    let mut measurements = Vec::new();
    for entry in entries {
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
        measurements.push(WindowSizeMeasurement {
            hwnd: entry.hwnd,
            window_id: entry.window_id,
            layout_w: entry.layout_w,
            layout_h: entry.layout_h,
            frame_w: entry.w,
            frame_h: entry.h,
            visible_w,
            visible_h,
        });
    }

    let artifact_windows: HashSet<WindowId> = if allow_inset_retry {
        measurements
            .iter()
            .filter(|measurement| {
                is_inset_artifact(
                    measurement.layout_w,
                    measurement.frame_w,
                    measurement.visible_w,
                ) || is_inset_artifact(
                    measurement.layout_h,
                    measurement.frame_h,
                    measurement.visible_h,
                )
            })
            .map(|measurement| measurement.window_id)
            .collect()
    } else {
        HashSet::new()
    };
    if should_retry_insets(allow_inset_retry, &artifact_windows) {
        return SizeViolationDetection {
            inset_artifact_windows: artifact_windows,
            ..Default::default()
        };
    }

    let classified = {
        let mut guard = lock_suspected_oversize();
        let suspects = guard.get_or_insert_with(HashMap::new);
        classify_measurements_and_update_suspects(&measurements, allow_inset_retry, suspects)
    };

    let mut detection = SizeViolationDetection::default();
    for classified in classified {
        let width = SizeMeasurement {
            hwnd: classified.measurement.hwnd,
            window_id: classified.measurement.window_id,
            layout_size: classified.measurement.layout_w,
            frame_size: classified.measurement.frame_w,
            visible_size: classified.measurement.visible_w,
        };
        let height = SizeMeasurement {
            hwnd: classified.measurement.hwnd,
            window_id: classified.measurement.window_id,
            layout_size: classified.measurement.layout_h,
            frame_size: classified.measurement.frame_h,
            visible_size: classified.measurement.visible_h,
        };
        report_axis_size_outcome(&mut detection, width, &classified.width, true);
        report_axis_size_outcome(&mut detection, height, &classified.height, false);
    }
    detection
}

fn report_axis_size_outcome(
    detection: &mut SizeViolationDetection,
    measurement: SizeMeasurement,
    classification: &AxisSizeClassification,
    is_width: bool,
) {
    let (record, suspect) = classification_outcome(classification);
    if record {
        let axis = if is_width { "Width" } else { "Height" };
        tracing::debug!(
            "{} violation: {:?} layout request {}px, frame request {}px, visible measurement {}px",
            axis,
            measurement.hwnd,
            measurement.layout_size,
            measurement.frame_size,
            measurement.visible_size,
        );
        detection.violating_windows.insert(measurement.window_id);
        if is_width {
            detection.width_violations.push(WidthViolation {
                window_id: measurement.window_id,
                min_width: measurement.visible_size,
            });
        } else {
            detection.height_violations.push(HeightViolation {
                window_id: measurement.window_id,
                min_height: measurement.visible_size,
            });
        }
    } else if suspect {
        let axis = if is_width { "width" } else { "height" };
        tracing::debug!(
            "Deferring suspect {} until next landing confirms: {:?} layout request {}px, frame request {}px, visible measurement {}px",
            axis,
            measurement.hwnd,
            measurement.layout_size,
            measurement.frame_size,
            measurement.visible_size,
        );
    }
}

/// Cloak newly off-screen entries and prune cloaks for windows no longer in the layout.
fn sync_cloak_state(
    entries: &[DeferEntry],
    placements: &[WindowPlacement],
    failed_window_ids: &HashSet<u64>,
) {
    let (to_cloak, to_uncloak): (Vec<WindowId>, Vec<WindowId>) = {
        let mut cloaked = lock_cloaked();
        let set = cloaked.get_or_insert_with(HashSet::new);

        let cloak: Vec<WindowId> = entries
            .iter()
            .filter(|e| {
                !failed_window_ids.contains(&e.window_id)
                    && e.visibility != Visibility::Visible
                    && set.insert(e.window_id)
            })
            .map(|e| e.window_id)
            .collect();

        // Prune windows no longer in the layout (e.g., workspace switch).
        let current_ids: HashSet<u64> = placements.iter().map(|p| p.window_id).collect();
        let uncloak: Vec<WindowId> = set
            .iter()
            .filter(|id| !current_ids.contains(id))
            .copied()
            .collect();
        set.retain(|id| current_ids.contains(id));

        (cloak, uncloak)
    };
    for wid in to_cloak {
        apply_cloak_state(wid);
    }
    for wid in to_uncloak {
        apply_cloak_state(wid);
    }
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

/// Monotonic generation of the global inset cache, bumped by every
/// `clear_inset_cache` call while the cache lock is held.
///
/// Inset tuples are resolved in `build_defer_entries` but published only after
/// positioning and the landing `DwmFlush`, so an invalidation can land in
/// between. Each resolved tuple carries the generation it was resolved under;
/// publication compares it against the generation observed while holding the
/// cache lock, so a tuple measured before an invalidation can never be promoted
/// afterwards.
static INSET_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn inset_cache_generation() -> u64 {
    INSET_CACHE_GENERATION.load(AtomicOrdering::SeqCst)
}

/// Clear the global inset cache. Must be called when system theme or DWM
/// metrics change (e.g., high contrast toggle, display change) so that stale
/// invisible-border values don't cause incorrect window sizing.
pub fn clear_inset_cache() {
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        *global = None;
        // Bump under the cache lock so a concurrent publication either observes
        // the pre-clear generation before we clear, or the post-clear one.
        INSET_CACHE_GENERATION.fetch_add(1, AtomicOrdering::SeqCst);
    }
}

/// Look up border insets for a window, using a sticky cache to protect against
/// stale DWM data for windows that were parked off-screen.
///
/// The caller finalizes a freshly queried tuple only after the batch's visible
/// measurements are accepted. This lets a stale cached Slack/Spotify or
/// transient Chromium client frame be evicted and re-queried without leaking
/// the first pass into either cache.
fn cached_border_insets(
    hwnd: HWND,
    window_id: WindowId,
    local_cache: Option<&PlacementCache>,
) -> ResolvedInsets {
    // Sampled before any lookup or DWM query so that an invalidation racing
    // either one is guaranteed to produce a mismatch at publication time.
    let generation = inset_cache_generation();
    if let Some(cached) = local_cache.and_then(|cache| cache.insets.get(&window_id).copied()) {
        return ResolvedInsets {
            insets: cached,
            source: InsetSource::LocalCache,
            generation,
        };
    }
    if let Ok(global) = GLOBAL_INSET_CACHE.lock() {
        if let Some(cached) = global.as_ref().and_then(|map| map.get(&window_id).copied()) {
            return ResolvedInsets {
                insets: cached,
                source: InsetSource::GlobalCache,
                generation,
            };
        }
    }
    ResolvedInsets {
        insets: invisible_border_insets(hwnd),
        source: InsetSource::Fresh,
        generation,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InsetFinalization {
    local: bool,
    global: bool,
}

fn inset_finalization(
    source: InsetSource,
    insets: (i32, i32, i32, i32),
    is_artifact: bool,
    generation_matches: bool,
) -> InsetFinalization {
    if insets == (0, 0, 0, 0) || is_artifact || !generation_matches {
        return InsetFinalization {
            local: false,
            global: false,
        };
    }
    match source {
        InsetSource::LocalCache => InsetFinalization {
            local: false,
            global: false,
        },
        InsetSource::GlobalCache => InsetFinalization {
            local: true,
            global: false,
        },
        InsetSource::Fresh => InsetFinalization {
            local: true,
            global: true,
        },
    }
}

fn finalize_cached_border_insets(
    entries: &[DeferEntry],
    inset_artifact_windows: &HashSet<WindowId>,
    cache: &mut Option<&mut PlacementCache>,
) {
    if let Some(cache) = cache.as_deref_mut() {
        let generation = inset_cache_generation();
        for entry in entries {
            if inset_finalization(
                entry.inset_source,
                entry.insets,
                inset_artifact_windows.contains(&entry.window_id),
                entry.inset_generation == generation,
            )
            .local
            {
                cache.insets.insert(entry.window_id, entry.insets);
            }
        }
    }
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        // Read under the cache lock: `clear_inset_cache` bumps the generation
        // while holding it, so a clear cannot slip between this read and the
        // inserts below.
        let generation = inset_cache_generation();
        let global = global.get_or_insert_with(HashMap::new);
        for entry in entries {
            if inset_finalization(
                entry.inset_source,
                entry.insets,
                inset_artifact_windows.contains(&entry.window_id),
                entry.inset_generation == generation,
            )
            .global
            {
                global.insert(entry.window_id, entry.insets);
            }
        }
    }
}

fn evict_cached_border_insets(
    window_ids: &HashSet<WindowId>,
    cache: &mut Option<&mut PlacementCache>,
) {
    if window_ids.is_empty() {
        return;
    }
    if let Some(cache) = cache.as_deref_mut() {
        for window_id in window_ids {
            cache.insets.remove(window_id);
        }
    }
    if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
        if let Some(global) = global.as_mut() {
            for window_id in window_ids {
                global.remove(window_id);
            }
        }
    }
}

pub fn visible_rect_to_frame_rect(
    visible_rect: Rect,
    insets: (i32, i32, i32, i32),
    high_contrast: bool,
) -> Rect {
    let (left, top, right, bottom) = if high_contrast { (0, 0, 0, 0) } else { insets };
    Rect::new(
        visible_rect.x.saturating_sub(left),
        visible_rect.y.saturating_sub(top),
        visible_rect.width.saturating_add(left).saturating_add(right),
        visible_rect.height.saturating_add(top).saturating_add(bottom),
    )
}

#[cfg(test)]
fn frame_rect_to_visible_rect(
    frame_rect: Rect,
    insets: (i32, i32, i32, i32),
    high_contrast: bool,
) -> Rect {
    let (left, top, right, bottom) = if high_contrast { (0, 0, 0, 0) } else { insets };
    Rect::new(
        frame_rect.x.saturating_add(left),
        frame_rect.y.saturating_add(top),
        frame_rect.width.saturating_sub(left).saturating_sub(right),
        frame_rect.height.saturating_sub(top).saturating_sub(bottom),
    )
}

const MAX_INVISIBLE_FRAME_INSET: i64 = 64;

fn frame_insets_from_rects(
    frame_rect: Rect,
    visible_rect: Rect,
) -> Option<(i32, i32, i32, i32)> {
    if frame_rect.width <= 0
        || frame_rect.height <= 0
        || visible_rect.width <= 0
        || visible_rect.height <= 0
    {
        return None;
    }

    let left = i64::from(visible_rect.x) - i64::from(frame_rect.x);
    let top = i64::from(visible_rect.y) - i64::from(frame_rect.y);
    let right = i64::from(frame_rect.x) + i64::from(frame_rect.width)
        - i64::from(visible_rect.x)
        - i64::from(visible_rect.width);
    let bottom = i64::from(frame_rect.y) + i64::from(frame_rect.height)
        - i64::from(visible_rect.y)
        - i64::from(visible_rect.height);
    let insets = [left, top, right, bottom];
    if insets
        .iter()
        .any(|inset| !(0..=MAX_INVISIBLE_FRAME_INSET).contains(inset))
    {
        return None;
    }

    Some((left as i32, top as i32, right as i32, bottom as i32))
}

fn query_window_frame_insets(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let mut frame_rect = RECT::default();
        GetWindowRect(hwnd, &mut frame_rect).ok()?;

        let mut visible_rect = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut visible_rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;

        frame_insets_from_rects(
            Rect::new(
                frame_rect.left,
                frame_rect.top,
                frame_rect.right - frame_rect.left,
                frame_rect.bottom - frame_rect.top,
            ),
            Rect::new(
                visible_rect.left,
                visible_rect.top,
                visible_rect.right - visible_rect.left,
                visible_rect.bottom - visible_rect.top,
            ),
        )
    }
}

pub fn get_window_frame_insets(window_id: WindowId) -> Option<(i32, i32, i32, i32)> {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else { return None };
    query_window_frame_insets(hwnd)
}

/// Public wrapper over `invisible_border_insets` that takes a `WindowId`.
/// Returns `(left, top, right, bottom)` insets, or `(0, 0, 0, 0)` if the
/// window has no DWM bounds available. Used by callers that need to
/// translate between chrome (`GetWindowRect`) coordinates and visible-
/// content (layout) coordinates without reaching into placement internals.
pub fn get_window_invisible_insets(window_id: WindowId) -> (i32, i32, i32, i32) {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else { return (0, 0, 0, 0) };
    invisible_border_insets(hwnd)
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
    fn test_visible_and_frame_rects_round_trip_with_insets() {
        let visible = Rect::new(120, 100, 1000, 700);
        let insets = (7, 1, 7, 8);
        let frame = visible_rect_to_frame_rect(visible, insets, false);
        assert_eq!(frame, Rect::new(113, 99, 1014, 709));
        assert_eq!(frame_rect_to_visible_rect(frame, insets, false), visible);
    }

    #[test]
    fn test_visible_and_frame_rects_ignore_insets_in_high_contrast() {
        let visible = Rect::new(120, 100, 1000, 700);
        let insets = (7, 1, 7, 8);
        assert_eq!(visible_rect_to_frame_rect(visible, insets, true), visible);
        assert_eq!(frame_rect_to_visible_rect(visible, insets, true), visible);
    }

    #[test]
    fn test_frame_insets_accept_aligned_normal_sample() {
        assert_eq!(
            frame_insets_from_rects(
                Rect::new(100, 100, 914, 709),
                Rect::new(107, 101, 900, 700),
            ),
            Some((7, 1, 7, 8))
        );
    }

    #[test]
    fn test_frame_insets_reject_stale_or_misaligned_samples() {
        let frame = Rect::new(100, 100, 914, 709);
        for visible in [
            Rect::new(117, 101, 900, 700),
            Rect::new(93, 101, 900, 700),
        ] {
            assert_eq!(frame_insets_from_rects(frame, visible), None);
        }
    }

    #[test]
    fn test_frame_insets_reject_implausible_deltas() {
        assert_eq!(
            frame_insets_from_rects(
                Rect::new(100, 100, 1100, 900),
                Rect::new(200, 200, 900, 700),
            ),
            None
        );
    }

    #[test]
    fn test_classify_size_axis_distinguishes_insets_from_minimums() {
        let cases = [
            (
                1323,
                1337,
                1337,
                true,
                false,
                AxisSizeClassification::InsetArtifact,
            ),
            (
                1372,
                1379,
                1379,
                true,
                false,
                AxisSizeClassification::InsetArtifact,
            ),
            (400, 400, 400, true, false, AxisSizeClassification::Fits),
            (400, 400, 402, true, false, AxisSizeClassification::Fits),
            (
                400,
                400,
                403,
                true,
                false,
                AxisSizeClassification::Violation {
                    record: true,
                    suspect: false,
                },
            ),
            (
                400,
                400,
                700,
                true,
                false,
                AxisSizeClassification::Violation {
                    record: false,
                    suspect: true,
                },
            ),
            (
                400,
                400,
                700,
                true,
                true,
                AxisSizeClassification::Violation {
                    record: true,
                    suspect: false,
                },
            ),
            (
                400,
                400,
                1601,
                true,
                true,
                AxisSizeClassification::Violation {
                    record: false,
                    suspect: false,
                },
            ),
            (
                400,
                414,
                403,
                false,
                false,
                AxisSizeClassification::Violation {
                    record: true,
                    suspect: false,
                },
            ),
            // Nonzero insets must not widen the >1.5x confirmation threshold:
            // 605 > 1.5 * 400 (layout), but 605 < 1.5 * 414 (frame). Judged
            // against the frame this would record a bogus 605px minimum on
            // first sighting instead of deferring it to a second landing.
            (
                400,
                414,
                605,
                false,
                false,
                AxisSizeClassification::Violation {
                    record: false,
                    suspect: true,
                },
            ),
            (
                400,
                414,
                605,
                true,
                false,
                AxisSizeClassification::Violation {
                    record: false,
                    suspect: true,
                },
            ),
            // Nor may they widen the >4x absurdity guard: 1700 > 4 * 400
            // (layout) but 1700 < 4 * 500 (frame), so a frame-judged absurdity
            // test would promote this implausible read to a real minimum on the
            // confirming pass.
            (
                400,
                500,
                1700,
                false,
                true,
                AxisSizeClassification::Violation {
                    record: false,
                    suspect: false,
                },
            ),
        ];

        for (layout, frame, visible, allow_retry, was_suspected, expected) in cases {
            assert_eq!(
                classify_size_axis(layout, frame, visible, allow_retry, was_suspected),
                expected,
                "layout={layout}, frame={frame}, visible={visible}, allow_retry={allow_retry}"
            );
        }
    }

    #[test]
    fn test_inset_retry_discards_whole_first_pass_and_records_stable_mixed_minimums() {
        let measurement = WindowSizeMeasurement {
            hwnd: HWND::default(),
            window_id: 7,
            layout_w: 400,
            layout_h: 400,
            frame_w: 414,
            frame_h: 400,
            visible_w: 403,
            visible_h: 500,
        };
        assert!(is_inset_artifact(
            measurement.layout_w,
            measurement.frame_w,
            measurement.visible_w,
        ));
        let artifact_windows = HashSet::from([measurement.window_id]);
        assert!(should_retry_insets(true, &artifact_windows));
        assert!(!should_retry_insets(false, &artifact_windows));
        assert_eq!(
            classify_size_axis(
                measurement.layout_h,
                measurement.frame_h,
                measurement.visible_h,
                true,
                false,
            ),
            AxisSizeClassification::Violation {
                record: true,
                suspect: false,
            },
            "the first pass may see a genuine height minimum, but the window retry discards it"
        );

        let mut suspects = HashMap::new();
        let second = classify_measurements_and_update_suspects(&[measurement], false, &mut suspects);
        assert_eq!(
            second[0].width,
            AxisSizeClassification::Violation {
                record: true,
                suspect: false,
            }
        );
        assert_eq!(
            second[0].height,
            AxisSizeClassification::Violation {
                record: true,
                suspect: false,
            }
        );
    }

    #[test]
    fn test_inset_finalization_preserves_cache_provenance() {
        let insets = (7, 0, 7, 7);
        assert_eq!(
            inset_finalization(InsetSource::LocalCache, insets, false, true),
            InsetFinalization {
                local: false,
                global: false,
            },
            "a worker-local tuple cannot resurrect a globally-cleared DPI cache"
        );
        assert_eq!(
            inset_finalization(InsetSource::GlobalCache, insets, false, true),
            InsetFinalization {
                local: true,
                global: false,
            }
        );
        assert_eq!(
            inset_finalization(InsetSource::Fresh, insets, false, true),
            InsetFinalization {
                local: true,
                global: true,
            }
        );
        assert_eq!(
            inset_finalization(InsetSource::Fresh, insets, true, true),
            InsetFinalization {
                local: false,
                global: false,
            }
        );
    }

    #[test]
    fn test_inset_finalization_requires_a_matching_generation() {
        let insets = (7, 0, 7, 7);
        for source in [
            InsetSource::Fresh,
            InsetSource::GlobalCache,
            InsetSource::LocalCache,
        ] {
            assert_eq!(
                inset_finalization(source, insets, false, false),
                InsetFinalization {
                    local: false,
                    global: false,
                },
                "a tuple resolved before an invalidation must not be published ({source:?})"
            );
        }
    }

    /// The two generation tests below both observe the process-global
    /// `INSET_CACHE_GENERATION`, and one of them bumps it. Serialize them so the
    /// match case cannot be invalidated by the mismatch case running in parallel.
    static GENERATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_inset_entry(window_id: u64, generation: u64) -> DeferEntry {
        DeferEntry {
            hwnd: HWND::default(),
            window_id,
            x: 0,
            y: 0,
            w: 414,
            h: 400,
            layout_w: 400,
            layout_h: 400,
            insets: (7, 0, 7, 0),
            inset_source: InsetSource::Fresh,
            inset_generation: generation,
            visibility: Visibility::Visible,
            flags: SET_WINDOW_POS_FLAGS(0),
            column_index: 0,
        }
    }

    fn global_cached_insets(window_id: WindowId) -> Option<(i32, i32, i32, i32)> {
        GLOBAL_INSET_CACHE
            .lock()
            .ok()
            .and_then(|global| global.as_ref().and_then(|map| map.get(&window_id).copied()))
    }

    fn forget_global_insets(window_id: WindowId) {
        if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
            if let Some(map) = global.as_mut() {
                map.remove(&window_id);
            }
        }
    }

    #[test]
    fn test_fresh_insets_publish_when_the_generation_still_matches() {
        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let wid: WindowId = 0x7FFF_FF11;
        forget_global_insets(wid);
        let entry = fresh_inset_entry(wid, inset_cache_generation());

        let mut local = PlacementCache::new();
        finalize_cached_border_insets(&[entry], &HashSet::new(), &mut Some(&mut local));

        assert_eq!(local.insets.get(&wid), Some(&(7, 0, 7, 0)));
        assert_eq!(global_cached_insets(wid), Some((7, 0, 7, 0)));
        forget_global_insets(wid);
    }

    #[test]
    fn test_clear_between_lookup_and_publication_blocks_the_stale_fresh_tuple() {
        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let wid: WindowId = 0x7FFF_FF12;
        forget_global_insets(wid);
        // Resolve first, invalidate second — the ordering the deferred
        // publication widened the window on.
        let entry = fresh_inset_entry(wid, inset_cache_generation());
        clear_inset_cache();
        assert_ne!(entry.inset_generation, inset_cache_generation());

        let mut local = PlacementCache::new();
        finalize_cached_border_insets(&[entry], &HashSet::new(), &mut Some(&mut local));

        assert_eq!(
            global_cached_insets(wid),
            None,
            "a pre-clear fresh tuple must not repopulate the cleared global cache"
        );
        assert_eq!(local.insets.get(&wid), None);
    }

    #[test]
    fn test_suspect_updates_merge_current_state_per_measurement() {
        let mut suspects = HashMap::from([(8, (true, false))]);
        let measurement = WindowSizeMeasurement {
            hwnd: HWND::default(),
            window_id: 7,
            layout_w: 400,
            layout_h: 400,
            frame_w: 400,
            frame_h: 400,
            visible_w: 700,
            visible_h: 400,
        };
        let classified = classify_measurements_and_update_suspects(&[measurement], true, &mut suspects);
        assert_eq!(
            classified[0].width,
            AxisSizeClassification::Violation {
                record: false,
                suspect: true,
            }
        );
        assert_eq!(suspects.get(&7), Some(&(true, false)));
        assert_eq!(suspects.get(&8), Some(&(true, false)));
    }

    #[test]
    fn test_classify_oversize_confirmation_and_absurd_guard() {
        let cases = [
            ((false, false, false, false), (false, false)),
            ((true, false, false, false), (true, false)),
            ((true, true, false, false), (false, true)),
            ((true, true, true, false), (true, false)),
            ((true, true, true, true), (false, false)),
            ((true, true, false, true), (false, false)),
        ];

        for ((over, looks_stale, was_suspected, absurd), expected) in cases {
            assert_eq!(
                classify_oversize(over, looks_stale, was_suspected, absurd),
                expected
            );
        }
    }

    #[test]
    fn test_direct_cloak_is_tracked_for_recovery() {
        // A directly-cloaked window (e.g. a stashed scratchpad) must be
        // tracked in DIRECT_CLOAKED so shutdown/panic recovery can restore
        // it; otherwise it would be permanently invisible. Uses a unique
        // wid so it won't collide with parallel tests touching the set.
        let wid: WindowId = 0x7FFF_FF01;
        dwm_cloak_window(wid);
        assert!(
            lock_direct_cloaked()
                .as_ref()
                .is_some_and(|s| s.contains(&wid)),
            "dwm_cloak_window must record the wid for recovery"
        );
        dwm_uncloak_window(wid);
        assert!(
            !lock_direct_cloaked()
                .as_ref()
                .is_some_and(|s| s.contains(&wid)),
            "dwm_uncloak_window must clear the recovery record"
        );
    }

    #[test]
    fn test_apply_placements_empty() {
        // Verify empty placements succeed without error
        let config = PlatformConfig::default();
        let result = apply_placements(&[], &config, None, false);
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
        let result = apply_placements(&placements, &config, None, false);
        assert!(result.is_ok());
    }

    /// Verifies the OR-cloak invariant by directly manipulating the two
    /// global sets and asserting `is_placement_cloaked` returns the OR.
    ///
    /// Uses a synthetic high-bit WindowId that won't collide with any
    /// real HWND on the test machine, since the tracking sets are
    /// process-global.
    #[test]
    fn test_or_cloak_invariant() {
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF00;

        // Snapshot any pre-existing state so we restore cleanly.
        let had_global_before = global_cloaked_contains(wid);
        let had_ghost_before = ghost_cloaked_contains(wid);

        // Case 1: neither set → false.
        {
            let mut g = lock_cloaked();
            if let Some(ref mut s) = *g { s.remove(&wid); }
        }
        {
            let mut g = lock_ghost_cloaked();
            if let Some(ref mut s) = *g { s.remove(&wid); }
        }
        assert!(!is_placement_cloaked(wid), "neither set should give false");

        // Case 2: global only → true.
        {
            let mut g = lock_cloaked();
            let s = g.get_or_insert_with(HashSet::new);
            s.insert(wid);
        }
        assert!(is_placement_cloaked(wid), "global only should give true");

        // Case 3: both sets → true.
        mark_ghost_cloaked(wid);
        assert!(is_placement_cloaked(wid), "both sets should give true");

        // Case 4: ghost only → true.
        {
            let mut g = lock_cloaked();
            if let Some(ref mut s) = *g { s.remove(&wid); }
        }
        assert!(is_placement_cloaked(wid), "ghost only should give true");

        // Case 5: neither → false again.
        unmark_ghost_cloaked(wid);
        assert!(!is_placement_cloaked(wid), "neither again should give false");

        // Restore pre-existing state for whatever ran before this test.
        if had_global_before {
            let mut g = lock_cloaked();
            let s = g.get_or_insert_with(HashSet::new);
            s.insert(wid);
        }
        if had_ghost_before {
            mark_ghost_cloaked(wid);
        }
    }

    /// Regression: a visible tiled placement skipped because the HWND is
    /// maximized retains placement ownership if position-only recovery fails,
    /// then releases exactly that ownership after a successful recovery.
    #[test]
    fn test_skip_visible_tiled_maximized_recovers_only_after_position_succeeds() {
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF10;
        let visible_tiled = WindowPlacement {
            window_id: wid,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        };
        let landing_flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;

        let had_global_before = global_cloaked_contains(wid);
        let had_ghost_before = ghost_cloaked_contains(wid);
        {
            let mut g = lock_cloaked();
            if let Some(ref mut s) = *g { s.remove(&wid); }
        }
        unmark_ghost_cloaked(wid);

        let mut cache = PlacementCache::new();
        assert!(!skip_visible_tiled_maximized(
            &visible_tiled,
            false,
            Some(&mut cache),
            SET_WINDOW_POS_FLAGS(0),
        ));
        let offscreen = WindowPlacement {
            visibility: Visibility::OffScreenLeft,
            ..visible_tiled
        };
        assert!(!skip_visible_tiled_maximized(
            &offscreen,
            true,
            None,
            SET_WINDOW_POS_FLAGS(0),
        ));

        cache
            .positions
            .insert(wid, (visible_tiled.rect, Visibility::Visible));
        mark_placement_parked(wid);
        assert!(skip_visible_tiled_maximized(
            &visible_tiled,
            true,
            Some(&mut cache),
            SET_WINDOW_POS_FLAGS(0),
        ));
        assert!(
            is_placement_parked(wid) && is_placement_cloaked(wid),
            "invalid recovery must retain placement ownership and its effective cloak"
        );
        assert!(
            !cache.positions.contains_key(&wid),
            "failed recovery may invalidate the position cache"
        );
        assert!(!recover_placement_parked(
            wid,
            SET_WINDOW_POS_FLAGS(0),
            |_| false,
        ));
        assert!(
            is_placement_parked(wid) && is_placement_cloaked(wid),
            "failed positioning must preserve placement ownership and its effective cloak"
        );

        assert!(recover_placement_parked(
            wid,
            SET_WINDOW_POS_FLAGS(0),
            |flags| flags == landing_flags,
        ));
        assert!(
            !is_placement_parked(wid),
            "successful landing recovery releases placement ownership exactly once"
        );
        assert!(!recover_placement_parked(
            wid,
            SET_WINDOW_POS_FLAGS(0),
            |_| true,
        ));

        mark_placement_parked(wid);
        mark_ghost_cloaked(wid);
        assert!(recover_placement_parked(wid, SWP_ASYNCWINDOWPOS, |flags| {
            flags == (landing_flags | SWP_ASYNCWINDOWPOS)
        }));
        assert!(!is_placement_parked(wid));
        assert!(
            is_placement_cloaked(wid),
            "successful recovery must retain a ghost-owned effective cloak"
        );
        unmark_ghost_cloaked(wid);

        if had_global_before {
            let mut g = lock_cloaked();
            g.get_or_insert_with(HashSet::new).insert(wid);
        }
        if had_ghost_before {
            mark_ghost_cloaked(wid);
        }
    }
}
