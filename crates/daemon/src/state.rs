//! AppState struct definition, constructor, and basic accessors.

use crate::config::{self, Config};
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_platform_win32::{MonitorId, MonitorInfo, PlatformConfig};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Tracks an in-progress window drag for column reorder.
pub(crate) struct DragState {
    /// HWND being dragged.
    pub(crate) hwnd: u64,
    /// Whether the dragged window is tiled (vs floating).
    pub(crate) is_tiled: bool,
    /// Source monitor at drag start.
    pub(crate) source_monitor: MonitorId,
    /// Source workspace index at drag start (0-based).
    pub(crate) source_workspace_idx: usize,
    /// Current column index (initialized to source, changes as we live-reorder during drag).
    pub(crate) current_column_index: usize,
    /// Last computed drop target (for change detection).
    pub(crate) last_drop_target: Option<DropTarget>,
    /// Last time the drop target hint was updated (for throttling).
    pub(crate) last_hint_update: Option<std::time::Instant>,
    /// Whether the window was removed from its source column during drag
    /// (multi-window columns only; single-window columns keep the window to
    /// preserve column space).
    pub(crate) removed_from_source: bool,
}

/// Where a column would be inserted if dropped at the current position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DropTarget {
    pub(crate) monitor: MonitorId,
    pub(crate) insert_index: usize,
    /// For window-merge mode: insertion position within the target column.
    pub(crate) window_slot: Option<usize>,
}

/// The single designated scratchpad window and whether it is currently
/// summoned. When hidden, the window is removed from all workspaces and
/// cloaked; when shown, it lives as a floating window on whichever
/// workspace was active at summon time. Session-scoped (HWND-keyed, not
/// persisted across daemon restart).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScratchpadState {
    pub(crate) window_id: u64,
    pub(crate) shown: bool,
    /// Column index the window occupied before being stashed. Used as the
    /// fallback insert position when the original column no longer exists.
    pub(crate) origin_column: usize,
    /// A window that shared the original column, if any. On release the window
    /// rejoins this sibling's current column (robust to index shifts from
    /// columns added/removed while stashed); `None` means it was alone in its
    /// column, so it returns as its own column at `origin_column`.
    pub(crate) origin_sibling: Option<u64>,
}

/// Where a tiled window sat before it was moved off a workspace, so that
/// moving it back lands it on its original column instead of right of focus.
/// Mirrors the scratchpad's `origin_column`/`origin_sibling` restore.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MoveOrigin {
    pub(crate) monitor: MonitorId,
    pub(crate) ws_idx: usize,
    /// Column index occupied before the move; the clamped fallback position.
    pub(crate) column: usize,
    /// A window that shared the original column, if any. On return the window
    /// rejoins this sibling's current column (robust to index shifts); `None`
    /// means it was alone, so it returns as its own column at `column`.
    pub(crate) sibling: Option<u64>,
}

/// Action to show/hide the drag hint overlay, communicated from event handler to main loop.
#[derive(Debug, Clone)]
pub(crate) enum DragHintAction {
    /// Show a semi-transparent ghost rectangle at the target column position.
    ShowGhost { rect: Rect },
    /// Hide the drag hint overlay.
    Hide,
}

/// Fallback viewport dimensions when no monitor is detected.
pub(crate) const FALLBACK_VIEWPORT_WIDTH: i32 = 1920;
pub(crate) const FALLBACK_VIEWPORT_HEIGHT: i32 = 1080;
pub(crate) const FALLBACK_WORK_AREA_HEIGHT: i32 = 1040;
pub(crate) const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
pub(crate) const MAX_SET_WIDTH_FRACTION: f64 = 1.0;
/// Sentinel window ID used as a placeholder during drag to reserve space in the
/// target column without moving the real window.
pub(crate) const DRAG_PLACEHOLDER_HWND: u64 = u64::MAX;

/// Maximum time to wait for the layout-apply worker before giving up and
/// pausing the daemon. Raised from 1500ms to 5000ms so that transient CPU
/// pressure (e.g. a `cargo build` or other all-cores workload running in the
/// background) cannot trip the timeout and force a daemon pause + recovery.
/// Genuinely hung windows hit Windows' own ~5s hung-app timeout anyway, so
/// this doesn't materially weaken responsiveness guarantees.
pub(crate) const APPLY_LAYOUT_TIMEOUT: Duration = Duration::from_millis(5000);
/// Suppress MovedOrResized events after placements are applied, so the
/// target window's own WM_SIZE-driven EVENT_OBJECT_LOCATIONCHANGE (which is
/// our own feedback) is not re-interpreted as a user-initiated move.
///
/// Kept at 250ms so drag-hint and resize-preview updates stay snappy — they
/// route through the same MovedOrResized handler and a larger suppression
/// window would delay the ghost/highlight feedback on drag start by that
/// much. The real defense against false-positive snap-back cascades under
/// CPU pressure is the position-based filter in `WindowEvent::MovedOrResized`
/// (it compares actual-vs-expected rect and short-circuits when they match
/// within a small epsilon), which operates independently of this suppression.
pub(crate) const MOVED_OR_RESIZED_SUPPRESSION_WINDOW: Duration = Duration::from_millis(250);
/// Max age for a `crossfade_sources` re-registration barrier entry. A
/// crossfade runs ~130ms (8 frames); 2s is a generous bound. An entry
/// older than this means its `CrossfadeComplete` never arrived (worker
/// died/stuck), so the barrier is swept to avoid stranding those wids.
pub(crate) const CROSSFADE_BARRIER_MAX_AGE: Duration = Duration::from_secs(2);
/// Windows managed for less than this duration before hiding are considered
/// transient (e.g., Electron notification popups) and suppressed on re-creation.
/// Windows managed longer (e.g., close-to-tray apps) are allowed to re-tile.
pub(crate) const TRANSIENT_WINDOW_THRESHOLD: Duration = Duration::from_secs(30);
/// How long transient window HWNDs stay in the suppression list before expiring.
pub(crate) const RECENTLY_HIDDEN_TTL: Duration = Duration::from_secs(300);
/// After "Edit Config" is clicked, how long to watch for the editor window (a
/// single-instance editor like VS Code may raise an existing window on another
/// workspace) so it can be pulled to the active workspace. Generous because a
/// cold editor start can take several seconds to raise its window.
pub(crate) const EDIT_CONFIG_PULL_TTL: Duration = Duration::from_secs(10);

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum TestApplyPlacementsBehavior {
    SleepAndSucceed(Duration),
    SleepAndFail(Duration),
}

/// Request for the main loop to spawn a resize preview animation thread.
pub(crate) struct ResizeAnimationRequest {
    pub(crate) start_rect: Rect,
    pub(crate) target_rect: Rect,
}

