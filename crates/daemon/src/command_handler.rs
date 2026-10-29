use crate::config::Config;
use crate::hotkey_resolution::resolve_hotkeys;
use crate::layout_apply::LayoutApplyOutcome;
use crate::state::{
    validate_set_width_fraction, AppState, ElevationBlockedRecord, PendingWorkspaceSwitchFocus,
};
use leopardwm_core_layout::{LayoutError, Rect, Visibility, WindowPlacement, Workspace};
use leopardwm_ipc::{
    ElevationBlockReason, ElevationBlockedWindow, HotkeyBindingInfo, IpcCommand, IpcResponse,
};
use leopardwm_platform_win32::{
    enumerate_windows, get_process_executable, monitor_above, monitor_below, monitor_to_left,
    monitor_to_right, ManageBlock, MonitorId, MonitorInfo,
};
use std::collections::HashMap;
use tracing::{debug, info, warn};

enum FullscreenPolicy {
    /// Drop fullscreen first, then run the command against the visible strip.
    /// For structural commands that would otherwise mutate the hidden layout
    /// invisibly and dispatch focus to cloaked windows.
    Exit,
    /// Move focus but stay fullscreen, carrying fullscreen to the newly focused
    /// window (monocle mode), so focus changes cycle which window is fullscreen
    /// instead of dropping out of fullscreen.
    FollowFocus,
    /// Ignore while fullscreen — pure sizing/scroll commands have no visible
    /// effect on a single fullscreen window, and a stray scroll gesture must
    /// not silently drop fullscreen.
    Suppress,
    Allow,
}

enum FloatingPolicy {
    /// Act on the focused floating window instead of the tiled focus.
    Target,
    /// Ignore while a floating window has focus — the command only makes
    /// sense for columns, and would otherwise reshape or scroll the strip
    /// behind it.
    Ignore,
    Unchanged,
}

fn humanize_action_id(action_id: &str) -> String {
    let mut label = action_id.replace('_', " ");
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    label
}

/// Explicit workspace switches do not warp the cursor. Edge-wrap focus commands
/// still qualify when they land on a different window, as do cross-monitor moves
/// whose focus travels with the window.
fn is_focus_navigation(cmd: &IpcCommand) -> bool {
    use IpcCommand::*;
    matches!(
        cmd,
        FocusLeft
            | FocusRight
            | FocusUp
            | FocusDown
            | FocusNext
            | FocusPrev
            | FocusStart
            | FocusEnd
            | FocusRowUp
            | FocusRowDown
            | FocusRow { .. }
            | FocusMonitorLeft
            | FocusMonitorRight
            | FocusMonitorUp
            | FocusMonitorDown
            | MoveWindowToMonitorLeft
            | MoveWindowToMonitorRight
            | MoveWindowToMonitorUp
            | MoveWindowToMonitorDown
    )
}

fn fullscreen_policy(cmd: &IpcCommand) -> FullscreenPolicy {
    use IpcCommand::*;
    match cmd {
        FocusLeft
        | FocusRight
        | FocusUp
        | FocusDown
        | FocusNext
        | FocusPrev
        | FocusStart
        | FocusEnd
        | FocusRowUp
        | FocusRowDown
        | FocusRow { .. } => FullscreenPolicy::FollowFocus,
        MoveColumnLeft | MoveColumnRight | MoveColumnToStart | MoveColumnToEnd | MoveWindowLeft
        | MoveWindowRight | MoveWindowUp | MoveWindowDown | MoveWindowToRowUp
        | MoveWindowToRowDown | ExpelToLeft | ExpelToRight | ConsumeFromLeft | ConsumeFromRight
        | ToggleTabbed => FullscreenPolicy::Exit,
        Resize { .. }
        | Scroll { .. }
        | SetColumnWidth { .. }
        | CycleWidth
        | CycleHeight
        | CycleWidthUp
        | CycleWidthDown
        | CycleHeightUp
        | CycleHeightDown
        | CenterColumn
        | MaximizeColumn
        | EqualizeColumnWidths
        | EqualizeColumnHeights => FullscreenPolicy::Suppress,
        _ => FullscreenPolicy::Allow,
    }
}

fn floating_policy(cmd: &IpcCommand) -> FloatingPolicy {
    use IpcCommand::*;
    match cmd {
        CloseWindow
        | ToggleFloating
        | ToggleSticky
        | ToggleFullscreen
        | Resize { .. }
        | QueryFocused
        | ScratchpadStash
        | MoveToWorkspace { .. }
        | MoveToWorkspacePrev
        | MoveToWorkspaceNext
        | MoveWindowToMonitorLeft
        | MoveWindowToMonitorRight
        | MoveWindowToMonitorUp
        | MoveWindowToMonitorDown => FloatingPolicy::Target,
        FocusLeft
        | FocusRight
        | FocusUp
        | FocusDown
        | FocusNext
        | FocusPrev
        | FocusStart
        | FocusEnd
        | SetColumnWidth { .. }
        | CycleWidth
        | CycleWidthUp
        | CycleWidthDown
        | CycleHeight
        | CycleHeightUp
        | CycleHeightDown
        | MaximizeColumn
        | CenterColumn
        | EqualizeColumnHeights
        | ToggleTabbed
        | MoveColumnLeft
        | MoveColumnRight
        | MoveColumnToStart
        | MoveColumnToEnd
        | MoveWindowLeft
        | MoveWindowRight
        | MoveWindowUp
        | MoveWindowDown
        | ConsumeFromLeft
        | ConsumeFromRight
        | ExpelToLeft
        | ExpelToRight => FloatingPolicy::Ignore,
        _ => FloatingPolicy::Unchanged,
    }
}

fn ipc_elevation_block_reason(block: ManageBlock) -> ElevationBlockReason {
    match block {
        ManageBlock::HigherIntegrity => ElevationBlockReason::HigherIntegrity,
        ManageBlock::Protected => ElevationBlockReason::Protected,
        ManageBlock::No => ElevationBlockReason::Unknown,
    }
}

fn elevation_blocked_health_views(
    blocked: &HashMap<u64, ElevationBlockedRecord>,
) -> (Vec<(u64, String)>, Vec<ElevationBlockedWindow>) {
    let mut entries: Vec<(u64, &ElevationBlockedRecord)> = blocked
        .iter()
        .map(|(&hwnd, record)| (hwnd, record))
        .collect();
    entries.sort_by(|a, b| a.1.title.cmp(&b.1.title).then(a.0.cmp(&b.0)));
    let mut legacy = Vec::with_capacity(entries.len());
    let mut records = Vec::with_capacity(entries.len());
    for (hwnd, record) in entries {
        legacy.push((hwnd, record.title.clone()));
        records.push(ElevationBlockedWindow {
            hwnd,
            title: record.title.clone(),
            reason: ipc_elevation_block_reason(record.reason),
        });
    }
    (legacy, records)
}

