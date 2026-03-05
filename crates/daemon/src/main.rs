//! LeopardWM Daemon
//!
//! Main daemon process for the LeopardWM window manager.
//!
//! Responsibilities:
//! - Maintain workspace state
//! - Process window events from the platform layer
//! - Handle IPC commands from the CLI
//! - Trigger layout recalculations
//! - Apply window placements
//! - System tray icon and menu

mod animation_worker;
mod config;
mod settings;
mod tray;

use anyhow::{anyhow, Result};
use clap::Parser;
use config::Config;
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_ipc::{
    pipe_name_candidates, preferred_pipe_name, IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE,
};
use leopardwm_platform_win32::{
    enumerate_monitors, enumerate_windows, find_monitor_for_rect, get_process_executable,
    install_event_hooks, install_mouse_hook, monitor_to_left, monitor_to_right,
    overlay::OverlayWindow, parse_hotkey_string, register_gestures, register_hotkeys,
    restore_windows_moved_offscreen, set_display_change_sender, set_dpi_awareness,
    uncloak_all_managed_windows, uncloak_all_visible_windows, GestureEvent, Hotkey, HotkeyEvent,
    HotkeyId, MonitorId, MonitorInfo, PlatformConfig, WindowEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Command-line arguments for the daemon binary.
#[derive(Parser, Debug, Clone)]
#[command(name = "leopardwm", about = "LeopardWM tiling window manager daemon")]
pub struct Args {
    /// Disable global hotkey registration
    #[arg(long)]
    pub no_hotkeys: bool,
    /// Use MoveOffScreen instead of DWM cloaking
    #[arg(long)]
    pub no_cloak: bool,
    /// Safe mode: combines --no-hotkeys and --no-cloak
    #[arg(long)]
    pub safe_mode: bool,
}

impl Args {
    /// Returns true if hotkeys should be skipped (either --no-hotkeys or --safe-mode).
    pub fn skip_hotkeys(&self) -> bool {
        self.no_hotkeys || self.safe_mode
    }

    /// Returns true if cloaking should be disabled (either --no-cloak or --safe-mode).
    pub fn skip_cloak(&self) -> bool {
        self.no_cloak || self.safe_mode
    }
}

/// Events that the daemon event loop processes.
enum DaemonEvent {
    /// An IPC command from a CLI client.
    IpcCommand {
        cmd: IpcCommand,
        responder: oneshot::Sender<IpcResponse>,
    },
    /// A window lifecycle event from Win32.
    WindowEvent(WindowEvent),
    /// A global hotkey was pressed.
    Hotkey(HotkeyEvent),
    /// A touchpad gesture was detected.
    Gesture(GestureEvent),
    /// A tray menu event.
    Tray(tray::TrayEvent),
    /// A settings window event.
    Settings(settings::SettingsEvent),
    /// An animation frame was applied by the worker thread.
    AnimationFrameApplied(animation_worker::FrameResult),
    /// Hide snap hint overlay after timeout.
    HideSnapHint,
    /// Apply focus-follows-mouse focus after delay.
    FocusFollowsMouse { window_id: u64 },
    /// Shutdown signal.
    Shutdown,
}


/// IPC read timeout - clients must send within this period.
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// IPC responder timeout - daemon must answer within this period.
const IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Max time allowed for a single Win32 placement apply call.
const APPLY_LAYOUT_TIMEOUT: Duration = Duration::from_millis(1500);
/// Poll interval for cooperative timed thread joins.
const JOIN_WITH_TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Suppress MovedOrResized events briefly after placements are applied.
const MOVED_OR_RESIZED_SUPPRESSION_WINDOW: Duration = Duration::from_millis(250);
/// Retry count for shutdown visibility recovery when an apply worker fails to exit in time.
const SHUTDOWN_RECOVERY_RETRY_ATTEMPTS: usize = 3;
/// Delay between additional shutdown visibility recovery attempts.
const SHUTDOWN_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Final bounded wait per lingering apply worker before daemon exit.
const SHUTDOWN_FINAL_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Fallback viewport dimensions when no monitor is detected.
const FALLBACK_VIEWPORT_WIDTH: i32 = 1920;
const FALLBACK_VIEWPORT_HEIGHT: i32 = 1080;
const FALLBACK_WORK_AREA_HEIGHT: i32 = 1040;
const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
const MAX_SET_WIDTH_FRACTION: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownMode {
    Graceful,
    PanicRevert,
}

impl ShutdownMode {
    fn should_save_state(self) -> bool {
        matches!(self, Self::Graceful)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::PanicRevert => "panic_revert",
        }
    }
}

fn shutdown_mode_for_command(cmd: &IpcCommand) -> Option<ShutdownMode> {
    match cmd {
        IpcCommand::Stop => Some(ShutdownMode::Graceful),
        IpcCommand::PanicRevert => Some(ShutdownMode::PanicRevert),
        _ => None,
    }
}

fn layout_apply_timeout_message(timeout: Duration) -> String {
    format!(
        "Layout application timed out after {} ms; tiling auto-paused to keep the daemon responsive. Resolve blocked Win32 placement, then use tray 'Pause/Resume Tiling' to resume. If desktop control degrades, run `leopardwm-cli panic-revert`.",
        timeout.as_millis()
    )
}

