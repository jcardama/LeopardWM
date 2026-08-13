//! Shared helper methods on AppState: window lookup, config application, snap suppression, pause.

use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::{Rect, Workspace};
#[cfg(not(test))]
use leopardwm_platform_win32::{is_excluded_tool_window_hwnd, is_window_alive_and_visible};
use leopardwm_platform_win32::{scale_px, MonitorId};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StaleCleanupResult {
    layout_changed: bool,
    focus_changed: bool,
}

#[derive(Debug)]
pub(crate) enum StalePruneLayout {
    Unchanged,
    Applied,
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskbarButtonAction {
    Show,
    Hide,
    Unchanged,
}

pub(crate) fn taskbar_button_action(
    is_application_fullscreen: bool,
    should_show: bool,
) -> TaskbarButtonAction {
    if is_application_fullscreen {
        TaskbarButtonAction::Unchanged
    } else if should_show {
        TaskbarButtonAction::Show
    } else {
        TaskbarButtonAction::Hide
    }
}

/// Pre-scaled layout parameters for a specific monitor's DPI.
///
/// Config values are in logical pixels (96 DPI). This struct holds the
/// scaled values for a specific monitor, avoiding repeated scaling in
/// multiple call sites.
pub(crate) struct ScaledLayoutParams {
    pub gap: i32,
    pub outer_gap_left: i32,
    pub outer_gap_right: i32,
    pub outer_gap_top: i32,
    pub outer_gap_bottom: i32,
    pub default_column_width: i32,
    pub tab_strip_reserve_px: i32,
}

impl ScaledLayoutParams {
    /// Compute scaled layout parameters from config + monitor DPI + viewport width.
    pub fn from_config(
        layout: &config::LayoutConfig,
        appearance: &config::AppearanceConfig,
        scale_factor: f64,
        viewport_width: i32,
    ) -> Self {
        let gap = scale_px(layout.gap, scale_factor);
        let outer_gap_left = scale_px(layout.outer_gap_left, scale_factor);
        let outer_gap_right = scale_px(layout.outer_gap_right, scale_factor);
        let outer_gap_top = scale_px(layout.outer_gap_top, scale_factor);
        let outer_gap_bottom = scale_px(layout.outer_gap_bottom, scale_factor);
        // Reserve room for the strip PLUS the inter-element gap below
        // it, so the strip's bottom edge sits `gap` pixels above the
        // active tab — same spacing as between adjacent columns and
        // within a Vertical column. Reusing `layout.gap` keeps the
        // visual rhythm consistent across the workspace.
        let strip_with_gap = appearance.tab_strip_height as i32 + layout.gap.max(0);
        let tab_strip_reserve_px = scale_px(strip_with_gap, scale_factor);

        // Compute default column width using scaled gap values (mirrors LayoutConfig::default_column_width_px)
        let base = viewport_width
            .saturating_sub(outer_gap_left.max(0))
            .saturating_sub(outer_gap_right.max(0))
            .saturating_add(gap.max(0));
        let frac = layout.default_width_fraction();
        let default_column_width = (base as f64 * frac - gap as f64).floor().max(100.0) as i32;

        Self {
            gap,
            outer_gap_left,
            outer_gap_right,
            outer_gap_top,
            outer_gap_bottom,
            default_column_width,
            tab_strip_reserve_px,
        }
    }

    /// Apply scaled gap and width values to a workspace.
    pub fn apply_to(&self, workspace: &mut Workspace) {
        workspace.set_gap(self.gap);
        workspace.set_outer_gaps(
            self.outer_gap_left,
            self.outer_gap_right,
            self.outer_gap_top,
            self.outer_gap_bottom,
        );
        workspace.set_default_column_width(self.default_column_width);
        workspace.set_tab_strip_reserve_px(self.tab_strip_reserve_px);
    }
}

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
        // Config reload may turn off swap_chain_ghost_animation, change
        // monitor geometry assumptions, or simply re-evaluate behavior.
        // Cleanest contract: any in-flight ghost animation dies on
        // reload — the next transition starts from a clean slate.
        self.abort_active_ghost_transition();
        let old_border_on = self.config.appearance.active_border;
        self.compiled_rules = config.compile_window_rules();