impl AppState {
    pub(crate) fn workspace_placements(
        &self,
        monitor: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
    ) -> Vec<WindowPlacement> {
        let viewport = self.layout_viewport(monitor);
        self.workspaces
            .get(&monitor)
            .and_then(|v| v.get(ws_idx))
            .filter(|_| self.monitors.contains_key(&monitor))
            .map(|ws| ws.compute_placements_animated(viewport))
            .unwrap_or_default()
    }

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
        // The command may have scrolled a column off-viewport (or back in);
        // update taskbar buttons to match the new visibility.
        self.sync_taskbar_buttons();
        if sync_focus {
            self.sync_foreground_window();
        }
        IpcResponse::Ok
    }

    fn exit_fullscreen_if_active(&mut self) -> bool {
        let Some(fs_wid) = self
            .focused_workspace()
            .and_then(|ws| ws.fullscreen_window_id())
        else {
            return false;
        };
        // Clear unconditionally — `toggle_fullscreen` would re-enter on the
        // focused window when the fullscreen pointer is stale.
        if let Some(ws) = self.focused_workspace_mut() {
            ws.clear_fullscreen_if_window(fs_wid);
        }
        let _ = self.apply_layout();
        self.sync_foreground_window();
        true
    }

    fn focus_in_fullscreen(&mut self, cmd: &IpcCommand) -> IpcResponse {
        let resp = self.execute_workspace_command(false, true, |ws, vw| {
            match cmd {
                IpcCommand::FocusLeft => ws.focus_left(),
                IpcCommand::FocusRight => ws.focus_right(),
                IpcCommand::FocusUp => ws.focus_up(),
                IpcCommand::FocusDown => ws.focus_down(),
                IpcCommand::FocusNext => ws.focus_next(),
                IpcCommand::FocusPrev => ws.focus_prev(),
                IpcCommand::FocusStart => ws.focus_start(),
                IpcCommand::FocusEnd => ws.focus_end(),
                IpcCommand::FocusRowUp => ws.focus_row_up(),
                IpcCommand::FocusRowDown => ws.focus_row_down(),
                IpcCommand::FocusRow { index } => ws.focus_row(*index),
                _ => {}
            }
            ws.fullscreen_follow_focus();
            // Keep scroll tracking the focused column even though the fullscreen
            // window hides it, so toggling fullscreen off later lands on the
            // focused window instead of a stale off-screen scroll position.
            ws.ensure_focused_visible_animated(vw);
        });
        self.hide_border();
        resp
    }

    fn handle_strip_end_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        match cmd {
            IpcCommand::FocusStart => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_start();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus start -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusEnd => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_end();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus end -> column {}", ws.focused_column_index());
            }),
            IpcCommand::MoveColumnToStart => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.move_column_to_start();
                    ws.ensure_focused_visible_animated(vw);
                    info!("Moved column to start");
                })
            }
            IpcCommand::MoveColumnToEnd => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_to_end();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column to end");
            }),
            _ => unreachable!("handle_strip_end_command called with a non-strip-end command"),
        }
    }

    // Fullscreen cloaks the rest of the strip: structural commands must reveal
    // it before mutating it. Focus may carry fullscreen; sizing/scroll is ignored.
    // Some means dispatch is complete; None continues normal dispatch.
    fn apply_fullscreen_policy(&mut self, cmd: &IpcCommand) -> Option<IpcResponse> {
        if !self
            .focused_workspace()
            .is_some_and(|ws| ws.is_fullscreen())
        {
            return None;
        }
        match fullscreen_policy(cmd) {
            FullscreenPolicy::Exit => {
                self.exit_fullscreen_if_active();
                None
            }
            FullscreenPolicy::FollowFocus => {
                // Following focus would hand a floating window's fullscreen to
                // the tiled focus behind it, so leave fullscreen instead.
                if self.config.behavior.fullscreen_follows_focus
                    && !self.focused_workspace().is_some_and(|ws| {
                        ws.fullscreen_window_id()
                            .is_some_and(|wid| ws.is_floating(wid))
                    })
                {
                    Some(self.focus_in_fullscreen(cmd))
                } else {
                    self.exit_fullscreen_if_active();
                    None
                }
            }
            FullscreenPolicy::Suppress => {
                debug!("Ignoring {:?} while fullscreen", cmd);
                Some(IpcResponse::Ok)
            }
            FullscreenPolicy::Allow => None,
        }
    }

    // A focused floating window has no column. Runs before the fullscreen
    // policy so an ignored column command does not drop fullscreen either.
    // Some means dispatch is complete; None continues normal dispatch.
    fn apply_floating_policy(&mut self, cmd: &IpcCommand) -> Option<IpcResponse> {
        let hwnd = self.focused_floating_window()?;
        match floating_policy(cmd) {
            FloatingPolicy::Target => self.handle_floating_target(cmd, hwnd),
            FloatingPolicy::Ignore => {
                debug!("Ignoring {:?} while a floating window has focus", cmd);
                Some(IpcResponse::Ok)
            }
            FloatingPolicy::Unchanged => None,
        }
    }

    fn handle_floating_target(&mut self, cmd: &IpcCommand, hwnd: u64) -> Option<IpcResponse> {
        match *cmd {
            IpcCommand::CloseWindow => Some(self.handle_close_window()),
            IpcCommand::ToggleFloating => Some(self.handle_unfloat_window(hwnd)),
            IpcCommand::ToggleSticky => {
                self.toggle_sticky_window(hwnd);
                Some(IpcResponse::Ok)
            }
            IpcCommand::MoveToWorkspace { index } => {
                Some(self.move_window_from_active_workspace(hwnd, index))
            }
            IpcCommand::MoveToWorkspacePrev => {
                let index = self.relative_workspace_index(false);
                Some(self.move_window_from_active_workspace(hwnd, index))
            }
            IpcCommand::MoveToWorkspaceNext => {
                let index = self.relative_workspace_index(true);
                Some(self.move_window_from_active_workspace(hwnd, index))
            }
            IpcCommand::MoveWindowToMonitorLeft => {
                Some(self.move_to_monitor(Some(hwnd), monitor_to_left, "left"))
            }
            IpcCommand::MoveWindowToMonitorRight => {
                Some(self.move_to_monitor(Some(hwnd), monitor_to_right, "right"))
            }
            IpcCommand::MoveWindowToMonitorUp => {
                Some(self.move_to_monitor(Some(hwnd), monitor_above, "up"))
            }
            IpcCommand::MoveWindowToMonitorDown => {
                Some(self.move_to_monitor(Some(hwnd), monitor_below, "down"))
            }
            IpcCommand::ScratchpadStash => {
                self.scratchpad_stash();
                Some(IpcResponse::Ok)
            }
            IpcCommand::ToggleFullscreen => {
                Some(self.toggle_fullscreen_with(|ws| ws.toggle_fullscreen_window(hwnd)))
            }
            IpcCommand::Resize { delta } => {
                if self
                    .focused_workspace()
                    .is_some_and(|ws| ws.fullscreen_window_id() == Some(hwnd))
                {
                    debug!("Ignoring {:?} while the floating window is fullscreen", cmd);
                    return Some(IpcResponse::Ok);
                }
                Some(self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.resize_floating(hwnd, delta);
                    info!("Resized floating window {} by {}", hwnd, delta);
                }))
            }
            IpcCommand::QueryFocused => {
                let ws = self.focused_workspace()?;
                // The indices keep pointing at the tiled focus so older clients
                // that ignore `is_floating` still read a valid strip position.
                Some(IpcResponse::FocusedWindow {
                    window_id: Some(hwnd),
                    column_index: ws.focused_column_index(),
                    window_index: ws.focused_window_index_in_column(),
                    row_index: ws.focused_row(),
                    is_floating: true,
                })
            }
            _ => None,
        }
    }

    pub(crate) fn handle_command(&mut self, cmd: IpcCommand) -> IpcResponse {
        if let Some(resp) = self.apply_floating_policy(&cmd) {
            return resp;
        }
        if let Some(resp) = self.apply_fullscreen_policy(&cmd) {
            return resp;
        }
        // Capture focus before dispatch refreshes previous_focused_hwnd: mouse
        // follows focus (#43) must not warp when navigation leaves focus unchanged.
        let focus_before = (self.config.behavior.mouse_follows_focus && is_focus_navigation(&cmd))
            .then_some(self.previous_focused_hwnd);
        let resp = match cmd {
            IpcCommand::FocusLeft => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus left -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusRight => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Focus right -> column {}", ws.focused_column_index());
            }),
            IpcCommand::FocusUp => self.focus_vertical(true),
            IpcCommand::FocusDown => self.focus_vertical(false),
            IpcCommand::FocusNext => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_next();
                ws.ensure_focused_visible_animated(vw);
                info!(
                    "Focus next -> column {} window {}",
                    ws.focused_column_index(),
                    ws.focused_window_index_in_column()
                );
            }),
            IpcCommand::FocusPrev => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_prev();
                ws.ensure_focused_visible_animated(vw);
                info!(
                    "Focus prev -> column {} window {}",
                    ws.focused_column_index(),
                    ws.focused_window_index_in_column()
                );
            }),
            IpcCommand::FocusStart
            | IpcCommand::FocusEnd
            | IpcCommand::MoveColumnToStart
            | IpcCommand::MoveColumnToEnd => self.handle_strip_end_command(cmd),
            IpcCommand::MoveColumnLeft => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column left");
            }),
            IpcCommand::MoveColumnRight => self.execute_workspace_command(true, false, |ws, vw| {
                ws.move_column_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved column right");
            }),
            IpcCommand::MoveWindowLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.move_window_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window left");
            }),
            IpcCommand::MoveWindowRight => self.execute_workspace_command(true, true, |ws, vw| {
                ws.move_window_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window right");
            }),
            IpcCommand::ExpelToLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.expel_to_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Expelled window to left");
            }),
            IpcCommand::ExpelToRight => self.expel_tiled_focus_to_right(),
            IpcCommand::ConsumeFromLeft => self.execute_workspace_command(true, true, |ws, vw| {
                ws.consume_from_left();
                ws.ensure_focused_visible_animated(vw);
                info!("Consumed window from left");
            }),
            IpcCommand::ConsumeFromRight => self.execute_workspace_command(true, true, |ws, vw| {
                ws.consume_from_right();
                ws.ensure_focused_visible_animated(vw);
                info!("Consumed window from right");
            }),
            IpcCommand::MoveWindowUp => self.move_window_vertical(true),
            IpcCommand::MoveWindowDown => self.move_window_vertical(false),
            IpcCommand::FocusRowUp => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_row_up();
                ws.ensure_focused_visible_animated(vw);
                info!("Focused row up -> row {}", ws.focused_row());
            }),
            IpcCommand::FocusRowDown => self.execute_workspace_command(false, true, |ws, vw| {
                ws.focus_row_down();
                ws.ensure_focused_visible_animated(vw);
                info!("Focused row down -> row {}", ws.focused_row());
            }),
            IpcCommand::FocusRow { index } => {
                let target = index;
                self.execute_workspace_command(false, true, move |ws, vw| {
                    ws.focus_row(target);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Focused row -> row {}", ws.focused_row());
                })
            }
            IpcCommand::MoveWindowToRowUp => self.execute_workspace_command(true, true, |ws, vw| {
                let _ = ws.move_window_to_row_up();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window to row up -> row {}", ws.focused_row());
            }),
            IpcCommand::MoveWindowToRowDown => self.execute_workspace_command(true, true, |ws, vw| {
                let _ = ws.move_window_to_row_down();
                ws.ensure_focused_visible_animated(vw);
                info!("Moved window to row down -> row {}", ws.focused_row());
            }),
            IpcCommand::FocusMonitorLeft => self.focus_monitor(monitor_to_left, "left"),
            IpcCommand::FocusMonitorRight => self.focus_monitor(monitor_to_right, "right"),
            IpcCommand::FocusMonitorUp => self.focus_monitor(monitor_above, "up"),
            IpcCommand::FocusMonitorDown => self.focus_monitor(monitor_below, "down"),
            IpcCommand::MoveWindowToMonitorLeft => {
                self.move_to_monitor(None, monitor_to_left, "left")
            }
            IpcCommand::MoveWindowToMonitorRight => {
                self.move_to_monitor(None, monitor_to_right, "right")
            }
            IpcCommand::MoveWindowToMonitorUp => self.move_to_monitor(None, monitor_above, "up"),
            IpcCommand::MoveWindowToMonitorDown => {
                self.move_to_monitor(None, monitor_below, "down")
            }
            IpcCommand::Resize { delta } => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.resize_focused_column(delta);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Resized column by {}", delta);
                })
            }
            IpcCommand::Scroll { delta } => self.handle_scroll(delta),
            IpcCommand::QueryWorkspace => self.handle_query_workspace(),
            IpcCommand::QueryFocused => self.handle_query_focused(),
            IpcCommand::QueryHotkeys => self.handle_query_hotkeys(),
            IpcCommand::Refresh => self.handle_refresh(),
            IpcCommand::Apply
                if self.paused && self.pending_idle_layout_reapply =>
            {
                IpcResponse::ApplyPending {
                    message: "Layout application remains pending while tiling is paused"
                        .to_string(),
                }
            }
            IpcCommand::Apply => match self.apply_layout() {
                Ok(LayoutApplyOutcome::Completed) => {
                    info!("Applied layout");
                    IpcResponse::Ok
                }
                Ok(LayoutApplyOutcome::DeferredByRecoveryBarrier) => IpcResponse::ApplyPending {
                    message: "Layout application remains pending while recovery animation placement finishes"
                        .to_string(),
                },
                Err(error) => IpcResponse::error(format!("Failed to apply layout: {}", error)),
            },
            IpcCommand::Reload => self.handle_reload(),
            IpcCommand::TogglePause => {
                if let Err(e) = self.toggle_pause("IPC toggle") {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                IpcResponse::Ok
            }
            IpcCommand::ReleaseAllWindows => match self.release_all_windows() {
                Ok(()) => IpcResponse::Ok,
                Err(e) => IpcResponse::error(format!("Failed to release all windows: {}", e)),
            },
            IpcCommand::ToggleIgnore => self.toggle_ignore(),
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
                    ws.ensure_focused_visible_animated(vw);
                    info!("Set column width fraction to {:.3}", fraction);
                })
            }
            IpcCommand::CenterColumn => self.execute_workspace_command(false, false, |ws, vw| {
                ws.center_focused_column_animated(vw);
                info!("Centered focused column");
            }),
            IpcCommand::MaximizeColumn => self.execute_workspace_command(true, false, |ws, vw| {
                let entering = ws.toggle_maximize_column(vw);
                ws.center_focused_column_animated(vw);
                info!("Maximize column: {}", if entering { "on" } else { "off" });
            }),
            IpcCommand::EqualizeColumnWidths => {
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.equalize_column_widths(vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Equalized column widths");
                })
            }
            IpcCommand::CycleWidth
            | IpcCommand::CycleHeight
            | IpcCommand::CycleWidthUp
            | IpcCommand::CycleWidthDown
            | IpcCommand::CycleHeightUp
            | IpcCommand::CycleHeightDown => self.handle_preset_cycle(cmd),
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
            IpcCommand::MoveToWorkspacePrev => self.handle_move_to_workspace_relative(false),
            IpcCommand::MoveToWorkspaceNext => self.handle_move_to_workspace_relative(true),
            IpcCommand::SwitchWorkspace { index } => self.handle_switch_workspace(index),
            IpcCommand::SwitchWorkspaceOnMonitor {
                monitor_device_name,
                index,
            } => self.handle_switch_workspace_on_monitor(&monitor_device_name, index),
            IpcCommand::MoveToWorkspace { index } => self.handle_move_to_workspace(index),
            IpcCommand::HealthCheck => self.handle_health_check(),
            IpcCommand::GetAutoStart => {
                match leopardwm_platform_win32::autostart::get_autostart() {
                    Ok(enabled) => IpcResponse::AutoStartState { enabled },
                    Err(e) => IpcResponse::error(format!("Failed to read auto-start state: {}", e)),
                }
            }
            IpcCommand::SetAutoStart { enabled } => self.handle_set_auto_start(enabled),
            IpcCommand::QueryWorkspaceState => IpcResponse::error(
                "QueryWorkspaceState must be handled by the IPC server, not the main command \
                 loop — this is an internal routing bug.",
            ),
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
            IpcCommand::ToggleTabbed => self.execute_workspace_command(true, false, |ws, _vw| {
                ws.toggle_focused_column_tabbed_mode();
                info!("Toggled tabbed mode on focused column");
            }),
            IpcCommand::SetActiveTab { column, tab } => self.handle_set_active_tab(column, tab),
        };
        if let Some(before) = focus_before {
            if let Some(hwnd) = self.previous_focused_hwnd {
                if Some(hwnd) != before {
                    leopardwm_platform_win32::warp_cursor_to_window(hwnd);
                }
            }
        }
        resp
    }

    fn handle_scroll(&mut self, delta: f64) -> IpcResponse {
        #[cfg(not(test))]
        let cursor_pos = leopardwm_platform_win32::get_cursor_pos();
        #[cfg(test)]
        let cursor_pos: Option<(i32, i32)> = None;

        let viewport = self.layout_viewport(self.focused_monitor);
        self.execute_workspace_command(false, false, move |ws, vw| {
            let mut scrolled = false;
            if let Some((cx, cy)) = cursor_pos {
                let rects = ws.row_rects(viewport);
                for (idx, r) in rects.iter().enumerate() {
                    if r.contains_point(cx, cy) {
                        ws.scroll_row_by(idx, delta, vw);
                        scrolled = true;
                        break;
                    }
                }
            }
            if !scrolled {
                ws.scroll_by(delta, vw);
            }
            info!("Scrolled by {}", delta);
        })
    }

    fn handle_preset_cycle(&mut self, cmd: IpcCommand) -> IpcResponse {
        match cmd {
            IpcCommand::CycleWidth => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width(&presets, vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Cycled column width");
                })
            }
            IpcCommand::CycleHeight => {
                let presets = self.config.layout.height_presets.clone();
                self.execute_workspace_command(true, false, |ws, _vw| {
                    ws.cycle_height(&presets);
                    info!("Cycled window height");
                })
            }
            IpcCommand::CycleWidthUp => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_up(&presets, vw);
                    ws.ensure_focused_visible_animated(vw);
                    info!("Cycled column width up");
                })
            }
            IpcCommand::CycleWidthDown => {
                let presets = self.config.layout.width_presets.clone();
                self.execute_workspace_command(true, false, |ws, vw| {
                    ws.cycle_width_down(&presets, vw);
                    ws.ensure_focused_visible_animated(vw);
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
            _ => unreachable!(),
        }
    }

    fn focus_monitor(
        &mut self,
        select: fn(&[MonitorInfo], MonitorId) -> Option<&MonitorInfo>,
        dir: &str,
    ) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = select(&monitors, self.focused_monitor) {
            let target_id = target.id;
            self.focused_monitor = target_id;
            info!("Focused monitor {} -> {}", dir, target_id);
            if let Err(e) = self.apply_layout() {
                return IpcResponse::error(format!("Failed to apply layout: {}", e));
            }
            self.sync_foreground_window();
        } else {
            info!("No monitor {}", dir);
        }
        IpcResponse::Ok
    }

    fn handle_query_focused(&mut self) -> IpcResponse {
        if let Some(workspace) = self.focused_workspace() {
            IpcResponse::FocusedWindow {
                window_id: workspace.focused_window(),
                row_index: workspace.focused_row(),
                column_index: workspace.focused_column_index(),
                window_index: workspace.focused_window_index_in_column(),
                is_floating: false,
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    fn handle_refresh(&mut self) -> IpcResponse {
        match self.enumerate_and_add_windows() {
            Ok(added) => {
                let prune = self.prune_stale_windows();
                info!("Refreshed: added {} new windows across all monitors", added);
                self.complete_refresh_layout(prune)
            }
            Err(e) => IpcResponse::error(format!("Failed to enumerate windows: {}", e)),
        }
    }

    pub(crate) fn complete_refresh_layout(
        &mut self,
        prune: crate::helpers::StalePruneLayout,
    ) -> IpcResponse {
        match prune {
            crate::helpers::StalePruneLayout::Applied => IpcResponse::Ok,
            crate::helpers::StalePruneLayout::Unchanged => {
                if let Err(e) = self.apply_layout() {
                    IpcResponse::error(format!("Failed to apply layout: {}", e))
                } else {
                    IpcResponse::Ok
                }
            }
            crate::helpers::StalePruneLayout::Failed(e) => {
                IpcResponse::error(format!("Failed to apply layout: {}", e))
            }
        }
    }

    fn handle_reload(&mut self) -> IpcResponse {
        self.reload_config(true)
    }

    pub(crate) fn reload_config(&mut self, log_validation_warnings: bool) -> IpcResponse {
        match Config::load() {
            Ok(mut new_config) => {
                for warning in new_config.validate() {
                    if log_validation_warnings {
                        warn!("Config: {} - {}", warning.field, warning.message);
                    }
                }
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

    fn close_window_target(&self) -> Option<u64> {
        self.focused_floating_window()
            .or_else(|| self.focused_workspace().and_then(|ws| ws.focused_window()))
    }

    fn handle_close_window(&mut self) -> IpcResponse {
        if let Some(hwnd) = self.close_window_target() {
            if let Err(e) = leopardwm_platform_win32::close_window(hwnd) {
                return IpcResponse::error(format!("Failed to close window: {}", e));
            }
            info!("Closed window {}", hwnd);
        } else {
            info!("No focused window to close");
        }
        IpcResponse::Ok
    }

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

    fn handle_toggle_fullscreen(&mut self) -> IpcResponse {
        self.toggle_fullscreen_with(Workspace::toggle_fullscreen)
    }

    fn toggle_fullscreen_with(
        &mut self,
        toggle: impl FnOnce(&mut Workspace) -> bool,
    ) -> IpcResponse {
        let resp = self.execute_workspace_command(true, false, |ws, _vw| {
            let entering = toggle(ws);
            info!("Fullscreen: {}", if entering { "on" } else { "off" });
        });
        if self
            .focused_workspace()
            .is_some_and(|ws| ws.is_fullscreen())
        {
            self.hide_border();
        } else {
            self.sync_foreground_window();
        }
        resp
    }

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

    fn handle_workspace_prev_next(&mut self, cmd: IpcCommand) -> IpcResponse {
        const COUNT: usize = 9;
        let monitor = self.focused_monitor;
        let current = self.active_workspace_idx(monitor);
        let forward = match cmd {
            IpcCommand::WorkspacePrev => false,
            IpcCommand::WorkspaceNext => true,
            _ => unreachable!(),
        };
        let target = (1..COUNT).find_map(|distance| {
            let idx = if forward {
                (current + distance) % COUNT
            } else {
                (current + COUNT - distance) % COUNT
            };
            let occupied = || {
                self.workspaces
                    .get(&monitor)
                    .and_then(|workspaces| workspaces.get(idx))
                    .is_some_and(|ws| {
                        ws.all_window_ids()
                            .iter()
                            .any(|hwnd| !self.sticky_windows.contains(hwnd))
                    })
            };
            (!self.config.behavior.skip_empty_workspaces || occupied()).then_some(idx)
        });
        let Some(target) = target else {
            return IpcResponse::Ok;
        };
        self.handle_switch_workspace_with_direction((target + 1) as u8, Some(forward))
    }

    fn handle_move_to_workspace_relative(&mut self, forward: bool) -> IpcResponse {
        self.handle_move_to_workspace(self.relative_workspace_index(forward))
    }

    /// The 1-based index of the workspace next to the active one, wrapping.
    fn relative_workspace_index(&self, forward: bool) -> u8 {
        const COUNT: usize = 9;
        let current = self.active_workspace_idx(self.focused_monitor);
        let target = if forward {
            (current + 1) % COUNT
        } else {
            (current + COUNT - 1) % COUNT
        };
        (target + 1) as u8
    }

    fn focus_vertical(&mut self, up: bool) -> IpcResponse {
        let empty_workspace = self.focused_workspace().is_some_and(|ws| ws.is_empty());
        if self.config.behavior.workspace_edge_wrap
            && (empty_workspace || self.focus_at_vertical_edge(up))
        {
            return self.handle_workspace_prev_next(if up {
                IpcCommand::WorkspacePrev
            } else {
                IpcCommand::WorkspaceNext
            });
        }
        self.execute_workspace_command(false, true, |ws, _vw| {
            if up {
                ws.focus_up();
            } else {
                ws.focus_down();
            }
            info!(
                "Focus {} -> window {}",
                if up { "up" } else { "down" },
                ws.focused_window_index_in_column()
            );
        })
    }

    /// Vertical move (`move_window_up`/`move_window_down` within a column). With
    /// `workspace_edge_wrap` on, a press at the workspace's top/bottom edge moves
    /// the focused window to the adjacent workspace instead of no-oping.
    fn move_window_vertical(&mut self, up: bool) -> IpcResponse {
        if self.config.behavior.workspace_edge_wrap && self.focus_at_vertical_edge(up) {
            return self.handle_move_to_workspace_relative(!up);
        }
        let move_across_rows = self.config.behavior.move_window_across_rows;
        self.execute_workspace_command(true, true, move |ws, _vw| {
            if up {
                ws.move_window_up(move_across_rows);
            } else {
                ws.move_window_down(move_across_rows);
            }
            info!("Moved window {} in column", if up { "up" } else { "down" });
        })
    }

    /// Whether vertical focus/move is pinned at the top (`up = true`) or bottom
    /// (`up = false`) of the focused workspace, so it can't move within it.
    fn focus_at_vertical_edge(&self, up: bool) -> bool {
        self.focused_workspace().is_some_and(|ws| {
            if up {
                ws.at_workspace_top()
            } else {
                ws.at_workspace_bottom()
            }
        })
    }

    /// Moves `window`, or the focused window when `None`.
    fn move_to_monitor(
        &mut self,
        window: Option<u64>,
        select: fn(&[MonitorInfo], MonitorId) -> Option<&MonitorInfo>,
        dir: &str,
    ) -> IpcResponse {
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        if let Some(target) = select(&monitors, self.focused_monitor) {
            let target_id = target.id;
            let moved = match window {
                Some(hwnd) => self.move_window_to_monitor_transactional(hwnd, target_id),
                None => self.move_focused_window_to_monitor_transactional(target_id),
            };
            match moved {
                Ok(Some(hwnd)) => {
                    info!("Moved window {} to monitor {}", hwnd, target_id);
                    if let Err(e) = self.apply_layout() {
                        return IpcResponse::error(format!("Failed to apply layout: {}", e));
                    }
                    self.sync_foreground_window();
                }
                Ok(None) => info!("No focused window to move"),
                Err(message) => return IpcResponse::error(message),
            }
        } else {
            info!("No monitor {}", dir);
        }
        IpcResponse::Ok
    }

    fn handle_query_workspace(&mut self) -> IpcResponse {
        let active_idx = self.active_workspace_idx(self.focused_monitor);
        let active_workspace_name = self.config.workspaces.name_for(active_idx);
        let focused_rows = self.focused_layout_rows();
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
                focused_row: workspace.focused_row(),
                rows: focused_rows,
            }
        } else {
            IpcResponse::error("No focused workspace")
        }
    }

    fn handle_query_hotkeys(&self) -> IpcResponse {
        let catalog = leopardwm_ipc::hotkeys::hotkey_catalog();
        let mut bindings_by_action: HashMap<String, Vec<String>> = HashMap::new();
        let resolved = resolve_hotkeys(&self.config.hotkeys);
        for entry in resolved.bindings {
            if entry.executable {
                bindings_by_action
                    .entry(entry.action_id)
                    .or_default()
                    .push(entry.binding);
            }
        }

        let mut hotkeys: Vec<_> = catalog
            .into_iter()
            .map(|action| {
                let mut bindings = bindings_by_action.remove(&action.id).unwrap_or_default();
                bindings.sort();
                HotkeyBindingInfo {
                    action_id: action.id,
                    label: action.label,
                    group: action.group.to_string(),
                    enabled: !bindings.is_empty(),
                    bindings,
                }
            })
            .collect();

        let mut extra_actions: Vec<_> = bindings_by_action.into_iter().collect();
        extra_actions.sort_by(|(left, _), (right, _)| left.cmp(right));
        hotkeys.extend(extra_actions.into_iter().map(|(action_id, mut bindings)| {
            bindings.sort();
            HotkeyBindingInfo {
                label: humanize_action_id(&action_id),
                group: "Other".to_string(),
                action_id,
                enabled: !bindings.is_empty(),
                bindings,
            }
        }));
        IpcResponse::HotkeyList {
            hotkeys,
            scroll_modifier: self.config.hotkeys.scroll_modifier.clone(),
            issues: resolved.issues,
        }
    }

    fn handle_query_all_windows(&mut self) -> IpcResponse {
        let mut windows = Vec::new();

        let focused_hwnd = self.focused_workspace().and_then(|ws| ws.focused_window());

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
                for (row_idx, row) in workspace.rows().iter().enumerate() {
                    for (col_idx, column) in row.columns().iter().enumerate() {
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
                                .then(|| {
                                    workspace.compute_placements(self.layout_viewport(*monitor_id))
                                })
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
                                row_index: Some(row_idx),
                                column_index: Some(col_idx),
                                window_index: Some(win_idx),
                                monitor_id: *monitor_id as i64,
                                is_floating: false,
                                is_focused: Some(window_id) == focused_hwnd,
                            });
                        }
                    }
                }

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
                        row_index: None,
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

    fn handle_toggle_floating(&mut self) -> IpcResponse {
        let viewport = self.focused_viewport();
        if let Some(wid) = self
            .focused_workspace_mut()
            .and_then(|ws| ws.toggle_floating(viewport))
        {
            self.restore_snap_for_window(wid);
            info!("Toggled window {} to floating", wid);
        }
        self.apply_floating_toggle()
    }

    fn handle_unfloat_window(&mut self, hwnd: u64) -> IpcResponse {
        if self
            .focused_workspace_mut()
            .is_some_and(|ws| ws.unfloat_window(hwnd))
        {
            self.disable_snap_for_window(hwnd);
            info!("Unfloated window {} back to tiling", hwnd);
        }
        self.apply_floating_toggle()
    }

    fn apply_floating_toggle(&mut self) -> IpcResponse {
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        self.sync_foreground_window();
        IpcResponse::Ok
    }

    fn handle_switch_workspace(&mut self, index: u8) -> IpcResponse {
        self.handle_switch_workspace_with_direction(index, None)
    }

    fn handle_switch_workspace_on_monitor(
        &mut self,
        monitor_device_name: &str,
        index: u8,
    ) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        let Some(monitor) = self
            .monitors
            .values()
            .find(|monitor| monitor.device_name == monitor_device_name)
            .map(|monitor| monitor.id)
        else {
            return IpcResponse::error(format!("Monitor not found: {}", monitor_device_name));
        };

        self.switch_workspace_on_monitor(monitor, index, None, true)
    }

    fn handle_switch_workspace_with_direction(
        &mut self,
        index: u8,
        relative_forward: Option<bool>,
    ) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        self.switch_workspace_on_monitor(self.focused_monitor, index, relative_forward, false)
    }

    fn switch_workspace_on_monitor(
        &mut self,
        monitor: MonitorId,
        index: u8,
        relative_forward: Option<bool>,
        focus_target_monitor: bool,
    ) -> IpcResponse {
        self.preserve_deferred_floating_moves();
        let idx = (index - 1) as usize;
        let target_was_focused = monitor == self.focused_monitor;
        // A switch initiated outside the overlay (hotkey, CLI) dismisses
        // an open overview; overlay-initiated switches hid it already. An
        // explicit switch to another monitor closes toward the overview's
        // current workspace, not a workspace slot on the destination monitor.
        if self.overview_open {
            let overview_target = (monitor == self.focused_monitor).then_some(idx);
            self.hide_overview_animated(overview_target);
        }
        if focus_target_monitor {
            self.focused_monitor = monitor;
        }
        let current_idx = self.active_workspace_idx(monitor);
        if idx == current_idx {
            if focus_target_monitor {
                if let Err(e) = self.apply_layout() {
                    return IpcResponse::error(format!("Failed to apply layout: {}", e));
                }
                if !target_was_focused {
                    self.restore_workspace_floating_focus(monitor, idx);
                }
                self.sync_foreground_window();
            }
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
        let leaving_focus =
            leopardwm_platform_win32::get_foreground_window().or(self.previous_focused_hwnd);
        #[cfg(test)]
        let leaving_focus = self.previous_focused_hwnd;
        // A focused sticky (pinned) window keeps focus across the switch:
        // capture that BEFORE the workspace changes. Any stale pending
        // refocus from a previous (aborted) switch is dropped here.
        self.pending_sticky_refocus = None;
        let sticky_focus = leaving_focus.filter(|hwnd| self.sticky_windows.contains(hwnd));
        if target_was_focused {
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
        }

        if let Some(drag) = self.drag_state.take() {
            if drag.removed_from_source
                && drag.is_tiled
                && leopardwm_platform_win32::is_valid_window(drag.hwnd)
            {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&drag.source_monitor)
                    .and_then(|v| v.get_mut(drag.source_workspace_idx))
                {
                    let _ = ws.insert_window(drag.hwnd, None);
                }
            }
            for ws_vec in self.workspaces.values_mut() {
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
                if !self.is_application_fullscreen(*wid) {
                    #[cfg(not(test))]
                    if !self.paused {
                        let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
                    }
                }
            }
        }
        // The slide installed below carries a deferred focus border.
        // Aborting here would paint it before that slide exists.
        self.abort_active_ghost_transition();

        let slide_height = self
            .monitors
            .get(&monitor)
            .map(|m| m.work_area.height)
            .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
        // Positive offset = new workspace enters from below (scrolling up).
        let y_offset = match relative_forward {
            Some(true) => slide_height,
            Some(false) => -slide_height,
            None if idx > current_idx => slide_height,
            None => -slide_height,
        };

        let mut old_placements = self.workspace_placements(monitor, current_idx);

        // Overview snapshot mode: grab the outgoing windows NOW, while
        // they are still on screen, so their cards show a real frame
        // after they move offscreen below. Skipped otherwise (PrintWindow
        // per window is not free).
        if self.config.overview.render == crate::config::OverviewRender::Snapshot {
            for placement in &old_placements {
                let _ = leopardwm_platform_win32::snapshot::snapshot_capture(placement.window_id);
            }
        }

        self.ensure_workspace_exists(monitor, idx);

        // A new explicit destination supersedes earlier switch-focus and
        // last-window departure guards before layout application can fail.
        self.pending_workspace_switch_focus = None;
        self.pending_last_window_departure = None;

        self.active_workspace.insert(monitor, idx);

        self.rehome_sticky_windows();

        let viewport_width = self.viewport_width_for(monitor);
        if let Some(workspace) = self.focused_workspace_mut() {
            workspace.commit_pending_min_size_clears();
            workspace.reconcile_scroll_bounds(viewport_width);
        }

        let mut new_placements = self.workspace_placements(monitor, idx);

        // Keep sticky windows out of the slide animation so they sit
        // still while the rest of the layout scrolls past.
        old_placements.retain(|p| {
            !self.sticky_windows.contains(&p.window_id)
                && !self.is_application_fullscreen(p.window_id)
        });
        new_placements.retain(|p| {
            !self.sticky_windows.contains(&p.window_id)
                && !self.is_application_fullscreen(p.window_id)
        });

        let mut start_rects = std::collections::HashMap::new();
        let mut exit_rects = std::collections::HashMap::new();

        for placement in &new_placements {
            let rect = placement.rect;
            start_rects.insert(
                placement.window_id,
                leopardwm_core_layout::Rect::new(
                    rect.x,
                    rect.y + y_offset,
                    rect.width,
                    rect.height,
                ),
            );
        }

        for placement in &old_placements {
            if placement.visibility != Visibility::Visible {
                continue;
            }
            let rect = placement.rect;
            start_rects.insert(placement.window_id, rect);
            exit_rects.insert(
                placement.window_id,
                leopardwm_core_layout::Rect::new(
                    rect.x,
                    rect.y - y_offset,
                    rect.width,
                    rect.height,
                ),
            );
        }

        // Animate only when there is something to move and motion isn't reduced.
        // Otherwise hide the leaving windows immediately: reduce_motion (e.g. on
        // battery) skips the transition, and the transition is the only path that
        // would otherwise move them off-screen, so without this they linger as
        // ghosts on top of the new workspace.
        let animating = !start_rects.is_empty() && !self.reduce_motion;
        for placement in &old_placements {
            if !animating || placement.visibility != Visibility::Visible {
                #[cfg(not(test))]
                if !self.paused {
                    let _ = leopardwm_platform_win32::move_window_offscreen(placement.window_id);
                }
            }
        }
        if animating {
            let duration = self.config.animation.workspace_switch_duration_ms;
            self.start_workspace_switch_transition(start_rects, exit_rects, duration);
            if let Some(transition) = self.layout_transition.as_mut() {
                transition.defer_focus_border = true;
            }
            // The previous workspace's border would otherwise stay put while
            // both workspaces slide. The incoming border waits until completion.
            self.hide_border();
        } else {
            // No replacement slide. A deferred border lands now; a
            // non-deferred transition only clears.
            self.abort_layout_transition();
        }

        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        // Hide the now-inactive workspace's windows from the taskbar (cloak).
        // Mid-slide windows are skipped here and cloaked when the transition
        // settles (see tick_animations).
        self.sync_taskbar_buttons();
        self.restore_workspace_floating_focus(monitor, idx);
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
        if self.previous_focused_hwnd.is_none() {
            if let Some(source_hwnd) = leaving_focus.filter(|hwnd| {
                self.workspaces
                    .get(&monitor)
                    .and_then(|workspaces| workspaces.get(current_idx))
                    .is_some_and(|workspace| workspace.contains_window(*hwnd))
            }) {
                self.pending_workspace_switch_focus = Some(PendingWorkspaceSwitchFocus {
                    monitor,
                    source_workspace: current_idx,
                    destination_workspace: idx,
                    source_hwnd,
                    set_at: std::time::Instant::now(),
                    armed_at_event_time_ms: leopardwm_platform_win32::current_event_time_ms(),
                });
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

    fn restore_workspace_floating_focus(&mut self, monitor: MonitorId, idx: usize) {
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
    }

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
                self.workspaces
                    .get(&monitor)
                    .and_then(|v| v.get(current_idx))
                    .filter(|ws| ws.contains_window(hwnd))
                    .map(|_| hwnd)
            });
            match os_focus.or(tiled_focus) {
                Some(hwnd) => hwnd,
                None => return IpcResponse::Ok,
            }
        };

        self.move_window_to_workspace(monitor, current_idx, focused_hwnd, index, true)
    }

    fn move_window_from_active_workspace(&mut self, hwnd: u64, index: u8) -> IpcResponse {
        let monitor = self.focused_monitor;
        let current_idx = self.active_workspace_idx(monitor);
        self.move_window_to_workspace(monitor, current_idx, hwnd, index, true)
    }

    pub(crate) fn move_window_to_workspace(
        &mut self,
        monitor: MonitorId,
        current_idx: usize,
        focused_hwnd: u64,
        index: u8,
        sync_focus: bool,
    ) -> IpcResponse {
        if !(1..=9).contains(&index) {
            return IpcResponse::error("Workspace index must be 1-9");
        }
        let idx = usize::from(index - 1);
        if idx == current_idx
            || !self
                .workspaces
                .get(&monitor)
                .and_then(|v| v.get(current_idx))
                .is_some_and(|ws| ws.contains_window(focused_hwnd))
        {
            return IpcResponse::Ok;
        }
        let snapshot = self.snapshot_layout();

        self.ensure_workspace_exists(monitor, idx);

        let is_floating = self
            .workspaces
            .get(&monitor)
            .and_then(|v| v.get(current_idx))
            .is_some_and(|ws| ws.is_floating(focused_hwnd));

        // Capture the tiled column width before removal so the window re-tiles
        // at its chosen width on the target workspace instead of the default.
        let tiled_width = if is_floating {
            None
        } else {
            self.tiled_column_width(monitor, current_idx, focused_hwnd)
        };

        // Capture the source column + sibling so a later move back can restore
        // the window to its original position.
        let source_origin = if is_floating {
            None
        } else {
            self.tiled_column_origin(monitor, current_idx, focused_hwnd)
        };

        // If this window is returning to the workspace it was moved out of,
        // resolve where it should land: rejoin a surviving sibling's column,
        // else a new column at the remembered (clamped) index. Otherwise the
        // normal insert (right of the focused column) applies.
        enum Landing {
            Default,
            Stack(usize, usize),
            NewColumn(usize, usize),
        }
        let landing = if is_floating {
            Landing::Default
        } else {
            match self
                .move_origins
                .get(&focused_hwnd)
                .copied()
                .filter(|o| o.monitor == monitor && o.ws_idx == idx)
            {
                Some(o) => {
                    let sibling_loc = o.sibling.and_then(|s| {
                        self.workspaces
                            .get(&monitor)?
                            .get(idx)?
                            .find_window_location_rc(s)
                            .map(|(r, c, _)| (r, c))
                    });
                    match sibling_loc {
                        Some((r, c)) => Landing::Stack(r, c),
                        None => Landing::NewColumn(o.row, o.column),
                    }
                }
                None => Landing::Default,
            }
        };

        // Workspace state owns the floating window's canonical position.
        let floating_rect = if is_floating {
            self.workspaces
                .get(&monitor)
                .and_then(|v| v.get(current_idx))
                .and_then(|ws| {
                    ws.floating_windows()
                        .iter()
                        .find(|f| f.id == focused_hwnd)
                        .map(|f| f.rect)
                })
        } else {
            None
        };

        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(current_idx))
        {
            if is_floating {
                workspace.remove_floating(focused_hwnd);
            } else if let Err(e) = workspace.remove_window(focused_hwnd) {
                return IpcResponse::error(format!("Failed to remove window: {}", e));
            }
        }

        self.ensure_workspace_exists(monitor, idx);

        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(idx))
        {
            if is_floating {
                let rect =
                    floating_rect.unwrap_or(leopardwm_core_layout::Rect::new(0, 0, 800, 600));
                if let Err(e) = workspace.add_floating(focused_hwnd, rect) {
                    if let Some(src_ws) = self
                        .workspaces
                        .get_mut(&monitor)
                        .and_then(|v| v.get_mut(current_idx))
                    {
                        let _ = src_ws.add_floating(focused_hwnd, rect);
                    }
                    return IpcResponse::error(format!("Failed to move floating window: {}", e));
                }
            } else {
                let result = match landing {
                    Landing::Stack(r, col) => {
                        workspace.insert_window_in_row_column(focused_hwnd, r, col)
                    }
                    Landing::NewColumn(r, col) => {
                        workspace.insert_window_at_row_column(focused_hwnd, r, tiled_width, col)
                    }
                    Landing::Default => workspace.insert_window(focused_hwnd, tiled_width),
                };
                if let Err(e) = result {
                    // Rollback: restore the window to its original source position
                    // so a failed move leaves the layout exactly as it was.
                    if let Some(src_ws) = self
                        .workspaces
                        .get_mut(&monitor)
                        .and_then(|v| v.get_mut(current_idx))
                    {
                        let rejoin = source_origin
                            .and_then(|(_, _, sib)| sib)
                            .and_then(|s| src_ws.find_window_location_rc(s))
                            .map(|(r, c, _)| (r, c));
                        let _ = match (rejoin, source_origin) {
                            (Some((r, c)), _) => {
                                src_ws.insert_window_in_row_column(focused_hwnd, r, c)
                            }
                            (None, Some((r, col, _))) => src_ws.insert_window_at_row_column(
                                focused_hwnd,
                                r,
                                tiled_width,
                                col,
                            ),
                            (None, None) => src_ws.insert_window(focused_hwnd, tiled_width),
                        };
                    }
                    return IpcResponse::error(format!(
                        "Failed to add window to target workspace: {}",
                        e
                    ));
                }
                // A rejoin (Stack) doesn't move focus; focus the window so it is
                // current when the user next switches to the target workspace,
                // matching the default insert.
                let _ = workspace.focus_window(focused_hwnd);
            }
        }

        // Record where the window came from so moving it back restores its
        // column; a floating move clears any stale tiled origin instead.
        if let Some((row, column, sibling)) = source_origin {
            self.move_origins.insert(
                focused_hwnd,
                crate::state::MoveOrigin {
                    monitor,
                    ws_idx: current_idx,
                    row,
                    column,
                    sibling,
                },
            );
        } else if is_floating {
            self.move_origins.remove(&focused_hwnd);
        }

        if idx != self.active_workspace_idx(monitor) {
            if self.config.overview.render == crate::config::OverviewRender::Snapshot {
                let _ = leopardwm_platform_win32::snapshot::snapshot_capture(focused_hwnd);
            }
            if !self.is_application_fullscreen(focused_hwnd) {
                #[cfg(not(test))]
                let _ = leopardwm_platform_win32::move_window_offscreen(focused_hwnd);
            }
        }

        let viewport_width = self.viewport_width_for(monitor);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(current_idx))
        {
            workspace.ensure_focused_visible_animated(viewport_width);
        }

        let started_transition = self.start_layout_transition(snapshot);
        if started_transition && !sync_focus {
            if let Some(transition) = &mut self.layout_transition {
                transition.suppress_landing_focus_resync = true;
            }
        }
        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("Failed to apply layout: {}", e));
        }
        self.sync_taskbar_buttons();
        if sync_focus {
            self.sync_foreground_window();
        }
        info!("Moved window {} to workspace {}", focused_hwnd, index);
        IpcResponse::Ok
    }

    fn handle_health_check(&mut self) -> IpcResponse {
        let uptime = self.start_time.elapsed().as_secs();
        let total_windows: usize = self
            .workspaces
            .values()
            .flat_map(|ws_vec| ws_vec.iter())
            .map(|ws| ws.window_count() + ws.floating_count())
            .sum();
        let (legacy, records) = elevation_blocked_health_views(&self.elevation_blocked);
        IpcResponse::HealthInfo {
            healthy: true,
            uptime_seconds: uptime,
            total_windows,
            monitors: self.monitors.len(),
            paused: self.paused,
            thumbnail_register_balance:
                leopardwm_platform_win32::thumbnail::current_register_balance(),
            elevation_blocked_windows: legacy,
            daemon_integrity: leopardwm_platform_win32::current_process_integrity(),
            elevation_blocked_records: Some(records),
            native_swipes: Some(self.native_swipes.clone()),
            daemon_log: self
                .daemon_log
                .as_ref()
                .map(crate::daemon_log::LogHealth::status),
        }
    }

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

    fn handle_set_active_tab(&mut self, column: usize, tab: usize) -> IpcResponse {
        self.handle_set_active_tab_with_restore(
            column,
            tab,
            leopardwm_platform_win32::restore_window_no_activate,
        )
    }

    fn handle_set_active_tab_with_restore(
        &mut self,
        column: usize,
        tab: usize,
        restore: impl FnOnce(u64) -> Result<(), leopardwm_platform_win32::Win32Error>,
    ) -> IpcResponse {
        let (row, target, was_minimized) = match self.focused_workspace() {
            None => return IpcResponse::error("No focused workspace"),
            Some(workspace) => {
                let row = workspace.focused_row();
                let Some(target_column) = workspace.column(column) else {
                    return IpcResponse::error(format!(
                        "set_active_tab failed: {}",
                        LayoutError::ColumnOutOfBounds(
                            column,
                            workspace.column_count().saturating_sub(1)
                        )
                    ));
                };
                let target = match target_column.get(tab) {
                    Some(target) if target_column.is_tabbed() => target,
                    _ => {
                        return IpcResponse::error(format!(
                            "set_active_tab failed: {}",
                            LayoutError::WindowIndexOutOfBounds(
                                tab,
                                column,
                                target_column.len().saturating_sub(1)
                            )
                        ));
                    }
                };
                (row, target, workspace.is_minimized(target))
            }
        };

        if let Err(e) = restore(target) {
            return IpcResponse::error(format!("restore tab failed: {}", e));
        }

        self.commit_active_tab_after_restore(row, column, tab, target, was_minimized)
    }

    /// The tab strip's Untab names its tab explicitly, so it skips the
    /// floating policy but keeps the fullscreen exit `ExpelToRight` gets
    /// from `handle_command`.
    pub(crate) fn untab_tiled_focus(&mut self) -> IpcResponse {
        self.exit_fullscreen_if_active();
        self.expel_tiled_focus_to_right()
    }

    fn expel_tiled_focus_to_right(&mut self) -> IpcResponse {
        self.execute_workspace_command(true, true, |ws, vw| {
            ws.expel_to_right();
            ws.ensure_focused_visible_animated(vw);
            info!("Expelled window to right");
        })
    }

    pub(crate) fn handle_tab_action_activation_with_restore(
        &mut self,
        monitor: isize,
        workspace_idx: usize,
        row: usize,
        column: usize,
        tab: usize,
        restore: impl FnOnce(u64) -> Result<(), leopardwm_platform_win32::Win32Error>,
    ) -> Option<IpcResponse> {
        if self.active_workspace_idx(monitor) != workspace_idx {
            return None;
        }
        let (target, was_minimized) = {
            let workspace = self.workspaces.get(&monitor)?.get(workspace_idx)?;
            let target_column = workspace.column_for_row(row, column)?;
            let target = target_column
                .get(tab)
                .filter(|_| target_column.is_tabbed())?;
            (target, workspace.is_minimized(target))
        };

        if let Err(e) = restore(target) {
            return Some(IpcResponse::error(format!("restore tab failed: {}", e)));
        }

        let workspace = self.workspaces.get_mut(&monitor)?.get_mut(workspace_idx)?;
        if let Err(e) = workspace.set_focus_in_row(row, column, 0) {
            return Some(IpcResponse::error(format!(
                "set focus for tab click failed: {}",
                e
            )));
        }
        self.focused_monitor = monitor;
        Some(self.commit_active_tab_after_restore(row, column, tab, target, was_minimized))
    }

    fn commit_active_tab_after_restore(
        &mut self,
        row: usize,
        column: usize,
        tab: usize,
        target: u64,
        was_minimized: bool,
    ) -> IpcResponse {
        let should_sync_target = {
            let Some(workspace) = self.focused_workspace_mut() else {
                return IpcResponse::error("No focused workspace");
            };
            if was_minimized {
                workspace.mark_restored(target);
            }
            if let Err(e) = workspace.set_active_tab_in_row(row, column, tab) {
                return IpcResponse::error(format!("set_active_tab failed: {}", e));
            }
            workspace.focused_row() == row
                && workspace.focused_column_index() == column
                && workspace.focused_window() == Some(target)
        };

        if let Err(e) = self.apply_layout() {
            return IpcResponse::error(format!("apply_layout failed: {}", e));
        }
        if should_sync_target {
            self.pending_tab_focus = Some(crate::state::PendingTabFocus {
                monitor: self.focused_monitor,
                workspace_idx: self.active_workspace_idx(self.focused_monitor),
                row_idx: row,
                column_idx: column,
                tab_idx: tab,
                set_at: std::time::Instant::now(),
            });
        }
        self.sync_foreground_window();
        info!("Set active tab: row={}, column={}, tab={}", row, column, tab);
        IpcResponse::Ok
    }
}

