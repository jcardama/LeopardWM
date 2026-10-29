use crate::state::*;
use leopardwm_platform_win32::get_process_executable;
use tracing::{debug, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DepartingFocusDecision {
    pub recover: bool,
    pub suppress_landing_resync: bool,
    pub replacement_hwnd: Option<u64>,
}

/// Why a window is leaving management. A replaced lifetime is not an OS
/// Destroyed or Hidden: the HWND in the foreground is already the new window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DepartureCause {
    Event,
    ReplacedLifetime,
}

pub(crate) fn departing_focus_decision(
    was_tracked_focus: bool,
    departing_hwnd: u64,
    foreground: Option<u64>,
    foreground_is_valid: bool,
) -> DepartingFocusDecision {
    let replacement_hwnd = match foreground {
        Some(id) if id != 0 && id != departing_hwnd && foreground_is_valid => Some(id),
        _ => None,
    };
    DepartingFocusDecision {
        recover: was_tracked_focus && replacement_hwnd.is_none(),
        suppress_landing_resync: replacement_hwnd.is_some(),
        replacement_hwnd,
    }
}

pub(crate) fn should_sync_foreground_on_animation_landing(
    paused: bool,
    suppress_landing_focus_resync: bool,
) -> bool {
    !paused && !suppress_landing_focus_resync
}

fn tab_icon(
    cache: &mut std::collections::HashMap<u64, Option<isize>>,
    hwnd: u64,
    refresh: bool,
    query: impl FnOnce(u64) -> Option<isize>,
) -> Option<isize> {
    if !refresh {
        if let Some(&icon) = cache.get(&hwnd) {
            return icon;
        }
    }
    let icon = query(hwnd);
    cache.insert(hwnd, icon);
    icon
}

impl AppState {
    /// Read high contrast live so toggling it overrides the configured border
    /// color immediately, without waiting for a config reload.
    pub(crate) fn border_color_bgr(&self) -> Option<u32> {
        if leopardwm_platform_win32::is_high_contrast_enabled() {
            return Some(leopardwm_platform_win32::get_system_highlight_color_bgr());
        }
        hex_rgb_to_bgr(&self.config.appearance.active_border_color)
    }

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

    pub(crate) fn border_position(&self) -> leopardwm_platform_win32::border::BorderPosition {
        if self.config.appearance.active_border_position == "inside" {
            leopardwm_platform_win32::border::BorderPosition::Inside
        } else {
            leopardwm_platform_win32::border::BorderPosition::Outside
        }
    }

    fn window_scale_factor(&self, hwnd: u64) -> f64 {
        self.find_window_workspace(hwnd)
            .and_then(|(mid, _)| self.monitors.get(&mid))
            .map(|m| m.scale_factor)
            .unwrap_or(1.0)
    }

    pub(crate) fn scaled_border_width(&self, hwnd: u64) -> u32 {
        (self.config.appearance.active_border_width as f64 * self.window_scale_factor(hwnd)).round()
            as u32
    }

