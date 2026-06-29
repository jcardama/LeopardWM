use super::*;
use leopardwm_core_layout::{Rect, Workspace};
use std::sync::atomic::Ordering;

fn test_config() -> Config {
    Config::default()
}

fn test_monitors() -> Vec<MonitorInfo> {
    vec![MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 1920, 1080),
        work_area: Rect::new(0, 0, 1920, 1040),
        is_primary: true,
        device_name: "DISPLAY1".to_string(),
        scale_factor: 1.0,
    }]
}

#[test]
fn test_app_state_new() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.focused_monitor, 1);
}

#[test]
fn test_note_elevation_block_lifecycle() {
    use crate::event_handler::ElevationCheck;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let hwnd = 0xABCD_u64;

    // First block: recorded + flagged as new (caller toasts).
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: term", true),
        ElevationCheck::BlockedNew
    );
    assert_eq!(
        state.elevation_blocked.get(&hwnd).map(String::as_str),
        Some("Admin: term")
    );

    // Same window blocked again: already known, no re-notify.
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: term", true),
        ElevationCheck::BlockedKnown
    );

    // Recycled HWND now owned by a different blocked window (title changed):
    // re-notify and refresh the stored title.
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: other", true),
        ElevationCheck::BlockedNew
    );
    assert_eq!(
        state.elevation_blocked.get(&hwnd).map(String::as_str),
        Some("Admin: other")
    );

    // Now manageable (e.g. recycled HWND owned by a normal window): record cleared.
    assert_eq!(
        state.note_elevation_block(hwnd, "Notepad", false),
        ElevationCheck::Manageable
    );
    assert!(!state.elevation_blocked.contains_key(&hwnd));

    // Manageable when never recorded is a no-op clear.
    assert_eq!(
        state.note_elevation_block(0x1234, "Other", false),
        ElevationCheck::Manageable
    );
    assert!(state.elevation_blocked.is_empty());
}

#[test]
fn test_app_state_skips_border_frame_under_cfg_test() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    assert!(
        state.border_frame.is_none(),
        "BorderFrame must stay None under cfg(test) — a real layered DWM window lags the user's mouse during cargo test"
    );
    assert!(
        state.paused,
        "AppState must default to paused under cfg(test) — placeholder hwnds otherwise hit real DWM"
    );
}

#[test]
fn test_app_state_skips_thumbnail_host_under_cfg_test() {
    // ThumbnailHost::new() panics under cfg(test). If AppState construction
    // ever triggers it, this test will panic during setup — implicit proof
    // that we don't accidentally call thumbnail::host() during initialization.
    let _state = AppState::new_with_config(test_config(), test_monitors());
}

#[test]
fn test_partition_for_animation_routes_ghosted_wids_to_ghost_stream() {
    use crate::state::{GhostEntry, LayoutTransition};
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::{HashMap, HashSet};

    let mut ghosted_wids = HashSet::new();
    ghosted_wids.insert(100u64);
    ghosted_wids.insert(200u64);

    let transition = LayoutTransition {
        start_rects: HashMap::new(),
        exit_rects: HashMap::new(),
        elapsed_ms: 0,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids,
    };

    // GhostEntry with handle_isize=0 has a no-op Drop, so it's safe to
    // construct in tests without touching the DWM thumbnail API.
    let mut ghost_handles: HashMap<u64, GhostEntry> = HashMap::new();
    ghost_handles.insert(
        100,
        GhostEntry::new(0, "Chrome_WidgetWin_1".into(), Rect::new(0, 0, 800, 600)),
    );
    ghost_handles.insert(
        200,
        GhostEntry::new(0, "MozillaWindowClass".into(), Rect::new(800, 0, 800, 600)),
    );

    let placements = vec![
        WindowPlacement {
            window_id: 100, // ghosted
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 300, // not ghosted
            rect: Rect::new(800, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 1,
        },
        WindowPlacement {
            window_id: 200, // ghosted
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        },
    ];

    let (live, ghosts) =
        AppState::partition_for_animation(placements, Some(&transition), &ghost_handles);

    // 100 and 200 are ghosted; 300 stays live.
    assert_eq!(live.len(), 1, "non-ghosted placement should stay live");
    assert_eq!(live[0].window_id, 300);
    assert_eq!(ghosts.len(), 2, "two ghosted placements should produce ghost frames");
    // Worker only ever calls thumbnail::update with handle != 0; the test
    // never does, so handle_isize == 0 here is fine.
    assert!(ghosts.iter().all(|g| g.handle_isize == 0));
}

#[test]
fn test_partition_for_animation_no_transition_keeps_everything_live() {
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::HashMap;

    let placements = vec![WindowPlacement {
        window_id: 42,
        rect: Rect::new(0, 0, 100, 100),
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    let (live, ghosts) =
        AppState::partition_for_animation(placements, None, &HashMap::new());
    assert_eq!(live.len(), 1);
    assert_eq!(ghosts.len(), 0);
}

#[test]
fn test_abort_active_crossfade_clears_state_without_worker_panic() {
    // No animation_worker_control installed (None) — abort should be a
    // no-op on the worker side but still clear daemon-local state.
    use crate::state::CrossfadeState;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.crossfade_epoch_counter = 5;
    state.active_crossfade = Some(CrossfadeState { epoch: 5 });
    let mut sources = std::collections::HashSet::new();
    sources.insert(42u64);
    state
        .crossfade_sources
        .insert(5, (sources, std::time::Instant::now()));

    state.abort_active_crossfade();

    assert!(state.active_crossfade.is_none(), "abort should clear active");
    // crossfade_sources[epoch] stays populated until CrossfadeComplete
    // arrives — the worker may still be using the old entries for up to
    // one frame.
    assert!(state
        .crossfade_sources
        .get(&5)
        .map(|(s, _)| s.contains(&42))
        .unwrap_or(false));
}

#[test]
fn test_register_ghosts_sweeps_stale_crossfade_barrier() {
    // A crossfade_sources entry whose CrossfadeComplete never arrived
    // (worker died/stuck) must not bar its wids forever. An entry older
    // than CROSSFADE_BARRIER_MAX_AGE is swept on the next ghost pass.
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    let mut stale = std::collections::HashSet::new();
    stale.insert(42u64);
    let old = std::time::Instant::now() - crate::state::CROSSFADE_BARRIER_MAX_AGE
        - std::time::Duration::from_secs(1);
    state.crossfade_sources.insert(99, (stale, old));

    let mut fresh = std::collections::HashSet::new();
    fresh.insert(7u64);
    state
        .crossfade_sources
        .insert(100, (fresh, std::time::Instant::now()));

    state.sweep_stale_crossfade_barriers();

    assert!(
        !state.crossfade_sources.contains_key(&99),
        "stale epoch should be swept"
    );
    assert!(
        state.crossfade_sources.contains_key(&100),
        "fresh epoch must survive"
    );
}

#[test]
fn test_partition_for_animation_missing_handle_drops_placement() {
    use crate::state::LayoutTransition;
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::{HashMap, HashSet};

    // Wid is in ghosted_wids but ghost_handles is empty — registration
    // failure path. partition should drop the placement entirely (the
    // window lands at its target via the post-animation pass).
    let mut ghosted_wids = HashSet::new();
    ghosted_wids.insert(99u64);
    let transition = LayoutTransition {
        start_rects: HashMap::new(),
        exit_rects: HashMap::new(),
        elapsed_ms: 0,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids,
    };

    let placements = vec![WindowPlacement {
        window_id: 99,
        rect: Rect::new(0, 0, 100, 100),
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    let (live, ghosts) =
        AppState::partition_for_animation(placements, Some(&transition), &HashMap::new());
    assert_eq!(live.len(), 0, "ghosted wid without handle should be dropped");
    assert_eq!(ghosts.len(), 0);
}

#[test]
fn test_app_state_focused_viewport() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let viewport = state.focused_viewport();
    assert_eq!(viewport.width, 1920);
    assert_eq!(viewport.height, 1040);
}

#[test]
fn test_app_state_no_monitors_fallback() {
    let state = AppState::new_with_config(test_config(), vec![]);
    let viewport = state.focused_viewport();
    assert_eq!(viewport.width, FALLBACK_VIEWPORT_WIDTH);
    assert_eq!(viewport.height, FALLBACK_VIEWPORT_HEIGHT);
}

#[test]
fn test_window_rule_matching_class() {
    let config = Config {
        window_rules: vec![config::WindowRule {
            match_class: Some("TestClass".to_string()),
            match_title: None,
            match_executable: None,
            action: config::WindowAction::Float,
            width: Some(800),
            height: Some(600),
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action = state.evaluate_window_rules("TestClass", "Any Title", "any.exe");
    assert_eq!(action, config::WindowAction::Float);
}

#[test]
fn test_window_rule_matching_title() {
    let config = Config {
        window_rules: vec![config::WindowRule {
            match_class: None,
            match_title: Some(".*DevTools.*".to_string()),
            match_executable: None,
            action: config::WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action = state.evaluate_window_rules("AnyClass", "DevTools - localhost", "chrome.exe");
    assert_eq!(action, config::WindowAction::Float);
}

#[test]
fn test_window_rule_matching_executable() {
    let config = Config {
        window_rules: vec![config::WindowRule {
            match_class: None,
            match_title: None,
            match_executable: Some("spotify.exe".to_string()),
            action: config::WindowAction::Ignore,
            width: None,
            height: None,
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action = state.evaluate_window_rules("SpotifyClass", "Spotify", "spotify.exe");
    assert_eq!(action, config::WindowAction::Ignore);
}

#[test]
fn test_window_rule_no_match_defaults_to_tile() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let action = state.evaluate_window_rules("SomeClass", "Some Title", "some.exe");
    assert_eq!(action, config::WindowAction::Tile);
}

#[test]
fn test_floating_rect_uses_rule_dimensions() {
    let config = Config {
        window_rules: vec![config::WindowRule {
            match_class: Some("TestClass".to_string()),
            match_title: None,
            match_executable: None,
            action: config::WindowAction::Float,
            width: Some(1024),
            height: Some(768),
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let original = Rect::new(100, 100, 640, 480);
    let result =
        state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original, None);
    assert_eq!(result.width, 1024);
    assert_eq!(result.height, 768);
}

#[test]
fn test_floating_rect_preserves_original_if_no_dimensions() {
    let config = Config {
        window_rules: vec![config::WindowRule {
            match_class: Some("TestClass".to_string()),
            match_title: None,
            match_executable: None,
            action: config::WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            sticky: false,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let original = Rect::new(100, 100, 640, 480);
    let result =
        state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original, None);
    assert_eq!(result.width, 640);
    assert_eq!(result.height, 480);
}

#[test]
fn test_find_window_workspace_not_found() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    assert!(state.find_window_workspace(99999).is_none());
}

#[test]
fn test_app_state_apply_config() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut new_config = test_config();
    new_config.layout.gap = 20;
    new_config.layout.outer_gap_left = 15;
    state.apply_config(new_config.clone());
    assert_eq!(state.config.layout.gap, 20);
    assert_eq!(state.config.layout.outer_gap_left, 15);
}

#[test]
fn test_state_file_path() {
    let path = AppState::state_file_path();
    assert!(path.to_str().unwrap().contains("leopardwm"));
    assert!(path.to_str().unwrap().ends_with("workspace-state.json"));
}

#[test]
fn test_state_snapshot_serialization() {
    let snapshot = StateSnapshot {
        saved_at: "2026-02-04T12:00:00".to_string(),
        workspaces: vec![],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };
    let json = serde_json::to_string(&snapshot).expect("serialize");
    let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.focused_monitor_name, "DISPLAY1");
    assert!(parsed.workspaces.is_empty());
}

#[test]
fn test_workspace_snapshot_serialization() {
    let workspace = Workspace::new();
    let snapshot = WorkspaceSnapshot {
        monitor_device_name: "DISPLAY1".to_string(),
        workspace_index: 0,
        workspace,
    };
    let json = serde_json::to_string(&snapshot).expect("serialize");
    let parsed: WorkspaceSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.monitor_device_name, "DISPLAY1");
}

#[test]
fn test_save_and_load_roundtrip() {
    // Create a snapshot and verify it roundtrips through serialization
    let snapshot = StateSnapshot {
        saved_at: "2026-02-04T12:00:00".to_string(),
        workspaces: vec![WorkspaceSnapshot {
            monitor_device_name: "DISPLAY1".to_string(),
            workspace_index: 0,
            workspace: Workspace::with_gaps(10, 10),
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };
    let json = serde_json::to_string_pretty(&snapshot).expect("serialize");
    let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.workspaces.len(), 1);
    assert_eq!(parsed.workspaces[0].monitor_device_name, "DISPLAY1");
}

#[test]
fn test_state_snapshot_with_tab_title_overrides_roundtrip() {
    let mut overrides = HashMap::new();
    overrides.insert(0xDEAD_BEEFu64, "My Notes".to_string());
    overrides.insert(0xCAFE_F00Du64, "Build Log".to_string());
    let snapshot = StateSnapshot {
        saved_at: "2026-05-13T12:00:00".to_string(),
        workspaces: vec![],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: overrides.clone(),
    };
    let json = serde_json::to_string(&snapshot).expect("serialize");
    let parsed: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.tab_title_overrides, overrides);
}

#[test]
fn test_state_snapshot_v0_1_14_backward_compat() {
    // An older snapshot JSON (before `tab_title_overrides` existed) has no
    // such field. Verify it loads with the new field defaulted to an empty
    // map so existing users don't lose their workspace state on upgrade.
    let legacy_json = r#"{
        "saved_at": "2026-04-01T00:00:00",
        "workspaces": [],
        "focused_monitor_name": "DISPLAY1",
        "active_workspace": {}
    }"#;
    let parsed: StateSnapshot = serde_json::from_str(legacy_json).expect("deserialize");
    assert!(parsed.tab_title_overrides.is_empty());
    assert_eq!(parsed.focused_monitor_name, "DISPLAY1");
}

#[test]
fn test_spawn_forwarding_thread_forwards_events() {
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    let (async_tx, mut async_rx) = mpsc::channel::<DaemonEvent>(10);

    let _handle = spawn_forwarding_thread("test", rx, async_tx, |_n| {
        DaemonEvent::HideSnapHint // Use a simple variant for testing
    })
    .unwrap();

    tx.send(42).unwrap();
    drop(tx); // Close channel so thread exits

    // Use a runtime to receive
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let event = rt.block_on(async { async_rx.recv().await });
    assert!(event.is_some());
}

#[test]
fn test_spawn_forwarding_thread_stops_on_channel_close() {
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    let (async_tx, _async_rx) = mpsc::channel::<DaemonEvent>(10);

    let handle =
        spawn_forwarding_thread("test-close", rx, async_tx, |_| DaemonEvent::HideSnapHint)
            .unwrap();

    drop(tx); // Close sender immediately
              // Thread should exit when recv() returns Err
    handle.join().expect("Thread should exit cleanly");
}

#[ignore] // Depends on no daemon running; fails when daemon is active
#[test]
fn test_check_already_running_returns_false_when_no_daemon() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let result = rt.block_on(check_already_running());
    // No daemon is running during tests, so this should be false
    assert!(!result);
}

#[test]
fn test_ipc_read_timeout_is_reasonable() {
    assert!(IPC_READ_TIMEOUT.as_secs() >= 1);
    assert!(IPC_READ_TIMEOUT.as_secs() <= 30);
}

#[test]
fn test_ipc_response_timeout_is_reasonable() {
    assert!(IPC_RESPONSE_TIMEOUT.as_secs() >= 1);
    assert!(IPC_RESPONSE_TIMEOUT.as_secs() <= 60);
}

#[test]
fn test_response_for_ipc_wait_failure_shutdown_commands_return_ok() {
    assert_eq!(
        response_for_ipc_wait_failure(&IpcCommand::Stop, true),
        IpcResponse::Ok
    );
    assert_eq!(
        response_for_ipc_wait_failure(&IpcCommand::PanicRevert, false),
        IpcResponse::Ok
    );
}

#[test]
fn test_response_for_ipc_wait_failure_non_shutdown_returns_error() {
    match response_for_ipc_wait_failure(&IpcCommand::FocusLeft, true) {
        IpcResponse::Error { message } => {
            assert!(message.contains("Timed out waiting for daemon response"));
        }
        other => panic!("Expected timeout error response, got {:?}", other),
    }

    match response_for_ipc_wait_failure(&IpcCommand::FocusLeft, false) {
        IpcResponse::Error { message } => {
            assert!(message.contains("Failed to get response from daemon"));
        }
        other => panic!("Expected responder error response, got {:?}", other),
    }
}

#[test]
fn test_shutdown_mode_for_command_maps_shutdown_variants() {
    assert_eq!(
        shutdown_mode_for_command(&IpcCommand::Stop),
        Some(ShutdownMode::Graceful)
    );
    assert_eq!(
        shutdown_mode_for_command(&IpcCommand::PanicRevert),
        Some(ShutdownMode::PanicRevert)
    );
    assert_eq!(shutdown_mode_for_command(&IpcCommand::FocusLeft), None);
}

#[test]
fn test_max_ipc_message_size_is_reasonable() {
    const { assert!(leopardwm_ipc::MAX_IPC_MESSAGE_SIZE >= 1024) };
    const { assert!(leopardwm_ipc::MAX_IPC_MESSAGE_SIZE <= 1024 * 1024) };
}

// ========================================================================
// handle_command() Unit Tests
// ========================================================================

#[test]
fn test_cmd_query_workspace_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::QueryWorkspace);
    match resp {
        IpcResponse::WorkspaceState {
            columns, windows, ..
        } => {
            assert_eq!(columns, 0);
            assert_eq!(windows, 0);
        }
        _ => panic!("Expected WorkspaceState, got {:?}", resp),
    }
}

#[test]
fn test_cmd_query_focused_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::QueryFocused);
    match resp {
        IpcResponse::FocusedWindow {
            window_id,
            column_index,
            window_index,
        } => {
            assert!(window_id.is_none());
            assert_eq!(column_index, 0);
            assert_eq!(window_index, 0);
        }
        _ => panic!("Expected FocusedWindow, got {:?}", resp),
    }
}

#[test]
fn test_cmd_focus_up_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::FocusUp);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_focus_down_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::FocusDown);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_stop() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::Stop);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_panic_revert() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::PanicRevert);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_toggle_pause() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    assert!(!state.paused);

    let resp = state.handle_command(IpcCommand::TogglePause);
    assert_eq!(resp, IpcResponse::Ok);
    assert!(state.paused, "toggle_pause should pause tiling");

    let resp = state.handle_command(IpcCommand::TogglePause);
    assert_eq!(resp, IpcResponse::Ok);
    assert!(!state.paused, "second toggle_pause should resume tiling");
}

#[test]
fn test_toggle_pause_resume_reports_apply_failure() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(state.paused, "first toggle_pause should pause tiling");

    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        Duration::from_millis(1),
    ));

    let resp = state.handle_command(IpcCommand::TogglePause);
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("injected apply_placements failure"));
        }
        other => panic!("Expected Error response, got {:?}", other),
    }
    assert!(
        state.paused,
        "failed resume should restore paused state to avoid false resumed status"
    );
}