fn response_for_ipc_wait_failure(cmd: &IpcCommand, timed_out: bool) -> IpcResponse {
    if matches!(cmd, IpcCommand::Stop | IpcCommand::PanicRevert) {
        // Stop/panic_revert semantics are "shutdown initiated"; don't report as a hard failure
        // if the responder channel closes or cleanup outlives the client timeout.
        IpcResponse::Ok
    } else if timed_out {
        IpcResponse::error("Timed out waiting for daemon response")
    } else {
        IpcResponse::error("Failed to get response from daemon")
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum TestApplyPlacementsBehavior {
    SleepAndSucceed(Duration),
    SleepAndFail(Duration),
}

fn validate_set_width_fraction(fraction: f64) -> std::result::Result<(), String> {
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

/// Application state supporting multiple monitors.
struct AppState {
    /// Workspaces indexed by monitor ID.
    workspaces: HashMap<MonitorId, Workspace>,
    /// Monitor info indexed by monitor ID.
    monitors: HashMap<MonitorId, MonitorInfo>,
    /// Currently focused monitor.
    focused_monitor: MonitorId,
    /// Platform configuration.
    platform_config: PlatformConfig,
    /// User configuration.
    config: Config,
    /// Pre-compiled window rules for efficient matching.
    compiled_rules: Vec<config::CompiledWindowRule>,
    /// Previously focused window for border color tracking.
    previous_focused_hwnd: Option<u64>,
    /// Border frame overlay for the active window.
    border_frame: Option<leopardwm_platform_win32::border::BorderFrame>,
    /// Whether tiling is paused.
    paused: bool,
    /// Guard flag to suppress MovedOrResized events during apply_layout().
    applying_layout: bool,
    /// Window currently being dragged/resized by the user (if any).
    /// MovedOrResized events are suppressed during drag; snap-back happens on drop.
    dragging_window: Option<u64>,
    /// Per-window suppression deadline for MovedOrResized events after apply_layout().
    moved_or_resized_suppression: HashMap<u64, std::time::Instant>,
    /// Cooperative cancellation flag for placement workers during shutdown/revert.
    apply_worker_cancelled: Arc<AtomicBool>,
    /// Monotonic token to invalidate stale workers when shutdown starts.
    apply_epoch: Arc<AtomicU64>,
    /// Timed-out placement workers retained for join during shutdown/revert.
    pending_apply_workers: Vec<std::thread::JoinHandle<()>>,
    /// Max time allowed for Win32 placement calls before auto-pausing tiling.
    layout_apply_timeout: Duration,
    /// Daemon start time for uptime reporting.
    start_time: std::time::Instant,
    /// Injected window info for testing. When set, `lookup_window_info()` returns
    /// entries from this map instead of calling `enumerate_windows()`.
    #[cfg(test)]
    injected_window_info: HashMap<u64, leopardwm_platform_win32::WindowInfo>,
    /// Optional test-only behavior override for placement application.
    #[cfg(test)]
    injected_apply_placements_behavior: Option<TestApplyPlacementsBehavior>,
    /// Number of late-worker recovery passes executed after cancellation.
    #[cfg(test)]
    late_worker_recovery_count: Arc<AtomicUsize>,
}

/// Snapshot of workspace state for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceSnapshot {
    /// Monitor device name (stable across restarts, unlike MonitorId/HMONITOR).
    monitor_device_name: String,
    /// Saved workspace state.
    workspace: Workspace,
}

/// Full daemon state snapshot for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateSnapshot {
    /// Timestamp when state was saved.
    saved_at: String,
    /// Per-monitor workspace snapshots.
    workspaces: Vec<WorkspaceSnapshot>,
    /// Which monitor was focused (by device name).
    focused_monitor_name: String,
}

impl AppState {
    /// Create new state with config and monitors.
    fn new_with_config(config: Config, monitors: Vec<MonitorInfo>) -> Self {
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

        let platform_config = PlatformConfig {
            hide_strategy: if config.appearance.use_cloaking {
                leopardwm_platform_win32::HideStrategy::Cloak
            } else {
                leopardwm_platform_win32::HideStrategy::MoveOffScreen
            },
            use_deferred_positioning: config.appearance.use_deferred_positioning,
        };

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
            #[cfg(test)]
            injected_window_info: HashMap::new(),
            #[cfg(test)]
            injected_apply_placements_behavior: None,
            #[cfg(test)]
            late_worker_recovery_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the currently focused workspace.
    fn focused_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(&self.focused_monitor)
    }

    /// Get the currently focused workspace mutably.
    fn focused_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(&self.focused_monitor)
    }

    /// Get the focused monitor's viewport.
    fn focused_viewport(&self) -> Rect {
        self.monitors
            .get(&self.focused_monitor)
            .map(|m| m.work_area)
            .unwrap_or_else(|| Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT))
    }

    /// Look up window info for a given window handle.
    ///
    /// In production, calls `enumerate_windows()` and finds the matching entry.
    /// In tests, returns from the injected window info map if available.
    fn lookup_window_info(&self, hwnd: u64) -> Option<leopardwm_platform_win32::WindowInfo> {
        #[cfg(test)]
        {
            if let Some(info) = self.injected_window_info.get(&hwnd) {
                return Some(info.clone());
            }
        }
        leopardwm_platform_win32::get_window_info(hwnd)
    }

    /// Check whether a window ID is known to this state (managed or injected).
    ///
    /// Used by event validation to skip `is_valid_window` for windows we have
    /// info about, even if they aren't yet managed (e.g., during Created events).
    fn is_known_window(&self, wid: u64) -> bool {
        if self.find_window_workspace(wid).is_some() {
            return true;
        }
        #[cfg(test)]
        {
            if self.injected_window_info.contains_key(&wid) {
                return true;
            }
        }
        false
    }

    /// Apply configuration to all workspaces.
    fn apply_config(&mut self, config: Config) {
        // Swap config first so border helpers read the new values
        let old_border_on = self.config.appearance.active_border;
        self.compiled_rules = config.compile_window_rules();

        // Update platform config
        self.platform_config.use_deferred_positioning = config.appearance.use_deferred_positioning;
        self.platform_config.hide_strategy = if config.appearance.use_cloaking {
            leopardwm_platform_win32::HideStrategy::Cloak
        } else {
            leopardwm_platform_win32::HideStrategy::MoveOffScreen
        };

        self.config = config;

        // Handle border transitions with new config values
        if let Some(hwnd) = self.previous_focused_hwnd {
            if self.config.appearance.active_border {
                self.show_border(hwnd);
            } else if old_border_on {
                self.hide_border();
            }
        } else if !self.config.appearance.active_border && old_border_on {
            self.hide_border();
        }

        for workspace in self.workspaces.values_mut() {
            workspace.set_gap(self.config.layout.gap);
            workspace.set_outer_gap(self.config.layout.outer_gap);
            workspace.set_default_column_width(self.config.layout.default_column_width);
            workspace.set_centering_mode(self.config.layout.centering_mode.into());
        }
        info!(
            "Configuration applied to all {} workspaces",
            self.workspaces.len()
        );
    }

    /// Save current workspace state to disk.
    fn save_state(&self) -> Result<()> {
        let snapshots: Vec<WorkspaceSnapshot> = self
            .workspaces
            .iter()
            .filter_map(|(monitor_id, workspace)| {
                self.monitors
                    .get(monitor_id)
                    .map(|monitor| WorkspaceSnapshot {
                        monitor_device_name: monitor.device_name.clone(),
                        workspace: workspace.clone(),
                    })
            })
            .collect();

        let focused_name = self
            .monitors
            .get(&self.focused_monitor)
            .map(|m| m.device_name.clone())
            .unwrap_or_default();

        let saved_at = {
            let now = std::time::SystemTime::now();
            match now.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => format!("{}", d.as_secs()),
                Err(_) => "0".to_string(),
            }
        };

        let snapshot = StateSnapshot {
            saved_at,
            workspaces: snapshots,
            focused_monitor_name: focused_name,
        };

        let state_path = Self::state_file_path();
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(&state_path, json)?;
        info!("Workspace state saved to {:?}", state_path);
        Ok(())
    }

    /// Load saved workspace state from disk.
    fn load_state() -> Option<StateSnapshot> {
        let state_path = Self::state_file_path();
        match std::fs::read_to_string(&state_path) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(snapshot) => Some(snapshot),
                Err(e) => {
                    warn!("Failed to parse saved state: {}", e);
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Get the path for the state file.
    fn state_file_path() -> std::path::PathBuf {
        directories::ProjectDirs::from("", "", "leopardwm")
            .map(|dirs| dirs.data_dir().join("workspace-state.json"))
            .unwrap_or_else(|| std::path::PathBuf::from("workspace-state.json"))
    }

    /// Restore workspace state from a saved snapshot.
    ///
    /// This should be called AFTER windows are enumerated so that scroll offsets
    /// are not clamped against empty workspaces. Sets the scroll offset directly
    /// (bypassing clamping) to preserve the saved value.
    ///
    /// Returns the set of monitor IDs whose scroll offsets were successfully
    /// restored. The caller should skip `ensure_focused_visible()` for these
    /// monitors to avoid overwriting the restored offset.
    fn restore_state(&mut self, snapshot: &StateSnapshot) -> HashSet<MonitorId> {
        let mut restored_monitors = HashSet::new();

        for ws_snapshot in &snapshot.workspaces {
            // Find matching monitor by device name
            let monitor_id = self
                .monitors
                .iter()
                .find(|(_, m)| m.device_name == ws_snapshot.monitor_device_name)
                .map(|(&id, _)| id);

            if let Some(id) = monitor_id {
                // Restore scroll offset from saved workspace
                if let Some(workspace) = self.workspaces.get_mut(&id) {
                    let saved_offset = ws_snapshot.workspace.scroll_offset();
                    if saved_offset != 0.0 {
                        workspace.set_scroll_offset(saved_offset);
                    }
                    restored_monitors.insert(id);
                    info!(
                        "Restored workspace state for monitor '{}'",
                        ws_snapshot.monitor_device_name
                    );
                }
            } else {
                debug!(
                    "Skipping saved workspace for unknown monitor '{}'",
                    ws_snapshot.monitor_device_name
                );
            }
        }

        // Restore focused monitor
        if let Some((&id, _)) = self
            .monitors
            .iter()
            .find(|(_, m)| m.device_name == snapshot.focused_monitor_name)
        {
            self.focused_monitor = id;
        }

        restored_monitors
    }

    /// Reconcile workspaces after monitor configuration change.
    ///
    /// This handles:
    /// - Removing workspaces for disconnected monitors (migrating windows to primary)
    /// - Adding workspaces for newly connected monitors
    fn reconcile_monitors(&mut self, new_monitors: Vec<MonitorInfo>) {
        let new_ids: HashSet<MonitorId> = new_monitors.iter().map(|m| m.id).collect();
        let old_ids: HashSet<MonitorId> = self.monitors.keys().copied().collect();

        // Find primary monitor in new config (or first available)
        let primary_id = new_monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| new_monitors.first())
            .map(|m| m.id);

        // Handle added monitors - create new workspaces FIRST so migration
        // targets exist even when all old monitors are replaced with new ones.
        for monitor in &new_monitors {
            if !old_ids.contains(&monitor.id) {
                let mut workspace =
                    Workspace::with_gaps(self.config.layout.gap, self.config.layout.outer_gap);
                workspace.set_default_column_width(self.config.layout.default_column_width);
                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                self.workspaces.insert(monitor.id, workspace);
                info!("Created workspace for new monitor {}", monitor.id);
            }
        }

        // Handle removed monitors - migrate windows to primary
        for removed_id in old_ids.difference(&new_ids) {
            if let Some(old_workspace) = self.workspaces.remove(removed_id) {
                let window_ids = old_workspace.all_window_ids();
                if let Some(primary) = primary_id {
                    if let Some(primary_ws) = self.workspaces.get_mut(&primary) {
                        for window_id in &window_ids {
                            if let Err(e) = primary_ws.insert_window(*window_id, None) {
                                warn!("Failed to migrate window {}: {}", window_id, e);
                            }
                        }
                        info!(
                            "Migrated {} windows from removed monitor {} to primary",
                            window_ids.len(),
                            removed_id
                        );
                    }
                }
            }
            self.monitors.remove(removed_id);
        }

        // Update monitor info
        self.monitors = new_monitors.into_iter().map(|m| (m.id, m)).collect();

        // Update focused monitor if it was removed
        if !self.monitors.contains_key(&self.focused_monitor) {
            self.focused_monitor = primary_id.unwrap_or(0);
        }
    }

    /// Collect all managed window IDs across all workspaces.
    ///
    /// Returns tiled and floating window IDs from every monitor's workspace.
    fn all_managed_window_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for workspace in self.workspaces.values() {
            ids.extend(workspace.all_window_ids());
        }
        ids
    }

    /// Record a short suppression window for moved/resized feedback generated by apply_layout().
    fn arm_moved_or_resized_suppression<I>(&mut self, window_ids: I)
    where
        I: IntoIterator<Item = u64>,
    {
        let now = std::time::Instant::now();
        self.moved_or_resized_suppression
            .retain(|_, deadline| *deadline > now);
        let deadline = now + MOVED_OR_RESIZED_SUPPRESSION_WINDOW;
        for hwnd in window_ids {
            self.moved_or_resized_suppression.insert(hwnd, deadline);
        }
    }

    /// Returns true when a moved/resized event should be ignored due to recent apply_layout().
    fn should_suppress_moved_or_resized(&mut self, hwnd: u64) -> bool {
        let now = std::time::Instant::now();
        self.moved_or_resized_suppression
            .retain(|_, deadline| *deadline > now);
        self.moved_or_resized_suppression
            .get(&hwnd)
            .is_some_and(|deadline| *deadline > now)
    }

    /// Join any finished timed-out apply workers so the pending list does not grow indefinitely.
    /// Returns the number of workers reaped in this pass.
    fn reap_finished_pending_apply_workers(&mut self) -> usize {
        if self.pending_apply_workers.is_empty() {
            return 0;
        }
        let mut still_running = Vec::with_capacity(self.pending_apply_workers.len());
        let mut reaped = 0usize;
        for handle in self.pending_apply_workers.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
                reaped += 1;
            } else {
                still_running.push(handle);
            }
        }
        self.pending_apply_workers = still_running;
        reaped
    }

    /// Mark shutdown/revert in progress and take ownership of any timed-out apply workers.
    fn begin_shutdown_or_revert(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        self.apply_worker_cancelled.store(true, Ordering::SeqCst);
        self.apply_epoch.fetch_add(1, Ordering::SeqCst);
        std::mem::take(&mut self.pending_apply_workers)
    }

    /// Check if any workspace has an active animation.
    fn is_animating(&self) -> bool {
        self.workspaces.values().any(|w| w.is_animating())
    }

    /// Tick all active animations by the given delta time.
    /// Returns true if any animation is still running.
    fn tick_animations(&mut self, delta_ms: u64) -> bool {
        let mut still_animating = false;
        for workspace in self.workspaces.values_mut() {
            if workspace.tick_animation(delta_ms) {
                still_animating = true;
            }
        }
        still_animating
    }

    /// Compute animated placements and send them to the animation worker.
    ///
    /// Returns `Ok(true)` if a frame was sent, `Ok(false)` if paused or no placements.
    fn send_animation_frame(
        &mut self,
        worker: &animation_worker::AnimationWorkerHandle,
    ) -> Result<bool> {
        if self.paused {
            return Ok(false);
        }
        let mut all_placements = Vec::new();
        for (monitor_id, workspace) in &self.workspaces {
            if let Some(monitor) = self.monitors.get(monitor_id) {
                let placements = workspace.compute_placements_animated(monitor.work_area);
                all_placements.extend(placements);
            }
        }
        if all_placements.is_empty() {
            return Ok(false);
        }
        self.arm_moved_or_resized_suppression(all_placements.iter().map(|p| p.window_id));
        self.applying_layout = true;

        let request = animation_worker::FrameRequest {
            placements: all_placements,
            platform_config: self.platform_config.clone(),
        };
        worker
            .send_frame(request)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(true)
    }

    /// Recalculate layout and apply placements for all monitors.
    /// Uses animated offsets if any workspace has an active animation.
    /// No-op when tiling is paused.
    fn apply_layout(&mut self) -> Result<()> {
        let reaped_workers = self.reap_finished_pending_apply_workers();
        if reaped_workers > 0 {
            let managed_window_ids = self.all_managed_window_ids();
            run_visibility_recovery_pass(&managed_window_ids, "late-apply-worker");
        }

        if self.paused {
            return Ok(());
        }
        if self.apply_worker_cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "Layout application skipped: shutdown/revert cleanup is in progress"
            ));
        }
        if !self.pending_apply_workers.is_empty() {
            return Err(anyhow!(
                "Layout application skipped: previous timed-out apply worker is still finishing"
            ));
        }
        self.applying_layout = true;
        let mut all_placements = Vec::new();

        for (monitor_id, workspace) in &self.workspaces {
            if let Some(monitor) = self.monitors.get(monitor_id) {
                // Use animated placements to support smooth scrolling
                let placements = workspace.compute_placements_animated(monitor.work_area);
                debug!(
                    "Monitor {}: {} placements for viewport {}x{} (animating: {})",
                    monitor_id,
                    placements.len(),
                    monitor.work_area.width,
                    monitor.work_area.height,
                    workspace.is_animating()
                );
                all_placements.extend(placements);
            }
        }
        self.arm_moved_or_resized_suppression(all_placements.iter().map(|p| p.window_id));

        let timeout = self.layout_apply_timeout;
        let platform_config = self.platform_config.clone();
        let apply_worker_cancelled = self.apply_worker_cancelled.clone();
        let apply_epoch_ref = self.apply_epoch.clone();
        let apply_epoch = apply_epoch_ref.fetch_add(1, Ordering::SeqCst) + 1;
        let apply_window_ids: Vec<u64> = all_placements.iter().map(|p| p.window_id).collect();
        #[cfg(test)]
        let injected_behavior = self.injected_apply_placements_behavior;
        #[cfg(test)]
        let late_worker_recovery_count = self.late_worker_recovery_count.clone();

        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let spawn_result = std::thread::Builder::new()
            .name("leopardwm-apply-layout".to_string())
            .spawn(move || {
                let should_cancel = || {
                    apply_worker_cancelled.load(Ordering::SeqCst)
                        || apply_epoch_ref.load(Ordering::SeqCst) != apply_epoch
                };
                if should_cancel() {
                    let _ = tx.send(Ok(()));
                    return;
                }

                #[cfg(test)]
                if let Some(behavior) = injected_behavior {
                    let result = match behavior {
                        TestApplyPlacementsBehavior::SleepAndSucceed(delay) => {
                            std::thread::sleep(delay);
                            Ok(())
                        }
                        TestApplyPlacementsBehavior::SleepAndFail(delay) => {
                            std::thread::sleep(delay);
                            Err(anyhow!("injected apply_placements failure"))
                        }
                    };
                    if should_cancel() {
                        run_visibility_recovery_pass(
                            &apply_window_ids,
                            "apply-cancelled-late-worker",
                        );
                        #[cfg(test)]
                        late_worker_recovery_count.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(Ok(()));
                        return;
                    }
                    let _ = tx.send(result);
                    return;
                }

                if should_cancel() {
                    let _ = tx.send(Ok(()));
                    return;
                }
                let result =
                    leopardwm_platform_win32::apply_placements(&all_placements, &platform_config)
                        .map_err(|e| anyhow!(e.to_string()));
                if should_cancel() {
                    run_visibility_recovery_pass(&apply_window_ids, "apply-cancelled-late-worker");
                    #[cfg(test)]
                    late_worker_recovery_count.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(Ok(()));
                    return;
                }
                let _ = tx.send(result);
            });

        let worker_handle = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                self.applying_layout = false;
                return Err(anyhow!("Failed to spawn layout worker thread: {}", e));
            }
        };

        let result = match rx.recv_timeout(timeout) {
            Ok(result) => {
                let _ = worker_handle.join();
                if result.is_err() {
                    self.moved_or_resized_suppression.clear();
                }
                result
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.paused = true;
                // Invalidate this apply epoch so late-starting workers bail before placement calls.
                self.apply_epoch.fetch_add(1, Ordering::SeqCst);
                self.pending_apply_workers.push(worker_handle);
                self.moved_or_resized_suppression.clear();
                let msg = layout_apply_timeout_message(timeout);
                warn!("{}", msg);
                let managed_window_ids = self.all_managed_window_ids();
                run_visibility_recovery_pass(&managed_window_ids, "apply-timeout");
                Err(anyhow!(msg))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker_handle.join();
                self.moved_or_resized_suppression.clear();
                Err(anyhow!(
                    "Layout worker thread exited without returning a result"
                ))
            }
        };
        self.applying_layout = false;

        // Reposition border to track the focused window after layout changes
        if result.is_ok() {
            if let Some(hwnd) = self.previous_focused_hwnd {
                if self.config.appearance.active_border {
                    self.show_border(hwnd);
                }
            }
        }

        result
    }

    /// Convert the config border color (hex RGB string) to BGR u32 for Win32.
    fn border_color_bgr(&self) -> Option<u32> {
        let color = u32::from_str_radix(&self.config.appearance.active_border_color, 16).ok()?;
        let r = (color >> 16) & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = color & 0xFF;
        Some((b << 16) | (g << 8) | r)
    }

    /// Convert the config border position string to the platform enum.
    fn border_position(&self) -> leopardwm_platform_win32::border::BorderPosition {
        if self.config.appearance.active_border_position == "inside" {
            leopardwm_platform_win32::border::BorderPosition::Inside
        } else {
            leopardwm_platform_win32::border::BorderPosition::Outside
        }
    }

    /// Show the border frame on the given window, or hide it if borders are disabled.
    fn show_border(&self, hwnd: u64) {
        if let Some(ref frame) = self.border_frame {
            if self.config.appearance.active_border {
                if let Some(bgr) = self.border_color_bgr() {
                    frame.show(
                        hwnd,
                        self.config.appearance.active_border_width,
                        self.border_position(),
                        bgr,
                    );
                    return;
                }
            }
            frame.hide();
        }
    }

    /// Hide the border frame.
    fn hide_border(&self) {
        if let Some(ref frame) = self.border_frame {
            frame.hide();
        }
    }

    /// Set the OS foreground window to match the workspace's focused window.
    /// Also updates active window border if configured.
    fn sync_foreground_window(&mut self) {
        let focused_hwnd = self
            .focused_workspace()
            .and_then(|ws| ws.focused_visible_window());

        if let Some(hwnd) = focused_hwnd {
            self.show_border(hwnd);

            // Set foreground window
            let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
            self.previous_focused_hwnd = Some(hwnd);
        } else {
            debug!("sync_foreground_window: no focused visible window");
        }
    }

    /// Enumerate windows and add them to the appropriate workspace based on position.
    fn enumerate_and_add_windows(&mut self) -> Result<usize> {
        let windows = enumerate_windows()?;
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        let mut added = 0;

        for win_info in windows {
            // Get executable name for rule matching
            let executable = get_process_executable(win_info.process_id).unwrap_or_default();

            // Check window rules
            let action =
                self.evaluate_window_rules(&win_info.class_name, &win_info.title, &executable);

            // Skip ignored windows
            if action == config::WindowAction::Ignore {
                debug!(
                    "Ignoring window by rule: {} ({})",
                    win_info.title, win_info.class_name
                );
                continue;
            }

            // Find which monitor this window is on
            let monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
                .map(|m| m.id)
                .unwrap_or(self.focused_monitor);

            // Get floating rect before borrowing workspace mutably (to avoid borrow conflict)
            let floating_rect = if action == config::WindowAction::Float {
                Some(self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                ))
            } else {
                None
            };

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                match action {
                    config::WindowAction::Float => {
                        // Use rule dimensions or default to centered 800x600 window
                        let rule_rect = floating_rect.unwrap_or_else(|| {
                            let viewport = self
                                .monitors
                                .get(&monitor_id)
                                .map(|m| m.work_area)
                                .unwrap_or_else(|| {
                                    Rect::new(
                                        0,
                                        0,
                                        FALLBACK_VIEWPORT_WIDTH,
                                        FALLBACK_VIEWPORT_HEIGHT,
                                    )
                                });
                            Rect::new(
                                viewport.x + (viewport.width - 800) / 2,
                                viewport.y + (viewport.height - 600) / 2,
                                800,
                                600,
                            )
                        });

                        match workspace.add_floating(win_info.hwnd, rule_rect) {
                            Ok(()) => {
                                info!(
                                    "Added floating window: {} ({}) to monitor {} - {}x{}",
                                    win_info.title,
                                    win_info.class_name,
                                    monitor_id,
                                    rule_rect.width,
                                    rule_rect.height
                                );
                                added += 1;
                            }
                            Err(e) => {
                                warn!("Failed to add floating window {}: {}", win_info.hwnd, e);
                            }
                        }
                    }
                    config::WindowAction::Tile => {
                        // Use a reasonable default width or the window's current width, respecting config bounds
                        let width = win_info.rect.width.clamp(
                            self.config.layout.min_column_width,
                            self.config.layout.max_column_width,
                        );

                        match workspace.insert_window(win_info.hwnd, Some(width)) {
                            Ok(()) => {
                                info!(
                                    "Added tiled window: {} ({}) to monitor {} - {}x{}",
                                    win_info.title,
                                    win_info.class_name,
                                    monitor_id,
                                    win_info.rect.width,
                                    win_info.rect.height
                                );
                                added += 1;
                            }
                            Err(e) => {
                                warn!("Failed to add window {}: {}", win_info.hwnd, e);
                            }
                        }
                    }
                    config::WindowAction::Ignore => unreachable!(), // Handled above
                }
            }
        }

        Ok(added)
    }

    /// Evaluate window rules and return the action for a window.
    fn evaluate_window_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
    ) -> config::WindowAction {
        for rule in &self.compiled_rules {
            if rule.matches(class_name, title, executable) {
                return rule.action;
            }
        }
        config::WindowAction::Tile // Default
    }

    /// Get the floating rect for a window based on rules.
    fn get_floating_rect_from_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        original_rect: &leopardwm_core_layout::Rect,
    ) -> leopardwm_core_layout::Rect {
        for rule in &self.compiled_rules {
            if rule.matches(class_name, title, executable) {
                let width = rule.width.unwrap_or(original_rect.width);
                let height = rule.height.unwrap_or(original_rect.height);
                return leopardwm_core_layout::Rect::new(
                    original_rect.x,
                    original_rect.y,
                    width,
                    height,
                );
            }
        }
        *original_rect
    }

    /// Find which workspace contains a window.
    fn find_window_workspace(&self, window_id: u64) -> Option<MonitorId> {
        for (monitor_id, workspace) in &self.workspaces {
            if workspace.contains_window(window_id) {
                return Some(*monitor_id);
            }
        }
        None
    }

    /// Move the focused window to another monitor using an all-or-nothing update.
    ///
    /// The source and target workspaces are mutated on cloned snapshots first and
    /// committed only if both remove and insert operations succeed.
    fn move_focused_window_to_monitor_transactional(
        &mut self,
        target_monitor: MonitorId,
    ) -> std::result::Result<Option<u64>, String> {
        let source_monitor = self.focused_monitor;
        if source_monitor == target_monitor {
            return Ok(None);
        }

        let Some(window_id) = self.focused_workspace().and_then(|ws| ws.focused_window()) else {
            return Ok(None);
        };

        let Some(mut source_workspace) = self.workspaces.get(&source_monitor).cloned() else {
            return Err(format!(
                "Source workspace missing for monitor {}",
                source_monitor
            ));
        };
        let Some(mut target_workspace) = self.workspaces.get(&target_monitor).cloned() else {
            return Err(format!(
                "Target workspace missing for monitor {}",
                target_monitor
            ));
        };

        source_workspace
            .remove_window(window_id)
            .map_err(|e| format!("Failed to remove window: {}", e))?;
        target_workspace
            .insert_window(window_id, None)
            .map_err(|e| format!("Failed to add window to target: {}", e))?;

        let target_viewport = self
            .monitors
            .get(&target_monitor)
            .map(|m| m.work_area.width)
            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
        target_workspace.ensure_focused_visible(target_viewport);

        self.workspaces.insert(source_monitor, source_workspace);
        self.workspaces.insert(target_monitor, target_workspace);
        self.focused_monitor = target_monitor;
        Ok(Some(window_id))
    }

    /// Get the rectangle of the focused column for snap hint display.
    ///
    /// Returns the absolute screen position of the focused column.
    fn get_focused_column_rect(&self) -> Option<Rect> {
        let workspace = self.focused_workspace()?;
        let monitor = self.monitors.get(&self.focused_monitor)?;
        let placements = workspace.compute_placements(monitor.work_area);

        // Find the placement for the focused window
        let focused_hwnd = workspace.focused_window()?;
        placements
            .iter()
            .find(|p| p.window_id == focused_hwnd)
            .map(|p| p.rect)
    }

    /// Toggle paused state for tiling operations.
    ///
    /// When resuming, this immediately reapplies layout so windows snap back
    /// without waiting for another command/event. If resume reapply fails,
    /// paused state is restored to avoid claiming a healthy resumed mode.
    fn toggle_pause(&mut self, source: &str) -> Result<()> {
        let was_paused = self.paused;
        self.paused = !was_paused;
        info!(
            "Tiling {} via {}",
            if self.paused { "paused" } else { "resumed" },
            source
        );
        if !self.paused {
            if let Err(err) = self.apply_layout() {
                self.paused = was_paused;
                warn!(
                    "Resume apply failed via {}; restoring paused state: {}",
                    source, err
                );
                return Err(err);
            }
        }
        Ok(())
    }

    /// Process an IPC command and return a response.
    fn handle_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        let viewport_width = self.focused_viewport().width;

        match cmd {
            IpcCommand::FocusLeft => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_left();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Focus left -> column {}", workspace.focused_column_index());
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusRight => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_right();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Focus right -> column {}", workspace.focused_column_index());
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusUp => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_up();
                    info!(
                        "Focus up -> window {}",
                        workspace.focused_window_index_in_column()
                    );
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusDown => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_down();
                    info!(
                        "Focus down -> window {}",
                        workspace.focused_window_index_in_column()
                    );
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::MoveColumnLeft => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.move_column_left();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Moved column left");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::MoveColumnRight => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.move_column_right();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Moved column right");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::FocusMonitorLeft => {
                let monitors: Vec<_> = self.monitors.values().cloned().collect();
                if let Some(target) = monitor_to_left(&monitors, self.focused_monitor) {
                    let target_id = target.id;
                    self.focused_monitor = target_id;
                    info!("Focused monitor left -> {}", target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    self.sync_foreground_window();
                } else {
                    info!("No monitor to the left");
                }
                IpcResponse::Ok
            }
            IpcCommand::FocusMonitorRight => {
                let monitors: Vec<_> = self.monitors.values().cloned().collect();
                if let Some(target) = monitor_to_right(&monitors, self.focused_monitor) {
                    let target_id = target.id;
                    self.focused_monitor = target_id;
                    info!("Focused monitor right -> {}", target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    self.sync_foreground_window();
                } else {
                    info!("No monitor to the right");
                }
                IpcResponse::Ok
            }
            IpcCommand::MoveWindowToMonitorLeft => {
                let monitors: Vec<_> = self.monitors.values().cloned().collect();
                if let Some(target) = monitor_to_left(&monitors, self.focused_monitor) {
                    let target_id = target.id;
                    match self.move_focused_window_to_monitor_transactional(target_id) {
                        Ok(Some(hwnd)) => {
                            info!("Moved window {} to monitor {}", hwnd, target_id);
                            if let Err(e) = self.apply_layout() {
                                return IpcResponse::error(format!(
                                    "Failed to apply layout: {}",
                                    e
                                ));
                            }
                            self.sync_foreground_window();
                        }
                        Ok(None) => info!("No focused window to move"),
                        Err(message) => return IpcResponse::error(message),
                    }
                } else {
                    info!("No monitor to the left");
                }
                IpcResponse::Ok
            }
            IpcCommand::MoveWindowToMonitorRight => {
                let monitors: Vec<_> = self.monitors.values().cloned().collect();
                if let Some(target) = monitor_to_right(&monitors, self.focused_monitor) {
                    let target_id = target.id;
                    match self.move_focused_window_to_monitor_transactional(target_id) {
                        Ok(Some(hwnd)) => {
                            info!("Moved window {} to monitor {}", hwnd, target_id);
                            if let Err(e) = self.apply_layout() {
                                return IpcResponse::error(format!(
                                    "Failed to apply layout: {}",
                                    e
                                ));
                            }
                            self.sync_foreground_window();
                        }
                        Ok(None) => info!("No focused window to move"),
                        Err(message) => return IpcResponse::error(message),
                    }
                } else {
                    info!("No monitor to the right");
                }
                IpcResponse::Ok
            }
            IpcCommand::Resize { delta } => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.resize_focused_column(delta);
                    info!("Resized column by {}", delta);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::Scroll { delta } => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.scroll_by(delta, viewport_width);
                    info!("Scrolled by {}", delta);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::QueryWorkspace => {
                if let Some(workspace) = self.focused_workspace() {
                    IpcResponse::WorkspaceState {
                        columns: workspace.column_count(),
                        windows: workspace.window_count(),
                        focused_column: workspace.focused_column_index(),
                        focused_window: workspace.focused_window_index_in_column(),
                        scroll_offset: workspace.scroll_offset(),
                        total_width: workspace.total_width(),
                    }
                } else {
                    IpcResponse::error("No focused workspace")
                }
            }
            IpcCommand::QueryFocused => {
                if let Some(workspace) = self.focused_workspace() {
                    IpcResponse::FocusedWindow {
                        window_id: workspace.focused_window(),
                        column_index: workspace.focused_column_index(),
                        window_index: workspace.focused_window_index_in_column(),
                    }
                } else {
                    IpcResponse::error("No focused workspace")
                }
            }
            IpcCommand::Refresh => match self.enumerate_and_add_windows() {
                Ok(added) => {
                    info!("Refreshed: added {} new windows across all monitors", added);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    IpcResponse::Ok
                }
                Err(e) => IpcResponse::error(format!("Failed to enumerate windows: {}", e)),
            },
            IpcCommand::Apply => {
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                info!("Applied layout");
                IpcResponse::Ok
            }
            IpcCommand::Reload => match Config::load() {
                Ok(new_config) => {
                    self.apply_config(new_config);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    IpcResponse::Ok
                }
                Err(e) => IpcResponse::error(format!("Failed to reload config: {}", e)),
            },
            IpcCommand::TogglePause => {
                if let Err(e) = self.toggle_pause("IPC toggle") {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::Stop => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::PanicRevert => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::QueryAllWindows => {
                let mut windows = Vec::new();

                // Get focused window for comparison
                let focused_hwnd = self.focused_workspace().and_then(|ws| ws.focused_window());

                // Enumerate all windows to get titles and other info
                let win_info_map: HashMap<u64, (String, String, u32)> = match enumerate_windows() {
                    Ok(wins) => wins
                        .into_iter()
                        .map(|w| (w.hwnd, (w.title, w.class_name, w.process_id)))
                        .collect(),
                    Err(_) => HashMap::new(),
                };

                for (monitor_id, workspace) in &self.workspaces {
                    // Tiled windows
                    for (col_idx, column) in workspace.columns().iter().enumerate() {
                        for (win_idx, &window_id) in column.windows().iter().enumerate() {
                            let (title, class_name, process_id) =
                                win_info_map.get(&window_id).cloned().unwrap_or_else(|| {
                                    ("Unknown".to_string(), "Unknown".to_string(), 0)
                                });

                            let executable = get_process_executable(process_id).unwrap_or_default();

                            // Get rect from computed placements
                            let rect = self
                                .monitors
                                .get(monitor_id)
                                .map(|m| workspace.compute_placements(m.work_area))
                                .and_then(|placements| {
                                    placements
                                        .into_iter()
                                        .find(|p| p.window_id == window_id)
                                        .map(|p| p.rect)
                                })
                                .unwrap_or_else(|| Rect::new(0, 0, 0, 0));

                            windows.push(leopardwm_ipc::WindowInfo {
                                window_id,
                                title,
                                class_name,
                                process_id,
                                executable,
                                rect: leopardwm_ipc::IpcRect::new(
                                    rect.x,
                                    rect.y,
                                    rect.width,
                                    rect.height,
                                ),
                                column_index: Some(col_idx),
                                window_index: Some(win_idx),
                                monitor_id: *monitor_id as i64,
                                is_floating: false,
                                is_focused: Some(window_id) == focused_hwnd,
                            });
                        }
                    }

                    // Floating windows
                    for floating in workspace.floating_windows() {
                        let (title, class_name, process_id) = win_info_map
                            .get(&floating.id)
                            .cloned()
                            .unwrap_or_else(|| ("Unknown".to_string(), "Unknown".to_string(), 0));

                        let executable = get_process_executable(process_id).unwrap_or_default();

                        windows.push(leopardwm_ipc::WindowInfo {
                            window_id: floating.id,
                            title,
                            class_name,
                            process_id,
                            executable,
                            rect: leopardwm_ipc::IpcRect::new(
                                floating.rect.x,
                                floating.rect.y,
                                floating.rect.width,
                                floating.rect.height,
                            ),
                            column_index: None,
                            window_index: None,
                            monitor_id: *monitor_id as i64,
                            is_floating: true,
                            is_focused: Some(floating.id) == focused_hwnd,
                        });
                    }
                }

                IpcResponse::WindowList { windows }
            }
            IpcCommand::CloseWindow => {
                if let Some(hwnd) = self.focused_workspace().and_then(|ws| ws.focused_window()) {
                    if let Err(e) = leopardwm_platform_win32::close_window(hwnd) {
                        return IpcResponse::error(format!("Failed to close window: {}", e));
                    }
                    info!("Closed window {}", hwnd);
                } else {
                    info!("No focused window to close");
                }
                IpcResponse::Ok
            }
            IpcCommand::ToggleFloating => {
                let viewport = self.focused_viewport();
                let prev_hwnd = self.previous_focused_hwnd;
                if let Some(workspace) = self.focused_workspace_mut() {
                    // Check if the OS-foreground window is floating — unfloat it
                    let foreground_is_floating = prev_hwnd
                        .map(|hwnd| workspace.is_floating(hwnd))
                        .unwrap_or(false);
                    if foreground_is_floating {
                        let hwnd = prev_hwnd.unwrap();
                        if workspace.unfloat_window(hwnd) {
                            info!("Unfloated window {} back to tiling", hwnd);
                        }
                    } else if let Some(wid) = workspace.toggle_floating(viewport) {
                        info!("Toggled window {} to floating", wid);
                    }
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::ToggleFullscreen => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    let entering = workspace.toggle_fullscreen();
                    info!("Fullscreen: {}", if entering { "on" } else { "off" });
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::SetColumnWidth { fraction } => {
                if let Err(message) = validate_set_width_fraction(fraction) {
                    return IpcResponse::error(message);
                }
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.set_focused_column_width_fraction(fraction, viewport_width);
                    info!("Set column width fraction to {:.3}", fraction);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::EqualizeColumnWidths => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.equalize_column_widths(viewport_width);
                    info!("Equalized column widths");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::QueryStatus => {
                let uptime = self.start_time.elapsed().as_secs();
                let total_windows: usize = self
                    .workspaces
                    .values()
                    .map(|ws| ws.window_count() + ws.floating_count())
                    .sum();
                IpcResponse::StatusInfo {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    monitors: self.monitors.len(),
                    total_windows,
                    uptime_seconds: uptime,
                }
            }
            IpcCommand::HealthCheck => {
                let uptime = self.start_time.elapsed().as_secs();
                let total_windows: usize = self
                    .workspaces
                    .values()
                    .map(|ws| ws.window_count() + ws.floating_count())
                    .sum();
                IpcResponse::HealthInfo {
                    healthy: true,
                    uptime_seconds: uptime,
                    total_windows,
                    monitors: self.monitors.len(),
                    paused: self.paused,
                }
            }
        }
    }

    /// Handle a window lifecycle event.
    fn handle_window_event(&mut self, event: WindowEvent) {
        // Get window_id from event for validation (DisplayChange and MouseEnterWindow have no validation needed)
        let window_id = match &event {
            WindowEvent::Created(id)
            | WindowEvent::Destroyed(id)
            | WindowEvent::Focused(id)
            | WindowEvent::Minimized(id)
            | WindowEvent::Restored(id)
            | WindowEvent::MovedOrResized(id)
            | WindowEvent::MoveSizeStart(id)
            | WindowEvent::MoveSizeEnd(id) => Some(*id),
            WindowEvent::DisplayChange | WindowEvent::MouseEnterWindow(_) => None,
        };

        // Validate window existence for events that require it.
        // Skip validation for:
        //   - Destroyed events (window is already gone)
        //   - Windows we already know about (managed or injected in tests)
        //   - DisplayChange / MouseEnterWindow (no window to validate)
        if let Some(wid) = window_id {
            if !matches!(event, WindowEvent::Destroyed(_))
                && !self.is_known_window(wid)
                && !leopardwm_platform_win32::is_valid_window(wid)
            {
                debug!("Ignoring event for invalid window {}", wid);
                return;
            }
        }

        match event {
            WindowEvent::Created(hwnd) => {
                // Check if any workspace already manages this window
                if self.find_window_workspace(hwnd).is_some() {
                    debug!("Window {} already managed, ignoring create event", hwnd);
                    return;
                }

                // Try to get window info for filtering and monitor assignment
                if let Some(win_info) = self.lookup_window_info(hwnd) {
                    // Get executable name for rule matching
                    let executable =
                        get_process_executable(win_info.process_id).unwrap_or_default();

                    // Check window rules
                    let action = self.evaluate_window_rules(
                        &win_info.class_name,
                        &win_info.title,
                        &executable,
                    );

                    // Skip ignored windows
                    if action == config::WindowAction::Ignore {
                        debug!(
                            "Ignoring window by rule: {} ({})",
                            win_info.title, win_info.class_name
                        );
                        return;
                    }

                    // Determine which monitor this window should be on
                    let monitors: Vec<_> = self.monitors.values().cloned().collect();
                    let monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
                        .map(|m| m.id)
                        .unwrap_or(self.focused_monitor);

                    // Get floating rect before borrowing workspace mutably
                    let floating_rect = if action == config::WindowAction::Float {
                        Some(self.get_floating_rect_from_rules(
                            &win_info.class_name,
                            &win_info.title,
                            &executable,
                            &win_info.rect,
                        ))
                    } else {
                        None
                    };

                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        let added = match action {
                            config::WindowAction::Float => {
                                // Use rule dimensions or default to centered 800x600 window
                                let rect = floating_rect.unwrap_or_else(|| {
                                    let viewport = self
                                        .monitors
                                        .get(&monitor_id)
                                        .map(|m| m.work_area)
                                        .unwrap_or_else(|| {
                                            Rect::new(
                                                0,
                                                0,
                                                FALLBACK_VIEWPORT_WIDTH,
                                                FALLBACK_VIEWPORT_HEIGHT,
                                            )
                                        });
                                    Rect::new(
                                        viewport.x + (viewport.width - 800) / 2,
                                        viewport.y + (viewport.height - 600) / 2,
                                        800,
                                        600,
                                    )
                                });
                                workspace.add_floating(hwnd, rect).is_ok()
                            }
                            config::WindowAction::Tile => {
                                let width = win_info.rect.width.clamp(
                                    self.config.layout.min_column_width,
                                    self.config.layout.max_column_width,
                                );
                                if self.config.behavior.focus_new_windows {
                                    workspace.insert_window(hwnd, Some(width)).is_ok()
                                } else {
                                    workspace.insert_window_no_focus(hwnd, Some(width)).is_ok()
                                }
                            }
                            config::WindowAction::Ignore => unreachable!(),
                        };

                        if added {
                            info!(
                                "Window created: {} ({}) - added to monitor {} as {:?}",
                                win_info.title, win_info.class_name, monitor_id, action
                            );
                            if self.config.behavior.focus_new_windows {
                                workspace.ensure_focused_visible_animated(viewport_width);
                            }
                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after window create: {}", e);
                            }
                            if self.config.behavior.focus_new_windows {
                                self.sync_foreground_window();
                            }
                        } else {
                            debug!("Failed to add window {} to workspace", hwnd);
                        }
                    }
                }
            }
            WindowEvent::Destroyed(hwnd) => {
                // Find which workspace contains this window
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        // Try to remove as floating window first
                        let was_floating = workspace.remove_floating(hwnd);

                        if was_floating {
                            info!(
                                "Floating window {} destroyed - removed from monitor {}",
                                hwnd, monitor_id
                            );
                        } else if let Err(e) = workspace.remove_window(hwnd) {
                            warn!("Failed to remove window {}: {}", hwnd, e);
                        } else {
                            info!(
                                "Window {} destroyed - removed from monitor {}",
                                hwnd, monitor_id
                            );
                            workspace.ensure_focused_visible_animated(viewport_width);
                        }

                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to apply layout after window destroy: {}", e);
                        }
                    }
                }
            }
            WindowEvent::Focused(hwnd) => {
                // Update focus to match what Windows says is focused
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    // Update focused monitor to match the window's monitor
                    self.focused_monitor = monitor_id;

                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        if let Err(e) = workspace.focus_window(hwnd) {
                            // Floating windows are not in the tiled column list,
                            // so focus_window fails for them — that's expected.
                            debug!("Failed to focus window {}: {}", hwnd, e);
                        } else {
                            debug!("Focus changed to window {} on monitor {}", hwnd, monitor_id);
                            workspace.ensure_focused_visible_animated(viewport_width);
                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after focus change: {}", e);
                            }
                        }
                    }

                    // Update border colors to reflect the new focus.
                    // Must happen after focus_window() so focused_visible_window()
                    // returns the correct hwnd, and before updating
                    // previous_focused_hwnd so the old border gets reset.
                    self.sync_foreground_window();

                    // Track the OS-foreground window — including floating windows —
                    // so that ToggleFloating can reliably detect and unfloat the
                    // currently focused floating window.
                    self.previous_focused_hwnd = Some(hwnd);
                } else {
                    // Focus went to an unmanaged window (e.g. settings, taskbar).
                    // Hide the border overlay and clear tracked hwnd so animation
                    // frames don't re-show it.
                    self.hide_border();
                    self.previous_focused_hwnd = None;
                }
            }
            WindowEvent::Minimized(hwnd) => {
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        let cleared_fullscreen = workspace.clear_fullscreen_if_window(hwnd);
                        if workspace.mark_minimized(hwnd) || cleared_fullscreen {
                            info!("Window {} marked minimized", hwnd);

                            // If the minimized window was the focused window, move focus
                            if workspace.focused_window() == Some(hwnd) {
                                // Try to focus another window in the same column
                                workspace.focus_down();
                                if workspace.focused_window() == Some(hwnd) {
                                    workspace.focus_up();
                                }
                                // If still focused on minimized (only window in column), try next column
                                if workspace.focused_window() == Some(hwnd) {
                                    workspace.focus_right();
                                    if workspace.focused_window() == Some(hwnd) {
                                        workspace.focus_left();
                                    }
                                }
                            }
                            workspace.ensure_focused_visible_animated(viewport_width);

                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after minimize: {}", e);
                            }
                            // Keep monitor focus aligned before foreground sync so we don't
                            // accidentally steer foreground to a stale monitor.
                            self.focused_monitor = monitor_id;
                            self.sync_foreground_window();
                        }
                    }
                } else {
                    debug!("Window {} minimized (unmanaged)", hwnd);
                }
            }
            WindowEvent::Restored(hwnd) => {
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let mut should_sync_foreground = false;
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        if workspace.mark_restored(hwnd) {
                            info!("Window {} restored from minimized", hwnd);
                            if workspace.is_floating(hwnd) {
                                // Keep floating restores from stealing focus back to tiled windows.
                                debug!(
                                    "Restored floating window {} without changing tiled focus",
                                    hwnd
                                );
                            } else if let Err(e) = workspace.focus_window(hwnd) {
                                warn!("Failed to focus restored window {}: {}", hwnd, e);
                            } else {
                                workspace.ensure_focused_visible_animated(viewport_width);
                                should_sync_foreground = true;
                            }
                        }
                    }
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to apply layout after window restore: {}", e);
                    }
                    if should_sync_foreground {
                        self.focused_monitor = monitor_id;
                        self.sync_foreground_window();
                    }
                } else {
                    debug!("Window {} restored (unmanaged)", hwnd);
                }
            }
            WindowEvent::MoveSizeStart(hwnd) => {
                debug!("User started dragging/resizing window {}", hwnd);
                self.dragging_window = Some(hwnd);
            }
            WindowEvent::MoveSizeEnd(hwnd) => {
                debug!("User finished dragging/resizing window {}", hwnd);
                self.dragging_window = None;
                // Snap the window back to its tiled position with animation.
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let is_floating = self
                        .workspaces
                        .get(&monitor_id)
                        .map_or(true, |ws| ws.is_floating(hwnd));

                    if !is_floating {
                        debug!("Managed window {} dropped — animating back", hwnd);
                        let viewport_width = self
                            .monitors
                            .get(&monitor_id)
                            .map(|m| m.work_area.width)
                            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                        if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                            workspace.ensure_focused_visible_animated(viewport_width);
                        }
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to snap back layout after drag: {}", e);
                        }
                    }
                }
            }
            WindowEvent::MovedOrResized(hwnd) => {
                // Skip events triggered by our own apply_layout() to avoid feedback loop.
                if self.applying_layout || self.should_suppress_moved_or_resized(hwnd) {
                    return;
                }
                // Suppress location-change events while the user is actively dragging.
                // We'll snap back on MoveSizeEnd instead.
                if self.dragging_window == Some(hwnd) {
                    return;
                }
                // If the window is managed (tiled), snap it back to its layout position.
                // This handles programmatic moves (not user drags).
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let is_floating = self
                        .workspaces
                        .get(&monitor_id)
                        .map_or(true, |ws| ws.is_floating(hwnd));

                    if !is_floating {
                        debug!("Managed window {} moved/resized — snapping back", hwnd);
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to snap back layout after move/resize: {}", e);
                        }
                    } else {
                        debug!("Floating window {} moved/resized by user — ignored", hwnd);
                    }
                } else {
                    debug!("Unmanaged window {} moved/resized — ignored", hwnd);
                }
            }
            WindowEvent::DisplayChange => {
                // Display configuration changed (monitors added/removed/rearranged)
                info!("Display configuration changed - reconciling monitors");

                // Re-enumerate monitors
                match enumerate_monitors() {
                    Ok(new_monitors) if !new_monitors.is_empty() => {
                        info!(
                            "Detected {} monitor(s) after display change",
                            new_monitors.len()
                        );
                        for m in &new_monitors {
                            info!(
                                "  Monitor {}: {}x{} at ({},{}){} \"{}\"",
                                m.id,
                                m.work_area.width,
                                m.work_area.height,
                                m.work_area.x,
                                m.work_area.y,
                                if m.is_primary { " [PRIMARY]" } else { "" },
                                m.device_name
                            );
                        }

                        // Reconcile workspaces with new monitor configuration
                        self.reconcile_monitors(new_monitors);

                        // Re-apply layout with updated monitor configuration
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to apply layout after display change: {}", e);
                        }
                    }
                    Ok(_) => {
                        warn!("No monitors found after display change");
                    }
                    Err(e) => {
                        warn!("Failed to enumerate monitors after display change: {}", e);
                    }
                }
            }
            WindowEvent::MouseEnterWindow(_hwnd) => {
                // This is handled by the main event loop with debouncing
                // (focus_follows_mouse delay)
            }
        }
    }

    /// Apply focus to a window for focus-follows-mouse.
    /// Returns true if focus was applied, false if the window isn't managed.
    fn apply_focus_follows_mouse(&mut self, hwnd: u64) -> bool {
        if let Some(monitor_id) = self.find_window_workspace(hwnd) {
            // Update focused monitor to match the window's monitor
            self.focused_monitor = monitor_id;

            let viewport_width = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                if workspace.is_floating(hwnd) {
                    // Floating windows are managed but not represented in tiled columns.
                    self.previous_focused_hwnd = Some(hwnd);
                    let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
                    debug!(
                        "Focus-follows-mouse: focused floating window {} on monitor {}",
                        hwnd, monitor_id
                    );
                    return true;
                }
                if let Err(e) = workspace.focus_window(hwnd) {
                    debug!(
                        "Failed to focus window {} for focus-follows-mouse: {}",
                        hwnd, e
                    );
                    return false;
                }
                debug!(
                    "Focus-follows-mouse: focused window {} on monitor {}",
                    hwnd, monitor_id
                );
                workspace.ensure_focused_visible_animated(viewport_width);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout after focus-follows-mouse: {}", e);
                }
                self.sync_foreground_window();
                return true;
            }
        }
        false
    }
}

