use crate::config::{self, Config};
use crate::event_handler::AdmitOutcome;
use crate::layout_apply::LayoutApplyOutcome;
use crate::state::{
    AppState, DragPreviewMode, DragState, MoveOrigin, StashedMonitorLayout,
    TestApplyPlacementsBehavior, TestApplyPlacementsOutcome, TestApplyPlacementsStep,
};
use crate::temporary_ignore::{IdentityReadError, IdleLayoutReapply};
use leopardwm_core_layout::{Rect, Visibility};
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{
    ManageBlock, MonitorInfo, PlacementLanding, WindowEvent, WindowInfo,
};
use std::sync::atomic::Ordering;
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

fn record_peer_placements(state: &mut AppState) {
    state.paused = false;
    state.reduce_motion = true;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: vec![PlacementLanding {
                    window_id: 20,
                    requested_rect: Rect::new(20, 20, 400, 300),
                    requested_visibility: Visibility::Visible,
                    actual_visible_rect: Some(Rect::new(20, 20, 400, 300)),
                    actual_outer_rect: Some(Rect::new(20, 20, 400, 300)),
                    failed: false,
                    unreadable: false,
                }],
            },
        },
    ]));
}

fn placement_batches_contain(state: &AppState, hwnd: u64) -> bool {
    state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .iter()
        .any(|batch| batch.contains(&hwnd))
}

fn last_placement_batch(state: &AppState) -> Vec<u64> {
    state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default()
}

fn consume_idle_until_settled(state: &mut AppState) -> IdleLayoutReapply {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let outcome = state.try_consume_idle_layout_reapply();
        if !matches!(outcome, IdleLayoutReapply::Waiting) {
            return outcome;
        }
        assert!(
            Instant::now() < deadline,
            "idle layout reapply stayed Waiting"
        );
        std::thread::yield_now();
    }
}

fn seed_recycled_lifetime_caches(state: &mut AppState, hwnd: u64) {
    state.overview_icon_cache.insert(hwnd, Some(0x1234));
    state.move_origins.insert(
        hwnd,
        MoveOrigin {
            monitor: 1,
            ws_idx: 0,
            column: 0,
            sibling: None,
        },
    );
    state.move_origins.insert(
        20,
        MoveOrigin {
            monitor: 1,
            ws_idx: 0,
            column: 0,
            sibling: Some(hwnd),
        },
    );
    let mut stale_workspace = state.focused_workspace().unwrap().clone();
    stale_workspace.insert_window(hwnd, None).unwrap();
    state.stashed_monitor_layouts.insert(
        "STALE".into(),
        StashedMonitorLayout {
            workspaces: vec![stale_workspace],
            active_workspace: 0,
            source_viewport_width: 1920,
        },
    );
}

fn assert_recycled_lifetime_caches_cleared(state: &AppState, hwnd: u64) {
    assert!(!state.overview_icon_cache.contains_key(&hwnd));
    assert!(!state.move_origins.contains_key(&hwnd));
    assert_eq!(state.move_origins.get(&20).unwrap().sibling, None);
    assert!(state.stashed_monitor_layouts.values().all(|layout| layout
        .workspaces
        .iter()
        .all(|workspace| !workspace.contains_window(hwnd))));
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
    state.injected_identity_read_error =
        Some(IdentityReadError::Transient("identity api failed".into()));
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
    assert_eq!(state.previous_focused_hwnd, Some(40));
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 40);
    assert_eq!(state.last_broadcast_focused, Some((1, Some(40))));
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
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );
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
    assert!(state.pending_idle_layout_reapply);
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
    assert!(state.pending_idle_layout_reapply);
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

#[test]
fn delayed_destroy_skips_cleanup_for_reused_managed_and_ignored_hwnd() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    let old_token = state.temporary_ignores.get(&10).unwrap().token;
    state.injected_lifetime_tokens.insert(10, old_token + 99);
    state.handle_window_event(WindowEvent::Created(10));
    assert!(state.find_window_workspace(10).is_some());
    state.tab_title_overrides.insert(10, "replacement".into());
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("replacement")
    );

    let mut ignored = managed_state();
    ignore_foreground(&mut ignored, 20);
    ignored.tab_title_overrides.insert(20, "ignored".into());
    ignored.handle_window_event(WindowEvent::Destroyed(20));
    assert!(ignored.temporary_ignores.contains_key(&20));
    assert!(ignored.find_window_workspace(20).is_none());
    assert_eq!(
        ignored.tab_title_overrides.get(&20).map(String::as_str),
        Some("ignored")
    );
}