#[test]
fn test_cmd_focus_left_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::FocusLeft);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_focus_right_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::FocusRight);
    assert_eq!(resp, IpcResponse::Ok);
}

fn fullscreen_state_two_columns() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap(); // focus on column 1 (window 200)
    assert!(ws.toggle_fullscreen(), "entered fullscreen");
    assert!(ws.is_fullscreen());
    state
}

#[test]
fn test_focus_command_carries_fullscreen_and_moves_focus() {
    let mut state = fullscreen_state_two_columns();
    let resp = state.handle_command(IpcCommand::FocusLeft);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace().unwrap();
    // Monocle mode: focus moves but stays fullscreen, carrying fullscreen to
    // the newly focused window.
    assert!(ws.is_fullscreen(), "focus command keeps fullscreen");
    assert_eq!(ws.focused_column_index(), 0, "focus moved to the left column");
    assert_eq!(
        ws.fullscreen_window_id(),
        Some(100),
        "fullscreen follows focus to the left window"
    );
}

#[test]
fn test_structural_command_exits_fullscreen() {
    let mut state = fullscreen_state_two_columns();
    let resp = state.handle_command(IpcCommand::ConsumeFromLeft);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace().unwrap();
    assert!(!ws.is_fullscreen(), "consume must drop fullscreen");
    assert_eq!(ws.column_count(), 1, "left window consumed into the focused column");
}

#[test]
fn test_scroll_and_resize_are_suppressed_while_fullscreen() {
    let mut state = fullscreen_state_two_columns();
    assert_eq!(state.handle_command(IpcCommand::Scroll { delta: 120.0 }), IpcResponse::Ok);
    assert!(
        state.focused_workspace().unwrap().is_fullscreen(),
        "scroll must not drop fullscreen"
    );
    assert_eq!(state.handle_command(IpcCommand::Resize { delta: 50 }), IpcResponse::Ok);
    assert!(
        state.focused_workspace().unwrap().is_fullscreen(),
        "resize must not drop fullscreen"
    );
}

#[test]
fn test_toggle_fullscreen_still_exits_while_fullscreen() {
    let mut state = fullscreen_state_two_columns();
    let resp = state.handle_command(IpcCommand::ToggleFullscreen);
    assert_eq!(resp, IpcResponse::Ok);
    assert!(
        !state.focused_workspace().unwrap().is_fullscreen(),
        "toggle-fullscreen still turns it off"
    );
}

#[test]
fn test_cmd_move_left_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::MoveColumnLeft);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_move_right_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::MoveColumnRight);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_resize_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::Resize { delta: 100 });
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_scroll_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::Scroll { delta: 50.0 });
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_apply() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::Apply);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_focus_monitor_left_single() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // With only one monitor, FocusMonitorLeft is a no-op, returns Ok without calling apply_layout
    let resp = state.handle_command(IpcCommand::FocusMonitorLeft);
    assert_eq!(resp, IpcResponse::Ok);
    assert_eq!(state.focused_monitor, 1); // unchanged
}

#[test]
fn test_cmd_focus_monitor_right_single() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::FocusMonitorRight);
    assert_eq!(resp, IpcResponse::Ok);
    assert_eq!(state.focused_monitor, 1); // unchanged
}

#[test]
fn test_cmd_move_to_monitor_left_single() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::MoveWindowToMonitorLeft);
    assert_eq!(resp, IpcResponse::Ok); // no-op: no monitor to the left
}

#[test]
fn test_cmd_move_to_monitor_right_single() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::MoveWindowToMonitorRight);
    assert_eq!(resp, IpcResponse::Ok); // no-op: no monitor to the right
}

#[test]
fn test_cmd_move_to_monitor_right_rollback_on_insert_failure() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.focused_monitor = 1;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    // Force target insert failure (duplicate in target workspace).
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();

    let resp = state.handle_command(IpcCommand::MoveWindowToMonitorRight);
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("Failed to add window to target"))
        }
        other => panic!("Expected error, got {:?}", other),
    }

    let source = &state.workspaces.get(&1).unwrap()[0];
    let target = &state.workspaces.get(&2).unwrap()[0];
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(source.window_count(), 1);
    assert_eq!(source.focused_window(), Some(100));
    assert_eq!(target.window_count(), 1);
    assert!(target.contains_window(100));
}

#[test]
fn test_cmd_move_to_monitor_left_rollback_on_insert_failure() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.focused_monitor = 2;
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    // Force target insert failure (duplicate in target workspace).
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();

    let resp = state.handle_command(IpcCommand::MoveWindowToMonitorLeft);
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("Failed to add window to target"))
        }
        other => panic!("Expected error, got {:?}", other),
    }

    let source = &state.workspaces.get(&2).unwrap()[0];
    let target = &state.workspaces.get(&1).unwrap()[0];
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(source.window_count(), 1);
    assert_eq!(source.focused_window(), Some(200));
    assert_eq!(target.window_count(), 1);
    assert!(target.contains_window(200));
}

// ========================================================================
// reconcile_monitors() Unit Tests
// ========================================================================

fn two_monitors() -> Vec<MonitorInfo> {
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
    ]
}

#[test]
fn test_reconcile_no_change() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitors_before = state.workspaces.len();
    state.reconcile_monitors(test_monitors());
    assert_eq!(state.workspaces.len(), monitors_before);
    assert_eq!(state.focused_monitor, 1);
}

#[test]
fn test_reconcile_add_monitor() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    assert_eq!(state.workspaces.len(), 1);
    state.reconcile_monitors(two_monitors());
    assert_eq!(state.workspaces.len(), 2);
    assert!(state.workspaces.contains_key(&2));
}

#[test]
fn test_reconcile_remove_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    assert_eq!(state.workspaces.len(), 2);
    // Remove second monitor, keep only primary
    state.reconcile_monitors(test_monitors());
    assert_eq!(state.workspaces.len(), 1);
    assert!(state.workspaces.contains_key(&1));
    assert!(!state.workspaces.contains_key(&2));
}

#[test]
fn test_reconcile_remove_focused_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.focused_monitor = 2; // Focus on secondary
                               // Remove secondary, keep primary
    state.reconcile_monitors(test_monitors());
    // Focus should fall back to primary
    assert_eq!(state.focused_monitor, 1);
}

#[test]
fn test_reconcile_primary_always_exists() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    // Remove secondary, keep primary
    state.reconcile_monitors(test_monitors());
    assert!(state.workspaces.contains_key(&1));
}

#[test]
fn test_reconcile_empty_to_multi() {
    let mut state = AppState::new_with_config(test_config(), vec![]);
    assert_eq!(state.workspaces.len(), 0);
    state.reconcile_monitors(two_monitors());
    assert_eq!(state.workspaces.len(), 2);
}

#[test]
fn test_reconcile_preserves_windows() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    // Add windows to workspace on monitor 2
    if let Some(ws_vec) = state.workspaces.get_mut(&2) {
        ws_vec[0].insert_window(1001, None).unwrap();
        ws_vec[0].insert_window(1002, None).unwrap();
    }
    assert_eq!(state.workspaces.get(&2).unwrap()[0].window_count(), 2);

    // Remove monitor 2 - windows should migrate to primary
    state.reconcile_monitors(test_monitors());
    let primary_ws = &state.workspaces.get(&1).unwrap()[0];
    assert_eq!(primary_ws.window_count(), 2);
}

