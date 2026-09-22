use crate::event_handler::{AdmissionKind, AdmitOutcome};
use crate::state::{
    AppState, DragPreviewMode, DragState, MoveOrigin, StashedMonitorLayout,
    TestApplyPlacementsBehavior, DRAG_PLACEHOLDER_HWND,
};
use crate::temporary_ignore::IdentityReadError;
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{IpcCommand, IpcResponse};
use leopardwm_platform_win32::{ManageBlock, MonitorInfo, WindowEvent, WindowInfo};
use std::time::{Duration, Instant};

fn state() -> AppState {
    AppState::new_with_config(
        crate::config::Config::default(),
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

fn set_foreground(state: &mut AppState, hwnd: u64) {
    state.injected_foreground_hwnd = Some(Some(hwnd));
    state.injected_foreground_is_valid = Some(true);
}

fn admit(state: &mut AppState, hwnd: u64) -> u64 {
    inject(state, hwnd, "Managed", "ManagedClass", 2010);
    let outcome = state.try_admit_window(hwnd, AdmissionKind::Automatic);
    assert_eq!(
        outcome,
        AdmitOutcome::Admitted,
        "hwnd {hwnd} should be admitted"
    );
    recorded_token(state, hwnd)
}

fn recorded_token(state: &AppState, hwnd: u64) -> u64 {
    let token = state
        .managed_lifetime_tokens
        .get(&hwnd)
        .copied()
        .unwrap_or_else(|| panic!("hwnd {hwnd} should have a managed lifetime record"));
    assert_eq!(state.injected_managed_tokens.get(&hwnd), Some(&token));
    token
}

fn membership_count(state: &AppState, hwnd: u64) -> usize {
    state
        .all_managed_window_ids()
        .iter()
        .filter(|&&id| id == hwnd)
        .count()
}

/// Tests treat every re-created HWND as a popup, and a window managed for less
/// than 30s is suppressed on the next Created. Recycle applies to a long-lived window.
fn backdate_admission(state: &mut AppState, hwnd: u64) {
    let managed_at = Instant::now()
        .checked_sub(Duration::from_secs(31))
        .expect("managed-at should predate the transient threshold");
    state.window_managed_at.insert(hwnd, managed_at);
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
    if stale_workspace.contains_window(hwnd) {
        let _ = stale_workspace.remove_window(hwnd);
        stale_workspace.remove_floating(hwnd);
    }
    stale_workspace.insert_window(hwnd, None).unwrap();
    state.stashed_monitor_layouts.insert(
        "STALE".into(),
        StashedMonitorLayout {
            workspaces: vec![stale_workspace],
            active_workspace: 0,
            source_viewport_width: 1920,
        },
    );
    state
        .tab_title_overrides
        .insert(hwnd, "old-title".to_string());
    state
        .last_placed_layout_rects
        .insert(hwnd, Rect::new(1, 2, 3, 4));
}

fn assert_recycled_lifetime_caches_cleared(state: &AppState, hwnd: u64) {
    assert!(!state.overview_icon_cache.contains_key(&hwnd));
    assert!(!state.move_origins.contains_key(&hwnd));
    assert_eq!(state.move_origins.get(&20).unwrap().sibling, None);
    assert!(state.stashed_monitor_layouts.values().all(|layout| layout
        .workspaces
        .iter()
        .all(|workspace| !workspace.contains_window(hwnd))));
    assert!(!state.tab_title_overrides.contains_key(&hwnd));
    assert!(!state.last_placed_layout_rects.contains_key(&hwnd));
}

fn simulate_missing_managed_token(state: &mut AppState, hwnd: u64) {
    state.injected_managed_tokens.remove(&hwnd);
    state.injected_live_hwnds.insert(hwnd);
}

fn focused_column_width(state: &AppState, hwnd: u64) -> i32 {
    let workspace = state
        .focused_workspace()
        .unwrap_or_else(|| panic!("hwnd {hwnd} should be on the focused workspace"));
    let (column, _) = workspace
        .find_window_location(hwnd)
        .unwrap_or_else(|| panic!("hwnd {hwnd} should occupy a column"));
    workspace.columns()[column].width()
}

fn assert_replacement_uses_default_column_width(state: &AppState, hwnd: u64) {
    let width = focused_column_width(state, hwnd);
    let expected = state
        .focused_workspace()
        .unwrap_or_else(|| panic!("hwnd {hwnd} should be on the focused workspace"))
        .default_column_width();
    assert_ne!(
        width, 400,
        "hwnd {hwnd} inherited a destroyed window's width"
    );
    assert_eq!(width, expected);
}

#[test]
fn recycled_managed_hwnd_destroyed_before_create_drops_old_lifetime() {
    let mut state = state();
    let old_token = admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    state.hidden_column_widths.insert(10, (Instant::now(), 400));
    backdate_admission(&mut state, 10);
    simulate_missing_managed_token(&mut state, 10);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 0);
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&state, 10);

    state.handle_window_event(WindowEvent::Created(10));

    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(membership_count(&state, 10), 1);
    let new_token = recorded_token(&state, 10);
    assert_ne!(new_token, old_token);
    assert_recycled_lifetime_caches_cleared(&state, 10);
    assert_replacement_uses_default_column_width(&state, 10);
}