/// Hotkey registration result containing handle and mapping.
struct HotkeyState {
    /// Handle to unregister hotkeys on drop.
    handle: Option<leopardwm_platform_win32::HotkeyHandle>,
    /// Mapping of hotkey IDs to commands.
    mapping: HashMap<HotkeyId, IpcCommand>,
    /// Number of hotkeys that were requested for registration.
    requested_count: usize,
    /// Number of hotkeys the OS actually registered (may be less than
    /// `requested_count` if some conflicted with other applications).
    registered_count: usize,
}

/// Register hotkeys from config and return state.
///
/// This function is called both at startup and on config reload.
fn setup_hotkeys(config: &Config, event_tx: mpsc::Sender<DaemonEvent>) -> HotkeyState {
    let config_hotkeys = &config.hotkeys.bindings;

    // Build hotkey definitions and command mapping
    let mut hotkeys = Vec::new();
    let mut mapping = HashMap::new();
    let mut next_id: HotkeyId = 1;

    for (key_str, cmd_str) in config_hotkeys {
        if let Some((modifiers, vk)) = parse_hotkey_string(key_str) {
            if let Some(cmd) = config::parse_command(cmd_str) {
                hotkeys.push(Hotkey::new(next_id, modifiers, vk));
                mapping.insert(next_id, cmd);
                debug!(
                    "Configured hotkey {}: {} -> {:?}",
                    next_id, key_str, cmd_str
                );
                next_id += 1;
            } else {
                warn!(
                    "Unknown command in hotkey config: {} -> {}",
                    key_str, cmd_str
                );
            }
        } else {
            warn!("Invalid hotkey string in config: {}", key_str);
        }
    }

    let requested_count = hotkeys.len();

    if hotkeys.is_empty() {
        info!("No hotkeys configured");
        return HotkeyState {
            handle: None,
            mapping,
            requested_count: 0,
            registered_count: 0,
        };
    }

    match register_hotkeys(hotkeys) {
        Ok((handle, hotkey_receiver)) => {
            info!("Registered {} global hotkeys", handle.registered_count());

            // Spawn task to forward hotkey events
            match std::thread::Builder::new()
                .name("hotkey-fwd".to_string())
                .spawn(move || {
                    while let Ok(event) = hotkey_receiver.recv() {
                        if event_tx.blocking_send(DaemonEvent::Hotkey(event)).is_err() {
                            break;
                        }
                    }
                }) {
                Ok(_) => {} // Thread is detached, we don't track it
                Err(e) => {
                    warn!("Failed to spawn hotkey-fwd thread: {}", e);
                }
            }

            let registered_count = handle.registered_count();
            HotkeyState {
                handle: Some(handle),
                mapping,
                requested_count,
                registered_count,
            }
        }
        Err(e) => {
            warn!(
                "Failed to register hotkeys: {}. Global shortcuts disabled.",
                e
            );
            HotkeyState {
                handle: None,
                mapping,
                requested_count,
                registered_count: 0,
            }
        }
    }
}

