//! Workspace state persistence: save, load, and restore snapshots.

use crate::helpers::ScaledLayoutParams;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::Workspace;
use leopardwm_platform_win32::MonitorId;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

impl AppState {
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
            tab_title_overrides: self.tab_title_overrides.clone(),
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
                    let scale = self.monitors.get(&id).map(|m| m.scale_factor).unwrap_or(1.0);
                    let vw = self.monitors.get(&id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let params = ScaledLayoutParams::from_config(
                        &self.config.layout,
                        &self.config.appearance,
                        scale,
                        vw,
                    );
                    while ws_vec.len() <= ws_idx {
                        let mut ws = Workspace::with_directional_gaps(
                            params.gap,
                            params.outer_gap_left,
                            params.outer_gap_right,
                            params.outer_gap_top,
                            params.outer_gap_bottom,
                        );
                        ws.set_default_column_width(params.default_column_width);
                        ws.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
                        ws.set_centering_mode(self.config.layout.centering_mode.into());
                        ws.set_center_past_edges(self.config.layout.center_past_edges);
                        ws.set_reduce_motion(self.reduce_motion);
                        ws.set_scroll_animation(
                            self.config.animation.scroll_duration_ms,
                            self.config.animation.easing,
                        );
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

        // Restore tab title overrides, pruning entries whose HWND is no
        // longer live. Guards against HWND reuse across daemon-offline
        // window closures: if the original window was destroyed while
        // the daemon was down, Windows can re-issue the same HWND to a
        // different process and the persisted override would silently
        // attach. The `IsWindow` check is cheap and catches the common
        // case; we don't bother with class/exe tagging in v0.1.15.
        for (&hwnd, title) in &snapshot.tab_title_overrides {
            if leopardwm_platform_win32::is_valid_window(hwnd) {
                self.tab_title_overrides.insert(hwnd, title.clone());
            } else {
                debug!(
                    "Pruning stale tab title override for dead HWND {}: {:?}",
                    hwnd, title
                );
            }
        }

        restored_monitors
    }
}
