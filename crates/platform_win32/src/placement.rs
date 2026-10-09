//! Window placement application via SetWindowPos / DeferWindowPos.

use crate::types::{PlatformConfig, Win32Error};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::{Rect, Visibility, WindowId, WindowPlacement};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWINDOWATTRIBUTE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClassNameW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsZoomed, SendMessageTimeoutW, SetWindowPos,
    SET_WINDOW_POS_FLAGS, SMTO_ABORTIFHUNG, SMTO_BLOCK, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_NULL,
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
    forget_offscreen_placement(window_id);
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

pub(crate) fn emergency_uncloak_tracked(window_ids: &[WindowId]) {
    for tracked in [&GLOBAL_CLOAKED, &GHOST_CLOAKED, &DIRECT_CLOAKED] {
        let ids: Vec<_> = match tracked.try_lock() {
            Ok(mut guard) => guard
                .as_mut()
                .map(|set| {
                    let ids: Vec<_> = set
                        .iter()
                        .copied()
                        .filter(|id| window_ids.contains(id))
                        .collect();
                    for id in &ids {
                        set.remove(id);
                    }
                    ids
                })
                .unwrap_or_default(),
            Err(_) => continue,
        };
        for id in ids {
            if let Ok(hwnd) = window_id_to_hwnd(id) {
                unsafe {
                    dwm_set_cloak(hwnd, false);
                }
            }
        }
    }
}

/// Force-uncloak every tracked window from both sets. Called during
/// shutdown and panic recovery. Bypasses `apply_cloak_state`.
pub fn dwm_uncloak_all() {
    if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
        records.clear();
    }
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
    for wid in global_ids.into_iter().chain(ghost_ids).chain(direct_ids) {
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

/// Release all placement-owned cloaks and recompute each window's effective
/// cloak state. An empty animation frame can contain only ghost-managed windows,
/// which must remain cloaked until their ghost transition finishes.
fn uncloak_all_tracked() {
    let ids: Vec<WindowId> = {
        let mut guard = lock_cloaked();
        match guard.as_mut() {
            Some(set) => set.drain().collect(),
            None => return,
        }
    };
    for wid in ids {
        apply_cloak_state(wid);
    }
}

/// Global set of window IDs currently cloaked by the placement system.
static GLOBAL_CLOAKED: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);

/// Tests that insert into or drain `GLOBAL_CLOAKED`, `GHOST_CLOAKED`, or
/// `DIRECT_CLOAKED` share those process-global sets. Per-set mutexes only
/// cover a single operation, so a multi-step parked-recovery sequence can
/// lose membership when an empty apply or shutdown uncloak runs in parallel.
#[cfg(test)]
pub(crate) fn lock_cloak_set_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(crate::recover_poisoned_mutex)
}

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
        for id in self.positions.keys() {
            forget_offscreen_placement(*id);
        }
        self.positions.clear();
        // Keep inset cache — insets are a window property, not position-dependent
    }

    /// Clear the cached border insets. Call when system theme or DWM metrics
    /// change (e.g., high contrast toggle) so that stale invisible-border
    /// values don't cause incorrect window sizing.
    pub fn clear_insets(&mut self) {
        for id in self.insets.keys() {
            forget_offscreen_placement(*id);
        }
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

/// Actual native geometry after a placement landing action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementLanding {
    pub window_id: WindowId,
    pub requested_rect: Rect,
    pub requested_visibility: Visibility,
    pub actual_visible_rect: Option<Rect>,
    pub actual_outer_rect: Option<Rect>,
    pub failed: bool,
    pub unreadable: bool,
    /// Left unmeasured on purpose because earlier async frames may still be
    /// queued; the landing stays unconfirmed and is re-checked on a later pass.
    pub measurement_deferred: bool,
}

/// Result of apply_placements, including any detected size violations.
#[derive(Debug, Clone, Default)]
pub struct ApplyPlacementsResult {
    /// Width violations detected after positioning (windows wider than requested).
    pub width_violations: Vec<WidthViolation>,
    /// Height violations detected after positioning (windows taller than requested).
    pub height_violations: Vec<HeightViolation>,
    /// Visible tiled windows omitted because they were maximized at execution time.
    pub maximized_skipped_window_ids: Vec<WindowId>,
    /// Per-window native readback after landing actions, including compositor nudges.
    /// Animation (async) passes leave this empty; containment is a sync-landing concern.
    pub landings: Vec<PlacementLanding>,
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

#[derive(Clone)]
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
    offscreen_rect: Option<Rect>,
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
    post_animation_landing: bool,
) -> Result<ApplyPlacementsResult, Win32Error> {
    let mut queued_endpoints = HashMap::new();
    let owner_deferrals = probe_placement_owners(placements, false, cache.is_none());
    apply_placements_inner(
        placements,
        config,
        &mut cache,
        post_animation_landing,
        true,
        &mut queued_endpoints,
        &owner_deferrals,
    )
}

pub fn apply_display_change_placements(
    placements: &[WindowPlacement],
    config: &PlatformConfig,
    post_animation_landing: bool,
) -> Result<(ApplyPlacementsResult, HashSet<WindowId>), Win32Error> {
    let mut queued_endpoints = HashMap::new();
    let owner_deferrals = probe_placement_owners(placements, true, true);
    apply_placements_inner(
        placements,
        config,
        &mut None,
        post_animation_landing,
        true,
        &mut queued_endpoints,
        &owner_deferrals,
    )
    .map(|result| (result, owner_deferrals.unresponsive))
}

fn apply_placements_inner(
    placements: &[WindowPlacement],
    config: &PlatformConfig,
    cache: &mut Option<&mut PlacementCache>,
    post_animation_landing: bool,
    allow_landing_measurement_retry: bool,
    queued_endpoints: &mut HashMap<WindowId, (i32, i32, i32, i32)>,
    owner_deferrals: &OwnerDeferrals,
) -> Result<ApplyPlacementsResult, Win32Error> {
    if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
        for p in placements {
            if p.visibility == Visibility::Visible {
                records.remove(&p.window_id);
            }
        }
    }
    let empty_result = ApplyPlacementsResult::default();
    if placements.is_empty() {
        if let Some(cache) = cache.as_deref_mut() {
            cache.clear();
        }
        // Uncloak all tracked windows — no placements means all previous
        // windows have left this layout (e.g., workspace switch to empty workspace).
        if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
            records.clear();
        }
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
    let force_positioning = !allow_landing_measurement_retry;
    let (
        entries,
        skipped,
        maximized_skipped_window_ids,
        async_recovery_window_ids,
        failed_recovery_window_ids,
    ) = build_defer_entries(
        placements,
        cache,
        async_flag,
        high_contrast,
        force_positioning,
        owner_deferrals,
        &config.monitor_rects,
    );
    // Uncloak before positioning so DWM composites returning windows at their
    // new rect before the landing measurement. The retry can repeat this safely.
    uncloak_becoming_visible(&entries);
    let mut pending_window_ids = pending_async_entries(&entries);
    pending_window_ids.extend(owner_deferrals.pending.iter().copied());
    let (applied, mut failed_window_ids) = position_entries_for_pending(
        &entries,
        cache.is_none() && post_animation_landing,
        &pending_window_ids,
        queued_endpoints,
    );
    if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
        for entry in &entries {
            if entry.visibility != Visibility::Visible {
                records.remove(&entry.window_id);
                if !failed_window_ids.contains(&entry.window_id) {
                    if let Some(rect) = entry.offscreen_rect {
                        records.insert(entry.window_id, rect);
                    }
                }
            }
        }
    }
    failed_window_ids.extend(failed_recovery_window_ids.iter().copied());
    let mut guard = lock_async_positions();
    if let Some(submissions) = guard.as_mut() {
        for window_id in &owner_deferrals.wait_for_owner {
            if !failed_window_ids.contains(window_id) {
                if let Some(submission) = submissions.get_mut(window_id) {
                    submission.wait_for_owner = true;
                }
            }
        }
    }
    drop(guard);

    // On the synchronous landing pass, compare the DWM visible measurement to
    // both the layout request and the expanded SetWindowPos frame request. A
    // visible rect between them is a stale-inset artifact, not a window minimum:
    // Slack/Spotify can change their client frame at runtime, and Chromium can
    // briefly become frameless after app fullscreen. Retry the complete batch
    // once after evicting affected inset tuples or confirming suspect oversize
    // measurements; first-pass suspect marks may carry, but first-pass
    // violation/cache/nudge finalization does not.
    let detection = if async_flag == SET_WINDOW_POS_FLAGS(0) {
        detect_size_violations(
            &entries,
            &failed_window_ids,
            &pending_window_ids,
            allow_landing_measurement_retry,
            high_contrast,
        )
    } else {
        SizeViolationDetection::default()
    };
    if should_retry_landing_measurement(allow_landing_measurement_retry, &detection) {
        tracing::debug!(
            "Retrying placement batch after {} stale inset artifact(s) and {} suspect oversize measurement(s)",
            detection.inset_artifact_windows.len(),
            detection.suspect_confirmation_windows.len(),
        );
        evict_cached_border_insets(&detection.inset_artifact_windows, cache);
        return apply_placements_inner(
            placements,
            config,
            cache,
            post_animation_landing,
            false,
            queued_endpoints,
            owner_deferrals,
        );
    }

    finalize_cached_border_insets(&entries, &detection.inset_artifact_windows, cache);
    evict_cached_border_insets(&detection.violating_windows, cache);

    // Update cache: remove stale entries (windows no longer in placements),
    // update positioned entries, and keep skipped-unchanged entries intact.
    if let Some(cache) = cache.as_deref_mut() {
        let current_ids: std::collections::HashSet<u64> =
            placements.iter().map(|p| p.window_id).collect();
        // Remove windows that are no longer in the layout
        cache.positions.retain(|id, _| {
            if !current_ids.contains(id) {
                forget_offscreen_placement(*id);
                return false;
            }
            true
        });
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
    if async_flag == SET_WINDOW_POS_FLAGS(0) && post_animation_landing {
        let nudge_targets: Vec<NudgeTarget> = entries
            .iter()
            .filter(|e| {
                e.visibility == Visibility::Visible
                    && e.w > 1
                    && !failed_window_ids.contains(&e.window_id)
                    && !pending_window_ids.contains(&e.window_id)
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

    let landings = if async_flag == SET_WINDOW_POS_FLAGS(0) {
        let mut queued_recoveries = lock_queued_recoveries();
        let submissions = queued_recoveries.get_or_insert_with(HashMap::new);
        collect_placement_landings_with_recoveries(
            placements,
            LandingFailures {
                window_ids: &failed_window_ids,
                recovery_window_ids: &failed_recovery_window_ids,
            },
            &pending_window_ids,
            &async_recovery_window_ids,
            submissions,
            |window_id| {
                window_id_to_hwnd(window_id).ok().and_then(|hwnd| {
                    let mut rect = RECT::default();
                    unsafe { GetWindowRect(hwnd, &mut rect).ok() }.map(|_| (rect.left, rect.top))
                })
            },
            |window_id| {
                (
                    crate::get_window_visible_rect(window_id),
                    crate::get_window_chrome_rect(window_id),
                )
            },
        )
    } else {
        Vec::new()
    };
    if let Some(cache) = cache.as_deref_mut() {
        for landing in &landings {
            if landing.failed || landing.unreadable {
                cache.positions.remove(&landing.window_id);
            }
        }
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
        landings,
    })
}

#[derive(Clone, Copy)]
struct LandingFailures<'a> {
    window_ids: &'a HashSet<u64>,
    recovery_window_ids: &'a HashSet<WindowId>,
}

fn collect_placement_landings_with_recoveries(
    placements: &[WindowPlacement],
    failures: LandingFailures<'_>,
    pending_window_ids: &HashSet<WindowId>,
    unmeasured_window_ids: &HashSet<WindowId>,
    recoveries: &mut HashMap<WindowId, AsyncPositionSubmission>,
    mut current_position: impl FnMut(WindowId) -> Option<(i32, i32)>,
    mut measure: impl FnMut(WindowId) -> (Option<Rect>, Option<Rect>),
) -> Vec<PlacementLanding> {
    let now = std::time::Instant::now();
    let mut unmeasured_window_ids = unmeasured_window_ids.clone();
    unmeasured_window_ids.extend(failures.recovery_window_ids.iter().copied());
    for placement in placements {
        if !recoveries.contains_key(&placement.window_id) {
            continue;
        }
        if retain_pending_queued_recovery(
            recoveries,
            placement.window_id,
            current_position(placement.window_id),
            now,
        ) {
            unmeasured_window_ids.insert(placement.window_id);
        }
    }

    placements
        .iter()
        .map(|placement| {
            let queued_recovery = unmeasured_window_ids.contains(&placement.window_id);
            let unmeasured = pending_window_ids.contains(&placement.window_id) || queued_recovery;
            let (actual_visible_rect, actual_outer_rect) = if unmeasured {
                (None, None)
            } else {
                measure(placement.window_id)
            };
            PlacementLanding {
                window_id: placement.window_id,
                requested_rect: placement.rect,
                requested_visibility: placement.visibility,
                failed: failures.recovery_window_ids.contains(&placement.window_id)
                    || (!queued_recovery && failures.window_ids.contains(&placement.window_id)),
                unreadable: unmeasured
                    || (actual_visible_rect.is_none() && actual_outer_rect.is_none()),
                measurement_deferred: unmeasured,
                actual_visible_rect,
                actual_outer_rect,
            }
        })
        .collect()
}

/// Read GWL_STYLE for a managed HWND. Used as physical-observation context, not
/// as a placement policy.
pub fn get_window_style_bits(window_id: WindowId) -> Option<u32> {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        Some(GetWindowLongW(hwnd, GWL_STYLE) as u32)
    }
}

