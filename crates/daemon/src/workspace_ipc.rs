//! Complete, deduplicated workspace state for external consumers.
//!
//! Projection and publication run under AppState's mutex. This module performs
//! no Win32 lookup or pipe I/O; transport writes the frozen frames separately.
use crate::state::{AppState, DRAG_PLACEHOLDER_HWND};
use leopardwm_ipc::{IpcEvent, WorkspaceStateRecord, WorkspaceStateSnapshot};
use std::sync::atomic::{AtomicU64, Ordering};

fn session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{epoch}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) struct WorkspaceIpcState {
    session_id: String,
    snapshot: Option<WorkspaceStateSnapshot>,
}

impl Default for WorkspaceIpcState {
    fn default() -> Self {
        Self {
            session_id: session_id(),
            snapshot: None,
        }
    }
}

impl AppState {
    pub(crate) fn project_workspace_state(&self) -> WorkspaceStateSnapshot {
        let mut monitors: Vec<_> = self.monitors.values().collect();
        monitors.sort_by(|a, b| a.device_name.cmp(&b.device_name).then(a.id.cmp(&b.id)));
        // A merge preview can detach the real window and insert a sentinel at
        // the prospective destination. Ownership remains at its source until
        // drop; do not make the app disappear from the bar during that preview.
        let detached_drag = self.drag_state.as_ref().filter(|drag| {
            drag.removed_from_source
                && !self
                    .workspaces
                    .values()
                    .flatten()
                    .any(|workspace| workspace.contains_window(drag.hwnd))
        });
        let mut records = Vec::new();
        for monitor in monitors {
            let device = &monitor.device_name;
            records.push(WorkspaceStateRecord::Monitor {
                monitor_device_name: device.clone(),
                monitor_id: monitor.id as i64,
                active_workspace_index: self.active_workspace_idx(monitor.id) as u8,
            });
            for index in 0..9 {
                let workspace = self.workspaces.get(&monitor.id).and_then(|w| w.get(index));
                let (focused_row, rows) = if let Some(workspace) = workspace {
                    let matching_drag = detached_drag.filter(|drag| {
                        drag.source_monitor == monitor.id
                            && drag.source_workspace_idx == index
                            && drag.is_tiled
                    });
                    let rows = workspace
                        .rows()
                        .iter()
                        .enumerate()
                        .map(|(r_idx, row)| {
                            let mut columns: Vec<leopardwm_ipc::ColumnSummary> = row
                                .columns()
                                .iter()
                                .filter_map(|col| {
                                    let window_ids: Vec<u64> = col
                                        .windows()
                                        .iter()
                                        .copied()
                                        .filter(|&hwnd| {
                                            hwnd != DRAG_PLACEHOLDER_HWND
                                                && !self.scratchpad.is_some_and(|s| {
                                                    !s.shown && s.window_id == hwnd
                                                })
                                        })
                                        .collect();
                                    if window_ids.is_empty() {
                                        return None;
                                    }
                                    let height_weights = if window_ids.len() == col.windows().len()
                                    {
                                        col.height_weights().to_vec()
                                    } else {
                                        vec![1.0 / window_ids.len() as f64; window_ids.len()]
                                    };
                                    let mode = match col.mode() {
                                        leopardwm_core_layout::ColumnMode::Vertical => {
                                            leopardwm_ipc::ColumnSummaryMode::Vertical
                                        }
                                        leopardwm_core_layout::ColumnMode::Tabbed {
                                            active_idx,
                                        } => leopardwm_ipc::ColumnSummaryMode::Tabbed {
                                            active_idx: (*active_idx)
                                                .min(window_ids.len().saturating_sub(1)),
                                        },
                                    };
                                    Some(leopardwm_ipc::ColumnSummary {
                                        window_ids,
                                        width_px: workspace.effective_column_width(col),
                                        height_weights,
                                        mode,
                                    })
                                })
                                .collect();
                            if let Some(drag) = matching_drag.filter(|d| d.source_row == r_idx) {
                                let merged_into_peer = !drag.source_column_peers.is_empty()
                                    && if let Some(col) = columns.iter_mut().find(|c| {
                                        c.window_ids
                                            .iter()
                                            .any(|w| drag.source_column_peers.contains(w))
                                    }) {
                                        let slot =
                                            drag.source_window_slot.min(col.window_ids.len());
                                        col.window_ids.insert(slot, drag.hwnd);
                                        col.height_weights = vec![
                                            1.0 / col.window_ids.len() as f64;
                                            col.window_ids.len()
                                        ];
                                        true
                                    } else {
                                        false
                                    };
                                if !merged_into_peer {
                                    let idx = drag.current_column_index.min(columns.len());
                                    columns.insert(
                                        idx,
                                        leopardwm_ipc::ColumnSummary {
                                            window_ids: vec![drag.hwnd],
                                            width_px: drag.source_column_width.unwrap_or_else(
                                                || workspace.default_column_width(),
                                            ),
                                            height_weights: vec![1.0],
                                            mode: leopardwm_ipc::ColumnSummaryMode::Vertical,
                                        },
                                    );
                                }
                            }
                            let focused_column = if columns.is_empty() {
                                None
                            } else {
                                Some(row.focused_column().min(columns.len() - 1))
                            };
                            leopardwm_ipc::RowSummary {
                                columns,
                                focused_column,
                                scroll_offset: row.scroll_offset(),
                            }
                        })
                        .collect();
                    (workspace.focused_row(), rows)
                } else {
                    (0, Vec::new())
                };
                records.push(WorkspaceStateRecord::Workspace {
                    monitor_device_name: device.clone(),
                    workspace_index: index as u8,
                    name: self.config.workspaces.name_for(index),
                    focused_row,
                    rows,
                });
                let Some(workspace) = workspace else {
                    continue;
                };
                let mut windows = workspace.all_window_ids();
                if let Some(drag) = detached_drag.filter(|drag| {
                    drag.source_monitor == monitor.id && drag.source_workspace_idx == index
                }) {
                    windows.push(drag.hwnd);
                }
                windows.sort_unstable();
                windows.dedup();
                for hwnd in windows {
                    if hwnd == DRAG_PLACEHOLDER_HWND
                        || self
                            .scratchpad
                            .is_some_and(|s| !s.shown && s.window_id == hwnd)
                    {
                        continue;
                    }
                    let is_floating = workspace.is_floating(hwnd)
                        || detached_drag.is_some_and(|drag| drag.hwnd == hwnd && !drag.is_tiled);
                    let row_index = if is_floating {
                        None
                    } else {
                        workspace
                            .find_window_location_rc(hwnd)
                            .map(|(r, _, _)| r)
                            .or_else(|| {
                                detached_drag
                                    .filter(|drag| drag.hwnd == hwnd && drag.is_tiled)
                                    .map(|drag| drag.source_row)
                            })
                    };
                    records.push(WorkspaceStateRecord::Window {
                        monitor_device_name: device.clone(),
                        workspace_index: index as u8,
                        hwnd,
                        is_floating,
                        is_sticky: self.sticky_windows.contains(&hwnd),
                        row_index,
                    });
                }
            }
        }
        WorkspaceStateSnapshot {
            session_id: self.workspace_ipc_state.session_id.clone(),
            revision: 0,
            focused_monitor_device_name: self
                .monitors
                .get(&self.focused_monitor)
                .map(|m| m.device_name.clone()),
            records,
        }
    }

