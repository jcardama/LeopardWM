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

/// Action to show/hide the drag hint overlay, communicated from event handler to main loop.
#[derive(Debug, Clone)]
pub(crate) enum DragHintAction {
    /// Show a semi-transparent ghost rectangle at the target column position.
    ShowGhost { rect: Rect },
    /// Hide the drag hint overlay.
    Hide,
}

/// Duration of layout transition animations in milliseconds.
pub(crate) const LAYOUT_TRANSITION_DURATION_MS: u64 = 150;

/// Duration of workspace switch slide animation in milliseconds.
pub(crate) const WORKSPACE_SWITCH_DURATION_MS: u64 = 200;

/// Fallback viewport dimensions when no monitor is detected.
pub(crate) const FALLBACK_VIEWPORT_WIDTH: i32 = 1920;
pub(crate) const FALLBACK_VIEWPORT_HEIGHT: i32 = 1080;
pub(crate) const FALLBACK_WORK_AREA_HEIGHT: i32 = 1040;
pub(crate) const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
pub(crate) const MAX_SET_WIDTH_FRACTION: f64 = 1.0;
/// Sentinel window ID used as a placeholder during drag to reserve space in the
/// target column without moving the real window.
pub(crate) const DRAG_PLACEHOLDER_HWND: u64 = u64::MAX;

/// Max time allowed for a single Win32 placement apply call.
pub(crate) const APPLY_LAYOUT_TIMEOUT: Duration = Duration::from_millis(1500);
/// Suppress MovedOrResized events briefly after placements are applied.
pub(crate) const MOVED_OR_RESIZED_SUPPRESSION_WINDOW: Duration = Duration::from_millis(250);
/// Windows managed for less than this duration before hiding are considered
/// transient (e.g., Electron notification popups) and suppressed on re-creation.
/// Windows managed longer (e.g., close-to-tray apps) are allowed to re-tile.
pub(crate) const TRANSIENT_WINDOW_THRESHOLD: Duration = Duration::from_secs(30);
/// How long transient window HWNDs stay in the suppression list before expiring.
pub(crate) const RECENTLY_HIDDEN_TTL: Duration = Duration::from_secs(300);

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

    /// Cubic ease-out (matches scroll animation default).
    pub(crate) fn eased_progress(&self) -> f64 {
        let t = self.progress();
        1.0 - (1.0 - t).powi(3)
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
    /// Previously focused window for border color tracking.
    pub(crate) previous_focused_hwnd: Option<u64>,
    /// Timestamp of the last Focused event that changed `previous_focused_hwnd`.
    /// Used to debounce rapid same-column focus switches (e.g., from scroll events).
    pub(crate) last_focus_change_at: Option<std::time::Instant>,
    /// Last time stale-window pruning ran (throttled to 1/sec).
    pub(crate) last_prune_at: Option<std::time::Instant>,
    /// Border frame overlay for the active window.
    pub(crate) border_frame: Option<leopardwm_platform_win32::border::BorderFrame>,
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
    /// Tracks when each managed window was added to a workspace. Used to
    /// distinguish transient popups (managed briefly) from real windows
    /// (managed for a long time, e.g., close-to-tray apps).
    pub(crate) window_managed_at: HashMap<u64, std::time::Instant>,
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
            workspace.set_centering_mode(config.layout.centering_mode.into());
            workspace.set_center_past_edges(config.layout.center_past_edges);
            workspace.set_reduce_motion(
                !leopardwm_platform_win32::are_animations_enabled()
                    || leopardwm_platform_win32::is_on_battery_or_power_saver(),
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
            monitors: monitor_map,
            focused_monitor,
            platform_config,
            config,
            compiled_rules,
            previous_focused_hwnd: None,
            last_focus_change_at: None,
            last_prune_at: None,
            border_frame: leopardwm_platform_win32::border::BorderFrame::new().ok(),
            paused: false,
            applying_layout: false,
            reapplying_after_violation: false,
            display_change_pending: false,
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
            apply_worker_cancelled: Arc::new(AtomicBool::new(false)),
            apply_epoch: Arc::new(AtomicU64::new(0)),
            pending_apply_workers: Vec::new(),
            layout_apply_timeout: APPLY_LAYOUT_TIMEOUT,
            start_time: std::time::Instant::now(),
            recently_hidden_hwnds: HashMap::new(),
            window_managed_at: HashMap::new(),
            snap_disabled_hwnds: HashSet::new(),
            on_battery_or_saver: leopardwm_platform_win32::is_on_battery_or_power_saver(),
            reduce_motion: !leopardwm_platform_win32::are_animations_enabled()
                || leopardwm_platform_win32::is_on_battery_or_power_saver(),
            high_contrast: leopardwm_platform_win32::is_high_contrast_enabled(),
            layout_transition: None,
            #[cfg(test)]
            injected_window_info: HashMap::new(),
            #[cfg(test)]
            injected_apply_placements_behavior: None,
            #[cfg(test)]
            late_worker_recovery_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the active workspace index (0-based) for a given monitor.
    pub(crate) fn active_workspace_idx(&self, monitor_id: MonitorId) -> usize {
        self.active_workspace.get(&monitor_id).copied().unwrap_or(0)
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

    /// Ensure workspace index exists for a monitor, creating empty workspaces as needed.
    /// Returns a mutable reference to the workspace at the given index.
    pub(crate) fn ensure_workspace_exists(&mut self, monitor_id: MonitorId, idx: usize) -> Option<&mut Workspace> {
        use crate::helpers::ScaledLayoutParams;

        let scale = self.monitors.get(&monitor_id).map(|m| m.scale_factor).unwrap_or(1.0);
        let vw = self.monitors.get(&monitor_id)
            .map(|m| m.work_area.width)
            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
        let params = ScaledLayoutParams::from_config(&self.config.layout, scale, vw);

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
            ws.set_centering_mode(config.layout.centering_mode.into());
            ws.set_center_past_edges(config.layout.center_past_edges);
            ws.set_reduce_motion(self.reduce_motion);
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
        uncloak_all_visible_windows,
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

    // Keep managed-window uncloak behavior.
    uncloak_all_managed_windows(managed_window_ids);
    // Safety net: also uncloak all visible windows on the desktop.
    uncloak_all_visible_windows();
}