fn recovery_flags(async_flag: SET_WINDOW_POS_FLAGS, pending: bool) -> SET_WINDOW_POS_FLAGS {
    let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | async_flag;
    if async_flag == SET_WINDOW_POS_FLAGS(0) && pending {
        flags | SWP_ASYNCWINDOWPOS
    } else {
        flags
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParkedRecoveryOutcome {
    NotAttempted,
    Succeeded,
    Failed,
}

fn recover_placement_parked<F>(
    window_id: WindowId,
    async_flag: SET_WINDOW_POS_FLAGS,
    pending: bool,
    position: F,
) -> ParkedRecoveryOutcome
where
    F: FnOnce(SET_WINDOW_POS_FLAGS) -> bool,
{
    if !is_placement_parked(window_id) {
        return ParkedRecoveryOutcome::NotAttempted;
    }
    let flags = recovery_flags(async_flag, pending);
    if !position(flags) {
        return ParkedRecoveryOutcome::Failed;
    }
    let released = {
        let mut cloaked = lock_cloaked();
        cloaked.as_mut().is_some_and(|set| set.remove(&window_id))
    };
    if released {
        apply_cloak_state(window_id);
        ParkedRecoveryOutcome::Succeeded
    } else {
        ParkedRecoveryOutcome::NotAttempted
    }
}

fn skip_visible_tiled_maximized(
    placement: &WindowPlacement,
    is_zoomed: bool,
    cache: Option<&mut PlacementCache>,
    async_flag: SET_WINDOW_POS_FLAGS,
    owner_pending: bool,
    async_recovery: &mut bool,
    failed_recovery: &mut bool,
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
        let pending = is_placement_parked(placement.window_id)
            && (owner_pending
                || (async_flag == SET_WINDOW_POS_FLAGS(0)
                    && pending_async_position(placement.window_id)));
        let recovery =
            recover_placement_parked(placement.window_id, async_flag, pending, |flags| {
                let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {
                    return false;
                };
                unsafe {
                    SetWindowPos(hwnd, None, placement.rect.x, placement.rect.y, 0, 0, flags)
                        .is_ok()
                }
            });
        *async_recovery = recovery == ParkedRecoveryOutcome::Succeeded && pending;
        *failed_recovery = recovery == ParkedRecoveryOutcome::Failed;
        if *async_recovery {
            record_queued_recovery(placement.window_id, placement.rect.x, placement.rect.y);
            if owner_pending {
                record_async_coordinates(
                    placement.window_id,
                    placement.rect.x,
                    placement.rect.y,
                    None,
                    false,
                );
            }
        }
    }
    skip
}

static OFFSCREEN_PLACEMENTS: std::sync::LazyLock<Mutex<HashMap<WindowId, Rect>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn forget_offscreen_placement(window_id: WindowId) {
    if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
        records.remove(&window_id);
    }
}

pub fn offscreen_placement_matches(window_id: WindowId) -> Option<bool> {
    let expected = OFFSCREEN_PLACEMENTS.lock().ok()?.get(&window_id).copied()?;
    crate::get_window_chrome_rect(window_id).map(|actual| actual == expected)
}

/// Resolve a position-only placement without exposing extra app-owned width
/// on a monitor that the requested slot did not already overlap.
fn offscreen_frame_origin(
    placement: &WindowPlacement,
    frame_rect: Rect,
    actual_size: (i32, i32),
    monitor_rects: &[Rect],
) -> (i32, i32) {
    if placement.visibility != Visibility::Visible
        && ((placement.rect.width == 0 && placement.rect.height == 0)
            || crate::is_move_offscreen_sentinel_rect(&placement.rect))
    {
        // Sentinel parking is already in outer-frame space, not visible-frame space.
        return (placement.rect.x, placement.rect.y);
    }
    if placement.visibility != Visibility::OffScreenLeft || actual_size.0 <= frame_rect.width {
        return (frame_rect.x, frame_rect.y);
    }
    let x = frame_rect
        .x
        .saturating_sub(actual_size.0 - frame_rect.width);
    let footprint = Rect::new(x, frame_rect.y, actual_size.0, actual_size.1);
    if monitor_rects.is_empty()
        || monitor_rects
            .iter()
            .any(|monitor| footprint.intersects(monitor) && !frame_rect.intersects(monitor))
    {
        let sentinel = crate::MOVE_OFFSCREEN_SENTINEL_COORD;
        let park_x = monitor_rects
            .iter()
            .map(|monitor| monitor.x.saturating_sub(actual_size.0))
            .min()
            .unwrap_or(sentinel)
            .min(sentinel);
        let park_y = monitor_rects
            .iter()
            .map(|monitor| monitor.y.saturating_sub(actual_size.1))
            .min()
            .unwrap_or(sentinel)
            .min(sentinel);
        (park_x, park_y)
    } else {
        (x, frame_rect.y)
    }
}

/// Build the defer-entry list for all placements, skipping cache-unchanged windows.
fn build_defer_entries(
    placements: &[WindowPlacement],
    cache: &mut Option<&mut PlacementCache>,
    async_flag: SET_WINDOW_POS_FLAGS,
    high_contrast: bool,
    force_positioning: bool,
    owner_deferrals: &OwnerDeferrals,
    monitor_rects: &[Rect],
) -> (
    Vec<DeferEntry>,
    u32,
    Vec<WindowId>,
    HashSet<WindowId>,
    HashSet<WindowId>,
) {
    let mut skipped = 0u32;
    let mut entries: Vec<DeferEntry> = Vec::with_capacity(placements.len());
    let mut maximized_skipped_window_ids = Vec::new();
    let mut async_recovery_window_ids = HashSet::new();
    let mut failed_recovery_window_ids = HashSet::new();

    for placement in placements {
        let Ok(hwnd) = window_id_to_hwnd(placement.window_id) else {
            continue;
        };
        let is_zoomed = unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                continue;
            }
            IsZoomed(hwnd).as_bool()
        };
        let mut async_recovery = false;
        let mut failed_recovery = false;
        if skip_visible_tiled_maximized(
            placement,
            is_zoomed,
            cache.as_deref_mut(),
            async_flag,
            owner_deferrals.pending.contains(&placement.window_id),
            &mut async_recovery,
            &mut failed_recovery,
        ) {
            maximized_skipped_window_ids.push(placement.window_id);
            if async_recovery {
                async_recovery_window_ids.insert(placement.window_id);
            }
            if failed_recovery {
                failed_recovery_window_ids.insert(placement.window_id);
            }
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
                offscreen_rect: None,
            });
        } else {
            // Off-screen: SWP_NOSIZE keeps current size (no resize side-effects).
            // w stores estimated frame width for clamping only — SetWindowPos
            // ignores it due to SWP_NOSIZE.
            //
            // Zero-size hidden-tab placeholders are not a visible-rect request.
            // Converting them through frame insets makes the parked origin
            // depend on whether the inset cache is warm or cold after restart.
            let zero_size_hidden_tab = placement.rect.width == 0 && placement.rect.height == 0;
            let actual_rect = crate::get_window_chrome_rect(placement.window_id);
            let actual_size = actual_rect
                .map(|rect| (rect.width, rect.height))
                .unwrap_or((frame_rect.width, frame_rect.height));
            let (x, y) = offscreen_frame_origin(placement, frame_rect, actual_size, monitor_rects);
            let w = if zero_size_hidden_tab {
                0
            } else {
                frame_rect.width
            };
            entries.push(DeferEntry {
                hwnd,
                window_id: placement.window_id,
                x,
                y,
                w,
                h: 0,
                layout_w: placement.rect.width,
                layout_h: placement.rect.height,
                insets,
                inset_source,
                inset_generation,
                visibility: placement.visibility,
                flags: SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | async_flag,
                column_index: placement.column_index,
                offscreen_rect: actual_rect.map(|rect| Rect::new(x, y, rect.width, rect.height)),
            });
        }
    }

    (
        entries,
        skipped,
        maximized_skipped_window_ids,
        async_recovery_window_ids,
        failed_recovery_window_ids,
    )
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

#[derive(Default)]
struct OwnerDeferrals {
    pending: HashSet<WindowId>,
    wait_for_owner: HashSet<WindowId>,
    unresponsive: HashSet<WindowId>,
}

