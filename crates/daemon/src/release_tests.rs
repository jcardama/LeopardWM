use crate::config::Config;
use crate::helpers::StalePruneLayout;
use crate::state::{AppState, DragHintAction, TestApplyPlacementsBehavior};
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{MonitorInfo, WindowEvent, WindowInfo};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::animation_worker::AnimationWorkerHandle;
use crate::layout_apply::AnimationPlacementResult;
use crate::state::{CrossfadeState, LayoutTransition};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::Instant;

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

fn managed_state() -> AppState {
    let mut state = state();
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

#[test]
fn release_all_windows_pauses_and_retains_tiled_and_floating_membership() {
    let mut state = managed_state();
    state.paused = false;
    state.previous_focused_hwnd = Some(10);
    state.snap_disabled_hwnds.insert(10);
    state.pending_drag_hint = Some(DragHintAction::ShowGhost {
        rect: Rect::new(0, 0, 1, 1),
    });
    let before = state.all_managed_window_ids();

    assert!(matches!(
        state.handle_command(IpcCommand::ReleaseAllWindows),
        IpcResponse::Ok
    ));
    assert!(state.paused);
    assert_eq!(state.all_managed_window_ids(), before);
    assert_eq!(state.released_window_id_batches, vec![before]);
    assert!(state.previous_focused_hwnd.is_none());
    assert!(state.snap_disabled_hwnds.is_empty());
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::Hide)
    ));
    assert!(state.border_hide_count.load(Ordering::Relaxed) > 0);
    assert!(state.tab_strip_hide_count.load(Ordering::Relaxed) > 0);
}

#[test]
fn release_all_windows_is_safe_when_already_paused_or_repeated() {
    let mut state = managed_state();
    let expected = state.all_managed_window_ids();

    assert!(state.release_all_windows().is_ok());
    assert!(state.release_all_windows().is_ok());
    assert!(state.paused);
    assert_eq!(state.all_managed_window_ids(), expected);
    assert_eq!(
        state.released_window_id_batches,
        vec![expected.clone(), expected]
    );
}

#[test]
fn release_all_windows_cascades_empty_state_and_keeps_paused_after_error() {
    let mut empty = state();
    empty.paused = false;
    assert!(empty.release_all_windows().is_ok());
    assert!(empty.paused);
    assert_eq!(empty.released_window_id_batches, vec![Vec::<u64>::new()]);

    let mut failing = managed_state();
    failing.paused = false;
    failing.injected_release_cascade_result = Some(Err("injected cascade failure".to_string()));
    let response = failing.handle_command(IpcCommand::ReleaseAllWindows);
    assert!(matches!(response, IpcResponse::Error { .. }));
    assert!(failing.paused);
    assert_eq!(failing.released_window_id_batches.len(), 1);
}

#[test]
fn toggle_pause_command_resumes_after_release() {
    let mut state = managed_state();
    state.paused = false;
    state.release_all_windows().unwrap();
    assert!(state.paused);

    assert!(matches!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    ));
    assert!(!state.paused);
}

#[test]
fn failed_toggle_pause_command_after_release_keeps_tiling_paused() {
    let mut state = managed_state();
    state.paused = false;
    state.release_all_windows().unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    assert!(matches!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Error { .. }
    ));
    assert!(state.paused);
}

#[test]
fn release_waits_for_retained_apply_worker_recovery_before_cascading() {
    let mut state = managed_state();
    let (finish_tx, finish_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || finish_rx.recv().unwrap());
    state.pending_apply_workers.push(handle);

    let error = state.release_all_windows().unwrap_err().to_string();
    assert!(error.contains("No cascade was performed"));
    assert!(state.paused);
    assert!(state.released_window_id_batches.is_empty());
    assert!(!state.apply_worker_cancelled.load(Ordering::SeqCst));

    finish_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(state.released_window_id_batches.is_empty());
    state.release_all_windows().unwrap();
    assert!(state.pending_apply_workers.is_empty());
    assert_eq!(state.released_window_id_batches.len(), 1);
}

