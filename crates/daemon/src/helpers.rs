use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::{Rect, Workspace};
#[cfg(not(test))]
use leopardwm_platform_win32::{is_excluded_tool_window_hwnd, window_presence};
use leopardwm_platform_win32::{scale_px, MonitorId, WindowPresence};
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

/// Set only for a prune reached from `Focused`. Refresh and direct test prunes
/// pass `None` and keep execution-time foreground sampling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FocusedPruneContext {
    pub(crate) focused_hwnd: u64,
    /// `previous_focused_hwnd` captured before stale cleanup.
    pub(crate) tracked: Option<u64>,
    pub(crate) event_time_ms: u32,
    /// The focused HWND is a restore activation, so tracked-iconic reconciliation
    /// must not infer an OS minimize/departure handoff from this Focused event.
    pub(crate) restore_activation: bool,
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

/// Config distances are logical pixels (96 DPI); workspace distances are
/// scaled for the monitor.
pub(crate) struct ScaledLayoutParams {
    pub gap: i32,
    pub outer_gap_left: i32,
    pub outer_gap_right: i32,
    pub outer_gap_top: i32,
    pub outer_gap_bottom: i32,
    pub default_column_width: i32,
    pub tab_strip_reserve_px: i32,
    pub row_gap: i32,
    pub row_shares_by_workspace: Vec<Vec<f64>>,
}

impl ScaledLayoutParams {
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
        let row_gap = scale_px(layout.rows.gap, scale_factor);
        let row_shares_by_workspace: Vec<Vec<f64>> = (0..9)
            .map(|idx| layout.rows.parse_shares_for_workspace(idx))
            .collect();
        // Reserve room for the strip PLUS the inter-element gap below
        // it, so the strip's bottom edge sits `gap` pixels above the
        // active tab — same spacing as between adjacent columns and
        // within a Vertical column. Reusing `layout.gap` keeps the
        // visual rhythm consistent across the workspace.
        let strip_with_gap = appearance.tab_strip_height as i32 + layout.gap.max(0);
        let tab_strip_reserve_px = scale_px(strip_with_gap, scale_factor);

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
            row_gap,
            row_shares_by_workspace,
        }
    }

    /// Row shares for a 0-based workspace index.
    pub fn row_shares_for(&self, ws_idx: usize) -> &[f64] {
        self.row_shares_by_workspace
            .get(ws_idx)
            .map(|s| s.as_slice())
            .unwrap_or(&[1.0])
    }

    /// Apply scaled gap and width values to a workspace at a given 0-based index.
    pub fn apply_to(&self, workspace: &mut Workspace, ws_idx: usize) {
        workspace.set_gap(self.gap);
        workspace.set_outer_gaps(
            self.outer_gap_left,
            self.outer_gap_right,
            self.outer_gap_top,
            self.outer_gap_bottom,
        );
        workspace.set_default_column_width(self.default_column_width);
        workspace.set_tab_strip_reserve_px(self.tab_strip_reserve_px);
        workspace.set_row_gap(self.row_gap);
        workspace.set_row_layout(self.row_shares_for(ws_idx));
    }
}

impl AppState {
    /// Tests may override individual HWNDs; otherwise query the window directly.
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

    /// Check whether a window ID is known to this state (managed, ignored, or injected).
    ///
    /// Used by event validation to skip `is_valid_window` for windows we have
    /// info about, even if they aren't yet managed (e.g., during Created events).
    /// Ignored HWND liveness is one identity read: matching tokens stay known,
    /// injected transient reads stay known, Gone stays unknown, mismatch/missing
    /// fall through. Production reads are `IsWindow` plus `GetPropW` (handle or
    /// NULL), not a documented GetLastError path.
    pub(crate) fn is_known_window(&self, wid: u64) -> bool {
        if self.find_window_workspace(wid).is_some() {
            return true;
        }
        if let Some(known) = self.temporary_ignore_known(wid) {
            return known;
        }
        #[cfg(test)]
        {
            if self.injected_window_info.contains_key(&wid) {
                return true;
            }
        }
        false
    }