    /// During a tiled drag, anchor the border to the layout slot rather than
    /// following the OS-dragged window.
    pub(crate) fn show_border(&self, hwnd: u64) {
        // Explicit workspace switches defer the border until the completion tick
        // clears the transition, so it does not paint while sliding onto the monitor.
        if self
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.defer_focus_border)
        {
            return;
        }
        #[cfg(test)]
        {
            self.border_show_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.last_border_show_hwnd
                .store(hwnd, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(ref frame) = self.border_frame {
            if self.paused
                || self.is_application_fullscreen(hwnd)
                || self
                    .focused_workspace()
                    .is_some_and(|ws| ws.is_fullscreen())
            {
                frame.hide();
                return;
            }
            if self.find_window_workspace(hwnd).is_none() {
                frame.hide();
                return;
            }
            let border_width = self.scaled_border_width(hwnd);
            let corner_radius = self.corner_radius_for_window(hwnd);
            if let Some(rect) = self.resize_preview_display_rect {
                if self.config.appearance.active_border {
                    if let Some(bgr) = self.border_color_bgr() {
                        frame.show_at_rect(
                            rect,
                            border_width,
                            self.border_position(),
                            bgr,
                            corner_radius,
                            hwnd,
                        );
                        return;
                    }
                }
            }
            if let Some(ref drag) = self.drag_state {
                if drag.is_tiled && self.config.appearance.active_border {
                    if let Some(bgr) = self.border_color_bgr() {
                        if let Some(layout_rect) = self.compute_window_layout_rect(hwnd) {
                            frame.show_at_rect(
                                layout_rect,
                                border_width,
                                self.border_position(),
                                bgr,
                                corner_radius,
                                hwnd,
                            );
                            return;
                        }
                    }
                    frame.hide();
                    return;
                }
            }
            // DWM bounds lag SetWindowPos by 1-2 frames after drag, resize, or scroll
            // landing; tiled borders use interpolated layout slots to avoid drift.
            // Floating windows have no layout slot: `frame.show(hwnd)` queries DWM
            // extended frame bounds, falling back to GetWindowRect.
            if self.config.appearance.active_border {
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    let is_floating = self
                        .workspaces
                        .get(&monitor_id)
                        .and_then(|v| v.get(ws_idx))
                        .is_some_and(|ws| ws.is_floating(hwnd));
                    if !is_floating {
                        match self.compute_window_layout_rect(hwnd) {
                            Some(layout_rect) => {
                                if self.is_physically_parked(hwnd) {
                                    frame.hide();
                                    return;
                                }
                                if let Some(bgr) = self.border_color_bgr() {
                                    if let Some(overlay) = self.compute_tiled_border_overlay_rect(
                                        hwnd,
                                        layout_rect,
                                        border_width,
                                    ) {
                                        frame.show_final_overlay(
                                            overlay,
                                            border_width,
                                            self.border_position(),
                                            bgr,
                                            corner_radius,
                                            hwnd,
                                        );
                                    } else {
                                        frame.hide();
                                    }
                                    return;
                                }
                            }
                            None => {
                                // With no tiled placement, DWM bounds may be stale; do not leave
                                // a border floating at the window's former position.
                                frame.hide();
                                return;
                            }
                        }
                    }
                }
            }
            if self.config.appearance.active_border {
                if let Some(bgr) = self.border_color_bgr() {
                    frame.show(
                        hwnd,
                        border_width,
                        self.border_position(),
                        bgr,
                        corner_radius,
                    );
                    return;
                }
            }
            frame.hide();
        }
    }

