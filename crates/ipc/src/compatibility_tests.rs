use super::*;
use serde::Deserialize;

#[test]
fn recording_wire_contract_is_additive() {
    let command: IpcCommand =
        serde_json::from_str(r#"{"type":"record","action":"start"}"#).unwrap();
    assert_eq!(
        command,
        IpcCommand::Record {
            action: RecordAction::Start,
            timeout_secs: None
        }
    );
    let command = IpcCommand::Record {
        action: RecordAction::Start,
        timeout_secs: Some(42),
    };
    assert_eq!(
        serde_json::from_str::<IpcCommand>(&serde_json::to_string(&command).unwrap()).unwrap(),
        command
    );
    let response = IpcResponse::Recording {
        active: Some(true),
        path: Some("capture.log".into()),
    };
    let wire = serde_json::to_string(&response).unwrap();
    assert_eq!(
        serde_json::from_str::<IpcResponse>(&wire).unwrap(),
        response
    );
    assert_eq!(
        serde_json::from_str::<OldResponse>(&wire).unwrap(),
        OldResponse::Unknown
    );
    assert_eq!(
        serde_json::from_str::<IpcResponse>(r#"{"status":"recording"}"#).unwrap(),
        IpcResponse::Recording {
            active: None,
            path: None
        }
    );
}

// Wire shapes from the 0.3.5 release, limited to the variants exercised here.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum OldResponse {
    WorkspaceStateReady {
        protocol_version: u32,
    },
    WorkspaceState {
        columns: usize,
        windows: usize,
        focused_column: usize,
        focused_window: usize,
        scroll_offset: f64,
        total_width: i32,
        #[serde(default = "default_active_workspace")]
        active_workspace: u8,
        #[serde(default)]
        active_workspace_name: Option<String>,
    },
    FocusedWindow {
        window_id: Option<u64>,
        column_index: usize,
        window_index: usize,
        #[serde(default)]
        is_floating: bool,
    },
    WindowList {
        windows: Vec<OldWindowInfo>,
    },
    HotkeyList {
        hotkeys: Vec<OldHotkeyBinding>,
        scroll_modifier: String,
        issues: Vec<OldHotkeyIssue>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize, PartialEq)]
struct OldWindowInfo {
    window_id: u64,
    title: String,
    class_name: String,
    process_id: u32,
    executable: String,
    rect: IpcRect,
    column_index: Option<usize>,
    window_index: Option<usize>,
    monitor_id: i64,
    is_floating: bool,
    is_focused: bool,
}

#[derive(Debug, Deserialize, PartialEq)]
struct OldHotkeyBinding {
    action_id: String,
    label: String,
    group: String,
    bindings: Vec<String>,
    enabled: bool,
}

#[derive(Debug, Deserialize, PartialEq)]
struct OldHotkeyIssue {
    binding: String,
    action_id: String,
    message: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OldEvent {
    LayoutChanged {
        monitor: i64,
        workspace_index: u8,
        focused_column: Option<usize>,
        columns: Vec<OldColumn>,
    },
    WorkspaceSnapshotBegin {
        protocol_version: u32,
        session_id: String,
        revision: u64,
        focused_monitor_device_name: Option<String>,
    },
    WorkspaceSnapshotChunk {
        revision: u64,
        records: Vec<OldRecord>,
    },
    WorkspaceSnapshotEnd {
        revision: u64,
    },
}

#[derive(Debug, Deserialize, PartialEq)]
struct OldColumn {
    window_ids: Vec<u64>,
    width_px: i32,
    #[serde(default)]
    height_weights: Vec<f64>,
    #[serde(default)]
    mode: OldColumnMode,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OldColumnMode {
    #[default]
    Vertical,
    Tabbed {
        active_idx: usize,
    },
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OldRecord {
    Monitor {
        monitor_device_name: String,
        monitor_id: i64,
        active_workspace_index: u8,
    },
    Workspace {
        monitor_device_name: String,
        workspace_index: u8,
        name: Option<String>,
    },
    Window {
        monitor_device_name: String,
        workspace_index: u8,
        hwnd: u64,
        is_floating: bool,
        is_sticky: bool,
    },
}

#[test]
fn snapshot_command_defaults_and_old_response_fallback() {
    let command: IpcCommand =
        serde_json::from_str(r#"{"type":"write_diagnostic_snapshot"}"#).unwrap();
    assert_eq!(
        command,
        IpcCommand::WriteDiagnosticSnapshot {
            requester_version: String::new(),
            requester_integrity: None
        }
    );
    let response = IpcResponse::DiagnosticSnapshotWritten {
        path: "snapshot.txt".into(),
    };
    let wire = serde_json::to_string(&response).unwrap();
    assert_eq!(
        serde_json::from_str::<OldResponse>(&wire).unwrap(),
        OldResponse::Unknown
    );
    assert_eq!(
        serde_json::from_str::<IpcResponse>(&wire).unwrap(),
        response
    );
}

#[test]
fn released_035_client_decodes_additive_rows_and_snapshot() {
    let ready = IpcResponse::WorkspaceStateReady {
        protocol_version: IPC_PROTOCOL_VERSION,
    };
    let old: OldResponse = serde_json::from_value(serde_json::to_value(ready).unwrap()).unwrap();
    match old {
        OldResponse::WorkspaceStateReady { protocol_version } => {
            assert!((1..=4).contains(&protocol_version))
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let column = ColumnSummary {
        window_ids: vec![42],
        width_px: 800,
        height_weights: vec![1.0],
        mode: ColumnSummaryMode::Vertical,
    };
    let rows = vec![
        RowSummary {
            columns: vec![],
            focused_column: None,
            scroll_offset: 0.0,
        },
        RowSummary {
            columns: vec![column.clone()],
            focused_column: Some(0),
            scroll_offset: 25.0,
        },
    ];
    let layout = IpcEvent::LayoutChanged {
        monitor: 1,
        workspace_index: 0,
        focused_column: Some(0),
        columns: vec![column],
        focused_row: 1,
        rows: rows.clone(),
    };
    let old: OldEvent = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
    assert!(
        matches!(old, OldEvent::LayoutChanged { focused_column: Some(0), columns, .. } if columns[0].window_ids == [42])
    );

    let workspace = IpcResponse::WorkspaceState {
        columns: 1,
        windows: 1,
        focused_column: 0,
        focused_window: 0,
        scroll_offset: 25.0,
        total_width: 800,
        active_workspace: 1,
        active_workspace_name: None,
        focused_row: 1,
        rows,
    };
    let focused = IpcResponse::FocusedWindow {
        window_id: Some(42),
        row_index: 1,
        column_index: 0,
        window_index: 0,
        is_floating: false,
    };
    let windows = IpcResponse::WindowList {
        windows: vec![WindowInfo {
            window_id: 42,
            title: "Window".into(),
            class_name: "Class".into(),
            process_id: 1,
            executable: "app.exe".into(),
            rect: IpcRect::new(0, 0, 800, 600),
            row_index: Some(1),
            column_index: Some(0),
            window_index: Some(0),
            monitor_id: 1,
            is_floating: false,
            is_focused: true,
        }],
    };
    let hotkeys = IpcResponse::HotkeyList {
        hotkeys: vec![HotkeyBindingInfo {
            action_id: "focus_row_up".into(),
            label: "Focus row up".into(),
            group: "Focus".into(),
            bindings: vec!["Ctrl+Alt+PageUp".into()],
            enabled: true,
        }],
        scroll_modifier: "Ctrl+Alt".into(),
        issues: vec![HotkeyIssue {
            binding: "Ctrl+Nope".into(),
            action_id: "focus_row_up".into(),
            message: "invalid chord".into(),
        }],
    };
    for response in [&workspace, &focused, &windows, &hotkeys] {
        let old: OldResponse =
            serde_json::from_value(serde_json::to_value(response).unwrap()).unwrap();
        assert!(!matches!(old, OldResponse::Unknown));
    }

    let snapshot = WorkspaceStateSnapshot {
        session_id: "session".into(),
        revision: 1,
        focused_monitor_device_name: Some("DISPLAY1".into()),
        records: vec![
            WorkspaceStateRecord::Monitor {
                monitor_device_name: "DISPLAY1".into(),
                monitor_id: 1,
                active_workspace_index: 0,
            },
            WorkspaceStateRecord::Workspace {
                monitor_device_name: "DISPLAY1".into(),
                workspace_index: 0,
                name: None,
                rows: Some(2),
            },
            WorkspaceStateRecord::Window {
                monitor_device_name: "DISPLAY1".into(),
                workspace_index: 0,
                hwnd: 42,
                is_floating: false,
                is_sticky: false,
                row_index: Some(1),
            },
        ],
    };
    for (record, field, expected) in [
        (&snapshot.records[1], "rows", 2),
        (&snapshot.records[2], "row_index", 1),
    ] {
        let mut wire = serde_json::to_value(record).unwrap();
        assert_eq!(wire[field], expected);
        assert_eq!(
            serde_json::from_value::<WorkspaceStateRecord>(wire.clone()).unwrap(),
            *record
        );
        wire.as_object_mut().unwrap().remove(field);
        let old = serde_json::from_value::<WorkspaceStateRecord>(wire).unwrap();
        assert!(matches!(
            old,
            WorkspaceStateRecord::Workspace { rows: None, .. }
                | WorkspaceStateRecord::Window {
                    row_index: None,
                    ..
                }
        ));
    }
    let events = snapshot.events().unwrap();
    let decoded: Vec<OldEvent> = events
        .iter()
        .map(|event| serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap())
        .collect();
    assert!(
        matches!(&decoded[..], [OldEvent::WorkspaceSnapshotBegin { protocol_version: 4, .. }, OldEvent::WorkspaceSnapshotChunk { records, .. }, OldEvent::WorkspaceSnapshotEnd { .. }] if records.len() == 3)
    );

    for value in [
        serde_json::to_value(layout).unwrap(),
        serde_json::to_value(workspace).unwrap(),
    ] {
        let mut old = value;
        let object = old.as_object_mut().unwrap();
        object.remove("focused_row");
        object.remove("rows");
        if object.contains_key("type") {
            serde_json::from_value::<IpcEvent>(old).unwrap();
        } else {
            serde_json::from_value::<IpcResponse>(old).unwrap();
        }
    }
    let mut old = serde_json::to_value(focused).unwrap();
    old.as_object_mut().unwrap().remove("row_index");
    serde_json::from_value::<IpcResponse>(old).unwrap();
    let mut old = serde_json::to_value(windows).unwrap();
    old["windows"][0]
        .as_object_mut()
        .unwrap()
        .remove("row_index");
    serde_json::from_value::<IpcResponse>(old).unwrap();
}
