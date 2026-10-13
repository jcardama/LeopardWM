use crate::config::{Config, NewWindowPlacement, WindowAction, WindowRule};
use crate::event_handler::AdmissionKind;
use crate::state::AppState;
use leopardwm_core_layout::Rect;
use leopardwm_platform_win32::{MonitorInfo, WindowEvent, WindowInfo};
use std::time::{Duration, Instant};

fn state() -> AppState {
    AppState::new_with_config(
        Config::default(),
        vec![
            MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".into(),
                scale_factor: 1.0,
            },
            MonitorInfo {
                id: 2,
                rect: Rect::new(1920, 0, 1920, 1080),
                work_area: Rect::new(1920, 0, 1920, 1040),
                is_primary: false,
                device_name: "DISPLAY2".into(),
                scale_factor: 1.0,
            },
        ],
    )
}

fn create(state: &mut AppState, hwnd: u64, pid: u32, class: &str) {
    inject(state, hwnd, pid, class);
    state.handle_window_event(WindowEvent::Created(hwnd, 0));
    assert!(state.find_window_workspace(hwnd).is_some());
}

fn inject(state: &mut AppState, hwnd: u64, pid: u32, class: &str) {
    state.injected_window_info.insert(
        hwnd,
        WindowInfo {
            hwnd,
            title: "App".into(),
            class_name: class.into(),
            process_id: pid,
            rect: Rect::new(100, 100, 800, 600),
            visible: true,
        },
    );
}

fn cycle(state: &mut AppState) {
    state.handle_system_suspend();
    state.recreated_window_slots.suspended_at = Some(Instant::now() - Duration::from_secs(2));
    state.handle_system_resume();
}

fn hide(state: &mut AppState, hwnd: u64) {
    state
        .window_managed_at
        .insert(hwnd, Instant::now() - Duration::from_secs(31));
    state.injected_window_info.remove(&hwnd);
    state.handle_window_event(WindowEvent::Hidden(hwnd, 1));
    assert!(state.find_window_workspace(hwnd).is_none());
}

fn tabbed_state() -> AppState {
    let mut state = state();
    create(&mut state, 20, 200, "OtherClass");
    state.config.behavior.new_window_placement = NewWindowPlacement::InColumn;
    create(&mut state, 10, 100, "AppClass");
    create(&mut state, 30, 300, "OtherClass");
    state.config.behavior.new_window_placement = NewWindowPlacement::default();
    let ws = state.focused_workspace_mut().unwrap();
    ws.focus_window(10).unwrap();
    ws.toggle_focused_column_tabbed_mode();
    state
}

#[test]
fn recreated_tab_rejoins_after_destroy_and_slot_is_single_use() {
    let mut state = tabbed_state();
    cycle(&mut state);
    hide(&mut state, 10);
    state.handle_window_event(WindowEvent::Destroyed(10));
    create(&mut state, 11, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 1);
    assert_eq!(ws.columns()[0].windows(), &[20, 11, 30]);
    assert_eq!(ws.focused_window(), Some(11));
    assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
    create(&mut state, 12, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 2);
    assert_eq!(ws.find_window_location(12), Some((1, 0)));
}

#[test]
fn recreated_single_window_restores_index_and_width_over_monitor_and_rules() {
    let mut state = state();
    create(&mut state, 20, 200, "OtherClass");
    create(&mut state, 10, 100, "AppClass");
    state
        .focused_workspace_mut()
        .unwrap()
        .resize_focused_column(173);
    let width = state.focused_workspace().unwrap().columns()[1].width();
    create(&mut state, 30, 300, "OtherClass");
    cycle(&mut state);
    hide(&mut state, 10);
    state
        .focused_workspace_mut()
        .unwrap()
        .focus_window(30)
        .unwrap();
    state.config.behavior.new_window_placement = NewWindowPlacement::InColumn;
    state
        .config
        .window_rules
        .push(rule(WindowAction::Tile, false));
    state.compiled_rules = state.config.compile_window_rules();
    inject(&mut state, 11, 100, "AppClass");
    state.injected_window_info.get_mut(&11).unwrap().rect.x = 2020;
    state.handle_window_event(WindowEvent::Created(11, 0));
    assert_eq!(state.find_window_workspace(11), Some((1, 0)));
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 3);
    assert_eq!(ws.find_window_location(11), Some((1, 0)));
    assert_eq!(ws.columns()[1].width(), width);
}