        self.config = config;

        // Update scroll modifier for the gesture hook
        leopardwm_platform_win32::set_scroll_modifier(&self.config.hotkeys.scroll_modifier);

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
        self.update_tab_strip();

        for (&monitor_id, ws_vec) in self.workspaces.iter_mut() {
            let scale = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            let viewport_width = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                viewport_width,
            );

            for workspace in ws_vec.iter_mut() {
                // Read the previously-applied scaled gap values from the workspace
                // (not the raw config values) so rescale_column_widths works correctly.
                let old_gap = workspace.gap();
                let (old_ol, old_or, _, _) = workspace.outer_gaps();

                params.apply_to(workspace);

                // Rescale column widths to preserve fractions under new gap values
                workspace.rescale_column_widths(old_gap, old_ol, old_or, viewport_width);

                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );

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

        // Re-check animation state (accessibility setting + power state)
        self.refresh_reduce_motion();

        // Handle snap layout config change (skip when paused — pause already restored all)
        if !self.paused {
            if self.config.behavior.disable_snap_layouts {
                self.disable_snap_for_all_tiled_windows();
            } else {
                self.restore_snap_for_all_windows();
            }
        }

        // Apply a toggled hide_offscreen_taskbar_buttons setting live (restores
        // all buttons when turned off, re-hides off-view ones when turned on).
        self.sync_taskbar_buttons();

        info!(
            "Configuration applied to all {} workspaces",
            self.workspaces.len()
        );
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

    /// Keep a window's taskbar button iff it's actually visible in a viewport:
    /// hidden when on an inactive workspace OR scrolled off-viewport on the
    /// active one; shown when visible (and always for floating/minimized
    /// windows, which the user still reaches via the taskbar). External windows
    /// can't be hidden from the taskbar by cloaking or off-screen position, so
    /// this drives `ITaskbarList` directly. Idempotent and change-gated in the
    /// controller, so it's cheap to call after any layout/scroll change.
    pub(crate) fn sync_taskbar_buttons(&self) {
        use leopardwm_core_layout::Visibility;
        use leopardwm_platform_win32::taskbar::{taskbar_hide, taskbar_show};
        // Disabled: make sure no button stays hidden (restores any we hid before
        // the user turned the option off), then leave the taskbar alone.
        if !self.config.behavior.hide_offscreen_taskbar_buttons {
            for ws_vec in self.workspaces.values() {
                for workspace in ws_vec {
                    for wid in workspace.all_window_ids() {
                        if taskbar_button_action(self.is_application_fullscreen(wid), true)
                            == TaskbarButtonAction::Show
                        {
                            taskbar_show(wid);
                        }
                    }
                }
            }
            return;
        }
        for (&monitor, ws_vec) in &self.workspaces {
            let active = self.active_workspace_idx(monitor);
            let viewport = self.layout_viewport(monitor);
            for (idx, workspace) in ws_vec.iter().enumerate() {
                if idx != active {
                    for wid in workspace.all_window_ids() {
                        match taskbar_button_action(self.is_application_fullscreen(wid), false) {
                            TaskbarButtonAction::Show => taskbar_show(wid),
                            TaskbarButtonAction::Hide => taskbar_hide(wid),
                            TaskbarButtonAction::Unchanged => {}
                        }
                    }
                    continue;
                }
                // Active workspace: a tiled window keeps its button only while
                // it's visible in the viewport; floating and minimized windows
                // always keep theirs.
                let visible: std::collections::HashSet<u64> = workspace
                    .compute_placements(viewport)
                    .iter()
                    .filter(|p| p.visibility == Visibility::Visible)
                    .map(|p| p.window_id)
                    .collect();
                for wid in workspace.all_window_ids() {
                    let keep = workspace.is_floating(wid)
                        || workspace.is_minimized(wid)
                        || visible.contains(&wid);
                    match taskbar_button_action(self.is_application_fullscreen(wid), keep) {
                        TaskbarButtonAction::Show => taskbar_show(wid),
                        TaskbarButtonAction::Hide => taskbar_hide(wid),
                        TaskbarButtonAction::Unchanged => {}
                    }
                }
            }
        }
    }