    pub(crate) fn compute_tiled_border_overlay_rect(
        &self,
        hwnd: u64,
        layout_rect: leopardwm_core_layout::Rect,
        border_width: u32,
    ) -> Option<leopardwm_core_layout::Rect> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        let workspace = self.workspaces.get(&monitor_id)?.get(ws_idx)?;
        let animating = workspace.is_animating()
            || self
                .layout_transition
                .as_ref()
                .and_then(|transition| transition.start_rects.get(&hwnd))
                .is_some_and(|start| *start != layout_rect);
        // Skip decoration clipping during motion to keep the bitmap full-sized.
        // Use the current interpolated rect, not the previous frame's presentation.
        let content = if animating {
            layout_rect
        } else {
            self.expected_physical_rect(hwnd).unwrap_or(layout_rect)
        };
        let overlay = crate::physical_placement::expand_border_overlay(
            content,
            border_width,
            matches!(
                self.border_position(),
                leopardwm_platform_win32::border::BorderPosition::Outside
            ),
        );
        if animating {
            Some(overlay)
        } else {
            self.project_decoration_rect(hwnd, overlay)
        }
    }

    pub(crate) fn corner_radius_for_window(&self, hwnd: u64) -> f32 {
        self.unscaled_corner_radius(hwnd) * self.window_scale_factor(hwnd) as f32
    }

    fn unscaled_corner_radius(&self, hwnd: u64) -> f32 {
        let info = leopardwm_platform_win32::get_window_info(hwnd);

        if let Some(ref info) = info {
            let any_corner_overrides = self.compiled_rules.iter().any(|r| r.corner_style.is_some());
            if any_corner_overrides {
                let exe = get_process_executable(info.process_id).unwrap_or_default();
                for rule in &self.compiled_rules {
                    if rule.matches(&info.class_name, &info.title, &exe, None) {
                        if let Some(style) = rule.corner_style {
                            return style.radius_px();
                        }
                    }
                }
            }
        }

        // DWM DEFAULT is not an explicit corner preference.
        leopardwm_platform_win32::get_window_corner_radius(hwnd)
            .unwrap_or(leopardwm_platform_win32::border::DEFAULT_CORNER_RADIUS)
    }

    /// Interpolate the border with the windows, or it leads them by the full
    /// transition duration. Explicit workspace switches defer it until landing.
    pub(crate) fn compute_window_layout_rect(
        &self,
        hwnd: u64,
    ) -> Option<leopardwm_core_layout::Rect> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        self.monitors.get(&monitor_id)?;
        let viewport = self.layout_viewport(monitor_id);
        let workspace = self.workspaces.get(&monitor_id)?.get(ws_idx)?;
        let mut placements = workspace.compute_placements_animated(viewport);
        if let Some(ref transition) = self.layout_transition {
            Self::apply_transition_interpolation(transition, &mut placements);
        }
        placements
            .iter()
            .find(|p| p.window_id == hwnd)
            .map(|p| p.rect)
    }

    pub(crate) fn hide_border(&self) {
        #[cfg(test)]
        self.border_hide_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(ref frame) = self.border_frame {
            frame.hide();
        }
    }

    pub(crate) fn reconcile_border_without_stealing_focus(&self) {
        self.reconcile_border_without_stealing_focus_for(None);
    }

    pub(crate) fn reconcile_border_without_stealing_focus_for(&self, replacement: Option<u64>) {
        if let Some(hwnd) = replacement {
            if self.find_window_workspace(hwnd).is_some() {
                self.show_border(hwnd);
                return;
            }
            self.hide_border();
            return;
        }
        if let Some(hwnd) = self.previous_focused_hwnd {
            if self.find_window_workspace(hwnd).is_some() {
                self.show_border(hwnd);
                return;
            }
        }
        self.hide_border();
    }

    pub(crate) fn hide_tab_strip(&self) {
        #[cfg(test)]
        self.tab_strip_hide_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for strip in self.tab_strip_overlays.values() {
            strip.hide();
        }
    }

    /// Strips persist across focus changes; changing focus only changes highlights.
    pub(crate) fn update_tab_strip(&mut self) {
        self.update_tab_strip_icons(true);
    }

    pub(crate) fn update_tab_strip_for_animation(&mut self) {
        self.update_tab_strip_icons(false);
    }

    fn update_tab_strip_icons(&mut self, refresh_icons: bool) {
        use leopardwm_platform_win32::tab_strip::{TabLabel, TabStripColors};

        #[cfg(test)]
        self.tab_strip_update_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let managed: std::collections::HashSet<_> =
            self.all_managed_window_ids().into_iter().collect();
        self.tab_icon_cache.retain(|hwnd, _| managed.contains(hwnd));
        if self.tab_strip_action_tx.is_none() {
            return;
        }
        if self.paused {
            self.tab_strip_overlays.clear();
            return;
        }

        let mut icon_cache = std::mem::take(&mut self.tab_icon_cache);

        // GDI COLORREF uses 0xBBGGRR, while config hex strings are RGB.
        let app = &self.config.appearance;
        let colors = TabStripColors {
            bg: hex_rgb_to_bgr(&app.tab_strip_bg).unwrap_or(0x1F1F1F),
            active_bg: hex_rgb_to_bgr(&app.tab_strip_active_bg).unwrap_or(0x303030),
            active_text: hex_rgb_to_bgr(&app.tab_strip_active_text).unwrap_or(0xFFFFFF),
            inactive_text: hex_rgb_to_bgr(&app.tab_strip_inactive_text).unwrap_or(0xA0A0A0),
            opacity: app.tab_strip_opacity,
        };
        let close_action = match self.config.behavior.tab_close_action {
            crate::config::TabCloseAction::CloseWindow => {
                leopardwm_platform_win32::TabCloseAction::CloseWindow
            }
            crate::config::TabCloseAction::Untab => leopardwm_platform_win32::TabCloseAction::Untab,
        };

        struct StripShow {
            rect: leopardwm_core_layout::Rect,
            tabs: Vec<TabLabel>,
            active_idx: usize,
            scale: f64,
        }
        let mut desired: std::collections::HashMap<(isize, usize, usize, usize), StripShow> =
            std::collections::HashMap::new();

        let monitor_ids: Vec<isize> = self.workspaces.keys().copied().collect();
        for monitor in monitor_ids {
            let ws_idx = self.active_workspace_idx(monitor);
            let Some(ws_vec) = self.workspaces.get(&monitor) else {
                continue;
            };
            let Some(ws) = ws_vec.get(ws_idx) else {
                continue;
            };
            if ws.is_fullscreen() {
                continue;
            }
            let scale = self
                .monitors
                .get(&monitor)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            for (row_idx, row) in ws.rows().iter().enumerate() {
                for (col_idx, col) in row.columns().iter().enumerate() {
                    if !col.is_tabbed() {
                        continue;
                    }
                    let Some(visible_tab) = col.effective_visible_tab(|w| ws.is_minimized(w))
                    else {
                        continue;
                    };
                    let Some(visible_hwnd) = col.get(visible_tab) else {
                        continue;
                    };
                    if self.is_application_fullscreen(visible_hwnd) {
                        continue;
                    }
                    let Some(content_rect) = self.compute_window_layout_rect(visible_hwnd) else {
                        continue;
                    };
                    if self.is_physically_parked(visible_hwnd) {
                        continue;
                    }
                    let content_rect = self
                        .expected_physical_rect(visible_hwnd)
                        .unwrap_or(content_rect);
                    let strip_height =
                        (self.config.appearance.tab_strip_height as f64 * scale).round() as i32;
                    let bottom_gap = (self.config.layout.gap.max(0) as f64 * scale).round() as i32;
                    let strip_rect = crate::physical_placement::tab_strip_rect(
                        content_rect,
                        strip_height,
                        bottom_gap,
                    );
                    let Some(rect) = self.project_decoration_rect(visible_hwnd, strip_rect) else {
                        continue;
                    };
                    let stored_active = col.active_tab_idx().unwrap_or(visible_tab);
                    let active_idx =
                        if col.get(stored_active).is_some_and(|w| !ws.is_minimized(w)) {
                            stored_active
                        } else {
                            visible_tab
                        };
                    let tabs: Vec<TabLabel> = col
                        .windows()
                        .iter()
                        .filter(|&&w| w != crate::state::DRAG_PLACEHOLDER_HWND)
                        .map(|&w| TabLabel {
                            title: self
                                .tab_title_overrides
                                .get(&w)
                                .cloned()
                                .or_else(|| self.lookup_window_info(w).map(|info| info.title))
                                .unwrap_or_default(),
                            icon: tab_icon(
                                &mut icon_cache,
                                w,
                                refresh_icons,
                                leopardwm_platform_win32::get_window_icon,
                            ),
                        })
                        .collect();
                    desired.insert(
                        (monitor, ws_idx, row_idx, col_idx),
                        StripShow {
                            rect,
                            tabs,
                            active_idx,
                            scale,
                        },
                    );
                }
            }
        }

        self.tab_icon_cache = icon_cache;

        // Dropping an undesired overlay tears down its thread, HWNDs, and tooltip.
        self.tab_strip_overlays
            .retain(|key, _| desired.contains_key(key));

        for (key, show) in desired {
            let (monitor, ws_idx, row_idx, col_idx) = key;
            if !self.tab_strip_overlays.contains_key(&key) {
                let Some(tx) = self.tab_strip_action_tx.clone() else {
                    continue;
                };
                match leopardwm_platform_win32::tab_strip::TabStripOverlay::new(tx) {
                    Ok(overlay) => {
                        self.tab_strip_overlays.insert(key, overlay);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create TabStripOverlay for {:?}: {}", key, e);
                        continue;
                    }
                }
            }
            let Some(strip) = self.tab_strip_overlays.get(&key) else {
                continue;
            };
            strip.show_overlay_rect(
                show.rect,
                show.tabs,
                show.active_idx,
                colors,
                monitor,
                ws_idx,
                row_idx,
                col_idx,
                show.scale,
                close_action,
            );
        }
    }

    /// The floating window that holds focus on the active workspace, if any.
    pub(crate) fn focused_floating_window(&self) -> Option<u64> {
        self.previous_focused_hwnd.filter(|&hwnd| {
            self.focused_workspace()
                .is_some_and(|ws| ws.is_floating(hwnd))
        })
    }

    /// Preserve floating focus on the active workspace instead of replacing it
    /// with the workspace's tiled focus.
    pub(crate) fn sync_foreground_window(&mut self) {
        let focused_hwnd = self.focused_floating_window().or_else(|| {
            self.focused_workspace()
                .and_then(|ws| ws.focused_visible_window())
        });

        if let Some(hwnd) = focused_hwnd {
            self.show_border(hwnd);

            // Skip SetForegroundWindow if the target is currently cloaked
            // (off-screen-parked or ghost-animating). Calling
            // SetForegroundWindow on a cloaked HWND is undocumented
            // behavior, so avoid it entirely. Internal focus
            // state still updates so Alt+Tab / next-Focused-event correctly
            // resync once the cloak lifts.
            let target_cloaked = leopardwm_platform_win32::is_placement_cloaked(hwnd);

            // Set foreground window — track it regardless of OS result since
            // this is our intended focus. The call can fail if the window
            // vanished between layout and here, which is a transient condition.
            // Skipped under #[cfg(test)] so placeholder hwnds (100, 200, …)
            // can't collide with a real running HWND and lag the user's mouse
            // via AttachThreadInput.
            #[cfg(not(test))]
            if !target_cloaked {
                let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
            }
            #[cfg(test)]
            let _ = target_cloaked; // silence unused warning under cfg(test)
            self.previous_focused_hwnd = Some(hwnd);
            self.update_tab_strip();
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, Some(hwnd));
        } else {
            self.release_parked_foreground_for_empty_selection();
            // No focused window on the active workspace — clear stale state
            // so border/focus don't target a window that's no longer here.
            self.previous_focused_hwnd = None;
            self.hide_border();
            self.update_tab_strip();
            debug!("sync_foreground_window: no focused visible window");
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
        }
    }

    #[cfg(test)]
    pub(crate) fn sync_foreground_after_animation_landing(&mut self) {
        self.sync_foreground_after_animation_landing_with_suppression(
            self.pending_suppress_landing_focus_resync,
        );
    }

    /// Keep captured landing-focus suppression only while recovery is still
    /// pending. Successful recovery clears that flag before this runs; a paused
    /// no-op apply does not. Non-recovery suppression still consumes even if
    /// this landing's apply failed, so a later ordinary landing can resync.
    pub(crate) fn finish_animation_landing_focus_resync(&mut self, captured_suppress: bool) {
        if captured_suppress && self.pending_idle_layout_reapply {
            return;
        }
        self.sync_foreground_after_animation_landing_with_suppression(captured_suppress);
    }

    pub(crate) fn sync_foreground_after_animation_landing_with_suppression(
        &mut self,
        suppress: bool,
    ) {
        self.pending_suppress_landing_focus_resync = false;
        if should_sync_foreground_on_animation_landing(self.paused, suppress) {
            self.sync_foreground_window();
        }
    }
}

