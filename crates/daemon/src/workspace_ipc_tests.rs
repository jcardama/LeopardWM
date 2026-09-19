use super::*;
use leopardwm_ipc::EventKind;

#[tokio::test]
async fn workspace_subscription_starts_with_complete_state() {
    let monitor = MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 1920, 1080),
        work_area: Rect::new(0, 0, 1920, 1040),
        is_primary: true,
        device_name: "DISPLAY1".into(),
        scale_factor: 1.0,
    };
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(AppState::new_with_config(
        Config::default(),
        vec![monitor],
    )));
    let kind: EventKind = serde_json::from_str("\"workspace_state\"").unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ipc_subscribe(&state, [kind].into_iter().collect(), tx).await;
    let startup = rx.await.unwrap();
    let frames: Vec<serde_json::Value> = startup
        .snapshot
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(
        frames.first().map(|f| f["type"].as_str()),
        Some(Some("workspace_snapshot_begin"))
    );
    let slots = frames
        .iter()
        .filter_map(|f| f["records"].as_array())
        .flatten()
        .filter(|r| r["kind"] == "workspace")
        .count();
    assert_eq!(slots, 9, "include lazily unallocated empty workspaces");
    assert_eq!(frames.last().unwrap()["type"], "workspace_snapshot_end");
}

fn fixture() -> AppState {
    let monitors = [1, 2]
        .into_iter()
        .map(|id| MonitorInfo {
            id,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: id == 1,
            device_name: format!("DISPLAY{id}"),
            scale_factor: 1.0,
        })
        .collect();
    AppState::new_with_config(Config::default(), monitors)
}

#[test]
fn targeted_active_workspace_restores_remembered_floating_focus() {
    let mut state = fixture();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, None)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .add_floating(200, Rect::new(0, 0, 100, 100))
        .unwrap();
    state.focused_monitor = 1;
    state.previous_focused_hwnd = Some(100);
    state.floating_focus.insert((2, 0), 200);

    let response = state.handle_command(IpcCommand::SwitchWorkspaceOnMonitor {
        monitor_device_name: "DISPLAY2".into(),
        index: 1,
    });

    assert!(matches!(response, IpcResponse::Ok));
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(state.active_workspace_idx(2), 0);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.last_broadcast_focused, Some((2, Some(200))));
}

#[test]
fn targeted_active_workspace_ignores_stale_floating_focus() {
    let mut state = fixture();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(300, None)
        .unwrap();
    state.focused_monitor = 1;
    state.previous_focused_hwnd = Some(100);
    state.floating_focus.insert((2, 0), 200);

    let response = state.handle_command(IpcCommand::SwitchWorkspaceOnMonitor {
        monitor_device_name: "DISPLAY2".into(),
        index: 1,
    });

    assert!(matches!(response, IpcResponse::Ok));
    assert_eq!(state.previous_focused_hwnd, Some(300));
}

#[test]
fn targeted_current_workspace_preserves_current_focus() {
    for current in [100, 300] {
        let mut state = fixture();
        let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
        ws.insert_window(100, None).unwrap();
        ws.add_floating(200, Rect::new(0, 0, 100, 100)).unwrap();
        ws.add_floating(300, Rect::new(0, 0, 100, 100)).unwrap();
        state.focused_monitor = 1;
        state.previous_focused_hwnd = Some(current);
        // Remembered from an earlier workspace visit, before the user focused
        // a different tiled or floating window on the current workspace.
        state.floating_focus.insert((1, 0), 200);

        let response = state.handle_command(IpcCommand::SwitchWorkspaceOnMonitor {
            monitor_device_name: "DISPLAY1".into(),
            index: 1,
        });

        assert!(matches!(response, IpcResponse::Ok));
        assert_eq!(state.previous_focused_hwnd, Some(current));
    }
}