fn probe_placement_owners(
    placements: &[WindowPlacement],
    display_change: bool,
    probe_marked: bool,
) -> OwnerDeferrals {
    let marked: HashSet<_> = lock_async_positions()
        .as_ref()
        .map(|submissions| {
            submissions
                .iter()
                .filter_map(|(&window_id, submission)| {
                    submission.wait_for_owner.then_some(window_id)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut deferrals = OwnerDeferrals::default();
    let mut owner_answers = HashMap::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let placed: HashSet<_> = placements
        .iter()
        .map(|placement| placement.window_id)
        .collect();
    let omitted = marked
        .iter()
        .copied()
        .filter(|window_id| probe_marked && !placed.contains(window_id));
    for window_id in placements
        .iter()
        .map(|placement| placement.window_id)
        .chain(omitted)
    {
        let was_marked = marked.contains(&window_id);
        if !display_change && !was_marked {
            continue;
        }
        if was_marked {
            deferrals.pending.insert(window_id);
        }
        if !probe_marked {
            continue;
        }
        let Ok(hwnd) = window_id_to_hwnd(window_id) else {
            continue;
        };
        if unsafe { !IsWindow(Some(hwnd)).as_bool() } {
            clear_async_position(window_id);
            deferrals.pending.remove(&window_id);
            continue;
        }
        if unsafe { IsIconic(hwnd).as_bool() } {
            continue;
        }
        let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
        let answered = *owner_answers.entry(owner_thread).or_insert_with(|| {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let timeout_ms = remaining.as_millis().min(100) as u32;
            if timeout_ms == 0 {
                return None;
            }
            Some(unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_NULL,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    timeout_ms,
                    None,
                )
                .0 != 0
            })
        });
        if answered == Some(true) {
            if was_marked {
                if let Some(submission) = lock_async_positions()
                    .as_mut()
                    .and_then(|submissions| submissions.get_mut(&window_id))
                {
                    submission.wait_for_owner = false;
                    submission.first_submitted = std::time::Instant::now();
                }
            }
        } else {
            deferrals.pending.insert(window_id);
            deferrals.wait_for_owner.insert(window_id);
            if answered == Some(false) {
                deferrals.unresponsive.insert(window_id);
            }
        }
    }
    deferrals
}

/// Position all entries in one DeferWindowPos batch; returns (applied, failed ids).
#[cfg(test)]
fn position_entries(entries: &[DeferEntry], post_animation_landing: bool) -> (u32, HashSet<u64>) {
    let pending = pending_async_entries(entries);
    let mut queued_endpoints = HashMap::new();
    position_entries_for_pending(
        entries,
        post_animation_landing,
        &pending,
        &mut queued_endpoints,
    )
}

fn position_entries_for_pending(
    entries: &[DeferEntry],
    post_animation_landing: bool,
    pending: &HashSet<WindowId>,
    queued_endpoints: &mut HashMap<WindowId, (i32, i32, i32, i32)>,
) -> (u32, HashSet<u64>) {
    position_entries_with_pending(
        entries,
        post_animation_landing,
        pending,
        queued_endpoints,
        |entry| unsafe {
            SetWindowPos(
                entry.hwnd,
                None,
                entry.x,
                entry.y,
                entry.w,
                entry.h,
                (entry.flags & !SWP_FRAMECHANGED) | SWP_ASYNCWINDOWPOS,
            )
        },
        position_entries_batch,
    )
}

#[cfg(test)]
fn position_entries_with<Q, P>(
    entries: &[DeferEntry],
    post_animation_landing: bool,
    queue_endpoint: Q,
    position_batch: P,
) -> (u32, HashSet<u64>)
where
    Q: FnMut(&DeferEntry) -> windows::core::Result<()>,
    P: FnOnce(&[DeferEntry]) -> HashSet<u64>,
{
    let mut queued_endpoints = HashMap::new();
    position_entries_with_pending(
        entries,
        post_animation_landing,
        &HashSet::new(),
        &mut queued_endpoints,
        queue_endpoint,
        position_batch,
    )
}

fn position_entries_with_pending<Q, P>(
    entries: &[DeferEntry],
    post_animation_landing: bool,
    pending_window_ids: &HashSet<WindowId>,
    queued_endpoints: &mut HashMap<WindowId, (i32, i32, i32, i32)>,
    mut queue_endpoint: Q,
    position_batch: P,
) -> (u32, HashSet<u64>)
where
    Q: FnMut(&DeferEntry) -> windows::core::Result<()>,
    P: FnOnce(&[DeferEntry]) -> HashSet<u64>,
{
    let mut failures = HashSet::new();
    for entry in entries {
        let endpoint_target = (entry.x, entry.y, entry.w, entry.h);
        if !(post_animation_landing || pending_window_ids.contains(&entry.window_id))
            || queued_endpoints.get(&entry.window_id) == Some(&endpoint_target)
        {
            continue;
        }
        if let Err(error) = queue_endpoint(entry) {
            tracing::warn!(window_id = entry.window_id, %error, "Could not queue animation endpoint");
            failures.insert(entry.window_id);
        } else {
            queued_endpoints.insert(entry.window_id, endpoint_target);
            record_async_position(entry);
        }
    }

    // A pending owner-thread move must not be overtaken by this pass's synchronous batch.
    let synchronous_entries: Vec<_> = entries
        .iter()
        .filter(|entry| !pending_window_ids.contains(&entry.window_id))
        .cloned()
        .collect();
    let batch_failures = position_batch(&synchronous_entries);
    for entry in &synchronous_entries {
        if batch_failures.contains(&entry.window_id) {
            continue;
        }
        if entry.flags.contains(SWP_ASYNCWINDOWPOS) {
            record_async_position(entry);
        } else if !post_animation_landing {
            clear_async_position(entry.window_id);
        }
    }
    failures.extend(batch_failures);
    ((entries.len() - failures.len()) as u32, failures)
}

fn pending_async_entries(entries: &[DeferEntry]) -> HashSet<WindowId> {
    let now = std::time::Instant::now();
    let mut guard = lock_async_positions();
    let Some(submissions) = guard.as_mut() else {
        return HashSet::new();
    };
    let mut pending = HashSet::new();
    for entry in entries {
        let Some(previous) = submissions.get(&entry.window_id).copied() else {
            continue;
        };
        let current = observed_async_position(entry.hwnd);
        match async_position_state(previous, current, now) {
            AsyncPositionState::Drained => {
                submissions.remove(&entry.window_id);
            }
            AsyncPositionState::Pending => {
                pending.insert(entry.window_id);
            }
            AsyncPositionState::Expired => {}
        }
    }
    pending
}

fn observed_async_position(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
    Some((rect.left, rect.top))
}

fn record_async_position(entry: &DeferEntry) {
    let size = (!entry.flags.contains(SWP_NOSIZE)).then_some((entry.w, entry.h));
    record_async_coordinates(entry.window_id, entry.x, entry.y, size, false);
}

pub(crate) fn record_queued_owner_position(window_id: WindowId, x: i32, y: i32) {
    record_async_coordinates(window_id, x, y, None, true);
}

pub(crate) fn record_queued_owner_rect(window_id: WindowId, rect: &Rect) {
    record_async_coordinates(
        window_id,
        rect.x,
        rect.y,
        Some((rect.width, rect.height)),
        true,
    );
}

fn record_async_coordinates(
    window_id: WindowId,
    x: i32,
    y: i32,
    size: Option<(i32, i32)>,
    wait_for_owner: bool,
) {
    let now = std::time::Instant::now();
    let mut guard = lock_async_positions();
    let submissions = guard.get_or_insert_with(HashMap::new);
    let mut next = async_position_submission(submissions.get(&window_id).copied(), (x, y), now);
    next.size = size.or(next.size);
    next.wait_for_owner |= wait_for_owner;
    submissions.insert(window_id, next);
}

fn async_position_submission(
    previous: Option<AsyncPositionSubmission>,
    position: (i32, i32),
    now: std::time::Instant,
) -> AsyncPositionSubmission {
    match previous {
        Some(previous) => AsyncPositionSubmission {
            x: position.0,
            y: position.1,
            first_submitted: previous.first_submitted,
            size: previous.size,
            wait_for_owner: previous.wait_for_owner,
        },
        None => AsyncPositionSubmission {
            x: position.0,
            y: position.1,
            first_submitted: now,
            size: None,
            wait_for_owner: false,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsyncPositionState {
    Drained,
    Pending,
    Expired,
}

fn async_position_state(
    submission: AsyncPositionSubmission,
    current_position: Option<(i32, i32)>,
    now: std::time::Instant,
) -> AsyncPositionState {
    if submission.wait_for_owner {
        AsyncPositionState::Pending
    } else if current_position == Some((submission.x, submission.y)) {
        AsyncPositionState::Drained
    } else if now.saturating_duration_since(submission.first_submitted)
        < ASYNC_POSITION_PENDING_LIMIT
    {
        AsyncPositionState::Pending
    } else {
        AsyncPositionState::Expired
    }
}

fn pending_async_position(window_id: WindowId) -> bool {
    let now = std::time::Instant::now();
    let mut guard = lock_async_positions();
    let Some(submissions) = guard.as_mut() else {
        return false;
    };
    let Some(submission) = submissions.get(&window_id).copied() else {
        return false;
    };
    let current = window_id_to_hwnd(window_id)
        .ok()
        .and_then(observed_async_position);
    match async_position_state(submission, current, now) {
        AsyncPositionState::Drained => {
            submissions.remove(&window_id);
            false
        }
        AsyncPositionState::Pending => true,
        AsyncPositionState::Expired => false,
    }
}

pub(crate) fn owner_wait_target(window_id: WindowId, current: &Rect) -> Option<Rect> {
    let submission = lock_async_positions()
        .as_ref()
        .and_then(|submissions| submissions.get(&window_id))
        .copied()?;
    if !submission.wait_for_owner {
        return None;
    }
    let (width, height) = submission.size.unwrap_or((current.width, current.height));
    Some(Rect::new(submission.x, submission.y, width, height))
}

fn clear_async_position(window_id: WindowId) {
    if let Some(positions) = lock_async_positions().as_mut() {
        positions.remove(&window_id);
    }
    if let Some(recoveries) = lock_queued_recoveries().as_mut() {
        recoveries.remove(&window_id);
    }
}

fn position_entries_batch(entries: &[DeferEntry]) -> HashSet<u64> {
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
                            entry.hwnd,
                            None,
                            entry.x,
                            entry.y,
                            entry.w,
                            entry.h,
                            entry.flags,
                        )
                        .is_err()
                        {
                            failed_window_ids.insert(entry.window_id);
                        }
                    }
                }
                Ok(initial_hdwp) => {
                    let mut hdwp = initial_hdwp;
                    let mut batch_ok = true;
                    for entry in entries {
                        match DeferWindowPos(
                            hdwp,
                            entry.hwnd,
                            None,
                            entry.x,
                            entry.y,
                            entry.w,
                            entry.h,
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
                                    entry.hwnd,
                                    None,
                                    entry.x,
                                    entry.y,
                                    entry.w,
                                    entry.h,
                                    entry.flags,
                                )
                                .is_err()
                                {
                                    failed_window_ids.insert(entry.window_id);
                                }
                            }
                        }
                    } else {
                        // DeferWindowPos failed — HDWP is already freed by Win32.
                        // Fall back to individual SetWindowPos calls.
                        for entry in entries {
                            if SetWindowPos(
                                entry.hwnd,
                                None,
                                entry.x,
                                entry.y,
                                entry.w,
                                entry.h,
                                entry.flags,
                            )
                            .is_err()
                            {
                                failed_window_ids.insert(entry.window_id);
                            }
                        }
                    }
                }
            }
        }
    }

    failed_window_ids
}

type SuspectedSizes = (Option<i32>, Option<i32>);

#[derive(Clone, Copy)]
struct AsyncPositionSubmission {
    x: i32,
    y: i32,
    size: Option<(i32, i32)>,
    first_submitted: std::time::Instant,
    // Display-change deferrals must not expire into a synchronous call while the owner is hung.
    wait_for_owner: bool,
}

// Expire pending work so windows that clamp or reject placement resume normal handling.
const ASYNC_POSITION_PENDING_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
static ASYNC_POSITIONS: Mutex<Option<HashMap<WindowId, AsyncPositionSubmission>>> =
    Mutex::new(None);
static QUEUED_RECOVERIES: Mutex<Option<HashMap<WindowId, AsyncPositionSubmission>>> =
    Mutex::new(None);