#[cfg(test)]
mod focus_nav_tests {
    use super::is_focus_navigation;
    use leopardwm_ipc::IpcCommand;

    #[test]
    fn focus_navigation_covers_focus_and_monitor_commands() {
        for cmd in [
            IpcCommand::FocusLeft,
            IpcCommand::FocusRight,
            IpcCommand::FocusUp,
            IpcCommand::FocusDown,
            IpcCommand::FocusNext,
            IpcCommand::FocusPrev,
            IpcCommand::FocusStart,
            IpcCommand::FocusEnd,
            IpcCommand::FocusMonitorLeft,
            IpcCommand::FocusMonitorRight,
            IpcCommand::FocusMonitorUp,
            IpcCommand::FocusMonitorDown,
            IpcCommand::MoveWindowToMonitorLeft,
            IpcCommand::MoveWindowToMonitorRight,
            IpcCommand::MoveWindowToMonitorUp,
            IpcCommand::MoveWindowToMonitorDown,
        ] {
            assert!(is_focus_navigation(&cmd), "{cmd:?} should warp the cursor");
        }
    }

    #[test]
    fn focus_navigation_excludes_workspace_and_structural_commands() {
        for cmd in [
            IpcCommand::WorkspaceNext,
            IpcCommand::WorkspacePrev,
            IpcCommand::MoveToWorkspaceNext,
            IpcCommand::MoveToWorkspacePrev,
            IpcCommand::MoveToWorkspace { index: 3 },
            IpcCommand::MoveWindowLeft,
            IpcCommand::MoveWindowUp,
            IpcCommand::CloseWindow,
        ] {
            assert!(
                !is_focus_navigation(&cmd),
                "{cmd:?} should not warp the cursor"
            );
        }
    }
}