#[test]
fn recreated_background_tab_preserves_both_workspaces_focus() {
    let mut state = tabbed_state();
    state.ensure_workspace_exists(1, 1);
    state.active_workspace.insert(1, 1);
    create(&mut state, 40, 400, "OtherClass");
    state.previous_focused_hwnd = Some(40);
    cycle(&mut state);
    hide(&mut state, 10);
    let background_focus = state.workspaces[&1][0].focused_window();
    let active_tab = state.workspaces[&1][0].columns()[0]
        .active_tab_idx()
        .unwrap();
    let background_active = state.workspaces[&1][0].columns()[0].windows()[active_tab];
    let active_focus = state.focused_workspace().unwrap().focused_window();
    create(&mut state, 11, 100, "AppClass");
    assert_eq!(state.find_window_workspace(11), Some((1, 0)));
    assert_eq!(state.active_workspace_idx(1), 1);
    assert_eq!(state.focused_monitor, 1);
    assert_eq!(
        state.focused_workspace().unwrap().focused_window(),
        active_focus
    );
    assert_eq!(state.previous_focused_hwnd, Some(40));
    let ws = &state.workspaces[&1][0];
    assert_eq!(ws.columns()[0].windows(), &[20, 11, 30]);
    assert_eq!(ws.focused_window(), background_focus);
    assert_eq!(
        ws.columns()[0].windows()[ws.columns()[0].active_tab_idx().unwrap()],
        background_active
    );
}

#[test]
fn recreated_tab_follows_moved_sibling_without_taking_focus() {
    let mut state = tabbed_state();
    cycle(&mut state);
    hide(&mut state, 10);
    create(&mut state, 40, 400, "OtherClass");
    let ws = state.focused_workspace_mut().unwrap();
    ws.remove_window(20).unwrap();
    ws.insert_window_in_column_at(20, 1, 0).unwrap();
    ws.focus_window(40).unwrap();
    state.config.behavior.focus_new_windows = false;
    state.previous_focused_hwnd = Some(40);
    create(&mut state, 11, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.find_window_location(11), Some((1, 1)));
    assert_eq!(ws.columns()[1].windows(), &[20, 11, 40]);
    assert_eq!(ws.focused_window(), Some(40));
    assert_eq!(state.previous_focused_hwnd, Some(40));
}

#[test]
fn hide_before_resume_delivery_still_rejoins() {
    let mut state = tabbed_state();
    state.handle_system_suspend();
    hide(&mut state, 10);
    state.handle_system_resume();
    create(&mut state, 11, 100, "AppClass");
    assert_eq!(state.focused_workspace().unwrap().column_count(), 1);
    assert_eq!(
        state.focused_workspace().unwrap().columns()[0].windows(),
        &[20, 11, 30]
    );
}

#[test]
fn ineligible_recreations_keep_normal_new_column_admission() {
    for case in [
        "pid",
        "class",
        "destroyed",
        "expired",
        "concurrent_tiled",
        "concurrent_floating",
        "concurrent_stashed_scratchpad",
        "no_power",
        "before_suspend",
        "late_hide",
        "same_hwnd",
        "donor_reshow",
    ] {
        let mut state = tabbed_state();
        if case == "concurrent_stashed_scratchpad" {
            create(&mut state, 15, 100, "AppClass");
            state.scratchpad_stash();
            assert!(state
                .scratchpad
                .is_some_and(|pad| pad.window_id == 15 && !pad.shown));
            assert!(state.find_window_workspace(15).is_none());
        }
        if case != "no_power" && case != "before_suspend" {
            cycle(&mut state);
        }
        if case == "concurrent_tiled" || case == "concurrent_floating" {
            create(&mut state, 15, 100, "DifferentClass");
            if case == "concurrent_floating" {
                let ws = state.focused_workspace_mut().unwrap();
                ws.remove_window(15).unwrap();
                ws.add_floating(15, Rect::new(0, 0, 400, 300)).unwrap();
            }
        }
        if case == "destroyed" {
            state.injected_window_info.remove(&10);
            state.handle_window_event(WindowEvent::Destroyed(10));
        } else {
            hide(&mut state, 10);
        }
        if case == "expired" {
            let now = Instant::now();
            state.recreated_window_slots.suspended_at = Some(now - Duration::from_secs(65));
            state.recreated_window_slots.resumed_at = Some(now - Duration::from_secs(64));
            for slot in state.recreated_window_slots.slots.values_mut() {
                slot.hidden_at = now - Duration::from_secs(61);
            }
        }
        if case == "before_suspend" {
            state.handle_system_suspend();
            state.handle_system_resume();
        }
        if case == "late_hide" {
            state.recreated_window_slots.suspended_at =
                Some(Instant::now() - Duration::from_secs(124));
            state.recreated_window_slots.resumed_at =
                Some(Instant::now() - Duration::from_secs(123));
        }
        if case == "donor_reshow" {
            create(&mut state, 10, 100, "AppClass");
        }
        let hwnd = if case == "same_hwnd" { 10 } else { 11 };
        let pid = if case == "pid" { 101 } else { 100 };
        let class = if case == "class" {
            "ChangedClass"
        } else {
            "AppClass"
        };
        let ws = state.focused_workspace().unwrap();
        let count = ws.column_count();
        let expected_column = if count == 0 {
            0
        } else {
            ws.focused_column_index() + 1
        };
        create(&mut state, hwnd, pid, class);
        let ws = state.focused_workspace().unwrap();
        assert_eq!(ws.column_count(), count + 1, "{case}");
        assert_eq!(
            ws.find_window_location(hwnd),
            Some((expected_column, 0)),
            "{case}"
        );
    }
}