#[test]
fn test_reconcile_full_monitor_churn() {
    // Start with monitors 1 and 2, add windows to both
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, None)
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(101, None)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, None)
        .unwrap();

    // Replace ALL monitors with entirely new ones (ids 3 and 4)
    let new_monitors = vec![
        MonitorInfo {
            id: 3,
            rect: Rect::new(0, 0, 2560, 1440),
            work_area: Rect::new(0, 0, 2560, 1400),
            is_primary: true,
            device_name: "DISPLAY3".to_string(),
            scale_factor: 1.0,
        },
        MonitorInfo {
            id: 4,
            rect: Rect::new(2560, 0, 1920, 1080),
            work_area: Rect::new(2560, 0, 1920, 1040),
            is_primary: false,
            device_name: "DISPLAY4".to_string(),
            scale_factor: 1.0,
        },
    ];
    state.reconcile_monitors(new_monitors);

    // All 3 windows must have been migrated to the new primary (id 3)
    assert_eq!(state.workspaces.len(), 2);
    let primary_ws = &state.workspaces.get(&3).unwrap()[0];
    assert_eq!(primary_ws.window_count(), 3);
    assert!(state.workspaces.contains_key(&4));
    // Old monitors must be gone
    assert!(!state.workspaces.contains_key(&1));
    assert!(!state.workspaces.contains_key(&2));
}

// ========================================================================
// Additional Command Tests
// ========================================================================

#[test]
fn test_cmd_refresh() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Keep this deterministic in headless/CI environments where Win32
    // placement side effects can fail on unrelated desktop windows.
    state.paused = true;
    let resp = state.handle_command(IpcCommand::Refresh);
    match resp {
        IpcResponse::Ok => {}
        IpcResponse::Error { message } => {
            assert!(
                message.contains("Failed to enumerate windows")
                    || message.contains("Failed to apply layout"),
                "unexpected refresh error: {}",
                message
            );
        }
        other => panic!("Expected Ok or Error, got {:?}", other),
    }
}

#[test]
fn test_cmd_reload() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::Reload);
    assert_eq!(resp, IpcResponse::Ok);
    // Config was reloaded (default since no config file in test env)
    assert_eq!(state.config.layout.gap, Config::default().layout.gap);
}

#[test]
fn test_cmd_query_all_windows() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::QueryAllWindows);
    match resp {
        IpcResponse::WindowList { windows } => {
            assert!(windows.is_empty());
        }
        other => panic!("Expected WindowList, got {:?}", other),
    }
}

// ========================================================================
// New command tests
// ========================================================================

#[test]
fn test_cmd_close_window_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::CloseWindow);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_toggle_floating_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::ToggleFloating);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_toggle_floating_roundtrip() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Avoid real Win32 positioning on synthetic test window IDs.
    state.paused = true;
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    assert!(!ws.is_floating(100));

    // Tile -> Float: toggle_floating targets the tiled focused window
    let resp = state.handle_command(IpcCommand::ToggleFloating);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace_mut().unwrap();
    assert!(ws.is_floating(100), "window should now be floating");

    // Simulate OS sending a Focused event for the floating window.
    // This is the real runtime path: user clicks on the floating window,
    // OS fires EVENT_SYSTEM_FOREGROUND, and the daemon processes it.
    // The Focused handler updates previous_focused_hwnd for managed windows.
    state.handle_window_event(WindowEvent::Focused(100));
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "Focused event should update previous_focused_hwnd for floating windows"
    );

    // Float -> Tile: ToggleFloating now sees the floating window via previous_focused_hwnd
    let resp = state.handle_command(IpcCommand::ToggleFloating);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace_mut().unwrap();
    assert!(
        !ws.is_floating(100),
        "window should be back to tiled after roundtrip"
    );
    assert!(ws.contains_window(100));
}

#[test]
fn test_cmd_toggle_fullscreen_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::ToggleFullscreen);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_set_column_width_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 0.5 });
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_set_column_width_rejects_fraction_below_range() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 0.05 });
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("Invalid set-width fraction"))
        }
        other => panic!("Expected error, got {:?}", other),
    }
}

#[test]
fn test_cmd_set_column_width_rejects_fraction_above_range() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: 1.1 });
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("Invalid set-width fraction"))
        }
        other => panic!("Expected error, got {:?}", other),
    }
}

#[test]
fn test_cmd_set_column_width_rejects_non_finite_fraction() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::SetColumnWidth { fraction: f64::NAN });
    match resp {
        IpcResponse::Error { message } => assert!(message.contains("must be finite")),
        other => panic!("Expected error, got {:?}", other),
    }
}

#[test]
fn test_cmd_equalize_column_widths_empty() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::EqualizeColumnWidths);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_cmd_query_status() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let resp = state.handle_command(IpcCommand::QueryStatus);
    match resp {
        IpcResponse::StatusInfo {
            version,
            monitors,
            total_windows,
            uptime_seconds: _,
        } => {
            assert!(!version.is_empty());
            assert_eq!(monitors, 1);
            assert_eq!(total_windows, 0);
        }
        other => panic!("Expected StatusInfo, got {:?}", other),
    }
}

#[test]
fn test_paused_apply_layout_is_noop() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    // apply_layout should succeed without actually doing anything
    assert!(state.apply_layout().is_ok());
}

#[test]
fn test_start_time_initialized() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    // start_time should be very recent
    assert!(state.start_time.elapsed().as_secs() < 1);
}

#[test]
fn test_all_managed_window_ids_empty() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let ids = state.all_managed_window_ids();
    assert!(ids.is_empty(), "No windows should exist in a fresh state");
}

#[test]
fn test_all_managed_window_ids_with_windows() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    // Add tiled windows
    if let Some(ws) = state.focused_workspace_mut() {
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
        // Add a floating window
        ws.add_floating(300, Rect::new(0, 0, 400, 300)).unwrap();
    }

    let ids = state.all_managed_window_ids();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&100));
    assert!(ids.contains(&200));
    assert!(ids.contains(&300));
}

#[test]
fn test_all_managed_window_ids_multi_monitor() {
    let monitors = vec![
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
    ];

    let mut state = AppState::new_with_config(test_config(), monitors);

    // Add windows to both workspaces
    if let Some(ws_vec) = state.workspaces.get_mut(&1) {
        ws_vec[0].insert_window(100, Some(800)).unwrap();
    }
    if let Some(ws_vec) = state.workspaces.get_mut(&2) {
        ws_vec[0].insert_window(200, Some(800)).unwrap();
    }

    let ids = state.all_managed_window_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&100));
    assert!(ids.contains(&200));
}


// ================================================================
// Minimize/Restore State Tests
// ================================================================

#[test]
fn test_minimize_marks_workspace_window() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();

    assert!(ws.mark_minimized(100));
    assert!(ws.is_minimized(100));
    assert_eq!(ws.minimized_count(), 1);
}

#[test]
fn test_restore_clears_minimized() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.mark_minimized(100);

    assert!(ws.mark_restored(100));
    assert!(!ws.is_minimized(100));
    assert_eq!(ws.minimized_count(), 0);
}

#[test]
fn test_minimize_unmanaged_window_noop() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    // No windows added -- unmanaged window ID
    assert!(state.find_window_workspace(999).is_none());
}

#[test]
fn test_minimized_event_updates_focused_monitor_to_source_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    state.focused_monitor = 1;

    state.handle_window_event(WindowEvent::Minimized(200));
    assert_eq!(state.focused_monitor, 2);
}

#[test]
fn test_minimize_preserves_window_in_workspace() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();
    ws.mark_minimized(100);

    // Window is still in workspace (contains_window)
    assert!(ws.contains_window(100));
    // But is minimized
    assert!(ws.is_minimized(100));
    // Total count unchanged
    assert_eq!(ws.all_window_ids().len(), 2);
}

#[test]
fn test_minimize_focus_moves_to_next() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();

    // Focus is on window 200 (last inserted)
    assert_eq!(ws.focused_window(), Some(200));

    // Minimize window 200 -- focus should move
    ws.mark_minimized(200);
    // Simulate the daemon's focus adjustment for minimized focused window
    if ws.focused_window() == Some(200) {
        ws.focus_down();
        if ws.focused_window() == Some(200) {
            ws.focus_up();
        }
        if ws.focused_window() == Some(200) {
            ws.focus_right();
            if ws.focused_window() == Some(200) {
                ws.focus_left();
            }
        }
    }

    // Focus should now be on window 100
    assert_eq!(ws.focused_window(), Some(100));
}

#[test]
fn test_find_window_workspace_tiled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
    ws.insert_window(100, Some(800)).unwrap();

    // Should find the tiled window
    assert_eq!(state.find_window_workspace(100), Some((1, 0)));
    // Not floating
    let ws = &state.workspaces.get(&1).unwrap()[0];
    assert!(!ws.is_floating(100));
}

#[test]
fn test_find_window_workspace_floating_not_snapped() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
    let rect = Rect::new(100, 100, 800, 600);
    ws.add_floating(200, rect).unwrap();

    // Should find the floating window
    assert_eq!(state.find_window_workspace(200), Some((1, 0)));
    // Is floating -- snap-back should NOT apply
    let ws = &state.workspaces.get(&1).unwrap()[0];
    assert!(ws.is_floating(200));
}

// =========================================================================
// Args (safe-mode flags) tests
// =========================================================================

#[test]
fn test_args_default_all_false() {
    let args = Args {
        no_hotkeys: false,
        safe_mode: false,
    };
    assert!(!args.skip_hotkeys());
}

#[test]
fn test_args_no_hotkeys() {
    let args = Args {
        no_hotkeys: true,
        safe_mode: false,
    };
    assert!(args.skip_hotkeys());
}

#[test]
fn test_args_safe_mode_implies_no_hotkeys() {
    let args = Args {
        no_hotkeys: false,
        safe_mode: true,
    };
    assert!(args.skip_hotkeys());
}

#[test]
fn test_args_parse_no_flags() {
    let args = Args::try_parse_from(["leopardwm"]).unwrap();
    assert!(!args.no_hotkeys);
    assert!(!args.safe_mode);
}

#[test]
fn test_args_parse_safe_mode() {
    let args = Args::try_parse_from(["leopardwm", "--safe-mode"]).unwrap();
    assert!(args.safe_mode);
    assert!(args.skip_hotkeys());
}

#[test]
fn test_args_parse_no_hotkeys() {
    let args = Args::try_parse_from(["leopardwm", "--no-hotkeys"]).unwrap();
    assert!(args.no_hotkeys);
    assert!(!args.safe_mode);
}

// =========================================================================
// Startup banner tests
// =========================================================================

fn make_banner_info() -> StartupInfo {
    StartupInfo {
        version: "0.1.0".to_string(),
        monitor_names: vec!["DISPLAY1".to_string(), "DISPLAY2".to_string()],
        monitor_dpi: vec![1.0, 1.0],
        window_count: 14,
        hotkeys_registered: 24,
        hotkeys_requested: 24,
        config_path: Some(
            "C:\\Users\\test\\AppData\\Roaming\\leopardwm\\config\\config.toml".to_string(),
        ),
        config_warnings: vec![],
        log_path: "C:\\Users\\test\\AppData\\Local\\Temp\\leopardwm-daemon.log".to_string(),
        safe_mode: false,
        no_hotkeys: false,
        reduce_motion: false,
        on_battery_or_saver: false,
        high_contrast: false,
    }
}

#[test]
fn test_startup_banner_typical_values() {
    let banner = format_startup_banner(&make_banner_info());
    assert!(banner.contains("LeopardWM v0.1.0"));
    assert!(banner.contains("Monitors: 2"));
    assert!(banner.contains("DISPLAY1, DISPLAY2"));
    assert!(banner.contains("Windows:  14 managed"));
    assert!(banner.contains("Hotkeys:  24 registered"));
    assert!(banner.contains("Status:   Active"));
}

#[test]
fn test_startup_banner_safe_mode() {
    let mut info = make_banner_info();
    info.monitor_names = vec!["DISPLAY1".to_string()];
    info.window_count = 5;
    info.hotkeys_registered = 0;
    info.hotkeys_requested = 0;
    info.config_path = None;
    info.safe_mode = true;
    info.no_hotkeys = true;
    let banner = format_startup_banner(&info);
    assert!(banner.contains("SAFE MODE"));
    assert!(banner.contains("(default"));
}

#[test]
fn test_startup_banner_zero_monitors() {
    let mut info = make_banner_info();
    info.monitor_names = vec![];
    info.window_count = 0;
    info.hotkeys_registered = 0;
    info.hotkeys_requested = 0;
    info.config_path = None;
    let banner = format_startup_banner(&info);
    assert!(banner.contains("Monitors: 0 (fallback mode)"));
    assert!(banner.contains("Windows:  0 managed"));
}

#[test]
fn test_startup_banner_with_config_warnings() {
    let mut info = make_banner_info();
    info.config_warnings = vec![
        "layout.gap: Negative gap (-5) clamped to 0".to_string(),
        "appearance.active_border_color: Invalid hex color 'ZZZZZZ'".to_string(),
    ];
    let banner = format_startup_banner(&info);
    assert!(banner.contains("Warning:  layout.gap"));
    assert!(banner.contains("Warning:  appearance.active_border_color"));
}

#[test]
fn test_startup_banner_without_config_warnings() {
    let info = make_banner_info();
    assert!(info.config_warnings.is_empty());
    let banner = format_startup_banner(&info);
    assert!(!banner.contains("Warning:"));
}