/// GDI COLORREF stores RGB bytes in BGR order.
pub(crate) fn hex_rgb_to_bgr(hex: &str) -> Option<u32> {
    let stripped = hex.strip_prefix('#').unwrap_or(hex);
    if stripped.len() != 6 {
        return None;
    }
    let color = u32::from_str_radix(stripped, 16).ok()?;
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    Some((b << 16) | (g << 8) | r)
}

#[cfg(test)]
mod tab_icon_tests {
    use super::tab_icon;

    #[test]
    fn animation_icons_reuse_cached_results_and_refresh_updates_them() {
        let mut cache = std::collections::HashMap::new();
        let calls = std::cell::Cell::new(0);
        let query = |_| {
            calls.set(calls.get() + 1);
            Some(calls.get())
        };
        assert_eq!(tab_icon(&mut cache, 10, false, query), Some(1));
        assert_eq!(tab_icon(&mut cache, 10, false, query), Some(1));
        assert_eq!(calls.get(), 1);
        assert_eq!(tab_icon(&mut cache, 10, true, query), Some(2));
        assert_eq!(tab_icon(&mut cache, 10, false, query), Some(2));
        assert_eq!(tab_icon(&mut cache, 20, false, |_| None), None);
        assert_eq!(
            tab_icon(&mut cache, 20, false, |_| panic!(
                "cached missing icon was queried"
            )),
            None
        );
        assert_eq!(tab_icon(&mut cache, 10, true, |_| None), None);
        assert_eq!(
            tab_icon(&mut cache, 10, false, |_| panic!(
                "refreshed missing icon was queried"
            )),
            None
        );
    }
}