#[cfg(test)]
mod floating_policy_tests {
    use super::*;
    use crate::config::Config;
    use leopardwm_core_layout::Rect;
    use leopardwm_platform_win32::MonitorInfo;

    const FLOATING: u64 = 900;

    const IGNORED: [IpcCommand; 31] = [
        IpcCommand::FocusLeft,
        IpcCommand::FocusRight,
        IpcCommand::FocusUp,
        IpcCommand::FocusDown,
        IpcCommand::FocusNext,
        IpcCommand::FocusPrev,
        IpcCommand::FocusStart,
        IpcCommand::FocusEnd,
        IpcCommand::SetColumnWidth { fraction: 0.5 },
        IpcCommand::CycleWidth,
        IpcCommand::CycleWidthUp,
        IpcCommand::CycleWidthDown,
        IpcCommand::CycleHeight,
        IpcCommand::CycleHeightUp,
        IpcCommand::CycleHeightDown,
        IpcCommand::MaximizeColumn,
        IpcCommand::CenterColumn,
        IpcCommand::EqualizeColumnHeights,
        IpcCommand::ToggleTabbed,
        IpcCommand::MoveColumnLeft,
        IpcCommand::MoveColumnRight,
        IpcCommand::MoveColumnToStart,
        IpcCommand::MoveColumnToEnd,
        IpcCommand::MoveWindowLeft,
        IpcCommand::MoveWindowRight,
        IpcCommand::MoveWindowUp,
        IpcCommand::MoveWindowDown,
        IpcCommand::ConsumeFromLeft,
        IpcCommand::ConsumeFromRight,
        IpcCommand::ExpelToLeft,
        IpcCommand::ExpelToRight,
    ];