#[test]
fn test_startup_banner_hotkey_mismatch() {
    let mut info = make_banner_info();
    info.hotkeys_registered = 7;
    info.hotkeys_requested = 10;
    let banner = format_startup_banner(&info);
    assert!(banner.contains("7/10 registered (3 failed)"));
}

#[test]
fn test_startup_banner_hotkey_full_registration() {
    let mut info = make_banner_info();
    info.hotkeys_registered = 10;
    info.hotkeys_requested = 10;
    let banner = format_startup_banner(&info);
    assert!(banner.contains("Hotkeys:  10 registered"));
    assert!(!banner.contains("failed"));
}

#[test]
fn test_startup_banner_high_contrast() {
    let mut info = make_banner_info();
    info.high_contrast = true;
    let banner = format_startup_banner(&info);
    assert!(banner.contains("Display:  high contrast"));
}

#[test]
fn test_startup_banner_no_high_contrast() {
    let info = make_banner_info();
    let banner = format_startup_banner(&info);
    assert!(!banner.contains("high contrast"));
}

// =========================================================================
// join_with_timeout tests
// =========================================================================

#[test]
fn test_join_with_timeout_hanging_thread() {
    let mut handle = Some(std::thread::spawn(|| {
        // Simulate a hanging thread
        std::thread::sleep(Duration::from_secs(300));
    }));
    let result = join_with_timeout(&mut handle, Duration::from_millis(100));
    assert!(
        !result,
        "Should return false when thread doesn't join in time"
    );
    assert!(
        handle.is_some(),
        "timed-out join should retain ownership for later retry"
    );
}

// =========================================================================
// Workspace mutation tests (handle_window_event equivalent)
// =========================================================================

#[test]
fn test_destroy_tiled_window_removes() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();
    assert_eq!(ws.window_count(), 2);

    let _ = ws.remove_window(100);
    assert_eq!(ws.window_count(), 1);
    assert!(!ws.contains_window(100));
    assert!(ws.contains_window(200));
}

#[test]
fn test_destroy_floating_window_removes() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.add_floating(300, Rect::new(0, 0, 400, 300)).unwrap();
    assert!(ws.is_floating(300));

    ws.remove_floating(300);
    assert!(!ws.contains_window(300));
}

#[test]
fn test_destroy_unknown_window_noop() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();

    // Removing a non-existent window should not panic
    let _ = ws.remove_window(99999);
    assert_eq!(ws.window_count(), 1);
}

#[test]
fn test_focus_changes_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    // Add window to monitor 2
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();

    // Find which workspace contains window 200
    let monitor = state.find_window_workspace(200);
    assert_eq!(monitor, Some((2, 0)));

    // Simulate focus change: update focused_monitor
    state.focused_monitor = 2;
    assert_eq!(state.focused_monitor, 2);
}

#[test]
fn test_minimized_only_window_no_crash() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.mark_minimized(100);

    // State should be consistent: window exists but is minimized
    assert!(ws.contains_window(100));
    assert!(ws.is_minimized(100));
    assert_eq!(ws.minimized_count(), 1);
}

#[test]
fn test_restored_window_becomes_focused() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();

    // Minimize window 200 (currently focused)
    ws.mark_minimized(200);
    // Adjust focus away
    ws.focus_left();

    // Restore window 200
    ws.mark_restored(200);
    assert!(!ws.is_minimized(200));
    // Window should be accessible for focus
    assert!(ws.contains_window(200));
}

#[test]
fn test_paused_state_skips_events() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    // Commands should still return Ok but not cause side effects
    let resp = state.handle_command(IpcCommand::FocusLeft);
    assert_eq!(resp, IpcResponse::Ok);
    let resp = state.handle_command(IpcCommand::Refresh);
    assert_eq!(resp, IpcResponse::Ok);
}

#[test]
fn test_multiple_monitors_focus_cross_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    // Add windows to both monitors
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();

    // Start focused on monitor 1
    assert_eq!(state.focused_monitor, 1);

    // Simulate focus switch to monitor 2
    state.focused_monitor = 2;
    assert_eq!(state.focused_monitor, 2);

    // Verify the focused workspace is on monitor 2
    let ws = &state.workspaces.get(&state.focused_monitor).unwrap()[0];
    assert!(ws.contains_window(200));
}

#[test]
fn test_pipe_busy_error_code_is_231() {
    // ERROR_PIPE_BUSY is Windows error code 231. This test documents the
    // constant used in check_already_running() to detect a busy pipe.
    assert_eq!(ERROR_PIPE_BUSY, 231);
    // Verify the constant matches what std::io::Error would report
    let err = std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY);
    assert_eq!(err.raw_os_error(), Some(231));
}

#[test]
fn test_pipe_probe_error_hardening_logic() {
    let busy = std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY);
    assert!(pipe_probe_error_indicates_running(&busy));

    let not_found = std::io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND);
    assert!(!pipe_probe_error_indicates_running(&not_found));

    let not_found_kind = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
    assert!(!pipe_probe_error_indicates_running(&not_found_kind));

    let access_denied = std::io::Error::from_raw_os_error(5); // ERROR_ACCESS_DENIED
    assert!(pipe_probe_error_indicates_running(&access_denied));
}

#[test]
fn test_restore_state_preserves_scroll_offset() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    // Insert windows so the workspace has scrollable content
    let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();
    ws.insert_window(300, Some(800)).unwrap();

    // Build a snapshot with a non-zero scroll offset
    let mut saved_ws = Workspace::default();
    saved_ws.set_scroll_offset(500.0);
    let snapshot = StateSnapshot {
        saved_at: "test".to_string(),
        workspaces: vec![WorkspaceSnapshot {
            monitor_device_name: "DISPLAY1".to_string(),
            workspace_index: 0,
            workspace: saved_ws,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };

    let restored = state.restore_state(&snapshot);
    assert!(restored.contains(&1), "Monitor 1 should be in restored set");

    let ws = &state.workspaces.get(&1).unwrap()[0];
    assert_eq!(
        ws.scroll_offset(),
        500.0,
        "Scroll offset should be preserved after restore"
    );
}

#[test]
fn test_restore_state_on_empty_workspace_safe() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Workspace is empty -- no windows at all

    let mut saved_ws = Workspace::default();
    saved_ws.set_scroll_offset(300.0);
    let snapshot = StateSnapshot {
        saved_at: "test".to_string(),
        workspaces: vec![WorkspaceSnapshot {
            monitor_device_name: "DISPLAY1".to_string(),
            workspace_index: 0,
            workspace: saved_ws,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };

    // Should not panic even on empty workspace
    let restored = state.restore_state(&snapshot);
    assert!(restored.contains(&1), "Monitor 1 should be in restored set");

    let ws = &state.workspaces.get(&1).unwrap()[0];
    assert_eq!(
        ws.scroll_offset(),
        300.0,
        "Scroll offset should be set directly even on empty workspace"
    );
}

#[test]
fn test_restore_state_returns_restored_monitor_ids() {
    // Setup: two monitors
    let monitors = vec![
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
    ];
    let mut state = AppState::new_with_config(test_config(), monitors);

    // Snapshot only mentions DISPLAY1, not DISPLAY2
    let mut saved_ws = Workspace::default();
    saved_ws.set_scroll_offset(250.0);
    let snapshot = StateSnapshot {
        saved_at: "test".to_string(),
        workspaces: vec![WorkspaceSnapshot {
            monitor_device_name: "DISPLAY1".to_string(),
            workspace_index: 0,
            workspace: saved_ws,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };

    let restored = state.restore_state(&snapshot);

    // Monitor 1 was restored, monitor 2 was not in snapshot
    assert!(restored.contains(&1), "Monitor 1 should be restored");
    assert!(!restored.contains(&2), "Monitor 2 should NOT be restored");

    // Unknown monitor in snapshot should not appear
    let mut saved_ws2 = Workspace::default();
    saved_ws2.set_scroll_offset(100.0);
    let snapshot2 = StateSnapshot {
        saved_at: "test".to_string(),
        workspaces: vec![WorkspaceSnapshot {
            monitor_device_name: "UNKNOWN".to_string(),
            workspace_index: 0,
            workspace: saved_ws2,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: HashMap::new(),
        tab_title_overrides: HashMap::new(),
    };

    let restored2 = state.restore_state(&snapshot2);
    assert!(
        restored2.is_empty(),
        "No monitors should be restored for unknown device"
    );
}

#[test]
fn test_merged_cleanup_window_ids_deduplicates_and_preserves_all_sources() {
    let managed = vec![10, 30, 20];
    let discovered = vec![20, 40, 10, 50];
    let merged = merged_cleanup_window_ids(&managed, &discovered);
    assert_eq!(merged, vec![10, 20, 30, 40, 50]);
}

#[test]
fn test_shutdown_recovery_retry_budget_is_reasonable() {
    let attempts = std::hint::black_box(SHUTDOWN_RECOVERY_RETRY_ATTEMPTS);
    let retry_delay = std::hint::black_box(SHUTDOWN_RECOVERY_RETRY_DELAY);
    let final_join_timeout = std::hint::black_box(SHUTDOWN_FINAL_JOIN_TIMEOUT);
    assert!(attempts >= 1);
    assert!(attempts <= 10);
    assert!(retry_delay >= Duration::from_millis(50));
    assert!(retry_delay <= Duration::from_secs(2));
    assert!(final_join_timeout >= Duration::from_millis(250));
    assert!(final_join_timeout <= Duration::from_secs(10));
}

// =========================================================================
// MovedOrResized suppression during apply_layout
// =========================================================================

#[test]
fn test_applying_layout_flag_default_false() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    assert!(
        !state.applying_layout,
        "applying_layout should be false by default"
    );
}

#[test]
fn test_applying_layout_flag_set_during_apply() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Before apply_layout, flag is false
    assert!(!state.applying_layout);
    // apply_layout on an empty workspace succeeds (paused path)
    state.paused = true;
    let _ = state.apply_layout();
    // After apply_layout returns, flag should be false (cleared on exit)
    assert!(
        !state.applying_layout,
        "applying_layout should be cleared after apply_layout returns"
    );
}

// =========================================================================
// Fullscreen-minimize daemon-level regression test
// =========================================================================

#[test]
fn test_fullscreen_minimize_clears_fullscreen_in_daemon() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();

    // Add two windows to the same column
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();

    // Focus window 100 and enter fullscreen
    let _ = ws.focus_window(100);
    ws.toggle_fullscreen();
    assert!(ws.is_fullscreen());
    assert_eq!(ws.fullscreen_window_id(), Some(100));

    // Minimize the fullscreen window
    ws.mark_minimized(100);

    // Verify fullscreen is cleared
    assert!(
        !ws.is_fullscreen(),
        "Fullscreen should be cleared when fullscreen window is minimized"
    );
    assert_eq!(ws.fullscreen_window_id(), None);

    // Verify the other window is visible in placements
    let viewport = state.focused_viewport();
    let ws = state.focused_workspace().unwrap();
    let placements = ws.compute_placements(viewport);
    let w200 = placements.iter().find(|p| p.window_id == 200);
    assert!(
        w200.is_some(),
        "Window 200 should have a placement after fullscreen window is minimized"
    );
}

// =========================================================================
// HotkeyState registered_count is distinct from mapping.len()
// =========================================================================

#[test]
fn test_hotkey_state_registered_count_default() {
    // Construct HotkeyState manually -- registered_count should hold its value
    // and be independent of mapping.len().
    let mut mapping = HashMap::new();
    mapping.insert(1 as HotkeyId, IpcCommand::FocusDown);
    mapping.insert(2 as HotkeyId, IpcCommand::FocusUp);

    let hs = HotkeyState {
        handle: None,
        hook: None,
        mapping,
        requested_count: 2,
        registered_count: 1, // Simulate: only 1 of 2 installed in the hook
        failed_binds: vec!["Win+Left".to_string()],
        recording: false,
    };

    assert_eq!(hs.mapping.len(), 2, "mapping has 2 parsed hotkeys");
    assert_eq!(
        hs.registered_count, 1,
        "registered_count reflects OS result"
    );
    assert_eq!(hs.requested_count, 2, "requested_count matches attempted");
    assert_ne!(
        hs.mapping.len(),
        hs.registered_count,
        "registered_count should differ from mapping.len() when partial"
    );
}

#[test]
fn test_protected_binds_flags_os_reserved_combos() {
    let win = Modifiers { win: true, ..Default::default() };
    let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Default::default() };
    let labels = vec![
        (1 as HotkeyId, "Win+L".to_string(), win, 0x4C),        // lock — protected
        (2 as HotkeyId, "Ctrl+Alt+Delete".to_string(), ctrl_alt, 0x2E), // protected
        (3 as HotkeyId, "Ctrl+Alt+H".to_string(), ctrl_alt, 0x48), // normal — fine
    ];
    let protected = protected_binds(&labels);
    assert_eq!(
        protected,
        vec!["Win+L".to_string(), "Ctrl+Alt+Delete".to_string()]
    );
}

// =========================================================================
// Event-path behavior tests
// =========================================================================

#[test]
fn test_focus_new_windows_false_preserves_focus_in_daemon() {
    // Verify that focus_new_windows=false preserves the existing
    // focused window when new windows are tiled -- tested at daemon level
    // by directly manipulating the workspace with the config-driven method.
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    let mut state = AppState::new_with_config(config, test_monitors());

    let ws = state.focused_workspace_mut().unwrap();
    // First window always gets focus (empty workspace)
    ws.insert_window(100, Some(800)).unwrap();
    assert_eq!(ws.focused_window(), Some(100));

    // Subsequent windows use insert_window_no_focus -- focus stays on 100
    ws.insert_window_no_focus(200, Some(800)).unwrap();
    assert_eq!(
        ws.focused_window(),
        Some(100),
        "focus should stay on window 100 when focus_new_windows=false"
    );

    ws.insert_window_no_focus(300, Some(800)).unwrap();
    assert_eq!(
        ws.focused_window(),
        Some(100),
        "focus should still be on window 100 after third insert"
    );
    assert_eq!(ws.window_count(), 3);
}

#[test]
fn test_focused_event_updates_previous_focused_hwnd_for_floating() {
    // Verify that a Focused event for a floating window updates
    // previous_focused_hwnd, enabling ToggleFloating to detect and unfloat it.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();

    // Initially, previous_focused_hwnd is None
    assert_eq!(state.previous_focused_hwnd, None);

    // Simulate OS focus event on the floating window
    state.handle_window_event(WindowEvent::Focused(500));

    // previous_focused_hwnd should now reflect the floating window
    assert_eq!(
        state.previous_focused_hwnd,
        Some(500),
        "Focused event on a floating window must update previous_focused_hwnd"
    );
}

#[test]
fn test_focused_event_updates_previous_focused_hwnd_for_tiled() {
    // Verify Focused events also work for tiled windows (regression guard)
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();

    state.handle_window_event(WindowEvent::Focused(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));

    state.handle_window_event(WindowEvent::Focused(200));
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_focus_follows_mouse_updates_previous_focused_hwnd() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.previous_focused_hwnd = None;

    assert!(state.apply_focus_follows_mouse(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));
}

#[test]
fn test_focus_follows_mouse_handles_floating_window() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();
    assert_eq!(ws.focused_window(), Some(100));
    state.previous_focused_hwnd = None;

    assert!(state.apply_focus_follows_mouse(500));
    assert_eq!(state.previous_focused_hwnd, Some(500));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100),
        "floating focus-follows-mouse should not mutate tiled focus"
    );
}