/// Duration of resize preview transition animation in milliseconds.
pub(crate) const RESIZE_PREVIEW_DURATION_MS: u64 = 100;

pub(crate) fn lerp_i32(a: i32, b: i32, t: f64) -> i32 {
    (a as f64 + (b as f64 - a as f64) * t).round() as i32
}

/// Tracks an in-progress layout transition animation.
/// Interpolates window positions from a pre-change snapshot to the new layout.
pub(crate) struct LayoutTransition {
    /// Per-window starting rects (before the structural change).
    pub(crate) start_rects: HashMap<u64, Rect>,
    /// Windows exiting the screen during the transition.
    /// Maps window_id → target rect (offscreen). Start rects are in `start_rects`.
    /// These windows are included in animation frames alongside entering windows.
    /// When the transition completes, they are moved offscreen.
    pub(crate) exit_rects: HashMap<u64, Rect>,
    /// Elapsed time in milliseconds.
    pub(crate) elapsed_ms: u64,
    /// Total duration in milliseconds.
    pub(crate) duration_ms: u64,
    /// Easing curve for this transition (from `[animation].easing`).
    pub(crate) easing: leopardwm_core_layout::Easing,
    /// Window IDs being driven via a DWM thumbnail "ghost" rather than
    /// per-frame `SetWindowPos` on the live HWND. Pure data — the owning
    /// `ThumbnailHandle`s live in `AppState.ghost_handles`, which decouples
    /// their lifetime from the transition's so they can survive into a
    /// post-landing crossfade. `partition_for_animation` uses this set to
    /// split frame placements into live + ghost streams.
    pub(crate) ghosted_wids: HashSet<u64>,
}

/// Owns a registered DWM thumbnail handle for a single window across a
/// ghost-animated layout transition. Dropping the entry unregisters the
/// thumbnail via the platform-layer `unregister_raw` helper, so removal
/// from `AppState.ghost_handles` (or `AppState::drop`-like cleanup) is
/// always leak-free.
///
/// At landing, the handle is transferred to the animation worker via
/// `take_isize` for the crossfade phase. After that point the entry is
/// gone from `ghost_handles` and worker-owned `CrossfadeEntry` values
/// own the registration until fade-complete.
pub(crate) struct GhostEntry {
    /// Raw `HTHUMBNAIL` value. `0` after `take_isize` consumes it, so
    /// Drop becomes a no-op.
    handle_isize: isize,
    /// Class name captured at registration. Used as an HWND-recycling
    /// guard at landing — if `GetClassNameW(hwnd)` no longer matches,
    /// the source has died and we drop the entry without uncloaking
    /// (we don't know what HWND we'd be uncloaking).
    pub(crate) class_at_register: String,
    /// Final on-screen rect (host-client coordinates) where the
    /// thumbnail will rest during the post-landing crossfade.
    #[allow(dead_code)] // Consumed in the crossfade phase.
    pub(crate) final_dest_client_rect: Rect,
}

impl Drop for GhostEntry {
    fn drop(&mut self) {
        if self.handle_isize != 0 {
            leopardwm_platform_win32::thumbnail::unregister_raw(self.handle_isize);
        }
    }
}

impl GhostEntry {
    pub(crate) fn new(
        handle_isize: isize,
        class_at_register: String,
        final_dest_client_rect: Rect,
    ) -> Self {
        Self {
            handle_isize,
            class_at_register,
            final_dest_client_rect,
        }
    }

    /// Raw handle for cross-thread updates from the animation worker.
    /// Read-only access — does not transfer ownership.
    pub(crate) fn handle(&self) -> isize {
        self.handle_isize
    }

    /// Consume this entry without firing Drop, returning the raw handle.
    /// Caller takes responsibility for eventual unregistration. Used at
    /// landing to transfer ownership into `WorkerCommand::Crossfade`
    /// entries owned by the worker thread.
    #[allow(dead_code)] // Consumed in the crossfade phase.
    pub(crate) fn take_isize(mut self) -> isize {
        let raw = self.handle_isize;
        self.handle_isize = 0;
        std::mem::forget(self);
        raw
    }
}

/// Marker for an in-flight crossfade. Held by `AppState.active_crossfade`
/// between the start of the worker's `Crossfade` command and the matching
/// `DaemonEvent::CrossfadeComplete { epoch }`. Stale completions (from
/// fades aborted by a newer transition) are ignored by checking the
/// epoch.
#[allow(dead_code)] // Read in the CrossfadeComplete handler.
pub(crate) struct CrossfadeState {
    pub(crate) epoch: u64,
}

impl LayoutTransition {
    pub(crate) fn progress(&self) -> f64 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        (self.elapsed_ms as f64 / self.duration_ms as f64).clamp(0.0, 1.0)
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.elapsed_ms >= self.duration_ms
    }

    /// Eased progress using this transition's configured easing curve.
    pub(crate) fn eased_progress(&self) -> f64 {
        self.easing.apply(self.progress())
    }

    pub(crate) fn tick(&mut self, delta_ms: u64) -> bool {
        self.elapsed_ms = self.elapsed_ms.saturating_add(delta_ms);
        !self.is_complete()
    }
}

