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
        let json = self.build_state_json()?;
        Self::write_state_file(&json)?;
        info!("Workspace state saved to {:?}", Self::state_file_path());
        Ok(())
    }

    /// Atomically write the state JSON: write a uniquely-named temp file then
    /// rename it over the target. Rename is atomic on the same volume, so a
    /// concurrent writer (the debounced background save vs the graceful
    /// shutdown save) or a crash mid-write can never leave a torn/truncated
    /// state file — readers always see a complete previous or new version.
    pub(crate) fn write_state_file(json: &str) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

        let path = Self::state_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Unique temp name per write so two concurrent writers don't share
        // (and tear) the same temp file before their renames.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("{seq}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Build the persisted-state JSON string (StateSnapshot serialized).
    /// Does everything `save_state` does except the filesystem write, so
    /// the debounced background task can build under the lock and write
    /// outside it.
    pub(crate) fn build_state_json(&self) -> Result<String> {
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

        let json = serde_json::to_string_pretty(&snapshot)?;
        Ok(json)
    }

    /// Cheap deterministic hash of the PERSISTED state (everything
    /// `build_state_json` would serialize): focused monitor, per-monitor
    /// active workspace index, every workspace's column membership +
    /// floating windows + rounded scroll offset, and tab title override
    /// keys/value lengths. Used to dedup save requests so unchanged
    /// state (e.g. mid-animation frames with no structural delta) does
    /// not enqueue a write.
    pub(crate) fn persisted_signature(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.focused_monitor.hash(&mut hasher);

        // Sort monitor ids for a deterministic traversal order.
        let mut monitor_ids: Vec<MonitorId> = self.workspaces.keys().copied().collect();
        monitor_ids.sort_unstable();

        for monitor_id in monitor_ids {
            monitor_id.hash(&mut hasher);
            self.active_workspace_idx(monitor_id).hash(&mut hasher);
            if let Some(ws_vec) = self.workspaces.get(&monitor_id) {
                for workspace in ws_vec {
                    // Column window-id membership (Vec<Vec<u64>>).
                    for column in workspace.columns() {
                        column.windows().len().hash(&mut hasher);
                        for &wid in column.windows() {
                            wid.hash(&mut hasher);
                        }
                    }
                    // Floating windows: id + rect.
                    for f in workspace.floating_windows() {
                        f.id.hash(&mut hasher);
                        f.rect.x.hash(&mut hasher);
                        f.rect.y.hash(&mut hasher);
                        f.rect.width.hash(&mut hasher);
                        f.rect.height.hash(&mut hasher);
                    }
                    // Scroll offset rounded to whole pixels.
                    (workspace.scroll_offset().round() as i64).hash(&mut hasher);
                }
            }
        }

        // Tab title overrides: sorted keys + value lengths (cheap).
        let mut overrides: Vec<(u64, usize)> = self
            .tab_title_overrides
            .iter()
            .map(|(&k, v)| (k, v.len()))
            .collect();
        overrides.sort_unstable();
        overrides.hash(&mut hasher);

        hasher.finish()
    }

    /// Request a debounced save iff the persisted state changed since the
    /// last request. Non-blocking: drops the request on a full channel
    /// (a queued request already covers the coalesced write). No-op when
    /// no sender is installed (cfg(test) / pre-wiring).
    pub(crate) fn request_save_if_changed(&mut self) {
        let sig = self.persisted_signature();
        if self.last_persisted_sig != Some(sig) {
            self.last_persisted_sig = Some(sig);
            if let Some(tx) = &self.save_request_tx {
                let _ = tx.try_send(());
            }
        }
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

    /// Restore the FULL saved workspace structure from a snapshot, BEFORE
    /// `enumerate_and_add_windows`. For each saved workspace whose monitor is
    /// still present, the cloned `Workspace` is pruned of dead windows (closed
    /// while the daemon was down), has its `#[serde(skip)]` runtime params
    /// re-applied, and is installed into `self.workspaces[monitor][ws_idx]`.
    /// This brings back column grouping, per-column widths, intra-column
    /// heights, and scroll offset — not just monitor+workspace+order.
    ///
    /// `enumerate_and_add_windows` then skips windows already managed (the
    /// restored ones) and only adds genuinely-new windows by current position.
    ///
    /// Returns the set of `(monitor, ws_idx)` slots that were restored, so the
    /// caller can skip startup width-normalization / scroll-reset for them
    /// (which would otherwise wipe the restored widths and scroll offset).
    pub(crate) fn restore_workspace_structure(
        &mut self,
        snapshot: &StateSnapshot,
    ) -> HashSet<(MonitorId, usize)> {
        self.restore_workspace_structure_with(snapshot, |hwnd| {
            leopardwm_platform_win32::is_valid_window(hwnd)
        })
    }

    /// Testable core of `restore_workspace_structure`: the `is_alive`
    /// predicate decides which HWNDs survive pruning. Production passes the
    /// real `is_valid_window` Win32 call; tests pass a fake so the
    /// structure-rebuild logic can be exercised without Win32.
    pub(crate) fn restore_workspace_structure_with(
        &mut self,
        snapshot: &StateSnapshot,
        is_alive: impl Fn(u64) -> bool,
    ) -> HashSet<(MonitorId, usize)> {
        let mut restored_slots = HashSet::new();

        for ws_snapshot in &snapshot.workspaces {
            let Some(monitor_id) = self
                .monitors
                .iter()
                .find(|(_, m)| m.device_name == ws_snapshot.monitor_device_name)
                .map(|(&id, _)| id)
            else {
                debug!(
                    "Skipping saved workspace for unknown monitor '{}'",
                    ws_snapshot.monitor_device_name
                );
                continue;
            };

            // Clamp to the 1-9 workspace range (0-based 0..=8). The snapshot is
            // user-writable JSON, so a garbage index must not drive the vec to
            // pathological length on startup.
            let ws_idx = ws_snapshot.workspace_index.min(8);

            // Clone the saved workspace and prune windows that closed while the
            // daemon was down. Mirror reconcile/migration pruning: use the
            // type-preserving remove APIs (remove_window / remove_floating).
            let mut ws = ws_snapshot.workspace.clone();
            let dead: Vec<u64> = ws.all_window_ids().into_iter().filter(|&w| !is_alive(w)).collect();
            for wid in dead {
                if ws.is_floating(wid) {
                    ws.remove_floating(wid);
                } else {
                    let _ = ws.remove_window(wid);
                }
            }

            // The clone's #[serde(skip)] runtime fields deserialized to
            // defaults; re-apply them exactly like reconcile_monitors does.
            // apply_to sets gaps + default_column_width + tab_strip_reserve_px
            // WITHOUT touching per-column widths, so the saved widths survive.
            let scale = self.monitors.get(&monitor_id).map(|m| m.scale_factor).unwrap_or(1.0);
            let vw = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                vw,
            );
            params.apply_to(&mut ws);
            ws.set_centering_mode(self.config.layout.centering_mode.into());
            ws.set_center_past_edges(self.config.layout.center_past_edges);
            ws.set_reduce_motion(self.reduce_motion);
            ws.set_scroll_animation(
                self.config.animation.scroll_duration_ms,
                self.config.animation.easing,
            );
            // Preserve the saved scroll offset (it serializes, but set it
            // explicitly so a future skip on this field would not regress).
            ws.set_scroll_offset(ws_snapshot.workspace.scroll_offset());

            // Install into the per-monitor vec, extending with fresh empty
            // workspaces as needed.
            let entry = self.workspaces.entry(monitor_id).or_default();
            while entry.len() <= ws_idx {
                let mut empty = Workspace::with_directional_gaps(
                    params.gap,
                    params.outer_gap_left,
                    params.outer_gap_right,
                    params.outer_gap_top,
                    params.outer_gap_bottom,
                );
                empty.set_default_column_width(params.default_column_width);
                empty.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
                empty.set_centering_mode(self.config.layout.centering_mode.into());
                empty.set_center_past_edges(self.config.layout.center_past_edges);
                empty.set_reduce_motion(self.reduce_motion);
                empty.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );
                entry.push(empty);
            }
            entry[ws_idx] = ws;
            restored_slots.insert((monitor_id, ws_idx));
            info!(
                "Restored workspace structure for monitor '{}' workspace {}",
                ws_snapshot.monitor_device_name, ws_idx
            );
        }

        restored_slots
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
        // case; we don't bother with class/exe tagging.
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