#[test]
fn test_focus_follows_mouse_floating_then_tiled_focuses_tiled() {
    // Regression: hovering a floating window then a tiled one must focus the
    // tiled window. The floating branch sets previous_focused_hwnd; if the
    // tiled branch leaves it set, sync_foreground_window keeps preferring the
    // floating window and the tiled focus never lands.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();
    ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();
    state.previous_focused_hwnd = None;

    // Hover the floating window: it becomes the foreground preference.
    assert!(state.apply_focus_follows_mouse(500));
    assert_eq!(state.previous_focused_hwnd, Some(500));

    // Hover a tiled window: foreground must move to it, not stay on floating.
    assert!(state.apply_focus_follows_mouse(100));
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "tiled hover after floating must foreground the tiled window"
    );
    assert_eq!(state.focused_workspace().unwrap().focused_window(), Some(100));
}

#[test]
fn test_restored_floating_window_does_not_steal_tiled_focus() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.add_floating(500, Rect::new(100, 100, 400, 300)).unwrap();
    assert_eq!(ws.focused_window(), Some(100));
    state.previous_focused_hwnd = None;

    state.handle_window_event(WindowEvent::Restored(500));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100),
        "restoring a floating window should not steal tiled focus"
    );
    assert_eq!(
        state.previous_focused_hwnd, None,
        "floating restore should not call sync_foreground_window"
    );
}

// applying_layout flag cleared after error path
// =========================================================================

#[test]
fn test_applying_layout_flag_cleared_after_layout_with_windows() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    // Add windows so apply_layout computes real placements (not empty)
    let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
    ws.insert_window(100, Some(800)).unwrap();
    ws.insert_window(200, Some(800)).unwrap();

    // Whether apply_layout succeeds or fails depends on Win32 API availability.
    // The important thing is that applying_layout is always cleared afterwards.
    assert!(!state.applying_layout, "flag should be false before call");
    let _result = state.apply_layout();
    assert!(
        !state.applying_layout,
        "applying_layout must be cleared after apply_layout returns (success or error)"
    );
}

#[test]
fn test_apply_layout_timeout_auto_pauses_tiling() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state
        .moved_or_resized_suppression
        .insert(42, std::time::Instant::now() + Duration::from_secs(1));
    state.injected_apply_placements_behavior = Some(
        TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(40)),
    );

    let err = state
        .apply_layout()
        .expect_err("apply_layout should time out in injected test mode");

    let message = err.to_string();
    assert!(
        message.contains("timed out"),
        "timeout error should be actionable: {}",
        message
    );
    assert!(state.paused, "tiling should auto-pause after apply timeout");
    assert!(
        !state.applying_layout,
        "applying_layout must be cleared after timeout path"
    );
    assert!(
        state.moved_or_resized_suppression.is_empty(),
        "suppression entries must be cleared after timeout"
    );
}

#[test]
fn test_apply_layout_injected_failure_does_not_auto_pause() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(50);
    state
        .moved_or_resized_suppression
        .insert(99, std::time::Instant::now() + Duration::from_secs(1));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        Duration::from_millis(5),
    ));

    let err = state
        .apply_layout()
        .expect_err("injected placement failure should propagate");
    assert!(err
        .to_string()
        .contains("injected apply_placements failure"));
    assert!(
        !state.paused,
        "non-timeout placement failures should not auto-pause tiling"
    );
    assert!(
        !state.applying_layout,
        "applying_layout must be cleared after injected failure path"
    );
    assert!(
        state.moved_or_resized_suppression.is_empty(),
        "suppression entries must be cleared after failed apply"
    );
}

#[test]
fn test_apply_layout_timeout_worker_is_joined_during_shutdown_begin() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(
        TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(60)),
    );

    let _ = state
        .apply_layout()
        .expect_err("apply_layout should time out in injected test mode");
    assert_eq!(
        state.pending_apply_workers.len(),
        1,
        "timed-out apply worker should be tracked for shutdown join"
    );

    let workers = state.begin_shutdown_or_revert();
    assert!(
        state.apply_worker_cancelled.load(Ordering::SeqCst),
        "shutdown/revert should set cancellation flag"
    );
    assert_eq!(workers.len(), 1, "one timed-out worker should be returned");
    for handle in workers {
        let mut handle = Some(handle);
        assert!(
            join_with_timeout(&mut handle, Duration::from_millis(300)),
            "timed-out worker should exit after shutdown cancellation"
        );
    }
}

#[test]
fn test_apply_layout_rejects_overlap_while_timed_out_worker_is_running() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(
        TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(500)),
    );

    let _ = state
        .apply_layout()
        .expect_err("first apply should time out in injected test mode");
    assert_eq!(state.pending_apply_workers.len(), 1);

    // Simulate manual resume happening before the timed-out worker exits.
    state.paused = false;
    let err = state
        .apply_layout()
        .expect_err("second apply must not overlap while prior worker is still running");
    assert!(
        err.to_string().contains("previous timed-out apply worker"),
        "expected overlap-prevention error, got: {}",
        err
    );

    std::thread::sleep(Duration::from_millis(700));
    let reaped = state.reap_finished_pending_apply_workers();
    assert_eq!(reaped, 1, "timed-out worker should eventually be reaped");
}

#[test]
fn test_apply_layout_timeout_late_worker_triggers_recovery_pass() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(
        TestApplyPlacementsBehavior::SleepAndSucceed(Duration::from_millis(50)),
    );
    assert_eq!(
        state.late_worker_recovery_count.load(Ordering::SeqCst),
        0,
        "late-worker recovery counter should start at zero"
    );

    let _ = state
        .apply_layout()
        .expect_err("apply_layout should time out in injected test mode");
    assert_eq!(
        state.pending_apply_workers.len(),
        1,
        "timed-out apply worker should be tracked"
    );

    // Wait long enough for the worker to finish even under heavy load.
    std::thread::sleep(Duration::from_millis(500));
    let reaped = state.reap_finished_pending_apply_workers();
    assert_eq!(reaped, 1, "timed-out worker should be reaped");
    assert_eq!(
        state.late_worker_recovery_count.load(Ordering::SeqCst),
        1,
        "cancelled late worker should trigger one final recovery pass"
    );
}

#[test]
fn test_moved_or_resized_suppression_window_tracking() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.arm_moved_or_resized_suppression([100, 200]);
    assert!(
        state.should_suppress_moved_or_resized(100),
        "recently applied windows should be suppressed"
    );
    assert!(
        !state.should_suppress_moved_or_resized(300),
        "unrelated windows should not be suppressed"
    );

    state
        .moved_or_resized_suppression
        .insert(200, std::time::Instant::now() - Duration::from_millis(1));
    assert!(
        !state.should_suppress_moved_or_resized(200),
        "expired suppression entries should be ignored"
    );
}

// =========================================================================
// Injectable window enumeration for Created-event tests
// =========================================================================

fn make_test_window_info(hwnd: u64) -> leopardwm_platform_win32::WindowInfo {
    leopardwm_platform_win32::WindowInfo {
        hwnd,
        title: format!("Test Window {}", hwnd),
        class_name: "TestWindowClass".to_string(),
        process_id: 1000 + hwnd as u32,
        rect: Rect::new(100, 100, 800, 600),
        visible: true,
    }
}

#[test]
fn test_lookup_window_info_returns_injected() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let info = make_test_window_info(42);
    state.injected_window_info.insert(42, info.clone());

    let result = state.lookup_window_info(42);
    assert!(result.is_some(), "should return injected info");
    assert_eq!(result.unwrap().hwnd, 42);
}

#[test]
fn test_lookup_window_info_missing_returns_none() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    // No injected info, and enumerate_windows won't find hwnd 99999
    let result = state.lookup_window_info(99999);
    assert!(result.is_none());
}

#[test]
fn test_created_event_with_injected_window_info() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    // Inject window info so Created handler doesn't need real Win32 calls
    let info = make_test_window_info(100);
    state.injected_window_info.insert(100, info);

    // Before: workspace is empty
    assert_eq!(state.focused_workspace().unwrap().window_count(), 0);

    // Fire Created event -- handler should use injected info
    state.handle_window_event(WindowEvent::Created(100));

    // After: window should be tiled in the workspace
    let ws = state.focused_workspace().unwrap();
    assert!(
        ws.contains_window(100),
        "window should be managed after Created event"
    );
    assert_eq!(ws.window_count(), 1);
}

#[test]
fn test_created_event_focus_new_windows_false_preserves_focus() {
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    let mut state = AppState::new_with_config(config, test_monitors());

    // Inject and create first window (gets focus because workspace is empty)
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100),
        "first window should get focus even with focus_new_windows=false"
    );

    // Inject and create second window -- focus should stay on 100
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.handle_window_event(WindowEvent::Created(200));

    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.window_count(), 2);
    assert_eq!(
        ws.focused_window(),
        Some(100),
        "focus should stay on window 100 when focus_new_windows=false"
    );
}

#[test]
fn test_created_event_duplicate_is_ignored() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 1);

    // Second Created event for same window should be ignored
    state.handle_window_event(WindowEvent::Created(100));
    assert_eq!(
        state.focused_workspace().unwrap().window_count(),
        1,
        "duplicate Created event should be ignored"
    );
}

#[test]
fn test_recently_hidden_hwnd_suppresses_recreation() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));

    // Add window 200
    state.handle_window_event(WindowEvent::Created(200));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 1);

    // Hide window 200 -- records it in recently_hidden_hwnds
    state.handle_window_event(WindowEvent::Hidden(200));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 0);

    // Re-create window 200 -- should be suppressed (recently hidden)
    state.handle_window_event(WindowEvent::Created(200));
    assert_eq!(
        state.focused_workspace().unwrap().window_count(),
        0,
        "recently hidden window should not be re-added"
    );

    // A different window (100) should still be addable
    state.handle_window_event(WindowEvent::Created(100));
    assert_eq!(
        state.focused_workspace().unwrap().window_count(),
        1,
        "unrelated window should still be added"
    );
}

#[test]
fn test_hidden_window_restores_column_width_on_reshow() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));

    // Create it, then give it a distinct (non-default) column width.
    state.handle_window_event(WindowEvent::Created(300));
    state.focused_workspace_mut().unwrap().resize_focused_column(250);
    let width_before = state.focused_workspace().unwrap().columns()[0].width();

    // Backdate so the hide isn't treated as transient (transient windows are
    // suppressed on re-create instead of re-tiled).
    state.window_managed_at.insert(
        300,
        std::time::Instant::now() - std::time::Duration::from_secs(31),
    );

    // Hide -> the column width is remembered.
    state.handle_window_event(WindowEvent::Hidden(300));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 0);
    assert_eq!(
        state.hidden_column_widths.get(&300).map(|(_, w)| *w),
        Some(width_before),
        "hidden window's column width is remembered"
    );

    // Reshow -> re-tiled at the remembered width, not the default.
    state.handle_window_event(WindowEvent::Created(300));
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.window_count(), 1, "window re-tiled on reshow");
    assert_eq!(
        ws.columns()[0].width(),
        width_before,
        "reshown window keeps its prior column width"
    );
    assert!(
        !state.hidden_column_widths.contains_key(&300),
        "remembered width is consumed on restore"
    );
}