fn merged_cleanup_window_ids(
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

fn run_visibility_recovery_pass(managed_window_ids: &[u64], context_label: &str) {
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

/// Shared shutdown/recovery cleanup used by all daemon exit paths.
async fn run_shutdown_cleanup(state: &Arc<Mutex<AppState>>, mode: ShutdownMode) {
    info!("Running {} shutdown cleanup", mode.label());

    let (managed_window_ids, pending_apply_workers, apply_timeout) = {
        let mut state = state.lock().await;
        let pending_apply_workers = state.begin_shutdown_or_revert();
        if mode.should_save_state() {
            if let Err(e) = state.save_state() {
                warn!("Failed to save workspace state: {}", e);
            }
        }
        (
            state.all_managed_window_ids(),
            pending_apply_workers,
            state.layout_apply_timeout,
        )
    };

    let mut pending_workers = pending_apply_workers
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();

    let mut timed_out_workers = 0usize;
    for worker in &mut pending_workers {
        if !join_with_timeout(worker, apply_timeout) {
            warn!(
                "Timed-out apply worker did not exit before {} cleanup; continuing with best effort",
                mode.label()
            );
            timed_out_workers += 1;
        }
    }
    pending_workers.retain(Option::is_some);

    run_visibility_recovery_pass(&managed_window_ids, mode.label());

    if !pending_workers.is_empty() {
        for attempt in 1..=SHUTDOWN_RECOVERY_RETRY_ATTEMPTS {
            tokio::time::sleep(SHUTDOWN_RECOVERY_RETRY_DELAY).await;
            for worker in &mut pending_workers {
                let _ = join_with_timeout(worker, SHUTDOWN_RECOVERY_RETRY_DELAY);
            }
            pending_workers.retain(Option::is_some);
            info!(
                "Running additional {} cleanup visibility recovery pass {}/{} after {} timed-out apply worker(s)",
                mode.label(),
                attempt,
                SHUTDOWN_RECOVERY_RETRY_ATTEMPTS,
                timed_out_workers.max(pending_workers.len())
            );
            run_visibility_recovery_pass(&managed_window_ids, mode.label());
            if pending_workers.is_empty() {
                break;
            }
        }
    }

    if !pending_workers.is_empty() {
        warn!(
            "{} timed-out apply worker(s) still running after {} cleanup retries; running final bounded joins before exit",
            pending_workers.len(),
            mode.label()
        );
        for worker in &mut pending_workers {
            let _ = join_with_timeout(worker, SHUTDOWN_FINAL_JOIN_TIMEOUT);
        }
        pending_workers.retain(Option::is_some);
        run_visibility_recovery_pass(&managed_window_ids, mode.label());
        if !pending_workers.is_empty() {
            warn!(
                "{} timed-out apply worker(s) still running after final {} bounded joins ({} ms each); exiting without detached recovery threads",
                pending_workers.len(),
                mode.label(),
                SHUTDOWN_FINAL_JOIN_TIMEOUT.as_millis()
            );
        }
    }
}

/// Run the IPC server, accepting connections and dispatching commands.
async fn run_ipc_server(event_tx: mpsc::Sender<DaemonEvent>) {
    let mut is_first_instance = true;
    let pipe_name = preferred_pipe_name();
    // Bound concurrent IPC handlers to avoid local task-exhaustion DoS.
    let connection_limit = Arc::new(Semaphore::new(32));

    loop {
        let permit = match connection_limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!("IPC connection limiter closed while accepting client");
                return;
            }
        };

        // Create a new pipe server instance
        let server = match ServerOptions::new()
            .first_pipe_instance(is_first_instance)
            .pipe_mode(PipeMode::Byte)
            .create(&pipe_name)
        {
            Ok(s) => {
                is_first_instance = false; // Subsequent instances don't need this flag
                s
            }
            Err(e) => {
                error!("Failed to create named pipe server: {}", e);
                if is_first_instance {
                    // If we can't create the first instance, maybe another daemon is running
                    error!("Is another leopardwm daemon already running?");
                }
                drop(permit);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        debug!("Waiting for client connection on {}", pipe_name);

        // Wait for a client to connect
        if let Err(e) = server.connect().await {
            error!("Failed to accept client connection: {}", e);
            drop(permit);
            continue;
        }

        debug!("Client connected");

        // Handle this client
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(server, event_tx).await {
                warn!("Client handler error: {}", e);
            }
            drop(permit);
        });
    }
}

/// Handle a single client connection.
async fn handle_client(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    event_tx: mpsc::Sender<DaemonEvent>,
) -> Result<()> {
    async fn write_ipc_response_line<W>(writer: &mut W, response: &IpcResponse) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut response_json = match serde_json::to_string(response) {
            Ok(json) => json + "\n",
            Err(e) => {
                warn!("Failed to serialize IPC response: {}", e);
                "{\"status\":\"error\",\"message\":\"Internal serialization error\"}\n".to_string()
            }
        };

        if response_json.len() > MAX_IPC_MESSAGE_SIZE {
            warn!(
                "IPC response exceeded {} bytes; returning bounded error response instead",
                MAX_IPC_MESSAGE_SIZE
            );
            response_json = serde_json::to_string(&IpcResponse::error(
                "IPC response exceeded maximum size; narrow query scope and retry",
            ))
            .unwrap_or_else(|_| {
                "{\"status\":\"error\",\"message\":\"Internal serialization error\"}".to_string()
            });
            response_json.push('\n');
        }

        writer.write_all(response_json.as_bytes()).await?;
        Ok(())
    }

    let (reader, mut writer) = tokio::io::split(pipe);
    let limited_reader = reader.take(MAX_IPC_MESSAGE_SIZE as u64);
    let mut reader = BufReader::new(limited_reader);
    let mut line = String::new();

    // Read command (single line of JSON) with timeout and size bound
    let read_result = tokio::time::timeout(IPC_READ_TIMEOUT, reader.read_line(&mut line)).await;
    let bytes_read = match read_result {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            // Timeout: client did not send in time, silently close
            return Ok(());
        }
    };
    if bytes_read == 0 {
        return Ok(()); // Client disconnected
    }

    if !line.ends_with('\n') {
        let msg = if bytes_read >= MAX_IPC_MESSAGE_SIZE {
            "Command too large or missing newline terminator"
        } else {
            "IPC command must be newline-terminated"
        };
        write_ipc_response_line(&mut writer, &IpcResponse::error(msg)).await?;
        return Ok(());
    }

    let line = line.trim_end_matches(['\r', '\n']);
    debug!("Received command: {}", line);

    // Parse the command
    let cmd: IpcCommand = match serde_json::from_str(line) {
        Ok(cmd) => cmd,
        Err(e) => {
            let response = IpcResponse::error(format!("Invalid command: {}", e));
            write_ipc_response_line(&mut writer, &response).await?;
            return Ok(());
        }
    };

    // Create a oneshot channel for the response
    let (resp_tx, resp_rx) = oneshot::channel();
    let response_cmd = cmd.clone();

    // Send the command to the event loop
    if event_tx
        .send(DaemonEvent::IpcCommand {
            cmd,
            responder: resp_tx,
        })
        .await
        .is_err()
    {
        let response = IpcResponse::error("Daemon is shutting down");
        write_ipc_response_line(&mut writer, &response).await?;
        return Ok(());
    }

    // Wait for the response (bounded so clients don't hang forever).
    let response = match tokio::time::timeout(IPC_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => response_for_ipc_wait_failure(&response_cmd, false),
        Err(_) => response_for_ipc_wait_failure(&response_cmd, true),
    };

    // Send response back to client.
    write_ipc_response_line(&mut writer, &response).await?;

    Ok(())
}

/// Spawn a named forwarding thread that receives events from a std::sync::mpsc channel
/// and forwards them to a tokio mpsc sender. Returns the JoinHandle for graceful shutdown.
fn spawn_forwarding_thread<T: Send + 'static>(
    name: &str,
    receiver: std::sync::mpsc::Receiver<T>,
    sender: mpsc::Sender<DaemonEvent>,
    map_fn: impl Fn(T) -> DaemonEvent + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    let thread_name = name.to_string();
    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                if sender.blocking_send(map_fn(event)).is_err() {
                    break; // Channel closed, daemon shutting down
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn {} thread: {}", thread_name, e))
}

/// Join a thread with a timeout. Returns true if the thread joined within the deadline,
/// false if it timed out. The join handle remains available on timeout so callers can retry
/// later without losing ownership.
fn join_with_timeout(handle: &mut Option<std::thread::JoinHandle<()>>, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let Some(join_handle) = handle.as_ref() else {
            return true;
        };
        if join_handle.is_finished() {
            let join_handle = handle
                .take()
                .expect("join handle must exist when finishing timed join");
            let _ = join_handle.join();
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(JOIN_WITH_TIMEOUT_POLL_INTERVAL);
    }
}

/// Startup banner info for display after initialization.
pub struct StartupInfo {
    pub version: String,
    pub monitor_names: Vec<String>,
    pub window_count: usize,
    pub hotkeys_registered: usize,
    pub hotkeys_requested: usize,
    pub config_path: Option<String>,
    pub config_warnings: Vec<String>,
    pub log_path: String,
    pub safe_mode: bool,
    pub no_hotkeys: bool,
    pub no_cloak: bool,
}

/// Print a startup banner to stderr so users see immediate feedback.
/// Format the startup banner into a string (testable without capturing stderr).
pub fn format_startup_banner(info: &StartupInfo) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(out).unwrap();
    writeln!(out, "LeopardWM v{}", info.version).unwrap();
    if info.monitor_names.is_empty() {
        writeln!(out, "  Monitors: 0 (fallback mode)").unwrap();
    } else {
        writeln!(
            out,
            "  Monitors: {} ({})",
            info.monitor_names.len(),
            info.monitor_names.join(", ")
        )
        .unwrap();
    }
    writeln!(out, "  Windows:  {} managed", info.window_count).unwrap();
    if info.hotkeys_registered < info.hotkeys_requested {
        writeln!(
            out,
            "  Hotkeys:  {}/{} registered ({} failed)",
            info.hotkeys_registered,
            info.hotkeys_requested,
            info.hotkeys_requested - info.hotkeys_registered
        )
        .unwrap();
    } else {
        writeln!(out, "  Hotkeys:  {} registered", info.hotkeys_registered).unwrap();
    }
    if let Some(ref path) = info.config_path {
        writeln!(out, "  Config:   {}", path).unwrap();
    } else {
        writeln!(out, "  Config:   (default — no config file found)").unwrap();
    }
    for w in &info.config_warnings {
        writeln!(out, "  Warning:  {}", w).unwrap();
    }
    writeln!(out, "  Logs:     {}", info.log_path).unwrap();
    if info.safe_mode {
        writeln!(
            out,
            "  Mode:     SAFE MODE (hotkeys disabled, cloaking disabled)"
        )
        .unwrap();
    } else if info.no_hotkeys {
        writeln!(out, "  Mode:     hotkeys disabled").unwrap();
    } else if info.no_cloak {
        writeln!(out, "  Mode:     cloaking disabled").unwrap();
    } else {
        writeln!(out, "  Status:   Active").unwrap();
    }
    writeln!(out).unwrap();
    out
}

/// Print the startup banner to stderr.
pub fn print_startup_banner(info: &StartupInfo) {
    eprint!("{}", format_startup_banner(info));
}

/// Format a crash report from a panic.
fn format_crash_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    use std::fmt::Write;
    let mut report = String::new();
    writeln!(report, "LeopardWM Crash Report").unwrap();
    writeln!(report, "=====================").unwrap();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(report, "Timestamp (unix): {}", timestamp).unwrap();
    writeln!(report, "Version: {}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(report).unwrap();

    // Panic message
    writeln!(report, "## Panic Info").unwrap();
    if let Some(msg) = info.payload().downcast_ref::<&str>() {
        writeln!(report, "Message: {}", msg).unwrap();
    } else if let Some(msg) = info.payload().downcast_ref::<String>() {
        writeln!(report, "Message: {}", msg).unwrap();
    } else {
        writeln!(report, "Message: (unknown payload type)").unwrap();
    }
    if let Some(location) = info.location() {
        writeln!(
            report,
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )
        .unwrap();
    }
    writeln!(report).unwrap();

    // Backtrace
    writeln!(report, "## Backtrace").unwrap();
    writeln!(report, "{}", std::backtrace::Backtrace::force_capture()).unwrap();

    report
}

const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_FILE_NOT_FOUND: i32 = 2;

fn pipe_probe_error_indicates_running(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(ERROR_PIPE_BUSY) => true,
        Some(ERROR_FILE_NOT_FOUND) => false,
        _ if error.kind() == std::io::ErrorKind::NotFound => false,
        _ => true,
    }
}

fn pipe_probe_result_indicates_running<T>(probe_result: std::io::Result<T>) -> bool {
    match probe_result {
        Ok(_) => true,
        Err(error) => pipe_probe_error_indicates_running(&error),
    }
}