fn lock_queued_recoveries(
) -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, AsyncPositionSubmission>>> {
    QUEUED_RECOVERIES
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn record_queued_recovery(window_id: WindowId, x: i32, y: i32) {
    let submission = AsyncPositionSubmission {
        x,
        y,
        first_submitted: std::time::Instant::now(),
        size: None,
        wait_for_owner: false,
    };
    lock_queued_recoveries()
        .get_or_insert_with(HashMap::new)
        .insert(window_id, submission);
}

/// Drops drained or expired recovery records.
fn retain_pending_queued_recovery(
    submissions: &mut HashMap<WindowId, AsyncPositionSubmission>,
    window_id: WindowId,
    current_position: Option<(i32, i32)>,
    now: std::time::Instant,
) -> bool {
    let Some(submission) = submissions.get(&window_id).copied() else {
        return false;
    };
    match async_position_state(submission, current_position, now) {
        AsyncPositionState::Drained | AsyncPositionState::Expired => {
            submissions.remove(&window_id);
            false
        }
        AsyncPositionState::Pending => true,
    }
}

fn lock_async_positions(
) -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, AsyncPositionSubmission>>> {
    ASYNC_POSITIONS
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

/// Keep measured width/height candidates across the bounded landing retry.
/// Two changing oversize samples must not become a native minimum. Destroy
/// cleanup prevents a recycled HWND from inheriting another window's candidate.
static SUSPECTED_OVERSIZE: Mutex<Option<HashMap<WindowId, SuspectedSizes>>> = Mutex::new(None);

fn lock_suspected_oversize(
) -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, SuspectedSizes>>> {
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
    clear_async_position(window_id);
}

const VISIBLE_SIZE_TOLERANCE: i32 = 2;
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
    used_insets: (i32, i32, i32, i32),
    landed_insets: Option<(i32, i32, i32, i32)>,
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
    suspect_confirmation_windows: HashSet<WindowId>,
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
    allow_landing_measurement_retry: bool,
    suspected_size: Option<i32>,
) -> AxisSizeClassification {
    if visible_size <= layout_size + VISIBLE_SIZE_TOLERANCE {
        return AxisSizeClassification::Fits;
    }
    if allow_landing_measurement_retry && visible_size <= frame_size + VISIBLE_SIZE_TOLERANCE {
        return AxisSizeClassification::InsetArtifact;
    }

    // Even a small excess can be an animation frame that has not settled yet.
    // Use the existing bounded retry before promoting it to a native minimum.
    let absurd = layout_size > 0 && visible_size > layout_size * ABSURD_BOUNDS_RATIO;
    let confirmed = suspected_size == Some(visible_size) && !allow_landing_measurement_retry;
    AxisSizeClassification::Violation {
        record: !absurd && confirmed,
        suspect: !absurd && !confirmed,
    }
}

fn is_inset_artifact(layout_size: i32, frame_size: i32, visible_size: i32) -> bool {
    visible_size > layout_size + VISIBLE_SIZE_TOLERANCE
        && visible_size <= frame_size + VISIBLE_SIZE_TOLERANCE
}

fn should_retry_landing_measurement(
    allow_landing_measurement_retry: bool,
    detection: &SizeViolationDetection,
) -> bool {
    allow_landing_measurement_retry
        && (!detection.inset_artifact_windows.is_empty()
            || !detection.suspect_confirmation_windows.is_empty())
}

fn classification_outcome(classification: &AxisSizeClassification) -> (bool, bool) {
    match classification {
        AxisSizeClassification::Violation { record, suspect } => (*record, *suspect),
        AxisSizeClassification::Fits | AxisSizeClassification::InsetArtifact => (false, false),
    }
}

fn classify_measurements_and_update_suspects(
    measurements: &[WindowSizeMeasurement],
    allow_landing_measurement_retry: bool,
    suspects: &mut HashMap<WindowId, SuspectedSizes>,
) -> Vec<ClassifiedSizeMeasurement> {
    measurements
        .iter()
        .map(|measurement| {
            let (was_w, was_h) = suspects
                .get(&measurement.window_id)
                .copied()
                .unwrap_or((None, None));
            let width = classify_size_axis(
                measurement.layout_w,
                measurement.frame_w,
                measurement.visible_w,
                allow_landing_measurement_retry,
                was_w,
            );
            let height = classify_size_axis(
                measurement.layout_h,
                measurement.frame_h,
                measurement.visible_h,
                allow_landing_measurement_retry,
                was_h,
            );
            let suspect_w = matches!(
                width,
                AxisSizeClassification::InsetArtifact
                    | AxisSizeClassification::Violation { suspect: true, .. }
            );
            let suspect_h = matches!(
                height,
                AxisSizeClassification::InsetArtifact
                    | AxisSizeClassification::Violation { suspect: true, .. }
            );
            if suspect_w || suspect_h {
                suspects.insert(
                    measurement.window_id,
                    (
                        suspect_w.then_some(measurement.visible_w),
                        suspect_h.then_some(measurement.visible_h),
                    ),
                );
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
    pending_window_ids: &HashSet<WindowId>,
    allow_landing_measurement_retry: bool,
    high_contrast: bool,
) -> SizeViolationDetection {
    // Synchronize with composition, but do not treat this as an owner-thread
    // queue barrier: a delayed animation frame can still yield stale bounds.
    // Oversize measurements use the bounded placement retry below.
    unsafe {
        let _ = DwmFlush();
    }

    let mut measurements = Vec::new();
    for entry in entries {
        if entry.column_index == usize::MAX
            || entry.visibility != Visibility::Visible
            || failed_window_ids.contains(&entry.window_id)
            || pending_window_ids.contains(&entry.window_id)
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
            used_insets: entry.insets,
            landed_insets: (allow_landing_measurement_retry && !high_contrast)
                .then(|| query_window_frame_insets(entry.hwnd))
                .flatten(),
        });
    }

    let mut guard = lock_suspected_oversize();
    let suspects = guard.get_or_insert_with(HashMap::new);
    detect_size_violations_from_measurements(
        &measurements,
        allow_landing_measurement_retry,
        suspects,
    )
}

fn detect_size_violations_from_measurements(
    measurements: &[WindowSizeMeasurement],
    allow_landing_measurement_retry: bool,
    suspects: &mut HashMap<WindowId, SuspectedSizes>,
) -> SizeViolationDetection {
    let classified = classify_measurements_and_update_suspects(
        measurements,
        allow_landing_measurement_retry,
        suspects,
    );

    let mut detection = SizeViolationDetection::default();
    for classified in classified {
        let undersized_with_changed_insets =
            classified.measurement.landed_insets.is_some_and(|landed| {
                landed != classified.measurement.used_insets
                    && (classified
                        .measurement
                        .visible_w
                        .saturating_add(VISIBLE_SIZE_TOLERANCE)
                        < classified.measurement.layout_w
                        || classified
                            .measurement
                            .visible_h
                            .saturating_add(VISIBLE_SIZE_TOLERANCE)
                            < classified.measurement.layout_h)
            });
        if allow_landing_measurement_retry
            && (undersized_with_changed_insets
                || is_inset_artifact(
                    classified.measurement.layout_w,
                    classified.measurement.frame_w,
                    classified.measurement.visible_w,
                )
                || is_inset_artifact(
                    classified.measurement.layout_h,
                    classified.measurement.frame_h,
                    classified.measurement.visible_h,
                ))
        {
            detection
                .inset_artifact_windows
                .insert(classified.measurement.window_id);
        }
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
        detection
            .suspect_confirmation_windows
            .insert(measurement.window_id);
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
    "Chrome_WidgetWin_1", // Electron / Chromium (Slack, Beeper, Spotify, TradingView)
    "MozillaWindowClass", // Firefox / Zen
    "CASCADIA_HOSTING_WINDOW_CLASS", // Windows Terminal
    "CabinetWClass",      // File Explorer
];

fn is_sticky_compositor_class(class: &str) -> bool {
    STICKY_COMPOSITOR_CLASSES.contains(&class)
}

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
        if !is_sticky_compositor_class(&class) {
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
    if let Ok(mut records) = OFFSCREEN_PLACEMENTS.lock() {
        records.clear();
    }
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
        visible_rect
            .width
            .saturating_add(left)
            .saturating_add(right),
        visible_rect
            .height
            .saturating_add(top)
            .saturating_add(bottom),
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

fn frame_insets_from_rects(frame_rect: Rect, visible_rect: Rect) -> Option<(i32, i32, i32, i32)> {
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
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return None;
    };
    query_window_frame_insets(hwnd)
}

/// Public wrapper over `invisible_border_insets` that takes a `WindowId`.
/// Returns `(left, top, right, bottom)` insets, or `(0, 0, 0, 0)` if the
/// window has no DWM bounds available. Used by callers that need to
/// translate between chrome (`GetWindowRect`) coordinates and visible-
/// content (layout) coordinates without reaching into placement internals.
pub fn get_window_invisible_insets(window_id: WindowId) -> (i32, i32, i32, i32) {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return (0, 0, 0, 0);
    };
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
    query_window_frame_insets(hwnd).unwrap_or((0, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticky_compositor_nudge_includes_explorer_and_existing_classes_only() {
        for class in [
            "CabinetWClass",
            "Chrome_WidgetWin_1",
            "MozillaWindowClass",
            "CASCADIA_HOSTING_WINDOW_CLASS",
        ] {
            assert!(is_sticky_compositor_class(class), "{class}");
        }
        assert!(!is_sticky_compositor_class("Notepad"));
    }

    #[test]
    fn async_position_pending_expires_and_preserves_first_submission_time() {
        use std::time::{Duration, Instant};

        let submitted = Instant::now();
        let position = (400, 100);
        let recent = submitted + Duration::from_secs(1);
        let expired = submitted + ASYNC_POSITION_PENDING_LIMIT + Duration::from_millis(1);

        let submission = AsyncPositionSubmission {
            x: position.0,
            y: position.1,
            first_submitted: submitted,
            size: None,
            wait_for_owner: false,
        };
        assert_eq!(
            async_position_state(submission, Some(position), recent),
            AsyncPositionState::Drained,
        );
        assert_eq!(
            async_position_state(submission, Some((100, 100)), recent),
            AsyncPositionState::Pending,
        );
        assert_eq!(
            async_position_state(submission, None, recent),
            AsyncPositionState::Pending,
        );
        assert_eq!(
            async_position_state(submission, Some((100, 100)), expired),
            AsyncPositionState::Expired,
        );
        assert_eq!(
            async_position_state(submission, None, expired),
            AsyncPositionState::Expired,
        );

        let updated_position = (300, 100);
        let updated = async_position_submission(
            Some(AsyncPositionSubmission {
                x: position.0,
                y: position.1,
                first_submitted: submitted,
                size: None,
                wait_for_owner: false,
            }),
            updated_position,
            recent,
        );
        assert_eq!((updated.x, updated.y), updated_position);
        assert_eq!(updated.first_submitted, submitted);
        assert_eq!(
            async_position_state(updated, Some(updated_position), recent),
            AsyncPositionState::Drained,
        );
    }

    #[test]
    fn queued_recovery_remains_unmeasured_until_drained_or_expired() {
        use std::time::{Duration, Instant};

        let window_id = 0x7FFF_FF31;
        let destination = (400, 100);
        let submission_time = Instant::now();
        let expired_submission_time =
            submission_time - ASYNC_POSITION_PENDING_LIMIT - Duration::from_millis(1);
        let placement = WindowPlacement {
            window_id,
            rect: Rect::new(destination.0, destination.1, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        };
        let placements = [placement];
        let measured = Rect::new(100, 100, 800, 600);
        let mut submissions = HashMap::from([(
            window_id,
            AsyncPositionSubmission {
                x: destination.0,
                y: destination.1,
                first_submitted: submission_time,
                size: None,
                wait_for_owner: false,
            },
        )]);

        let landings = collect_placement_landings_with_recoveries(
            &placements,
            LandingFailures {
                window_ids: &HashSet::from([window_id]),
                recovery_window_ids: &HashSet::new(),
            },
            &HashSet::new(),
            &HashSet::new(),
            &mut submissions,
            |_| Some((100, 100)),
            |_| (Some(measured), Some(measured)),
        );
        assert!(landings[0].unreadable);
        assert!(landings[0].measurement_deferred);
        assert!(!landings[0].failed);
        assert_eq!(landings[0].actual_visible_rect, None);
        assert_eq!(landings[0].actual_outer_rect, None);
        assert!(submissions.contains_key(&window_id));

        let landings = collect_placement_landings_with_recoveries(
            &placements,
            LandingFailures {
                window_ids: &HashSet::new(),
                recovery_window_ids: &HashSet::new(),
            },
            &HashSet::new(),
            &HashSet::new(),
            &mut submissions,
            |_| Some(destination),
            |_| (Some(measured), Some(measured)),
        );
        assert!(!submissions.contains_key(&window_id));
        assert!(!landings[0].unreadable);
        assert!(!landings[0].measurement_deferred);
        assert_eq!(landings[0].actual_visible_rect, Some(measured));
        assert_eq!(landings[0].actual_outer_rect, Some(measured));

        submissions.insert(
            window_id,
            AsyncPositionSubmission {
                x: destination.0,
                y: destination.1,
                first_submitted: expired_submission_time,
                size: None,
                wait_for_owner: false,
            },
        );
        let landings = collect_placement_landings_with_recoveries(
            &placements,
            LandingFailures {
                window_ids: &HashSet::new(),
                recovery_window_ids: &HashSet::new(),
            },
            &HashSet::new(),
            &HashSet::new(),
            &mut submissions,
            |_| Some((100, 100)),
            |_| (Some(measured), Some(measured)),
        );
        assert!(!submissions.contains_key(&window_id));
        assert!(!landings[0].unreadable);
        assert_eq!(landings[0].actual_visible_rect, Some(measured));
    }

    #[test]
    fn failed_parked_recovery_reports_unmeasured_failed_landing() {
        let _serialize = lock_cloak_set_tests();
        let window_id = 0x7FFF_FF32;
        let _membership = CloakMembershipGuard::claim(window_id);
        mark_placement_parked(window_id);
        let outcome =
            recover_placement_parked(window_id, SET_WINDOW_POS_FLAGS(0), false, |_| false);
        assert_eq!(outcome, ParkedRecoveryOutcome::Failed);
        assert!(is_placement_parked(window_id));

        let failed_recovery_window_ids = HashSet::from([window_id]);
        let failed_window_ids = HashSet::from([window_id]);
        let placements = [WindowPlacement {
            window_id,
            rect: Rect::new(400, 100, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        }];
        let landings = collect_placement_landings_with_recoveries(
            &placements,
            LandingFailures {
                window_ids: &failed_window_ids,
                recovery_window_ids: &failed_recovery_window_ids,
            },
            &HashSet::new(),
            &HashSet::new(),
            &mut HashMap::new(),
            |_| Some((i32::MIN, i32::MIN)),
            |_| panic!("failed parked recovery must not be measured"),
        );

        assert!(landings[0].failed);
        assert_eq!(landings[0].actual_visible_rect, None);
        assert_eq!(landings[0].actual_outer_rect, None);
    }

    #[test]
    fn recovery_adds_async_only_for_pending_synchronous_moves() {
        let synchronous = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
        assert_eq!(
            recovery_flags(SET_WINDOW_POS_FLAGS(0), true),
            synchronous | SWP_ASYNCWINDOWPOS,
        );
        assert_eq!(recovery_flags(SET_WINDOW_POS_FLAGS(0), false), synchronous);
        assert_eq!(
            recovery_flags(SWP_ASYNCWINDOWPOS, true),
            synchronous | SWP_ASYNCWINDOWPOS,
        );
    }

    #[test]
    fn test_landing_outlasts_queued_animation_positions() {
        assert_eq!(queued_landing_rect(false), (10, 100, 200, 200));
    }

    #[test]
    fn test_landing_stays_after_queued_frames_while_owner_waits_for_sent_messages() {
        let trajectory = queued_landing_while_owner_waits();
        let first_endpoint = trajectory
            .iter()
            .position(|x| *x == 10)
            .expect("landing endpoint was never applied; trajectory: {trajectory:?}");
        assert!(
            trajectory[first_endpoint..].iter().all(|x| *x == 10),
            "window departed from landing endpoint; trajectory: {trajectory:?}"
        );
        assert_eq!(trajectory.last(), Some(&10), "trajectory: {trajectory:?}");
    }

    #[test]
    fn test_synchronous_correction_after_landing() {
        assert_eq!(queued_landing_rect(true), (20, 100, 200, 200));
    }

    fn queued_landing_while_owner_waits() -> Vec<i32> {
        use std::sync::{mpsc, Condvar, Mutex, OnceLock};
        use std::time::{Duration, Instant};
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
            GetQueueStatus, GetWindowRect, PeekMessageW, PostThreadMessageW, RegisterClassW,
            SendMessageTimeoutW, MSG, PM_REMOVE, QS_ALLINPUT, SEND_MESSAGE_TIMEOUT_FLAGS,
            SMTO_ABORTIFHUNG, WM_APP, WM_WINDOWPOSCHANGED, WNDCLASSW, WS_POPUP,
        };

        static TRAJECTORY: Mutex<Vec<i32>> = Mutex::new(Vec::new());
        static HELPER_WAIT: OnceLock<(Mutex<(bool, bool)>, Condvar)> = OnceLock::new();
        const HELPER_ENTER: u32 = WM_APP + 41;
        let helper_wait = HELPER_WAIT.get_or_init(|| (Mutex::new((false, false)), Condvar::new()));
        *helper_wait.0.lock().unwrap() = (false, false);
        TRAJECTORY.lock().unwrap().clear();

        unsafe extern "system" fn owner_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            if message == WM_WINDOWPOSCHANGED {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    TRAJECTORY.lock().unwrap().push(rect.left);
                }
            }
            result
        }

        unsafe extern "system" fn helper_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == HELPER_ENTER {
                let (state, changed) = HELPER_WAIT.get().unwrap();
                let mut state = state.lock().unwrap();
                state.0 = true;
                changed.notify_all();
                let deadline = Instant::now() + Duration::from_secs(8);
                while !state.1 {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let (next, timeout) = changed.wait_timeout(state, remaining).unwrap();
                    state = next;
                    if timeout.timed_out() {
                        break;
                    }
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }

        fn register_test_class(
            name: windows::core::PCWSTR,
            proc: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
        ) {
            let class = WNDCLASSW {
                lpfnWndProc: proc,
                lpszClassName: name,
                ..Default::default()
            };
            assert_ne!(unsafe { RegisterClassW(&class) }, 0);
        }

        let (helper_tx, helper_rx) = mpsc::channel();
        let helper_thread = std::thread::spawn(move || unsafe {
            register_test_class(w!("LeopardWMLandingWaitHelper"), Some(helper_proc));
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("LeopardWMLandingWaitHelper"),
                None,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            helper_tx
                .send((hwnd.0 as usize, GetCurrentThreadId()))
                .unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).unwrap();
        });
        let (helper_hwnd, helper_thread_id) =
            helper_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        struct HelperReleaseGuard(u32);
        impl Drop for HelperReleaseGuard {
            fn drop(&mut self) {
                if let Some((state, changed)) = HELPER_WAIT.get() {
                    state.lock().unwrap().1 = true;
                    changed.notify_all();
                }
                unsafe {
                    let _ = PostThreadMessageW(self.0, 0x0012, WPARAM(0), LPARAM(0));
                }
            }
        }
        let _helper_release_guard = HelperReleaseGuard(helper_thread_id);
        let helper_hwnd_value = helper_hwnd;

        let (owner_tx, owner_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || unsafe {
            let helper_hwnd = HWND(helper_hwnd_value as *mut _);
            register_test_class(w!("LeopardWMLandingWaitOwner"), Some(owner_proc));
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("LeopardWMLandingWaitOwner"),
                None,
                WS_POPUP,
                100,
                100,
                200,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            owner_tx.send(hwnd.0 as usize as u64).unwrap();
            let mut result = 0usize;
            let _ = SendMessageTimeoutW(
                helper_hwnd,
                HELPER_ENTER,
                WPARAM(0),
                LPARAM(0),
                SEND_MESSAGE_TIMEOUT_FLAGS(SMTO_ABORTIFHUNG.0),
                8_000,
                Some(&mut result),
            );
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut settled_since = None;
            let mut rect = RECT::default();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "owner did not drain queued positions"
                );
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&message);
                }
                GetWindowRect(hwnd, &mut rect).unwrap();
                let queue_empty = GetQueueStatus(QS_ALLINPUT) >> 16 & QS_ALLINPUT.0 == 0;
                if rect.left == 10 && queue_empty {
                    let since = settled_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(50) {
                        break;
                    }
                } else {
                    settled_since = None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            DestroyWindow(hwnd).unwrap();
            rect.left
        });
        let window_id = owner_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let release = || {
            let mut state = helper_wait.0.lock().unwrap();
            state.1 = true;
            helper_wait.1.notify_all();
        };
        let result = std::panic::catch_unwind(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut state = helper_wait.0.lock().unwrap();
            while !state.0 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "helper did not enter its wait");
                let (next, timeout) = helper_wait.1.wait_timeout(state, remaining).unwrap();
                state = next;
                assert!(!timeout.timed_out(), "helper did not enter its wait");
            }
            drop(state);

            let position = |x, flags, landing| {
                let entry = DeferEntry {
                    hwnd: window_id_to_hwnd(window_id).unwrap(),
                    window_id,
                    x,
                    y: 100,
                    w: 200,
                    h: 200,
                    layout_w: 200,
                    layout_h: 200,
                    insets: (0, 0, 0, 0),
                    inset_source: InsetSource::Fresh,
                    inset_generation: 0,
                    visibility: Visibility::Visible,
                    flags,
                    column_index: 0,
                    offscreen_rect: None,
                };
                position_entries(&[entry], landing)
            };
            let flags = SWP_NOACTIVATE | SWP_NOZORDER;
            assert_eq!(
                position(400, flags | SWP_ASYNCWINDOWPOS, false),
                (1, HashSet::new())
            );
            assert_eq!(
                position(300, flags | SWP_ASYNCWINDOWPOS, false),
                (1, HashSet::new())
            );
            assert_eq!(position(10, flags, true), (1, HashSet::new()));
        });

        release();
        let _ = unsafe { PostThreadMessageW(helper_thread_id, 0x0012, WPARAM(0), LPARAM(0)) };
        let owner_result = owner.join();
        let helper_result = helper_thread.join();
        if let Err(panic) = result {
            let _ = owner_result;
            let _ = helper_result;
            std::panic::resume_unwind(panic);
        }
        assert_eq!(owner_result.unwrap(), 10);
        helper_result.unwrap();
        TRAJECTORY.lock().unwrap().clone()
    }

    fn queued_landing_rect(correct_after_landing: bool) -> (i32, i32, i32, i32) {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetQueueStatus, PeekMessageW,
            PostThreadMessageW, MSG, PM_REMOVE, QS_SENDMESSAGE, WINDOW_EX_STYLE, WM_APP, WS_POPUP,
        };

        let (ready_tx, ready_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("LeopardWM hidden placement test"),
                WS_POPUP,
                100,
                100,
                200,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            struct OwnedWindow(HWND);
            impl Drop for OwnedWindow {
                fn drop(&mut self) {
                    let _ = unsafe { DestroyWindow(self.0) };
                }
            }
            let window = OwnedWindow(hwnd);
            ready_tx
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            // Hold posted animation moves until the synchronous landing is waiting.
            while GetQueueStatus(QS_SENDMESSAGE) >> 16 & QS_SENDMESSAGE.0 == 0 {
                assert!(Instant::now() < deadline, "landing never reached the owner");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut message = MSG::default();
            loop {
                assert!(Instant::now() < deadline, "placement queue did not finish");
                if PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_APP {
                        // Windows may apply queued async moves after a later posted message,
                        // so measure only once the window stops moving.
                        let mut rect = RECT::default();
                        GetWindowRect(hwnd, &mut rect).unwrap();
                        let mut stable_since = Instant::now();
                        while stable_since.elapsed() < Duration::from_millis(50) {
                            assert!(Instant::now() < deadline, "placement queue did not settle");
                            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                                DispatchMessageW(&message);
                            }
                            let mut current = RECT::default();
                            GetWindowRect(hwnd, &mut current).unwrap();
                            if current != rect {
                                rect = current;
                                stable_since = Instant::now();
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        DestroyWindow(window.0).unwrap();
                        std::mem::forget(window);
                        return (
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                        );
                    }
                    DispatchMessageW(&message);
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        let (window_id, thread_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let position = |x, flags, landing| {
            std::thread::spawn(move || {
                let entry = DeferEntry {
                    hwnd: window_id_to_hwnd(window_id).unwrap(),
                    window_id,
                    x,
                    y: 100,
                    w: 200,
                    h: 200,
                    layout_w: 200,
                    layout_h: 200,
                    insets: (0, 0, 0, 0),
                    inset_source: InsetSource::Fresh,
                    inset_generation: 0,
                    visibility: Visibility::Visible,
                    flags,
                    column_index: 0,
                    offscreen_rect: None,
                };
                position_entries(&[entry], landing)
            })
            .join()
            .unwrap()
        };
        let flags = SWP_NOACTIVATE | SWP_NOZORDER;
        let intermediate = position(400, flags | SWP_ASYNCWINDOWPOS, false);
        let landing = position(10, flags, true);
        // A later synchronous correction remains authoritative after the landing queue drains.
        let correction = correct_after_landing.then(|| position(20, flags, false));
        unsafe { PostThreadMessageW(thread_id, WM_APP, WPARAM(0), LPARAM(0)).unwrap() };
        let observed = owner.join().unwrap();
        for result in [intermediate, landing].into_iter().chain(correction) {
            assert_eq!(result, (1, HashSet::new()));
        }
        observed
    }

    #[test]
    fn test_visible_and_frame_rects_round_trip_with_insets() {
        let visible = Rect::new(120, 100, 1000, 700);
        let insets = (7, 1, 7, 8);
        let frame = visible_rect_to_frame_rect(visible, insets, false);
        assert_eq!(frame, Rect::new(113, 99, 1014, 709));
        assert_eq!(frame_rect_to_visible_rect(frame, insets, false), visible);
    }

    #[test]
    fn test_offscreen_sentinel_origin_ignores_frame_insets() {
        let sentinel = crate::MOVE_OFFSCREEN_SENTINEL_COORD;
        let monitors = [Rect::new(0, 0, 5120, 1440), Rect::new(5120, 0, 800, 600)];
        for (x, y) in [(sentinel, sentinel), (-100_000, -100_000)] {
            for visibility in [Visibility::OffScreenLeft, Visibility::OffScreenRight] {
                let park = Rect::new(x, y, 2545, 1372);
                let placement = offscreen_placement(1, park, visibility);
                for (insets, high_contrast) in [
                    ((7, 0, 7, 7), false),
                    ((0, 0, 0, 0), false),
                    ((7, 1, 7, 8), true),
                ] {
                    let frame = visible_rect_to_frame_rect(park, insets, high_contrast);
                    assert_eq!(
                        offscreen_frame_origin(&placement, frame, (2559, 1379), &monitors),
                        (x, y),
                        "sentinel parking already supplies an outer-frame origin"
                    );
                }
            }
        }
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
            frame_insets_from_rects(Rect::new(100, 100, 914, 709), Rect::new(107, 101, 900, 700),),
            Some((7, 1, 7, 8))
        );
    }

    #[test]
    fn test_frame_insets_reject_stale_or_misaligned_samples() {
        let frame = Rect::new(100, 100, 914, 709);
        for visible in [Rect::new(117, 101, 900, 700), Rect::new(93, 101, 900, 700)] {
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
    fn test_frame_insets_enforce_maximum_delta() {
        let maximum = MAX_INVISIBLE_FRAME_INSET as i32;
        let visible = Rect::new(100 + maximum, 100 + maximum, 800, 700);
        let frame = Rect::new(100, 100, 800 + maximum * 2, 700 + maximum * 2);
        assert_eq!(
            frame_insets_from_rects(frame, visible),
            Some((maximum, maximum, maximum, maximum))
        );

        let above_maximum = maximum + 1;
        let visible = Rect::new(100 + above_maximum, 100 + above_maximum, 800, 700);
        let frame = Rect::new(100, 100, 800 + above_maximum * 2, 700 + above_maximum * 2);
        assert_eq!(frame_insets_from_rects(frame, visible), None);
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
                    record: false,
                    suspect: true,
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
                    record: false,
                    suspect: true,
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
                    record: false,
                    suspect: true,
                },
            ),
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
                classify_size_axis(
                    layout,
                    frame,
                    visible,
                    allow_retry,
                    was_suspected.then_some(visible),
                ),
                expected,
                "layout={layout}, frame={frame}, visible={visible}, allow_retry={allow_retry}"
            );
        }
    }

    fn width_measurement(
        window_id: WindowId,
        layout_w: i32,
        frame_w: i32,
        visible_w: i32,
    ) -> WindowSizeMeasurement {
        WindowSizeMeasurement {
            hwnd: HWND::default(),
            window_id,
            layout_w,
            layout_h: 400,
            frame_w,
            frame_h: 400,
            visible_w,
            visible_h: 400,
            used_insets: (0, 0, 0, 0),
            landed_insets: None,
        }
    }

    #[test]
    fn undersize_with_changed_landed_insets_retries_only_on_first_pass() {
        let measurement = WindowSizeMeasurement {
            layout_h: 1334,
            visible_w: 1253,
            visible_h: 1327,
            landed_insets: Some((7, 0, 7, 7)),
            ..width_measurement(21, 1267, 1267, 1253)
        };
        let mut suspects = HashMap::new();
        let detection =
            detect_size_violations_from_measurements(&[measurement], true, &mut suspects);
        assert_eq!(detection.inset_artifact_windows, HashSet::from([21]));

        let unchanged = WindowSizeMeasurement {
            landed_insets: Some(measurement.used_insets),
            ..measurement
        };
        let detection =
            detect_size_violations_from_measurements(&[unchanged], true, &mut HashMap::new());
        assert!(detection.inset_artifact_windows.is_empty());

        let detection =
            detect_size_violations_from_measurements(&[measurement], false, &mut HashMap::new());
        assert!(detection.inset_artifact_windows.is_empty());

        let unreadable = WindowSizeMeasurement {
            landed_insets: None,
            ..measurement
        };
        let detection =
            detect_size_violations_from_measurements(&[unreadable], true, &mut HashMap::new());
        assert!(detection.inset_artifact_windows.is_empty());
    }

    #[test]
    fn test_landing_confirms_small_oversize_before_recording_minimums() {
        let width = width_measurement(7, 2545, 2559, 3789);
        let height = WindowSizeMeasurement {
            layout_h: 681,
            frame_h: 688,
            visible_h: 880,
            ..width_measurement(8, 2840, 2854, 2840)
        };
        for measurement in [width, height] {
            for fits_after_retry in [true, false] {
                let mut suspects = HashMap::new();
                let first =
                    detect_size_violations_from_measurements(&[measurement], true, &mut suspects);
                assert!(first.width_violations.is_empty());
                assert!(first.height_violations.is_empty());
                assert!(should_retry_landing_measurement(true, &first));

                let confirmed = if fits_after_retry {
                    WindowSizeMeasurement {
                        visible_w: measurement.layout_w,
                        visible_h: measurement.layout_h,
                        ..measurement
                    }
                } else {
                    measurement
                };
                let second =
                    detect_size_violations_from_measurements(&[confirmed], false, &mut suspects);
                if fits_after_retry {
                    assert!(second.width_violations.is_empty());
                    assert!(second.height_violations.is_empty());
                } else if measurement.window_id == width.window_id {
                    assert_eq!(second.width_violations.len(), 1);
                    assert_eq!(second.width_violations[0].min_width, 3789);
                    assert!(second.height_violations.is_empty());
                } else {
                    assert!(second.width_violations.is_empty());
                    assert_eq!(second.height_violations.len(), 1);
                    assert_eq!(second.height_violations[0].min_height, 880);
                }
                assert!(suspects.is_empty());
                assert!(!should_retry_landing_measurement(false, &second));
            }
        }
    }

    #[test]
    fn test_changing_oversize_is_not_a_confirmed_minimum() {
        let first_measurement = WindowSizeMeasurement {
            layout_h: 681,
            frame_h: 688,
            visible_h: 821,
            ..width_measurement(7, 2545, 2559, 3789)
        };
        let second_measurement = WindowSizeMeasurement {
            visible_w: 2978,
            visible_h: 740,
            ..first_measurement
        };
        let mut suspects = HashMap::new();
        detect_size_violations_from_measurements(&[first_measurement], true, &mut suspects);
        let second =
            detect_size_violations_from_measurements(&[second_measurement], false, &mut suspects);
        assert!(second.width_violations.is_empty());
        assert!(second.height_violations.is_empty());
        assert!(!should_retry_landing_measurement(false, &second));

        let mut settled_suspects = suspects.clone();
        let settled_measurement = WindowSizeMeasurement {
            visible_w: first_measurement.layout_w,
            visible_h: first_measurement.layout_h,
            ..first_measurement
        };
        let settled = detect_size_violations_from_measurements(
            &[settled_measurement],
            true,
            &mut settled_suspects,
        );
        assert!(settled.width_violations.is_empty());
        assert!(settled.height_violations.is_empty());
        assert!(!should_retry_landing_measurement(true, &settled));
        assert!(settled_suspects.is_empty());

        detect_size_violations_from_measurements(&[second_measurement], true, &mut suspects);
        let stable =
            detect_size_violations_from_measurements(&[second_measurement], false, &mut suspects);
        assert_eq!(stable.width_violations[0].min_width, 2978);
        assert_eq!(stable.height_violations[0].min_height, 740);
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_fitting_sample_clears_suspects_even_when_another_axis_retries() {
        let oversize = WindowSizeMeasurement {
            visible_h: 700,
            ..width_measurement(7, 400, 414, 700)
        };
        for fits_height in [false, true] {
            let mut suspects = HashMap::new();
            detect_size_violations_from_measurements(&[oversize], true, &mut suspects);
            let fitting = WindowSizeMeasurement {
                visible_w: 400,
                visible_h: if fits_height { 400 } else { 700 },
                ..oversize
            };
            let peer = width_measurement(8, 400, 414, 500);
            let first =
                detect_size_violations_from_measurements(&[fitting, peer], true, &mut suspects);
            assert!(should_retry_landing_measurement(true, &first));
            let second =
                detect_size_violations_from_measurements(&[oversize], false, &mut suspects);
            assert!(second.width_violations.is_empty());
            assert_eq!(second.height_violations.is_empty(), fits_height);
            assert!(!should_retry_landing_measurement(false, &second));
        }
    }

    #[test]
    fn test_new_retry_oversize_requires_its_own_confirmation() {
        for first_width in [400, 403] {
            let mut suspects = HashMap::new();
            let first = width_measurement(7, 400, 414, first_width);
            let peer = width_measurement(8, 400, 414, 500);
            detect_size_violations_from_measurements(&[first, peer], true, &mut suspects);
            let oversize = width_measurement(7, 400, 414, 450);
            let second =
                detect_size_violations_from_measurements(&[oversize], false, &mut suspects);
            assert!(second.width_violations.is_empty());
            assert!(!should_retry_landing_measurement(false, &second));
            detect_size_violations_from_measurements(&[oversize], true, &mut suspects);
            let stable =
                detect_size_violations_from_measurements(&[oversize], false, &mut suspects);
            assert_eq!(stable.width_violations[0].min_width, 450);
        }
    }

    #[test]
    fn test_unmeasured_retry_requires_fresh_landing_confirmation() {
        let measurement = WindowSizeMeasurement {
            layout_h: 681,
            frame_h: 688,
            visible_h: 880,
            ..width_measurement(7, 2545, 2559, 3789)
        };
        let mut suspects = HashMap::new();
        let first = detect_size_violations_from_measurements(&[measurement], true, &mut suspects);
        assert!(should_retry_landing_measurement(true, &first));
        detect_size_violations_from_measurements(&[], false, &mut suspects);

        let next = detect_size_violations_from_measurements(&[measurement], true, &mut suspects);
        assert!(next.width_violations.is_empty());
        assert!(next.height_violations.is_empty());
        assert!(should_retry_landing_measurement(true, &next));
        let fitting = WindowSizeMeasurement {
            visible_w: measurement.layout_w,
            visible_h: measurement.layout_h,
            ..measurement
        };
        let confirmed = detect_size_violations_from_measurements(&[fitting], false, &mut suspects);
        assert!(confirmed.width_violations.is_empty());
        assert!(confirmed.height_violations.is_empty());
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_suspect_oversize_retries_and_confirms_stable_native_minimum() {
        let measurement = width_measurement(7, 1267, 1281, 3186);
        let mut suspects = HashMap::new();

        let first = detect_size_violations_from_measurements(&[measurement], true, &mut suspects);
        assert_eq!(first.suspect_confirmation_windows, HashSet::from([7]));
        assert!(should_retry_landing_measurement(true, &first));
        assert_eq!(suspects.get(&7), Some(&(Some(measurement.visible_w), None)));

        let second = detect_size_violations_from_measurements(&[measurement], false, &mut suspects);
        assert_eq!(second.width_violations.len(), 1);
        assert_eq!(second.width_violations[0].min_width, 3186);
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_suspect_oversize_drops_one_off_stale_second_measurement() {
        let first_measurement = width_measurement(7, 1267, 1281, 3186);
        let second_measurement = width_measurement(7, 1267, 1281, 1267);
        let mut suspects = HashMap::new();

        let first =
            detect_size_violations_from_measurements(&[first_measurement], true, &mut suspects);
        assert!(should_retry_landing_measurement(true, &first));

        let second =
            detect_size_violations_from_measurements(&[second_measurement], false, &mut suspects);
        assert!(second.width_violations.is_empty());
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_suspect_oversize_rejects_absurd_second_measurement() {
        let first_measurement = width_measurement(7, 1267, 1281, 3186);
        let second_measurement = width_measurement(7, 1267, 1281, 5069);
        let mut suspects = HashMap::new();

        detect_size_violations_from_measurements(&[first_measurement], true, &mut suspects);
        let second =
            detect_size_violations_from_measurements(&[second_measurement], false, &mut suspects);
        assert!(second.width_violations.is_empty());
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_mixed_measurements_share_one_retry_and_discard_first_pass_violations() {
        let mut height_violation = width_measurement(4, 400, 400, 400);
        height_violation.visible_h = 500;
        let measurements = [
            width_measurement(1, 400, 414, 403),
            width_measurement(2, 1267, 1281, 3186),
            width_measurement(3, 400, 400, 500),
            height_violation,
        ];
        let mut suspects = HashMap::new();

        let first = detect_size_violations_from_measurements(&measurements, true, &mut suspects);
        assert_eq!(first.inset_artifact_windows, HashSet::from([1]));
        assert_eq!(first.suspect_confirmation_windows, HashSet::from([2, 3, 4]));
        assert!(first.width_violations.is_empty());
        assert!(first.height_violations.is_empty());
        assert!(should_retry_landing_measurement(true, &first));

        let second = detect_size_violations_from_measurements(&measurements, false, &mut suspects);
        assert_eq!(second.width_violations.len(), 3);
        assert_eq!(
            second
                .width_violations
                .iter()
                .map(|violation| violation.window_id)
                .collect::<HashSet<_>>(),
            HashSet::from([1, 2, 3])
        );
        assert_eq!(second.height_violations.len(), 1);
        assert_eq!(second.height_violations[0].window_id, 4);
        assert_eq!(second.height_violations[0].min_height, 500);
        assert!(suspects.is_empty());
        assert!(!should_retry_landing_measurement(false, &second));
    }

    #[test]
    fn test_discarded_retry_preserves_existing_suspect_for_authoritative_confirmation() {
        let a = width_measurement(1, 1267, 1281, 3186);
        let b = width_measurement(2, 400, 414, 403);
        let mut suspects = HashMap::from([(1, (Some(a.visible_w), None))]);

        let first = detect_size_violations_from_measurements(&[a, b], true, &mut suspects);
        assert_eq!(first.inset_artifact_windows, HashSet::from([2]));
        assert!(first.width_violations.is_empty());
        assert_eq!(first.suspect_confirmation_windows, HashSet::from([1]));
        assert!(should_retry_landing_measurement(true, &first));
        assert_eq!(suspects.get(&1), Some(&(Some(a.visible_w), None)));

        let second = detect_size_violations_from_measurements(&[a, b], false, &mut suspects);
        assert!(second
            .width_violations
            .iter()
            .any(|violation| violation.window_id == 1 && violation.min_width == 3186));
        assert!(!should_retry_landing_measurement(false, &second));
        assert!(suspects.is_empty());
    }

    #[test]
    fn test_retry_disabled_suspect_is_saved_without_recursing() {
        let measurement = width_measurement(7, 1267, 1281, 3186);
        let mut suspects = HashMap::new();

        let detection =
            detect_size_violations_from_measurements(&[measurement], false, &mut suspects);
        assert_eq!(detection.suspect_confirmation_windows, HashSet::from([7]));
        assert_eq!(suspects.get(&7), Some(&(Some(measurement.visible_w), None)));
        assert!(!should_retry_landing_measurement(false, &detection));
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
            offscreen_rect: None,
        }
    }

    #[test]
    fn endpoint_failure_survives_successful_batch_or_fallback() {
        let entries = [fresh_inset_entry(10, 0), fresh_inset_entry(20, 0)];
        for batch_failures in [HashSet::new(), HashSet::from([20]), HashSet::from([10])] {
            let mut queued = Vec::new();
            let (applied, failed) = position_entries_with(
                &entries,
                true,
                |entry| {
                    queued.push(entry.window_id);
                    if entry.window_id == 10 {
                        Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                            -1,
                        )))
                    } else {
                        Ok(())
                    }
                },
                |_| batch_failures.clone(),
            );
            assert_eq!(queued, [10, 20]);
            let mut expected = batch_failures;
            expected.insert(10);
            assert_eq!(applied, (2 - expected.len()) as u32);
            assert_eq!(failed, expected);
        }
    }

    #[test]
    fn ordered_retry_skips_queued_endpoints_and_retries_failures() {
        let entries = [fresh_inset_entry(10, 0), fresh_inset_entry(20, 0)];
        let mut queued_endpoints = HashMap::new();
        let mut queued = Vec::new();
        let (applied, failed) = position_entries_with_pending(
            &entries,
            true,
            &HashSet::new(),
            &mut queued_endpoints,
            |entry| {
                queued.push(entry.window_id);
                if entry.window_id == 10 {
                    Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        -1,
                    )))
                } else {
                    Ok(())
                }
            },
            |_| HashSet::new(),
        );
        assert_eq!(queued, [10, 20]);
        assert_eq!(applied, 1);
        assert_eq!(failed, HashSet::from([10]));

        let (applied, failed) = position_entries_with_pending(
            &entries,
            true,
            &HashSet::new(),
            &mut queued_endpoints,
            |entry| {
                queued.push(entry.window_id);
                Ok(())
            },
            |_| HashSet::new(),
        );
        assert_eq!(queued, [10, 20, 10]);
        assert_eq!(applied, 2);
        assert!(failed.is_empty());

        let mut changed_entry = entries[1].clone();
        changed_entry.x += 1;
        let changed_entries = [changed_entry];
        let (applied, failed) = position_entries_with_pending(
            &changed_entries,
            true,
            &HashSet::new(),
            &mut queued_endpoints,
            |entry| {
                queued.push(entry.window_id);
                Ok(())
            },
            |_| HashSet::new(),
        );
        assert_eq!(queued, [10, 20, 10, 20]);
        assert_eq!(applied, 1);
        assert!(failed.is_empty());

        let (applied, failed) = position_entries_with(
            &entries,
            false,
            |_| panic!("ordinary positioning must not queue an endpoint"),
            |_| HashSet::new(),
        );
        assert_eq!(applied, 2);
        assert!(failed.is_empty());
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

    fn seed_global_insets(window_id: WindowId, insets: (i32, i32, i32, i32)) {
        if let Ok(mut global) = GLOBAL_INSET_CACHE.lock() {
            global
                .get_or_insert_with(HashMap::new)
                .insert(window_id, insets);
        }
    }

    struct ForgetGlobalInsetsOnDrop(WindowId);

    impl Drop for ForgetGlobalInsetsOnDrop {
        fn drop(&mut self) {
            forget_global_insets(self.0);
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
        let mut suspects = HashMap::from([(8, (Some(820), None))]);
        let measurement = WindowSizeMeasurement {
            hwnd: HWND::default(),
            window_id: 7,
            layout_w: 400,
            layout_h: 400,
            frame_w: 400,
            frame_h: 400,
            visible_w: 700,
            visible_h: 400,
            used_insets: (0, 0, 0, 0),
            landed_insets: None,
        };
        let classified =
            classify_measurements_and_update_suspects(&[measurement], true, &mut suspects);
        assert_eq!(
            classified[0].width,
            AxisSizeClassification::Violation {
                record: false,
                suspect: true,
            }
        );
        assert_eq!(suspects.get(&7), Some(&(Some(measurement.visible_w), None)));
        assert_eq!(suspects.get(&8), Some(&(Some(820), None)));
    }

    struct CloakMembershipGuard {
        window_id: WindowId,
        had_global: bool,
        had_ghost: bool,
        had_direct: bool,
    }

    impl CloakMembershipGuard {
        fn claim(window_id: WindowId) -> Self {
            let this = Self {
                window_id,
                had_global: global_cloaked_contains(window_id),
                had_ghost: ghost_cloaked_contains(window_id),
                had_direct: lock_direct_cloaked()
                    .as_ref()
                    .is_some_and(|set| set.contains(&window_id)),
            };
            restore_cloak_membership(window_id, false, lock_cloaked);
            restore_cloak_membership(window_id, false, lock_ghost_cloaked);
            restore_cloak_membership(window_id, false, lock_direct_cloaked);
            this
        }
    }

    impl Drop for CloakMembershipGuard {
        fn drop(&mut self) {
            restore_cloak_membership(self.window_id, self.had_global, lock_cloaked);
            restore_cloak_membership(self.window_id, self.had_ghost, lock_ghost_cloaked);
            restore_cloak_membership(self.window_id, self.had_direct, lock_direct_cloaked);
        }
    }

    fn restore_cloak_membership(
        window_id: WindowId,
        present: bool,
        lock: impl FnOnce() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>>,
    ) {
        let mut guard = lock();
        if present {
            guard.get_or_insert_with(HashSet::new).insert(window_id);
        } else if let Some(set) = guard.as_mut() {
            set.remove(&window_id);
        }
    }

    #[test]
    fn test_direct_cloak_is_tracked_for_recovery() {
        // A directly-cloaked window (e.g. a stashed scratchpad) must be
        // tracked in DIRECT_CLOAKED so shutdown/panic recovery can restore
        // it; otherwise it would be permanently invisible. Uses a unique
        // wid so it won't collide with parallel tests touching the set.
        let _serialize = lock_cloak_set_tests();
        let wid: WindowId = 0x7FFF_FF01;
        let _membership = CloakMembershipGuard::claim(wid);
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
    fn dwm_uncloak_window_clears_placement_park_and_ghost_cloak() {
        let _serialize = lock_cloak_set_tests();
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF22;
        let _membership = CloakMembershipGuard::claim(wid);
        mark_placement_parked(wid);
        mark_ghost_cloaked(wid);
        assert!(
            is_placement_parked(wid),
            "mark_placement_parked must record GLOBAL_CLOAKED ownership"
        );
        assert!(
            is_placement_cloaked(wid),
            "ghost cloak must make the window placement-cloaked"
        );
        dwm_uncloak_window(wid);
        assert!(
            !is_placement_parked(wid),
            "dwm_uncloak_window must clear placement park ownership"
        );
        assert!(
            !is_placement_cloaked(wid),
            "dwm_uncloak_window must clear ghost cloak"
        );
    }

    #[test]
    fn test_apply_placements_empty() {
        let _serialize = lock_cloak_set_tests();
        // Verify empty placements succeed without error
        let config = PlatformConfig::default();
        let result = apply_placements(&[], &config, None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apply_placements_skips_invalid_windows() {
        let _serialize = lock_cloak_set_tests();
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
        let _serialize = lock_cloak_set_tests();
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF00;
        let _membership = CloakMembershipGuard::claim(wid);

        // Case 1: neither set → false.
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
            if let Some(ref mut s) = *g {
                s.remove(&wid);
            }
        }
        assert!(is_placement_cloaked(wid), "ghost only should give true");

        // Case 5: neither → false again.
        unmark_ghost_cloaked(wid);
        assert!(
            !is_placement_cloaked(wid),
            "neither again should give false"
        );
    }

    #[test]
    fn test_empty_apply_and_managed_uncloak_clear_foreign_parked_cloaks() {
        let _serialize = lock_cloak_set_tests();
        let parked: WindowId = 0xFFFF_FFFF_FFFF_FF13;
        let direct: WindowId = 0xFFFF_FFFF_FFFF_FF14;
        let _parked_membership = CloakMembershipGuard::claim(parked);
        let _direct_membership = CloakMembershipGuard::claim(direct);
        let visible_tiled = WindowPlacement {
            window_id: parked,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        };
        let mut cache = PlacementCache::new();
        cache
            .positions
            .insert(parked, (visible_tiled.rect, Visibility::Visible));

        mark_placement_parked(parked);
        apply_placements(&[], &PlatformConfig::default(), None, false)
            .expect("empty apply should succeed");
        let mut async_recovery = false;
        let mut failed_recovery = false;
        assert!(skip_visible_tiled_maximized(
            &visible_tiled,
            true,
            Some(&mut cache),
            SET_WINDOW_POS_FLAGS(0),
            false,
            &mut async_recovery,
            &mut failed_recovery,
        ));
        assert!(
            !is_placement_parked(parked),
            "empty apply_placements drains GLOBAL_CLOAKED, so invalid maximized recovery cannot retain placement ownership"
        );

        mark_placement_parked(parked);
        mark_ghost_cloaked(parked);
        dwm_cloak_window(direct);
        crate::uncloak_all_managed_windows(&[]);
        assert!(
            !is_placement_cloaked(parked),
            "uncloak_all_managed_windows drains GLOBAL and GHOST via dwm_uncloak_all"
        );
        assert!(
            !lock_direct_cloaked()
                .as_ref()
                .is_some_and(|set| set.contains(&direct)),
            "uncloak_all_managed_windows drains DIRECT_CLOAKED via dwm_uncloak_all"
        );
    }

    /// Regression: a visible tiled placement skipped because the HWND is
    /// maximized retains placement ownership if position-only recovery fails,
    /// then releases exactly that ownership after a successful recovery.
    #[test]
    fn test_skip_visible_tiled_maximized_recovers_only_after_position_succeeds() {
        let _serialize = lock_cloak_set_tests();
        let wid: WindowId = 0xFFFF_FFFF_FFFF_FF10;
        let _membership = CloakMembershipGuard::claim(wid);
        let visible_tiled = WindowPlacement {
            window_id: wid,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        };
        let landing_flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;

        let mut cache = PlacementCache::new();
        let mut async_recovery = false;
        let mut failed_recovery = false;
        assert!(!skip_visible_tiled_maximized(
            &visible_tiled,
            false,
            Some(&mut cache),
            SET_WINDOW_POS_FLAGS(0),
            false,
            &mut async_recovery,
            &mut failed_recovery,
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
            false,
            &mut async_recovery,
            &mut failed_recovery,
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
            false,
            &mut async_recovery,
            &mut failed_recovery,
        ));
        assert!(
            is_placement_parked(wid) && is_placement_cloaked(wid),
            "invalid recovery must retain placement ownership and its effective cloak"
        );
        assert!(
            !cache.positions.contains_key(&wid),
            "failed recovery may invalidate the position cache"
        );
        assert_eq!(
            recover_placement_parked(wid, SET_WINDOW_POS_FLAGS(0), false, |_| false),
            ParkedRecoveryOutcome::Failed,
        );
        assert!(
            is_placement_parked(wid) && is_placement_cloaked(wid),
            "failed positioning must preserve placement ownership and its effective cloak"
        );

        assert_eq!(
            recover_placement_parked(wid, SET_WINDOW_POS_FLAGS(0), false, |flags| flags
                == landing_flags),
            ParkedRecoveryOutcome::Succeeded,
        );
        assert!(
            !is_placement_parked(wid),
            "successful landing recovery releases placement ownership exactly once"
        );
        assert_eq!(
            recover_placement_parked(wid, SET_WINDOW_POS_FLAGS(0), false, |_| true),
            ParkedRecoveryOutcome::NotAttempted,
        );

        mark_placement_parked(wid);
        mark_ghost_cloaked(wid);
        assert_eq!(
            recover_placement_parked(wid, SWP_ASYNCWINDOWPOS, false, |flags| {
                flags == (landing_flags | SWP_ASYNCWINDOWPOS)
            },),
            ParkedRecoveryOutcome::Succeeded,
        );
        assert!(!is_placement_parked(wid));
        assert!(
            is_placement_cloaked(wid),
            "successful recovery must retain a ghost-owned effective cloak"
        );
        unmark_ghost_cloaked(wid);
    }

    struct HiddenPopup {
        hwnd: HWND,
        id: WindowId,
    }

    impl HiddenPopup {
        fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
            };
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("LeopardWM hidden tab park test"),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            Self {
                hwnd,
                id: hwnd.0 as usize as u64,
            }
        }

        fn new_framed(x: i32, y: i32, w: i32, h: i32) -> Self {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_OVERLAPPEDWINDOW,
            };
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("LeopardWM framed landing test"),
                    WS_OVERLAPPEDWINDOW,
                    x,
                    y,
                    w,
                    h,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            Self {
                hwnd,
                id: hwnd.0 as usize as u64,
            }
        }

        fn rect(&self) -> Rect {
            let mut rect = RECT::default();
            unsafe { GetWindowRect(self.hwnd, &mut rect).unwrap() };
            Rect::new(
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            )
        }
    }

    impl Drop for HiddenPopup {
        fn drop(&mut self) {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }

    fn offscreen_placement(
        window_id: WindowId,
        rect: Rect,
        visibility: Visibility,
    ) -> WindowPlacement {
        WindowPlacement {
            window_id,
            rect,
            visibility,
            column_index: 0,
        }
    }

    fn defer_origin(
        placement: WindowPlacement,
        insets: (i32, i32, i32, i32),
        high_contrast: bool,
    ) -> (i32, i32, i32, i32) {
        let mut cache = PlacementCache::new();
        cache.insets.insert(placement.window_id, insets);
        let mut cache_opt = Some(&mut cache);
        let (entries, skipped, _, _, _) = build_defer_entries(
            &[placement],
            &mut cache_opt,
            SET_WINDOW_POS_FLAGS(0),
            high_contrast,
            true,
            &OwnerDeferrals::default(),
            &[Rect::new(0, 0, 800, 600)],
        );
        assert_eq!(skipped, 0);
        let entry = entries
            .first()
            .expect("hidden HWND fixture must produce a defer entry");
        (entry.x, entry.y, entry.w, entry.h)
    }

    #[test]
    fn test_zero_size_hidden_tab_origin_ignores_inset_cache() {
        let window = HiddenPopup::new(100, 100, 684, 408);
        let logical = Rect::new(-5120, 0, 0, 0);
        let placement = offscreen_placement(window.id, logical, Visibility::OffScreenLeft);
        let warm = defer_origin(placement.clone(), (7, 1, 7, 8), false);
        let cold = defer_origin(placement.clone(), (0, 0, 0, 0), false);
        let high_contrast = defer_origin(placement, (7, 1, 7, 8), true);
        assert_eq!(warm, (logical.x, logical.y, 0, 0));
        assert_eq!(cold, warm);
        assert_eq!(high_contrast, warm);
    }

    #[test]
    fn test_nonzero_offscreen_origin_still_applies_insets() {
        let window = HiddenPopup::new(120, 80, 400, 300);
        let logical = Rect::new(-5120, 40, 1600, 900);
        let placement = offscreen_placement(window.id, logical, Visibility::OffScreenLeft);
        let insets = (7, 1, 7, 8);
        let converted = visible_rect_to_frame_rect(logical, insets, false);
        assert_eq!(
            defer_origin(placement.clone(), insets, false),
            (converted.x, converted.y, converted.width, 0)
        );
        assert_eq!(
            defer_origin(placement, insets, true),
            (logical.x, logical.y, logical.width, 0)
        );
    }

    #[test]
    fn test_offscreen_frame_origin_avoids_new_monitor_overlap() {
        let slot = Rect::new(-300, 0, 300, 560);
        let placement = offscreen_placement(1, slot, Visibility::OffScreenLeft);
        let owner = Rect::new(0, 0, 800, 560);
        let neighbor = Rect::new(-1110, 0, 800, 560);
        let origin = offscreen_frame_origin(&placement, slot, (315, 560), &[owner, neighbor]);
        let footprint = Rect::new(origin.0, origin.1, 315, 560);
        assert!(!footprint.intersects(&owner) && !footprint.intersects(&neighbor));
        assert_eq!(
            offscreen_frame_origin(&placement, slot, (315, 560), &[owner]),
            (-315, 0)
        );
        let overlapping_neighbor = Rect::new(-1920, 0, 1920, 560);
        assert_eq!(
            offscreen_frame_origin(&placement, slot, (315, 560), &[owner, overlapping_neighbor]),
            (-315, 0),
            "a monitor already overlapped by the slot must not trigger parking"
        );
        let bordered_frame = Rect::new(-307, -1, 314, 569);
        assert_eq!(
            offscreen_frame_origin(&placement, bordered_frame, (330, 569), &[owner]),
            (-323, -1),
            "invisible borders already overlapping the owner are not new monitor overlap"
        );
        let slot = Rect::new(-300, 10, 300, 560);
        let frame = Rect::new(-307, 9, 314, 569);
        let monitors = [Rect::new(-1920, 0, 1920, 1080)];
        for (visibility, size) in [
            (Visibility::OffScreenLeft, (300, 600)),
            (Visibility::OffScreenLeft, (314, 600)),
            (Visibility::OffScreenRight, (500, 600)),
            (Visibility::Visible, (500, 600)),
        ] {
            let placement = offscreen_placement(1, slot, visibility);
            assert_eq!(
                offscreen_frame_origin(&placement, frame, size, &monitors),
                (-307, 9)
            );
        }
        let placeholder =
            offscreen_placement(1, Rect::new(-800, 0, 0, 0), Visibility::OffScreenLeft);
        assert_eq!(
            offscreen_frame_origin(
                &placeholder,
                Rect::new(-807, -1, 14, 9),
                (500, 600),
                &monitors
            ),
            (-800, 0)
        );
    }

    #[test]
    fn test_offscreen_record_uses_cached_insets_and_retained_frame_size() {
        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _cloak = lock_cloak_set_tests();
        let window = HiddenPopup::new(-300, 0, 315, 560);
        let _membership = CloakMembershipGuard::claim(window.id);
        let _forget_insets = ForgetGlobalInsetsOnDrop(window.id);
        let mut cache = PlacementCache::new();
        cache.insets.insert(window.id, (7, 1, 7, 8));
        assert_eq!(get_window_invisible_insets(window.id), (0, 0, 0, 0));
        let config = PlatformConfig {
            monitor_rects: vec![Rect::new(0, 0, 800, 600)],
        };
        let placement = offscreen_placement(
            window.id,
            Rect::new(-300, 0, 300, 560),
            Visibility::OffScreenLeft,
        );
        apply_placements(
            std::slice::from_ref(&placement),
            &config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        let landed = window.rect();
        assert_eq!(
            (landed.x, landed.y),
            if crate::is_high_contrast_enabled() {
                (-315, 0)
            } else {
                (-308, -1)
            }
        );
        assert_eq!(offscreen_placement_matches(window.id), Some(true));
        unsafe {
            SetWindowPos(
                window.hwnd,
                None,
                landed.x,
                landed.y,
                315,
                570,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
        }
        assert_eq!(
            offscreen_placement_matches(window.id),
            Some(false),
            "size drift at the same origin must be detected"
        );
        cache.clear();
        assert_eq!(offscreen_placement_matches(window.id), None);
        apply_placements(
            std::slice::from_ref(&placement),
            &config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        assert_eq!(offscreen_placement_matches(window.id), Some(true));
        forget_offscreen_placement(window.id);
        assert_eq!(offscreen_placement_matches(window.id), None);
        cache.clear();
        apply_placements(
            std::slice::from_ref(&placement),
            &config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        let visible = offscreen_placement(
            window.id,
            Rect::new(-30000, -1000, 300, 560),
            Visibility::Visible,
        );
        apply_placements(&[visible], &config, Some(&mut cache), false).unwrap();
        assert_eq!(offscreen_placement_matches(window.id), None);
    }

    #[test]
    fn test_offscreen_batch_uses_passed_topology_and_parks_without_it() {
        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _cloak = lock_cloak_set_tests();
        let window = HiddenPopup::new(-300, 0, 315, 560);
        let _membership = CloakMembershipGuard::claim(window.id);
        let _forget_insets = ForgetGlobalInsetsOnDrop(window.id);
        let placement = offscreen_placement(
            window.id,
            Rect::new(-300, 0, 300, 560),
            Visibility::OffScreenLeft,
        );
        let owner = Rect::new(0, 0, 800, 600);
        let neighbor = Rect::new(-1110, 0, 800, 600);
        for (monitors, expected) in [
            (vec![owner], (-315, 0)),
            (
                vec![owner, neighbor],
                (
                    crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                    crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                ),
            ),
            (
                vec![],
                (
                    crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                    crate::MOVE_OFFSCREEN_SENTINEL_COORD,
                ),
            ),
        ] {
            let config = PlatformConfig {
                monitor_rects: monitors,
            };
            apply_placements(std::slice::from_ref(&placement), &config, None, false).unwrap();
            let actual = window.rect();
            assert_eq!((actual.x, actual.y), expected);
            assert_eq!((actual.width, actual.height), (315, 560));
            assert_eq!(offscreen_placement_matches(window.id), Some(true));
        }
    }

    #[test]
    fn test_offscreen_left_window_wider_than_column_stays_off_screen() {
        let window = HiddenPopup::new(120, 80, 700, 300);
        let logical = Rect::new(-1000, 40, 600, 900);
        let insets = (7, 1, 7, 8);
        let converted = visible_rect_to_frame_rect(logical, insets, false);
        let (left_x, _, _, _) = defer_origin(
            offscreen_placement(window.id, logical, Visibility::OffScreenLeft),
            insets,
            false,
        );
        assert_eq!(
            left_x + 700 - insets.2,
            logical.x + logical.width,
            "the window's visible right edge must stay at its column's right edge"
        );
        let (right_x, _, _, _) = defer_origin(
            offscreen_placement(window.id, logical, Visibility::OffScreenRight),
            insets,
            false,
        );
        assert_eq!(right_x, converted.x);
    }

    #[test]
    fn apply_placements_retries_tab_landing_after_wrong_zero_insets() {
        if crate::is_high_contrast_enabled() {
            return;
        }

        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _cloak = lock_cloak_set_tests();
        let virtual_left = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN,
            )
        };
        assert!(
            -5120 + 684 < virtual_left,
            "hidden-tab parking must remain outside every monitor (virtual left={virtual_left})"
        );
        let window = HiddenPopup::new_framed(-30000, -1000, 684, 408);
        let _membership = CloakMembershipGuard::claim(window.id);
        let _forget_insets = ForgetGlobalInsetsOnDrop(window.id);
        let real_insets = query_window_frame_insets(window.hwnd)
            .expect("shown framed fixture must expose DWM frame bounds");
        assert_ne!(
            real_insets,
            (0, 0, 0, 0),
            "fixture needs genuine resize borders"
        );
        forget_global_insets(window.id);
        let config = PlatformConfig::default();
        let hidden = offscreen_placement(
            window.id,
            Rect::new(-5120, 0, 0, 0),
            Visibility::OffScreenLeft,
        );
        apply_placements(std::slice::from_ref(&hidden), &config, None, false).unwrap();

        // Windows caps resizable windows near the desktop size, which is 1024x768 on CI runners.
        let requested = Rect::new(-30000, -1000, 900, 700);
        assert!(
            requested.x + requested.width < virtual_left,
            "test target must remain left of every monitor (virtual left={virtual_left})"
        );
        let visible = offscreen_placement(window.id, requested, Visibility::Visible);
        seed_global_insets(window.id, (0, 0, 0, 0));
        apply_placements(std::slice::from_ref(&visible), &config, None, false).unwrap();

        let actual = crate::get_window_visible_rect(window.id)
            .expect("visible fixture must expose DWM frame bounds");
        assert!(
            (actual.x - requested.x).abs() <= VISIBLE_SIZE_TOLERANCE
                && (actual.y - requested.y).abs() <= VISIBLE_SIZE_TOLERANCE
                && (actual.width - requested.width).abs() <= VISIBLE_SIZE_TOLERANCE
                && (actual.height - requested.height).abs() <= VISIBLE_SIZE_TOLERANCE,
            "DWM visible rect {actual:?} did not land at requested rect {requested:?}"
        );
    }

    #[test]
    fn test_zero_size_hidden_tab_native_park_is_stable_across_inset_cache() {
        let _serialize = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _cloak = lock_cloak_set_tests();
        let window = HiddenPopup::new(100, 100, 684, 408);
        let _membership = CloakMembershipGuard::claim(window.id);
        forget_global_insets(window.id);
        clear_inset_cache();
        let original = window.rect();
        assert_eq!((original.width, original.height), (684, 408));
        let logical = Rect::new(-5120, 0, 0, 0);
        let placement = offscreen_placement(window.id, logical, Visibility::OffScreenLeft);
        let config = PlatformConfig::default();

        apply_placements(std::slice::from_ref(&placement), &config, None, false).unwrap();
        let cold = window.rect();

        seed_global_insets(window.id, (7, 1, 7, 8));
        apply_placements(std::slice::from_ref(&placement), &config, None, false).unwrap();
        let warm = window.rect();

        clear_inset_cache();
        apply_placements(std::slice::from_ref(&placement), &config, None, false).unwrap();
        let after_restart = window.rect();

        for observed in [cold, warm, after_restart] {
            assert_eq!(observed.x, logical.x);
            assert_eq!(observed.y, logical.y);
            assert_eq!((observed.width, observed.height), (684, 408));
        }
        forget_global_insets(window.id);
    }
}
