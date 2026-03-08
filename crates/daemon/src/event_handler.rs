//! Window event handling for AppState.

use crate::config;
use crate::state::{
    AppState, DragHintAction, DragState, DropTarget, FALLBACK_VIEWPORT_HEIGHT,
    FALLBACK_VIEWPORT_WIDTH, RECENTLY_HIDDEN_TTL, TRANSIENT_WINDOW_THRESHOLD,
};
use leopardwm_core_layout::{Rect, Visibility};
use leopardwm_platform_win32::{
    enumerate_monitors, find_monitor_for_rect, get_process_executable, is_shift_key_pressed,
    MonitorId, WindowEvent,
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

                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    // Snapshot before structural change for tiled window animation.
                    let snapshot = if action == config::WindowAction::Tile {
                        Some(self.snapshot_layout())
                    } else {
                        None
                    };

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    let snapshot = self.snapshot_layout();
                    let mut was_tiled = false;
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    // Update focused monitor to match the window's monitor
                    self.focused_monitor = monitor_id;

                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let snapshot = self.snapshot_layout();
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let viewport_width = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let snapshot = self.snapshot_layout();
                    let mut should_sync_foreground = false;
                    let mut was_tiled_restore = false;
                    if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                    if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                        let is_floating = self
                            .workspaces
                            .get(&monitor_id)
                            .is_none_or(|ws| ws.is_floating(hwnd));
                        let col_idx = if !is_floating {
                            self.workspaces
                                .get(&monitor_id)
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
                    if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                        if let Some(win_info) = self.lookup_window_info(hwnd) {
                            if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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
                    for (_, ws) in self.workspaces.iter_mut() {
                        let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
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
                if let Some(monitor_id) = self.find_window_workspace(hwnd) {
                    let is_floating = self
                        .workspaces
                        .get(&monitor_id)
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

    /// Compute and show a drag hint overlay.
    /// Default drag = move window between columns (merge mode).
    /// Shift+drag = move entire column (reorder mode).
    fn update_drag_hint(&mut self, hwnd: u64) {
        let Some(win_info) = self.lookup_window_info(hwnd) else {
            return;
        };

        // Use actual cursor position for more intuitive hit-testing —
        // the window center lags behind the cursor for large windows.
        let (cursor_x, cursor_y) =
            leopardwm_platform_win32::get_cursor_pos().unwrap_or_else(|| {
                (
                    win_info.rect.x + win_info.rect.width / 2,
                    win_info.rect.y + win_info.rect.height / 2,
                )
            });

        // Determine which monitor the dragged window is on.
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        let target_monitor_id = find_monitor_for_rect(&monitors, &win_info.rect)
            .map(|m| m.id)
            .unwrap_or(self.focused_monitor);

        let shift_held = is_shift_key_pressed();

        // Read drag state fields.
        let (current_col, source_monitor) = match self.drag_state {
            Some(ref d) => (d.current_column_index, d.source_monitor),
            None => return,
        };

        if shift_held {
            // --- Shift+drag: column reorder mode ---
            // Only live-reorder on the source monitor; cross-monitor happens on drop.
            if target_monitor_id != source_monitor {
                // Show ghost at the edge of the target monitor.
                let viewport = match self.monitors.get(&target_monitor_id) {
                    Some(m) => m.work_area,
                    None => return,
                };
                let Some(workspace) = self.workspaces.get(&target_monitor_id) else {
                    return;
                };
                let column_bounds = column_bounds_from_placements(workspace, viewport);
                let insert_index = compute_insertion_index(&column_bounds, cursor_x);
                let drop_target = DropTarget {
                    monitor: target_monitor_id,
                    insert_index,
                    window_slot: None,
                };
                if let Some(ref mut drag) = self.drag_state {
                    if drag.last_drop_target == Some(drop_target) {
                        return;
                    }
                    drag.last_drop_target = Some(drop_target);
                }
                let gap = workspace.gap();
                let hint_x = compute_insertion_hint_x(&column_bounds, insert_index, gap);
                self.pending_drag_hint = Some(DragHintAction::ShowGhost {
                    rect: Rect::new(hint_x - 2, viewport.y, 4, viewport.height),
                });
                return;
            }

            let viewport = match self.monitors.get(&source_monitor) {
                Some(m) => m.work_area,
                None => return,
            };
            let Some(workspace) = self.workspaces.get(&source_monitor) else {
                return;
            };

            let column_bounds = column_bounds_from_placements(workspace, viewport);
            // Trigger reorder when cursor enters another column's area.
            let target_idx = match compute_target_column_index(&column_bounds, cursor_x) {
                Some(idx) => idx,
                None => {
                    // Cursor is in the gap between columns — keep current position.
                    // Still show the ghost at the current column.
                    if let Some(rect) = compute_column_rect(workspace, viewport, current_col) {
                        self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect });
                    }
                    return;
                }
            };

            if target_idx != current_col {
                debug!(
                    "Live drag reorder: column {} → {} on monitor {}",
                    current_col, target_idx, source_monitor
                );
                let snapshot = self.snapshot_layout();
                if let Some(workspace) = self.workspaces.get_mut(&source_monitor) {
                    workspace.reorder_column(current_col, target_idx);
                }
                if let Some(ref mut drag) = self.drag_state {
                    drag.current_column_index = target_idx;
                }
                self.start_layout_transition(snapshot);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout during live drag reorder: {}", e);
                }
            }

            // Show ghost at the dragged column's new position.
            let workspace = match self.workspaces.get(&source_monitor) {
                Some(ws) => ws,
                None => return,
            };
            let new_col_idx = match self.drag_state {
                Some(ref d) => d.current_column_index,
                None => return,
            };
            if let Some(rect) = compute_column_rect(workspace, viewport, new_col_idx) {
                self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect });
            }
        } else {
            // --- Default drag: window merge mode with live preview ---
            // Source column keeps the dragged window (preserving its space).
            // Target column gets a placeholder so its windows shift to make room.
            use crate::state::DRAG_PLACEHOLDER_HWND;

            let viewport = match self.monitors.get(&target_monitor_id) {
                Some(m) => m.work_area,
                None => return,
            };

            // Remove any existing placeholder before recomputing bounds.
            for (_, ws) in self.workspaces.iter_mut() {
                let _ = ws.remove_window(DRAG_PLACEHOLDER_HWND);
            }

            let Some(workspace) = self.workspaces.get(&target_monitor_id) else {
                return;
            };
            let column_bounds = column_bounds_from_placements(workspace, viewport);
            let target_col = match compute_target_column_index(&column_bounds, cursor_x) {
                Some(idx) => idx,
                None => return,
            };

            // Determine if the dragged window is in the target column.
            let is_same_column = workspace
                .column(target_col)
                .is_some_and(|c| c.contains(hwnd));

            let n_existing = workspace
                .column(target_col)
                .map(|c| c.len())
                .unwrap_or(0);
            // Same column: N slots (reorder). Different column: N+1 (placeholder added).
            let n_total = if is_same_column {
                n_existing
            } else {
                n_existing + 1
            };
            if n_total == 0 {
                return;
            }

            let col_rect = match compute_column_rect(workspace, viewport, target_col) {
                Some(r) => r,
                None => return,
            };

            let window_slot = compute_window_slot(&col_rect, n_total, cursor_y);

            let drop_target = DropTarget {
                monitor: target_monitor_id,
                insert_index: target_col,
                window_slot: Some(window_slot),
            };
            if let Some(ref mut drag) = self.drag_state {
                if drag.last_drop_target == Some(drop_target) {
                    return;
                }
                drag.last_drop_target = Some(drop_target);
            }

            if is_same_column {
                // Same column: reorder the window within its column.
                let current_location = workspace.find_window_location(hwnd);
                let needs_move = match current_location {
                    Some((_, cur_win_idx)) => cur_win_idx != window_slot,
                    None => false,
                };
                if needs_move {
                    let snapshot = self.snapshot_layout();
                    if let Some(ws) = self.workspaces.get_mut(&target_monitor_id) {
                        let _ = ws.remove_window(hwnd);
                        let _ = ws.insert_window_in_column_at(hwnd, target_col, window_slot);
                    }
                    self.start_layout_transition(snapshot);
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to apply layout during live drag reorder: {}", e);
                    }
                }
            } else {
                // Different column: remove window from multi-window source so
                // remaining windows expand, then insert placeholder at target.
                let snapshot = self.snapshot_layout();

                let already_removed = self
                    .drag_state
                    .as_ref()
                    .is_some_and(|d| d.removed_from_source);
                if !already_removed {
                    // Remove window from multi-window source columns so remaining
                    // windows expand. Single-window columns keep the window to
                    // preserve column space until drop.
                    let should_remove = self
                        .workspaces
                        .get(&source_monitor)
                        .and_then(|ws| {
                            let (col, _) = ws.find_window_location(hwnd)?;
                            Some(ws.column(col)?.len() > 1)
                        })
                        .unwrap_or(false);
                    if should_remove {
                        if let Some(ws) = self.workspaces.get_mut(&source_monitor) {
                            let _ = ws.remove_window(hwnd);
                        }
                        if let Some(ref mut drag) = self.drag_state {
                            drag.removed_from_source = true;
                        }
                    }
                }

                // Insert placeholder at target to shift target windows.
                // Recompute target_col since removing from source may have shifted indices.
                let adj_target_col = if self
                    .drag_state
                    .as_ref()
                    .is_some_and(|d| d.removed_from_source)
                {
                    // Re-derive from updated layout.
                    let ws = match self.workspaces.get(&target_monitor_id) {
                        Some(ws) => ws,
                        None => return,
                    };
                    let bounds = column_bounds_from_placements(ws, viewport);
                    match compute_target_column_index(&bounds, cursor_x) {
                        Some(idx) => idx,
                        None => return,
                    }
                } else {
                    target_col
                };

                if let Some(ws) = self.workspaces.get_mut(&target_monitor_id) {
                    if adj_target_col < ws.column_count() {
                        let _ = ws.insert_window_in_column_at(
                            DRAG_PLACEHOLDER_HWND,
                            adj_target_col,
                            window_slot,
                        );
                    } else {
                        let _ = ws.insert_window(DRAG_PLACEHOLDER_HWND, None);
                    }
                }
                self.start_layout_transition(snapshot);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout during live drag preview: {}", e);
                }
            }

            // Show ghost at the target slot position (recompute from updated layout).
            let workspace = match self.workspaces.get(&target_monitor_id) {
                Some(ws) => ws,
                None => return,
            };

            // For cross-column, ghost at the placeholder position; for same-column, at the window.
            let ghost_id = if is_same_column {
                hwnd
            } else {
                DRAG_PLACEHOLDER_HWND
            };
            if let Some((ghost_col, ghost_slot)) = workspace.find_window_location(ghost_id) {
                if let Some(ghost_col_rect) =
                    compute_column_rect(workspace, viewport, ghost_col)
                {
                    let ghost_n = workspace
                        .column(ghost_col)
                        .map(|c| c.len())
                        .unwrap_or(1);
                    let gap = workspace.gap();
                    let total_gaps = (ghost_n as i32 - 1) * gap;
                    let usable_height = ghost_col_rect.height - total_gaps;
                    let slot_height = usable_height / ghost_n as i32;
                    let slot_y =
                        ghost_col_rect.y + ghost_slot as i32 * (slot_height + gap);
                    let ghost = Rect::new(
                        ghost_col_rect.x,
                        slot_y,
                        ghost_col_rect.width,
                        slot_height,
                    );
                    self.pending_drag_hint = Some(DragHintAction::ShowGhost { rect: ghost });
                }
            }
        }
    }

    /// Execute window merge: extract the dragged window from its column and
    /// insert it at the target slot in the target column.
    fn execute_window_merge(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        let source_monitor = drag.source_monitor;

        // Find target column and slot from cursor position.
        let target_viewport = match self.monitors.get(&target_monitor) {
            Some(m) => m.work_area,
            None => {
                self.snap_back_tiled(source_monitor);
                return;
            }
        };

        let (target_col_idx, window_slot) = {
            let Some(workspace) = self.workspaces.get(&target_monitor) else {
                self.snap_back_tiled(source_monitor);
                return;
            };
            let column_bounds = column_bounds_from_placements(workspace, target_viewport);
            // Use cursor position for intuitive drop targeting.
            let (cx, cy) = leopardwm_platform_win32::get_cursor_pos().unwrap_or_else(|| {
                (
                    win_rect.x + win_rect.width / 2,
                    win_rect.y + win_rect.height / 2,
                )
            });
            let col_idx = match compute_target_column_index(&column_bounds, cx) {
                Some(idx) => idx,
                None => {
                    self.snap_back_tiled(source_monitor);
                    return;
                }
            };
            let is_same_col = workspace
                .column(col_idx)
                .is_some_and(|c| c.contains(hwnd));
            let n_existing = workspace
                .column(col_idx)
                .map(|c| c.len())
                .unwrap_or(0);
            let n_total = if is_same_col { n_existing } else { n_existing + 1 };
            let col_rect = compute_column_rect(workspace, target_viewport, col_idx);
            let slot = match col_rect {
                Some(ref r) if n_total > 0 => compute_window_slot(r, n_total, cy),
                _ => 0,
            };
            (col_idx, slot)
        };

        // Check if window was already removed from source during live drag preview.
        let already_removed = drag.removed_from_source;

        // Find source column info before removal.
        let src_col_info = if !already_removed && target_monitor == source_monitor {
            self.workspaces
                .get(&source_monitor)
                .and_then(|ws| ws.find_window_location(hwnd))
                .map(|(col_idx, _)| {
                    let col_len = self
                        .workspaces
                        .get(&source_monitor)
                        .and_then(|ws| ws.column(col_idx))
                        .map(|c| c.len())
                        .unwrap_or(0);
                    (col_idx, col_len)
                })
        } else {
            None
        };

        // Single-window column dropped onto itself → snap back, nothing to merge.
        if let Some((src_col, src_len)) = src_col_info {
            if target_col_idx == src_col && src_len == 1 {
                self.snap_back_tiled(source_monitor);
                return;
            }
        }

        // Snapshot AFTER all early returns, right before structural changes.
        let snapshot = self.snapshot_layout();

        // Remove the window from its source column (skip if already removed during drag).
        if !already_removed {
            if let Some(workspace) = self.workspaces.get_mut(&source_monitor) {
                if let Err(e) = workspace.remove_window(hwnd) {
                    warn!("Failed to remove window {} for merge: {}", hwnd, e);
                    self.snap_back_tiled(source_monitor);
                    return;
                }
            }
        }

        // Adjust target column index if removing from the same monitor shifted indices.
        // If the source column was before the target and was fully removed (single window),
        // all subsequent columns shifted left by 1.
        let effective_target_col = if let Some((src_col, src_len)) = src_col_info {
            if src_len == 1 && src_col < target_col_idx {
                target_col_idx - 1
            } else {
                target_col_idx
            }
        } else {
            target_col_idx
        };

        // Add window to the target column at the computed slot.
        if let Some(workspace) = self.workspaces.get_mut(&target_monitor) {
            if workspace.column_count() == 0 {
                let _ = workspace.insert_window(hwnd, None);
            } else if let Err(e) =
                workspace.insert_window_in_column_at(hwnd, effective_target_col, window_slot)
            {
                warn!(
                    "Failed to merge window {} into column {} at slot {}: {}",
                    hwnd, effective_target_col, window_slot, e
                );
                let _ = workspace.insert_window(hwnd, None);
            }
            if let Err(e) = workspace.focus_window(hwnd) {
                debug!("Failed to focus merged window {}: {}", hwnd, e);
            }
            workspace.ensure_focused_visible_animated(target_viewport.width);
        }

        self.focused_monitor = target_monitor;
        info!(
            "Window merge: {} into column {} slot {} on monitor {}",
            hwnd, effective_target_col, window_slot, target_monitor
        );

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after window merge: {}", e);
        }
        self.sync_foreground_window();
    }

    /// Finalize a default drag-drop by swapping the placeholder with the real window.
    /// Avoids a redundant transition since windows are already at their final positions
    /// from the live preview during drag.
    fn finalize_drag_merge(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        use crate::state::DRAG_PLACEHOLDER_HWND;
        let source_monitor = drag.source_monitor;

        // Find where the placeholder is (this is where the real window should go).
        let placeholder_location = self
            .workspaces
            .get(&target_monitor)
            .and_then(|ws| ws.find_window_location(DRAG_PLACEHOLDER_HWND));

        if let Some((ph_col, ph_slot)) = placeholder_location {
            // Capture source column info BEFORE any removals.
            let src_info = if !drag.removed_from_source && target_monitor == source_monitor {
                self.workspaces
                    .get(&source_monitor)
                    .and_then(|ws| ws.find_window_location(hwnd))
                    .map(|(col, _)| {
                        let len = self
                            .workspaces
                            .get(&source_monitor)
                            .and_then(|ws| ws.column(col))
                            .map(|c| c.len())
                            .unwrap_or(0);
                        (col, len)
                    })
            } else {
                None
            };

            // Remove placeholder.
            if let Some(ws) = self.workspaces.get_mut(&target_monitor) {
                let _ = ws.remove_window(DRAG_PLACEHOLDER_HWND);
            }

            // Remove real window from source (if not already removed during drag).
            if !drag.removed_from_source {
                if let Some(ws) = self.workspaces.get_mut(&source_monitor) {
                    let _ = ws.remove_window(hwnd);
                }
            }

            // Adjust column index if source column was removed and was before placeholder.
            let adj_col = if let Some((src_col, src_len)) = src_info {
                if src_len == 1 && src_col < ph_col {
                    ph_col - 1
                } else {
                    ph_col
                }
            } else {
                ph_col
            };

            // Insert real window at the placeholder's former position.
            if let Some(ws) = self.workspaces.get_mut(&target_monitor) {
                if ws.column_count() == 0 {
                    let _ = ws.insert_window(hwnd, None);
                } else if let Err(e) = ws.insert_window_in_column_at(hwnd, adj_col, ph_slot)
                {
                    warn!(
                        "Failed to place window {} at col {} slot {}: {}",
                        hwnd, adj_col, ph_slot, e
                    );
                    let _ = ws.insert_window(hwnd, None);
                }
                if let Err(e) = ws.focus_window(hwnd) {
                    debug!("Failed to focus merged window {}: {}", hwnd, e);
                }
                let vw = self
                    .monitors
                    .get(&target_monitor)
                    .map(|m| m.work_area.width)
                    .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                ws.ensure_focused_visible_animated(vw);
            }

            self.focused_monitor = target_monitor;
            // Clear any in-progress transition so windows stay at their current positions.
            self.layout_transition = None;
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after drag merge: {}", e);
            }
            self.sync_foreground_window();
        } else {
            // No placeholder found — fall back to full merge (cross-monitor or edge case).
            for (_, ws) in self.workspaces.iter_mut() {
                let _ = ws.remove_window(DRAG_PLACEHOLDER_HWND);
            }
            self.execute_window_merge(hwnd, drag, target_monitor, win_rect);
        }
    }

    /// Move a column to a different monitor after cross-monitor drag-drop.
    fn execute_cross_monitor_drag(
        &mut self,
        hwnd: u64,
        drag: &DragState,
        target_monitor: MonitorId,
        win_rect: &Rect,
    ) {
        let source_monitor = drag.source_monitor;
        let snapshot = self.snapshot_layout();

        // Get current column index (may differ from drag start if events intervened).
        let col_idx = match self
            .workspaces
            .get(&source_monitor)
            .and_then(|ws| ws.find_window_location(hwnd))
            .map(|(col, _)| col)
        {
            Some(idx) => idx,
            None => {
                self.snap_back_tiled(source_monitor);
                return;
            }
        };

        // Compute target insertion index.
        let target_viewport = match self.monitors.get(&target_monitor) {
            Some(m) => m.work_area,
            None => {
                self.snap_back_tiled(source_monitor);
                return;
            }
        };

        let target_bounds = self
            .workspaces
            .get(&target_monitor)
            .map(|ws| column_bounds_from_placements(ws, target_viewport))
            .unwrap_or_default();
        let win_center_x = win_rect.x + win_rect.width / 2;
        let insert_idx = compute_insertion_index(&target_bounds, win_center_x);

        // Remove column from source workspace.
        let column = match self
            .workspaces
            .get_mut(&source_monitor)
            .and_then(|ws| ws.remove_column(col_idx))
        {
            Some(col) => col,
            None => {
                self.snap_back_tiled(source_monitor);
                return;
            }
        };

        debug!(
            "Cross-monitor drag: column with {} window(s) from monitor {} → monitor {} at index {}",
            column.len(),
            source_monitor,
            target_monitor,
            insert_idx
        );

        // Insert into target workspace.
        if let Some(target_ws) = self.workspaces.get_mut(&target_monitor) {
            target_ws.insert_column_at(column, insert_idx);
            if let Err(e) = target_ws.focus_window(hwnd) {
                debug!("Failed to focus moved window after cross-monitor drag: {}", e);
            }
            target_ws.ensure_focused_visible_animated(target_viewport.width);
        }

        self.focused_monitor = target_monitor;

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after cross-monitor drag: {}", e);
        }
        self.sync_foreground_window();
    }

    /// Snap a tiled window back to its layout position with animation.
    fn snap_back_tiled(&mut self, monitor_id: MonitorId) {
        let snapshot = self.snapshot_layout();
        let viewport_width = self
            .monitors
            .get(&monitor_id)
            .map(|m| m.work_area.width)
            .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
        if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
            workspace.ensure_focused_visible_animated(viewport_width);
        }
        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to snap back layout after drag: {}", e);
        }
    }

    /// Apply focus to a window for focus-follows-mouse.
    /// Returns true if focus was applied, false if the window isn't managed.
    pub(crate) fn apply_focus_follows_mouse(&mut self, hwnd: u64) -> bool {
        if let Some(monitor_id) = self.find_window_workspace(hwnd) {
            // Update focused monitor to match the window's monitor
            self.focused_monitor = monitor_id;

            let viewport_width = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id) {
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

/// Screen-space boundary of a single column.
struct ColumnBound {
    column_index: usize,
    screen_left: i32,
    screen_right: i32,
}

/// Derive column screen boundaries from animated placements.
fn column_bounds_from_placements(
    workspace: &leopardwm_core_layout::Workspace,
    viewport: Rect,
) -> Vec<ColumnBound> {
    let placements = workspace.compute_placements_animated(viewport);
    // Group placements by column_index and compute left/right per column.
    let mut map: std::collections::HashMap<usize, (i32, i32)> = std::collections::HashMap::new();
    for p in &placements {
        let entry = map
            .entry(p.column_index)
            .or_insert((p.rect.x, p.rect.x + p.rect.width));
        entry.0 = entry.0.min(p.rect.x);
        entry.1 = entry.1.max(p.rect.x + p.rect.width);
    }
    let mut bounds: Vec<ColumnBound> = map
        .into_iter()
        .map(|(idx, (left, right))| ColumnBound {
            column_index: idx,
            screen_left: left,
            screen_right: right,
        })
        .collect();
    bounds.sort_by_key(|b| b.screen_left);
    bounds
}

/// Determine the insertion column index for a given screen X coordinate.
fn compute_insertion_index(bounds: &[ColumnBound], screen_x: i32) -> usize {
    if bounds.is_empty() {
        return 0;
    }
    for b in bounds {
        let midpoint = b.screen_left + (b.screen_right - b.screen_left) / 2;
        if screen_x < midpoint {
            return b.column_index;
        }
    }
    // Past the last column: insert at end.
    bounds
        .last()
        .map(|b| b.column_index + 1)
        .unwrap_or(0)
}

/// Compute the screen X coordinate for the vertical insertion indicator line.
fn compute_insertion_hint_x(bounds: &[ColumnBound], insert_index: usize, gap: i32) -> i32 {
    if bounds.is_empty() {
        return 0;
    }
    if insert_index == 0 {
        return bounds[0].screen_left - gap / 2;
    }
    // Find the bound right before the insertion point.
    let prev = bounds.iter().rev().find(|b| b.column_index < insert_index);
    let next = bounds.iter().find(|b| b.column_index >= insert_index);
    match (prev, next) {
        (Some(p), Some(n)) => (p.screen_right + n.screen_left) / 2,
        (Some(p), None) => p.screen_right + gap / 2,
        _ => bounds[0].screen_left - gap / 2,
    }
}

/// Find which column the cursor is directly over (not between columns).
fn compute_target_column_index(bounds: &[ColumnBound], screen_x: i32) -> Option<usize> {
    bounds
        .iter()
        .find(|b| screen_x >= b.screen_left && screen_x <= b.screen_right)
        .map(|b| b.column_index)
}

/// Compute the bounding rect of a column from its placements.
fn compute_column_rect(
    workspace: &leopardwm_core_layout::Workspace,
    viewport: Rect,
    column_index: usize,
) -> Option<Rect> {
    let placements = workspace.compute_placements_animated(viewport);
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_right = i32::MIN;
    let mut max_bottom = i32::MIN;
    let mut found = false;
    for p in &placements {
        if p.column_index == column_index && p.visibility == Visibility::Visible {
            found = true;
            min_x = min_x.min(p.rect.x);
            min_y = min_y.min(p.rect.y);
            max_right = max_right.max(p.rect.x + p.rect.width);
            max_bottom = max_bottom.max(p.rect.y + p.rect.height);
        }
    }
    if found {
        Some(Rect::new(min_x, min_y, max_right - min_x, max_bottom - min_y))
    } else {
        None
    }
}

/// Compute the vertical insertion slot by dividing the column rect into equal zones.
/// For N possible slots, the column height is split into N equal regions.
/// Returns 0..n_slots-1.
fn compute_window_slot(
    col_rect: &Rect,
    n_slots: usize,
    screen_y: i32,
) -> usize {
    if n_slots <= 1 {
        return 0;
    }
    let relative_y = (screen_y - col_rect.y).max(0);
    let zone_height = col_rect.height / n_slots as i32;
    if zone_height <= 0 {
        return 0;
    }
    let slot = (relative_y / zone_height) as usize;
    slot.min(n_slots - 1)
}
