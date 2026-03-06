//! AppState struct definition, constructor, and basic accessors.

use crate::config::{self, Config};
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_platform_win32::{MonitorId, MonitorInfo, PlatformConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Fallback viewport dimensions when no monitor is detected.
pub(crate) const FALLBACK_VIEWPORT_WIDTH: i32 = 1920;
pub(crate) const FALLBACK_VIEWPORT_HEIGHT: i32 = 1080;
pub(crate) const FALLBACK_WORK_AREA_HEIGHT: i32 = 1040;
pub(crate) const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
pub(crate) const MAX_SET_WIDTH_FRACTION: f64 = 1.0;

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

/// Application state supporting multiple monitors.
pub(crate) struct AppState {
    /// Workspaces indexed by monitor ID.
    pub(crate) workspaces: HashMap<MonitorId, Workspace>,
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
    /// Border frame overlay for the active window.
    pub(crate) border_frame: Option<leopardwm_platform_win32::border::BorderFrame>,
    /// Whether tiling is paused.
    pub(crate) paused: bool,
    /// Guard flag to suppress MovedOrResized events during apply_layout().
    pub(crate) applying_layout: bool,
    /// Window currently being dragged/resized by the user (if any).
    /// MovedOrResized events are suppressed during drag; snap-back happens on drop.
    pub(crate) dragging_window: Option<u64>,
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
}

impl AppState {
    /// Create new state with config and monitors.
    pub(crate) fn new_with_config(config: Config, monitors: Vec<MonitorInfo>) -> Self {
        let mut workspaces = HashMap::new();
        let mut monitor_map = HashMap::new();
        let mut focused_monitor = 0;

        for monitor in monitors {
            let mut workspace = Workspace::with_gaps(config.layout.gap, config.layout.outer_gap);
            workspace.set_default_column_width(config.layout.default_column_width);
            workspace.set_centering_mode(config.layout.centering_mode.into());

            if monitor.is_primary {
                focused_monitor = monitor.id;
            }

            workspaces.insert(monitor.id, workspace);
            monitor_map.insert(monitor.id, monitor);
        }

        // If no primary found, use first monitor (defensive pattern avoids unwrap)
        if focused_monitor == 0 {
            if let Some(&first_id) = monitor_map.keys().next() {
                focused_monitor = first_id;
            }
            // If map is empty, focused_monitor stays 0; focused_workspace() returns None
        }

        let platform_config = PlatformConfig;

        let compiled_rules = config.compile_window_rules();

        Self {
            workspaces,
            monitors: monitor_map,
            focused_monitor,
            platform_config,
            config,
            compiled_rules,
            previous_focused_hwnd: None,
            border_frame: leopardwm_platform_win32::border::BorderFrame::new().ok(),
            paused: false,
            applying_layout: false,
            dragging_window: None,
            moved_or_resized_suppression: HashMap::new(),
            apply_worker_cancelled: Arc::new(AtomicBool::new(false)),
            apply_epoch: Arc::new(AtomicU64::new(0)),
            pending_apply_workers: Vec::new(),
            layout_apply_timeout: APPLY_LAYOUT_TIMEOUT,
            start_time: std::time::Instant::now(),
            recently_hidden_hwnds: HashMap::new(),
            window_managed_at: HashMap::new(),
            #[cfg(test)]
            injected_window_info: HashMap::new(),
            #[cfg(test)]
            injected_apply_placements_behavior: None,
            #[cfg(test)]
            late_worker_recovery_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the currently focused workspace.
    pub(crate) fn focused_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(&self.focused_monitor)
    }

    /// Get the currently focused workspace mutably.
    pub(crate) fn focused_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(&self.focused_monitor)
    }

    /// Get the focused monitor's viewport.
    pub(crate) fn focused_viewport(&self) -> Rect {
        self.monitors
            .get(&self.focused_monitor)
            .map(|m| m.work_area)
            .unwrap_or_else(|| Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT))
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