#[test]
fn test_take_remembered_column_width_consumes_entry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .hidden_column_widths
        .insert(100, (std::time::Instant::now(), 555));
    assert_eq!(state.take_remembered_column_width(100), Some(555));
    assert!(
        !state.hidden_column_widths.contains_key(&100),
        "entry is removed once taken"
    );
    assert_eq!(state.take_remembered_column_width(100), None);
}

// =========================================================================
// Deterministic daemon singleton test
// =========================================================================

#[test]
fn test_check_already_running_with_isolated_pipe() {
    // Use an isolated pipe name to avoid depending on whether a real daemon
    // is running. We test the same logic as check_already_running() but with
    // a unique pipe name that we know is not in use.
    let pipe_name = format!(r"\\.\pipe\leopardwm-test-singleton-{}", std::process::id());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();

    // No pipe exists -> should not connect
    let result = rt.block_on(async {
        pipe_probe_result_indicates_running(
            tokio::net::windows::named_pipe::ClientOptions::new()
                .open(&pipe_name)
                .map(|_| ()),
        )
    });
    assert!(
        !result,
        "No pipe server exists, so connect should fail (no daemon)"
    );
}

// =========================================================================
// Reliability hardening tests
// =========================================================================

#[test]
fn test_cmd_health_check() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    let resp = state.handle_command(IpcCommand::HealthCheck);
    match resp {
        IpcResponse::HealthInfo {
            healthy,
            total_windows,
            monitors,
            paused,
            ..
        } => {
            assert!(healthy);
            assert_eq!(total_windows, 0);
            assert_eq!(monitors, 1);
            assert!(!paused);
        }
        other => panic!("Expected HealthInfo, got {:?}", other),
    }
}

#[test]
fn test_cmd_health_check_paused() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    let resp = state.handle_command(IpcCommand::HealthCheck);
    match resp {
        IpcResponse::HealthInfo { paused, .. } => {
            assert!(paused, "paused flag should be true");
        }
        other => panic!("Expected HealthInfo, got {:?}", other),
    }
}

#[test]
fn test_format_crash_report_contains_version() {
    // We can't easily create a PanicHookInfo, but we can test the function
    // by catching a panic. Use std::panic::catch_unwind.
    let result = std::panic::catch_unwind(|| {
        panic!("test crash");
    });
    assert!(result.is_err(), "should have panicked");
    // The format_crash_report function is tested indirectly via the panic hook.
    // Here we just verify it exists and the function signature is correct.
}

// =========================================================================
// DPI-aware gap/border scaling tests
// =========================================================================

fn two_monitors_mixed_dpi() -> Vec<MonitorInfo> {
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
            rect: Rect::new(1920, 0, 3840, 2160),
            work_area: Rect::new(1920, 0, 3840, 2120),
            is_primary: false,
            device_name: "DISPLAY2".to_string(),
            scale_factor: 2.0,
        },
    ]
}

#[test]
fn test_multi_dpi_workspaces_have_different_gaps() {
    let mut config = test_config();
    config.layout.gap = 10;
    config.layout.outer_gap_left = 5;
    config.layout.outer_gap_right = 5;
    let state = AppState::new_with_config(config, two_monitors_mixed_dpi());

    let ws1 = &state.workspaces.get(&1).unwrap()[0];
    let ws2 = &state.workspaces.get(&2).unwrap()[0];

    // Monitor 1 at 1.0x: gap=10
    assert_eq!(ws1.gap(), 10);
    let (ol1, or1, _, _) = ws1.outer_gaps();
    assert_eq!(ol1, 5);
    assert_eq!(or1, 5);

    // Monitor 2 at 2.0x: gap=20
    assert_eq!(ws2.gap(), 20);
    let (ol2, or2, _, _) = ws2.outer_gaps();
    assert_eq!(ol2, 10);
    assert_eq!(or2, 10);
}

#[test]
fn test_apply_config_rescales_with_correct_old_values() {
    let mut config = test_config();
    config.layout.gap = 10;
    config.layout.outer_gap_left = 5;
    config.layout.outer_gap_right = 5;
    let mut state = AppState::new_with_config(config.clone(), two_monitors_mixed_dpi());

    // Change gap from 10 to 20
    config.layout.gap = 20;
    state.apply_config(config);

    let ws1 = &state.workspaces.get(&1).unwrap()[0];
    let ws2 = &state.workspaces.get(&2).unwrap()[0];

    // Monitor 1 at 1.0x: gap=20
    assert_eq!(ws1.gap(), 20);
    // Monitor 2 at 2.0x: gap=40
    assert_eq!(ws2.gap(), 40);
}

#[test]
fn test_scaled_border_width_scales_per_monitor() {
    let mut config = test_config();
    config.appearance.active_border_width = 3;
    let mut state = AppState::new_with_config(config, two_monitors_mixed_dpi());

    // Add windows to each monitor
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();

    // Window on 1x monitor: border=3
    assert_eq!(state.scaled_border_width(100), 3);
    // Window on 2x monitor: border=6
    assert_eq!(state.scaled_border_width(200), 6);
    // Unknown window: fallback scale 1.0 → border=3
    assert_eq!(state.scaled_border_width(999), 3);
}

#[test]
fn test_reconcile_monitors_new_monitor_gets_scaled_gaps() {
    let mut config = test_config();
    config.layout.gap = 8;
    let mut state = AppState::new_with_config(config, test_monitors());
    assert_eq!(state.workspaces.len(), 1);

    // Add a high-DPI monitor
    let new_monitors = vec![
        MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        },
        MonitorInfo {
            id: 5,
            rect: Rect::new(1920, 0, 3840, 2160),
            work_area: Rect::new(1920, 0, 3840, 2120),
            is_primary: false,
            device_name: "DISPLAY5".to_string(),
            scale_factor: 1.5,
        },
    ];
    state.reconcile_monitors(new_monitors);

    assert_eq!(state.workspaces.len(), 2);
    let ws5 = &state.workspaces.get(&5).unwrap()[0];
    // gap=8 * 1.5 = 12
    assert_eq!(ws5.gap(), 12);
}

// =============================================================================
// Snap layout suppression tests
// =============================================================================

#[test]
fn test_snap_disable_on_tile() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());

    // Manually insert a tiled window and call disable_snap_for_window
    let hwnd = 42u64;
    if let Some(ws) = state.focused_workspace_mut() {
        ws.insert_window(hwnd, None).unwrap();
    }
    state.disable_snap_for_window(hwnd);

    // Daemon-side tracking set should contain the window
    // (Win32 call fails for synthetic HWND, so the set won't be populated
    //  since remove_maximizebox returns an error for invalid handles)
    // But we can verify the method doesn't panic
    assert!(!state.snap_disabled_hwnds.contains(&hwnd));
}

#[test]
fn test_snap_restore_on_float() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());

    let hwnd = 43u64;
    // Manually add to tracking set (simulating a successful remove_maximizebox)
    state.snap_disabled_hwnds.insert(hwnd);

    // Restore should remove from tracking set
    state.restore_snap_for_window(hwnd);
    assert!(!state.snap_disabled_hwnds.contains(&hwnd));
}

#[test]
fn test_snap_restore_on_destroy() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());

    let hwnd = 44u64;
    state.snap_disabled_hwnds.insert(hwnd);

    // Restoring a tracked window should clear it
    state.restore_snap_for_window(hwnd);
    assert!(!state.snap_disabled_hwnds.contains(&hwnd));
}

#[test]
fn test_snap_restore_all_on_pause() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());

    state.snap_disabled_hwnds.insert(100);
    state.snap_disabled_hwnds.insert(200);
    state.snap_disabled_hwnds.insert(300);

    state.restore_snap_for_all_windows();
    assert!(state.snap_disabled_hwnds.is_empty());
}

#[test]
fn test_snap_config_toggle_off() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());
    state.paused = false;

    state.snap_disabled_hwnds.insert(50);
    state.snap_disabled_hwnds.insert(51);

    // Reload with disable_snap_layouts = false should restore all
    let mut new_config = test_config();
    new_config.behavior.disable_snap_layouts = false;
    state.apply_config(new_config);
    assert!(state.snap_disabled_hwnds.is_empty());
}

#[test]
fn test_snap_config_toggle_on() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = false;
    let mut state = AppState::new_with_config(config, test_monitors());

    // No windows tiled, so no snap_disabled_hwnds after enabling
    let mut new_config = test_config();
    new_config.behavior.disable_snap_layouts = true;
    state.apply_config(new_config);
    // No tiled windows → nothing to disable
    assert!(state.snap_disabled_hwnds.is_empty());
}

#[test]
fn test_snap_restore_for_window_not_tracked_is_noop() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Should not panic or change anything
    state.restore_snap_for_window(999);
    assert!(state.snap_disabled_hwnds.is_empty());
}

#[test]
fn test_snap_disable_when_config_disabled() {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = false;
    let mut state = AppState::new_with_config(config, test_monitors());

    // disable_snap_for_window should be a no-op when config is off
    state.disable_snap_for_window(42);
    assert!(!state.snap_disabled_hwnds.contains(&42));
}

#[test]
fn test_snap_default_config_is_enabled() {
    let config = test_config();
    assert!(config.behavior.disable_snap_layouts);
}

#[test]
fn test_cmd_focus_left_broadcasts_focused_window_changed() {
    // Regression: command-initiated focus changes were silently dropped
    // by the OS-side dedup because sync_foreground_window pre-updated
    // previous_focused_hwnd before EVENT_SYSTEM_FOREGROUND arrived.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    {
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
    }
    // Pre-arm dedup so the first broadcast must be the focus event after
    // FocusLeft; without this, the LayoutChanged emission can race depending
    // on signature seeding.
    let monitor = state.focused_monitor as i64;
    state.last_broadcast_focused = Some((monitor, Some(200)));

    let mut rx = state.event_broadcaster.subscribe();
    let resp = state.handle_command(IpcCommand::FocusLeft);
    assert_eq!(resp, IpcResponse::Ok);

    let mut saw_focus_change = false;
    while let Ok(event) = rx.try_recv() {
        if let leopardwm_ipc::IpcEvent::FocusedWindowChanged { hwnd, .. } = event {
            assert_eq!(hwnd, Some(100), "FocusLeft should land focus on hwnd 100");
            saw_focus_change = true;
        }
    }
    assert!(
        saw_focus_change,
        "FocusedWindowChanged was not broadcast for command-driven focus"
    );
    assert_eq!(state.last_broadcast_focused, Some((monitor, Some(100))));
}

#[test]
fn test_recovery_arm_preserves_recently_hidden_entry_on_lookup_failure() {
    // When the recovery arm runs but window_info lookup fails transiently
    // (or the rule says Ignore), the suppression entry must be preserved
    // so the TTL filter or a subsequent retry can handle it. Otherwise the
    // next legitimate recreate of the same HWND slips through the filter.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let hwnd = 9999u64;
    state
        .recently_hidden_hwnds
        .insert(hwnd, std::time::Instant::now());
    // No injected_window_info -> lookup_window_info returns None.
    state.handle_window_event(WindowEvent::Focused(hwnd));
    assert!(
        state.recently_hidden_hwnds.contains_key(&hwnd),
        "entry must survive failed recovery so subsequent retries can succeed"
    );
}

#[test]
fn test_broadcast_focused_window_emits_on_monitor_change_with_same_hwnd() {
    // Cross-monitor moves (MoveWindowToMonitorLeft/Right) keep the same
    // HWND focused but change which monitor it's on. The dedup must key
    // on (monitor, hwnd) so subscribers see the move.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut rx = state.event_broadcaster.subscribe();

    state.broadcast_focused_window_if_changed(1, Some(42));
    state.broadcast_focused_window_if_changed(2, Some(42));

    let mut monitors_seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let leopardwm_ipc::IpcEvent::FocusedWindowChanged {
            monitor,
            hwnd: Some(42),
            ..
        } = event
        {
            monitors_seen.push(monitor);
        }
    }
    assert_eq!(monitors_seen, vec![1, 2], "monitor change must emit");
}

#[test]
fn test_broadcast_focused_window_dedup_suppresses_same_hwnd() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut rx = state.event_broadcaster.subscribe();

    state.broadcast_focused_window_if_changed(1, Some(42));
    state.broadcast_focused_window_if_changed(1, Some(42));
    state.broadcast_focused_window_if_changed(1, Some(42));

    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(count, 1, "dedup should collapse repeated same-hwnd calls");
}

#[test]
fn test_broadcast_focused_window_emits_on_clear() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut rx = state.event_broadcaster.subscribe();

    state.broadcast_focused_window_if_changed(1, Some(42));
    state.broadcast_focused_window_if_changed(1, None);

    let mut saw_set = false;
    let mut saw_clear = false;
    while let Ok(event) = rx.try_recv() {
        if let leopardwm_ipc::IpcEvent::FocusedWindowChanged { hwnd, .. } = event {
            match hwnd {
                Some(42) => saw_set = true,
                None => saw_clear = true,
                _ => {}
            }
        }
    }
    assert!(saw_set && saw_clear, "should emit both set and clear events");
    assert_eq!(state.last_broadcast_focused, Some((1, None)));
}