    fn monitor(id: MonitorId, x: i32) -> MonitorInfo {
        MonitorInfo {
            id,
            rect: Rect::new(x, 0, 1920, 1080),
            work_area: Rect::new(x, 0, 1920, 1040),
            is_primary: id == 1,
            device_name: format!("DISPLAY{id}"),
            scale_factor: 1.0,
        }
    }

    /// Three columns (100 | 200+201 | 300) focused on 201, with a floating
    /// window holding focus over them.
    fn floating_focus_state() -> AppState {
        floating_focus_state_on(vec![monitor(1, 0)])
    }

    fn floating_focus_state_on(monitors: Vec<MonitorInfo>) -> AppState {
        let mut state = AppState::new_with_config(Config::default(), monitors);
        assert!(state.paused);
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(400)).unwrap();
        ws.insert_window(200, Some(600)).unwrap();
        ws.insert_window_in_column(201, 1).unwrap();
        ws.insert_window(300, Some(800)).unwrap();
        ws.set_focus(1, 1).unwrap();
        ws.add_floating(FLOATING, Rect::new(100, 100, 500, 400))
            .unwrap();
        state.previous_focused_hwnd = Some(FLOATING);
        assert_eq!(state.focused_floating_window(), Some(FLOATING));
        state
    }

    #[derive(Debug, PartialEq)]
    struct StripSnapshot {
        columns: Vec<(Vec<u64>, i32, Vec<f64>, bool)>,
        focus: (usize, usize),
        fullscreen: Option<u64>,
    }

    fn strip_snapshot(state: &AppState) -> StripSnapshot {
        let ws = state.focused_workspace().unwrap();
        StripSnapshot {
            columns: ws
                .columns()
                .iter()
                .map(|c| {
                    (
                        c.windows().to_vec(),
                        c.width(),
                        c.height_weights().to_vec(),
                        c.is_tabbed(),
                    )
                })
                .collect(),
            focus: (
                ws.focused_column_index(),
                ws.focused_window_index_in_column(),
            ),
            fullscreen: ws.fullscreen_window_id(),
        }
    }

    #[test]
    fn floating_policy_groups_commands() {
        for cmd in [
            IpcCommand::CloseWindow,
            IpcCommand::ToggleFloating,
            IpcCommand::ToggleSticky,
            IpcCommand::ToggleFullscreen,
            IpcCommand::Resize { delta: 100 },
            IpcCommand::QueryFocused,
            IpcCommand::ScratchpadStash,
            IpcCommand::MoveToWorkspace { index: 2 },
            IpcCommand::MoveToWorkspacePrev,
            IpcCommand::MoveToWorkspaceNext,
            IpcCommand::MoveWindowToMonitorLeft,
            IpcCommand::MoveWindowToMonitorRight,
            IpcCommand::MoveWindowToMonitorUp,
            IpcCommand::MoveWindowToMonitorDown,
        ] {
            assert!(
                matches!(floating_policy(&cmd), FloatingPolicy::Target),
                "{cmd:?} should target the floating window"
            );
        }
        for cmd in IGNORED {
            assert!(
                matches!(floating_policy(&cmd), FloatingPolicy::Ignore),
                "{cmd:?} should be ignored"
            );
        }
        for cmd in [
            IpcCommand::Scroll { delta: 100.0 },
            IpcCommand::EqualizeColumnWidths,
        ] {
            assert!(
                matches!(floating_policy(&cmd), FloatingPolicy::Unchanged),
                "{cmd:?} should keep its normal dispatch"
            );
        }
    }

    #[test]
    fn column_commands_are_ignored_while_floating_has_focus() {
        for fullscreen in [false, true] {
            let mut state = floating_focus_state();
            if fullscreen {
                state.focused_workspace_mut().unwrap().toggle_fullscreen();
            }
            let before = strip_snapshot(&state);
            assert_eq!(before.fullscreen.is_some(), fullscreen);
            for cmd in IGNORED {
                assert_eq!(
                    state.handle_command(cmd.clone()),
                    IpcResponse::Ok,
                    "{cmd:?}"
                );
                assert_eq!(
                    strip_snapshot(&state),
                    before,
                    "{cmd:?} changed the strip (fullscreen: {fullscreen})"
                );
                assert_eq!(state.focused_floating_window(), Some(FLOATING));
            }
        }
    }

    #[test]
    fn strip_commands_still_act_while_floating_has_focus() {
        let mut state = floating_focus_state();
        assert_eq!(
            state.handle_command(IpcCommand::EqualizeColumnWidths),
            IpcResponse::Ok
        );
        let widths: Vec<i32> = state
            .focused_workspace()
            .unwrap()
            .columns()
            .iter()
            .map(|c| c.width())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn toggle_floating_unfloats_the_focused_floating_window() {
        let mut state = floating_focus_state();
        assert_eq!(
            state.handle_command(IpcCommand::ToggleFloating),
            IpcResponse::Ok
        );
        let ws = state.focused_workspace().unwrap();
        assert!(!ws.is_floating(FLOATING));
        assert!(ws.columns().iter().any(|c| c.windows().contains(&FLOATING)));

        let mut state = floating_focus_state();
        state.previous_focused_hwnd = Some(201);
        assert_eq!(
            state.handle_command(IpcCommand::ToggleFloating),
            IpcResponse::Ok
        );
        let ws = state.focused_workspace().unwrap();
        assert!(ws.is_floating(201));
        assert!(ws.is_floating(FLOATING));
    }

    #[test]
    fn toggle_sticky_pins_the_focused_floating_window() {
        let mut state = floating_focus_state();
        let before = strip_snapshot(&state);
        assert_eq!(
            state.handle_command(IpcCommand::ToggleSticky),
            IpcResponse::Ok
        );
        assert!(state.sticky_windows.contains(&FLOATING));
        assert!(state.focused_workspace().unwrap().is_floating(FLOATING));
        assert_eq!(strip_snapshot(&state), before);

        assert_eq!(
            state.handle_command(IpcCommand::ToggleSticky),
            IpcResponse::Ok
        );
        assert!(state.sticky_windows.is_empty());
    }

    #[test]
    fn move_to_workspace_targets_the_focused_floating_window() {
        for (cmd, target) in [
            (IpcCommand::MoveToWorkspace { index: 3 }, 2),
            (IpcCommand::MoveToWorkspacePrev, 8),
            (IpcCommand::MoveToWorkspaceNext, 1),
        ] {
            let mut state = floating_focus_state();
            let before = strip_snapshot(&state);
            assert_eq!(state.handle_command(cmd.clone()), IpcResponse::Ok);
            assert_eq!(strip_snapshot(&state), before, "{cmd:?}");
            assert!(!state.focused_workspace().unwrap().contains_window(FLOATING));
            assert!(
                state.workspaces[&1][target].is_floating(FLOATING),
                "{cmd:?}"
            );
        }
    }

    #[test]
    fn move_to_monitor_targets_the_focused_floating_window() {
        let mut state = floating_focus_state_on(vec![monitor(1, 0), monitor(2, 1920)]);
        let before = strip_snapshot(&state);
        assert_eq!(
            state.handle_command(IpcCommand::MoveWindowToMonitorRight),
            IpcResponse::Ok
        );
        assert_eq!(state.focused_monitor, 2);
        let target = state.focused_workspace().unwrap();
        assert!(target.is_floating(FLOATING));
        assert!(target.columns().is_empty());
        state.focused_monitor = 1;
        assert!(!state.focused_workspace().unwrap().contains_window(FLOATING));
        assert_eq!(strip_snapshot(&state), before);
    }

    fn floating_rect(state: &AppState) -> Rect {
        state
            .focused_workspace()
            .unwrap()
            .floating_windows()
            .iter()
            .find(|f| f.id == FLOATING)
            .unwrap()
            .rect
    }

    #[test]
    fn close_targets_the_focused_floating_window() {
        let mut state = floating_focus_state();
        assert_eq!(state.close_window_target(), Some(FLOATING));

        state.previous_focused_hwnd = Some(201);
        assert_eq!(state.close_window_target(), Some(201));
    }

    #[test]
    fn scratchpad_stash_targets_the_focused_floating_window() {
        let mut state = floating_focus_state();
        let before = strip_snapshot(&state);
        assert_eq!(
            state.handle_command(IpcCommand::ScratchpadStash),
            IpcResponse::Ok
        );
        assert_eq!(state.scratchpad.map(|sp| sp.window_id), Some(FLOATING));
        let ws = state.focused_workspace().unwrap();
        assert!(!ws.contains_window(FLOATING));
        assert_eq!(strip_snapshot(&state), before);
    }

    #[test]
    fn resize_targets_the_focused_floating_window() {
        let mut state = floating_focus_state();
        let before = strip_snapshot(&state);
        assert_eq!(
            state.handle_command(IpcCommand::Resize { delta: 100 }),
            IpcResponse::Ok
        );
        assert_eq!(floating_rect(&state), Rect::new(100, 100, 600, 400));
        assert_eq!(strip_snapshot(&state), before);
    }

    #[test]
    fn resize_hint_follows_the_focused_floating_window() {
        let mut state = floating_focus_state();
        assert_eq!(
            state.get_focused_column_rect(),
            Some(Rect::new(100, 100, 500, 400))
        );

        state.previous_focused_hwnd = Some(201);
        let tiled = state.get_focused_column_rect().unwrap();
        assert_ne!(tiled, Rect::new(100, 100, 500, 400));
    }

    #[test]
    fn resize_is_ignored_while_the_floating_window_is_fullscreen() {
        let mut state = floating_focus_state();
        assert!(state
            .focused_workspace_mut()
            .unwrap()
            .toggle_fullscreen_window(FLOATING));
        assert_eq!(
            state.handle_command(IpcCommand::Resize { delta: 100 }),
            IpcResponse::Ok
        );
        assert_eq!(floating_rect(&state), Rect::new(100, 100, 500, 400));
    }

    #[test]
    fn toggle_fullscreen_targets_the_focused_floating_window() {
        let mut state = floating_focus_state();
        let before = strip_snapshot(&state);
        assert_eq!(
            state.handle_command(IpcCommand::ToggleFullscreen),
            IpcResponse::Ok
        );
        assert_eq!(
            state.focused_workspace().unwrap().fullscreen_window_id(),
            Some(FLOATING)
        );
        assert_eq!(state.focused_floating_window(), Some(FLOATING));

        assert_eq!(
            state.handle_command(IpcCommand::ToggleFullscreen),
            IpcResponse::Ok
        );
        assert_eq!(strip_snapshot(&state), before);
        assert_eq!(floating_rect(&state), Rect::new(100, 100, 500, 400));
        assert_eq!(state.focused_floating_window(), Some(FLOATING));
    }

    #[test]
    fn focus_is_ignored_while_floating_is_fullscreen() {
        let mut state = floating_focus_state();
        assert!(state.config.behavior.fullscreen_follows_focus);
        assert_eq!(
            state.handle_command(IpcCommand::ToggleFullscreen),
            IpcResponse::Ok
        );
        let before = strip_snapshot(&state);
        assert_eq!(before.fullscreen, Some(FLOATING));
        assert_eq!(state.handle_command(IpcCommand::FocusLeft), IpcResponse::Ok);
        assert_eq!(strip_snapshot(&state), before);
        assert_eq!(state.focused_floating_window(), Some(FLOATING));
    }

    #[test]
    fn query_focused_reports_the_floating_window() {
        let mut state = floating_focus_state();
        assert_eq!(
            state.handle_command(IpcCommand::QueryFocused),
            IpcResponse::FocusedWindow {
                window_id: Some(FLOATING),
                column_index: 1,
                window_index: 1,
                row_index: 0,
                is_floating: true,
            }
        );

        state.previous_focused_hwnd = Some(201);
        assert_eq!(
            state.handle_command(IpcCommand::QueryFocused),
            IpcResponse::FocusedWindow {
                window_id: Some(201),
                column_index: 1,
                window_index: 1,
                row_index: 0,
                is_floating: false,
            }
        );
    }
}