    /// Remove managed windows that are no longer valid or visible.
    ///
    /// Some apps (e.g., Electron close-to-tray) hide windows without firing
    /// Win32 destroy/hide events. This reconciliation pass detects and removes them.
    ///
    /// Skipped in test builds because test window IDs are not real Win32 handles.
    /// Distinguishes no apply, successful apply, and failed apply so callers
    /// do not treat a logged apply error as success.
    pub(crate) fn prune_stale_windows(&mut self) -> StalePruneLayout {
        #[cfg(test)]
        {
            return StalePruneLayout::Unchanged;
        }

        #[cfg(not(test))]
        {
            let mut stale: Vec<u64> = Vec::new();
            for ws_vec in self.workspaces.values() {
                for workspace in ws_vec.iter() {
                    for &wid in &workspace.all_window_ids() {
                        let alive_visible = is_window_alive_and_visible(wid);
                        let gone = !alive_visible && !workspace.is_minimized(wid);
                        let unmanageable = alive_visible && is_excluded_tool_window_hwnd(wid);
                        if gone || unmanageable {
                            stale.push(wid);
                        }
                    }
                }
            }
            if let Some(drag) = self.drag_state.as_ref() {
                let hwnd = drag.hwnd;
                if !stale.contains(&hwnd) {
                    let alive_visible = is_window_alive_and_visible(hwnd);
                    let minimized = self
                        .find_window_workspace(hwnd)
                        .and_then(|(mid, idx)| self.workspaces.get(&mid)?.get(idx))
                        .is_some_and(|ws| ws.is_minimized(hwnd));
                    let gone = !alive_visible && !minimized;
                    let unmanageable = alive_visible && is_excluded_tool_window_hwnd(hwnd);
                    if gone || unmanageable {
                        stale.push(hwnd);
                    }
                }
            }
            self.finish_stale_window_prune(&stale)
        }
    }

    fn finish_stale_window_prune(&mut self, stale: &[u64]) -> StalePruneLayout {
        if stale.is_empty() {
            self.evict_unmanaged_window_metadata();
            return StalePruneLayout::Unchanged;
        }

        let snapshot = self.snapshot_layout();
        let mut layout_changed = false;
        let mut focus_changed = false;
        for &wid in stale {
            let result = self.cleanup_stale_managed_window(self.find_window_workspace(wid), wid);
            layout_changed |= result.layout_changed;
            focus_changed |= result.focus_changed;
        }
        self.evict_unmanaged_window_metadata();

        let apply = if layout_changed {
            let mut snapshot = snapshot;
            for &wid in stale {
                snapshot.remove(&wid);
            }
            snapshot.remove(&crate::state::DRAG_PLACEHOLDER_HWND);
            self.start_layout_transition(snapshot);
            match self.apply_layout() {
                Ok(()) => StalePruneLayout::Applied,
                Err(e) => StalePruneLayout::Failed(e),
            }
        } else {
            StalePruneLayout::Unchanged
        };
        if layout_changed || focus_changed {
            self.reconcile_border_without_stealing_focus();
        }
        apply
    }

