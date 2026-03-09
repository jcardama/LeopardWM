//! Window event handling for AppState.

use crate::config;
use crate::state::{
    AppState, DragHintAction, DragState, FALLBACK_VIEWPORT_HEIGHT, FALLBACK_VIEWPORT_WIDTH,
    RECENTLY_HIDDEN_TTL, TRANSIENT_WINDOW_THRESHOLD,
};
use leopardwm_core_layout::Rect;
use leopardwm_platform_win32::{
    enumerate_monitors, find_monitor_for_rect, get_process_executable, is_shift_key_pressed,
    WindowEvent,
};
use tracing::{debug, info, warn};

impl AppState {
    /// Handle a window lifecycle event.
    pub(crate) fn handle_window_event(&mut self, event: WindowEvent) {
        // Get window_id from event for validation (DisplayChange and MouseEnterWindow have no validation needed)
        let window_id = match &event {
            WindowEvent::Created(id)
            | WindowEvent::Destroyed(id)
            | WindowEvent::Hidden(id)
            | WindowEvent::Focused(id)
            | WindowEvent::Minimized(id)
            | WindowEvent::Restored(id)
            | WindowEvent::MovedOrResized(id)
            | WindowEvent::MoveSizeStart(id)
            | WindowEvent::MoveSizeEnd(id) => Some(*id),
            WindowEvent::DisplayChange | WindowEvent::MouseEnterWindow(_) => None,
        };

        // Validate window existence for events that require it.
        // Skip validation for:
        //   - Destroyed/Hidden events (window is already gone or invisible)
        //   - Windows we already know about (managed or injected in tests)
        //   - DisplayChange / MouseEnterWindow (no window to validate)
        if let Some(wid) = window_id {
            if !matches!(event, WindowEvent::Destroyed(_) | WindowEvent::Hidden(_))
                && !self.is_known_window(wid)
                && !leopardwm_platform_win32::is_valid_window(wid)
            {
                debug!("Ignoring event for invalid window {}", wid);
                return;
            }
        }

        match event {
            WindowEvent::Created(hwnd) => {
                // Suppress transient windows that rapidly show/hide the same HWND
                // (e.g., Electron notification popups from Beeper, Slack).
                if let Some(&hidden_at) = self.recently_hidden_hwnds.get(&hwnd) {
                    if hidden_at.elapsed() < RECENTLY_HIDDEN_TTL {
                        debug!(
                            "Ignoring transient re-created window {} (hidden {}ms ago)",
                            hwnd,
                            hidden_at.elapsed().as_millis()
                        );
                        return;
                    }
                }
                // Lazily evict expired entries on the Created path too
                self.recently_hidden_hwnds
                    .retain(|_, t| t.elapsed() < RECENTLY_HIDDEN_TTL);

                // Check if any workspace already manages this window
                if self.find_window_workspace(hwnd).is_some() {
                    debug!("Window {} already managed, ignoring create event", hwnd);
                    return;
                }

                // Try to get window info for filtering and monitor assignment
                if let Some(win_info) = self.lookup_window_info(hwnd) {
                    // Get executable name for rule matching
                    let executable =
                        get_process_executable(win_info.process_id).unwrap_or_default();

                    // Check window rules
                    let action = self.evaluate_window_rules(
                        &win_info.class_name,
                        &win_info.title,
                        &executable,
                    );

                    // Skip ignored windows
                    if action == config::WindowAction::Ignore {
                        debug!(
                            "Ignoring window by rule: {} ({})",
                            win_info.title, win_info.class_name
                        );
                        return;
                    }

                    // Determine which monitor this window should be on
                    let monitors: Vec<_> = self.monitors.values().cloned().collect();
                    let monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
                        .map(|m| m.id)
                        .unwrap_or(self.focused_monitor);

                    // Get floating rect before borrowing workspace mutably
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

                    let viewport_width = self.viewport_width_for(monitor_id);

                    // Snapshot before structural change for tiled window animation.
                    let snapshot = if action == config::WindowAction::Tile {
                        Some(self.snapshot_layout())
                    } else {
                        None
                    };

                    let active_idx = self.active_workspace_idx(monitor_id);
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(active_idx)) {
                        let added = match action {
                            config::WindowAction::Float => {
                                // Use rule dimensions or default to centered 800x600 window
                                let rect = floating_rect.unwrap_or_else(|| {
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
                                workspace.add_floating(hwnd, rect).is_ok()
                            }
                            config::WindowAction::Tile => {
                                if self.config.behavior.focus_new_windows {
                                    workspace.insert_window(hwnd, None).is_ok()
                                } else {
                                    workspace.insert_window_no_focus(hwnd, None).is_ok()
                                }
                            }
                            config::WindowAction::Ignore => unreachable!(),
                        };

                        if added {
                            self.window_managed_at.insert(hwnd, std::time::Instant::now());
                            info!(
                                "Window created: {} ({}) - added to monitor {} as {:?}",
                                win_info.title, win_info.class_name, monitor_id, action
                            );
                            if self.config.behavior.focus_new_windows {
                                workspace.ensure_focused_visible_animated(viewport_width);
                            }
                            if let Some(snapshot) = snapshot {
                                self.start_layout_transition(snapshot);
                            }
                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after window create: {}", e);
                            }
                            if self.config.behavior.focus_new_windows {
                                self.sync_foreground_window();
                            }
                        } else {
                            debug!("Failed to add window {} to workspace", hwnd);
                        }
                    }
                }
            }
            WindowEvent::Destroyed(hwnd) | WindowEvent::Hidden(hwnd) => {
                let is_hidden_event = matches!(event, WindowEvent::Hidden(_));
                let event_name = if is_hidden_event { "hidden" } else { "destroyed" };

                // For Hidden events, verify the window is actually gone.
                // Electron apps (Slack, Beeper, Obsidian) fire spurious
                // EVENT_OBJECT_HIDE on their main window during internal
                // state changes (notification badges, focus between panes).
                // If the HWND is still valid and visible, ignore the event.
                if is_hidden_event
                    && self.find_window_workspace(hwnd).is_some()
                    && leopardwm_platform_win32::is_window_visible(hwnd)
                {
                    debug!(
                        "Ignoring spurious Hidden event for still-visible window {}",
                        hwnd
                    );
                    return;
                }

                // Only mark as transient (suppress future re-creation) if the
                // window was managed briefly. Long-lived windows (e.g., close-to-tray
                // apps) should be allowed to re-tile when restored.
                if let Some(managed_at) = self.window_managed_at.remove(&hwnd) {
                    if managed_at.elapsed() < TRANSIENT_WINDOW_THRESHOLD {
                        debug!(
                            "Marking window {} as transient (managed {}ms)",
                            hwnd,
                            managed_at.elapsed().as_millis()
                        );
                        self.recently_hidden_hwnds.insert(hwnd, std::time::Instant::now());
                    } else {
                        debug!(
                            "Window {} was managed {}s, not marking as transient",
                            hwnd,
                            managed_at.elapsed().as_secs()
                        );
                    }
                }
                // Lazily evict stale entries
                self.recently_hidden_hwnds
                    .retain(|_, t| t.elapsed() < RECENTLY_HIDDEN_TTL);

                // Find which workspace contains this window
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    let viewport_width = self.viewport_width_for(monitor_id);

                    let snapshot = self.snapshot_layout();
                    let mut was_tiled = false;
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        // Try to remove as floating window first
                        let was_floating = workspace.remove_floating(hwnd);

                        if was_floating {
                            info!(
                                "Floating window {} {} - removed from monitor {}",
                                hwnd, event_name, monitor_id
                            );
                        } else if let Err(e) = workspace.remove_window(hwnd) {
                            warn!("Failed to remove window {}: {}", hwnd, e);
                        } else {
                            was_tiled = true;
                            info!(
                                "Window {} {} - removed from monitor {}",
                                hwnd, event_name, monitor_id
                            );
                            workspace.ensure_focused_visible_animated(viewport_width);
                        }
                    }

                    if was_tiled {
                        self.start_layout_transition(snapshot);
                    }
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to apply layout after window {}: {}", event_name, e);
                    }
                }
            }
            WindowEvent::Focused(hwnd) => {
                // Skip if this window is already our tracked focus — avoids
                // feedback loops where sync_foreground_window triggers another
                // EVENT_SYSTEM_FOREGROUND for the same window.
                if self.previous_focused_hwnd == Some(hwnd) {
                    return;
                }

                // Reconcile: prune windows that vanished without events
                // (e.g., Electron close-to-tray apps).
                // Throttle to at most once per second to avoid per-event overhead.
                let now = std::time::Instant::now();
                if self.last_prune_at.is_none_or(|t| now.duration_since(t).as_secs() >= 1) {
                    self.last_prune_at = Some(now);
                    let pre_count = self.all_managed_window_ids().len();
                    self.prune_stale_windows();
                    let pruned = pre_count - self.all_managed_window_ids().len();
                    if pruned > 0 {
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to apply layout after pruning {} stale window(s): {}", pruned, e);
                        }
                    }
                }

                // Update focus to match what Windows says is focused
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    // Update focused monitor to match the window's monitor
                    self.focused_monitor = monitor_id;

                    // Auto-switch workspace if the focused window is on an inactive workspace
                    // (e.g., user Alt+Tabbed to it)
                    let active_idx = self.active_workspace_idx(monitor_id);
                    if ws_idx != active_idx {
                        info!("Auto-switching to workspace {} on monitor {} (focus follows window)", ws_idx + 1, monitor_id);

                        // Clean up any in-progress drag and placeholder
                        if self.drag_state.take().is_some() {
                            for (_, ws_vec) in self.workspaces.iter_mut() {
                                for ws in ws_vec.iter_mut() {
                                    let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                                }
                            }
                        }
                        self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
                        // Move exit windows offscreen before clearing the transition
                        if let Some(ref transition) = self.layout_transition {
                            for wid in transition.exit_rects.keys() {
                                let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
                            }
                        }
                        self.layout_transition = None;

                        let slide_height = self.monitors.get(&monitor_id)
                            .map(|m| m.work_area.height)
                            .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
                        let y_offset = if ws_idx > active_idx { slide_height } else { -slide_height };

                        // Snapshot old workspace positions for exit animation.
                        let old_placements: Vec<(u64, leopardwm_core_layout::Rect)> =
                            self.workspaces.get(&monitor_id)
                                .and_then(|v| v.get(active_idx))
                                .and_then(|ws| self.monitors.get(&monitor_id).map(|m| (ws, m)))
                                .map(|(ws, mon)| {
                                    ws.compute_placements_animated(mon.work_area)
                                        .into_iter()
                                        .map(|p| (p.window_id, p.rect))
                                        .collect()
                                })
                                .unwrap_or_default();

                        self.active_workspace.insert(monitor_id, ws_idx);

                        // Compute new workspace's final placements for enter animation.
                        let new_placements: Vec<(u64, leopardwm_core_layout::Rect)> =
                            self.workspaces.get(&monitor_id)
                                .and_then(|v| v.get(ws_idx))
                                .and_then(|ws| self.monitors.get(&monitor_id).map(|m| (ws, m)))
                                .map(|(ws, mon)| {
                                    ws.compute_placements_animated(mon.work_area)
                                        .into_iter()
                                        .map(|p| (p.window_id, p.rect))
                                        .collect()
                                })
                                .unwrap_or_default();

                        let mut start_rects = std::collections::HashMap::new();
                        let mut exit_rects = std::collections::HashMap::new();

                        for (wid, rect) in &new_placements {
                            start_rects.insert(*wid, leopardwm_core_layout::Rect::new(
                                rect.x, rect.y + y_offset, rect.width, rect.height,
                            ));
                        }
                        for (wid, rect) in &old_placements {
                            start_rects.insert(*wid, *rect);
                            exit_rects.insert(*wid, leopardwm_core_layout::Rect::new(
                                rect.x, rect.y - y_offset, rect.width, rect.height,
                            ));
                        }

                        if !start_rects.is_empty() {
                            self.start_workspace_switch_transition(
                                start_rects,
                                exit_rects,
                                crate::state::WORKSPACE_SWITCH_DURATION_MS,
                            );
                        }
                    }

                    let viewport_width = self.viewport_width_for(monitor_id);

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        if let Err(e) = workspace.focus_window(hwnd) {
                            // Floating windows are not in the tiled column list,
                            // so focus_window fails for them — that's expected.
                            debug!("Failed to focus window {}: {}", hwnd, e);
                        } else {
                            debug!("Focus changed to window {} on monitor {}", hwnd, monitor_id);
                            workspace.ensure_focused_visible_animated(viewport_width);
                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after focus change: {}", e);
                            }
                        }
                    }

                    // Update border only — do NOT call sync_foreground_window()
                    // here because the window is already focused (that's why we
                    // received this event). Calling set_foreground_window again
                    // would trigger another EVENT_SYSTEM_FOREGROUND feedback loop.
                    self.show_border(hwnd);

                    // Track the OS-foreground window — including floating windows —
                    // so that ToggleFloating can reliably detect and unfloat the
                    // currently focused floating window.
                    self.previous_focused_hwnd = Some(hwnd);
                } else {
                    // Recovery path: if a user focuses a window that was
                    // suppressed by recently_hidden_hwnds (e.g., tray-restored
                    // app), re-add it now. A user focusing a window proves it's
                    // not a transient popup.
                    if self.recently_hidden_hwnds.remove(&hwnd).is_some() {
                        if let Some(win_info) = self.lookup_window_info(hwnd) {
                            let executable =
                                get_process_executable(win_info.process_id).unwrap_or_default();
                            let action = self.evaluate_window_rules(
                                &win_info.class_name,
                                &win_info.title,
                                &executable,
                            );
                            if action != config::WindowAction::Ignore {
                                info!(
                                    "Recovering suppressed window: {} ({}) - user focused it",
                                    win_info.title, win_info.class_name
                                );
                                // Re-dispatch as Created to reuse add logic.
                                // Safe: we already removed hwnd from recently_hidden_hwnds
                                // above, so the Created handler won't re-suppress it.
                                self.handle_window_event(WindowEvent::Created(hwnd));
                                return;
                            }
                        }
                    }

                    // Focus went to an unmanaged window (e.g. settings, taskbar).
                    // Hide the border overlay and clear tracked hwnd so animation
                    // frames don't re-show it.
                    self.hide_border();
                    self.previous_focused_hwnd = None;
                }
            }
            WindowEvent::Minimized(hwnd) => {
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    let viewport_width = self.viewport_width_for(monitor_id);
                    let snapshot = self.snapshot_layout();
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        let cleared_fullscreen = workspace.clear_fullscreen_if_window(hwnd);
                        if workspace.mark_minimized(hwnd) || cleared_fullscreen {
                            info!("Window {} minimized", hwnd);

                            // If the minimized window was the focused window, move focus
                            if workspace.focused_window() == Some(hwnd) {
                                // Try to focus another window in the same column
                                workspace.focus_down();
                                if workspace.focused_window() == Some(hwnd) {
                                    workspace.focus_up();
                                }
                                // If still focused on minimized (only window in column), try next column
                                if workspace.focused_window() == Some(hwnd) {
                                    workspace.focus_right();
                                    if workspace.focused_window() == Some(hwnd) {
                                        workspace.focus_left();
                                    }
                                }
                            }
                            workspace.ensure_focused_visible_animated(viewport_width);

                            self.start_layout_transition(snapshot);
                            if let Err(e) = self.apply_layout() {
                                warn!("Failed to apply layout after minimize: {}", e);
                            }
                            // Keep monitor focus aligned before foreground sync so we don't
                            // accidentally steer foreground to a stale monitor.
                            self.focused_monitor = monitor_id;
                            self.sync_foreground_window();
                        }
                    }
                } else {
                    debug!("Window {} minimized (unmanaged)", hwnd);
                }
            }
            WindowEvent::Restored(hwnd) => {
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    let viewport_width = self.viewport_width_for(monitor_id);
                    let snapshot = self.snapshot_layout();
                    let mut should_sync_foreground = false;
                    let mut was_tiled_restore = false;
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                        if workspace.mark_restored(hwnd) {
                            info!("Window {} restored from minimized", hwnd);
                            if workspace.is_floating(hwnd) {
                                // Keep floating restores from stealing focus back to tiled windows.
                                debug!(
                                    "Restored floating window {} without changing tiled focus",
                                    hwnd
                                );
                            } else if let Err(e) = workspace.focus_window(hwnd) {
                                warn!("Failed to focus restored window {}: {}", hwnd, e);
                            } else {
                                workspace.ensure_focused_visible_animated(viewport_width);
                                should_sync_foreground = true;
                                was_tiled_restore = true;
                            }
                        }
                    }
                    if was_tiled_restore {
                        self.start_layout_transition(snapshot);
                    }
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to apply layout after window restore: {}", e);
                    }
                    if should_sync_foreground {
                        self.focused_monitor = monitor_id;
                        self.sync_foreground_window();
                    }
                } else {
                    debug!("Window {} restored (unmanaged)", hwnd);
                }
            }
            WindowEvent::MoveSizeStart(hwnd) => {
                debug!("User started dragging/resizing window {}", hwnd);
                let (is_tiled, source_monitor, col_idx) =
                    if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                        let is_floating = self
                            .workspaces
                            .get(&monitor_id)
                            .and_then(|v| v.get(ws_idx))
                            .is_none_or(|ws| ws.is_floating(hwnd));
                        let col_idx = if !is_floating {
                            self.workspaces
                                .get(&monitor_id)
                                .and_then(|v| v.get(ws_idx))
                                .and_then(|ws| ws.find_window_location(hwnd))
                                .map(|(col, _)| col)
                                .unwrap_or(0)
                        } else {
                            0
                        };
                        (!is_floating, monitor_id, col_idx)
                    } else {
                        (false, self.focused_monitor, 0)
                    };
                self.drag_state = Some(DragState {
                    hwnd,
                    is_tiled,
                    source_monitor,
                    current_column_index: col_idx,
                    last_drop_target: None,
                    last_hint_update: None,
                    removed_from_source: false,
                });
            }
            WindowEvent::MoveSizeEnd(hwnd) => {
                debug!("User finished dragging/resizing window {}", hwnd);
                let drag = self.drag_state.take();
                // Always hide the drag hint overlay on drop.
                self.pending_drag_hint = Some(DragHintAction::Hide);

                let Some(drag) = drag else { return };

                if !drag.is_tiled {
                    // Floating window: update stored rect so layout won't snap it back.
                    if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                        if let Some(win_info) = self.lookup_window_info(hwnd) {
                            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                                workspace.update_floating(hwnd, win_info.rect);
                                debug!(
                                    "Floating window {} dropped at {:?}",
                                    hwnd, win_info.rect
                                );
                            }
                        }
                    }
                    return;
                }

                // Tiled window: determine final drop target.
                let Some(win_info) = self.lookup_window_info(hwnd) else {
                    self.snap_back_tiled(drag.source_monitor);
                    return;
                };
                let monitors: Vec<_> = self.monitors.values().cloned().collect();
                let target_monitor = find_monitor_for_rect(&monitors, &win_info.rect)
                    .map(|m| m.id)
                    .unwrap_or(drag.source_monitor);

                let shift_held = is_shift_key_pressed();

                if shift_held {
                    // Clean up placeholder (shouldn't exist in shift mode, but be safe).
                    for (_, ws_vec) in self.workspaces.iter_mut() {
                        for ws in ws_vec.iter_mut() {
                            let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                        }
                    }
                    // Shift+drop: column reorder (already live-reordered, or cross-monitor).
                    if target_monitor == drag.source_monitor {
                        self.snap_back_tiled(drag.source_monitor);
                    } else {
                        self.execute_cross_monitor_drag(
                            hwnd,
                            &drag,
                            target_monitor,
                            &win_info.rect,
                        );
                    }
                } else {
                    // Default drop: swap placeholder with real window in-place.
                    self.finalize_drag_merge(hwnd, &drag, target_monitor, &win_info.rect);
                }
            }
            WindowEvent::MovedOrResized(hwnd) => {
                // Skip events triggered by our own apply_layout() to avoid feedback loop.
                if self.applying_layout || self.should_suppress_moved_or_resized(hwnd) {
                    return;
                }
                // During drag: compute drop target and show snap hint for tiled windows.
                if let Some(ref mut drag) = self.drag_state {
                    if drag.hwnd == hwnd {
                        if drag.is_tiled {
                            // Throttle hint updates to ~60fps
                            let now = std::time::Instant::now();
                            if drag
                                .last_hint_update
                                .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                            {
                                return;
                            }
                            drag.last_hint_update = Some(now);
                            self.update_drag_hint(hwnd);
                        }
                        return;
                    }
                }
                // Non-drag: if the window is managed (tiled), snap it back to its layout position.
                if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                    let is_floating = self
                        .workspaces
                        .get(&monitor_id)
                        .and_then(|v| v.get(ws_idx))
                        .is_none_or(|ws| ws.is_floating(hwnd));

                    if !is_floating {
                        debug!("Managed window {} moved/resized — snapping back", hwnd);
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to snap back layout after move/resize: {}", e);
                        }
                    }
                }
            }
            WindowEvent::DisplayChange => {
                // Display configuration changed (monitors added/removed/rearranged)
                info!("Display configuration changed - reconciling monitors");

                // Re-enumerate monitors
                match enumerate_monitors() {
                    Ok(new_monitors) if !new_monitors.is_empty() => {
                        info!(
                            "Detected {} monitor(s) after display change",
                            new_monitors.len()
                        );
                        for m in &new_monitors {
                            info!(
                                "  Monitor {}: {}x{} at ({},{}){} \"{}\"",
                                m.id,
                                m.work_area.width,
                                m.work_area.height,
                                m.work_area.x,
                                m.work_area.y,
                                if m.is_primary { " [PRIMARY]" } else { "" },
                                m.device_name
                            );
                        }

                        // Reconcile workspaces with new monitor configuration
                        self.reconcile_monitors(new_monitors);

                        // Re-apply layout with updated monitor configuration
                        if let Err(e) = self.apply_layout() {
                            warn!("Failed to apply layout after display change: {}", e);
                        }
                    }
                    Ok(_) => {
                        warn!("No monitors found after display change");
                    }
                    Err(e) => {
                        warn!("Failed to enumerate monitors after display change: {}", e);
                    }
                }
            }
            WindowEvent::MouseEnterWindow(_hwnd) => {
                // This is handled by the main event loop with debouncing
                // (focus_follows_mouse delay)
            }
        }
    }

    /// Apply focus to a window for focus-follows-mouse.
    /// Returns true if focus was applied, false if the window isn't managed.
    pub(crate) fn apply_focus_follows_mouse(&mut self, hwnd: u64) -> bool {
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            // Update focused monitor to match the window's monitor
            self.focused_monitor = monitor_id;

            let viewport_width = self.viewport_width_for(monitor_id);

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                if workspace.is_floating(hwnd) {
                    // Floating windows are managed but not represented in tiled columns.
                    self.previous_focused_hwnd = Some(hwnd);
                    let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
                    debug!(
                        "Focus-follows-mouse: focused floating window {} on monitor {}",
                        hwnd, monitor_id
                    );
                    return true;
                }
                if let Err(e) = workspace.focus_window(hwnd) {
                    debug!(
                        "Failed to focus window {} for focus-follows-mouse: {}",
                        hwnd, e
                    );
                    return false;
                }
                debug!(
                    "Focus-follows-mouse: focused window {} on monitor {}",
                    hwnd, monitor_id
                );
                workspace.ensure_focused_visible_animated(viewport_width);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout after focus-follows-mouse: {}", e);
                }
                self.sync_foreground_window();
                return true;
            }
        }
        false
    }
}
