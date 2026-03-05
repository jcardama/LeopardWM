//! IPC command handling for AppState.

use crate::config::Config;
use crate::state::{validate_set_width_fraction, AppState};
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{
    enumerate_windows, get_process_executable, monitor_to_left, monitor_to_right,
};
use std::collections::HashMap;
use tracing::info;

impl AppState {
    /// Process an IPC command and return a response.
    pub(crate) fn handle_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        let viewport_width = self.focused_viewport().width;

        match cmd {
            IpcCommand::FocusLeft => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_left();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Focus left -> column {}", workspace.focused_column_index());
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusRight => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_right();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Focus right -> column {}", workspace.focused_column_index());
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusUp => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_up();
                    info!(
                        "Focus up -> window {}",
                        workspace.focused_window_index_in_column()
                    );
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::FocusDown => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.focus_down();
                    info!(
                        "Focus down -> window {}",
                        workspace.focused_window_index_in_column()
                    );
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::MoveColumnLeft => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.move_column_left();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Moved column left");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::MoveColumnRight => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.move_column_right();
                    workspace.ensure_focused_visible_animated(viewport_width);
                    info!("Moved column right");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::FocusMonitorLeft => {
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
            IpcCommand::FocusMonitorRight => {
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
            IpcCommand::MoveWindowToMonitorLeft => {
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
            IpcCommand::MoveWindowToMonitorRight => {
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
            IpcCommand::Resize { delta } => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.resize_focused_column(delta);
                    info!("Resized column by {}", delta);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::Scroll { delta } => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.scroll_by(delta, viewport_width);
                    info!("Scrolled by {}", delta);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::QueryWorkspace => {
                if let Some(workspace) = self.focused_workspace() {
                    IpcResponse::WorkspaceState {
                        columns: workspace.column_count(),
                        windows: workspace.window_count(),
                        focused_column: workspace.focused_column_index(),
                        focused_window: workspace.focused_window_index_in_column(),
                        scroll_offset: workspace.scroll_offset(),
                        total_width: workspace.total_width(),
                    }
                } else {
                    IpcResponse::error("No focused workspace")
                }
            }
            IpcCommand::QueryFocused => {
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
            IpcCommand::Refresh => match self.enumerate_and_add_windows() {
                Ok(added) => {
                    info!("Refreshed: added {} new windows across all monitors", added);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    IpcResponse::Ok
                }
                Err(e) => IpcResponse::error(format!("Failed to enumerate windows: {}", e)),
            },
            IpcCommand::Apply => {
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                info!("Applied layout");
                IpcResponse::Ok
            }
            IpcCommand::Reload => match Config::load() {
                Ok(new_config) => {
                    self.apply_config(new_config);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    IpcResponse::Ok
                }
                Err(e) => IpcResponse::error(format!("Failed to reload config: {}", e)),
            },
            IpcCommand::TogglePause => {
                if let Err(e) = self.toggle_pause("IPC toggle") {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::Stop => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::PanicRevert => {
                // This is handled specially in the event loop
                IpcResponse::Ok
            }
            IpcCommand::QueryAllWindows => {
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

                for (monitor_id, workspace) in &self.workspaces {
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
                                .get(monitor_id)
                                .map(|m| workspace.compute_placements(m.work_area))
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

                IpcResponse::WindowList { windows }
            }
            IpcCommand::CloseWindow => {
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
            IpcCommand::ToggleFloating => {
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
                            info!("Unfloated window {} back to tiling", hwnd);
                        }
                    } else if let Some(wid) = workspace.toggle_floating(viewport) {
                        info!("Toggled window {} to floating", wid);
                    }
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                self.sync_foreground_window();
                IpcResponse::Ok
            }
            IpcCommand::ToggleFullscreen => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    let entering = workspace.toggle_fullscreen();
                    info!("Fullscreen: {}", if entering { "on" } else { "off" });
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::SetColumnWidth { fraction } => {
                if let Err(message) = validate_set_width_fraction(fraction) {
                    return IpcResponse::error(message);
                }
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.set_focused_column_width_fraction(fraction, viewport_width);
                    info!("Set column width fraction to {:.3}", fraction);
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::EqualizeColumnWidths => {
                if let Some(workspace) = self.focused_workspace_mut() {
                    workspace.equalize_column_widths(viewport_width);
                    info!("Equalized column widths");
                }
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::QueryStatus => {
                let uptime = self.start_time.elapsed().as_secs();
                let total_windows: usize = self
                    .workspaces
                    .values()
                    .map(|ws| ws.window_count() + ws.floating_count())
                    .sum();
                IpcResponse::StatusInfo {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    monitors: self.monitors.len(),
                    total_windows,
                    uptime_seconds: uptime,
                }
            }
            IpcCommand::HealthCheck => {
                let uptime = self.start_time.elapsed().as_secs();
                let total_windows: usize = self
                    .workspaces
                    .values()
                    .map(|ws| ws.window_count() + ws.floating_count())
                    .sum();
                IpcResponse::HealthInfo {
                    healthy: true,
                    uptime_seconds: uptime,
                    total_windows,
                    monitors: self.monitors.len(),
                    paused: self.paused,
                }
            }
        }
    }
}
