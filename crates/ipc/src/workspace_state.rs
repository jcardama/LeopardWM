//! Complete workspace-state snapshot records and bounded event framing.

use serde::{Deserialize, Serialize};

use crate::{IpcEvent, IPC_PROTOCOL_VERSION, MAX_IPC_MESSAGE_SIZE};

/// One semantic record in a complete workspace-state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceStateRecord {
    /// A connected monitor and its active zero-based workspace index.
    Monitor {
        /// Current Win32 display device name.
        monitor_device_name: String,
        /// Transient HMONITOR value for the current topology.
        monitor_id: i64,
        /// Active workspace index, zero-based.
        active_workspace_index: u8,
    },
    /// One configured zero-based workspace slot on a monitor.
    Workspace {
        /// Owning monitor device name.
        monitor_device_name: String,
        /// Workspace index, zero-based.
        workspace_index: u8,
        /// Configured display name, if any.
        name: Option<String>,
    },
    /// A managed window's owning workspace.
    Window {
        /// Owning monitor device name.
        monitor_device_name: String,
        /// Owning workspace index, zero-based.
        workspace_index: u8,
        /// Transient Win32 window handle.
        hwnd: u64,
        /// Whether the window is in the workspace floating layer.
        is_floating: bool,
        /// Whether the window is pinned across its monitor workspaces.
        is_sticky: bool,
    },
}

/// An immutable, coherent view of workspace membership across all monitors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStateSnapshot {
    /// Opaque daemon-instance identifier; changes after daemon restart.
    pub session_id: String,
    /// Monotonic semantic-state revision within this daemon session.
    pub revision: u64,
    /// Device name of the globally focused monitor, if known.
    pub focused_monitor_device_name: Option<String>,
    /// Deterministically ordered monitor, workspace, and membership records.
    pub records: Vec<WorkspaceStateRecord>,
}

impl WorkspaceStateSnapshot {
    /// Encode this snapshot as begin/chunk/end events whose serialized JSON
    /// frames, including the trailing newline, fit the IPC message limit.
    pub fn events(&self) -> Result<Vec<IpcEvent>, String> {
        let begin = IpcEvent::WorkspaceSnapshotBegin {
            protocol_version: IPC_PROTOCOL_VERSION,
            session_id: self.session_id.clone(),
            revision: self.revision,
            focused_monitor_device_name: self.focused_monitor_device_name.clone(),
        };
        ensure_frame_fits(&begin, "workspace snapshot begin")?;

        let end = IpcEvent::WorkspaceSnapshotEnd {
            revision: self.revision,
        };
        ensure_frame_fits(&end, "workspace snapshot end")?;

        let mut events = vec![begin];
        let mut chunk = Vec::new();
        let empty_chunk_len = frame_len(&IpcEvent::WorkspaceSnapshotChunk {
            revision: self.revision,
            records: Vec::new(),
        })?;
        let mut chunk_len = empty_chunk_len;

        for (index, record) in self.records.iter().cloned().enumerate() {
            let record_len = serde_json::to_vec(&record)
                .map_err(|error| format!("failed to serialize workspace state record: {error}"))?
                .len();
            let separator_len = usize::from(!chunk.is_empty());
            let candidate_len = chunk_len + separator_len + record_len;

            if candidate_len > MAX_IPC_MESSAGE_SIZE {
                if chunk.is_empty() {
                    return Err(format!(
                        "workspace state record {index} requires {candidate_len} bytes; maximum IPC frame size is {MAX_IPC_MESSAGE_SIZE} bytes"
                    ));
                }
                events.push(IpcEvent::WorkspaceSnapshotChunk {
                    revision: self.revision,
                    records: std::mem::take(&mut chunk),
                });
                chunk_len = empty_chunk_len;
            }

            let separator_len = usize::from(!chunk.is_empty());
            let single_chunk_len = chunk_len + separator_len + record_len;
            if single_chunk_len > MAX_IPC_MESSAGE_SIZE {
                return Err(format!(
                    "workspace state record {index} requires {single_chunk_len} bytes; maximum IPC frame size is {MAX_IPC_MESSAGE_SIZE} bytes"
                ));
            }
            chunk.push(record);
            chunk_len = single_chunk_len;
        }

        if !chunk.is_empty() {
            events.push(IpcEvent::WorkspaceSnapshotChunk {
                revision: self.revision,
                records: chunk,
            });
        }
        events.push(end);

        for event in &events {
            ensure_frame_fits(event, "workspace snapshot")?;
        }
        Ok(events)
    }
}