#[test]
fn recycled_managed_hwnd_created_before_destroy_keeps_replacement() {
    let mut state = state();
    let old_token = admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    state.hidden_column_widths.insert(10, (Instant::now(), 400));
    state.elevation_blocked.insert(
        10,
        crate::state::ElevationBlockedRecord {
            title: "old".to_string(),
            reason: ManageBlock::HigherIntegrity,
        },
    );
    simulate_missing_managed_token(&mut state, 10);

    state.handle_window_event(WindowEvent::Created(10));

    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_recycled_lifetime_caches_cleared(&state, 10);
    assert!(!state.hidden_column_widths.contains_key(&10));
    assert_replacement_uses_default_column_width(&state, 10);
    assert!(!state.elevation_blocked.contains_key(&10));
    let new_token = recorded_token(&state, 10);
    assert_ne!(new_token, old_token);

    state
        .tab_title_overrides
        .insert(10, "replacement".to_string());
    state
        .last_placed_layout_rects
        .insert(10, Rect::new(5, 6, 7, 8));
    state.overview_icon_cache.insert(10, Some(0x5678));

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("replacement")
    );
    assert_eq!(
        state.last_placed_layout_rects.get(&10),
        Some(&Rect::new(5, 6, 7, 8))
    );
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x5678)));
    assert_eq!(state.managed_lifetime_tokens.get(&10), Some(&new_token));
}

#[test]
fn different_stamped_managed_token_is_a_replacement() {
    let mut destroyed = state();
    let old_token = admit(&mut destroyed, 10);
    seed_recycled_lifetime_caches(&mut destroyed, 10);
    destroyed.injected_live_hwnds.insert(10);
    destroyed
        .injected_managed_tokens
        .insert(10, old_token.wrapping_add(9));

    destroyed.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&destroyed, 10), 0);
    assert!(!destroyed.managed_lifetime_tokens.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&destroyed, 10);

    let mut created = state();
    let old_token = admit(&mut created, 10);
    seed_recycled_lifetime_caches(&mut created, 10);
    let stamped = old_token.wrapping_add(9);
    created.injected_live_hwnds.insert(10);
    created.injected_managed_tokens.insert(10, stamped);

    created.handle_window_event(WindowEvent::Created(10));

    assert_eq!(membership_count(&created, 10), 1);
    assert_recycled_lifetime_caches_cleared(&created, 10);
    let new_token = recorded_token(&created, 10);
    assert_ne!(new_token, old_token);
    assert_ne!(new_token, stamped);
}

#[test]
fn matching_managed_token_spurious_destroy_keeps_membership_and_caches() {
    let mut state = state();
    let token = admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    state.injected_live_hwnds.insert(10);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.managed_lifetime_tokens.get(&10), Some(&token));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x1234)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("old-title")
    );
    assert_eq!(
        state.last_placed_layout_rects.get(&10),
        Some(&Rect::new(1, 2, 3, 4))
    );
    assert_eq!(state.move_origins.get(&20).unwrap().sibling, Some(10));
}

