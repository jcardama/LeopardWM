use crate::config::{self, Config};
use crate::event_handler::AdmitOutcome;
use crate::state::AppState;
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{ManageBlock, MonitorInfo, WindowEvent, WindowInfo};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn state() -> AppState {
    AppState::new_with_config(
        Config::default(),
        vec![MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        }],
    )
}

fn info(hwnd: u64, title: &str, class_name: &str, process_id: u32) -> WindowInfo {
    WindowInfo {
        hwnd,
        title: title.to_string(),
        class_name: class_name.to_string(),
        process_id,
        rect: Rect::new(100, 100, 800, 600),
        visible: true,
    }
}

fn inject(state: &mut AppState, hwnd: u64, title: &str, class_name: &str, process_id: u32) {
    state
        .injected_window_info
        .insert(hwnd, info(hwnd, title, class_name, process_id));
}

fn set_foreground(state: &mut AppState, hwnd: u64) {
    state.injected_foreground_hwnd = Some(Some(hwnd));
    state.injected_foreground_is_valid = Some(true);
}

fn managed_state() -> AppState {
    let mut state = state();
    inject(&mut state, 10, "Tiled", "TiledClass", 1010);
    inject(&mut state, 20, "Floating", "FloatingClass", 1020);
    inject(&mut state, 30, "Other", "OtherClass", 1030);
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(10, None)
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .add_floating(20, Rect::new(20, 20, 400, 300))
        .unwrap();
    state.ensure_workspace_exists(1, 2);
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(30, None)
        .unwrap();
    state
}

fn ignore_foreground(state: &mut AppState, hwnd: u64) {
    set_foreground(state, hwnd);
    assert!(
        matches!(
            state.handle_command(IpcCommand::ToggleIgnore),
            IpcResponse::Ok
        ),
        "toggle-ignore out should succeed for {hwnd}"
    );
    assert!(state.temporary_ignores.contains_key(&hwnd));
    assert!(state.find_window_workspace(hwnd).is_none());
}

fn error_message(response: IpcResponse) -> String {
    match response {
        IpcResponse::Error { message } => message,
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn toggle_ignore_uses_actual_foreground_not_cached_focus() {
    let mut state = managed_state();
    state.previous_focused_hwnd = Some(10);
    set_foreground(&mut state, 20);

    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert!(state.find_window_workspace(10).is_some());
    assert!(state.find_window_workspace(20).is_none());
    assert!(state.temporary_ignores.contains_key(&20));
    assert!(!state.temporary_ignores.contains_key(&10));
}

#[test]
fn toggle_ignore_rejects_missing_or_invalid_foreground() {
    let mut state = managed_state();
    state.injected_foreground_hwnd = Some(None);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("No foreground window"));

    state.injected_foreground_hwnd = Some(Some(10));
    state.injected_foreground_is_valid = Some(false);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("not valid"));
}

#[test]
fn tiled_and_floating_toggle_out_and_back() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    set_foreground(&mut state, 10);
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert!(state.find_window_workspace(10).is_some());
    assert!(!state.temporary_ignores.contains_key(&10));
    assert!(!state.focused_workspace().unwrap().is_floating(10));

    ignore_foreground(&mut state, 20);
    set_foreground(&mut state, 20);
    state.config.window_rules.push(config::WindowRule {
        match_class: Some("FloatingClass".to_string()),
        action: config::WindowAction::Float,
        ..Default::default()
    });
    state.compiled_rules = state.config.compile_window_rules();
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert!(state.focused_workspace().unwrap().is_floating(20));
    assert!(!state.temporary_ignores.contains_key(&20));
}

#[test]
fn ignored_window_stays_out_through_lifecycle_and_release() {
    let mut state = managed_state();
    let paused = state.paused;
    ignore_foreground(&mut state, 10);
    assert_eq!(state.paused, paused);
    assert_eq!(
        state
            .temporary_ignores
            .get(&10)
            .map(|entry| (entry.process_id, entry.class_name.as_str())),
        Some((1010, "TiledClass"))
    );

    state.handle_window_event(WindowEvent::Created(10));
    state.handle_window_event(WindowEvent::Created(10));
    state.handle_window_event(WindowEvent::Focused(10, 0));
    state.handle_window_event(WindowEvent::Hidden(10));
    state.handle_window_event(WindowEvent::Minimized(10));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));

    state.injected_enumerated_windows = Some(vec![info(10, "Tiled", "TiledClass", 1010)]);
    assert!(matches!(
        state.handle_command(IpcCommand::Refresh),
        IpcResponse::Ok
    ));
    assert!(state.find_window_workspace(10).is_none());

    let config = state.config.clone();
    state.apply_config(config);
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));

    state.paused = false;
    state.release_all_windows().unwrap();
    assert!(state.paused);
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(state.find_window_workspace(20).is_some());
    assert!(!state.all_managed_window_ids().contains(&10));

    state.toggle_pause("test resume after ignore").unwrap();
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(!state.paused);
}

#[test]
fn new_daemon_has_empty_temporary_ignore_set() {
    let state = state();
    assert!(state.temporary_ignores.is_empty());
}