    fn evict_unmanaged_window_metadata(&mut self) {
        if self.window_managed_at.is_empty()
            && self.window_last_maximized_at.is_empty()
            && self.application_fullscreen.is_empty()
        {
            return;
        }
        let managed: std::collections::HashSet<u64> = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter().flat_map(|ws| ws.all_window_ids()))
            .collect();
        self.window_managed_at
            .retain(|hwnd, _| managed.contains(hwnd));
        self.window_last_maximized_at
            .retain(|hwnd, _| managed.contains(hwnd));
        self.application_fullscreen
            .retain(|hwnd, _| managed.contains(hwnd));
    }

    fn cleanup_stale_managed_window(
        &mut self,
        home: Option<(MonitorId, usize)>,
        wid: u64,
    ) -> StaleCleanupResult {
        let cancel = self.cancel_matching_unfinished_move_size_ui(wid);
        self.release_departing_hwnd_ghost(wid);
        let mut layout_changed = cancel.needs_layout();
        let home = home.or(cancel.drag_source);
        if let Some((monitor_id, ws_idx)) = home {
            let viewport_width = self.viewport_width_for(monitor_id);
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(ws_idx))
            {
                if workspace.contains_window(wid) {
                    let was_floating = workspace.remove_floating(wid);
                    if !was_floating {
                        let _ = workspace.remove_window(wid);
                    }
                    layout_changed = true;
                }
                if layout_changed {
                    workspace.ensure_focused_visible_animated(viewport_width);
                }
            }
            self.restore_snap_for_window(wid);
            self.window_managed_at.remove(&wid);
            self.application_fullscreen.remove(&wid);
            info!("Pruned stale window {} from monitor {}", wid, monitor_id);
        } else {
            self.restore_snap_for_window(wid);
            self.window_managed_at.remove(&wid);
            self.application_fullscreen.remove(&wid);
        }

        let mut focus_changed = false;
        if self.previous_focused_hwnd == Some(wid) {
            self.hide_border();
            self.previous_focused_hwnd = None;
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
            focus_changed = true;
        }
        StaleCleanupResult {
            layout_changed,
            focus_changed,
        }
    }

    #[cfg(test)]
    pub(crate) fn prune_stale_window(&mut self, monitor_id: MonitorId, ws_idx: usize, wid: u64) {
        let _ = (monitor_id, ws_idx);
        self.prune_stale_windows_for_test(&[wid]);
    }

    #[cfg(test)]
    pub(crate) fn prune_stale_windows_for_test(&mut self, stale: &[u64]) -> StalePruneLayout {
        self.finish_stale_window_prune(stale)
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

    /// Pixel width of the tiled column currently holding `window_id` on
    /// `(monitor, ws_idx)`, or `None` if the window isn't tiled there. Used to
    /// carry a window's chosen width across a workspace move so it re-tiles at
    /// the same width instead of snapping back to the default column width.
    pub(crate) fn tiled_column_width(
        &self,
        monitor: MonitorId,
        ws_idx: usize,
        window_id: u64,
    ) -> Option<i32> {
        let ws = self.workspaces.get(&monitor)?.get(ws_idx)?;
        let (col, _) = ws.find_window_location(window_id)?;
        ws.column(col).map(|c| c.width())
    }

    /// The column index `window_id` occupies on `(monitor, ws_idx)` plus one
    /// same-column sibling (any other window sharing it), if it is tiled there.
    /// The sibling anchors the column so a later restore survives index shifts
    /// from columns added or removed in the meantime.
    pub(crate) fn tiled_column_origin(
        &self,
        monitor: MonitorId,
        ws_idx: usize,
        window_id: u64,
    ) -> Option<(usize, Option<u64>)> {
        let ws = self.workspaces.get(&monitor)?.get(ws_idx)?;
        let (col, _) = ws.find_window_location(window_id)?;
        let sibling = ws
            .column(col)?
            .windows()
            .iter()
            .copied()
            .find(|&w| w != window_id);
        Some((col, sibling))
    }

    /// Get the rectangle of the focused column for snap hint display.
    ///
    /// Returns the absolute screen position of the focused column.
    pub(crate) fn get_focused_column_rect(&self) -> Option<Rect> {
        let workspace = self.focused_workspace()?;
        self.monitors.get(&self.focused_monitor)?;
        let placements = workspace.compute_placements(self.layout_viewport(self.focused_monitor));

        // Find the placement for the focused window
        let focused_hwnd = workspace.focused_window()?;
        placements
            .iter()
            .find(|p| p.window_id == focused_hwnd)
            .map(|p| p.rect)
    }

    // =========================================================================
    // Snap layout suppression helpers
    // =========================================================================

    /// Remove WS_MAXIMIZEBOX from a tiled window to disable Snap Layouts.
    /// Only acts if `disable_snap_layouts` is enabled and the window isn't already tracked.
    pub(crate) fn disable_snap_for_window(&mut self, hwnd: u64) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        if self.snap_disabled_hwnds.contains(&hwnd) {
            return;
        }
        match leopardwm_platform_win32::remove_maximizebox(hwnd) {
            Ok(true) => {
                self.snap_disabled_hwnds.insert(hwnd);
                debug!("Removed WS_MAXIMIZEBOX from window {}", hwnd);
            }
            Ok(false) => {
                debug!("Window {} already lacks WS_MAXIMIZEBOX, skipping", hwnd);
            }
            Err(e) => {
                warn!("Failed to remove WS_MAXIMIZEBOX for window {}: {}", hwnd, e);
            }
        }
    }

    /// Restore WS_MAXIMIZEBOX on a window when it leaves tiled management.
    pub(crate) fn restore_snap_for_window(&mut self, hwnd: u64) {
        if !self.snap_disabled_hwnds.remove(&hwnd) {
            return;
        }
        match leopardwm_platform_win32::restore_maximizebox(hwnd) {
            Ok(_) => {}
            Err(e) => {
                debug!(
                    "Failed to restore WS_MAXIMIZEBOX for window {}: {}",
                    hwnd, e
                );
            }
        }
    }

    /// Restore WS_MAXIMIZEBOX on all tracked windows (bulk).
    pub(crate) fn restore_snap_for_all_windows(&mut self) {
        let hwnds: Vec<u64> = self.snap_disabled_hwnds.drain().collect();
        if !hwnds.is_empty() {
            leopardwm_platform_win32::restore_maximizebox_all(&hwnds);
            info!("Restored WS_MAXIMIZEBOX for {} window(s)", hwnds.len());
        }
    }

    /// Apply snap layout suppression to all currently tiled (non-floating) windows.
    pub(crate) fn disable_snap_for_all_tiled_windows(&mut self) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        let mut tiled_ids = Vec::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                for col in workspace.columns() {
                    for &wid in col.windows() {
                        tiled_ids.push(wid);
                    }
                }
            }
        }
        for hwnd in tiled_ids {
            self.disable_snap_for_window(hwnd);
        }
    }

    /// Toggle paused state for tiling operations.
    ///
    /// When resuming, this immediately reapplies layout so windows snap back
    /// without waiting for another command/event. If resume reapply fails,
    /// paused state is restored to avoid claiming a healthy resumed mode.
    pub(crate) fn toggle_pause(&mut self, source: &str) -> Result<()> {
        // Pause/resume invalidates any in-flight ghost animation:
        // - apply_layout no-ops while paused (helpers.rs:1044), so a
        //   ghosted window would remain cloaked indefinitely.
        // - send_animation_frame returns Ok(false) when paused, so
        //   the per-frame thumbnail updates would stop mid-animation.
        // Aborting now drops handles + uncloaks sources cleanly.
        self.abort_active_ghost_transition();
        let was_paused = self.paused;
        self.paused = !was_paused;
        info!(
            "Tiling {} via {}",
            if self.paused { "paused" } else { "resumed" },
            source
        );
        if self.paused {
            // Restore WS_MAXIMIZEBOX so windows behave normally while paused
            self.restore_snap_for_all_windows();
            self.hide_border();
            self.hide_tab_strip();
            // Hide any visible drag ghost overlay
            self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        } else {
            self.pending_layout_apply_timeout_report = None;
            if let Err(err) = self.apply_layout() {
                self.paused = was_paused;
                warn!(
                    "Resume apply failed via {}; restoring paused state: {}",
                    source, err
                );
                return Err(err);
            }
            // Re-apply snap suppression after resuming
            self.disable_snap_for_all_tiled_windows();
            self.sync_foreground_window();
        }
        Ok(())
    }
}
