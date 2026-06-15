//! IPC command handling for AppState.

use crate::config::Config;
use crate::state::{validate_set_width_fraction, AppState};
use leopardwm_core_layout::{Rect, Workspace};
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{
    enumerate_windows, get_process_executable, monitor_to_left, monitor_to_right,
    move_window_offscreen,
};
use std::collections::HashMap;
use tracing::info;

impl AppState {
    /// Snapshot a workspace's current animated placements as `(window_id, rect)` pairs.
    fn workspace_placements(&self, monitor: leopardwm_platform_win32::MonitorId, ws_idx: usize) -> Vec<(u64, Rect)> {
        let viewport = self.layout_viewport(monitor);
        self.workspaces
            .get(&monitor)
            .and_then(|v| v.get(ws_idx))
            .filter(|_| self.monitors.contains_key(&monitor))
            .map(|ws| {
                ws.compute_placements_animated(viewport)
                    .into_iter()
                    .map(|p| (p.window_id, p.rect))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Execute a command on the focused workspace, handling snapshot/transition
    /// and layout application boilerplate.
    ///
    /// - `animated`: if true, snapshots before and starts a layout transition after
    /// - `sync_focus`: if true, syncs the OS foreground window after layout apply
    /// - `f`: receives the focused workspace and viewport width
    fn execute_workspace_command(
        &mut self,
        animated: bool,
        sync_focus: bool,
        f: impl FnOnce(&mut Workspace, i32),
    ) -> IpcResponse {
        let viewport_width = self.focused_viewport().width;
        let snapshot = if animated {
            Some(self.snapshot_layout())
        } else {
            None
        };
        if let Some(workspace) = self.focused_workspace_mut() {
            f(workspace, viewport_width);
        }
        if let Some(snapshot) = snapshot {
            self.start_layout_transition(snapshot);
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        if sync_focus {
            self.sync_foreground_window();
        }
        IpcResponse::Ok
    }

    /// Process an IPC command and return a response.
    pub(crate) fn handle_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        match cmd {
            IpcCommand::FocusLeft => {
                self.execute_workspace_command(false, true, |ws, vw| {
                    ws.focus_left();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Focus left -> column {}", ws.focused_column_index());
                })
            }
            IpcCommand::FocusRight => {
                self.execute_workspace_command(false, true, |ws, vw| {
                    ws.focus_right();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Focus right -> column {}", ws.focused_column_index());
                })
            }
            IpcCommand::FocusUp => {
                self.execute_workspace_command(false, true, |ws, _vw| {
                    ws.focus_up();
                    info!(
                        "Focus up -> window {}",
                        ws.focused_window_index_in_column()
                    );
                })
            }
            IpcCommand::FocusDown => {
                self.execute_workspace_command(false, true, |ws, _vw| {
                    ws.focus_down();
                    info!(
                        "Focus down -> window {}",
                        ws.focused_window_index_in_column()
                    );
                })
            }
            IpcCommand::FocusNext => {
                self.execute_workspace_command(false, true, |ws, vw| {
                    ws.focus_next();
                    ws.ensure_focused_visible_animated(vw);
                    info!(
                        "Focus next -> column {} window {}",
                        ws.focused_column_index(),
                        ws.focused_window_index_in_column()
                    );
                })
            }
            IpcCommand::FocusPrev => {
                self.execute_workspace_command(false, true, |ws, vw| {
                    ws.focus_prev();
                    ws.ensure_focused_visible_animated(vw);
                    info!(
                        "Focus prev -> column {} window {}",
                        ws.focused_column_index(),
                        ws.focused_window_index_in_column()
                    );
                })
            }
            IpcCommand::MoveColumnLeft => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.move_column_left();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved column left");
                })
            }
            IpcCommand::MoveColumnRight => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.move_column_right();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved column right");
                })
            }
            IpcCommand::MoveWindowLeft => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.move_window_left();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved window left to adjacent column");
                })
            }
            IpcCommand::MoveWindowRight => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.move_window_right();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved window right to adjacent column");
                })
            }
            IpcCommand::ExpelToLeft => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.expel_to_left();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Expelled window to left");
                })
            }
            IpcCommand::ExpelToRight => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.expel_to_right();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Expelled window to right");
                })
            }
            IpcCommand::ConsumeFromLeft => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.consume_from_left();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Consumed window from left");
                })
            }
            IpcCommand::ConsumeFromRight => {
                self.execute_workspace_command(true, true, |ws, vw| {
                    ws.consume_from_right();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Consumed window from right");
                })
            }
            IpcCommand::MoveWindowUp => {
                self.execute_workspace_command(true, true, |ws, _vw| {
                    ws.move_window_up_in_column();
                    info!("Moved window up in column");
                })
            }
            IpcCommand::MoveWindowDown => {
                self.execute_workspace_command(true, true, |ws, _vw| {
                    ws.move_window_down_in_column();
                    info!("Moved window down in column");
                })
            }
            IpcCommand::FocusMonitorLeft => self.handle_focus_monitor_left(),
            IpcCommand::FocusMonitorRight => self.handle_focus_monitor_right(),
            IpcCommand::MoveWindowToMonitorLeft => self.handle_move_window_to_monitor_left(),
            IpcCommand::MoveWindowToMonitorRight => self.handle_move_window_to_monitor_right(),
            IpcCommand::Resize { delta } => {
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.resize_focused_column(delta);
                    info!("Resized column by {}", delta);
                })
            }
            IpcCommand::Scroll { delta } => {
                self.execute_workspace_command(false, false, |ws, vw| {
                    ws.scroll_by(delta, vw);
                    info!("Scrolled by {}", delta);
                })
            }
            IpcCommand::QueryWorkspace => self.handle_query_workspace(),
            IpcCommand::QueryFocused => self.handle_query_focused(),
            IpcCommand::Refresh => self.handle_refresh(),
            IpcCommand::Apply => {
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                info!("Applied layout");
                IpcResponse::Ok
            }
            IpcCommand::Reload => self.handle_reload(),
            IpcCommand::TogglePause => {
                if let Err(e) = self.toggle_pause("IPC toggle") {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::SetGhostAnimation { enabled } => self.handle_set_ghost_animation(enabled),
            IpcCommand::Stop => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::PanicRevert => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::QueryAllWindows => self.handle_query_all_windows(),
            IpcCommand::CloseWindow => self.handle_close_window(),
            IpcCommand::ToggleFloating => self.handle_toggle_floating(),
            IpcCommand::ScratchpadStash => {
                self.scratchpad_stash();
                IpcResponse::Ok
            }
            IpcCommand::ScratchpadToggle => {
                self.scratchpad_toggle();
                IpcResponse::Ok
            }
            IpcCommand::ToggleSticky => {
                self.toggle_sticky();
                IpcResponse::Ok
            }
            IpcCommand::ToggleNewWindowPlacement => self.handle_toggle_new_window_placement(),
            IpcCommand::ToggleFullscreen => self.handle_toggle_fullscreen(),
            IpcCommand::SetColumnWidth { fraction } => {
                if let Err(message) = validate_set_width_fraction(fraction) {
                    return IpcResponse::error(message);
                }
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.set_focused_column_width_fraction(fraction, vw);
                    info!("Set column width fraction to {:.3}", fraction);
                })
            }
            IpcCommand::CenterColumn => {
                self.execute_workspace_command(false, false, |ws, vw| {
                    ws.center_focused_column_animated(vw);
                    info!("Centered focused column");
                })
            }
            IpcCommand::MaximizeColumn => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    let entering = ws.toggle_maximize_column(vw);
                    ws.center_focused_column_animated(vw);
                    info!("Maximize column: {}", if entering { "on" } else { "off" });
                })
            }
            IpcCommand::EqualizeColumnWidths => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.equalize_column_widths(vw);
                    info!("Equalized column widths");
                })
            }
            IpcCommand::CycleWidthUp => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_up(&presets, vw);
                    info!("Cycled column width up");
                })
            }
            IpcCommand::CycleWidthDown => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_down(&presets, vw);
                    info!("Cycled column width down");
                })
            }
            IpcCommand::CycleHeightUp => {
                let presets = self.config.layout.height_presets.clone();
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.cycle_height_up(&presets);
                    info!("Cycled window height up");
                })
            }
            IpcCommand::CycleHeightDown => {
                let presets = self.config.layout.height_presets.clone();
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.cycle_height_down(&presets);
                    info!("Cycled window height down");
                })
            }
            IpcCommand::EqualizeColumnHeights => {
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.equalize_focused_column_heights();
                    info!("Equalized column heights");
                })
            }
            IpcCommand::QueryStatus => self.handle_query_status(),
            IpcCommand::WorkspacePrev | IpcCommand::WorkspaceNext => {
                self.handle_workspace_prev_next(cmd)
            }
            IpcCommand::SwitchWorkspace { index } => self.handle_switch_workspace(index),
            IpcCommand::MoveToWorkspace { index } => self.handle_move_to_workspace(index),
            IpcCommand::HealthCheck => self.handle_health_check(),
            IpcCommand::GetAutoStart => {
                match leopardwm_platform_win32::autostart::get_autostart() {
                    Ok(enabled) => IpcResponse::AutoStartState { enabled },
                    Err(e) => IpcResponse::error(format!("Failed to read auto-start state: {}", e)),
                }
            }
            IpcCommand::SetAutoStart { enabled } => self.handle_set_auto_start(enabled),
            IpcCommand::Subscribe { .. } => {
                // Subscribe is handled out-of-band by ipc_server.rs
                // (per-client task acquires AppState directly so subscribe
                // + snapshot are atomic). Reaching this arm means the IPC
                // server accidentally routed a Subscribe through the
                // command path — it's a bug, not a user error.
                IpcResponse::error(
                    "Subscribe must be handled in stream mode by the IPC server, not the main \
                     command loop — this is an internal routing bug.",
                )
            }
            IpcCommand::ToggleOverview => {
                self.toggle_overview();
                IpcResponse::Ok
            }
            IpcCommand::ToggleTabbed => {
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.toggle_focused_column_tabbed_mode();
                    info!("Toggled tabbed mode on focused column");
                })
            }
            IpcCommand::SetActiveTab { column, tab } => self.handle_set_active_tab(column, tab),
        }
    }

    /// Handle `IpcCommand::FocusMonitorLeft`.
    fn handle_focus_monitor_left(&mut self) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = monitor_to_left(&monitors, self.focused_monitor) {
            let target_id = target.id;
            self.focused_monitor = target_id;
            info!("Focused monitor left -> {}", target_id);
            if let Err(e) = self.apply_layout() {
                return IpcResponse::error(format!("Failed to apply layout: {}", e));
            }
            self.sync_foreground_window();
        } else {
            info!("No monitor to the left");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::FocusMonitorRight`.
    fn handle_focus_monitor_right(&mut self) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = monitor_to_right(&monitors, self.focused_monitor) {
            let target_id = target.id;
            self.focused_monitor = target_id;
            info!("Focused monitor right -> {}", target_id);
            if let Err(e) = self.apply_layout() {
                return IpcResponse::error(format!("Failed to apply layout: {}", e));
            }
            self.sync_foreground_window();
        } else {
            info!("No monitor to the right");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::QueryFocused`.
    fn handle_query_focused(&mut self) -> IpcResponse {
        if let Some(workspace) = self.focused_workspace() {
            IpcResponse::FocusedWindow {
                window_id: workspace.focused_window(),
                column_index: workspace.focused_column_index(),
                window_index: workspace.focused_window_index_in_column(),
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    /// Handle `IpcCommand::Refresh`.
    fn handle_refresh(&mut self) -> IpcResponse {
        match self.enumerate_and_add_windows() {
            Ok(added) => {
                info!("Refreshed: added {} new windows across all monitors", added);
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to enumerate windows: {}", e)),
        }
    }

    /// Handle `IpcCommand::Reload`.
    fn handle_reload(&mut self) -> IpcResponse {
        match Config::load() {
            Ok(new_config) => {
                self.apply_config(new_config);
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.broadcast_event(leopardwm_ipc::IpcEvent::ConfigReloaded);
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to reload config: {}", e)),
        }
    }

    /// Handle `IpcCommand::SetGhostAnimation`.
    fn handle_set_ghost_animation(&mut self, enabled: Option<bool>) -> IpcResponse {
        if let Some(new_value) = enabled {
            // Aborts any active ghost transition first — the flag
            // flip mid-flight would otherwise leak handles.
            self.abort_active_ghost_transition();
            self.config.behavior.swap_chain_ghost_animation = new_value;
        }
        IpcResponse::BoolValue {
            value: self.config.behavior.swap_chain_ghost_animation,
        }
    }

    /// Handle `IpcCommand::CloseWindow`.
    fn handle_close_window(&mut self) -> IpcResponse {
        if let Some(hwnd) = self.focused_workspace().and_then(|ws| ws.focused_window()) {
            if let Err(e) = leopardwm_platform_win32::close_window(hwnd) {
                return IpcResponse::error(format!("Failed to close window: {}", e));
            }
            info!("Closed window {}", hwnd);
        } else {
            info!("No focused window to close");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::ToggleNewWindowPlacement`.
    fn handle_toggle_new_window_placement(&mut self) -> IpcResponse {
        use crate::config::NewWindowPlacement;
        let next = match self.config.behavior.new_window_placement {
            NewWindowPlacement::NewColumn => NewWindowPlacement::InColumn,
            NewWindowPlacement::InColumn => NewWindowPlacement::NewColumn,
        };
        self.config.behavior.new_window_placement = next;
        let _ = self.config.save();
        info!("New-window placement set to {:?}", next);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::ToggleFullscreen`.
    fn handle_toggle_fullscreen(&mut self) -> IpcResponse {
        let resp = self.execute_workspace_command(true, false, |ws, _vw| {
            let entering = ws.toggle_fullscreen();
            info!("Fullscreen: {}", if entering { "on" } else { "off" });
        });
        if self.focused_workspace().is_some_and(|ws| ws.is_fullscreen()) {
            self.hide_border();
        } else {
            self.sync_foreground_window();
        }
        resp
    }

    /// Handle `IpcCommand::QueryStatus`.
    fn handle_query_status(&mut self) -> IpcResponse {
        let uptime = self.start_time.elapsed().as_secs();
        let total_windows: usize = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter())
            .map(|ws| ws.window_count() + ws.floating_count())
            .sum();
        IpcResponse::StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            monitors: self.monitors.len(),
            total_windows,
            uptime_seconds: uptime,
        }
    }

    /// Handle `IpcCommand::WorkspacePrev` and `IpcCommand::WorkspaceNext`.
    fn handle_workspace_prev_next(&mut self, cmd: IpcCommand) -> IpcResponse {
        const COUNT: usize = 9;
        let monitor = self.focused_monitor;
        let current = self.active_workspace_idx(monitor);
        let target = match cmd {
            IpcCommand::WorkspacePrev => (current + COUNT - 1) % COUNT,
            IpcCommand::WorkspaceNext => (current + 1) % COUNT,
            _ => unreachable!(),
        };
        self.handle_command(IpcCommand::SwitchWorkspace {
            index: (target + 1) as u8,
        })
    }

    /// Handle `IpcCommand::MoveWindowToMonitorLeft`.
    fn handle_move_window_to_monitor_left(&mut self) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = monitor_to_left(&monitors, self.focused_monitor) {
            let target_id = target.id;
            match self.move_focused_window_to_monitor_transactional(target_id) {
                Ok(Some(hwnd)) => {
                    info!("Moved window {} to monitor {}", hwnd, target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!(
                            "Failed to apply layout: {}",
                            e
                        ));
                    }
                    self.sync_foreground_window();
                }
                Ok(None) => info!("No focused window to move"),
                Err(message) => return IpcResponse::error(message),
            }
        } else {
            info!("No monitor to the left");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::MoveWindowToMonitorRight`.
    fn handle_move_window_to_monitor_right(&mut self) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = monitor_to_right(&monitors, self.focused_monitor) {
            let target_id = target.id;
            match self.move_focused_window_to_monitor_transactional(target_id) {
                Ok(Some(hwnd)) => {
                    info!("Moved window {} to monitor {}", hwnd, target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!(
                            "Failed to apply layout: {}",
                            e
                        ));
                    }
                    self.sync_foreground_window();
                }
                Ok(None) => info!("No focused window to move"),
                Err(message) => return IpcResponse::error(message),
            }
        } else {
            info!("No monitor to the right");
        }
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::QueryWorkspace`.
    fn handle_query_workspace(&mut self) -> IpcResponse {
        let active_idx = self.active_workspace_idx(self.focused_monitor);
        let active_workspace_name = self.config.workspaces.name_for(active_idx);
        if let Some(workspace) = self.focused_workspace() {
            IpcResponse::WorkspaceState {
                columns: workspace.column_count(),
                windows: workspace.window_count(),
                focused_column: workspace.focused_column_index(),
                focused_window: workspace.focused_window_index_in_column(),
                scroll_offset: workspace.scroll_offset(),
                total_width: workspace.total_width(),
                active_workspace: active_idx as u8 + 1,
                active_workspace_name,
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    /// Handle `IpcCommand::QueryAllWindows`.
    fn handle_query_all_windows(&mut self) -> IpcResponse {
        let mut windows = Vec::new();

        // Get focused window for comparison
        let focused_hwnd = self.focused_workspace().and_then(|ws| ws.focused_window());

        // Enumerate all windows to get titles and other info
        let win_info_map: HashMap<u64, (String, String, u32)> = match enumerate_windows() {
            Ok(wins) => wins
                .into_iter()
                .map(|w| (w.hwnd, (w.title, w.class_name, w.process_id)))
                .collect(),
            Err(_) => HashMap::new(),
        };

        for (monitor_id, ws_vec) in &self.workspaces {
          for workspace in ws_vec {
            // Tiled windows
            for (col_idx, column) in workspace.columns().iter().enumerate() {
                for (win_idx, &window_id) in column.windows().iter().enumerate() {
                    let (title, class_name, process_id) =
                        win_info_map.get(&window_id).cloned().unwrap_or_else(|| {
                            ("Unknown".to_string(), "Unknown".to_string(), 0)
                        });

                    let executable = get_process_executable(process_id).unwrap_or_default();

                    // Get rect from computed placements
                    let rect = self
                        .monitors
                        .contains_key(monitor_id)
                        .then(|| workspace.compute_placements(self.layout_viewport(*monitor_id)))
                        .and_then(|placements| {
                            placements
                                .into_iter()
                                .find(|p| p.window_id == window_id)
                                .map(|p| p.rect)
                        })
                        .unwrap_or_else(|| Rect::new(0, 0, 0, 0));

                    windows.push(leopardwm_ipc::WindowInfo {
                        window_id,
                        title,
                        class_name,
                        process_id,
                        executable,
                        rect: leopardwm_ipc::IpcRect::new(
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height,
                        ),
                        column_index: Some(col_idx),
                        window_index: Some(win_idx),
                        monitor_id: *monitor_id as i64,
                        is_floating: false,
                        is_focused: Some(window_id) == focused_hwnd,
                    });
                }
            }

            // Floating windows
            for floating in workspace.floating_windows() {
                let (title, class_name, process_id) = win_info_map
                    .get(&floating.id)
                    .cloned()
                    .unwrap_or_else(|| ("Unknown".to_string(), "Unknown".to_string(), 0));

                let executable = get_process_executable(process_id).unwrap_or_default();

                windows.push(leopardwm_ipc::WindowInfo {
                    window_id: floating.id,
                    title,
                    class_name,
                    process_id,
                    executable,
                    rect: leopardwm_ipc::IpcRect::new(
                        floating.rect.x,
                        floating.rect.y,
                        floating.rect.width,
                        floating.rect.height,
                    ),
                    column_index: None,
                    window_index: None,
                    monitor_id: *monitor_id as i64,
                    is_floating: true,
                    is_focused: Some(floating.id) == focused_hwnd,
                });
            }
          }
        }

        IpcResponse::WindowList { windows }
    }

    /// Handle `IpcCommand::ToggleFloating`.
    fn handle_toggle_floating(&mut self) -> IpcResponse {
        let viewport = self.focused_viewport();
        let prev_hwnd = self.previous_focused_hwnd;
        if let Some(workspace) = self.focused_workspace_mut() {
            // Check if the OS-foreground window is floating — unfloat it
            let foreground_is_floating = prev_hwnd
                .map(|hwnd| workspace.is_floating(hwnd))
                .unwrap_or(false);
            if foreground_is_floating {
                let hwnd = prev_hwnd.unwrap();
                if workspace.unfloat_window(hwnd) {
                    self.disable_snap_for_window(hwnd);
                    info!("Unfloated window {} back to tiling", hwnd);
                }
            } else if let Some(wid) = workspace.toggle_floating(viewport) {
                self.restore_snap_for_window(wid);
                info!("Toggled window {} to floating", wid);
            }
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        self.sync_foreground_window();
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::SwitchWorkspace`.
    fn handle_switch_workspace(&mut self, index: u8) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        // A switch initiated outside the overlay (hotkey, CLI) dismisses
        // an open overview; overlay-initiated switches hid it already.
        if self.overview_open {
            self.hide_overview_animated(Some((index - 1) as usize));
        }
        let idx = (index - 1) as usize;
        let monitor = self.focused_monitor;
        let current_idx = self.active_workspace_idx(monitor);
        if idx == current_idx {
            return IpcResponse::Ok;
        }

        // Remember the floating window focused on the workspace we
        // are leaving, so returning re-focuses it. If the last focus
        // was tiled, forget any prior floating focus for it (the
        // column state already restores tiled focus). Prefer the live
        // OS foreground over the cached focus so a missed focus event
        // can't record the wrong window. Under cfg(test) there is no
        // meaningful OS foreground; tests drive previous_focused_hwnd.
        #[cfg(not(test))]
        let leaving_focus = leopardwm_platform_win32::get_foreground_window()
            .or(self.previous_focused_hwnd);
        #[cfg(test)]
        let leaving_focus = self.previous_focused_hwnd;
        // A focused sticky (pinned) window keeps focus across the switch:
        // capture that BEFORE the workspace changes. Any stale pending
        // refocus from a previous (aborted) switch is dropped here.
        self.pending_sticky_refocus = None;
        let sticky_focus =
            leaving_focus.filter(|hwnd| self.sticky_windows.contains(hwnd));
        if let Some(hwnd) = leaving_focus {
            if self
                .focused_workspace()
                .is_some_and(|ws| ws.is_floating(hwnd))
            {
                self.floating_focus.insert((monitor, current_idx), hwnd);
            } else {
                self.floating_focus.remove(&(monitor, current_idx));
            }
        }

        // Cancel any in-progress drag: reinsert window if it was
        // removed from source during live preview, then remove placeholders.
        // Only reinsert if the window still exists.
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
        // Move exit windows offscreen before clearing the transition,
        // so they don't get stranded at intermediate positions.
        if let Some(ref transition) = self.layout_transition {
            for wid in transition.exit_rects.keys() {
                let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
            }
        }
        self.abort_active_ghost_transition();
        self.layout_transition = None;

        let slide_height = self.monitors.get(&monitor)
            .map(|m| m.work_area.height)
            .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
        // Positive offset = new workspace enters from below (scrolling up).
        let y_offset = if idx > current_idx { slide_height } else { -slide_height };

        // Snapshot old workspace's current positions (start for exiting windows).
        let mut old_placements = self.workspace_placements(monitor, current_idx);

        // Overview snapshot mode: grab the outgoing windows NOW, while
        // they are still on screen, so their cards show a real frame
        // after they move offscreen below. Skipped otherwise (PrintWindow
        // per window is not free).
        if self.config.overview.render == crate::config::OverviewRender::Snapshot {
            for (wid, _) in &old_placements {
                let _ = leopardwm_platform_win32::snapshot::snapshot_capture(*wid);
            }
        }

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Switch active workspace
        self.active_workspace.insert(monitor, idx);

        // Sticky windows follow the switch: move them onto the
        // now-active workspace.
        self.rehome_sticky_windows();

        // Compute new workspace's final placements.
        let mut new_placements = self.workspace_placements(monitor, idx);

        // Keep sticky windows out of the slide animation so they sit
        // still while the rest of the layout scrolls past.
        old_placements.retain(|(w, _)| !self.sticky_windows.contains(w));
        new_placements.retain(|(w, _)| !self.sticky_windows.contains(w));

        // Build animation rects:
        // - Entering windows: start offscreen, end at final position
        // - Exiting windows: start at current position, end offscreen
        let mut start_rects = std::collections::HashMap::new();
        let mut exit_rects = std::collections::HashMap::new();

        // New workspace windows enter from the opposite side.
        for (wid, rect) in &new_placements {
            start_rects.insert(*wid, leopardwm_core_layout::Rect::new(
                rect.x,
                rect.y + y_offset,
                rect.width,
                rect.height,
            ));
        }

        // Old workspace windows slide out.
        for (wid, rect) in &old_placements {
            start_rects.insert(*wid, *rect);
            exit_rects.insert(*wid, leopardwm_core_layout::Rect::new(
                rect.x,
                rect.y - y_offset,
                rect.width,
                rect.height,
            ));
        }

        if !start_rects.is_empty() {
            let duration = self.config.animation.workspace_switch_duration_ms;
            self.start_workspace_switch_transition(start_rects, exit_rects, duration);
        } else {
            // No windows to animate — hide old immediately
            for (wid, _) in &old_placements {
                let _ = move_window_offscreen(*wid);
            }
        }

        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        // Restore the floating window that was focused on this
        // workspace (if it still floats here) so it regains focus on
        // return, before syncing the OS foreground.
        if let Some(&hwnd) = self.floating_focus.get(&(monitor, idx)) {
            let still_floating = self
                .workspaces
                .get(&monitor)
                .and_then(|v| v.get(idx))
                .is_some_and(|ws| ws.is_floating(hwnd));
            if still_floating {
                self.previous_focused_hwnd = Some(hwnd);
            }
        }
        self.sync_foreground_window();
        // If a summoned scratchpad lives on this workspace, restore
        // its focus (it would otherwise stay visible but lose focus
        // to a tiled window on the switch back).
        self.refocus_scratchpad_if_active();
        // The user was focused on a pinned window: it followed the switch
        // (re-homed above), so focus stays on it. Re-assert again at the
        // animation landing — a spurious foreground event from the
        // destination's windows mid-slide (e.g. a fullscreen window
        // activating) can clobber previous_focused_hwnd before the
        // landing re-sync.
        if let Some(hwnd) = sticky_focus {
            if self.refocus_sticky_window(hwnd) && self.layout_transition.is_some() {
                self.pending_sticky_refocus = Some(hwnd);
            }
        }
        self.broadcast_event(leopardwm_ipc::IpcEvent::WorkspaceChanged {
            monitor: monitor as i64,
            old_index: current_idx as u8,
            new_index: idx as u8,
            name: self.config.workspaces.name_for(idx),
        });
        info!("Switched to workspace {}", index);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::MoveToWorkspace`.
    fn handle_move_to_workspace(&mut self, index: u8) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        let idx = (index - 1) as usize;
        let monitor = self.focused_monitor;
        let current_idx = self.active_workspace_idx(monitor);
        if idx == current_idx {
            return IpcResponse::Ok;
        }

        // Get focused window — prefer the OS-foreground window (previous_focused_hwnd)
        // so that floating windows can also be moved between workspaces.
        // Fall back to tiled focus if previous_focused_hwnd is not on this workspace.
        let focused_hwnd = {
            let tiled_focus = self.focused_workspace().and_then(|ws| ws.focused_window());
            let os_focus = self.previous_focused_hwnd.and_then(|hwnd| {
                // Verify the OS-focused window is actually on the current workspace
                self.workspaces.get(&monitor)
                    .and_then(|v| v.get(current_idx))
                    .filter(|ws| ws.contains_window(hwnd))
                    .map(|_| hwnd)
            });
            match os_focus.or(tiled_focus) {
                Some(hwnd) => hwnd,
                None => return IpcResponse::Ok,
            }
        };

        let snapshot = self.snapshot_layout();

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Check if the window is floating so we use the correct add/remove APIs.
        let is_floating = self.workspaces.get(&monitor)
            .and_then(|v| v.get(current_idx))
            .is_some_and(|ws| ws.is_floating(focused_hwnd));

        // Remove from source and insert into target.
        // For floating windows, get the rect from workspace state (canonical position).
        let floating_rect = if is_floating {
            self.workspaces.get(&monitor)
                .and_then(|v| v.get(current_idx))
                .and_then(|ws| ws.floating_windows().iter()
                    .find(|f| f.id == focused_hwnd).map(|f| f.rect))
        } else {
            None
        };

        if let Some(workspace) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(current_idx)) {
            if is_floating {
                workspace.remove_floating(focused_hwnd);
            } else if let Err(e) = workspace.remove_window(focused_hwnd) {
                return IpcResponse::error(format!("Failed to remove window: {}", e));
            }
        }

        // Ensure target workspace exists (lazy creation)
        self.ensure_workspace_exists(monitor, idx);

        // Insert into target workspace
        if let Some(workspace) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(idx)) {
            if is_floating {
                let rect = floating_rect.unwrap_or(leopardwm_core_layout::Rect::new(0, 0, 800, 600));
                if let Err(e) = workspace.add_floating(focused_hwnd, rect) {
                    // Rollback: re-add to source
                    if let Some(src_ws) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(current_idx)) {
                        let _ = src_ws.add_floating(focused_hwnd, rect);
                    }
                    return IpcResponse::error(format!("Failed to move floating window: {}", e));
                }
            } else if let Err(e) = workspace.insert_window(focused_hwnd, None) {
                // Rollback: re-insert into source since target insert failed
                if let Some(src_ws) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(current_idx)) {
                    let _ = src_ws.insert_window(focused_hwnd, None);
                }
                return IpcResponse::error(format!("Failed to add window to target workspace: {}", e));
            }
        }

        // Target workspace is not active — hide the moved window
        // (capture-on-hide first for the overview's snapshot mode).
        if self.config.overview.render == crate::config::OverviewRender::Snapshot {
            let _ = leopardwm_platform_win32::snapshot::snapshot_capture(focused_hwnd);
        }
        let _ = move_window_offscreen(focused_hwnd);

        // Ensure the source workspace scrolls to show its new focused window
        let viewport_width = self.viewport_width_for(monitor);
        if let Some(workspace) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(current_idx)) {
            workspace.ensure_focused_visible_animated(viewport_width);
        }

        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        self.sync_foreground_window();
        info!("Moved window {} to workspace {}", focused_hwnd, index);
        IpcResponse::Ok
    }

    /// Handle `IpcCommand::HealthCheck`.
    fn handle_health_check(&mut self) -> IpcResponse {
        let uptime = self.start_time.elapsed().as_secs();
        let total_windows: usize = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter())
            .map(|ws| ws.window_count() + ws.floating_count())
            .sum();
        IpcResponse::HealthInfo {
            healthy: true,
            uptime_seconds: uptime,
            total_windows,
            monitors: self.monitors.len(),
            paused: self.paused,
            thumbnail_register_balance:
                leopardwm_platform_win32::thumbnail::current_register_balance(),
        }
    }

    /// Handle `IpcCommand::SetAutoStart`.
    fn handle_set_auto_start(&mut self, enabled: bool) -> IpcResponse {
        use leopardwm_platform_win32::autostart;
        let result = if enabled {
            match std::env::current_exe() {
                Ok(exe) => autostart::enable_autostart(&exe).map(|()| exe),
                Err(e) => Err(anyhow::anyhow!("resolve daemon executable: {}", e)),
            }
        } else {
            autostart::disable_autostart().map(|()| std::path::PathBuf::new())
        };
        match result {
            Ok(exe) => {
                if enabled {
                    info!("Auto-start enabled (path: {})", exe.display());
                } else {
                    info!("Auto-start disabled");
                }
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::error(format!("Failed to update auto-start: {}", e)),
        }
    }

    /// Handle `IpcCommand::SetActiveTab`.
    fn handle_set_active_tab(&mut self, column: usize, tab: usize) -> IpcResponse {
        // Pre-arm the same-column-suppression bypass so the
        // synthesized SetForegroundWindow that follows doesn't get
        // squashed as redundant intra-column churn.
        let monitor = self.focused_monitor;
        let ws_idx = self.active_workspace_idx(monitor);
        self.pending_tab_focus = Some(crate::state::PendingTabFocus {
            monitor,
            workspace_idx: ws_idx,
            column_idx: column,
            tab_idx: tab,
            set_at: std::time::Instant::now(),
        });
        let Some(workspace) = self.focused_workspace_mut() else {
            return IpcResponse::error("No focused workspace");
        };
        if let Err(e) = workspace.set_active_tab(column, tab) {
            self.pending_tab_focus = None;
            return IpcResponse::error(format!("set_active_tab failed: {}", e));
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("apply_layout failed: {}", e));
        }
        self.sync_foreground_window();
        info!("Set active tab: column={}, tab={}", column, tab);
        IpcResponse::Ok
    }
}
