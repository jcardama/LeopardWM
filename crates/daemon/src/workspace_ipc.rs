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
                records.push(WorkspaceStateRecord::Workspace {
                    monitor_device_name: device.clone(),
                    workspace_index: index as u8,
                    name: self.config.workspaces.name_for(index),
                });
                let Some(workspace) = self.workspaces.get(&monitor.id).and_then(|w| w.get(index))
                else {
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
                    records.push(WorkspaceStateRecord::Window {
                        monitor_device_name: device.clone(),
                        workspace_index: index as u8,
                        hwnd,
                        is_floating: workspace.is_floating(hwnd)
                            || detached_drag
                                .is_some_and(|drag| drag.hwnd == hwnd && !drag.is_tiled),
                        is_sticky: self.sticky_windows.contains(&hwnd),
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