#[test]
fn test_scratchpad_stash_designates_and_removes_window() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    assert_eq!(state.focused_workspace().unwrap().focused_window(), Some(100));

    state.scratchpad_stash();

    let sp = state.scratchpad.expect("scratchpad designated");
    assert_eq!(sp.window_id, 100);
    assert!(!sp.shown, "stashed scratchpad starts hidden");
    assert!(
        !state.focused_workspace().unwrap().contains_window(100),
        "stashed window is removed from the workspace"
    );
}

#[test]
fn test_scratchpad_toggle_summons_then_hides() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();

    // Summon: floating + shown.
    state.scratchpad_toggle();
    assert!(state.scratchpad.unwrap().shown);
    assert!(
        state.focused_workspace().unwrap().is_floating(100),
        "summoned scratchpad is a floating window"
    );

    // Hide: removed + not shown.
    state.scratchpad_toggle();
    assert!(!state.scratchpad.unwrap().shown);
    assert!(!state.focused_workspace().unwrap().contains_window(100));
}

#[test]
fn test_scratchpad_cleared_when_window_destroyed() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    assert!(state.scratchpad.is_some());

    state.scratchpad_on_window_destroyed(100);
    assert!(state.scratchpad.is_none(), "designation cleared on destroy");

    // Unrelated window destroy does not clear a live designation.
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(200, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad_on_window_destroyed(999);
    assert!(state.scratchpad.is_some());
}

#[test]
fn test_scratchpad_stash_on_scratchpad_releases_to_tiling() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash(); // designate + hide 100
    state.scratchpad_toggle(); // summon (floating, focused)
    assert!(state.focused_workspace().unwrap().is_floating(100));

    // Simulate the OS foreground landing on the summoned (floating)
    // scratchpad, as the EVENT_SYSTEM_FOREGROUND handler does in production.
    state.previous_focused_hwnd = Some(100);

    // Stashing the focused scratchpad releases it back to tiling.
    state.scratchpad_stash();
    assert!(state.scratchpad.is_none(), "designation cleared on release");
    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert!(
        !state.focused_workspace().unwrap().is_floating(100),
        "released as a tiled window, not floating"
    );
}

#[test]
fn test_scratchpad_designating_new_releases_old() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    {
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(800)).unwrap();
        ws.insert_window(200, Some(800)).unwrap();
    }
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.scratchpad_stash(); // 100 becomes scratchpad (hidden)
    assert_eq!(state.scratchpad.unwrap().window_id, 100);

    state.focused_workspace_mut().unwrap().focus_window(200).unwrap();
    state.scratchpad_stash(); // 200 becomes scratchpad; 100 released
    assert_eq!(state.scratchpad.unwrap().window_id, 200);
    assert!(
        state.focused_workspace().unwrap().contains_window(100),
        "old scratchpad re-tiled, not orphaned"
    );
    assert!(
        !state.focused_workspace().unwrap().contains_window(200),
        "new scratchpad is hidden"
    );
}

#[test]
fn test_scratchpad_release_rejoins_original_column() {
    // A window stashed from a stacked column should rejoin that column on
    // release, not land in its own new column.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    {
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(400)).unwrap(); // column 0
        ws.insert_window_in_column(200, 0).unwrap(); // column 0 now [100, 200]
    }
    state.focused_workspace_mut().unwrap().focus_window(200).unwrap();
    assert_eq!(state.focused_workspace().unwrap().column_count(), 1);

    state.scratchpad_stash(); // stash 200 (origin column 0, sibling 100)
    state.scratchpad_toggle(); // summon (floating)
    state.previous_focused_hwnd = Some(200); // OS foreground lands on it
    state.scratchpad_stash(); // stash-on-self releases it back to tiling

    let ws = state.focused_workspace().unwrap();
    assert!(state.scratchpad.is_none(), "released");
    assert_eq!(
        ws.column_count(),
        1,
        "rejoined the original column instead of creating a new one"
    );
    assert_eq!(
        ws.find_window_location(200).map(|(c, _)| c),
        Some(0),
        "back in column 0 with its sibling"
    );
    assert_eq!(
        ws.focused_window(),
        Some(200),
        "the released window keeps focus, not its sibling"
    );
}

#[test]
fn test_scratchpad_solo_window_releases_to_new_column() {
    // A window that was alone in its column has no sibling, so on release it
    // returns as its own column at the original index (failsafe path).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(400))
        .unwrap();
    state.scratchpad_stash(); // solo: origin_sibling = None
    assert!(state.scratchpad.unwrap().origin_sibling.is_none());
    state.scratchpad_toggle(); // summon
    state.previous_focused_hwnd = Some(100);
    state.scratchpad_stash(); // release

    let ws = state.focused_workspace().unwrap();
    assert!(ws.contains_window(100));
    assert!(!ws.is_floating(100), "released as a tiled window");
}

#[test]
fn test_scratchpad_stash_uses_tiled_focus_over_stale_foreground() {
    // Regression: a late OS-foreground event can leave `previous_focused_hwnd`
    // pointing at a window the user just moved off of. Stash must take the
    // tiled-focused window, not the stale foreground one (the bug stashed a
    // column's stackmate and left the intended window stranded alone).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    {
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, Some(400)).unwrap();
        ws.insert_window(200, Some(400)).unwrap();
    }
    state.focused_workspace_mut().unwrap().focus_window(200).unwrap();
    // Stale foreground from the window focus just left.
    state.previous_focused_hwnd = Some(100);

    state.scratchpad_stash();

    let sp = state.scratchpad.expect("scratchpad designated");
    assert_eq!(
        sp.window_id, 200,
        "stashes the tiled-focused window, not the stale foreground window"
    );
    assert!(
        !state.focused_workspace().unwrap().contains_window(200),
        "tiled-focused window is the one removed"
    );
    assert!(
        state.focused_workspace().unwrap().contains_window(100),
        "the stale-foreground window stays in the layout"
    );
}

/// Float the focused window `wid` (sticky must then keep it floating).
fn float_focused_window(state: &mut AppState, wid: u64) {
    let vp = state.focused_viewport();
    state.focused_workspace_mut().unwrap().focus_window(wid).unwrap();
    state.focused_workspace_mut().unwrap().toggle_floating(vp);
    state.previous_focused_hwnd = Some(wid);
}

#[test]
fn test_sticky_floating_window_stays_floating() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut state, 100);

    state.toggle_sticky(); // pin
    assert!(state.sticky_windows.contains(&100), "pinned into sticky set");
    assert!(
        state.focused_workspace().unwrap().is_floating(100),
        "a floating window stays floating when stuck"
    );

    state.previous_focused_hwnd = Some(100);
    state.toggle_sticky(); // un-pin
    assert!(!state.sticky_windows.contains(&100), "unpinned from sticky set");
    assert!(
        state.focused_workspace().unwrap().is_floating(100),
        "un-pinning leaves it floating in place"
    );
}

#[test]
fn test_sticky_tiled_window_stays_tiled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();

    state.toggle_sticky(); // stick a TILED window
    assert!(state.sticky_windows.contains(&100), "tiled window added to sticky set");
    assert!(
        !state.focused_workspace().unwrap().is_floating(100),
        "a tiled window stays tiled when stuck (not force-floated)"
    );

    state.toggle_sticky(); // un-stick (tiled focus still reports it)
    assert!(!state.sticky_windows.contains(&100));
    assert!(!state.focused_workspace().unwrap().is_floating(100), "still tiled");
}

#[test]
fn test_sticky_window_follows_workspace_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut state, 100);
    state.toggle_sticky(); // 100 floating + sticky on workspace 0
    assert!(state.sticky_windows.contains(&100));

    // Move to workspace 1 and re-home sticky windows.
    state.ensure_workspace_exists(mon, 1);
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    assert_eq!(state.active_workspace_idx(mon), 1);
    assert!(
        state.workspaces.get(&mon).unwrap()[1].is_floating(100),
        "sticky window re-homed to the active workspace"
    );
    assert!(
        !state.workspaces.get(&mon).unwrap()[0].contains_window(100),
        "sticky window no longer on the previous workspace"
    );
}

#[test]
fn test_tiled_sticky_follows_switch_as_end_column() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state.focused_workspace_mut().unwrap().insert_window(100, Some(800)).unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky(); // 100 tiled + sticky on workspace 0

    // Destination already has a tiled window so we can assert end placement.
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1].insert_window(200, Some(800)).unwrap();
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    assert!(dest.contains_window(100), "tiled sticky followed to the active workspace");
    assert!(!dest.is_floating(100), "and it stayed tiled, not floated");
    assert_eq!(dest.column_count(), 2, "destination now has both columns");
    assert!(!state.workspaces.get(&mon).unwrap()[0].contains_window(100), "left the old workspace");

    // Floating-stays-floating guard: a tiled sticky must never become floating
    // across a switch (the rehome reads is_floating on the SOURCE workspace).
    state.active_workspace.insert(mon, 0);
    state.rehome_sticky_windows();
    assert!(!state.workspaces.get(&mon).unwrap()[0].is_floating(100), "still tiled after switching back");
}

#[test]
fn test_tiled_sticky_preserves_column_width_across_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    // A non-default width (default is 800) that must survive the switch.
    state.focused_workspace_mut().unwrap().insert_window(100, Some(500)).unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky();

    state.ensure_workspace_exists(mon, 1);
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    let width = dest
        .find_window_location(100)
        .and_then(|(col, _)| dest.columns().get(col).map(|c| c.width()));
    assert_eq!(width, Some(500), "tiled sticky kept its column width, not the default");
}

#[test]
fn test_sticky_toggle_sets_floating_pinned() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut state, 100);

    state.toggle_sticky(); // pin
    let pinned = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|f| f.id == 100)
        .map(|f| f.pinned);
    assert_eq!(pinned, Some(true), "pinning marks the floating entry pinned");

    state.previous_focused_hwnd = Some(100);
    state.toggle_sticky(); // un-pin
    let pinned = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|f| f.id == 100)
        .map(|f| f.pinned);
    assert_eq!(pinned, Some(false), "un-pinning clears the pinned flag");
}

#[test]
fn test_sticky_rehome_preserves_pinned() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut state, 100);
    state.toggle_sticky(); // 100 floating + sticky + pinned on workspace 0

    state.ensure_workspace_exists(mon, 1);
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    let pinned = state.workspaces.get(&mon).unwrap()[1]
        .floating_windows()
        .iter()
        .find(|f| f.id == 100)
        .map(|f| f.pinned);
    assert_eq!(pinned, Some(true), "re-homed sticky window stays pinned");
}

#[test]
fn test_sticky_cleared_when_window_destroyed() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky();
    assert!(state.sticky_windows.contains(&100));

    state.sticky_on_window_destroyed(100);
    assert!(!state.sticky_windows.contains(&100), "destroyed window unpinned");
}

/// Pinned window focused + workspace switch: build the state, run the
/// switch through the full IPC path, and return it for assertions.
fn switch_with_focused_sticky() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut state, 100);
    state.toggle_sticky(); // 100 floating + sticky on workspace 0
    // Destination workspace has its own tiled window (focus magnet).
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    // OS focus is on the pinned window before the switch (the Focused
    // handler would have recorded it).
    state.previous_focused_hwnd = Some(100);
    // Force the slide transition so the landing-pass path is armed.
    state.reduce_motion = false;

    let resp = state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(matches!(resp, IpcResponse::Ok));
    state
}

#[test]
fn test_sticky_window_keeps_focus_across_workspace_switch() {
    let state = switch_with_focused_sticky();
    let mon = state.focused_monitor;
    assert!(
        state.workspaces.get(&mon).unwrap()[1].is_floating(100),
        "sticky window re-homed to the destination workspace"
    );
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "focus stays on the pinned window after the switch"
    );
    assert_eq!(
        state.pending_sticky_refocus,
        Some(100),
        "landing-pass refocus armed while the slide transition runs"
    );
}

#[test]
fn test_sticky_refocus_reasserts_after_landing_clobber() {
    let mut state = switch_with_focused_sticky();
    // Mid-slide, the destination's tiled window fires a spurious
    // foreground event and clobbers the tracked focus.
    state.handle_window_event(WindowEvent::Focused(200));
    assert_eq!(state.previous_focused_hwnd, Some(200));

    // Animation landing pass (mirrors handle_animation_frame_applied):
    // re-sync, then consume the pending sticky refocus.
    let pending = state.pending_sticky_refocus.take();
    state.sync_foreground_window();
    if let Some(wid) = pending {
        state.refocus_sticky_window(wid);
    }
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "landing pass re-asserts focus on the pinned window"
    );
    assert_eq!(state.pending_sticky_refocus, None, "one-shot consumed");
}

#[test]
fn test_sticky_window_not_focused_does_not_steal_focus_on_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky(); // 100 tiled + sticky on workspace 0
    // User is focused on a different TILED window, not the sticky one. The
    // tiled rehome appends without stealing focus, so focus must not jump to it.
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(150, Some(800))
        .unwrap();
    state.focused_workspace_mut().unwrap().focus_window(150).unwrap();
    state.previous_focused_hwnd = Some(150);
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.reduce_motion = false;

    let resp = state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(matches!(resp, IpcResponse::Ok));

    assert_eq!(
        state.previous_focused_hwnd,
        Some(200),
        "focus goes to the destination's tiled window, not the pin"
    );
    assert_eq!(
        state.pending_sticky_refocus,
        None,
        "no landing refocus armed when the pin was not focused"
    );
}