    /// Publish one complete transaction when externally visible state changes.
    /// Geometry, animation progress and window focus within one monitor do not
    /// change this model. All frames are preflighted before the begin is emitted.
    pub(crate) fn publish_workspace_state_if_changed(&mut self) {
        let mut snapshot = self.project_workspace_state();
        if let Some(previous) = &self.workspace_ipc_state.snapshot {
            if previous.focused_monitor_device_name == snapshot.focused_monitor_device_name
                && previous.records == snapshot.records
            {
                return;
            }
            if let Some(revision) = previous.revision.checked_add(1) {
                snapshot.revision = revision;
            } else {
                self.workspace_ipc_state.session_id = session_id();
                snapshot.session_id = self.workspace_ipc_state.session_id.clone();
            }
        }
        self.workspace_ipc_state.snapshot = Some(snapshot);
        if self.workspace_event_broadcaster.receiver_count() != 0 {
            for event in self.workspace_snapshot_events() {
                self.broadcast_event(event);
            }
        }
    }

    /// Synchronize and publish after an ordinary daemon event only when a
    /// workspace-enabled stream client exists. Query and subscribe handoffs call
    /// `publish_workspace_state_if_changed` directly so their snapshot is
    /// always fresh before the receiver is installed.
    pub(crate) fn publish_workspace_state_if_subscribed(&mut self) {
        if self.workspace_event_broadcaster.receiver_count() == 0 {
            return;
        }
        self.publish_workspace_state_if_changed();
    }

    /// Return frames for the cached state, synchronized by the caller first.
    pub(crate) fn workspace_snapshot_events(&self) -> Vec<IpcEvent> {
        self.workspace_ipc_state
            .snapshot
            .as_ref()
            .expect("workspace state synchronized before snapshot")
            .events()
            .unwrap_or_else(|message| vec![IpcEvent::WorkspaceSnapshotError { message }])
    }
}