fn frame_len(event: &IpcEvent) -> Result<usize, String> {
    serde_json::to_vec(event)
        .map(|frame| frame.len() + 1)
        .map_err(|error| format!("failed to serialize workspace snapshot frame: {error}"))
}

fn ensure_frame_fits(event: &IpcEvent, frame_name: &str) -> Result<(), String> {
    let len = frame_len(event)?;
    if len > MAX_IPC_MESSAGE_SIZE {
        return Err(format!(
            "{frame_name} requires {len} bytes; maximum IPC frame size is {MAX_IPC_MESSAGE_SIZE} bytes"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_records() -> Vec<WorkspaceStateRecord> {
        vec![
            WorkspaceStateRecord::Monitor {
                monitor_device_name: r"\\.\DISPLAY2".to_string(),
                monitor_id: 65_537,
                active_workspace_index: 1,
            },
            WorkspaceStateRecord::Workspace {
                monitor_device_name: r"\\.\DISPLAY2".to_string(),
                workspace_index: 1,
                name: Some("開発".to_string()),
            },
            WorkspaceStateRecord::Window {
                monitor_device_name: r"\\.\DISPLAY2".to_string(),
                workspace_index: 1,
                hwnd: 123_456,
                is_floating: true,
                is_sticky: false,
            },
        ]
    }

    #[test]
    fn workspace_state_records_round_trip_with_all_fields() {
        for record in sample_records() {
            let json = serde_json::to_string(&record).unwrap();
            let decoded: WorkspaceStateRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn snapshot_events_round_trip_and_preserve_records() {
        let snapshot = WorkspaceStateSnapshot {
            session_id: "daemon-session".to_string(),
            revision: 42,
            focused_monitor_device_name: Some(r"\\.\DISPLAY2".to_string()),
            records: sample_records(),
        };

        let events = snapshot.events().unwrap();
        assert!(matches!(
            events.first(),
            Some(IpcEvent::WorkspaceSnapshotBegin {
                protocol_version: 4,
                revision: 42,
                ..
            })
        ));
        assert!(matches!(
            events.last(),
            Some(IpcEvent::WorkspaceSnapshotEnd { revision: 42 })
        ));

        let decoded_records: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                IpcEvent::WorkspaceSnapshotChunk { records, .. } => Some(records.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(decoded_records, snapshot.records);

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let decoded: IpcEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn snapshot_events_chunk_non_ascii_records_by_utf8_frame_bytes() {
        let records = (0..220)
            .map(|index| WorkspaceStateRecord::Workspace {
                monitor_device_name: r"\\.\DISPLAY2".to_string(),
                workspace_index: (index % 9) as u8,
                name: Some(format!("開発-{}", "界".repeat(120))),
            })
            .collect();
        let snapshot = WorkspaceStateSnapshot {
            session_id: "session".to_string(),
            revision: 7,
            focused_monitor_device_name: None,
            records,
        };

        let events = snapshot.events().unwrap();
        let chunks = events
            .iter()
            .filter(|event| matches!(event, IpcEvent::WorkspaceSnapshotChunk { .. }))
            .count();
        assert!(chunks > 1, "fixture must exceed a single IPC frame");
        assert!(events
            .iter()
            .all(|event| { serde_json::to_vec(event).unwrap().len() + 1 <= MAX_IPC_MESSAGE_SIZE }));
    }

    #[test]
    fn snapshot_events_reject_an_oversized_single_record() {
        let snapshot = WorkspaceStateSnapshot {
            session_id: "session".to_string(),
            revision: 9,
            focused_monitor_device_name: None,
            records: vec![WorkspaceStateRecord::Workspace {
                monitor_device_name: r"\\.\DISPLAY2".to_string(),
                workspace_index: 0,
                name: Some("界".repeat(MAX_IPC_MESSAGE_SIZE)),
            }],
        };

        let error = snapshot.events().unwrap_err();
        assert!(error.contains("record 0"));
        assert!(error.contains("maximum IPC frame size"));
        assert!(error.len() < 256);
    }

    #[test]
    fn snapshot_events_reject_an_oversized_begin_frame() {
        let snapshot = WorkspaceStateSnapshot {
            session_id: "s".repeat(MAX_IPC_MESSAGE_SIZE),
            revision: 0,
            focused_monitor_device_name: None,
            records: Vec::new(),
        };

        let error = snapshot.events().unwrap_err();
        assert!(error.contains("workspace snapshot begin"));
        assert!(error.len() < 256);
    }
}