fn records(state: &AppState) -> Vec<leopardwm_ipc::WorkspaceStateRecord> {
    state
        .workspace_snapshot_events()
        .into_iter()
        .flat_map(|e| match e {
            leopardwm_ipc::IpcEvent::WorkspaceSnapshotChunk { records, .. } => records,
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn complete_membership_includes_inactive_and_floating_excludes_placeholders() {
    use leopardwm_ipc::WorkspaceStateRecord as R;
    let mut state = fixture();
    state.ensure_workspace_exists(2, 3);
    let ws = &mut state.workspaces.get_mut(&2).unwrap()[3];
    ws.insert_window(100, None).unwrap();
    ws.insert_window_in_column(101, 0).unwrap();
    ws.toggle_focused_column_tabbed_mode();
    ws.mark_minimized(101);
    ws.add_floating(200, Rect::new(0, 0, 100, 100)).unwrap();
    ws.insert_window(DRAG_PLACEHOLDER_HWND, None).unwrap();
    state.sticky_windows.insert(200);
    state.config.workspaces.names = vec![" code ".into()];
    state.publish_workspace_state_if_changed();
    let all = records(&state);
    assert_eq!(
        all.iter()
            .filter(|r| matches!(r, R::Workspace { .. }))
            .count(),
        18
    );
    let windows: Vec<_> = all
        .iter()
        .filter_map(|r| match r {
            R::Window {
                monitor_device_name,
                workspace_index,
                hwnd,
                is_floating,
                is_sticky,
            } => {
                assert_eq!(monitor_device_name, "DISPLAY2");
                assert_eq!(*workspace_index, 3);
                Some((*hwnd, *is_floating, *is_sticky))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        windows,
        vec![(100, false, false), (101, false, false), (200, true, true)]
    );
    assert!(all.iter().any(
        |r| matches!(r, R::Workspace { workspace_index: 0, name: Some(n), .. } if n == "code")
    ));
}

#[test]
fn workspace_revisions_track_semantics_and_ignore_geometry() {
    use leopardwm_ipc::IpcEvent;
    let mut state = fixture();
    state.workspaces.get_mut(&1).unwrap()[0]
        .add_floating(200, Rect::new(0, 0, 100, 100))
        .unwrap();
    state.config.workspaces.names = vec!["old label".into()];
    state.publish_workspace_state_if_changed();
    let mut receiver = state.workspace_event_broadcaster.subscribe();
    state.workspaces.get_mut(&1).unwrap()[0].update_floating(200, Rect::new(10, 10, 200, 300));
    state.previous_focused_hwnd = Some(200);
    state.publish_workspace_state_if_changed();
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    for revision in 1..=5 {
        match revision {
            1 => state.focused_monitor = 2,
            2 => {
                state.active_workspace.insert(2, 3);
            }
            3 => state.config.workspaces.names = vec!["new label".into()],
            4 => {
                state.sticky_windows.insert(200);
            }
            5 => {
                state.monitors.remove(&2);
            }
            _ => unreachable!(),
        }
        state.publish_workspace_state_if_changed();
        assert!(
            matches!(receiver.try_recv().unwrap(), IpcEvent::WorkspaceSnapshotBegin { revision: r, .. } if r == revision)
        );
        loop {
            if matches!(receiver.try_recv().unwrap(), IpcEvent::WorkspaceSnapshotEnd { revision: r } if r == revision)
            {
                break;
            }
        }
        state.publish_workspace_state_if_changed();
        assert!(receiver.try_recv().is_err());
    }
    assert!(!records(&state).iter().any(|r| matches!(r, leopardwm_ipc::WorkspaceStateRecord::Monitor { monitor_device_name, .. } if monitor_device_name == "DISPLAY2")));
}

#[test]
fn event_publication_skips_projection_without_stream_subscribers() {
    use leopardwm_ipc::WorkspaceStateRecord as R;

    let mut state = fixture();
    state.publish_workspace_state_if_changed();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(42, None)
        .unwrap();

    state.publish_workspace_state_if_subscribed();
    assert!(
        !records(&state)
            .iter()
            .any(|record| matches!(record, R::Window { hwnd: 42, .. })),
        "ordinary events must not rebuild workspace IPC state without an opted-in subscriber"
    );

    state.publish_workspace_state_if_changed();
    assert!(
        records(&state)
            .iter()
            .any(|record| matches!(record, R::Window { hwnd: 42, .. })),
        "query/subscribe synchronization must force a fresh projection"
    );
}

#[tokio::test]
async fn legacy_subscription_does_not_trigger_workspace_publication() {
    use leopardwm_ipc::IpcEvent;
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(fixture()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ipc_subscribe(&state, EventKind::legacy_default(), tx).await;
    let mut legacy = rx.await.unwrap();
    let mut s = state.lock().await;
    s.publish_workspace_state_if_changed();
    let initial = s.workspace_snapshot_events();
    s.config.workspaces.names = vec!["fresh".into()];
    s.publish_workspace_state_if_subscribed();
    assert_eq!(s.workspace_snapshot_events(), initial);

    // Forced query capture must be fresh without enqueueing workspace frames
    // for an existing legacy stream.
    s.publish_workspace_state_if_changed();
    assert_ne!(s.workspace_snapshot_events(), initial);
    s.broadcast_event(IpcEvent::ConfigReloaded);
    assert!(matches!(
        legacy.receiver.try_recv().unwrap(),
        IpcEvent::ConfigReloaded
    ));
    assert!(legacy.receiver.try_recv().is_err());
}

#[tokio::test]
async fn workspace_traffic_cannot_evict_legacy_events_with_mixed_subscribers() {
    use leopardwm_ipc::IpcEvent;
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(fixture()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ipc_subscribe(&state, EventKind::legacy_default(), tx).await;
    let mut legacy = rx.await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut kinds = EventKind::legacy_default();
    kinds.insert(EventKind::WorkspaceState);
    handle_ipc_subscribe(&state, kinds, tx).await;
    let mut mixed = rx.await.unwrap();
    let mut s = state.lock().await;

    s.broadcast_event(IpcEvent::ConfigReloaded);
    assert!(matches!(
        mixed.receiver.try_recv().unwrap(),
        IpcEvent::ConfigReloaded
    ));
    s.broadcast_focused_window_if_changed(1, None);
    assert!(matches!(
        mixed.receiver.try_recv().unwrap(),
        IpcEvent::FocusedWindowChanged {
            monitor: 1,
            hwnd: None,
            ..
        }
    ));
    for i in 0..300 {
        s.config.workspaces.names = vec![format!("label {i}")];
        s.publish_workspace_state_if_subscribed();
        assert!(matches!(
            mixed.receiver.try_recv().unwrap(),
            IpcEvent::WorkspaceSnapshotBegin { .. }
        ));
        loop {
            if matches!(
                mixed.receiver.try_recv().unwrap(),
                IpcEvent::WorkspaceSnapshotEnd { .. }
            ) {
                break;
            }
        }
    }
    assert!(matches!(
        legacy.receiver.try_recv().unwrap(),
        IpcEvent::ConfigReloaded
    ));
    assert!(matches!(
        legacy.receiver.try_recv().unwrap(),
        IpcEvent::FocusedWindowChanged {
            monitor: 1,
            hwnd: None,
            ..
        }
    ));
    assert!(legacy.receiver.try_recv().is_err());

    // Dropping the last workspace receiver stops ordinary projection, even
    // while legacy clients remain; a later handoff still captures fresh state.
    drop(mixed);
    let last = s.workspace_snapshot_events();
    s.config.workspaces.names = vec!["after disconnect".into()];
    s.publish_workspace_state_if_subscribed();
    assert_eq!(s.workspace_snapshot_events(), last);
    drop(s);
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ipc_subscribe(
        &state,
        [EventKind::WorkspaceState].into_iter().collect(),
        tx,
    )
    .await;
    let fresh = rx.await.unwrap();
    assert!(fresh.snapshot.iter().any(|event| match event {
        IpcEvent::WorkspaceSnapshotChunk { records, .. } => records.iter().any(|record| {
            matches!(record, leopardwm_ipc::WorkspaceStateRecord::Workspace { name: Some(name), .. } if name == "after disconnect")
        }),
        _ => false,
    }));
}

#[test]
#[ignore = "manual workspace projection benchmark"]
fn benchmark_workspace_projection_with_large_membership() {
    let mut state = fixture();
    for hwnd in 1..=2_000 {
        let monitor = if hwnd % 2 == 0 { 1 } else { 2 };
        let workspace = (hwnd % 9) as usize;
        state.ensure_workspace_exists(monitor, workspace);
        state.workspaces.get_mut(&monitor).unwrap()[workspace]
            .insert_window(hwnd, None)
            .unwrap();
    }

    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        std::hint::black_box(state.project_workspace_state());
    }
    let elapsed = started.elapsed();
    eprintln!(
        "workspace projection: 2,000 windows x 1,000 iterations in {elapsed:?} ({:?}/projection)",
        elapsed / 1_000
    );
}

#[tokio::test]
async fn subscribe_handoff_has_no_duplicate_or_missing_revision() {
    use leopardwm_ipc::IpcEvent;
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(fixture()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ipc_subscribe(
        &state,
        [EventKind::WorkspaceState].into_iter().collect(),
        tx,
    )
    .await;
    let mut startup = rx.await.unwrap();
    assert!(startup.receiver.try_recv().is_err());
    let initial = match &startup.snapshot[0] {
        IpcEvent::WorkspaceSnapshotBegin {
            revision,
            session_id,
            ..
        } => (*revision, session_id.clone()),
        _ => panic!("missing begin"),
    };
    let mut s = state.lock().await;
    s.focused_monitor = 2;
    s.publish_workspace_state_if_changed();
    assert!(
        matches!(startup.receiver.try_recv().unwrap(), IpcEvent::WorkspaceSnapshotBegin { revision, session_id, .. } if revision == initial.0 + 1 && session_id == initial.1)
    );
}

#[test]
fn new_daemon_state_has_a_distinct_session() {
    let mut a = fixture();
    let mut b = fixture();
    a.publish_workspace_state_if_changed();
    b.publish_workspace_state_if_changed();
    assert_ne!(
        serde_json::to_value(&a.workspace_snapshot_events()[0]).unwrap()["session_id"],
        serde_json::to_value(&b.workspace_snapshot_events()[0]).unwrap()["session_id"]
    );
}

#[test]
fn membership_moves_and_scratchpad_visibility_replace_the_whole_model() {
    use leopardwm_ipc::WorkspaceStateRecord as R;
    let mut state = fixture();
    state.ensure_workspace_exists(2, 4);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(10, None)
        .unwrap();
    state.publish_workspace_state_if_changed();
    state.workspaces.get_mut(&1).unwrap()[0]
        .remove_window(10)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[4]
        .add_floating(10, Rect::new(0, 0, 100, 100))
        .unwrap();
    state.publish_workspace_state_if_changed();
    let windows: Vec<_> = records(&state)
        .into_iter()
        .filter(|r| matches!(r, R::Window { .. }))
        .collect();
    assert_eq!(
        windows,
        vec![R::Window {
            monitor_device_name: "DISPLAY2".into(),
            workspace_index: 4,
            hwnd: 10,
            is_floating: true,
            is_sticky: false
        }]
    );
    state.scratchpad = Some(ScratchpadState {
        window_id: 10,
        shown: false,
        saved_rect: None,
        frame_insets: None,
        origin_column: 0,
        origin_sibling: None,
    });
    state.publish_workspace_state_if_changed();
    assert!(!records(&state)
        .iter()
        .any(|r| matches!(r, R::Window { .. })));
    state.scratchpad.as_mut().unwrap().shown = true;
    state.publish_workspace_state_if_changed();
    assert!(records(&state).iter().any(|r| matches!(
        r,
        R::Window {
            hwnd: 10,
            is_floating: true,
            ..
        }
    )));
    state.workspaces.get_mut(&2).unwrap()[4].remove_floating(10);
    state.publish_workspace_state_if_changed();
    assert!(!records(&state)
        .iter()
        .any(|r| matches!(r, R::Window { .. })));
}

#[test]
fn oversize_label_fails_before_begin_and_can_recover() {
    use leopardwm_ipc::IpcEvent;
    let mut state = fixture();
    state.config.workspaces.names = vec!["界".repeat(leopardwm_ipc::MAX_IPC_MESSAGE_SIZE)];
    state.publish_workspace_state_if_changed();
    assert!(matches!(
        state.workspace_snapshot_events().as_slice(),
        [IpcEvent::WorkspaceSnapshotError { .. }]
    ));
    state.config.workspaces.names.clear();
    state.publish_workspace_state_if_changed();
    assert!(matches!(
        state.workspace_snapshot_events().last(),
        Some(IpcEvent::WorkspaceSnapshotEnd { .. })
    ));
}

#[test]
fn drag_preview_keeps_temporarily_detached_window_in_source_workspace() {
    use leopardwm_ipc::WorkspaceStateRecord as R;
    let mut state = fixture();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(10, None)
        .unwrap();
    state.publish_workspace_state_if_changed();
    let mut rx = state.workspace_event_broadcaster.subscribe();
    state.workspaces.get_mut(&1).unwrap()[0]
        .remove_window(10)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(DRAG_PLACEHOLDER_HWND, None)
        .unwrap();
    state.drag_state = Some(DragState {
        hwnd: 10,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: true,
        preview_mode: DragPreviewMode::Body,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });
    state.publish_workspace_state_if_changed();
    assert!(records(&state).iter().any(|r| matches!(r, R::Window { monitor_device_name, hwnd: 10, .. } if monitor_device_name == "DISPLAY1")));
    assert!(
        rx.try_recv().is_err(),
        "drag preview alone is not a membership change"
    );
    state.workspaces.get_mut(&2).unwrap()[0]
        .remove_window(DRAG_PLACEHOLDER_HWND)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(10, None)
        .unwrap();
    state.drag_state = None;
    state.publish_workspace_state_if_changed();
    assert!(records(&state).iter().any(|r| matches!(r, R::Window { monitor_device_name, hwnd: 10, .. } if monitor_device_name == "DISPLAY2")));
}