fn rule(action: WindowAction, sticky: bool) -> WindowRule {
    WindowRule {
        match_class: Some("^AppClass$".into()),
        match_title: None,
        match_executable: None,
        action,
        width: None,
        height: None,
        corner_style: None,
        open_on_workspace: Some(2),
        open_maximized: true,
        column_width: Some(0.2),
        open_in_column: Some(8),
        row: None,
        sticky,
        ..Default::default()
    }
}

#[test]
fn float_ignore_sticky_and_explicit_readmit_do_not_consume_slots() {
    for case in ["float", "ignore", "sticky", "readmit"] {
        let mut state = tabbed_state();
        cycle(&mut state);
        hide(&mut state, 10);
        let action = match case {
            "float" => WindowAction::Float,
            "ignore" => WindowAction::Ignore,
            _ => WindowAction::Tile,
        };
        state
            .config
            .window_rules
            .push(rule(action, case == "sticky"));
        state.compiled_rules = state.config.compile_window_rules();
        inject(&mut state, 11, 100, "AppClass");
        if case == "readmit" {
            state.try_admit_window(11, AdmissionKind::ExplicitReadmit);
        } else {
            state.handle_window_event(WindowEvent::Created(11, 0));
        }
        assert_eq!(state.recreated_window_slots.slots.len(), 1, "{case}");
        if case == "ignore" {
            assert_eq!(state.find_window_workspace(11), None);
        } else if case == "float" {
            let (monitor, workspace) = state.find_window_workspace(11).unwrap();
            assert!(state.workspaces[&monitor][workspace].is_floating(11));
        } else {
            assert_eq!(
                state.focused_workspace().unwrap().column_count(),
                2,
                "{case}"
            );
        }
    }
}

#[test]
fn destroyed_sibling_cannot_redirect_restore_to_recycled_hwnd() {
    let mut state = tabbed_state();
    cycle(&mut state);
    hide(&mut state, 10);
    state.injected_window_info.remove(&20);
    state.handle_window_event(WindowEvent::Destroyed(20));
    create(&mut state, 40, 400, "OtherClass");
    create(&mut state, 20, 500, "UnrelatedClass");
    create(&mut state, 11, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 4);
    assert_eq!(ws.find_window_location(11), Some((0, 0)));
    assert_eq!(ws.find_window_location(20), Some((3, 0)));
}

#[test]
fn missing_slot_monitor_or_workspace_uses_normal_admission() {
    for case in ["monitor", "workspace"] {
        let mut state = state();
        state.ensure_workspace_exists(1, 1);
        state.active_workspace.insert(1, 1);
        create(&mut state, 10, 100, "AppClass");
        cycle(&mut state);
        hide(&mut state, 10);
        state.active_workspace.insert(1, 0);
        if case == "monitor" {
            state.monitors.remove(&1);
            state.workspaces.remove(&1);
            state.focused_monitor = 2;
        } else {
            state.workspaces.get_mut(&1).unwrap().truncate(1);
        }
        inject(&mut state, 20, 200, "OtherClass");
        inject(&mut state, 11, 100, "AppClass");
        if case == "monitor" {
            state.injected_window_info.get_mut(&20).unwrap().rect.x = 2020;
            state.injected_window_info.get_mut(&11).unwrap().rect.x = 2020;
        }
        state.handle_window_event(WindowEvent::Created(20, 0));
        state.handle_window_event(WindowEvent::Created(11, 0));
        let target_monitor = if case == "monitor" { 2 } else { 1 };
        assert_eq!(
            state.find_window_workspace(11),
            Some((target_monitor, 0)),
            "{case}"
        );
        let ws = state.focused_workspace().unwrap();
        assert_eq!(ws.column_count(), 2, "{case}");
        assert_eq!(ws.find_window_location(11), Some((1, 0)), "{case}");
    }
}