#[test]
fn transient_managed_identity_read_keeps_membership() {
    let mut state = state();
    let token = admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    state.injected_identity_read_error =
        Some(IdentityReadError::Transient("identity api failed".into()));

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.managed_lifetime_tokens.get(&10), Some(&token));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x1234)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("old-title")
    );
}

#[test]
fn gone_managed_identity_read_clears_membership_and_record() {
    let mut state = state();
    admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    state.injected_identity_read_error = Some(IdentityReadError::Gone);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 0);
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(&state, 10);
}

#[test]
fn non_replacement_reads_do_not_retire_on_create() {
    let mut gone = state();
    let token = admit(&mut gone, 10);
    gone.injected_identity_read_error = Some(IdentityReadError::Gone);
    gone.handle_window_event(WindowEvent::Created(10));
    assert_eq!(gone.find_window_workspace(10), Some((1, 0)));
    assert_eq!(membership_count(&gone, 10), 1);
    assert_eq!(gone.managed_lifetime_tokens.get(&10), Some(&token));

    let mut transient = state();
    let token = admit(&mut transient, 10);
    transient.injected_identity_read_error =
        Some(IdentityReadError::Transient("identity api failed".into()));
    transient.handle_window_event(WindowEvent::Created(10));
    assert_eq!(transient.find_window_workspace(10), Some((1, 0)));
    assert_eq!(membership_count(&transient, 10), 1);
    assert_eq!(transient.managed_lifetime_tokens.get(&10), Some(&token));
}

#[test]
fn enumerate_records_unrecorded_already_managed_windows() {
    let mut state = managed_state();
    assert!(state.managed_lifetime_tokens.is_empty());
    state.injected_enumerated_windows = Some(vec![
        info(10, "Tiled", "TiledClass", 1010),
        info(40, "New", "NewClass", 1040),
    ]);

    let added = state.enumerate_and_add_windows().unwrap();

    assert_eq!(added, 1);
    assert_eq!(state.find_window_workspace(40), Some((1, 0)));
    let token_10 = recorded_token(&state, 10);
    let _token_40 = recorded_token(&state, 40);
    assert!(!state.managed_lifetime_tokens.contains_key(&20));
    assert!(!state.managed_lifetime_tokens.contains_key(&30));

    let added_again = state.enumerate_and_add_windows().unwrap();
    assert_eq!(added_again, 0);
    assert_eq!(state.managed_lifetime_tokens.get(&10), Some(&token_10));
    assert_eq!(membership_count(&state, 40), 1);
}

#[test]
fn readmit_records_managed_lifetime_independent_of_ignore_token() {
    let mut state = managed_state();
    set_foreground(&mut state, 10);
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.injected_lifetime_tokens.contains_key(&10));

    set_foreground(&mut state, 10);
    assert!(matches!(
        state.handle_command(IpcCommand::ToggleIgnore),
        IpcResponse::Ok
    ));

    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert!(!state.temporary_ignores.contains_key(&10));
    assert!(
        !state.injected_lifetime_tokens.contains_key(&10),
        "clearing the ignore property must not be required to keep the managed token"
    );
    let token = recorded_token(&state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    backdate_admission(&mut state, 10);
    simulate_missing_managed_token(&mut state, 10);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 0);
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    assert_ne!(
        state.injected_managed_tokens.get(&10).copied(),
        Some(token),
        "the departed property is already gone"
    );
    assert_recycled_lifetime_caches_cleared(&state, 10);

    state.handle_window_event(WindowEvent::Created(10));
    assert_eq!(membership_count(&state, 10), 1);
    assert_ne!(recorded_token(&state, 10), token);
}

#[test]
fn unrecorded_live_member_keeps_legacy_destroy_skip() {
    let mut state = managed_state();
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    state.injected_live_hwnds.insert(10);
    seed_recycled_lifetime_caches(&mut state, 10);

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    assert_eq!(membership_count(&state, 10), 1);
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    assert_eq!(state.overview_icon_cache.get(&10), Some(&Some(0x1234)));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("old-title")
    );
    assert_eq!(state.move_origins.get(&20).unwrap().sibling, Some(10));
}

