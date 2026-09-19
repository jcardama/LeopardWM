use super::*;

fn two_monitor_state() -> AppState {
    AppState::new_with_config(
        Config::default(),
        vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY2".to_string(),
                scale_factor: 1.0,
            },
        ],
    )
}

fn targeted_switch(monitor_device_name: &str, index: u8) -> IpcCommand {
    serde_json::from_value(serde_json::json!({
        "type": "switch_workspace_on_monitor",
        "monitor_device_name": monitor_device_name,
        "index": index,
    }))
    .expect("targeted workspace switch command should deserialize")
}

fn assert_error_contains(response: IpcResponse, expected: &str) {
    match response {
        IpcResponse::Error { message } => assert!(
            message.contains(expected),
            "expected error containing {expected:?}, got {message:?}"
        ),
        other => panic!("expected error containing {expected:?}, got {other:?}"),
    }
}

#[test]
fn targeted_switch_rejects_invalid_index_without_side_effects() {
    let mut state = two_monitor_state();
    state.overview_open = true;
    let active_before = state.active_workspace.clone();

    let response = state.handle_command(targeted_switch("DISPLAY2", 0));

    assert_error_contains(response, "1-9");
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace, active_before);
    assert!(state.overview_open);
    assert!(state.pending_drag_hint.is_none());
}

#[test]
fn targeted_switch_rejects_unknown_monitor_without_side_effects() {
    let mut state = two_monitor_state();
    state.overview_open = true;
    let active_before = state.active_workspace.clone();

    let response = state.handle_command(targeted_switch("MISSING", 2));

    assert_error_contains(response, "MISSING");
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace, active_before);
    assert!(state.overview_open);
    assert!(state.pending_drag_hint.is_none());
}

#[test]
fn targeted_switch_changes_and_focuses_an_unfocused_monitor() {
    let mut state = two_monitor_state();

    let response = state.handle_command(targeted_switch("DISPLAY2", 2));

    assert_eq!(response, IpcResponse::Ok);
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(state.active_workspace_idx(2), 1);
}

#[test]
fn targeted_switch_focuses_target_when_workspace_is_already_active() {
    let mut state = two_monitor_state();
    state.ensure_workspace_exists(2, 1);
    state.workspaces.get_mut(&2).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.active_workspace.insert(2, 1);

    let response = state.handle_command(targeted_switch("DISPLAY2", 2));

    assert_eq!(response, IpcResponse::Ok);
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(state.active_workspace_idx(2), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn targeted_switch_creates_an_empty_destination() {
    let mut state = two_monitor_state();

    let response = state.handle_command(targeted_switch("DISPLAY2", 4));

    assert_eq!(response, IpcResponse::Ok);
    assert_eq!(state.active_workspace_idx(2), 3);
    assert_eq!(state.workspaces[&2].len(), 4);
    assert_eq!(state.workspaces[&2][3].window_count(), 0);
    assert_eq!(state.workspaces[&2][3].floating_count(), 0);
}

#[test]
fn targeted_switch_leaves_other_monitor_workspace_unchanged() {
    let mut state = two_monitor_state();
    state.ensure_workspace_exists(1, 2);
    state.active_workspace.insert(1, 2);

    let response = state.handle_command(targeted_switch("DISPLAY2", 2));

    assert_eq!(response, IpcResponse::Ok);
    assert_eq!(state.active_workspace_idx(1), 2);
    assert_eq!(state.active_workspace_idx(2), 1);
}

#[test]
fn targeted_switch_preserves_the_unfocused_monitors_floating_focus_history() {
    let mut state = two_monitor_state();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.previous_focused_hwnd = Some(100);

    state.ensure_workspace_exists(2, 1);
    state.workspaces.get_mut(&2).unwrap()[0]
        .add_floating(200, Rect::new(100, 100, 400, 300))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[1]
        .add_floating(201, Rect::new(200, 200, 400, 300))
        .unwrap();
    state.floating_focus.insert((2, 0), 200);
    state.floating_focus.insert((2, 1), 201);

    let response = state.handle_command(targeted_switch("DISPLAY2", 2));

    assert_eq!(response, IpcResponse::Ok);
    assert_eq!(state.floating_focus.get(&(2, 0)), Some(&200));
    assert_eq!(state.previous_focused_hwnd, Some(201));
}