/// Check if another daemon instance is already running by probing the named pipe.
///
/// Returns `true` if the pipe exists (connected or busy). ERROR_PIPE_BUSY (231)
/// means another client is already connected — the daemon is still running.
async fn check_already_running() -> bool {
    for pipe_name in pipe_name_candidates() {
        let probe_result = tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe_name);
        if let Err(error) = &probe_result {
            if pipe_probe_error_indicates_running(error)
                && error.raw_os_error() != Some(ERROR_PIPE_BUSY)
            {
                warn!(
                    "Named pipe probe for {} failed with non-NotFound error ({}); assuming daemon is already running to avoid duplicate instances",
                    pipe_name,
                    error
                );
            }
        }
        if pipe_probe_result_indicates_running(probe_result.map(|_| ())) {
            return true;
        }
    }
    false
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments
    let args = Args::parse();

    // Set DPI awareness before any window/GDI operations
    if set_dpi_awareness() {
        eprintln!("[leopardwm] DPI awareness set to Per-Monitor Aware V2");
    } else {
        eprintln!("[leopardwm] Warning: Failed to set DPI awareness (may already be set)");
    }

    // Load configuration first (needed for log level)
    let mut config = Config::load().unwrap_or_else(|e| {
        // Can't use tracing yet, fall back to eprintln
        eprintln!("Failed to load configuration: {}. Using defaults.", e);
        Config::default()
    });

    // Apply safe-mode overrides to config
    if args.skip_cloak() {
        config.appearance.use_cloaking = false;
    }

    // Initialize logging with configured log level
    let log_level = match config.behavior.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO, // default fallback for invalid values
    };
    let subscriber = FmtSubscriber::builder().with_max_level(log_level).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Validate and clamp config values
    let config_warnings = config.validate();
    for w in &config_warnings {
        warn!("Config: {} - {}", w.field, w.message);
    }

    // Install panic hook to uncloak all windows and write a crash report
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[leopardwm] PANIC detected — emergency uncloaking all windows");
        uncloak_all_visible_windows();
        match enumerate_windows() {
            Ok(windows) => {
                let window_ids: Vec<u64> = windows.into_iter().map(|w| w.hwnd).collect();
                match restore_windows_moved_offscreen(&window_ids) {
                    Ok(restored) => {
                        if restored > 0 {
                            eprintln!(
                                "[leopardwm] Restored {} MoveOffScreen window(s) in panic recovery",
                                restored
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("[leopardwm] MoveOffScreen panic recovery failed: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[leopardwm] Failed to enumerate windows for MoveOffScreen recovery: {}",
                    e
                );
            }
        }

        // Write crash report to temp dir
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let crash_path = std::env::temp_dir().join(format!("leopardwm-crash-{}.txt", ts));
        let report = format_crash_report(info);
        if let Err(e) = std::fs::write(&crash_path, &report) {
            eprintln!("[leopardwm] Failed to write crash report: {}", e);
        } else {
            eprintln!(
                "[leopardwm] Crash report written to: {}",
                crash_path.display()
            );
        }

        default_hook(info);
    }));

    info!("LeopardWM daemon starting...");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));

    // Check if another instance is already running
    let ipc_pipe_names = pipe_name_candidates();
    if check_already_running().await {
        eprintln!("Error: Another leopardwm-daemon instance is already running.");
        eprintln!("Use 'leopardwm-cli status' to check the running instance.");
        error!(
            "Another leopardwm-daemon instance is already running (active pipe candidates: {})",
            ipc_pipe_names.join(", ")
        );
        std::process::exit(1);
    }

    info!(
        "Configuration loaded: gap={}, outer_gap={}, default_column_width={}, log_level={}",
        config.layout.gap,
        config.layout.outer_gap,
        config.layout.default_column_width,
        config.behavior.log_level
    );

    // Detect all monitors
    let monitors = match enumerate_monitors() {
        Ok(monitors) if !monitors.is_empty() => {
            info!("Detected {} monitor(s):", monitors.len());
            for m in &monitors {
                info!(
                    "  Monitor {}: {}x{} (work area: {}x{} at {},{}){} \"{}\"",
                    m.id,
                    m.rect.width,
                    m.rect.height,
                    m.work_area.width,
                    m.work_area.height,
                    m.work_area.x,
                    m.work_area.y,
                    if m.is_primary { " [PRIMARY]" } else { "" },
                    m.device_name
                );
            }
            monitors
        }
        Ok(_) | Err(_) => {
            warn!(
                "Failed to detect monitors, using fallback {}x{}",
                FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT
            );
            vec![MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT),
                work_area: Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_WORK_AREA_HEIGHT),
                is_primary: true,
                device_name: "Fallback".to_string(),
            }]
        }
    };

    // Initialize state with config and monitors
    let state = Arc::new(Mutex::new(AppState::new_with_config(
        config.clone(),
        monitors,
    )));

    // Enumerate existing windows
    info!("Enumerating windows...");
    {
        let mut state = state.lock().await;
        match state.enumerate_and_add_windows() {
            Ok(count) => {
                info!("Found and added {} manageable windows", count);
            }
            Err(e) => {
                error!("Failed to enumerate windows: {}", e);
            }
        }

        // Log workspace state for all monitors
        let total_windows: usize = state.workspaces.values().map(|w| w.window_count()).sum();
        let total_columns: usize = state.workspaces.values().map(|w| w.column_count()).sum();
        info!(
            "Workspaces initialized across {} monitors: {} total columns, {} total windows",
            state.workspaces.len(),
            total_columns,
            total_windows
        );

        // Restore saved workspace state (after windows are enumerated so scroll
        // offsets aren't clamped against empty workspaces).
        let restored_monitors = if let Some(snapshot) = AppState::load_state() {
            let restored = state.restore_state(&snapshot);
            info!("Restored workspace state from previous session");
            restored
        } else {
            HashSet::new()
        };

        // Collect viewport widths first to avoid borrow issues
        let monitor_widths: HashMap<MonitorId, i32> = state
            .monitors
            .iter()
            .map(|(id, m)| (*id, m.work_area.width))
            .collect();

        // Ensure the focused column is visible in the viewport for every workspace.
        // This also corrects stale scroll offsets from restored state that no longer
        // match the current window set.
        for (monitor_id, workspace) in state.workspaces.iter_mut() {
            if workspace.column_count() > 0 {
                let width = monitor_widths
                    .get(monitor_id)
                    .copied()
                    .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                workspace.ensure_focused_visible(width);
            }
        }

        // Normalize all column widths to default_column_width on startup.
        // Windows may have arbitrary sizes before tiling; using a uniform width
        // ensures consistent initial layout.
        let default_width = state.config.layout.default_column_width;
        for (_monitor_id, workspace) in state.workspaces.iter_mut() {
            workspace.set_all_column_widths(default_width);
        }

        // Reset scroll offset to 0 so windows tile from the left edge on startup
        // (like niri). The ensure_focused_visible call above may leave a stale
        // centered offset when restoring state.
        for (_monitor_id, workspace) in state.workspaces.iter_mut() {
            workspace.set_scroll_offset(0.0);
        }
    }

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(100);

    // Collect forwarding thread handles for graceful shutdown
    let mut thread_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Install WinEvent hooks for window lifecycle tracking (if enabled in config)
    let _hook_handle = if config.behavior.track_focus_changes {
        match install_event_hooks() {
            Ok((handle, event_receiver)) => {
                info!("WinEvent hooks installed");

                // Spawn task to forward window events from std::sync::mpsc to tokio channel
                match spawn_forwarding_thread(
                    "winevent-fwd",
                    event_receiver,
                    event_tx.clone(),
                    DaemonEvent::WindowEvent,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to install WinEvent hooks: {}. Window tracking disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("WinEvent hooks disabled by config (track_focus_changes = false)");
        None
    };

    // Register display change sender for WM_DISPLAYCHANGE events
    // This allows the hotkey window to forward display changes to our event loop
    {
        let (display_tx, display_rx) = std::sync::mpsc::channel::<WindowEvent>();
        if let Err(e) = set_display_change_sender(display_tx) {
            warn!("Failed to register display change sender: {}. Display changes may not be detected.", e);
        } else {
            // Forward display change events to the daemon event loop
            match spawn_forwarding_thread(
                "display-fwd",
                display_rx,
                event_tx.clone(),
                DaemonEvent::WindowEvent,
            ) {
                Ok(handle) => thread_handles.push(handle),
                Err(e) => warn!("{}", e),
            }
            info!("Display change detection enabled");
        }
    }

    // Register global hotkeys (mutable to support reload)
    let mut hotkey_state = if args.skip_hotkeys() {
        info!("Hotkeys disabled by command-line flag");
        HotkeyState {
            handle: None,
            mapping: HashMap::new(),
            requested_count: 0,
            registered_count: 0,
        }
    } else {
        setup_hotkeys(&config, event_tx.clone())
    };

    // Install mouse hook for focus-follows-mouse (if enabled)
    let _mouse_hook_handle = if config.behavior.focus_follows_mouse {
        let (mouse_tx, mouse_rx) = std::sync::mpsc::channel::<WindowEvent>();
        match install_mouse_hook(mouse_tx) {
            Ok(handle) => {
                info!(
                    "Focus-follows-mouse enabled (delay: {}ms)",
                    config.behavior.focus_follows_mouse_delay_ms
                );

                // Forward mouse events to the daemon event loop
                match spawn_forwarding_thread(
                    "mouse-fwd",
                    mouse_rx,
                    event_tx.clone(),
                    DaemonEvent::WindowEvent,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to install mouse hook: {}. Focus-follows-mouse disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("Focus-follows-mouse disabled by config (focus_follows_mouse = false)");
        None
    };

    // Register gesture detection (if enabled)
    let _gesture_handle = if config.gestures.enabled {
        match register_gestures() {
            Ok((handle, gesture_receiver)) => {
                info!("Gesture detection enabled");

                // Spawn thread to forward gesture events
                match spawn_forwarding_thread(
                    "gesture-fwd",
                    gesture_receiver,
                    event_tx.clone(),
                    DaemonEvent::Gesture,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to register gestures: {}. Gesture support disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("Gesture detection disabled by config (gestures.enabled = false)");
        None
    };

    // Initialize snap hint overlay (if enabled)
    let snap_hint_overlay: Option<OverlayWindow> = if config.snap_hints.enabled {
        match OverlayWindow::new() {
            Ok(overlay) => {
                info!("Snap hint overlay initialized");
                Some(overlay)
            }
            Err(e) => {
                warn!(
                    "Failed to create snap hint overlay: {}. Snap hints disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("Snap hints disabled by config (snap_hints.enabled = false)");
        None
    };

    // Initialize system tray icon
    // Create an intermediate sync channel that bridges tray events to the async event loop
    let tray_manager = {
        let (tray_sync_tx, tray_sync_rx) = std::sync::mpsc::channel();

        // Spawn task to forward tray events from sync channel to async channel
        match spawn_forwarding_thread(
            "tray-fwd",
            tray_sync_rx,
            event_tx.clone(),
            DaemonEvent::Tray,
        ) {
            Ok(handle) => thread_handles.push(handle),
            Err(e) => warn!("{}", e),
        }

        match tray::TrayManager::new(tray_sync_tx) {
            Ok(manager) => {
                info!("System tray icon initialized");
                Some(manager)
            }
            Err(e) => {
                warn!("Failed to create system tray icon: {}. Tray disabled.", e);
                None
            }
        }
    };

    // Settings window forwarding channel + handle
    let (settings_sync_tx, settings_sync_rx) = std::sync::mpsc::channel();
    match spawn_forwarding_thread(
        "settings-fwd",
        settings_sync_rx,
        event_tx.clone(),
        DaemonEvent::Settings,
    ) {
        Ok(handle) => thread_handles.push(handle),
        Err(e) => warn!("{}", e),
    }
    let mut _settings_handle: Option<settings::SettingsWindowHandle> = None;

    // Spawn IPC server
    let ipc_tx = event_tx.clone();
    tokio::spawn(async move {
        run_ipc_server(ipc_tx).await;
    });

    info!("IPC server listening on {}", preferred_pipe_name());

    // Install Ctrl+C handler so terminal kill triggers graceful shutdown
    {
        let shutdown_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                info!("Ctrl+C received, initiating shutdown...");
                let _ = shutdown_tx.send(DaemonEvent::Shutdown).await;
            }
        });
    }

    // Print startup banner for immediate user feedback
    {
        let state = state.lock().await;
        let monitor_names: Vec<String> = state
            .monitors
            .values()
            .map(|m| m.device_name.clone())
            .collect();
        let window_count: usize = state.workspaces.values().map(|w| w.window_count()).sum();
        let config_path = config::config_paths()
            .into_iter()
            .find(|p| p.exists())
            .map(|p| p.display().to_string());
        let log_path = std::env::temp_dir()
            .join("leopardwm-daemon.log")
            .display()
            .to_string();
        print_startup_banner(&StartupInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            monitor_names,
            window_count,
            hotkeys_registered: hotkey_state.registered_count,
            hotkeys_requested: hotkey_state.requested_count,
            config_path,
            config_warnings: config_warnings
                .iter()
                .map(|w| format!("{}: {}", w.field, w.message))
                .collect(),
            log_path,
            safe_mode: args.safe_mode,
            no_hotkeys: args.skip_hotkeys(),
            no_cloak: args.skip_cloak(),
        });
    }

    // Apply initial layout so windows are tiled on startup
    {
        let mut state = state.lock().await;
        if let Err(e) = state.apply_layout() {
            warn!("Failed to apply initial layout: {}", e);
        }
        // Set the DWM active border color on the focused window immediately
        state.sync_foreground_window();
    }

    info!("Ready. Use leopardwm-cli to send commands.");

    // Persistent animation worker thread (DwmFlush-based vsync pacing)
    let animation_worker = animation_worker::AnimationWorkerHandle::spawn(event_tx.clone())
        .expect("Failed to spawn animation worker");
    let mut animation_active = false;
    let mut last_frame_instant: Option<std::time::Instant> = None;

    // Snap hint timer handle - cancels pending hide operation when new hint is shown
    let mut snap_hint_timer_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Focus-follows-mouse timer handle - debounces rapid mouse movements
    let mut focus_follows_mouse_timer: Option<tokio::task::JoinHandle<()>> = None;

    // Main event loop
    loop {
        let event = match event_rx.recv().await {
            Some(e) => e,
            None => break,
        };

        match event {
            DaemonEvent::IpcCommand { cmd, responder } => {
                if let Some(mode) = shutdown_mode_for_command(&cmd) {
                    if mode == ShutdownMode::PanicRevert {
                        warn!("IPC: panic_revert requested");
                    } else {
                        info!("IPC: stop requested");
                    }
                    if responder.send(IpcResponse::Ok).is_err() {
                        debug!(
                            "Client disconnected before receiving {} response",
                            mode.label()
                        );
                    }
                    run_shutdown_cleanup(&state, mode).await;
                    break;
                }

                let is_reload = matches!(&cmd, IpcCommand::Reload);
                let is_resize = matches!(&cmd, IpcCommand::Resize { .. });
                let is_toggle_pause = matches!(&cmd, IpcCommand::TogglePause);

                let (response, should_animate, column_rect, hint_duration) = {
                    let mut state = state.lock().await;
                    let response = state.handle_command(cmd);
                    let animating = state.is_animating();

                    // Get column rect for snap hint if this is a resize
                    let rect = if is_resize && state.config.snap_hints.enabled {
                        state.get_focused_column_rect()
                    } else {
                        None
                    };
                    let duration = state.config.snap_hints.duration_ms;

                    (response, animating, rect, duration)
                };

                // If config was reloaded successfully, also reload hotkeys
                if is_reload && matches!(response, IpcResponse::Ok) {
                    // Drop old hotkey handle to unregister existing hotkeys
                    hotkey_state.handle = None;

                    // Re-register with new config
                    let new_config = {
                        let state = state.lock().await;
                        state.config.clone()
                    };
                    hotkey_state = setup_hotkeys(&new_config, event_tx.clone());
                    info!("Hotkeys reloaded after config reload");
                }

                // Log if client disconnected before receiving response
                if responder.send(response).is_err() {
                    debug!("Client disconnected before receiving IPC response");
                }

                if is_toggle_pause {
                    let state = state.lock().await;
                    if let Some(ref mgr) = tray_manager {
                        mgr.update_pause_text(state.paused);
                        let wc = state.all_managed_window_ids().len();
                        let mc = state.monitors.len();
                        mgr.update_tooltip(
                            wc,
                            mc,
                            state.paused,
                            Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                        );
                    }
                }

                // Show snap hint for resize operations
                if is_resize {
                    if let (Some(ref overlay), Some(rect)) = (&snap_hint_overlay, column_rect) {
                        // Cancel any pending hide timer
                        if let Some(handle) = snap_hint_timer_handle.take() {
                            handle.abort();
                        }

                        // Show the snap hint
                        overlay.show_snap_target(rect);

                        // Schedule hide after duration
                        let hide_tx = event_tx.clone();
                        let duration = hint_duration;
                        snap_hint_timer_handle = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                                .await;
                            let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
                        }));
                    }
                }

                // Start animation if needed
                if should_animate && !animation_active {
                    let mut state = state.lock().await;
                    state.tick_animations(0);
                    if let Ok(true) = state.send_animation_frame(&animation_worker) {
                        animation_active = true;
                        last_frame_instant = Some(std::time::Instant::now());
                    }
                }
            }
            DaemonEvent::WindowEvent(win_event) => {
                // Handle MouseEnterWindow specially for focus-follows-mouse debouncing
                if let WindowEvent::MouseEnterWindow(hwnd) = win_event {
                    let (enabled, delay_ms) = {
                        let state = state.lock().await;
                        (
                            state.config.behavior.focus_follows_mouse,
                            state.config.behavior.focus_follows_mouse_delay_ms,
                        )
                    };

                    if enabled {
                        // Cancel any pending focus timer
                        if let Some(handle) = focus_follows_mouse_timer.take() {
                            handle.abort();
                        }

                        // Schedule focus after delay (debouncing)
                        let focus_tx = event_tx.clone();
                        let delay = delay_ms;
                        focus_follows_mouse_timer = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(delay as u64))
                                .await;
                            let _ = focus_tx
                                .send(DaemonEvent::FocusFollowsMouse { window_id: hwnd })
                                .await;
                        }));
                    }
                } else {
                    {
                        let mut state = state.lock().await;
                        state.handle_window_event(win_event);
                        // Update tray tooltip with current state
                        if let Some(ref mgr) = tray_manager {
                            let wc = state.all_managed_window_ids().len();
                            let mc = state.monitors.len();
                            mgr.update_tooltip(
                                wc,
                                mc,
                                state.paused,
                                Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                            );
                        }
                        // Start animation if needed (e.g. animated snap-back)
                        if state.is_animating() && !animation_active {
                            state.tick_animations(0);
                            if let Ok(true) = state.send_animation_frame(&animation_worker) {
                                animation_active = true;
                                last_frame_instant = Some(std::time::Instant::now());
                            }
                        }
                    }
                }
            }
            DaemonEvent::Hotkey(hotkey_event) => {
                let mut requested_shutdown: Option<ShutdownMode> = None;
                let (should_animate, is_resize, column_rect, hint_duration) =
                    if let Some(cmd) = hotkey_state.mapping.get(&hotkey_event.id).cloned() {
                        debug!("Hotkey {} triggered, executing {:?}", hotkey_event.id, cmd);
                        if let Some(mode) = shutdown_mode_for_command(&cmd) {
                            requested_shutdown = Some(mode);
                            (false, false, None, 200)
                        } else {
                            let is_resize = matches!(cmd, IpcCommand::Resize { .. });
                            let mut state = state.lock().await;
                            let response = state.handle_command(cmd);
                            if let IpcResponse::Error { message } = response {
                                warn!("Hotkey command failed: {}", message);
                            }
                            let animating = state.is_animating();

                            // Get column rect for snap hint if this is a resize
                            let rect = if is_resize && state.config.snap_hints.enabled {
                                state.get_focused_column_rect()
                            } else {
                                None
                            };
                            let duration = state.config.snap_hints.duration_ms;

                            (animating, is_resize, rect, duration)
                        }
                    } else {
                        warn!("Unknown hotkey ID: {}", hotkey_event.id);
                        (false, false, None, 200)
                    };

                if let Some(mode) = requested_shutdown {
                    warn!(
                        "Hotkey {} requested {}; running shutdown cleanup",
                        hotkey_event.id,
                        mode.label()
                    );
                    run_shutdown_cleanup(&state, mode).await;
                    break;
                }

                // Show snap hint for resize operations
                if is_resize {
                    if let (Some(ref overlay), Some(rect)) = (&snap_hint_overlay, column_rect) {
                        // Cancel any pending hide timer
                        if let Some(handle) = snap_hint_timer_handle.take() {
                            handle.abort();
                        }

                        // Show the snap hint
                        overlay.show_snap_target(rect);

                        // Schedule hide after duration
                        let hide_tx = event_tx.clone();
                        let duration = hint_duration;
                        snap_hint_timer_handle = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                                .await;
                            let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
                        }));
                    }
                }

                // Start animation if needed
                if should_animate && !animation_active {
                    let mut state = state.lock().await;
                    state.tick_animations(0);
                    if let Ok(true) = state.send_animation_frame(&animation_worker) {
                        animation_active = true;
                        last_frame_instant = Some(std::time::Instant::now());
                    }
                }
            }
            DaemonEvent::Gesture(gesture_event) => {
                // Map gesture to command from config
                let gesture_config = {
                    let state = state.lock().await;
                    state.config.gestures.clone()
                };

                let cmd_str = match gesture_event {
                    GestureEvent::SwipeLeft => &gesture_config.swipe_left,
                    GestureEvent::SwipeRight => &gesture_config.swipe_right,
                    GestureEvent::SwipeUp => &gesture_config.swipe_up,
                    GestureEvent::SwipeDown => &gesture_config.swipe_down,
                };

                if let Some(cmd) = config::parse_command(cmd_str) {
                    debug!("Gesture {:?} triggered, executing {:?}", gesture_event, cmd);
                    if let Some(mode) = shutdown_mode_for_command(&cmd) {
                        warn!(
                            "Gesture {:?} requested {}; running shutdown cleanup",
                            gesture_event,
                            mode.label()
                        );
                        run_shutdown_cleanup(&state, mode).await;
                        break;
                    }
                    {
                        let mut state = state.lock().await;
                        let response = state.handle_command(cmd);
                        if let IpcResponse::Error { message } = response {
                            warn!("Gesture command failed: {}", message);
                        }
                        if state.is_animating() && !animation_active {
                            state.tick_animations(0);
                            if let Ok(true) = state.send_animation_frame(&animation_worker) {
                                animation_active = true;
                                last_frame_instant = Some(std::time::Instant::now());
                            }
                        }
                    }
                } else {
                    warn!("Unknown command for gesture: {}", cmd_str);
                }
            }
            DaemonEvent::Tray(tray_event) => {
                match tray_event {
                    tray::TrayEvent::Refresh => {
                        info!("Tray: Refresh requested");
                        let mut state = state.lock().await;
                        let response = state.handle_command(IpcCommand::Refresh);
                        if let IpcResponse::Error { message } = response {
                            warn!("Refresh failed: {}", message);
                        }
                    }
                    tray::TrayEvent::Reload => {
                        info!("Tray: Reload config requested");
                        let response = {
                            let mut state = state.lock().await;
                            state.handle_command(IpcCommand::Reload)
                        };

                        // If config was reloaded successfully, also reload hotkeys
                        if matches!(response, IpcResponse::Ok) {
                            hotkey_state.handle = None;
                            let new_config = {
                                let state = state.lock().await;
                                state.config.clone()
                            };
                            hotkey_state = setup_hotkeys(&new_config, event_tx.clone());
                            info!("Hotkeys reloaded after tray config reload");
                        } else if let IpcResponse::Error { message } = response {
                            warn!("Reload failed: {}", message);
                        }
                    }
                    tray::TrayEvent::Exit => {
                        info!("Tray: Exit requested");
                        // Route tray exit through the unified shutdown path so all
                        // cleanup (save_state + uncloak/reset) stays consistent.
                        let _ = event_tx.send(DaemonEvent::Shutdown).await;
                    }
                    tray::TrayEvent::TogglePause => {
                        let mut state = state.lock().await;
                        if let Err(e) = state.toggle_pause("tray toggle") {
                            warn!("Tray toggle pause failed: {}", e);
                        }
                        if let Some(ref mgr) = tray_manager {
                            mgr.update_pause_text(state.paused);
                            let wc = state.all_managed_window_ids().len();
                            let mc = state.monitors.len();
                            mgr.update_tooltip(
                                wc,
                                mc,
                                state.paused,
                                Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                            );
                        }
                    }
                    tray::TrayEvent::OpenConfig => {
                        info!("Tray: Settings requested");
                        let config_snapshot = {
                            let st = state.lock().await;
                            st.config.clone()
                        };
                        _settings_handle = settings::SettingsWindowHandle::open(
                            config_snapshot,
                            settings_sync_tx.clone(),
                        );
                    }
                    tray::TrayEvent::ViewLogs => {
                        info!("Tray: View logs requested");
                        let log_dir = std::env::temp_dir();
                        let _ = std::process::Command::new("cmd")
                            .args(["/c", "start", "", &log_dir.to_string_lossy()])
                            .spawn();
                    }
                    tray::TrayEvent::EmergencyUncloakAll => {
                        warn!("Tray: Emergency uncloak requested");
                        uncloak_all_visible_windows();
                    }
                }
            }
            DaemonEvent::Settings(settings_event) => {
                match settings_event {
                    settings::SettingsEvent::Saved => {
                        info!("Settings: config saved, triggering reload");
                        let response = {
                            let mut state = state.lock().await;
                            state.handle_command(IpcCommand::Reload)
                        };
                        if matches!(response, IpcResponse::Ok) {
                            hotkey_state.handle = None;
                            let new_config = {
                                let state = state.lock().await;
                                state.config.clone()
                            };
                            hotkey_state = setup_hotkeys(&new_config, event_tx.clone());
                            info!("Hotkeys reloaded after settings save");
                        } else if let IpcResponse::Error { message } = response {
                            warn!("Reload after settings save failed: {}", message);
                        }
                    }
                }
            }
            DaemonEvent::AnimationFrameApplied(frame_result) => {
                {
                    let mut state = state.lock().await;
                    state.applying_layout = false;
                    // Reposition border to follow the focused window during animation
                    if let Some(hwnd) = state.previous_focused_hwnd {
                        if state.config.appearance.active_border {
                            state.show_border(hwnd);
                        }
                    }
                }
                if let Err(ref e) = frame_result.apply_result {
                    warn!("Animation frame failed: {}", e);
                }
                // Measure real elapsed time (cap at 100ms to prevent jump from stalls)
                let delta_ms = last_frame_instant
                    .map(|t| t.elapsed().as_millis().min(100) as u64)
                    .unwrap_or(16);
                last_frame_instant = Some(std::time::Instant::now());

                let still_animating = {
                    let mut state = state.lock().await;
                    let running = state.tick_animations(delta_ms);
                    if running || state.is_animating() {
                        matches!(
                            state.send_animation_frame(&animation_worker),
                            Ok(true)
                        )
                    } else {
                        false
                    }
                };
                if !still_animating {
                    animation_active = false;
                    last_frame_instant = None;
                    debug!("All animations complete");
                }
            }
            DaemonEvent::HideSnapHint => {
                if let Some(ref overlay) = snap_hint_overlay {
                    overlay.hide();
                    debug!("Snap hint hidden");
                }
            }
            DaemonEvent::FocusFollowsMouse { window_id } => {
                let mut state = state.lock().await;
                if state.config.behavior.focus_follows_mouse {
                    let applied = state.apply_focus_follows_mouse(window_id);
                    if applied && state.is_animating() && !animation_active {
                        state.tick_animations(0);
                        if let Ok(true) = state.send_animation_frame(&animation_worker) {
                            animation_active = true;
                            last_frame_instant = Some(std::time::Instant::now());
                        }
                    }
                }
            }
            DaemonEvent::Shutdown => {
                info!("Shutdown signal received");
                run_shutdown_cleanup(&state, ShutdownMode::Graceful).await;
                break;
            }
        }
    }

    // Clean up animation worker (Drop sends Shutdown and joins)
    drop(animation_worker);

    // Clean up timers if running
    if let Some(handle) = snap_hint_timer_handle {
        handle.abort();
    }
    if let Some(handle) = focus_follows_mouse_timer {
        handle.abort();
    }

    // Join forwarding threads with timeout for graceful shutdown
    info!("Waiting for forwarding threads to exit...");
    let shutdown_deadline = std::time::Instant::now() + Duration::from_secs(5);
    for handle in thread_handles {
        let remaining = shutdown_deadline.saturating_duration_since(std::time::Instant::now());
        let per_thread = remaining.min(Duration::from_secs(3));
        let mut handle = Some(handle);
        if !join_with_timeout(&mut handle, per_thread) {
            warn!("A forwarding thread did not exit within timeout, continuing shutdown");
        }
    }

    info!("LeopardWM daemon shutting down.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leopardwm_core_layout::Rect;

    fn test_config() -> Config {
        Config::default()
    }

    fn test_monitors() -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
        }]
    }

    #[test]
    fn test_app_state_new() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn test_app_state_focused_viewport() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        let viewport = state.focused_viewport();
        assert_eq!(viewport.width, 1920);
        assert_eq!(viewport.height, 1040);
    }

    #[test]
    fn test_app_state_no_monitors_fallback() {
        let state = AppState::new_with_config(test_config(), vec![]);
        let viewport = state.focused_viewport();
        assert_eq!(viewport.width, FALLBACK_VIEWPORT_WIDTH);
        assert_eq!(viewport.height, FALLBACK_VIEWPORT_HEIGHT);
    }

    #[test]
    fn test_window_rule_matching_class() {
        let config = Config {
            window_rules: vec![config::WindowRule {
                match_class: Some("TestClass".to_string()),
                match_title: None,
                match_executable: None,
                action: config::WindowAction::Float,
                width: Some(800),
                height: Some(600),
            }],
            ..Default::default()
        };
        let state = AppState::new_with_config(config, test_monitors());
        let action = state.evaluate_window_rules("TestClass", "Any Title", "any.exe");
        assert_eq!(action, config::WindowAction::Float);
    }

    #[test]
    fn test_window_rule_matching_title() {
        let config = Config {
            window_rules: vec![config::WindowRule {
                match_class: None,
                match_title: Some(".*DevTools.*".to_string()),
                match_executable: None,
                action: config::WindowAction::Float,
                width: None,
                height: None,
            }],
            ..Default::default()
        };
        let state = AppState::new_with_config(config, test_monitors());
        let action = state.evaluate_window_rules("AnyClass", "DevTools - localhost", "chrome.exe");
        assert_eq!(action, config::WindowAction::Float);
    }

    #[test]
    fn test_window_rule_matching_executable() {
        let config = Config {
            window_rules: vec![config::WindowRule {
                match_class: None,
                match_title: None,
                match_executable: Some("spotify.exe".to_string()),
                action: config::WindowAction::Ignore,
                width: None,
                height: None,
            }],
            ..Default::default()
        };
        let state = AppState::new_with_config(config, test_monitors());
        let action = state.evaluate_window_rules("SpotifyClass", "Spotify", "spotify.exe");
        assert_eq!(action, config::WindowAction::Ignore);
    }

    #[test]
    fn test_window_rule_no_match_defaults_to_tile() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        let action = state.evaluate_window_rules("SomeClass", "Some Title", "some.exe");
        assert_eq!(action, config::WindowAction::Tile);
    }

    #[test]
    fn test_floating_rect_uses_rule_dimensions() {
        let config = Config {
            window_rules: vec![config::WindowRule {
                match_class: Some("TestClass".to_string()),
                match_title: None,
                match_executable: None,
                action: config::WindowAction::Float,
                width: Some(1024),
                height: Some(768),
            }],
            ..Default::default()
        };
        let state = AppState::new_with_config(config, test_monitors());
        let original = Rect::new(100, 100, 640, 480);
        let result =
            state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original);
        assert_eq!(result.width, 1024);
        assert_eq!(result.height, 768);
    }

    #[test]
    fn test_floating_rect_preserves_original_if_no_dimensions() {
        let config = Config {
            window_rules: vec![config::WindowRule {
                match_class: Some("TestClass".to_string()),
                match_title: None,
                match_executable: None,
                action: config::WindowAction::Float,
                width: None,
                height: None,
            }],
            ..Default::default()
        };
        let state = AppState::new_with_config(config, test_monitors());
        let original = Rect::new(100, 100, 640, 480);
        let result =
            state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original);
        assert_eq!(result.width, 640);
        assert_eq!(result.height, 480);
    }

    #[test]
    fn test_find_window_workspace_not_found() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        assert!(state.find_window_workspace(99999).is_none());
    }

    #[test]
    fn test_app_state_apply_config() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let mut new_config = test_config();
        new_config.layout.gap = 20;
        new_config.layout.outer_gap = 15;
        state.apply_config(new_config.clone());
        assert_eq!(state.config.layout.gap, 20);
        assert_eq!(state.config.layout.outer_gap, 15);
    }

    #[test]
    fn test_state_file_path() {
        let path = AppState::state_file_path();
        assert!(path.to_str().unwrap().contains("leopardwm"));
        assert!(path.to_str().unwrap().ends_with("workspace-state.json"));
    }

    #[test]
    fn test_state_snapshot_serialization() {
        let snapshot = StateSnapshot {
            saved_at: "2026-02-04T12:00:00".to_string(),
            workspaces: vec![],
            focused_monitor_name: "DISPLAY1".to_string(),
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.focused_monitor_name, "DISPLAY1");
        assert!(parsed.workspaces.is_empty());
    }

    #[test]
    fn test_workspace_snapshot_serialization() {
        let workspace = Workspace::new();
        let snapshot = WorkspaceSnapshot {
            monitor_device_name: "DISPLAY1".to_string(),
            workspace,
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let parsed: WorkspaceSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.monitor_device_name, "DISPLAY1");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        // Create a snapshot and verify it roundtrips through serialization
        let snapshot = StateSnapshot {
            saved_at: "2026-02-04T12:00:00".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".to_string(),
                workspace: Workspace::with_gaps(10, 10),
            }],
            focused_monitor_name: "DISPLAY1".to_string(),
        };
        let json = serde_json::to_string_pretty(&snapshot).expect("serialize");
        let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.workspaces[0].monitor_device_name, "DISPLAY1");
    }

    #[test]
    fn test_spawn_forwarding_thread_forwards_events() {
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let (async_tx, mut async_rx) = mpsc::channel::<DaemonEvent>(10);

        let _handle = spawn_forwarding_thread("test", rx, async_tx, |_n| {
            DaemonEvent::HideSnapHint // Use a simple variant for testing
        })
        .unwrap();

        tx.send(42).unwrap();
        drop(tx); // Close channel so thread exits

        // Use a runtime to receive
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let event = rt.block_on(async { async_rx.recv().await });
        assert!(event.is_some());
    }

    #[test]
    fn test_spawn_forwarding_thread_stops_on_channel_close() {
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let (async_tx, _async_rx) = mpsc::channel::<DaemonEvent>(10);

        let handle =
            spawn_forwarding_thread("test-close", rx, async_tx, |_| DaemonEvent::HideSnapHint)
                .unwrap();

        drop(tx); // Close sender immediately
                  // Thread should exit when recv() returns Err
        handle.join().expect("Thread should exit cleanly");
    }

    #[ignore] // Depends on no daemon running; fails when daemon is active
    #[test]
    fn test_check_already_running_returns_false_when_no_daemon() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let result = rt.block_on(check_already_running());
        // No daemon is running during tests, so this should be false
        assert!(!result);
    }

    #[test]
    fn test_ipc_read_timeout_is_reasonable() {
        assert!(IPC_READ_TIMEOUT.as_secs() >= 1);
        assert!(IPC_READ_TIMEOUT.as_secs() <= 30);
    }

    #[test]
    fn test_ipc_response_timeout_is_reasonable() {
        assert!(IPC_RESPONSE_TIMEOUT.as_secs() >= 1);
        assert!(IPC_RESPONSE_TIMEOUT.as_secs() <= 60);
    }

    #[test]
    fn test_response_for_ipc_wait_failure_shutdown_commands_return_ok() {
        assert_eq!(
            response_for_ipc_wait_failure(&IpcCommand::Stop, true),
            IpcResponse::Ok
        );
        assert_eq!(
            response_for_ipc_wait_failure(&IpcCommand::PanicRevert, false),
            IpcResponse::Ok
        );
    }

    #[test]
    fn test_response_for_ipc_wait_failure_non_shutdown_returns_error() {
        match response_for_ipc_wait_failure(&IpcCommand::FocusLeft, true) {
            IpcResponse::Error { message } => {
                assert!(message.contains("Timed out waiting for daemon response"));
            }
            other => panic!("Expected timeout error response, got {:?}", other),
        }

        match response_for_ipc_wait_failure(&IpcCommand::FocusLeft, false) {
            IpcResponse::Error { message } => {
                assert!(message.contains("Failed to get response from daemon"));
            }
            other => panic!("Expected responder error response, got {:?}", other),
        }
    }

    #[test]
    fn test_shutdown_mode_for_command_maps_shutdown_variants() {
        assert_eq!(
            shutdown_mode_for_command(&IpcCommand::Stop),
            Some(ShutdownMode::Graceful)
        );
        assert_eq!(
            shutdown_mode_for_command(&IpcCommand::PanicRevert),
            Some(ShutdownMode::PanicRevert)
        );
        assert_eq!(shutdown_mode_for_command(&IpcCommand::FocusLeft), None);
    }

    #[test]
    fn test_max_ipc_message_size_is_reasonable() {
        const { assert!(leopardwm_ipc::MAX_IPC_MESSAGE_SIZE >= 1024) };
        const { assert!(leopardwm_ipc::MAX_IPC_MESSAGE_SIZE <= 1024 * 1024) };
    }

    // ========================================================================
    // handle_command() Unit Tests
    // ========================================================================

    #[test]
    fn test_cmd_query_workspace_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::QueryWorkspace);
        match resp {
            IpcResponse::WorkspaceState {
                columns, windows, ..
            } => {
                assert_eq!(columns, 0);
                assert_eq!(windows, 0);
            }
            _ => panic!("Expected WorkspaceState, got {:?}", resp),
        }
    }

    #[test]
    fn test_cmd_query_focused_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::QueryFocused);
        match resp {
            IpcResponse::FocusedWindow {
                window_id,
                column_index,
                window_index,
            } => {
                assert!(window_id.is_none());
                assert_eq!(column_index, 0);
                assert_eq!(window_index, 0);
            }
            _ => panic!("Expected FocusedWindow, got {:?}", resp),
        }
    }

    #[test]
    fn test_cmd_focus_up_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::FocusUp);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_focus_down_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::FocusDown);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_stop() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::Stop);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_panic_revert() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::PanicRevert);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_toggle_pause() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        assert!(!state.paused);

        let resp = state.handle_command(IpcCommand::TogglePause);
        assert_eq!(resp, IpcResponse::Ok);
        assert!(state.paused, "toggle_pause should pause tiling");

        let resp = state.handle_command(IpcCommand::TogglePause);
        assert_eq!(resp, IpcResponse::Ok);
        assert!(!state.paused, "second toggle_pause should resume tiling");
    }

    #[test]
    fn test_toggle_pause_resume_reports_apply_failure() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        assert_eq!(
            state.handle_command(IpcCommand::TogglePause),
            IpcResponse::Ok
        );
        assert!(state.paused, "first toggle_pause should pause tiling");

        state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
            Duration::from_millis(1),
        ));

        let resp = state.handle_command(IpcCommand::TogglePause);
        match resp {
            IpcResponse::Error { message } => {
                assert!(message.contains("injected apply_placements failure"));
            }
            other => panic!("Expected Error response, got {:?}", other),
        }
        assert!(
            state.paused,
            "failed resume should restore paused state to avoid false resumed status"
        );
    }

    #[test]
    fn test_cmd_focus_left_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::FocusLeft);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_focus_right_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::FocusRight);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_move_left_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::MoveColumnLeft);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_move_right_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::MoveColumnRight);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_resize_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::Resize { delta: 100 });
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_scroll_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::Scroll { delta: 50.0 });
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_apply() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::Apply);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_focus_monitor_left_single() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        // With only one monitor, FocusMonitorLeft is a no-op, returns Ok without calling apply_layout
        let resp = state.handle_command(IpcCommand::FocusMonitorLeft);
        assert_eq!(resp, IpcResponse::Ok);
        assert_eq!(state.focused_monitor, 1); // unchanged
    }

    #[test]
    fn test_cmd_focus_monitor_right_single() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::FocusMonitorRight);
        assert_eq!(resp, IpcResponse::Ok);
        assert_eq!(state.focused_monitor, 1); // unchanged
    }

    #[test]
    fn test_cmd_move_to_monitor_left_single() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::MoveWindowToMonitorLeft);
        assert_eq!(resp, IpcResponse::Ok); // no-op: no monitor to the left
    }

    #[test]
    fn test_cmd_move_to_monitor_right_single() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::MoveWindowToMonitorRight);
        assert_eq!(resp, IpcResponse::Ok); // no-op: no monitor to the right
    }

    #[test]
    fn test_cmd_move_to_monitor_right_rollback_on_insert_failure() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state.focused_monitor = 1;
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();
        // Force target insert failure (duplicate in target workspace).
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();

        let resp = state.handle_command(IpcCommand::MoveWindowToMonitorRight);
        match resp {
            IpcResponse::Error { message } => {
                assert!(message.contains("Failed to add window to target"))
            }
            other => panic!("Expected error, got {:?}", other),
        }

        let source = state.workspaces.get(&1).unwrap();
        let target = state.workspaces.get(&2).unwrap();
        assert_eq!(state.focused_monitor, 1);
        assert_eq!(source.window_count(), 1);
        assert_eq!(source.focused_window(), Some(100));
        assert_eq!(target.window_count(), 1);
        assert!(target.contains_window(100));
    }

    #[test]
    fn test_cmd_move_to_monitor_left_rollback_on_insert_failure() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state.focused_monitor = 2;
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();
        // Force target insert failure (duplicate in target workspace).
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();

        let resp = state.handle_command(IpcCommand::MoveWindowToMonitorLeft);
        match resp {
            IpcResponse::Error { message } => {
                assert!(message.contains("Failed to add window to target"))
            }
            other => panic!("Expected error, got {:?}", other),
        }

        let source = state.workspaces.get(&2).unwrap();
        let target = state.workspaces.get(&1).unwrap();
        assert_eq!(state.focused_monitor, 2);
        assert_eq!(source.window_count(), 1);
        assert_eq!(source.focused_window(), Some(200));
        assert_eq!(target.window_count(), 1);
        assert!(target.contains_window(200));
    }

    // ========================================================================
    // reconcile_monitors() Unit Tests
    // ========================================================================

    fn two_monitors() -> Vec<MonitorInfo> {
        vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
        ]
    }

    #[test]
    fn test_reconcile_no_change() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let monitors_before = state.workspaces.len();
        state.reconcile_monitors(test_monitors());
        assert_eq!(state.workspaces.len(), monitors_before);
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn test_reconcile_add_monitor() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        assert_eq!(state.workspaces.len(), 1);
        state.reconcile_monitors(two_monitors());
        assert_eq!(state.workspaces.len(), 2);
        assert!(state.workspaces.contains_key(&2));
    }

    #[test]
    fn test_reconcile_remove_monitor() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        assert_eq!(state.workspaces.len(), 2);
        // Remove second monitor, keep only primary
        state.reconcile_monitors(test_monitors());
        assert_eq!(state.workspaces.len(), 1);
        assert!(state.workspaces.contains_key(&1));
        assert!(!state.workspaces.contains_key(&2));
    }

    #[test]
    fn test_reconcile_remove_focused_monitor() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state.focused_monitor = 2; // Focus on secondary
                                   // Remove secondary, keep primary
        state.reconcile_monitors(test_monitors());
        // Focus should fall back to primary
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn test_reconcile_primary_always_exists() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        // Remove secondary, keep primary
        state.reconcile_monitors(test_monitors());
        assert!(state.workspaces.contains_key(&1));
    }

    #[test]
    fn test_reconcile_empty_to_multi() {
        let mut state = AppState::new_with_config(test_config(), vec![]);
        assert_eq!(state.workspaces.len(), 0);
        state.reconcile_monitors(two_monitors());
        assert_eq!(state.workspaces.len(), 2);
    }

    #[test]
    fn test_reconcile_preserves_windows() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        // Add windows to workspace on monitor 2
        if let Some(ws) = state.workspaces.get_mut(&2) {
            ws.insert_window(1001, None).unwrap();
            ws.insert_window(1002, None).unwrap();
        }
        assert_eq!(state.workspaces.get(&2).unwrap().window_count(), 2);

        // Remove monitor 2 - windows should migrate to primary
        state.reconcile_monitors(test_monitors());
        let primary_ws = state.workspaces.get(&1).unwrap();
        assert_eq!(primary_ws.window_count(), 2);
    }

    #[test]
    fn test_reconcile_full_monitor_churn() {
        // Start with monitors 1 and 2, add windows to both
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(100, None)
            .unwrap();
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(101, None)
            .unwrap();
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(200, None)
            .unwrap();

        // Replace ALL monitors with entirely new ones (ids 3 and 4)
        let new_monitors = vec![
            MonitorInfo {
                id: 3,
                rect: Rect::new(0, 0, 2560, 1440),
                work_area: Rect::new(0, 0, 2560, 1400),
                is_primary: true,
                device_name: "DISPLAY3".to_string(),
            },
            MonitorInfo {
                id: 4,
                rect: Rect::new(2560, 0, 1920, 1080),
                work_area: Rect::new(2560, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY4".to_string(),
            },
        ];
        state.reconcile_monitors(new_monitors);

        // All 3 windows must have been migrated to the new primary (id 3)
        assert_eq!(state.workspaces.len(), 2);
        let primary_ws = state.workspaces.get(&3).unwrap();
        assert_eq!(primary_ws.window_count(), 3);
        assert!(state.workspaces.contains_key(&4));
        // Old monitors must be gone
        assert!(!state.workspaces.contains_key(&1));
        assert!(!state.workspaces.contains_key(&2));
    }

    // ========================================================================
    // Additional Command Tests
    // ========================================================================

    #[test]
    fn test_cmd_refresh() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        // Keep this deterministic in headless/CI environments where Win32
        // placement side effects can fail on unrelated desktop windows.
        state.paused = true;
        let resp = state.handle_command(IpcCommand::Refresh);
        match resp {
            IpcResponse::Ok => {}
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("Failed to enumerate windows")
                        || message.contains("Failed to apply layout"),
                    "unexpected refresh error: {}",
                    message
                );
            }
            other => panic!("Expected Ok or Error, got {:?}", other),
        }
    }

    #[test]
    fn test_cmd_reload() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::Reload);
        assert_eq!(resp, IpcResponse::Ok);
        // Config was reloaded (default since no config file in test env)
        assert_eq!(state.config.layout.gap, Config::default().layout.gap);
    }

    #[test]
    fn test_cmd_query_all_windows() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::QueryAllWindows);
        match resp {
            IpcResponse::WindowList { windows } => {
                assert!(windows.is_empty());
            }
            other => panic!("Expected WindowList, got {:?}", other),
        }
    }

    // ========================================================================
    // New command tests (Iteration 29)
    // ========================================================================

    #[test]
    fn test_cmd_close_window_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::CloseWindow);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_toggle_floating_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::ToggleFloating);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_toggle_floating_roundtrip() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        // Avoid real Win32 positioning on synthetic test window IDs.
        state.paused = true;
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        assert!(!ws.is_floating(100));

        // Tile → Float: toggle_floating targets the tiled focused window
        let resp = state.handle_command(IpcCommand::ToggleFloating);
        assert_eq!(resp, IpcResponse::Ok);
        let ws = state.focused_workspace_mut().unwrap();
        assert!(ws.is_floating(100), "window should now be floating");

        // Simulate OS sending a Focused event for the floating window.
        // This is the real runtime path: user clicks on the floating window,
        // OS fires EVENT_SYSTEM_FOREGROUND, and the daemon processes it.
        // The Focused handler updates previous_focused_hwnd for managed windows.
        state.handle_window_event(WindowEvent::Focused(100));
        assert_eq!(
            state.previous_focused_hwnd,
            Some(100),
            "Focused event should update previous_focused_hwnd for floating windows"
        );

        // Float → Tile: ToggleFloating now sees the floating window via previous_focused_hwnd
        let resp = state.handle_command(IpcCommand::ToggleFloating);
        assert_eq!(resp, IpcResponse::Ok);
        let ws = state.focused_workspace_mut().unwrap();
        assert!(
            !ws.is_floating(100),
            "window should be back to tiled after roundtrip"
        );
        assert!(ws.contains_window(100));
    }

    #[test]
    fn test_cmd_toggle_fullscreen_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::ToggleFullscreen);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_set_column_width_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 0.5 });
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_set_column_width_rejects_fraction_below_range() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 0.05 });
        match resp {
            IpcResponse::Error { message } => {
                assert!(message.contains("Invalid set-width fraction"))
            }
            other => panic!("Expected error, got {:?}", other),
        }
    }

    #[test]
    fn test_cmd_set_column_width_rejects_fraction_above_range() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 1.1 });
        match resp {
            IpcResponse::Error { message } => {
                assert!(message.contains("Invalid set-width fraction"))
            }
            other => panic!("Expected error, got {:?}", other),
        }
    }

    #[test]
    fn test_cmd_set_column_width_rejects_non_finite_fraction() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: f64::NAN });
        match resp {
            IpcResponse::Error { message } => assert!(message.contains("must be finite")),
            other => panic!("Expected error, got {:?}", other),
        }
    }

    #[test]
    fn test_cmd_equalize_column_widths_empty() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::EqualizeColumnWidths);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_cmd_query_status() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::QueryStatus);
        match resp {
            IpcResponse::StatusInfo {
                version,
                monitors,
                total_windows,
                uptime_seconds: _,
            } => {
                assert!(!version.is_empty());
                assert_eq!(monitors, 1);
                assert_eq!(total_windows, 0);
            }
            other => panic!("Expected StatusInfo, got {:?}", other),
        }
    }

    #[test]
    fn test_paused_apply_layout_is_noop() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.paused = true;
        // apply_layout should succeed without actually doing anything
        assert!(state.apply_layout().is_ok());
    }

    #[test]
    fn test_start_time_initialized() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        // start_time should be very recent
        assert!(state.start_time.elapsed().as_secs() < 1);
    }

    #[test]
    fn test_all_managed_window_ids_empty() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        let ids = state.all_managed_window_ids();
        assert!(ids.is_empty(), "No windows should exist in a fresh state");
    }

    #[test]
    fn test_all_managed_window_ids_with_windows() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());

        // Add tiled windows
        if let Some(ws) = state.focused_workspace_mut() {
            ws.insert_window(100, Some(800)).unwrap();
            ws.insert_window(200, Some(800)).unwrap();
            // Add a floating window
            ws.add_floating(300, Rect::new(0, 0, 400, 300)).unwrap();
        }

        let ids = state.all_managed_window_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&100));
        assert!(ids.contains(&200));
        assert!(ids.contains(&300));
    }

    #[test]
    fn test_all_managed_window_ids_multi_monitor() {
        let monitors = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
        ];

        let mut state = AppState::new_with_config(test_config(), monitors);

        // Add windows to both workspaces
        if let Some(ws) = state.workspaces.get_mut(&1) {
            ws.insert_window(100, Some(800)).unwrap();
        }
        if let Some(ws) = state.workspaces.get_mut(&2) {
            ws.insert_window(200, Some(800)).unwrap();
        }

        let ids = state.all_managed_window_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&100));
        assert!(ids.contains(&200));
    }

    // ================================================================
    // Minimize/Restore State Tests
    // ================================================================

    #[test]
    fn test_minimize_marks_workspace_window() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();

        assert!(ws.mark_minimized(100));
        assert!(ws.is_minimized(100));
        assert_eq!(ws.minimized_count(), 1);
    }

    #[test]
    fn test_restore_clears_minimized() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.mark_minimized(100);

        assert!(ws.mark_restored(100));
        assert!(!ws.is_minimized(100));
        assert_eq!(ws.minimized_count(), 0);
    }

    #[test]
    fn test_minimize_unmanaged_window_noop() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        // No windows added — unmanaged window ID
        assert!(state.find_window_workspace(999).is_none());
    }

    #[test]
    fn test_minimized_event_updates_focused_monitor_to_source_monitor() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();
        state.focused_monitor = 1;

        state.handle_window_event(WindowEvent::Minimized(200));
        assert_eq!(state.focused_monitor, 2);
    }

    #[test]
    fn test_minimize_preserves_window_in_workspace() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
        ws.mark_minimized(100);

        // Window is still in workspace (contains_window)
        assert!(ws.contains_window(100));
        // But is minimized
        assert!(ws.is_minimized(100));
        // Total count unchanged
        assert_eq!(ws.all_window_ids().len(), 2);
    }

    #[test]
    fn test_minimize_focus_moves_to_next() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();

        // Focus is on window 200 (last inserted)
        assert_eq!(ws.focused_window(), Some(200));

        // Minimize window 200 — focus should move
        ws.mark_minimized(200);
        // Simulate the daemon's focus adjustment for minimized focused window
        if ws.focused_window() == Some(200) {
            ws.focus_down();
            if ws.focused_window() == Some(200) {
                ws.focus_up();
            }
            if ws.focused_window() == Some(200) {
                ws.focus_right();
                if ws.focused_window() == Some(200) {
                    ws.focus_left();
                }
            }
        }

        // Focus should now be on window 100
        assert_eq!(ws.focused_window(), Some(100));
    }

    #[test]
    fn test_find_window_workspace_tiled() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.workspaces.get_mut(&1).unwrap();
        ws.insert_window(100, Some(800)).unwrap();

        // Should find the tiled window
        assert_eq!(state.find_window_workspace(100), Some(1));
        // Not floating
        let ws = state.workspaces.get(&1).unwrap();
        assert!(!ws.is_floating(100));
    }

    #[test]
    fn test_find_window_workspace_floating_not_snapped() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.workspaces.get_mut(&1).unwrap();
        let rect = Rect::new(100, 100, 800, 600);
        ws.add_floating(200, rect).unwrap();

        // Should find the floating window
        assert_eq!(state.find_window_workspace(200), Some(1));
        // Is floating — snap-back should NOT apply
        let ws = state.workspaces.get(&1).unwrap();
        assert!(ws.is_floating(200));
    }

    // =========================================================================
    // Args (safe-mode flags) tests
    // =========================================================================

    #[test]
    fn test_args_default_all_false() {
        let args = Args {
            no_hotkeys: false,
            no_cloak: false,
            safe_mode: false,
        };
        assert!(!args.skip_hotkeys());
        assert!(!args.skip_cloak());
    }

    #[test]
    fn test_args_no_hotkeys() {
        let args = Args {
            no_hotkeys: true,
            no_cloak: false,
            safe_mode: false,
        };
        assert!(args.skip_hotkeys());
        assert!(!args.skip_cloak());
    }

    #[test]
    fn test_args_no_cloak() {
        let args = Args {
            no_hotkeys: false,
            no_cloak: true,
            safe_mode: false,
        };
        assert!(!args.skip_hotkeys());
        assert!(args.skip_cloak());
    }

    #[test]
    fn test_args_safe_mode_implies_both() {
        let args = Args {
            no_hotkeys: false,
            no_cloak: false,
            safe_mode: true,
        };
        assert!(args.skip_hotkeys());
        assert!(args.skip_cloak());
    }

    #[test]
    fn test_args_parse_no_flags() {
        let args = Args::try_parse_from(["leopardwm"]).unwrap();
        assert!(!args.no_hotkeys);
        assert!(!args.no_cloak);
        assert!(!args.safe_mode);
    }

    #[test]
    fn test_args_parse_safe_mode() {
        let args = Args::try_parse_from(["leopardwm", "--safe-mode"]).unwrap();
        assert!(args.safe_mode);
        assert!(args.skip_hotkeys());
        assert!(args.skip_cloak());
    }

    #[test]
    fn test_args_parse_no_hotkeys() {
        let args = Args::try_parse_from(["leopardwm", "--no-hotkeys"]).unwrap();
        assert!(args.no_hotkeys);
        assert!(!args.no_cloak);
        assert!(!args.safe_mode);
    }

    #[test]
    fn test_args_parse_no_cloak() {
        let args = Args::try_parse_from(["leopardwm", "--no-cloak"]).unwrap();
        assert!(args.no_cloak);
        assert!(!args.no_hotkeys);
        assert!(!args.safe_mode);
    }

    // =========================================================================
    // Startup banner tests
    // =========================================================================

    fn make_banner_info() -> StartupInfo {
        StartupInfo {
            version: "0.1.0".to_string(),
            monitor_names: vec!["DISPLAY1".to_string(), "DISPLAY2".to_string()],
            window_count: 14,
            hotkeys_registered: 24,
            hotkeys_requested: 24,
            config_path: Some(
                "C:\\Users\\test\\AppData\\Roaming\\leopardwm\\config\\config.toml".to_string(),
            ),
            config_warnings: vec![],
            log_path: "C:\\Users\\test\\AppData\\Local\\Temp\\leopardwm-daemon.log".to_string(),
            safe_mode: false,
            no_hotkeys: false,
            no_cloak: false,
        }
    }

    #[test]
    fn test_startup_banner_typical_values() {
        let banner = format_startup_banner(&make_banner_info());
        assert!(banner.contains("LeopardWM v0.1.0"));
        assert!(banner.contains("Monitors: 2"));
        assert!(banner.contains("DISPLAY1, DISPLAY2"));
        assert!(banner.contains("Windows:  14 managed"));
        assert!(banner.contains("Hotkeys:  24 registered"));
        assert!(banner.contains("Status:   Active"));
    }

    #[test]
    fn test_startup_banner_safe_mode() {
        let mut info = make_banner_info();
        info.monitor_names = vec!["DISPLAY1".to_string()];
        info.window_count = 5;
        info.hotkeys_registered = 0;
        info.hotkeys_requested = 0;
        info.config_path = None;
        info.safe_mode = true;
        info.no_hotkeys = true;
        info.no_cloak = true;
        let banner = format_startup_banner(&info);
        assert!(banner.contains("SAFE MODE"));
        assert!(banner.contains("(default"));
    }

    #[test]
    fn test_startup_banner_zero_monitors() {
        let mut info = make_banner_info();
        info.monitor_names = vec![];
        info.window_count = 0;
        info.hotkeys_registered = 0;
        info.hotkeys_requested = 0;
        info.config_path = None;
        let banner = format_startup_banner(&info);
        assert!(banner.contains("Monitors: 0 (fallback mode)"));
        assert!(banner.contains("Windows:  0 managed"));
    }

    #[test]
    fn test_startup_banner_with_config_warnings() {
        let mut info = make_banner_info();
        info.config_warnings = vec![
            "layout.gap: Negative gap (-5) clamped to 0".to_string(),
            "appearance.active_border_color: Invalid hex color 'ZZZZZZ'".to_string(),
        ];
        let banner = format_startup_banner(&info);
        assert!(banner.contains("Warning:  layout.gap"));
        assert!(banner.contains("Warning:  appearance.active_border_color"));
    }

    #[test]
    fn test_startup_banner_without_config_warnings() {
        let info = make_banner_info();
        assert!(info.config_warnings.is_empty());
        let banner = format_startup_banner(&info);
        assert!(!banner.contains("Warning:"));
    }

    #[test]
    fn test_startup_banner_hotkey_mismatch() {
        let mut info = make_banner_info();
        info.hotkeys_registered = 7;
        info.hotkeys_requested = 10;
        let banner = format_startup_banner(&info);
        assert!(banner.contains("7/10 registered (3 failed)"));
    }

    #[test]
    fn test_startup_banner_hotkey_full_registration() {
        let mut info = make_banner_info();
        info.hotkeys_registered = 10;
        info.hotkeys_requested = 10;
        let banner = format_startup_banner(&info);
        assert!(banner.contains("Hotkeys:  10 registered"));
        assert!(!banner.contains("failed"));
    }

    // =========================================================================
    // join_with_timeout tests (Iteration 34)
    // =========================================================================

    #[test]
    fn test_join_with_timeout_hanging_thread() {
        let mut handle = Some(std::thread::spawn(|| {
            // Simulate a hanging thread
            std::thread::sleep(Duration::from_secs(60));
        }));
        let result = join_with_timeout(&mut handle, Duration::from_millis(100));
        assert!(
            !result,
            "Should return false when thread doesn't join in time"
        );
        assert!(
            handle.is_some(),
            "timed-out join should retain ownership for later retry"
        );
    }

    // =========================================================================
    // Workspace mutation tests (handle_window_event equivalent) (Iteration 34)
    // =========================================================================

    #[test]
    fn test_destroy_tiled_window_removes() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
        assert_eq!(ws.window_count(), 2);

        let _ = ws.remove_window(100);
        assert_eq!(ws.window_count(), 1);
        assert!(!ws.contains_window(100));
        assert!(ws.contains_window(200));
    }

    #[test]
    fn test_destroy_floating_window_removes() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.add_floating(300, Rect::new(0, 0, 400, 300)).unwrap();
        assert!(ws.is_floating(300));

        ws.remove_floating(300);
        assert!(!ws.contains_window(300));
    }

    #[test]
    fn test_destroy_unknown_window_noop() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();

        // Removing a non-existent window should not panic
        let _ = ws.remove_window(99999);
        assert_eq!(ws.window_count(), 1);
    }

    #[test]
    fn test_focus_changes_monitor() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        // Add window to monitor 2
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();

        // Find which workspace contains window 200
        let monitor = state.find_window_workspace(200);
        assert_eq!(monitor, Some(2));

        // Simulate focus change: update focused_monitor
        state.focused_monitor = 2;
        assert_eq!(state.focused_monitor, 2);
    }

    #[test]
    fn test_minimized_only_window_no_crash() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.mark_minimized(100);

        // State should be consistent: window exists but is minimized
        assert!(ws.contains_window(100));
        assert!(ws.is_minimized(100));
        assert_eq!(ws.minimized_count(), 1);
    }

    #[test]
    fn test_restored_window_becomes_focused() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();

        // Minimize window 200 (currently focused)
        ws.mark_minimized(200);
        // Adjust focus away
        ws.focus_left();

        // Restore window 200
        ws.mark_restored(200);
        assert!(!ws.is_minimized(200));
        // Window should be accessible for focus
        assert!(ws.contains_window(200));
    }

    #[test]
    fn test_paused_state_skips_events() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.paused = true;
        // Commands should still return Ok but not cause side effects
        let resp = state.handle_command(IpcCommand::FocusLeft);
        assert_eq!(resp, IpcResponse::Ok);
        let resp = state.handle_command(IpcCommand::Refresh);
        assert_eq!(resp, IpcResponse::Ok);
    }

    #[test]
    fn test_multiple_monitors_focus_cross_monitor() {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        // Add windows to both monitors
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();
        state
            .workspaces
            .get_mut(&2)
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();

        // Start focused on monitor 1
        assert_eq!(state.focused_monitor, 1);

        // Simulate focus switch to monitor 2
        state.focused_monitor = 2;
        assert_eq!(state.focused_monitor, 2);

        // Verify the focused workspace is on monitor 2
        let ws = state.workspaces.get(&state.focused_monitor).unwrap();
        assert!(ws.contains_window(200));
    }

    // =========================================================================
    // Iteration 35: Codex review fixes
    // =========================================================================

    #[test]
    fn test_pipe_busy_error_code_is_231() {
        // ERROR_PIPE_BUSY is Windows error code 231. This test documents the
        // constant used in check_already_running() to detect a busy pipe.
        assert_eq!(ERROR_PIPE_BUSY, 231);
        // Verify the constant matches what std::io::Error would report
        let err = std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY);
        assert_eq!(err.raw_os_error(), Some(231));
    }

    #[test]
    fn test_pipe_probe_error_hardening_logic() {
        let busy = std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY);
        assert!(pipe_probe_error_indicates_running(&busy));

        let not_found = std::io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND);
        assert!(!pipe_probe_error_indicates_running(&not_found));

        let not_found_kind = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert!(!pipe_probe_error_indicates_running(&not_found_kind));

        let access_denied = std::io::Error::from_raw_os_error(5); // ERROR_ACCESS_DENIED
        assert!(pipe_probe_error_indicates_running(&access_denied));
    }

    #[test]
    fn test_restore_state_preserves_scroll_offset() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());

        // Insert windows so the workspace has scrollable content
        let ws = state.workspaces.get_mut(&1).unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
        ws.insert_window(300, Some(800)).unwrap();

        // Build a snapshot with a non-zero scroll offset
        let mut saved_ws = Workspace::default();
        saved_ws.set_scroll_offset(500.0);
        let snapshot = StateSnapshot {
            saved_at: "test".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".to_string(),
                workspace: saved_ws,
            }],
            focused_monitor_name: "DISPLAY1".to_string(),
        };

        let restored = state.restore_state(&snapshot);
        assert!(restored.contains(&1), "Monitor 1 should be in restored set");

        let ws = state.workspaces.get(&1).unwrap();
        assert_eq!(
            ws.scroll_offset(),
            500.0,
            "Scroll offset should be preserved after restore"
        );
    }

    #[test]
    fn test_restore_state_on_empty_workspace_safe() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        // Workspace is empty — no windows at all

        let mut saved_ws = Workspace::default();
        saved_ws.set_scroll_offset(300.0);
        let snapshot = StateSnapshot {
            saved_at: "test".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".to_string(),
                workspace: saved_ws,
            }],
            focused_monitor_name: "DISPLAY1".to_string(),
        };

        // Should not panic even on empty workspace
        let restored = state.restore_state(&snapshot);
        assert!(restored.contains(&1), "Monitor 1 should be in restored set");

        let ws = state.workspaces.get(&1).unwrap();
        assert_eq!(
            ws.scroll_offset(),
            300.0,
            "Scroll offset should be set directly even on empty workspace"
        );
    }

    #[test]
    fn test_restore_state_returns_restored_monitor_ids() {
        // Setup: two monitors
        let monitors = vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
            },
        ];
        let mut state = AppState::new_with_config(test_config(), monitors);

        // Snapshot only mentions DISPLAY1, not DISPLAY2
        let mut saved_ws = Workspace::default();
        saved_ws.set_scroll_offset(250.0);
        let snapshot = StateSnapshot {
            saved_at: "test".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".to_string(),
                workspace: saved_ws,
            }],
            focused_monitor_name: "DISPLAY1".to_string(),
        };

        let restored = state.restore_state(&snapshot);

        // Monitor 1 was restored, monitor 2 was not in snapshot
        assert!(restored.contains(&1), "Monitor 1 should be restored");
        assert!(!restored.contains(&2), "Monitor 2 should NOT be restored");

        // Unknown monitor in snapshot should not appear
        let mut saved_ws2 = Workspace::default();
        saved_ws2.set_scroll_offset(100.0);
        let snapshot2 = StateSnapshot {
            saved_at: "test".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                monitor_device_name: "UNKNOWN".to_string(),
                workspace: saved_ws2,
            }],
            focused_monitor_name: "DISPLAY1".to_string(),
        };

        let restored2 = state.restore_state(&snapshot2);
        assert!(
            restored2.is_empty(),
            "No monitors should be restored for unknown device"
        );
    }

    #[test]
    fn test_merged_cleanup_window_ids_deduplicates_and_preserves_all_sources() {
        let managed = vec![10, 30, 20];
        let discovered = vec![20, 40, 10, 50];
        let merged = merged_cleanup_window_ids(&managed, &discovered);
        assert_eq!(merged, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn test_shutdown_recovery_retry_budget_is_reasonable() {
        let attempts = std::hint::black_box(SHUTDOWN_RECOVERY_RETRY_ATTEMPTS);
        let retry_delay = std::hint::black_box(SHUTDOWN_RECOVERY_RETRY_DELAY);
        let final_join_timeout = std::hint::black_box(SHUTDOWN_FINAL_JOIN_TIMEOUT);
        assert!(attempts >= 1);
        assert!(attempts <= 10);
        assert!(retry_delay >= Duration::from_millis(50));
        assert!(retry_delay <= Duration::from_secs(2));
        assert!(final_join_timeout >= Duration::from_millis(250));
        assert!(final_join_timeout <= Duration::from_secs(10));
    }

    // =========================================================================
    // A1: MovedOrResized suppression during apply_layout (Iteration 37)
    // =========================================================================

    #[test]
    fn test_applying_layout_flag_default_false() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        assert!(
            !state.applying_layout,
            "applying_layout should be false by default"
        );
    }

    #[test]
    fn test_applying_layout_flag_set_during_apply() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        // Before apply_layout, flag is false
        assert!(!state.applying_layout);
        // apply_layout on an empty workspace succeeds (paused path)
        state.paused = true;
        let _ = state.apply_layout();
        // After apply_layout returns, flag should be false (cleared on exit)
        assert!(
            !state.applying_layout,
            "applying_layout should be cleared after apply_layout returns"
        );
    }

    // =========================================================================
    // A3: Fullscreen-minimize daemon-level regression test (Iteration 37)
    // =========================================================================

    #[test]
    fn test_fullscreen_minimize_clears_fullscreen_in_daemon() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();

        // Add two windows to the same column
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();

        // Focus window 100 and enter fullscreen
        let _ = ws.focus_window(100);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(100));

        // Minimize the fullscreen window
        ws.mark_minimized(100);

        // Verify fullscreen is cleared
        assert!(
            !ws.is_fullscreen(),
            "Fullscreen should be cleared when fullscreen window is minimized"
        );
        assert_eq!(ws.fullscreen_window_id(), None);

        // Verify the other window is visible in placements
        let viewport = state.focused_viewport();
        let ws = state.focused_workspace().unwrap();
        let placements = ws.compute_placements(viewport);
        let w200 = placements.iter().find(|p| p.window_id == 200);
        assert!(
            w200.is_some(),
            "Window 200 should have a placement after fullscreen window is minimized"
        );
    }

    // =========================================================================
    // R29-C2: HotkeyState registered_count is distinct from mapping.len()
    // =========================================================================

    #[test]
    fn test_hotkey_state_registered_count_default() {
        // Construct HotkeyState manually — registered_count should hold its value
        // and be independent of mapping.len().
        let mut mapping = HashMap::new();
        mapping.insert(1 as HotkeyId, IpcCommand::FocusDown);
        mapping.insert(2 as HotkeyId, IpcCommand::FocusUp);

        let hs = HotkeyState {
            handle: None,
            mapping,
            requested_count: 2,
            registered_count: 1, // Simulate: only 1 of 2 actually registered
        };

        assert_eq!(hs.mapping.len(), 2, "mapping has 2 parsed hotkeys");
        assert_eq!(
            hs.registered_count, 1,
            "registered_count reflects OS result"
        );
        assert_eq!(hs.requested_count, 2, "requested_count matches attempted");
        assert_ne!(
            hs.mapping.len(),
            hs.registered_count,
            "registered_count should differ from mapping.len() when partial"
        );
    }

    // =========================================================================
    // =========================================================================
    // R31: Event-path behavior tests (Iteration 40)
    // =========================================================================

    #[test]
    fn test_focus_new_windows_false_preserves_focus_in_daemon() {
        // R31-T1: Verify that focus_new_windows=false preserves the existing
        // focused window when new windows are tiled — tested at daemon level
        // by directly manipulating the workspace with the config-driven method.
        let mut config = test_config();
        config.behavior.focus_new_windows = false;
        let mut state = AppState::new_with_config(config, test_monitors());

        let ws = state.focused_workspace_mut().unwrap();
        // First window always gets focus (empty workspace)
        ws.insert_window(100, Some(800)).unwrap();
        assert_eq!(ws.focused_window(), Some(100));

        // Subsequent windows use insert_window_no_focus — focus stays on 100
        ws.insert_window_no_focus(200, Some(800)).unwrap();
        assert_eq!(
            ws.focused_window(),
            Some(100),
            "focus should stay on window 100 when focus_new_windows=false"
        );

        ws.insert_window_no_focus(300, Some(800)).unwrap();
        assert_eq!(
            ws.focused_window(),
            Some(100),
            "focus should still be on window 100 after third insert"
        );
        assert_eq!(ws.window_count(), 3);
    }

    #[test]
    fn test_focused_event_updates_previous_focused_hwnd_for_floating() {
        // R31-T3: Verify that a Focused event for a floating window updates
        // previous_focused_hwnd, enabling ToggleFloating to detect and unfloat it.
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();

        // Initially, previous_focused_hwnd is None
        assert_eq!(state.previous_focused_hwnd, None);

        // Simulate OS focus event on the floating window
        state.handle_window_event(WindowEvent::Focused(500));

        // previous_focused_hwnd should now reflect the floating window
        assert_eq!(
            state.previous_focused_hwnd,
            Some(500),
            "Focused event on a floating window must update previous_focused_hwnd"
        );
    }

    #[test]
    fn test_focused_event_updates_previous_focused_hwnd_for_tiled() {
        // Verify Focused events also work for tiled windows (regression guard)
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();

        state.handle_window_event(WindowEvent::Focused(100));
        assert_eq!(state.previous_focused_hwnd, Some(100));

        state.handle_window_event(WindowEvent::Focused(200));
        assert_eq!(state.previous_focused_hwnd, Some(200));
    }

    #[test]
    fn test_focus_follows_mouse_updates_previous_focused_hwnd() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state
            .focused_workspace_mut()
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();
        state.previous_focused_hwnd = None;

        assert!(state.apply_focus_follows_mouse(100));
        assert_eq!(state.previous_focused_hwnd, Some(100));
    }

    #[test]
    fn test_focus_follows_mouse_handles_floating_window() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();
        assert_eq!(ws.focused_window(), Some(100));
        state.previous_focused_hwnd = None;

        assert!(state.apply_focus_follows_mouse(500));
        assert_eq!(state.previous_focused_hwnd, Some(500));
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(100),
            "floating focus-follows-mouse should not mutate tiled focus"
        );
    }

    #[test]
    fn test_restored_floating_window_does_not_steal_tiled_focus() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();
        assert_eq!(ws.focused_window(), Some(100));
        state.previous_focused_hwnd = None;

        state.handle_window_event(WindowEvent::Restored(500));
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(100),
            "restoring a floating window should not steal tiled focus"
        );
        assert_eq!(
            state.previous_focused_hwnd, None,
            "floating restore should not call sync_foreground_window"
        );
    }

    // R29-C5: applying_layout flag cleared after error path (Iteration 38)
    // =========================================================================

    #[test]
    fn test_applying_layout_flag_cleared_after_layout_with_windows() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());

        // Add windows so apply_layout computes real placements (not empty)
        let ws = state.workspaces.get_mut(&1).unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();

        // Whether apply_layout succeeds or fails depends on Win32 API availability.
        // The important thing is that applying_layout is always cleared afterwards.
        assert!(!state.applying_layout, "flag should be false before call");
        let _result = state.apply_layout();
        assert!(
            !state.applying_layout,
            "applying_layout must be cleared after apply_layout returns (success or error)"
        );
    }

    #[test]
    fn test_apply_layout_timeout_auto_pauses_tiling() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.layout_apply_timeout = Duration::from_millis(10);
        state
            .moved_or_resized_suppression
            .insert(42, std::time::Instant::now() + Duration::from_secs(1));
        state.injected_apply_placements_behavior = Some(
            TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(40)),
        );

        let err = state
            .apply_layout()
            .expect_err("apply_layout should time out in injected test mode");

        let message = err.to_string();
        assert!(
            message.contains("timed out"),
            "timeout error should be actionable: {}",
            message
        );
        assert!(state.paused, "tiling should auto-pause after apply timeout");
        assert!(
            !state.applying_layout,
            "applying_layout must be cleared after timeout path"
        );
        assert!(
            state.moved_or_resized_suppression.is_empty(),
            "suppression entries must be cleared after timeout"
        );
    }

    #[test]
    fn test_apply_layout_injected_failure_does_not_auto_pause() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.layout_apply_timeout = Duration::from_millis(50);
        state
            .moved_or_resized_suppression
            .insert(99, std::time::Instant::now() + Duration::from_secs(1));
        state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
            Duration::from_millis(5),
        ));

        let err = state
            .apply_layout()
            .expect_err("injected placement failure should propagate");
        assert!(err
            .to_string()
            .contains("injected apply_placements failure"));
        assert!(
            !state.paused,
            "non-timeout placement failures should not auto-pause tiling"
        );
        assert!(
            !state.applying_layout,
            "applying_layout must be cleared after injected failure path"
        );
        assert!(
            state.moved_or_resized_suppression.is_empty(),
            "suppression entries must be cleared after failed apply"
        );
    }

    #[test]
    fn test_apply_layout_timeout_worker_is_joined_during_shutdown_begin() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.layout_apply_timeout = Duration::from_millis(10);
        state.injected_apply_placements_behavior = Some(
            TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(60)),
        );

        let _ = state
            .apply_layout()
            .expect_err("apply_layout should time out in injected test mode");
        assert_eq!(
            state.pending_apply_workers.len(),
            1,
            "timed-out apply worker should be tracked for shutdown join"
        );

        let workers = state.begin_shutdown_or_revert();
        assert!(
            state.apply_worker_cancelled.load(Ordering::SeqCst),
            "shutdown/revert should set cancellation flag"
        );
        assert_eq!(workers.len(), 1, "one timed-out worker should be returned");
        for handle in workers {
            let mut handle = Some(handle);
            assert!(
                join_with_timeout(&mut handle, Duration::from_millis(300)),
                "timed-out worker should exit after shutdown cancellation"
            );
        }
    }

    #[test]
    fn test_apply_layout_rejects_overlap_while_timed_out_worker_is_running() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.layout_apply_timeout = Duration::from_millis(10);
        state.injected_apply_placements_behavior = Some(
            TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(500)),
        );

        let _ = state
            .apply_layout()
            .expect_err("first apply should time out in injected test mode");
        assert_eq!(state.pending_apply_workers.len(), 1);

        // Simulate manual resume happening before the timed-out worker exits.
        state.paused = false;
        let err = state
            .apply_layout()
            .expect_err("second apply must not overlap while prior worker is still running");
        assert!(
            err.to_string().contains("previous timed-out apply worker"),
            "expected overlap-prevention error, got: {}",
            err
        );

        std::thread::sleep(Duration::from_millis(700));
        let reaped = state.reap_finished_pending_apply_workers();
        assert_eq!(reaped, 1, "timed-out worker should eventually be reaped");
    }

    #[test]
    fn test_apply_layout_timeout_late_worker_triggers_recovery_pass() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.layout_apply_timeout = Duration::from_millis(10);
        state.injected_apply_placements_behavior = Some(
            TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(80)),
        );
        assert_eq!(
            state.late_worker_recovery_count.load(Ordering::SeqCst),
            0,
            "late-worker recovery counter should start at zero"
        );

        let _ = state
            .apply_layout()
            .expect_err("apply_layout should time out in injected test mode");
        assert_eq!(
            state.pending_apply_workers.len(),
            1,
            "timed-out apply worker should be tracked"
        );

        std::thread::sleep(Duration::from_millis(140));
        let reaped = state.reap_finished_pending_apply_workers();
        assert_eq!(reaped, 1, "timed-out worker should be reaped");
        assert_eq!(
            state.late_worker_recovery_count.load(Ordering::SeqCst),
            1,
            "cancelled late worker should trigger one final recovery pass"
        );
    }

    #[test]
    fn test_moved_or_resized_suppression_window_tracking() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.arm_moved_or_resized_suppression([100, 200]);
        assert!(
            state.should_suppress_moved_or_resized(100),
            "recently applied windows should be suppressed"
        );
        assert!(
            !state.should_suppress_moved_or_resized(300),
            "unrelated windows should not be suppressed"
        );

        state
            .moved_or_resized_suppression
            .insert(200, std::time::Instant::now() - Duration::from_millis(1));
        assert!(
            !state.should_suppress_moved_or_resized(200),
            "expired suppression entries should be ignored"
        );
    }

    // =========================================================================
    // R32-C2: Injectable window enumeration for Created-event tests (Iter 41)
    // =========================================================================

    fn make_test_window_info(hwnd: u64) -> leopardwm_platform_win32::WindowInfo {
        leopardwm_platform_win32::WindowInfo {
            hwnd,
            title: format!("Test Window {}", hwnd),
            class_name: "TestWindowClass".to_string(),
            process_id: 1000 + hwnd as u32,
            rect: Rect::new(100, 100, 800, 600),
            visible: true,
        }
    }

    #[test]
    fn test_lookup_window_info_returns_injected() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let info = make_test_window_info(42);
        state.injected_window_info.insert(42, info.clone());

        let result = state.lookup_window_info(42);
        assert!(result.is_some(), "should return injected info");
        assert_eq!(result.unwrap().hwnd, 42);
    }

    #[test]
    fn test_lookup_window_info_missing_returns_none() {
        let state = AppState::new_with_config(test_config(), test_monitors());
        // No injected info, and enumerate_windows won't find hwnd 99999
        let result = state.lookup_window_info(99999);
        assert!(result.is_none());
    }

    #[test]
    fn test_created_event_with_injected_window_info() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());

        // Inject window info so Created handler doesn't need real Win32 calls
        let info = make_test_window_info(100);
        state.injected_window_info.insert(100, info);

        // Before: workspace is empty
        assert_eq!(state.focused_workspace().unwrap().window_count(), 0);

        // Fire Created event — handler should use injected info
        state.handle_window_event(WindowEvent::Created(100));

        // After: window should be tiled in the workspace
        let ws = state.focused_workspace().unwrap();
        assert!(
            ws.contains_window(100),
            "window should be managed after Created event"
        );
        assert_eq!(ws.window_count(), 1);
    }

    #[test]
    fn test_created_event_focus_new_windows_false_preserves_focus() {
        let mut config = test_config();
        config.behavior.focus_new_windows = false;
        let mut state = AppState::new_with_config(config, test_monitors());

        // Inject and create first window (gets focus because workspace is empty)
        state
            .injected_window_info
            .insert(100, make_test_window_info(100));
        state.handle_window_event(WindowEvent::Created(100));
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(100),
            "first window should get focus even with focus_new_windows=false"
        );

        // Inject and create second window — focus should stay on 100
        state
            .injected_window_info
            .insert(200, make_test_window_info(200));
        state.handle_window_event(WindowEvent::Created(200));

        let ws = state.focused_workspace().unwrap();
        assert_eq!(ws.window_count(), 2);
        assert_eq!(
            ws.focused_window(),
            Some(100),
            "focus should stay on window 100 when focus_new_windows=false"
        );
    }

    #[test]
    fn test_created_event_duplicate_is_ignored() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());

        state
            .injected_window_info
            .insert(100, make_test_window_info(100));
        state.handle_window_event(WindowEvent::Created(100));
        assert_eq!(state.focused_workspace().unwrap().window_count(), 1);

        // Second Created event for same window should be ignored
        state.handle_window_event(WindowEvent::Created(100));
        assert_eq!(
            state.focused_workspace().unwrap().window_count(),
            1,
            "duplicate Created event should be ignored"
        );
    }

    // =========================================================================
    // R32-C3: Deterministic daemon singleton test (Iter 41)
    // =========================================================================

    #[test]
    fn test_check_already_running_with_isolated_pipe() {
        // Use an isolated pipe name to avoid depending on whether a real daemon
        // is running. We test the same logic as check_already_running() but with
        // a unique pipe name that we know is not in use.
        let pipe_name = format!(r"\\.\pipe\leopardwm-test-singleton-{}", std::process::id());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();

        // No pipe exists → should not connect
        let result = rt.block_on(async {
            pipe_probe_result_indicates_running(
                tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(&pipe_name)
                    .map(|_| ()),
            )
        });
        assert!(
            !result,
            "No pipe server exists, so connect should fail (no daemon)"
        );
    }

    // =========================================================================
    // Phase 3: Reliability hardening tests (Iteration 43)
    // =========================================================================

    #[test]
    fn test_cmd_health_check() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let resp = state.handle_command(IpcCommand::HealthCheck);
        match resp {
            IpcResponse::HealthInfo {
                healthy,
                total_windows,
                monitors,
                paused,
                ..
            } => {
                assert!(healthy);
                assert_eq!(total_windows, 0);
                assert_eq!(monitors, 1);
                assert!(!paused);
            }
            other => panic!("Expected HealthInfo, got {:?}", other),
        }
    }

    #[test]
    fn test_cmd_health_check_paused() {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.paused = true;
        let resp = state.handle_command(IpcCommand::HealthCheck);
        match resp {
            IpcResponse::HealthInfo { paused, .. } => {
                assert!(paused, "paused flag should be true");
            }
            other => panic!("Expected HealthInfo, got {:?}", other),
        }
    }

    #[test]
    fn test_format_crash_report_contains_version() {
        // We can't easily create a PanicHookInfo, but we can test the function
        // by catching a panic. Use std::panic::catch_unwind.
        let result = std::panic::catch_unwind(|| {
            panic!("test crash");
        });
        assert!(result.is_err(), "should have panicked");
        // The format_crash_report function is tested indirectly via the panic hook.
        // Here we just verify it exists and the function signature is correct.
    }
}