#[cfg(test)]
mod set_active_tab_tests {
    use super::*;
    use crate::config::Config;
    use crate::state::{PendingTabFocus, TestApplyPlacementsBehavior};
    use leopardwm_core_layout::Rect;
    use leopardwm_platform_win32::{MonitorInfo, Win32Error, WindowEvent};
    use std::time::{Duration, Instant};

    fn tabbed_state() -> AppState {
        let mut state = AppState::new_with_config(
            Config::default(),
            vec![MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            }],
        );
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, None).unwrap();
        workspace.insert_window_in_column(200, 0).unwrap();
        workspace.set_focus(0, 0).unwrap();
        workspace.toggle_focused_column_tabbed_mode();
        state
    }

    #[test]
    fn set_active_tab_restores_minimized_target_before_activation() {
        let mut state = tabbed_state();
        state.focused_workspace_mut().unwrap().mark_minimized(200);
        let mut restored = None;

        let response = state.handle_set_active_tab_with_restore(0, 1, |hwnd| {
            restored = Some(hwnd);
            Ok(())
        });

        assert!(matches!(response, IpcResponse::Ok));
        assert_eq!(restored, Some(200));
        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(1));
        assert_eq!(workspace.focused_window(), Some(200));
        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(state.pending_tab_focus.is_some());
    }

    #[test]
    fn set_active_tab_restore_failure_preserves_state() {
        let mut state = tabbed_state();
        let (active_before, focus_before) = {
            let workspace = state.focused_workspace().unwrap();
            (
                workspace.column(0).unwrap().active_tab_idx(),
                (workspace.focused_column_index(), workspace.focused_window()),
            )
        };
        state.previous_focused_hwnd = Some(100);
        state.pending_tab_focus = Some(PendingTabFocus {
            monitor: 1,
            workspace_idx: 0,
            row_idx: 0,
            column_idx: 0,
            tab_idx: 0,
            set_at: Instant::now(),
        });

        let mut restored = None;
        let response = state.handle_set_active_tab_with_restore(0, 1, |hwnd| {
            restored = Some(hwnd);
            Err(Win32Error::WindowNotFound(hwnd))
        });

        assert!(matches!(response, IpcResponse::Error { .. }));
        assert_eq!(restored, Some(200));
        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), active_before);
        assert_eq!(
            (workspace.focused_column_index(), workspace.focused_window()),
            focus_before
        );
        assert_eq!(state.previous_focused_hwnd, Some(100));
        assert!(state.last_placed_layout_rects.is_empty());
        assert_eq!(state.pending_tab_focus.unwrap().tab_idx, 0);
    }

    #[test]
    fn set_active_tab_restores_model_visible_target_when_os_state_is_stale() {
        let mut state = tabbed_state();
        let mut restored = None;

        let response = state.handle_set_active_tab_with_restore(0, 1, |hwnd| {
            restored = Some(hwnd);
            Ok(())
        });

        assert!(matches!(response, IpcResponse::Ok));
        assert_eq!(restored, Some(200));
        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(1));
        assert_eq!(workspace.focused_window(), Some(200));
    }

    #[test]
    fn tab_click_restores_model_visible_target_when_os_state_is_stale() {
        let mut state = tabbed_state();
        let mut restored = None;

        let response = state.handle_tab_action_activation_with_restore(1, 0, 0, 0, 1, |hwnd| {
            restored = Some(hwnd);
            Ok(())
        });

        assert!(matches!(response, Some(IpcResponse::Ok)));
        assert_eq!(restored, Some(200));
        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(1));
        assert_eq!(workspace.focused_window(), Some(200));
    }

    #[test]
    fn set_active_tab_validates_target_before_restoring() {
        let mut state = tabbed_state();
        state.focused_workspace_mut().unwrap().mark_minimized(200);

        let response = state.handle_set_active_tab_with_restore(0, 2, |_| {
            panic!("invalid tab must not be restored")
        });

        assert!(matches!(response, IpcResponse::Error { .. }));
        let workspace = state.focused_workspace().unwrap();
        assert!(workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(0));
        assert_eq!(workspace.focused_window(), Some(100));
        assert!(state.pending_tab_focus.is_none());
    }

    #[test]
    fn delayed_restored_event_after_tab_activation_is_idempotent() {
        let mut state = tabbed_state();
        state.focused_workspace_mut().unwrap().mark_minimized(200);
        assert!(matches!(
            state.handle_set_active_tab_with_restore(0, 1, |_| Ok(())),
            IpcResponse::Ok
        ));

        state.handle_window_event(WindowEvent::Restored(200));

        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(1));
        assert_eq!(workspace.focused_window(), Some(200));
    }

    #[test]
    fn layout_failure_keeps_successfully_restored_tab_visible() {
        let mut state = tabbed_state();
        state.focused_workspace_mut().unwrap().mark_minimized(200);
        state.paused = false;
        state.injected_apply_placements_behavior =
            Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

        let response = state.handle_set_active_tab_with_restore(0, 1, |_| Ok(()));

        assert!(matches!(response, IpcResponse::Error { .. }));
        let workspace = state.focused_workspace().unwrap();
        assert!(!workspace.is_minimized(200));
        assert_eq!(workspace.column(0).unwrap().active_tab_idx(), Some(1));
        assert_eq!(workspace.focused_window(), Some(200));
        assert!(state.pending_tab_focus.is_none());
    }
}