/// Application state supporting multiple monitors.
pub(crate) struct AppState {
    /// Per-monitor workspace lists (multiple workspaces per monitor).
    pub(crate) workspaces: HashMap<MonitorId, Vec<Workspace>>,
    /// Active workspace index (0-based) per monitor.
    pub(crate) active_workspace: HashMap<MonitorId, usize>,
    /// Last-focused floating window per workspace. Tiled focus is already
    /// restored by the workspace's column state on switch, but floating
    /// focus is not, so we remember it here and re-focus it on return.
    pub(crate) floating_focus: HashMap<(MonitorId, usize), u64>,
    /// Monitor info indexed by monitor ID.
    pub(crate) monitors: HashMap<MonitorId, MonitorInfo>,
    /// Currently focused monitor.
    pub(crate) focused_monitor: MonitorId,
    /// Platform configuration.
    pub(crate) platform_config: PlatformConfig,
    /// User configuration.
    pub(crate) config: Config,
    /// Pre-compiled window rules for efficient matching.
    pub(crate) compiled_rules: Vec<config::CompiledWindowRule>,
    /// The designated scratchpad window and its shown/hidden state, if any.
    pub(crate) scratchpad: Option<ScratchpadState>,
    /// Windows pinned visible on every workspace. A sticky window is kept
    /// floating and re-homed to the active workspace on each switch.
    /// Session-scoped (HWND-keyed, not persisted across restart).
    pub(crate) sticky_windows: HashSet<u64>,
    /// One-shot: sticky window to re-focus at the workspace-switch
    /// animation landing pass. Spurious foreground events from the
    /// destination's windows mid-slide can clobber `previous_focused_hwnd`,
    /// so the landing re-sync must re-assert the pinned window's focus.
    pub(crate) pending_sticky_refocus: Option<u64>,
    /// Previously focused window for border color tracking.
    pub(crate) previous_focused_hwnd: Option<u64>,
    /// `(monitor, hwnd)` of the most-recently-broadcast
    /// `FocusedWindowChanged` event. Independent from
    /// `previous_focused_hwnd`: command-driven focus paths
    /// (sync_foreground_window) proactively update `previous_focused_hwnd`
    /// to the intended target before Windows fires EVENT_SYSTEM_FOREGROUND,
    /// which would mask the OS-side broadcast dedup. Tracking monitor too
    /// keeps cross-monitor moves of the same HWND from being suppressed —
    /// the IPC event carries `monitor`, and a subscriber that only sees
    /// `hwnd`-keyed dedup would think the window stayed on its old monitor.
    pub(crate) last_broadcast_focused: Option<(i64, Option<u64>)>,
    /// Timestamp of the last Focused event that changed `previous_focused_hwnd`.
    /// Used to debounce rapid same-column focus switches (e.g., from scroll events).
    pub(crate) last_focus_change_at: Option<std::time::Instant>,
    /// Last time stale-window pruning ran (throttled to 1/sec).
    pub(crate) last_prune_at: Option<std::time::Instant>,
    /// Border frame overlay for the active window.
    pub(crate) border_frame: Option<leopardwm_platform_win32::border::BorderFrame>,
    /// Tab strip overlays — one per tabbed column across all visible
    /// workspaces. Keyed by `(monitor, workspace_idx, column_idx)`.
    /// `helpers.rs::update_tab_strip` reconciles this map against the
    /// current set of tabbed columns on every relayout / focus change:
    /// new tabbed columns get a fresh overlay, columns that became
    /// non-tabbed (or disappeared) have their overlay dropped.
    pub(crate) tab_strip_overlays: std::collections::HashMap<
        (isize, usize, usize),
        leopardwm_platform_win32::tab_strip::TabStripOverlay,
    >,
    /// Cached `mpsc::Sender<TabActionEvent>` used when spawning new tab
    /// strip overlays on demand. Set once during startup (in `state.rs`
    /// `init_tab_strip_overlay`) and reused for every strip created
    /// afterward.
    pub(crate) tab_strip_action_tx: Option<
        std::sync::mpsc::Sender<
            leopardwm_platform_win32::tab_strip::TabActionEvent,
        >,
    >,
    /// Whether the workspace overview overlay is currently shown.
    pub(crate) overview_open: bool,
    /// Overview overlay. NOT constructed in `new` — lazily created on the
    /// first `show_overview` so tests (and headless runs) never spawn the
    /// interactive top-level window.
    pub(crate) overview_overlay: Option<leopardwm_platform_win32::overview::OverviewOverlay>,
    /// Sender cloned into the overlay at lazy-init. Installed once during
    /// daemon startup (parallel to `tab_strip_action_tx`).
    pub(crate) overview_event_tx:
        Option<std::sync::mpsc::Sender<leopardwm_platform_win32::overview::OverviewEvent>>,
    /// Per-window `get_window_icon` results (raw shared HICONs as
    /// `isize`) reused across overview model rebuilds: a miss costs up
    /// to two 50ms `SendMessageTimeoutW` probes against a hung app, on
    /// the state lock. HICONs are app-owned shared handles — never
    /// destroyed here. Evicted on real window destroy.
    pub(crate) overview_icon_cache: HashMap<u64, Option<isize>>,
    /// Whether tiling is paused.
    pub(crate) paused: bool,
    /// Guard flag to suppress MovedOrResized events during apply_layout().
    pub(crate) applying_layout: bool,
    /// Guard flag: prevents recursive re-apply after a size-violation
    /// propagation. The first apply_layout call may detect fresh min-width
    /// /min-height constraints and widen columns or shift distribution; we
    /// trigger a single immediate re-apply so the current frame reflects the
    /// correction. This flag ensures the recursive call cannot itself trigger
    /// another recursive call.
    pub(crate) reapplying_after_violation: bool,
    /// Suppress MovedOrResized snap-backs while a display change is being debounced.
    /// Set on WM_DISPLAYCHANGE, cleared after the debounced handler runs.
    pub(crate) display_change_pending: bool,
    /// Whether the pending debounced change needs the full topology/DPI
    /// reconcile (clearing stale min-size constraints, resizing the thumbnail
    /// host) vs a lightweight work-area-only refit. A real display change sets
    /// this; a taskbar work-area toggle leaves it false so it doesn't recompute
    /// column widths and shift partially-visible windows. Reset after the
    /// settle handler runs.
    pub(crate) display_change_needs_full: bool,
    /// Active drag state: tracks the window being dragged, source position, and drop target.
    pub(crate) drag_state: Option<DragState>,
    /// HWND being actively resized via border drag (not title bar move).
    /// Set on MoveSizeStart when cursor is on resize border, cleared on MoveSizeEnd.
    /// While set, layout snap-back is suppressed to prevent jitter.
    pub(crate) resize_hwnd: Option<u64>,
    /// Throttle timestamp for resize preview hint updates (~60fps).
    pub(crate) last_resize_hint_update: Option<std::time::Instant>,
    /// Current snap target rect during resize (for change detection).
    pub(crate) resize_preview_target: Option<Rect>,
    /// Current displayed rect for overlay/border during resize preview.
    pub(crate) resize_preview_display_rect: Option<Rect>,
    /// Pending animation request (consumed by main loop to spawn DwmFlush thread).
    pub(crate) pending_resize_animation: Option<ResizeAnimationRequest>,
    /// Cancel flag for running resize preview animation thread.
    pub(crate) resize_preview_cancel: Arc<AtomicBool>,
    /// Whether a resize preview animation thread is currently running.
    pub(crate) resize_animation_active: Arc<AtomicBool>,
    /// Pending overlay action from drag event handler (consumed by main loop).
    pub(crate) pending_drag_hint: Option<DragHintAction>,
    /// Per-window suppression deadline for MovedOrResized events after apply_layout().
    pub(crate) moved_or_resized_suppression: HashMap<u64, std::time::Instant>,
    /// Last-placed layout rect per managed window (in layout coordinates, the
    /// rect the layout engine asked for — not the OS-level window rect with
    /// invisible borders). Updated on every successful apply_layout. Used by
    /// the MovedOrResized handler to short-circuit false-positive snap-backs
    /// when Windows fires EVENT_OBJECT_LOCATIONCHANGE for reasons that aren't
    /// actual position changes (Z-order, DWM composition, focus shuffle, DPI,
    /// etc.). If the window's current visible bounds match the last-placed
    /// rect within a few pixels, the layout is already correct and we skip
    /// the expensive full-retile snap-back.
    pub(crate) last_placed_layout_rects: HashMap<u64, leopardwm_core_layout::Rect>,
    /// Cooperative cancellation flag for placement workers during shutdown/revert.
    pub(crate) apply_worker_cancelled: Arc<AtomicBool>,
    /// Monotonic token to invalidate stale workers when shutdown starts.
    pub(crate) apply_epoch: Arc<AtomicU64>,
    /// Timed-out placement workers retained for join during shutdown/revert.
    pub(crate) pending_apply_workers: Vec<std::thread::JoinHandle<()>>,
    /// Max time allowed for Win32 placement calls before auto-pausing tiling.
    pub(crate) layout_apply_timeout: Duration,
    /// Daemon start time for uptime reporting.
    pub(crate) start_time: std::time::Instant,
    /// HWNDs of transient windows (managed briefly then hidden), used to suppress
    /// re-creation of Electron popup windows (Beeper, Slack) that rapidly
    /// show/hide the same HWND.  Entries older than 5 minutes are lazily evicted.
    pub(crate) recently_hidden_hwnds: HashMap<u64, std::time::Instant>,
    /// Armed when "Edit Config" is clicked in the tray: `(armed_at, filename)`.
    /// A single-instance editor may raise an existing window on another
    /// workspace; within `EDIT_CONFIG_PULL_TTL` a window whose title contains
    /// the config `filename` is identified as the editor and pulled to the
    /// active workspace instead of following focus to it. Never persisted.
    pub(crate) pending_edit_config_pull: Option<(std::time::Instant, String)>,
    /// Windows skipped this session because UIPI blocks the non-elevated daemon
    /// from managing them (an elevated window). HWND -> title (for `lwm
    /// doctor`). Kept until the window dies so it's never re-tiled and the user
    /// is notified only once. Session-only, never persisted.
    pub(crate) elevation_blocked: HashMap<u64, String>,
    /// Column width a tiled window had when it was hidden, keyed by HWND, so a
    /// window that disappears and reappears (e.g. a third-party virtual-desktop
    /// tool hiding/showing windows on switch) re-tiles at its prior width
    /// instead of resetting to default. Entries expire after RECENTLY_HIDDEN_TTL.
    pub(crate) hidden_column_widths: HashMap<u64, (std::time::Instant, i32)>,
    /// Where each tiled window last sat before being moved to another workspace,
    /// keyed by HWND. Moving the window back to that workspace restores it to the
    /// original column instead of right of focus. Cleared when consumed or when
    /// the window dies. Session-only, never persisted.
    pub(crate) move_origins: HashMap<u64, MoveOrigin>,
    /// Layout of monitors that disconnected this session, keyed by the stable
    /// `device_name` (e.g. `\\.\DISPLAY2`) rather than the volatile HMONITOR.
    /// When a monitor with a matching device name reconnects, its saved
    /// workspaces (columns, widths, minimized state) and active index are
    /// restored, so a screen powering off overnight or an undock/redock no
    /// longer flattens the layout. Windows are migrated to primary while the
    /// monitor is gone and pulled back on return. Session-only, never persisted.
    pub(crate) stashed_monitor_layouts: HashMap<String, (Vec<Workspace>, usize)>,
    /// Tracks when each managed window was added to a workspace. Used to
    /// distinguish transient popups (managed briefly) from real windows
    /// (managed for a long time, e.g., close-to-tray apps).
    pub(crate) window_managed_at: HashMap<u64, std::time::Instant>,
    /// Last time each tiled window was seen maximized. Lets a window that opens
    /// maximized and momentarily restores itself mid-burst (an app opening
    /// several windows/tabs at once) re-assert maximize instead of being snapped
    /// to a narrow tile. Keyed by HWND; one entry per live window, evicted with
    /// `window_managed_at`.
    pub(crate) window_last_maximized_at: HashMap<u64, std::time::Instant>,
    /// HWNDs whose WS_MAXIMIZEBOX was removed to suppress Snap Layouts.
    /// Lightweight daemon-side mirror — the platform-layer global static is
    /// the authoritative recovery set.
    pub(crate) snap_disabled_hwnds: HashSet<u64>,
    /// System is on battery power or Windows power saver is active.
    pub(crate) on_battery_or_saver: bool,
    /// Skip animations and snap instantly (accessibility setting off or on battery/power saver).
    pub(crate) reduce_motion: bool,
    /// Windows High Contrast mode is active — override border color with system highlight.
    pub(crate) high_contrast: bool,
    /// Active layout transition animation (window position interpolation).
    pub(crate) layout_transition: Option<LayoutTransition>,
    /// DWM thumbnail handles owned across a ghost-animated transition.
    /// Survives `LayoutTransition` clearing (so the landing pass can drain
    /// them into the worker for crossfade). Each `GhostEntry::Drop` calls
    /// `thumbnail::unregister_raw`, so every removal path is leak-safe.
    pub(crate) ghost_handles: HashMap<u64, GhostEntry>,
    /// Set when a crossfade is in flight on the animation worker. The
    /// `epoch` lets us discriminate stale `CrossfadeComplete` events
    /// (from fades aborted by a newer transition).
    pub(crate) active_crossfade: Option<CrossfadeState>,
    /// Window IDs whose thumbnails the worker is currently fading out.
    /// Per-epoch map of source wids currently in a worker-side crossfade
    /// (live or aborted-but-pending-ack). An entry is inserted when sending
    /// `WorkerCommand::Crossfade` and removed when the matching
    /// `CrossfadeComplete { epoch }` arrives. `should_ghost` refuses to
    /// register a new thumbnail for any wid that appears in ANY entry's
    /// value set — Microsoft Q&A 3229922 documents that registering a
    /// second thumbnail for the same source while the first is still alive
    /// can break the first on Win10 1903. Tracking per-epoch (rather than
    /// a flat union) is required because abort paths can leave multiple
    /// crossfades pending-ack simultaneously; clearing on any completion
    /// would prematurely release the barrier for a still-live epoch.
    ///
    /// Each entry carries the dispatch `Instant`. Normally the entry is
    /// removed when its `CrossfadeComplete` arrives; the timestamp is a
    /// safety net: if a worker ever dies without acking (today only at
    /// shutdown), `register_ghosts_for_transition` sweeps any epoch older
    /// than `CROSSFADE_BARRIER_MAX_AGE` so the barrier self-heals instead
    /// of stranding those wids out of the ghost path forever.
    pub(crate) crossfade_sources:
        std::collections::HashMap<u64, (HashSet<u64>, std::time::Instant)>,
    /// Monotonic counter minted at each new crossfade. Used to tag
    /// `WorkerCommand::Crossfade` and `DaemonEvent::CrossfadeComplete`
    /// so daemon-side can ignore stale completions from aborted fades.
    pub(crate) crossfade_epoch_counter: u64,
    /// Cloneable handle for sending `AbortCrossfade` to the animation
    /// worker from `abort_active_crossfade` (which is called by many
    /// paths that don't have direct access to the owning worker).
    /// Installed once at daemon startup via `install_animation_worker_control`.
    pub(crate) animation_worker_control:
        Option<crate::animation_worker::AnimationWorkerControl>,
    /// True when the next sync `apply_layout` is the landing pass after an
    /// animation (scroll or layout transition) and therefore needs the
    /// `(w-1 → w)` nudge to repair sticky-compositor swap-chain desyncs.
    /// Routine `apply_layout` calls (focus shifts in the already-visible
    /// range, event-handler refreshes, drag finalizations) do not run after
    /// an async-frame burst, so nudging them just produces a visible 1 px
    /// resize on every Chromium / Firefox / Cascadia window with no benefit.
    pub(crate) post_animation_nudge_pending: bool,
    /// Injected window info for testing. When set, `lookup_window_info()` returns
    /// entries from this map instead of calling `enumerate_windows()`.
    #[cfg(test)]
    pub(crate) injected_window_info: HashMap<u64, leopardwm_platform_win32::WindowInfo>,
    /// Optional test-only behavior override for placement application.
    #[cfg(test)]
    pub(crate) injected_apply_placements_behavior: Option<TestApplyPlacementsBehavior>,
    /// Number of late-worker recovery passes executed after cancellation.
    #[cfg(test)]
    pub(crate) late_worker_recovery_count: Arc<AtomicUsize>,
    /// Fanout for IPC pub/sub. The IPC server's per-client task calls
    /// `subscribe()` on this to receive an `IpcEvent` stream.
    pub(crate) event_broadcaster: tokio::sync::broadcast::Sender<leopardwm_ipc::IpcEvent>,
    /// Hash of the last `IpcEvent::LayoutChanged` payload we emitted —
    /// used to dedup repeat emissions when the layout signature is
    /// unchanged (e.g. animation frames between settled positions).
    pub(crate) last_emitted_layout_sig: Option<u64>,
    /// Sender for debounced workspace-state saves. Installed at startup
    /// via `install_save_channel`; left `None` under cfg(test) and before
    /// wiring so `request_save_if_changed` is a no-op then.
    pub(crate) save_request_tx: Option<tokio::sync::mpsc::Sender<()>>,
    /// Hash of the persisted state at the last save request, used to skip
    /// redundant save requests (e.g. animation frames that don't change
    /// any persisted field).
    pub(crate) last_persisted_sig: Option<u64>,
    /// One-shot intent flag set by tab-click / tab-cycle commands so the
    /// resulting `WindowEvent::Focused` bypasses same-column suppression.
    /// `(monitor_id, ws_idx, col_idx, tab_idx, set_at)` — `set_at` is a
    /// monotonic timestamp used to TTL the flag (~500ms) so a missed
    /// Focused event doesn't permanently disable suppression.
    pub(crate) pending_tab_focus: Option<PendingTabFocus>,
    /// User-supplied tab title overrides, keyed globally by `WindowId`
    /// (HWND). Survives workspace moves and column membership changes —
    /// re-entering a Tabbed column anywhere restores the preferred name.
    /// Cleared when the window is destroyed/hidden (see event_handler).
    pub(crate) tab_title_overrides: HashMap<u64, String>,
    /// Guard preventing concurrent rename dialogs. Set to `true` by the
    /// handler before spawning the dialog thread; cleared by that thread
    /// on exit. `Arc` so the spawned thread can clear it after the main
    /// event loop has moved on.
    pub(crate) rename_dialog_active: Arc<AtomicBool>,
    /// Sender for rename-dialog results. Installed once during daemon
    /// startup (parallel to `install_tab_strip`); the spawned dialog
    /// thread posts a `TabRenameResult` here, which a forwarder thread
    /// translates to `DaemonEvent::TabRenameSubmitted`.
    pub(crate) rename_result_tx:
        Option<std::sync::mpsc::Sender<crate::events::TabRenameResult>>,
}

