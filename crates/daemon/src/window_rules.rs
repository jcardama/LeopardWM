use crate::config;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::Rect;
use leopardwm_ipc::log_text;
#[cfg(not(test))]
use leopardwm_platform_win32::enumerate_windows;
use leopardwm_platform_win32::{
    find_monitor_for_rect, get_process_executable, scale_px, MonitorId, WindowInfo,
};
use tracing::{debug, info, warn};

impl AppState {
    /// Apply changed effective actions, preserving manual overrides when they are unchanged.
    pub(crate) fn reapply_window_rules(
        &mut self,
        previous_rules: &[config::CompiledWindowRule],
        previous_policy: config::FixedSizeWindows,
    ) {
        let mut transitions: Vec<(u64, MonitorId, usize, config::WindowAction, bool)> = Vec::new();

        for (&monitor_id, ws_vec) in &self.workspaces {
            for (ws_idx, workspace) in ws_vec.iter().enumerate() {
                for wid in workspace.all_window_ids() {
                    let is_floating = workspace.is_floating(wid);
                    if let Some(win_info) = self.lookup_window_info(wid) {
                        let executable =
                            get_process_executable(win_info.process_id).unwrap_or_default();
                        let matched = self.matched_rule(
                            &win_info.class_name,
                            &win_info.title,
                            &executable,
                            None,
                        );
                        let previous_matched = previous_rules.iter().find(|rule| {
                            rule.matches(&win_info.class_name, &win_info.title, &executable, None)
                        });
                        let action = self.effective_admission_action(
                            &win_info,
                            self.evaluate_window_rules(
                                &win_info.class_name,
                                &win_info.title,
                                &executable,
                                None,
                            ),
                            matched.is_some(),
                            self.config.behavior.fixed_size_windows,
                        );
                        let previous_action = self.effective_admission_action(
                            &win_info,
                            previous_matched.map_or(config::WindowAction::Tile, |rule| rule.action),
                            previous_matched.is_some(),
                            previous_policy,
                        );
                        if action != previous_action {
                            transitions.push((wid, monitor_id, ws_idx, action, is_floating));
                        }
                    }
                }
            }
        }

        let float_rects: std::collections::HashMap<u64, Rect> = transitions
            .iter()
            .filter(|(_, _, _, action, is_floating)| {
                *action == config::WindowAction::Float && !is_floating
            })
            .filter_map(|(wid, monitor_id, _, _, _)| {
                let win_info = self.lookup_window_info(*wid)?;
                let executable = get_process_executable(win_info.process_id).unwrap_or_default();
                let rect = self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                    Some(*monitor_id),
                    None,
                );
                Some((*wid, rect))
            })
            .collect();

