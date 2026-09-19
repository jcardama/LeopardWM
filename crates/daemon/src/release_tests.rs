use crate::config::Config;
use crate::helpers::StalePruneLayout;
use crate::state::{AppState, DragHintAction, TestApplyPlacementsBehavior};
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{MonitorInfo, WindowEvent, WindowInfo};
use std::sync::atomic::Ordering;
use std::time::Duration;

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
    failing.injected_release_cascade_error = Some("injected cascade failure".to_string());
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

    state.handle_window_event(WindowEvent::Created(40));
    state.handle_window_event(WindowEvent::Created(40));
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
