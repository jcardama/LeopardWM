//! Shared helper methods on AppState: layout, borders, config, state persistence, etc.

use crate::animation_worker;
use crate::config;
use crate::state::*;
use anyhow::{anyhow, Result};
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_platform_win32::{
    enumerate_windows, find_monitor_for_rect, get_process_executable, MonitorId, MonitorInfo,
};
#[cfg(not(test))]
use leopardwm_platform_win32::is_window_alive_and_visible;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

impl AppState {
    /// Look up window info for a given window handle.
    ///
    /// In production, calls `enumerate_windows()` and finds the matching entry.
    /// In tests, returns from the injected window info map if available.
    pub(crate) fn lookup_window_info(
        &self,
        hwnd: u64,
    ) -> Option<leopardwm_platform_win32::WindowInfo> {
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
    pub(crate) fn is_known_window(&self, wid: u64) -> bool {
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
    pub(crate) fn apply_config(&mut self, config: config::Config) {
        // Save old gap values before swapping config so we can rescale columns
        let old_border_on = self.config.appearance.active_border;
        let old_gap = self.config.layout.gap;
        let old_outer_left = self.config.layout.outer_gap_left;
        let old_outer_right = self.config.layout.outer_gap_right;
        self.compiled_rules = config.compile_window_rules();

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

        for (&monitor_id, workspace) in self.workspaces.iter_mut() {
            workspace.set_gap(self.config.layout.gap);
            workspace.set_outer_gaps(
                self.config.layout.outer_gap_left,
                self.config.layout.outer_gap_right,
                self.config.layout.outer_gap_top,
                self.config.layout.outer_gap_bottom,
            );
            let viewport_width = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

            // Rescale column widths to preserve fractions under new gap values
            workspace.rescale_column_widths(
                old_gap, old_outer_left, old_outer_right, viewport_width,
            );

            workspace.set_default_column_width(
                self.config.layout.default_column_width_px(viewport_width),
            );
            workspace.set_centering_mode(self.config.layout.centering_mode.into());

            // Recalculate scroll offset for new gap values so all columns
            // are positioned correctly (not just the rightmost ones).
            workspace.ensure_focused_visible_animated(viewport_width);
        }

        // Re-evaluate window rules for already-managed windows so that
        // newly added/changed rules take effect without restart.
        self.reapply_window_rules();

        info!(
            "Configuration applied to all {} workspaces",
            self.workspaces.len()
        );
    }

    /// Re-evaluate window rules for all managed windows.
    ///
    /// Moves windows between tiled/floating/ignored states based on current rules.
    fn reapply_window_rules(&mut self) {
        // Collect all managed windows with their current state
        let mut transitions: Vec<(u64, MonitorId, config::WindowAction, bool)> = Vec::new();

        for (&monitor_id, workspace) in &self.workspaces {
            for wid in workspace.all_window_ids() {
                let is_floating = workspace.is_floating(wid);
                if let Some(win_info) = self.lookup_window_info(wid) {
                    let executable =
                        get_process_executable(win_info.process_id).unwrap_or_default();
                    let action = self.evaluate_window_rules(
                        &win_info.class_name,
                        &win_info.title,
                        &executable,
                    );
                    transitions.push((wid, monitor_id, action, is_floating));
                }
            }
        }

        // Pre-compute floating rects before mutating workspaces (avoids borrow conflicts)
        let float_rects: std::collections::HashMap<u64, Rect> = transitions
            .iter()
            .filter(|(_, _, action, is_floating)| {
                *action == config::WindowAction::Float && !is_floating
            })
            .filter_map(|(wid, _monitor_id, _, _)| {
                let win_info = self.lookup_window_info(*wid)?;
                let executable =
                    get_process_executable(win_info.process_id).unwrap_or_default();
                let rect = self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                );
                Some((*wid, rect))
            })
            .collect();

        for (wid, monitor_id, action, is_floating) in transitions {
            match action {
                config::WindowAction::Float if !is_floating => {
                    // Currently tiled, should be floating
                    let viewport = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area)
                        .unwrap_or_else(|| {
                            Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT)
                        });
                    let rect = float_rects.get(&wid).copied().unwrap_or_else(|| {
                        Rect::new(
                            viewport.x + (viewport.width - 800) / 2,
                            viewport.y + (viewport.height - 600) / 2,
                            800,
                            600,
                        )
                    });
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        let _ = workspace.remove_window(wid);
                        let _ = workspace.add_floating(wid, rect);
                        info!("Rule change: moved window {} to floating", wid);
                    }
                }
                config::WindowAction::Tile if is_floating => {
                    // Currently floating, should be tiled
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        workspace.unfloat_window(wid);
                        info!("Rule change: moved window {} to tiled", wid);
                    }
                }
                config::WindowAction::Ignore => {
                    // Should no longer be managed — remove from workspace
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
                        if is_floating {
                            workspace.remove_floating(wid);
                        } else {
                            let _ = workspace.remove_window(wid);
                        }
                        self.window_managed_at.remove(&wid);
                        info!("Rule change: unmanaged window {} (ignore)", wid);
                    }
                }
                _ => {} // No change needed
            }
        }
    }

    /// Save current workspace state to disk.
    pub(crate) fn save_state(&self) -> Result<()> {
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
    pub(crate) fn load_state() -> Option<StateSnapshot> {
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
    pub(crate) fn state_file_path() -> std::path::PathBuf {
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
    pub(crate) fn restore_state(&mut self, snapshot: &StateSnapshot) -> HashSet<MonitorId> {
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
    pub(crate) fn reconcile_monitors(&mut self, new_monitors: Vec<MonitorInfo>) {
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
                let mut workspace = Workspace::with_directional_gaps(
                    self.config.layout.gap,
                    self.config.layout.outer_gap_left,
                    self.config.layout.outer_gap_right,
                    self.config.layout.outer_gap_top,
                    self.config.layout.outer_gap_bottom,
                );
                let vw = monitor.work_area.width;
                workspace.set_default_column_width(
                    self.config.layout.default_column_width_px(vw),
                );
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
    pub(crate) fn all_managed_window_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for workspace in self.workspaces.values() {
            ids.extend(workspace.all_window_ids());
        }
        ids
    }

    /// Remove managed windows that are no longer valid or visible.
    ///
    /// Some apps (e.g., Electron close-to-tray) hide windows without firing
    /// Win32 destroy/hide events. This reconciliation pass detects and removes them.
    ///
    /// Skipped in test builds because test window IDs are not real Win32 handles.
    pub(crate) fn prune_stale_windows(&mut self) {
        #[cfg(test)]
        return;

        #[cfg(not(test))]
        {
            let mut stale: Vec<(MonitorId, u64)> = Vec::new();
            for (&monitor_id, workspace) in &self.workspaces {
                for &wid in &workspace.all_window_ids() {
                    if !is_window_alive_and_visible(wid) && !workspace.is_minimized(wid) {
                        stale.push((monitor_id, wid));
                    }
                }
            }
            for (monitor_id, wid) in &stale {
                if let Some(workspace) = self.workspaces.get_mut(monitor_id) {
                    let was_floating = workspace.remove_floating(*wid);
                    if !was_floating {
                        let _ = workspace.remove_window(*wid);
                    }
                    self.window_managed_at.remove(wid);
                    info!("Pruned stale window {} from monitor {}", wid, monitor_id);
                }
            }
        }
    }

    /// Record a short suppression window for moved/resized feedback generated by apply_layout().
    pub(crate) fn arm_moved_or_resized_suppression<I>(&mut self, window_ids: I)
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
    pub(crate) fn should_suppress_moved_or_resized(&mut self, hwnd: u64) -> bool {
        let now = std::time::Instant::now();
        self.moved_or_resized_suppression
            .retain(|_, deadline| *deadline > now);
        self.moved_or_resized_suppression
            .get(&hwnd)
            .is_some_and(|deadline| *deadline > now)
    }

    /// Join any finished timed-out apply workers so the pending list does not grow indefinitely.
    /// Returns the number of workers reaped in this pass.
    pub(crate) fn reap_finished_pending_apply_workers(&mut self) -> usize {
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
    pub(crate) fn begin_shutdown_or_revert(&mut self) -> Vec<std::thread::JoinHandle<()>> {
        self.apply_worker_cancelled.store(true, Ordering::SeqCst);
        self.apply_epoch.fetch_add(1, Ordering::SeqCst);
        std::mem::take(&mut self.pending_apply_workers)
    }

    /// Check if any workspace has an active animation or layout transition.
    pub(crate) fn is_animating(&self) -> bool {
        self.layout_transition.is_some()
            || self.workspaces.values().any(|w| w.is_animating())
    }

    /// Tick all active animations by the given delta time.
    /// Returns true if any animation is still running.
    pub(crate) fn tick_animations(&mut self, delta_ms: u64) -> bool {
        let mut still_animating = false;
        for workspace in self.workspaces.values_mut() {
            if workspace.tick_animation(delta_ms) {
                still_animating = true;
            }
        }
        if let Some(ref mut transition) = self.layout_transition {
            if transition.tick(delta_ms) {
                still_animating = true;
            } else {
                self.layout_transition = None;
            }
        }
        still_animating
    }

    /// Snapshot the current placement rects for all tiled windows.
    /// Call this *before* a structural layout change.
    pub(crate) fn snapshot_layout(&self) -> std::collections::HashMap<u64, leopardwm_core_layout::Rect> {
        let mut rects = std::collections::HashMap::new();
        for (monitor_id, workspace) in &self.workspaces {
            if let Some(monitor) = self.monitors.get(monitor_id) {
                for p in workspace.compute_placements_animated(monitor.work_area) {
                    rects.insert(p.window_id, p.rect);
                }
            }
        }
        rects
    }

    /// Start a layout transition animation from a pre-change snapshot.
    /// Call this *after* the structural change and ensure_focused_visible_animated.
    pub(crate) fn start_layout_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        use crate::state::LAYOUT_TRANSITION_DURATION_MS;
        // Start with one frame (~16ms) already elapsed so the first
        // apply_layout/send_animation_frame shows visible movement.
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            elapsed_ms: 16,
            duration_ms: LAYOUT_TRANSITION_DURATION_MS,
        });
    }

    /// Compute animated placements and send them to the animation worker.
    ///
    /// Returns `Ok(true)` if a frame was sent, `Ok(false)` if paused or no placements.
    pub(crate) fn send_animation_frame(
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

        // Interpolate layout transitions (structural changes like move/expel).
        if let Some(ref transition) = self.layout_transition {
            let t = transition.eased_progress();
            for p in &mut all_placements {
                if let Some(start) = transition.start_rects.get(&p.window_id) {
                    p.rect = leopardwm_core_layout::Rect::new(
                        start.x + ((p.rect.x - start.x) as f64 * t).round() as i32,
                        start.y + ((p.rect.y - start.y) as f64 * t).round() as i32,
                        start.width + ((p.rect.width - start.width) as f64 * t).round() as i32,
                        start.height
                            + ((p.rect.height - start.height) as f64 * t).round() as i32,
                    );
                }
            }
        }

        // Filter out the dragged window and placeholder so SetWindowPos doesn't
        // fight the OS drag or try to position the sentinel.
        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd
                        && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
            }
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
    pub(crate) fn apply_layout(&mut self) -> Result<()> {
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
                    "Monitor {}: {} placements for viewport {}x{} (animating: {}, minimized: {})",
                    monitor_id,
                    placements.len(),
                    monitor.work_area.width,
                    monitor.work_area.height,
                    workspace.is_animating(),
                    workspace.minimized_count()
                );
                all_placements.extend(placements);
            }
        }

        // Interpolate layout transitions (structural changes like move/expel).
        if let Some(ref transition) = self.layout_transition {
            let t = transition.eased_progress();
            for p in &mut all_placements {
                if let Some(start) = transition.start_rects.get(&p.window_id) {
                    p.rect = leopardwm_core_layout::Rect::new(
                        start.x + ((p.rect.x - start.x) as f64 * t).round() as i32,
                        start.y + ((p.rect.y - start.y) as f64 * t).round() as i32,
                        start.width + ((p.rect.width - start.width) as f64 * t).round() as i32,
                        start.height
                            + ((p.rect.height - start.height) as f64 * t).round() as i32,
                    );
                }
            }
        }

        // Filter out the dragged window and placeholder so SetWindowPos doesn't
        // fight the OS drag or try to position the sentinel.
        if let Some(ref drag) = self.drag_state {
            if drag.is_tiled {
                all_placements.retain(|p| {
                    p.window_id != drag.hwnd
                        && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                });
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
                    leopardwm_platform_win32::apply_placements(&all_placements, &platform_config, None)
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
    pub(crate) fn border_color_bgr(&self) -> Option<u32> {
        let color = u32::from_str_radix(&self.config.appearance.active_border_color, 16).ok()?;
        let r = (color >> 16) & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = color & 0xFF;
        Some((b << 16) | (g << 8) | r)
    }

    /// Convert the config border position string to the platform enum.
    pub(crate) fn border_position(&self) -> leopardwm_platform_win32::border::BorderPosition {
        if self.config.appearance.active_border_position == "inside" {
            leopardwm_platform_win32::border::BorderPosition::Inside
        } else {
            leopardwm_platform_win32::border::BorderPosition::Outside
        }
    }

    /// Show the border frame on the given window, or hide it if borders are disabled.
    /// During an active tiled drag, the border is hidden so it doesn't follow
    /// the OS-dragged window — the ghost overlay provides visual feedback instead.
    pub(crate) fn show_border(&self, hwnd: u64) {
        if let Some(ref frame) = self.border_frame {
            // During tiled drag: show border at the window's layout position.
            if let Some(ref drag) = self.drag_state {
                if drag.is_tiled && self.config.appearance.active_border {
                    if let Some(bgr) = self.border_color_bgr() {
                        if let Some(layout_rect) = self.compute_window_layout_rect(hwnd) {
                            frame.show_at_rect(
                                layout_rect,
                                self.config.appearance.active_border_width,
                                self.border_position(),
                                bgr,
                            );
                            return;
                        }
                    }
                    frame.hide();
                    return;
                }
            }
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

    /// Compute the layout rect for a window from the workspace placements.
    fn compute_window_layout_rect(&self, hwnd: u64) -> Option<leopardwm_core_layout::Rect> {
        let monitor_id = self.find_window_workspace(hwnd)?;
        let viewport = self.monitors.get(&monitor_id)?.work_area;
        let workspace = self.workspaces.get(&monitor_id)?;
        let placements = workspace.compute_placements_animated(viewport);
        placements
            .iter()
            .find(|p| p.window_id == hwnd)
            .map(|p| p.rect)
    }

    /// Hide the border frame.
    pub(crate) fn hide_border(&self) {
        if let Some(ref frame) = self.border_frame {
            frame.hide();
        }
    }

    /// Set the OS foreground window to match the workspace's focused window.
    /// Also updates active window border if configured.
    pub(crate) fn sync_foreground_window(&mut self) {
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
    pub(crate) fn enumerate_and_add_windows(&mut self) -> Result<usize> {
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
                                self.window_managed_at.insert(win_info.hwnd, std::time::Instant::now());
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
                        match workspace.insert_window(win_info.hwnd, None) {
                            Ok(()) => {
                                self.window_managed_at.insert(win_info.hwnd, std::time::Instant::now());
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
    pub(crate) fn evaluate_window_rules(
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
    pub(crate) fn get_floating_rect_from_rules(
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
    pub(crate) fn find_window_workspace(&self, window_id: u64) -> Option<MonitorId> {
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
    pub(crate) fn move_focused_window_to_monitor_transactional(
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
    pub(crate) fn get_focused_column_rect(&self) -> Option<Rect> {
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
    pub(crate) fn toggle_pause(&mut self, source: &str) -> Result<()> {
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
}