    pub(crate) fn apply_config(&mut self, config: config::Config) {
        // Reload can change ghost-animation settings or monitor assumptions;
        // abort the in-flight transition before applying the new config.
        self.abort_active_ghost_transition();
        let old_border_on = self.config.appearance.active_border;
        let previous_rules =
            std::mem::replace(&mut self.compiled_rules, config.compile_window_rules());

        self.config = config;

        leopardwm_platform_win32::set_scroll_modifier(&self.config.hotkeys.scroll_modifier);
        leopardwm_platform_win32::set_wheel_swipes(self.config.gestures.wheel_swipes);

        self.refresh_high_contrast();

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

            for (ws_idx, workspace) in ws_vec.iter_mut().enumerate() {
                // Read the previously-applied scaled gap values from the workspace
                // (not the raw config values) so rescale_column_widths works correctly.
                let old_gap = workspace.gap();
                let (old_ol, old_or, _, _) = workspace.outer_gaps();
                let was_center_single_column = workspace.center_single_column();

                params.apply_to(workspace, ws_idx);
                workspace.set_centering_mode(self.config.layout.centering_mode.into());
                workspace.set_center_past_edges(self.config.layout.center_past_edges);
                workspace.set_center_single_column(self.config.layout.center_single_column);

                workspace.rescale_column_widths(
                    old_gap,
                    old_ol,
                    old_or,
                    viewport_width,
                    viewport_width,
                );
                workspace.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );

                if was_center_single_column && !self.config.layout.center_single_column {
                    workspace.reconcile_scroll_bounds(viewport_width);
                }

                workspace.ensure_focused_visible_animated(viewport_width);
            }
        }

        self.reapply_window_rules(&previous_rules);

        if let Ok(added) = self.enumerate_and_add_windows() {
            if added > 0 {
                info!("Config reload: tiled {} previously-ignored windows", added);
            }
        }

        self.refresh_reduce_motion();

        // Handle snap layout config change (skip when paused — pause already restored all)
        if !self.paused {
            if self.config.behavior.disable_snap_layouts {
                self.disable_snap_for_all_tiled_windows();
            } else {
                self.restore_snap_for_all_windows();
            }
        }

        self.sync_taskbar_buttons();

        info!(
            "Configuration applied to all {} workspaces",
            self.workspaces.len()
        );
    }

    pub(crate) fn all_managed_window_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                ids.extend(workspace.all_window_ids());
            }
        }
        ids
    }

    fn apply_taskbar_button_action(&self, wid: u64, action: TaskbarButtonAction) {
        use leopardwm_platform_win32::taskbar::{taskbar_hide, taskbar_show};
        match action {
            TaskbarButtonAction::Show => {
                #[cfg(test)]
                self.recorded_taskbar_commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((wid, true));
                taskbar_show(wid);
            }
            TaskbarButtonAction::Hide => {
                #[cfg(test)]
                self.recorded_taskbar_commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((wid, false));
                taskbar_hide(wid);
            }
            TaskbarButtonAction::Unchanged => {}
        }
    }

    pub(crate) fn restore_taskbar_buttons(&self) {
        for wid in self.all_managed_window_ids() {
            self.apply_taskbar_button_action(wid, TaskbarButtonAction::Show);
        }
    }

    pub(crate) fn sync_inactive_workspace_taskbar_button(&self, wid: u64) {
        let show =
            self.paused || self.config.behavior.taskbar_buttons == config::TaskbarButtons::ShowAll;
        self.apply_taskbar_button_action(wid, taskbar_button_action(false, show));
    }

    pub(crate) fn sync_taskbar_buttons(&self) {
        use leopardwm_core_layout::Visibility;
        if self.paused {
            self.restore_taskbar_buttons();
            return;
        }
        if self.config.behavior.taskbar_buttons == config::TaskbarButtons::ShowAll {
            for ws_vec in self.workspaces.values() {
                for workspace in ws_vec {
                    for wid in workspace.all_window_ids() {
                        if taskbar_button_action(self.is_application_fullscreen(wid), true)
                            == TaskbarButtonAction::Show
                        {
                            self.apply_taskbar_button_action(wid, TaskbarButtonAction::Show);
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
                        self.apply_taskbar_button_action(
                            wid,
                            taskbar_button_action(self.is_application_fullscreen(wid), false),
                        );
                    }
                    continue;
                }
                // HideOffscreen keeps visible, floating, and minimized buttons
                // on the active workspace; HideInactiveWorkspaces keeps them all.
                let visible: std::collections::HashSet<u64> = workspace
                    .compute_placements(viewport)
                    .iter()
                    .filter(|p| p.visibility == Visibility::Visible)
                    .map(|p| p.window_id)
                    .collect();
                for wid in workspace.all_window_ids() {
                    let keep = self.config.behavior.taskbar_buttons
                        == config::TaskbarButtons::HideInactiveWorkspaces
                        || workspace.is_floating(wid)
                        || workspace.is_minimized(wid)
                        || visible.contains(&wid);
                    self.apply_taskbar_button_action(
                        wid,
                        taskbar_button_action(self.is_application_fullscreen(wid), keep),
                    );
                }
            }
        }
    }

    /// Remove managed windows that are no longer valid or visible.
    ///
    /// Some apps (e.g., Electron close-to-tray) hide windows without firing
    /// Win32 destroy/hide events. This reconciliation pass detects and removes them.
    ///
    /// Standalone entry used by Refresh and the tracked-focus liveness tick.
    /// Focus handling passes attribution context through
    /// `prune_stale_windows_with`. Test builds use injected presence states in
    /// the same scan, consuming the injected stale list only when this prune
    /// runs. Distinguishes no apply, successful apply, and failed apply so
    /// callers do not treat a logged apply error as success.
    pub(crate) fn prune_stale_windows(&mut self) -> StalePruneLayout {
        self.prune_stale_windows_with(None)
    }

    /// Drop a tracked focus window that vanished without Destroyed or Hidden.
    ///
    /// No WinEvent covers that disappearance. The daemon tick calls this so the
    /// empty workspace is discovered on its own; a later activation is then
    /// newer than the prune instead of being attributed as auto-activation.
    /// Returns whether the prune changed managed state. An alive tracked window
    /// costs one liveness check and does not scan the other windows.
    pub(crate) fn check_tracked_focus_liveness(&mut self) -> bool {
        if self.paused {
            return false;
        }
        let Some(tracked) = self.previous_focused_hwnd else {
            return false;
        };
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(tracked) else {
            return false;
        };
        let minimized = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .is_some_and(|workspace| workspace.is_minimized(tracked));
        if !self.tracked_focus_is_gone_or_unmanageable(tracked, minimized) {
            return false;
        }

        let now = std::time::Instant::now();
        let pre_count = self.all_managed_window_ids().len();
        let prune = self.prune_stale_windows();
        let pruned = pre_count - self.all_managed_window_ids().len();
        let changed = pruned > 0 || !matches!(prune, StalePruneLayout::Unchanged);
        match prune {
            StalePruneLayout::Applied => {}
            StalePruneLayout::Failed(e) => {
                warn!(
                    "Failed to apply layout after pruning {} stale window(s): {}",
                    pruned, e
                );
            }
            StalePruneLayout::Unchanged if pruned > 0 => {
                if let Err(e) = self.apply_layout() {
                    warn!(
                        "Failed to apply layout after pruning {} stale window(s): {}",
                        pruned, e
                    );
                }
            }
            StalePruneLayout::Unchanged => {}
        }
        self.last_prune_at = Some(now);
        changed
    }

    pub(crate) fn stale_window_probe(&self, hwnd: u64) -> (WindowPresence, bool) {
        #[cfg(test)]
        {
            let presence = if self.injected_stale_hwnds.contains(&hwnd) {
                WindowPresence::Hidden
            } else if self.injected_iconic_hwnds.contains(&hwnd) {
                WindowPresence::Minimized
            } else {
                WindowPresence::Visible
            };
            (presence, false)
        }

        #[cfg(not(test))]
        {
            let presence = window_presence(hwnd);
            let excluded_tool_window =
                presence == WindowPresence::Visible && is_excluded_tool_window_hwnd(hwnd);
            (presence, excluded_tool_window)
        }
    }

    fn presence_is_stale(
        presence: WindowPresence,
        excluded_tool_window: bool,
        marked_minimized: bool,
    ) -> bool {
        match presence {
            WindowPresence::Gone | WindowPresence::Hidden => !marked_minimized,
            WindowPresence::Minimized => false,
            WindowPresence::Visible => excluded_tool_window,
        }
    }

    fn window_is_stale(&self, hwnd: u64, marked_minimized: bool) -> bool {
        let (presence, excluded_tool_window) = self.stale_window_probe(hwnd);
        Self::presence_is_stale(presence, excluded_tool_window, marked_minimized)
    }

    fn tracked_focus_is_gone_or_unmanageable(&self, tracked: u64, minimized: bool) -> bool {
        self.window_is_stale(tracked, minimized)
    }

    pub(crate) fn prune_stale_windows_with(
        &mut self,
        focused_prune: Option<FocusedPruneContext>,
    ) -> StalePruneLayout {
        let mut stale: Vec<u64> = Vec::new();
        let mut unmarked_iconic = Vec::new();
        let mut observed = std::collections::HashSet::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec.iter() {
                for &wid in &workspace.all_window_ids() {
                    observed.insert(wid);
                    let marked_minimized = workspace.is_minimized(wid);
                    let (presence, excluded_tool_window) = self.stale_window_probe(wid);
                    if !marked_minimized && presence == WindowPresence::Minimized {
                        unmarked_iconic.push(wid);
                    }
                    if Self::presence_is_stale(presence, excluded_tool_window, marked_minimized) {
                        stale.push(wid);
                    }
                }
            }
        }
        if let Some(drag) = self.drag_state.as_ref() {
            let hwnd = drag.hwnd;
            if observed.insert(hwnd) {
                let minimized = self
                    .find_window_workspace(hwnd)
                    .and_then(|(mid, idx)| self.workspaces.get(&mid)?.get(idx))
                    .is_some_and(|ws| ws.is_minimized(hwnd));
                let (presence, excluded_tool_window) = self.stale_window_probe(hwnd);
                if !minimized && presence == WindowPresence::Minimized {
                    unmarked_iconic.push(hwnd);
                }
                if Self::presence_is_stale(presence, excluded_tool_window, minimized) {
                    stale.push(hwnd);
                }
            }
        }
        #[cfg(test)]
        self.injected_stale_hwnds.clear();
        let result = self.finish_stale_window_prune(&stale, focused_prune);

        let selected = (
            self.focused_monitor,
            self.active_workspace_idx(self.focused_monitor),
        );
        let tracked_selected = self
            .previous_focused_hwnd
            .filter(|hwnd| self.find_window_workspace(*hwnd) == Some(selected));
        let focused_event_hwnd = focused_prune.map(|focus| focus.focused_hwnd);
        let mut batch_snapshot = None;
        let mut batch_changed = false;
        for hwnd in unmarked_iconic.iter().copied() {
            if Some(hwnd) == tracked_selected || Some(hwnd) == focused_event_hwnd {
                continue;
            }
            if batch_snapshot.is_none() {
                batch_snapshot = Some(self.snapshot_layout());
            }
            if self.stale_window_probe(hwnd).0 == WindowPresence::Minimized
                && self.mark_minimized_and_reflow(hwnd).is_some()
            {
                batch_changed = true;
            }
        }

        if let Some(hwnd) = tracked_selected
            .filter(|hwnd| unmarked_iconic.contains(hwnd) && Some(*hwnd) != focused_event_hwnd)
        {
            if self.stale_window_probe(hwnd).0 == WindowPresence::Minimized {
                if focused_prune.is_some_and(|focus| focus.restore_activation) {
                    self.reconcile_minimized_without_departure(hwnd);
                } else if let Some(focus) = focused_prune {
                    self.on_window_minimized_with_snapshot_at(
                        hwnd,
                        batch_snapshot,
                        Some(focus.event_time_ms),
                    );
                } else {
                    self.on_window_minimized_with_snapshot(hwnd, batch_snapshot);
                }
                return result;
            }
        }

        if batch_changed {
            if self.start_layout_transition(batch_snapshot.unwrap()) {
                if let Some(transition) = self.layout_transition.as_mut() {
                    transition.suppress_landing_focus_resync = true;
                }
            }
            if let Err(e) = self.apply_layout() {
                warn!(
                    "Failed to apply layout after minimize reconciliation: {}",
                    e
                );
            }
        }
        result
    }

    fn finish_stale_window_prune(
        &mut self,
        stale: &[u64],
        focused_prune: Option<FocusedPruneContext>,
    ) -> StalePruneLayout {
        if stale.is_empty() {
            self.evict_unmanaged_window_metadata();
            return StalePruneLayout::Unchanged;
        }

        let selected_was_occupied = !self.selected_workspace_is_genuinely_empty();
        let selected = (
            self.focused_monitor,
            self.active_workspace_idx(self.focused_monitor),
        );
        let stale_homes: Vec<(u64, Option<(MonitorId, usize)>)> = stale
            .iter()
            .copied()
            .map(|wid| (wid, self.find_window_workspace(wid)))
            .collect();

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
                Ok(crate::layout_apply::LayoutApplyOutcome::Completed) => StalePruneLayout::Applied,
                Ok(crate::layout_apply::LayoutApplyOutcome::DeferredByRecoveryBarrier) => {
                    StalePruneLayout::Failed(anyhow::anyhow!(
                        "Layout application deferred while recovery animation placement finishes"
                    ))
                }
                Err(e) => StalePruneLayout::Failed(e),
            }
        } else {
            StalePruneLayout::Unchanged
        };
        if selected_was_occupied && self.selected_workspace_is_genuinely_empty() {
            self.clear_logical_focus_for_empty_selection();
            let sample_replacement = match focused_prune {
                None => true,
                Some(focus) => focus.tracked.is_some_and(|tracked| {
                    stale_homes
                        .iter()
                        .any(|&(wid, home)| wid == tracked && home == Some(selected))
                }),
            };
            let replacement = if sample_replacement {
                self.departing_foreground_evidence().and_then(
                    |(foreground, valid)| match foreground {
                        Some(id) if id != 0 && valid && !stale.contains(&id) => Some(id),
                        _ => None,
                    },
                )
            } else {
                None
            };
            let armed_at = match focused_prune {
                Some(focus) => focus.event_time_ms,
                None => self.event_time_now_ms(),
            };
            self.arm_pending_last_window_departure(
                replacement,
                armed_at,
                LastWindowDepartureOrigin::EventlessPrune,
            );
        } else if layout_changed || focus_changed {
            self.reconcile_border_without_stealing_focus();
        }
        apply
    }

    fn evict_unmanaged_window_metadata(&mut self) {
        if self.window_managed_at.is_empty()
            && self.window_last_maximized_at.is_empty()
            && self.post_admission_maximize_restore_eligible.is_empty()
            && self.application_fullscreen.is_empty()
            && self.recreated_window_slots.identities.is_empty()
            && self.managed_lifetime_tokens.is_empty()
            && self.managed_lifetime_admitted_at_event_ms.is_empty()
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
        // A tiled drag or stashed scratchpad leaves its workspace while still managed.
        self.managed_lifetime_tokens.retain(|hwnd, _| {
            managed.contains(hwnd)
                || self
                    .drag_state
                    .as_ref()
                    .is_some_and(|drag| drag.is_tiled && drag.hwnd == *hwnd)
                || crate::managed_lifetime::is_stashed_scratchpad(self.scratchpad, *hwnd)
        });
        self.managed_lifetime_admitted_at_event_ms
            .retain(|hwnd, _| self.managed_lifetime_tokens.contains_key(hwnd));
        self.post_admission_maximize_restore_eligible
            .retain(|hwnd, _| self.managed_lifetime_tokens.contains_key(hwnd));
        self.recreated_window_slots
            .identities
            .retain(|hwnd, _| self.managed_lifetime_tokens.contains_key(hwnd));
    }

    fn cleanup_stale_managed_window(
        &mut self,
        home: Option<(MonitorId, usize)>,
        wid: u64,
    ) -> StaleCleanupResult {
        if self
            .pending_workspace_switch_focus
            .is_some_and(|intent| intent.source_hwnd == wid)
        {
            self.pending_workspace_switch_focus = None;
        }
        self.take_managed_lifetime_token(wid);
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
        self.finish_stale_window_prune(stale, None)
    }

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
        let (row, col, _) = ws.find_window_location_rc(window_id)?;
        ws.rows().get(row)?.column(col).map(|c| c.width())
    }

    /// The (row, column) index `window_id` occupies on `(monitor, ws_idx)` plus one
    /// same-column sibling (any other window sharing it), if it is tiled there.
    /// The sibling anchors the column so a later restore survives index shifts
    /// from columns added or removed in the meantime.
    pub(crate) fn tiled_column_origin(
        &self,
        monitor: MonitorId,
        ws_idx: usize,
        window_id: u64,
    ) -> Option<(usize, usize, Option<u64>)> {
        let ws = self.workspaces.get(&monitor)?.get(ws_idx)?;
        let (row, col, _) = ws.find_window_location_rc(window_id)?;
        let sibling = ws
            .rows()
            .get(row)?
            .column(col)?
            .windows()
            .iter()
            .copied()
            .find(|&w| w != window_id);
        Some((row, col, sibling))
    }

    pub(crate) fn get_focused_column_rect(&self) -> Option<Rect> {
        let workspace = self.focused_workspace()?;
        self.monitors.get(&self.focused_monitor)?;
        let placements = workspace.compute_placements(self.layout_viewport(self.focused_monitor));

        // Resize targets a focused floating window, so the hint must follow it.
        let focused_hwnd = self
            .focused_floating_window()
            .or_else(|| workspace.focused_window())?;
        placements
            .iter()
            .find(|p| p.window_id == focused_hwnd)
            .map(|p| p.rect)
    }

    /// Removing WS_MAXIMIZEBOX suppresses Snap Layouts. Skip when suppression is
    /// disabled in config, tiling is paused, or this HWND is already tracked.
    pub(crate) fn disable_snap_for_window(&mut self, hwnd: u64) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        if self.paused {
            return;
        }
        if self.snap_disabled_hwnds.contains(&hwnd) {
            return;
        }
        #[cfg(test)]
        {
            if let Some(result) = self.injected_snap_disable_override.clone() {
                self.injected_snap_disable_attempt_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match result {
                    Ok(true) => {
                        self.snap_disabled_hwnds.insert(hwnd);
                        debug!("Removed WS_MAXIMIZEBOX from window {}", hwnd);
                    }
                    Ok(false) => {
                        debug!("Window {} already lacks WS_MAXIMIZEBOX, skipping", hwnd);
                    }
                    Err(error) => {
                        warn!(
                            "Failed to remove WS_MAXIMIZEBOX for window {}: {}",
                            hwnd, error
                        );
                    }
                }
                return;
            }
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

    pub(crate) fn restore_snap_for_all_windows(&mut self) {
        let hwnds: Vec<u64> = self.snap_disabled_hwnds.drain().collect();
        if !hwnds.is_empty() {
            leopardwm_platform_win32::restore_maximizebox_all(&hwnds);
            info!("Restored WS_MAXIMIZEBOX for {} window(s)", hwnds.len());
        }
    }

    pub(crate) fn disable_snap_for_all_tiled_windows(&mut self) {
        if !self.config.behavior.disable_snap_layouts {
            return;
        }
        let mut tiled_ids = Vec::new();
        for ws_vec in self.workspaces.values() {
            for workspace in ws_vec {
                for row in workspace.rows() {
                    for col in row.columns() {
                        for &wid in col.windows() {
                            tiled_ids.push(wid);
                        }
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
        // Paused layout application is a no-op and animation frames stop.
        // Abort first so ghost sources cannot remain cloaked indefinitely.
        self.abort_active_ghost_transition();
        let was_paused = self.paused;
        self.paused = !was_paused;
        info!(
            "Tiling {} via {}",
            if self.paused { "paused" } else { "resumed" },
            source
        );
        if self.paused {
            self.restore_taskbar_buttons();
            self.restore_snap_for_all_windows();
            self.hide_border();
            self.hide_tab_strip();
            self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        } else {
            self.resume_deferred_apply_worker_recovery();
            self.display_change_apply_retry = None;
            self.pending_layout_apply_timeout_report = None;
            if let Err(error) = self.resume_layout_after_unpause() {
                self.paused = was_paused;
                warn!(
                    "Resume apply failed via {}; restoring paused state: {}",
                    source, error
                );
                return Err(error);
            }
            // Park inactive workspaces only after a successful active apply so a
            // failed resume cannot move them while rolling back to paused.
            self.prepare_inactive_workspace_windows();
            self.disable_snap_for_all_tiled_windows();
            self.sync_taskbar_buttons();
            self.sync_foreground_window();
        }
        Ok(())
    }

    fn resume_layout_after_unpause(&mut self) -> Result<()> {
        const RESUME_DEFERRED: &str =
            "Resume apply deferred while recovery animation placement finishes";
        if self.pending_idle_layout_reapply && self.is_animating() {
            match self.try_consume_idle_layout_reapply() {
                crate::temporary_ignore::IdleLayoutReapply::Applied => return Ok(()),
                crate::temporary_ignore::IdleLayoutReapply::Failed { message } => {
                    return Err(anyhow::anyhow!(message));
                }
                crate::temporary_ignore::IdleLayoutReapply::Waiting => {
                    return Err(anyhow::anyhow!(RESUME_DEFERRED));
                }
                crate::temporary_ignore::IdleLayoutReapply::NotPending
                | crate::temporary_ignore::IdleLayoutReapply::Paused => {}
            }
        }
        match self.apply_layout() {
            Ok(crate::layout_apply::LayoutApplyOutcome::Completed) => Ok(()),
            Ok(crate::layout_apply::LayoutApplyOutcome::DeferredByRecoveryBarrier) => {
                Err(anyhow::anyhow!(RESUME_DEFERRED))
            }
            Err(error) => Err(error),
        }
    }
}