/// Tracks an in-flight tab focus change synthesized by the tab strip
/// overlay's click handler or `Ctrl+Alt+J/K` cycle. Consumed by
/// `event_handler.rs`'s same-column suppression bypass when the matching
/// `WindowEvent::Focused` arrives.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingTabFocus {
    pub(crate) monitor: MonitorId,
    pub(crate) workspace_idx: usize,
    pub(crate) column_idx: usize,
    pub(crate) tab_idx: usize,
    pub(crate) set_at: std::time::Instant,
}

impl PendingTabFocus {
    /// TTL after which the flag is considered stale and ignored.
    pub(crate) const TTL: std::time::Duration = std::time::Duration::from_millis(500);

    /// Whether this pending intent still applies (within TTL).
    pub(crate) fn is_fresh(&self) -> bool {
        self.set_at.elapsed() < Self::TTL
    }
}

/// Snapshot of workspace state for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceSnapshot {
    /// Monitor device name (stable across restarts, unlike MonitorId/HMONITOR).
    pub(crate) monitor_device_name: String,
    /// Workspace index within the monitor's workspace list (0-based).
    /// Defaults to 0 for backward compatibility with old snapshots.
    #[serde(default)]
    pub(crate) workspace_index: usize,
    /// Saved workspace state.
    pub(crate) workspace: Workspace,
}

