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
use std::collections::{HashMap, HashSet};
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

        // Update scroll modifier for the gesture hook
        leopardwm_platform_win32::set_scroll_modifier(&self.config.hotkeys.scroll_modifier);

        // Re-check high contrast mode on config reload
        self.refresh_high_contrast();

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

        for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
            let viewport_width = self.monitors.get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

            for workspace in ws_vec.iter_mut() {
                workspace.set_gap(self.config.layout.gap);
                workspace.set_outer_gaps(
                    self.config.layout.outer_gap_left,
                    self.config.layout.outer_gap_right,
                    self.config.layout.outer_gap_top,
                    self.config.layout.outer_gap_bottom,
                );

                // Rescale column widths to preserve fractions under new gap values
                workspace.rescale_column_widths(
                    old_gap, old_outer_left, old_outer_right, viewport_width,
                );

                workspace.set_default_column_width(
                    self.config.layout.default_column_width_px(viewport_width),
                );
                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);

                // Recalculate scroll offset for new gap values so all columns
                // are positioned correctly (not just the rightmost ones).
                workspace.ensure_focused_visible_animated(viewport_width);
            }
        }

        // Re-evaluate window rules for already-managed windows so that
        // newly added/changed rules take effect without restart.
        self.reapply_window_rules();

        // Pick up previously-ignored windows that should now be tiled/floated.
        if let Ok(added) = self.enumerate_and_add_windows() {
            if added > 0 {
                info!("Config reload: tiled {} previously-ignored windows", added);
            }
        }

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
        let mut transitions: Vec<(u64, MonitorId, usize, config::WindowAction, bool)> = Vec::new();

        for (&monitor_id, ws_vec) in &self.workspaces {
            for (ws_idx, workspace) in ws_vec.iter().enumerate() {
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
                    transitions.push((wid, monitor_id, ws_idx, action, is_floating));
                }
            }
            }
        }

        // Pre-compute floating rects before mutating workspaces (avoids borrow conflicts)
        let float_rects: std::collections::HashMap<u64, Rect> = transitions
            .iter()
            .filter(|(_, _, _, action, is_floating)| {
                *action == config::WindowAction::Float && !is_floating
            })
            .filter_map(|(wid, _monitor_id, _, _, _)| {
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

        for (wid, monitor_id, ws_idx, action, is_floating) in transitions {
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
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        let _ = workspace.remove_window(wid);
                        let _ = workspace.add_floating(wid, rect);
                        info!("Rule change: moved window {} to floating", wid);
                    }
                }
                config::WindowAction::Tile if is_floating => {
                    // Currently floating, should be tiled
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        workspace.unfloat_window(wid);
                        info!("Rule change: moved window {} to tiled", wid);
                    }
                }
                config::WindowAction::Ignore => {
                    // Should no longer be managed — remove from workspace
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
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
        let mut snapshots: Vec<WorkspaceSnapshot> = Vec::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let active_idx = self.active_workspace_idx(*monitor_id);
            if let Some(monitor) = self.monitors.get(monitor_id) {
                for (idx, workspace) in ws_vec.iter().enumerate() {
                    // Save non-empty workspaces and the active workspace (even if empty)
                    if !workspace.all_window_ids().is_empty() || idx == active_idx {
                        snapshots.push(WorkspaceSnapshot {
                            monitor_device_name: monitor.device_name.clone(),
                            workspace_index: idx,
                            workspace: workspace.clone(),
                        });
                    }
                }
            }
        }

        let focused_name = self
            .monitors
            .get(&self.focused_monitor)
            .map(|m| m.device_name.clone())
            .unwrap_or_default();

        // Build active workspace map by device name
        let mut active_ws_map = HashMap::new();
        for (&monitor_id, &ws_idx) in &self.active_workspace {
            if let Some(monitor) = self.monitors.get(&monitor_id) {
                active_ws_map.insert(monitor.device_name.clone(), ws_idx);
            }
        }

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
            active_workspace: active_ws_map,
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
                let ws_idx = ws_snapshot.workspace_index;
                if let Some(ws_vec) = self.workspaces.get_mut(&id) {
                    // Extend the vec with empty workspaces if needed
                    while ws_vec.len() <= ws_idx {
                        let mut ws = Workspace::with_directional_gaps(
                            self.config.layout.gap,
                            self.config.layout.outer_gap_left,
                            self.config.layout.outer_gap_right,
                            self.config.layout.outer_gap_top,
                            self.config.layout.outer_gap_bottom,
                        );
                        let vw = self.monitors.get(&id)
                            .map(|m| m.work_area.width)
                            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                        ws.set_default_column_width(self.config.layout.default_column_width_px(vw));
                        ws.set_centering_mode(self.config.layout.centering_mode.into());
                        ws.set_center_past_edges(self.config.layout.center_past_edges);
                        ws.set_reduce_motion(self.reduce_motion);
                        ws_vec.push(ws);
                    }
                    // Restore scroll offset from saved workspace
                    let saved_offset = ws_snapshot.workspace.scroll_offset();
                    ws_vec[ws_idx].set_scroll_offset(saved_offset);
                    restored_monitors.insert(id);
                    info!(
                        "Restored workspace state for monitor '{}' workspace {}",
                        ws_snapshot.monitor_device_name, ws_idx
                    );
                }
            } else {
                debug!(
                    "Skipping saved workspace for unknown monitor '{}'",
                    ws_snapshot.monitor_device_name
                );
            }
        }

        // Restore active workspace indices (validate in range)
        for (device_name, &ws_idx) in &snapshot.active_workspace {
            if let Some((&id, _)) = self
                .monitors
                .iter()
                .find(|(_, m)| &m.device_name == device_name)
            {
                // Clamp to valid range — index must be within the workspace vec
                let max_idx = self.workspaces.get(&id)
                    .map(|v| v.len().saturating_sub(1))
                    .unwrap_or(0);
                self.active_workspace.insert(id, ws_idx.min(max_idx));
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

        // Detect HMONITOR handle changes without physical topology change
        // (e.g., contrast theme switch). Match old→new by device_name and
        // re-key workspace data instead of destroying and recreating it.
        if new_ids != old_ids && new_monitors.len() == self.monitors.len() {
            let old_by_name: HashMap<&str, MonitorId> = self
                .monitors
                .values()
                .map(|m| (m.device_name.as_str(), m.id))
                .collect();
            let new_by_name: HashMap<&str, MonitorId> = new_monitors
                .iter()
                .map(|m| (m.device_name.as_str(), m.id))
                .collect();

            // If every device_name from the old set has a match in the new set,
            // this is a handle change, not a topology change.
            let remap: HashMap<MonitorId, MonitorId> = old_by_name
                .iter()
                .filter_map(|(name, &old_id)| {
                    new_by_name.get(name).map(|&new_id| (old_id, new_id))
                })
                .collect();

            if remap.len() == self.monitors.len() {
                info!(
                    "Monitor handles changed without topology change — re-keying {} workspace(s)",
                    remap.len()
                );
                for (&old_id, &new_id) in &remap {
                    if old_id != new_id {
                        if let Some(ws) = self.workspaces.remove(&old_id) {
                            self.workspaces.insert(new_id, ws);
                        }
                        if let Some(idx) = self.active_workspace.remove(&old_id) {
                            self.active_workspace.insert(new_id, idx);
                        }
                        if self.focused_monitor == old_id {
                            self.focused_monitor = new_id;
                        }
                    }
                }
                // Update monitor info and return — no migration needed
                self.monitors = new_monitors.into_iter().map(|m| (m.id, m)).collect();
                return;
            }
        }

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
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_reduce_motion(self.reduce_motion);
                self.workspaces.insert(monitor.id, vec![workspace]);
                self.active_workspace.insert(monitor.id, 0);
                info!("Created workspace for new monitor {}", monitor.id);
            }
        }

        // Handle removed monitors - migrate ALL workspaces' windows to primary's active workspace
        for removed_id in old_ids.difference(&new_ids) {
            if let Some(old_ws_vec) = self.workspaces.remove(removed_id) {
                // Collect tiled and floating windows separately to preserve their type
                let mut tiled_window_ids = Vec::new();
                let mut floating_windows = Vec::new();
                let mut minimized_ids = Vec::new();
                for old_workspace in &old_ws_vec {
                    for col in old_workspace.columns() {
                        for &wid in col.windows() {
                            tiled_window_ids.push(wid);
                            if old_workspace.is_minimized(wid) {
                                minimized_ids.push(wid);
                            }
                        }
                    }
                    for fw in old_workspace.floating_windows() {
                        floating_windows.push((fw.id, fw.rect));
                        if old_workspace.is_minimized(fw.id) {
                            minimized_ids.push(fw.id);
                        }
                    }
                }
                if let Some(primary) = primary_id {
                    let primary_active_idx = self.active_workspace_idx(primary);
                    // Source monitor info is still available (removed from self.monitors later).
                    let source_wa = self.monitors.get(removed_id).map(|m| m.work_area);
                    let target_wa = self.monitors.get(&primary).map(|m| m.work_area);
                    if let Some(primary_ws) = self.workspaces.get_mut(&primary).and_then(|v| v.get_mut(primary_active_idx)) {
                        for window_id in &tiled_window_ids {
                            if let Err(e) = primary_ws.insert_window(*window_id, None) {
                                warn!("Failed to migrate tiled window {}: {}", window_id, e);
                            }
                        }
                        for (wid, rect) in &floating_windows {
                            let translated = match (source_wa, target_wa) {
                                (Some(src), Some(tgt)) => {
                                    let dx = tgt.x - src.x;
                                    let dy = tgt.y - src.y;
                                    let max_x = (tgt.x + tgt.width - rect.width).max(tgt.x);
                                    let max_y = (tgt.y + tgt.height - rect.height).max(tgt.y);
                                    leopardwm_core_layout::Rect::new(
                                        (rect.x + dx).clamp(tgt.x, max_x),
                                        (rect.y + dy).clamp(tgt.y, max_y),
                                        rect.width,
                                        rect.height,
                                    )
                                }
                                _ => *rect,
                            };
                            if let Err(e) = primary_ws.add_floating(*wid, translated) {
                                warn!("Failed to migrate floating window {}: {}", wid, e);
                            }
                        }
                        // Restore minimized state for migrated windows
                        for wid in &minimized_ids {
                            primary_ws.mark_minimized(*wid);
                        }
                        info!(
                            "Migrated {} tiled + {} floating windows from removed monitor {} to primary",
                            tiled_window_ids.len(),
                            floating_windows.len(),
                            removed_id
                        );
                    }
                }
            }
            self.active_workspace.remove(removed_id);
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
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                ids.extend(workspace.all_window_ids());
            }
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
            let mut stale: Vec<(MonitorId, usize, u64)> = Vec::new();
            for (&monitor_id, ws_vec) in &self.workspaces {
                for (ws_idx, workspace) in ws_vec.iter().enumerate() {
                    for &wid in &workspace.all_window_ids() {
                        if !is_window_alive_and_visible(wid) && !workspace.is_minimized(wid) {
                            stale.push((monitor_id, ws_idx, wid));
                        }
                    }
                }
            }
            for (monitor_id, ws_idx, wid) in &stale {
                if let Some(workspace) = self.workspaces.get_mut(monitor_id).and_then(|v| v.get_mut(*ws_idx)) {
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
            || self.workspaces.values().any(|ws_vec| ws_vec.iter().any(|w| w.is_animating()))
    }

    /// Tick all active animations by the given delta time.
    /// Returns true if any animation is still running.
    pub(crate) fn tick_animations(&mut self, delta_ms: u64) -> bool {
        let mut still_animating = false;
        for ws_vec in self.workspaces.values_mut() {
            for workspace in ws_vec.iter_mut() {
                if workspace.tick_animation(delta_ms) {
                    still_animating = true;
                }
            }
        }
        if let Some(ref mut transition) = self.layout_transition {
            if transition.tick(delta_ms) {
                still_animating = true;
            } else {
                // Transition complete — move exiting windows offscreen.
                for wid in transition.exit_rects.keys() {
                    let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
                }
                self.layout_transition = None;
                // Signal one more frame so entering windows land at their
                // exact final positions (previous frame had t < 1.0).
                still_animating = true;
            }
        }
        still_animating
    }

    /// Snapshot the current placement rects for all tiled windows.
    /// Call this *before* a structural layout change.
    pub(crate) fn snapshot_layout(&self) -> std::collections::HashMap<u64, leopardwm_core_layout::Rect> {
        let mut rects = std::collections::HashMap::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if let Some(monitor) = self.monitors.get(monitor_id) {
                    for p in workspace.compute_placements_animated(monitor.work_area) {
                        rects.insert(p.window_id, p.rect);
                    }
                }
            }
        }
        rects
    }

    /// Start a layout transition animation from a pre-change snapshot.
    /// Call this *after* the structural change and ensure_focused_visible_animated.
    /// No-op when reduce_motion is active.
    pub(crate) fn start_layout_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        if self.reduce_motion {
            return;
        }
        use crate::state::LAYOUT_TRANSITION_DURATION_MS;
        self.start_layout_transition_with_duration(start_rects, LAYOUT_TRANSITION_DURATION_MS);
    }

    pub(crate) fn start_layout_transition_with_duration(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        // Start with one frame (~16ms) already elapsed so the first
        // apply_layout/send_animation_frame shows visible movement.
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects: HashMap::new(),
            elapsed_ms: 16,
            duration_ms,
        });
    }

    /// Start a workspace switch transition that animates both entering and
    /// exiting windows simultaneously (continuous vertical scroll effect).
    /// No-op when reduce_motion is active.
    pub(crate) fn start_workspace_switch_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        exit_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        if self.reduce_motion {
            return;
        }
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects,
            elapsed_ms: 16,
            duration_ms,
        });
    }

    /// Apply layout transition interpolation to placements, including exit windows.
    fn apply_transition_interpolation(
        transition: &LayoutTransition,
        placements: &mut Vec<leopardwm_core_layout::WindowPlacement>,
    ) {
        let t = transition.eased_progress();
        // Interpolate entering/morphing windows.
        for p in placements.iter_mut() {
            if let Some(start) = transition.start_rects.get(&p.window_id) {
                p.rect = leopardwm_core_layout::Rect::new(
                    start.x + ((p.rect.x - start.x) as f64 * t).round() as i32,
                    start.y + ((p.rect.y - start.y) as f64 * t).round() as i32,
                    start.width + ((p.rect.width - start.width) as f64 * t).round() as i32,
                    start.height + ((p.rect.height - start.height) as f64 * t).round() as i32,
                );
            }
        }
        // Interpolate exiting windows (e.g., old workspace sliding out).
        for (wid, target) in &transition.exit_rects {
            if let Some(start) = transition.start_rects.get(wid) {
                placements.push(leopardwm_core_layout::WindowPlacement {
                    window_id: *wid,
                    rect: leopardwm_core_layout::Rect::new(
                        start.x + ((target.x - start.x) as f64 * t).round() as i32,
                        start.y + ((target.y - start.y) as f64 * t).round() as i32,
                        start.width + ((target.width - start.width) as f64 * t).round() as i32,
                        start.height
                            + ((target.height - start.height) as f64 * t).round() as i32,
                    ),
                    visibility: leopardwm_core_layout::Visibility::Visible,
                    column_index: 0,
                });
            }
        }
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
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if let Some(monitor) = self.monitors.get(monitor_id) {
                    let placements = workspace.compute_placements_animated(monitor.work_area);
                    all_placements.extend(placements);
                }
            }
        }
        if all_placements.is_empty() && self.layout_transition.as_ref().is_none_or(|t| t.exit_rects.is_empty()) {
            return Ok(false);
        }

        // Interpolate layout transitions (structural changes like move/expel).
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut all_placements);
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
        if let Err(e) = worker.send_frame(request) {
            self.applying_layout = false;
            return Err(anyhow::anyhow!(e));
        }
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
        // During layout transitions, the animation worker drives positioning.
        if self.layout_transition.is_some() {
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

        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if let Some(monitor) = self.monitors.get(monitor_id) {
                    // Use animated placements to support smooth scrolling
                    let placements = workspace.compute_placements_animated(monitor.work_area);
                    debug!(
                        "Monitor {}: {} placements for viewport {}x{} (animating: {}, scroll: {:.1}, minimized: {})",
                        monitor_id,
                        placements.len(),
                        monitor.work_area.width,
                        monitor.work_area.height,
                        workspace.is_animating(),
                        workspace.effective_scroll_offset(),
                        workspace.minimized_count()
                    );
                    for p in &placements {
                        if p.visibility == leopardwm_core_layout::Visibility::Visible {
                            debug!(
                                "  placement hwnd={:#x} col={} rect=({},{} {}x{}) vis={:?}",
                                p.window_id, p.column_index,
                                p.rect.x, p.rect.y, p.rect.width, p.rect.height,
                                p.visibility,
                            );
                        }
                    }
                    all_placements.extend(placements);
                }
            }
        }

        // Interpolate layout transitions (structural changes like move/expel).
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut all_placements);
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

        let (tx, rx) = std::sync::mpsc::channel::<(Result<()>, Vec<leopardwm_platform_win32::WidthViolation>)>();
        let spawn_result = std::thread::Builder::new()
            .name("leopardwm-apply-layout".to_string())
            .spawn(move || {
                let should_cancel = || {
                    apply_worker_cancelled.load(Ordering::SeqCst)
                        || apply_epoch_ref.load(Ordering::SeqCst) != apply_epoch
                };
                if should_cancel() {
                    let _ = tx.send((Ok(()), Vec::new()));
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
                        let _ = tx.send((Ok(()), Vec::new()));
                        return;
                    }
                    let _ = tx.send((result, Vec::new()));
                    return;
                }

                if should_cancel() {
                    let _ = tx.send((Ok(()), Vec::new()));
                    return;
                }
                let (result, violations) =
                    match leopardwm_platform_win32::apply_placements(&all_placements, &platform_config, None) {
                        Ok(r) => (Ok(()), r.width_violations),
                        Err(e) => (Err(anyhow!(e.to_string())), Vec::new()),
                    };
                if should_cancel() {
                    run_visibility_recovery_pass(&apply_window_ids, "apply-cancelled-late-worker");
                    #[cfg(test)]
                    late_worker_recovery_count.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send((Ok(()), Vec::new()));
                    return;
                }
                let _ = tx.send((result, violations));
            });

        let worker_handle = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                self.applying_layout = false;
                return Err(anyhow!("Failed to spawn layout worker thread: {}", e));
            }
        };

        let result = match rx.recv_timeout(timeout) {
            Ok((result, violations)) => {
                let _ = worker_handle.join();
                if result.is_err() {
                    self.moved_or_resized_suppression.clear();
                }
                // Feed width violations back to the layout engine
                if result.is_ok() && !violations.is_empty() {
                    for v in &violations {
                        for ws_vec in self.workspaces.values_mut() {
                            for ws in ws_vec.iter_mut() {
                                if ws.contains_window(v.window_id) {
                                    ws.set_window_min_width(v.window_id, v.min_width);
                                }
                            }
                        }
                    }
                    for ws_vec in self.workspaces.values_mut() {
                        for ws in ws_vec.iter_mut() {
                            ws.apply_min_width_constraints();
                        }
                    }
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
    /// When high contrast mode is active, returns the system highlight color instead.
    /// Checks the live system setting so toggling high contrast takes effect
    /// immediately without a config reload.
    pub(crate) fn border_color_bgr(&self) -> Option<u32> {
        if leopardwm_platform_win32::is_high_contrast_enabled() {
            return Some(leopardwm_platform_win32::get_system_highlight_color_bgr());
        }
        let color = u32::from_str_radix(&self.config.appearance.active_border_color, 16).ok()?;
        let r = (color >> 16) & 0xFF;
        let g = (color >> 8) & 0xFF;
        let b = color & 0xFF;
        Some((b << 16) | (g << 8) | r)
    }

    /// Refresh the cached `high_contrast` flag from the live system setting.
    /// Returns `true` if the value changed.
    pub(crate) fn refresh_high_contrast(&mut self) -> bool {
        let now = leopardwm_platform_win32::is_high_contrast_enabled();
        if now != self.high_contrast {
            self.high_contrast = now;
            // Theme change invalidates DWM invisible border metrics
            leopardwm_platform_win32::clear_inset_cache();
            if now {
                info!("High contrast mode: border color overridden with system highlight color");
            } else {
                info!("High contrast mode disabled: using config border color");
            }
            true
        } else {
            false
        }
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
            // During resize preview: show border at the preview snap target.
            if let Some(rect) = self.resize_preview_display_rect {
                if self.config.appearance.active_border {
                    if let Some(bgr) = self.border_color_bgr() {
                        frame.show_at_rect(
                            rect,
                            self.config.appearance.active_border_width,
                            self.border_position(),
                            bgr,
                        );
                        return;
                    }
                }
            }
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
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        let viewport = self.monitors.get(&monitor_id)?.work_area;
        let workspace = self.workspaces.get(&monitor_id)?.get(ws_idx)?;
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
    ///
    /// Prefers `previous_focused_hwnd` if it points to a floating window on the
    /// active workspace, so that floating window focus isn't stolen by tiled focus.
    pub(crate) fn sync_foreground_window(&mut self) {
        // If the OS-focused window is a floating window on the active workspace,
        // keep it focused rather than overriding with the tiled focus.
        let floating_focus = self.previous_focused_hwnd.and_then(|hwnd| {
            self.focused_workspace()
                .filter(|ws| ws.is_floating(hwnd))
                .map(|_| hwnd)
        });

        let focused_hwnd = floating_focus.or_else(|| {
            self.focused_workspace()
                .and_then(|ws| ws.focused_visible_window())
        });

        if let Some(hwnd) = focused_hwnd {
            self.show_border(hwnd);

            // Set foreground window — track it regardless of OS result since
            // this is our intended focus. The call can fail if the window
            // vanished between layout and here, which is a transient condition.
            let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
            self.previous_focused_hwnd = Some(hwnd);
        } else {
            // No focused window on the active workspace — clear stale state
            // so border/focus don't target a window that's no longer here.
            self.previous_focused_hwnd = None;
            self.hide_border();
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

            let active_idx = self.active_workspace_idx(monitor_id);
            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(active_idx)) {
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
    /// Returns `(monitor_id, workspace_index)` so callers can index into the correct workspace.
    pub(crate) fn find_window_workspace(&self, window_id: u64) -> Option<(MonitorId, usize)> {
        for (monitor_id, ws_vec) in &self.workspaces {
            for (idx, workspace) in ws_vec.iter().enumerate() {
                if workspace.contains_window(window_id) {
                    return Some((*monitor_id, idx));
                }
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

        // Prefer the OS-foreground window so floating windows can be moved too.
        let src_idx = self.active_workspace_idx(source_monitor);
        let tgt_idx = self.active_workspace_idx(target_monitor);

        let os_focus = self.previous_focused_hwnd.and_then(|hwnd| {
            self.workspaces.get(&source_monitor)
                .and_then(|v| v.get(src_idx))
                .filter(|ws| ws.contains_window(hwnd))
                .map(|_| hwnd)
        });
        let tiled_focus = self.focused_workspace().and_then(|ws| ws.focused_window());
        let Some(window_id) = os_focus.or(tiled_focus) else {
            return Ok(None);
        };

        let Some(source_ws_vec) = self.workspaces.get(&source_monitor) else {
            return Err(format!(
                "Source workspace missing for monitor {}",
                source_monitor
            ));
        };
        let Some(mut source_workspace) = source_ws_vec.get(src_idx).cloned() else {
            return Err(format!(
                "Source workspace index {} missing for monitor {}",
                src_idx, source_monitor
            ));
        };
        let Some(target_ws_vec) = self.workspaces.get(&target_monitor) else {
            return Err(format!(
                "Target workspace missing for monitor {}",
                target_monitor
            ));
        };
        let Some(mut target_workspace) = target_ws_vec.get(tgt_idx).cloned() else {
            return Err(format!(
                "Target workspace index {} missing for monitor {}",
                tgt_idx, target_monitor
            ));
        };

        let is_floating = source_workspace.is_floating(window_id);
        if is_floating {
            let rect = source_workspace.floating_windows()
                .iter()
                .find(|f| f.id == window_id)
                .map(|f| f.rect)
                .unwrap_or(leopardwm_core_layout::Rect::new(0, 0, 800, 600));
            // Translate floating coordinates from source to target monitor work area
            let translated_rect = match (
                self.monitors.get(&source_monitor),
                self.monitors.get(&target_monitor),
            ) {
                (Some(src_mon), Some(tgt_mon)) => {
                    let dx = tgt_mon.work_area.x - src_mon.work_area.x;
                    let dy = tgt_mon.work_area.y - src_mon.work_area.y;
                    let max_x = (tgt_mon.work_area.x + tgt_mon.work_area.width - rect.width).max(tgt_mon.work_area.x);
                    let max_y = (tgt_mon.work_area.y + tgt_mon.work_area.height - rect.height).max(tgt_mon.work_area.y);
                    leopardwm_core_layout::Rect::new(
                        (rect.x + dx).clamp(tgt_mon.work_area.x, max_x),
                        (rect.y + dy).clamp(tgt_mon.work_area.y, max_y),
                        rect.width,
                        rect.height,
                    )
                }
                _ => rect,
            };
            source_workspace.remove_floating(window_id);
            target_workspace
                .add_floating(window_id, translated_rect)
                .map_err(|e| format!("Failed to add floating window to target: {}", e))?;
        } else {
            source_workspace
                .remove_window(window_id)
                .map_err(|e| format!("Failed to remove window: {}", e))?;
            target_workspace
                .insert_window(window_id, None)
                .map_err(|e| format!("Failed to add window to target: {}", e))?;
        }

        let target_viewport = self.viewport_width_for(target_monitor);
        if !is_floating {
            target_workspace.ensure_focused_visible(target_viewport);
        }

        self.workspaces.get_mut(&source_monitor).unwrap()[src_idx] = source_workspace;
        self.workspaces.get_mut(&target_monitor).unwrap()[tgt_idx] = target_workspace;
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