#[test]
fn production_equivalent_dead_identity_prunes_ignore() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.injected_identity_read_error = Some(IdentityReadError::Gone);
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert!(state.temporary_ignores.is_empty());
    assert!(state.find_window_workspace(10).is_none());
    assert!(!state.all_managed_window_ids().contains(&10));
}

#[test]
fn injected_live_unmarked_hwnd_skips_delayed_destroy_cleanup() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.injected_lifetime_tokens.remove(&10);
    state.injected_live_hwnds.insert(10);
    state.handle_window_event(WindowEvent::Created(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    state.tab_title_overrides.insert(10, "replacement".into());
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("replacement")
    );
}

#[test]
fn apply_layout_failure_is_truthful_on_unmanage_and_readmit() {
    let mut unmanage = managed_state();
    unmanage.reduce_motion = true;
    unmanage.paused = false;
    unmanage
        .apply_worker_cancelled
        .store(true, Ordering::SeqCst);
    set_foreground(&mut unmanage, 10);
    let message = error_message(unmanage.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("layout failed"));
    assert!(unmanage.find_window_workspace(10).is_none());
    assert!(unmanage.temporary_ignores.contains_key(&10));
    assert!(!unmanage.all_managed_window_ids().contains(&10));

    let mut readmit = managed_state();
    ignore_foreground(&mut readmit, 10);
    readmit.reduce_motion = true;
    readmit.paused = false;
    readmit.apply_worker_cancelled.store(true, Ordering::SeqCst);
    set_foreground(&mut readmit, 10);
    let message = error_message(readmit.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("re-admitted"));
    assert!(message.contains("layout failed"));
    assert!(readmit.find_window_workspace(10).is_some());
    assert!(!readmit.temporary_ignores.contains_key(&10));
    assert_eq!(
        readmit
            .all_managed_window_ids()
            .iter()
            .filter(|&&id| id == 10)
            .count(),
        1
    );
}

#[test]
fn unmanage_clears_owned_tab_title_and_survives_hwnd_reuse() {
    let mut state = managed_state();
    state.tab_title_overrides.insert(10, "owned".into());
    set_foreground(&mut state, 10);
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert!(!state.tab_title_overrides.contains_key(&10));
    assert!(state.temporary_ignores.contains_key(&10));

    let old_token = state.temporary_ignores.get(&10).unwrap().token;
    state.injected_lifetime_tokens.insert(10, old_token + 7);
    state.handle_window_event(WindowEvent::Created(10));
    assert!(state.find_window_workspace(10).is_some());
    assert!(!state.temporary_ignores.contains_key(&10));
}

#[test]
fn identity_mismatch_reflows_peers_without_restoring_native() {
    let mut state = managed_state();
    state.injected_native_restore_error = Some("must not restore onto replacement".into());
    state.injected_identity_read_override = Some(Ok(Some(u64::MAX)));
    set_foreground(&mut state, 10);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("not ignored"));
    assert!(
        !message.contains("must not restore"),
        "mismatch must not restore native state onto the replacement: {message}"
    );
    assert!(state.find_window_workspace(10).is_none());
    assert!(!state.temporary_ignores.contains_key(&10));
    assert!(state.find_window_workspace(20).is_some());
    assert!(state.find_window_workspace(30).is_some());
}

#[test]
fn failed_drain_reapplies_only_after_workers_idle() {
    let mut state = managed_state();
    record_peer_placements(&mut state);
    set_foreground(&mut state, 10);
    let (finish_tx, finish_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || finish_rx.recv().unwrap());
    state.pending_apply_workers.push(handle);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains managed"));
    assert!(state.find_window_workspace(10).is_some());
    assert!(!state.paused);
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Waiting
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );

    finish_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        consume_idle_until_settled(&mut state),
        IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.paused);
    assert!(state.find_window_workspace(10).is_some());
    assert!(
        placement_batches_contain(&state, 20),
        "idle reapply must place remaining peer 20, got {:?}",
        state.injected_apply_placements_batches.lock().unwrap()
    );
}

