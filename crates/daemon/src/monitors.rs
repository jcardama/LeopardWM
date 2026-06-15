//! Monitor topology reconciliation and cross-monitor window moves.

use crate::helpers::ScaledLayoutParams;
use crate::state::*;
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_platform_win32::{MonitorId, MonitorInfo};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

impl AppState {
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
                // Update monitor info — no migration needed
                self.monitors = new_monitors.into_iter().map(|m| (m.id, m)).collect();
                // Re-apply scaled gaps in case DPI or work area changed
                for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
                    let scale = self.monitors.get(&monitor_id).map(|m| m.scale_factor).unwrap_or(1.0);
                    let viewport_width = self.monitors.get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let params = ScaledLayoutParams::from_config(
                        &self.config.layout,
                        &self.config.appearance,
                        scale,
                        viewport_width,
                    );
                    for workspace in ws_vec.iter_mut() {
                        let old_gap = workspace.gap();
                        let (old_ol, old_or, _, _) = workspace.outer_gaps();
                        params.apply_to(workspace);
                        workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);
                    }
                }
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
                let params = ScaledLayoutParams::from_config(
                    &self.config.layout,
                    &self.config.appearance,
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
                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_reduce_motion(self.reduce_motion);
                workspace.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );
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

        // Re-apply scaled gaps to ALL existing workspaces — monitor DPI or work
        // area may have changed even if the monitor ID stayed the same (e.g.,
        // Windows scaling change, or work area resize from taskbar move).
        for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
            let scale = self.monitors.get(&monitor_id).map(|m| m.scale_factor).unwrap_or(1.0);
            let viewport_width = self.monitors.get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                viewport_width,
            );

            for workspace in ws_vec.iter_mut() {
                let old_gap = workspace.gap();
                let (old_ol, old_or, _, _) = workspace.outer_gaps();
                params.apply_to(workspace);
                workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);
            }
        }

        // Update focused monitor if it was removed
        if !self.monitors.contains_key(&self.focused_monitor) {
            self.focused_monitor = primary_id.unwrap_or(0);
        }
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

    /// The layout viewport for a monitor: its full work area. Single source of
    /// truth for the rect fed into `compute_placements*`; columns fill the work
    /// area edge to edge with no shared-edge inset.
    pub(crate) fn layout_viewport(&self, monitor_id: MonitorId) -> Rect {
        self.monitors
            .get(&monitor_id)
            .map(|m| m.work_area)
            .unwrap_or_else(|| {
                Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT)
            })
    }
}