#[test]
fn managed_lifetime_record_skips_drag_placeholder() {
    let mut state = state();
    state.record_managed_lifetime(DRAG_PLACEHOLDER_HWND);
    assert!(state.managed_lifetime_tokens.is_empty());
    assert!(state.injected_managed_tokens.is_empty());
}

fn ignore_managed_class(state: &mut AppState) {
    state.config.window_rules.push(crate::config::WindowRule {
        match_class: Some("ManagedClass".into()),
        action: crate::config::WindowAction::Ignore,
        ..Default::default()
    });
    state.compiled_rules = state.config.compile_window_rules();
}

fn enable_scripted_layout(state: &mut AppState) {
    state.paused = false;
    state.reduce_motion = true;
    // Admission while paused still starts a transition, and apply_layout
    // returns before the placement worker while one is active.
    state.layout_transition = None;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
}

fn placement_batches(state: &AppState) -> Vec<Vec<u64>> {
    state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .clone()
}

/// Peer 20 stays managed. Hwnd 10's managed property is already gone, and a
/// persistent Ignore rule makes the replacement fail admission.
fn replaced_ignored_peer_state() -> AppState {
    let mut state = state();
    admit(&mut state, 20);
    admit(&mut state, 10);
    seed_recycled_lifetime_caches(&mut state, 10);
    simulate_missing_managed_token(&mut state, 10);
    ignore_managed_class(&mut state);
    enable_scripted_layout(&mut state);
    state
}

fn assert_failed_replacement_departed(state: &AppState) {
    assert_eq!(membership_count(state, 10), 0);
    assert_eq!(state.find_window_workspace(20), Some((1, 0)));
    assert!(!state.managed_lifetime_tokens.contains_key(&10));
    assert_recycled_lifetime_caches_cleared(state, 10);
    assert!(state.drag_state.is_none());
    let batches = placement_batches(state);
    assert!(
        batches
            .iter()
            .any(|batch| batch.contains(&20) && !batch.contains(&10)),
        "peer 20 should be reflowed without the departed hwnd, got {batches:?}"
    );
}

#[test]
fn created_before_destroy_failed_admission_matches_destroy_then_create() {
    let mut created_first = replaced_ignored_peer_state();
    created_first.handle_window_event(WindowEvent::Created(10));
    assert_failed_replacement_departed(&created_first);

    let mut destroyed_first = replaced_ignored_peer_state();
    destroyed_first.handle_window_event(WindowEvent::Destroyed(10));
    destroyed_first.handle_window_event(WindowEvent::Created(10));
    assert_failed_replacement_departed(&destroyed_first);

    assert_eq!(
        placement_batches(&created_first),
        placement_batches(&destroyed_first)
    );
    assert_eq!(
        created_first.find_window_workspace(10),
        destroyed_first.find_window_workspace(10)
    );
    assert_eq!(
        created_first.managed_lifetime_tokens.contains_key(&10),
        destroyed_first.managed_lifetime_tokens.contains_key(&10)
    );
}

fn removed_tiled_drag(hwnd: u64) -> DragState {
    DragState {
        hwnd,
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
    }
}

#[test]
fn created_before_destroy_cancels_tiled_drag_source_and_keeps_replacement() {
    let mut state = state();
    let old_token = admit(&mut state, 10);
    state
        .focused_workspace_mut()
        .unwrap()
        .remove_window(10)
        .unwrap();
    assert!(state.find_window_workspace(10).is_none());
    assert!(state.managed_lifetime_tokens.contains_key(&10));
    state.drag_state = Some(removed_tiled_drag(10));
    simulate_missing_managed_token(&mut state, 10);

    state.handle_window_event(WindowEvent::Created(10));

    assert!(state.drag_state.is_none());
    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.find_window_workspace(10), Some((1, 0)));
    let new_token = recorded_token(&state, 10);
    assert_ne!(new_token, old_token);
    state
        .tab_title_overrides
        .insert(10, "replacement".to_string());

    state.handle_window_event(WindowEvent::Destroyed(10));

    assert_eq!(membership_count(&state, 10), 1);
    assert_eq!(state.managed_lifetime_tokens.get(&10), Some(&new_token));
    assert_eq!(
        state.tab_title_overrides.get(&10).map(String::as_str),
        Some("replacement")
    );
}
