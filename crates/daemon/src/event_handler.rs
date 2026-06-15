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
            | WindowEvent::MoveSizeEnd(id)
            | WindowEvent::TitleChanged(id) => Some(*id),
            WindowEvent::DisplayChange
            | WindowEvent::WorkAreaChanged
            | WindowEvent::MouseEnterWindow(_) => None,
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
            WindowEvent::Created(hwnd) => self.on_window_created(hwnd),
            WindowEvent::Destroyed(hwnd) => self.on_window_destroyed_or_hidden(hwnd, false),
            WindowEvent::Hidden(hwnd) => self.on_window_destroyed_or_hidden(hwnd, true),
            WindowEvent::Focused(hwnd) => self.on_window_focused(hwnd),
            WindowEvent::Minimized(hwnd) => self.on_window_minimized(hwnd),
            WindowEvent::Restored(hwnd) => self.on_window_restored(hwnd),
            WindowEvent::MoveSizeStart(hwnd) => self.on_move_size_start(hwnd),
            WindowEvent::MoveSizeEnd(hwnd) => self.on_move_size_end(hwnd),
            WindowEvent::MovedOrResized(hwnd) => self.on_window_moved_or_resized(hwnd),
            WindowEvent::DisplayChange => self.on_display_change(),
            // Work-area changes reach the reconcile via the debounced
            // DisplayChangeSettled path (see process_window_event), so a raw
            // event here is a no-op; reconcile defensively if one arrives.
            WindowEvent::WorkAreaChanged => self.on_display_change(),
            WindowEvent::MouseEnterWindow(_hwnd) => {
                // This is handled by the main event loop with debouncing
                // (focus_follows_mouse delay)
            }
            WindowEvent::TitleChanged(hwnd) => {
                // Only refresh the tab strip when the title change is
                // for a window that's a tab in the focused workspace's
                // visible Tabbed column — every other title change
                // (e.g. a background app's notification badge) would
                // waste a render. `update_tab_strip` already rebuilds
                // labels from `lookup_window_info`, so we don't have
                // to thread the new title through ourselves.
                let in_visible_tabbed_column = self
                    .focused_workspace()
                    .map(|ws| {
                        ws.columns()
                            .iter()
                            .any(|c| c.is_tabbed() && c.contains(hwnd))
                    })
                    .unwrap_or(false);
                if in_visible_tabbed_column {
                    self.update_tab_strip();
                }
            }
        }
    }

    /// Handle a window-created event: rules, monitor/workspace placement, insertion.
    fn on_window_created(&mut self, hwnd: u64) {
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
            // Skip shell-cloaked windows (suspended UWP frames, windows
            // on other virtual desktops). These are valid HWNDs with
            // WS_VISIBLE but no rendered content.
            #[cfg(not(test))]
            if leopardwm_platform_win32::is_window_shell_cloaked(hwnd) {
                debug!(
                    "Ignoring shell-cloaked window: {} ({})",
                    win_info.title, win_info.class_name
                );
                return;
            }

            // Get executable name for rule matching
            let executable =
                get_process_executable(win_info.process_id).unwrap_or_default();

            // Skip transient script-runner windows whose title is just
            // the executable path. PowerShell, cmd, and similar console
            // hosts briefly show this title before they finish setting
            // a real one — but a scheduled-task spawn (like the Windows
            // PowerShell that fires every 5 minutes) is destroyed
            // within ~200 ms before it ever gets a real title. Tiling
            // those caused a layout reflow on Created and another on
            // Hidden, which the user perceived as "windows randomly
            // resizing while idle". A persistent interactive console
            // sets a real title (e.g. "Administrator: Windows
            // PowerShell") almost immediately, so this filter does not
            // affect normal terminal usage.
            let title_lower = win_info.title.to_ascii_lowercase();
            let title_looks_like_exe_path = title_lower.ends_with(".exe")
                || (!executable.is_empty()
                    && title_lower == executable.to_ascii_lowercase());
            if title_looks_like_exe_path
                && win_info.class_name == "ConsoleWindowClass"
            {
                debug!(
                    "Skipping transient console-host window with exe-path title: {} ({})",
                    win_info.title, win_info.class_name
                );
                return;
            }

            // Check window rules
            let action = self.evaluate_window_rules(
                &win_info.class_name,
                &win_info.title,
                &executable,
            );
            // Per-app open extras from the same (first-match) rule.
            let (rule_workspace, rule_maximized, rule_column_width) = self
                .matched_rule(&win_info.class_name, &win_info.title, &executable)
                .map(|r| (r.open_on_workspace, r.open_maximized, r.column_width))
                .unwrap_or((None, false, None));

            // Skip ignored windows
            if action == config::WindowAction::Ignore {
                debug!(
                    "Ignoring window by rule: {} ({})",
                    win_info.title, win_info.class_name
                );
                return;
            }

            // New windows open on the focused monitor — the active monitor
            // follows the focused window (see on_window_focused), so a new
            // window lands where the user is working rather than wherever
            // the app happened to spawn it (which often defaults to another
            // monitor). A per-app rule's open_on_workspace can still
            // redirect it below.
            let monitor_id = self.focused_monitor;

            // Get floating rect before borrowing workspace mutably
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

            let viewport_width = self.viewport_width_for(monitor_id);

            // A rule can target a different workspace; the window then
            // opens in the background (no focus steal, hidden until
            // that workspace is activated).
            let active_idx = self.active_workspace_idx(monitor_id);
            let target_idx = rule_workspace.unwrap_or(active_idx);
            let opens_in_background = target_idx != active_idx;
            if opens_in_background {
                self.ensure_workspace_exists(monitor_id, target_idx);
            }

            // Snapshot before structural change for tiled window
            // animation. A background open doesn't change the active
            // layout, so no transition is needed.
            let snapshot = if action == config::WindowAction::Tile && !opens_in_background
            {
                Some(self.snapshot_layout())
            } else {
                None
            };

            // Per-app initial column width (viewport fraction -> px).
            let rule_width_px = rule_column_width
                .map(|f| ((f * f64::from(viewport_width)).round() as i32).max(100));

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(target_idx)) {
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
                        let in_column = self.config.behavior.new_window_placement
                            == config::NewWindowPlacement::InColumn
                            && workspace.column_count() > 0;
                        let ok = if in_column {
                            // Stack into the focused column, directly
                            // below the focused window (matches
                            // hyprscroller's column mode rather than
                            // appending at the bottom of the stack).
                            let col = workspace.focused_column_index();
                            let row = workspace.focused_window_index_in_column() + 1;
                            let ok = workspace
                                .insert_window_in_column_at(hwnd, col, row)
                                .is_ok();
                            if ok && self.config.behavior.focus_new_windows {
                                if let Err(e) = workspace.focus_window(hwnd) {
                                    warn!(
                                        "Focusing new in-column window {} failed: {:?}",
                                        hwnd, e
                                    );
                                }
                            }
                            ok
                        } else if self.config.behavior.focus_new_windows
                            || opens_in_background
                        {
                            // A background open still takes the target
                            // workspace's local focus (so it's focused
                            // when that workspace is activated); OS
                            // focus is never touched for it.
                            workspace.insert_window(hwnd, rule_width_px).is_ok()
                        } else {
                            workspace
                                .insert_window_no_focus(hwnd, rule_width_px)
                                .is_ok()
                        };
                        // Per-app open_maximized: only when the new
                        // window's column is the focused one (always
                        // true for the focused new-column path).
                        if ok
                            && rule_maximized
                            && workspace.focused_window() == Some(hwnd)
                        {
                            workspace.maximize_focused_column(viewport_width);
                        }
                        ok
                    }
                    config::WindowAction::Ignore => unreachable!(),
                };

                if added {
                    self.window_managed_at.insert(hwnd, std::time::Instant::now());
                    info!(
                        "Window created: {} ({}) - added to monitor {} workspace {} as {:?}",
                        win_info.title,
                        win_info.class_name,
                        monitor_id,
                        target_idx + 1,
                        action
                    );
                    if self.config.behavior.focus_new_windows && !opens_in_background {
                        self.focused_monitor = monitor_id;
                        if matches!(action, config::WindowAction::Float) {
                            self.previous_focused_hwnd = Some(hwnd);
                        }
                        workspace.ensure_focused_visible_animated(viewport_width);
                    }
                    if opens_in_background {
                        // Target workspace is not active: hide the
                        // window until that workspace is switched to.
                        let _ = leopardwm_platform_win32::move_window_offscreen(hwnd);
                    }
                    if let Some(snapshot) = snapshot {
                        self.start_layout_transition(snapshot);
                    }
                    // Disable snap layouts for tiled windows (after workspace borrow)
                    if matches!(action, config::WindowAction::Tile) {
                        self.disable_snap_for_window(hwnd);
                    }
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to apply layout after window create: {}", e);
                    }
                    if self.config.behavior.focus_new_windows && !opens_in_background {
                        self.sync_foreground_window();
                    }
                } else {
                    debug!("Failed to add window {} to workspace", hwnd);
                }
            }
        }
    }

    /// Shared handler for destroyed and hidden window events.
    fn on_window_destroyed_or_hidden(&mut self, hwnd: u64, is_hidden_event: bool) {
        let event_name = if is_hidden_event { "hidden" } else { "destroyed" };

        // A stashed scratchpad window lives outside all workspaces and
        // is cloaked, so only a real destroy (not a spurious Hidden
        // from cloaking) should clear its designation.
        if !is_hidden_event {
            self.scratchpad_on_window_destroyed(hwnd);
            self.sticky_on_window_destroyed(hwnd);
            // Forget any remembered floating focus for this window so
            // a recycled HWND can't wrongly re-focus on workspace return.
            self.floating_focus.retain(|_, &mut h| h != hwnd);
            // Drop the cached window icon: the HICON dies with its
            // window, and a recycled HWND must re-probe.
            self.overview_icon_cache.remove(&hwnd);
        }

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

        // Drop the recorded layout rect so the map doesn't retain
        // entries for windows that no longer exist.
        self.last_placed_layout_rects.remove(&hwnd);

        // Drop any cached overview snapshot for the same reason.
        leopardwm_platform_win32::snapshot::snapshot_remove(hwnd);

        // Drop any tab title override too — both Destroyed and
        // Hidden imply the window is no longer in any tabbed
        // column. Without this, hidden-but-not-destroyed apps
        // (minimize-to-tray patterns) would accumulate stale
        // overrides indefinitely in the persisted state.
        self.tab_title_overrides.remove(&hwnd);

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

        // Clear stale focus reference
        if self.previous_focused_hwnd == Some(hwnd) {
            self.previous_focused_hwnd = None;
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
        }

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
            // Restore WS_MAXIMIZEBOX (no-op if not tracked)
            self.restore_snap_for_window(hwnd);

            if was_tiled {
                self.start_layout_transition(snapshot);
            }
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after window {}: {}", event_name, e);
            }
        }
    }

    /// Handle a foreground-focus change event.
    fn on_window_focused(&mut self, hwnd: u64) {
        // Skip if this window is already our tracked focus — avoids
        // feedback loops where sync_foreground_window triggers another
        // EVENT_SYSTEM_FOREGROUND for the same window.
        if self.previous_focused_hwnd == Some(hwnd) {
            return;
        }

        // Suppress rapid same-column focus switches caused by mouse wheel
        // scrolling near the boundary between stacked windows. Windows'
        // "scroll inactive windows" feature can cause the foreground to
        // ping-pong between adjacent windows during rapid scrolling.
        //
        // Exception: a tab-strip click or `Ctrl+Alt+J/K` cycle in a
        // Tabbed column synthesizes a deliberate same-column focus
        // change. The command handler sets `pending_tab_focus`
        // before triggering it; we consume that flag here so the
        // expected event flows through.
        let now = std::time::Instant::now();
        if let Some(prev_hwnd) = self.previous_focused_hwnd {
            if let Some(last_change) = self.last_focus_change_at {
                if now.duration_since(last_change).as_millis() < 200 {
                    // Check if both windows are in the same column
                    if let Some((mon_a, ws_a)) = self.find_window_workspace(prev_hwnd) {
                        if let Some((mon_b, ws_b)) = self.find_window_workspace(hwnd) {
                            if mon_a == mon_b && ws_a == ws_b {
                                let same_col = self.workspaces.get(&mon_a)
                                    .and_then(|v| v.get(ws_a))
                                    .is_some_and(|ws| {
                                        let loc_a = ws.find_window_location(prev_hwnd);
                                        let loc_b = ws.find_window_location(hwnd);
                                        matches!((loc_a, loc_b), (Some((ca, _)), Some((cb, _))) if ca == cb)
                                    });
                                if same_col {
                                    // Check for a fresh tab-focus intent that
                                    // matches this event. If it does, consume the
                                    // flag and fall through (the focus change is
                                    // expected, not noisy churn).
                                    let consumed = self
                                        .consume_pending_tab_focus_for(mon_a, ws_a, hwnd);
                                    if !consumed {
                                        debug!(
                                            "Suppressed rapid same-column focus switch: {} -> {}",
                                            prev_hwnd, hwnd
                                        );
                                        return;
                                    }
                                    debug!(
                                        "Same-column focus switch allowed (tab intent): {} -> {}",
                                        prev_hwnd, hwnd
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Reconcile: prune windows that vanished without events
        // (e.g., Electron close-to-tray apps).
        // Throttle to at most once per second to avoid per-event overhead.
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

                // Clean up any in-progress drag: reinsert window if it was
                // removed from source during live preview, then remove placeholders.
                // Only reinsert if the window still exists (it may have been closed).
                if let Some(drag) = self.drag_state.take() {
                    if drag.removed_from_source && drag.is_tiled
                        && leopardwm_platform_win32::is_valid_window(drag.hwnd)
                    {
                        if let Some(ws) = self.workspaces.get_mut(&drag.source_monitor)
                            .and_then(|v| v.get_mut(drag.source_workspace_idx))
                        {
                            let _ = ws.insert_window(drag.hwnd, None);
                        }
                    }
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
                self.abort_active_ghost_transition();
                self.layout_transition = None;

                let slide_height = self.monitors.get(&monitor_id)
                    .map(|m| m.work_area.height)
                    .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
                let y_offset = if ws_idx > active_idx { slide_height } else { -slide_height };

                let viewport = self.layout_viewport(monitor_id);

                // Snapshot old workspace positions for exit animation.
                let old_placements: Vec<(u64, leopardwm_core_layout::Rect)> =
                    self.workspaces.get(&monitor_id)
                        .and_then(|v| v.get(active_idx))
                        .map(|ws| {
                            ws.compute_placements_animated(viewport)
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
                        .map(|ws| {
                            ws.compute_placements_animated(viewport)
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
                    let duration =
                        self.config.animation.workspace_switch_duration_ms;
                    self.start_workspace_switch_transition(
                        start_rects,
                        exit_rects,
                        duration,
                    );
                }
            }

            let viewport_width = self.viewport_width_for(monitor_id);

            // Distinguish user-initiated focus changes (clicks /
            // hotkeys / Alt-Tab) from spurious foreground events
            // fired by background apps. Without recent user input
            // we still update internal focus tracking but skip the
            // auto-scroll that would yank the viewport to a window
            // the user did not actually request — the classic
            // "I was on Terminal and Zen suddenly stole focus and
            // scrolled the layout" symptom.
            //
            // Threshold is generous (1.5 s) because a Focused event
            // delivered through WinEventProc -> our hook -> tokio
            // mpsc -> daemon mutex can lag well past 500 ms when the
            // daemon is busy or DWM is loaded. Spurious events from
            // notification toasts and tray apps fire from timers
            // unrelated to user input, so even at 1.5 s the false-
            // positive rate stays low. Fail CLOSED on
            // `GetLastInputInfo` failure — if the API ever returns
            // None we cannot prove user intent, so we don't auto-
            // scroll. The user can still hotkey the focus shift,
            // which goes through `command_handler` and bypasses
            // this gate entirely.
            const FOCUS_INPUT_RECENT_MS: u32 = 1500;
            let user_initiated = leopardwm_platform_win32::ms_since_last_user_input()
                .map(|ms| ms <= FOCUS_INPUT_RECENT_MS)
                .unwrap_or(false);
            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                if let Err(e) = workspace.focus_window(hwnd) {
                    // Floating windows are not in the tiled column list,
                    // so focus_window fails for them — that's expected.
                    debug!("Failed to focus window {}: {}", hwnd, e);
                } else {
                    debug!(
                        "Focus changed to window {} on monitor {} (user_initiated={})",
                        hwnd, monitor_id, user_initiated
                    );
                    if user_initiated {
                        workspace.ensure_focused_visible_animated(viewport_width);
                    }
                }
            }
            // Always apply layout — even if focus_window failed (floating windows),
            // we still need to repaint if we just switched workspaces.
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after focus change: {}", e);
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
            self.last_focus_change_at = Some(now);
            self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
        } else {
            self.on_unmanaged_window_focused(hwnd);
        }
    }

    /// Recovery and cleanup when focus lands on an unmanaged window.
    fn on_unmanaged_window_focused(&mut self, hwnd: u64) {
        // Recovery path: if a user focuses a window that was
        // suppressed by recently_hidden_hwnds (e.g., tray-restored
        // app), re-add it now. A user focusing a window proves it's
        // not a transient popup.
        //
        // Peek first, remove only on commit. If lookup_window_info
        // transiently fails or the rule says Ignore, leaving the
        // entry intact lets a subsequent Focused event retry the
        // recovery (or the TTL filter at the top of this handler
        // ages it out).
        if self.recently_hidden_hwnds.contains_key(&hwnd) {
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
                    // Consume the entry now (immediately before
                    // dispatch) so the Created handler doesn't
                    // re-suppress on this same recovery path.
                    self.recently_hidden_hwnds.remove(&hwnd);
                    self.handle_window_event(WindowEvent::Created(hwnd));
                    // Update tiled focus to match OS — the user just
                    // focused this window. focus_window may fail for
                    // floating windows, which is fine.
                    let recovery_monitor =
                        if let Some((mid, widx)) = self.find_window_workspace(hwnd) {
                            if let Some(ws) = self.workspaces.get_mut(&mid).and_then(|v| v.get_mut(widx)) {
                                let _ = ws.focus_window(hwnd);
                            }
                            mid
                        } else {
                            self.focused_monitor
                        };
                    self.previous_focused_hwnd = Some(hwnd);
                    self.show_border(hwnd);
                    self.broadcast_focused_window_if_changed(
                        recovery_monitor as i64,
                        Some(hwnd),
                    );
                    return;
                }
            }
        }

        // Recovery path for the transient-console-host filter:
        // a real interactive PowerShell or cmd window may have
        // hit the filter at Created time if its title was still
        // the exe path. By the time the user actually focuses
        // it the title has been set (e.g. "Administrator:
        // Windows PowerShell"), so re-check and re-add. A user
        // focusing the window proves it is not a transient
        // scheduled-task spawn.
        if let Some(win_info) = self.lookup_window_info(hwnd) {
            if win_info.class_name == "ConsoleWindowClass" {
                let executable = get_process_executable(win_info.process_id)
                    .unwrap_or_default();
                let title_lower = win_info.title.to_ascii_lowercase();
                let title_still_exe_path = title_lower.ends_with(".exe")
                    || (!executable.is_empty()
                        && title_lower == executable.to_ascii_lowercase());
                if !title_still_exe_path {
                    let action = self.evaluate_window_rules(
                        &win_info.class_name,
                        &win_info.title,
                        &executable,
                    );
                    if action != config::WindowAction::Ignore {
                        info!(
                            "Recovering console-host window with real title: {} ({}) - user focused it",
                            win_info.title, win_info.class_name
                        );
                        self.handle_window_event(WindowEvent::Created(hwnd));
                        let recovery_monitor =
                            if let Some((mid, widx)) = self.find_window_workspace(hwnd) {
                                if let Some(ws) = self.workspaces.get_mut(&mid).and_then(|v| v.get_mut(widx)) {
                                    let _ = ws.focus_window(hwnd);
                                }
                                mid
                            } else {
                                self.focused_monitor
                            };
                        self.previous_focused_hwnd = Some(hwnd);
                        self.show_border(hwnd);
                        self.broadcast_focused_window_if_changed(
                            recovery_monitor as i64,
                            Some(hwnd),
                        );
                        return;
                    }
                }
            }
        }

        // Focus went to an unmanaged window (e.g. settings, taskbar).
        // Hide the border overlay and clear tracked hwnd so animation
        // frames don't re-show it.
        self.hide_border();
        self.previous_focused_hwnd = None;
        let monitor_id = self.focused_monitor as i64;
        self.broadcast_focused_window_if_changed(monitor_id, None);
    }

    /// Handle a window-minimized event.
    fn on_window_minimized(&mut self, hwnd: u64) {
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            let viewport_width = self.viewport_width_for(monitor_id);
            let layout_viewport = self.layout_viewport(monitor_id);
            let snapshot = self.snapshot_layout();

            // If the minimized window is a floating window tracked as
            // previous_focused_hwnd, clear it so sync_foreground_window
            // doesn't try to re-focus a minimized floating window.
            let is_floating = self.workspaces.get(&monitor_id)
                .and_then(|v| v.get(ws_idx))
                .is_some_and(|ws| ws.is_floating(hwnd));
            if is_floating && self.previous_focused_hwnd == Some(hwnd) {
                self.previous_focused_hwnd = None;
            }

            if let Some(workspace) = self.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(ws_idx)) {
                let cleared_fullscreen = workspace.clear_fullscreen_if_window(hwnd);
                // mark_minimized only handles tiled windows; floating windows
                // are not in the minimized set. Handle both paths.
                if workspace.mark_minimized(hwnd) || cleared_fullscreen || is_floating {
                    let col_loc = workspace.find_window_location(hwnd);
                    let col_info = col_loc.map(|(ci, _)| {
                        let col = &workspace.columns()[ci];
                        let visible = col.windows().iter()
                            .filter(|w| !workspace.is_minimized(**w))
                            .count();
                        (ci, col.len(), visible)
                    });
                    info!(
                        "Window {} minimized (col={:?}, minimized_total={})",
                        hwnd, col_info, workspace.minimized_count()
                    );

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

                    // Log expected post-minimize placements for debugging
                    {
                        let post_placements = workspace.compute_placements(layout_viewport);
                        for p in &post_placements {
                            info!(
                                "  post-minimize placement: hwnd={} rect=({},{} {}x{})",
                                p.window_id, p.rect.x, p.rect.y,
                                p.rect.width, p.rect.height,
                            );
                        }
                    }

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

    /// Handle a window-restored event.
    fn on_window_restored(&mut self, hwnd: u64) {
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
            // The daemon's startup enumeration skips IsIconic windows, so
            // tray apps that boot in a minimized state (Raw Accel, Discord
            // close-to-tray, Spotify minimized) never enter the managed set
            // until they are restored. Treating an unmanaged restore as a
            // Created event lets the standard rule/tile pipeline pick them
            // up the first time the user actually brings them on screen.
            debug!(
                "Window {} restored (unmanaged) — re-dispatching as Created",
                hwnd
            );
            self.handle_window_event(WindowEvent::Created(hwnd));
        }
    }

    /// Handle the start of a user drag or resize.
    fn on_move_size_start(&mut self, hwnd: u64) {
        debug!("User started dragging/resizing window {}", hwnd);

        // Distinguish resize (border drag) from move (title bar drag).
        // Only create drag state for moves — resizes should not trigger
        // the drag-and-drop overlay.
        if leopardwm_platform_win32::is_cursor_on_resize_border(hwnd) {
            debug!("Detected resize (not move) for window {}, tracking", hwnd);
            self.resize_hwnd = Some(hwnd);
            return;
        }

        let (is_tiled, source_monitor, source_ws_idx, col_idx) =
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
                (!is_floating, monitor_id, ws_idx, col_idx)
            } else {
                (false, self.focused_monitor, self.active_workspace_idx(self.focused_monitor), 0)
            };
        self.drag_state = Some(DragState {
            hwnd,
            is_tiled,
            source_monitor,
            source_workspace_idx: source_ws_idx,
            current_column_index: col_idx,
            last_drop_target: None,
            last_hint_update: None,
            removed_from_source: false,
        });
        // Disable DWM-managed position interpolation on the
        // dragged window so its final SetWindowPos on drop
        // lands instantly. Without this, DWM smooths the
        // transition between the drop point and the layout
        // slot — the user perceives this as the column
        // "sliding" into place when dropping into a Tabbed
        // target where no layout-transition animation is
        // running.
        leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, true);
    }

    /// Handle the end of a user drag or resize.
    fn on_move_size_end(&mut self, hwnd: u64) {
        debug!("User finished dragging/resizing window {}", hwnd);

        // The dragged/resized window has physically drifted from its
        // layout slot. Evict its last_placed entry so apply_layout's
        // fast-path can't short-circuit on no-layout-change drop
        // paths (small in-column drag → snap_back_tiled, single-
        // window same-column merge, resize that lands within the
        // existing preset bucket). Without this the window is left
        // wherever the user released it until something else
        // triggers a real layout change.
        self.last_placed_layout_rects.remove(&hwnd);

        // Handle resize completion (border drag) — snap to nearest preset.
        if self.resize_hwnd.take() == Some(hwnd) {
            self.handle_resize_complete(hwnd);
            // Re-enable DWM transitions before returning (paired
            // with MoveSizeStart's disable). Each early-return path
            // needs this — otherwise the window's transitions stay
            // suppressed for the rest of its lifetime.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        // Verify this MoveSizeEnd matches the active drag — a mismatched
        // event for a different window should not tear down the drag state.
        if self.drag_state.as_ref().is_some_and(|d| d.hwnd != hwnd) {
            debug!("Ignoring MoveSizeEnd for {} — drag active for different window", hwnd);
            // Mismatched event — re-enable transitions on the hwnd
            // we just got the event for; the original drag's hwnd
            // will re-enable on its own MoveSizeEnd.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        let drag = self.drag_state.take();
        // Always hide the drag hint overlay on drop.
        self.pending_drag_hint = Some(DragHintAction::Hide);

        let Some(drag) = drag else {
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        };

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
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        // Tiled window: determine final drop target.
        let Some(win_info) = self.lookup_window_info(hwnd) else {
            // Window vanished during drag — clean up placeholder.
            // Do NOT reinsert the window — it no longer exists.
            for (_, ws_vec) in self.workspaces.iter_mut() {
                for ws in ws_vec.iter_mut() {
                    let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                }
            }
            self.snap_back_tiled(drag.source_monitor, drag.source_workspace_idx);
            // No-op if hwnd is truly destroyed, but cheap and
            // covers the edge case where lookup returns None
            // transiently while the window still exists.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
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
                self.snap_back_tiled(drag.source_monitor, drag.source_workspace_idx);
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
        // Re-enable DWM transitions on the dropped window now
        // that the final SetWindowPos has already landed. We
        // disable them at MoveSizeStart specifically to suppress
        // the drop-position-to-layout-slot slide; once the
        // window is settled there's no reason to keep its
        // minimize/maximize/etc. transitions suppressed.
        leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
    }

    /// Handle a window move/resize notification.
    fn on_window_moved_or_resized(&mut self, hwnd: u64) {
        // Skip events triggered by our own apply_layout() to avoid feedback loop.
        // Also suppress during display change debounce — Windows resizes windows
        // during contrast theme transitions and the stale border metrics would
        // cause incorrect snap-back sizes.
        if self.applying_layout
            || self.display_change_pending
            || self.should_suppress_moved_or_resized(hwnd)
        {
            return;
        }
        // During active border resize: show ghost preview of the snap target
        // for tiled windows, or update border for floating windows.
        if self.resize_hwnd == Some(hwnd) {
            let is_floating = self.find_window_workspace(hwnd)
                .and_then(|(mid, wsi)| self.workspaces.get(&mid)?.get(wsi).map(|ws| ws.is_floating(hwnd)))
                .unwrap_or(false);
            if is_floating {
                // Throttle floating border updates to ~60fps
                let now = std::time::Instant::now();
                if self
                    .last_resize_hint_update
                    .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                {
                    return;
                }
                self.last_resize_hint_update = Some(now);
                self.show_border(hwnd);
            } else if self.config.snap_hints.enabled {
                // Throttle preview updates to ~60fps
                let now = std::time::Instant::now();
                if self
                    .last_resize_hint_update
                    .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                {
                    return;
                }
                self.last_resize_hint_update = Some(now);
                self.update_resize_preview(hwnd);
            }
            return;
        }
        // During drag: compute drop target and show snap hint for tiled windows.
        // For floating drags, update the border to follow the window.
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
                } else {
                    // Floating window drag — throttle border updates to ~60fps
                    let now = std::time::Instant::now();
                    if drag
                        .last_hint_update
                        .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                    {
                        return;
                    }
                    drag.last_hint_update = Some(now);
                    self.show_border(hwnd);
                }
                return;
            }
        }
        // Non-drag: if the window is managed (tiled), snap it back to its layout position.
        // For floating windows, update the border to track position changes.
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            let is_floating = self
                .workspaces
                .get(&monitor_id)
                .and_then(|v| v.get(ws_idx))
                .is_none_or(|ws| ws.is_floating(hwnd));

            if is_floating {
                if self.previous_focused_hwnd == Some(hwnd) {
                    self.show_border(hwnd);
                }
            } else if leopardwm_platform_win32::is_window_maximized(hwnd) {
                // User maximized a tiled window — let it stay maximized.
                debug!("Tiled window {} maximized — allowing", hwnd);
            } else {
                // Position-based false-positive filter: EVENT_OBJECT_LOCATIONCHANGE
                // fires for many reasons besides actual movement (Z-order,
                // DWM composition, focus shuffles, DPI nudges, app-internal
                // size adjustments). Under CPU pressure these spurious
                // events trigger cascading full retiles. If the window's
                // current visible bounds are close to the last-placed
                // layout rect, skip the snap-back.
                //
                // Epsilon is generous (20px) because some apps report
                // their own content rect rather than the requested frame
                // rect — DPI rounding, custom chrome, internal min-sizes
                // all create small legitimate deltas we don't want to
                // chase. Real user drags are typically tens to hundreds
                // of pixels off, so 20px comfortably separates them.
                const POSITION_EPSILON_PX: i32 = 20;
                let expected = self.last_placed_layout_rects.get(&hwnd).copied();
                let dwm_actual = leopardwm_platform_win32::get_window_visible_rect(hwnd);
                // Cross-check with GetWindowRect — for Chromium /
                // Firefox / Cascadia under the swap-chain-stale bug,
                // EXTENDED_FRAME_BOUNDS reports the visual content
                // position (where DWM is compositing) rather than the
                // actual chrome HWND position, which can read tens to
                // thousands of pixels off after a rapid burst even
                // though the window has not moved. GetWindowRect is
                // the OS's authoritative position and stays correct.
                //
                // The chrome rect is offset from the layout rect by
                // the invisible-border insets (apply_placements does
                // SetWindowPos at `rect.x - inset_l`), so we subtract
                // the insets before comparing. That makes the chrome
                // comparison apples-to-apples against the layout rect
                // and lets us use the same tight POSITION_EPSILON_PX.
                // Without this, real displacements in the
                // 21..(20+inset_l*2) px band were misclassified as
                // swap-chain artifacts and the snap-back was skipped.
                let chrome_actual = leopardwm_platform_win32::get_window_chrome_rect(hwnd);
                let chrome_visible = chrome_actual.map(|c| {
                    let (il, it, _, _) =
                        leopardwm_platform_win32::get_window_invisible_insets(hwnd);
                    Rect::new(c.x + il, c.y + it, c.width, c.height)
                });
                let within_all = |a: Rect, e: Rect, eps: i32| -> bool {
                    (a.x - e.x).abs() <= eps
                        && (a.y - e.y).abs() <= eps
                        && (a.width - e.width).abs() <= eps
                        && (a.height - e.height).abs() <= eps
                };
                let at_expected_position = match expected {
                    Some(expected) => {
                        // Honest comparison — DWM bounds match
                        // expected layout in both position and size.
                        let dwm_ok = dwm_actual
                            .is_some_and(|a| within_all(a, expected, POSITION_EPSILON_PX));
                        // Swap-chain bug guard — chrome HWND
                        // (visible-area-corrected) is at the
                        // expected position even though DWM is
                        // lying. Position only: the chrome rect's
                        // size is inflated by invisible borders
                        // and we don't trivially correct that, so
                        // a size comparison would mask real edge
                        // resizes.
                        let chrome_position_ok = chrome_visible.is_some_and(|a| {
                            (a.x - expected.x).abs() <= POSITION_EPSILON_PX
                                && (a.y - expected.y).abs() <= POSITION_EPSILON_PX
                        });
                        let dwm_position_displaced = dwm_actual.is_some_and(|a| {
                            (a.x - expected.x).abs() > POSITION_EPSILON_PX
                                || (a.y - expected.y).abs() > POSITION_EPSILON_PX
                        });
                        let swap_chain_bug =
                            chrome_position_ok && dwm_position_displaced;
                        let result = dwm_ok || swap_chain_bug;
                        if !result {
                            debug!(
                                "Window {} off expected position: expected {:?} dwm {:?} chrome_visible {:?}",
                                hwnd, expected, dwm_actual, chrome_visible
                            );
                        }
                        result
                    }
                    None => false,
                };
                if at_expected_position {
                    debug!(
                        "Ignoring spurious MovedOrResized for {} — already at expected layout position",
                        hwnd
                    );
                } else {
                    debug!("Managed window {} moved/resized — snapping back", hwnd);
                    // Evict the displaced hwnd's last-applied entry so
                    // apply_layout's fast-path can't short-circuit when
                    // the layout itself hasn't changed but the window's
                    // visible rect has drifted away from it. Without
                    // this the window stays where the user dragged it.
                    self.last_placed_layout_rects.remove(&hwnd);
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to snap back layout after move/resize: {}", e);
                    }
                }
            }
        }
    }

    /// Handle a display configuration change.
    fn on_display_change(&mut self) {
        // Display configuration changed (monitors added/removed/rearranged).
        // Note: inset cache clearing and high contrast refresh happen
        // immediately on WM_DISPLAYCHANGE receipt (before debounce) in the
        // event loop. This handler runs after the debounce settles.
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

    /// Compute and show the resize preview ghost overlay during active border resize.
    /// When the snap target changes, requests a vsync-aligned animation thread
    /// for smooth interpolation between snap positions.
    fn update_resize_preview(&mut self, hwnd: u64) {
        let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) else {
            return;
        };
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return;
        };
        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|v| v.get(ws_idx))
            .is_none_or(|ws| ws.is_floating(hwnd));
        if is_floating {
            return;
        }

        let work_area = match self.monitors.get(&monitor_id) {
            Some(m) => m.work_area,
            None => return,
        };
        let width_presets = self.config.layout.width_presets.clone();
        let height_presets = self.config.layout.height_presets.clone();

        let snap_rect = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
            .and_then(|ws| {
                ws.preview_resize_snap(
                    hwnd,
                    visible_rect.width,
                    visible_rect.height,
                    &width_presets,
                    &height_presets,
                    work_area,
                )
            });

        let Some(target_rect) = snap_rect else {
            return;
        };

        if self.resize_preview_target == Some(target_rect) {
            // Target unchanged — if animation thread is driving the overlay, let it.
            if !self
                .resize_animation_active
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.pending_drag_hint =
                    Some(crate::state::DragHintAction::ShowGhost { rect: target_rect });
            }
            self.resize_preview_display_rect = Some(target_rect);
            self.show_border(hwnd);
            return;
        }

        // Snap target changed — request a vsync-aligned animation.
        let start_rect = self
            .resize_preview_display_rect
            .unwrap_or(target_rect);
        self.resize_preview_target = Some(target_rect);
        self.resize_preview_display_rect = Some(start_rect);
        self.pending_resize_animation = Some(crate::state::ResizeAnimationRequest {
            start_rect,
            target_rect,
        });

        // Show overlay at current position immediately (animation will take over).
        self.pending_drag_hint =
            Some(crate::state::DragHintAction::ShowGhost { rect: start_rect });
        self.show_border(hwnd);
    }

    /// Handle resize completion: snap the resized window's column width and height
    /// to the nearest presets, then re-apply layout.
    fn handle_resize_complete(&mut self, hwnd: u64) {
        // Hide the resize preview overlay and clear all preview state.
        self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        self.resize_preview_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.resize_preview_target = None;
        self.resize_preview_display_rect = None;
        self.pending_resize_animation = None;
        self.last_resize_hint_update = None;
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            let _ = self.apply_layout();
            return;
        };

        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|v| v.get(ws_idx))
            .is_none_or(|ws| ws.is_floating(hwnd));

        if is_floating {
            // Floating: just update stored rect from the visible area.
            if let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&monitor_id)
                    .and_then(|v| v.get_mut(ws_idx))
                {
                    ws.update_floating(hwnd, visible_rect);
                }
            }
            return;
        }

        // Tiled: snap to width/height presets.
        let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) else {
            let _ = self.apply_layout();
            return;
        };

        let viewport_width = self.viewport_width_for(monitor_id);
        let width_presets = self.config.layout.width_presets.clone();
        let height_presets = self.config.layout.height_presets.clone();

        if let Some(ws) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
        {
            if let Some((col_idx, win_idx)) = ws.find_window_location(hwnd) {
                // Snap width to nearest preset
                ws.snap_column_width_to_preset(
                    col_idx,
                    visible_rect.width,
                    &width_presets,
                    viewport_width,
                );

                // Snap height to nearest preset (multi-window columns only)
                let col_len = ws.columns().get(col_idx).map(|c| c.len()).unwrap_or(0);
                if col_len > 1 {
                    let viewport_height = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.height)
                        .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
                    ws.snap_window_height_to_preset(
                        col_idx,
                        win_idx,
                        visible_rect.height,
                        &height_presets,
                        viewport_height,
                    );
                }

                info!(
                    "Resize snap: window {} → width preset, new column width = {}",
                    hwnd,
                    ws.columns()
                        .get(col_idx)
                        .map(|c| c.width())
                        .unwrap_or(0)
                );
            }
        }

        // Evict the resized hwnd from last_placed_layout_rects: when the
        // user's resize falls inside the current preset bucket the snap is
        // a no-op, so apply_layout's fast-path would see placements
        // unchanged and skip repositioning, leaving the window at the
        // user-resized size instead of the column's preset width.
        self.last_placed_layout_rects.remove(&hwnd);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after resize snap: {}", e);
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
                    // Skip the real Win32 call in tests — placeholder hwnds collide
                    // with real running windows and lag the user's mouse / steal
                    // focus via AttachThreadInput.
                    #[cfg(not(test))]
                    let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
                    debug!(
                        "Focus-follows-mouse: focused floating window {} on monitor {}",
                        hwnd, monitor_id
                    );
                    // Pre-setting previous_focused_hwnd above would otherwise
                    // make the OS-side EVENT_SYSTEM_FOREGROUND dedup at the
                    // top of WindowEvent::Focused early-return, swallowing
                    // the broadcast. Route through the helper directly.
                    self.broadcast_focused_window_if_changed(
                        monitor_id as i64,
                        Some(hwnd),
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