#[test]
fn blocked_animation_worker_requires_explicit_retry_even_after_resume() {
    let mut state = managed_state();
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker =
        AnimationWorkerHandle::spawn(event_tx, state.apply_worker_cancelled.clone()).unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    let start = Instant::now();
    let error = state.release_all_windows().unwrap_err().to_string();
    assert!(error.contains("No cascade was performed"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(state.paused);
    assert!(state.released_window_id_batches.is_empty());
    state.toggle_pause("test resume").unwrap();
    assert!(!state.paused);
    assert!(
        state.release_all_windows().is_err(),
        "a failed barrier must not authorize the next release"
    );
    assert!(state.paused);

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert!(
        state.released_window_id_batches.is_empty(),
        "no deferred cascade"
    );
    state.release_all_windows().unwrap();
    assert_eq!(state.released_window_id_batches.len(), 1);
    state.toggle_pause("test resume after release").unwrap();
    assert!(!state.paused);
}

#[test]
fn release_invalidates_queued_frames_and_bounds_full_event_channel_wait() {
    let mut state = managed_state();
    let worker;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
    event_tx
        .try_send(crate::DaemonEvent::CrossfadeComplete { epoch: 99 })
        .unwrap();
    worker = AnimationWorkerHandle::spawn(event_tx, state.apply_worker_cancelled.clone()).unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());
    let placements =
        state.apply_physical_projection(vec![leopardwm_core_layout::WindowPlacement {
            window_id: 10,
            rect: Rect::new(0, 0, 500, 500),
            visibility: leopardwm_core_layout::Visibility::Visible,
            column_index: 0,
        }]);
    let request = state.prepare_animation_frame(placements, &HashSet::new());
    worker.send_frame(request).unwrap();

    assert!(state.release_all_windows().is_err());
    unblock.send(()).unwrap();
    let start = Instant::now();
    assert!(
        state.release_all_windows().is_err(),
        "full result channel blocks the barrier"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(state.released_window_id_batches.is_empty());
    assert!(state.paused);

    assert!(matches!(
        event_rx.blocking_recv(),
        Some(crate::DaemonEvent::CrossfadeComplete { epoch: 99 })
    ));
    let Some(crate::DaemonEvent::AnimationFrameApplied(result)) = event_rx.blocking_recv() else {
        panic!("missing stale frame result");
    };
    assert!(
        result.landings.is_empty(),
        "invalidated frame must skip native dispatch"
    );
    assert!(matches!(
        state.handle_animation_placement_result(&result),
        AnimationPlacementResult::Stale
    ));
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert!(state.released_window_id_batches.is_empty());
    state.release_all_windows().unwrap();
    assert_eq!(state.released_window_id_batches.len(), 1);
}

#[test]
fn release_aborts_transitions_but_preserves_crossfade_acknowledgement_ownership() {
    let mut state = managed_state();
    state.layout_transition = Some(LayoutTransition {
        start_rects: HashMap::from([(10, Rect::new(0, 0, 400, 400))]),
        exit_rects: HashMap::new(),
        exit_provenance: HashMap::new(),
        elapsed_ms: 16,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids: HashSet::new(),
        suppress_landing_focus_resync: true,
        defer_focus_border: false,
    });
    state.active_crossfade = Some(CrossfadeState { epoch: 7 });
    state
        .crossfade_sources
        .insert(7, (HashSet::from([10]), Instant::now()));
    state.ghost_sources_pending_safe_landing.insert(10);
    state.pending_suppress_landing_focus_resync = true;

    state.release_all_windows().unwrap();
    assert!(state.layout_transition.is_none());
    assert!(state.active_crossfade.is_none());
    assert!(!state.pending_suppress_landing_focus_resync);
    assert!(state.ghost_sources_pending_safe_landing.is_empty());
    assert!(state.crossfade_sources.contains_key(&7));
    state.acknowledge_crossfade_complete(7);
    assert!(!state.crossfade_sources.contains_key(&7));
    assert!(state.ghost_sources_pending_safe_landing.is_empty());
    assert!(state.layout_transition.is_none());
    assert_eq!(state.released_window_id_batches.len(), 1);
    assert!(state.paused);

    state.active_crossfade = Some(CrossfadeState { epoch: 8 });
    state
        .crossfade_sources
        .insert(8, (HashSet::from([20]), Instant::now()));
    state.acknowledge_crossfade_complete(7);
    assert_eq!(state.active_crossfade.as_ref().unwrap().epoch, 8);
    assert!(state.crossfade_sources.contains_key(&8));
}

#[test]
fn released_state_admits_created_windows_once_and_refreshes_while_paused() {
    let mut state = managed_state();
    state.release_all_windows().unwrap();
    state.injected_window_info.insert(
        40,
        WindowInfo {
            hwnd: 40,
            title: "Released admission".to_string(),
            class_name: "TestWindowClass".to_string(),
            process_id: 1040,
            rect: Rect::new(100, 100, 800, 600),
            visible: true,
        },
    );

    state.handle_window_event(WindowEvent::Created(40, 0));
    state.handle_window_event(WindowEvent::Created(40, 0));
    assert!(state.paused);
    assert!(state.all_managed_window_ids().contains(&40));
    assert_eq!(
        state
            .all_managed_window_ids()
            .iter()
            .filter(|&&window_id| window_id == 40)
            .count(),
        1
    );
    assert!(matches!(
        state.complete_refresh_layout(StalePruneLayout::Unchanged),
        IpcResponse::Ok
    ));
}