/// Full daemon state snapshot for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StateSnapshot {
    /// Timestamp when state was saved.
    pub(crate) saved_at: String,
    /// Per-monitor workspace snapshots.
    pub(crate) workspaces: Vec<WorkspaceSnapshot>,
    /// Which monitor was focused (by device name).
    pub(crate) focused_monitor_name: String,
    /// Active workspace index per monitor (by device name).
    /// Defaults to empty for backward compatibility.
    #[serde(default)]
    pub(crate) active_workspace: HashMap<String, usize>,
    /// User-supplied tab title overrides keyed by HWND. Defaults to
    /// empty so older snapshots without this field load cleanly.
    #[serde(default)]
    pub(crate) tab_title_overrides: HashMap<u64, String>,
}

impl AppState {
    /// Create new state with config and monitors.
    pub(crate) fn new_with_config(config: Config, monitors: Vec<MonitorInfo>) -> Self {
        use crate::helpers::ScaledLayoutParams;

        let mut workspaces = HashMap::new();
        let mut active_workspace_map = HashMap::new();
        let mut monitor_map = HashMap::new();
        let mut focused_monitor = 0;

        for monitor in monitors {
            let params = ScaledLayoutParams::from_config(
                &config.layout,
                &config.appearance,
                monitor.scale_factor,
                monitor.work_area.width,
            );
            let mut workspace = Workspace::with_directional_gaps(
                params.gap,
                params.outer_gap_left,
                params.outer_gap_right,
                params.outer_gap_top,
                params.outer_gap_bottom,
            );
            workspace.set_default_column_width(params.default_column_width);
            workspace.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
            workspace.set_centering_mode(config.layout.centering_mode.into());
            workspace.set_center_past_edges(config.layout.center_past_edges);
            workspace.set_reduce_motion(
                !leopardwm_platform_win32::are_animations_enabled()
                    || leopardwm_platform_win32::is_on_battery_or_power_saver(),
            );
            workspace.set_scroll_animation(
                config.animation.scroll_duration_ms,
                config.animation.easing,
            );

            if monitor.is_primary {
                focused_monitor = monitor.id;
            }

            workspaces.insert(monitor.id, vec![workspace]);
            active_workspace_map.insert(monitor.id, 0usize);
            monitor_map.insert(monitor.id, monitor);
        }

        // If no primary found, use first monitor (defensive pattern avoids unwrap)
        if focused_monitor == 0 {
            if let Some(&first_id) = monitor_map.keys().next() {
                focused_monitor = first_id;
            }
            // If map is empty, focused_monitor stays 0; focused_workspace() returns None
        }

        let platform_config = PlatformConfig::default();

        let compiled_rules = config.compile_window_rules();

        Self {
            workspaces,
            active_workspace: active_workspace_map,
            floating_focus: HashMap::new(),
            monitors: monitor_map,
            focused_monitor,
            platform_config,
            config,
            compiled_rules,
            scratchpad: None,
            sticky_windows: HashSet::new(),
            pending_sticky_refocus: None,
            previous_focused_hwnd: None,
            last_broadcast_focused: None,
            last_focus_change_at: None,
            last_prune_at: None,
            // Skipped under cfg(test): the layered DWM window would lag the mouse.
            border_frame: if cfg!(test) {
                None
            } else {
                leopardwm_platform_win32::border::BorderFrame::new().ok()
            },
            // Tab strip overlays are spawned on demand by `update_tab_strip`
            // — there's no global "the strip" anymore. `install_tab_strip`
            // just stashes the action sender so subsequent spawns can wire
            // it to the same DaemonEvent drain.
            tab_strip_overlays: std::collections::HashMap::new(),
            tab_strip_action_tx: None,
            overview_open: false,
            overview_overlay: None,
            overview_event_tx: None,
            overview_icon_cache: HashMap::new(),
            // Paused under cfg(test): placeholder hwnds collide with real
            // HWNDs and lag the mouse via DWM. Tests opt out as needed.
            paused: cfg!(test),
            applying_layout: false,
            reapplying_after_violation: false,
            display_change_pending: false,
            display_change_needs_full: false,
            drag_state: None,
            resize_hwnd: None,
            last_resize_hint_update: None,
            resize_preview_target: None,
            resize_preview_display_rect: None,
            pending_resize_animation: None,
            resize_preview_cancel: Arc::new(AtomicBool::new(false)),
            resize_animation_active: Arc::new(AtomicBool::new(false)),
            pending_drag_hint: None,
            moved_or_resized_suppression: HashMap::new(),
            last_placed_layout_rects: HashMap::new(),
            apply_worker_cancelled: Arc::new(AtomicBool::new(false)),
            apply_epoch: Arc::new(AtomicU64::new(0)),
            pending_apply_workers: Vec::new(),
            layout_apply_timeout: APPLY_LAYOUT_TIMEOUT,
            start_time: std::time::Instant::now(),
            recently_hidden_hwnds: HashMap::new(),
            pending_edit_config_pull: None,
            elevation_blocked: HashMap::new(),
            hidden_column_widths: HashMap::new(),
            move_origins: HashMap::new(),
            stashed_monitor_layouts: HashMap::new(),
            window_managed_at: HashMap::new(),
            window_last_maximized_at: HashMap::new(),
            snap_disabled_hwnds: HashSet::new(),
            on_battery_or_saver: leopardwm_platform_win32::is_on_battery_or_power_saver(),
            reduce_motion: !leopardwm_platform_win32::are_animations_enabled()
                || leopardwm_platform_win32::is_on_battery_or_power_saver(),
            high_contrast: leopardwm_platform_win32::is_high_contrast_enabled(),
            layout_transition: None,
            ghost_handles: HashMap::new(),
            active_crossfade: None,
            crossfade_sources: std::collections::HashMap::new(),
            crossfade_epoch_counter: 0,
            animation_worker_control: None,
            post_animation_nudge_pending: false,
            #[cfg(test)]
            injected_window_info: HashMap::new(),
            #[cfg(test)]
            injected_apply_placements_behavior: None,
            #[cfg(test)]
            late_worker_recovery_count: Arc::new(AtomicUsize::new(0)),
            // Capacity 256 is comfortable for human-rate events. A
            // subscriber that lags >256 events behind receives `Lagged`
            // and is expected to reconnect with a fresh Subscribe.
            event_broadcaster: tokio::sync::broadcast::channel(256).0,
            last_emitted_layout_sig: None,
            save_request_tx: None,
            last_persisted_sig: None,
            pending_tab_focus: None,
            tab_title_overrides: HashMap::new(),
            rename_dialog_active: Arc::new(AtomicBool::new(false)),
            rename_result_tx: None,
        }
    }

