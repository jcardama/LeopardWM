//! Window rule evaluation and application: tile/float/ignore decisions and rule-driven enumeration.

use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::Rect;
use leopardwm_platform_win32::{
    enumerate_windows, find_monitor_for_rect, get_process_executable, scale_px, MonitorId,
};
use tracing::{debug, info, warn};

impl AppState {
    /// Re-evaluate window rules for all managed windows.
    ///
    /// Moves windows between tiled/floating/ignored states based on current rules.
    pub(crate) fn reapply_window_rules(&mut self) {
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
            .filter_map(|(wid, monitor_id, _, _, _)| {
                let win_info = self.lookup_window_info(*wid)?;
                let executable =
                    get_process_executable(win_info.process_id).unwrap_or_default();
                let rect = self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                    Some(*monitor_id),
                );
                Some((*wid, rect))
            })
            .collect();

        for (wid, monitor_id, ws_idx, action, is_floating) in transitions {
            match action {
                config::WindowAction::Float if !is_floating => {
                    // Currently tiled, should be floating — restore snap before moving
                    self.restore_snap_for_window(wid);
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
                        self.disable_snap_for_window(wid);
                        info!("Rule change: moved window {} to tiled", wid);
                    }
                }
                config::WindowAction::Ignore => {
                    // Should no longer be managed — restore snap and remove from workspace
                    self.restore_snap_for_window(wid);
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
                    Some(monitor_id),
                ))
            } else {
                None
            };

            // Skip windows already managed on any workspace (including inactive ones)
            // to prevent duplicates during config reload re-enumeration.
            if self.find_window_workspace(win_info.hwnd).is_some() {
                continue;
            }

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
                                self.disable_snap_for_window(win_info.hwnd);
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
        self.matched_rule(class_name, title, executable)
            .map(|r| r.action)
            .unwrap_or(config::WindowAction::Tile)
    }

    /// The first window rule matching this window, if any (first match wins).
    pub(crate) fn matched_rule(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
    ) -> Option<&config::CompiledWindowRule> {
        self.compiled_rules
            .iter()
            .find(|rule| rule.matches(class_name, title, executable))
    }

    /// Get the floating rect for a window based on rules.
    ///
    /// Rule-defined `width`/`height` are config values (logical pixels) and
    /// are scaled by the monitor's DPI factor. The `monitor_id` parameter
    /// is used to look up the scale factor; pass `None` to skip scaling.
    pub(crate) fn get_floating_rect_from_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        original_rect: &leopardwm_core_layout::Rect,
        monitor_id: Option<MonitorId>,
    ) -> leopardwm_core_layout::Rect {
        let scale = monitor_id
            .and_then(|id| self.monitors.get(&id))
            .map(|m| m.scale_factor)
            .unwrap_or(1.0);
        for rule in &self.compiled_rules {
            if rule.matches(class_name, title, executable) {
                // Only scale rule-provided dimensions (config logical pixels).
                // If a dimension is not specified, use the original rect value
                // which is already in physical pixels from the OS.
                let width = rule.width.map(|w| scale_px(w, scale)).unwrap_or(original_rect.width);
                let height = rule.height.map(|h| scale_px(h, scale)).unwrap_or(original_rect.height);
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
}