#[test]
fn background_rejoin_focus_guard_is_bounded() {
    for delay in [600, 48] {
        let mut state = tabbed_state();
        state.ensure_workspace_exists(1, 1);
        state.active_workspace.insert(1, 1);
        create(&mut state, 40, 400, "OtherClass");
        state.previous_focused_hwnd = Some(40);
        cycle(&mut state);
        hide(&mut state, 10);
        let background_focus = state.workspaces[&1][0].focused_window();
        let background_column = state.workspaces[&1][0].focused_column_index();
        let active_tab = state.workspaces[&1][0].columns()[0]
            .active_tab_idx()
            .unwrap();
        let background_tab = state.workspaces[&1][0].columns()[0].windows()[active_tab];
        inject(&mut state, 11, 100, "AppClass");
        state.handle_window_event(WindowEvent::Created(11, 1000));
        state.injected_foreground_hwnd = Some(Some(11));
        state.injected_foreground_is_valid = Some(true);
        state.last_prune_at = Some(Instant::now());
        state.handle_window_event(WindowEvent::Focused(11, 1000 + delay));
        if delay == 48 {
            assert_eq!(state.active_workspace_idx(1), 1);
            assert_eq!(state.focused_monitor, 1);
            assert_eq!(state.workspaces[&1][1].focused_window(), Some(40));
            assert_eq!(state.previous_focused_hwnd, Some(40));
            let ws = &state.workspaces[&1][0];
            assert_eq!(ws.focused_column_index(), background_column);
            assert_eq!(ws.focused_window(), background_focus);
            assert_eq!(
                ws.columns()[0].windows()[ws.columns()[0].active_tab_idx().unwrap()],
                background_tab
            );
            assert!(
                state
                    .foreground_release_requests
                    .iter()
                    .any(|&(hwnd, _)| hwnd == 11),
                "parked native foreground must be released"
            );
        } else {
            assert_eq!(state.active_workspace_idx(1), 0);
            assert_eq!(state.workspaces[&1][0].focused_window(), Some(11));
            assert_eq!(state.previous_focused_hwnd, Some(11));
        }
    }
}

#[test]
fn hidden_sibling_cannot_redirect_restore_to_recycled_hwnd() {
    let mut state = tabbed_state();
    cycle(&mut state);
    hide(&mut state, 10);
    hide(&mut state, 20);
    create(&mut state, 40, 400, "OtherClass");
    create(&mut state, 20, 500, "UnrelatedClass");
    create(&mut state, 11, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 4);
    assert_eq!(ws.find_window_location(11), Some((0, 0)));
    assert_eq!(ws.find_window_location(20), Some((3, 0)));
}

#[test]
fn background_rejoin_activation_reconciles_selected_minimized_window() {
    let mut state = tabbed_state();
    state.ensure_workspace_exists(1, 1);
    state.active_workspace.insert(1, 1);
    create(&mut state, 40, 400, "OtherClass");
    state.previous_focused_hwnd = Some(40);
    cycle(&mut state);
    hide(&mut state, 10);
    inject(&mut state, 11, 100, "AppClass");
    state.handle_window_event(WindowEvent::Created(11, 1000));
    let background_focus = state.workspaces[&1][0].focused_window();
    let active_tab = state.workspaces[&1][0].columns()[0].active_tab_idx();
    state.injected_foreground_hwnd = Some(Some(11));
    state.injected_foreground_is_valid = Some(true);
    state.last_prune_at = Some(Instant::now());
    state.injected_iconic_hwnds.insert(40);
    state.handle_window_event(WindowEvent::Focused(11, 1048));
    assert!(state.workspaces[&1][1].is_minimized(40));
    assert_eq!(state.active_workspace_idx(1), 1);
    assert_eq!(state.previous_focused_hwnd, None);
    assert_eq!(state.workspaces[&1][0].focused_window(), background_focus);
    assert_eq!(
        state.workspaces[&1][0].columns()[0].active_tab_idx(),
        active_tab
    );
    assert!(state
        .foreground_release_requests
        .iter()
        .any(|&(hwnd, _)| hwnd == 11));
}

#[test]
fn pruned_sibling_cannot_redirect_restore_to_recycled_hwnd() {
    let mut state = tabbed_state();
    cycle(&mut state);
    hide(&mut state, 10);
    state.injected_window_info.remove(&20);
    state.injected_stale_hwnds.push(20);
    state.prune_stale_windows();
    assert!(state.find_window_workspace(20).is_none());
    create(&mut state, 40, 400, "OtherClass");
    create(&mut state, 20, 500, "UnrelatedClass");
    create(&mut state, 11, 100, "AppClass");
    let ws = state.focused_workspace().unwrap();
    assert_eq!(ws.column_count(), 4);
    assert_eq!(ws.find_window_location(11), Some((0, 0)));
    assert_eq!(ws.find_window_location(20), Some((3, 0)));
}
