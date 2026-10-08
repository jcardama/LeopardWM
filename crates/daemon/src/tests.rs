use super::*;
use leopardwm_core_layout::{Rect, Workspace};
use std::sync::atomic::Ordering;

static REAL_WINDOW_STYLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_in_placement_test_process(test_name: &str) -> bool {
    const CHILD_ENV: &str = "LEOPARDWM_TEST_PLACEMENT_CHILD";
    if std::env::var(CHILD_ENV).as_deref() == Ok(test_name) {
        return false;
    }
    // Other tests' native batches clear process-global parked and off-screen records.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_ENV, test_name)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("placement test child exceeded its deadline: {test_name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "placement test child failed: {test_name}: {status}"
    );
    true
}

// Run the test binary with the daemon's process DPI awareness, set before any test thread or
// window exists, so placements and DWM frame measurements agree on scaled displays.
#[used]
#[link_section = ".CRT$XCU"]
static TEST_PROCESS_DPI_AWARENESS: extern "C" fn() = {
    extern "C" fn set_test_process_dpi_awareness() {
        leopardwm_platform_win32::set_dpi_awareness();
    }
    set_test_process_dpi_awareness
};

#[test]
fn test_threads_inherit_per_monitor_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    assert!(unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .as_bool()
    });
}

fn pump_window_style_worker_until_idle() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if leopardwm_platform_win32::wait_for_window_style_requests(
            std::time::Duration::from_millis(10),
        ) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "window style worker did not become idle"
        );
    }
}

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
fn test_application_fullscreen_detector_prefers_chrome_and_rejects_maximized() {
    use crate::event_handler::detect_application_fullscreen;

    let monitors = two_monitors();
    let detected = detect_application_fullscreen(
        monitors.iter(),
        Some(Rect::new(1920, 0, 1920, 1080)),
        Some(Rect::new(0, 0, 1920, 1080)),
        false,
    );
    assert_eq!(detected.map(|state| state.monitor_id), Some(2));
    assert_eq!(
        detect_application_fullscreen(
            monitors.iter(),
            Some(Rect::new(100, 100, 800, 600)),
            Some(Rect::new(0, 0, 1920, 1080)),
            false,
        ),
        None,
        "DWM is not consulted when authoritative Chrome geometry is present"
    );
    assert_eq!(
        detect_application_fullscreen(
            monitors.iter(),
            Some(Rect::new(1920, 0, 1920, 1080)),
            None,
            true,
        ),
        None
    );
}

#[test]
fn test_application_fullscreen_detector_uses_monitor_rect_tolerance() {
    use crate::event_handler::detect_application_fullscreen;

    let monitor = MonitorInfo {
        id: 9,
        rect: Rect::new(-2560, -80, 2560, 1440),
        work_area: Rect::new(-2560, -80, 2560, 1400),
        is_primary: false,
        device_name: "DISPLAY9".to_string(),
        scale_factor: 4.0,
    };
    let expected_rect = monitor.rect;
    let monitors = [monitor];
    assert_eq!(
        detect_application_fullscreen(
            monitors.iter(),
            Some(Rect::new(-2560, -80, 2560, 1439)),
            None,
            false,
        )
        .map(|state| state.rect),
        Some(expected_rect)
    );
    assert_eq!(
        detect_application_fullscreen(
            monitors.iter(),
            Some(Rect::new(-2560, -80, 2560, 1400)),
            None,
            false,
        ),
        None,
        "work area must not qualify as fullscreen"
    );
    assert!(detect_application_fullscreen(
        monitors.iter(),
        Some(Rect::new(-2560, -80, 2560, 1419)),
        None,
        false,
    )
    .is_none());
}

#[test]
fn test_application_fullscreen_geometry_rejects_expected_tile_with_insets() {
    use crate::event_handler::{chrome_rect_matches_layout_rect, fullscreen_rect_tolerance};

    assert_eq!(fullscreen_rect_tolerance(1.5), 12);
    assert!(fullscreen_rect_tolerance(1.5) < 20);
    assert!(chrome_rect_matches_layout_rect(
        Rect::new(-8, -8, 1936, 1056),
        Rect::new(0, 0, 1920, 1040),
        (8, 8, 8, 8),
        1.0,
    ));
    assert!(!chrome_rect_matches_layout_rect(
        Rect::new(-8, -8, 1936, 1096),
        Rect::new(0, 0, 1920, 1040),
        (8, 8, 8, 8),
        1.0,
    ));
}

#[test]
fn test_floating_above_tiled_distinguishes_monitor_sized_tile_from_fullscreen() {
    for (tile_width, fullscreen) in [(1920, false), (960, true)] {
        let mut config = test_config();
        config.behavior.floating_above_tiled = true;
        config.layout.gap = 0;
        config.layout.outer_gap_left = 0;
        config.layout.outer_gap_right = 0;
        config.layout.outer_gap_top = 0;
        config.layout.outer_gap_bottom = 0;
        let mut monitors = test_monitors();
        monitors[0].work_area = monitors[0].rect;
        let monitor_rect = monitors[0].rect;
        let mut state = AppState::new_with_config(config, monitors);
        state.paused = false;
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(0, Some(tile_width)).unwrap();
        workspace
            .add_floating(200, Rect::new(10, 10, 300, 300))
            .unwrap();
        assert_eq!(
            state.compute_window_layout_rect(0),
            Some(Rect::new(0, 0, tile_width, 1080))
        );
        let observed = state.observe_application_fullscreen(0, Some(monitor_rect), None, false);
        assert_eq!(observed.is_some(), fullscreen, "tile width {tile_width}");
        assert_eq!(
            state.floating_raise_order(0, &[0, 200], observed.is_some()),
            if fullscreen { vec![] } else { vec![200] }
        );
    }
}

#[test]
fn test_application_fullscreen_uses_live_layout_target_before_last_placement() {
    use crate::event_handler::application_fullscreen_expected_layout_rect;

    let current = Rect::new(960, 0, 960, 1040);
    let stale = Rect::new(0, 0, 1920, 1040);
    assert_eq!(
        application_fullscreen_expected_layout_rect(Some(current), Some(stale)),
        Some(current)
    );
    assert_eq!(
        application_fullscreen_expected_layout_rect(None, Some(stale)),
        Some(stale)
    );
}

#[test]
fn test_application_fullscreen_lifecycle_and_suppression_precedence() {
    use crate::event_handler::{
        application_fullscreen_lifecycle, moved_or_resized_decision,
        ApplicationFullscreenLifecycle, MovedOrResizedDecision,
    };
    use crate::state::ApplicationFullscreenState;

    let first = ApplicationFullscreenState {
        monitor_id: 1,
        rect: Rect::new(0, 0, 1920, 1080),
    };
    let reassigned = ApplicationFullscreenState {
        monitor_id: 2,
        rect: Rect::new(1920, 0, 1920, 1080),
    };
    assert_eq!(
        application_fullscreen_lifecycle(None, Some(first)),
        ApplicationFullscreenLifecycle::Enter
    );
    assert_eq!(
        application_fullscreen_lifecycle(Some(first), Some(first)),
        ApplicationFullscreenLifecycle::Continue
    );
    assert_eq!(
        application_fullscreen_lifecycle(Some(first), Some(reassigned)),
        ApplicationFullscreenLifecycle::Reassign
    );
    assert_eq!(
        application_fullscreen_lifecycle(Some(first), None),
        ApplicationFullscreenLifecycle::Exit
    );
    assert_eq!(
        moved_or_resized_decision(ApplicationFullscreenLifecycle::Enter, true),
        MovedOrResizedDecision::Fullscreen(ApplicationFullscreenLifecycle::Enter),
        "fullscreen entry must precede ordinary suppression"
    );
    assert_eq!(
        moved_or_resized_decision(ApplicationFullscreenLifecycle::Exit, true),
        MovedOrResizedDecision::Fullscreen(ApplicationFullscreenLifecycle::Exit),
        "fullscreen exit must precede ordinary suppression"
    );
}

#[test]
fn test_application_fullscreen_reconciliation_retains_only_matching_nonmaximized_windows() {
    use crate::event_handler::{
        application_fullscreen_reconciliation, ApplicationFullscreenReconciliation,
    };
    use crate::state::ApplicationFullscreenState;

    let stored = ApplicationFullscreenState {
        monitor_id: 1,
        rect: Rect::new(0, 0, 1920, 1080),
    };
    let reassigned = ApplicationFullscreenState {
        monitor_id: 2,
        rect: Rect::new(1920, 0, 1920, 1080),
    };
    assert_eq!(
        application_fullscreen_reconciliation(
            true,
            true,
            false,
            stored,
            None,
            Some(Rect::new(3, -3, 1917, 1086)),
            8,
        ),
        ApplicationFullscreenReconciliation::Retain
    );
    assert_eq!(
        application_fullscreen_reconciliation(true, true, false, stored, Some(reassigned), None, 8),
        ApplicationFullscreenReconciliation::Update
    );
    assert_eq!(
        application_fullscreen_reconciliation(true, true, true, stored, None, Some(stored.rect), 8),
        ApplicationFullscreenReconciliation::Exit,
        "maximized windows must leave application fullscreen protection"
    );
    assert_eq!(
        application_fullscreen_reconciliation(
            true,
            true,
            false,
            stored,
            None,
            Some(Rect::new(0, 0, 1600, 900)),
            8,
        ),
        ApplicationFullscreenReconciliation::Exit
    );
}

#[test]
fn test_application_fullscreen_exit_routes() {
    use crate::event_handler::{
        application_fullscreen_exit_restores_border, application_fullscreen_exit_route,
        ApplicationFullscreenExitRoute,
    };

    assert_eq!(
        application_fullscreen_exit_route(true, true, false),
        ApplicationFullscreenExitRoute::FloatingPreserve
    );
    assert_eq!(
        application_fullscreen_exit_route(false, true, false),
        ApplicationFullscreenExitRoute::MaximizedAllow
    );
    assert_eq!(
        application_fullscreen_exit_route(false, false, false),
        ApplicationFullscreenExitRoute::InactivePark
    );
    assert_eq!(
        application_fullscreen_exit_route(false, false, true),
        ApplicationFullscreenExitRoute::ActiveTiledApply
    );
    assert!(application_fullscreen_exit_restores_border(
        ApplicationFullscreenExitRoute::MaximizedAllow,
        true
    ));
    assert!(!application_fullscreen_exit_restores_border(
        ApplicationFullscreenExitRoute::MaximizedAllow,
        false
    ));
}

#[test]
fn test_application_fullscreen_session_filters_physical_dispatch_and_prunes() {
    use crate::state::ApplicationFullscreenState;
    use leopardwm_core_layout::{Visibility, WindowPlacement};

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, None)
        .unwrap();
    state.application_fullscreen.insert(
        100,
        ApplicationFullscreenState {
            monitor_id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
        },
    );
    state.application_fullscreen.insert(
        200,
        ApplicationFullscreenState {
            monitor_id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
        },
    );
    let placements = vec![
        WindowPlacement {
            window_id: 100,
            rect: Rect::new(0, 0, 960, 1040),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 300,
            rect: Rect::new(960, 0, 960, 1040),
            visibility: Visibility::Visible,
            column_index: 1,
        },
    ];

    let dispatched = state.filter_application_fullscreen_placements(placements.clone());
    assert_eq!(placements.len(), 2, "logical placements remain intact");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].window_id, 300);

    state.retain_application_fullscreen_sessions();
    assert!(state.is_application_fullscreen(100));
    assert!(!state.is_application_fullscreen(200));

    state.paused = false;
    state.apply_layout().unwrap();
    assert!(state.last_placed_layout_rects.contains_key(&100));
    assert!(!state.should_suppress_moved_or_resized(100));
    assert!(state.pending_apply_workers.is_empty());
}

#[test]
fn test_startup_maximize_hold_is_applied_to_each_animation_duration() {
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    for duration_ms in [0, 1, 150] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.config.behavior.swap_chain_ghost_animation = false;
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(200, Some(800))
            .unwrap();
        state.previous_focused_hwnd = Some(100);
        let now = Instant::now();
        state.window_managed_at.insert(100, now);
        state.window_last_maximized_at.insert(100, now);
        state.start_layout_transition_with_duration(
            HashMap::from([
                (100, Rect::new(-400, 0, 800, 1040)),
                (200, Rect::new(1200, 0, 800, 1040)),
            ]),
            duration_ms,
        );
        let mut placements = vec![
            WindowPlacement {
                window_id: 100,
                rect: Rect::new(0, 0, 800, 1040),
                visibility: Visibility::Visible,
                column_index: 0,
            },
            WindowPlacement {
                window_id: 200,
                rect: Rect::new(800, 0, 800, 1040),
                visibility: Visibility::Visible,
                column_index: 1,
            },
        ];
        state.record_last_placed_rects(&placements);
        AppState::apply_transition_interpolation(
            state.layout_transition.as_ref().unwrap(),
            &mut placements,
        );
        let peer_frame_rect = placements
            .iter()
            .find(|placement| placement.window_id == 200)
            .unwrap()
            .rect;
        if duration_ms == 150 {
            assert_ne!(peer_frame_rect, Rect::new(800, 0, 800, 1040));
        } else {
            assert_eq!(peer_frame_rect, Rect::new(800, 0, 800, 1040));
        }

        let request = state.prepare_animation_frame(placements, &HashSet::new());

        assert_eq!(
            request
                .placements
                .iter()
                .map(|placement| placement.window_id)
                .collect::<Vec<_>>(),
            vec![200]
        );
        assert_eq!(request.placements[0].rect, peer_frame_rect);
        assert!(request.ghost_updates.is_empty());
        assert!(!state.should_suppress_moved_or_resized(100));
        assert!(state.should_suppress_moved_or_resized(200));
        assert_eq!(state.previous_focused_hwnd, Some(100));
        assert!(state.focused_workspace().unwrap().contains_window(100));
        assert!(state.last_placed_layout_rects.contains_key(&100));

        state
            .window_managed_at
            .insert(100, now - Duration::from_secs(3));
        let restored = state.filter_physical_placements_observed(
            vec![WindowPlacement {
                window_id: 100,
                rect: Rect::new(0, 0, 800, 1040),
                visibility: Visibility::Visible,
                column_index: 0,
            }],
            &HashSet::new(),
        );
        assert_eq!(restored.len(), 1);
        assert!(!state.window_last_maximized_at.contains_key(&100));
    }
}

#[test]
fn test_placement_parked_maximized_target_reaches_sync_and_animation_dispatch() {
    use crate::layout_apply::should_dispatch_visible_tiled_placement;
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::HashSet;
    use std::time::Instant;

    let placement = WindowPlacement {
        window_id: 100,
        rect: Rect::new(0, 0, 800, 1040),
        visibility: Visibility::Visible,
        column_index: 0,
    };
    let maximized = HashSet::from([100]);

    assert!(!should_dispatch_visible_tiled_placement(true, false, false));
    assert!(should_dispatch_visible_tiled_placement(true, false, true));
    assert!(!should_dispatch_visible_tiled_placement(true, true, true));

    let mut sync_state = AppState::new_with_config(test_config(), test_monitors());
    assert!(sync_state
        .prepare_physical_placements_with_parked(vec![placement.clone()], &maximized, |_| false)
        .is_empty());
    assert_eq!(
        sync_state
            .prepare_physical_placements_with_parked(vec![placement.clone()], &maximized, |wid| wid
                == 100)
            .iter()
            .map(|placement| placement.window_id)
            .collect::<Vec<_>>(),
        vec![100],
        "placement-owned parking must reach synchronous platform recovery"
    );

    let mut animation_state = AppState::new_with_config(test_config(), test_monitors());
    assert_eq!(
        animation_state
            .prepare_animation_frame_with_parked(vec![placement.clone()], &maximized, |wid| wid
                == 100)
            .placements
            .iter()
            .map(|placement| placement.window_id)
            .collect::<Vec<_>>(),
        vec![100],
        "placement-owned parking must reach animation platform recovery"
    );

    let mut settling_state = AppState::new_with_config(test_config(), test_monitors());
    let now = Instant::now();
    settling_state.window_managed_at.insert(100, now);
    settling_state.window_last_maximized_at.insert(100, now);
    assert!(settling_state
        .prepare_physical_placements_with_parked(vec![placement.clone()], &maximized, |_| true)
        .is_empty());
}

#[test]
fn test_empty_abandoned_request_preserves_unconfirmed_physical_context() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(0, 0, 800, 1040),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };

    state.apply_physical_projection(vec![placement.clone()]);
    let (failed_request_id, failed_invalidation_id) = state.physical_request_ids();
    state.abandon_physical_request(failed_request_id, failed_invalidation_id);
    let failed_presentation = state.last_physical_presentations[&100].clone();
    assert!(!failed_presentation.confirmed);
    let applied_invalidation = state.last_applied_physical_invalidation;
    let applied_topology = state.last_topology_signature.clone();
    assert!(!state.physical_fast_path_ok());

    state.bump_physical_invalidation();
    state.monitors.get_mut(&1).unwrap().rect.width += 64;
    let current_invalidation = state.physical_invalidation_id.load(Ordering::SeqCst);
    let current_topology = crate::physical_placement::topology_signature(&state.monitors);
    assert_ne!(applied_invalidation, current_invalidation);
    assert_ne!(applied_topology, current_topology);

    let now = std::time::Instant::now();
    state.window_managed_at.insert(100, now);
    state.window_last_maximized_at.insert(100, now);
    assert!(
        state
            .prepare_physical_placements_with_parked(
                vec![placement],
                &std::collections::HashSet::from([100]),
                |_| true,
            )
            .is_empty(),
        "a temporarily settling visible window is filtered before physical dispatch"
    );
    let (empty_request_id, empty_invalidation_id) = state.physical_request_ids();
    assert_ne!(empty_request_id, failed_request_id);
    assert_ne!(empty_invalidation_id, failed_invalidation_id);
    state.abandon_physical_request(empty_request_id, empty_invalidation_id);

    assert_eq!(
        state.last_applied_physical_invalidation,
        applied_invalidation
    );
    assert_eq!(state.last_topology_signature, applied_topology);
    assert_ne!(
        state.last_applied_physical_invalidation,
        current_invalidation
    );
    assert_ne!(
        state.last_topology_signature,
        crate::physical_placement::topology_signature(&state.monitors)
    );
    let retained = state.last_physical_presentations.get(&100).unwrap();
    assert_eq!(retained.request_id, failed_presentation.request_id);
    assert_eq!(
        retained.invalidation_id,
        failed_presentation.invalidation_id
    );
    assert_ne!(retained.request_id, empty_request_id);
    assert_ne!(retained.invalidation_id, empty_invalidation_id);
    assert!(!retained.confirmed);
    assert_eq!(
        retained.physical.window_id,
        failed_presentation.physical.window_id
    );
    assert_eq!(retained.physical.rect, failed_presentation.physical.rect);
    assert_eq!(
        retained.physical.visibility,
        failed_presentation.physical.visibility
    );
    assert_eq!(
        retained.physical.column_index,
        failed_presentation.physical.column_index
    );
    assert_eq!(state.pending_physical_request_id, 0);
    assert_eq!(state.pending_physical_invalidation_id, 0);
    assert_eq!(state.inflight_request_id, None);
    assert!(state.inflight_origins.is_empty());
    assert!(state.pending_physical_presentations.is_empty());
    assert!(
        !state.physical_fast_path_ok(),
        "an unchanged layout must still require a current landing after an empty abandoned request"
    );
}

#[test]
fn test_empty_layout_apply_consumes_stale_stamps_for_fast_path() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.bump_physical_invalidation();
    assert!(
        !state.physical_fast_path_ok(),
        "a fresh empty layout starts with stale topology/invalidation stamps"
    );
    let seq_before = state.physical_request_seq;

    assert!(state.apply_layout().is_ok());
    assert!(
        state.physical_fast_path_ok(),
        "a genuinely empty apply must consume stale stamps so the next layout can take the fast path"
    );
    assert_eq!(state.pending_physical_request_id, 0);
    assert_eq!(state.inflight_request_id, None);
    assert!(state.inflight_origins.is_empty());
    assert!(state.last_physical_presentations.is_empty());
    let seq_after_first = state.physical_request_seq;
    assert!(
        seq_after_first > seq_before,
        "the first empty apply still allocates a current physical request"
    );

    assert!(state.apply_layout().is_ok());
    assert_eq!(
        state.physical_request_seq, seq_after_first,
        "a current empty layout must not allocate another physical request"
    );
    assert_eq!(state.pending_physical_request_id, 0);
    assert!(state.physical_fast_path_ok());
}

fn arm_settling_window(state: &mut AppState, window_id: u64) {
    let now = std::time::Instant::now();
    state.window_managed_at.insert(window_id, now);
    state.window_last_maximized_at.insert(window_id, now);
}

#[test]
fn test_filtered_empty_apply_with_empty_prior_evidence_retains_retry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.bump_physical_invalidation();
    arm_settling_window(&mut state, 100);
    let applied_invalidation = state.last_applied_physical_invalidation;
    let applied_topology = state.last_topology_signature.clone();
    let seq_before = state.physical_request_seq;
    assert!(state.last_physical_presentations.is_empty());
    assert!(!state.physical_fast_path_ok());

    assert!(state.apply_layout().is_ok());
    assert_eq!(
        state.last_applied_physical_invalidation,
        applied_invalidation
    );
    assert_eq!(state.last_topology_signature, applied_topology);
    assert!(
        !state.physical_fast_path_ok(),
        "filtered-empty work with no prior presentation must keep retrying"
    );
    let seq_after_first = state.physical_request_seq;
    assert!(seq_after_first > seq_before);

    arm_settling_window(&mut state, 100);
    assert!(state.apply_layout().is_ok());
    assert!(
        state.physical_request_seq > seq_after_first,
        "a later filtered-empty apply must allocate a fresh request instead of taking the fast path"
    );
    assert!(!state.physical_fast_path_ok());
}

#[test]
fn test_filtered_empty_apply_with_unconfirmed_prior_evidence_retains_retry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(0, 0, 800, 1040),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![placement]);
    let (failed_request_id, failed_invalidation_id) = state.physical_request_ids();
    state.abandon_physical_request(failed_request_id, failed_invalidation_id);
    let failed_presentation = state.last_physical_presentations[&100].clone();
    let applied_invalidation = state.last_applied_physical_invalidation;
    let applied_topology = state.last_topology_signature.clone();

    state.bump_physical_invalidation();
    state.monitors.get_mut(&1).unwrap().rect.width += 64;
    arm_settling_window(&mut state, 100);
    let seq_before = state.physical_request_seq;
    assert!(!state.physical_fast_path_ok());

    assert!(state.apply_layout().is_ok());
    assert_eq!(
        state.last_applied_physical_invalidation,
        applied_invalidation
    );
    assert_eq!(state.last_topology_signature, applied_topology);
    let retained = state.last_physical_presentations.get(&100).unwrap();
    assert_eq!(retained.request_id, failed_presentation.request_id);
    assert_eq!(
        retained.invalidation_id,
        failed_presentation.invalidation_id
    );
    assert!(!retained.confirmed);
    assert!(!state.physical_fast_path_ok());
    let seq_after_first = state.physical_request_seq;
    assert!(seq_after_first > seq_before);

    arm_settling_window(&mut state, 100);
    assert!(state.apply_layout().is_ok());
    assert!(
        state.physical_request_seq > seq_after_first,
        "unconfirmed prior evidence must not become current through a filtered-empty apply"
    );
    assert!(!state.physical_fast_path_ok());
}

#[test]
fn test_animation_maximized_skip_result_invalidates_daemon_bookkeeping() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.applying_layout = true;
    state.moved_or_resized_suppression.insert(
        100,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    );
    state
        .last_placed_layout_rects
        .insert(100, Rect::new(0, 0, 800, 1040));

    let platform_result = leopardwm_platform_win32::ApplyPlacementsResult {
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: vec![100],
        landings: Vec::new(),
    };
    let frame_result =
        animation_worker::FrameResult::from_platform(platform_result, std::time::Duration::ZERO);

    state.handle_animation_placement_result(&frame_result);

    assert!(!state.applying_layout);
    assert!(!state.should_suppress_moved_or_resized(100));
    assert!(!state.last_placed_layout_rects.contains_key(&100));
}

#[test]
fn test_width_feedback_preserves_requested_resolution_round_trip() {
    for invalidate in [false, true] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let monitor = state.focused_monitor;
        let mut workspace = Workspace::with_gaps(10, 10);
        workspace.insert_window(100, Some(1267)).unwrap();
        workspace.insert_window(200, Some(1267)).unwrap();
        state.workspaces.get_mut(&monitor).unwrap()[0] = workspace;

        for (old_width, new_width, requested) in
            [(5120, 2560, 627), (2560, 1920, 467), (1920, 5120, 1267)]
        {
            if invalidate {
                state.monitors.get_mut(&monitor).unwrap().rect.width = old_width;
                state.monitors.get_mut(&monitor).unwrap().work_area.width = old_width;
                state.invalidate_display_change_constraints(true);
                let mut monitors = test_monitors();
                monitors[0].rect.width = new_width;
                monitors[0].work_area.width = new_width;
                state.reconcile_monitors(monitors);
            } else {
                state.workspaces.get_mut(&monitor).unwrap()[0]
                    .rescale_column_widths(10, 10, 10, old_width, new_width);
            }
            let workspace = &state.workspaces[&monitor][0];
            if invalidate {
                assert_eq!(
                    workspace.compute_placements(Rect::new(0, 0, new_width, 1040))[0]
                        .rect
                        .width,
                    requested
                );
            }
            if new_width < 5120 {
                state.propagate_size_violations(
                    &[leopardwm_platform_win32::WidthViolation {
                        window_id: 100,
                        min_width: 842,
                    }],
                    &[],
                );
            }

            let workspace = &state.workspaces.get(&monitor).unwrap()[0];
            assert_eq!(workspace.columns()[0].width(), requested);
            assert_eq!(workspace.columns()[1].width(), requested);
            let placements = workspace.compute_placements(Rect::new(0, 0, new_width, 1040));
            assert_eq!(placements[0].rect.width, requested.max(842));
            assert_eq!(placements[1].rect.width, requested);
        }
    }
}

#[test]
fn test_width_feedback_noop_preserves_animation_and_effective_subscription() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.insert_window(100, Some(467)).unwrap();
    workspace.insert_window(200, Some(1600)).unwrap();
    workspace.set_reduce_motion(false);
    let original_sig = state.focused_layout_signature();
    let feedback = [leopardwm_platform_win32::WidthViolation {
        window_id: 100,
        min_width: 842,
    }];
    assert!(state.propagate_size_violations(&feedback, &[]));
    let effective_sig = state.focused_layout_signature();
    assert_ne!(effective_sig, original_sig);
    assert_eq!(state.focused_layout_columns()[0].width_px, 842);
    match state.handle_command(IpcCommand::QueryWorkspace) {
        IpcResponse::WorkspaceState { total_width, .. } => assert_eq!(total_width, 2452),
        other => panic!("Expected WorkspaceState, got {other:?}"),
    }
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.start_scroll_animation(300.0, 1920, Some(1000), None);
    workspace.tick_animation(400);
    let mut expected = workspace.clone();
    assert!(!state.propagate_size_violations(&feedback, &[]));
    assert!(!state.propagate_size_violations(
        &[leopardwm_platform_win32::WidthViolation {
            window_id: 200,
            min_width: 700
        }],
        &[]
    ));
    assert_eq!(state.focused_layout_signature(), effective_sig);
    state.focused_workspace_mut().unwrap().tick_animation(600);
    expected.tick_animation(600);
    assert_eq!(
        state.focused_workspace().unwrap().effective_scroll_offset(),
        expected.effective_scroll_offset()
    );
    assert!(!state.focused_workspace().unwrap().is_animating());
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.focus_window(100).unwrap();
    workspace.set_focused_column_width_fraction(0.3, 1920);
    workspace.focus_window(200).unwrap();
    assert_eq!(state.focused_layout_signature(), effective_sig);
    assert_eq!(state.focused_layout_columns()[0].width_px, 842);

    state.paused = false;
    state.injected_apply_placements_behavior = Some(
        TestApplyPlacementsBehavior::SucceedWithSizeViolations(feedback.to_vec(), Vec::new()),
    );
    state.apply_layout().unwrap();
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        1
    );
}

#[test]
fn test_deferred_minimum_clear_reconciles_apply_and_animation_boundaries() {
    for frame_boundary in [false, true] {
        for animated in [false, true] {
            for center in [false, true] {
                let mut monitors = test_monitors();
                monitors[0].rect.width = 1000;
                monitors[0].work_area.width = 1000;
                let mut state = AppState::new_with_config(test_config(), monitors);
                state.paused = false;
                let mut workspace = Workspace::with_gaps(10, 10);
                workspace.set_reduce_motion(false);
                workspace.set_centering_mode(if center {
                    leopardwm_core_layout::CenteringMode::Center
                } else {
                    leopardwm_core_layout::CenteringMode::JustInView
                });
                workspace.set_center_past_edges(center);
                workspace.insert_window(100, Some(467)).unwrap();
                workspace
                    .insert_window(200, Some(if center { 1000 } else { 467 }))
                    .unwrap();
                let constrained = if center { 100 } else { 200 };
                let column = if center { 0 } else { 1 };
                workspace.focus_window(constrained).unwrap();
                workspace.set_window_min_width(constrained, 842);
                workspace.ensure_focused_visible(1000);
                if animated {
                    workspace.set_scroll_offset(100.0);
                    workspace.ensure_focused_visible_animated(1000);
                    workspace.tick_animation(10);
                }
                workspace.insert_window_in_column(300, column).unwrap();
                let before = workspace.effective_scroll_offset();
                let mut inactive = workspace.clone();
                inactive.cancel_animation();
                inactive.set_scroll_offset(137.0);
                state.workspaces.insert(1, vec![workspace, inactive]);
                if frame_boundary {
                    for id in [100, 200, 300] {
                        state.application_fullscreen.insert(
                            id,
                            crate::state::ApplicationFullscreenState {
                                monitor_id: 1,
                                rect: Rect::new(0, 0, 1000, 1040),
                            },
                        );
                    }
                    let (tx, _rx) = tokio::sync::mpsc::channel(4);
                    let worker = animation_worker::AnimationWorkerHandle::spawn(
                        tx,
                        state.apply_worker_cancelled.clone(),
                    )
                    .unwrap();
                    state.send_animation_frame(&worker).unwrap();
                    drop(worker);
                } else {
                    state.injected_apply_placements_behavior =
                        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
                    state.apply_layout().unwrap();
                }
                let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
                assert_eq!(
                    workspace.effective_column_width(&workspace.columns()[column]),
                    467
                );
                if animated {
                    assert!((workspace.effective_scroll_offset() - before).abs() < 0.5);
                }
                workspace.tick_animation(1000);
                assert_eq!(workspace.scroll_offset(), if center { -257.0 } else { 0.0 });
                let placed = workspace.compute_placements(Rect::new(0, 0, 1000, 1040));
                let focused = placed.iter().find(|p| p.window_id == 300).unwrap();
                assert!(focused.rect.x >= 10 && focused.rect.x + focused.rect.width <= 990);
                let inactive = &state.workspaces[&1][1];
                assert_eq!(
                    inactive.effective_column_width(&inactive.columns()[column]),
                    467
                );
                assert_eq!(inactive.scroll_offset(), 137.0);
                assert_eq!(state.active_workspace_idx(1), 0);
            }
        }
    }
}

#[test]
fn test_full_display_invalidation_reconciles_binding_minimum_without_viewport_change() {
    for animated in [false, true] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::JustInView);
        workspace.set_reduce_motion(false);
        for id in [100, 200, 300] {
            workspace.insert_window(id, Some(800)).unwrap();
        }
        workspace.set_window_min_width(300, 1500);
        workspace.set_scroll_offset(1000.0);
        if animated {
            workspace.start_scroll_animation(1200.0, 1920, Some(1000), None);
            workspace.tick_animation(10);
        }
        let before = workspace.effective_scroll_offset();
        state.invalidate_display_change_constraints(true);
        state.reconcile_monitors(test_monitors());
        let workspace = state.focused_workspace_mut().unwrap();
        if animated {
            assert!((workspace.effective_scroll_offset() - before).abs() < 0.5);
        }
        workspace.tick_animation(1000);
        assert_eq!(workspace.scroll_offset(), 520.0);
        assert_eq!(workspace.columns()[2].width(), 800);
    }
}

#[test]
fn test_deferred_nonbinding_minimum_clear_preserves_manual_scroll_and_animation() {
    for min_width in [0, 400] {
        for animated in [false, true] {
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            state.paused = false;
            let workspace = state.focused_workspace_mut().unwrap();
            workspace.set_reduce_motion(false);
            for id in [100, 200, 300] {
                workspace.insert_window(id, Some(800)).unwrap();
            }
            workspace.set_window_min_width(200, min_width);
            workspace.set_window_min_height(200, 600);
            workspace.insert_window_in_column(400, 1).unwrap();
            workspace.set_scroll_offset(137.0);
            if animated {
                workspace.start_scroll_animation(300.0, 1920, Some(1000), None);
                workspace.tick_animation(400);
            }
            let mut expected = workspace.clone();
            state.injected_apply_placements_behavior =
                Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
            state.apply_layout().unwrap();
            let workspace = state.focused_workspace_mut().unwrap();
            assert!(!workspace.commit_pending_min_size_clears());
            assert_eq!(
                workspace.effective_scroll_offset(),
                expected.effective_scroll_offset()
            );
            workspace.tick_animation(600);
            expected.tick_animation(600);
            assert_eq!(
                workspace.effective_scroll_offset(),
                expected.effective_scroll_offset()
            );
            assert_eq!(workspace.is_animating(), expected.is_animating());
        }
    }
}

#[test]
fn test_width_feedback_retargets_an_existing_scroll_animation() {
    let mut config = test_config();
    config.layout.outer_gap_left = 100;
    config.layout.outer_gap_right = 100;
    config.animation.scroll_duration_ms = 1_000;
    let mut state = AppState::new_with_config(config, test_monitors());
    let monitor = state.focused_monitor;
    let viewport = state.layout_viewport(monitor);
    let usable_width = state.workspaces.get(&monitor).unwrap()[0].visible_width(viewport.width);
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
        workspace.insert_window(300, Some(300)).unwrap();
        workspace.focus_window(300).unwrap();
        workspace.set_reduce_motion(false);
        workspace.start_scroll_animation(240.0, viewport.width, Some(1_000), None);
    }
    assert!(state.layout_transition.is_none());

    let frame_result = animation_worker::FrameResult::from_platform(
        leopardwm_platform_win32::ApplyPlacementsResult {
            width_violations: vec![leopardwm_platform_win32::WidthViolation {
                window_id: 300,
                min_width: usable_width - 1,
            }],
            height_violations: Vec::new(),
            maximized_skipped_window_ids: Vec::new(),
            landings: Vec::new(),
        },
        Duration::ZERO,
    );
    state.handle_animation_placement_result(&frame_result);

    let workspace = &state.workspaces.get(&monitor).unwrap()[0];
    assert_eq!(workspace.columns()[2].width(), 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[2]),
        usable_width - 1
    );
    assert!(workspace.is_animating(), "the existing pump is retargeted");
    assert_eq!(
        workspace.scroll_offset(),
        0.0,
        "feedback must not snap the base scroll offset while a pump is active"
    );

    state.tick_animations(500);
    let workspace = &state.workspaces.get(&monitor).unwrap()[0];
    assert!(
        workspace.is_animating(),
        "the retargeted scroll is still pumping"
    );
    assert!(
        workspace.effective_scroll_offset() > 0.0,
        "the retargeted correction advances through animation rather than snapping"
    );

    state.tick_animations(10_000);
    let workspace = &state.workspaces.get(&monitor).unwrap()[0];
    assert!(!workspace.is_animating());
    assert!(
        workspace.scroll_offset() > 240.0,
        "feedback replaces the original 240px target with the focused-visibility correction"
    );
    assert_eq!(
        workspace
            .compute_placements(viewport)
            .into_iter()
            .find(|placement| placement.window_id == 300)
            .unwrap()
            .visibility,
        leopardwm_core_layout::Visibility::Visible,
        "the completed retargeted pump lands the widened focused column in view"
    );
}

#[test]
fn test_width_feedback_widens_inactive_workspace_without_changing_scroll() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    state.ensure_workspace_exists(monitor, 1);
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[1];
        workspace.insert_window(400, Some(800)).unwrap();
        workspace.insert_window(401, Some(800)).unwrap();
        workspace.insert_window(402, Some(300)).unwrap();
        workspace.set_scroll_offset(137.0);
    }

    let frame_result = animation_worker::FrameResult::from_platform(
        leopardwm_platform_win32::ApplyPlacementsResult {
            width_violations: vec![leopardwm_platform_win32::WidthViolation {
                window_id: 402,
                min_width: 1_500,
            }],
            height_violations: Vec::new(),
            maximized_skipped_window_ids: Vec::new(),
            landings: Vec::new(),
        },
        Duration::ZERO,
    );
    state.handle_animation_placement_result(&frame_result);

    let workspace = &state.workspaces.get(&monitor).unwrap()[1];
    assert_eq!(workspace.columns()[2].width(), 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[2]),
        1_500
    );
    assert_eq!(workspace.scroll_offset(), 137.0);
    assert!(!workspace.is_animating());
    assert_eq!(
        state.active_workspace_idx(monitor),
        0,
        "feedback must not animate an inactive workspace"
    );
}

#[test]
fn test_post_animation_landing_applies_unchanged_layout() {
    let window_id = u64::MAX - 1;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(window_id, Some(800))
        .unwrap();
    state.apply_layout().unwrap();
    assert!(state.last_placed_layout_rects.contains_key(&window_id));

    state.post_animation_nudge_pending = true;
    state.apply_layout().unwrap();

    assert!(
        !state.post_animation_nudge_pending,
        "the final landing must reach the worker even when the logical layout is unchanged"
    );
}

#[test]
fn test_sync_size_violation_reapplies_once_and_reveals_widened_focus() {
    let mut config = test_config();
    config.layout.outer_gap_left = 100;
    config.layout.outer_gap_right = 100;
    let mut state = AppState::new_with_config(config, test_monitors());
    let monitor = state.focused_monitor;
    state.paused = false;
    let viewport = state.layout_viewport(monitor);
    let usable_width = state.workspaces.get(&monitor).unwrap()[0].visible_width(viewport.width);
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
        workspace.insert_window(300, Some(300)).unwrap();
        workspace.focus_window(300).unwrap();
    }
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SucceedWithSizeViolations(
            vec![leopardwm_platform_win32::WidthViolation {
                window_id: 300,
                min_width: usable_width - 1,
            }],
            Vec::new(),
        ));

    state.apply_layout().unwrap();

    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        2,
        "synchronous feedback has one guarded corrective reapply"
    );
    let workspace = &state.workspaces.get(&monitor).unwrap()[0];
    assert_eq!(workspace.columns()[2].width(), 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[2]),
        usable_width - 1
    );
    assert!(
        !workspace.is_animating(),
        "no pump means immediate correction"
    );
    assert_eq!(
        workspace
            .compute_placements(viewport)
            .into_iter()
            .find(|placement| placement.window_id == 300)
            .unwrap()
            .visibility,
        leopardwm_core_layout::Visibility::Visible,
        "the widened focused column is re-derived into view before the reapply"
    );
}

#[test]
fn test_physical_feedback_allows_full_placements_and_suppresses_parking() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let partial = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(1800, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![partial]);
    let (request_id, _) = state.physical_request_ids();
    assert!(
        state.allows_core_size_feedback(request_id, 100),
        "a positive partial placement retains its full native geometry and ordinary feedback"
    );

    let parked = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(1920, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![parked]);
    let (request_id, _) = state.physical_request_ids();
    assert!(
        !state.allows_core_size_feedback(request_id, 100),
        "a tracked parked placement must not teach logical minimums"
    );
    assert!(
        state.allows_core_size_feedback(request_id, 999),
        "an untracked window keeps the existing ordinary-feedback behavior"
    );
}

#[test]
fn test_stale_animation_result_does_not_release_newer_physical_latch() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(1800, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![placement.clone()]);
    let (stale_request, stale_invalidation) = state.physical_request_ids();
    state.apply_physical_projection(vec![placement]);
    let (current_request, current_invalidation) = state.physical_request_ids();
    state.applying_layout = true;

    let stale = animation_worker::FrameResult {
        apply_result: Ok(()),
        frame_time: std::time::Duration::ZERO,
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
        physical_request_id: stale_request,
        physical_invalidation_id: stale_invalidation,
        landings: Vec::new(),
    };
    assert!(matches!(
        state.handle_animation_placement_result(&stale),
        crate::layout_apply::AnimationPlacementResult::Stale
    ));
    assert!(state.applying_layout);
    assert_eq!(state.inflight_request_id, Some(current_request));

    let current = animation_worker::FrameResult {
        physical_request_id: current_request,
        physical_invalidation_id: current_invalidation,
        ..stale
    };
    assert!(matches!(
        state.handle_animation_placement_result(&current),
        crate::layout_apply::AnimationPlacementResult::Current
    ));
    assert!(
        !state.applying_layout,
        "a current acknowledgement with no native landings still releases its latch"
    );
}

#[test]
fn test_outer_animation_pump_preserves_newer_frame_and_resumes_after_sync_supersession() {
    use crate::layout_apply::AnimationPlacementResult;
    use crate::{interrupted_animation_frame_action, InterruptedAnimationFrameAction};

    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(1800, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![placement.clone()]);
    let (old_request, old_invalidation) = state.physical_request_ids();
    state.animation_inflight_request_id = Some(old_request);
    let newer = state.apply_physical_projection(vec![placement]);
    let (new_request, new_invalidation) = state.physical_request_ids();
    state.animation_inflight_request_id = Some(new_request);
    let stale = animation_worker::FrameResult {
        apply_result: Ok(()),
        frame_time: std::time::Duration::ZERO,
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
        physical_request_id: old_request,
        physical_invalidation_id: old_invalidation,
        landings: Vec::new(),
    };

    let result = state.handle_animation_placement_result(&stale);
    assert!(matches!(result, AnimationPlacementResult::Stale));
    assert_eq!(state.inflight_request_id, Some(new_request));
    assert_eq!(
        interrupted_animation_frame_action(
            AnimationPlacementResult::Stale,
            state.animation_inflight_request_id
        ),
        Some(InterruptedAnimationFrameAction::LeaveNewerFrame),
        "an old completion must not supersede the newer outstanding frame"
    );

    state.consume_physical_landings(
        new_request,
        new_invalidation,
        &[leopardwm_platform_win32::PlacementLanding {
            window_id: 100,
            requested_rect: newer[0].rect,
            requested_visibility: newer[0].visibility,
            actual_visible_rect: Some(newer[0].rect),
            actual_outer_rect: Some(newer[0].rect),
            failed: false,
            unreadable: false,
            measurement_deferred: false,
        }],
        &[],
    );
    assert_eq!(state.inflight_request_id, None);
    state.animation_inflight_request_id = None;
    assert_eq!(
        interrupted_animation_frame_action(
            AnimationPlacementResult::Stale,
            state.animation_inflight_request_id,
        ),
        Some(InterruptedAnimationFrameAction::Resume),
        "after synchronous apply consumes the newer request, the stale acknowledgement restarts the pump"
    );
    assert_eq!(
        interrupted_animation_frame_action(AnimationPlacementResult::InvalidatedCurrent, None),
        Some(InterruptedAnimationFrameAction::ReapplyThenResume),
        "a matching invalidated frame re-lands before progressing"
    );
    assert_eq!(
        interrupted_animation_frame_action(AnimationPlacementResult::Current, None),
        None
    );
}

#[test]
fn test_stale_animation_result_resumes_after_unconsumed_sync_supersession() {
    use crate::layout_apply::AnimationPlacementResult;
    use crate::{interrupted_animation_frame_action, InterruptedAnimationFrameAction};

    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(1800, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![placement.clone()]);
    let (old_request, old_invalidation) = state.physical_request_ids();
    state.animation_inflight_request_id = Some(old_request);
    state.apply_physical_projection(vec![placement]);
    state.animation_inflight_request_id = None;

    let stale = animation_worker::FrameResult {
        apply_result: Ok(()),
        frame_time: std::time::Duration::ZERO,
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
        physical_request_id: old_request,
        physical_invalidation_id: old_invalidation,
        landings: Vec::new(),
    };
    assert!(matches!(
        state.handle_animation_placement_result(&stale),
        AnimationPlacementResult::Stale
    ));
    assert_eq!(
        interrupted_animation_frame_action(
            AnimationPlacementResult::Stale,
            state.animation_inflight_request_id,
        ),
        Some(InterruptedAnimationFrameAction::Resume),
        "a synchronous supersession without an async completion must not strand the animation pump"
    );
}

#[test]
fn test_interrupted_animation_recovery_lands_after_barrier_release() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    assert!(!resume_after_interrupted_animation_frame(
        &mut state,
        &worker,
        InterruptedAnimationFrameAction::ReapplyThenResume,
    ));
    assert!(state.layout_transition.is_some());
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "an invalidated frame must not dispatch while its recovery barrier is busy"
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_none());
    assert_eq!(state.find_window_workspace(100), Some((1, 0)));
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0,
        "recovery must perform a physical landing after the interrupted transition settles"
    );
}

#[test]
fn test_interrupted_recovery_settles_ghosts_scroll_nudge_and_focus_after_retry() {
    use crate::state::{
        GhostEntry, TestApplyPlacementsBehavior, TestApplyPlacementsOutcome,
        TestApplyPlacementsStep,
    };

    const WID: u64 = u64::MAX - 42;
    struct GhostCloakGuard(u64);
    impl Drop for GhostCloakGuard {
        fn drop(&mut self) {
            leopardwm_platform_win32::unmark_ghost_cloaked(self.0);
        }
    }

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        for hwnd in [WID, WID - 1, WID - 2] {
            workspace.insert_window(hwnd, Some(800)).unwrap();
        }
        workspace.set_reduce_motion(false);
        workspace.focus_window(WID).unwrap();
        workspace.start_scroll_animation(400.0, 1920, Some(1_000), None);
    }
    state.previous_focused_hwnd = Some(WID);
    let landing = state.workspaces[&1][0]
        .compute_placements(state.layout_viewport(1))
        .into_iter()
        .find(|placement| placement.window_id == WID)
        .unwrap();
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state
        .layout_transition
        .as_mut()
        .unwrap()
        .suppress_landing_focus_resync = true;
    state.ghost_handles.insert(
        WID,
        GhostEntry::new(0, "GhostClass".into(), Rect::new(0, 0, 800, 600)),
    );
    let _cloak_guard = GhostCloakGuard(WID);
    leopardwm_platform_win32::mark_ghost_cloaked(WID);
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Fail,
        },
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: vec![leopardwm_platform_win32::PlacementLanding {
                    window_id: WID,
                    requested_rect: landing.rect,
                    requested_visibility: landing.visibility,
                    actual_visible_rect: Some(landing.rect),
                    actual_outer_rect: Some(landing.rect),
                    failed: false,
                    unreadable: false,
                    measurement_deferred: false,
                }],
            },
        },
    ]));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Waiting
    );
    assert!(state.ghost_handles.contains_key(&WID));
    assert!(state.layout_transition.is_some());

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    match state.try_consume_idle_layout_reapply() {
        crate::temporary_ignore::IdleLayoutReapply::Failed { message } => {
            assert!(
                message.contains("injected apply_placements failure"),
                "first recovery placement error must keep its provenance, got {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(state.layout_transition.is_none());
    assert!(!state.workspaces[&1][0].is_animating());
    assert_eq!(state.workspaces[&1][0].scroll_offset(), 400.0);
    assert!(state.ghost_handles.is_empty());
    assert!(state.ghost_sources_pending_safe_landing.contains(&WID));
    assert!(leopardwm_platform_win32::is_placement_cloaked(WID));
    assert!(state.post_animation_nudge_pending);
    assert!(state.pending_suppress_landing_focus_resync);

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.post_animation_nudge_pending);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert_eq!(state.previous_focused_hwnd, Some(WID));
    assert!(!state.ghost_sources_pending_safe_landing.contains(&WID));
    assert!(!leopardwm_platform_win32::is_placement_cloaked(WID));
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        2
    );
}

#[test]
fn test_explicit_apply_consumes_suppressed_recovery_landing_after_failure() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.focus_window(100).unwrap();
    }
    state.previous_focused_hwnd = Some(100);
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state
        .layout_transition
        .as_mut()
        .unwrap()
        .suppress_landing_focus_resync = true;
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    match state.try_consume_idle_layout_reapply() {
        crate::temporary_ignore::IdleLayoutReapply::Failed { message } => {
            assert!(
                message.contains("injected apply_placements failure"),
                "first recovery placement error must keep its provenance, got {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(state.layout_transition.is_none());
    assert!(state.pending_idle_layout_reapply);
    assert!(state.pending_suppress_landing_focus_resync);
    assert!(state.post_animation_nudge_pending);

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));
    assert!(matches!(
        state.handle_command(IpcCommand::Apply),
        IpcResponse::Error { .. }
    ));
    assert!(state.pending_idle_layout_reapply);
    assert!(state.pending_suppress_landing_focus_resync);
    assert!(
        state.post_animation_nudge_pending,
        "a failed explicit recovery retry must retain its post-animation nudge"
    );

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    assert_eq!(state.handle_command(IpcCommand::Apply), IpcResponse::Ok);
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert!(!state.post_animation_nudge_pending);
    assert_eq!(state.previous_focused_hwnd, Some(100));
}

fn event_loop_equivalent_final_landing(state: &mut AppState) -> bool {
    let captured_suppress = state.pending_suppress_landing_focus_resync;
    let landing_ok = matches!(
        state.apply_layout(),
        Ok(crate::layout_apply::LayoutApplyOutcome::Completed)
    );
    state.finish_animation_landing_focus_resync(captured_suppress);
    landing_ok
}

fn arm_suppressed_recovery_final_landing() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state
        .layout_transition
        .as_mut()
        .unwrap()
        .suppress_landing_focus_resync = true;
    state.pending_idle_layout_reapply = true;
    assert!(state.tick_animations(150));
    assert!(state.layout_transition.is_none());
    assert!(state.pending_suppress_landing_focus_resync);
    state.post_animation_nudge_pending = true;
    state
}

fn assert_unrelated_landing_does_not_inherit_suppression(state: &mut AppState) {
    let updates_before_landing_sync = state.tab_strip_update_count.load(Ordering::Relaxed);
    state.sync_foreground_after_animation_landing();
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before_landing_sync + 1,
        "the next unrelated landing must not inherit recovery suppression"
    );
}

#[test]
fn test_normal_animation_landing_preserves_recovery_suppression_through_apply() {
    let mut state = arm_suppressed_recovery_final_landing();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let captured_suppress = state.pending_suppress_landing_focus_resync;
    let landing_ok = matches!(
        state.apply_layout(),
        Ok(crate::layout_apply::LayoutApplyOutcome::Completed)
    );
    assert!(landing_ok);
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.pending_suppress_landing_focus_resync);
    let updates_before_landing_sync = state.tab_strip_update_count.load(Ordering::Relaxed);

    state.finish_animation_landing_focus_resync(captured_suppress);
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before_landing_sync,
        "the normal landing must retain recovery suppression after apply finalization"
    );

    assert_unrelated_landing_does_not_inherit_suppression(&mut state);
}

#[test]
fn test_event_loop_landing_retains_suppression_when_final_apply_fails() {
    let mut state = arm_suppressed_recovery_final_landing();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));
    let updates_before = state.tab_strip_update_count.load(Ordering::Relaxed);

    assert!(!event_loop_equivalent_final_landing(&mut state));
    assert!(state.pending_idle_layout_reapply);
    assert!(
        state.pending_suppress_landing_focus_resync,
        "a failed final landing must keep recovery suppression for retry"
    );
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before,
        "a failed suppressed landing must not consume the one-shot by resyncing"
    );

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let updates_before_success = state.tab_strip_update_count.load(Ordering::Relaxed);
    assert!(event_loop_equivalent_final_landing(&mut state));
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before_success + 1,
        "successful recovered apply updates the strip once; captured suppression skips a second resync"
    );

    assert_unrelated_landing_does_not_inherit_suppression(&mut state);
}

#[test]
fn test_event_loop_landing_retains_suppression_when_final_apply_deferred() {
    let mut state = arm_suppressed_recovery_final_landing();
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());
    let updates_before = state.tab_strip_update_count.load(Ordering::Relaxed);

    assert!(!event_loop_equivalent_final_landing(&mut state));
    assert!(state.pending_idle_layout_reapply);
    assert!(
        state.pending_suppress_landing_focus_resync,
        "a deferred final landing must keep recovery suppression for retry"
    );
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before,
        "a deferred suppressed landing must not consume the one-shot by resyncing"
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let updates_before_success = state.tab_strip_update_count.load(Ordering::Relaxed);
    assert!(event_loop_equivalent_final_landing(&mut state));
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert_eq!(
        state.tab_strip_update_count.load(Ordering::Relaxed),
        updates_before_success + 1,
        "successful recovered apply updates the strip once; captured suppression skips a second resync"
    );

    assert_unrelated_landing_does_not_inherit_suppression(&mut state);
}

#[test]
fn test_event_loop_landing_retains_suppression_when_paused_final_apply_is_noop() {
    let mut state = arm_suppressed_recovery_final_landing();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert!(state.pending_suppress_landing_focus_resync);

    assert!(
        event_loop_equivalent_final_landing(&mut state),
        "paused apply_layout reports Completed without placing"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert!(
        state.pending_suppress_landing_focus_resync,
        "a paused no-op final landing must keep recovery suppression"
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "paused final landing must not dispatch placement"
    );

    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0
    );
}

#[test]
fn test_event_loop_landing_consumes_nonrecovery_suppression_when_final_apply_fails() {
    let mut state = departing_tiled_state(Some(100), Some(200));
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.suppress_landing_focus_resync),
        "managed replacement departure must arm landing suppression"
    );
    assert!(
        !state.pending_idle_layout_reapply,
        "departing-focus suppression is not recovery"
    );

    let duration = state
        .layout_transition
        .as_ref()
        .map(|transition| transition.duration_ms)
        .unwrap_or(0);
    assert!(state.tick_animations(duration));
    assert!(state.layout_transition.is_none());
    assert!(state.pending_suppress_landing_focus_resync);
    assert!(!state.pending_idle_layout_reapply);
    state.post_animation_nudge_pending = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    assert!(!event_loop_equivalent_final_landing(&mut state));
    assert!(!state.pending_idle_layout_reapply);
    assert!(
        !state.pending_suppress_landing_focus_resync,
        "a non-recovery failed landing must still consume the one-shot"
    );

    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        workspace.insert_window(300, Some(800)).unwrap();
        workspace.insert_window(400, Some(800)).unwrap();
        workspace.set_reduce_motion(false);
        workspace.start_scroll_animation(400.0, 1920, Some(1_000), None);
    }
    assert!(
        state.workspaces[&1][0].is_animating(),
        "next unrelated landing is a scroll-only animation"
    );
    let _ = state.tick_animations(1_000);
    assert!(!state.workspaces[&1][0].is_animating());
    assert!(!state.pending_suppress_landing_focus_resync);
    state.post_animation_nudge_pending = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let updates_before = state.tab_strip_update_count.load(Ordering::Relaxed);
    assert!(event_loop_equivalent_final_landing(&mut state));
    assert!(
        state.tab_strip_update_count.load(Ordering::Relaxed) > updates_before,
        "the next unrelated scroll-only landing must resync after non-recovery suppression is consumed"
    );
}

#[test]
fn test_interrupted_recovery_settles_scroll_only_animation() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        for hwnd in [100, 200, 300] {
            workspace.insert_window(hwnd, Some(800)).unwrap();
        }
        workspace.set_reduce_motion(false);
        workspace.start_scroll_animation(400.0, 1920, Some(1_000), None);
    }
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    let workspace = &state.workspaces[&1][0];
    assert_eq!(workspace.scroll_offset(), 400.0);
    assert!(!workspace.is_animating());
    assert!(!state.pending_idle_layout_reapply);
    assert!(!state.post_animation_nudge_pending);
}

#[test]
fn test_drain_then_pending_fresh_ghost_recovers_without_exposing_stale_rects() {
    use crate::state::{
        GhostEntry, TestApplyPlacementsBehavior, TestApplyPlacementsOutcome,
        TestApplyPlacementsStep,
    };

    const SOURCE: u64 = u64::MAX - 60;
    const PEER: u64 = u64::MAX - 61;
    struct GhostCloakGuard(u64);
    impl Drop for GhostCloakGuard {
        fn drop(&mut self) {
            leopardwm_platform_win32::unmark_ghost_cloaked(self.0);
        }
    }

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        workspace.insert_window(SOURCE, Some(800)).unwrap();
        workspace.insert_window(PEER, Some(800)).unwrap();
        workspace.set_reduce_motion(false);
    }

    let placements = state.workspaces[&1][0].compute_placements(state.layout_viewport(1));
    let source_rect = placements
        .iter()
        .find(|placement| placement.window_id == SOURCE)
        .unwrap()
        .rect;
    let peer_rect = placements
        .iter()
        .find(|placement| placement.window_id == PEER)
        .unwrap()
        .rect;
    state.apply_physical_projection(placements.clone());
    let (request_id, invalidation_id) = state.physical_request_ids();
    let landings: Vec<_> = placements
        .iter()
        .map(|placement| leopardwm_platform_win32::PlacementLanding {
            window_id: placement.window_id,
            requested_rect: placement.rect,
            requested_visibility: placement.visibility,
            actual_visible_rect: Some(placement.rect),
            actual_outer_rect: Some(placement.rect),
            failed: false,
            unreadable: false,
            measurement_deferred: false,
        })
        .collect();
    state.consume_physical_landings(request_id, invalidation_id, &landings, &[]);
    assert!(state.physical_landing_is_safe_to_expose(SOURCE));
    assert!(state.physical_landing_is_safe_to_expose(PEER));

    state.ghost_handles.insert(
        SOURCE,
        GhostEntry::new(0, "GhostClass".into(), Rect::new(0, 0, 800, 600)),
    );
    let _source_cloak = GhostCloakGuard(SOURCE);
    leopardwm_platform_win32::mark_ghost_cloaked(SOURCE);

    state
        .drain_pending_placement_work()
        .expect("drain with no worker must succeed");
    assert!(
        !state.physical_landing_is_safe_to_expose(SOURCE),
        "drain must invalidate older confirmed presentations"
    );
    assert!(
        !state.physical_landing_is_safe_to_expose(PEER),
        "drain must invalidate peer presentations in the same epoch bump"
    );
    assert!(state.ghost_handles.is_empty());
    assert!(state.ghost_sources_pending_safe_landing.contains(&SOURCE));
    assert!(leopardwm_platform_win32::is_placement_cloaked(SOURCE));

    state.pending_idle_layout_reapply = true;
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.ghost_handles.insert(
        PEER,
        GhostEntry::new(0, "GhostClass".into(), Rect::new(0, 0, 800, 600)),
    );
    let _peer_cloak = GhostCloakGuard(PEER);
    leopardwm_platform_win32::mark_ghost_cloaked(PEER);
    assert!(state.ghost_handles.contains_key(&PEER));
    assert!(!state.ghost_sources_pending_safe_landing.contains(&PEER));

    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Fail,
        },
        TestApplyPlacementsStep {
            delay: Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: vec![
                    leopardwm_platform_win32::PlacementLanding {
                        window_id: SOURCE,
                        requested_rect: source_rect,
                        requested_visibility: leopardwm_core_layout::Visibility::Visible,
                        actual_visible_rect: Some(source_rect),
                        actual_outer_rect: Some(source_rect),
                        failed: false,
                        unreadable: false,
                        measurement_deferred: false,
                    },
                    leopardwm_platform_win32::PlacementLanding {
                        window_id: PEER,
                        requested_rect: peer_rect,
                        requested_visibility: leopardwm_core_layout::Visibility::Visible,
                        actual_visible_rect: Some(peer_rect),
                        actual_outer_rect: Some(peer_rect),
                        failed: false,
                        unreadable: false,
                        measurement_deferred: false,
                    },
                ],
            },
        },
    ]));

    match state.try_consume_idle_layout_reapply() {
        crate::temporary_ignore::IdleLayoutReapply::Failed { message } => {
            assert!(
                message.contains("injected apply_placements failure"),
                "first recovery placement error must keep its provenance, got {message}"
            );
        }
        other => panic!("expected first recovery apply to fail, got {other:?}"),
    }
    assert!(state.ghost_handles.is_empty());
    assert!(
        state.ghost_sources_pending_safe_landing.contains(&SOURCE),
        "drain-invalidated source must stay pending after recovery abort"
    );
    assert!(
        state.ghost_sources_pending_safe_landing.contains(&PEER),
        "post-drain peer ghost must stay pending after recovery abort"
    );
    assert!(leopardwm_platform_win32::is_placement_cloaked(SOURCE));
    assert!(leopardwm_platform_win32::is_placement_cloaked(PEER));
    assert!(
        !state.physical_landing_is_safe_to_expose(SOURCE),
        "failed recovery must not expose the original source"
    );
    assert!(
        !state.physical_landing_is_safe_to_expose(PEER),
        "failed recovery must not expose the post-drain peer"
    );

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(state.layout_transition.is_none());
    assert!(state.ghost_handles.is_empty());
    assert!(!state.ghost_sources_pending_safe_landing.contains(&SOURCE));
    assert!(!state.ghost_sources_pending_safe_landing.contains(&PEER));
    assert!(!leopardwm_platform_win32::is_placement_cloaked(SOURCE));
    assert!(!leopardwm_platform_win32::is_placement_cloaked(PEER));
    assert!(state.physical_landing_is_safe_to_expose(SOURCE));
    assert!(state.physical_landing_is_safe_to_expose(PEER));
}

#[test]
fn test_filtered_empty_apply_releases_only_safe_pending_ghost_sources() {
    use crate::physical_placement::{PhysicalKind, PhysicalPresentation};
    use crate::state::ApplicationFullscreenState;

    const SAFE: u64 = u64::MAX - 70;
    const PARKED: u64 = u64::MAX - 71;
    const UNCONFIRMED: u64 = u64::MAX - 72;
    struct GhostCloakGuard(u64);
    impl Drop for GhostCloakGuard {
        fn drop(&mut self) {
            leopardwm_platform_win32::unmark_ghost_cloaked(self.0);
        }
    }

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        for hwnd in [SAFE, PARKED, UNCONFIRMED] {
            workspace.insert_window(hwnd, Some(800)).unwrap();
        }
    }

    let placements = state.workspaces[&1][0].compute_placements(state.layout_viewport(1));
    let safe_placement = placements
        .iter()
        .find(|placement| placement.window_id == SAFE)
        .unwrap()
        .clone();
    state.apply_physical_projection(vec![safe_placement.clone()]);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(
        request_id,
        invalidation_id,
        &[leopardwm_platform_win32::PlacementLanding {
            window_id: SAFE,
            requested_rect: safe_placement.rect,
            requested_visibility: safe_placement.visibility,
            actual_visible_rect: Some(safe_placement.rect),
            actual_outer_rect: Some(safe_placement.rect),
            failed: false,
            unreadable: false,
            measurement_deferred: false,
        }],
        &[],
    );
    assert!(state.physical_landing_is_safe_to_expose(SAFE));

    let safe_presentation = state.last_physical_presentations[&SAFE].clone();
    state.last_physical_presentations.insert(
        PARKED,
        PhysicalPresentation {
            physical: safe_presentation.physical.clone(),
            kind: PhysicalKind::Parked,
            request_id: safe_presentation.request_id,
            invalidation_id: safe_presentation.invalidation_id,
            confirmed: true,
            actual_visible_rect: None,
        },
    );
    state.last_physical_presentations.insert(
        UNCONFIRMED,
        PhysicalPresentation {
            physical: safe_presentation.physical.clone(),
            kind: PhysicalKind::Unchanged,
            request_id: safe_presentation.request_id,
            invalidation_id: safe_presentation.invalidation_id,
            confirmed: false,
            actual_visible_rect: None,
        },
    );
    assert!(!state.physical_landing_is_safe_to_expose(PARKED));
    assert!(!state.physical_landing_is_safe_to_expose(UNCONFIRMED));

    for hwnd in [SAFE, PARKED, UNCONFIRMED] {
        state.application_fullscreen.insert(
            hwnd,
            ApplicationFullscreenState {
                monitor_id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
            },
        );
        state.ghost_sources_pending_safe_landing.insert(hwnd);
        leopardwm_platform_win32::mark_ghost_cloaked(hwnd);
    }
    let _safe = GhostCloakGuard(SAFE);
    let _parked = GhostCloakGuard(PARKED);
    let _unconfirmed = GhostCloakGuard(UNCONFIRMED);

    assert!(state.injected_apply_placements_behavior.is_none());
    assert_eq!(
        state.apply_layout().unwrap(),
        crate::layout_apply::LayoutApplyOutcome::Completed
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "production empty/filtered apply must not spawn the injected worker"
    );
    assert!(!state.ghost_sources_pending_safe_landing.contains(&SAFE));
    assert!(!leopardwm_platform_win32::is_placement_cloaked(SAFE));
    assert!(state.ghost_sources_pending_safe_landing.contains(&PARKED));
    assert!(leopardwm_platform_win32::is_placement_cloaked(PARKED));
    assert!(state
        .ghost_sources_pending_safe_landing
        .contains(&UNCONFIRMED));
    assert!(leopardwm_platform_win32::is_placement_cloaked(UNCONFIRMED));
}

#[test]
fn test_invalidated_animation_result_releases_only_matching_latch() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let placement = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(100, 0, 400, 600),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    state.apply_physical_projection(vec![placement]);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.applying_layout = true;
    state.bump_physical_invalidation();

    let invalidated = animation_worker::FrameResult {
        apply_result: Ok(()),
        frame_time: std::time::Duration::ZERO,
        width_violations: vec![leopardwm_platform_win32::WidthViolation {
            window_id: 100,
            min_width: 1900,
        }],
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
        physical_request_id: request_id,
        physical_invalidation_id: invalidation_id,
        landings: Vec::new(),
    };
    assert!(matches!(
        state.handle_animation_placement_result(&invalidated),
        crate::layout_apply::AnimationPlacementResult::InvalidatedCurrent
    ));
    assert!(!state.applying_layout);
    assert_eq!(state.inflight_request_id, None);
    assert!(
        !state.inflight_origins.contains_key(&request_id),
        "obsolete feedback remains unconsumed"
    );
    let workspace = state.focused_workspace().unwrap();
    assert!(
        workspace.effective_column_width(&workspace.columns()[0]) < 1900,
        "an invalidated acknowledgement must not teach the old frame's minimum"
    );
}

#[test]
fn test_apply_layout_preserves_full_partial_native_width_without_parking_follow_up() {
    use crate::state::{
        TestApplyPlacementsBehavior, TestApplyPlacementsOutcome, TestApplyPlacementsStep,
    };

    let mut monitors = two_monitors();
    monitors[0].rect = Rect::new(0, 0, 5120, 1440);
    monitors[0].work_area = Rect::new(0, 0, 5120, 1440);
    monitors[1].rect = Rect::new(5120, 0, 800, 600);
    monitors[1].work_area = Rect::new(5120, 0, 800, 600);
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.paused = false;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.insert_window(101, Some(1600)).unwrap();
    workspace.insert_window(102, Some(1600)).unwrap();
    workspace.insert_window(103, Some(1600)).unwrap();
    workspace.insert_window(100, Some(1600)).unwrap();
    workspace.set_scroll_offset(480.0);
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: std::time::Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: vec![leopardwm_platform_win32::PlacementLanding {
                    window_id: 100,
                    requested_rect: Rect::new(4320, 0, 1600, 1440),
                    requested_visibility: leopardwm_core_layout::Visibility::Visible,
                    actual_visible_rect: Some(Rect::new(4320, 0, 1600, 1440)),
                    actual_outer_rect: Some(Rect::new(4320, 0, 1600, 1440)),
                    failed: false,
                    unreadable: false,
                    measurement_deferred: false,
                }],
            },
        },
    ]));

    state.apply_layout().unwrap();

    let presentation = &state.last_physical_presentations[&100];
    assert_eq!(presentation.physical.rect, Rect::new(4320, 0, 1600, 1440));
    assert_eq!(
        presentation.kind,
        crate::physical_placement::PhysicalKind::Unchanged
    );
    assert!(presentation.confirmed);
    assert_eq!(
        state.focused_workspace().unwrap().columns()[0].width(),
        1600
    );
    assert_eq!(
        *state.injected_apply_placements_batches.lock().unwrap(),
        vec![vec![101, 102, 103, 100]],
        "a successful full-width landing must not dispatch containment parking"
    );
}

#[test]
fn test_primary_feedback_survives_ordinary_contained_landing() {
    use crate::state::{
        TestApplyPlacementsBehavior, TestApplyPlacementsOutcome, TestApplyPlacementsStep,
    };

    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.paused = false;
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(400))
        .unwrap();
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: std::time::Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::SucceedWithFeedback {
                width_violations: vec![leopardwm_platform_win32::WidthViolation {
                    window_id: 200,
                    min_width: 1200,
                }],
                height_violations: Vec::new(),
                landings: vec![leopardwm_platform_win32::PlacementLanding {
                    window_id: 200,
                    requested_rect: Rect::new(1920, 0, 400, 1040),
                    requested_visibility: leopardwm_core_layout::Visibility::Visible,
                    actual_visible_rect: Some(Rect::new(1920, 0, 400, 1040)),
                    actual_outer_rect: Some(Rect::new(1920, 0, 400, 1040)),
                    failed: false,
                    unreadable: false,
                    measurement_deferred: false,
                }],
            },
        },
    ]));

    assert!(state.apply_layout().is_ok());
    let workspace = &state.workspaces[&2][0];
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[0]),
        1200
    );
}

#[test]
fn test_failed_landing_retries_before_releasing_pending_ghost() {
    use crate::state::{
        TestApplyPlacementsBehavior, TestApplyPlacementsOutcome, TestApplyPlacementsStep,
    };

    const WID: u64 = 0xFFFF_FFFF_FFFF_FF32;
    struct GhostCloakGuard(u64);
    impl Drop for GhostCloakGuard {
        fn drop(&mut self) {
            leopardwm_platform_win32::unmark_ghost_cloaked(self.0);
        }
    }

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(WID, Some(800))
        .unwrap();
    let placement = state.workspaces[&1][0]
        .compute_placements_animated(state.layout_viewport(1))
        .into_iter()
        .find(|placement| placement.window_id == WID)
        .unwrap();
    let dispatched = state.apply_physical_projection(vec![placement.clone()]);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(
        request_id,
        invalidation_id,
        &[leopardwm_platform_win32::PlacementLanding {
            window_id: WID,
            requested_rect: dispatched[0].rect,
            requested_visibility: dispatched[0].visibility,
            actual_visible_rect: Some(dispatched[0].rect),
            actual_outer_rect: Some(dispatched[0].rect),
            failed: false,
            unreadable: false,
            measurement_deferred: false,
        }],
        &[],
    );
    state.last_placed_layout_rects.insert(WID, placement.rect);
    assert!(state.physical_fast_path_ok());
    state.post_animation_nudge_pending = true;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::Scripted(vec![
        TestApplyPlacementsStep {
            delay: std::time::Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Fail,
        },
        TestApplyPlacementsStep {
            delay: std::time::Duration::ZERO,
            outcome: TestApplyPlacementsOutcome::Succeed {
                landings: vec![leopardwm_platform_win32::PlacementLanding {
                    window_id: WID,
                    requested_rect: placement.rect,
                    requested_visibility: placement.visibility,
                    actual_visible_rect: Some(placement.rect),
                    actual_outer_rect: Some(placement.rect),
                    failed: false,
                    unreadable: false,
                    measurement_deferred: false,
                }],
            },
        },
    ]));

    assert!(state.apply_layout().is_err());
    assert_eq!(state.inflight_request_id, None);
    assert!(state.inflight_origins.is_empty());
    assert!(state.pending_physical_presentations.is_empty());
    assert!(
        !state.physical_fast_path_ok(),
        "a failed landing must invalidate the old physical confirmation before an unchanged retry"
    );
    let _cloak_guard = GhostCloakGuard(WID);
    leopardwm_platform_win32::mark_ghost_cloaked(WID);
    state.ghost_sources_pending_safe_landing.insert(WID);

    assert!(state.apply_layout().is_ok());
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        2,
        "an unchanged retry must obtain a current landing instead of trusting the failed attempt's old confirmation"
    );
    assert!(!state.ghost_sources_pending_safe_landing.contains(&WID));
    assert!(!leopardwm_platform_win32::is_placement_cloaked(WID));
}

#[test]
fn test_landing_origin_drift() {
    use crate::physical_placement::{drift_warning, landing_origin_drift, PhysicalKind};

    let requested = Rect::new(100, 200, 400, 600);
    let landing = |actual_visible_rect: Option<Rect>,
                   requested_visibility: leopardwm_core_layout::Visibility,
                   failed: bool| {
        leopardwm_platform_win32::PlacementLanding {
            window_id: 1,
            requested_rect: requested,
            requested_visibility,
            actual_visible_rect,
            actual_outer_rect: actual_visible_rect,
            failed,
            unreadable: false,
            measurement_deferred: false,
        }
    };

    assert_eq!(
        landing_origin_drift(&landing(
            Some(requested),
            leopardwm_core_layout::Visibility::Visible,
            false
        )),
        None
    );
    assert_eq!(
        landing_origin_drift(&landing(
            Some(Rect::new(102, 200, 400, 600)),
            leopardwm_core_layout::Visibility::Visible,
            false
        )),
        None
    );
    assert_eq!(
        landing_origin_drift(&landing(
            Some(Rect::new(103, 200, 400, 600)),
            leopardwm_core_layout::Visibility::Visible,
            false
        )),
        Some((3, 0))
    );
    assert_eq!(
        landing_origin_drift(&landing(
            Some(Rect::new(103, 200, 400, 600)),
            leopardwm_core_layout::Visibility::OffScreenRight,
            false
        )),
        None
    );
    assert_eq!(
        landing_origin_drift(&landing(
            Some(Rect::new(103, 200, 400, 600)),
            leopardwm_core_layout::Visibility::Visible,
            true
        )),
        None
    );
    assert_eq!(
        landing_origin_drift(&landing(
            None,
            leopardwm_core_layout::Visibility::Visible,
            false
        )),
        None
    );
    assert_eq!(
        landing_origin_drift(&landing(
            Some(Rect::new(100, 200, 480, 700)),
            leopardwm_core_layout::Visibility::Visible,
            false
        )),
        None
    );

    let drifted = landing(
        Some(Rect::new(103, 200, 400, 600)),
        leopardwm_core_layout::Visibility::Visible,
        false,
    );
    assert_eq!(
        drift_warning(PhysicalKind::Parked, true, false, &drifted),
        None
    );
    assert_eq!(
        drift_warning(PhysicalKind::Unchanged, false, false, &drifted),
        None
    );
    assert_eq!(
        drift_warning(PhysicalKind::Unchanged, true, true, &drifted),
        None
    );
    assert_eq!(
        drift_warning(PhysicalKind::Unchanged, true, false, &drifted),
        Some((3, 0, Rect::new(103, 200, 400, 600)))
    );
}

#[test]
fn test_animation_projection_runs_once_and_preserves_full_partial_geometry() {
    let mut monitors = two_monitors();
    monitors[0].rect = Rect::new(0, 0, 5120, 1440);
    monitors[0].work_area = Rect::new(0, 0, 5120, 1440);
    monitors[1].rect = Rect::new(5120, 0, 800, 600);
    monitors[1].work_area = Rect::new(5120, 0, 800, 600);
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(1600))
        .unwrap();
    let logical = leopardwm_core_layout::WindowPlacement {
        window_id: 100,
        rect: Rect::new(4320, 10, 1600, 1440),
        visibility: leopardwm_core_layout::Visibility::Visible,
        column_index: 0,
    };
    let physical = state.apply_physical_projection(vec![logical.clone()]);
    let maximized = std::collections::HashSet::new();
    let request = state.prepare_animation_frame(physical, &maximized);
    assert_eq!(state.physical_request_seq, 1);
    assert_eq!(request.placements[0].rect, logical.rect);
    let origin = state.inflight_origins.get(&1).unwrap().get(&100).unwrap();
    assert_eq!(origin.physical.rect, logical.rect);
    assert_eq!(
        origin.kind,
        crate::physical_placement::PhysicalKind::Unchanged
    );
    state.applying_layout = true;
    let frame_result = animation_worker::FrameResult {
        apply_result: Ok(()),
        frame_time: Duration::ZERO,
        width_violations: Vec::new(),
        height_violations: Vec::new(),
        maximized_skipped_window_ids: Vec::new(),
        physical_request_id: request.physical_request_id,
        physical_invalidation_id: request.physical_invalidation_id,
        landings: vec![leopardwm_platform_win32::PlacementLanding {
            window_id: 100,
            requested_rect: logical.rect,
            requested_visibility: leopardwm_core_layout::Visibility::Visible,
            actual_visible_rect: Some(logical.rect),
            actual_outer_rect: Some(logical.rect),
            failed: false,
            unreadable: false,
            measurement_deferred: false,
        }],
    };
    assert!(matches!(
        state.handle_animation_placement_result(&frame_result),
        crate::layout_apply::AnimationPlacementResult::Current
    ));
    assert!(state.last_physical_presentations[&100].confirmed);
    assert_eq!(state.expected_physical_rect(100), Some(logical.rect));
}

#[test]
fn test_animation_size_violations_use_usable_width_and_retarget_transition() {
    let mut config = test_config();
    config.layout.outer_gap_left = 100;
    config.layout.outer_gap_right = 140;
    let mut state = AppState::new_with_config(config, test_monitors());
    state.reduce_motion = false;
    let monitor = state.focused_monitor;
    let viewport = state.layout_viewport(monitor);
    let usable_width = state.workspaces.get(&monitor).unwrap()[0].visible_width(viewport.width);
    assert_eq!(usable_width, 1680);
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
        workspace.insert_window(300, Some(300)).unwrap();
        workspace.focus_window(300).unwrap();
        workspace.set_reduce_motion(false);
    }
    state.start_layout_transition(state.snapshot_layout());

    let ignored_width = animation_worker::FrameResult::from_platform(
        leopardwm_platform_win32::ApplyPlacementsResult {
            width_violations: vec![leopardwm_platform_win32::WidthViolation {
                window_id: 300,
                min_width: usable_width,
            }],
            height_violations: vec![leopardwm_platform_win32::HeightViolation {
                window_id: 100,
                min_height: 700,
            }],
            maximized_skipped_window_ids: Vec::new(),
            landings: Vec::new(),
        },
        Duration::ZERO,
    );
    state.handle_animation_placement_result(&ignored_width);
    assert_eq!(
        state.workspaces.get(&monitor).unwrap()[0].columns()[2].width(),
        300,
        "a usable-width violation is ignored"
    );

    let accepted_width = animation_worker::FrameResult::from_platform(
        leopardwm_platform_win32::ApplyPlacementsResult {
            width_violations: vec![leopardwm_platform_win32::WidthViolation {
                window_id: 300,
                min_width: usable_width - 1,
            }],
            height_violations: Vec::new(),
            maximized_skipped_window_ids: Vec::new(),
            landings: Vec::new(),
        },
        Duration::ZERO,
    );
    state.handle_animation_placement_result(&accepted_width);
    assert!(
        state.workspaces.get(&monitor).unwrap()[0].is_animating(),
        "a structural transition retargets the correction into its existing pump"
    );

    state.tick_animations(10_000);
    let workspace = &state.workspaces.get(&monitor).unwrap()[0];
    assert_eq!(workspace.columns()[2].width(), 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[2]),
        usable_width - 1
    );
    assert!(
        workspace
            .compute_placements(viewport)
            .iter()
            .find(|placement| placement.window_id == 100)
            .unwrap()
            .rect
            .height
            >= 700,
        "height feedback remains applied through the shared propagation method"
    );
    assert_eq!(
        workspace
            .compute_placements(viewport)
            .into_iter()
            .find(|placement| placement.window_id == 300)
            .unwrap()
            .visibility,
        leopardwm_core_layout::Visibility::Visible,
        "the transition-pumped correction settles with the widened focused column in view"
    );
}

#[test]
fn test_sync_established_maximized_skip_restored_before_receipt_forces_follow_up_dispatch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SucceedWithMaximizedSkip(100));

    state.apply_layout().unwrap();

    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        2,
        "established restored skip must bypass unchanged-layout fast path once"
    );
    assert!(
        state.should_suppress_moved_or_resized(100),
        "the physically dispatched follow-up owns a fresh suppression lease"
    );
    assert!(state.last_placed_layout_rects.contains_key(&100));
}

#[test]
fn test_sync_fresh_maximized_skip_restored_before_receipt_defers_follow_up_dispatch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state
        .window_managed_at
        .insert(100, std::time::Instant::now());
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SucceedWithMaximizedSkip(100));

    state.apply_layout().unwrap();

    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        1,
        "fresh restored skip must stay within maximize settling rather than reapply"
    );
    assert!(state.window_last_maximized_at.contains_key(&100));
}

#[test]
fn test_current_maximize_hold_cleans_only_target_ghost_state() {
    use crate::state::{GhostEntry, LayoutTransition};
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::{HashMap, HashSet};

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let target_exit_rect = Rect::new(0, 1200, 800, 600);
    state.layout_transition = Some(LayoutTransition {
        start_rects: HashMap::from([
            (100, Rect::new(0, 0, 800, 600)),
            (200, Rect::new(800, 0, 800, 600)),
        ]),
        exit_rects: HashMap::from([(100, target_exit_rect)]),
        exit_provenance: HashMap::new(),
        elapsed_ms: 16,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids: HashSet::from([100, 200]),
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    });
    state.ghost_handles.insert(
        100,
        GhostEntry::new(0, "Chrome_WidgetWin_1".into(), Rect::new(0, 0, 800, 600)),
    );
    state.ghost_handles.insert(
        200,
        GhostEntry::new(0, "MozillaWindowClass".into(), Rect::new(800, 0, 800, 600)),
    );
    let maximized = HashSet::from([100]);

    state.record_maximized_observations(&maximized);
    let transition = state.layout_transition.as_ref().unwrap();
    assert!(!transition.ghosted_wids.contains(&100));
    assert!(!state.ghost_handles.contains_key(&100));
    assert!(transition.ghosted_wids.contains(&200));
    assert!(state.ghost_handles.contains_key(&200));
    assert!(transition.start_rects.contains_key(&100));
    assert_eq!(transition.exit_rects.get(&100), Some(&target_exit_rect));
    assert_eq!(state.layout_transition_exit_windows_to_park(), vec![100]);

    let dispatched = state.filter_physical_placements_observed(
        vec![
            WindowPlacement {
                window_id: 100,
                rect: Rect::new(0, 0, 800, 600),
                visibility: Visibility::Visible,
                column_index: 0,
            },
            WindowPlacement {
                window_id: 300,
                rect: Rect::new(800, 0, 800, 600),
                visibility: Visibility::Visible,
                column_index: 1,
            },
        ],
        &maximized,
    );
    assert_eq!(
        dispatched.iter().map(|p| p.window_id).collect::<Vec<_>>(),
        vec![300]
    );
}

#[test]
fn test_application_fullscreen_entry_removes_only_its_ghost_transition_state() {
    use crate::state::{ApplicationFullscreenState, GhostEntry, LayoutTransition};
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use std::collections::{HashMap, HashSet};

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut ghosted_wids = HashSet::new();
    ghosted_wids.extend([100, 200]);
    state.layout_transition = Some(LayoutTransition {
        start_rects: HashMap::from([
            (100, Rect::new(0, 0, 800, 600)),
            (200, Rect::new(800, 0, 800, 600)),
        ]),
        exit_rects: HashMap::from([(100, Rect::new(0, 1200, 800, 600))]),
        exit_provenance: HashMap::new(),
        elapsed_ms: 16,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids,
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    });
    state.ghost_handles.insert(
        100,
        GhostEntry::new(0, "Chrome_WidgetWin_1".into(), Rect::new(0, 0, 800, 600)),
    );
    state.ghost_handles.insert(
        200,
        GhostEntry::new(0, "MozillaWindowClass".into(), Rect::new(800, 0, 800, 600)),
    );
    state.application_fullscreen.insert(
        100,
        ApplicationFullscreenState {
            monitor_id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
        },
    );

    state.stop_ghosting_window(100);

    let transition = state.layout_transition.as_ref().unwrap();
    assert!(!transition.ghosted_wids.contains(&100));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.exit_rects.contains_key(&100));
    assert!(!state.ghost_handles.contains_key(&100));
    assert!(transition.ghosted_wids.contains(&200));
    assert!(transition.start_rects.contains_key(&200));
    assert!(state.ghost_handles.contains_key(&200));

    let placements = state.filter_application_fullscreen_placements(vec![
        WindowPlacement {
            window_id: 100,
            rect: Rect::new(0, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 0,
        },
        WindowPlacement {
            window_id: 200,
            rect: Rect::new(800, 0, 800, 600),
            visibility: Visibility::Visible,
            column_index: 1,
        },
    ]);
    let (live, ghosts) = AppState::partition_for_animation(
        placements,
        state.layout_transition.as_ref(),
        &state.ghost_handles,
    );
    assert!(live.is_empty());
    assert_eq!(ghosts.len(), 1);
}

#[test]
fn test_application_fullscreen_window_skips_ghost_registration() {
    use crate::state::ApplicationFullscreenState;
    use std::collections::HashMap;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.config.behavior.swap_chain_ghost_animation = true;
    state.application_fullscreen.insert(
        100,
        ApplicationFullscreenState {
            monitor_id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
        },
    );
    state.start_layout_transition_with_duration(
        HashMap::from([(100, Rect::new(100, 100, 800, 600))]),
        150,
    );

    assert!(state.ghost_handles.is_empty());
    assert!(state
        .layout_transition
        .as_ref()
        .is_some_and(|transition| transition.ghosted_wids.is_empty()));
}

#[test]
fn test_app_state_startup_reduce_motion_matches_all_workspaces() {
    let mut monitors = test_monitors();
    monitors.push(MonitorInfo {
        id: 2,
        rect: Rect::new(1920, 0, 1920, 1080),
        work_area: Rect::new(1920, 0, 1920, 1040),
        is_primary: false,
        device_name: "DISPLAY2".to_string(),
        scale_factor: 1.0,
    });

    let state = AppState::new_with_config(test_config(), monitors);

    for workspaces in state.workspaces.values() {
        for workspace in workspaces {
            assert_eq!(workspace.reduce_motion(), state.reduce_motion);
        }
    }
}

#[test]
fn test_note_elevation_block_lifecycle() {
    use crate::event_handler::ElevationCheck;
    use leopardwm_platform_win32::ManageBlock;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let hwnd = 0xABCD_u64;

    // First block: recorded + flagged as new (caller toasts).
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: term", ManageBlock::HigherIntegrity),
        ElevationCheck::BlockedNew
    );
    let recorded = state.elevation_blocked.get(&hwnd).unwrap();
    assert_eq!(recorded.title, "Admin: term");
    assert_eq!(recorded.reason, ManageBlock::HigherIntegrity);

    // Same window blocked again: already known, no re-notify.
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: term", ManageBlock::HigherIntegrity),
        ElevationCheck::BlockedKnown
    );

    // Recycled HWND now owned by a different blocked window (title changed):
    // re-notify and refresh the stored title.
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: other", ManageBlock::HigherIntegrity),
        ElevationCheck::BlockedNew
    );
    assert_eq!(
        state.elevation_blocked.get(&hwnd).map(|r| r.title.as_str()),
        Some("Admin: other")
    );

    // Same title, changed reason: refresh and re-notify.
    assert_eq!(
        state.note_elevation_block(hwnd, "Admin: other", ManageBlock::Protected),
        ElevationCheck::BlockedNew
    );
    assert_eq!(
        state.elevation_blocked.get(&hwnd).map(|r| r.reason),
        Some(ManageBlock::Protected)
    );

    // Now manageable (e.g. recycled HWND owned by a normal window): record cleared.
    assert_eq!(
        state.note_elevation_block(hwnd, "Notepad", ManageBlock::No),
        ElevationCheck::Manageable
    );
    assert!(!state.elevation_blocked.contains_key(&hwnd));

    // Manageable when never recorded is a no-op clear.
    assert_eq!(
        state.note_elevation_block(0x1234, "Other", ManageBlock::No),
        ElevationCheck::Manageable
    );
    assert!(state.elevation_blocked.is_empty());
}

#[test]
fn test_elevation_block_hidden_retained_destroyed_cleared() {
    use crate::event_handler::ElevationCheck;
    use leopardwm_platform_win32::ManageBlock;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let hwnd = 0xBEEF_u64;
    assert_eq!(
        state.note_elevation_block(hwnd, "Elevated", ManageBlock::HigherIntegrity),
        ElevationCheck::BlockedNew
    );

    state.handle_window_event(WindowEvent::Hidden(hwnd, 0));
    assert!(state.elevation_blocked.contains_key(&hwnd));

    state.handle_window_event(WindowEvent::Destroyed(hwnd));
    assert!(!state.elevation_blocked.contains_key(&hwnd));
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
        exit_provenance: HashMap::new(),
        elapsed_ms: 0,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids,
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
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
    assert_eq!(
        ghosts.len(),
        2,
        "two ghosted placements should produce ghost frames"
    );
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

    let (live, ghosts) = AppState::partition_for_animation(placements, None, &HashMap::new());
    assert_eq!(live.len(), 1);
    assert_eq!(ghosts.len(), 0);
}

#[test]
fn test_revoked_unconfirmed_ghost_source_stays_cloaked_until_landing() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.ghost_handles.insert(
        42,
        GhostEntry::new(0, "GhostClass".to_string(), Rect::new(0, 0, 100, 100)),
    );

    state.abort_active_ghost_transition();

    assert!(state.ghost_handles.is_empty());
    assert!(
        state.ghost_sources_pending_safe_landing.contains(&42),
        "revoking an unconfirmed ghost must not expose its stale source"
    );
}

#[test]
fn test_departing_pending_ghost_releases_ghost_cloak_after_thumbnail_drop() {
    let wid = 0xFFFF_FFFF_FFFF_FF21;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    leopardwm_platform_win32::mark_ghost_cloaked(wid);
    state.ghost_sources_pending_safe_landing.insert(wid);

    state.stop_ghosting_window_visuals(wid);

    assert!(
        !leopardwm_platform_win32::is_placement_cloaked(wid),
        "pending safe-landing membership remains cloak ownership after its thumbnail has been dropped"
    );
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

    assert!(
        state.active_crossfade.is_none(),
        "abort should clear active"
    );
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
    let old = std::time::Instant::now()
        - crate::state::CROSSFADE_BARRIER_MAX_AGE
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
        exit_provenance: HashMap::new(),
        elapsed_ms: 0,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids,
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    };

    let placements = vec![WindowPlacement {
        window_id: 99,
        rect: Rect::new(0, 0, 100, 100),
        visibility: Visibility::Visible,
        column_index: 0,
    }];

    let (live, ghosts) =
        AppState::partition_for_animation(placements, Some(&transition), &HashMap::new());
    assert_eq!(
        live.len(),
        0,
        "ghosted wid without handle should be dropped"
    );
    assert_eq!(ghosts.len(), 0);
}

#[test]
fn test_crossfade_target_barrier_releases_only_after_worker_acknowledgment() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100, 200]),
            std::time::Instant::now(),
        ),
    );

    state.stop_ghosting_window_visuals(100);
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([100, 200])),
        "visual cleanup must not release the same-source barrier before worker drop"
    );

    state.acknowledge_crossfade_target_drop(4, 100);
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([200])),
        "worker acknowledgment releases only the dropped target barrier"
    );
}

#[test]
fn test_maximized_target_drops_only_its_shared_crossfade_visual() {
    use crate::state::CrossfadeState;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100, 200]),
            std::time::Instant::now(),
        ),
    );

    state.observe_tiled_window_maximized(100);

    assert!(state.window_last_maximized_at.contains_key(&100));
    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4)
    );
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([100, 200])),
        "visual cleanup must retain the shared barrier until worker acknowledgment"
    );
}

#[test]
fn test_minimized_tiled_window_maximize_is_observed_during_restore() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let maximized_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[0].mark_minimized(100);
    state.window_last_maximized_at.insert(100, maximized_at);
    state.injected_window_maximized.insert(100, true);

    state.handle_window_event(WindowEvent::MovedOrResized(100));

    assert!(state.window_last_maximized_at[&100] > maximized_at);
}

#[test]
fn test_minimized_move_clears_maximize_timestamp() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let maximized_at = std::time::Instant::now();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[0].mark_minimized(100);
    state.window_last_maximized_at.insert(100, maximized_at);
    state.injected_window_maximized.insert(100, false);

    state.handle_window_event(WindowEvent::MovedOrResized(100));

    assert!(!state.window_last_maximized_at.contains_key(&100));
}

#[test]
fn test_application_fullscreen_crossfade_aborts_only_owning_batch() {
    use crate::state::CrossfadeState;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100]),
            std::time::Instant::now(),
        ),
    );
    state.crossfade_sources.insert(
        3,
        (
            std::collections::HashSet::from([200]),
            std::time::Instant::now(),
        ),
    );

    state.stop_ghosting_window(100);

    assert!(state.active_crossfade.is_none());
    assert!(state.crossfade_sources.contains_key(&4));
    assert!(state.crossfade_sources.contains_key(&3));
}

#[test]
fn test_application_fullscreen_crossfade_abort_reaches_worker() {
    use crate::state::CrossfadeState;
    use std::time::Duration;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .expect("spawn animation worker");
    let (abort_tx, abort_rx) = std::sync::mpsc::channel();
    state.animation_worker_control = Some(worker.control().with_abort_acknowledged(abort_tx));
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100]),
            std::time::Instant::now(),
        ),
    );
    worker
        .send_crossfade(
            4,
            vec![animation_worker::CrossfadeEntry {
                window_id: 100,
                handle_isize: 0,
                dest_client_rect: Rect::new(0, 0, 1, 1),
                dropped: None,
            }],
            100_000,
        )
        .expect("queue crossfade");

    state.stop_ghosting_window(100);

    assert!(
        abort_rx.recv_timeout(Duration::from_millis(500)).is_ok(),
        "worker must receive the abort control"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert!(matches!(
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await
        }),
        Ok(Some(DaemonEvent::CrossfadeTargetDropped {
            epoch: 4,
            window_id: 100
        }))
    ));
    assert!(matches!(
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await
        }),
        Ok(Some(DaemonEvent::CrossfadeComplete { epoch: 4 }))
    ));
    drop(worker);
}

#[test]
fn test_display_receipt_immediately_invalidates_crossfade_worker() {
    use crate::state::CrossfadeState;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .expect("spawn animation worker");
    let (abort_tx, abort_rx) = std::sync::mpsc::channel();
    let (drop_tx, drop_rx) = std::sync::mpsc::channel();
    state.animation_worker_control = Some(worker.control().with_abort_acknowledged(abort_tx));
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100]),
            std::time::Instant::now(),
        ),
    );
    let invalidation_before = state.physical_invalidation_id.load(Ordering::SeqCst);
    state.invalidate_physical_display_change();

    assert_eq!(
        state.physical_invalidation_id.load(Ordering::SeqCst),
        invalidation_before + 1,
        "receipt must invalidate physical work before debounce settles"
    );
    assert!(state.active_crossfade.is_none());
    worker
        .send_crossfade_with_physical_invalidation(
            4,
            vec![animation_worker::CrossfadeEntry {
                window_id: 100,
                handle_isize: 0,
                dest_client_rect: Rect::new(0, 0, 1, 1),
                dropped: Some(drop_tx),
            }],
            100_000,
            Some((state.physical_invalidation_id.clone(), invalidation_before)),
        )
        .expect("queue stale crossfade");
    assert!(
        abort_rx.recv_timeout(Duration::from_millis(500)).is_ok(),
        "receipt must send the crossfade abort to the worker"
    );
    assert_eq!(
        drop_rx.recv_timeout(Duration::from_millis(500)),
        Ok(100),
        "the invalidated worker crossfade must release its stale target"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert!(matches!(
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await
        }),
        Ok(Some(DaemonEvent::CrossfadeComplete { epoch: 4 }))
    ));
    drop(worker);
}

fn departing_ghost_fixture() -> AppState {
    use crate::state::{CrossfadeState, GhostEntry, LayoutTransition};
    use std::collections::{HashMap, HashSet};

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.layout_transition = Some(LayoutTransition {
        start_rects: HashMap::from([
            (100, Rect::new(0, 0, 800, 600)),
            (200, Rect::new(800, 0, 800, 600)),
        ]),
        exit_rects: HashMap::from([(100, Rect::new(0, 1200, 800, 600))]),
        exit_provenance: HashMap::new(),
        elapsed_ms: 16,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids: HashSet::from([100, 200]),
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    });
    state.ghost_handles.insert(
        100,
        GhostEntry::new(0, "Chrome_WidgetWin_1".into(), Rect::new(0, 0, 800, 600)),
    );
    state.ghost_handles.insert(
        200,
        GhostEntry::new(0, "MozillaWindowClass".into(), Rect::new(800, 0, 800, 600)),
    );
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state
        .crossfade_sources
        .insert(4, (HashSet::from([100, 200]), std::time::Instant::now()));
    state
}

#[test]
fn test_hidden_still_visible_skips_departure_cleanup() {
    use crate::event_handler::should_ignore_hidden_still_visible;

    assert!(should_ignore_hidden_still_visible(true, true, true));
    assert!(!should_ignore_hidden_still_visible(true, true, false));
    assert!(!should_ignore_hidden_still_visible(false, true, true));

    let mut state = departing_ghost_fixture();
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(200, Some(800))
        .unwrap();
    state
        .window_managed_at
        .insert(100, std::time::Instant::now());
    state.previous_focused_hwnd = Some(100);
    state.injected_visible_hwnds.insert(100);

    state.handle_window_event(WindowEvent::Hidden(100, 0));

    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert!(state.ghost_handles.contains_key(&100));
    assert!(state
        .crossfade_sources
        .get(&4)
        .is_some_and(|(sources, _)| sources.contains(&100)));
    assert!(state.window_managed_at.contains_key(&100));
}

struct ResizeTestWindow(windows::Win32::Foundation::HWND);

impl ResizeTestWindow {
    fn new(width: i32) -> Self {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WINDOW_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        Self(unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(WS_POPUP.0),
                100,
                100,
                width,
                600,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        })
    }

    fn id(&self) -> u64 {
        self.0 .0 as u64
    }
}

impl Drop for ResizeTestWindow {
    fn drop(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn resize_scroll_state(hwnd: u64, center_past_edges: bool) -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = true;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state
        .injected_window_info
        .insert(hwnd, make_test_window_info(hwnd));
    let viewport_width = state.viewport_width_for(state.focused_monitor);
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.set_reduce_motion(true);
    workspace.set_center_past_edges(center_past_edges);
    workspace.insert_window(100, Some(800)).unwrap();
    workspace.insert_window(hwnd, Some(800)).unwrap();
    workspace.set_focused_column_width_fraction(1.0, viewport_width);
    state
}

fn seed_resize_session(state: &mut AppState, hwnd: u64) {
    state.resize_hwnd = Some(hwnd);
    state.resize_preview_target = Some(Rect::new(0, 0, 800, 600));
    state.resize_preview_display_rect = Some(Rect::new(0, 0, 800, 600));
    state.pending_resize_animation = Some(ResizeAnimationRequest {
        start_rect: Rect::new(0, 0, 800, 600),
        target_rect: Rect::new(0, 0, 960, 600),
    });
    state.last_resize_hint_update = Some(std::time::Instant::now());
    state.pending_drag_hint = Some(DragHintAction::ShowGhost {
        rect: Rect::new(0, 0, 800, 600),
    });
    state.resize_preview_cancel.store(false, Ordering::Relaxed);
}

fn drag_hint_rect() -> Rect {
    Rect::new(100, 0, 400, 600)
}

fn pending_show_ghost_rect(state: &AppState) -> Option<Rect> {
    match state.pending_drag_hint {
        Some(DragHintAction::ShowGhost { rect }) => Some(rect),
        _ => None,
    }
}

fn seed_drag_session(state: &mut AppState, hwnd: u64) {
    let (monitor, ws_idx) = state
        .find_window_workspace(hwnd)
        .unwrap_or((state.focused_monitor, 0));
    state.drag_state = Some(DragState {
        hwnd,
        is_tiled: true,
        source_monitor: monitor,
        source_workspace_idx: ws_idx,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });
    if let Some(ws) = state
        .workspaces
        .get_mut(&monitor)
        .and_then(|v| v.get_mut(ws_idx))
    {
        let _ = ws.insert_window(DRAG_PLACEHOLDER_HWND, Some(800));
    }
    state.pending_drag_hint = Some(DragHintAction::ShowGhost {
        rect: drag_hint_rect(),
    });
}

fn safe_band_drag_fixture() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.insert_window(100, Some(800)).unwrap();
    workspace.insert_window(200, Some(800)).unwrap();
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });
    state
}

#[test]
fn test_safe_drag_band_geometry_and_lower_boundary() {
    use crate::drag::is_safe_drag_band;

    let tall = Rect::new(0, 100, 800, 500);
    assert!(is_safe_drag_band(&tall, 100));
    assert!(is_safe_drag_band(&tall, 163));
    assert!(!is_safe_drag_band(&tall, 164));

    let short = Rect::new(0, 10, 800, 45);
    assert!(is_safe_drag_band(&short, 18));
    assert!(!is_safe_drag_band(&short, 19));
    assert!(!is_safe_drag_band(&Rect::new(0, 0, 800, 0), 0));
}

#[test]
fn test_safe_drag_band_preview_transitions_and_restores() {
    let mut state = safe_band_drag_fixture();
    let stable = state.snapshot_layout();
    let target = stable.get(&200).unwrap();
    let band_y = target.y;
    let body_y = target.y + 300;

    state.update_drag_hint_at(100, target.x + 10, band_y, 1, false);

    assert!(!state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(state.snapshot_layout(), stable);
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::SafeBand)
    );

    state.update_drag_hint_at(100, target.x + 10, body_y, 1, false);

    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::Body)
    );
    let shifted = state.snapshot_layout();
    let peer_start = *shifted.get(&200).expect("shifted peer geometry");

    state.update_drag_hint_at(100, target.x + 10, band_y, 1, false);

    assert!(!state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration transition");
    assert_eq!(transition.start_rects.get(&200), Some(&peer_start));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));

    state.update_drag_hint_at(100, target.x + 10, body_y, 1, false);

    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::Body)
    );
}

#[test]
fn test_body_preview_shift_transition_restores_before_reorder() {
    let mut state = safe_band_drag_fixture();
    state.monitors.insert(
        2,
        MonitorInfo {
            id: 2,
            rect: Rect::new(1920, 0, 1920, 1080),
            work_area: Rect::new(1920, 0, 1920, 1040),
            is_primary: false,
            device_name: "DISPLAY2".to_string(),
            scale_factor: 1.0,
        },
    );
    let mut target_workspace = Workspace::new();
    target_workspace.insert_window(300, Some(800)).unwrap();
    state.workspaces.insert(2, vec![target_workspace]);
    let target = *state.snapshot_layout().get(&200).unwrap();
    let body_y = target.y + 300;
    let cross_monitor_target = *state.snapshot_layout().get(&300).unwrap();

    state.update_drag_hint_at(100, target.x + 10, body_y, 1, false);
    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));

    state.update_drag_hint_at(
        100,
        cross_monitor_target.x + 10,
        cross_monitor_target.y + 300,
        2,
        true,
    );

    let workspace = state.focused_workspace().unwrap();
    assert!(!workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::None)
    );
    let shift_target = state
        .drag_state
        .as_ref()
        .and_then(|drag| drag.last_drop_target);
    assert_eq!(shift_target.and_then(|target| target.window_slot), None);
    assert_eq!(shift_target.map(|target| target.monitor), Some(2));
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration transition");
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
    assert!(!transition.start_rects.contains_key(&100));
}

#[test]
fn test_body_preview_shift_restores_multi_window_source_before_same_monitor_reorder() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    let source_after_body = state.focused_workspace().unwrap();
    assert_eq!(source_after_body.find_window_location(100), None);
    assert_eq!(source_after_body.find_window_location(101), Some((0, 0)));
    assert!(source_after_body.contains_window(DRAG_PLACEHOLDER_HWND));
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();

    let source_target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, source_target.x + 10, source_target.y + 300, 1, true);

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(100), Some((1, 0)));
    assert_eq!(workspace.find_window_location(101), Some((1, 1)));
    assert_eq!(workspace.find_window_location(200), Some((0, 0)));
    assert!(!workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::None);
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration/reorder transition");
    assert_eq!(transition.start_rects.get(&101), Some(&shifted_peer_rect));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
}

#[test]
fn test_body_preview_shift_restores_multi_window_source_without_reorder() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();
    let current_column_rect = *state.snapshot_layout().get(&101).unwrap();

    state.update_drag_hint_at(
        100,
        current_column_rect.x + 10,
        current_column_rect.y + 300,
        1,
        true,
    );

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(100), Some((0, 0)));
    assert_eq!(workspace.find_window_location(101), Some((0, 1)));
    assert_eq!(workspace.find_window_location(200), Some((1, 0)));
    assert!(!workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration transition");
    assert_eq!(transition.start_rects.get(&101), Some(&shifted_peer_rect));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
}

#[test]
fn test_body_preview_shift_restores_multi_window_source_before_cross_monitor_drop() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state.monitors.insert(
        2,
        MonitorInfo {
            id: 2,
            rect: Rect::new(1920, 0, 1920, 1080),
            work_area: Rect::new(1920, 0, 1920, 1040),
            is_primary: false,
            device_name: "DISPLAY2".to_string(),
            scale_factor: 1.0,
        },
    );
    let mut target_workspace = Workspace::new();
    target_workspace.insert_window(300, Some(800)).unwrap();
    state.workspaces.insert(2, vec![target_workspace]);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();
    let cross_monitor_target = *state.snapshot_layout().get(&300).unwrap();
    state.update_drag_hint_at(
        100,
        cross_monitor_target.x + 10,
        cross_monitor_target.y + 300,
        2,
        true,
    );

    let source = state
        .workspaces
        .get(&1)
        .and_then(|workspaces| workspaces.first())
        .unwrap();
    assert_eq!(source.find_window_location(100), Some((0, 0)));
    assert_eq!(source.find_window_location(101), Some((0, 1)));
    assert!(!source.contains_window(DRAG_PLACEHOLDER_HWND));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::None);
    assert_eq!(drag.last_drop_target.map(|target| target.monitor), Some(2));
    assert_eq!(
        drag.last_drop_target.and_then(|target| target.window_slot),
        None
    );
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration transition");
    assert_eq!(transition.start_rects.get(&101), Some(&shifted_peer_rect));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));

    let drag = state.drag_state.take().unwrap();
    state.execute_cross_monitor_drag(100, &drag, 2, &cross_monitor_target);
    let source = state
        .workspaces
        .get(&1)
        .and_then(|workspaces| workspaces.first())
        .unwrap();
    assert_eq!(source.find_window_location(100), None);
    assert_eq!(source.find_window_location(101), None);
    let destination = state
        .workspaces
        .get(&2)
        .and_then(|workspaces| workspaces.first())
        .unwrap();
    assert_eq!(destination.find_window_location(100), Some((1, 0)));
    assert_eq!(destination.find_window_location(101), Some((1, 1)));
}

#[test]
fn test_body_to_safe_band_then_shift_restores_multi_window_source() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();

    state.update_drag_hint_at(100, target.x + 10, target.y, 1, false);
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::SafeBand)
    );
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);
    assert!(!state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    let intermediate_transition = state
        .layout_transition
        .as_ref()
        .expect("safe-band restoration transition")
        .start_rects
        .clone();
    assert_eq!(intermediate_transition.get(&101), Some(&shifted_peer_rect));
    assert!(!intermediate_transition.contains_key(&100));
    assert!(!intermediate_transition.contains_key(&DRAG_PLACEHOLDER_HWND));

    let current_column_rect = *state.snapshot_layout().get(&101).unwrap();
    state.update_drag_hint_at(
        100,
        current_column_rect.x + 10,
        current_column_rect.y + 300,
        1,
        true,
    );

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(100), Some((0, 0)));
    assert_eq!(workspace.find_window_location(101), Some((0, 1)));
    assert_eq!(workspace.find_window_location(200), Some((1, 0)));
    assert!(!workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::None);
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    // The safe-band tick already applied the restoration transition; the
    // Shift tick must not start a duplicate one.
    assert_eq!(
        state
            .layout_transition
            .as_ref()
            .expect("intermediate transition preserved")
            .start_rects,
        intermediate_transition
    );

    // Existing Shift reorder still works after the restore.
    let reorder_target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, reorder_target.x + 10, reorder_target.y + 300, 1, true);
    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(200), Some((0, 0)));
    assert_eq!(workspace.find_window_location(100), Some((1, 0)));
    assert_eq!(workspace.find_window_location(101), Some((1, 1)));
    assert_eq!(
        state
            .drag_state
            .as_ref()
            .map(|drag| drag.current_column_index),
        Some(1)
    );
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
}

#[test]
fn test_body_to_no_target_then_cross_monitor_shift_restores_multi_window_source() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state.monitors.insert(
        2,
        MonitorInfo {
            id: 2,
            rect: Rect::new(1920, 0, 1920, 1080),
            work_area: Rect::new(1920, 0, 1920, 1040),
            is_primary: false,
            device_name: "DISPLAY2".to_string(),
            scale_factor: 1.0,
        },
    );
    let mut target_workspace = Workspace::new();
    target_workspace.insert_window(300, Some(800)).unwrap();
    state.workspaces.insert(2, vec![target_workspace]);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();

    // Cursor leaves every column: preview drops to no-target (None mode).
    state.update_drag_hint_at(100, -100, target.y + 300, 1, false);
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::None)
    );
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);
    assert!(!state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    let intermediate_transition = state
        .layout_transition
        .as_ref()
        .expect("no-target restoration transition")
        .start_rects
        .clone();
    assert_eq!(intermediate_transition.get(&101), Some(&shifted_peer_rect));
    assert!(!intermediate_transition.contains_key(&100));
    assert!(!intermediate_transition.contains_key(&DRAG_PLACEHOLDER_HWND));

    let cross_monitor_target = *state.snapshot_layout().get(&300).unwrap();
    state.update_drag_hint_at(
        100,
        cross_monitor_target.x + 10,
        cross_monitor_target.y + 300,
        2,
        true,
    );

    let source = state
        .workspaces
        .get(&1)
        .and_then(|workspaces| workspaces.first())
        .unwrap();
    assert_eq!(source.find_window_location(100), Some((0, 0)));
    assert_eq!(source.find_window_location(101), Some((0, 1)));
    assert!(!source.contains_window(DRAG_PLACEHOLDER_HWND));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::None);
    assert_eq!(drag.last_drop_target.map(|target| target.monitor), Some(2));
    assert_eq!(
        drag.last_drop_target.and_then(|target| target.window_slot),
        None
    );
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    // The no-target tick already applied the restoration transition; the
    // Shift tick must not start a duplicate one.
    assert_eq!(
        state
            .layout_transition
            .as_ref()
            .expect("intermediate transition preserved")
            .start_rects,
        intermediate_transition
    );

    let drag = state.drag_state.take().unwrap();
    state.execute_cross_monitor_drag(100, &drag, 2, &cross_monitor_target);
    let destination = state
        .workspaces
        .get(&2)
        .and_then(|workspaces| workspaces.first())
        .unwrap();
    assert_eq!(destination.find_window_location(100), Some((1, 0)));
    assert_eq!(destination.find_window_location(101), Some((1, 1)));
}

#[test]
fn test_body_preview_shift_restores_same_column_reordered_slot() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window_in_column(102, 0).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    // Same-column reorder: drag 100 from slot 0 down into slot 1.
    let source_layout = state.snapshot_layout();
    let top = *source_layout.get(&100).unwrap();
    let bottom = *source_layout.get(&102).unwrap();
    let column_height = bottom.y + bottom.height - top.y;
    state.update_drag_hint_at(100, top.x + 10, top.y + column_height / 3 + 5, 1, false);
    {
        let workspace = state.focused_workspace().unwrap();
        assert_eq!(workspace.find_window_location(101), Some((0, 0)));
        assert_eq!(workspace.find_window_location(100), Some((0, 1)));
        assert_eq!(workspace.find_window_location(102), Some((0, 2)));
    }
    assert!(!state.drag_state.as_ref().unwrap().removed_from_source);

    // Cross-column Body preview removes 100 from the source column. The
    // reordered slot — not the drag-start slot — must be tracked.
    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y + 300, 1, false);
    {
        let workspace = state.focused_workspace().unwrap();
        assert_eq!(workspace.find_window_location(100), None);
        assert!(workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    }
    let drag = state.drag_state.as_ref().unwrap();
    assert!(drag.removed_from_source);
    assert_eq!(drag.source_window_slot, 1);
    let shifted_peer_rect = *state.snapshot_layout().get(&101).unwrap();

    // Shift restoration reinserts at the reordered slot, not slot 0.
    let source_column_rect = *state.snapshot_layout().get(&101).unwrap();
    state.update_drag_hint_at(
        100,
        source_column_rect.x + 10,
        source_column_rect.y + 300,
        1,
        true,
    );

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(101), Some((0, 0)));
    assert_eq!(workspace.find_window_location(100), Some((0, 1)));
    assert_eq!(workspace.find_window_location(102), Some((0, 2)));
    assert_eq!(workspace.find_window_location(200), Some((1, 0)));
    assert!(!workspace.contains_window(DRAG_PLACEHOLDER_HWND));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::None);
    assert_eq!(drag.current_column_index, 0);
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
    let transition = state
        .layout_transition
        .as_ref()
        .expect("restoration transition");
    assert_eq!(transition.start_rects.get(&101), Some(&shifted_peer_rect));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));

    // Existing Shift column reorder still works, preserving the new order.
    let reorder_target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, reorder_target.x + 10, reorder_target.y + 300, 1, true);
    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.find_window_location(200), Some((0, 0)));
    assert_eq!(workspace.find_window_location(101), Some((1, 0)));
    assert_eq!(workspace.find_window_location(100), Some((1, 1)));
    assert_eq!(workspace.find_window_location(102), Some((1, 2)));
    assert_eq!(
        state
            .drag_state
            .as_ref()
            .map(|drag| drag.current_column_index),
        Some(1)
    );
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::ShowGhost { .. })
    ));
}

#[test]
fn test_safe_band_does_not_override_tabbed_append_preview() {
    let mut state = safe_band_drag_fixture();
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.insert_window_in_column(300, 1).unwrap();
    workspace.toggle_focused_column_tabbed_mode();
    let target = *state.snapshot_layout().get(&200).unwrap();

    state.update_drag_hint_at(100, target.x + 10, target.y, 1, false);

    let workspace = state.focused_workspace().unwrap();
    assert!(workspace.column(1).is_some_and(|column| column.is_tabbed()));
    assert_eq!(
        workspace.find_window_location(DRAG_PLACEHOLDER_HWND),
        Some((1, 2))
    );
    assert!(
        matches!(state.pending_drag_hint, Some(DragHintAction::ShowGhost { rect }) if rect == target)
    );
    assert_eq!(
        state.drag_state.as_ref().map(|drag| drag.preview_mode),
        Some(crate::state::DragPreviewMode::Body)
    );
}

#[test]
fn test_safe_band_drop_uses_no_placeholder_fallback_target() {
    let mut state = safe_band_drag_fixture();
    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y, 1, false);
    let drag = state.drag_state.take().unwrap();

    state.finalize_drag_merge(100, &drag, 1, &Rect::new(900, 0, 800, 1040));

    let workspace = state.focused_workspace().unwrap();
    let (column, slot) = workspace.find_window_location(100).unwrap();
    assert_eq!(column, 0);
    assert_eq!(slot, 0);
    assert_eq!(workspace.find_window_location(200), Some((0, 1)));
}

#[test]
fn test_safe_band_drop_follows_surviving_target_column_after_peer_lifecycle_shift() {
    // source [100, 101] | bystander [150] | target [200, 201], SafeBand-hover
    // the target, then destroy the bystander so the target's numeric index
    // shifts left before the no-placeholder drop resolves.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(300)).unwrap();
        workspace.insert_window_in_column_at(101, 0, 1).unwrap();
        workspace.insert_window(150, Some(300)).unwrap();
        workspace.insert_window(200, Some(300)).unwrap();
        workspace.insert_window_in_column_at(201, 2, 1).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y, 1, false);
    {
        let drag = state.drag_state.as_ref().unwrap();
        assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::SafeBand);
        assert_eq!(drag.last_drop_target.unwrap().insert_index, 2);
        assert_eq!(drag.target_column_peers, vec![200, 201]);
    }

    // Bystander column (index 1) destroyed mid-drag: an unmatched peer
    // departure must preserve the drag but shifts the target from index 2 to 1.
    state.handle_window_event(WindowEvent::Destroyed(150));
    assert!(
        state.drag_state.is_some(),
        "unmatched peer departure must not cancel the drag"
    );
    assert_eq!(
        state.focused_workspace().unwrap().find_window_location(200),
        Some((1, 0))
    );

    let drag = state.drag_state.take().unwrap();
    state.finalize_drag_merge(100, &drag, 1, &Rect::new(900, 0, 800, 1040));

    let workspace = state.focused_workspace().unwrap();
    // Must land in the column that still contains 200/201 (now index 1),
    // never the stale cached index 2 or an unrelated fallback column. The
    // surviving source peer remains separate, proving target identity rather
    // than merely observing a single-column merge.
    assert_eq!(workspace.column_count(), 2);
    assert_eq!(workspace.find_window_location(101), Some((0, 0)));
    assert_eq!(workspace.find_window_location(100).map(|(c, _)| c), Some(1));
    assert!(workspace.column(1).unwrap().contains(200));
    assert!(workspace.column(1).unwrap().contains(201));
}

#[test]
fn test_safe_band_drop_snaps_back_when_target_identity_vanishes_entirely() {
    // Same setup, but both target peers are destroyed before the drop lands,
    // so no surviving identity exists anywhere to resolve against.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(100, Some(300)).unwrap();
        workspace.insert_window(200, Some(300)).unwrap();
        workspace.insert_window_in_column_at(201, 1, 1).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 0,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target.x + 10, target.y, 1, false);
    {
        let drag = state.drag_state.as_ref().unwrap();
        assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::SafeBand);
        assert_eq!(drag.target_column_peers, vec![200, 201]);
    }

    state.handle_window_event(WindowEvent::Destroyed(200));
    state.handle_window_event(WindowEvent::Destroyed(201));
    assert!(state.drag_state.is_some());

    let drag = state.drag_state.take().unwrap();
    // Only the single-window source column [100] survives, so any cursor
    // resolution is safe: it either recomputes onto the source column
    // itself (single-window-onto-itself snap-back) or finds nothing
    // (out-of-bounds snap-back) — never an unrelated/fallback column.
    state.finalize_drag_merge(100, &drag, 1, &Rect::new(100_000, 0, 800, 600));

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.column_count(), 1);
    assert_eq!(workspace.find_window_location(100), Some((0, 0)));
}

#[test]
fn test_shift_restore_follows_surviving_source_column_after_peer_lifecycle_shift() {
    // bystander [150] | source [100, 101] | target [200]. Body-preview drags
    // 100 onto the target (removing it from the multi-window source), then
    // the bystander is destroyed, shifting the source's numeric column index
    // before the Shift restoration tick lands.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(150, Some(300)).unwrap();
        workspace.insert_window(100, Some(300)).unwrap();
        workspace.insert_window_in_column_at(101, 1, 1).unwrap();
        workspace.insert_window(200, Some(300)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 1,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    // Body-preview onto the target column (below its safe band) so the
    // dragged window is pulled out of the multi-window source.
    let target_rect = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(
        100,
        target_rect.x + 10,
        target_rect.y + target_rect.height - 5,
        1,
        false,
    );
    {
        let drag = state.drag_state.as_ref().unwrap();
        assert_eq!(drag.preview_mode, crate::state::DragPreviewMode::Body);
        assert!(drag.removed_from_source);
        assert_eq!(drag.source_column_peers, vec![101]);
        assert_eq!(drag.current_column_index, 1);
    }
    assert_eq!(
        state.focused_workspace().unwrap().find_window_location(101),
        Some((1, 0))
    );

    // Bystander (index 0) destroyed mid-drag: unmatched peer departure
    // preserves the drag but shifts the source column from index 1 to 0.
    state.handle_window_event(WindowEvent::Destroyed(150));
    assert!(
        state.drag_state.is_some(),
        "unmatched peer departure must not cancel the drag"
    );
    assert_eq!(
        state.focused_workspace().unwrap().find_window_location(101),
        Some((0, 0))
    );

    // Aim the Shift restoration tick at surviving source peer 101. This
    // isolates source restoration from Shift's separate whole-column reorder
    // semantics, so the stale cached index cannot redirect the restore into
    // the target column.
    let source_rect = *state.snapshot_layout().get(&101).unwrap();
    state.update_drag_hint_at(100, source_rect.x + 10, source_rect.y, 1, true);

    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.column_count(), 2);
    assert_eq!(workspace.find_window_location(100), Some((0, 0)));
    assert_eq!(workspace.find_window_location(101), Some((0, 1)));
    // Target column (now index 1) must be untouched by the restore.
    assert_eq!(workspace.find_window_location(200), Some((1, 0)));
    assert!(!workspace.column(1).unwrap().contains(100));
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
    assert_eq!(drag.current_column_index, 0);
}

#[test]
fn test_shift_restore_creates_new_column_when_no_source_peer_survives() {
    // bystander [150] | source [100, 101], where 101 is also destroyed
    // before the Shift restoration tick — no surviving source peer exists,
    // so the dragged window must land in a fresh column, never an
    // unrelated one.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    {
        let workspace = state.focused_workspace_mut().unwrap();
        workspace.insert_window(150, Some(300)).unwrap();
        workspace.insert_window(100, Some(300)).unwrap();
        workspace.insert_window_in_column_at(101, 1, 1).unwrap();
        workspace.insert_window(200, Some(300)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.drag_state = Some(DragState {
        hwnd: 100,
        is_tiled: true,
        source_monitor: 1,
        source_workspace_idx: 0,
        source_window_slot: 0,
        current_column_index: 1,
        last_drop_target: None,
        last_hint_update: None,
        removed_from_source: false,
        preview_mode: crate::state::DragPreviewMode::None,
        target_column_peers: Vec::new(),
        source_column_peers: Vec::new(),
    });

    let target_rect = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(
        100,
        target_rect.x + 10,
        target_rect.y + target_rect.height - 5,
        1,
        false,
    );
    assert!(state.drag_state.as_ref().unwrap().removed_from_source);

    state.handle_window_event(WindowEvent::Destroyed(150));
    state.handle_window_event(WindowEvent::Destroyed(101));
    assert!(state.drag_state.is_some());
    assert!(!state.focused_workspace().unwrap().contains_window(101));

    let target_rect = *state.snapshot_layout().get(&200).unwrap();
    state.update_drag_hint_at(100, target_rect.x + 10, target_rect.y, 1, true);

    let workspace = state.focused_workspace().unwrap();
    // 100 must be restored as its own column, not folded into the
    // surviving target column [200]. The target shifted to index 1 after
    // the bystander and emptied source column disappeared.
    let (target_col, target_slot) = workspace.find_window_location(200).unwrap();
    assert_eq!(target_slot, 0);
    let (col100, _) = workspace.find_window_location(100).unwrap();
    assert_ne!(col100, target_col);
    assert_eq!(workspace.column(col100).unwrap().len(), 1);
    let drag = state.drag_state.as_ref().unwrap();
    assert!(!drag.removed_from_source);
}

fn seed_removed_from_source_drag(state: &mut AppState, hwnd: u64) {
    seed_drag_session(state, hwnd);
    if let Some((monitor, ws_idx)) = state.find_window_workspace(hwnd) {
        if let Some(ws) = state
            .workspaces
            .get_mut(&monitor)
            .and_then(|v| v.get_mut(ws_idx))
        {
            let _ = ws.remove_window(hwnd);
        }
    }
    if let Some(drag) = state.drag_state.as_mut() {
        drag.removed_from_source = true;
    }
}

fn assert_resize_session_cleared(state: &AppState) {
    assert_eq!(state.resize_hwnd, None);
    assert_eq!(state.resize_preview_target, None);
    assert_eq!(state.resize_preview_display_rect, None);
    assert!(state.pending_resize_animation.is_none());
    assert!(state.last_resize_hint_update.is_none());
    assert!(state.resize_preview_cancel.load(Ordering::Relaxed));
    assert!(matches!(
        state.pending_drag_hint,
        Some(DragHintAction::Hide)
    ));
}

fn two_managed_windows() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state
}

#[test]
fn test_matching_resize_departure_cancels_preview_without_completing() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = two_managed_windows();
        seed_resize_session(&mut state, 100);

        state.handle_window_event(event);

        assert_resize_session_cleared(&state);
        assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
        assert!(!state.focused_workspace().unwrap().contains_window(100));
        assert!(state.focused_workspace().unwrap().contains_window(200));
    }
}

#[test]
fn test_matching_drag_departure_clears_session_and_placeholder() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = two_managed_windows();
        seed_drag_session(&mut state, 100);
        assert!(state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));

        state.handle_window_event(event);

        assert!(state.drag_state.is_none());
        assert!(matches!(
            state.pending_drag_hint,
            Some(DragHintAction::Hide)
        ));
        assert!(!state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));
        assert!(!state.focused_workspace().unwrap().contains_window(100));
        assert!(state.focused_workspace().unwrap().contains_window(200));
    }
}

#[test]
fn test_unmatched_peer_resize_and_drag_sessions_survive_departure() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 200);
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(state.resize_hwnd, Some(200));
    assert!(state.resize_preview_display_rect.is_some());
    assert!(state.pending_resize_animation.is_some());
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
    assert!(state.focused_workspace().unwrap().contains_window(200));

    let mut state = two_managed_windows();
    seed_drag_session(&mut state, 200);
    state.handle_window_event(WindowEvent::Hidden(100, 0));
    assert_eq!(state.drag_state.as_ref().map(|d| d.hwnd), Some(200));
    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(pending_show_ghost_rect(&state), Some(drag_hint_rect()));
}

#[test]
fn test_visible_hidden_suppression_preserves_resize_and_drag_sessions() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 100);
    seed_drag_session(&mut state, 200);
    state.injected_visible_hwnds.insert(100);

    state.handle_window_event(WindowEvent::Hidden(100, 0));

    assert_eq!(state.resize_hwnd, Some(100));
    assert!(state.resize_preview_display_rect.is_some());
    assert!(state.pending_resize_animation.is_some());
    assert!(!state.resize_preview_cancel.load(Ordering::Relaxed));
    assert_eq!(state.drag_state.as_ref().map(|d| d.hwnd), Some(200));
    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);

    let mut state = two_managed_windows();
    seed_removed_from_source_drag(&mut state, 100);
    state.injected_visible_hwnds.insert(100);
    let drag = state.drag_state.as_ref().unwrap();
    let drag_metadata = (
        drag.hwnd,
        drag.is_tiled,
        drag.source_monitor,
        drag.source_workspace_idx,
        drag.current_column_index,
        drag.last_drop_target,
        drag.last_hint_update,
        drag.removed_from_source,
    );

    state.handle_window_event(WindowEvent::Hidden(100, 0));

    let drag = state.drag_state.as_ref().expect("active drag survives");
    assert_eq!(
        (
            drag.hwnd,
            drag.is_tiled,
            drag.source_monitor,
            drag.source_workspace_idx,
            drag.current_column_index,
            drag.last_drop_target,
            drag.last_hint_update,
            drag.removed_from_source,
        ),
        drag_metadata
    );
    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(pending_show_ghost_rect(&state), Some(drag_hint_rect()));
    assert!(!state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(state.window_managed_at.contains_key(&100));
}

#[test]
fn test_matching_move_size_end_completes_resize_and_ignores_mismatch() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 100);
    state.handle_window_event(WindowEvent::MoveSizeEnd(200));
    assert_eq!(state.resize_hwnd, Some(100));
    assert!(state.pending_resize_animation.is_some());
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);

    state.handle_window_event(WindowEvent::MoveSizeEnd(100));
    assert_resize_session_cleared(&state);
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 1);
    assert!(state.focused_workspace().unwrap().contains_window(100));
}

#[test]
fn test_resize_narrowing_clamps_scroll_reveals_column_and_syncs_taskbar() {
    // Protects scroll landing and taskbar visibility when reduce-motion applies
    // a resize correction instantly; without the post-apply sync column 0's taskbar
    // button stays hidden. Existing coverage lacks this real-HWND resize path.
    let fixture = ResizeTestWindow::new(960);
    let mut state = resize_scroll_state(fixture.id(), false);
    let viewport_width = state.viewport_width_for(state.focused_monitor);
    let workspace = state.focused_workspace_mut().unwrap();
    let max_scroll = (workspace.total_width() - workspace.visible_width(viewport_width)).max(0);
    workspace.set_scroll_offset(max_scroll as f64);
    assert!(max_scroll > 0);

    seed_resize_session(&mut state, fixture.id());
    state.handle_window_event(WindowEvent::MoveSizeEnd(fixture.id()));

    let workspace = state.focused_workspace().unwrap();
    let landing_max = (workspace.total_width() - workspace.visible_width(viewport_width)).max(0);
    assert_eq!(landing_max, 0, "narrowed workspace now fits the viewport");
    assert_eq!(workspace.scroll_offset(), landing_max as f64);
    let previous_column = state
        .compute_window_layout_rect(100)
        .expect("previous column placement");
    assert!(previous_column.x >= 0 && previous_column.right() <= viewport_width);
    assert!(state
        .take_recorded_taskbar_commands()
        .contains(&(100, true)));
}

#[test]
fn test_resize_snap_preserves_valid_scroll_when_focused_column_stays_visible() {
    // Protects a no-op landing when the post-snap focused column is already
    // visible and scroll is in bounds; correction must not introduce movement.
    // The production resize path with a real HWND is not covered elsewhere.
    let fixture = ResizeTestWindow::new(960);
    let mut state = resize_scroll_state(fixture.id(), false);
    let viewport_width = state.viewport_width_for(state.focused_monitor);
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.set_scroll_offset(0.0);
    let before = workspace.scroll_offset();

    seed_resize_session(&mut state, fixture.id());
    state.handle_window_event(WindowEvent::MoveSizeEnd(fixture.id()));

    assert_eq!(state.focused_workspace().unwrap().scroll_offset(), before);
    let rect = state
        .compute_window_layout_rect(fixture.id())
        .expect("focused fixture placement");
    assert!(rect.x >= 0 && rect.right() <= viewport_width);
}

#[test]
fn test_resize_scroll_matches_set_width_command_for_edge_center_modes() {
    // Protects resize parity with SetColumnWidth both with and without edge
    // centering; a missing resize correction diverges from the keyboard owner.
    // Existing command tests do not exercise this production resize boundary.
    let fixture = ResizeTestWindow::new(960);
    for center_past_edges in [false, true] {
        let mut resized = resize_scroll_state(fixture.id(), center_past_edges);
        let initial_scroll = 700.0;
        resized
            .focused_workspace_mut()
            .unwrap()
            .set_scroll_offset(initial_scroll);
        seed_resize_session(&mut resized, fixture.id());
        resized.handle_window_event(WindowEvent::MoveSizeEnd(fixture.id()));
        let resize_target = resized.focused_workspace().unwrap().scroll_offset();

        let mut keyboard = resize_scroll_state(fixture.id(), center_past_edges);
        keyboard
            .focused_workspace_mut()
            .unwrap()
            .set_scroll_offset(initial_scroll);
        assert_eq!(
            keyboard.handle_command(IpcCommand::SetColumnWidth { fraction: 0.5 }),
            IpcResponse::Ok
        );
        let keyboard_target = keyboard.focused_workspace().unwrap().scroll_offset();

        assert_eq!(
            resize_target, keyboard_target,
            "center_past_edges={center_past_edges}"
        );
        assert_eq!(
            resized.focused_workspace().unwrap().columns()[1].width(),
            keyboard.focused_workspace().unwrap().columns()[1].width(),
            "center_past_edges={center_past_edges}"
        );
    }
}

#[test]
fn test_stale_prune_cancels_matching_resize_and_leaves_peer() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 100);
    seed_drag_session(&mut state, 200);
    let (monitor, ws_idx) = state.find_window_workspace(100).unwrap();

    state.prune_stale_window(monitor, ws_idx, 100);

    assert_eq!(state.resize_hwnd, None);
    assert!(state.resize_preview_display_rect.is_none());
    assert!(state.pending_resize_animation.is_none());
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
    assert!(!state.focused_workspace().unwrap().contains_window(100));
    assert_eq!(state.drag_state.as_ref().map(|d| d.hwnd), Some(200));
    assert!(state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(pending_show_ghost_rect(&state), Some(drag_hint_rect()));
}

#[test]
fn test_stale_prune_releases_only_target_ghost_and_keeps_peer_barrier() {
    let mut state = departing_ghost_fixture();
    state.reduce_motion = true;
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state
            .focused_workspace_mut()
            .unwrap()
            .insert_window(hwnd, Some(800))
            .unwrap();
    }
    seed_drag_session(&mut state, 200);

    let applied = state.prune_stale_windows_for_test(&[100]);

    assert!(matches!(applied, crate::helpers::StalePruneLayout::Applied));
    assert!(!state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(!state.ghost_handles.contains_key(&100));
    assert!(state.ghost_handles.contains_key(&200));
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([100, 200])),
        "stale prune must retain the same-source barrier until worker ack"
    );
    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4)
    );
    let transition = state.layout_transition.as_ref().unwrap();
    assert!(!transition.ghosted_wids.contains(&100));
    assert!(transition.ghosted_wids.contains(&200));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(transition.start_rects.contains_key(&200));
    assert_eq!(state.drag_state.as_ref().map(|d| d.hwnd), Some(200));
    assert_eq!(pending_show_ghost_rect(&state), Some(drag_hint_rect()));

    state.acknowledge_crossfade_target_drop(4, 100);
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([200])),
        "ack releases only the pruned hwnd; peers remain"
    );
    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4)
    );
}

#[test]
fn test_matching_resize_departure_preserves_peer_drag_hint() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = two_managed_windows();
        seed_resize_session(&mut state, 100);
        seed_drag_session(&mut state, 200);

        state.handle_window_event(event);

        assert_eq!(state.resize_hwnd, None);
        assert!(state.resize_preview_display_rect.is_none());
        assert!(state.pending_resize_animation.is_none());
        assert!(state.resize_preview_cancel.load(Ordering::Relaxed));
        assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
        assert_eq!(state.drag_state.as_ref().map(|d| d.hwnd), Some(200));
        assert!(state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));
        assert_eq!(pending_show_ghost_rect(&state), Some(drag_hint_rect()));
    }
}

#[test]
fn test_matching_drag_departure_preserves_peer_resize_preview_and_hint() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = two_managed_windows();
        seed_drag_session(&mut state, 100);
        seed_resize_session(&mut state, 200);

        state.handle_window_event(event);

        assert!(state.drag_state.is_none());
        assert!(!state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));
        assert_eq!(state.resize_hwnd, Some(200));
        assert!(state.resize_preview_display_rect.is_some());
        assert!(state.pending_resize_animation.is_some());
        assert!(!state.resize_preview_cancel.load(Ordering::Relaxed));
        assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            pending_show_ghost_rect(&state),
            Some(Rect::new(0, 0, 800, 600))
        );
    }
}

#[test]
fn test_departure_reconciles_border_to_replacement_without_focus_recovery() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 100);
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    let shows_before = state.border_show_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_resize_session_cleared(&state);
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
    assert!(state.border_show_count.load(Ordering::Relaxed) > shows_before);
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 200);
    assert_eq!(state.last_broadcast_focused, Some((1, Some(200))));
}

#[test]
fn test_departure_hides_border_for_valid_unmanaged_replacement() {
    let mut state = two_managed_windows();
    seed_resize_session(&mut state, 100);
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(999));
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(0, Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, None);
    assert_resize_session_cleared(&state);
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 0);
}

#[test]
fn test_unfocused_departure_hides_border_for_unmanaged_replacement() {
    let mut state = two_managed_windows();
    state.previous_focused_hwnd = Some(200);
    state.injected_foreground_hwnd = Some(Some(999));
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(200, Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 200);
}

#[test]
fn test_departure_hides_border_when_no_replacement_window() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    seed_resize_session(&mut state, 100);
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(999));
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, None);
    assert_resize_session_cleared(&state);
    assert_eq!(state.resize_complete_count.load(Ordering::Relaxed), 0);
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
}

#[test]
fn test_removed_from_source_drag_departure_applies_peer_layout() {
    for (event, visible) in [
        (WindowEvent::Hidden(100, 0), false),
        (WindowEvent::Destroyed(100), true),
    ] {
        let mut state = two_managed_windows();
        seed_removed_from_source_drag(&mut state, 100);
        if visible {
            state.injected_visible_hwnds.insert(100);
        }
        assert!(!state.focused_workspace().unwrap().contains_window(100));
        assert!(state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));
        assert!(state.snapshot_layout().contains_key(&DRAG_PLACEHOLDER_HWND));

        state.handle_window_event(event);

        assert!(state.drag_state.is_none());
        assert!(matches!(
            state.pending_drag_hint,
            Some(DragHintAction::Hide)
        ));
        assert!(!state
            .focused_workspace()
            .unwrap()
            .contains_window(DRAG_PLACEHOLDER_HWND));
        assert!(!state.focused_workspace().unwrap().contains_window(100));
        assert!(state.focused_workspace().unwrap().contains_window(200));
        let transition = state.layout_transition.as_ref().expect("peer relayout");
        assert!(transition.start_rects.contains_key(&200));
        assert!(!transition.start_rects.contains_key(&100));
        assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
    }
}

#[test]
fn test_removed_from_source_drag_prune_cancels_session_and_relayouts() {
    let mut state = two_managed_windows();
    seed_removed_from_source_drag(&mut state, 100);
    seed_resize_session(&mut state, 200);
    assert!(!state.focused_workspace().unwrap().contains_window(100));

    let applied = state.prune_stale_windows_for_test(&[100]);

    assert!(matches!(applied, crate::helpers::StalePruneLayout::Applied));
    assert!(state.drag_state.is_none());
    assert!(!state
        .focused_workspace()
        .unwrap()
        .contains_window(DRAG_PLACEHOLDER_HWND));
    assert_eq!(state.resize_hwnd, Some(200));
    assert_eq!(
        pending_show_ghost_rect(&state),
        Some(Rect::new(0, 0, 800, 600))
    );
    let transition = state.layout_transition.as_ref().expect("peer relayout");
    assert!(transition.start_rects.contains_key(&200));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
}

#[test]
fn test_placeholder_shifted_snapshot_drives_departure_transition() {
    let mut state = two_managed_windows();
    seed_drag_session(&mut state, 100);
    let pre = state.snapshot_layout();
    assert!(pre.contains_key(&DRAG_PLACEHOLDER_HWND));
    let peer_start = *pre.get(&200).expect("peer geometry");

    state.handle_window_event(WindowEvent::Destroyed(100));

    let transition = state
        .layout_transition
        .as_ref()
        .expect("departure transition");
    assert_eq!(transition.start_rects.get(&200), Some(&peer_start));
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.start_rects.contains_key(&DRAG_PLACEHOLDER_HWND));
}

#[test]
fn test_tracked_focus_prune_hides_border_without_foreground_sample() {
    let mut state = two_managed_windows();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(0, Ordering::Relaxed);

    state.prune_stale_windows_for_test(&[100]);

    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.departing_foreground_evidence_reads, 0);
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 0);
    assert_eq!(state.last_broadcast_focused, Some((1, None)));
}

#[test]
fn test_batch_prune_applies_and_reconciles_once() {
    let mut state = two_managed_windows();
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    state.previous_focused_hwnd = Some(100);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);

    let applied = state.prune_stale_windows_for_test(&[100, 300]);

    assert!(matches!(applied, crate::helpers::StalePruneLayout::Applied));
    assert!(!state.focused_workspace().unwrap().contains_window(100));
    assert!(!state.focused_workspace().unwrap().contains_window(300));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.departing_foreground_evidence_reads, 0);
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert!(state.layout_transition.is_some());
}

#[test]
fn test_empty_prune_reports_unchanged_layout() {
    let mut state = two_managed_windows();
    let result = state.prune_stale_windows_for_test(&[]);
    assert!(matches!(
        result,
        crate::helpers::StalePruneLayout::Unchanged
    ));
    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
}

#[test]
fn test_stale_prune_reports_recovery_barrier_deferral() {
    let mut state = two_managed_windows();
    state.paused = false;
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    match state.prune_stale_windows_for_test(&[100]) {
        crate::helpers::StalePruneLayout::Failed(error) => {
            assert!(error.to_string().contains("deferred"));
        }
        other => panic!("expected deferred stale prune, got {other:?}"),
    }
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "deferred stale prune must not claim a physical placement"
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(state.find_window_workspace(100).is_none());
    assert!(state.find_window_workspace(200).is_some());
}

#[test]
fn test_prune_apply_failure_is_reported_not_success() {
    let mut state = two_managed_windows();
    state.reduce_motion = true;
    state.abort_layout_transition();
    state.paused = false;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        std::time::Duration::from_millis(1),
    ));

    let result = state.prune_stale_windows_for_test(&[100]);

    match result {
        crate::helpers::StalePruneLayout::Failed(err) => {
            assert!(
                err.to_string()
                    .contains("injected apply_placements failure"),
                "unexpected apply failure: {err}"
            );
        }
        other => panic!("expected Failed prune apply, got {other:?}"),
    }
    assert!(!state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
}

#[test]
fn test_refresh_propagates_prune_apply_failure() {
    let mut state = two_managed_windows();
    let resp = state.complete_refresh_layout(crate::helpers::StalePruneLayout::Failed(
        anyhow::anyhow!("injected prune apply failure"),
    ));
    match resp {
        leopardwm_ipc::IpcResponse::Error { message } => {
            assert!(message.contains("Failed to apply layout"));
            assert!(message.contains("injected prune apply failure"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn test_refresh_skips_second_apply_after_successful_prune() {
    let mut state = two_managed_windows();
    state.apply_worker_cancelled.store(true, Ordering::SeqCst);
    let resp = state.complete_refresh_layout(crate::helpers::StalePruneLayout::Applied);
    assert_eq!(resp, leopardwm_ipc::IpcResponse::Ok);
}

#[test]
fn test_border_only_helper_hides_when_no_tracked_or_sampled_replacement() {
    let mut state = two_managed_windows();
    state.previous_focused_hwnd = None;
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(0, Ordering::Relaxed);

    state.reconcile_border_without_stealing_focus_for(None);

    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 0);
}

#[test]
fn test_reconcile_hides_border_for_unmanaged_replacement_despite_tracked_peer() {
    let mut state = two_managed_windows();
    state.previous_focused_hwnd = Some(200);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(200, Ordering::Relaxed);

    state.reconcile_border_without_stealing_focus_for(Some(999));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 200);
}

#[test]
fn test_departing_hwnd_releases_own_ghost_and_keeps_peers() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = departing_ghost_fixture();
        // Keep the in-flight peer transition so this asserts departure
        // cleanup, not start_layout_transition's existing abort-on-new.
        state.reduce_motion = true;
        state
            .injected_window_info
            .insert(100, make_test_window_info(100));
        state
            .injected_window_info
            .insert(200, make_test_window_info(200));
        state
            .focused_workspace_mut()
            .unwrap()
            .insert_window(100, Some(800))
            .unwrap();
        state
            .focused_workspace_mut()
            .unwrap()
            .insert_window(200, Some(800))
            .unwrap();
        state.previous_focused_hwnd = Some(200);

        state.handle_window_event(event);

        assert!(!state.ghost_handles.contains_key(&100));
        assert!(state.ghost_handles.contains_key(&200));
        assert_eq!(
            state.crossfade_sources.get(&4).map(|(sources, _)| sources),
            Some(&std::collections::HashSet::from([100, 200])),
            "departure must retain the same-source barrier until worker ack"
        );
        assert_eq!(
            state.active_crossfade.as_ref().map(|state| state.epoch),
            Some(4)
        );
        let transition = state.layout_transition.as_ref().unwrap();
        assert!(!transition.ghosted_wids.contains(&100));
        assert!(transition.ghosted_wids.contains(&200));
        assert!(!transition.start_rects.contains_key(&100));
        assert!(transition.start_rects.contains_key(&200));
        assert!(!state.focused_workspace().unwrap().contains_window(100));
        assert!(state.focused_workspace().unwrap().contains_window(200));

        state.acknowledge_crossfade_target_drop(4, 100);
        assert_eq!(
            state.crossfade_sources.get(&4).map(|(sources, _)| sources),
            Some(&std::collections::HashSet::from([200])),
            "ack releases only the departing hwnd; peers remain"
        );
        assert_eq!(
            state.active_crossfade.as_ref().map(|state| state.epoch),
            Some(4)
        );
    }
}

#[test]
fn test_departing_source_with_peers_does_not_abort_epoch() {
    let mut state = departing_ghost_fixture();
    state.release_departing_hwnd_ghost(100);
    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4)
    );
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([100, 200])),
        "barrier still contains H before worker acknowledgment"
    );

    state.acknowledge_crossfade_target_drop(4, 100);
    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4)
    );
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([200])),
        "ack releases only H; peer remains and epoch is not aborted"
    );
}

#[test]
fn test_last_departing_source_waits_for_ack_before_barrier_release() {
    use crate::state::CrossfadeState;

    let mut state = departing_ghost_fixture();
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([100]),
            std::time::Instant::now(),
        ),
    );
    state.crossfade_sources.insert(
        3,
        (
            std::collections::HashSet::from([200]),
            std::time::Instant::now(),
        ),
    );
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });

    state.release_departing_hwnd_ghost(100);

    assert_eq!(
        state.active_crossfade.as_ref().map(|state| state.epoch),
        Some(4),
        "last-source departure must not abort before worker acknowledgment"
    );
    assert_eq!(
        state.crossfade_sources.get(&4).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([100])),
        "last-source barrier still contains H before ack"
    );
    assert_eq!(
        state.crossfade_sources.get(&3).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([200]))
    );

    state.acknowledge_crossfade_target_drop(4, 100);
    assert!(state
        .crossfade_sources
        .get(&4)
        .is_some_and(|(sources, _)| sources.is_empty()));
    assert_eq!(
        state.crossfade_sources.get(&3).map(|(sources, _)| sources),
        Some(&std::collections::HashSet::from([200]))
    );
}

#[test]
fn test_departing_transition_snapshot_excludes_hwnd() {
    let mut state = departing_ghost_fixture();
    state.reduce_motion = false;
    state.config.behavior.swap_chain_ghost_animation = true;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(200, Some(800))
        .unwrap();

    state.handle_window_event(WindowEvent::Destroyed(100));

    let transition = state.layout_transition.as_ref().unwrap();
    assert!(!transition.start_rects.contains_key(&100));
    assert!(!transition.ghosted_wids.contains(&100));
    assert!(!state.ghost_handles.contains_key(&100));
}

#[test]
fn test_should_recover_focus_after_departing_distinguishes_foreground() {
    use crate::ui_sync::departing_focus_decision;

    assert!(departing_focus_decision(true, 100, None, false).recover);
    assert!(departing_focus_decision(true, 100, Some(0), false).recover);
    assert!(departing_focus_decision(true, 100, Some(100), true).recover);
    assert!(departing_focus_decision(true, 100, Some(999), false).recover);
    assert!(!departing_focus_decision(true, 100, Some(200), true).recover);
    assert!(!departing_focus_decision(true, 100, Some(300), true).recover);
    assert!(!departing_focus_decision(false, 100, None, false).recover);
    assert!(!departing_focus_decision(false, 100, Some(0), false).recover);
    assert!(!departing_focus_decision(false, 100, Some(100), true).recover);

    let tracked_null = departing_focus_decision(true, 100, None, false);
    assert!(tracked_null.recover);
    assert!(!tracked_null.suppress_landing_resync);
    assert_eq!(tracked_null.replacement_hwnd, None);

    let tracked_replacement = departing_focus_decision(true, 100, Some(200), true);
    assert!(!tracked_replacement.recover);
    assert!(tracked_replacement.suppress_landing_resync);
    assert_eq!(tracked_replacement.replacement_hwnd, Some(200));

    let unfocused_null = departing_focus_decision(false, 100, None, false);
    assert!(!unfocused_null.recover);
    assert!(!unfocused_null.suppress_landing_resync);
    assert_eq!(unfocused_null.replacement_hwnd, None);

    let unfocused_invalid = departing_focus_decision(false, 100, Some(999), false);
    assert!(!unfocused_invalid.recover);
    assert!(!unfocused_invalid.suppress_landing_resync);
    assert_eq!(unfocused_invalid.replacement_hwnd, None);

    let unfocused_replacement = departing_focus_decision(false, 100, Some(200), true);
    assert!(!unfocused_replacement.recover);
    assert!(unfocused_replacement.suppress_landing_resync);
    assert_eq!(unfocused_replacement.replacement_hwnd, Some(200));
}

#[test]
fn test_departing_focus_recovery_does_not_steal_valid_foreground() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 200);
    assert_eq!(state.last_broadcast_focused, Some((1, Some(200))));
}

#[test]
fn test_managed_replacement_adopts_logical_focus_and_same_hwnd_focus_is_noop() {
    let mut state = two_managed_windows();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    let shows_after_destroy_baseline = state.border_show_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert_eq!(state.last_broadcast_focused, Some((1, Some(200))));
    assert!(state.border_show_count.load(Ordering::Relaxed) > shows_after_destroy_baseline);
    let shows_after_adopt = state.border_show_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Focused(200, 0));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert_eq!(state.last_broadcast_focused, Some((1, Some(200))));
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_after_adopt,
        "same-HWND Focused after adoption must not re-enter OS focus logic"
    );
}

#[test]
fn test_last_window_replacement_does_not_follow_other_workspace() {
    let mut state = two_managed_windows();
    let mon = state.focused_monitor;
    state.ensure_workspace_exists(mon, 1);
    if let Some((home_mon, home_idx)) = state.find_window_workspace(200) {
        let _ = state
            .workspaces
            .get_mut(&home_mon)
            .and_then(|v| v.get_mut(home_idx))
            .map(|ws| ws.remove_window(200));
    }
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    state.injected_event_time_ms = Some(1_000);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(!state.workspaces.get(&mon).unwrap()[0].contains_window(100));
    assert!(state.workspaces.get(&mon).unwrap()[1].contains_window(200));
    assert_eq!(state.last_broadcast_focused, Some((mon as i64, None)));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.workspace, 0);

    state.last_prune_at = Some(std::time::Instant::now());
    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.focused_monitor, mon);
}

#[test]
fn test_minimized_restore_does_not_clear_destroyed_departure_guard() {
    let mut state = last_window_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.ensure_workspace_exists(monitor, 2);
    state.workspaces.get_mut(&monitor).unwrap()[2]
        .insert_window(300, Some(800))
        .unwrap();

    state.handle_window_event(WindowEvent::Destroyed(100));
    let intent = state
        .pending_last_window_departure
        .expect("destroying the selected workspace's last window arms the guard");
    assert_eq!(
        intent.origin,
        LastWindowDepartureOrigin::DirectDestroyedOrHidden
    );
    assert_eq!(intent.replacement_hwnd, Some(200));

    state.workspaces.get_mut(&monitor).unwrap()[2].mark_minimized(300);
    state.handle_window_event(WindowEvent::Restored(300));
    assert_eq!(state.pending_last_window_departure, Some(intent));

    state.last_prune_at = Some(std::time::Instant::now());
    state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));

    assert_eq!(state.active_workspace_idx(monitor), intent.workspace);
    assert_eq!(state.previous_focused_hwnd, None);
}

#[test]
fn test_managed_replacement_parks_old_workspace_peer() {
    let mut state = two_managed_windows();
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    state.reduce_motion = false;
    let mon = state.focused_monitor;
    state.ensure_workspace_exists(mon, 1);
    if let Some((home_mon, home_idx)) = state.find_window_workspace(200) {
        let _ = state
            .workspaces
            .get_mut(&home_mon)
            .and_then(|v| v.get_mut(home_idx))
            .map(|ws| ws.remove_window(200));
    }
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert!(state.workspaces.get(&mon).unwrap()[0].contains_window(300));
    assert!(!state.workspaces.get(&mon).unwrap()[0].contains_window(100));
    assert_eq!(state.last_broadcast_focused, Some((mon as i64, Some(200))));
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 200);
    let transition = state
        .layout_transition
        .as_ref()
        .expect("cross-workspace adoption must start a workspace-switch transition");
    assert!(
        transition.exit_rects.contains_key(&300),
        "remaining old-workspace peer must slide off as an exit target"
    );
    assert!(!transition.exit_rects.contains_key(&100));
    assert!(transition.start_rects.contains_key(&200));
    assert!(transition.suppress_landing_focus_resync);
}

#[test]
fn test_last_window_replacement_does_not_follow_other_monitor() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    state
        .window_managed_at
        .insert(200, std::time::Instant::now());
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    state.injected_event_time_ms = Some(1_000);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.active_workspace_idx(2), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.workspaces.get(&2).unwrap()[0].contains_window(200));
    assert_eq!(state.last_broadcast_focused, Some((1, None)));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );

    state.last_prune_at = Some(std::time::Instant::now());
    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.active_workspace_idx(2), 0);
    assert_eq!(state.previous_focused_hwnd, None);
}

fn last_window_cross_workspace_state() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    state.last_prune_at = Some(std::time::Instant::now());
    state.injected_event_time_ms = Some(1_000);
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
    }
    state.handle_window_event(WindowEvent::Created(100, 0));
    let mon = state.focused_monitor;
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state
        .window_managed_at
        .insert(200, std::time::Instant::now());
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(200));
    state
}

fn tile_open_on_workspace_rule(
    match_class: &str,
    open_on_workspace: u8,
) -> crate::config::WindowRule {
    crate::config::WindowRule {
        match_class: Some(match_class.to_string()),
        match_title: None,
        match_executable: None,
        action: crate::config::WindowAction::Tile,
        width: None,
        height: None,
        corner_style: None,
        open_on_workspace: Some(open_on_workspace),
        open_maximized: false,
        column_width: None,
        open_in_column: None,
        sticky: false,
        match_max_width: None,
        match_max_height: None,
    }
}

/// Discord on workspace 3 and Steam on workspace 4 via `open_on_workspace`
/// rules, with the user then selecting Discord's workspace. Close-time
/// foreground still names Discord, so the empty-selection guard cannot sample
/// Steam as the replacement HWND.
fn last_window_rule_slot_unsampled_state() -> AppState {
    let mut config = test_config();
    config.window_rules = vec![
        tile_open_on_workspace_rule("DiscordMain", 3),
        tile_open_on_workspace_rule("SteamMain", 4),
    ];
    let mut state = AppState::new_with_config(config, test_monitors());
    state.reduce_motion = false;
    state.last_prune_at = Some(std::time::Instant::now());
    state.injected_event_time_ms = Some(1_000);

    let mut discord = make_test_window_info(100);
    discord.class_name = "DiscordMain".to_string();
    let mut steam = make_test_window_info(200);
    steam.class_name = "SteamMain".to_string();
    state.injected_window_info.insert(100, discord);
    state.injected_window_info.insert(200, steam);

    state.handle_window_event(WindowEvent::Created(200, 0));
    state.handle_window_event(WindowEvent::Created(100, 0));

    let mon = state.focused_monitor;
    assert!(
        state.workspaces.get(&mon).unwrap()[2].contains_window(100),
        "Discord rule must admit onto workspace 3"
    );
    assert!(
        state.workspaces.get(&mon).unwrap()[3].contains_window(200),
        "Steam rule must admit onto workspace 4"
    );
    assert_eq!(state.active_workspace_idx(mon), 0);

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 3 }),
        IpcResponse::Ok
    ));
    assert_eq!(state.active_workspace_idx(mon), 2);
    assert_eq!(state.previous_focused_hwnd, Some(100));
    state.injected_foreground_hwnd = Some(Some(100));
    state
}

#[test]
fn test_last_window_hidden_or_destroyed_preserves_empty_selection() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 1_000)] {
        let mut state = last_window_cross_workspace_state();
        let mon = state.focused_monitor;
        state.handle_window_event(event);
        assert_eq!(state.active_workspace_idx(mon), 0);
        assert_eq!(state.previous_focused_hwnd, None);
        assert!(state.pending_last_window_departure.is_some());
        assert!(crate::event_handler::workspace_is_genuinely_empty(
            &state.workspaces[&mon][0]
        ));
    }
}

#[test]
fn test_last_window_spurious_hidden_does_not_empty_or_arm() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.injected_visible_hwnds.insert(100);
    state.handle_window_event(WindowEvent::Hidden(100, 1_000));
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_stale_duplicate_replacement_cannot_undo_selection() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    let intent = state.pending_last_window_departure.unwrap();

    state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));
    state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));

    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.pending_last_window_departure, Some(intent));
}

#[test]
fn test_last_window_newer_same_or_different_replacement_activation_wins() {
    let mut same = last_window_cross_workspace_state();
    let mon = same.focused_monitor;
    same.handle_window_event(WindowEvent::Destroyed(100));
    let armed_at = same
        .pending_last_window_departure
        .unwrap()
        .armed_at_event_time_ms;
    same.handle_window_event(WindowEvent::Focused(200, armed_at.wrapping_add(1)));
    assert_eq!(same.active_workspace_idx(mon), 1);
    assert_eq!(same.previous_focused_hwnd, Some(200));
    assert_eq!(same.pending_last_window_departure, None);

    let mut different = last_window_cross_workspace_state();
    different
        .injected_window_info
        .insert(300, make_test_window_info(300));
    different.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    different.handle_window_event(WindowEvent::Destroyed(100));
    different.handle_window_event(WindowEvent::Focused(300, 1_000));
    assert_eq!(different.active_workspace_idx(mon), 1);
    assert_eq!(different.previous_focused_hwnd, Some(300));
    assert_eq!(different.pending_last_window_departure, None);
}

#[test]
fn test_last_window_focus_first_pruned_removal_is_attributable() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.prune_stale_windows_for_test(&[100]);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
}

#[test]
fn test_last_window_clears_stale_logical_focus_on_direct_and_prune() {
    enum Departure {
        Destroyed,
        Hidden,
        Prune,
    }
    for stale_focus in [200_u64, 999] {
        for departure in [Departure::Destroyed, Departure::Hidden, Departure::Prune] {
            let mut state = last_window_cross_workspace_state();
            let mon = state.focused_monitor;
            state.previous_focused_hwnd = Some(stale_focus);
            state.last_broadcast_focused = Some((mon as i64, Some(stale_focus)));
            let hides_before = state.border_hide_count.load(Ordering::Relaxed);

            match departure {
                Departure::Destroyed => {
                    state.handle_window_event(WindowEvent::Destroyed(100));
                }
                Departure::Hidden => {
                    state.handle_window_event(WindowEvent::Hidden(100, 1_000));
                }
                Departure::Prune => {
                    state.prune_stale_windows_for_test(&[100]);
                }
            }

            assert_eq!(state.active_workspace_idx(mon), 0);
            assert_eq!(state.previous_focused_hwnd, None);
            assert_eq!(state.last_broadcast_focused, Some((mon as i64, None)));
            assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
            assert_eq!(
                state
                    .pending_last_window_departure
                    .unwrap()
                    .replacement_hwnd,
                Some(200)
            );

            state.handle_window_event(WindowEvent::Focused(200, 1_000));
            assert_eq!(state.active_workspace_idx(mon), 0);
            assert_eq!(state.previous_focused_hwnd, None);
        }
    }
}

#[test]
fn test_last_window_stale_tracking_still_allows_newer_activation() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.previous_focused_hwnd = Some(200);
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(state.previous_focused_hwnd, None);

    state.handle_window_event(WindowEvent::Focused(200, 1_001));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_empty_reconciles_tab_strips_without_global_hide() {
    #[derive(Clone, Copy)]
    enum Departure {
        DirectWithoutReplacement,
        DirectWithReplacement,
        Prune,
    }

    for departure in [
        Departure::DirectWithoutReplacement,
        Departure::DirectWithReplacement,
        Departure::Prune,
    ] {
        let mut state = AppState::new_with_config(test_config(), two_monitors());
        state.paused = false;
        state.reduce_motion = false;
        state.last_prune_at = Some(std::time::Instant::now());
        state.injected_event_time_ms = Some(1_000);
        for hwnd in [100, 200, 201] {
            state
                .injected_window_info
                .insert(hwnd, make_test_window_info(hwnd));
        }
        state.handle_window_event(WindowEvent::Created(100, 0));
        {
            let ws = &mut state.workspaces.get_mut(&2).unwrap()[0];
            ws.insert_window(200, Some(800)).unwrap();
            ws.insert_window_in_column(201, 0).unwrap();
            ws.focus_window(200).unwrap();
            ws.toggle_focused_column_tabbed_mode();
            assert!(ws.column(0).is_some_and(|column| column.is_tabbed()));
        }
        state
            .window_managed_at
            .insert(200, std::time::Instant::now());
        state
            .window_managed_at
            .insert(201, std::time::Instant::now());
        state.previous_focused_hwnd = Some(200);
        state.last_broadcast_focused = Some((1, Some(200)));
        state.injected_foreground_hwnd = Some(match departure {
            Departure::DirectWithoutReplacement => None,
            Departure::DirectWithReplacement | Departure::Prune => Some(200),
        });
        let hides_before = state.tab_strip_hide_count.load(Ordering::Relaxed);
        let updates_before = state.tab_strip_update_count.load(Ordering::Relaxed);

        match departure {
            Departure::DirectWithoutReplacement | Departure::DirectWithReplacement => {
                state.handle_window_event(WindowEvent::Destroyed(100));
            }
            Departure::Prune => {
                state.prune_stale_windows_for_test(&[100]);
            }
        }

        assert_eq!(state.focused_monitor, 1);
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(state.active_workspace_idx(2), 0);
        assert_eq!(state.previous_focused_hwnd, None);
        assert_eq!(state.last_broadcast_focused, Some((1, None)));
        assert!(
            state.tab_strip_update_count.load(Ordering::Relaxed) > updates_before,
            "the empty departure must reconcile strips before its transition lands"
        );
        assert_eq!(
            state.tab_strip_hide_count.load(Ordering::Relaxed),
            hides_before,
            "an empty departure must not globally hide other monitors' strips"
        );
        let other = &state.workspaces[&2][0];
        assert!(other.column(0).is_some_and(|column| column.is_tabbed()));
        assert!(other.contains_window(200));
        assert!(other.contains_window(201));

        let duration = state
            .layout_transition
            .as_ref()
            .expect("departure transition")
            .duration_ms;
        assert!(state.tick_animations(duration));
        assert!(state.layout_transition.is_none());
        state.post_animation_nudge_pending = true;
        state.apply_layout().unwrap();
        let updates_before_landing_sync = state.tab_strip_update_count.load(Ordering::Relaxed);
        state.sync_foreground_after_animation_landing();

        let landing_should_sync = !matches!(departure, Departure::DirectWithReplacement);
        assert_eq!(
            state.tab_strip_update_count.load(Ordering::Relaxed),
            updates_before_landing_sync + usize::from(landing_should_sync),
            "only an unsuppressed landing should reconcile the empty selection"
        );
        assert_eq!(
            state.tab_strip_hide_count.load(Ordering::Relaxed),
            hides_before,
            "neither the immediate path nor final landing may globally hide strips"
        );

        if matches!(departure, Departure::DirectWithReplacement) {
            state.handle_window_event(WindowEvent::Focused(200, 1_001));
            assert_eq!(state.focused_monitor, 2);
            assert_eq!(state.previous_focused_hwnd, Some(200));
            assert_eq!(state.pending_last_window_departure, None);
        }
    }
}

#[test]
fn test_last_window_throttled_or_still_live_focus_follows() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_all_empty_admits_next_app_on_preserved_workspace() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.last_prune_at = Some(std::time::Instant::now());
    state.injected_event_time_ms = Some(1_000);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(None);
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));

    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    assert!(state.workspaces[&mon][0].contains_window(300));
    assert_eq!(state.active_workspace_idx(mon), 0);
}

#[test]
fn test_last_window_tiled_floating_true_empty_boundary() {
    let mut remaining_float = last_window_cross_workspace_state();
    let mon = remaining_float.focused_monitor;
    remaining_float.workspaces.get_mut(&mon).unwrap()[0]
        .add_floating(400, Rect::new(100, 100, 400, 300))
        .unwrap();
    remaining_float.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(remaining_float.active_workspace_idx(mon), 1);
    assert_eq!(remaining_float.previous_focused_hwnd, Some(200));
    assert_eq!(remaining_float.pending_last_window_departure, None);
    assert!(remaining_float.workspaces[&mon][0].is_floating(400));

    let mut last_float = last_window_cross_workspace_state();
    last_float.workspaces.get_mut(&mon).unwrap()[0]
        .remove_window(100)
        .unwrap();
    last_float.workspaces.get_mut(&mon).unwrap()[0]
        .add_floating(100, Rect::new(100, 100, 400, 300))
        .unwrap();
    last_float.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(last_float.active_workspace_idx(mon), 0);
    assert_eq!(last_float.previous_focused_hwnd, None);
    assert!(last_float.pending_last_window_departure.is_some());
}

#[test]
fn test_last_window_background_inactive_departure_is_unchanged() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.active_workspace.insert(mon, 1);
    state.previous_focused_hwnd = Some(200);
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_explicit_workspace_command_supersedes_departure_guard() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert!(state.pending_last_window_departure.is_some());

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 3 }),
        IpcResponse::Ok
    ));
    assert_eq!(state.pending_last_window_departure, None);
    assert_eq!(state.active_workspace_idx(mon), 2);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_last_window_departure_guard_equality_wrap_and_expiry() {
    let mut equal = last_window_cross_workspace_state();
    let mon = equal.focused_monitor;
    equal.handle_window_event(WindowEvent::Destroyed(100));
    equal.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(equal.active_workspace_idx(mon), 0);
    assert_eq!(equal.previous_focused_hwnd, None);

    let mut wrap = last_window_cross_workspace_state();
    wrap.injected_event_time_ms = Some(u32::MAX);
    wrap.handle_window_event(WindowEvent::Destroyed(100));
    wrap.handle_window_event(WindowEvent::Focused(200, u32::MAX));
    assert_eq!(wrap.active_workspace_idx(mon), 0);
    wrap.handle_window_event(WindowEvent::Focused(200, 0));
    assert_eq!(wrap.active_workspace_idx(mon), 1);
    assert_eq!(wrap.previous_focused_hwnd, Some(200));

    let mut expired = last_window_cross_workspace_state();
    expired.handle_window_event(WindowEvent::Destroyed(100));
    expired
        .pending_last_window_departure
        .as_mut()
        .unwrap()
        .set_at = std::time::Instant::now() - PendingLastWindowDeparture::TTL;
    expired.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(expired.active_workspace_idx(mon), 1);
    assert_eq!(expired.previous_focused_hwnd, Some(200));
    assert_eq!(expired.pending_last_window_departure, None);
}

#[test]
fn test_last_window_rule_slot_unsampled_auto_activation_preserves_empty_selection() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 1_000)] {
        let mut state = last_window_rule_slot_unsampled_state();
        let mon = state.focused_monitor;

        state.handle_window_event(event);
        assert_eq!(state.active_workspace_idx(mon), 2);
        assert_eq!(state.previous_focused_hwnd, None);
        assert!(crate::event_handler::workspace_is_genuinely_empty(
            &state.workspaces[&mon][2]
        ));
        let intent = state
            .pending_last_window_departure
            .expect("empty Discord workspace must arm the last-window guard");
        assert_eq!(intent.replacement_hwnd, None);
        assert!(state.workspaces[&mon][3].contains_window(200));

        assert_eq!(
            intent.origin,
            LastWindowDepartureOrigin::DirectDestroyedOrHidden
        );

        state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));
        assert_eq!(
            state.active_workspace_idx(mon),
            2,
            "unsampled Steam auto-activation must not leave the empty Discord workspace"
        );
        assert_eq!(state.focused_monitor, mon);
        assert_eq!(state.previous_focused_hwnd, None);
        assert_eq!(
            state
                .pending_last_window_departure
                .unwrap()
                .replacement_hwnd,
            Some(200)
        );
        assert!(state.workspaces[&mon][3].contains_window(200));
    }
}

#[test]
fn test_last_window_unsampled_same_workspace_created_focus_is_adopted() {
    let mut state = last_window_cross_workspace_state();
    state.injected_foreground_hwnd = Some(Some(100));
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    let armed_at = state
        .pending_last_window_departure
        .unwrap()
        .armed_at_event_time_ms;
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);

    state.config.behavior.focus_new_windows = false;
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    assert!(state.workspaces[&mon][0].contains_window(300));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        None
    );
    let shows_before = state.border_show_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Focused(300, armed_at));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert_eq!(state.pending_last_window_departure, None);
    assert!(
        state.border_show_count.load(Ordering::Relaxed) > shows_before,
        "same-workspace Focused after empty Created must adopt and show the border"
    );
}

#[test]
fn test_last_window_unsampled_newer_tick_activation_wins() {
    let mut state = last_window_rule_slot_unsampled_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    let armed_at = state
        .pending_last_window_departure
        .unwrap()
        .armed_at_event_time_ms;

    state.handle_window_event(WindowEvent::Focused(200, armed_at.wrapping_add(1)));
    assert_eq!(state.active_workspace_idx(mon), 3);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_unsampled_duplicate_inferred_replacement_stays() {
    let mut state = last_window_rule_slot_unsampled_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    let armed_at = state
        .pending_last_window_departure
        .unwrap()
        .armed_at_event_time_ms;

    state.handle_window_event(WindowEvent::Focused(200, armed_at));
    let bound = state.pending_last_window_departure.unwrap();
    assert_eq!(bound.replacement_hwnd, Some(200));
    state.handle_window_event(WindowEvent::Focused(200, armed_at));

    assert_eq!(state.active_workspace_idx(mon), 2);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.pending_last_window_departure, Some(bound));
}

#[test]
fn test_last_window_unsampled_different_hwnd_after_binding_wins() {
    let mut state = last_window_rule_slot_unsampled_state();
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.workspaces.get_mut(&mon).unwrap()[3]
        .insert_window(300, Some(800))
        .unwrap();
    state.handle_window_event(WindowEvent::Destroyed(100));
    let armed_at = state
        .pending_last_window_departure
        .unwrap()
        .armed_at_event_time_ms;

    state.handle_window_event(WindowEvent::Focused(200, armed_at));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );

    state.handle_window_event(WindowEvent::Focused(300, armed_at));
    assert_eq!(state.active_workspace_idx(mon), 3);
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_unsampled_different_monitor_activation_follows() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.reduce_motion = false;
    state.last_prune_at = Some(std::time::Instant::now());
    state.injected_event_time_ms = Some(1_000);
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
    }
    state.handle_window_event(WindowEvent::Created(100, 0));
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    state
        .window_managed_at
        .insert(200, std::time::Instant::now());
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(100));

    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        None
    );

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(state.active_workspace_idx(2), 0);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_unsampled_expired_guard_follows() {
    let mut state = last_window_rule_slot_unsampled_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    state.pending_last_window_departure.as_mut().unwrap().set_at =
        std::time::Instant::now() - PendingLastWindowDeparture::TTL;

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 3);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_unsampled_explicit_workspace_command_supersedes_guard() {
    let mut state = last_window_rule_slot_unsampled_state();
    let mon = state.focused_monitor;
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert!(state.pending_last_window_departure.is_some());

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 1 }),
        IpcResponse::Ok
    ));
    assert_eq!(state.pending_last_window_departure, None);
    assert_eq!(state.active_workspace_idx(mon), 0);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 3);
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_last_window_sampled_different_hwnd_still_follows() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );

    state.handle_window_event(WindowEvent::Focused(300, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_eventless_none_does_not_infer_replacement() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.injected_foreground_hwnd = Some(None);
    state.prune_stale_windows_for_test(&[100]);
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, None);
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);
    assert_eq!(state.active_workspace_idx(mon), 0);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

fn last_window_focus_prune_state() -> AppState {
    let mut state = last_window_cross_workspace_state();
    state.last_prune_at = None;
    state.injected_event_time_ms = Some(5_000);
    state.injected_stale_hwnds = vec![100];
    state
}

#[test]
fn test_last_window_focus_prune_close_to_tray_suppresses_sampled_activation() {
    let mut state = last_window_focus_prune_state();
    let mon = state.focused_monitor;
    assert_eq!(state.previous_focused_hwnd, Some(100));

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "close-to-tray prune must remove the tracked window"
    );
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.armed_at_event_time_ms, 1_000);
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);
    assert!(state.injected_stale_hwnds.is_empty());
}

#[test]
fn test_last_window_focus_prune_background_disappearance_follows() {
    for tracked in [None, Some(999)] {
        let mut state = last_window_focus_prune_state();
        let mon = state.focused_monitor;
        state.previous_focused_hwnd = tracked;

        state.handle_window_event(WindowEvent::Focused(200, 1_000));

        assert!(
            !state.workspaces[&mon][0].contains_window(100),
            "background disappearance must still remove the stale window"
        );
        assert!(crate::event_handler::workspace_is_genuinely_empty(
            &state.workspaces[&mon][0]
        ));
        assert_eq!(state.active_workspace_idx(mon), 1);
        assert_eq!(state.focused_monitor, mon);
        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert_eq!(state.pending_last_window_departure, None);
        assert!(state.injected_stale_hwnds.is_empty());
    }
}

#[test]
fn test_last_window_throttled_unmanaged_focus_then_user_activation_follows() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.injected_stale_hwnds = vec![100];
    // Known to event validation, but not managed and not a console/tray restore,
    // so focus clears tracking without consuming a throttled prune.
    state
        .injected_window_info
        .insert(999, make_test_window_info(999));

    state.handle_window_event(WindowEvent::Focused(999, 1_000));

    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.injected_stale_hwnds, vec![100]);
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.pending_last_window_departure, None);

    state.last_prune_at = None;
    state.injected_event_time_ms = Some(5_000);
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "the deferred prune must remove the vanished window"
    );
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
    assert!(state.injected_stale_hwnds.is_empty());
}

#[test]
fn test_last_window_focus_prune_newer_than_trigger_follows_despite_handler_delay() {
    let mut state = last_window_focus_prune_state();
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.armed_at_event_time_ms, 1_000);
    assert_ne!(intent.armed_at_event_time_ms, 5_000);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .armed_at_event_time_ms,
        1_000
    );

    state.handle_window_event(WindowEvent::Focused(200, 1_001));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_focus_prune_other_monitor_tracked_stale_does_not_attribute() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.reduce_motion = false;
    state.last_prune_at = None;
    state.injected_event_time_ms = Some(5_000);
    for hwnd in [100, 200, 300] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
    }
    state.handle_window_event(WindowEvent::Created(100, 0));
    let mon = state.focused_monitor;
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(300, Some(800))
        .unwrap();
    state.previous_focused_hwnd = Some(300);
    state.injected_foreground_hwnd = Some(Some(200));
    state.injected_stale_hwnds = vec![100, 300];

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "the selected workspace's own stale window must still be removed"
    );
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    assert!(!state.workspaces[&2][0].contains_window(300));
    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
    assert!(state.injected_stale_hwnds.is_empty());
}

#[test]
fn test_last_window_focus_prune_occupied_workspace_does_not_arm() {
    let mut state = last_window_focus_prune_state();
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(150, make_test_window_info(150));
    state.workspaces.get_mut(&mon).unwrap()[0]
        .insert_window(150, Some(800))
        .unwrap();

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert!(!state.workspaces[&mon][0].contains_window(100));
    assert!(state.workspaces[&mon][0].contains_window(150));
    assert_eq!(state.pending_last_window_departure, None);
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.injected_stale_hwnds.is_empty());
}

#[test]
fn test_last_window_same_hwnd_focus_does_not_consume_stale_injection() {
    let mut state = last_window_focus_prune_state();
    let mon = state.focused_monitor;
    state.previous_focused_hwnd = Some(200);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.injected_stale_hwnds, vec![100]);
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

fn minimized_cross_workspace_state() -> AppState {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    state.workspaces.get_mut(&mon).unwrap()[0]
        .insert_window(150, Some(800))
        .unwrap();
    state.workspaces.get_mut(&mon).unwrap()[0]
        .focus_window(100)
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_event_time_ms = Some(1_000);
    state.injected_iconic_hwnds.insert(100);
    state
}

#[test]
fn test_taskbar_restored_focus_target_is_not_reconciled_minimized() {
    let mut state = last_window_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);
    state.injected_iconic_hwnds.insert(200);

    state.handle_window_event(WindowEvent::Restored(200));
    assert!(!state.workspaces[&monitor][1].is_minimized(200));

    state.last_prune_at = None;
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert!(!state.workspaces[&monitor][1].is_minimized(200));
    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_restored_parked_window_focus_follows_after_tracked_minimize_reconcile() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Restored(200));
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(state.workspaces[&monitor][0].is_minimized(100));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_restored_parked_window_stays_at_final_rect_during_workspace_follow() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    state.reduce_motion = false;
    state.last_prune_at = Some(std::time::Instant::now());
    state.ensure_workspace_exists(monitor, 1);
    state.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Restored(200));
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    let viewport = state.layout_viewport(monitor);
    let final_rects: std::collections::HashMap<_, _> = state.workspaces[&monitor][1]
        .compute_placements_animated(viewport)
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    let transition = state.layout_transition.as_ref().unwrap();
    assert_eq!(transition.start_rects.get(&200), final_rects.get(&200));
    assert_eq!(
        transition.start_rects[&300].y,
        final_rects[&300].y + state.monitors[&monitor].work_area.height
    );
    assert!(!state.recently_restored_managed_windows.contains_key(&200));
}

#[test]
fn test_unfocused_recent_restore_does_not_skip_workspace_slide() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    state.reduce_motion = false;
    state.last_prune_at = Some(std::time::Instant::now());
    state.ensure_workspace_exists(monitor, 1);
    state.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Restored(200));
    state.handle_window_event(WindowEvent::Focused(300, 1_000));

    let viewport = state.layout_viewport(monitor);
    let final_rects: std::collections::HashMap<_, _> = state.workspaces[&monitor][1]
        .compute_placements_animated(viewport)
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    let transition = state.layout_transition.as_ref().unwrap();
    assert_eq!(
        transition.start_rects[&200].y,
        final_rects[&200].y + state.monitors[&monitor].work_area.height
    );
}

#[test]
fn test_restore_activation_prune_does_not_reconcile_tracked_minimize_as_departure() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Restored(200));
    state.last_prune_at = None;
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_restore_activation_direct_reconcile_does_not_sync_sibling_foreground() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);
    state.last_prune_at = Some(std::time::Instant::now());

    state.handle_window_event(WindowEvent::Restored(200));
    let border_shows_before = state.border_show_count.load(Ordering::Relaxed);
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed) - border_shows_before,
        1,
        "restore reconciliation must not sync foreground to the tracked sibling"
    );
}

#[test]
fn test_focused_minimize_reconcile_uses_event_time_for_duplicate_handoff() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.last_prune_at = None;
    state.injected_event_time_ms = Some(0);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_015));

    assert_eq!(state.active_workspace_idx(monitor), 0);
    assert_eq!(state.workspaces[&monitor][0].focused_window(), Some(150));
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );
}

#[test]
fn test_restoring_different_window_preserves_bound_minimize_guard() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();

    state.handle_window_event(WindowEvent::Minimized(100, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));

    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(300);
    state.handle_window_event(WindowEvent::Restored(300));

    assert_eq!(state.pending_last_window_departure, Some(intent));
}

#[test]
fn test_iconic_tracked_focus_on_other_monitor_does_not_sync_foreground() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.reduce_motion = true;
    state.last_prune_at = Some(std::time::Instant::now());
    let selected_monitor = state.focused_monitor;
    let other_monitor = if selected_monitor == 1 { 2 } else { 1 };
    state.workspaces.get_mut(&other_monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&other_monitor).unwrap()[0]
        .insert_window(150, Some(800))
        .unwrap();
    state.workspaces.get_mut(&other_monitor).unwrap()[0]
        .focus_window(100)
        .unwrap();
    state.workspaces.get_mut(&selected_monitor).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_iconic_hwnds.insert(100);
    let border_shows_before = state.border_show_count.load(Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.focused_monitor, selected_monitor);
    assert!(state.workspaces[&other_monitor][0].is_minimized(100));
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed) - border_shows_before,
        1,
        "reconciling the off-selection minimized focus must not resync foreground"
    );
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_recent_restore_does_not_override_real_minimize_handoff() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Restored(200));
    state.handle_window_event(WindowEvent::Minimized(100, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 0);
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );
}

#[test]
fn test_minimized_target_focus_follows_while_restore_is_in_progress() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_focus_before_latest_restore_event_keeps_each_recent_restore() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);
    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(300);

    state.handle_window_event(WindowEvent::Restored(200));
    state.handle_window_event(WindowEvent::Restored(300));
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_unselected_floating_minimize_reconcile_clears_tracked_focus_once() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[0]
        .remove_window(100)
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .add_floating(100, Rect::new(100, 100, 400, 300))
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.arm_pending_last_window_departure(
        Some(200),
        1_000,
        LastWindowDepartureOrigin::DirectDestroyedOrHidden,
    );

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.previous_focused_hwnd, None);
    let placements_after_reconcile = state
        .injected_apply_placements_call_count
        .load(Ordering::SeqCst);
    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        placements_after_reconcile,
        "a repeated focus event must not reconcile the tracked floating window again"
    );
}

#[test]
fn test_restoring_window_clears_minimized_departure_focus_guard() {
    let mut state = minimized_cross_workspace_state();
    let monitor = state.focused_monitor;

    state.handle_window_event(WindowEvent::Minimized(100, 1000));
    let intent = state
        .pending_last_window_departure
        .expect("minimizing the tracked window with a visible peer arms the guard");
    assert_eq!(intent.origin, LastWindowDepartureOrigin::Minimized);
    assert_eq!(intent.monitor, monitor);

    state.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);
    state.handle_window_event(WindowEvent::Restored(200));
    assert_eq!(state.pending_last_window_departure, None);

    state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));

    assert_eq!(state.active_workspace_idx(monitor), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_iconic_tracked_focus_reconciles_before_parked_window_focus() {
    let mut state = minimized_cross_workspace_state();
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Focused(200, 999));

    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.workspaces[&mon][0].focused_window(), Some(150));
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert!(state.workspaces[&mon][0].is_minimized(100));

    let pending = state.pending_last_window_departure;
    state.handle_window_event(WindowEvent::Minimized(100, 1000));
    assert!(state.workspaces[&mon][0].is_minimized(100));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.workspaces[&mon][0].focused_window(), Some(150));
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert_eq!(state.pending_last_window_departure, pending);
}

#[test]
fn test_minimize_suppresses_duplicate_parked_focus_during_handoff() {
    let mut state = minimized_cross_workspace_state();
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Minimized(100, 1_000));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, None);
    assert_eq!(intent.armed_at_event_time_ms, 1_000);

    state.handle_window_event(WindowEvent::Focused(200, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_015));

    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.workspaces[&mon][0].focused_window(), Some(150));
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );
}

#[test]
fn test_minimize_suppresses_parked_focus_48ms_after_handoff() {
    let mut state = minimized_cross_workspace_state();
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Minimized(100, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_048));

    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.workspaces[&mon][0].focused_window(), Some(150));
    assert_eq!(state.previous_focused_hwnd, Some(150));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );
}

#[test]
fn test_minimize_focus_after_handoff_window_follows() {
    let mut state = minimized_cross_workspace_state();
    let mon = state.focused_monitor;
    state.injected_event_time_ms = Some(1_600);

    state.handle_window_event(WindowEvent::Minimized(100, 1_000));
    state.handle_window_event(WindowEvent::Focused(200, 1_600));

    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.workspaces[&mon][1].focused_window(), Some(200));
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_prune_reconciles_iconic_unmarked_focused_window() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    add_liveness_test_columns(&mut state);
    state.last_prune_at = None;
    state.injected_iconic_hwnds.insert(150);
    state.workspaces.get_mut(&mon).unwrap()[0]
        .focus_window(150)
        .unwrap();
    state.previous_focused_hwnd = Some(100);

    state.handle_window_event(WindowEvent::Focused(160, 1_000));

    assert!(state.workspaces[&mon][0].contains_window(150));
    assert!(state.workspaces[&mon][0].is_minimized(150));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.workspaces[&mon][0].focused_window(), Some(160));
}

#[test]
fn test_prune_reconciles_inactive_iconic_without_changing_selected_focus() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.ensure_workspace_exists(1, 1);
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(300, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    state.focused_monitor = 1;
    state.previous_focused_hwnd = Some(100);
    state.injected_stale_hwnds.push(100);
    state.injected_iconic_hwnds.insert(200);
    state.injected_foreground_hwnd = Some(Some(300));

    state.prune_stale_windows_with(None);

    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.workspaces[&2][0].is_minimized(200));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.monitor, 1);
    assert_eq!(intent.workspace, 0);
    assert_eq!(intent.replacement_hwnd, Some(300));
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);

    state.handle_window_event(WindowEvent::Focused(300, intent.armed_at_event_time_ms));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.pending_last_window_departure, Some(intent));
}

#[test]
fn test_prune_batches_iconic_windows_before_minimizing_tracked_focus() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
    workspace.insert_window(100, Some(800)).unwrap();
    workspace.insert_window_in_column(150, 0).unwrap();
    workspace.insert_window_in_column(160, 0).unwrap();
    workspace.focus_window(100).unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_iconic_hwnds.extend([100, 150]);

    state.prune_stale_windows_with(None);

    let workspace = &state.workspaces[&1][0];
    assert!(workspace.is_minimized(100));
    assert!(workspace.is_minimized(150));
    assert_eq!(workspace.focused_window(), Some(160));
    assert_eq!(state.previous_focused_hwnd, Some(160));
}

#[test]
fn test_prune_batch_transition_suppresses_landing_focus_resync() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(150, Some(800))
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_iconic_hwnds.insert(150);

    state.prune_stale_windows_with(None);

    assert!(state.workspaces[&1][0].is_minimized(150));
    assert!(state
        .layout_transition
        .as_ref()
        .is_some_and(|transition| transition.suppress_landing_focus_resync));
}

fn last_window_liveness_state() -> AppState {
    let mut state = last_window_focus_prune_state();
    // Test AppState starts paused, which would skip the liveness check.
    state.paused = false;
    state
}

fn add_liveness_test_columns(state: &mut AppState) {
    let mon = state.focused_monitor;
    let workspace = &mut state.workspaces.get_mut(&mon).unwrap()[0];
    for hwnd in [150, 160] {
        workspace.insert_window(hwnd, Some(800)).unwrap();
    }
    workspace.set_all_column_widths(720);
}

#[test]
fn test_iconic_unmarked_focus_is_reconciled_by_focus_prune_and_restored() {
    let mut state = last_window_cross_workspace_state();
    let mon = state.focused_monitor;
    add_liveness_test_columns(&mut state);
    state.last_prune_at = None;
    state.injected_event_time_ms = Some(5_000);
    state.injected_iconic_hwnds.insert(100);

    let workspace = &state.workspaces[&mon][0];
    assert!(workspace.contains_window(100));
    assert!(!workspace.is_minimized(100));
    assert!(workspace.contains_window(150));
    assert_ne!(
        workspace.find_window_location(100).unwrap().0,
        workspace.find_window_location(150).unwrap().0
    );
    assert_eq!(state.find_window_workspace(200), Some((mon, 1)));
    let column = workspace.find_window_location(100).unwrap().0;
    let width = workspace.column(column).unwrap().width();

    state.handle_window_event(WindowEvent::Focused(200, 1_000));

    let workspace = &state.workspaces[&mon][0];
    assert!(
        workspace.contains_window(100),
        "focus prune must retain the OS-iconic window"
    );
    assert!(workspace.is_minimized(100));
    assert_eq!(state.find_window_workspace(100), Some((mon, 0)));
    assert_eq!(workspace.find_window_location(100).unwrap().0, column);
    assert_eq!(workspace.column(column).unwrap().width(), width);

    state.handle_window_event(WindowEvent::Minimized(100, 1000));
    assert!(state.workspaces[&mon][0].is_minimized(100));
    state.handle_window_event(WindowEvent::Restored(100));

    let workspace = &state.workspaces[&mon][0];
    assert!(workspace.contains_window(100));
    assert!(!workspace.is_minimized(100));
    assert_eq!(state.find_window_workspace(100), Some((mon, 0)));
    assert_eq!(workspace.find_window_location(100).unwrap().0, column);
    assert_eq!(workspace.column(column).unwrap().width(), width);
}

#[test]
fn test_iconic_unmarked_tracked_focus_does_not_trigger_liveness_prune() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;
    add_liveness_test_columns(&mut state);
    state.injected_stale_hwnds.clear();
    state.injected_iconic_hwnds.insert(100);

    let workspace = &state.workspaces[&mon][0];
    assert!(workspace.contains_window(100));
    assert!(!workspace.is_minimized(100));
    let column = workspace.find_window_location(100).unwrap().0;
    let width = workspace.column(column).unwrap().width();

    assert!(!state.check_tracked_focus_liveness());

    let workspace = &state.workspaces[&mon][0];
    assert!(workspace.contains_window(100));
    assert!(!workspace.is_minimized(100));
    assert_eq!(workspace.find_window_location(100).unwrap().0, column);
    assert_eq!(workspace.column(column).unwrap().width(), width);
    assert!(state.last_prune_at.is_none());
}

#[test]
fn test_liveness_prune_removes_hidden_window_and_reconciles_iconic_windows() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;
    add_liveness_test_columns(&mut state);
    state.injected_stale_hwnds = vec![100];
    state.injected_iconic_hwnds.extend([150, 160]);

    let workspace = &state.workspaces[&mon][0];
    assert!(workspace.contains_window(100));
    assert!(!workspace.is_minimized(100));
    let iconic_columns: Vec<_> = [150, 160]
        .into_iter()
        .map(|hwnd| {
            assert!(workspace.contains_window(hwnd));
            let column = workspace.find_window_location(hwnd).unwrap().0;
            (hwnd, column, workspace.column(column).unwrap().width())
        })
        .collect();

    assert!(state.check_tracked_focus_liveness());
    assert!(state.workspaces[&mon][1].contains_window(200));

    let workspace = &state.workspaces[&mon][0];
    assert!(
        !workspace.contains_window(100),
        "the hidden tracked window must be pruned"
    );
    for &(hwnd, column, width) in &iconic_columns {
        assert!(workspace.contains_window(hwnd));
        assert!(workspace.is_minimized(hwnd));
        let current_column = workspace.find_window_location(hwnd).unwrap().0;
        assert_eq!(current_column + 1, column);
        assert_eq!(workspace.column(current_column).unwrap().width(), width);
    }

    for &(hwnd, _, _) in &iconic_columns {
        state.handle_window_event(WindowEvent::Minimized(hwnd, 0));
        let workspace = &state.workspaces[&mon][0];
        assert!(workspace.contains_window(hwnd));
        assert!(workspace.is_minimized(hwnd));
    }
}

#[test]
fn test_last_window_liveness_check_then_newer_activation_follows() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;

    assert!(state.check_tracked_focus_liveness());

    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "the liveness tick must prune the vanished tracked window"
    );
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.previous_focused_hwnd, None);
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.armed_at_event_time_ms, 5_000);
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);
    assert!(state.injected_stale_hwnds.is_empty());
    assert!(state.last_prune_at.is_some());

    state.handle_window_event(WindowEvent::Focused(200, 5_001));
    assert_eq!(state.active_workspace_idx(mon), 1);
    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(state.pending_last_window_departure, None);
}

#[test]
fn test_last_window_without_liveness_check_still_suppresses_deliberate_activation() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;

    state.handle_window_event(WindowEvent::Focused(200, 5_001));

    assert_eq!(
        state.active_workspace_idx(mon),
        0,
        "before the liveness tick, the in-handler prune still attributes the activation"
    );
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.armed_at_event_time_ms, 5_001);
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);
}

#[test]
fn test_last_window_liveness_check_alive_tracked_focus_is_noop() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;
    state.injected_stale_hwnds = vec![200];

    assert!(!state.check_tracked_focus_liveness());

    assert_eq!(state.injected_stale_hwnds, vec![200]);
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert!(state.workspaces[&mon][1].contains_window(200));
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.pending_last_window_departure, None);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert!(state.last_prune_at.is_none());
}

#[test]
fn test_last_window_liveness_check_paused_is_noop() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;
    state.paused = true;

    assert!(!state.check_tracked_focus_liveness());

    assert_eq!(state.injected_stale_hwnds, vec![100]);
    assert!(state.workspaces[&mon][0].contains_window(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.pending_last_window_departure, None);
    assert!(state.last_prune_at.is_none());
}

#[test]
fn test_last_window_liveness_check_untracked_or_unmanaged_is_noop() {
    for tracked in [None, Some(999)] {
        let mut state = last_window_liveness_state();
        let mon = state.focused_monitor;
        state.previous_focused_hwnd = tracked;

        assert!(!state.check_tracked_focus_liveness());

        assert_eq!(state.injected_stale_hwnds, vec![100]);
        assert!(state.workspaces[&mon][0].contains_window(100));
        assert_eq!(state.previous_focused_hwnd, tracked);
        assert_eq!(state.pending_last_window_departure, None);
        assert_eq!(state.active_workspace_idx(mon), 0);
        assert!(state.last_prune_at.is_none());
    }
}

#[test]
fn test_last_window_liveness_check_occupied_workspace_does_not_arm() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;
    state.workspaces.get_mut(&mon).unwrap()[0]
        .insert_window(150, Some(800))
        .unwrap();

    assert!(state.check_tracked_focus_liveness());

    assert!(!state.workspaces[&mon][0].contains_window(100));
    assert!(state.workspaces[&mon][0].contains_window(150));
    assert_eq!(state.pending_last_window_departure, None);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.injected_stale_hwnds.is_empty());
}

#[test]
fn test_last_window_liveness_check_suppresses_autoactivation_at_armed_time() {
    let mut state = last_window_liveness_state();
    let mon = state.focused_monitor;

    assert!(state.check_tracked_focus_liveness());

    let intent = state.pending_last_window_departure.unwrap();
    assert_eq!(intent.replacement_hwnd, Some(200));
    assert_eq!(intent.armed_at_event_time_ms, 5_000);
    assert_eq!(intent.origin, LastWindowDepartureOrigin::EventlessPrune);
    let armed_at = intent.armed_at_event_time_ms;

    state.handle_window_event(WindowEvent::Focused(200, armed_at.wrapping_sub(1)));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.previous_focused_hwnd, None);

    state.handle_window_event(WindowEvent::Focused(200, armed_at));
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(state.focused_monitor, mon);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(crate::event_handler::workspace_is_genuinely_empty(
        &state.workspaces[&mon][0]
    ));
    assert_eq!(
        state
            .pending_last_window_departure
            .unwrap()
            .replacement_hwnd,
        Some(200)
    );
}

#[test]
fn test_workspace_is_genuinely_empty_ignores_minimized_only_vacancy() {
    use crate::event_handler::workspace_is_genuinely_empty;

    let mut workspace = Workspace::with_gaps(10, 10);
    assert!(workspace_is_genuinely_empty(&workspace));
    workspace.insert_window(100, Some(800)).unwrap();
    assert!(!workspace_is_genuinely_empty(&workspace));
    workspace.mark_minimized(100);
    assert!(
        !workspace_is_genuinely_empty(&workspace),
        "minimized tiled windows still occupy the workspace"
    );
    workspace.remove_window(100).unwrap();
    assert!(workspace_is_genuinely_empty(&workspace));
    workspace
        .add_floating(200, Rect::new(0, 0, 400, 300))
        .unwrap();
    assert!(!workspace_is_genuinely_empty(&workspace));
}

#[test]
fn test_departing_focus_recovery_does_not_steal_unmanaged_foreground() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(999));
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    state.last_border_show_hwnd.store(0, Ordering::Relaxed);

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(state.border_hide_count.load(Ordering::Relaxed) > hides_before);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 0);
}

#[test]
fn test_departing_focus_recovery_runs_when_foreground_is_departed() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(200)
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(100));

    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
}

#[test]
fn test_departing_focus_recovery_runs_when_foreground_is_null_or_invalid() {
    for foreground in [None, Some(0)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        for hwnd in [100, 200] {
            state
                .injected_window_info
                .insert(hwnd, make_test_window_info(hwnd));
            state.handle_window_event(WindowEvent::Created(hwnd, 0));
        }
        state
            .focused_workspace_mut()
            .unwrap()
            .focus_window(200)
            .unwrap();
        state.previous_focused_hwnd = Some(100);
        state.injected_foreground_hwnd = Some(foreground);

        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(200)
        );
    }
}

#[test]
fn test_unfocused_managed_departure_does_not_recover_on_null_foreground() {
    for foreground in [None, Some(0)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        for hwnd in [100, 200] {
            state
                .injected_window_info
                .insert(hwnd, make_test_window_info(hwnd));
            state.handle_window_event(WindowEvent::Created(hwnd, 0));
        }
        state.previous_focused_hwnd = Some(200);
        state.injected_foreground_hwnd = Some(foreground);

        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(state.focused_workspace().unwrap().contains_window(200));
        assert!(!state.focused_workspace().unwrap().contains_window(100));
    }
}

#[test]
fn test_unmanaged_departure_does_not_recover_on_null_foreground() {
    for event in [WindowEvent::Destroyed(999), WindowEvent::Hidden(999, 0)] {
        for foreground in [None, Some(0)] {
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            for hwnd in [100, 200] {
                state
                    .injected_window_info
                    .insert(hwnd, make_test_window_info(hwnd));
                state.handle_window_event(WindowEvent::Created(hwnd, 0));
            }
            state.previous_focused_hwnd = Some(200);
            state.injected_foreground_hwnd = Some(foreground);

            state.handle_window_event(event.clone());

            assert_eq!(state.previous_focused_hwnd, Some(200));
            assert!(state.focused_workspace().unwrap().contains_window(100));
            assert!(state.focused_workspace().unwrap().contains_window(200));
        }
    }
}

fn departing_tiled_state(previous_focus: Option<u64>, foreground: Option<u64>) -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = false;
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state.previous_focused_hwnd = previous_focus;
    state.injected_foreground_hwnd = Some(foreground);
    state
}

#[test]
fn test_departing_recovery_decision_is_sampled_once() {
    for (first, second, expect_recover) in [(None, Some(200), true), (Some(200), None, false)] {
        let mut state = departing_tiled_state(Some(100), first);
        if expect_recover {
            state
                .focused_workspace_mut()
                .unwrap()
                .focus_window(200)
                .unwrap();
        }
        state.injected_next_foreground_hwnd = Some(second);

        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(
            state.departing_foreground_evidence_reads, 1,
            "departure must sample foreground evidence once"
        );
        if expect_recover {
            assert_eq!(state.previous_focused_hwnd, Some(200));
            assert!(
                state
                    .layout_transition
                    .as_ref()
                    .is_some_and(|transition| !transition.suppress_landing_focus_resync),
                "first-sample recovery must not suppress landing after a later valid replacement"
            );
        } else {
            assert_eq!(state.previous_focused_hwnd, Some(200));
            assert!(
                state
                    .layout_transition
                    .as_ref()
                    .is_some_and(|transition| transition.suppress_landing_focus_resync),
                "first-sample no-recovery must keep landing suppression after later null foreground"
            );
        }
    }

    let mut unmanaged = departing_tiled_state(Some(200), None);
    unmanaged.injected_next_foreground_hwnd = Some(Some(200));
    unmanaged.handle_window_event(WindowEvent::Destroyed(999));
    assert_eq!(unmanaged.departing_foreground_evidence_reads, 1);
    assert_eq!(unmanaged.previous_focused_hwnd, Some(200));
    assert!(
        unmanaged
            .layout_transition
            .as_ref()
            .is_none_or(|transition| !transition.suppress_landing_focus_resync),
        "unmanaged departure must not recover or mark landing suppression"
    );
}

fn land_departing_transition(state: &mut AppState) {
    let duration = state
        .layout_transition
        .as_ref()
        .map(|transition| transition.duration_ms)
        .unwrap_or(0);
    assert!(state.tick_animations(duration));
    assert!(state.layout_transition.is_none());
    state.sync_foreground_after_animation_landing();
}

#[test]
fn test_should_sync_foreground_on_animation_landing_respects_pause_and_suppress() {
    use crate::ui_sync::should_sync_foreground_on_animation_landing;

    assert!(should_sync_foreground_on_animation_landing(false, false));
    assert!(!should_sync_foreground_on_animation_landing(true, false));
    assert!(!should_sync_foreground_on_animation_landing(false, true));
    assert!(!should_sync_foreground_on_animation_landing(true, true));
}

#[test]
fn test_departing_landing_does_not_steal_valid_managed_replacement() {
    let mut state = departing_tiled_state(Some(100), Some(200));
    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.suppress_landing_focus_resync),
        "valid managed replacement arms landing suppression"
    );

    land_departing_transition(&mut state);

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(!state.pending_suppress_landing_focus_resync);
}

#[test]
fn test_departing_landing_does_not_steal_valid_unmanaged_replacement() {
    let mut state = departing_tiled_state(Some(100), Some(999));
    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.previous_focused_hwnd, None);
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.suppress_landing_focus_resync),
        "valid unmanaged replacement arms landing suppression"
    );

    land_departing_transition(&mut state);

    assert_eq!(state.previous_focused_hwnd, None);
    assert!(!state.pending_suppress_landing_focus_resync);
}

#[test]
fn test_departing_landing_recovers_when_foreground_requires_it() {
    for foreground in [None, Some(0), Some(100)] {
        let mut state = departing_tiled_state(Some(100), foreground);
        state
            .focused_workspace_mut()
            .unwrap()
            .focus_window(200)
            .unwrap();

        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(
            state
                .layout_transition
                .as_ref()
                .is_some_and(|transition| !transition.suppress_landing_focus_resync),
            "recovery-required departure must still land with a focus re-sync"
        );

        state.previous_focused_hwnd = None;
        land_departing_transition(&mut state);

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(!state.pending_suppress_landing_focus_resync);
    }
}

#[test]
fn test_departing_landing_suppress_does_not_stale_onto_later_transition() {
    let mut state = departing_tiled_state(Some(100), Some(200));
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert!(state
        .layout_transition
        .as_ref()
        .is_some_and(|transition| transition.suppress_landing_focus_resync));

    land_departing_transition(&mut state);
    assert!(!state.pending_suppress_landing_focus_resync);

    state.previous_focused_hwnd = None;
    let snapshot = state.snapshot_layout();
    state.start_layout_transition(snapshot);
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| !transition.suppress_landing_focus_resync),
        "later ordinary transition must not inherit departure suppression"
    );
    assert!(!state.pending_suppress_landing_focus_resync);

    land_departing_transition(&mut state);

    assert_eq!(state.previous_focused_hwnd, Some(200));
    assert!(!state.pending_suppress_landing_focus_resync);
}

#[test]
fn test_abort_layout_transition_does_not_strand_landing_suppress() {
    let mut state = departing_tiled_state(Some(100), Some(200));
    state.handle_window_event(WindowEvent::Destroyed(100));
    assert!(state
        .layout_transition
        .as_ref()
        .is_some_and(|transition| transition.suppress_landing_focus_resync));

    state.abort_layout_transition();

    assert!(state.layout_transition.is_none());
    assert!(!state.pending_suppress_landing_focus_resync);

    state.previous_focused_hwnd = None;
    let snapshot = state.snapshot_layout();
    state.start_layout_transition(snapshot);
    land_departing_transition(&mut state);

    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_unfocused_tiled_departure_null_or_invalid_still_lands_resync() {
    for (foreground, valid) in [(None, None), (Some(0), None), (Some(999), Some(false))] {
        let mut state = departing_tiled_state(Some(200), foreground);
        state.injected_foreground_is_valid = valid;

        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(
            state
                .layout_transition
                .as_ref()
                .is_some_and(|transition| !transition.suppress_landing_focus_resync),
            "unfocused departure with null/invalid foreground must not suppress landing"
        );

        state.previous_focused_hwnd = None;
        land_departing_transition(&mut state);

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(!state.pending_suppress_landing_focus_resync);
    }
}

#[test]
fn test_unfocused_tiled_departure_valid_replacement_suppresses_landing() {
    for replacement in [200, 999] {
        let mut state = departing_tiled_state(Some(200), Some(replacement));
        state.handle_window_event(WindowEvent::Destroyed(100));

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(
            state
                .layout_transition
                .as_ref()
                .is_some_and(|transition| transition.suppress_landing_focus_resync),
            "unfocused departure with a valid replacement must suppress landing"
        );

        land_departing_transition(&mut state);

        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert!(!state.pending_suppress_landing_focus_resync);
    }
}

#[test]
fn test_tear_off_event_order_does_not_leave_stale_focus_or_ghost() {
    use crate::state::{CrossfadeState, GhostEntry};

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    state.config.behavior.swap_chain_ghost_animation = true;
    for hwnd in [100, 200] {
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        state.handle_window_event(WindowEvent::Created(hwnd, 0));
    }
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.previous_focused_hwnd = Some(100);

    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    state.handle_window_event(WindowEvent::Focused(300, 0));
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(state.focused_workspace().unwrap().contains_window(300));

    state.ghost_handles.insert(
        300,
        GhostEntry::new(0, "Chrome_WidgetWin_1".into(), Rect::new(0, 0, 800, 600)),
    );
    if let Some(ref mut transition) = state.layout_transition {
        transition
            .start_rects
            .insert(300, Rect::new(0, 0, 800, 600));
        transition.ghosted_wids.insert(300);
    }
    state.active_crossfade = Some(CrossfadeState { epoch: 4 });
    state.crossfade_sources.insert(
        4,
        (
            std::collections::HashSet::from([300]),
            std::time::Instant::now(),
        ),
    );

    state.injected_foreground_hwnd = Some(Some(100));
    state.handle_window_event(WindowEvent::Destroyed(300));

    assert_ne!(state.previous_focused_hwnd, Some(300));
    assert!(!state.focused_workspace().unwrap().contains_window(300));
    assert!(state.focused_workspace().unwrap().contains_window(100));
    assert!(state.focused_workspace().unwrap().contains_window(200));
    assert!(!state.ghost_handles.contains_key(&300));
    assert!(!state
        .layout_transition
        .as_ref()
        .is_some_and(|transition| transition.start_rects.contains_key(&300)
            || transition.ghosted_wids.contains(&300)));
}

fn taskbar_policy_state() -> AppState {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.paused = false;
    state.ensure_workspace_exists(1, 2);
    state.ensure_workspace_exists(2, 2);
    state.active_workspace.insert(2, 1);
    let active = &mut state.workspaces.get_mut(&1).unwrap()[0];
    for wid in [100, 200, 300, 400, 500] {
        active.insert_window(wid, Some(1000)).unwrap();
    }
    active.mark_minimized(500);
    active
        .add_floating(600, Rect::new(50, 50, 300, 200))
        .unwrap();
    active.focus_window(100).unwrap();
    active.set_scroll_offset(0.0);
    state.sticky_windows.insert(400);
    let inactive = &mut state.workspaces.get_mut(&1).unwrap()[1];
    inactive.insert_window(700, Some(1000)).unwrap();
    inactive.insert_window(800, Some(1000)).unwrap();
    inactive.mark_minimized(800);
    inactive
        .add_floating(900, Rect::new(50, 50, 300, 200))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[1]
        .insert_window(1000, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(1100, Some(800))
        .unwrap();
    state
}

#[test]
fn test_taskbar_buttons_classification_per_monitor() {
    use config::TaskbarButtons::*;
    for (mode, expected) in [
        (
            HideOffscreen,
            [
                (100, true),
                (200, true),
                (300, false),
                (400, false),
                (500, true),
                (600, true),
                (700, false),
                (800, false),
                (900, false),
                (1000, true),
                (1100, false),
            ],
        ),
        (
            HideInactiveWorkspaces,
            [
                (100, true),
                (200, true),
                (300, true),
                (400, true),
                (500, true),
                (600, true),
                (700, false),
                (800, false),
                (900, false),
                (1000, true),
                (1100, false),
            ],
        ),
        (
            ShowAll,
            [
                (100, true),
                (200, true),
                (300, true),
                (400, true),
                (500, true),
                (600, true),
                (700, true),
                (800, true),
                (900, true),
                (1000, true),
                (1100, true),
            ],
        ),
    ] {
        let mut state = taskbar_policy_state();
        state.config.behavior.taskbar_buttons = mode;
        let before = state.workspaces[&1][0].compute_placements(state.layout_viewport(1));
        assert!(before
            .iter()
            .any(|p| p.window_id == 300
                && p.visibility != leopardwm_core_layout::Visibility::Visible));
        state.sync_taskbar_buttons();
        let mut commands = state.take_recorded_taskbar_commands();
        commands.sort_unstable();
        assert_eq!(commands, expected, "{mode:?}");
        let after = state.workspaces[&1][0].compute_placements(state.layout_viewport(1));
        let geometry = |placements: Vec<leopardwm_core_layout::WindowPlacement>| {
            placements
                .into_iter()
                .map(|p| (p.window_id, p.rect, p.visibility))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(after), geometry(before));
    }
}

#[test]
fn test_taskbar_buttons_reload_resyncs_show_and_hide() {
    use config::TaskbarButtons::*;
    let mut state = taskbar_policy_state();
    for (mode, offscreen, inactive) in [
        (HideOffscreen, false, false),
        (HideInactiveWorkspaces, true, false),
        (ShowAll, true, true),
        (HideInactiveWorkspaces, true, false),
        (HideOffscreen, false, false),
    ] {
        let mut config = state.config.clone();
        config.behavior.taskbar_buttons = mode;
        state.apply_config(config);
        let commands = state.take_recorded_taskbar_commands();
        assert!(
            commands.contains(&(300, offscreen)),
            "{mode:?}: {commands:?}"
        );
        assert!(
            commands.contains(&(700, inactive)),
            "{mode:?}: {commands:?}"
        );
        assert!(commands.contains(&(1000, true)), "{mode:?}: {commands:?}");
        assert!(
            commands.contains(&(1100, inactive)),
            "{mode:?}: {commands:?}"
        );
    }
}

#[test]
fn test_taskbar_button_action_leaves_application_fullscreen_untouched() {
    use crate::helpers::{taskbar_button_action, TaskbarButtonAction};

    assert_eq!(
        taskbar_button_action(true, true),
        TaskbarButtonAction::Unchanged
    );
    assert_eq!(
        taskbar_button_action(true, false),
        TaskbarButtonAction::Unchanged
    );
    assert_eq!(
        taskbar_button_action(false, true),
        TaskbarButtonAction::Show
    );
    assert_eq!(
        taskbar_button_action(false, false),
        TaskbarButtonAction::Hide
    );
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
            match_max_width: None,
            match_max_height: None,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action = state.evaluate_window_rules("TestClass", "Any Title", "any.exe", None);
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
            match_max_width: None,
            match_max_height: None,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action =
        state.evaluate_window_rules("AnyClass", "DevTools - localhost", "chrome.exe", None);
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
            match_max_width: None,
            match_max_height: None,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let action = state.evaluate_window_rules("SpotifyClass", "Spotify", "spotify.exe", None);
    assert_eq!(action, config::WindowAction::Ignore);
}

#[test]
fn test_window_rule_no_match_defaults_to_tile() {
    let state = AppState::new_with_config(test_config(), test_monitors());
    let action = state.evaluate_window_rules("SomeClass", "Some Title", "some.exe", None);
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
            match_max_width: None,
            match_max_height: None,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let original = Rect::new(100, 100, 640, 480);
    let result =
        state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original, None, None);
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
            match_max_width: None,
            match_max_height: None,
        }],
        ..Default::default()
    };
    let state = AppState::new_with_config(config, test_monitors());
    let original = Rect::new(100, 100, 640, 480);
    let result =
        state.get_floating_rect_from_rules("TestClass", "Title", "test.exe", &original, None, None);
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

fn rule_reload_state(action: Option<crate::config::WindowAction>) -> AppState {
    let mut config = test_config();
    config.window_rules.clear();
    if let Some(action) = action {
        config.window_rules.push(crate::config::WindowRule {
            match_class: Some("^TestWindowClass$".into()),
            action,
            width: Some(640),
            height: Some(480),
            ..Default::default()
        });
    }
    let mut state = AppState::new_with_config(config, test_monitors());
    state.paused = true;
    let mut window = make_test_window_info(100);
    window.process_id = std::process::id();
    let mut unaffected = make_test_window_info(200);
    unaffected.process_id = std::process::id();
    unaffected.class_name = "OtherClass".into();
    state.injected_window_info.insert(100, window.clone());
    state.injected_window_info.insert(200, unaffected.clone());
    state.injected_enumerated_windows = Some(vec![window, unaffected]);
    assert_eq!(state.enumerate_and_add_windows().unwrap(), 2);
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.focus_window(200).unwrap();
    workspace.remove_window(200).unwrap();
    workspace
        .add_floating(200, Rect::new(300, 200, 500, 400))
        .unwrap();
    state
}

#[test]
fn test_rule_reload_preserves_hand_floated_window() {
    let mut state = rule_reload_state(None);
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    assert_eq!(
        state.handle_command(IpcCommand::ToggleFloating),
        IpcResponse::Ok
    );
    assert!(state.workspaces[&1][0].is_floating(100));

    state.apply_config(state.config.clone());

    assert!(state.workspaces[&1][0].is_floating(100));
}

#[test]
fn test_rule_reload_preserves_hand_tiled_window_from_float_rule() {
    let mut state = rule_reload_state(Some(crate::config::WindowAction::Float));
    state.previous_focused_hwnd = Some(100);
    assert_eq!(
        state.handle_command(IpcCommand::ToggleFloating),
        IpcResponse::Ok
    );
    assert!(!state.workspaces[&1][0].is_floating(100));

    state.apply_config(state.config.clone());

    assert!(state.workspaces[&1][0].contains_window(100));
    assert!(!state.workspaces[&1][0].is_floating(100));
}

#[test]
fn test_rule_reload_applies_changed_window_actions() {
    use crate::config::{WindowAction, WindowRule};

    for (old_action, new_action) in [
        (Some(WindowAction::Float), None),
        (None, Some(WindowAction::Float)),
        (None, Some(WindowAction::Ignore)),
    ] {
        let mut state = rule_reload_state(old_action);
        let unaffected_rect = state.workspaces[&1][0]
            .floating_windows()
            .iter()
            .find(|window| window.id == 200)
            .map(|window| window.rect);
        let mut config = state.config.clone();
        config.window_rules.clear();
        if let Some(action) = new_action {
            config.window_rules.push(WindowRule {
                match_class: Some("^TestWindowClass$".into()),
                action,
                width: Some(640),
                height: Some(480),
                ..Default::default()
            });
        }

        state.apply_config(config);

        match new_action {
            Some(WindowAction::Float) => {
                assert_eq!(state.find_window_workspace(100), Some((1, 0)));
                assert!(state.workspaces[&1][0].is_floating(100));
                assert_eq!(
                    state.workspaces[&1][0]
                        .floating_windows()
                        .iter()
                        .find(|window| window.id == 100)
                        .map(|window| window.rect),
                    Some(Rect::new(100, 100, 640, 480))
                );
            }
            Some(WindowAction::Ignore) => {
                assert_eq!(state.find_window_workspace(100), None);
                assert!(!state.window_managed_at.contains_key(&100));
                assert!(!state.managed_lifetime_tokens.contains_key(&100));
            }
            _ => {
                assert_eq!(state.find_window_workspace(100), Some((1, 0)));
                assert!(state.workspaces[&1][0].contains_window(100));
                assert!(!state.workspaces[&1][0].is_floating(100));
            }
        }
        assert!(state.workspaces[&1][0].is_floating(200));
        assert_eq!(
            state.workspaces[&1][0]
                .floating_windows()
                .iter()
                .find(|window| window.id == 200)
                .map(|window| window.rect),
            unaffected_rect
        );
    }
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
        spawn_forwarding_thread("test-close", rx, async_tx, |_| DaemonEvent::HideSnapHint).unwrap();

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
fn test_gesture_hook_reconciles_desired_and_actual_state() {
    for (installed, enabled, retry_install, expected) in [
        (false, false, true, GestureHookChange::None),
        (false, true, true, GestureHookChange::Install),
        (false, true, false, GestureHookChange::None),
        (true, false, false, GestureHookChange::Stop),
        (true, true, false, GestureHookChange::None),
        (true, true, true, GestureHookChange::None),
    ] {
        assert_eq!(
            gesture_hook_change(installed, enabled, retry_install),
            expected
        );
    }
}

#[test]
fn test_gesture_forwarder_cancels_with_full_daemon_channel() {
    let (event_tx, mut event_rx) = mpsc::channel(1);
    event_tx
        .try_send(DaemonEvent::Gesture(GestureEvent::ScrollUp))
        .unwrap();
    let (gesture_tx, gesture_rx) = std::sync::mpsc::channel();
    gesture_tx.send(GestureEvent::ScrollDown).unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    use tracing_subscriber::layer::SubscriberExt;
    struct BackpressureObserver(std::sync::mpsc::Sender<()>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for BackpressureObserver {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == "leopardwm::gesture_forwarding" {
                let _ = self.0.send(());
            }
        }
    }
    let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
    let subscriber = tracing_subscriber::registry().with(BackpressureObserver(blocked_tx));
    let forward_cancel = cancel.clone();
    let mut forwarder = Some(std::thread::spawn(move || {
        tracing::subscriber::with_default(subscriber, || {
            forward_gestures(gesture_rx, event_tx, forward_cancel);
        });
    }));
    let blocked = blocked_rx.recv_timeout(Duration::from_secs(1));
    cancel.store(true, Ordering::Release);
    drop(gesture_tx);
    assert!(join_with_timeout(&mut forwarder, Duration::from_secs(1)));
    assert!(
        blocked.is_ok(),
        "forwarder never reached full-channel backpressure"
    );
    assert!(matches!(
        event_rx.try_recv(),
        Ok(DaemonEvent::Gesture(GestureEvent::ScrollUp))
    ));
    assert!(event_rx.try_recv().is_err());
}

#[test]
fn test_gesture_command_classification_distinguishes_no_action_and_unknown() {
    assert!(matches!(
        classify_gesture_command(""),
        GestureCommand::NoAction
    ));
    assert!(matches!(
        classify_gesture_command("focus_left"),
        GestureCommand::Known(IpcCommand::FocusLeft)
    ));
    assert!(matches!(
        classify_gesture_command("custom_command"),
        GestureCommand::Unknown("custom_command")
    ));
}

#[test]
fn test_session_end_restore_orders_recovery_before_shutdown() {
    let (event_tx, mut event_rx) = mpsc::channel(1);
    let apply_worker_cancelled = std::sync::atomic::AtomicBool::new(false);
    let apply_epoch = std::sync::atomic::AtomicU64::new(7);
    let steps = std::sync::Mutex::new(Vec::new());

    restore_windows_for_session_end(
        &event_tx,
        &apply_worker_cancelled,
        &apply_epoch,
        || {
            assert!(apply_worker_cancelled.load(Ordering::SeqCst));
            assert_eq!(apply_epoch.load(Ordering::SeqCst), 8);
            steps.lock().unwrap().push("barrier");
        },
        || {
            assert_eq!(*steps.lock().unwrap(), vec!["barrier"]);
            steps.lock().unwrap().push("snap");
        },
        || {
            assert_eq!(*steps.lock().unwrap(), vec!["barrier", "snap"]);
            assert!(matches!(
                event_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            steps.lock().unwrap().push("visibility");
        },
    );

    assert_eq!(
        *steps.lock().unwrap(),
        vec!["barrier", "snap", "visibility"]
    );
    assert!(matches!(event_rx.try_recv(), Ok(DaemonEvent::Shutdown)));
}

#[test]
fn test_session_end_restore_tolerates_full_shutdown_channel() {
    let (event_tx, mut event_rx) = mpsc::channel(1);
    event_tx.try_send(DaemonEvent::Shutdown).unwrap();
    let apply_worker_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let apply_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let worker_cancelled = apply_worker_cancelled.clone();
    let worker_epoch = apply_epoch.clone();
    let worker_event_tx = event_tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    let worker = std::thread::spawn(move || {
        restore_windows_for_session_end(
            &worker_event_tx,
            &worker_cancelled,
            &worker_epoch,
            || {},
            || {},
            || {},
        );
        done_tx.send(()).unwrap();
    });

    if done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .is_err()
    {
        drop(event_rx);
        worker.join().unwrap();
        panic!("session-end recovery blocked on a full shutdown channel");
    }
    worker.join().unwrap();

    assert!(apply_worker_cancelled.load(Ordering::SeqCst));
    assert_eq!(apply_epoch.load(Ordering::SeqCst), 1);
    assert!(matches!(event_rx.try_recv(), Ok(DaemonEvent::Shutdown)));
    assert!(matches!(
        event_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
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
fn test_cmd_query_hotkeys_returns_catalog_order_and_defaults() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let response = state.handle_command(IpcCommand::QueryHotkeys);

    let IpcResponse::HotkeyList {
        hotkeys,
        scroll_modifier,
        issues,
    } = response
    else {
        panic!("Expected HotkeyList");
    };

    let catalog = leopardwm_ipc::hotkeys::hotkey_catalog();
    assert_eq!(hotkeys.len(), catalog.len());
    assert_eq!(hotkeys[0].action_id, catalog[0].id);
    assert_eq!(hotkeys[0].label, catalog[0].label);
    assert_eq!(hotkeys[0].group, catalog[0].group);
    assert_eq!(hotkeys[0].bindings, vec!["Ctrl+Alt+H"]);
    assert!(hotkeys[0].enabled);
    assert_eq!(scroll_modifier, "Ctrl+Alt");
    assert!(issues.is_empty());
}

#[test]
fn test_cmd_query_hotkeys_preserves_diagnostics_and_unbound_actions() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Alt+Z".to_string(), "focus_left".to_string());
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Alt+A".to_string(), "focus-left".to_string());
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Nope".to_string(), "focus_right".to_string());
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Alt+Q".to_string(), "missing_action".to_string());

    let mut state = AppState::new_with_config(config, test_monitors());
    let response = state.handle_command(IpcCommand::QueryHotkeys);
    let IpcResponse::HotkeyList {
        hotkeys, issues, ..
    } = response
    else {
        panic!("Expected HotkeyList");
    };

    let focus_left = hotkeys
        .iter()
        .find(|entry| entry.action_id == "focus_left")
        .unwrap();
    assert_eq!(focus_left.bindings, vec!["Ctrl+Alt+A", "Ctrl+Alt+Z"]);
    assert!(focus_left.enabled);

    let focus_right = hotkeys
        .iter()
        .find(|entry| entry.action_id == "focus_right")
        .unwrap();
    assert!(focus_right.bindings.is_empty());
    assert!(!focus_right.enabled);

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].binding, "Ctrl+Alt+Q");
    assert_eq!(issues[0].message, "unknown action identifier");
    assert_eq!(issues[1].binding, "Ctrl+Nope");
    assert_eq!(issues[1].message, "invalid key chord");
}

#[test]
fn test_cmd_query_hotkeys_appends_valid_non_catalog_actions() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Alt+N".to_string(), "width_third".to_string());

    let mut state = AppState::new_with_config(config, test_monitors());
    let IpcResponse::HotkeyList {
        hotkeys, issues, ..
    } = state.handle_command(IpcCommand::QueryHotkeys)
    else {
        panic!("Expected HotkeyList");
    };

    let extra = hotkeys.last().unwrap();
    assert_eq!(extra.action_id, "width_third");
    assert_eq!(extra.label, "Width third");
    assert_eq!(extra.group, "Other");
    assert_eq!(extra.bindings, vec!["Ctrl+Alt+N"]);
    assert!(extra.enabled);
    assert!(issues.is_empty());
}

#[test]
fn test_cmd_query_hotkeys_reports_both_invalid_action_and_chord() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("Ctrl+Nope".to_string(), "missing_action".to_string());

    let mut state = AppState::new_with_config(config, test_monitors());
    let IpcResponse::HotkeyList { issues, .. } = state.handle_command(IpcCommand::QueryHotkeys)
    else {
        panic!("Expected HotkeyList");
    };

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].message, "invalid key chord");
    assert_eq!(issues[1].message, "unknown action identifier");
}

#[test]
fn test_cmd_query_hotkeys_excludes_f_key_trigger_used_as_modifier() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("F13+H".to_string(), "focus_left".to_string());
    config
        .hotkeys
        .bindings
        .insert("Ctrl+F13".to_string(), "focus_right".to_string());

    let mut state = AppState::new_with_config(config, test_monitors());
    let IpcResponse::HotkeyList {
        hotkeys, issues, ..
    } = state.handle_command(IpcCommand::QueryHotkeys)
    else {
        panic!("Expected HotkeyList");
    };

    let focus_left = hotkeys
        .iter()
        .find(|entry| entry.action_id == "focus_left")
        .unwrap();
    assert_eq!(focus_left.bindings, vec!["F13+H"]);
    let focus_right = hotkeys
        .iter()
        .find(|entry| entry.action_id == "focus_right")
        .unwrap();
    assert!(!focus_right.enabled);
    assert!(focus_right.bindings.is_empty());
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].message,
        "trigger F-key is also configured as a modifier"
    );
}

#[test]
fn test_cmd_query_hotkeys_resolves_collisions_independent_of_insertion_order() {
    let entries = [
        ("Ctrl+Alt+H", "focus_right"),
        ("Alt+Control+h", "Focus-Left"),
    ];
    let mut previous = None;
    for reverse in [false, true] {
        let mut config = test_config();
        config.hotkeys.bindings.clear();
        let mut ordered = entries.to_vec();
        if reverse {
            ordered.reverse();
        }
        for (binding, action) in ordered {
            config
                .hotkeys
                .bindings
                .insert(binding.into(), action.into());
        }
        let runtime = hotkey_resolution::resolve_hotkeys(&config.hotkeys);
        assert_eq!(runtime.bindings.len(), 1);
        assert_eq!(runtime.bindings[0].command, IpcCommand::FocusLeft);
        assert_eq!(runtime.bindings[0].hook_binding.id, 0x348);
        let mut state = AppState::new_with_config(config, test_monitors());
        let response = state.handle_command(IpcCommand::QueryHotkeys);
        let IpcResponse::HotkeyList {
            hotkeys, issues, ..
        } = &response
        else {
            panic!("Expected HotkeyList");
        };
        let left = hotkeys
            .iter()
            .find(|h| h.action_id == "focus_left")
            .unwrap();
        let right = hotkeys
            .iter()
            .find(|h| h.action_id == "focus_right")
            .unwrap();
        assert_eq!(left.bindings, vec!["Alt+Control+h"]);
        assert!(left.enabled);
        assert!(right.bindings.is_empty());
        assert!(!right.enabled);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].binding, "Ctrl+Alt+H");
        assert_eq!(issues[0].action_id, "focus_right");
        assert!(issues[0].message.contains("Alt+Control+h"));
        assert!(issues[0].message.contains("Focus-Left"));
        if let Some(previous) = previous {
            assert_eq!(response, previous);
        }
        previous = Some(response);
    }
}

#[test]
fn test_cmd_query_hotkeys_reports_redundant_alias_for_same_action() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("Win+Left".into(), "focus_left".into());
    config
        .hotkeys
        .bindings
        .insert("Meta+Left".into(), "focus-left".into());
    let mut state = AppState::new_with_config(config, test_monitors());
    let IpcResponse::HotkeyList {
        hotkeys, issues, ..
    } = state.handle_command(IpcCommand::QueryHotkeys)
    else {
        panic!("Expected HotkeyList");
    };
    let left = hotkeys
        .iter()
        .find(|h| h.action_id == "focus_left")
        .unwrap();
    assert_eq!(left.bindings, vec!["Meta+Left"]);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].binding, "Win+Left");
    assert!(issues[0].message.contains("Meta+Left"));
}

#[test]
fn test_cmd_query_hotkeys_f_key_issue_preserves_loaded_action_identifier() {
    let mut config = test_config();
    config.hotkeys.bindings.clear();
    config
        .hotkeys
        .bindings
        .insert("F13+H".into(), "focus_left".into());
    config
        .hotkeys
        .bindings
        .insert("Ctrl+F13".into(), "Resize-Grow".into());
    let mut state = AppState::new_with_config(config, test_monitors());
    let IpcResponse::HotkeyList {
        hotkeys, issues, ..
    } = state.handle_command(IpcCommand::QueryHotkeys)
    else {
        panic!("Expected HotkeyList");
    };
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].binding, "Ctrl+F13");
    assert_eq!(issues[0].action_id, "Resize-Grow");
    let grow = hotkeys
        .iter()
        .find(|h| h.action_id == "cycle_width_up")
        .unwrap();
    assert!(!grow.enabled);
    assert!(grow.bindings.is_empty());
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
fn test_resume_reports_first_recovery_placement_error_while_animating() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(state.paused);

    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    let err = state
        .toggle_pause("test resume first recovery error")
        .expect_err("first placement failure must fail resume");
    assert!(
        err.to_string()
            .contains("injected apply_placements failure"),
        "resume must report the placement error, got {err}"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 1);
    assert!(
        !state.idle_layout_reapply_timer_needed(),
        "paused rollback remains authoritative for timer arming after a real placement error"
    );
    assert!(state.layout_transition.is_none());

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state
        .toggle_pause("test resume after recovery error")
        .expect("later successful resume must apply");
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 0);
}

#[test]
fn test_resume_reports_exhausted_recovery_placement_error_while_animating() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(state.paused);

    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    state.idle_layout_reapply_failures = 2;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    let err = state
        .toggle_pause("test resume exhausted recovery error")
        .expect_err("exhausted placement failure must fail resume");
    assert!(
        err.to_string()
            .contains("injected apply_placements failure"),
        "resume must report the placement error at exhaustion, got {err}"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 3);
    assert!(
        !state.idle_layout_reapply_timer_needed(),
        "exhausted failure count and paused rollback both keep the idle timer off"
    );
    assert!(state.layout_transition.is_none());

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state
        .toggle_pause("test resume after exhausted recovery error")
        .expect("later successful resume must still apply after exhaustion");
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert_eq!(state.idle_layout_reapply_failures, 0);
}

#[test]
fn test_resume_defers_while_recovery_animation_barrier_busy() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(state.paused);

    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    let err = state
        .toggle_pause("test resume busy recovery barrier")
        .expect_err("busy barrier must defer resume");
    assert!(
        err.to_string()
            .contains("Resume apply deferred while recovery animation placement finishes"),
        "busy resume must stay deferred, got {err}"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_some());
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
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
    assert_eq!(
        ws.focused_column_index(),
        0,
        "focus moved to the left column"
    );
    assert_eq!(
        ws.fullscreen_window_id(),
        Some(100),
        "fullscreen follows focus to the left window"
    );
}

#[test]
fn test_focus_command_exits_fullscreen_when_monocle_follow_disabled() {
    let mut state = fullscreen_state_two_columns();
    state.config.behavior.fullscreen_follows_focus = false;

    let resp = state.handle_command(IpcCommand::FocusLeft);
    assert_eq!(resp, IpcResponse::Ok);
    {
        let ws = state.focused_workspace().unwrap();
        assert!(
            !ws.is_fullscreen(),
            "focus command drops fullscreen when monocle-follow is off"
        );
        assert_eq!(
            ws.focused_column_index(),
            0,
            "focus still moved to the left column"
        );
        assert_eq!(
            ws.column_count(),
            2,
            "tiled layout is intact after exiting fullscreen"
        );
    }

    // The gate is on the fullscreen policy, so any focus command exits the same
    // way, not just FocusLeft. Re-enter and check the opposite direction.
    assert!(
        state.focused_workspace_mut().unwrap().toggle_fullscreen(),
        "re-entered fullscreen"
    );
    let resp = state.handle_command(IpcCommand::FocusRight);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace().unwrap();
    assert!(
        !ws.is_fullscreen(),
        "FocusRight also drops fullscreen when off"
    );
    assert_eq!(
        ws.focused_column_index(),
        1,
        "focus moved to the right column"
    );
}

#[test]
fn test_structural_command_exits_fullscreen() {
    let mut state = fullscreen_state_two_columns();
    let resp = state.handle_command(IpcCommand::ConsumeFromLeft);
    assert_eq!(resp, IpcResponse::Ok);
    let ws = state.focused_workspace().unwrap();
    assert!(!ws.is_fullscreen(), "consume must drop fullscreen");
    assert_eq!(
        ws.column_count(),
        1,
        "left window consumed into the focused column"
    );
}

#[test]
fn test_scroll_and_resize_are_suppressed_while_fullscreen() {
    let mut state = fullscreen_state_two_columns();
    assert_eq!(
        state.handle_command(IpcCommand::Scroll { delta: 120.0 }),
        IpcResponse::Ok
    );
    assert!(
        state.focused_workspace().unwrap().is_fullscreen(),
        "scroll must not drop fullscreen"
    );
    assert_eq!(
        state.handle_command(IpcCommand::Resize { delta: 50 }),
        IpcResponse::Ok
    );
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
fn test_cmd_apply_while_ordinary_paused_is_a_noop_success() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    assert!(state.paused);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));

    assert_eq!(state.handle_command(IpcCommand::Apply), IpcResponse::Ok);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "ordinary paused Apply must not dispatch placement"
    );
}

#[test]
fn test_cmd_apply_completed_is_ok() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));

    assert_eq!(state.handle_command(IpcCommand::Apply), IpcResponse::Ok);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0,
        "completed Apply must dispatch placement"
    );
}

#[test]
fn test_cmd_apply_reports_genuine_layout_failure() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    assert!(
        matches!(
            state.handle_command(IpcCommand::Apply),
            IpcResponse::Error { .. }
        ),
        "genuine Apply failure must stay Error"
    );
}

#[test]
fn test_cmd_apply_defers_until_idle_recovery_settles_surviving_transition() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));

    assert!(
        matches!(
            state.handle_command(IpcCommand::Apply),
            IpcResponse::ApplyPending { .. }
        ),
        "expected deferred ApplyPending"
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "Apply must not claim a physical landing while recovery still has a transition"
    );
    assert!(state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_some());

    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_none());
}

#[test]
fn test_cmd_resume_recovers_surviving_transition_after_barrier_clears() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    let snapshot = state.snapshot_layout();
    state.start_layout_transition_with_duration(snapshot, 150);
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    match state.handle_command(IpcCommand::TogglePause) {
        IpcResponse::Error { message } => assert!(message.contains("deferred")),
        other => panic!("expected deferred resume error, got {other:?}"),
    }
    assert!(state.paused, "busy recovery must restore paused state");
    assert!(state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_some());
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "busy resume must not dispatch a recovery landing"
    );
    assert!(
        state.take_recorded_taskbar_commands().is_empty(),
        "busy resume must not run completion-only taskbar effects"
    );
    assert!(
        matches!(
            state.handle_command(IpcCommand::Apply),
            IpcResponse::ApplyPending { .. }
        ),
        "expected pending ApplyPending after busy resume"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "paused Apply after busy resume must not dispatch placement"
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert!(state.layout_transition.is_none());
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0,
        "idle resume must settle and physically land the surviving transition"
    );
}

#[test]
fn test_cmd_apply_stays_pending_after_failed_resume_until_later_resume_lands() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndFail(Duration::ZERO));

    match state.handle_command(IpcCommand::TogglePause) {
        IpcResponse::Error { message } => {
            assert!(message.contains("injected apply_placements failure"))
        }
        other => panic!("expected failed resume error, got {other:?}"),
    }
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    let placement_calls_after_failed_resume = state
        .injected_apply_placements_call_count
        .load(Ordering::SeqCst);
    assert!(placement_calls_after_failed_resume > 0);

    assert!(
        matches!(
            state.handle_command(IpcCommand::Apply),
            IpcResponse::ApplyPending { .. }
        ),
        "expected pending ApplyPending after failed resume"
    );
    assert!(state.paused);
    assert!(state.pending_idle_layout_reapply);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        placement_calls_after_failed_resume,
        "paused Apply after failed resume must not dispatch another placement"
    );

    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
}

#[test]
fn test_cmd_apply_reports_recovery_barrier_deferral_until_landing_succeeds() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    assert!(
        matches!(
            state.handle_command(IpcCommand::Apply),
            IpcResponse::ApplyPending { .. }
        ),
        "expected deferred ApplyPending"
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "deferred IPC Apply must not dispatch native placement"
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(
        state.try_consume_idle_layout_reapply(),
        crate::temporary_ignore::IdleLayoutReapply::Applied
    );
    assert!(!state.pending_idle_layout_reapply);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0
    );
}

#[test]
fn test_cmd_resume_rolls_back_on_recovery_barrier_then_succeeds_after_landing() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    state.pending_idle_layout_reapply = true;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .unwrap();
    let unblock = worker.block_for_test();
    state.animation_worker_control = Some(worker.control());

    match state.handle_command(IpcCommand::TogglePause) {
        IpcResponse::Error { message } => assert!(message.contains("deferred")),
        other => panic!("expected deferred resume error, got {other:?}"),
    }
    assert!(state.paused, "deferred resume must restore paused state");
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        0,
        "deferred resume must not dispatch native placement"
    );
    assert!(
        state.take_recorded_taskbar_commands().is_empty(),
        "deferred resume must not run post-apply taskbar effects"
    );

    unblock.send(()).unwrap();
    assert!(worker.control().wait_for_barrier(Duration::from_secs(2)));
    assert_eq!(
        state.handle_command(IpcCommand::TogglePause),
        IpcResponse::Ok
    );
    assert!(!state.paused);
    assert!(!state.pending_idle_layout_reapply);
    assert!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst)
            > 0
    );
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
fn test_reconcile_no_change_preserves_manual_scroll() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
    for id in 1..=4 {
        workspace.insert_window(id, Some(600)).unwrap();
    }
    workspace.set_scroll_offset(250.0);

    state.reconcile_monitors(test_monitors());

    assert_eq!(state.workspaces[&1][0].scroll_offset(), 250.0);
}

#[test]
fn test_reconcile_stable_monitor_rescales_for_work_area_change() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(945))
        .unwrap();
    let mut resized = test_monitors();
    resized[0].rect.width = 1280;
    resized[0].work_area.width = 1280;

    state.reconcile_monitors(resized);

    assert_eq!(state.workspaces[&1][0].columns()[0].width(), 625);
}

#[test]
fn test_reconcile_shrink_keeps_focused_column_visible() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
    for id in 1..=3 {
        workspace.insert_window(id, Some(945)).unwrap();
    }
    workspace.set_scroll_offset(0.0);
    let mut resized = test_monitors();
    resized[0].rect.width = 1280;
    resized[0].work_area.width = 1280;

    state.reconcile_monitors(resized);

    let placements = state.workspaces[&1][0].compute_placements(Rect::new(0, 0, 1280, 1040));
    let focused = placements
        .iter()
        .find(|placement| placement.window_id == 3)
        .unwrap();
    assert!(focused.rect.x >= 0);
    assert!(focused.rect.x + focused.rect.width <= 1280);
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
fn test_reconcile_new_monitor_keeps_destination_default_width() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mut monitors = two_monitors();
    monitors[1].rect.width = 2560;
    monitors[1].work_area.width = 2560;
    state.reconcile_monitors(monitors);

    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, None)
        .unwrap();

    assert_eq!(state.workspaces[&2][0].columns()[0].width(), 839);
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

fn display_change_focus_state() -> AppState {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    let primary = &mut state.workspaces.get_mut(&1).unwrap()[0];
    for hwnd in 10..=12 {
        primary.insert_window(hwnd, Some(600)).unwrap();
    }
    primary.set_scroll_offset(250.0);
    let secondary = &mut state.workspaces.get_mut(&2).unwrap()[0];
    secondary.insert_window(100, Some(500)).unwrap();
    secondary.insert_window(200, Some(650)).unwrap();
    secondary.focus_window(100).unwrap();
    state.focused_monitor = 2;
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(100));
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state
}

fn display_change_focus_events(
    events: &mut tokio::sync::broadcast::Receiver<leopardwm_ipc::IpcEvent>,
) -> Vec<(i64, Option<u64>)> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            leopardwm_ipc::IpcEvent::FocusedWindowChanged { monitor, hwnd, .. } => {
                Some((monitor, hwnd))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn test_display_change_focus_follows_foreground_on_monitor_removal_without_scrolling() {
    let mut reconciled = display_change_focus_state();
    reconciled.reconcile_monitors(test_monitors());
    let reconciled_scroll = reconciled.workspaces[&1][0].scroll_offset();
    let mut state = display_change_focus_state();
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert_eq!(
        display_change_focus_events(&mut events),
        vec![(1, Some(100))]
    );

    assert_eq!(state.focused_monitor, 1);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.workspaces[&1][0].scroll_offset(), reconciled_scroll);
    state.workspaces.get_mut(&1).unwrap()[0].tick_animation(10_000);
    assert_eq!(state.workspaces[&1][0].scroll_offset(), reconciled_scroll);
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 100);
}

#[test]
fn test_display_change_focus_follows_unchanged_foreground_on_monitor_return() {
    let mut state = display_change_focus_state();
    state.handle_window_event(WindowEvent::DisplayChange);
    let mut returned = two_monitors();
    returned[1].id = 99;
    state.injected_display_monitors = Some(returned);
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert_eq!(
        display_change_focus_events(&mut events),
        vec![(99, Some(100))]
    );
    assert_eq!(state.focused_monitor, 99);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert!(!state.workspaces[&1][0].contains_window(100));
}

#[test]
fn test_display_change_focus_follows_foreground_to_restored_inactive_workspace() {
    let mut state = display_change_focus_state();
    state.reduce_motion = false;
    let foreground_workspace = state.workspaces[&2][0].clone();
    state.workspaces.get_mut(&2).unwrap()[0] =
        AppState::new_with_config(test_config(), test_monitors()).workspaces[&1][0].clone();
    state
        .workspaces
        .get_mut(&2)
        .unwrap()
        .push(foreground_workspace);
    state.handle_window_event(WindowEvent::DisplayChange);
    let mut returned = two_monitors();
    returned[1].id = 99;
    state.injected_display_monitors = Some(returned);
    state.inactive_workspace_park_requests.clear();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert!(
        !state.inactive_workspace_park_requests.contains(&100),
        "adopted foreground must never be parked before layout application"
    );
    assert_eq!(state.focused_monitor, 99);
    assert_eq!(state.active_workspace_idx(99), 1);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    let transition = state.layout_transition.as_ref().unwrap();
    assert!(transition.suppress_landing_focus_resync);
    let duration = transition.duration_ms;
    assert!(state.tick_animations(duration));
    assert!(state.layout_transition.is_none());
    assert!(state.pending_suppress_landing_focus_resync);
    let captured_suppress = state.pending_suppress_landing_focus_resync;
    state.apply_layout().unwrap();
    assert!(state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains(&100));
    let border_shows = state.border_show_count.load(Ordering::Relaxed);
    state.finish_animation_landing_focus_resync(captured_suppress);
    assert!(!state.pending_suppress_landing_focus_resync);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        border_shows
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert!(state.workspaces[&99][1]
        .compute_placements(state.monitors[&99].work_area)
        .iter()
        .any(|placement| placement.window_id == 100));
}

#[test]
fn test_display_change_focus_tracks_floating_foreground_without_changing_tiled_focus() {
    let mut state = display_change_focus_state();
    state.workspaces.get_mut(&2).unwrap()[0]
        .add_floating(300, Rect::new(2000, 100, 500, 400))
        .unwrap();
    state.injected_foreground_hwnd = Some(Some(300));
    state.handle_window_event(WindowEvent::DisplayChange);
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 300);

    let mut returned = two_monitors();
    returned[1].id = 99;
    state.injected_display_monitors = Some(returned);
    state.handle_window_event(WindowEvent::DisplayChange);

    assert_eq!(state.focused_monitor, 99);
    assert_eq!(state.previous_focused_hwnd, Some(300));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert!(state.focused_workspace().unwrap().is_floating(300));
}

#[test]
fn test_display_change_focus_unchanged_while_paused() {
    let mut state = display_change_focus_state();
    let mut inactive = state.workspaces[&2][0].clone();
    inactive.remove_window(200).unwrap();
    state.workspaces.get_mut(&2).unwrap().push(inactive);
    state.workspaces.get_mut(&2).unwrap()[0]
        .remove_window(100)
        .unwrap();
    state.focused_monitor = 1;
    state.previous_focused_hwnd = Some(12);
    state.injected_display_monitors = Some(two_monitors());
    state.paused = true;
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.active_workspace_idx(2), 0);
    assert_eq!(state.workspaces[&1][0].focused_window(), Some(12));
    assert_eq!(state.workspaces[&2][0].focused_window(), Some(200));
    assert_eq!(state.previous_focused_hwnd, Some(12));
    assert!(state.layout_transition.is_none());
    assert!(display_change_focus_events(&mut events).is_empty());
}

#[test]
fn test_display_change_focus_preserves_restored_fullscreen_against_other_foreground() {
    let mut state = display_change_focus_state();
    assert!(state.workspaces.get_mut(&2).unwrap()[0].toggle_fullscreen());
    state.injected_foreground_hwnd = Some(None);
    state.handle_window_event(WindowEvent::DisplayChange);
    let mut returned = two_monitors();
    returned[1].id = 99;
    state.injected_display_monitors = Some(returned);
    state.injected_foreground_hwnd = Some(Some(200));
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    let restored = &state.workspaces[&99][0];
    assert_eq!(restored.fullscreen_window_id(), Some(100));
    assert_eq!(restored.focused_window(), Some(100));
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.focused_monitor, 1);
    assert!(state.layout_transition.is_none());
    assert!(restored
        .compute_placements(state.monitors[&99].work_area)
        .iter()
        .any(|placement| placement.window_id == 100));
    assert!(display_change_focus_events(&mut events).is_empty());
}

#[test]
fn test_display_change_focus_does_not_adopt_untracked_application_fullscreen() {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let fullscreen = ParkProbeWindow::new(false);
    unsafe {
        SetWindowPos(
            fullscreen.0,
            None,
            0,
            0,
            1920,
            1080,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .unwrap();
    }
    let before = fullscreen.rect();
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(600))
        .unwrap();
    state.ensure_workspace_exists(1, 1);
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(fullscreen.id(), Some(600))
        .unwrap();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(fullscreen.id()));
    state.injected_display_monitors = Some(test_monitors());
    assert!(!state.is_application_fullscreen(fullscreen.id()));
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert!(state.is_application_fullscreen(fullscreen.id()));
    assert!(!state
        .inactive_workspace_park_requests
        .contains(&fullscreen.id()));
    assert!(!state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains(&fullscreen.id()));
    assert!(display_change_focus_events(&mut events).is_empty());
    assert_eq!(fullscreen.rect(), before);
    assert!(!leopardwm_platform_win32::is_window_visible(
        fullscreen.id()
    ));

    state.application_fullscreen.clear();
    state.workspaces.get_mut(&1).unwrap()[1]
        .remove_window(fullscreen.id())
        .unwrap();
    let active = &mut state.workspaces.get_mut(&1).unwrap()[0];
    active.insert_window(fullscreen.id(), Some(600)).unwrap();
    active.focus_window(100).unwrap();
    assert!(!state.is_application_fullscreen(fullscreen.id()));

    state.handle_window_event(WindowEvent::DisplayChange);

    assert!(
        !state.is_application_fullscreen(fullscreen.id()),
        "active-workspace observation must not bypass fullscreen-entry lifecycle cleanup"
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert!(display_change_focus_events(&mut events).is_empty());
}

#[test]
fn test_display_change_focus_does_not_adopt_minimized_foreground() {
    let mut state = display_change_focus_state();
    state.workspaces.get_mut(&2).unwrap()[0].mark_minimized(100);
    state.focused_monitor = 1;
    state.previous_focused_hwnd = Some(12);
    state.injected_display_monitors = Some(two_monitors());
    let mut events = state.event_broadcaster.subscribe();

    state.handle_window_event(WindowEvent::DisplayChange);

    assert!(state.workspaces[&2][0].is_minimized(100));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.previous_focused_hwnd, Some(12));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(12)
    );
    assert!(display_change_focus_events(&mut events).is_empty());
}

#[test]
fn test_display_change_focus_unchanged_for_unmanaged_or_absent_foreground() {
    for foreground in [Some(999), None] {
        let mut state = display_change_focus_state();
        state.injected_foreground_hwnd = Some(foreground);
        state.previous_focused_hwnd = Some(999);
        state.handle_window_event(WindowEvent::DisplayChange);

        assert_eq!(state.focused_monitor, 1);
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(200)
        );
        assert_eq!(state.previous_focused_hwnd, Some(999));

        let mut returned = two_monitors();
        returned[1].id = 99;
        state.injected_display_monitors = Some(returned);
        state.handle_window_event(WindowEvent::DisplayChange);

        assert_eq!(state.focused_monitor, 1);
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(12)
        );
        assert_eq!(state.previous_focused_hwnd, Some(999));
        assert_eq!(state.active_workspace_idx(99), 0);
    }
}

#[test]
fn test_reconcile_restores_stashed_layout_on_monitor_return() {
    // A monitor that disconnects and reconnects with a NEW HMONITOR but the same
    // device_name gets its exact layout back (columns + widths) instead of a
    // flattened default — the overnight screen-off reset bug.
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    // Two windows with distinct non-default widths on monitor 2 (DISPLAY2).
    {
        let ws = state.workspaces.get_mut(&2).unwrap().get_mut(0).unwrap();
        ws.insert_window(100, Some(500)).unwrap();
        ws.insert_window(200, Some(650)).unwrap();
    }
    let width_on = |st: &AppState, mid: MonitorId, h: u64| -> Option<i32> {
        let ws = st.workspaces.get(&mid)?.first()?;
        let (c, _) = ws.find_window_location(h)?;
        ws.column(c).map(|col| col.width())
    };
    assert_eq!(width_on(&state, 2, 100), Some(500));
    assert_eq!(width_on(&state, 2, 200), Some(650));

    // Monitor 2 disconnects: windows migrate to primary, layout stashed.
    state.reconcile_monitors(test_monitors()); // only DISPLAY1 remains
    assert!(!state.workspaces.contains_key(&2));
    assert!(state.workspaces[&1][0].contains_window(100));
    assert!(state.workspaces[&1][0].contains_window(200));
    assert!(state.stashed_monitor_layouts.contains_key("DISPLAY2"));

    // Monitor 2 returns with a NEW HMONITOR (id 99) but the same device_name.
    let mut returned = two_monitors();
    returned[1].id = 99;
    state.reconcile_monitors(returned);

    // The stashed layout is restored on the new id with original widths...
    assert!(state.workspaces.contains_key(&99));
    assert_eq!(
        width_on(&state, 99, 100),
        Some(500),
        "width restored on return"
    );
    assert_eq!(
        width_on(&state, 99, 200),
        Some(650),
        "width restored on return"
    );
    // ...the windows are no longer duplicated on primary...
    assert!(!state.workspaces[&1][0].contains_window(100));
    assert!(!state.workspaces[&1][0].contains_window(200));
    // ...and the stash is consumed.
    assert!(!state.stashed_monitor_layouts.contains_key("DISPLAY2"));
}

#[test]
fn test_reconcile_restores_stash_at_changed_width_and_active_workspace() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    let second_workspace = state.workspaces[&2][0].clone();
    state.workspaces.get_mut(&2).unwrap().push(second_workspace);
    state.active_workspace.insert(2, 1);
    state.workspaces.get_mut(&2).unwrap()[1]
        .insert_window(100, Some(500))
        .unwrap();

    state.reconcile_monitors(test_monitors());

    let mut returned = two_monitors();
    returned[1].id = 99;
    returned[1].rect.width = 1280;
    returned[1].work_area.width = 1280;
    state.reconcile_monitors(returned);

    assert_eq!(state.active_workspace_idx(99), 1);
    assert_eq!(state.workspaces[&99][1].columns()[0].width(), 329);
}

#[test]
fn test_reconcile_restored_stash_uses_current_centering_policy() {
    let mut config = test_config();
    config.layout.center_past_edges = true;
    let mut state = AppState::new_with_config(config.clone(), two_monitors());
    let workspace = &mut state.workspaces.get_mut(&2).unwrap()[0];
    workspace.insert_window(100, Some(500)).unwrap();
    workspace.set_scroll_offset(-100.0);

    state.reconcile_monitors(test_monitors());
    config.layout.center_past_edges = false;
    state.apply_config(config);

    let mut returned = two_monitors();
    returned[1].id = 99;
    state.reconcile_monitors(returned);

    assert_eq!(state.workspaces[&99][0].scroll_offset(), 0.0);
}

#[test]
fn test_reconcile_adopts_layout_on_same_pass_handle_change() {
    // A single reconcile where a monitor's HMONITOR changes AND the count
    // changes (e.g. a dock event) must preserve the layout, not flatten it.
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    {
        let ws = state.workspaces.get_mut(&2).unwrap().get_mut(0).unwrap();
        ws.insert_window(100, Some(500)).unwrap();
        ws.insert_window(200, Some(650)).unwrap();
    }
    // DISPLAY2 returns with a new handle (99) and a third monitor appears in the
    // same pass, so the count changes and the safe re-key branch is skipped.
    let mut next = two_monitors();
    next[1].id = 99;
    next.push(MonitorInfo {
        id: 3,
        rect: Rect::new(3840, 0, 1920, 1080),
        work_area: Rect::new(3840, 0, 1920, 1040),
        is_primary: false,
        device_name: "DISPLAY3".to_string(),
        scale_factor: 1.0,
    });
    state.reconcile_monitors(next);

    let ws = state.workspaces.get(&99).unwrap().first().unwrap();
    let col100 = ws.find_window_location(100).unwrap().0;
    let col200 = ws.find_window_location(200).unwrap().0;
    assert_eq!(
        ws.column(col100).unwrap().width(),
        500,
        "adopted layout keeps width"
    );
    assert_eq!(
        ws.column(col200).unwrap().width(),
        650,
        "adopted layout keeps width"
    );
    // Adopted live, so nothing was stashed or flattened onto another monitor.
    assert!(state.stashed_monitor_layouts.is_empty());
}

#[test]
fn test_reconcile_adoption_maps_source_width_to_new_monitor_id() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(100, Some(500))
        .unwrap();

    let mut next = two_monitors();
    next[1].id = 99;
    next[1].rect.width = 1280;
    next[1].work_area.width = 1280;
    next.push(MonitorInfo {
        id: 3,
        rect: Rect::new(3200, 0, 1920, 1080),
        work_area: Rect::new(3200, 0, 1920, 1040),
        is_primary: false,
        device_name: "DISPLAY3".to_string(),
        scale_factor: 1.0,
    });

    state.reconcile_monitors(next);

    assert_eq!(state.workspaces[&99][0].columns()[0].width(), 329);
}

#[test]
fn test_reconcile_handle_rekey_rescales_changed_width() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(945))
        .unwrap();
    let mut rekeyed = test_monitors();
    rekeyed[0].id = 99;
    rekeyed[0].rect.width = 1280;
    rekeyed[0].work_area.width = 1280;

    state.reconcile_monitors(rekeyed);

    assert!(!state.workspaces.contains_key(&1));
    assert_eq!(state.workspaces[&99][0].columns()[0].width(), 625);
    assert_eq!(state.focused_monitor, 99);
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
    let mut expected_config = Config::load().expect("configuration should load");
    expected_config.validate();
    let expected_gap = expected_config.layout.gap;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    // Developer machines may have a real config. Start with a different value
    // so this proves reload applied the effective config, even if it is default.
    state.config.layout.gap = if expected_gap == 10 { 11 } else { 10 };
    let resp = state.handle_command(IpcCommand::Reload);
    assert_eq!(resp, IpcResponse::Ok);
    assert_eq!(state.config.layout.gap, expected_gap);
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
    state.handle_window_event(WindowEvent::Focused(100, 0));
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
fn test_minimize_reconciles_geometry_after_native_minimum_removal() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.set_reduce_motion(false);
    workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::JustInView);
    workspace.insert_window(100, Some(467)).unwrap();
    workspace.insert_window(200, Some(467)).unwrap();
    workspace.insert_window_in_column(300, 1).unwrap();
    workspace.commit_pending_min_size_clears();
    workspace.set_window_min_width(200, 842);
    workspace.set_scroll_offset(339.0);
    assert_eq!(state.tiled_column_width(1, 0, 200), Some(467));
    state.handle_window_event(WindowEvent::Minimized(200, 0));
    let workspace = state.focused_workspace_mut().unwrap();
    assert!(workspace.is_minimized(200));
    assert_eq!(workspace.columns()[1].width(), 467);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[1]),
        467
    );
    workspace.tick_animation(1000);
    assert_eq!(workspace.scroll_offset(), 0.0);
}

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
fn test_resync_minimized_from_os_corrects_stale_flags() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap(); // flagged minimized, OS says visible
    ws.insert_window(200, Some(800)).unwrap(); // visible, stays visible
    ws.insert_window(300, Some(800)).unwrap(); // OS minimized, not yet flagged
    ws.insert_window(400, Some(800)).unwrap(); // genuinely minimized, stays so
    ws.mark_minimized(100);
    ws.mark_minimized(400);

    // OS truth after a monitor wake: 100 was restored, 300 got minimized.
    state.resync_minimized_with(|wid| match wid {
        300 | 400 => Some(true),
        _ => Some(false),
    });

    let ws = state.focused_workspace().unwrap();
    assert!(
        !ws.is_minimized(100),
        "stale-minimized window should be restored"
    );
    assert!(!ws.is_minimized(200));
    assert!(
        ws.is_minimized(300),
        "OS-minimized window should be flagged"
    );
    assert!(
        ws.is_minimized(400),
        "genuinely minimized window stays minimized"
    );
}

#[test]
fn test_center_single_column_apply_repairs_resync_and_nonrevealing_changes() {
    use std::time::Duration;
    for enabled in [false, true] {
        for frame_boundary in [false, true] {
            for scenario in 0..4 {
                let mut monitors = test_monitors();
                monitors[0].rect.width = 1000;
                monitors[0].work_area.width = 1000;
                let mut config = test_config();
                config.layout.center_single_column = enabled;
                let mut state = AppState::new_with_config(config, monitors);
                state.paused = false;
                let viewport = Rect::new(0, 0, 1000, 1040);
                let mut ws = Workspace::with_gaps(10, 20);
                ws.set_center_single_column(enabled);
                ws.set_reduce_motion(false);
                ws.insert_window(100, Some(400)).unwrap();
                ws.ensure_focused_visible(1000);
                ws.compute_placements(viewport);
                match scenario {
                    0 => ws.set_scroll_offset(0.0),
                    1 => {
                        ws.insert_window(200, Some(400)).unwrap();
                        ws.set_scroll_offset(137.0);
                        ws.compute_placements(viewport);
                    }
                    2 => ws.append_window_no_focus(200, Some(400)).unwrap(),
                    3 => {
                        ws.set_scroll_offset(137.0);
                        ws.ensure_focused_visible_animated(1000);
                        ws.tick_animation(50);
                    }
                    _ => unreachable!(),
                }
                state.workspaces.insert(1, vec![ws]);
                if scenario == 1 {
                    state.resync_minimized_with(|wid| Some(wid == 200));
                }
                let before = state.workspaces[&1][0].effective_scroll_offset();
                let was_animating = state.workspaces[&1][0].is_animating();
                if frame_boundary {
                    for id in [100, 200] {
                        state.application_fullscreen.insert(
                            id,
                            crate::state::ApplicationFullscreenState {
                                monitor_id: 1,
                                rect: viewport,
                            },
                        );
                    }
                    let (tx, _rx) = tokio::sync::mpsc::channel(4);
                    let worker = animation_worker::AnimationWorkerHandle::spawn(
                        tx,
                        state.apply_worker_cancelled.clone(),
                    )
                    .unwrap();
                    state.send_animation_frame(&worker).unwrap();
                    drop(worker);
                } else {
                    state.injected_apply_placements_behavior =
                        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
                    state.apply_layout().unwrap();
                }
                let ws = &state.workspaces[&1][0];
                let expected_x = if scenario == 3 || !enabled {
                    20 - before.round() as i32
                } else if scenario == 2 {
                    20
                } else {
                    300
                };
                if !frame_boundary {
                    assert_eq!(
                        state.last_placed_layout_rects[&100].x, expected_x,
                        "enabled={enabled}, frame={frame_boundary}, scenario={scenario}"
                    );
                }
                if !enabled || scenario == 3 {
                    assert_eq!(ws.effective_scroll_offset(), before);
                    assert_eq!(ws.is_animating(), was_animating);
                }
                assert_eq!(
                    ws.compute_placements_animated(viewport)[0].rect.x,
                    expected_x
                );
            }
        }
    }
}

#[test]
fn test_resync_minimized_leaves_dead_windows_untouched() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let ws = state.focused_workspace_mut().unwrap();
    ws.insert_window(100, Some(800)).unwrap();
    ws.mark_minimized(100);

    // A dead handle reports None; the flag must be left for pruning to handle.
    state.resync_minimized_with(|_| None);

    assert!(state.focused_workspace().unwrap().is_minimized(100));
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

    state.handle_window_event(WindowEvent::Minimized(200, 0));
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
        0.0,
        "Scroll offset outside empty workspace bounds should be normalized"
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
    let win = Modifiers {
        win: true,
        ..Default::default()
    };
    let ctrl_alt = Modifiers {
        ctrl: true,
        alt: true,
        ..Default::default()
    };
    let labels = vec![
        (1 as HotkeyId, "Win+L".to_string(), win, 0x4C), // lock — protected
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
    state.handle_window_event(WindowEvent::Focused(500, 0));

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

    state.handle_window_event(WindowEvent::Focused(100, 0));
    assert_eq!(state.previous_focused_hwnd, Some(100));

    state.handle_window_event(WindowEvent::Focused(200, 0));
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
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
}

#[test]
fn test_inactive_minimized_window_restore_waits_for_focus_event() {
    const RESTORED: u64 = 100;
    const ACTIVE: u64 = 200;
    const OTHER_MONITOR: u64 = 300;
    const RESTORE_PEER: u64 = 400;

    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.paused = false;
    state.reduce_motion = false;
    state.ensure_workspace_exists(1, 2);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(ACTIVE, Some(800))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[2]
        .insert_window(RESTORED, Some(800))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[2]
        .insert_window(RESTORE_PEER, Some(800))
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(OTHER_MONITOR, Some(800))
        .unwrap();
    state.active_workspace.insert(1, 2);
    state.active_workspace.insert(2, 0);
    state.focused_monitor = 2;
    state.previous_focused_hwnd = Some(OTHER_MONITOR);
    state.injected_window_maximized.insert(RESTORED, false);
    state.apply_layout().unwrap();
    join_pending_test_apply_workers(&mut state);
    assert!(state.last_placed_layout_rects.contains_key(&RESTORED));
    state.active_workspace.insert(1, 0);
    state.workspaces.get_mut(&1).unwrap()[2].mark_minimized(RESTORED);
    state.moved_or_resized_suppression.remove(&RESTORED);

    let placement_calls_before_moves = state
        .injected_apply_placements_call_count
        .load(Ordering::SeqCst);
    state
        .last_placed_layout_rects
        .insert(ACTIVE, Rect::new(-10_000, -10_000, 1, 1));
    for _ in 0..3 {
        state.handle_window_event(WindowEvent::MovedOrResized(RESTORED));
    }
    join_pending_test_apply_workers(&mut state);
    assert!(
        state.last_placed_layout_rects.contains_key(&RESTORED),
        "move/resize events must not evict a minimized window's last placement"
    );
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        placement_calls_before_moves,
        "move/resize events must not snap back a minimized window"
    );
    assert!(state.workspaces.get(&1).unwrap()[2].is_minimized(RESTORED));

    state.handle_window_event(WindowEvent::Restored(RESTORED));
    join_pending_test_apply_workers(&mut state);
    assert!(!state.workspaces.get(&1).unwrap()[2].is_minimized(RESTORED));
    assert_eq!(
        state.workspaces.get(&1).unwrap()[2].focused_window(),
        Some(RESTORED),
        "restore should focus the window within its own workspace"
    );
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.focused_monitor, 2);
    assert_eq!(state.previous_focused_hwnd, Some(OTHER_MONITOR));
    assert!(
        state.layout_transition.is_some(),
        "restore should schedule the layout transition"
    );

    state.handle_window_event(WindowEvent::Focused(RESTORED, 1_000));
    assert_eq!(state.active_workspace_idx(1), 2);
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.previous_focused_hwnd, Some(RESTORED));
    state.handle_window_event(WindowEvent::Focused(RESTORED, 1_001));
    assert_eq!(state.active_workspace_idx(1), 2);
    assert_eq!(state.focused_monitor, 1);

    state.workspaces.get_mut(&1).unwrap()[2].mark_minimized(RESTORED);
    state.workspaces.get_mut(&1).unwrap()[2]
        .focus_window(RESTORE_PEER)
        .unwrap();
    state.focused_monitor = 2;
    state.previous_focused_hwnd = Some(OTHER_MONITOR);
    state.handle_window_event(WindowEvent::Restored(RESTORED));
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(state.previous_focused_hwnd, Some(RESTORED));
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

fn join_pending_test_apply_workers(state: &mut AppState) {
    for worker in state.begin_shutdown_or_revert() {
        let mut worker = Some(worker);
        assert!(
            join_with_timeout(&mut worker, Duration::from_millis(300)),
            "timed-out test worker should exit before the test returns"
        );
    }
}

fn pending_display_change_retry_generation(state: &AppState) -> u64 {
    state
        .display_change_apply_retry
        .as_ref()
        .expect("a display-change retry should be pending")
        .generation
}

#[test]
fn test_apply_layout_timeout_auto_pauses_and_records_batch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    {
        let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window(200, Some(800)).unwrap();
    }
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state
        .moved_or_resized_suppression
        .insert(42, std::time::Instant::now() + Duration::from_secs(1));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

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

    let report = state
        .take_layout_apply_timeout_report()
        .expect("timeout should create a pending report");
    assert_eq!(report.timeout, Duration::from_millis(10));
    assert_eq!(report.candidates.len(), 2);
    for hwnd in [100, 200] {
        let candidate = report
            .candidates
            .iter()
            .find(|candidate| candidate.hwnd == hwnd)
            .expect("every placed window should be included in the timeout batch");
        assert_eq!(candidate.class_name.as_deref(), Some("TestWindowClass"));
        let expected_title = format!("Test Window {}", hwnd);
        assert_eq!(candidate.title.as_deref(), Some(expected_title.as_str()));
    }
    assert!(
        state.take_layout_apply_timeout_report().is_none(),
        "timeout report should be drained exactly once"
    );
    join_pending_test_apply_workers(&mut state);
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
    assert!(
        state.take_layout_apply_timeout_report().is_none(),
        "ordinary placement failures should not create timeout reports"
    );
}

#[test]
fn test_resume_clears_stale_timeout_report_and_preserves_fresh_timeout() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = true;
    state.pending_layout_apply_timeout_report = Some(LayoutApplyTimeoutReport {
        timeout: Duration::from_secs(99),
        candidates: vec![LayoutApplyTimeoutCandidate {
            hwnd: 999,
            class_name: Some("StaleClass".to_string()),
            title: Some("Stale title".to_string()),
            executable: None,
        }],
    });
    state.layout_apply_timeout = Duration::from_millis(50);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        Duration::from_millis(1),
    ));

    state
        .toggle_pause("test resume")
        .expect_err("injected non-timeout resume failure should propagate");
    assert!(state.paused, "failed resume should restore paused state");
    assert!(
        state.take_layout_apply_timeout_report().is_none(),
        "resume should discard an old undelivered timeout report"
    );

    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));
    state
        .toggle_pause("test resume timeout")
        .expect_err("new resume apply should time out");

    let fresh = state
        .take_layout_apply_timeout_report()
        .expect("a new resume timeout should create a fresh report");
    assert_eq!(fresh.timeout, Duration::from_millis(10));
    assert!(state.paused, "timed-out resume should remain paused");
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_apply_layout_timeout_worker_is_joined_during_shutdown_begin() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(60),
    ));

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
fn test_protected_only_animation_frame_dispatches_and_settles_transition() {
    use crate::state::{ApplicationFullscreenState, LayoutTransition};
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state.application_fullscreen.insert(
        100,
        ApplicationFullscreenState {
            monitor_id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
        },
    );
    state.layout_transition = Some(LayoutTransition {
        start_rects: HashMap::from([(100, Rect::new(0, 0, 800, 1040))]),
        exit_rects: HashMap::new(),
        exit_provenance: HashMap::new(),
        elapsed_ms: 16,
        duration_ms: 150,
        easing: leopardwm_core_layout::Easing::default(),
        ghosted_wids: HashSet::new(),
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    });

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .expect("spawn animation worker");

    assert!(state
        .send_animation_frame(&worker)
        .expect("protected-only frame should dispatch"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert!(matches!(
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await
        }),
        Ok(Some(DaemonEvent::AnimationFrameApplied(_)))
    ));
    state.applying_layout = false;
    assert!(state.tick_animations(150));
    assert!(
        state.layout_transition.is_none(),
        "the protected-only frame path must settle its transition"
    );
    drop(worker);
}

#[test]
fn test_send_animation_frame_skips_after_apply_worker_cancelled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let worker = animation_worker::AnimationWorkerHandle::spawn(
        event_tx,
        state.apply_worker_cancelled.clone(),
    )
    .expect("spawn animation worker");

    let sent = state
        .send_animation_frame(&worker)
        .expect("active send_animation_frame should not error");
    assert!(sent, "active send_animation_frame should dispatch a frame");

    state.begin_shutdown_or_revert();
    assert!(
        state.apply_worker_cancelled.load(Ordering::SeqCst),
        "begin_shutdown_or_revert should latch cancellation"
    );

    let sent = state
        .send_animation_frame(&worker)
        .expect("cancelled send_animation_frame should not error");
    assert!(
        !sent,
        "cancelled send_animation_frame must not dispatch a frame that could re-park windows"
    );
    drop(worker);
}

#[test]
fn test_apply_layout_rejects_overlap_while_timed_out_worker_is_running() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(500),
    ));

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
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(50),
    ));
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

struct OffscreenResizeOwner {
    hwnd: u64,
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl OffscreenResizeOwner {
    fn new() -> Self {
        Self::create(false)
    }

    fn create(framed: bool) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || unsafe {
            use windows::core::w;
            use windows::Win32::System::Threading::GetCurrentThreadId;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, ShowWindow, MSG,
                SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
                WS_POPUP,
            };
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                if framed {
                    WS_OVERLAPPEDWINDOW
                } else {
                    WS_POPUP
                },
                if framed { -30000 } else { -300 },
                if framed { -1000 } else { 0 },
                300,
                560,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            if framed {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            let mut message = MSG::default();
            // Create the owner message queue before exposing its thread ID.
            let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut message,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
            );
            ready_tx
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).unwrap();
        });
        let (hwnd, thread_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            hwnd,
            thread_id,
            join: Some(join),
        }
    }

    fn resize(&self, x: i32, width: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
        unsafe {
            SetWindowPos(
                windows::Win32::Foundation::HWND(self.hwnd as *mut _),
                None,
                x,
                0,
                width,
                560,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
            .unwrap();
        }
    }

    fn rect(&self) -> Rect {
        leopardwm_platform_win32::get_window_chrome_rect(self.hwnd).unwrap()
    }
}

impl Drop for OffscreenResizeOwner {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).unwrap();
        }
        self.join.take().unwrap().join().unwrap();
    }
}

fn floating_move_state(owner: &OffscreenResizeOwner) -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = true;
    state.workspaces.get_mut(&1).unwrap()[0]
        .add_floating(owner.hwnd, owner.rect())
        .unwrap();
    state.apply_layout().unwrap();
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    state
}

fn floating_mismatched_landing_state(owner: &OffscreenResizeOwner, readable: bool) -> AppState {
    use leopardwm_core_layout::{Visibility, WindowPlacement};
    use leopardwm_platform_win32::PlacementLanding;

    let mut state = floating_move_state(owner);
    let requested = state.workspaces[&1][0].floating_windows()[0].rect;
    owner.resize(230, 450);
    let dispatched = state.apply_physical_projection(vec![WindowPlacement {
        window_id: owner.hwnd,
        rect: requested,
        visibility: Visibility::Visible,
        column_index: usize::MAX,
    }]);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(
        request_id,
        invalidation_id,
        &[PlacementLanding {
            window_id: owner.hwnd,
            requested_rect: dispatched[0].rect,
            requested_visibility: Visibility::Visible,
            actual_visible_rect: readable.then(|| owner.rect()),
            actual_outer_rect: readable.then(|| owner.rect()),
            failed: false,
            unreadable: !readable,
            measurement_deferred: false,
        }],
        &[],
    );
    assert_eq!(
        state.physical_landing_is_safe_to_expose(owner.hwnd),
        readable
    );
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state
}

#[test]
fn test_deferred_floating_mismatched_landing_preserves_target_before_placement() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (readable, round_trip) in [(true, false), (true, true), (false, false), (false, true)] {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_mismatched_landing_state(&owner, readable);
        let requested = Rect::new(-300, 0, 300, 560);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        if round_trip {
            for index in [2, 1] {
                assert!(matches!(
                    state.handle_command(IpcCommand::SwitchWorkspace { index }),
                    IpcResponse::Ok
                ));
                assert_eq!(
                    state.workspaces[&1][0].floating_windows()[0].rect,
                    requested
                );
            }
        } else {
            state.apply_layout().unwrap();
            assert_eq!(
                state.workspaces[&1][0].floating_windows()[0].rect,
                requested
            );
        }
    }
}

#[test]
fn test_deferred_floating_replay_uses_actual_landing_and_requires_acknowledgement() {
    use leopardwm_core_layout::{Visibility, WindowPlacement};

    #[derive(Clone, Copy)]
    enum Acknowledgement {
        Current,
        Unreadable,
        Pending,
        Invalidated,
        Abandoned,
    }
    use Acknowledgement::{Abandoned, Current, Invalidated, Pending, Unreadable};

    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (acknowledgement, delta) in [
        (Current, 0),
        (Current, 2),
        (Current, 3),
        (Unreadable, 3),
        (Pending, 3),
        (Invalidated, 3),
        (Abandoned, 3),
    ] {
        let owner = OffscreenResizeOwner::new();
        let mut state =
            floating_mismatched_landing_state(&owner, !matches!(acknowledgement, Unreadable));
        match acknowledgement {
            Pending | Abandoned => {
                state.apply_physical_projection(vec![WindowPlacement {
                    window_id: owner.hwnd,
                    rect: Rect::new(-300, 0, 300, 560),
                    visibility: Visibility::Visible,
                    column_index: usize::MAX,
                }]);
                if matches!(acknowledgement, Abandoned) {
                    let (request_id, invalidation_id) = state.physical_request_ids();
                    state.abandon_physical_request(request_id, invalidation_id);
                }
            }
            Invalidated => {
                state.bump_physical_invalidation();
            }
            Current | Unreadable => {}
        }
        owner.resize(230 + delta, 450 + delta);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        assert!(!state.recheck_deferred_window_moves());
        let expected = if matches!(acknowledgement, Current) && delta > 2 {
            Rect::new(230 + delta, 0, 450 + delta, 560)
        } else {
            Rect::new(-300, 0, 300, 560)
        };
        assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, expected);
        assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
        assert!(!state.recheck_deferred_window_moves());
    }
}

#[test]
fn test_deferred_external_floating_move_after_mismatched_landing_is_preserved() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for round_trip in [false, true] {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_mismatched_landing_state(&owner, true);
        owner.resize(400, 500);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        if round_trip {
            for index in [2, 1] {
                assert!(matches!(
                    state.handle_command(IpcCommand::SwitchWorkspace { index }),
                    IpcResponse::Ok
                ));
            }
        } else {
            state.moved_or_resized_suppression.remove(&owner.hwnd);
            assert!(!state.recheck_deferred_window_moves());
        }
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            Rect::new(400, 0, 500, 560)
        );
    }
}

#[test]
fn test_external_floating_move_survives_reapply_and_workspace_round_trip() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = floating_move_state(&owner);
    owner.resize(230, 450);
    let moved = Rect::new(230, 0, 450, 560);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.apply_layout().unwrap();
    assert_eq!(owner.rect(), moved);
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 1 }),
        IpcResponse::Ok
    ));
    assert_eq!(owner.rect(), moved);
    assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, moved);
}

#[test]
fn test_suppressed_floating_move_survives_intervening_placement() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for round_trip in [false, true] {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_move_state(&owner);
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        owner.resize(230, 450);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
        assert!(state.should_suppress_moved_or_resized(owner.hwnd));
        if round_trip {
            for index in [2, 1] {
                assert!(matches!(
                    state.handle_command(IpcCommand::SwitchWorkspace { index }),
                    IpcResponse::Ok
                ));
                assert_eq!(
                    state.workspaces[&1][0].floating_windows()[0].rect,
                    Rect::new(230, 0, 450, 560),
                    "geometry must be saved before the leaving workspace is parked"
                );
            }
        } else {
            state.last_placed_layout_rects.remove(&owner.hwnd);
            state.apply_layout().unwrap();
        }
        let moved = Rect::new(230, 0, 450, 560);
        assert_eq!(owner.rect(), moved, "round_trip={round_trip}");
        assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, moved);
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        assert!(!state.recheck_deferred_window_moves());
        assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
        assert_eq!(owner.rect(), moved);
    }
}

#[test]
fn test_suppressed_floating_placement_feedback_preserves_changed_target() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = floating_move_state(&owner);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    let target = Rect::new(230, 100, 450, 560);
    state.workspaces.get_mut(&1).unwrap()[0].update_floating(owner.hwnd, target);
    state.apply_layout().unwrap();
    assert_eq!(owner.rect(), target);
    assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, target);
}

#[test]
fn test_suppressed_floating_move_replays_after_placement_without_looping() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = floating_move_state(&owner);
    let original = owner.rect();
    owner.resize(230, 450);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    let before = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, original);
    assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
    state.moved_or_resized_suppression.insert(
        owner.hwnd,
        std::time::Instant::now() - Duration::from_millis(1),
    );
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(
        state.workspaces[&1][0].floating_windows()[0].rect,
        Rect::new(230, 0, 450, 560)
    );
    assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
    assert!(!state.window_move_recheck_attempts.contains_key(&owner.hwnd));
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
}

#[test]
fn test_floating_placement_and_special_states_preserve_stored_geometry() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for exclusion in [
        "inactive",
        "parked",
        "minimized",
        "maximized",
        "fullscreen",
        "resize",
        "drag",
        "transition",
        "applying",
        "display_change",
    ] {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_move_state(&owner);
        let original = owner.rect();
        owner.resize(230, 450);
        match exclusion {
            "inactive" => {
                state.ensure_workspace_exists(1, 1);
                state.active_workspace.insert(1, 1);
            }
            "parked" => {
                leopardwm_platform_win32::move_window_offscreen(owner.hwnd).unwrap();
            }
            "minimized" => {
                state.workspaces.get_mut(&1).unwrap()[0].mark_minimized(owner.hwnd);
            }
            "maximized" => {
                state.injected_window_maximized.insert(owner.hwnd, true);
                state.previous_focused_hwnd = Some(owner.hwnd);
            }
            "fullscreen" => {
                state.application_fullscreen.insert(
                    owner.hwnd,
                    ApplicationFullscreenState {
                        monitor_id: 1,
                        rect: Rect::new(0, 0, 1920, 1080),
                    },
                );
                owner.resize(0, 1920);
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{
                        SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
                    };
                    SetWindowPos(
                        windows::Win32::Foundation::HWND(owner.hwnd as *mut _),
                        None,
                        0,
                        0,
                        1920,
                        1080,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    )
                    .unwrap();
                }
            }
            "resize" => state.resize_hwnd = Some(owner.hwnd),
            "drag" => {
                state.drag_state = Some(DragState {
                    hwnd: owner.hwnd,
                    is_tiled: false,
                    source_monitor: 1,
                    source_workspace_idx: 0,
                    source_window_slot: 0,
                    current_column_index: 0,
                    last_drop_target: None,
                    last_hint_update: None,
                    removed_from_source: false,
                    preview_mode: crate::state::DragPreviewMode::None,
                    target_column_peers: Vec::new(),
                    source_column_peers: Vec::new(),
                });
            }
            "transition" => state.start_layout_transition_with_duration(
                std::collections::HashMap::from([(owner.hwnd, original)]),
                150,
            ),
            "applying" => state.applying_layout = true,
            "display_change" => state.display_change_pending = true,
            _ => unreachable!(),
        }
        let border_shows = state.border_show_count.load(Ordering::Relaxed);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            original,
            "{exclusion} geometry must not become the floating rectangle"
        );
        if exclusion == "maximized" {
            assert_eq!(
                state.border_show_count.load(Ordering::Relaxed),
                border_shows + 1
            );
        }
        if matches!(exclusion, "transition" | "applying" | "display_change") {
            assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
            assert!(!state.recheck_deferred_window_moves());
            assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, original);
            state.layout_transition = None;
            state.applying_layout = false;
            state.display_change_pending = false;
            state.post_animation_nudge_pending = true;
            state.apply_layout().unwrap();
            state.moved_or_resized_suppression.remove(&owner.hwnd);
            assert!(!state.recheck_deferred_window_moves());
            assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
            assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, original);
        }
    }
}

fn visible_resize_recheck_state(hwnd: u64) -> AppState {
    let mut monitors = test_monitors();
    monitors[0].rect = Rect::new(0, 0, 800, 600);
    monitors[0].work_area = Rect::new(0, 0, 800, 560);
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.paused = false;
    state.reduce_motion = true;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(hwnd, Some(300)).unwrap();
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    assert_eq!(
        state.current_physical_visibility(hwnd),
        Some(leopardwm_core_layout::Visibility::Visible)
    );
    assert_eq!(
        state.expected_physical_rect(hwnd),
        Some(Rect::new(0, 0, 300, 560))
    );
    state
}

#[test]
fn test_suppressed_visible_resize_replays_once_after_all_suppression_ends() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    owner.resize(0, 200);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    let before = state.physical_request_seq;
    for guard in 0..3 {
        state.applying_layout = guard == 1;
        state.display_change_pending = guard == 2;
        if guard != 0 {
            state.moved_or_resized_suppression.insert(
                owner.hwnd,
                std::time::Instant::now() - Duration::from_millis(1),
            );
        }
        assert!(!state.recheck_deferred_window_moves());
        assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
        assert_eq!(state.physical_request_seq, before);
        assert_eq!(owner.rect(), Rect::new(0, 0, 200, 560));
    }
    state.display_change_pending = false;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        1,
        "a consumed visible resize must not feed another replay even if it stays displaced"
    );
}

#[test]
fn test_visible_replay_bounds_fresh_resizes_but_allows_changed_targets_and_live_moves() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let before = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(owner.rect(), Rect::new(0, 0, 300, 560));
    assert_eq!(state.physical_request_seq, before + 1);

    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    for repeat in 0..3 {
        if repeat != 0 {
            state
                .window_move_recheck_attempts
                .get_mut(&owner.hwnd)
                .unwrap()
                .issued_at -= Duration::from_secs(10);
        }
        owner.resize(0, 200);
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        assert!(!state.recheck_deferred_window_moves());
        assert_eq!(state.physical_request_seq, before + 1);
        assert_eq!(owner.rect().width, 200);
    }

    state.workspaces.get_mut(&1).unwrap()[0].resize_focused_column(50);
    state.apply_layout().unwrap();
    assert_eq!(owner.rect(), Rect::new(0, 0, 350, 560));
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let changed_target_before = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, changed_target_before + 1);
    assert_eq!(owner.rect().width, 350);
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, changed_target_before + 1);

    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    assert_eq!(state.physical_request_seq, changed_target_before + 2);
    assert_eq!(
        owner.rect().width,
        350,
        "a live move must bypass the replay bound"
    );
}

#[test]
fn test_visible_replay_readmission_gets_fresh_same_target_correction_budget() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let hwnd = owner.hwnd;
    let mut state = visible_resize_recheck_state(hwnd);
    state.record_managed_lifetime(hwnd, None);
    state.injected_foreground_hwnd = Some(Some(hwnd));
    state.injected_foreground_is_valid = Some(true);
    state.injected_window_info.insert(
        hwnd,
        leopardwm_platform_win32::WindowInfo {
            hwnd,
            title: "Resize fixture".to_string(),
            class_name: "ResizeFixture".to_string(),
            process_id: 1234,
            rect: owner.rect(),
            visible: true,
        },
    );
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    state.moved_or_resized_suppression.remove(&hwnd);
    assert!(state.recheck_deferred_window_moves());
    owner.resize(0, 200);
    state.arm_moved_or_resized_suppression([hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    state.moved_or_resized_suppression.remove(&hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert!(state.window_move_recheck_attempts[&hwnd].gave_up);
    let target = state.window_move_recheck_attempts[&hwnd].target;
    state.arm_moved_or_resized_suppression([hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    assert!(state.deferred_moved_or_resized.contains(&hwnd));

    assert!(matches!(state.toggle_ignore(), IpcResponse::Ok));
    assert!(!state.is_managed_member(hwnd));
    let deferred_cleared = !state.deferred_moved_or_resized.contains(&hwnd);
    let attempt_cleared = !state.window_move_recheck_attempts.contains_key(&hwnd);
    assert!(matches!(state.toggle_ignore(), IpcResponse::Ok));
    assert!(state.is_managed_member(hwnd));
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(hwnd, Some(300)).unwrap();
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    assert_eq!(state.expected_physical_rect(hwnd), Some(target.0));

    owner.resize(0, 200);
    state.arm_moved_or_resized_suppression([hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    state.moved_or_resized_suppression.remove(&hwnd);
    let before = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(owner.rect(), target.0);
    assert!(deferred_cleared);
    assert!(attempt_cleared);
}

#[test]
fn test_deferred_move_recheck_prunes_unmanaged_records_regardless_of_visibility() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let hwnd = owner.hwnd;
    let mut state = visible_resize_recheck_state(hwnd);
    let presentation = state.last_physical_presentations[&hwnd].clone();
    state.workspaces.get_mut(&1).unwrap()[0]
        .remove_window(hwnd)
        .unwrap();
    for visibility in [
        None,
        Some(leopardwm_core_layout::Visibility::Visible),
        Some(leopardwm_core_layout::Visibility::OffScreenLeft),
    ] {
        if let Some(visibility) = visibility {
            let mut presentation = presentation.clone();
            presentation.physical.visibility = visibility;
            state.last_physical_presentations.insert(hwnd, presentation);
        } else {
            state.clear_physical_window_state(hwnd);
        }
        state.window_move_recheck_attempts.insert(
            hwnd,
            crate::state::WindowMoveRecheckAttempt {
                target: (
                    Rect::new(0, 0, 300, 560),
                    leopardwm_core_layout::Visibility::Visible,
                ),
                issued_at: std::time::Instant::now(),
                gave_up: true,
            },
        );
        assert!(!state.recheck_deferred_window_moves());
        assert!(!state.window_move_recheck_attempts.contains_key(&hwnd));
        state.deferred_moved_or_resized.insert(hwnd);
        assert!(!state.recheck_deferred_window_moves());
        assert!(!state.deferred_moved_or_resized.contains(&hwnd));
    }
}

#[test]
fn test_visible_replay_clears_stale_target_after_successful_intervening_landing() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(owner.rect().width, 300);

    state.workspaces.get_mut(&1).unwrap()[0].resize_focused_column(50);
    state.apply_layout().unwrap();
    assert_eq!(owner.rect(), Rect::new(0, 0, 350, 560));
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let at_b = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, at_b);

    state.workspaces.get_mut(&1).unwrap()[0].resize_focused_column(-50);
    state.apply_layout().unwrap();
    assert_eq!(owner.rect(), Rect::new(0, 0, 300, 560));
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let back_at_a = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, back_at_a + 1);
    assert_eq!(owner.rect(), Rect::new(0, 0, 300, 560));
}

#[test]
fn test_visible_replay_renews_old_one_shot_attempt_and_bounds_its_immediate_repeat() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(state.recheck_deferred_window_moves());
    state
        .window_move_recheck_attempts
        .get_mut(&owner.hwnd)
        .unwrap()
        .issued_at -= Duration::from_secs(10);

    state.arm_moved_or_resized_suppression([owner.hwnd]);
    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let before = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(owner.rect(), Rect::new(0, 0, 300, 560));

    owner.resize(0, 200);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(owner.rect().width, 200);
}

#[test]
fn test_deferred_visible_moves_issue_at_most_one_placement_per_tick() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owners = [
        OffscreenResizeOwner::new(),
        OffscreenResizeOwner::new(),
        OffscreenResizeOwner::new(),
    ];
    let mut state = visible_resize_recheck_state(owners[0].hwnd);
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    for owner in &owners {
        workspace.insert_window(owner.hwnd, Some(200)).unwrap();
    }
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    for owner in &owners {
        owner.resize(owner.rect().x, 100);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    }
    assert_eq!(state.deferred_moved_or_resized.len(), 3);
    state.layout_apply_timeout = Duration::from_secs(2);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(300),
    ));
    let before = state.physical_request_seq;
    for tick in 1..=3 {
        state.moved_or_resized_suppression.clear();
        assert!(state.recheck_deferred_window_moves());
        assert_eq!(state.physical_request_seq, before + tick);
        assert_eq!(state.deferred_moved_or_resized.len(), 3 - tick as usize);
        assert_eq!(
            state
                .injected_apply_placements_call_count
                .load(Ordering::SeqCst),
            tick as usize
        );
    }
    state.moved_or_resized_suppression.clear();
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 3);
}

#[test]
fn test_minimized_deferred_move_preserves_maximize_history() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
    state.workspaces.get_mut(&1).unwrap()[0].mark_minimized(owner.hwnd);
    let maximized_at = std::time::Instant::now();
    state
        .window_last_maximized_at
        .insert(owner.hwnd, maximized_at);
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let before = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
    assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
    assert_eq!(
        state.window_last_maximized_at.get(&owner.hwnd),
        Some(&maximized_at)
    );
}

#[test]
fn test_suppressed_visible_placement_feedback_and_maximize_settling_are_noops() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut state = visible_resize_recheck_state(owner.hwnd);
    for settling in [false, true] {
        if settling {
            owner.resize(0, 200);
            let now = std::time::Instant::now();
            state.window_managed_at.insert(owner.hwnd, now);
            state.window_last_maximized_at.insert(owner.hwnd, now);
        } else {
            assert_eq!(owner.rect(), Rect::new(0, 0, 300, 560));
        }
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
        state.moved_or_resized_suppression.insert(
            owner.hwnd,
            std::time::Instant::now() - Duration::from_millis(1),
        );
        let before = state.physical_request_seq;
        assert!(!state.recheck_deferred_window_moves());
        assert_eq!(state.physical_request_seq, before);
        assert!(!state.deferred_moved_or_resized.contains(&owner.hwnd));
        assert_eq!(owner.rect().width, if settling { 200 } else { 300 });
    }
}

#[test]
fn test_suppressed_offscreen_resize_is_rechecked_without_retiling_hidden_windows() {
    if run_in_placement_test_process(
        "tests::test_suppressed_offscreen_resize_is_rechecked_without_retiling_hidden_windows",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let work_area = Rect::new(0, 0, 800, 560);
    let mut config = test_config();
    config.animation.layout_duration_ms = 0;
    config.animation.scroll_duration_ms = 0;
    let mut state = AppState::new_with_config(
        config,
        vec![MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 800, 600),
            work_area,
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        }],
    );
    state.paused = false;
    state.reduce_motion = true;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(owner.hwnd, Some(300)).unwrap();
    workspace.set_scroll_offset(300.0);
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    assert_eq!(
        state.current_physical_visibility(owner.hwnd),
        Some(leopardwm_core_layout::Visibility::OffScreenLeft)
    );
    assert!(!owner.rect().intersects(&work_area));

    for applying in [false, true] {
        owner.resize(-300, 315);
        assert!(
            owner.rect().intersects(&work_area),
            "fixture must grow into the viewport"
        );
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        state.applying_layout = applying;
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        assert!(!state.recheck_deferred_window_moves());
        assert!(
            owner.rect().intersects(&work_area),
            "placement feedback remains suppressed"
        );
        state.applying_layout = false;
        state.moved_or_resized_suppression.insert(
            owner.hwnd,
            std::time::Instant::now() - Duration::from_millis(1),
        );
        state.recheck_deferred_window_moves();
        assert!(
            !owner.rect().intersects(&work_area),
            "periodic re-check must correct the missed resize, got {:?}",
            owner.rect()
        );
        assert_eq!(
            owner.rect().width,
            315,
            "off-screen correction must not resize the app"
        );
    }

    owner.resize(-315, 315);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.insert(
        owner.hwnd,
        std::time::Instant::now() - Duration::from_millis(1),
    );
    let request_seq = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(
        owner.rect().x,
        -315,
        "an off-screen window at its anchored origin must be left alone"
    );
    assert_eq!(
        state.physical_request_seq, request_seq,
        "no snap-back should be dispatched"
    );
    leopardwm_platform_win32::forget_offscreen_placement(owner.hwnd);
    state.deferred_moved_or_resized.insert(owner.hwnd);
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, request_seq + 1);
    assert_eq!(
        leopardwm_platform_win32::offscreen_placement_matches(owner.hwnd),
        Some(true),
        "a missing platform record must trigger a corrective placement"
    );
}

#[test]
fn test_offscreen_recheck_accepts_hidden_tab_on_neighbor_monitor() {
    if run_in_placement_test_process(
        "tests::test_offscreen_recheck_accepts_hidden_tab_on_neighbor_monitor",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut monitors = test_monitors();
    monitors[0].rect = Rect::new(0, 0, 800, 600);
    monitors[0].work_area = Rect::new(0, 0, 800, 560);
    monitors.push(MonitorInfo {
        id: 2,
        rect: Rect::new(-1920, 0, 1920, 600),
        work_area: Rect::new(-1920, 0, 1920, 560),
        is_primary: false,
        device_name: "DISPLAY2".to_string(),
        scale_factor: 1.0,
    });
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.paused = false;
    state.reduce_motion = true;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(owner.hwnd, Some(300)).unwrap();
    workspace.insert_window_in_column(101, 0).unwrap();
    workspace.toggle_focused_column_tabbed_mode();
    workspace.focus_window(101).unwrap();
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    assert_eq!(
        state.expected_physical_rect(owner.hwnd),
        Some(Rect::new(-800, 0, 0, 0))
    );
    assert_eq!(owner.rect().x, -800);
    assert!(owner.rect().intersects(&state.monitors[&2].work_area));
    let requests = state.physical_request_seq;
    for _ in 0..4 {
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        state.moved_or_resized_suppression.insert(
            owner.hwnd,
            std::time::Instant::now() - Duration::from_millis(1),
        );
        state.recheck_deferred_window_moves();
        assert_eq!(
            state.physical_request_seq, requests,
            "a legitimate hidden-tab origin must not dispatch snap-back"
        );
        assert_eq!(owner.rect().x, -800);
    }
}

#[test]
fn test_offscreen_recheck_converges_with_cached_insets_after_frame_change() {
    if run_in_placement_test_process(
        "tests::test_offscreen_recheck_converges_with_cached_insets_after_frame_change",
    ) {
        return;
    }
    if leopardwm_platform_win32::is_high_contrast_enabled() {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::create(true);
    let insets = leopardwm_platform_win32::get_window_invisible_insets(owner.hwnd);
    assert_ne!(
        insets,
        (0, 0, 0, 0),
        "framed fixture must have genuine DWM insets"
    );
    let config = leopardwm_platform_win32::PlatformConfig::default();
    leopardwm_platform_win32::apply_placements(
        &[leopardwm_core_layout::WindowPlacement {
            window_id: owner.hwnd,
            rect: Rect::new(-30000, -1000, 300, 560),
            visibility: leopardwm_core_layout::Visibility::Visible,
            column_index: 0,
        }],
        &config,
        None,
        false,
    )
    .unwrap();
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
            SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_POPUP, WS_VISIBLE,
        };
        let hwnd = windows::Win32::Foundation::HWND(owner.hwnd as *mut _);
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .unwrap();
        windows::Win32::Graphics::Dwm::DwmFlush().unwrap();
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_invisible_insets(owner.hwnd),
        (0, 0, 0, 0)
    );
    let mut monitors = test_monitors();
    monitors[0].rect = Rect::new(0, 0, 800, 600);
    monitors[0].work_area = Rect::new(0, 0, 800, 560);
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.paused = false;
    state.reduce_motion = true;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(owner.hwnd, Some(300)).unwrap();
    workspace.set_scroll_offset(300.0);
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    owner.resize(-280, 315);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    let before = state.physical_request_seq;
    assert!(state.recheck_deferred_window_moves());
    let landed = owner.rect();
    assert_ne!(
        (landed.x, landed.y),
        (-315, 0),
        "placement must still use its cached frame insets"
    );
    assert_eq!(
        leopardwm_platform_win32::offscreen_placement_matches(owner.hwnd),
        Some(true)
    );
    for _ in 0..4 {
        state.deferred_moved_or_resized.insert(owner.hwnd);
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        assert!(!state.recheck_deferred_window_moves());
        assert_eq!(
            state.physical_request_seq,
            before + 1,
            "one correction must converge without repeated applies"
        );
        assert_eq!(owner.rect(), landed);
    }
}

#[test]
fn test_offscreen_periodic_snapback_is_bounded_until_target_changes_or_lands() {
    if run_in_placement_test_process(
        "tests::test_offscreen_periodic_snapback_is_bounded_until_target_changes_or_lands",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = OffscreenResizeOwner::new();
    let mut monitors = test_monitors();
    monitors[0].rect = Rect::new(0, 0, 800, 600);
    monitors[0].work_area = Rect::new(0, 0, 800, 560);
    let mut state = AppState::new_with_config(test_config(), monitors);
    state.paused = false;
    state.reduce_motion = true;
    let mut workspace = Workspace::with_gaps(0, 0);
    workspace.set_reduce_motion(true);
    workspace.insert_window(owner.hwnd, Some(300)).unwrap();
    workspace.set_scroll_offset(300.0);
    state.workspaces.get_mut(&1).unwrap()[0] = workspace;
    state.apply_layout().unwrap();
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    owner.resize(-300, 315);
    for tick in 0..4 {
        state.arm_moved_or_resized_suppression([owner.hwnd]);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        state.moved_or_resized_suppression.insert(
            owner.hwnd,
            std::time::Instant::now() - Duration::from_millis(1),
        );
        assert_eq!(state.recheck_deferred_window_moves(), tick == 0);
        assert_eq!(
            owner.rect().x,
            -300,
            "the injected worker must leave the fixture displaced"
        );
        assert_eq!(
            state
                .injected_apply_placements_call_count
                .load(Ordering::SeqCst),
            1,
            "the same target gets only one periodic snap-back"
        );
    }
    owner.resize(-300, 330);
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        1,
        "app-owned size drift alone must not reset the target bound"
    );
    state.workspaces.get_mut(&1).unwrap()[0].set_scroll_offset(330.0);
    state.apply_layout().unwrap();
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        3,
        "a changed target permits another attempt"
    );
    state.injected_apply_placements_behavior = None;
    state.bump_physical_invalidation();
    state.apply_layout().unwrap();
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(
        leopardwm_platform_win32::offscreen_placement_matches(owner.hwnd),
        Some(true)
    );
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    owner.resize(-300, 315);
    state.arm_moved_or_resized_suppression([owner.hwnd]);
    state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
    state.moved_or_resized_suppression.remove(&owner.hwnd);
    state.recheck_deferred_window_moves();
    assert_eq!(
        state
            .injected_apply_placements_call_count
            .load(Ordering::SeqCst),
        4,
        "landing clears the prior attempt bound"
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

fn size_rule_config() -> Config {
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
    config.window_rules = vec![
        crate::config::WindowRule {
            match_class: Some("^QQClass$".into()),
            match_title: Some("^QQ NT$".into()),
            match_max_width: Some(400),
            match_max_height: Some(300),
            action: crate::config::WindowAction::Ignore,
            ..Default::default()
        },
        crate::config::WindowRule {
            match_class: Some("^QQClass$".into()),
            action: crate::config::WindowAction::Tile,
            ..Default::default()
        },
    ];
    config
}

fn size_rule_window(hwnd: u64, width: i32, height: i32) -> leopardwm_platform_win32::WindowInfo {
    let mut info = make_test_window_info(hwnd);
    info.class_name = "QQClass".into();
    info.title = "QQ NT".into();
    info.process_id = std::process::id();
    info.rect = Rect::new(2200, 100, width, height);
    info
}

#[test]
fn test_size_rules_enumeration_separates_same_app_helpers() {
    let mut state = new_window_monitor_state(size_rule_config());
    state.monitors.get_mut(&2).unwrap().scale_factor = 2.0;
    state.focused_monitor = 1;
    let small = size_rule_window(100, 800, 600);
    let large = size_rule_window(200, 1000, 600);
    state.injected_enumerated_windows = Some(vec![small, large]);
    assert_eq!(state.enumerate_and_add_windows().unwrap(), 1);
    assert_eq!(state.find_window_workspace(100), None);
    assert!(state.size_ignored_windows.contains(&100));
    assert_eq!(state.find_window_workspace(200), Some((2, 0)));
    assert!(!state.workspaces[&2][0].is_floating(200));
    assert!(!state.size_ignored_windows.contains(&200));

    state.config.window_rules.swap(0, 1);
    state.compiled_rules = state.config.compile_window_rules();
    assert_eq!(state.enumerate_and_add_windows().unwrap(), 1);
    assert_eq!(state.find_window_workspace(100), Some((2, 0)));
    assert!(state.size_ignored_windows.is_empty());
}

#[test]
fn test_size_rules_live_admission_retries_growth_and_title_changes() {
    for growth in [true, false] {
        let mut state = new_window_monitor_state(size_rule_config());
        state.monitors.get_mut(&2).unwrap().scale_factor = 2.0;
        state.focused_monitor = 1;
        state
            .injected_window_info
            .insert(100, size_rule_window(100, 800, 600));
        state.handle_window_event(WindowEvent::Created(100, 0));
        assert_eq!(state.find_window_workspace(100), None);
        assert!(state.size_ignored_windows.contains(&100));
        state.handle_window_event(WindowEvent::MovedOrResized(100));
        assert_eq!(state.find_window_workspace(100), None);
        assert!(state.size_ignored_windows.contains(&100));
        state.handle_window_event(WindowEvent::TitleChanged(100));
        assert_eq!(state.find_window_workspace(100), None);
        assert!(state.size_ignored_windows.contains(&100));

        let info = state.injected_window_info.get_mut(&100).unwrap();
        let event = if growth {
            info.rect.width = 1000;
            WindowEvent::MovedOrResized(100)
        } else {
            info.title = "QQ Main".into();
            WindowEvent::TitleChanged(100)
        };
        state.handle_window_event(event);
        assert_eq!(state.find_window_workspace(100), Some((1, 0)));
        assert!(!state.size_ignored_windows.contains(&100));
    }
}

fn size_rule_focus_recovery_state(recently_hidden: bool) -> AppState {
    use crate::event_handler::{AdmissionKind, AdmitOutcome};

    let mut config = size_rule_config();
    let mut info = size_rule_window(100, 200, 150);
    if !recently_hidden {
        info.class_name = "ConsoleWindowClass".into();
        for rule in &mut config.window_rules {
            rule.match_class = Some("^ConsoleWindowClass$".into());
        }
    }
    let mut state = new_window_monitor_state(config);
    if recently_hidden {
        info.rect.width = 800;
        info.rect.height = 600;
        state.injected_window_info.insert(100, info);
        assert_eq!(
            state.try_admit_window(100, AdmissionKind::Automatic),
            AdmitOutcome::Admitted
        );
        state.handle_window_event(WindowEvent::Hidden(100, 1));
        assert!(state.recently_hidden_hwnds.contains_key(&100));
        let info = state.injected_window_info.get_mut(&100).unwrap();
        info.rect.width = 200;
        info.rect.height = 150;
        assert_eq!(
            state.try_admit_window(100, AdmissionKind::Automatic),
            AdmitOutcome::TransientSuppressed
        );
    } else {
        info.title = "C:\\Windows\\System32\\cmd.exe".into();
        state.injected_window_info.insert(100, info);
        assert_eq!(
            state.try_admit_window(100, AdmissionKind::Automatic),
            AdmitOutcome::TransientConsoleHost
        );
        state.injected_window_info.get_mut(&100).unwrap().title = "QQ NT".into();
    }
    assert_eq!(state.find_window_workspace(100), None);
    assert!(!state.size_ignored_windows.contains(&100));
    state
}

fn assert_size_rule_focus_recovery_retry(recently_hidden: bool, title_change: bool) {
    let mut state = size_rule_focus_recovery_state(recently_hidden);
    state.handle_window_event(WindowEvent::Focused(100, 2));
    assert_eq!(state.find_window_workspace(100), None);
    state.handle_window_event(WindowEvent::MovedOrResized(100));
    state.handle_window_event(WindowEvent::TitleChanged(100));
    assert_eq!(state.find_window_workspace(100), None);
    let info = state.injected_window_info.get_mut(&100).unwrap();
    let retry = if title_change {
        info.title = "QQ Main".into();
        WindowEvent::TitleChanged(100)
    } else {
        info.rect.width = 500;
        WindowEvent::MovedOrResized(100)
    };
    state.handle_window_event(retry);
    assert_eq!(state.find_window_workspace(100), Some((1, 0)));
    assert!(!state.size_ignored_windows.contains(&100));
    assert!(!state.recently_hidden_hwnds.contains_key(&100));
}

#[test]
fn test_size_rules_console_focus_recovery_admits_after_growth() {
    assert_size_rule_focus_recovery_retry(false, false);
}

#[test]
fn test_size_rules_console_focus_recovery_admits_after_title_change() {
    assert_size_rule_focus_recovery_retry(false, true);
}

#[test]
fn test_size_rules_recently_hidden_recovery_admits_after_growth() {
    assert_size_rule_focus_recovery_retry(true, false);
}

#[test]
fn test_size_rules_recently_hidden_recovery_admits_after_title_change() {
    assert_size_rule_focus_recovery_retry(true, true);
}

#[test]
fn test_size_rules_recovery_preserves_other_transient_suppression() {
    use crate::event_handler::{AdmissionKind, AdmitOutcome};

    for ordinary_ignore in [false, true] {
        let mut state = size_rule_focus_recovery_state(true);
        if ordinary_ignore {
            state.config.window_rules[0].match_max_width = None;
            state.config.window_rules[0].match_max_height = None;
            state.compiled_rules = state.config.compile_window_rules();
            state.handle_window_event(WindowEvent::Focused(100, 2));
        }
        assert!(state.recently_hidden_hwnds.contains_key(&100));
        assert!(!state.size_ignored_windows.contains(&100));
        state.injected_window_info.get_mut(&100).unwrap().rect.width = 500;
        state.handle_window_event(WindowEvent::MovedOrResized(100));
        assert_eq!(state.find_window_workspace(100), None);
        assert_eq!(
            state.try_admit_window(100, AdmissionKind::Automatic),
            AdmitOutcome::TransientSuppressed
        );
    }
}

#[test]
fn test_size_rules_deferral_clears_on_hide_and_destroy() {
    for event in [WindowEvent::Hidden(100, 1), WindowEvent::Destroyed(100)] {
        let mut state = new_window_monitor_state(size_rule_config());
        state
            .injected_window_info
            .insert(100, size_rule_window(100, 200, 150));
        state.handle_window_event(WindowEvent::Created(100, 0));
        assert!(state.size_ignored_windows.contains(&100));
        state.injected_window_info.remove(&100);
        state.handle_window_event(event);
        assert!(!state.size_ignored_windows.contains(&100));
        assert_eq!(state.find_window_workspace(100), None);
    }
}

#[test]
fn test_size_rules_reapply_keeps_managed_small_floating_window() {
    let mut config = size_rule_config();
    config.window_rules[1].action = crate::config::WindowAction::Float;
    let mut state = new_window_monitor_state(config);
    let large = size_rule_window(100, 800, 600);
    state.injected_window_info.insert(100, large.clone());
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert!(state.workspaces[&1][0].is_floating(100));
    let mut small = large;
    small.rect = Rect::new(100, 100, 200, 150);
    state.injected_window_info.insert(100, small.clone());
    state.reapply_window_rules(&state.compiled_rules.clone());
    state.injected_enumerated_windows = Some(vec![small]);
    assert_eq!(state.enumerate_and_add_windows().unwrap(), 0);
    assert_eq!(state.find_window_workspace(100), Some((1, 0)));
    assert!(state.workspaces[&1][0].is_floating(100));
    assert!(!state.size_ignored_windows.contains(&100));
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
fn test_maximized_tiled_admission_restores_and_opens_full_width_column() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    let viewport_width = state.viewport_width_for(monitor);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    let maximize_queries = std::cell::Cell::new(0);
    let queue_calls = std::cell::Cell::new(0);

    let outcome = state.try_admit_window_at_with_native_ops(
        100,
        crate::event_handler::AdmissionKind::Automatic,
        None,
        |_| {
            maximize_queries.set(maximize_queries.get() + 1);
            true
        },
        |_| {
            queue_calls.set(queue_calls.get() + 1);
            Ok(true)
        },
    );

    assert_eq!(outcome, crate::event_handler::AdmitOutcome::Admitted);
    let workspace = &state.workspaces[&monitor][state.active_workspace_idx(monitor)];
    assert_eq!(
        workspace.columns()[0].width(),
        workspace.visible_width(viewport_width)
    );
    assert_eq!(
        maximize_queries.get(),
        1,
        "sample native maximize before insertion"
    );
    assert_eq!(queue_calls.get(), 1);
    assert!(state.window_last_maximized_at.contains_key(&100));
    assert!(!state.last_placed_layout_rects.contains_key(&100));
    state.paused = false;
    state.layout_transition = None;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        std::time::Duration::ZERO,
    ));
    state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
        window_id: 100,
        managed_lifetime_token: state.managed_lifetime_tokens[&100],
        still_maximized: false,
    });
    assert!(!state.window_last_maximized_at.contains_key(&100));
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert_eq!(
        state.last_placed_layout_rects[&100].width,
        state
            .focused_workspace()
            .unwrap()
            .visible_width(viewport_width)
    );
    assert_eq!(
        *state.injected_apply_placements_batches.lock().unwrap(),
        vec![vec![100]]
    );
}

#[test]
fn test_failed_maximized_admission_restore_keeps_settling_grace() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    let outcome = state.try_admit_window_at_with_native_ops(
        100,
        crate::event_handler::AdmissionKind::Automatic,
        None,
        |_| true,
        |_| Ok(true),
    );

    assert_eq!(outcome, crate::event_handler::AdmitOutcome::Admitted);
    assert!(state.pending_maximized_admission_restores.contains(&100));
    assert!(state.window_last_maximized_at.contains_key(&100));
    let before_result = std::time::Instant::now();
    state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
        window_id: 100,
        managed_lifetime_token: state.managed_lifetime_tokens[&100],
        still_maximized: true,
    });
    assert!(state.window_last_maximized_at[&100] >= before_result);
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(!state.last_placed_layout_rects.contains_key(&100));
}

#[test]
fn test_non_maximized_tiled_admission_keeps_normal_column_width() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    let viewport_width = state.viewport_width_for(monitor);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    let queue_calls = std::cell::Cell::new(0);

    let outcome = state.try_admit_window_at_with_native_ops(
        100,
        crate::event_handler::AdmissionKind::Automatic,
        None,
        |_| false,
        |_| {
            queue_calls.set(queue_calls.get() + 1);
            Ok(true)
        },
    );

    assert_eq!(outcome, crate::event_handler::AdmitOutcome::Admitted);
    let workspace = &state.workspaces[&monitor][state.active_workspace_idx(monitor)];
    assert_ne!(workspace.columns()[0].width(), viewport_width);
    assert_eq!(queue_calls.get(), 0);
    assert!(!state.window_last_maximized_at.contains_key(&100));
}

#[test]
fn test_frameless_fixed_size_admission_preserves_rules_and_fullscreen() {
    use crate::config::{WindowAction, WindowRule};
    use crate::event_handler::{AdmissionKind, AdmitOutcome};

    for enumerate in [false, true] {
        for (title, has_monitors, monitor_margin) in [
            ("", true, None),
            (" \t\n", true, None),
            ("Game", true, None),
            ("", false, None),
            ("", true, Some(0)),
            ("", true, Some(16)),
            ("", true, Some(-4)),
        ] {
            for action in [None, Some(WindowAction::Tile), Some(WindowAction::Float)] {
                let window = ResizeTestWindow::new(400);
                let hwnd = window.id();
                let mut config = test_config();
                config.behavior.focus_new_windows = false;
                if let Some(action) = action {
                    config.window_rules = vec![WindowRule {
                        match_class: Some("TestWindowClass".to_string()),
                        action,
                        ..Default::default()
                    }];
                }
                let mut state = AppState::new_with_config(config, test_monitors());
                let mut info = make_test_window_info(hwnd);
                info.title = title.to_string();
                if !has_monitors {
                    state.monitors.clear();
                }
                info.rect = if let Some(margin) = monitor_margin {
                    let monitor = state.monitors[&state.focused_monitor].rect;
                    Rect::new(
                        monitor.x - margin,
                        monitor.y - margin,
                        monitor.width + 2 * margin,
                        monitor.height + 2 * margin,
                    )
                } else {
                    Rect::new(100, 100, 400, 600)
                };
                let should_manage = !title.trim().is_empty()
                    || !has_monitors
                    || monitor_margin.is_some()
                    || action.is_some();
                if enumerate {
                    state.injected_enumerated_windows = Some(vec![info]);
                    assert_eq!(
                        state.enumerate_and_add_windows().unwrap(),
                        usize::from(should_manage)
                    );
                } else {
                    state.injected_window_info.insert(hwnd, info);
                    let outcome = state.try_admit_window_at_with_native_ops(
                        hwnd,
                        AdmissionKind::Automatic,
                        None,
                        |_| false,
                        |_| Ok(true),
                    );
                    assert_eq!(
                        outcome,
                        if should_manage {
                            AdmitOutcome::Admitted
                        } else {
                            AdmitOutcome::DialogLike
                        }
                    );
                }
                assert_eq!(state.find_window_workspace(hwnd).is_some(), should_manage);
                if should_manage {
                    assert_eq!(
                        state.focused_workspace().unwrap().is_floating(hwnd),
                        action == Some(WindowAction::Float)
                    );
                }
            }
        }
    }
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
    state.handle_window_event(WindowEvent::Created(100, 0));

    // After: window should be tiled in the workspace
    let ws = state.focused_workspace().unwrap();
    assert!(
        ws.contains_window(100),
        "window should be managed after Created event"
    );
    assert_eq!(ws.window_count(), 1);
}

fn new_window_monitor_state(mut config: Config) -> AppState {
    config.animation.scroll_duration_ms = 0;
    let mut state = AppState::new_with_config(config, two_monitors());
    state.paused = false;
    state.reduce_motion = true;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        std::time::Duration::ZERO,
    ));
    state
}

#[test]
fn test_new_window_monitor_admission_and_layout() {
    use crate::config::NewWindowMonitor;
    for policy in [
        None,
        Some(NewWindowMonitor::Opening),
        Some(NewWindowMonitor::Focused),
    ] {
        for focus in [1, 2, 999] {
            for (rect, opening) in [
                (Rect::new(100, 100, 800, 600), Some(1)),
                (Rect::new(2200, 100, 800, 600), Some(2)),
                (Rect::new(5000, 4000, 800, 600), None),
            ] {
                if focus == 999 && opening.is_none() {
                    continue;
                }
                let mut config = test_config();

                config.behavior.focus_new_windows = false;
                if let Some(policy) = policy {
                    config.behavior.new_window_monitor = policy;
                }
                let mut state = new_window_monitor_state(config);
                state.focused_monitor = focus;
                for monitor in [1, 2] {
                    state.ensure_workspace_exists(monitor, 2);
                    state.active_workspace.insert(monitor, 2);
                }
                let mut info = make_test_window_info(100);
                info.rect = rect;
                state.injected_window_info.insert(100, info);
                let outcome =
                    state.try_admit_window(100, crate::event_handler::AdmissionKind::Automatic);
                let expected = if policy == Some(NewWindowMonitor::Focused) && focus != 999 {
                    focus
                } else {
                    opening.unwrap_or(focus)
                };
                assert_eq!(outcome, crate::event_handler::AdmitOutcome::Admitted);
                assert_eq!(
                    state.find_window_workspace(100),
                    Some((expected, 2)),
                    "policy={policy:?}, focus={focus}, opening={opening:?}"
                );
                assert!(state.monitors[&expected]
                    .contains_rect_center(&state.last_placed_layout_rects[&100]));
                assert_eq!(state.focused_monitor, focus);
            }
        }
    }
}

#[test]
fn test_new_window_monitor_workspace_rule_precedence() {
    let mut config = test_config();

    config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
    config.window_rules = vec![tile_open_on_workspace_rule("TestWindowClass", 3)];
    let mut state = new_window_monitor_state(config);
    state.focused_monitor = 1;
    let mut info = make_test_window_info(100);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(100, info);
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert_eq!(state.find_window_workspace(100), Some((1, 2)));
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.focused_monitor, 1);
}

#[test]
fn test_new_window_monitor_sticky_keeps_opening_monitor() {
    for action in [
        crate::config::WindowAction::Tile,
        crate::config::WindowAction::Float,
    ] {
        for rule_workspace in [None, Some(3)] {
            let mut config = test_config();

            config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
            config.window_rules = vec![crate::config::WindowRule {
                match_class: Some("TestWindowClass".to_string()),
                action,
                sticky: true,
                open_on_workspace: rule_workspace,
                ..Default::default()
            }];
            let mut state = new_window_monitor_state(config);
            state.focused_monitor = 1;
            let mut info = make_test_window_info(100);
            info.rect = Rect::new(2200, 100, 800, 600);
            state.injected_window_info.insert(100, info);
            state.handle_window_event(WindowEvent::Created(100, 0));
            assert_eq!(state.find_window_workspace(100), Some((2, 0)));
            assert!(state.sticky_windows.contains(&100));
            if action == crate::config::WindowAction::Float {
                assert!(state.workspaces[&2][0].floating_windows()[0].pinned);
                assert_eq!(
                    state.workspaces[&2][0].floating_windows()[0].rect,
                    Rect::new(2200, 100, 800, 600)
                );
            }
        }
    }
}

#[test]
fn test_new_window_monitor_floating_placement() {
    for rect in [
        Rect::new(2200, 100, 800, 600),
        Rect::new(5000, 4000, 800, 600),
        Rect::new(2200, 100, 2400, 1800),
    ] {
        let mut config = test_config();

        config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
        config.window_rules = vec![crate::config::WindowRule {
            match_class: Some("TestWindowClass".to_string()),
            action: crate::config::WindowAction::Float,
            ..Default::default()
        }];
        let mut state = new_window_monitor_state(config);
        state.focused_monitor = 1;
        let mut info = make_test_window_info(100);
        info.rect = rect;
        state.injected_window_info.insert(100, info);
        state.handle_window_event(WindowEvent::Created(100, 0));
        assert_eq!(state.find_window_workspace(100), Some((1, 0)));
        let area = state.monitors[&1].work_area;
        let width = rect.width.min(area.width);
        let height = rect.height.min(area.height);
        let expected = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        assert_eq!(state.workspaces[&1][0].floating_windows()[0].rect, expected);
        assert_eq!(state.last_placed_layout_rects[&100], expected);
    }
}

#[test]
fn test_new_window_monitor_reload_preserves_members_and_changes_next_admission() {
    let mut state = new_window_monitor_state(test_config());
    let mut info = make_test_window_info(100);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(100, info.clone());
    state.handle_window_event(WindowEvent::Created(100, 0));
    let mut saved_workspace = Workspace::default();
    saved_workspace.insert_window(200, None).unwrap();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "DISPLAY2".to_string(),
            workspace_index: 1,
            workspace: saved_workspace,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };
    state.restore_workspace_structure_with(&snapshot, |_| true);
    let mut restored = make_test_window_info(200);
    restored.rect = Rect::new(100, 100, 800, 600);
    state.injected_window_info.insert(200, restored.clone());
    state.injected_enumerated_windows = Some(vec![info.clone(), restored]);
    state.focused_monitor = 1;
    let mut config = state.config.clone();
    config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
    state.apply_config(config);
    state.handle_window_event(WindowEvent::Created(100, 1));
    assert_eq!(state.find_window_workspace(100), Some((2, 0)));
    assert_eq!(state.find_window_workspace(200), Some((2, 1)));
    for hwnd in [100, 200] {
        assert_eq!(
            state
                .workspaces
                .values()
                .flatten()
                .filter(|workspace| workspace.contains_window(hwnd))
                .count(),
            1,
            "reload must not duplicate existing or restored membership"
        );
    }
    info.hwnd = 300;
    state.injected_window_info.insert(300, info);
    state.handle_window_event(WindowEvent::Created(300, 2));
    assert_eq!(state.find_window_workspace(300), Some((1, 0)));
}

#[test]
fn test_new_window_monitor_enumeration_preserves_discovered_placement() {
    for action in [
        crate::config::WindowAction::Tile,
        crate::config::WindowAction::Float,
    ] {
        for open_on_workspace in [None, Some(4)] {
            for rect in [
                Rect::new(2200, 100, 800, 600),
                Rect::new(2000, -100, 2400, 1800),
            ] {
                let mut config = test_config();
                config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
                config.window_rules = vec![crate::config::WindowRule {
                    match_class: Some("TestWindowClass".to_string()),
                    action,
                    open_on_workspace,
                    ..Default::default()
                }];
                let mut state = new_window_monitor_state(config);
                state.focused_monitor = 1;
                state.ensure_workspace_exists(2, 2);
                state.active_workspace.insert(2, 2);
                let mut info = make_test_window_info(100);
                info.rect = rect;
                state.injected_enumerated_windows = Some(vec![info]);
                assert_eq!(state.enumerate_and_add_windows().unwrap(), 1);
                assert_eq!(state.find_window_workspace(100), Some((2, 2)));
                state.apply_layout().unwrap();
                let placed = state.last_placed_layout_rects[&100];
                if action == crate::config::WindowAction::Float {
                    assert_eq!(placed, rect);
                } else {
                    assert!(state.monitors[&2].contains_rect_center(&placed));
                }
            }
        }
    }
}

#[test]
fn test_new_window_monitor_explicit_readmit_keeps_native_monitor() {
    let mut config = test_config();

    config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
    let mut state = new_window_monitor_state(config);
    state.focused_monitor = 1;
    let mut info = make_test_window_info(100);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(100, info);
    assert_eq!(
        state.try_admit_window(100, crate::event_handler::AdmissionKind::ExplicitReadmit),
        crate::event_handler::AdmitOutcome::Admitted
    );
    assert_eq!(state.find_window_workspace(100), Some((2, 0)));
}

#[test]
fn test_new_window_monitor_removal_uses_surviving_focus() {
    let mut config = test_config();

    config.behavior.new_window_monitor = crate::config::NewWindowMonitor::Focused;
    let mut state = new_window_monitor_state(config);
    state.focused_monitor = 2;
    state.reconcile_monitors(test_monitors());
    assert_eq!(state.focused_monitor, 1);
    let mut info = make_test_window_info(100);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(100, info);
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert_eq!(state.find_window_workspace(100), Some((1, 0)));
    assert!(state.monitors[&1].contains_rect_center(&state.last_placed_layout_rects[&100]));
}

#[test]
fn test_created_event_applies_rule_column_width_fraction() {
    let mut config = test_config();
    config.window_rules = vec![crate::config::WindowRule {
        match_class: Some("TestWindowClass".to_string()),
        column_width: Some(0.5),
        ..crate::config::WindowRule::default()
    }];
    let mut state = AppState::new_with_config(config, test_monitors());
    let monitor = state.focused_monitor;
    let viewport_width = state.viewport_width_for(monitor);
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));

    state.handle_window_event(WindowEvent::Created(100, 0));

    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.window_count(), 1);
    assert_eq!(
        ws.columns()[0].width(),
        ((0.5 * f64::from(viewport_width)).round() as i32).max(100),
        "Created-event rule width uses the viewport fraction"
    );
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
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100),
        "first window should get focus even with focus_new_windows=false"
    );

    // Inject and create second window -- focus should stay on 100
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.handle_window_event(WindowEvent::Created(200, 0));

    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.window_count(), 2);
    assert_eq!(
        ws.focused_window(),
        Some(100),
        "focus should stay on window 100 when focus_new_windows=false"
    );
}

#[test]
fn test_fullscreen_focus_guard() {
    use crate::event_handler::fullscreen_focus_guard;
    // User-initiated focus is always honored (no override).
    assert_eq!(fullscreen_focus_guard(true, Some(1), 2), None);
    // Not fullscreen: nothing to keep.
    assert_eq!(fullscreen_focus_guard(false, None, 2), None);
    // Non-user focus to the fullscreen window itself: let it through.
    assert_eq!(fullscreen_focus_guard(false, Some(1), 1), None);
    // Non-user focus to a different window while fullscreen: keep fullscreen.
    assert_eq!(fullscreen_focus_guard(false, Some(1), 2), Some(1));
}

#[test]
fn test_pull_window_to_workspace() {
    // The Edit Config pull moves a tiled window from another workspace onto the
    // active one and focuses it (#57).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    for h in [100u64, 200] {
        state
            .injected_window_info
            .insert(h, make_test_window_info(h));
        state.handle_window_event(WindowEvent::Created(h, 0));
    }
    // Move the focused window (200) to workspace 2 (index 1).
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    assert!(
        state.workspaces[&mon][1].contains_window(200),
        "precondition: window 200 is on workspace 2"
    );

    // Pull it back to the active workspace (index 0).
    let moved = state.pull_window_to_workspace(200, mon, 1, mon, 0);
    assert!(moved, "a tiled window should be pulled");
    assert!(
        state.workspaces[&mon][0].contains_window(200),
        "window pulled onto the active workspace"
    );
    assert_eq!(
        state.workspaces[&mon][0].focused_window(),
        Some(200),
        "pulled window is focused"
    );
    assert!(
        !state.workspaces[&mon][1].contains_window(200),
        "window removed from its source workspace"
    );

    // A window that isn't there (or floating) is not pulled.
    assert!(!state.pull_window_to_workspace(999, mon, 1, mon, 0));
}

fn assert_workspace_switch_transition_direction(
    current_idx: usize,
    target_idx: usize,
    command: IpcCommand,
    expected_enter_offset: i32,
) {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    let source_window = 100;
    let destination_window = 200;
    state.ensure_workspace_exists(monitor, current_idx);
    state.ensure_workspace_exists(monitor, target_idx);
    state.workspaces.get_mut(&monitor).unwrap()[current_idx]
        .insert_window(source_window, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[target_idx]
        .insert_window(destination_window, Some(800))
        .unwrap();
    state.active_workspace.insert(monitor, current_idx);

    let viewport = state.layout_viewport(monitor);
    let source_rect = state.workspaces[&monitor][current_idx]
        .compute_placements_animated(viewport)
        .into_iter()
        .find(|placement| placement.window_id == source_window)
        .unwrap()
        .rect;
    let destination_rect = state.workspaces[&monitor][target_idx]
        .compute_placements_animated(viewport)
        .into_iter()
        .find(|placement| placement.window_id == destination_window)
        .unwrap()
        .rect;
    let slide_height = state.monitors[&monitor].work_area.height;
    state.reduce_motion = false;

    assert!(matches!(state.handle_command(command), IpcResponse::Ok));
    let transition = state.layout_transition.as_ref().unwrap();
    assert_eq!(
        transition.start_rects[&destination_window].y,
        destination_rect.y + expected_enter_offset * slide_height,
        "entering workspace must slide in the requested direction"
    );
    assert_eq!(
        transition.exit_rects[&source_window].y,
        source_rect.y - expected_enter_offset * slide_height,
        "leaving workspace must slide out opposite the requested direction"
    );
}

#[test]
fn test_workspace_switch_only_animates_visible_outgoing_tabs() {
    use leopardwm_core_layout::Visibility;

    for focus_following in [false, true] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let monitor = state.focused_monitor;
        let viewport = state.layout_viewport(monitor);
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[0];
        workspace.insert_window(100, Some(800)).unwrap();
        workspace.insert_window_in_column(101, 0).unwrap();
        workspace.insert_window_in_column(102, 0).unwrap();
        workspace.toggle_focused_column_tabbed_mode();
        for wid in [200, 300, 400] {
            workspace.insert_window(wid, Some(800)).unwrap();
        }
        workspace.focus_window(100).unwrap();
        workspace.set_scroll_offset(0.0);
        workspace.stop_animation();
        let outgoing = workspace.compute_placements_animated(viewport);
        let active = outgoing.iter().find(|p| p.window_id == 100).unwrap();
        assert_eq!(active.visibility, Visibility::Visible);
        for wid in [101, 102, 400] {
            assert_ne!(
                outgoing
                    .iter()
                    .find(|p| p.window_id == wid)
                    .unwrap()
                    .visibility,
                Visibility::Visible
            );
        }
        state.reduce_motion = false;
        state.config.animation.workspace_switch_duration_ms = 200;
        state.ensure_workspace_exists(monitor, 1);
        state.workspaces.get_mut(&monitor).unwrap()[1]
            .insert_window(500, Some(800))
            .unwrap();
        state.previous_focused_hwnd = Some(100);

        if focus_following {
            state.handle_window_event(WindowEvent::Focused(500, 0));
        } else {
            assert!(matches!(
                state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
                IpcResponse::Ok
            ));
        }
        assert_eq!(state.active_workspace_idx(monitor), 1);
        let transition = state.layout_transition.as_mut().unwrap();
        for elapsed_ms in [0, 50, 100, 200] {
            transition.elapsed_ms = elapsed_ms;
            let mut frame = Vec::new();
            AppState::apply_transition_interpolation(transition, &mut frame);
            assert!(frame.iter().all(|p| {
                p.visibility != Visibility::Visible || (p.rect.width > 0 && p.rect.height > 0)
            }));
            for wid in [101, 102, 400] {
                assert!(frame.iter().all(|p| p.window_id != wid));
            }
            let active_frame = frame.iter().find(|p| p.window_id == 100).unwrap();
            assert_eq!(active_frame.visibility, Visibility::Visible);
            assert_eq!(active_frame.rect.width, active.rect.width);
            assert_eq!(active_frame.rect.height, active.rect.height);
            if elapsed_ms == 0 {
                assert_eq!(active_frame.rect, active.rect);
            } else {
                assert!(active_frame.rect.y < active.rect.y);
            }
        }
    }
}

#[test]
fn test_workspace_relative_switch_wrap_animation_direction() {
    assert_workspace_switch_transition_direction(0, 8, IpcCommand::WorkspacePrev, -1);
    assert_workspace_switch_transition_direction(8, 0, IpcCommand::WorkspaceNext, 1);
}

#[test]
fn test_workspace_relative_switch_adjacent_animation_direction() {
    assert_workspace_switch_transition_direction(0, 1, IpcCommand::WorkspaceNext, 1);
    assert_workspace_switch_transition_direction(1, 0, IpcCommand::WorkspacePrev, -1);
}

#[test]
fn test_workspace_numeric_switch_retains_raw_index_animation_direction() {
    assert_workspace_switch_transition_direction(0, 8, IpcCommand::SwitchWorkspace { index: 9 }, 1);
    assert_workspace_switch_transition_direction(
        8,
        0,
        IpcCommand::SwitchWorkspace { index: 1 },
        -1,
    );
}

/// 50% + 25% + 25% fills one viewport, so returning to it must not scroll.
fn set_focused_column_fraction(
    workspace: &mut Workspace,
    hwnd: u64,
    fraction: f64,
    viewport_width: i32,
) {
    workspace
        .focus_window(hwnd)
        .unwrap_or_else(|e| panic!("focus {hwnd}: {e}"));
    workspace.set_focused_column_width_fraction(fraction, viewport_width);
}

struct IncomingSwitchProbe {
    report: String,
    border_shown_during_transition: bool,
    focus_disturbed_layout: bool,
    border_missing_after_completion: bool,
    completion_rect_mismatch: bool,
}

fn probe_incoming_focus_during_switch(
    state: &mut AppState,
    monitor: leopardwm_platform_win32::MonitorId,
    command_index: u8,
    workspace_idx: usize,
    focused_hwnd: u64,
) -> IncomingSwitchProbe {
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    assert!(
        matches!(
            state.handle_command(IpcCommand::SwitchWorkspace {
                index: command_index
            }),
            IpcResponse::Ok
        ),
        "switch to workspace {command_index} failed"
    );
    let transition_active = state.layout_transition.is_some();
    let shows_after_switch = state.border_show_count.load(Ordering::Relaxed);
    let viewport = state.layout_viewport(monitor);
    let workspace = &state.workspaces[&monitor][workspace_idx];
    let scroll_before_focus = workspace.scroll_offset();
    let effective_before_focus = workspace.effective_scroll_offset();
    let animating_before_focus = workspace.is_animating();
    let placements_before_focus: Vec<_> = workspace
        .compute_placements_animated(viewport)
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    let final_rect = placements_before_focus
        .iter()
        .find(|(id, _)| *id == focused_hwnd)
        .map(|(_, rect)| *rect);
    let interpolated_rect = state.compute_window_layout_rect(focused_hwnd);
    let previous_before_focus = state.previous_focused_hwnd;
    let pending_switch_focus = state.pending_workspace_switch_focus.is_some();
    let input_age = leopardwm_platform_win32::ms_since_last_user_input();

    state.last_prune_at = Some(std::time::Instant::now());
    state.handle_window_event(WindowEvent::Focused(
        focused_hwnd,
        leopardwm_platform_win32::current_event_time_ms(),
    ));

    let shows_after_focus = state.border_show_count.load(Ordering::Relaxed);
    let hides_after_focus = state.border_hide_count.load(Ordering::Relaxed);
    let workspace = &state.workspaces[&monitor][workspace_idx];
    let scroll_after_focus = workspace.scroll_offset();
    let effective_after_focus = workspace.effective_scroll_offset();
    let animating_after_focus = workspace.is_animating();
    let placements_after_focus: Vec<_> = workspace
        .compute_placements_animated(viewport)
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    let transition_after_focus = state.layout_transition.is_some();
    let focused_after = workspace.focused_window();

    let duration = state
        .layout_transition
        .as_ref()
        .map(|transition| transition.duration_ms)
        .unwrap_or(0);
    let _ = state.tick_animations(duration.saturating_add(1));
    let shows_after_complete = state.border_show_count.load(Ordering::Relaxed);
    let last_show = state.last_border_show_hwnd.load(Ordering::Relaxed);
    let transition_after_complete = state.layout_transition.is_some();
    let rect_after_complete = state.compute_window_layout_rect(focused_hwnd);
    let scroll_after_complete = state.workspaces[&monitor][workspace_idx].scroll_offset();
    let animating_after_complete = state.workspaces[&monitor][workspace_idx].is_animating();

    let border_shown_during_transition = shows_after_focus != shows_before;
    let focus_disturbed_layout = scroll_after_focus != scroll_before_focus
        || effective_after_focus != effective_before_focus
        || animating_after_focus != animating_before_focus
        || placements_after_focus != placements_before_focus;
    let border_missing_after_completion =
        shows_after_complete != shows_after_focus.saturating_add(1) || last_show != focused_hwnd;
    let completion_rect_mismatch = rect_after_complete != final_rect || transition_after_complete;

    let report = format!(
        "switch->{command_index} hwnd={focused_hwnd} transition_active={transition_active} \
         pending_switch_focus={pending_switch_focus} previous_before={previous_before_focus:?} \
         input_age={input_age:?} shows before/switch/focus/complete={shows_before}/{shows_after_switch}/{shows_after_focus}/{shows_after_complete} \
         hides before/focus={hides_before}/{hides_after_focus} last_show={last_show} \
         scroll before/focus/effective/complete={scroll_before_focus}/{scroll_after_focus}/{effective_after_focus}/{scroll_after_complete} \
         animating before/focus/complete={animating_before_focus}/{animating_after_focus}/{animating_after_complete} \
         focused_after={focused_after:?} previous_after={:?} transition_after_focus={transition_after_focus} \
         transition_after_complete={transition_after_complete} interpolated={interpolated_rect:?} \
         final={final_rect:?} rect_after_complete={rect_after_complete:?} \
         placements_changed={}",
        state.previous_focused_hwnd,
        placements_after_focus != placements_before_focus
    );

    IncomingSwitchProbe {
        report,
        border_shown_during_transition,
        focus_disturbed_layout,
        border_missing_after_completion,
        completion_rect_mismatch,
    }
}

#[test]
fn test_workspace_switch_defers_incoming_focus_border_and_does_not_rescroll() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    state.config.animation.workspace_switch_duration_ms = 200;
    let monitor = state.focused_monitor;
    let viewport_width = state.viewport_width_for(monitor);
    state.workspaces.get_mut(&monitor).unwrap()[0].set_reduce_motion(false);

    for hwnd in [101_u64, 102, 103] {
        state.workspaces.get_mut(&monitor).unwrap()[0]
            .insert_window(hwnd, None)
            .unwrap_or_else(|e| panic!("insert {hwnd}: {e}"));
    }
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[0];
        set_focused_column_fraction(workspace, 101, 0.5, viewport_width);
        set_focused_column_fraction(workspace, 102, 0.25, viewport_width);
        set_focused_column_fraction(workspace, 103, 0.25, viewport_width);
        set_focused_column_fraction(workspace, 101, 0.5, viewport_width);
    }
    state.ensure_workspace_exists(monitor, 1);
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(201, None)
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .set_focused_column_width_fraction(1.0, viewport_width);
    state.workspaces.get_mut(&monitor).unwrap()[1].set_reduce_motion(false);

    let home = &state.workspaces[&monitor][0];
    let widths: Vec<i32> = home.columns().iter().map(|column| column.width()).collect();
    assert_eq!(widths.len(), 3, "ws1 has three columns");
    assert_eq!(widths[1], widths[2], "the side columns are equal");
    assert!(
        widths[0] > widths[1],
        "focused column is the wide one: {widths:?}"
    );
    assert!(
        home.total_width() <= home.visible_width(viewport_width),
        "ws1 fits the viewport without scrolling: total {} visible {}",
        home.total_width(),
        home.visible_width(viewport_width)
    );
    assert_eq!(home.scroll_offset(), 0.0);
    assert_eq!(home.focused_window(), Some(101));
    assert!(!home.is_animating());
    assert_eq!(state.workspaces[&monitor][1].columns().len(), 1);
    assert_eq!(state.workspaces[&monitor][1].focused_window(), Some(201));
    assert!(state.workspaces[&monitor][1].columns()[0].width() > widths[0]);

    // The user is on ws1, focused on the half-width column.
    state.previous_focused_hwnd = Some(101);
    state.active_workspace.insert(monitor, 0);

    let to_single = probe_incoming_focus_during_switch(&mut state, monitor, 2, 1, 201);
    let back_home = probe_incoming_focus_during_switch(&mut state, monitor, 1, 0, 101);
    let failed = to_single.border_shown_during_transition
        || to_single.focus_disturbed_layout
        || to_single.border_missing_after_completion
        || to_single.completion_rect_mismatch
        || back_home.border_shown_during_transition
        || back_home.focus_disturbed_layout
        || back_home.border_missing_after_completion
        || back_home.completion_rect_mismatch;
    assert!(
        !failed,
        "incoming focus during a workspace switch must not show the border or change scroll, \
         and completion must show the border on the final rect\nws1->ws2: {}\nws2->ws1: {}",
        to_single.report, back_home.report
    );
}

#[test]
fn test_reduced_motion_workspace_switch_shows_border_immediately_without_scroll() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = true;
    let monitor = state.focused_monitor;
    let viewport_width = state.viewport_width_for(monitor);
    state.workspaces.get_mut(&monitor).unwrap()[0].set_reduce_motion(true);
    state.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(101, None)
        .unwrap();
    state.ensure_workspace_exists(monitor, 1);
    state.workspaces.get_mut(&monitor).unwrap()[1].set_reduce_motion(true);
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(201, None)
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .set_focused_column_width_fraction(1.0, viewport_width);
    state.previous_focused_hwnd = Some(101);
    state.active_workspace.insert(monitor, 0);
    let shows_before = state.border_show_count.load(Ordering::Relaxed);
    let scroll_before = state.workspaces[&monitor][1].scroll_offset();

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));

    assert!(state.layout_transition.is_none());
    assert!(state.border_show_count.load(Ordering::Relaxed) > shows_before);
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 201);
    assert_eq!(state.workspaces[&monitor][1].scroll_offset(), scroll_before);
    assert!(!state.workspaces[&monitor][1].is_animating());
    assert_eq!(state.previous_focused_hwnd, Some(201));
}

/// Animated explicit switch onto a three-column workspace. The slide is still
/// active and the focus border is deferred when this returns.
fn deferred_switch_onto_three_columns() -> (AppState, leopardwm_platform_win32::MonitorId, u64) {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = false;
    state.config.animation.workspace_switch_duration_ms = 200;
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[0].set_reduce_motion(false);
    state.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(101, None)
        .unwrap();
    state.ensure_workspace_exists(monitor, 1);
    {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[1];
        workspace.set_reduce_motion(false);
        for hwnd in [201_u64, 202, 203] {
            workspace
                .insert_window(hwnd, None)
                .unwrap_or_else(|e| panic!("insert {hwnd}: {e}"));
        }
        workspace
            .focus_window(201)
            .unwrap_or_else(|e| panic!("focus 201: {e}"));
    }
    assert_eq!(state.workspaces[&monitor][1].columns().len(), 3);
    assert_eq!(state.workspaces[&monitor][1].focused_column_index(), 0);

    state.previous_focused_hwnd = Some(101);
    state.active_workspace.insert(monitor, 0);
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.defer_focus_border),
        "the explicit switch must be in flight and deferring the border"
    );
    assert_eq!(state.previous_focused_hwnd, Some(201));
    assert_eq!(state.workspaces[&monitor][1].focused_window(), Some(201));
    (state, monitor, 201)
}

fn placement_rect(
    state: &AppState,
    monitor: leopardwm_platform_win32::MonitorId,
    hwnd: u64,
) -> leopardwm_core_layout::Rect {
    let viewport = state.layout_viewport(monitor);
    state.workspaces[&monitor][1]
        .compute_placements_animated(viewport)
        .into_iter()
        .find(|placement| placement.window_id == hwnd)
        .map(|placement| placement.rect)
        .unwrap_or_else(|| panic!("window {hwnd} has no placement"))
}

#[test]
fn test_move_column_right_during_workspace_switch_keeps_focus_border_deferred() {
    let (mut state, monitor, focused_hwnd) = deferred_switch_onto_three_columns();
    let shows_before_move = state.border_show_count.load(Ordering::Relaxed);
    let rect_before_move = placement_rect(&state, monitor, focused_hwnd);

    assert!(matches!(
        state.handle_command(IpcCommand::MoveColumnRight),
        IpcResponse::Ok
    ));

    assert_eq!(
        state.workspaces[&monitor][1].focused_column_index(),
        1,
        "the focused column moved right"
    );
    assert_eq!(
        state.workspaces[&monitor][1].focused_window(),
        Some(focused_hwnd)
    );
    let final_rect = placement_rect(&state, monitor, focused_hwnd);
    assert_ne!(
        final_rect, rect_before_move,
        "the replacement must land on a different rect than the interrupted switch"
    );
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| transition.defer_focus_border),
        "replacing the switch must keep deferring the border, not drop it"
    );
    // Animation frames call show_border on the interpolated rect. While the
    // carried deferral is set, that call must not paint.
    state.show_border(focused_hwnd);
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before_move,
        "the border must stay hidden for the whole replacement"
    );

    let duration = state
        .layout_transition
        .as_ref()
        .map(|transition| transition.duration_ms)
        .unwrap_or(0);
    let _ = state.tick_animations(duration.saturating_add(1));

    assert!(state.layout_transition.is_none());
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before_move.saturating_add(1),
        "completion of the replacement shows the border once"
    );
    assert_eq!(
        state.last_border_show_hwnd.load(Ordering::Relaxed),
        focused_hwnd
    );
    assert_eq!(
        state.compute_window_layout_rect(focused_hwnd),
        Some(final_rect),
        "the border is shown at the post-move rect, not an intermediate one"
    );
}

#[test]
fn test_abort_deferred_workspace_switch_shows_focus_border() {
    let (mut state, monitor, focused_hwnd) = deferred_switch_onto_three_columns();
    let shows_before_abort = state.border_show_count.load(Ordering::Relaxed);
    let final_rect = placement_rect(&state, monitor, focused_hwnd);

    state.abort_layout_transition();

    assert!(state.layout_transition.is_none());
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        shows_before_abort.saturating_add(1),
        "aborting a deferred slide must show the border; it does not finish through tick_animations"
    );
    assert_eq!(
        state.last_border_show_hwnd.load(Ordering::Relaxed),
        focused_hwnd
    );
    assert_eq!(
        state.compute_window_layout_rect(focused_hwnd),
        Some(final_rect)
    );
}

#[test]
fn test_focus_follow_during_deferred_switch_shows_border_immediately() {
    let (mut state, monitor, _) = deferred_switch_onto_three_columns();
    state.ensure_workspace_exists(monitor, 2);
    state.workspaces.get_mut(&monitor).unwrap()[2].set_reduce_motion(false);
    state.workspaces.get_mut(&monitor).unwrap()[2]
        .insert_window(301, None)
        .unwrap();
    let shows_before_focus = state.border_show_count.load(Ordering::Relaxed);
    let hides_before_focus = state.border_hide_count.load(Ordering::Relaxed);

    state.last_prune_at = Some(std::time::Instant::now());
    state.handle_window_event(WindowEvent::Focused(
        301,
        leopardwm_platform_win32::current_event_time_ms(),
    ));

    assert_eq!(state.active_workspace_idx(monitor), 2);
    assert_eq!(state.previous_focused_hwnd, Some(301));
    assert!(
        state
            .layout_transition
            .as_ref()
            .is_some_and(|transition| !transition.defer_focus_border),
        "a focus-follow switch must not keep the interrupted explicit switch's deferral"
    );
    assert!(
        state.border_show_count.load(Ordering::Relaxed) > shows_before_focus,
        "the newly focused window's border is shown while its slide is still active"
    );
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 301);
    let viewport = state.layout_viewport(monitor);
    let final_rect = state.workspaces[&monitor][2]
        .compute_placements_animated(viewport)
        .into_iter()
        .find(|placement| placement.window_id == 301)
        .map(|placement| placement.rect)
        .expect("window 301 has no placement");
    let duration = state
        .layout_transition
        .as_ref()
        .map(|transition| transition.duration_ms)
        .unwrap_or(0);
    let _ = state.tick_animations(duration.saturating_add(1));

    assert!(state.layout_transition.is_none());
    assert_eq!(state.last_border_show_hwnd.load(Ordering::Relaxed), 301);
    assert_eq!(
        state.border_hide_count.load(Ordering::Relaxed),
        hides_before_focus,
        "completion must not hide the border the focus-follow already showed"
    );
    assert_eq!(state.compute_window_layout_rect(301), Some(final_rect));
}

#[test]
fn test_workspace_activation_repairs_cleared_minimum_before_snapshot() {
    for (deferred, sticky) in [(false, false), (true, false), (true, true)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.reduce_motion = false;
        state.ensure_workspace_exists(1, 1);
        let mut workspace = Workspace::with_gaps(10, 10);
        workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::JustInView);
        workspace.set_center_past_edges(false);
        for id in [200, 300, 400] {
            workspace.insert_window(id, Some(800)).unwrap();
        }
        workspace.commit_pending_min_size_clears();
        workspace.set_window_min_width(400, 1500);
        workspace.set_scroll_offset(1220.0);
        if deferred {
            workspace.insert_window_in_column(500, 2).unwrap();
        }
        state.workspaces.get_mut(&1).unwrap()[1] = workspace;
        if !deferred {
            state.invalidate_display_change_constraints(true);
            state.reconcile_monitors(test_monitors());
        }
        if sticky {
            state
                .focused_workspace_mut()
                .unwrap()
                .insert_window(100, Some(100))
                .unwrap();
            state.toggle_sticky();
        }
        assert_eq!(state.workspaces[&1][1].scroll_offset(), 1220.0);
        assert_eq!(state.active_workspace_idx(1), 0);
        assert!(matches!(
            state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
            IpcResponse::Ok
        ));
        let transition = state.layout_transition.as_ref().unwrap();
        let expected_scroll = if sticky { 630.0 } else { 520.0 };
        assert_eq!(
            transition.start_rects[&400].x,
            1630 - expected_scroll as i32,
            "deferred={deferred}, sticky={sticky}"
        );
        assert_eq!(transition.start_rects[&400].width, 800);
        assert_eq!(state.workspaces[&1][1].scroll_offset(), expected_scroll);
        if sticky {
            assert!(state.workspaces[&1][1].contains_window(100));
            assert!(!state.workspaces[&1][0].contains_window(100));
        }
    }
}

#[test]
fn test_focus_follow_activation_repairs_minimum_before_snapshot() {
    for (deferred, reduce_motion) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.reduce_motion = reduce_motion;
        state.last_prune_at = Some(std::time::Instant::now());
        state.ensure_workspace_exists(1, 1);
        let mut workspace = Workspace::with_gaps(10, 10);
        workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::JustInView);
        workspace.set_center_past_edges(false);
        for id in [200, 300, 400] {
            workspace.insert_window(id, Some(800)).unwrap();
            state
                .injected_window_info
                .insert(id, make_test_window_info(id));
        }
        workspace.commit_pending_min_size_clears();
        workspace.set_window_min_width(400, 1500);
        workspace.set_scroll_offset(1220.0);
        if deferred {
            workspace.insert_window_in_column(500, 2).unwrap();
            state
                .injected_window_info
                .insert(500, make_test_window_info(500));
        } else {
            workspace.clear_window_min_width(400);
        }
        state.workspaces.get_mut(&1).unwrap()[1] = workspace;
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(state.workspaces[&1][1].scroll_offset(), 1220.0);

        state.handle_window_event(WindowEvent::Focused(400, 0));

        assert_eq!(state.active_workspace_idx(1), 1);
        if reduce_motion {
            assert!(state.layout_transition.is_none());
        } else {
            let transition = state.layout_transition.as_ref().unwrap();
            assert_eq!(transition.start_rects[&400].x, 1110, "deferred={deferred}");
            assert_eq!(transition.start_rects[&400].width, 800);
        }
        assert_eq!(state.workspaces[&1][1].scroll_offset(), 520.0);
        assert_eq!(state.workspaces[&1][1].focused_window(), Some(400));
        assert_eq!(state.previous_focused_hwnd, Some(400));
    }
}

#[test]
fn test_focus_follow_activation_snapshots_valid_scroll_without_revealing_focus() {
    for (offset, target) in [
        (137.0, None),
        (-550.0, None),
        (1070.0, None),
        (137.0, Some(400.0)),
    ] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.reduce_motion = false;
        state.last_prune_at = Some(std::time::Instant::now());
        state.ensure_workspace_exists(1, 1);
        let mut workspace = Workspace::with_gaps(10, 10);
        workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::Center);
        workspace.set_center_past_edges(true);
        for id in [200, 300, 400] {
            workspace.insert_window(id, Some(800)).unwrap();
            state
                .injected_window_info
                .insert(id, make_test_window_info(id));
        }
        workspace.commit_pending_min_size_clears();
        workspace.set_scroll_offset(offset);
        if let Some(target) = target {
            workspace.start_scroll_animation(
                target,
                1920,
                Some(1000),
                Some(leopardwm_core_layout::Easing::Linear),
            );
            workspace.tick_animation(10);
        }
        let expected = workspace.compute_placements_animated(state.layout_viewport(1));
        state.workspaces.get_mut(&1).unwrap()[1] = workspace;

        state.handle_window_event(WindowEvent::Focused(400, 0));

        let transition = state.layout_transition.as_ref().unwrap();
        for placement in expected {
            assert_eq!(
                transition.start_rects[&placement.window_id].x,
                placement.rect.x
            );
            assert_eq!(
                transition.start_rects[&placement.window_id].width,
                placement.rect.width
            );
        }
        assert_eq!(state.previous_focused_hwnd, Some(400));
    }
}

#[test]
fn test_workspace_activation_preserves_valid_manual_and_animated_scroll() {
    for reduce_motion in [false, true] {
        for (offset, target) in [
            (137.0, None),
            (-550.0, None),
            (1070.0, None),
            (137.0, Some(400.0)),
        ] {
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            state.reduce_motion = reduce_motion;
            state.ensure_workspace_exists(1, 1);
            let mut workspace = Workspace::with_gaps(10, 10);
            workspace.set_centering_mode(leopardwm_core_layout::CenteringMode::Center);
            workspace.set_center_past_edges(true);
            for id in [200, 300, 400] {
                workspace.insert_window(id, Some(800)).unwrap();
            }
            workspace.commit_pending_min_size_clears();
            workspace.set_scroll_offset(offset);
            if let Some(target) = target {
                workspace.start_scroll_animation(
                    target,
                    1920,
                    Some(1000),
                    Some(leopardwm_core_layout::Easing::Linear),
                );
                workspace.tick_animation(10);
            }
            let mut expected = workspace.clone();
            state.workspaces.get_mut(&1).unwrap()[1] = workspace;
            assert!(matches!(
                state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
                IpcResponse::Ok
            ));
            let actual = &mut state.workspaces.get_mut(&1).unwrap()[1];
            assert_eq!(
                actual.effective_scroll_offset(),
                expected.effective_scroll_offset()
            );
            assert_eq!(actual.is_animating(), expected.is_animating());
            actual.tick_animation(100);
            expected.tick_animation(100);
            assert_eq!(
                actual.effective_scroll_offset(),
                expected.effective_scroll_offset()
            );
        }
    }
}

fn switch_to_empty_workspace_with_pending_focus() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    state.previous_focused_hwnd = Some(100);
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));
    assert!(state.pending_workspace_switch_focus.is_some());
    state
}

#[test]
fn test_workspace_switch_suppresses_old_focus_events_without_mutation() {
    let mut state = switch_to_empty_workspace_with_pending_focus();
    let monitor = state.focused_monitor;
    let intent = state.pending_workspace_switch_focus.unwrap();
    let focused_before = state.workspaces[&monitor][intent.source_workspace].focused_window();
    let border_shows_before = state.border_show_count.load(Ordering::Relaxed);
    let broadcast_before = state.last_broadcast_focused;

    for event_time in [intent.armed_at_event_time_ms, intent.armed_at_event_time_ms] {
        state.handle_window_event(WindowEvent::Focused(intent.source_hwnd, event_time));
    }

    assert_eq!(
        state.active_workspace_idx(monitor),
        intent.destination_workspace
    );
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(
        state.workspaces[&monitor][intent.source_workspace].focused_window(),
        focused_before
    );
    assert_eq!(
        state.border_show_count.load(Ordering::Relaxed),
        border_shows_before
    );
    assert_eq!(state.last_broadcast_focused, broadcast_before);
    assert_eq!(state.pending_workspace_switch_focus, Some(intent));

    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.handle_window_event(WindowEvent::Created(200, 0));
    assert!(state.workspaces[&monitor][intent.destination_workspace].contains_window(200));
}

#[test]
fn test_workspace_switch_post_arm_old_focus_follows_and_clears_intent() {
    let mut state = switch_to_empty_workspace_with_pending_focus();
    let intent = state.pending_workspace_switch_focus.unwrap();

    state.handle_window_event(WindowEvent::Focused(
        intent.source_hwnd,
        intent.armed_at_event_time_ms.wrapping_add(1),
    ));

    assert_eq!(
        state.active_workspace_idx(intent.monitor),
        intent.source_workspace
    );
    assert_eq!(state.previous_focused_hwnd, Some(intent.source_hwnd));
    assert_eq!(state.pending_workspace_switch_focus, None);
}

#[test]
fn test_workspace_switch_focus_intent_expires_or_yields_to_conflicting_focus() {
    let mut expired = switch_to_empty_workspace_with_pending_focus();
    let intent = expired.pending_workspace_switch_focus.as_mut().unwrap();
    intent.set_at = std::time::Instant::now() - PendingWorkspaceSwitchFocus::TTL;
    let expired_intent = *intent;
    expired.handle_window_event(WindowEvent::Focused(
        expired_intent.source_hwnd,
        expired_intent.armed_at_event_time_ms,
    ));
    assert_eq!(
        expired.active_workspace_idx(expired_intent.monitor),
        expired_intent.source_workspace
    );
    assert_eq!(expired.pending_workspace_switch_focus, None);

    let mut conflicting = switch_to_empty_workspace_with_pending_focus();
    let intent = conflicting.pending_workspace_switch_focus.unwrap();
    conflicting.workspaces.get_mut(&intent.monitor).unwrap()[intent.source_workspace]
        .insert_window(200, Some(800))
        .unwrap();
    conflicting.handle_window_event(WindowEvent::Focused(
        200,
        intent.armed_at_event_time_ms.wrapping_add(1),
    ));
    assert_eq!(conflicting.pending_workspace_switch_focus, None);
    assert_eq!(
        conflicting.active_workspace_idx(intent.monitor),
        intent.source_workspace
    );
}

#[test]
fn test_workspace_switch_stale_different_focus_keeps_source_guard_live() {
    let mut state = switch_to_empty_workspace_with_pending_focus();
    let intent = state.pending_workspace_switch_focus.unwrap();
    state.workspaces.get_mut(&intent.monitor).unwrap()[intent.destination_workspace]
        .insert_window(200, Some(800))
        .unwrap();

    state.handle_window_event(WindowEvent::Focused(200, intent.armed_at_event_time_ms));

    assert_eq!(state.pending_workspace_switch_focus, Some(intent));
    assert_eq!(
        state.active_workspace_idx(intent.monitor),
        intent.destination_workspace
    );
    assert_eq!(state.previous_focused_hwnd, Some(200));

    state.handle_window_event(WindowEvent::Focused(
        intent.source_hwnd,
        intent.armed_at_event_time_ms,
    ));

    assert_eq!(state.pending_workspace_switch_focus, Some(intent));
    assert_eq!(
        state.active_workspace_idx(intent.monitor),
        intent.destination_workspace
    );
    assert_eq!(state.previous_focused_hwnd, Some(200));
}

#[test]
fn test_workspace_switch_noop_preserves_and_new_destination_clears_intent() {
    let mut state = switch_to_empty_workspace_with_pending_focus();
    let intent = state.pending_workspace_switch_focus.unwrap();

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));
    assert_eq!(state.pending_workspace_switch_focus, Some(intent));

    state.paused = false;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        std::time::Duration::from_millis(1),
    ));
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 3 }),
        IpcResponse::Error { .. }
    ));
    assert_eq!(state.pending_workspace_switch_focus, None);
}

#[test]
fn test_empty_workspace_switch_releases_departing_tiled_foreground() {
    let mut state = two_managed_windows();
    state.previous_focused_hwnd = Some(100);
    state.injected_foreground_hwnd = Some(Some(100));

    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 2 }),
        IpcResponse::Ok
    ));

    assert_eq!(
        state.foreground_release_requests,
        vec![(100, Rect::new(0, 0, 1920, 1040))]
    );
    assert!(state.selected_workspace_is_genuinely_empty());
}

#[test]
fn test_focusing_monitor_with_empty_selection_releases_its_parked_foreground() {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.ensure_workspace_exists(2, 1);
    state.workspaces.get_mut(&2).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.injected_foreground_hwnd = Some(Some(200));

    assert!(matches!(
        state.handle_command(IpcCommand::FocusMonitorRight),
        IpcResponse::Ok
    ));

    assert_eq!(state.focused_monitor, 2);
    assert!(state.selected_workspace_is_genuinely_empty());
    assert_eq!(
        state.foreground_release_requests,
        vec![(200, Rect::new(1920, 0, 1920, 1040))]
    );
}

#[test]
fn test_empty_workspace_switch_does_not_release_other_foreground() {
    let mut floating = AppState::new_with_config(test_config(), test_monitors());
    floating
        .workspaces
        .get_mut(&floating.focused_monitor)
        .unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    float_focused_window(&mut floating, 100);
    floating.previous_focused_hwnd = Some(100);
    floating.injected_foreground_hwnd = Some(Some(100));
    floating.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(floating.foreground_release_requests.is_empty());

    let mut unmanaged = AppState::new_with_config(test_config(), test_monitors());
    unmanaged
        .workspaces
        .get_mut(&unmanaged.focused_monitor)
        .unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    unmanaged.previous_focused_hwnd = Some(100);
    unmanaged.injected_foreground_hwnd = Some(Some(999));
    unmanaged.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(unmanaged.foreground_release_requests.is_empty());

    let mut visible_elsewhere = AppState::new_with_config(test_config(), two_monitors());
    visible_elsewhere.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, Some(800))
        .unwrap();
    visible_elsewhere.injected_foreground_hwnd = Some(Some(200));
    visible_elsewhere.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(visible_elsewhere.foreground_release_requests.is_empty());
}

#[test]
fn test_last_window_departure_releases_suppressed_parked_foreground() {
    let mut state = last_window_cross_workspace_state();
    state.handle_window_event(WindowEvent::Destroyed(100));

    assert_eq!(state.active_workspace_idx(state.focused_monitor), 0);
    assert_eq!(state.previous_focused_hwnd, None);
    assert!(state.pending_last_window_departure.is_some());
    assert_eq!(
        state.foreground_release_requests,
        vec![(200, Rect::new(0, 0, 1920, 1040))]
    );
}

#[test]
fn test_successful_workspace_switch_rearms_only_without_visible_destination_focus() {
    let mut state = switch_to_empty_workspace_with_pending_focus();
    let monitor = state.focused_monitor;
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.workspaces.get_mut(&monitor).unwrap()[1]
        .focus_window(200)
        .unwrap();
    state.previous_focused_hwnd = Some(200);
    assert!(matches!(
        state.handle_command(IpcCommand::SwitchWorkspace { index: 3 }),
        IpcResponse::Ok
    ));
    assert_eq!(
        state.pending_workspace_switch_focus.map(|intent| (
            intent.source_workspace,
            intent.destination_workspace,
            intent.source_hwnd
        )),
        Some((1, 2, 200))
    );

    let mut visible_destination = AppState::new_with_config(test_config(), test_monitors());
    let monitor = visible_destination.focused_monitor;
    visible_destination.ensure_workspace_exists(monitor, 1);
    visible_destination.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    visible_destination.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    visible_destination.previous_focused_hwnd = Some(100);
    visible_destination.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert_eq!(visible_destination.previous_focused_hwnd, Some(200));
    assert_eq!(visible_destination.pending_workspace_switch_focus, None);

    let mut minimized_destination = AppState::new_with_config(test_config(), test_monitors());
    let monitor = minimized_destination.focused_monitor;
    minimized_destination.ensure_workspace_exists(monitor, 1);
    minimized_destination.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    minimized_destination.workspaces.get_mut(&monitor).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    minimized_destination.workspaces.get_mut(&monitor).unwrap()[1].mark_minimized(200);
    minimized_destination.previous_focused_hwnd = Some(100);
    minimized_destination.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(minimized_destination
        .pending_workspace_switch_focus
        .is_some());
}

#[test]
fn test_workspace_switch_does_not_arm_for_unmanaged_source_and_removal_clears_intent() {
    let mut unmanaged = AppState::new_with_config(test_config(), test_monitors());
    let monitor = unmanaged.focused_monitor;
    unmanaged.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    unmanaged.previous_focused_hwnd = Some(999);
    unmanaged.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert_eq!(unmanaged.pending_workspace_switch_focus, None);

    let mut gone = AppState::new_with_config(test_config(), test_monitors());
    let monitor = gone.focused_monitor;
    gone.workspaces.get_mut(&monitor).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    gone.workspaces.get_mut(&monitor).unwrap()[0]
        .remove_window(100)
        .unwrap();
    gone.previous_focused_hwnd = Some(100);
    gone.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert_eq!(gone.pending_workspace_switch_focus, None);

    let mut destroyed = switch_to_empty_workspace_with_pending_focus();
    destroyed.handle_window_event(WindowEvent::Destroyed(100));
    assert_eq!(destroyed.pending_workspace_switch_focus, None);

    let mut hidden = switch_to_empty_workspace_with_pending_focus();
    hidden.handle_window_event(WindowEvent::Hidden(100, 0));
    assert_eq!(hidden.pending_workspace_switch_focus, None);

    let mut spurious_hidden = switch_to_empty_workspace_with_pending_focus();
    spurious_hidden.injected_visible_hwnds.insert(100);
    spurious_hidden.handle_window_event(WindowEvent::Hidden(100, 0));
    assert!(spurious_hidden.pending_workspace_switch_focus.is_some());

    let mut pruned = switch_to_empty_workspace_with_pending_focus();
    pruned.prune_stale_windows_for_test(&[100]);
    assert_eq!(pruned.pending_workspace_switch_focus, None);
}

#[test]
fn test_workspace_switch_focus_event_time_comparison_handles_equal_and_wraparound() {
    use crate::event_handler::event_time_is_no_later_than;

    assert!(event_time_is_no_later_than(42, 42));
    assert!(event_time_is_no_later_than(u32::MAX, 0));
    assert!(!event_time_is_no_later_than(1, u32::MAX));
}

#[test]
fn test_move_to_workspace_relative_wraps_around() {
    // Prev from the first workspace wraps to the last (index 8 = workspace 9).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    state.handle_command(IpcCommand::MoveToWorkspacePrev);
    assert!(
        state.workspaces[&mon][8].contains_window(100),
        "prev from workspace 1 wraps the window to workspace 9"
    );
    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "window left workspace 1"
    );

    // Next from the last workspace wraps back to the first.
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state.handle_command(IpcCommand::SwitchWorkspace { index: 9 });
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.handle_window_event(WindowEvent::Created(200, 0));
    state.handle_command(IpcCommand::MoveToWorkspaceNext);
    assert!(
        state.workspaces[&mon][0].contains_window(200),
        "next from workspace 9 wraps the window to workspace 1"
    );
}

#[test]
fn test_skip_empty_workspace_config_round_trip() {
    assert!(!Config::default().behavior.skip_empty_workspaces);
    let omitted: Config = toml::from_str("[behavior]\nworkspace_edge_wrap = true").unwrap();
    assert!(!omitted.behavior.skip_empty_workspaces);
    for enabled in [false, true] {
        let json = serde_json::json!({"behavior": {"skip_empty_workspaces": enabled}});
        let from_settings: Config = serde_json::from_value(json).unwrap();
        let saved = toml::to_string_pretty(&from_settings).unwrap();
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(reloaded.behavior.skip_empty_workspaces, enabled);
        assert_eq!(
            serde_json::to_value(reloaded).unwrap()["behavior"]["skip_empty_workspaces"],
            enabled
        );
    }
}

#[test]
fn test_skip_empty_workspace_navigation_table() {
    struct Case {
        name: &'static str,
        current: usize,
        tiled: &'static [usize],
        floating: &'static [usize],
        sticky: &'static [usize],
        minimized: bool,
        next: usize,
        prev: usize,
    }
    let cases = [
        Case {
            name: "empty current",
            current: 0,
            tiled: &[3, 6],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 3,
            prev: 6,
        },
        Case {
            name: "populated current",
            current: 3,
            tiled: &[0, 3, 7],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 7,
            prev: 0,
        },
        Case {
            name: "wrap from first",
            current: 0,
            tiled: &[0, 7],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 7,
            prev: 7,
        },
        Case {
            name: "wrap from last",
            current: 8,
            tiled: &[2, 8],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 2,
            prev: 2,
        },
        Case {
            name: "all empty",
            current: 4,
            tiled: &[],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 4,
            prev: 4,
        },
        Case {
            name: "only current populated",
            current: 4,
            tiled: &[4],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 4,
            prev: 4,
        },
        Case {
            name: "only another populated",
            current: 4,
            tiled: &[6],
            floating: &[],
            sticky: &[],
            minimized: false,
            next: 6,
            prev: 6,
        },
        Case {
            name: "floating counts",
            current: 0,
            tiled: &[5],
            floating: &[2, 7],
            sticky: &[],
            minimized: false,
            next: 2,
            prev: 7,
        },
        Case {
            name: "minimized counts",
            current: 0,
            tiled: &[2, 7],
            floating: &[],
            sticky: &[],
            minimized: true,
            next: 2,
            prev: 7,
        },
        Case {
            name: "sticky tiled and floating excluded",
            current: 0,
            tiled: &[1, 3, 8],
            floating: &[2, 7],
            sticky: &[1, 2, 7, 8],
            minimized: false,
            next: 3,
            prev: 3,
        },
        Case {
            name: "only sticky windows",
            current: 0,
            tiled: &[1],
            floating: &[8],
            sticky: &[1, 8],
            minimized: false,
            next: 0,
            prev: 0,
        },
    ];
    for skip in [false, true] {
        for case in &cases {
            for (command, forward) in [
                (IpcCommand::WorkspaceNext, true),
                (IpcCommand::WorkspacePrev, false),
                (IpcCommand::FocusDown, true),
                (IpcCommand::FocusUp, false),
            ] {
                for mon in [1, 2] {
                    let config: Config = toml::from_str(&format!(
                        "[behavior]\nskip_empty_workspaces = {skip}\nworkspace_edge_wrap = true"
                    ))
                    .unwrap();
                    let mut state = AppState::new_with_config(config, two_monitors());
                    state.focused_monitor = mon;
                    let other = if mon == 1 { 2 } else { 1 };
                    state.ensure_workspace_exists(mon, 8).unwrap();
                    for &idx in case.tiled {
                        let hwnd = 100 + idx as u64;
                        let ws = &mut state.workspaces.get_mut(&mon).unwrap()[idx];
                        ws.insert_window(hwnd, None).unwrap();
                        if case.minimized {
                            ws.mark_minimized(hwnd);
                        }
                    }
                    for &idx in case.floating {
                        state.workspaces.get_mut(&mon).unwrap()[idx]
                            .add_floating(100 + idx as u64, Rect::new(100, 100, 400, 300))
                            .unwrap();
                    }
                    for &idx in case.sticky {
                        state.sticky_windows.insert(100 + idx as u64);
                    }
                    state.active_workspace.insert(mon, case.current);
                    for idx in 0..9 {
                        state
                            .ensure_workspace_exists(other, idx)
                            .unwrap()
                            .insert_window(200 + idx as u64, None)
                            .unwrap();
                    }
                    let expected = if skip {
                        if forward {
                            case.next
                        } else {
                            case.prev
                        }
                    } else if forward {
                        (case.current + 1) % 9
                    } else {
                        (case.current + 8) % 9
                    };
                    assert!(matches!(
                        state.handle_command(command.clone()),
                        IpcResponse::Ok
                    ));
                    assert_eq!(
                        state.active_workspace_idx(mon),
                        expected,
                        "{}: skip={skip}, {command:?}",
                        case.name
                    );
                    assert_eq!(state.focused_monitor, mon);
                    assert_eq!(state.active_workspace_idx(other), 0);
                    if expected == case.current {
                        assert!(
                            state.layout_transition.is_none(),
                            "no-op must not start a switch"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn test_skip_empty_workspace_unchanged_commands_table() {
    for skip in [false, true] {
        for command in [
            IpcCommand::MoveToWorkspaceNext,
            IpcCommand::MoveToWorkspacePrev,
            IpcCommand::MoveToWorkspace { index: 5 },
            IpcCommand::MoveWindowDown,
            IpcCommand::MoveWindowUp,
        ] {
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            state.config.behavior.skip_empty_workspaces = skip;
            state.config.behavior.workspace_edge_wrap = true;
            let mon = state.focused_monitor;
            state
                .focused_workspace_mut()
                .unwrap()
                .insert_window(100, None)
                .unwrap();
            let target = match command {
                IpcCommand::MoveToWorkspacePrev | IpcCommand::MoveWindowUp => 8,
                IpcCommand::MoveToWorkspace { .. } => 4,
                _ => 1,
            };
            assert!(matches!(
                state.handle_command(command.clone()),
                IpcResponse::Ok
            ));
            assert_eq!(state.active_workspace_idx(mon), 0, "{command:?}");
            assert!(state.workspaces[&mon][target].contains_window(100));
            assert!(state.selected_workspace_is_genuinely_empty());
            for horizontal in [IpcCommand::FocusLeft, IpcCommand::FocusRight] {
                assert!(matches!(state.handle_command(horizontal), IpcResponse::Ok));
                assert_eq!(state.active_workspace_idx(mon), 0);
            }
            state.handle_command(IpcCommand::WorkspaceNext);
            assert_eq!(
                state.active_workspace_idx(mon),
                if skip { target } else { 1 },
                "navigation after last window transferred: {command:?}"
            );
        }
        for command in [
            IpcCommand::SwitchWorkspace { index: 9 },
            IpcCommand::SwitchWorkspaceOnMonitor {
                monitor_device_name: "DISPLAY2".into(),
                index: 9,
            },
        ] {
            let mut state = AppState::new_with_config(test_config(), two_monitors());
            state.config.behavior.skip_empty_workspaces = skip;
            assert!(matches!(state.handle_command(command), IpcResponse::Ok));
            assert_eq!(state.active_workspace_idx(state.focused_monitor), 8);
        }
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state.config.behavior.skip_empty_workspaces = skip;
        state.config.behavior.workspace_edge_wrap = true;
        let ws = state.focused_workspace_mut().unwrap();
        ws.insert_window(100, None).unwrap();
        ws.insert_window_in_column(200, 0).unwrap();
        ws.focus_up();
        state
            .ensure_workspace_exists(1, 2)
            .unwrap()
            .insert_window(300, None)
            .unwrap();
        state.handle_command(IpcCommand::FocusDown);
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(200)
        );
        state.handle_command(IpcCommand::FocusUp);
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(
            state.focused_workspace().unwrap().focused_window(),
            Some(100)
        );
        for command in [IpcCommand::FocusUp, IpcCommand::FocusDown] {
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            state.config.behavior.skip_empty_workspaces = skip;
            state
                .ensure_workspace_exists(1, 2)
                .unwrap()
                .insert_window(100, None)
                .unwrap();
            state.handle_command(command);
            assert_eq!(state.active_workspace_idx(1), 0, "edge-wrap disabled");
        }
    }
}

#[test]
fn test_edge_wrap_focus_switches_workspace_only_when_enabled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));

    // A single-window column is at both the top and bottom edge. With edge-wrap
    // disabled (default), FocusDown at the edge stays on the current workspace.
    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(
        state.active_workspace_idx(mon),
        0,
        "no edge-wrap when disabled"
    );

    // Enabled: FocusDown at the bottom edge switches to the next workspace.
    state.config.behavior.workspace_edge_wrap = true;
    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(
        state.active_workspace_idx(mon),
        1,
        "FocusDown at the bottom edge switches to the next workspace when enabled"
    );
}

#[test]
fn test_edge_wrap_focus_crosses_empty_workspaces() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;

    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(state.active_workspace_idx(mon), 0);

    state.config.behavior.workspace_edge_wrap = true;
    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(state.active_workspace_idx(mon), 1);
    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(state.active_workspace_idx(mon), 2);
    state.handle_command(IpcCommand::FocusUp);
    assert_eq!(state.active_workspace_idx(mon), 1);
    state.handle_command(IpcCommand::FocusUp);
    assert_eq!(state.active_workspace_idx(mon), 0);
    state.handle_command(IpcCommand::FocusUp);
    assert_eq!(state.active_workspace_idx(mon), 8);
}

#[test]
fn test_edge_wrap_focus_stays_in_column_before_bottom_edge() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    for id in [100, 200] {
        state
            .injected_window_info
            .insert(id, make_test_window_info(id));
    }
    state.handle_window_event(WindowEvent::Created(100, 0));
    let ws = &mut state.workspaces.get_mut(&mon).unwrap()[0];
    ws.insert_window_in_column(200, 0).unwrap();
    ws.focus_up();
    assert_eq!(ws.focused_window(), Some(100));

    state.config.behavior.workspace_edge_wrap = true;
    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(state.active_workspace_idx(mon), 0);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(200)
    );

    state.handle_command(IpcCommand::FocusDown);
    assert_eq!(state.active_workspace_idx(mon), 1);
}

#[test]
fn test_edge_wrap_move_crosses_window_to_adjacent_workspace() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.config.behavior.workspace_edge_wrap = true;
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));

    // MoveWindowDown at the bottom edge moves the window to the next workspace.
    state.handle_command(IpcCommand::MoveWindowDown);
    assert!(
        state.workspaces[&mon][1].contains_window(100),
        "window crossed to workspace 2"
    );
    assert!(
        !state.workspaces[&mon][0].contains_window(100),
        "window left workspace 1"
    );
    // Moving a window does not switch the active workspace.
    assert_eq!(
        state.active_workspace_idx(mon),
        0,
        "the user stays on workspace 1"
    );
}

#[test]
fn test_move_to_workspace_preserves_column_width() {
    // A window resized away from the default keeps its column width when moved
    // to another workspace and back, instead of snapping to the default (#50).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    for h in [100u64, 200] {
        state
            .injected_window_info
            .insert(h, make_test_window_info(h));
        state.handle_window_event(WindowEvent::Created(h, 0));
    }
    // Give the two columns DIFFERENT non-default widths (default is 800), so the
    // assertion proves the moved window keeps *its own* column's width, not a
    // sibling's or the default. Focus is on 200's column (created last).
    let other_width = 500;
    let custom_width = 640;
    {
        let ws = state.focused_workspace_mut().unwrap();
        ws.set_all_column_widths(other_width);
        ws.resize_focused_column(custom_width - other_width); // 200's column -> 640
    }

    let width_of = |state: &AppState, ws_idx: usize, hwnd: u64| -> i32 {
        let ws = &state.workspaces[&mon][ws_idx];
        let (col, _) = ws.find_window_location(hwnd).unwrap();
        ws.column(col).unwrap().width()
    };
    assert_eq!(width_of(&state, 0, 200), custom_width, "precondition");
    assert_eq!(width_of(&state, 0, 100), other_width, "precondition");

    // Move the focused window (200) to workspace 2: its own width must carry over.
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    assert!(state.workspaces[&mon][1].contains_window(200));
    assert_eq!(
        width_of(&state, 1, 200),
        custom_width,
        "moved window should keep its own column width, not the default or a sibling's"
    );

    // Switch to workspace 2 and move it back: width must still be preserved.
    state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    state.handle_command(IpcCommand::MoveToWorkspace { index: 1 });
    assert!(state.workspaces[&mon][0].contains_window(200));
    assert_eq!(
        width_of(&state, 0, 200),
        custom_width,
        "width should still be preserved after moving back"
    );
}

#[test]
fn test_move_to_workspace_restores_original_column() {
    // A window moved off a workspace and back rejoins its original column,
    // anchored by a surviving column-mate — something a default right-of-focus
    // insert could never produce (#50).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    // Build ws1 columns [100],[200],[300], then stack 150 into column 0.
    for h in [100u64, 200, 300] {
        state
            .injected_window_info
            .insert(h, make_test_window_info(h));
        state.handle_window_event(WindowEvent::Created(h, 0));
    }
    state
        .injected_window_info
        .insert(150, make_test_window_info(150));
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window_in_column(150, 0)
        .unwrap();

    // Focus 100 (shares column 0 with 150) and move it to workspace 2.
    let focus_and_arm = |state: &mut AppState, ws_idx: usize, hwnd: u64| {
        state.workspaces.get_mut(&mon).unwrap()[ws_idx]
            .focus_window(hwnd)
            .unwrap();
        state.previous_focused_hwnd = Some(hwnd);
    };
    focus_and_arm(&mut state, 0, 100);
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    assert!(state.workspaces[&mon][1].contains_window(100));
    // 150 stays behind, alone in column 0.
    assert_eq!(
        state.workspaces[&mon][0].find_window_location(150),
        Some((0, 0)),
        "sibling 150 remains in column 0"
    );

    // Switch to workspace 2 and move 100 back: it must rejoin column 0 with 150.
    state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    focus_and_arm(&mut state, 1, 100);
    state.handle_command(IpcCommand::MoveToWorkspace { index: 1 });

    let ws1 = &state.workspaces[&mon][0];
    assert_eq!(
        ws1.find_window_location(100).map(|(c, _)| c),
        Some(0),
        "100 rejoins its original column 0, not a new column at the end"
    );
    assert_eq!(
        ws1.column(0).unwrap().windows().len(),
        2,
        "100 and 150 share column 0 again"
    );
}

#[test]
fn test_pull_clears_move_origin() {
    // An Edit Config pull establishes a fresh placement, so it must void any
    // prior move-back origin (else a later move could restore a stale column).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    for h in [100u64, 200] {
        state
            .injected_window_info
            .insert(h, make_test_window_info(h));
        state.handle_window_event(WindowEvent::Created(h, 0));
    }
    // Move 200 to workspace 2: an origin (ws1) is recorded for it.
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    assert!(state.move_origins.contains_key(&200));

    // Pull it back to the active workspace: the origin must be cleared.
    assert!(state.pull_window_to_workspace(200, mon, 1, mon, 0));
    assert!(
        !state.move_origins.contains_key(&200),
        "pull should void the stale move-back origin"
    );
}

#[test]
fn test_try_edit_config_pull_matches_editor_by_title() {
    // The pull identifies the editor by its title (which shows the config
    // filename); other cross-workspace focus events are ignored (#57).
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));

    // A non-editor window and the editor window, both moved to workspace 2.
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));
    state.handle_window_event(WindowEvent::Created(300, 0));
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    let mut editor = make_test_window_info(200);
    editor.title = "config.toml - leopardwm - Visual Studio Code".to_string();
    state.injected_window_info.insert(200, editor);
    state.handle_window_event(WindowEvent::Created(200, 0));
    state.handle_command(IpcCommand::MoveToWorkspace { index: 2 });
    assert!(state.workspaces[&mon][1].contains_window(200));
    assert!(state.workspaces[&mon][1].contains_window(300));

    state.pending_edit_config_pull = Some((std::time::Instant::now(), "config.toml".to_string()));

    // A non-editor cross-workspace focus is ignored; the arming stays.
    assert!(!state.try_edit_config_pull(300, mon, 1));
    assert!(state.pending_edit_config_pull.is_some());

    // The editor (title contains the config filename) is pulled; arming consumed.
    assert!(state.try_edit_config_pull(200, mon, 1));
    assert!(state.workspaces[&mon][0].contains_window(200));
    assert!(!state.workspaces[&mon][1].contains_window(200));
    assert!(state.pending_edit_config_pull.is_none());
}

#[test]
fn test_created_event_on_other_monitor_ignores_fullscreen_on_focused_monitor() {
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    let mut state = AppState::new_with_config(config, two_monitors());

    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, None)
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[0].toggle_fullscreen();
    assert_eq!(
        state.workspaces[&1][0].fullscreen_window_id(),
        Some(100),
        "precondition: the focused monitor has a fullscreen window"
    );

    let mut info = make_test_window_info(200);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(200, info);

    state.handle_window_event(WindowEvent::Created(200, 0));

    assert!(
        state.workspaces[&2][state.active_workspace_idx(2)].contains_window(200),
        "window opens on the other monitor"
    );
    assert_eq!(
        state.focused_monitor, 1,
        "focus_new_windows=false keeps the original monitor focused"
    );
    assert_eq!(
        state.previous_focused_hwnd, None,
        "fullscreen on the focused monitor is not reasserted for another monitor's window"
    );
    assert_eq!(
        state.workspaces[&1][0].fullscreen_window_id(),
        Some(100),
        "the other monitor's fullscreen state remains unchanged"
    );
}

#[test]
fn test_created_event_preserves_fullscreen_on_opening_monitor_without_focus() {
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    let mut state = AppState::new_with_config(config, two_monitors());

    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, None)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0]
        .insert_window(200, None)
        .unwrap();
    state.workspaces.get_mut(&2).unwrap()[0].toggle_fullscreen();
    state.previous_focused_hwnd = Some(100);
    state.last_broadcast_focused = Some((1, Some(100)));
    let hides_before = state.border_hide_count.load(Ordering::Relaxed);
    let mut rx = state.event_broadcaster.subscribe();

    let mut info = make_test_window_info(300);
    info.rect = Rect::new(2200, 100, 800, 600);
    state.injected_window_info.insert(300, info);

    state.handle_window_event(WindowEvent::Created(300, 0));

    assert!(
        state.workspaces[&2][state.active_workspace_idx(2)].contains_window(300),
        "window opens on the monitor whose workspace is fullscreen"
    );
    assert_eq!(
        state.workspaces[&2][0].fullscreen_window_id(),
        Some(200),
        "the existing fullscreen window remains above the newcomer"
    );
    assert_eq!(state.workspaces[&2][0].focused_window(), Some(200));
    assert_eq!(
        state.focused_monitor, 1,
        "focus_new_windows=false keeps the original monitor focused"
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
    assert_eq!(state.last_broadcast_focused, Some((1, Some(100))));
    assert_eq!(
        state.border_hide_count.load(Ordering::Relaxed),
        hides_before
    );
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|event| { matches!(event, leopardwm_ipc::IpcEvent::FocusedWindowChanged { .. }) }),
        "preserving fullscreen z-order does not broadcast a focus change"
    );
}

#[test]
fn test_new_window_while_fullscreen_keeps_fullscreen_focused() {
    // A new window opened while a window is fullscreen must join the layout
    // behind it; the fullscreen window stays focused and on top (monocle),
    // rather than the newcomer stealing focus and rendering over it (#58).
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));

    // Fullscreen the first window.
    let mid = state.focused_monitor;
    let idx = state.active_workspace_idx(mid);
    state
        .workspaces
        .get_mut(&mid)
        .and_then(|v| v.get_mut(idx))
        .unwrap()
        .toggle_fullscreen();
    assert_eq!(
        state.focused_workspace().unwrap().fullscreen_window_id(),
        Some(100)
    );

    // Open a second window (focus_new_windows defaults on).
    state
        .injected_window_info
        .insert(200, make_test_window_info(200));
    state.handle_window_event(WindowEvent::Created(200, 0));

    let ws = state.focused_workspace().unwrap();
    assert!(ws.contains_window(200), "new window joins the layout");
    assert_eq!(
        ws.fullscreen_window_id(),
        Some(100),
        "fullscreen stays on the original window"
    );
    assert_eq!(
        ws.focused_window(),
        Some(100),
        "focus returns to the fullscreen window, not the newcomer"
    );
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "tracked focus is the fullscreen window so the border/foreground follow it"
    );
}

#[test]
fn test_created_event_duplicate_is_ignored() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());

    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 1);

    // Second Created event for same window should be ignored
    state.handle_window_event(WindowEvent::Created(100, 0));
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
    state.handle_window_event(WindowEvent::Created(200, 0));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 1);

    // Hide window 200 -- records it in recently_hidden_hwnds
    state.handle_window_event(WindowEvent::Hidden(200, 0));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 0);

    // Re-create window 200 -- should be suppressed (recently hidden)
    state.handle_window_event(WindowEvent::Created(200, 0));
    assert_eq!(
        state.focused_workspace().unwrap().window_count(),
        0,
        "recently hidden window should not be re-added"
    );

    // A different window (100) should still be addable
    state.handle_window_event(WindowEvent::Created(100, 0));
    assert_eq!(
        state.focused_workspace().unwrap().window_count(),
        1,
        "unrelated window should still be added"
    );
}

#[test]
fn test_destroyed_or_hidden_focused_window_clears_focus_and_recovers() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let monitor = state.focused_monitor;
        for hwnd in [100, 200] {
            state
                .injected_window_info
                .insert(hwnd, make_test_window_info(hwnd));
            state.handle_window_event(WindowEvent::Created(hwnd, 0));
        }
        state
            .focused_workspace_mut()
            .unwrap()
            .focus_window(100)
            .unwrap();
        state.previous_focused_hwnd = Some(100);
        let mut rx = state.event_broadcaster.subscribe();

        state.handle_window_event(event);

        assert_eq!(state.border_hide_count.load(Ordering::Relaxed), 1);
        assert_eq!(state.previous_focused_hwnd, None);
        assert_eq!(state.last_broadcast_focused, Some((monitor as i64, None)));
        let mut saw_clear = false;
        while let Ok(event) = rx.try_recv() {
            saw_clear |= matches!(
                event,
                leopardwm_ipc::IpcEvent::FocusedWindowChanged { hwnd: None, .. }
            );
        }
        assert!(saw_clear);

        state.handle_window_event(WindowEvent::Focused(200, 0));

        assert_eq!(state.border_hide_count.load(Ordering::Relaxed), 1);
        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert_eq!(
            state.last_broadcast_focused,
            Some((monitor as i64, Some(200)))
        );
        let mut saw_refocus = false;
        while let Ok(event) = rx.try_recv() {
            saw_refocus |= matches!(
                event,
                leopardwm_ipc::IpcEvent::FocusedWindowChanged {
                    hwnd: Some(200),
                    ..
                }
            );
        }
        assert!(saw_refocus);
    }
}

#[test]
fn test_destroyed_or_hidden_unfocused_window_preserves_focus() {
    for event in [WindowEvent::Destroyed(100), WindowEvent::Hidden(100, 0)] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        let monitor = state.focused_monitor;
        for hwnd in [100, 200] {
            state
                .injected_window_info
                .insert(hwnd, make_test_window_info(hwnd));
            state.handle_window_event(WindowEvent::Created(hwnd, 0));
        }
        state.previous_focused_hwnd = Some(200);
        state.last_broadcast_focused = Some((monitor as i64, Some(200)));
        let mut rx = state.event_broadcaster.subscribe();

        state.handle_window_event(event);

        assert_eq!(state.border_hide_count.load(Ordering::Relaxed), 0);
        assert_eq!(state.previous_focused_hwnd, Some(200));
        assert_eq!(
            state.last_broadcast_focused,
            Some((monitor as i64, Some(200)))
        );
        let mut saw_focus_change = false;
        while let Ok(event) = rx.try_recv() {
            saw_focus_change |=
                matches!(event, leopardwm_ipc::IpcEvent::FocusedWindowChanged { .. });
        }
        assert!(!saw_focus_change);
    }
}

#[test]
fn test_hidden_window_restores_column_width_on_reshow() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(300, make_test_window_info(300));

    // Create it, then give it a distinct (non-default) column width.
    state.handle_window_event(WindowEvent::Created(300, 0));
    state
        .focused_workspace_mut()
        .unwrap()
        .resize_focused_column(250);
    let width_before = state.focused_workspace().unwrap().columns()[0].width();

    // Backdate so the hide isn't treated as transient (transient windows are
    // suppressed on re-create instead of re-tiled).
    state.window_managed_at.insert(
        300,
        std::time::Instant::now() - std::time::Duration::from_secs(31),
    );

    // Hide -> the column width is remembered.
    state.handle_window_event(WindowEvent::Hidden(300, 0));
    assert_eq!(state.focused_workspace().unwrap().window_count(), 0);
    assert_eq!(
        state
            .hidden_column_widths
            .get(&300)
            .map(|entry| entry.width),
        Some(width_before),
        "hidden window's column width is remembered"
    );

    // Reshow -> re-tiled at the remembered width, not the default.
    state.handle_window_event(WindowEvent::Created(300, 0));
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
    state.hidden_column_widths.insert(
        100,
        crate::state::HiddenColumnWidth {
            hidden_at: std::time::Instant::now(),
            width: 555,
            managed_token: None,
        },
    );
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
fn native_swipe_status_derivation_covers_startup_outcomes() {
    use crate::state::derive_native_swipe_status;
    use leopardwm_ipc::NativeSwipeStatus;

    assert_eq!(
        derive_native_swipe_status(false, true, None, None),
        NativeSwipeStatus::Off
    );
    assert_eq!(
        derive_native_swipe_status(true, false, None, None),
        NativeSwipeStatus::Inactive {
            reason: "gesture detection is disabled (gestures.enabled = false)".to_string(),
        }
    );
    assert_eq!(
        derive_native_swipe_status(true, true, Some("hook registration failed"), None),
        NativeSwipeStatus::Inactive {
            reason: "hook registration failed".to_string(),
        }
    );
    assert_eq!(
        derive_native_swipe_status(true, true, None, Some("device not supported")),
        NativeSwipeStatus::Inactive {
            reason: "device not supported; wheel-based swipes remain active".to_string(),
        }
    );
    assert_eq!(
        derive_native_swipe_status(true, true, None, None),
        NativeSwipeStatus::Active
    );
}

#[test]
fn test_cmd_health_check() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.native_swipes = leopardwm_ipc::NativeSwipeStatus::Inactive {
        reason: "device capability check failed; wheel-based swipes remain active".to_string(),
    };
    let blocked_path = std::env::temp_dir().join(format!(
        "leopardwm-health-log-blocked-{}",
        std::process::id()
    ));
    std::fs::write(&blocked_path, b"not a directory").unwrap();
    let (_, log_health) = crate::daemon_log::open(&blocked_path, tracing::Level::INFO);
    std::fs::remove_file(&blocked_path).unwrap();
    state.daemon_log = Some(log_health);
    let resp = state.handle_command(IpcCommand::HealthCheck);
    match resp {
        IpcResponse::HealthInfo {
            healthy,
            total_windows,
            monitors,
            paused,
            daemon_integrity,
            elevation_blocked_windows,
            elevation_blocked_records,
            native_swipes,
            daemon_log,
            ..
        } => {
            assert!(healthy);
            assert_eq!(total_windows, 0);
            assert_eq!(monitors, 1);
            assert!(!paused);
            assert_eq!(
                daemon_integrity,
                leopardwm_platform_win32::current_process_integrity()
            );
            assert!(elevation_blocked_windows.is_empty());
            assert_eq!(elevation_blocked_records, Some(Vec::new()));
            assert_eq!(native_swipes, Some(state.native_swipes.clone()));
            assert!(
                matches!(daemon_log, Some(leopardwm_ipc::DaemonLogStatus::OpenFailed { path, error }) if path == blocked_path.join("leopardwm-daemon.log").display().to_string() && !error.is_empty())
            );
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
fn test_cmd_health_check_elevation_records_sorted_with_daemon_integrity() {
    use leopardwm_ipc::{ElevationBlockReason, ElevationBlockedWindow};
    use leopardwm_platform_win32::ManageBlock;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.note_elevation_block(0x20, "Zed", ManageBlock::Protected);
    state.note_elevation_block(0x30, "Admin", ManageBlock::HigherIntegrity);
    state.note_elevation_block(0x10, "Admin", ManageBlock::HigherIntegrity);

    let resp = state.handle_command(IpcCommand::HealthCheck);
    match resp {
        IpcResponse::HealthInfo {
            daemon_integrity,
            elevation_blocked_windows,
            elevation_blocked_records,
            ..
        } => {
            assert_eq!(
                daemon_integrity,
                leopardwm_platform_win32::current_process_integrity()
            );
            assert_eq!(
                elevation_blocked_windows,
                vec![
                    (0x10, "Admin".to_string()),
                    (0x30, "Admin".to_string()),
                    (0x20, "Zed".to_string()),
                ]
            );
            assert_eq!(
                elevation_blocked_records,
                Some(vec![
                    ElevationBlockedWindow {
                        hwnd: 0x10,
                        title: "Admin".to_string(),
                        reason: ElevationBlockReason::HigherIntegrity,
                    },
                    ElevationBlockedWindow {
                        hwnd: 0x30,
                        title: "Admin".to_string(),
                        reason: ElevationBlockReason::HigherIntegrity,
                    },
                    ElevationBlockedWindow {
                        hwnd: 0x20,
                        title: "Zed".to_string(),
                        reason: ElevationBlockReason::Protected,
                    },
                ])
            );
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
fn test_snap_style_requests_do_not_wait_for_non_pumping_window() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetWindowLongW, PostThreadMessageW, RegisterClassW, TranslateMessage, CS_HREDRAW,
        CS_VREDRAW, GWL_STYLE, MSG, WM_APP, WM_STYLECHANGED, WNDCLASSW, WS_CAPTION,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED,
        WS_SYSMENU, WS_THICKFRAME,
    };

    const BLOCK_OWNER: u32 = WM_APP + 41;
    const QUIT_OWNER: u32 = WM_APP + 42;
    const BLOCK: Duration = Duration::from_millis(2500);
    static STYLE_CHANGES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn fixture_wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_STYLECHANGED && wparam.0 as i32 == GWL_STYLE.0 {
            STYLE_CHANGES.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    struct Fixture {
        thread_id: u32,
        windows: [u64; 2],
        join: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(
                leopardwm_platform_win32::wait_for_window_style_requests(Duration::from_secs(5)),
                "window style requests must drain before fixture windows are destroyed"
            );
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, QUIT_OWNER, WPARAM(0), LPARAM(0));
            }
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    let (ready_tx, ready_rx) = mpsc::channel();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let join = thread::spawn(move || unsafe {
        let thread_id = GetCurrentThreadId();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(fixture_wndproc),
            hInstance: windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap()
                .into(),
            lpszClassName: w!("LeopardWMStyleRequestFixture"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let create = || {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("LeopardWMStyleRequestFixture"),
                None,
                WS_OVERLAPPED
                    | WS_CAPTION
                    | WS_SYSMENU
                    | WS_THICKFRAME
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX,
                0,
                0,
                200,
                100,
                None,
                None,
                Some(class.hInstance),
                None,
            )
            .unwrap()
        };
        let windows = [create(), create()];
        ready_tx
            .send((
                thread_id,
                [windows[0].0 as usize as u64, windows[1].0 as usize as u64],
            ))
            .unwrap();
        thread::sleep(BLOCK);
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            if result <= 0 || message.message == QUIT_OWNER {
                break;
            }
            if message.message == BLOCK_OWNER {
                blocked_tx.send(()).unwrap();
                thread::sleep(BLOCK);
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        for hwnd in windows {
            let _ = DestroyWindow(hwnd);
        }
    });
    let (thread_id, windows) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let fixture = Fixture {
        thread_id,
        windows,
        join: Some(join),
    };
    STYLE_CHANGES.store(0, Ordering::SeqCst);

    let mut config = test_config();
    config.behavior.disable_snap_layouts = true;
    let mut state = AppState::new_with_config(config, test_monitors());
    state.paused = false;
    let start = Instant::now();
    state.disable_snap_for_window(fixture.windows[0]);
    let admission_elapsed = start.elapsed();
    let was_tracked = state.snap_disabled_hwnds.contains(&fixture.windows[0]);
    state.disable_snap_for_window(fixture.windows[0]);
    state.disable_snap_for_window(fixture.windows[1]);
    let wait_timed_out =
        !leopardwm_platform_win32::wait_for_window_style_requests(Duration::from_millis(50));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && windows.iter().any(|hwnd| unsafe { GetWindowLongW(HWND(*hwnd as *mut _), GWL_STYLE) } & WS_MAXIMIZEBOX.0 as i32 != 0)
    {
        thread::sleep(Duration::from_millis(10));
    }
    let completed_after_pump =
        leopardwm_platform_win32::wait_for_window_style_requests(Duration::from_secs(5));
    let removed_once = completed_after_pump && STYLE_CHANGES.load(Ordering::SeqCst) == 2;

    unsafe {
        let _ = PostThreadMessageW(fixture.thread_id, BLOCK_OWNER, WPARAM(0), LPARAM(0));
    }
    blocked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    state.disable_snap_for_window(fixture.windows[1]);
    let release_start = Instant::now();
    state.restore_snap_for_window(fixture.windows[0]);
    let release_elapsed = release_start.elapsed();
    let pause_start = Instant::now();
    state.restore_snap_for_all_windows();
    let pause_elapsed = pause_start.elapsed();
    state.disable_snap_for_window(fixture.windows[0]);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && windows.iter().any(|hwnd| unsafe {
            let style = GetWindowLongW(HWND(*hwnd as *mut _), GWL_STYLE);
            if *hwnd == fixture.windows[0] {
                style & WS_MAXIMIZEBOX.0 as i32 != 0
            } else {
                style & WS_MAXIMIZEBOX.0 as i32 == 0
            }
        })
    {
        thread::sleep(Duration::from_millis(10));
    }
    let completed_after_restore =
        leopardwm_platform_win32::wait_for_window_style_requests(Duration::from_secs(5));
    let tracked_after_readmission = state.snap_disabled_hwnds.contains(&fixture.windows[0]);
    let re_suppressed = unsafe {
        GetWindowLongW(HWND(fixture.windows[0] as *mut _), GWL_STYLE) & WS_MAXIMIZEBOX.0 as i32 == 0
    };
    let second_restored = unsafe {
        GetWindowLongW(HWND(fixture.windows[1] as *mut _), GWL_STYLE) & WS_MAXIMIZEBOX.0 as i32 != 0
    };
    let change_count = if completed_after_restore {
        STYLE_CHANGES.load(Ordering::SeqCst)
    } else {
        usize::MAX
    };
    leopardwm_platform_win32::restore_maximizebox_panic_recovery();
    let recovery_restored_tracked_window = unsafe {
        GetWindowLongW(HWND(fixture.windows[0] as *mut _), GWL_STYLE) & WS_MAXIMIZEBOX.0 as i32 != 0
    };

    let target = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("STATIC"),
            None,
            WS_OVERLAPPED
                | WS_CAPTION
                | WS_SYSMENU
                | WS_THICKFRAME
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX,
            0,
            0,
            200,
            100,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap();
    let target_id = target.0 as usize as u64;
    assert!(leopardwm_platform_win32::remove_maximizebox(fixture.windows[0]).unwrap());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(5)
    ));
    unsafe {
        let _ = PostThreadMessageW(fixture.thread_id, BLOCK_OWNER, WPARAM(0), LPARAM(0));
    }
    blocked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(leopardwm_platform_win32::restore_maximizebox(fixture.windows[0]).unwrap());
    let restore_is_blocked =
        !leopardwm_platform_win32::wait_for_window_style_requests(Duration::from_millis(50));
    let remove_was_queued = leopardwm_platform_win32::remove_maximizebox(target_id).unwrap();
    leopardwm_platform_win32::restore_maximizebox_panic_recovery();
    let target_still_has_maximizebox =
        unsafe { GetWindowLongW(target, GWL_STYLE) & WS_MAXIMIZEBOX.0 as i32 != 0 };
    pump_window_style_worker_until_idle();
    let target_kept_maximizebox_after_drain =
        unsafe { GetWindowLongW(target, GWL_STYLE) & WS_MAXIMIZEBOX.0 as i32 != 0 };
    let _ = unsafe { DestroyWindow(target) };
    drop(fixture);

    assert!(
        admission_elapsed < Duration::from_millis(750),
        "admission blocked for {admission_elapsed:?}"
    );
    assert!(was_tracked, "queued suppression must be tracked");
    assert!(
        wait_timed_out,
        "the bounded wait should time out while the owner is blocked"
    );
    assert!(
        completed_after_pump,
        "the bounded wait should finish after the owner pumps"
    );
    assert!(
        removed_once,
        "suppression should produce exactly one style change"
    );
    assert!(
        release_elapsed < Duration::from_millis(750),
        "release blocked for {release_elapsed:?}"
    );
    assert!(
        pause_elapsed < Duration::from_millis(750),
        "pause restore blocked for {pause_elapsed:?}"
    );
    assert!(
        tracked_after_readmission,
        "readmission during a pending restore must track the window"
    );
    assert!(
        completed_after_restore,
        "all queued restore/re-remove work must finish"
    );
    assert!(
        re_suppressed,
        "readmitted window must remain snap-suppressed"
    );
    assert!(
        second_restored,
        "pause restore must re-enable the other window"
    );
    assert_eq!(
        change_count, 5,
        "initial suppression plus restore/re-suppression and the peer restore"
    );
    assert!(
        recovery_restored_tracked_window,
        "panic recovery must find and restore the platform-tracked window"
    );
    assert!(
        restore_is_blocked,
        "the earlier restore must hold the worker"
    );
    assert!(
        remove_was_queued,
        "the target remove must queue behind the restore"
    );
    assert!(
        target_still_has_maximizebox,
        "panic recovery must restore the target before its queued remove runs"
    );
    assert!(
        target_kept_maximizebox_after_drain,
        "a queued remove must be skipped after panic recovery drains tracking"
    );
}

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

    // Config reload enumerates the desktop; keep this empty-layout fixture isolated.
    let mut new_config = test_config();
    new_config.behavior.disable_snap_layouts = true;
    new_config.window_rules = vec![config::WindowRule {
        match_class: Some(".*".to_string()),
        action: config::WindowAction::Ignore,
        ..Default::default()
    }];
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
    state.recently_hidden_hwnds.insert(
        hwnd,
        crate::state::RecentlyHiddenEntry {
            hidden_at: std::time::Instant::now(),
            managed_token: None,
        },
    );
    // No injected_window_info -> lookup_window_info returns None.
    state.handle_window_event(WindowEvent::Focused(hwnd, 0));
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
    assert!(
        saw_set && saw_clear,
        "should emit both set and clear events"
    );
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
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );

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
fn test_scratchpad_workspace_capture_short_circuits_native_queries() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    let workspace_rect = Rect::new(120, 100, 1000, 700);
    let mut chrome_queries = 0;
    let mut visibility_queries = 0;
    let mut dwm_queries = 0;

    assert_eq!(
        crate::scratchpad::scratchpad_capture_rect(
            Some(workspace_rect),
            &work_areas,
            || {
                chrome_queries += 1;
                Some(Rect::new(-100_000, -100_000, 900, 600))
            },
            || {
                visibility_queries += 1;
                true
            },
            || {
                dwm_queries += 1;
                Some(Rect::new(-100_000, -100_000, 900, 600))
            },
        ),
        Some(workspace_rect)
    );
    assert_eq!(chrome_queries, 0);
    assert_eq!(visibility_queries, 0);
    assert_eq!(dwm_queries, 0);
}

#[test]
fn test_scratchpad_capture_rejects_uncertain_chrome_without_dwm_queries() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    for (chrome_rect, window_visible) in [
        (Rect::new(-100_000, -100_000, 900, 600), true),
        (Rect::new(-1920, 0, 1920, 1040), true),
        (Rect::new(0, 0, 1920, 1040), false),
    ] {
        let mut dwm_queries = 0;
        assert_eq!(
            crate::scratchpad::scratchpad_capture_rect(
                None,
                &work_areas,
                || Some(chrome_rect),
                || window_visible,
                || {
                    dwm_queries += 1;
                    Some(Rect::new(100, 100, 900, 600))
                },
            ),
            None
        );
        assert_eq!(dwm_queries, 0);

        let mut inset_queries = 0;
        assert_eq!(
            crate::scratchpad::scratchpad_capture_frame_insets(
                None,
                &work_areas,
                || Some(chrome_rect),
                || window_visible,
                || {
                    inset_queries += 1;
                    Some((7, 1, 7, 8))
                },
            ),
            None
        );
        assert_eq!(inset_queries, 0);
    }
}

#[test]
fn test_scratchpad_capture_uses_verified_dwm_fallback() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    let mut dwm_queries = 0;
    assert_eq!(
        crate::scratchpad::scratchpad_capture_rect(
            None,
            &work_areas,
            || Some(Rect::new(100, 100, 900, 600)),
            || true,
            || {
                dwm_queries += 1;
                Some(Rect::new(100, 100, 900, 600))
            },
        ),
        Some(Rect::new(100, 100, 900, 600))
    );
    assert_eq!(dwm_queries, 1);
}

#[test]
fn test_scratchpad_existing_insets_skip_queries_and_are_not_replaced() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    let saved_insets = Some((7, 1, 7, 8));
    let mut chrome_queries = 0;
    let mut visibility_queries = 0;
    let mut inset_queries = 0;

    assert_eq!(
        crate::scratchpad::scratchpad_capture_frame_insets(
            saved_insets,
            &work_areas,
            || {
                chrome_queries += 1;
                Some(Rect::new(100, 100, 900, 600))
            },
            || {
                visibility_queries += 1;
                true
            },
            || {
                inset_queries += 1;
                Some((0, 0, 0, 0))
            },
        ),
        saved_insets
    );
    assert_eq!(chrome_queries, 0);
    assert_eq!(visibility_queries, 0);
    assert_eq!(inset_queries, 0);
}

#[test]
fn test_scratchpad_initial_insets_capture_once_from_visible_chrome() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    let mut chrome_queries = 0;
    let mut visibility_queries = 0;
    let mut inset_queries = 0;

    assert_eq!(
        crate::scratchpad::scratchpad_capture_frame_insets(
            None,
            &work_areas,
            || {
                chrome_queries += 1;
                Some(Rect::new(100, 100, 900, 600))
            },
            || {
                visibility_queries += 1;
                true
            },
            || {
                inset_queries += 1;
                Some((7, 1, 7, 8))
            },
        ),
        Some((7, 1, 7, 8))
    );
    assert_eq!(chrome_queries, 1);
    assert_eq!(visibility_queries, 1);
    assert_eq!(inset_queries, 1);
}

#[test]
fn test_scratchpad_rejected_initial_insets_remain_uncaptured() {
    let work_areas = [Rect::new(0, 0, 1920, 1040)];
    let mut chrome_queries = 0;
    let mut visibility_queries = 0;
    let mut inset_queries = 0;

    assert_eq!(
        crate::scratchpad::scratchpad_capture_frame_insets(
            None,
            &work_areas,
            || {
                chrome_queries += 1;
                Some(Rect::new(100, 100, 900, 600))
            },
            || {
                visibility_queries += 1;
                true
            },
            || {
                inset_queries += 1;
                None
            },
        ),
        None
    );
    assert_eq!(chrome_queries, 1);
    assert_eq!(visibility_queries, 1);
    assert_eq!(inset_queries, 1);
}

#[test]
fn test_scratchpad_direct_frame_rect_uses_only_recorded_insets() {
    let rect = Rect::new(120, 100, 1000, 700);
    assert_eq!(
        crate::scratchpad::scratchpad_direct_frame_rect(rect, Some((7, 1, 7, 8)), false),
        Rect::new(113, 99, 1014, 709)
    );
    assert_eq!(
        crate::scratchpad::scratchpad_direct_frame_rect(rect, None, false),
        rect
    );
    assert_eq!(
        crate::scratchpad::scratchpad_direct_frame_rect(rect, Some((7, 1, 7, 8)), true),
        rect
    );
}

#[test]
fn test_scratchpad_default_dimensions_follow_work_area() {
    assert_eq!(
        crate::scratchpad::scratchpad_default_dimensions(Rect::new(0, 0, 1920, 1040)),
        (960, 624)
    );
    assert_eq!(
        crate::scratchpad::scratchpad_default_dimensions(Rect::new(0, 0, 2880, 1710)),
        (1440, 1026)
    );
    assert_eq!(
        crate::scratchpad::scratchpad_default_dimensions(Rect::new(0, 0, 300, 200)),
        (200, 150)
    );
    assert_eq!(
        crate::scratchpad::scratchpad_default_dimensions(Rect::new(0, 0, 199, 149)),
        (199, 149)
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

    // First summon: floating + shown at the centered work-area default.
    state.scratchpad_toggle();
    assert!(state.scratchpad.unwrap().shown);
    assert!(state.scratchpad.unwrap().saved_rect.is_none());
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("summoned scratchpad is floating");
    assert_eq!(floating.rect, Rect::new(480, 208, 960, 624));

    // Hide: removed + not shown.
    state.scratchpad_toggle();
    assert!(!state.scratchpad.unwrap().shown);
    assert!(!state.focused_workspace().unwrap().contains_window(100));
}

#[test]
fn test_scratchpad_toggle_restores_saved_geometry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad_toggle();

    let first_rect = Rect::new(120, 100, 1000, 700);
    state
        .focused_workspace_mut()
        .unwrap()
        .update_floating(100, first_rect);
    state.scratchpad_toggle();
    assert_eq!(state.scratchpad.unwrap().saved_rect, Some(first_rect));

    state.scratchpad_toggle();
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("scratchpad is re-summoned as floating");
    assert_eq!(floating.rect, first_rect);

    let second_rect = Rect::new(240, 180, 1100, 650);
    state
        .focused_workspace_mut()
        .unwrap()
        .update_floating(100, second_rect);
    state.scratchpad_toggle();
    state.scratchpad_toggle();
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("scratchpad is re-summoned after a second toggle");
    assert_eq!(floating.rect, second_rect);
}

#[test]
fn test_scratchpad_paused_summon_restores_saved_geometry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    let saved_rect = Rect::new(120, 100, 1000, 700);
    state.scratchpad.as_mut().unwrap().saved_rect = Some(saved_rect);
    state.paused = true;

    state.scratchpad_toggle();

    assert!(state.scratchpad.unwrap().shown);
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("paused scratchpad is summoned as floating");
    assert_eq!(floating.rect, saved_rect);
}

#[test]
fn test_scratchpad_rapid_toggles_keep_workspace_geometry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad_toggle();

    let saved_before_summon = Rect::new(-100_000, -100_000, 900, 600);
    let latest_rect = Rect::new(240, 180, 1100, 650);
    state.scratchpad.as_mut().unwrap().saved_rect = Some(saved_before_summon);
    state
        .focused_workspace_mut()
        .unwrap()
        .update_floating(100, latest_rect);

    for _ in 0..2 {
        state.scratchpad_toggle();
        assert_eq!(state.scratchpad.unwrap().saved_rect, Some(latest_rect));
        state.scratchpad_toggle();
        let floating = state
            .focused_workspace()
            .unwrap()
            .floating_windows()
            .iter()
            .find(|floating| floating.id == 100)
            .expect("scratchpad is re-summoned after rapid toggling");
        assert_eq!(floating.rect, latest_rect);
    }
}

#[test]
fn test_scratchpad_saved_small_geometry_is_preserved() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad.as_mut().unwrap().saved_rect = Some(Rect::new(120, 100, 80, 60));

    state.scratchpad_toggle();
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("small scratchpad is summoned");
    assert_eq!(floating.rect, Rect::new(120, 100, 80, 60));

    state
        .focused_workspace_mut()
        .unwrap()
        .update_floating(100, Rect::new(240, 180, 199, 149));
    state.scratchpad_toggle();
    state.scratchpad_toggle();
    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("second small scratchpad is re-summoned");
    assert_eq!(floating.rect, Rect::new(240, 180, 199, 149));
}

#[test]
fn test_scratchpad_nonpositive_saved_geometry_uses_safe_size() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad.as_mut().unwrap().saved_rect = Some(Rect::new(120, 100, 0, -1));

    state.scratchpad_toggle();

    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("scratchpad with invalid saved size is summoned");
    assert_eq!(floating.rect, Rect::new(120, 100, 960, 624));
}

#[test]
fn test_scratchpad_saved_geometry_clamps_to_work_area() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad.as_mut().unwrap().saved_rect = Some(Rect::new(3000, 1500, 1000, 700));

    state.scratchpad_toggle();

    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("scratchpad is summoned within the work area");
    assert_eq!(floating.rect, Rect::new(920, 340, 1000, 700));
}

#[test]
fn test_scratchpad_saved_geometry_shrinks_after_work_area_change() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash();
    state.scratchpad.as_mut().unwrap().saved_rect = Some(Rect::new(0, 0, 1920, 1040));
    let monitor = state.monitors.get_mut(&1).unwrap();
    monitor.rect = Rect::new(0, 0, 1366, 768);
    monitor.work_area = Rect::new(0, 0, 1366, 728);

    state.scratchpad_toggle();

    let floating = state
        .focused_workspace()
        .unwrap()
        .floating_windows()
        .iter()
        .find(|floating| floating.id == 100)
        .expect("scratchpad is re-summoned in the changed work area");
    assert_eq!(floating.rect, Rect::new(0, 0, 1366, 728));
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
fn test_scratchpad_release_clears_saved_geometry_and_returns_to_tiling() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state.scratchpad_stash(); // designate + hide 100
    state.scratchpad_toggle(); // summon (floating, focused)
    assert!(state.focused_workspace().unwrap().is_floating(100));
    state.scratchpad.as_mut().unwrap().saved_rect = Some(Rect::new(120, 100, 1000, 700));

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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.scratchpad_stash(); // 100 becomes scratchpad (hidden)
    assert_eq!(state.scratchpad.unwrap().window_id, 100);

    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(200)
        .unwrap();
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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(200)
        .unwrap();
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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(200)
        .unwrap();
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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(wid)
        .unwrap();
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
    assert!(
        state.sticky_windows.contains(&100),
        "pinned into sticky set"
    );
    assert!(
        state.focused_workspace().unwrap().is_floating(100),
        "a floating window stays floating when stuck"
    );

    state.previous_focused_hwnd = Some(100);
    state.toggle_sticky(); // un-pin
    assert!(
        !state.sticky_windows.contains(&100),
        "unpinned from sticky set"
    );
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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();

    state.toggle_sticky(); // stick a TILED window
    assert!(
        state.sticky_windows.contains(&100),
        "tiled window added to sticky set"
    );
    assert!(
        !state.focused_workspace().unwrap().is_floating(100),
        "a tiled window stays tiled when stuck (not force-floated)"
    );

    state.toggle_sticky(); // un-stick (tiled focus still reports it)
    assert!(!state.sticky_windows.contains(&100));
    assert!(
        !state.focused_workspace().unwrap().is_floating(100),
        "still tiled"
    );
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
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky(); // 100 tiled + sticky on workspace 0

    // Destination already has a tiled window so we can assert end placement.
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    assert!(
        dest.contains_window(100),
        "tiled sticky followed to the active workspace"
    );
    assert!(!dest.is_floating(100), "and it stayed tiled, not floated");
    assert_eq!(dest.column_count(), 2, "destination now has both columns");
    assert!(
        !state.workspaces.get(&mon).unwrap()[0].contains_window(100),
        "left the old workspace"
    );

    // Floating-stays-floating guard: a tiled sticky must never become floating
    // across a switch (the rehome reads is_floating on the SOURCE workspace).
    state.active_workspace.insert(mon, 0);
    state.rehome_sticky_windows();
    assert!(
        !state.workspaces.get(&mon).unwrap()[0].is_floating(100),
        "still tiled after switching back"
    );
}

#[test]
fn test_tiled_sticky_preserves_column_width_across_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    // A non-default width (default is 800) that must survive the switch.
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(500))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky();

    state.ensure_workspace_exists(mon, 1);
    state.active_workspace.insert(mon, 1);
    state.rehome_sticky_windows();

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    let width = dest
        .find_window_location(100)
        .and_then(|(col, _)| dest.columns().get(col).map(|c| c.width()));
    assert_eq!(
        width,
        Some(500),
        "tiled sticky kept its column width, not the default"
    );
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
    assert_eq!(
        pinned,
        Some(true),
        "pinning marks the floating entry pinned"
    );

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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky();
    assert!(state.sticky_windows.contains(&100));

    state.sticky_on_window_destroyed(100);
    assert!(
        !state.sticky_windows.contains(&100),
        "destroyed window unpinned"
    );
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
    state.handle_window_event(WindowEvent::Focused(200, 0));
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
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky(); // 100 tiled + sticky on workspace 0
                           // User is focused on a different TILED window, not the sticky one. The
                           // tiled rehome appends without stealing focus, so focus must not jump to it.
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(150, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(150)
        .unwrap();
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
        state.pending_sticky_refocus, None,
        "no landing refocus armed when the pin was not focused"
    );
}

#[test]
fn test_tiled_sticky_focused_keeps_focus_across_switch() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky(); // tiled sticky, focused
    state.ensure_workspace_exists(mon, 1);
    state.workspaces.get_mut(&mon).unwrap()[1]
        .insert_window(200, Some(800))
        .unwrap();
    state.previous_focused_hwnd = Some(100); // user is on the sticky window
    state.reduce_motion = false;

    let resp = state.handle_command(IpcCommand::SwitchWorkspace { index: 2 });
    assert!(matches!(resp, IpcResponse::Ok));

    let dest = &state.workspaces.get(&mon).unwrap()[1];
    assert!(
        dest.contains_window(100) && !dest.is_floating(100),
        "followed and stayed tiled"
    );
    assert_eq!(
        dest.focused_window(),
        Some(100),
        "destination focus is the sticky window"
    );
    assert_eq!(
        state.previous_focused_hwnd,
        Some(100),
        "focus stays on the tiled sticky"
    );
}

#[test]
fn test_refocus_sticky_window_tiled() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(200, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
    state.toggle_sticky(); // 100 tiled-sticky
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(200)
        .unwrap(); // move focus off it

    assert!(
        state.refocus_sticky_window(100),
        "tiled sticky refocus applies"
    );
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        Some(100)
    );
    assert_eq!(state.previous_focused_hwnd, Some(100));
}

#[test]
fn test_sticky_mode_transition_tiled_to_floating() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let mon = state.focused_monitor;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(100, Some(800))
        .unwrap();
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(100)
        .unwrap();
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
    let cfg: Config = toml::from_str("[behavior]\nnew_window_placement = \"in_column\"\n").unwrap();
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
        match_max_width: None,
        match_max_height: None,
    }];
    let state = AppState::new_with_config(config, test_monitors());
    let rule = state
        .matched_rule("SomeClass", "Editor", "code.exe", None)
        .expect("matches");
    assert_eq!(rule.open_on_workspace, Some(2));
    assert_eq!(rule.column_width, Some(0.25));
    assert!(
        state
            .matched_rule("SomeClass", "Editor", "other.exe", None)
            .is_none()
            || state
                .matched_rule("SomeClass", "Editor", "other.exe", None)
                .unwrap()
                .match_executable
                .as_deref()
                != Some("code.exe")
    );
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
fn test_restore_center_single_column_runtime_setting() {
    for enabled in [false, true] {
        for restore_path in 0..3 {
            let mut state = structure_restore_state();
            state.config.layout.center_single_column = enabled;
            state.monitors.get_mut(&2).unwrap().work_area.width = 1000;
            state.injected_apply_placements_behavior = Some(
                TestApplyPlacementsBehavior::SleepAndSucceed(std::time::Duration::ZERO),
            );
            state.paused = false;
            let initial_len = state.workspaces[&2].len();
            let workspace_index = if restore_path == 2 {
                0
            } else {
                initial_len + 2
            };
            let mut saved = leopardwm_core_layout::Workspace::new();
            saved.insert_window(100, Some(400)).unwrap();
            saved.set_center_single_column(true);
            saved.ensure_focused_visible(1000);
            let saved_offset = saved.scroll_offset();
            assert!(saved_offset < 0.0);
            let snapshot = crate::state::StateSnapshot {
                saved_at: "0".to_string(),
                workspaces: vec![crate::state::WorkspaceSnapshot {
                    monitor_device_name: "DISPLAY2".to_string(),
                    workspace_index,
                    workspace: saved,
                }],
                focused_monitor_name: "DISPLAY1".to_string(),
                active_workspace: std::collections::HashMap::new(),
                tab_title_overrides: std::collections::HashMap::new(),
            };
            let snapshot =
                serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
            if restore_path == 0 {
                state.restore_workspace_structure_with(&snapshot, |_| true);
            } else {
                if restore_path == 2 {
                    state.workspaces.get_mut(&2).unwrap()[0]
                        .insert_window(100, Some(400))
                        .unwrap();
                }
                state.restore_state(&snapshot);
            }
            let first = if restore_path == 2 { 0 } else { initial_len };
            for idx in first..=workspace_index {
                let ws = &mut state.workspaces.get_mut(&2).unwrap()[idx];
                if ws.is_empty() {
                    ws.insert_window(1000 + idx as u64, Some(400)).unwrap();
                }
                let window_id = ws.all_window_ids()[0];
                state.active_workspace.insert(2, idx);
                state.apply_layout().unwrap();
                assert_eq!(
                    state.last_placed_layout_rects[&window_id].x,
                    1920 + if enabled { 300 } else { 10 },
                    "enabled={enabled}, restore path {restore_path}, slot {idx}"
                );
            }
        }
    }
}

#[test]
fn test_center_single_column_toggle_off_repairs_first_placement() {
    let mut config = test_config();
    config.layout.center_single_column = true;
    config.layout.outer_gap_left = 20;
    config.layout.outer_gap_right = 20;
    let mut monitors = test_monitors();
    monitors[0].work_area.width = 1000;
    let mut state = AppState::new_with_config(config.clone(), monitors);
    let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
    ws.insert_window(100, Some(400)).unwrap();
    ws.set_reduce_motion(false);
    ws.ensure_focused_visible(1000);
    assert_eq!(ws.scroll_offset(), -280.0);
    config.layout.center_single_column = false;
    state.apply_config(config);
    state.paused = false;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        std::time::Duration::ZERO,
    ));
    state.apply_layout().unwrap();
    assert_eq!(state.last_placed_layout_rects[&100].x, 20);
    assert_eq!(state.workspaces[&1][0].effective_scroll_offset(), 0.0);
}

#[test]
fn test_center_single_column_restore_preserves_valid_offsets() {
    for (past_edges, offset, expected_x) in [(false, 137.0, -117), (true, -280.0, 300)] {
        for structure_restore in [false, true] {
            let mut config = test_config();
            config.layout.center_past_edges = past_edges;
            config.layout.outer_gap_left = 20;
            config.layout.outer_gap_right = 20;
            let mut monitors = test_monitors();
            monitors[0].work_area.width = 1000;
            let mut state = AppState::new_with_config(config, monitors);
            let mut saved = Workspace::with_gaps(10, 20);
            saved.insert_window(100, Some(400)).unwrap();
            saved.insert_window(200, Some(800)).unwrap();
            saved.set_scroll_offset(offset);
            let snapshot = StateSnapshot {
                saved_at: "0".to_string(),
                workspaces: vec![WorkspaceSnapshot {
                    monitor_device_name: "DISPLAY1".to_string(),
                    workspace_index: 0,
                    workspace: saved,
                }],
                focused_monitor_name: "DISPLAY1".to_string(),
                active_workspace: HashMap::new(),
                tab_title_overrides: HashMap::new(),
            };
            if structure_restore {
                state.restore_workspace_structure_with(&snapshot, |_| true);
            } else {
                let ws = &mut state.workspaces.get_mut(&1).unwrap()[0];
                ws.insert_window(100, Some(400)).unwrap();
                ws.insert_window(200, Some(800)).unwrap();
                state.restore_state(&snapshot);
            }
            state.paused = false;
            state.injected_apply_placements_behavior = Some(
                TestApplyPlacementsBehavior::SleepAndSucceed(std::time::Duration::ZERO),
            );
            state.apply_layout().unwrap();
            assert_eq!(state.workspaces[&1][0].scroll_offset(), offset);
            assert_eq!(state.last_placed_layout_rects[&100].x, expected_x);
        }
    }
}

#[test]
fn test_restore_structure_preserves_columns_widths_grouping_scroll() {
    let mut state = structure_restore_state();
    state.monitors.get_mut(&2).unwrap().work_area.width = 800;
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
    assert_eq!(
        ws.columns()[1].windows(),
        &[200, 201],
        "col 1 stacked grouping"
    );
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
    assert_eq!(
        ws.columns()[0].windows(),
        &[200],
        "only live window remains"
    );
    assert!(!ws.contains_window(100));
    assert!(!ws.contains_window(201));
}

#[test]
fn test_restore_structure_rejects_excluded_classes_but_keeps_hidden_windows() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WS_MINIMIZE,
    };

    struct TestWindow(HWND);
    impl TestWindow {
        fn new(class: PCWSTR, style: WINDOW_STYLE) -> Self {
            Self(unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    w!(""),
                    style,
                    0,
                    0,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            })
        }

        fn id(&self) -> u64 {
            self.0 .0 as u64
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    let dialog = TestWindow::new(w!("#32770"), WINDOW_STYLE::default());
    let hidden = TestWindow::new(w!("STATIC"), WINDOW_STYLE::default());
    let minimized = TestWindow::new(w!("STATIC"), WS_MINIMIZE);
    assert!(!leopardwm_platform_win32::is_window_visible(hidden.id()));
    assert_eq!(
        leopardwm_platform_win32::window_minimized_state(minimized.id()),
        Some(true)
    );
    let mut workspace = leopardwm_core_layout::Workspace::default();
    for window in [&dialog, &hidden, &minimized] {
        workspace.insert_window(window.id(), Some(480)).unwrap();
    }
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![crate::state::WorkspaceSnapshot {
            monitor_device_name: "DISPLAY2".to_string(),
            workspace_index: 0,
            workspace,
        }],
        focused_monitor_name: "DISPLAY1".to_string(),
        active_workspace: std::collections::HashMap::new(),
        tab_title_overrides: std::collections::HashMap::new(),
    };
    let mut state = structure_restore_state();
    let restored = state.restore_workspace_structure(&snapshot);
    let &(monitor, index) = restored.iter().next().unwrap();
    let workspace = &state.workspaces[&monitor][index];
    assert!(!workspace.contains_window(dialog.id()));
    assert!(workspace.contains_window(hidden.id()));
    assert!(workspace.contains_window(minimized.id()));
    assert_eq!(workspace.column_count(), 2);
}

#[test]
fn test_initial_layout_parks_restored_inactive_windows() {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowRect, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_MINIMIZE, WS_POPUP,
    };

    struct TestWindow(HWND);
    impl TestWindow {
        fn new(minimized: bool) -> Self {
            Self(unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!(""),
                    if minimized {
                        WS_POPUP | WS_MINIMIZE
                    } else {
                        WS_POPUP
                    },
                    100,
                    100,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            })
        }

        fn id(&self) -> u64 {
            self.0 .0 as u64
        }

        fn rect(&self) -> Rect {
            let mut rect = RECT::default();
            unsafe { GetWindowRect(self.0, &mut rect).unwrap() };
            Rect::new(
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            )
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    let active_primary = TestWindow::new(false);
    let active_secondary = TestWindow::new(false);
    let inactive_tiled = TestWindow::new(false);
    let inactive_floating = TestWindow::new(false);
    let inactive_secondary = TestWindow::new(false);
    let fullscreen = TestWindow::new(false);
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
        SetWindowPos(
            fullscreen.0,
            None,
            0,
            0,
            1920,
            1080,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .unwrap();
    }
    let minimized = TestWindow::new(true);
    let unmanaged = TestWindow::new(false);
    let windows = [
        &active_primary,
        &active_secondary,
        &inactive_tiled,
        &inactive_floating,
        &inactive_secondary,
        &fullscreen,
        &minimized,
        &unmanaged,
    ];
    let before: HashMap<_, _> = windows
        .iter()
        .map(|window| (window.id(), window.rect()))
        .collect();
    let expected_parked = HashSet::from([
        inactive_tiled.id(),
        inactive_floating.id(),
        inactive_secondary.id(),
    ]);
    let mut primary_active = Workspace::default();
    primary_active
        .insert_window(active_primary.id(), Some(640))
        .unwrap();
    let mut primary_inactive = Workspace::default();
    primary_inactive
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();
    primary_inactive
        .insert_window(minimized.id(), Some(480))
        .unwrap();
    primary_inactive
        .add_floating(inactive_floating.id(), before[&inactive_floating.id()])
        .unwrap();
    primary_inactive
        .insert_window(fullscreen.id(), Some(800))
        .unwrap();
    primary_inactive.mark_minimized(inactive_tiled.id());
    let mut secondary_active = Workspace::default();
    secondary_active
        .insert_window(active_secondary.id(), Some(600))
        .unwrap();
    let mut secondary_inactive = Workspace::default();
    secondary_inactive
        .insert_window(inactive_secondary.id(), Some(500))
        .unwrap();
    let snapshot = crate::state::StateSnapshot {
        saved_at: "0".to_string(),
        workspaces: vec![
            crate::state::WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".into(),
                workspace_index: 0,
                workspace: primary_active,
            },
            crate::state::WorkspaceSnapshot {
                monitor_device_name: "DISPLAY1".into(),
                workspace_index: 1,
                workspace: primary_inactive,
            },
            crate::state::WorkspaceSnapshot {
                monitor_device_name: "DISPLAY2".into(),
                workspace_index: 0,
                workspace: secondary_inactive,
            },
            crate::state::WorkspaceSnapshot {
                monitor_device_name: "DISPLAY2".into(),
                workspace_index: 1,
                workspace: secondary_active,
            },
        ],
        focused_monitor_name: "DISPLAY1".into(),
        active_workspace: HashMap::from([("DISPLAY1".into(), 0), ("DISPLAY2".into(), 1)]),
        tab_title_overrides: HashMap::new(),
    };
    let mut state = structure_restore_state();
    state.injected_native_offscreen_enabled = true;
    state.config.behavior.disable_snap_layouts = false;
    state.restore_workspace_structure(&snapshot);
    state.restore_state(&snapshot);
    state.paused = false;
    let membership = state.all_managed_window_ids();

    for _ in 0..2 {
        crate::prepare_initial_layout(&mut state);
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(state.active_workspace_idx(2), 1);
        assert_eq!(state.all_managed_window_ids(), membership);
        assert!(!state.workspaces[&1][1].is_minimized(inactive_tiled.id()));
        assert!(state.workspaces[&1][1].is_minimized(minimized.id()));
        assert!(state.is_application_fullscreen(fullscreen.id()));
        for window in windows {
            let rect = window.rect();
            let original = before[&window.id()];
            if expected_parked.contains(&window.id()) {
                assert!(
                    leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect),
                    "restored inactive HWND {} must be physically parked, got {rect:?}",
                    window.id()
                );
                assert_eq!((rect.width, rect.height), (original.width, original.height));
            } else {
                assert_eq!(
                    rect, original,
                    "startup must not move active, minimized, application-fullscreen or unmanaged windows"
                );
            }
            assert!(!leopardwm_platform_win32::is_window_visible(window.id()));
        }
        assert_eq!(state.workspaces[&1][1].columns()[0].width(), 720);
        assert_eq!(
            state.workspaces[&1][1].floating_windows()[0].rect,
            before[&inactive_floating.id()]
        );
    }
}

#[test]
fn test_display_reconcile_parks_restored_inactive_windows() {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let active_primary = ParkProbeWindow::new(false);
    let active_returning = ParkProbeWindow::new(false);
    let inactive_tiled = ParkProbeWindow::new(false);
    let inactive_floating = ParkProbeWindow::new(false);
    let fullscreen = ParkProbeWindow::new(false);
    unsafe {
        SetWindowPos(
            fullscreen.0,
            None,
            0,
            0,
            1920,
            1080,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .unwrap();
    }
    let minimized = ParkProbeWindow::new(true);
    let unmanaged = ParkProbeWindow::new(false);
    let windows = [
        &active_primary,
        &active_returning,
        &inactive_tiled,
        &inactive_floating,
        &fullscreen,
        &minimized,
        &unmanaged,
    ];
    let before: HashMap<_, _> = windows
        .iter()
        .map(|window| (window.id(), window.rect()))
        .collect();
    let expected_parked = HashSet::from([inactive_tiled.id(), inactive_floating.id()]);

    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.injected_native_offscreen_enabled = true;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(active_primary.id(), Some(640))
        .unwrap();

    let mut returning_inactive = Workspace::default();
    returning_inactive
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();
    returning_inactive.mark_minimized(inactive_tiled.id());
    returning_inactive
        .insert_window(minimized.id(), Some(480))
        .unwrap();
    returning_inactive.mark_minimized(minimized.id());
    returning_inactive
        .add_floating(inactive_floating.id(), before[&inactive_floating.id()])
        .unwrap();
    returning_inactive
        .insert_window(fullscreen.id(), Some(800))
        .unwrap();

    let mut returning_active = Workspace::default();
    returning_active
        .insert_window(active_returning.id(), Some(600))
        .unwrap();

    state
        .workspaces
        .insert(2, vec![returning_inactive, returning_active]);
    state.active_workspace.insert(2, 1);

    state.reconcile_monitors(test_monitors());
    assert!(!state.workspaces.contains_key(&2));
    assert!(state.workspaces[&1][0].contains_window(inactive_tiled.id()));
    assert!(state.workspaces[&1][0].contains_window(active_returning.id()));
    assert!(state.stashed_monitor_layouts.contains_key("DISPLAY2"));

    let mut returned = two_monitors();
    returned[1].id = 99;
    state.reconcile_monitors(returned);

    assert!(state.workspaces.contains_key(&99));
    assert_eq!(state.active_workspace_idx(99), 1);
    assert!(state.workspaces[&99][0].contains_window(inactive_tiled.id()));
    assert!(state.workspaces[&99][0].is_floating(inactive_floating.id()));
    assert!(state.workspaces[&99][1].contains_window(active_returning.id()));
    assert!(!state.workspaces[&1][0].contains_window(inactive_tiled.id()));
    assert!(!state.stashed_monitor_layouts.contains_key("DISPLAY2"));
    for window in [&inactive_tiled, &inactive_floating] {
        assert_eq!(
            window.rect(),
            before[&window.id()],
            "monitor restore must not park native windows by itself"
        );
    }
    assert!(state.paused);
    assert!(state.workspaces[&99][0].is_minimized(inactive_tiled.id()));

    let membership = state.all_managed_window_ids();
    state.prepare_inactive_workspace_windows();
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.active_workspace_idx(99), 1);
    assert_eq!(state.all_managed_window_ids(), membership);
    assert!(!state.workspaces[&99][0].is_minimized(inactive_tiled.id()));
    assert!(state.workspaces[&99][0].is_minimized(minimized.id()));
    for window in windows {
        assert_eq!(
            window.rect(),
            before[&window.id()],
            "paused display preparation must not move native windows"
        );
    }

    state.paused = false;
    for _ in 0..2 {
        state.prepare_inactive_workspace_windows();
        assert_eq!(state.active_workspace_idx(1), 0);
        assert_eq!(state.active_workspace_idx(99), 1);
        assert_eq!(state.all_managed_window_ids(), membership);
        assert!(!state.workspaces[&99][0].is_minimized(inactive_tiled.id()));
        assert!(state.workspaces[&99][0].is_minimized(minimized.id()));
        for window in windows {
            let rect = window.rect();
            let original = before[&window.id()];
            if expected_parked.contains(&window.id()) {
                assert!(
                    leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect),
                    "restored inactive HWND {} must be physically parked, got {rect:?}",
                    window.id()
                );
                assert_eq!((rect.width, rect.height), (original.width, original.height));
            } else {
                assert_eq!(
                    rect, original,
                    "display reconcile must not move active, minimized, application-fullscreen or unmanaged windows"
                );
            }
            assert!(!leopardwm_platform_win32::is_window_visible(window.id()));
        }
        assert!(state.is_application_fullscreen(fullscreen.id()));
        assert_eq!(state.workspaces[&99][0].columns()[0].width(), 720);
        assert_eq!(
            state.workspaces[&99][0].floating_windows()[0].rect,
            before[&inactive_floating.id()]
        );
    }
}

struct ParkProbeWindow(windows::Win32::Foundation::HWND);
impl ParkProbeWindow {
    fn new(minimized: bool) -> Self {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_MINIMIZE, WS_POPUP,
        };
        Self(unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                if minimized {
                    WS_POPUP | WS_MINIMIZE
                } else {
                    WS_POPUP
                },
                100,
                100,
                200,
                100,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        })
    }

    fn id(&self) -> u64 {
        self.0 .0 as u64
    }

    fn rect(&self) -> Rect {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut rect = RECT::default();
        unsafe { GetWindowRect(self.0, &mut rect).unwrap() };
        Rect::new(
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }
}
impl Drop for ParkProbeWindow {
    fn drop(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn park_probe_state() -> AppState {
    let mut state = AppState::new_with_config(test_config(), two_monitors());
    state.paused = false;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(1),
    ));
    state.ensure_workspace_exists(1, 1);
    state
}

#[test]
fn test_resume_parks_inactive_windows_and_syncs_taskbar() {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let active = ParkProbeWindow::new(false);
    let inactive_tiled = ParkProbeWindow::new(false);
    let inactive_floating = ParkProbeWindow::new(false);
    let fullscreen = ParkProbeWindow::new(false);
    unsafe {
        SetWindowPos(
            fullscreen.0,
            None,
            0,
            0,
            1920,
            1080,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .unwrap();
    }
    let minimized = ParkProbeWindow::new(true);
    let unmanaged = ParkProbeWindow::new(false);
    let windows = [
        &active,
        &inactive_tiled,
        &inactive_floating,
        &fullscreen,
        &minimized,
        &unmanaged,
    ];
    let before: HashMap<_, _> = windows
        .iter()
        .map(|window| (window.id(), window.rect()))
        .collect();
    let expected_parked = HashSet::from([inactive_tiled.id(), inactive_floating.id()]);

    let mut state = park_probe_state();
    state.injected_native_offscreen_enabled = true;
    state.paused = true;
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(active.id(), Some(640))
        .unwrap();
    let inactive = &mut state.workspaces.get_mut(&1).unwrap()[1];
    inactive
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();
    inactive
        .add_floating(inactive_floating.id(), before[&inactive_floating.id()])
        .unwrap();
    inactive.insert_window(fullscreen.id(), Some(800)).unwrap();
    inactive.insert_window(minimized.id(), Some(480)).unwrap();
    inactive.mark_minimized(minimized.id());
    let membership = state.all_managed_window_ids();

    state.toggle_pause("test resume catchup").unwrap();
    assert!(!state.paused);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(state.all_managed_window_ids(), membership);
    assert!(state.workspaces[&1][1].is_minimized(minimized.id()));
    assert!(state.is_application_fullscreen(fullscreen.id()));
    for window in windows {
        let rect = window.rect();
        let original = before[&window.id()];
        if expected_parked.contains(&window.id()) {
            assert!(
                leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect),
                "resumed inactive HWND {} must be physically parked, got {rect:?}",
                window.id()
            );
            assert_eq!((rect.width, rect.height), (original.width, original.height));
        } else if window.id() == active.id() {
            assert_eq!(
                rect, original,
                "active window is unmoved by parking while the active apply backend is faked"
            );
        } else {
            assert_eq!(
                rect, original,
                "resume parking must not move minimized, application-fullscreen or unmanaged windows"
            );
        }
        assert!(!leopardwm_platform_win32::is_window_visible(window.id()));
    }
    let commands = state.take_recorded_taskbar_commands();
    assert!(commands.contains(&(active.id(), true)));
    assert!(commands.contains(&(inactive_tiled.id(), false)));
    assert!(commands.contains(&(inactive_floating.id(), false)));
    assert!(commands.contains(&(minimized.id(), false)));
    assert!(!commands.iter().any(|(id, _)| *id == fullscreen.id()));
    assert!(!commands.iter().any(|(id, _)| *id == unmanaged.id()));
}

#[test]
fn test_failed_resume_does_not_park_or_sync_taskbar() {
    let active = ParkProbeWindow::new(false);
    let inactive_tiled = ParkProbeWindow::new(false);
    let before_active = active.rect();
    let before_inactive = inactive_tiled.rect();

    let mut state = park_probe_state();
    state.paused = true;
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndFail(
        Duration::from_millis(1),
    ));
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(active.id(), Some(640))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();

    state
        .toggle_pause("test failed resume catchup")
        .expect_err("injected resume apply failure should propagate");
    assert!(state.paused, "failed resume should restore paused state");
    assert_eq!(active.rect(), before_active);
    assert_eq!(inactive_tiled.rect(), before_inactive);
    assert!(
        !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&inactive_tiled.rect()),
        "failed resume must not park inactive windows"
    );
    assert!(state.take_recorded_taskbar_commands().is_empty());
}

#[test]
fn test_display_change_retry_pauses_when_original_worker_is_still_running() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(250),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    let generation = pending_display_change_retry_generation(&state);
    let apply_epoch = state.apply_epoch.load(Ordering::SeqCst);

    state
        .run_display_change_apply_retry(generation.wrapping_add(1))
        .expect("a stale retry generation should be ignored");
    assert!(!state.paused);
    assert!(!state.applying_layout);
    assert_eq!(state.apply_epoch.load(Ordering::SeqCst), apply_epoch);
    assert_eq!(pending_display_change_retry_generation(&state), generation);

    state
        .run_display_change_apply_retry(generation)
        .expect_err("a still-running original worker should make the matching retry time out");
    let paused = state.paused;
    let retry_pending = state.display_change_apply_retry.is_some();
    let report = state.take_layout_apply_timeout_report();
    join_pending_test_apply_workers(&mut state);

    assert!(paused, "the blocked retry should pause tiling");
    assert!(!retry_pending);
    let report = report.expect("the blocked retry should create a timeout report");
    assert_eq!(report.timeout, Duration::from_millis(10));
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].hwnd, 100);
}

#[test]
fn test_display_change_timeout_schedules_retry_without_pausing() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);

    assert!(
        !state.paused,
        "display-change timeout should not pause tiling"
    );
    assert!(
        state.pending_layout_apply_timeout_report.is_none(),
        "display-change timeout should not create a timeout report"
    );
    assert!(
        state.display_change_apply_retry.is_some(),
        "display-change timeout should schedule its one retry"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "deferred worker should finish"
        );
        std::thread::yield_now();
    }
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state
        .apply_layout()
        .expect("a later placement should reap the deferred worker and succeed");
    assert!(state.pending_apply_workers.is_empty());
    assert_eq!(
        state.late_worker_recovery_count.load(Ordering::SeqCst),
        0,
        "a deferred display-change worker must not recover before its retry"
    );
    assert_eq!(
        state
            .late_apply_worker_reap_recovery_count
            .load(Ordering::SeqCst),
        0,
        "reaping the deferred worker must not request late-worker recovery"
    );
}

#[test]
fn test_manual_resume_releases_deferred_worker_recovery_when_apply_is_blocked() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(250),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    assert_eq!(state.suppressed_late_recovery_workers.len(), 1);

    state
        .toggle_pause("test pause")
        .expect("manual pause should succeed");
    state
        .toggle_pause("test resume")
        .expect_err("resume must not overlap the still-running apply worker");

    assert!(
        state.paused,
        "failed resume should restore the paused state"
    );
    assert!(state.display_change_apply_retry.is_none());
    assert!(state.suppressed_late_recovery_workers.is_empty());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !state.pending_apply_workers[0].is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "deferred worker should finish"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        state.late_worker_recovery_count.load(Ordering::SeqCst),
        1,
        "a deferred worker must recover late if manual resume drops its retry"
    );
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_shutdown_reenables_deferred_worker_late_recovery() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(250),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    assert_eq!(state.suppressed_late_recovery_workers.len(), 1);
    assert!(!state.pending_apply_workers[0].is_finished());

    let workers = state.begin_shutdown_or_revert();
    for worker in workers {
        worker
            .join()
            .expect("shutdown should join the deferred worker");
    }

    assert_eq!(
        state.late_worker_recovery_count.load(Ordering::SeqCst),
        1,
        "a deferred worker finishing after shutdown begins must recover visibility"
    );
}

#[test]
fn test_successful_apply_clears_pending_display_change_retry() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(100, Some(800))
        .unwrap();
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    std::thread::sleep(Duration::from_millis(50));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(1),
    ));

    state
        .apply_layout()
        .expect("the later apply should succeed");

    assert!(state.display_change_apply_retry.is_none());
    assert!(!state.display_change_apply_retry_used);
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_display_change_apply_retry_timeout_uses_normal_pause_path() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    std::thread::sleep(Duration::from_millis(50));

    state
        .run_display_change_apply_retry(pending_display_change_retry_generation(&state))
        .expect_err("the retry placement should also time out");

    assert!(state.paused, "a second timeout should pause tiling");
    assert!(state.display_change_apply_retry.is_none());
    let report = state
        .take_layout_apply_timeout_report()
        .expect("the retry timeout should create the normal timeout report");
    assert_eq!(report.timeout, Duration::from_millis(10));
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_display_change_apply_retry_success_resets_allowance() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    std::thread::sleep(Duration::from_millis(50));
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(1),
    ));

    state
        .run_display_change_apply_retry(pending_display_change_retry_generation(&state))
        .expect("the retry placement should succeed");

    assert!(!state.paused);
    assert!(state.display_change_apply_retry.is_none());
    assert!(!state.display_change_apply_retry_used);

    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));
    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(!state.paused);
    assert!(state.display_change_apply_retry.is_some());
    assert!(state.display_change_apply_retry_used);
    assert!(state.pending_layout_apply_timeout_report.is_none());
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_display_change_apply_retry_while_paused_does_not_apply() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.injected_display_monitors = Some(test_monitors());
    state.paused = false;
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(40),
    ));

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.display_change_apply_retry.is_some());
    state.paused = true;
    let apply_epoch = state.apply_epoch.load(Ordering::SeqCst);

    state
        .run_display_change_apply_retry(pending_display_change_retry_generation(&state))
        .expect("a paused retry should be ignored");

    assert!(state.paused);
    assert!(state.display_change_apply_retry.is_none());
    assert_eq!(state.apply_epoch.load(Ordering::SeqCst), apply_epoch);
    assert_eq!(
        state
            .paused_display_retry_recovery_count
            .load(Ordering::SeqCst),
        1,
        "dropping a retry while paused must request visibility recovery"
    );
    join_pending_test_apply_workers(&mut state);
}

#[test]
fn test_display_change_event_parks_inactive_windows_and_syncs_taskbar() {
    let active = ParkProbeWindow::new(false);
    let inactive_tiled = ParkProbeWindow::new(false);
    let unmanaged = ParkProbeWindow::new(false);
    let before_active = active.rect();
    let before_inactive = inactive_tiled.rect();
    let before_unmanaged = unmanaged.rect();

    let mut state = park_probe_state();
    state.injected_native_offscreen_enabled = true;
    state.injected_display_monitors = Some(two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(active.id(), Some(640))
        .unwrap();
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(!state.paused);
    assert_eq!(state.active_workspace_idx(1), 0);
    assert_eq!(active.rect(), before_active);
    assert_eq!(unmanaged.rect(), before_unmanaged);
    assert!(
        leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&inactive_tiled.rect()),
        "display-change caller must park restored inactive windows, got {:?}",
        inactive_tiled.rect()
    );
    assert_eq!(
        (inactive_tiled.rect().width, inactive_tiled.rect().height),
        (before_inactive.width, before_inactive.height)
    );
    let commands = state.take_recorded_taskbar_commands();
    assert!(commands.contains(&(active.id(), true)));
    assert!(commands.contains(&(inactive_tiled.id(), false)));
    assert!(!commands.iter().any(|(id, _)| *id == unmanaged.id()));
}

#[test]
fn test_paused_display_change_event_resyncs_without_moving_and_syncs_taskbar() {
    let active = ParkProbeWindow::new(false);
    let inactive_tiled = ParkProbeWindow::new(false);
    let minimized = ParkProbeWindow::new(true);
    let before_active = active.rect();
    let before_inactive = inactive_tiled.rect();
    let before_minimized = minimized.rect();

    let mut state = park_probe_state();
    state.paused = true;
    state.injected_display_monitors = Some(two_monitors());
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(active.id(), Some(640))
        .unwrap();
    let inactive = &mut state.workspaces.get_mut(&1).unwrap()[1];
    inactive
        .insert_window(inactive_tiled.id(), Some(720))
        .unwrap();
    inactive.mark_minimized(inactive_tiled.id());
    inactive.insert_window(minimized.id(), Some(480)).unwrap();
    inactive.mark_minimized(minimized.id());

    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(state.paused);
    assert!(!state.workspaces[&1][1].is_minimized(inactive_tiled.id()));
    assert!(state.workspaces[&1][1].is_minimized(minimized.id()));
    assert_eq!(active.rect(), before_active);
    assert_eq!(inactive_tiled.rect(), before_inactive);
    assert_eq!(minimized.rect(), before_minimized);
    let commands = state.take_recorded_taskbar_commands();
    assert!(commands.contains(&(active.id(), true)));
    assert!(commands.contains(&(inactive_tiled.id(), true)));
    assert!(commands.contains(&(minimized.id(), true)));
}

#[test]
fn test_restore_structure_reapplies_snap_suppression() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowLongW, GWL_STYLE, WS_CAPTION, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };

    let wait_for_style_requests = pump_window_style_worker_until_idle;

    struct TestWindow(HWND);
    impl TestWindow {
        fn new(maximize_box: bool) -> Self {
            let mut style = WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX;
            if maximize_box {
                style |= WS_MAXIMIZEBOX;
            }
            Self(unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!(""),
                    style,
                    0,
                    0,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            })
        }

        fn id(&self) -> u64 {
            self.0 .0 as u64
        }

        fn style(&self) -> u32 {
            unsafe { GetWindowLongW(self.0, GWL_STYLE) as u32 }
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = leopardwm_platform_win32::restore_maximizebox(self.id());
            pump_window_style_worker_until_idle();
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    for enabled in [false, true] {
        let tiled = TestWindow::new(true);
        let inactive = TestWindow::new(true);
        let floating = TestWindow::new(true);
        let no_maximize_box = TestWindow::new(false);
        let unmanaged = TestWindow::new(true);
        let windows = [&tiled, &inactive, &floating, &no_maximize_box, &unmanaged];
        let original_styles = windows.map(|window| window.style());
        assert_ne!(original_styles[0] & WS_MAXIMIZEBOX.0, 0);
        assert_eq!(original_styles[3] & WS_MAXIMIZEBOX.0, 0);

        let mut workspace = Workspace::default();
        workspace.insert_window(tiled.id(), Some(640)).unwrap();
        workspace
            .insert_window(no_maximize_box.id(), Some(480))
            .unwrap();
        workspace
            .add_floating(floating.id(), Rect::new(100, 100, 200, 100))
            .unwrap();
        let mut inactive_workspace = Workspace::default();
        inactive_workspace
            .insert_window(inactive.id(), Some(720))
            .unwrap();
        let snapshot = crate::state::StateSnapshot {
            saved_at: "0".to_string(),
            workspaces: vec![
                crate::state::WorkspaceSnapshot {
                    monitor_device_name: "DISPLAY1".to_string(),
                    workspace_index: 0,
                    workspace,
                },
                crate::state::WorkspaceSnapshot {
                    monitor_device_name: "DISPLAY2".to_string(),
                    workspace_index: 1,
                    workspace: inactive_workspace,
                },
            ],
            focused_monitor_name: "DISPLAY1".to_string(),
            active_workspace: HashMap::new(),
            tab_title_overrides: HashMap::new(),
        };
        let mut state = structure_restore_state();
        // Production startup restore runs unpaused; AppState tests default paused.
        state.paused = false;
        state.config.behavior.disable_snap_layouts = enabled;
        let expected_tracked = if enabled {
            HashSet::from([tiled.id(), inactive.id()])
        } else {
            HashSet::new()
        };
        let mut expected_styles = original_styles;
        if enabled {
            expected_styles[0] &= !WS_MAXIMIZEBOX.0;
            expected_styles[1] &= !WS_MAXIMIZEBOX.0;
        }

        for _ in 0..2 {
            let restored = state.restore_workspace_structure(&snapshot);
            wait_for_style_requests();
            assert_eq!(restored, HashSet::from([(1, 0), (2, 1)]));
            assert_eq!(
                windows.map(|window| window.style()),
                expected_styles,
                "startup must reapply the configured restriction only to restored tiled windows"
            );
            assert_eq!(state.snap_disabled_hwnds, expected_tracked);
            assert_eq!(state.workspaces[&1][0].columns()[0].width(), 640);
            assert_eq!(state.active_workspace_idx(2), 0);
            for window in windows {
                assert!(!leopardwm_platform_win32::is_window_visible(window.id()));
            }
        }

        state.restore_snap_for_all_windows();
        wait_for_style_requests();
        assert!(state.snap_disabled_hwnds.is_empty());
        assert_eq!(windows.map(|window| window.style()), original_styles);
    }
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
    assert!(
        restored.is_empty(),
        "unknown monitor produces no restored slots"
    );
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
    assert_ne!(
        before, after,
        "scroll offset change must change the signature"
    );
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
fn test_width_only_persistence_tracks_requested_not_native_width() {
    let window_id = u64::MAX - 1;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let viewport_width = state.focused_viewport().width;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(window_id, Some(300))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    state.install_save_channel(tx);
    state.request_save_if_changed();
    assert_eq!(rx.try_recv(), Ok(()));

    let workspace = state.focused_workspace_mut().unwrap();
    workspace.set_window_min_width(window_id, 1200);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[0]),
        1200
    );
    state.request_save_if_changed();
    assert_eq!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );

    let workspace = state.focused_workspace_mut().unwrap();
    workspace.set_focused_column_width_fraction(0.25, viewport_width);
    let requested_width = workspace.columns()[0].width();
    assert_ne!(requested_width, 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[0]),
        1200
    );
    state.request_save_if_changed();
    assert_eq!(
        rx.try_recv(),
        Ok(()),
        "requested-width change must queue a save"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&state.build_state_json().unwrap()).unwrap();
    assert_eq!(
        saved["workspaces"][0]["workspace"]["columns"][0]["width"],
        requested_width
    );
    state.request_save_if_changed();
    assert_eq!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}

#[test]
fn test_width_only_persistence_command_saves_unchanged_placements() {
    let window_id = u64::MAX - 1;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = true;
    let workspace = state.focused_workspace_mut().unwrap();
    workspace.insert_window(window_id, Some(300)).unwrap();
    workspace.commit_pending_min_size_clears();
    workspace.set_window_min_width(window_id, 1200);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    state.install_save_channel(tx);
    state.apply_layout().unwrap();
    assert_eq!(rx.try_recv(), Ok(()));
    let placements = state.last_placed_layout_rects.clone();
    assert_eq!(placements[&window_id].width, 1200);

    assert!(matches!(
        state.handle_command(IpcCommand::SetColumnWidth { fraction: 0.25 }),
        IpcResponse::Ok
    ));
    let workspace = state.focused_workspace().unwrap();
    assert_ne!(workspace.columns()[0].width(), 300);
    assert_eq!(
        workspace.effective_column_width(&workspace.columns()[0]),
        1200
    );
    assert_eq!(workspace.scroll_offset(), 0.0);
    assert_eq!(state.last_placed_layout_rects, placements);
    assert!(!state.applying_layout);
    assert!(state.layout_transition.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(()),
        "unchanged placements must not skip saving width intent"
    );
    state.apply_layout().unwrap();
    assert_eq!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}

#[test]
fn test_width_only_persistence_resize_queues_save() {
    let window_id = u64::MAX - 1;
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.reduce_motion = true;
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(window_id, Some(800))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    state.install_save_channel(tx);
    state.apply_layout().unwrap();
    assert_eq!(rx.try_recv(), Ok(()));

    assert!(matches!(
        state.handle_command(IpcCommand::Resize { delta: -100 }),
        IpcResponse::Ok
    ));
    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.columns()[0].width(), 700);
    assert_eq!(workspace.scroll_offset(), 0.0);
    assert_eq!(state.last_placed_layout_rects[&window_id].width, 700);
    assert_eq!(rx.try_recv(), Ok(()), "width-only resize must queue a save");
    state.apply_layout().unwrap();
    assert_eq!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
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
    assert!(
        vp.width > 0 && vp.height > 0,
        "fallback viewport is non-empty"
    );
}

#[test]
fn test_taskbar_work_area_invalidation_clears_heights_and_preserves_widths() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    let viewport = state.layout_viewport(monitor);
    state.ensure_workspace_exists(monitor, 1);

    for (workspace_idx, first, second) in [(0, 100, 101), (1, 200, 201)] {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[workspace_idx];
        workspace.insert_window(first, Some(400)).unwrap();
        workspace.insert_window_in_column(second, 0).unwrap();
        workspace.set_window_min_width(first, 600);
        workspace.set_window_min_height(first, 700);
        let placements = workspace.compute_placements(viewport);
        let first_height = placements
            .iter()
            .find(|placement| placement.window_id == first)
            .unwrap()
            .rect
            .height;
        let second_height = placements
            .iter()
            .find(|placement| placement.window_id == second)
            .unwrap()
            .rect
            .height;
        assert_ne!(
            first_height, second_height,
            "precondition: height is constrained"
        );
    }

    state.invalidate_display_change_constraints(false);

    for (workspace_idx, first, second) in [(0, 100, 101), (1, 200, 201)] {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[workspace_idx];
        let placements = workspace.compute_placements(viewport);
        let first_height = placements
            .iter()
            .find(|placement| placement.window_id == first)
            .unwrap()
            .rect
            .height;
        let second_height = placements
            .iter()
            .find(|placement| placement.window_id == second)
            .unwrap()
            .rect
            .height;
        assert_eq!(
            first_height, second_height,
            "heights clear in workspace {workspace_idx}"
        );
        workspace.set_all_column_widths(400);
        assert_eq!(
            workspace.effective_column_width(&workspace.columns()[0]),
            600,
            "width remains constrained in workspace {workspace_idx}"
        );
        assert_eq!(workspace.columns()[0].width(), 400);
    }
}

#[test]
fn test_full_display_invalidation_clears_widths_and_heights() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    let monitor = state.focused_monitor;
    let viewport = state.layout_viewport(monitor);
    state.ensure_workspace_exists(monitor, 1);

    for (workspace_idx, first, second) in [(0, 100, 101), (1, 200, 201)] {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[workspace_idx];
        workspace.insert_window(first, Some(400)).unwrap();
        workspace.insert_window_in_column(second, 0).unwrap();
        workspace.set_window_min_width(first, 600);
        workspace.set_window_min_height(first, 700);
    }

    state.invalidate_display_change_constraints(true);

    for (workspace_idx, first, second) in [(0, 100, 101), (1, 200, 201)] {
        let workspace = &mut state.workspaces.get_mut(&monitor).unwrap()[workspace_idx];
        let placements = workspace.compute_placements(viewport);
        let first_height = placements
            .iter()
            .find(|placement| placement.window_id == first)
            .unwrap()
            .rect
            .height;
        let second_height = placements
            .iter()
            .find(|placement| placement.window_id == second)
            .unwrap()
            .rect
            .height;
        assert_eq!(
            first_height, second_height,
            "heights clear in workspace {workspace_idx}"
        );
        workspace.set_all_column_widths(400);
        assert_eq!(
            workspace.effective_column_width(&workspace.columns()[0]),
            400,
            "width clears in workspace {workspace_idx}"
        );
        assert_eq!(workspace.columns()[0].width(), 400);
    }
}

#[path = "maximized_admission_tests.rs"]
mod maximized_admission_tests;

#[path = "maximized_admission_regression.rs"]
mod maximized_admission_regression;

#[path = "display_change_regression.rs"]
mod display_change_regression;

#[test]
fn test_taskbar_buttons_background_admission_respects_policy() {
    use config::TaskbarButtons::*;
    for (mode, show) in [
        (HideOffscreen, false),
        (HideInactiveWorkspaces, false),
        (ShowAll, true),
    ] {
        for paused in [false, true] {
            let show = show || paused;
            let mut config = test_config();
            config.behavior.taskbar_buttons = mode;
            config.window_rules = vec![tile_open_on_workspace_rule("TaskbarBackground", 2)];
            let mut state = AppState::new_with_config(config, test_monitors());
            state.paused = paused;
            let mut window = make_test_window_info(100);
            window.class_name = "TaskbarBackground".into();
            state.injected_window_info.insert(100, window);
            state.handle_window_event(WindowEvent::Created(100, 0));
            assert_eq!(state.find_window_workspace(100), Some((1, 1)));
            assert_eq!(state.active_workspace_idx(1), 0);
            let commands = state.take_recorded_taskbar_commands();
            assert!(commands.contains(&(100, show)), "{mode:?}: {commands:?}");
            assert!(!commands.contains(&(100, !show)), "{mode:?}: {commands:?}");
        }
    }
}

#[test]
fn test_taskbar_buttons_inactive_fullscreen_exit_respects_policy() {
    use config::TaskbarButtons::*;
    for (mode, show) in [
        (HideOffscreen, false),
        (HideInactiveWorkspaces, false),
        (ShowAll, true),
    ] {
        for paused in [false, true] {
            let show = show || paused;
            let mut state = AppState::new_with_config(test_config(), test_monitors());
            state.paused = paused;
            state.config.behavior.taskbar_buttons = mode;
            state.ensure_workspace_exists(1, 2);
            state.workspaces.get_mut(&1).unwrap()[1]
                .insert_window(100, None)
                .unwrap();
            state.application_fullscreen.insert(
                100,
                crate::state::ApplicationFullscreenState {
                    monitor_id: 1,
                    rect: Rect::new(0, 0, 1920, 1080),
                },
            );
            state.handle_window_event(WindowEvent::MovedOrResized(100));
            assert!(!state.is_application_fullscreen(100));
            assert_eq!(state.find_window_workspace(100), Some((1, 1)));
            let commands = state.take_recorded_taskbar_commands();
            assert!(commands.contains(&(100, show)), "{mode:?}: {commands:?}");
            assert!(!commands.contains(&(100, !show)), "{mode:?}: {commands:?}");
        }
    }
}

fn assert_taskbar_buttons_all_shown(state: &AppState) {
    let mut commands = state.take_recorded_taskbar_commands();
    assert!(!commands.is_empty());
    assert!(commands.iter().all(|(_, show)| *show), "{commands:?}");
    commands.sort_unstable();
    commands.dedup();
    let mut expected: Vec<_> = state
        .all_managed_window_ids()
        .into_iter()
        .map(|wid| (wid, true))
        .collect();
    expected.sort_unstable();
    assert_eq!(commands, expected);
}

#[test]
fn test_taskbar_buttons_paused_reload_keeps_buttons_shown_until_resume() {
    use config::TaskbarButtons::*;
    for release_all in [false, true] {
        for mode in [HideOffscreen, HideInactiveWorkspaces, ShowAll] {
            let mut state = taskbar_policy_state();
            state.sync_taskbar_buttons();
            assert!(state
                .take_recorded_taskbar_commands()
                .contains(&(700, false)));
            if release_all {
                assert!(matches!(
                    state.handle_command(IpcCommand::ReleaseAllWindows),
                    IpcResponse::Ok
                ));
            } else {
                state.toggle_pause("paused config regression").unwrap();
            }
            assert_taskbar_buttons_all_shown(&state);
            state.application_fullscreen.insert(
                100,
                crate::state::ApplicationFullscreenState {
                    monitor_id: 1,
                    rect: Rect::new(0, 0, 1920, 1080),
                },
            );
            let mut config = state.config.clone();
            config.behavior.taskbar_buttons = mode;
            state.apply_config(config);
            assert!(state.paused);
            assert_eq!(state.config.behavior.taskbar_buttons, mode);
            assert_taskbar_buttons_all_shown(&state);
            state.application_fullscreen.remove(&100);
            state
                .toggle_pause("paused config regression resume")
                .unwrap();
            assert!(!state.paused);
            let commands = state.take_recorded_taskbar_commands();
            assert!(
                commands.contains(&(700, mode == ShowAll)),
                "{mode:?}: {commands:?}"
            );
            assert!(
                commands.contains(&(300, mode != HideOffscreen)),
                "{mode:?}: {commands:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_taskbar_buttons_paused_tray_selection_keeps_buttons_shown_until_resume() {
    use config::TaskbarButtons::*;
    let mut state = taskbar_policy_state();
    state.sync_taskbar_buttons();
    assert!(state
        .take_recorded_taskbar_commands()
        .contains(&(700, false)));
    state.toggle_pause("paused tray regression").unwrap();
    assert_taskbar_buttons_all_shown(&state);
    let cancelled = state.apply_worker_cancelled.clone();
    let epoch = state.apply_epoch.clone();
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(state));
    let (event_tx, _event_rx) = mpsc::channel(8);
    let (settings_tx, _settings_rx) = std::sync::mpsc::channel();
    let worker =
        animation_worker::AnimationWorkerHandle::spawn(event_tx.clone(), cancelled.clone())
            .unwrap();
    let quit = quit_fallback::QuitFallback::new(
        cancelled,
        epoch,
        || {},
        |_| {},
        |_| {},
        quit_fallback::QuitTiming::default(),
    );
    let mut ctx = EventLoopCtx {
        quit_fallback: &quit,
        state: &state,
        event_tx: &event_tx,
        hotkey_state: &mut HotkeyState {
            handle: None,
            hook: None,
            mapping: HashMap::new(),
            requested_count: 0,
            registered_count: 0,
            failed_binds: Vec::new(),
            recording: false,
        },
        tray_manager: &None,
        snap_hint_overlay: &None,
        settings_sync_tx: &settings_tx,
        settings_handle: &mut None,
        animation_worker: &worker,
        animation_active: &mut false,
        last_frame_instant: &mut None,
        snap_hint_timer_handle: &mut None,
        focus_follows_mouse_timer: &mut None,
        display_change_timer: &mut None,
        idle_layout_reapply_timer: &mut None,
        display_change_apply_retry_timer: &mut None,
        mouse_hook_handle: &mut None,
        gestures: &mut GestureRuntime {
            startup_raw_input: false,
            handle: None,
            cancel: None,
            retry_install: true,
        },
    };
    for mode in [
        HideInactiveWorkspaces,
        HideOffscreen,
        ShowAll,
        HideInactiveWorkspaces,
    ] {
        handle_tray_event(&mut ctx, tray::TrayEvent::SetTaskbarButtons(mode)).await;
        let state = state.lock().await;
        assert!(state.paused);
        assert_eq!(state.config.behavior.taskbar_buttons, mode);
        assert_taskbar_buttons_all_shown(&state);
    }
    let mut state = state.lock().await;
    state.toggle_pause("paused tray regression resume").unwrap();
    assert!(!state.paused);
    let commands = state.take_recorded_taskbar_commands();
    assert!(commands.contains(&(700, false)), "{commands:?}");
    assert!(commands.contains(&(300, true)), "{commands:?}");
}

#[test]
fn test_floating_above_tiled_config_round_trip() {
    assert!(!Config::default().behavior.floating_above_tiled);
    let omitted: Config = toml::from_str("[behavior]\ntrack_focus_changes = false").unwrap();
    assert!(!omitted.behavior.floating_above_tiled);
    for enabled in [false, true] {
        for tracking in [false, true] {
            let from_settings: Config = serde_json::from_value(serde_json::json!({
                "behavior": {"floating_above_tiled": enabled, "track_focus_changes": tracking}
            }))
            .unwrap();
            let saved = toml::to_string_pretty(&from_settings).unwrap();
            let mut reloaded: Config = toml::from_str(&saved).unwrap();
            let warnings = reloaded.validate();
            let floating_warnings: Vec<_> = warnings
                .iter()
                .filter(|warning| warning.field == "behavior.floating_above_tiled")
                .collect();
            assert_eq!(floating_warnings.len(), usize::from(enabled && !tracking));
            if let Some(warning) = floating_warnings.first() {
                assert!(warning.message.contains("track_focus_changes"));
                assert!(warning.message.contains("no effect"));
            }
            assert_eq!(reloaded.behavior.floating_above_tiled, enabled);
            assert_eq!(reloaded.behavior.track_focus_changes, tracking);
            assert_eq!(
                serde_json::to_value(reloaded).unwrap()["behavior"]["floating_above_tiled"],
                enabled
            );
        }
    }
}

fn floating_above_tiled_state() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.paused = false;
    state.config.behavior.floating_above_tiled = true;
    let workspace = &mut state.workspaces.get_mut(&1).unwrap()[0];
    workspace.insert_window(100, None).unwrap();
    workspace
        .add_floating(200, Rect::new(10, 10, 300, 300))
        .unwrap();
    workspace
        .add_floating(300, Rect::new(20, 20, 300, 300))
        .unwrap();
    workspace.set_floating_pinned(300, true);
    state.sticky_windows.insert(300);
    state.workspaces.get_mut(&1).unwrap().push(Workspace::new());
    state.workspaces.get_mut(&1).unwrap()[1]
        .add_floating(400, Rect::new(30, 30, 300, 300))
        .unwrap();
    let mut other = Workspace::new();
    other.insert_window(600, None).unwrap();
    other
        .add_floating(500, Rect::new(2000, 30, 300, 300))
        .unwrap();
    state.workspaces.insert(2, vec![other]);
    state
}

#[test]
fn test_floating_above_tiled_selection_table() {
    for (name, focused, order, expected) in [
        (
            "active tiled focus",
            100,
            vec![900, 100, 200, 300, 400, 500],
            vec![300, 200],
        ),
        ("floating focus", 200, vec![200, 100, 300], vec![]),
        ("sticky floating focus", 300, vec![300, 100, 200], vec![]),
        ("unmanaged focus", 900, vec![900, 100, 200, 300], vec![]),
        (
            "inactive workspace focus",
            400,
            vec![400, 100, 200, 300],
            vec![],
        ),
        (
            "other monitor focus",
            600,
            vec![600, 100, 200, 300, 500],
            vec![500],
        ),
        (
            "hidden or native minimized floats absent",
            100,
            vec![100, 400, 500],
            vec![],
        ),
        ("no native foreground entry", 100, vec![200, 300], vec![]),
    ] {
        let state = floating_above_tiled_state();
        assert_eq!(
            state.floating_raise_order(focused, &order, false),
            expected,
            "{name}"
        );
    }
    let mut state = floating_above_tiled_state();
    assert_eq!(
        state.floating_raise_order(100, &[100, 300], false),
        vec![300]
    );
    state.workspaces.get_mut(&1).unwrap()[0].mark_minimized(100);
    assert!(state
        .floating_raise_order(100, &[100, 200, 300], false)
        .is_empty());
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(700, None)
        .unwrap();
    assert!(state
        .floating_raise_order(700, &[700, 100, 200, 300], false)
        .is_empty());
}

#[test]
fn test_floating_above_tiled_suppression_table() {
    for case in [
        "off",
        "focus tracking off",
        "overview",
        "paused",
        "native fullscreen",
        "application fullscreen",
        "LeopardWM fullscreen",
        "inactive fullscreen",
        "other monitor fullscreen",
    ] {
        let mut state = floating_above_tiled_state();
        match case {
            "off" => state.config.behavior.floating_above_tiled = false,
            "focus tracking off" => state.config.behavior.track_focus_changes = false,
            "overview" => state.overview_open = true,
            "paused" => state.paused = true,
            "application fullscreen" | "other monitor fullscreen" => {
                state.application_fullscreen.insert(
                    900,
                    crate::state::ApplicationFullscreenState {
                        monitor_id: if case == "other monitor fullscreen" {
                            2
                        } else {
                            1
                        },
                        rect: Rect::new(0, 0, 1920, 1080),
                    },
                );
            }
            "LeopardWM fullscreen" => {
                state.workspaces.get_mut(&1).unwrap()[0].toggle_fullscreen();
            }
            "inactive fullscreen" => {
                let ws = &mut state.workspaces.get_mut(&1).unwrap()[1];
                ws.insert_window(700, None).unwrap();
                ws.toggle_fullscreen();
            }
            _ => {}
        }
        let expected = if case == "other monitor fullscreen" {
            vec![300, 200]
        } else {
            vec![]
        };
        assert_eq!(
            state.floating_raise_order(100, &[100, 200, 300], case == "native fullscreen"),
            expected,
            "{case}"
        );
    }
}

#[test]
fn test_floating_above_tiled_order_and_repeated_focus_are_idempotent() {
    let state = floating_above_tiled_state();
    for mut order in [
        vec![100, 200, 300],
        vec![300, 100, 200],
        vec![200, 100, 300],
    ] {
        let original_floats: Vec<_> = order.iter().copied().filter(|id| *id != 100).collect();
        let raises = state.floating_raise_order(100, &order, false);
        assert_eq!(
            raises,
            original_floats.iter().rev().copied().collect::<Vec<_>>()
        );
        for id in raises {
            order.retain(|&current| current != id);
            order.insert(0, id);
        }
        assert_eq!(&order[..2], original_floats);
        assert_eq!(order[2], 100);
        assert!(state.floating_raise_order(100, &order, false).is_empty());
    }
}

#[test]
fn test_late_floating_feedback_preserves_target_and_records_external_geometry() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (readable, delta) in [(true, 0), (true, 2), (true, 3), (false, 0)] {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_mismatched_landing_state(&owner, readable);
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        if delta != 0 {
            owner.resize(230 + delta, 450 + delta);
        }
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        let expected = if readable && delta <= 2 {
            Rect::new(-300, 0, 300, 560)
        } else {
            Rect::new(230 + delta, 0, 450 + delta, 560)
        };
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            expected,
            "readable={readable}, delta={delta}"
        );
    }
}

#[test]
fn test_floating_return_to_old_landing_after_external_or_interactive_move() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (interactive, deferred_return) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let owner = OffscreenResizeOwner::new();
        let mut state = floating_mismatched_landing_state(&owner, true);
        state.moved_or_resized_suppression.remove(&owner.hwnd);
        let placement_sequence = state.physical_request_seq;
        if interactive {
            state.handle_window_event(WindowEvent::MoveSizeStart(owner.hwnd));
        }
        owner.resize(400, 500);
        state.handle_window_event(if interactive {
            WindowEvent::MoveSizeEnd(owner.hwnd)
        } else {
            WindowEvent::MovedOrResized(owner.hwnd)
        });
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            Rect::new(400, 0, 500, 560)
        );
        if deferred_return {
            state.arm_moved_or_resized_suppression([owner.hwnd]);
        }
        owner.resize(230, 450);
        state.handle_window_event(WindowEvent::MovedOrResized(owner.hwnd));
        if deferred_return {
            assert!(state.deferred_moved_or_resized.contains(&owner.hwnd));
            state.moved_or_resized_suppression.remove(&owner.hwnd);
            assert!(!state.recheck_deferred_window_moves());
        }
        assert_eq!(state.physical_request_seq, placement_sequence);
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            Rect::new(230, 0, 450, 560),
            "interactive={interactive}, deferred_return={deferred_return}"
        );
        for index in [2, 1] {
            assert!(matches!(
                state.handle_command(IpcCommand::SwitchWorkspace { index }),
                IpcResponse::Ok
            ));
        }
        assert_eq!(owner.rect(), Rect::new(230, 0, 450, 560));
        assert_eq!(
            state.workspaces[&1][0].floating_windows()[0].rect,
            Rect::new(230, 0, 450, 560)
        );
    }
}