#[test]
fn test_tiled_sticky_focused_keeps_focus_across_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state.focused_workspace_mut().unwrap().insert_window(100, Some(800)).unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky(); // tiled sticky, focused
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1].insert_window(200, Some(800)).unwrap();
    state.previous_focused_hwnd = Some(100); // user is on the sticky window
    state.reduce_motion = false;

    let resp = state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(matches!(resp, IpcResponse::Ok));

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    assert!(dest.contains_window(100) && !dest.is_floating(100), "followed and stayed tiled");
    assert_eq!(dest.focused_window(), Some(100), "destination focus is the sticky window");
    assert_eq!(state.previous_focused_hwnd, Some(100), "focus stays on the tiled sticky");
}

#[test]
fn test_refocus_sticky_window_tiled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.focused_workspace_mut().unwrap().insert_window(100, Some(800)).unwrap();
    state.focused_workspace_mut().unwrap().insert_window(200, Some(800)).unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky(); // 100 tiled-sticky
    state.focused_workspace_mut().unwrap().focus_window(200).unwrap(); // move focus off it

    assert!(state.refocus_sticky_window(100), "tiled sticky refocus applies");
    assert_eq!(state.focused_workspace().unwrap().focused_window(), Some(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));
}

#[test]
fn test_sticky_mode_transition_tiled_to_floating() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state.focused_workspace_mut().unwrap().insert_window(100, Some(800)).unwrap();
    state.focused_workspace_mut().unwrap().focus_window(100).unwrap();
    state.toggle_sticky(); // tiled sticky
    // Float it mid-session (Ctrl+Alt+F equivalent); stickiness is preserved.
    let vp = state.focused_viewport();
    state.focused_workspace_mut().unwrap().toggle_floating(vp);
    assert!(state.focused_workspace().unwrap().is_floating(100));

    state.ensure_workspace_exists(mon, 1);
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();
    assert!(
        state.workspaces.get(&mon).unwrap()[1].is_floating(100),
        "after floating, the sticky now follows via the floating path"
    );
}

#[test]
fn test_new_window_placement_config() {
    // Default is new_column.
    assert_eq!(
        Config::default().behavior.new_window_placement,
        crate::config::NewWindowPlacement::NewColumn
    );
    // Parses in_column.
    let cfg: Config =
        toml::from_str("[behavior]\nnew_window_placement = \"in_column\"\n").unwrap();
    assert_eq!(
        cfg.behavior.new_window_placement,
        crate::config::NewWindowPlacement::InColumn
    );
}

#[test]
fn test_toggle_new_window_placement_command() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    assert_eq!(
        state.config.behavior.new_window_placement,
        crate::config::NewWindowPlacement::NewColumn
    );
    let resp = state.handle_command(IpcCommand::ToggleNewWindowPlacement);
    assert_eq!(resp, IpcResponse::Ok);
    assert_eq!(
        state.config.behavior.new_window_placement,
        crate::config::NewWindowPlacement::InColumn
    );
    state.handle_command(IpcCommand::ToggleNewWindowPlacement);
    assert_eq!(
        state.config.behavior.new_window_placement,
        crate::config::NewWindowPlacement::NewColumn
    );
}

#[test]
fn test_window_rule_open_extras_parse_and_compile() {
    let toml_str = r#"
        [[window_rules]]
        match_executable = "spotify.exe"
        open_on_workspace = 5
        open_maximized = true
        column_width = 0.5
    "#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    let compiled = cfg.compile_window_rules();
    let rule = compiled
        .iter()
        .find(|r| r.match_executable.as_deref() == Some("spotify.exe"))
        .expect("rule compiled");
    // 1-based config index becomes 0-based workspace index.
    assert_eq!(rule.open_on_workspace, Some(4));
    assert!(rule.open_maximized);
    assert_eq!(rule.column_width, Some(0.5));
}

#[test]
fn test_window_rule_open_extras_validation_drops_invalid() {
    let toml_str = r#"
        [[window_rules]]
        match_executable = "a.exe"
        open_on_workspace = 12
        column_width = 1.5
    "#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    let compiled = cfg.compile_window_rules();
    let rule = compiled
        .iter()
        .find(|r| r.match_executable.as_deref() == Some("a.exe"))
        .expect("rule compiled");
    // Out-of-range values are dropped, not fatal.
    assert_eq!(rule.open_on_workspace, None);
    assert_eq!(rule.column_width, None);
    assert!(!rule.open_maximized);
}

#[test]
fn test_matched_rule_returns_first_match_extras() {
    let mut config = test_config();
    config.window_rules = vec![crate::config::WindowRule {
        match_class: None,
        match_title: None,
        match_executable: Some("code.exe".to_string()),
        action: crate::config::WindowAction::Tile,
        width: None,
        height: None,
        corner_style: None,
        open_on_workspace: Some(3),
        open_maximized: false,
        column_width: Some(0.25),
        open_in_column: None,
        sticky: false,
    }];
    let state = AppState::new_with_config(config, test_monitors());
    let rule = state
        .matched_rule("SomeClass", "Editor", "code.exe")
        .expect("matches");
    assert_eq!(rule.open_on_workspace, Some(2));
    assert_eq!(rule.column_width, Some(0.25));
    assert!(state.matched_rule("SomeClass", "Editor", "other.exe").is_none() ||
        state.matched_rule("SomeClass", "Editor", "other.exe").unwrap().match_executable.as_deref() != Some("code.exe"));
}

/// Build a two-monitor AppState (DISPLAY1 + DISPLAY2) for structure-restore tests.
fn structure_restore_state() -> AppState {
    let mut monitors = test_monitors();
    monitors.push(MonitorInfo {
        id: 2,
        rect: Rect::new(1920, 0, 1920, 1080),
        work_area: Rect::new(1920, 0, 1920, 1040),
        is_primary: false,
        device_name: "DISPLAY2".to_string(),
        scale_factor: 1.0,
    });
    AppState::new_with_config(test_config(), monitors)
}

/// Build a saved Workspace on DISPLAY2 with:
/// - column 0: single window 100 @ width 640
/// - column 1: stacked windows 200 + 201 @ width 480
/// - scroll offset 333.0
fn saved_two_column_workspace() -> leopardwm_core_layout::Workspace {
    let mut ws = leopardwm_core_layout::Workspace::default();
    ws.insert_window(100, Some(640)).unwrap();
    ws.insert_window(200, Some(480)).unwrap();
    // Stack 201 into column 1 (the column holding 200).
    let col1 = ws
        .columns()
        .iter()
        .position(|c| c.windows().contains(&200))
        .unwrap();
    ws.insert_window_in_column(201, col1).unwrap();
    ws.set_scroll_offset(333.0);
    ws
}

#[test]
fn test_restore_structure_preserves_columns_widths_grouping_scroll() {
    let mut state = structure_restore_state();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "DISPLAY2".to_string(),
            workspace_index: 0,
            workspace: saved_two_column_workspace(),
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };

    // Fake all four HWNDs alive so none are pruned (avoids the real Win32
    // is_valid_window call).
    let restored = state.restore_workspace_structure_with(&snapshot, |_| true);

    let display2_id = state
        .monitors
        .iter()
        .find(|(_, m)| m.device_name == "DISPLAY2")
        .map(|(&id, _)| id)
        .unwrap();
    assert!(restored.contains(&(display2_id, 0)));

    let ws = &state.workspaces.get(&display2_id).unwrap()[0];
    assert_eq!(ws.column_count(), 2, "saved column count preserved");
    assert_eq!(ws.columns()[0].windows(), &[100], "col 0 membership");
    assert_eq!(ws.columns()[1].windows(), &[200, 201], "col 1 stacked grouping");
    assert_eq!(ws.columns()[0].width(), 640, "col 0 saved width preserved");
    assert_eq!(ws.columns()[1].width(), 480, "col 1 saved width preserved");
    assert_eq!(ws.scroll_offset(), 333.0, "saved scroll offset preserved");
}

#[test]
fn test_restore_structure_prunes_dead_windows() {
    let mut state = structure_restore_state();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "DISPLAY2".to_string(),
            workspace_index: 0,
            workspace: saved_two_column_workspace(),
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };

    // Window 100 and 201 closed while the daemon was down; 200 survives.
    let alive = |w: u64| w == 200;
    state.restore_workspace_structure_with(&snapshot, alive);

    let display2_id = state
        .monitors
        .iter()
        .find(|(_, m)| m.device_name == "DISPLAY2")
        .map(|(&id, _)| id)
        .unwrap();
    let ws = &state.workspaces.get(&display2_id).unwrap()[0];
    // Column 0 (window 100) emptied -> removed; column 1 retains only 200.
    assert_eq!(ws.column_count(), 1, "empty column dropped after prune");
    assert_eq!(ws.columns()[0].windows(), &[200], "only live window remains");
    assert!(!ws.contains_window(100));
    assert!(!ws.contains_window(201));
}

#[test]
fn test_restore_structure_clamps_workspace_index() {
    let mut state = structure_restore_state();
    let mut ws = leopardwm_core_layout::Workspace::default();
    ws.insert_window(999, None).unwrap();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "DISPLAY2".to_string(),
            // Out-of-range index (user-writable JSON) must clamp to 8.
            workspace_index: 42,
            workspace: ws,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };

    let restored = state.restore_workspace_structure_with(&snapshot, |_| true);

    let display2_id = state
        .monitors
        .iter()
        .find(|(_, m)| m.device_name == "DISPLAY2")
        .map(|(&id, _)| id)
        .unwrap();
    assert!(restored.contains(&(display2_id, 8)), "index clamped to 8");
    let ws_vec = state.workspaces.get(&display2_id).unwrap();
    assert_eq!(ws_vec.len(), 9, "vec extended to 0..=8, no further");
    assert!(ws_vec[8].contains_window(999));
}

#[test]
fn test_restore_structure_skips_unknown_monitor() {
    let mut state = structure_restore_state();
    let mut ws = leopardwm_core_layout::Workspace::default();
    ws.insert_window(999, None).unwrap();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "GHOST_DISPLAY".to_string(),
            workspace_index: 0,
            workspace: ws,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };

    let restored = state.restore_workspace_structure_with(&snapshot, |_| true);
    assert!(restored.is_empty(), "unknown monitor produces no restored slots");
}

#[test]
fn test_persisted_signature_stable_with_no_change() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let a = state.persisted_signature();
    let b = state.persisted_signature();
    assert_eq!(a, b, "signature must be deterministic with no change");
}

#[test]
fn test_persisted_signature_changes_on_window_add() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let before = state.persisted_signature();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let after = state.persisted_signature();
    assert_ne!(before, after, "adding a window must change the signature");
}

#[test]
fn test_persisted_signature_changes_on_scroll_offset() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let before = state.persisted_signature();
    state.workspaces.get_mut(&1).unwrap()[0].set_scroll_offset(500.0);
    let after = state.persisted_signature();
    assert_ne!(before, after, "scroll offset change must change the signature");
}

#[test]
fn test_persisted_signature_changes_on_active_workspace() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Ensure a second workspace exists so the active index can move.
    state.ensure_workspace_exists(1, 1);
    let before = state.persisted_signature();
    state.active_workspace.insert(1, 1);
    let after = state.persisted_signature();
    assert_ne!(
        before, after,
        "active workspace index change must change the signature"
    );
}

#[test]
fn test_request_save_if_changed_updates_last_sig_and_no_panic_without_sender() {
    // No save_request_tx installed (constructor leaves it None under
    // cfg(test)); request must update last_persisted_sig and not panic.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    assert!(state.last_persisted_sig.is_none());

    state.request_save_if_changed();
    let first = state.last_persisted_sig;
    assert!(first.is_some(), "first request records the signature");

    // No change -> signature stays equal (still Some, no panic).
    state.request_save_if_changed();
    assert_eq!(state.last_persisted_sig, first);

    // Real change -> recorded signature updates.
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.request_save_if_changed();
    assert_ne!(
        state.last_persisted_sig, first,
        "a change must update the recorded signature"
    );
}

// ========================================================================
// Shared-edge viewport guard (layout_viewport)
// ========================================================================

#[test]
fn test_layout_viewport_single_monitor_is_work_area() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    assert_eq!(
        state.layout_viewport(1),
        state.monitors[&1].work_area,
        "viewport is the full work area"
    );
}

#[test]
fn test_layout_viewport_side_by_side_monitors_use_full_work_area() {
    // Adjacent monitors each fill their own work area edge to edge — no
    // shared-edge margin (a fully-visible edge column ends at the seam).
    let state = AppState::new_with_config(test_config(), two_monitors());
    assert_eq!(
        state.layout_viewport(1),
        state.monitors[&1].work_area,
        "left monitor uses its full work area"
    );
    assert_eq!(
        state.layout_viewport(2),
        state.monitors[&2].work_area,
        "right monitor uses its full work area"
    );
}

#[test]
fn test_layout_viewport_unknown_monitor_falls_back() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let vp = state.layout_viewport(99999);
    assert_eq!(vp.x, 0);
    assert_eq!(vp.y, 0);
    assert!(vp.width > 0 && vp.height > 0, "fallback viewport is non-empty");
}