#[cfg(test)]
#[path = "workspace_switch_tests.rs"]
mod workspace_switch_tests;

#[cfg(test)]
mod wrap_cycle_tests {
    use super::*;
    use crate::config::Config;
    use leopardwm_core_layout::Rect;
    use leopardwm_platform_win32::MonitorInfo;

    #[test]
    fn test_wrap_cycle_dispatch_uses_configured_presets() {
        let mut config = Config::default();
        config.layout.gap = 0;
        config.layout.outer_gap_left = 0;
        config.layout.outer_gap_right = 0;
        config.layout.outer_gap_top = 0;
        config.layout.outer_gap_bottom = 0;
        config.layout.width_presets = vec![0.8, 0.3, 0.6];
        config.layout.height_presets = vec![0.8, 0.3, 0.6];
        let mut state = AppState::new_with_config(
            config,
            vec![MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1000, 1000),
                work_area: Rect::new(0, 0, 1000, 1000),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            }],
        );
        assert!(state.paused);
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window_in_column(200, 0).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws.cycle_height_up(&[0.8]);
        assert_eq!(
            state.handle_command(IpcCommand::CycleWidth),
            IpcResponse::Ok
        );
        assert_eq!(state.focused_workspace().unwrap().columns()[0].width(), 300);
        assert_eq!(
            state.handle_command(IpcCommand::CycleHeight),
            IpcResponse::Ok
        );
        assert!(
            (state.focused_workspace().unwrap().columns()[0].height_weights()[0] - 0.3).abs()
                < 1e-9
        );
        let ws = state.focused_workspace_mut().unwrap();
        ws.toggle_fullscreen();
        assert_eq!(
            state.handle_command(IpcCommand::CycleWidth),
            IpcResponse::Ok
        );
        assert_eq!(
            state.handle_command(IpcCommand::CycleHeight),
            IpcResponse::Ok
        );
        let ws = state.focused_workspace().unwrap();
        assert_eq!(ws.columns()[0].width(), 300);
        assert!((ws.columns()[0].height_weights()[0] - 0.3).abs() < 1e-9);
        assert!(ws.is_fullscreen());
    }
}