    /// Get the active workspace index (0-based) for a given monitor.
    pub(crate) fn active_workspace_idx(&self, monitor_id: MonitorId) -> usize {
        self.active_workspace.get(&monitor_id).copied().unwrap_or(0)
    }

    /// Send an event to all IPC subscribers. `broadcast::Sender::send` is
    /// sync (no .await), so this is safe to call while holding any tokio
    /// mutex. Err on zero-receivers is ignored — that just means nobody
    /// is subscribed yet.
    pub(crate) fn broadcast_event(&self, event: leopardwm_ipc::IpcEvent) {
        let _ = self.event_broadcaster.send(event);
    }

    /// Broadcast `FocusedWindowChanged` if `hwnd` differs from the last
    /// broadcast. Centralizes the dedup so OS-driven and command-driven
    /// focus paths emit through the same gate. Looks up window info for
    /// the title/class/executable payload; pass `hwnd: None` when focus
    /// was cleared.
    pub(crate) fn broadcast_focused_window_if_changed(
        &mut self,
        monitor: i64,
        hwnd: Option<u64>,
    ) {
        if self.last_broadcast_focused == Some((monitor, hwnd)) {
            return;
        }
        let (title, class_name, executable) = match hwnd.and_then(|h| self.lookup_window_info(h)) {
            Some(info) => (
                Some(info.title.clone()),
                Some(info.class_name.clone()),
                Some(
                    leopardwm_platform_win32::get_process_executable(info.process_id)
                        .unwrap_or_default(),
                ),
            ),
            None => (None, None, None),
        };
        let _ = self
            .event_broadcaster
            .send(leopardwm_ipc::IpcEvent::FocusedWindowChanged {
                monitor,
                hwnd,
                title,
                class_name,
                executable,
            });
        self.last_broadcast_focused = Some((monitor, hwnd));
    }