        for (wid, monitor_id, ws_idx, action, is_floating) in transitions {
            let viewport_width = self.viewport_width_for(monitor_id);
            match action {
                config::WindowAction::Float if !is_floating => {
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
                    if let Some(workspace) = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        if workspace.remove_window(wid).is_ok() {
                            workspace.reconcile_scroll_bounds(viewport_width);
                        }
                        let _ = workspace.add_floating(wid, rect);
                        info!("Rule change: moved window {} to floating", wid);
                    }
                }
                config::WindowAction::Tile if is_floating => {
                    if let Some(workspace) = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        workspace.unfloat_window(wid);
                        self.disable_snap_for_window(wid);
                        info!("Rule change: moved window {} to tiled", wid);
                    }
                }
                config::WindowAction::Ignore => {
                    self.restore_snap_for_window(wid);
                    if let Some(workspace) = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        if is_floating {
                            workspace.remove_floating(wid);
                        } else if workspace.remove_window(wid).is_ok() {
                            workspace.reconcile_scroll_bounds(viewport_width);
                        }
                        self.window_managed_at.remove(&wid);
                        self.window_last_maximized_at.remove(&wid);
                        self.take_managed_lifetime_token(wid);
                        info!("Rule change: unmanaged window {} (ignore)", wid);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn preferred_new_window_monitor(&self, sticky: bool) -> Option<MonitorId> {
        if self.config.behavior.new_window_monitor == config::NewWindowMonitor::Focused
            && !sticky
            && self.monitors.contains_key(&self.focused_monitor)
        {
            Some(self.focused_monitor)
        } else {
            None
        }
    }

    pub(crate) fn floating_rect_for_admission(
        &self,
        window: &WindowInfo,
        executable: &str,
        monitor_id: MonitorId,
        use_preferred_monitor: bool,
    ) -> Rect {
        let mut rect = self.get_floating_rect_from_rules(
            &window.class_name,
            &window.title,
            executable,
            &window.rect,
            Some(monitor_id),
            Some(self.window_rule_size(&window.rect)),
        );
        let positioned_by_rule = self
            .matched_rule(
                &window.class_name,
                &window.title,
                executable,
                Some(self.window_rule_size(&window.rect)),
            )
            .is_some_and(|rule| {
                rule.action == config::WindowAction::Float && (rule.x.is_some() || rule.y.is_some())
            });
        if use_preferred_monitor && !positioned_by_rule {
            let monitor = &self.monitors[&monitor_id];
            if !monitor.contains_rect_center(&rect) {
                let area = monitor.work_area;
                rect.width = rect.width.min(area.width.max(1));
                rect.height = rect.height.min(area.height.max(1));
                rect.x = area.x + (area.width - rect.width) / 2;
                rect.y = area.y + (area.height - rect.height) / 2;
            }
        }
        rect
    }

    pub(crate) fn enumerate_and_add_windows(&mut self) -> Result<usize> {
        let windows = self.windows_for_enumeration()?;
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        let mut added = 0;
        // Reconcile the previous replaced HWND before this iteration departs,
        // including when that window was skipped by the ignore gate or a rule.
        let mut pending_replaced: Option<u64> = None;

        for win_info in windows {
            if let Some(hwnd) = pending_replaced.take() {
                self.reconcile_replaced_lifetime_admission(hwnd);
            }
            // Recycle departs before the ignore gate and rules, matching
            // admission. The replacement is then evaluated like any new window.
            if self.depart_replaced_managed_lifetime(win_info.hwnd) {
                pending_replaced = Some(win_info.hwnd);
            }

            if matches!(
                self.temporary_ignore_gate(win_info.hwnd),
                crate::temporary_ignore::IgnoreGate::Block
            ) {
                continue;
            }
            let executable = get_process_executable(win_info.process_id).unwrap_or_default();

            self.size_ignored_windows.remove(&win_info.hwnd);
            // Restored and previously managed windows are not new admissions.
            if self.find_window_workspace(win_info.hwnd).is_some() {
                self.record_managed_lifetime_if_unrecorded(win_info.hwnd);
                self.record_managed_window_identity(&win_info);
                continue;
            }
            let matched = self.admission_rule(&win_info, &executable);
            let action = matched
                .map(|rule| rule.action)
                .unwrap_or(config::WindowAction::Tile);
            let rule_matched = matched.is_some();

            if action == config::WindowAction::Ignore {
                debug!(
                    "Ignoring window by rule: {} ({})",
                    log_text::title(&win_info.title),
                    log_text::name(&win_info.class_name)
                );
                continue;
            }

            let monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
                .map(|m| m.id)
                .unwrap_or(self.focused_monitor);

            // Elevated window the non-elevated daemon can't reposition (UIPI):
            // skip + notify instead of reserving a column we can't fill. Mirrors
            // the live-create path; covers windows already open at startup and
            // any seen via `lwm refresh`.
            if self.elevation_blocks_admission(
                win_info.hwnd,
                win_info.process_id,
                &win_info.title,
                &win_info.class_name,
            ) {
                continue;
            }

            if let Some(kind) = self.unmanaged_helper_kind(&win_info, rule_matched) {
                debug!(
                    "Leaving {} window unmanaged: {} ({})",
                    kind,
                    log_text::title(&win_info.title),
                    log_text::name(&win_info.class_name)
                );
                continue;
            }

            let action = self.effective_admission_action(
                &win_info,
                action,
                rule_matched,
                self.config.behavior.fixed_size_windows,
            );
            let floating_rect = (action == config::WindowAction::Float).then(|| {
                self.get_floating_rect_from_rules(
                    &win_info.class_name,
                    &win_info.title,
                    &executable,
                    &win_info.rect,
                    Some(monitor_id),
                    Some(self.window_rule_size(&win_info.rect)),
                )
            });

            let target_idx = self.active_workspace_idx(monitor_id);
            let _ = self.ensure_workspace_exists(monitor_id, target_idx);
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(target_idx))
            {
                match action {
                    config::WindowAction::Float => {
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
                                self.window_managed_at
                                    .insert(win_info.hwnd, std::time::Instant::now());
                                self.record_managed_lifetime(win_info.hwnd, None);
                                self.record_managed_window_identity(&win_info);
                                info!(
                                    "Added floating window: {} ({}) to monitor {} - {}x{}",
                                    log_text::title(&win_info.title),
                                    log_text::name(&win_info.class_name),
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
                                self.window_managed_at
                                    .insert(win_info.hwnd, std::time::Instant::now());
                                self.record_managed_lifetime(win_info.hwnd, None);
                                self.record_managed_window_identity(&win_info);
                                self.disable_snap_for_window(win_info.hwnd);
                                info!(
                                    "Added tiled window: {} ({}) to monitor {} - {}x{}",
                                    log_text::title(&win_info.title),
                                    log_text::name(&win_info.class_name),
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
                    config::WindowAction::Ignore => unreachable!(),
                }
            }
        }

        if let Some(hwnd) = pending_replaced {
            self.reconcile_replaced_lifetime_admission(hwnd);
        }

        Ok(added)
    }

    /// Leave dialog-like windows, or untitled frameless fixed-size windows not
    /// covering a monitor when topology is known, unmanaged. A matching user
    /// rule overrides this.
    pub(crate) fn unmanaged_helper_kind(
        &self,
        window: &WindowInfo,
        rule_matched: bool,
    ) -> Option<&'static str> {
        if rule_matched {
            return None;
        }
        if leopardwm_platform_win32::is_dialog_like_window(window.hwnd) {
            return Some("dialog-like");
        }
        if window.title.trim().is_empty()
            && !self.monitors.is_empty()
            && leopardwm_platform_win32::is_frameless_fixed_size_window(window.hwnd)
            && !self
                .monitors
                .values()
                .any(|monitor| crate::event_handler::rect_covers_monitor(window.rect, monitor))
        {
            return Some("frameless fixed-size");
        }
        None
    }

    pub(crate) fn effective_admission_action(
        &self,
        window: &WindowInfo,
        action: config::WindowAction,
        rule_matched: bool,
        policy: config::FixedSizeWindows,
    ) -> config::WindowAction {
        if !rule_matched
            && policy == config::FixedSizeWindows::Float
            && leopardwm_platform_win32::is_fixed_caption_window(window.hwnd)
        {
            debug!(
                "Floating fixed-size caption window: {} ({})",
                log_text::title(&window.title),
                log_text::name(&window.class_name)
            );
            config::WindowAction::Float
        } else {
            action
        }
    }

    fn windows_for_enumeration(&self) -> Result<Vec<WindowInfo>> {
        #[cfg(test)]
        {
            Ok(self.injected_enumerated_windows.clone().unwrap_or_default())
        }
        #[cfg(not(test))]
        Ok(enumerate_windows()?)
    }

    /// Match once at admission and remember size-ignored windows for size/title retries.
    pub(crate) fn admission_rule(
        &mut self,
        window: &WindowInfo,
        executable: &str,
    ) -> Option<&config::CompiledWindowRule> {
        self.size_ignored_windows.remove(&window.hwnd);
        let size = Some(self.window_rule_size(&window.rect));
        let matched = self
            .compiled_rules
            .iter()
            .find(|rule| rule.matches(&window.class_name, &window.title, executable, size));
        if matched.is_some_and(|rule| {
            rule.action == config::WindowAction::Ignore && rule.has_size_condition()
        }) {
            self.size_ignored_windows.insert(window.hwnd);
        }
        matched
    }

    pub(crate) fn window_rule_size(&self, rect: &Rect) -> (i32, i32) {
        let scale = self
            .monitors
            .values()
            .find(|monitor| monitor.contains_rect_center(rect))
            .map(|monitor| monitor.scale_factor)
            .unwrap_or(1.0);
        (
            scale_px(rect.width, 1.0 / scale),
            scale_px(rect.height, 1.0 / scale),
        )
    }

    pub(crate) fn evaluate_window_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        size: Option<(i32, i32)>,
    ) -> config::WindowAction {
        self.matched_rule(class_name, title, executable, size)
            .map(|r| r.action)
            .unwrap_or(config::WindowAction::Tile)
    }

    pub(crate) fn matched_rule(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        size: Option<(i32, i32)>,
    ) -> Option<&config::CompiledWindowRule> {
        self.compiled_rules
            .iter()
            .find(|rule| rule.matches(class_name, title, executable, size))
    }

    /// Get the floating rect for a window based on rules.
    ///
    /// Rule-defined dimensions and work-area offsets are logical pixels scaled
    /// by the opening monitor's DPI factor. Without a monitor, dimensions use
    /// scale 1 and offsets cannot be applied.
    pub(crate) fn get_floating_rect_from_rules(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        original_rect: &leopardwm_core_layout::Rect,
        monitor_id: Option<MonitorId>,
        size: Option<(i32, i32)>,
    ) -> leopardwm_core_layout::Rect {
        let monitor = monitor_id.and_then(|id| self.monitors.get(&id));
        let scale = monitor.map(|m| m.scale_factor).unwrap_or(1.0);
        for rule in &self.compiled_rules {
            if rule.matches(class_name, title, executable, size) {
                // Only scale rule-provided dimensions (config logical pixels).
                // If a dimension is not specified, use the original rect value
                // which is already in physical pixels from the OS.
                let width = rule
                    .width
                    .map(|w| scale_px(w, scale))
                    .unwrap_or(original_rect.width);
                let height = rule
                    .height
                    .map(|h| scale_px(h, scale))
                    .unwrap_or(original_rect.height);
                let mut rect = Rect::new(original_rect.x, original_rect.y, width, height);
                if rule.action == config::WindowAction::Float
                    && (rule.x.is_some() || rule.y.is_some())
                {
                    if let Some(monitor) = monitor {
                        let area = monitor.work_area;
                        if let Some(x) = rule.x {
                            rect.x = area.x.saturating_add(scale_px(x, scale));
                        }
                        if let Some(y) = rule.y {
                            rect.y = area.y.saturating_add(scale_px(y, scale));
                        }
                        rect.x = rect.x.clamp(
                            area.x,
                            area.x
                                .saturating_add(area.width.saturating_sub(width).max(0)),
                        );
                        rect.y = rect.y.clamp(
                            area.y,
                            area.y
                                .saturating_add(area.height.saturating_sub(height).max(0)),
                        );
                    }
                }
                return rect;
            }
        }
        *original_rect
    }
}
