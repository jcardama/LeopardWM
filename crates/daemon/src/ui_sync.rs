//! Border and tab-strip overlay synchronization with focus and layout state.

use crate::state::*;
use leopardwm_platform_win32::get_process_executable;
use tracing::{debug, info};

impl AppState {
    /// Convert the config border color (hex RGB string) to BGR u32 for Win32.
    /// When high contrast mode is active, returns the system highlight color instead.
    /// Checks the live system setting so toggling high contrast takes effect
    /// immediately without a config reload.
    pub(crate) fn border_color_bgr(&self) -> Option<u32> {
        if leopardwm_platform_win32::is_high_contrast_enabled() {
            return Some(leopardwm_platform_win32::get_system_highlight_color_bgr());
        }
        hex_rgb_to_bgr(&self.config.appearance.active_border_color)
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

    /// Compute the DPI-scaled border width for a window's monitor.
    pub(crate) fn scaled_border_width(&self, hwnd: u64) -> u32 {
        let scale = self.find_window_workspace(hwnd)
            .and_then(|(mid, _)| self.monitors.get(&mid))
            .map(|m| m.scale_factor)
            .unwrap_or(1.0);
        (self.config.appearance.active_border_width as f64 * scale).round() as u32
    }

    /// Show the border frame on the given window, or hide it if borders are disabled.
    /// During an active tiled drag, the border is hidden so it doesn't follow
    /// the OS-dragged window — the ghost overlay provides visual feedback instead.
    pub(crate) fn show_border(&self, hwnd: u64) {
        if let Some(ref frame) = self.border_frame {
            // No border while paused or in fullscreen.
            if self.paused || self.focused_workspace().is_some_and(|ws| ws.is_fullscreen()) {
                frame.hide();
                return;
            }
            // No border on unmanaged/ignored windows.
            if self.find_window_workspace(hwnd).is_none() {
                frame.hide();
                return;
            }
            let border_width = self.scaled_border_width(hwnd);
            let corner_radius = self.corner_radius_for_window(hwnd);
            // During resize preview: show border at the preview snap target.
            if let Some(rect) = self.resize_preview_display_rect {
                if self.config.appearance.active_border {
                    if let Some(bgr) = self.border_color_bgr() {
                        frame.show_at_rect(
                            rect,
                            border_width,
                            self.border_position(),
                            bgr,
                            corner_radius,
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
                                border_width,
                                self.border_position(),
                                bgr,
                                corner_radius,
                            );
                            return;
                        }
                    }
                    frame.hide();
                    return;
                }
            }
            // For managed tiled windows the layout rect is authoritative —
            // the window is either at that rect or animating toward it.
            // Querying DWM bounds via `frame.show(hwnd)` lags SetWindowPos by
            // 1-2 frames, which produces a visible border drift after drag
            // snap-back / resize / scroll landing (the SetWindowPos commits
            // but DWM still reports the old rect when the immediately-
            // following show_border fires). Use the layout rect directly so
            // the border is always at the slot the workspace says it should
            // be — same rect the layout engine is steering the window
            // toward, no DWM lag.
            //
            // Floating windows fall through to the chrome-tracking
            // `frame.show(hwnd)` path because their position is whatever
            // the user last set, not a layout slot.
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
                                if let Some(bgr) = self.border_color_bgr() {
                                    frame.show_at_rect(
                                        layout_rect,
                                        border_width,
                                        self.border_position(),
                                        bgr,
                                        corner_radius,
                                    );
                                    return;
                                }
                            }
                            None => {
                                // Managed tiled window with no current
                                // placement — minimized, or in a transient
                                // mid-removal state. Hide the border
                                // rather than falling through to
                                // `frame.show(hwnd)`, which would track the
                                // stale DWM bounds and leave a colored
                                // frame floating where the window used to
                                // be.
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

    fn corner_radius_for_window(&self, hwnd: u64) -> f32 {
        let info = leopardwm_platform_win32::get_window_info(hwnd);

        // 1. User-rule override always wins.
        if let Some(ref info) = info {
            let any_corner_overrides = self
                .compiled_rules
                .iter()
                .any(|r| r.corner_style.is_some());
            if any_corner_overrides {
                let exe = get_process_executable(info.process_id).unwrap_or_default();
                for rule in &self.compiled_rules {
                    if rule.matches(&info.class_name, &info.title, &exe) {
                        if let Some(style) = rule.corner_style {
                            return style.radius_px();
                        }
                    }
                }
            }
        }

        // 2. Trust only explicit non-DEFAULT DWM signals; otherwise default.
        leopardwm_platform_win32::get_window_corner_radius(hwnd)
            .unwrap_or(leopardwm_platform_win32::border::DEFAULT_CORNER_RADIUS)
    }

    /// Compute the layout rect for a window from the workspace placements.
    /// Applies the active layout transition's interpolation so the border
    /// follows the windows' interpolated mid-flight rect — without this the
    /// border jumps to the FINAL post-transition rect on frame 1 of a
    /// workspace switch / move-to-column / expel / drag merge while the
    /// windows are still sliding to it (border leads windows by the entire
    /// transition duration).
    fn compute_window_layout_rect(&self, hwnd: u64) -> Option<leopardwm_core_layout::Rect> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        let viewport = self.monitors.get(&monitor_id)?.work_area;
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

    /// Hide the border frame.
    pub(crate) fn hide_border(&self) {
        if let Some(ref frame) = self.border_frame {
            frame.hide();
        }
    }

    /// Hide every tab strip overlay if installed. Used by paths that
    /// know strips must not be visible (e.g., before re-applying layout
    /// during a configuration reload, prior to fullscreen entry).
    /// Doesn't drop the overlays — `update_tab_strip` will reuse them.
    pub(crate) fn hide_tab_strip(&self) {
        for strip in self.tab_strip_overlays.values() {
            strip.hide();
        }
    }

    /// Reconcile the set of tab strip overlays against the current
    /// "every tabbed column gets its own strip" model.
    ///
    /// For each visible workspace across all monitors:
    ///   - every column whose mode is Tabbed gets a strip
    ///   - strips for non-tabbed (or removed) columns are dropped
    ///   - fullscreen workspaces show no strips
    ///
    /// Strips persist across focus changes — refocusing a different
    /// column just changes which strip's highlight is "active", not
    /// which strips are visible.
    pub(crate) fn update_tab_strip(&mut self) {
        use leopardwm_platform_win32::tab_strip::{TabLabel, TabStripColors};

        // No-op when the action sender wasn't installed (tests / headless).
        if self.tab_strip_action_tx.is_none() {
            return;
        }
        if self.paused {
            // Tear everything down. The next non-paused update repopulates.
            self.tab_strip_overlays.clear();
            return;
        }

        // Pull configurable colors from the [appearance] config section.
        // Hex strings are RGB; convert to the 0xBBGGRR layout the strip
        // expects (GDI's COLORREF is BGR-byte-order in a u32).
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
            crate::config::TabCloseAction::Untab => {
                leopardwm_platform_win32::TabCloseAction::Untab
            }
        };

        // First pass: figure out the desired key set + per-key show args.
        struct StripShow {
            rect: leopardwm_core_layout::Rect,
            tabs: Vec<TabLabel>,
            active_idx: usize,
            scale: f64,
        }
        let mut desired: std::collections::HashMap<(isize, usize, usize), StripShow> =
            std::collections::HashMap::new();

        // Snapshot monitor + workspace identity into a vec so the
        // subsequent column walk can borrow `self` (immutable) again
        // without aliasing the iteration over `self.workspaces`.
        let monitor_ids: Vec<isize> = self.workspaces.keys().copied().collect();
        for monitor in monitor_ids {
            let ws_idx = self.active_workspace_idx(monitor);
            let Some(ws_vec) = self.workspaces.get(&monitor) else {
                continue;
            };
            let Some(ws) = ws_vec.get(ws_idx) else { continue };
            if ws.is_fullscreen() {
                continue;
            }
            let scale = self
                .monitors
                .get(&monitor)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            for col_idx in 0..ws.column_count() {
                let Some(col) = ws.column(col_idx) else { continue };
                if !col.is_tabbed() {
                    continue;
                }
                let Some(visible_tab) =
                    col.effective_visible_tab(|w| ws.is_minimized(w))
                else {
                    continue;
                };
                let Some(visible_hwnd) = col.get(visible_tab) else { continue };
                let Some(rect) = self.compute_window_layout_rect(visible_hwnd) else {
                    continue;
                };
                let stored_active = col.active_tab_idx().unwrap_or(visible_tab);
                let active_idx = if col
                    .get(stored_active)
                    .is_some_and(|w| !ws.is_minimized(w))
                {
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
                        icon: leopardwm_platform_win32::get_window_icon(w),
                    })
                    .collect();
                desired.insert(
                    (monitor, ws_idx, col_idx),
                    StripShow {
                        rect,
                        tabs,
                        active_idx,
                        scale,
                    },
                );
            }
        }

        // Drop overlays whose key is no longer desired (column became
        // Vertical, workspace switched out, monitor disconnected, etc.).
        // Drop happens via `retain` so each removed overlay's `Drop` impl
        // tears down its thread + hwnds + tooltip cleanly.
        self.tab_strip_overlays
            .retain(|key, _| desired.contains_key(key));

        // For each desired key, ensure an overlay exists and call show.
        let strip_height =
            (self.config.appearance.tab_strip_height as f64 * 1.0).round() as u32;
        let bottom_gap_px = self.config.layout.gap.max(0) as u32;
        let _ = strip_height; // per-strip scaling done below
        let _ = bottom_gap_px;
        for (key, show) in desired {
            let (monitor, ws_idx, col_idx) = key;
            // Spawn the overlay if missing. New overlays always render
            // at the next show() so the user sees the strip immediately.
            if !self.tab_strip_overlays.contains_key(&key) {
                let Some(tx) = self.tab_strip_action_tx.clone() else {
                    continue;
                };
                match leopardwm_platform_win32::tab_strip::TabStripOverlay::new(tx) {
                    Ok(overlay) => {
                        self.tab_strip_overlays.insert(key, overlay);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to create TabStripOverlay for {:?}: {}",
                            key,
                            e
                        );
                        continue;
                    }
                }
            }
            let Some(strip) = self.tab_strip_overlays.get(&key) else { continue };
            let scaled_strip_height =
                (self.config.appearance.tab_strip_height as f64 * show.scale).round() as u32;
            let scaled_bottom_gap_px =
                (self.config.layout.gap.max(0) as f64 * show.scale).round() as u32;
            strip.show(
                show.rect,
                show.tabs,
                show.active_idx,
                colors,
                scaled_strip_height,
                scaled_bottom_gap_px,
                monitor,
                ws_idx,
                col_idx,
                show.scale,
                close_action,
            );
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

            // Skip SetForegroundWindow if the target is currently cloaked
            // (off-screen-parked or ghost-animating). Calling
            // SetForegroundWindow on a cloaked HWND is undocumented
            // behavior; the v3 plan dodges it entirely. Internal focus
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
            // No focused window on the active workspace — clear stale state
            // so border/focus don't target a window that's no longer here.
            self.previous_focused_hwnd = None;
            self.hide_border();
            self.hide_tab_strip();
            debug!("sync_foreground_window: no focused visible window");
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
        }
    }
}

/// Parse a hex RGB string (e.g. `"4285F4"` or `"#4285F4"`) into a
/// BGR-byte-order `u32` suitable for GDI `COLORREF`. Accepts 6-digit
/// hex with optional leading `#`. Returns `None` on malformed input.
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