#[test]
fn hwnd_reuse_and_delayed_destroy_preserve_new_lifetime() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    let old_token = state.temporary_ignores.get(&10).unwrap().token;

    state.injected_lifetime_tokens.insert(10, old_token + 99);
    state.handle_window_event(WindowEvent::Created(10));
    assert!(state.find_window_workspace(10).is_some());
    assert!(!state.temporary_ignores.contains_key(&10));

    ignore_foreground(&mut state, 10);
    let new_token = state.temporary_ignores.get(&10).unwrap().token;
    assert_ne!(new_token, old_token);
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(
        state.temporary_ignores.get(&10).map(|entry| entry.token),
        Some(new_token)
    );
    assert!(state.find_window_workspace(10).is_none());
}

#[test]
fn stale_identity_is_pruned_and_failed_read_stays_closed() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.injected_lifetime_tokens.remove(&10);
    assert!(matches!(
        state.try_admit_window(10, crate::event_handler::AdmissionKind::Automatic),
        AdmitOutcome::Admitted
    ));
    assert!(state.find_window_workspace(10).is_some());

    ignore_foreground(&mut state, 10);
    state.injected_identity_read_error = Some("identity api failed".into());
    state.handle_window_event(WindowEvent::Created(10));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));
}

#[test]
fn readmit_rejects_keep_ignore_for_rule_elevation_and_ineligible() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.config.window_rules.push(config::WindowRule {
        match_class: Some("TiledClass".to_string()),
        action: config::WindowAction::Ignore,
        ..Default::default()
    });
    state.compiled_rules = state.config.compile_window_rules();
    set_foreground(&mut state, 10);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("persistent Ignore"));
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(state.find_window_workspace(10).is_none());

    let mut elevated = managed_state();
    ignore_foreground(&mut elevated, 20);
    elevated
        .injected_manage_block
        .insert(20, ManageBlock::HigherIntegrity);
    set_foreground(&mut elevated, 20);
    let message = error_message(elevated.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("elevation"));
    assert!(elevated.temporary_ignores.contains_key(&20));

    let mut ineligible = managed_state();
    set_foreground(&mut ineligible, 99);
    let message = error_message(ineligible.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("not managed"));
}

#[test]
fn explicit_readmit_overrides_workspace_routing_without_focus_switch() {
    let mut config = Config::default();
    config.behavior.focus_new_windows = false;
    config.window_rules.push(config::WindowRule {
        match_class: Some("RoutedClass".to_string()),
        open_on_workspace: Some(5),
        action: config::WindowAction::Tile,
        ..Default::default()
    });
    let mut state = AppState::new_with_config(
        config,
        vec![MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        }],
    );
    inject(&mut state, 40, "Routed", "RoutedClass", 1040);
    state.handle_window_event(WindowEvent::Created(40));
    assert_eq!(state.find_window_workspace(40), Some((1, 4)));
    assert_eq!(state.active_workspace_idx(1), 0);
    let focused_monitor = state.focused_monitor;
    state.previous_focused_hwnd = Some(10);

    ignore_foreground(&mut state, 40);
    set_foreground(&mut state, 40);
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert_eq!(state.find_window_workspace(40), Some((1, 0)));
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.focused_monitor, focused_monitor);
    assert_eq!(state.previous_focused_hwnd, Some(10));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(40)
    );
    state.handle_window_event(WindowEvent::Created(40));
    assert_eq!(
        state
            .all_managed_window_ids()
            .iter()
            .filter(|&&id| id == 40)
            .count(),
        1
    );
}

#[test]
fn stamp_and_drain_failures_keep_ownership_and_pause() {
    let mut state = managed_state();
    let paused = state.paused;
    set_foreground(&mut state, 10);
    state.injected_identity_stamp_error = Some("stamp failed".into());
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("stamp"));
    assert!(state.find_window_workspace(10).is_some());
    assert!(state.temporary_ignores.is_empty());
    assert_eq!(state.paused, paused);

    set_foreground(&mut state, 10);
    let (finish_tx, finish_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || finish_rx.recv().unwrap());
    state.pending_apply_workers.push(handle);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains managed"));
    assert!(state.find_window_workspace(10).is_some());
    assert!(state.temporary_ignores.is_empty());
    assert_eq!(state.paused, paused);
    finish_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn readmit_drain_failure_keeps_ignore_and_pause() {
    let mut state = managed_state();
    let paused = state.paused;
    ignore_foreground(&mut state, 10);
    set_foreground(&mut state, 10);
    let (finish_tx, finish_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || finish_rx.recv().unwrap());
    state.pending_apply_workers.push(handle);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains ignored"));
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(state.find_window_workspace(10).is_none());
    assert_eq!(state.paused, paused);
    finish_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn animation_barrier_failure_keeps_managed_window() {
    let mut state = managed_state();
    let paused = state.paused;
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = crate::animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());
    set_foreground(&mut state, 10);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains managed"));
    assert!(state.find_window_workspace(10).is_some());
    assert_eq!(state.paused, paused);
    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
}

#[test]
fn native_restore_failure_still_unmanages_and_ignores() {
    let mut state = managed_state();
    set_foreground(&mut state, 10);
    state.injected_native_restore_error = Some("restore failed".into());
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("was unmanaged"));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(!state.all_managed_window_ids().contains(&10));
}

#[test]
fn recently_hidden_recovery_does_not_readmit_ignored_window() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.recently_hidden_hwnds.insert(10, Instant::now());
    state.handle_window_event(WindowEvent::Focused(10, 0));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));
}