#[test]
fn failed_drain_reapplies_after_animation_barrier_idle() {
    let mut state = managed_state();
    record_peer_placements(&mut state);
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
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Waiting
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );
    unblock.send(()).unwrap();
    assert_eq!(
        consume_idle_until_settled(&mut state),
        IdleLayoutReapply::Applied
    );
    assert!(!state.paused);
    assert!(state.find_window_workspace(10).is_some());
    assert!(
        placement_batches_contain(&state, 20),
        "barrier-idle reapply must place remaining peer 20, got {:?}",
        state.injected_apply_placements_batches.lock().unwrap()
    );
}

#[test]
fn transient_identity_after_drain_reapplies_peers() {
    let mut state = managed_state();
    record_peer_placements(&mut state);
    set_foreground(&mut state, 10);
    state.injected_identity_read_override = Some(Err(IdentityReadError::Transient(
        "identity probe failed".into(),
    )));
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains managed"));
    assert!(state.find_window_workspace(10).is_some());
    assert!(!state.temporary_ignores.contains_key(&10));
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::NotPending
    );
    assert!(
        placement_batches_contain(&state, 20),
        "transient-after-drain must reapply peer 20, got {:?}",
        state.injected_apply_placements_batches.lock().unwrap()
    );
}

#[test]
fn rejected_readmit_after_drain_reapplies_peers() {
    let mut state = managed_state();
    record_peer_placements(&mut state);
    ignore_foreground(&mut state, 10);
    let calls_after_unmanage = state
        .injected_apply_placements_call_count
        .load(Ordering::SeqCst);
    state.config.window_rules.push(config::WindowRule {
        match_class: Some("TiledClass".into()),
        action: config::WindowAction::Ignore,
        ..Default::default()
    });
    state.compiled_rules = state.config.compile_window_rules();
    set_foreground(&mut state, 10);
    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));
    assert!(message.contains("remains ignored"));
    assert!(state.temporary_ignores.contains_key(&10));
    assert!(state.find_window_workspace(10).is_none());
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > calls_after_unmanage
    );
    assert!(
        last_placement_batch(&state).contains(&20),
        "rejected readmit after drain must reapply peer 20, last batch {:?}",
        last_placement_batch(&state)
    );
}

#[test]
fn unmanage_cancels_unfinished_move_size_so_end_does_not_reinsert() {
    let mut state = managed_state();
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
        preview_mode: DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });
    ignore_foreground(&mut state, 10);
    assert!(state.drag_state.is_none());
    state.handle_window_event(WindowEvent::MoveSizeEnd(10));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.temporary_ignores.contains_key(&10));
    assert_eq!(
        state
            .all_managed_window_ids()
            .iter()
            .filter(|&&id| id == 10)
            .count(),
        0
    );
}

#[test]
fn dead_ignored_hwnd_does_not_clear_live_focus() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    state.previous_focused_hwnd = Some(20);
    state.last_broadcast_focused = Some((1, Some(20)));
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    state.injected_identity_read_error = Some(IdentityReadError::Gone);
    state.handle_window_event(WindowEvent::Focused(10, 0));
    assert_eq!(state.previous_focused_hwnd, Some(20));
    assert_eq!(state.last_broadcast_focused, Some((1, Some(20))));
    assert_eq!(
        state.border_hide_count.load(Ordering::Relaxed),
        hides_before
    );
}

#[test]
fn ordinary_apply_waits_for_pending_recovery_animation_barrier() {
    let mut state = managed_state();
    record_peer_placements(&mut state);
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = crate::animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());
    state.pending_idle_layout_reapply = true;

    assert_eq!(
        state.apply_layout().unwrap(),
        LayoutApplyOutcome::DeferredByRecoveryBarrier
    );

    assert!(state.pending_idle_layout_reapply);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "ordinary apply must not dispatch while recovery's animation barrier is busy"
    );
    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(state.apply_layout().unwrap(), LayoutApplyOutcome::Completed);
    assert!(!state.pending_idle_layout_reapply);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0
    );
}