    /// Compute a cheap signature of the focused workspace's column
    /// structure for `LayoutChanged` dedup. Animation frames between two
    /// settled layouts produce the same signature, so the sender-side
    /// dedup suppresses all but the structurally-distinct emission.
    pub(crate) fn focused_layout_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.focused_monitor.hash(&mut hasher);
        let ws_idx = self.active_workspace_idx(self.focused_monitor);
        ws_idx.hash(&mut hasher);
        if let Some(ws) = self
            .workspaces
            .get(&self.focused_monitor)
            .and_then(|list| list.get(ws_idx))
        {
            ws.focused_column_index().hash(&mut hasher);
            ws.columns().len().hash(&mut hasher);
            for col in ws.columns() {
                col.width().hash(&mut hasher);
                col.windows().len().hash(&mut hasher);
                for &w in col.windows() {
                    w.hash(&mut hasher);
                }
                // Include height weights so vertical-split changes
                // (cycle-height, equalize-heights, resize) emit a fresh
                // LayoutChanged. Hash the bit pattern; weights are f64
                // so plain Hash isn't implemented.
                for w in col.height_weights() {
                    w.to_bits().hash(&mut hasher);
                }
                // Fold the column's display mode + active tab into the
                // signature so tab switches (Tabbed -> different active_idx)
                // and toggling Vertical<->Tabbed produce a fresh
                // `LayoutChanged` event for IPC subscribers.
                match col.mode() {
                    leopardwm_core_layout::ColumnMode::Vertical => 0u8.hash(&mut hasher),
                    leopardwm_core_layout::ColumnMode::Tabbed { active_idx } => {
                        1u8.hash(&mut hasher);
                        active_idx.hash(&mut hasher);
                    }
                }
            }
        }
        hasher.finish()
    }

    /// Build the column summary list for a `LayoutChanged` event payload.
    pub(crate) fn focused_layout_columns(&self) -> Vec<leopardwm_ipc::ColumnSummary> {
        let ws_idx = self.active_workspace_idx(self.focused_monitor);
        self.workspaces
            .get(&self.focused_monitor)
            .and_then(|list| list.get(ws_idx))
            .map(|ws| {
                ws.columns()
                    .iter()
                    .map(|col| leopardwm_ipc::ColumnSummary {
                        window_ids: col.windows().to_vec(),
                        width_px: col.width(),
                        height_weights: col.height_weights().to_vec(),
                        mode: match col.mode() {
                            leopardwm_core_layout::ColumnMode::Vertical => {
                                leopardwm_ipc::ColumnSummaryMode::Vertical
                            }
                            leopardwm_core_layout::ColumnMode::Tabbed { active_idx } => {
                                leopardwm_ipc::ColumnSummaryMode::Tabbed {
                                    active_idx: *active_idx,
                                }
                            }
                        },
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the currently focused workspace (active workspace on the focused monitor).
    pub(crate) fn focused_workspace(&self) -> Option<&Workspace> {
        let idx = self.active_workspace_idx(self.focused_monitor);
        self.workspaces.get(&self.focused_monitor)?.get(idx)
    }

    /// Get the currently focused workspace mutably.
    pub(crate) fn focused_workspace_mut(&mut self) -> Option<&mut Workspace> {
        let idx = self.active_workspace_idx(self.focused_monitor);
        self.workspaces.get_mut(&self.focused_monitor)?.get_mut(idx)
    }

    /// Resolve the displayed title for the tab at the given strip-relative
    /// identity. Single source of truth for both strip rendering AND the
    /// rename dialog seed: override wins, then live window title, then "".
    /// An absent override always falls back to the live title — never an
    /// empty cleared string — so re-opening the dialog after an
    /// empty-submit clear seeds with the real title.
    pub(crate) fn tab_title_for(
        &self,
        monitor: MonitorId,
        workspace_idx: usize,
        column_idx: usize,
        tab_idx: usize,
    ) -> String {
        let hwnd = self
            .workspaces
            .get(&monitor)
            .and_then(|wss| wss.get(workspace_idx))
            .and_then(|ws| ws.column(column_idx))
            .and_then(|col| col.get(tab_idx));
        match hwnd {
            Some(h) => self
                .tab_title_overrides
                .get(&h)
                .cloned()
                .or_else(|| self.lookup_window_info(h).map(|i| i.title))
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Install the tab strip overlay. Called once during daemon startup
    /// after the action channel pair has been created in main.rs scope —
    /// the overlay receives `tab_action_tx` and posts to it on
    /// WM_LBUTTONDOWN / WM_MBUTTONUP / WM_RBUTTONUP / close-X hit; main.rs
    /// drains the matching receiver and routes the action.
    ///
    /// Skipped in test builds (would create real layered DWM windows).
    pub(crate) fn install_tab_strip(
        &mut self,
        tab_action_tx: std::sync::mpsc::Sender<
            leopardwm_platform_win32::tab_strip::TabActionEvent,
        >,
    ) {
        if cfg!(test) {
            drop(tab_action_tx);
            return;
        }
        // Stash the sender for on-demand spawning. `update_tab_strip`
        // creates one `TabStripOverlay` per tabbed column the next time
        // it runs — all of them share this cloned sender so click /
        // close / rename actions land in the same DaemonEvent drain.
        self.tab_strip_action_tx = Some(tab_action_tx);
    }

    /// Install the overview event sender. The overlay itself is created
    /// lazily on first `show_overview`; this just stashes the channel.
    ///
    /// Skipped in test builds (the overlay would create a real top-level
    /// window) — with no sender, `show_overview` skips overlay creation.
    pub(crate) fn install_overview(
        &mut self,
        event_tx: std::sync::mpsc::Sender<leopardwm_platform_win32::overview::OverviewEvent>,
    ) {
        if cfg!(test) {
            drop(event_tx);
            return;
        }
        self.overview_event_tx = Some(event_tx);
    }

    /// Install the debounced save-request sender. Stashed at startup; the
    /// background save task owns the matching receiver. `None` until this
    /// is called (and under cfg(test)), so saves are simply not requested.
    pub(crate) fn install_save_channel(&mut self, tx: tokio::sync::mpsc::Sender<()>) {
        self.save_request_tx = Some(tx);
    }

    /// Try to consume `pending_tab_focus` if it matches the given event
    /// (same monitor + workspace, and `hwnd` resolves to the expected tab
    /// in the expected column). Returns `true` if the flag was consumed
    /// and the caller should bypass same-column suppression.
    ///
    /// Stale flags (older than `PendingTabFocus::TTL`) are dropped
    /// silently as a safety net for cases where the synthetic
    /// SetForegroundWindow never produced an event.
    pub(crate) fn consume_pending_tab_focus_for(
        &mut self,
        monitor: MonitorId,
        workspace_idx: usize,
        hwnd: u64,
    ) -> bool {
        let Some(pending) = self.pending_tab_focus else {
            return false;
        };
        if !pending.is_fresh() {
            self.pending_tab_focus = None;
            return false;
        }
        if pending.monitor != monitor || pending.workspace_idx != workspace_idx {
            return false;
        }
        // Verify the event's hwnd matches the expected (column, tab) in
        // the workspace state. If it doesn't, leave the flag in place so a
        // later matching event can still consume it.
        let matches = self
            .workspaces
            .get(&monitor)
            .and_then(|list| list.get(workspace_idx))
            .and_then(|ws| ws.column(pending.column_idx))
            .and_then(|col| col.get(pending.tab_idx))
            .is_some_and(|w| w == hwnd);
        if matches {
            self.pending_tab_focus = None;
            true
        } else {
            false
        }
    }

    /// Ensure workspace index exists for a monitor, creating empty workspaces as needed.
    /// Returns a mutable reference to the workspace at the given index.
    pub(crate) fn ensure_workspace_exists(&mut self, monitor_id: MonitorId, idx: usize) -> Option<&mut Workspace> {
        use crate::helpers::ScaledLayoutParams;

        let scale = self.monitors.get(&monitor_id).map(|m| m.scale_factor).unwrap_or(1.0);
        let vw = self.monitors.get(&monitor_id)
            .map(|m| m.work_area.width)
            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
        let params = ScaledLayoutParams::from_config(
            &self.config.layout,
            &self.config.appearance,
            scale,
            vw,
        );

        let config = &self.config;
        let ws_vec = self.workspaces.get_mut(&monitor_id)?;
        while ws_vec.len() <= idx {
            let mut ws = Workspace::with_directional_gaps(
                params.gap,
                params.outer_gap_left,
                params.outer_gap_right,
                params.outer_gap_top,
                params.outer_gap_bottom,
            );
            ws.set_default_column_width(params.default_column_width);
            ws.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
            ws.set_centering_mode(config.layout.centering_mode.into());
            ws.set_center_past_edges(config.layout.center_past_edges);
            ws.set_reduce_motion(self.reduce_motion);
            ws.set_scroll_animation(
                config.animation.scroll_duration_ms,
                config.animation.easing,
            );
            ws_vec.push(ws);
        }
        ws_vec.get_mut(idx)
    }

    /// Get the focused monitor's viewport.
    pub(crate) fn focused_viewport(&self) -> Rect {
        self.monitors
            .get(&self.focused_monitor)
            .map(|m| m.work_area)
            .unwrap_or_else(|| Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT))
    }

    /// Get the viewport width for a specific monitor.
    pub(crate) fn viewport_width_for(&self, monitor_id: MonitorId) -> i32 {
        self.monitors
            .get(&monitor_id)
            .map(|m| m.work_area.width)
            .unwrap_or(FALLBACK_VIEWPORT_WIDTH)
    }

}

pub(crate) fn validate_set_width_fraction(fraction: f64) -> std::result::Result<(), String> {
    if !fraction.is_finite() {
        return Err("Invalid set-width fraction: value must be finite".to_string());
    }
    if !(MIN_SET_WIDTH_FRACTION..=MAX_SET_WIDTH_FRACTION).contains(&fraction) {
        return Err(format!(
            "Invalid set-width fraction ({}): expected value in [{:.1}, {:.1}]",
            fraction, MIN_SET_WIDTH_FRACTION, MAX_SET_WIDTH_FRACTION
        ));
    }
    Ok(())
}

pub(crate) fn layout_apply_timeout_message(timeout: Duration) -> String {
    format!(
        "Layout application timed out after {} ms; tiling auto-paused to keep the daemon responsive. Resolve blocked Win32 placement, then use tray 'Pause/Resume Tiling' to resume. If desktop control degrades, run `leopardwm-cli panic-revert`.",
        timeout.as_millis()
    )
}

pub(crate) fn merged_cleanup_window_ids(
    managed_window_ids: &[u64],
    discovered_window_ids: &[u64],
) -> Vec<u64> {
    let mut merged = Vec::with_capacity(managed_window_ids.len() + discovered_window_ids.len());
    merged.extend_from_slice(managed_window_ids);
    merged.extend_from_slice(discovered_window_ids);
    merged.sort_unstable();
    merged.dedup();
    merged
}

pub(crate) fn run_visibility_recovery_pass(managed_window_ids: &[u64], context_label: &str) {
    use leopardwm_platform_win32::{
        enumerate_windows, restore_windows_moved_offscreen, uncloak_all_managed_windows,
    };
    use tracing::warn;

    let discovered_window_ids = match enumerate_windows() {
        Ok(windows) => windows.into_iter().map(|w| w.hwnd).collect::<Vec<_>>(),
        Err(e) => {
            warn!(
                "Failed to enumerate windows during {} recovery: {}",
                context_label, e
            );
            Vec::new()
        }
    };
    let recovery_window_ids = merged_cleanup_window_ids(managed_window_ids, &discovered_window_ids);

    match restore_windows_moved_offscreen(&recovery_window_ids) {
        Ok(restored) => {
            if restored > 0 {
                info!(
                    "Restored {} windows from MoveOffScreen sentinel positions",
                    restored
                );
            }
        }
        Err(e) => warn!("MoveOffScreen recovery failed: {}", e),
    }

    // Uncloak only managed windows. The panic-grade `uncloak_all_visible_windows()`
    // sweep — which yanks every top-level sentinel-parked window on the desktop
    // back onto the primary monitor — is deliberately NOT called here. Transient
    // apply timeouts (e.g. from CPU pressure during a heavy rebuild) should not
    // drag off-workspace windows onto the active one. Panic hooks still call
    // `uncloak_all_visible_windows()` directly for real crash recovery.
    uncloak_all_managed_windows(managed_window_ids);
}