#[test]
fn pending_recovery_retries_failed_apply_then_clears_only_after_success() {
    let mut state = managed_state();
    state.reduce_motion = true;
    state.paused = false;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Fail,
        },
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: Vec::new(),
            },
        },
    ]));
    state.pending_idle_layout_reapply = true;

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Waiting
    );
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 1);

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 0);
}

#[test]
fn pending_recovery_defers_after_bounded_failures_without_clearing_request() {
    let mut state = managed_state();
    state.reduce_motion = true;
    state.paused = false;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));
    state.pending_idle_layout_reapply = true;

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Waiting
    );
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::Waiting
    );
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        IdleLayoutReapply::DeferredFailure
    );
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 3);
    assert!(!state.idle_layout_reapply_timer_needed());
}

#[test]
fn reused_ignored_hwnd_clears_old_caches_before_preserving_new_lifetime() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    let old_token = state.temporary_ignores.get(&10).unwrap().token;
    seed_recycled_lifetime_caches(&mut state, 10);

    state.injected_lifetime_tokens.insert(10, old_token + 1);
    state.handle_window_event(WindowEvent::Created(10));

    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert!(!state.temporary_ignores.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&state, 10);

    state.overview_icon_cache.insert(10, Some(0x5678));
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x5678)));
}

#[test]
fn delayed_destroy_before_created_retires_old_ignored_lifetime_caches() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    let old_token = state.temporary_ignores.get(&10).unwrap().token;
    seed_recycled_lifetime_caches(&mut state, 10);
    state.injected_lifetime_tokens.insert(10, old_token + 1);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert!(!state.temporary_ignores.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&state, 10);
    state.handle_window_event(WindowEvent::Created(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));

    state.overview_icon_cache.insert(10, Some(0x5678));
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x5678)));
}

#[test]
fn explicit_mismatch_retires_old_caches_before_automatic_admission() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);
    let old_token = state.temporary_ignores.get(&10).unwrap().token;
    seed_recycled_lifetime_caches(&mut state, 10);
    state.injected_lifetime_tokens.insert(10, old_token + 1);
    set_foreground(&mut state, 10);

    let message = error_message(state.handle_command(IpcCommand::ToggleIgnore));

    assert!(message.contains("not the ignored lifetime"));
    assert!(!state.temporary_ignores.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&state, 10);
    state.handle_window_event(WindowEvent::Created(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));

    state.overview_icon_cache.insert(10, Some(0x5678));
    state.handle_window_event(WindowEvent::Destroyed(10));
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x5678)));
}

#[test]
fn pending_recovery_respects_paused_and_cancelled_guards_before_barrier() {
    let mut paused = managed_state();
    let (paused_tx, _paused_rx) = tokio::sync::mpsc::channel(4);
    let paused_worker = crate::animation_worker::AnimationWorkerHandle::spawn(
        paused_tx,
        paused.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let paused_unblock = paused_worker.block_for_test();
    paused.animation_worker_control = Some(paused_worker.control());
    paused.pending_idle_layout_reapply = true;

    assert_eq!(
        paused.apply_layout().unwrap(),
        LayoutApplyOutcome::Completed
    );
    assert_eq!(
        paused
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );
    paused_unblock.send(()).unwrap();

    let mut cancelled = managed_state();
    cancelled.paused = false;
    let (cancelled_tx, _cancelled_rx) = tokio::sync::mpsc::channel(4);
    let cancelled_worker = crate::animation_worker::AnimationWorkerHandle::spawn(
        cancelled_tx,
        cancelled.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let cancelled_unblock = cancelled_worker.block_for_test();
    cancelled.animation_worker_control = Some(cancelled_worker.control());
    cancelled.pending_idle_layout_reapply = true;
    cancelled
        .apply_worker_cancelled
        .store(true, Ordering::SeqCst);

    let error = cancelled.apply_layout().unwrap_err();
    assert!(error.to_string().contains("shutdown/revert cleanup"));
    assert_eq!(
        cancelled
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );
    cancelled_unblock.send(()).unwrap();
}

#[test]
fn temporary_ignore_test_seam_records_native_uncloak_intent() {
    let mut state = managed_state();
    ignore_foreground(&mut state, 10);

    assert_eq!(
        state.injected_native_uncloak_count.load(Ordering::Relaxed),
        1,
        "temporary ignore must request native uncloaking in production"
    );
}
