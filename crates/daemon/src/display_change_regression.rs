use super::*;
use leopardwm_core_layout::{Visibility, WindowPlacement};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetPropW,
    IsZoomed, PostThreadMessageW, SetPropW, SetWindowLongPtrW, ShowWindow, GWLP_WNDPROC, MSG,
    SWP_NOSIZE, SW_HIDE, SW_SHOWMAXIMIZED, WINDOWPOS, WM_QUIT, WM_WINDOWPOSCHANGING,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *const u16,
        mode: *const std::ffi::c_void,
        flags: u32,
        access: u32,
        attributes: *const std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn GetThreadDesktop(thread: u32) -> *mut std::ffi::c_void;
    fn SetThreadDesktop(desktop: *mut std::ffi::c_void) -> i32;
    fn CloseDesktop(desktop: *mut std::ffi::c_void) -> i32;
}

pub(crate) struct FixtureDesktop {
    original: usize,
    desktop: usize,
}

impl FixtureDesktop {
    pub(crate) fn enter() -> Self {
        static NEXT_DESKTOP_ID: AtomicU64 = AtomicU64::new(0);
        let name: Vec<u16> = format!(
            "LeopardWM-placement-test-{}-{}",
            std::process::id(),
            NEXT_DESKTOP_ID.fetch_add(1, Ordering::Relaxed)
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        unsafe {
            let original = GetThreadDesktop(GetCurrentThreadId());
            let desktop = CreateDesktopW(
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0x10000000,
                std::ptr::null(),
            );
            assert!(
                !desktop.is_null(),
                "could not create isolated fixture desktop"
            );
            assert_ne!(
                SetThreadDesktop(desktop),
                0,
                "could not enter fixture desktop"
            );
            Self {
                original: original as usize,
                desktop: desktop as usize,
            }
        }
    }
}

impl Drop for FixtureDesktop {
    fn drop(&mut self) {
        unsafe {
            assert_ne!(SetThreadDesktop(self.original as *mut _), 0);
            assert_ne!(CloseDesktop(self.desktop as *mut _), 0);
        }
    }
}

unsafe extern "system" fn placement_owner_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == windows::Win32::UI::WindowsAndMessaging::WM_APP + 42 {
        let _ = SetPropW(
            hwnd,
            w!("LeopardWMFixtureQueueDrained"),
            Some(HANDLE(std::ptr::dangling_mut())),
        );
        return LRESULT(0);
    }
    if message == windows::Win32::UI::WindowsAndMessaging::WM_APP + 43 {
        windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            None,
            wparam.0 as i32,
            lparam.0 as i32,
            0,
            0,
            SWP_NOSIZE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        )
        .unwrap();
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        (*(control as *const MoveControl))
            .sent_messages_only
            .store(true, Ordering::SeqCst);
        return LRESULT(0);
    }
    if message == WM_WINDOWPOSCHANGING {
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        if !control.is_null() {
            let control = &*(control as *const MoveControl);
            let delay = control.move_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                thread::sleep(Duration::from_millis(delay));
            }
            if control.block_next.swap(false, Ordering::SeqCst) {
                let released = control.released.lock().unwrap();
                control.entered.store(true, Ordering::SeqCst);
                let (_released, _timeout) = control
                    .wake
                    .wait_timeout_while(
                        released,
                        Duration::from_millis(
                            control.hold_timeout_ms.load(Ordering::SeqCst).max(5000),
                        ),
                        |released| !*released,
                    )
                    .unwrap();
            }
        }
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        if !position.flags.contains(SWP_NOSIZE) {
            let min_width = GetPropW(hwnd, w!("LeopardWMFixtureMinWidth")).0 as usize as i32;
            position.cx = position.cx.max(min_width);
        }
    }
    let result = DefWindowProcW(hwnd, message, wparam, lparam);
    if message == windows::Win32::UI::WindowsAndMessaging::WM_STYLECHANGED {
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        if !control.is_null() {
            let control = &*(control as *const MoveControl);
            if control
                .block_after_style_change
                .swap(false, Ordering::SeqCst)
            {
                control.style_change_seen.store(true, Ordering::SeqCst);
                // Let SetWindowLongW return while this owner can no longer answer probes.
                let _ = windows::Win32::UI::WindowsAndMessaging::ReplyMessage(result);
                let released = control.released.lock().unwrap();
                control.entered.store(true, Ordering::SeqCst);
                let (_released, _timeout) = control
                    .wake
                    .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                    .unwrap();
            }
        }
    }
    if message == windows::Win32::UI::WindowsAndMessaging::WM_WINDOWPOSCHANGED {
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        if !control.is_null() {
            let control = &*(control as *const MoveControl);
            let position = &*(lparam.0 as *const WINDOWPOS);
            if position
                .flags
                .contains(windows::Win32::UI::WindowsAndMessaging::SWP_FRAMECHANGED)
                && control.style_change_seen.load(Ordering::SeqCst)
            {
                control.frame_change_seen.store(true, Ordering::SeqCst);
            }
            control.moves.lock().unwrap().push((position.x, position.y));
            if control.pause_after_move.swap(false, Ordering::SeqCst) {
                control.pause_pump.store(true, Ordering::SeqCst);
            }
        }
    }
    result
}

#[derive(Default)]
struct MoveControl {
    block_next: AtomicBool,
    move_delay_ms: AtomicU64,
    hold_timeout_ms: AtomicU64,
    entered: AtomicBool,
    pause_pump: AtomicBool,
    pause_after_move: AtomicBool,
    block_after_style_change: AtomicBool,
    style_change_seen: AtomicBool,
    frame_change_seen: AtomicBool,
    pump_paused: AtomicBool,
    sent_messages_only: AtomicBool,
    moves: Mutex<Vec<(i32, i32)>>,
    released: Mutex<bool>,
    wake: Condvar,
}

impl MoveControl {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
        self.pause_pump.store(false, Ordering::SeqCst);
        self.sent_messages_only.store(false, Ordering::SeqCst);
    }
}

struct PlacementOwner {
    window_id: u64,
    thread_id: u32,
    release: mpsc::Sender<()>,
    pumping: mpsc::Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
    control: Arc<MoveControl>,
}

impl PlacementOwner {
    fn spawn() -> Self {
        Self::spawn_at(Rect::new(48, 48, 160, 120))
    }

    fn spawn_at(rect: Rect) -> Self {
        Self::spawn_with(rect, 0, false)
    }

    fn spawn_with(rect: Rect, min_width: i32, parked_maximized: bool) -> Self {
        Self::spawn_with_message_mode(rect, min_width, parked_maximized, false)
    }

    fn spawn_with_message_mode(
        rect: Rect,
        min_width: i32,
        parked_maximized: bool,
        sent_messages_only: bool,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (pumping_tx, pumping) = mpsc::channel();
        let control = Arc::new(MoveControl::default());
        let owner_control = Arc::clone(&control);
        let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
        let join = thread::spawn(move || unsafe {
            assert_ne!(SetThreadDesktop(desktop as *mut _), 0);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("display-change placement fixture"),
                if parked_maximized {
                    WS_OVERLAPPEDWINDOW
                } else {
                    WS_POPUP
                },
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            SetPropW(
                hwnd,
                w!("LeopardWMFixtureMoveControl"),
                Some(HANDLE(Arc::as_ptr(&owner_control) as *mut _)),
            )
            .unwrap();
            if min_width > 0 || sent_messages_only {
                SetPropW(
                    hwnd,
                    w!("LeopardWMFixtureMinWidth"),
                    Some(HANDLE(min_width as usize as *mut _)),
                )
                .unwrap();
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_WNDPROC,
                    placement_owner_proc as *const () as isize,
                );
            }
            if parked_maximized {
                let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                if !IsZoomed(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
                leopardwm_platform_win32::park_window_for_placement(hwnd.0 as usize as u64)
                    .unwrap();
                assert!(IsZoomed(hwnd).as_bool());
            }
            ready_tx
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            if sent_messages_only {
                let deadline = Instant::now() + Duration::from_secs(20);
                while resume.try_recv().is_err() && Instant::now() < deadline {
                    if owner_control.pause_pump.load(Ordering::SeqCst) {
                        owner_control.pump_paused.store(true, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    owner_control.pump_paused.store(false, Ordering::SeqCst);
                    let mut message = MSG::default();
                    let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                        &mut message,
                        None,
                        WM_QUIT,
                        WM_QUIT,
                        windows::Win32::UI::WindowsAndMessaging::PEEK_MESSAGE_REMOVE_TYPE(
                            windows::Win32::UI::WindowsAndMessaging::QS_SENDMESSAGE.0 << 16,
                        ),
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            } else {
                let _ = resume.recv_timeout(Duration::from_secs(20));
            }
            pumping_tx.send(()).unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
                let deadline = Instant::now() + Duration::from_secs(5);
                while owner_control.sent_messages_only.load(Ordering::SeqCst)
                    && Instant::now() < deadline
                {
                    let mut sent = MSG::default();
                    let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                        &mut sent,
                        None,
                        WM_QUIT,
                        WM_QUIT,
                        windows::Win32::UI::WindowsAndMessaging::PEEK_MESSAGE_REMOVE_TYPE(
                            windows::Win32::UI::WindowsAndMessaging::QS_SENDMESSAGE.0 << 16,
                        ),
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
            DestroyWindow(hwnd).unwrap();
        });
        let (window_id, thread_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            window_id,
            thread_id,
            release,
            pumping,
            join: Some(join),
            control,
        }
    }

    fn resume(&self) {
        self.release.send(()).unwrap();
        self.pumping.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

impl Drop for PlacementOwner {
    fn drop(&mut self) {
        self.control.release();
        leopardwm_platform_win32::clear_suspected_oversize(self.window_id);
        let _ = self.release.send(());
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}

fn placement_state(window_ids: &[u64]) -> AppState {
    let monitors = vec![MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 800, 600),
        work_area: Rect::new(0, 0, 800, 560),
        is_primary: true,
        device_name: "DISPLAY1".to_string(),
        scale_factor: 1.0,
    }];
    let mut config = test_config();
    config.appearance.active_border = false;
    config.animation.layout_duration_ms = 0;
    config.animation.scroll_duration_ms = 0;
    let mut state = AppState::new_with_config(config, monitors.clone());
    state.reduce_motion = true;
    state.paused = false;
    state.injected_display_monitors = Some(monitors);
    for &hwnd in window_ids {
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(hwnd, Some(320))
            .unwrap();
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
    }
    state
}

fn pending_landing_recheck_state(owner: &PlacementOwner) -> (AppState, Vec<WindowPlacement>) {
    let mut state = placement_state(&[owner.window_id]);
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let placements = state.apply_physical_projection(placements);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert!(!landing.landings[0].failed);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(request_id, invalidation_id, &landing.landings, &[]);
    assert!(state.deferred_physical_landings.contains(&owner.window_id));
    (state, placements)
}

fn wait_for_fixture(mut ready: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

fn classify_non_draining_owner(owner: &PlacementOwner) -> (AppState, Vec<WindowPlacement>) {
    let (state, placements) = pending_landing_recheck_state(owner);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2),
    ));
    (state, placements)
}

#[test]
fn test_completed_mover_landing_is_measured_and_confirmed_without_another_move() {
    if run_in_placement_test_process("tests::display_change_regression::test_completed_mover_landing_is_measured_and_confirmed_without_another_move") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, placements) = classify_non_draining_owner(&owner);
    let placements = state.apply_physical_projection(placements);
    let queued = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    let (request, invalidation) = state.physical_request_ids();
    state.consume_physical_landings(request, invalidation, &queued.landings, &[]);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(!state.last_physical_presentations[&owner.window_id].confirmed);
    let moves = owner.control.moves.lock().unwrap().len();
    state.moved_or_resized_suppression.clear();
    assert!(
        state.recheck_deferred_window_moves(),
        "exact deferred landing was ignored instead of finalized"
    );
    assert!(
        state.last_physical_presentations[&owner.window_id].confirmed,
        "completed mover landing remained unconfirmed"
    );
    assert!(!state.deferred_physical_landings.contains(&owner.window_id));
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    assert_eq!(
        owner.control.moves.lock().unwrap().len(),
        moves,
        "finalization submitted another native move"
    );
}

#[test]
fn test_completed_mover_minimum_width_reports_direct_landing_feedback() {
    if run_in_placement_test_process("tests::display_change_regression::test_completed_mover_minimum_width_reports_direct_landing_feedback") { return; }
    let marked =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 600, false, true);
    let (state, placements) = classify_non_draining_owner(&marked);
    leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, false)
        .unwrap();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(marked.window_id),
        Duration::from_secs(3)
    ));
    let moves = marked.control.moves.lock().unwrap().len();
    let measured = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(
        !measured.landings[0].measurement_deferred,
        "completed minimum-width landing was not measured"
    );
    assert_eq!(
        marked.control.moves.lock().unwrap().len(),
        moves,
        "minimum-width measurement submitted another native move"
    );
    let direct = PlacementOwner::spawn_with(Rect::new(48, 48, 160, 120), 600, false);
    direct.resume();
    let mut direct_placements = placements.clone();
    direct_placements[0].window_id = direct.window_id;
    let direct_result = leopardwm_platform_win32::apply_placements(
        &direct_placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert_eq!(measured.width_violations.len(), 1);
    assert_eq!(measured.width_violations[0].min_width, 600);
    assert_eq!(measured.width_violations[0].window_id, marked.window_id);
    assert_eq!(direct_result.width_violations.len(), 1);
    assert_eq!(direct_result.width_violations[0].min_width, 600);
}

fn inactive_workspace_move_state(owner: &PlacementOwner) -> AppState {
    let mut state = placement_state(&[]);
    let inactive = state.workspaces[&1][0].clone();
    state.workspaces.get_mut(&1).unwrap().push(inactive);
    state.workspaces.get_mut(&1).unwrap()[1]
        .insert_window(owner.window_id, Some(320))
        .unwrap();
    state
        .injected_window_info
        .insert(owner.window_id, make_test_window_info(owner.window_id));
    state.injected_native_offscreen_enabled = true;
    state
}

fn notify_fixture_move(state: &mut AppState, owner: &PlacementOwner) {
    state.on_window_moved_or_resized_with_native_ops(
        owner.window_id,
        false,
        || None,
        |_| panic!("inactive fixture must not request a maximized restore"),
        |_| false,
    );
}

fn fixture_is_at_sentinel(owner: &PlacementOwner) -> bool {
    leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
        .is_some_and(|rect| leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect))
}

#[test]
fn test_inactive_workspace_escape_is_parked_without_active_snapback() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_inactive_workspace_escape_is_parked_without_active_snapback",
    ) {
        return;
    }
    let _desktop = FixtureDesktop::enter();
    let owner = PlacementOwner::spawn_at(Rect::new(0, 0, 160, 120));
    owner.resume();
    let mut state = inactive_workspace_move_state(&owner);
    let cached_rect = Rect::new(400, 48, 160, 120);
    state
        .last_placed_layout_rects
        .insert(owner.window_id, cached_rect);
    let request_seq = state.physical_request_seq;

    notify_fixture_move(&mut state, &owner);

    assert_eq!(
        state.inactive_workspace_park_requests,
        vec![owner.window_id]
    );
    assert!(wait_for_fixture(
        || fixture_is_at_sentinel(&owner),
        Duration::from_secs(2)
    ));
    assert_eq!(
        state.physical_request_seq, request_seq,
        "inactive escape applied active layout"
    );
    assert_eq!(
        state.last_placed_layout_rects.get(&owner.window_id),
        Some(&cached_rect)
    );
}

#[test]
fn test_inactive_workspace_sentinel_move_does_not_park_again() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_inactive_workspace_sentinel_move_does_not_park_again",
    ) {
        return;
    }
    let _desktop = FixtureDesktop::enter();
    let owner = PlacementOwner::spawn();
    owner.resume();
    leopardwm_platform_win32::queue_window_offscreen(owner.window_id).unwrap();
    assert!(wait_for_fixture(
        || fixture_is_at_sentinel(&owner),
        Duration::from_secs(2)
    ));
    let mut state = inactive_workspace_move_state(&owner);
    let request_seq = state.physical_request_seq;

    for _ in 0..8 {
        notify_fixture_move(&mut state, &owner);
    }

    assert!(state.inactive_workspace_park_requests.is_empty());
    assert_eq!(state.physical_request_seq, request_seq);
    assert!(fixture_is_at_sentinel(&owner));
}

#[test]
fn test_inactive_workspace_escape_replays_after_layout_transition() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_inactive_workspace_escape_replays_after_layout_transition",
    ) {
        return;
    }
    let _desktop = FixtureDesktop::enter();
    let owner = PlacementOwner::spawn_at(Rect::new(0, 0, 160, 120));
    owner.resume();
    let mut state = inactive_workspace_move_state(&owner);
    state.layout_transition = Some(crate::state::LayoutTransition {
        start_rects: Default::default(),
        exit_rects: Default::default(),
        exit_provenance: Default::default(),
        elapsed_ms: 0,
        duration_ms: 150,
        easing: Default::default(),
        ghosted_wids: Default::default(),
        suppress_landing_focus_resync: false,
        defer_focus_border: false,
    });
    let request_seq = state.physical_request_seq;
    assert_eq!(state.current_physical_visibility(owner.window_id), None);

    notify_fixture_move(&mut state, &owner);
    assert!(state.deferred_moved_or_resized.contains(&owner.window_id));
    assert!(state.inactive_workspace_park_requests.is_empty());
    assert!(!fixture_is_at_sentinel(&owner));
    state.recheck_deferred_window_moves();
    assert!(state.deferred_moved_or_resized.contains(&owner.window_id));
    assert!(state.inactive_workspace_park_requests.is_empty());

    state.layout_transition = None;
    state.recheck_deferred_window_moves();

    assert!(!state.deferred_moved_or_resized.contains(&owner.window_id));
    assert_eq!(
        state.inactive_workspace_park_requests,
        vec![owner.window_id]
    );
    assert!(wait_for_fixture(
        || fixture_is_at_sentinel(&owner),
        Duration::from_secs(2)
    ));
    assert_eq!(state.physical_request_seq, request_seq);
}

#[test]
fn test_workspace_park_supersedes_queued_marked_landing() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_workspace_park_supersedes_queued_marked_landing",
    ) {
        return;
    }
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut barrier_placements) = classify_non_draining_owner(&barrier);
    let (_, mut placements) = classify_non_draining_owner(&owner);
    barrier.control.block_next.store(true, Ordering::SeqCst);
    barrier_placements[0].rect.x += 40;
    leopardwm_platform_win32::apply_placements(
        &barrier_placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || barrier.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    placements[0].rect.x += 70;
    leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, false)
        .unwrap();
    leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
    barrier.control.release();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id)
            && !leopardwm_platform_win32::pending_async_position(barrier.window_id),
        Duration::from_secs(3)
    ));
    let parked = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap();
    assert!(
        leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&parked),
        "older queued landing restored parked window: {parked:?}"
    );
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
}

fn parked_owner_release_reaches_target(cascade: bool) {
    let _desktop = FixtureDesktop::enter();
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    classify_non_draining_owner(&owner);
    let before_park = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap();
    leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap()
    ));
    let target = if cascade {
        let work_area = leopardwm_platform_win32::get_primary_monitor()
            .unwrap()
            .work_area;
        let height = (work_area.height as f64 * 0.5) as i32;
        leopardwm_platform_win32::cascade_windows(&[owner.window_id]).unwrap();
        Rect::new(
            work_area.x,
            work_area.y,
            (height as f64 * 1.33) as i32,
            height,
        )
    } else {
        let work_area = leopardwm_platform_win32::get_primary_monitor()
            .unwrap()
            .work_area;
        let target = Rect::new(
            work_area.x,
            work_area.y,
            before_park.width,
            before_park.height,
        );
        assert_eq!(
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[owner.window_id]).unwrap(),
            1
        );
        target
    };
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(target)
    );
    assert!(!leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &target
    ));
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
}

#[test]
fn test_marked_park_reaches_recovery_target_without_posted_drain() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_marked_park_reaches_recovery_target_without_posted_drain",
    ) {
        return;
    }
    parked_owner_release_reaches_target(false);
}

#[test]
fn test_marked_park_reaches_cascade_target_without_posted_drain() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_marked_park_reaches_cascade_target_without_posted_drain",
    ) {
        return;
    }
    parked_owner_release_reaches_target(true);
}

#[test]
fn test_marked_owner_recovers_twice_without_draining_posted_moves() {
    if run_in_placement_test_process("tests::display_change_regression::test_marked_owner_recovers_twice_without_draining_posted_moves") { return; }
    let _desktop = FixtureDesktop::enter();
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, placements) = classify_non_draining_owner(&owner);
    let work_area = leopardwm_platform_win32::get_primary_monitor()
        .unwrap()
        .work_area;
    let restored = Rect::new(
        work_area.x,
        work_area.y,
        placements[0].rect.width,
        placements[0].rect.height,
    );
    for cycle in 0..2 {
        leopardwm_platform_win32::apply_placements(
            &placements,
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
        assert!(wait_for_fixture(
            || !leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(3)
        ));
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            Some(placements[0].rect)
        );
        leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
        assert!(wait_for_fixture(
            || !leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(3)
        ));
        let started = Instant::now();
        assert_eq!(
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[owner.window_id]).unwrap(),
            1
        );
        assert!(
            started.elapsed() < Duration::from_millis(75),
            "cycle {cycle} recovery blocked: {:?}",
            started.elapsed()
        );
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            Some(restored),
            "cycle {cycle} recovery waited for posted-message drain"
        );
        assert!(!leopardwm_platform_win32::pending_async_position(
            owner.window_id
        ));
    }
}

#[test]
fn test_visibility_recovery_uncloaks_unmarked_owner_before_marked_move_blocks() {
    if run_in_placement_test_process("tests::display_change_regression::test_visibility_recovery_uncloaks_unmarked_owner_before_marked_move_blocks") { return; }
    let _desktop = FixtureDesktop::enter();
    let marked =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let unmarked =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    classify_non_draining_owner(&marked);
    leopardwm_platform_win32::move_window_offscreen(marked.window_id).unwrap();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(marked.window_id),
        Duration::from_secs(3)
    ));
    leopardwm_platform_win32::park_window_for_placement(unmarked.window_id).unwrap();
    assert!(leopardwm_platform_win32::is_placement_cloaked(
        unmarked.window_id
    ));
    marked.control.block_next.store(true, Ordering::SeqCst);
    let ids = [marked.window_id, unmarked.window_id];
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
    let (done_tx, done_rx) = mpsc::channel();
    let recovery = thread::spawn(move || {
        assert_ne!(unsafe { SetThreadDesktop(desktop as *mut _) }, 0);
        crate::state::run_visibility_recovery_pass(&ids, "placement test");
        done_tx.send(()).unwrap();
    });
    let blocked = wait_for_fixture(
        || marked.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2),
    );
    let unmarked_restored = leopardwm_platform_win32::get_window_chrome_rect(unmarked.window_id)
        .is_some_and(|rect| !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect));
    let unmarked_uncloaked = !leopardwm_platform_win32::is_placement_cloaked(unmarked.window_id);
    let still_running = done_rx.try_recv().is_err();
    marked.control.release();
    let completed = done_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    recovery.join().unwrap();
    assert!(
        blocked,
        "marked owner did not block inside the native move after answering the probe"
    );
    assert!(
        still_running,
        "recovery finished before the blocked-call observation"
    );
    assert!(
        completed,
        "recovery did not finish after releasing its owned move gate"
    );
    assert!(
        unmarked_restored,
        "unmarked owner remained parked while marked move blocked"
    );
    assert!(
        unmarked_uncloaked,
        "unmarked owner remained cloaked while marked move blocked"
    );
}

#[test]
fn test_emergency_recovery_skips_hung_marked_owner_after_unmarked_work() {
    if run_in_placement_test_process("tests::display_change_regression::test_emergency_recovery_skips_hung_marked_owner_after_unmarked_work") { return; }
    let _desktop = FixtureDesktop::enter();
    let hung = PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let unmarked =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let responsive =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    classify_non_draining_owner(&hung);
    classify_non_draining_owner(&responsive);
    for owner in [&hung, &unmarked, &responsive] {
        leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
        assert!(wait_for_fixture(
            || !leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(3)
        ));
    }
    unmarked.resume();
    hung.control.pause_pump.store(true, Ordering::SeqCst);
    assert!(wait_for_fixture(
        || hung.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let ids = [hung.window_id, unmarked.window_id, responsive.window_id];
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
    let (done_tx, done_rx) = mpsc::channel();
    let started = Instant::now();
    let recovery = thread::spawn(move || {
        assert_ne!(unsafe { SetThreadDesktop(desktop as *mut _) }, 0);
        leopardwm_platform_win32::emergency_restore_windows(
            &ids,
            Instant::now() + Duration::from_secs(2),
        );
        done_tx.send(()).unwrap();
    });
    let returned = done_rx.recv_timeout(Duration::from_millis(800)).is_ok();
    let elapsed = started.elapsed();
    let unmarked_restored = wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(unmarked.window_id).is_some_and(
                |rect| !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect),
            )
        },
        Duration::from_millis(100),
    );
    let responsive_restored =
        leopardwm_platform_win32::get_window_chrome_rect(responsive.window_id)
            .is_some_and(|rect| !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect));
    let hung_skipped = leopardwm_platform_win32::get_window_chrome_rect(hung.window_id)
        .is_some_and(|rect| leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect));
    hung.control.release();
    recovery.join().unwrap();
    assert!(returned, "emergency recovery blocked on hung marked owner for {elapsed:?}; unmarked restored={unmarked_restored}");
    assert!(unmarked_restored, "unmarked parked owner was not restored");
    assert!(
        responsive_restored,
        "responsive marked owner was not restored"
    );
    assert!(hung_skipped, "hung marked owner was not skipped");
}

#[test]
fn test_emergency_recovery_continues_after_marked_move_blocks() {
    if run_in_placement_test_process("tests::display_change_regression::test_emergency_recovery_continues_after_marked_move_blocks") { return; }
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, GWL_STYLE, WS_MAXIMIZEBOX,
    };
    let _desktop = FixtureDesktop::enter();
    let first =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let second =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    for owner in [&first, &second] {
        classify_non_draining_owner(owner);
        leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
        assert!(wait_for_fixture(
            || !leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(3)
        ));
        assert!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).is_some_and(|rect| {
                leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect)
            })
        );
    }
    let second_hwnd = HWND(second.window_id as usize as *mut _);
    unsafe {
        let style = GetWindowLongW(second_hwnd, GWL_STYLE);
        SetWindowLongW(second_hwnd, GWL_STYLE, style | WS_MAXIMIZEBOX.0 as i32);
    }
    assert!(leopardwm_platform_win32::remove_maximizebox(second.window_id).unwrap());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(3)
    ));
    assert_eq!(
        unsafe { GetWindowLongW(second_hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX.0 as i32,
        0
    );
    first.control.block_next.store(true, Ordering::SeqCst);
    second.control.move_delay_ms.store(300, Ordering::SeqCst);
    let ids = [first.window_id, second.window_id];
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
    let (done_tx, done_rx) = mpsc::channel();
    let deadline = Instant::now() + Duration::from_secs(2);
    let recovery = thread::spawn(move || {
        assert_ne!(unsafe { SetThreadDesktop(desktop as *mut _) }, 0);
        leopardwm_platform_win32::emergency_restore_windows(&ids, deadline);
        let finished = Instant::now();
        let second_landed = leopardwm_platform_win32::get_window_chrome_rect(ids[1])
            .is_some_and(|rect| !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect));
        let maximize_restored =
            unsafe { GetWindowLongW(HWND(ids[1] as usize as *mut _), GWL_STYLE) }
                & WS_MAXIMIZEBOX.0 as i32
                != 0;
        let _ = done_tx.send((finished, second_landed, maximize_restored));
    });
    let blocked = wait_for_fixture(
        || first.control.entered.load(Ordering::SeqCst),
        deadline.saturating_duration_since(Instant::now()),
    );
    let second_restored = wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(second.window_id).is_some_and(|rect| {
                !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect)
            })
        },
        deadline.saturating_duration_since(Instant::now()),
    );
    let return_limit = deadline + Duration::from_millis(200);
    let completion = done_rx
        .recv_timeout(return_limit.saturating_duration_since(Instant::now()))
        .ok();
    let returned = completion.is_some_and(|(finished, _, _)| finished < return_limit);
    let second_landed_at_return = completion.is_some_and(|(_, landed, _)| landed);
    let maximize_restored_at_return = completion.is_some_and(|(_, _, restored)| restored);
    let first_still_parked = leopardwm_platform_win32::get_window_chrome_rect(first.window_id)
        .is_some_and(|rect| leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect));
    first.control.release();
    let cleaned_up = wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(first.window_id).is_some_and(|rect| {
                !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect)
            })
        },
        Duration::from_secs(2),
    );
    let completed = completion.is_some() || done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    if completed {
        recovery.join().unwrap();
    }
    assert!(
        blocked,
        "first marked owner did not block inside the native move"
    );
    assert!(
        second_restored,
        "second marked window was not restored before the deadline while the first move blocked"
    );
    assert!(
        second_landed_at_return,
        "slow marked owner was still off screen when recovery returned"
    );
    assert!(
        maximize_restored_at_return,
        "slow marked owner still lacked its maximize button when recovery returned"
    );
    assert!(
        returned,
        "emergency recovery exceeded its deadline plus scheduling margin"
    );
    assert!(
        first_still_parked,
        "blocked first owner completed before release"
    );
    assert!(
        cleaned_up,
        "first owner did not recover after releasing its move gate"
    );
    assert!(
        completed,
        "recovery did not finish after releasing the first owner"
    );
}

#[test]
fn test_emergency_recovery_refreshes_frame_after_marked_owner_stops_answering() {
    if run_in_placement_test_process("tests::display_change_regression::test_emergency_recovery_refreshes_frame_after_marked_owner_stops_answering") { return; }
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, GWL_STYLE, WS_MAXIMIZEBOX,
    };
    let _desktop = FixtureDesktop::enter();
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    classify_non_draining_owner(&owner);
    owner.resume();
    let hwnd = HWND(owner.window_id as usize as *mut _);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 42,
            WPARAM(0),
            LPARAM(0),
        )
        .unwrap();
    }
    assert!(wait_for_fixture(
        || !unsafe { GetPropW(hwnd, w!("LeopardWMFixtureQueueDrained")) }.is_invalid(),
        Duration::from_secs(2),
    ));
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        SetWindowLongW(hwnd, GWL_STYLE, style | WS_MAXIMIZEBOX.0 as i32);
    }
    assert!(leopardwm_platform_win32::remove_maximizebox(owner.window_id).unwrap());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(3)
    ));
    assert_eq!(
        unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX.0 as i32,
        0
    );
    owner
        .control
        .block_after_style_change
        .store(true, Ordering::SeqCst);
    let window_id = owner.window_id;
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
    let (done_tx, done_rx) = mpsc::channel();
    let recovery = thread::spawn(move || {
        assert_ne!(unsafe { SetThreadDesktop(desktop as *mut _) }, 0);
        leopardwm_platform_win32::emergency_restore_windows(
            &[window_id],
            Instant::now() + Duration::from_secs(2),
        );
        done_tx.send(()).unwrap();
    });
    let blocked_after_style = wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(3),
    );
    let returned = done_rx.recv_timeout(Duration::from_millis(800)).is_ok();
    let style_restored = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX.0 as i32 != 0;
    owner.control.release();
    let frame_refreshed = wait_for_fixture(
        || owner.control.frame_change_seen.load(Ordering::SeqCst),
        Duration::from_secs(2),
    );
    if returned {
        recovery.join().unwrap();
    }
    assert!(
        blocked_after_style,
        "owner did not stop answering after the style change"
    );
    assert!(returned, "emergency recovery blocked on the paused owner");
    assert!(style_restored, "maximize-box style was not restored");
    assert!(
        frame_refreshed,
        "no frame refresh after the marked owner resumed"
    );
}

#[test]
fn test_workspace_park_of_slow_marked_owner_returns_promptly() {
    if run_in_placement_test_process("tests::display_change_regression::test_workspace_park_of_slow_marked_owner_returns_promptly") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    classify_non_draining_owner(&owner);
    owner.control.move_delay_ms.store(200, Ordering::SeqCst);
    let started = Instant::now();
    leopardwm_platform_win32::move_window_offscreen(owner.window_id).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(75),
        "workspace park blocked on native owner: {:?}",
        started.elapsed()
    );
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap()
    ));
}

#[test]
fn test_marked_mover_serves_both_slow_owners_during_continuous_frames() {
    if run_in_placement_test_process("tests::display_change_regression::test_marked_mover_serves_both_slow_owners_during_continuous_frames") { return; }
    let first =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let second =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&first);
    let (_, second_placements) = classify_non_draining_owner(&second);
    placements.extend(second_placements);
    for owner in [&first, &second] {
        owner.control.moves.lock().unwrap().clear();
        owner.control.move_delay_ms.store(40, Ordering::SeqCst);
    }
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        for placement in &mut placements {
            placement.rect.x += 1;
        }
        leopardwm_platform_win32::apply_placements(
            &placements,
            &state.platform_config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(4));
    }
    for owner in [&first, &second] {
        owner.control.move_delay_ms.store(0, Ordering::SeqCst);
        assert!(
            owner.control.moves.lock().unwrap().len() >= 3,
            "one slow owner was starved of intermediate frames"
        );
    }
}

#[test]
fn test_visibility_recovery_stops_pending_and_late_mover_frames() {
    if run_in_placement_test_process("tests::display_change_regression::test_visibility_recovery_stops_pending_and_late_mover_frames") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap();
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    placements[0].visibility = Visibility::OffScreenLeft;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    placements[0].rect = Rect::new(240, 48, 160, 120);
    placements[0].visibility = Visibility::Visible;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    let started = Instant::now();
    leopardwm_platform_win32::dwm_uncloak_all();
    leopardwm_platform_win32::restore_windows_moved_offscreen(&[owner.window_id]).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "recovery waited for a held mover call"
    );
    cache.prepare_frame();
    leopardwm_platform_win32::dwm_uncloak_all();
    placements[0].rect.x = 360;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    owner.control.release();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id) == Some(before),
        Duration::from_secs(2)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(before),
        "a delayed mover frame undid visibility recovery"
    );
    let moves = owner.control.moves.lock().unwrap();
    assert!(
        !moves.contains(&(240, 48)),
        "pending frame survived recovery"
    );
    assert!(
        !moves.contains(&(360, 48)),
        "mover accepted a frame prepared before recovery"
    );
    drop(moves);
    placements[0].rect.x = 480;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect),
            Duration::from_secs(2),
        ),
        "marked owner did not move after recovery resumed"
    );
}

#[test]
fn test_frame_prepared_before_landing_cannot_move_after_landing() {
    if run_in_placement_test_process("tests::display_change_regression::test_frame_prepared_before_landing_cannot_move_after_landing") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let fresh =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut fresh_placements) = classify_non_draining_owner(&fresh);
    fresh_placements[0].rect.x += 300;
    let mut stale_cache = leopardwm_platform_win32::PlacementCache::new();
    stale_cache.prepare_frame();
    let mut stale = placements.clone();
    stale[0].rect.x += 100;
    placements[0].rect.x += 200;
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &stale,
        &state.platform_config,
        Some(&mut stale_cache),
        false,
    )
    .unwrap();
    // A fresh owner's delivered frame fences the FIFO without a sleep-based absence assertion.
    let mut fresh_cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &fresh_placements,
        &state.platform_config,
        Some(&mut fresh_cache),
        false,
    )
    .unwrap();
    let delivered = wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(fresh.window_id)
                == Some(fresh_placements[0].rect)
        },
        Duration::from_secs(2),
    );
    let stale_entered = owner.control.entered.load(Ordering::SeqCst);
    owner.control.release();
    assert!(
        !stale_entered,
        "a frame prepared before the seal entered the mover"
    );
    assert!(delivered);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_unrelated_landing_preserves_pending_marked_frame_and_parked_record() {
    if run_in_placement_test_process("tests::display_change_regression::test_unrelated_landing_preserves_pending_marked_frame_and_parked_record") { return; }
    let blocker =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let parked = PlacementOwner::spawn();
    parked.resume();
    let (state, mut blocking) = classify_non_draining_owner(&blocker);
    let (_, mut pending) = classify_non_draining_owner(&owner);
    let mut parked_placement = pending.clone();
    parked_placement[0].window_id = parked.window_id;
    parked_placement[0].rect = Rect::new(
        leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD,
        0,
        0,
        0,
    );
    parked_placement[0].visibility = Visibility::OffScreenLeft;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &parked_placement,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::offscreen_placement_matches(parked.window_id) == Some(true),
        Duration::from_secs(2)
    ));
    blocking[0].rect.x += 100;
    blocker.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &blocking,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || blocker.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    pending[0].rect.x += 200;
    leopardwm_platform_win32::apply_placements(
        &pending,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    cache.prepare_frame();
    leopardwm_platform_win32::apply_placements(&blocking, &state.platform_config, None, true)
        .unwrap();
    leopardwm_platform_win32::apply_placements(
        &parked_placement,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    cache.prepare_frame();
    assert_eq!(
        leopardwm_platform_win32::offscreen_placement_matches(parked.window_id),
        Some(true)
    );
    blocker.control.release();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(pending[0].rect),
            Duration::from_secs(2)
        ),
        "unrelated landing discarded the pending marked frame"
    );
}

#[test]
fn test_late_cancelled_move_skips_undo_when_owner_stops_answering() {
    if run_in_placement_test_process("tests::display_change_regression::test_late_cancelled_move_skips_undo_when_owner_stops_answering") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut barrier_placements) = classify_non_draining_owner(&barrier);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    owner.control.pause_after_move.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    leopardwm_platform_win32::stop_placement_mover();
    owner.control.release();
    assert!(wait_for_fixture(
        || owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    barrier_placements[0].rect.x += 200;
    leopardwm_platform_win32::apply_placements(
        &barrier_placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
                == Some(barrier_placements[0].rect),
            Duration::from_secs(2)
        ),
        "unresponsive recovery undo blocked the mover"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_cascade_accepts_in_flight_mover_and_recovers_late_offscreen_move() {
    if run_in_placement_test_process("tests::display_change_regression::test_cascade_accepts_in_flight_mover_and_recovers_late_offscreen_move") { return; }
    let _desktop = FixtureDesktop::enter();
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let work_area = leopardwm_platform_win32::get_primary_monitor()
        .unwrap()
        .work_area;
    let cascade_height = (work_area.height as f64 * 0.5) as i32;
    let cascade_rect = Rect::new(
        work_area.x,
        work_area.y,
        (cascade_height as f64 * 1.33) as i32,
        cascade_height,
    );
    placements[0].rect.x = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    placements[0].visibility = Visibility::OffScreenLeft;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let control = Arc::clone(&owner.control);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        control.release();
    });
    let result = leopardwm_platform_win32::cascade_windows(&[owner.window_id]);
    release.join().unwrap();
    result.unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(cascade_rect),
            Duration::from_secs(2)
        ),
        "late mover frame did not converge on the cascade rectangle"
    );
}

fn cascade_probe_retry_case(resume: bool) {
    let _desktop = FixtureDesktop::enter();
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut barrier_placements) = classify_non_draining_owner(&barrier);
    let work_area = leopardwm_platform_win32::get_primary_monitor()
        .unwrap()
        .work_area;
    let height = (work_area.height as f64 * 0.5) as i32;
    let cascade_rect = Rect::new(
        work_area.x,
        work_area.y,
        (height as f64 * 1.33) as i32,
        height,
    );
    placements[0].rect.x = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    placements[0].visibility = Visibility::OffScreenLeft;
    owner.control.hold_timeout_ms.store(10000, Ordering::SeqCst);
    owner.control.block_next.store(true, Ordering::SeqCst);
    owner.control.pause_after_move.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    leopardwm_platform_win32::cascade_windows(&[owner.window_id]).unwrap();
    owner.control.release();
    assert!(wait_for_fixture(
        || owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    for _ in 0..2 {
        barrier_placements[0].rect.x += 100;
        leopardwm_platform_win32::apply_placements(
            &barrier_placements,
            &state.platform_config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        assert!(
            wait_for_fixture(
                || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
                    == Some(barrier_placements[0].rect),
                Duration::from_secs(1)
            ),
            "correction retry blocked another owner"
        );
    }
    if resume {
        owner.control.pause_pump.store(false, Ordering::SeqCst);
        assert!(
            wait_for_fixture(
                || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                    == Some(cascade_rect),
                Duration::from_secs(3)
            ),
            "cascade landing was discarded after a missed correction probe"
        );
    } else {
        assert!(
            leopardwm_platform_win32::pending_async_position(owner.window_id),
            "correction was dropped on its first missed probe"
        );
        let stalled = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
        let moves = owner.control.moves.lock().unwrap().len();
        assert!(
            wait_for_fixture(
                || !leopardwm_platform_win32::pending_async_position(owner.window_id),
                Duration::from_secs(3)
            ),
            "correction did not expire within its retry bound"
        );
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            stalled
        );
        assert_eq!(
            owner.control.moves.lock().unwrap().len(),
            moves,
            "non-answering owner was moved"
        );
        owner.control.pause_pump.store(false, Ordering::SeqCst);
        barrier_placements[0].rect.x += 100;
        leopardwm_platform_win32::apply_placements(
            &barrier_placements,
            &state.platform_config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        assert!(wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
                == Some(barrier_placements[0].rect),
            Duration::from_secs(1)
        ));
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            stalled,
            "expired correction ran after owner resumed"
        );
    }
}

#[test]
fn test_cascade_recovery_leaves_other_desktop_windows_parked() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_cascade_recovery_leaves_other_desktop_windows_parked",
    ) {
        return;
    }
    let _user_desktop = FixtureDesktop::enter();
    let bystander = PlacementOwner::spawn();
    bystander.resume();
    leopardwm_platform_win32::move_window_offscreen(bystander.window_id).unwrap();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(bystander.window_id)
            && fixture_is_at_sentinel(&bystander),
        Duration::from_secs(3)
    ));

    cascade_probe_retry_case(true);

    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(bystander.window_id),
        Duration::from_secs(3)
    ));
    assert!(
        fixture_is_at_sentinel(&bystander),
        "cascade recovery moved a window outside its fixture desktop"
    );
}

#[test]
fn test_cascade_correction_retries_after_missed_probe() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_cascade_correction_retries_after_missed_probe",
    ) {
        return;
    }
    cascade_probe_retry_case(true);
}

#[test]
fn test_cascade_correction_expires_without_blocking_other_owners() {
    if run_in_placement_test_process("tests::display_change_regression::test_cascade_correction_expires_without_blocking_other_owners") { return; }
    cascade_probe_retry_case(false);
}

#[test]
fn test_slow_landing_corrections_share_fifo_with_other_owner_frames() {
    if run_in_placement_test_process("tests::display_change_regression::test_slow_landing_corrections_share_fifo_with_other_owner_frames") { return; }
    let first =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let second =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut landings) = classify_non_draining_owner(&first);
    let (_, mut frames) = classify_non_draining_owner(&second);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    first.control.block_next.store(true, Ordering::SeqCst);
    landings[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(
        &landings,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || first.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    landings[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(&landings, &state.platform_config, None, true)
        .unwrap();
    first.control.move_delay_ms.store(100, Ordering::SeqCst);
    second.control.moves.lock().unwrap().clear();
    let config = state.platform_config.clone();
    let churn = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            landings[0].rect.x += 1;
            leopardwm_platform_win32::apply_placements(&landings, &config, None, true).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });
    first.control.release();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        frames[0].rect.x += 1;
        leopardwm_platform_win32::apply_placements(
            &frames,
            &state.platform_config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(4));
    }
    let served = second.control.moves.lock().unwrap().len();
    churn.join().unwrap();
    first.control.move_delay_ms.store(0, Ordering::SeqCst);
    assert!(
        served >= 3,
        "slow correction churn starved the other owner's frames: {served}"
    );
}

#[test]
fn test_newer_landing_supersedes_cancelled_in_flight_move() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_newer_landing_supersedes_cancelled_in_flight_move",
    ) {
        return;
    }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut barrier_placements) = classify_non_draining_owner(&barrier);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    leopardwm_platform_win32::stop_placement_mover();
    placements[0].rect.x += 200;
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    owner.control.release();
    barrier_placements[0].rect.x += 300;
    leopardwm_platform_win32::apply_placements(
        &barrier_placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
            == Some(barrier_placements[0].rect),
        Duration::from_secs(2)
    ));
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(3)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect),
        "cancel-only undo overwrote the newer authoritative landing"
    );
}

#[test]
fn test_slow_marked_owner_never_blocks_initial_apply_frames_or_landing() {
    if run_in_placement_test_process("tests::display_change_regression::test_slow_marked_owner_never_blocks_initial_apply_frames_or_landing") { return; }
    let slow = PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&slow);
    let normal = PlacementOwner::spawn();
    normal.resume();
    let mut normal_placement = placements[0].clone();
    normal_placement.window_id = normal.window_id;
    placements.push(normal_placement);
    slow.control.move_delay_ms.store(150, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    for phase in 0..3 {
        for placement in &mut placements {
            placement.rect.x += 100;
        }
        let started = Instant::now();
        let result = leopardwm_platform_win32::apply_placements(
            &placements,
            &state.platform_config,
            (phase == 1).then_some(&mut cache),
            phase == 2,
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(75),
            "phase {phase} apply blocked on the marked owner: {:?}",
            started.elapsed()
        );
        assert!(
            wait_for_fixture(
                || leopardwm_platform_win32::get_window_chrome_rect(normal.window_id)
                    == Some(placements[1].rect),
                Duration::from_millis(50)
            ),
            "phase {phase} normal window was delayed by the marked owner"
        );
        if phase != 1 {
            assert!(
                result
                    .landings
                    .iter()
                    .find(|landing| landing.window_id == slow.window_id)
                    .unwrap()
                    .measurement_deferred
            );
        }
    }
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(slow.window_id),
        Duration::from_secs(3)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(slow.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_non_draining_owner_receives_animation_frames_without_blocking_caller() {
    if run_in_placement_test_process("tests::display_change_regression::test_non_draining_owner_receives_animation_frames_without_blocking_caller") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let started = Instant::now();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(
        wait_for_fixture(
            || owner.control.entered.load(Ordering::SeqCst),
            Duration::from_secs(2)
        ),
        "animation frame never reached the non-draining owner"
    );
    placements[0].rect.x += 100;
    let superseded_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    placements[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    owner.control.release();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2)
    ));
    assert!(
        !owner
            .control
            .moves
            .lock()
            .unwrap()
            .contains(&superseded_origin),
        "superseded pending frame was applied"
    );
}

#[test]
fn test_marked_owner_lands_without_latest_async_drain_wait() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_marked_owner_lands_without_latest_async_drain_wait",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect)
                && !leopardwm_platform_win32::pending_async_position(owner.window_id)
        },
        Duration::from_millis(150),
    );
    placements[0].rect.x += 100;
    let started = Instant::now();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(
        landing.landings[0].measurement_deferred,
        "marked landing must not move on the apply thread"
    );
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_millis(200)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
}

#[test]
fn test_marked_landing_seals_stale_frame_and_defers_in_flight_move() {
    if run_in_placement_test_process("tests::display_change_regression::test_marked_landing_seals_stale_frame_and_defers_in_flight_move") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || owner.control.entered.load(Ordering::SeqCst),
            Duration::from_secs(2)
        ),
        "frame did not enter the synchronous mover"
    );
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    placements[0].rect.x += 100;
    let stale_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    placements[0].rect.x -= 200;
    let started = Instant::now();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "landing waited for the mover"
    );
    assert!(landing.landings[0].measurement_deferred);
    let (request, invalidation) = state.physical_request_ids();
    state.consume_physical_landings(request, invalidation, &landing.landings, &[]);
    owner.control.release();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    let moves = owner.control.moves.lock().unwrap().len();
    state.moved_or_resized_suppression.clear();
    assert!(state.recheck_deferred_window_moves());
    assert!(state.last_physical_presentations[&owner.window_id].confirmed);
    assert_eq!(
        owner.control.moves.lock().unwrap().len(),
        moves,
        "finalizing a mover-completed landing must not position it again"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(
        !owner.control.moves.lock().unwrap().contains(&stale_origin),
        "sealed frame was applied after landing"
    );
    assert!(!state.deferred_physical_landings.contains(&owner.window_id));
}

#[test]
fn test_non_draining_mover_drops_targets_when_owner_stops_answering() {
    if run_in_placement_test_process("tests::display_change_regression::test_non_draining_mover_drops_targets_when_owner_stops_answering") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    owner.control.pause_pump.store(true, Ordering::SeqCst);
    assert!(wait_for_fixture(
        || owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let hwnd = HWND(owner.window_id as usize as *mut _);
    assert!(wait_for_fixture(
        || unsafe {
            windows::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_NULL,
                WPARAM(0),
                LPARAM(0),
                windows::Win32::UI::WindowsAndMessaging::SMTO_ABORTIFHUNG
                    | windows::Win32::UI::WindowsAndMessaging::SMTO_BLOCK,
                10,
                None,
            )
            .0 == 0
        },
        Duration::from_secs(1)
    ));
    // Let the successful owner-answer cache expire before submitting this frame.
    let expires = Instant::now() + Duration::from_millis(260);
    assert!(wait_for_fixture(
        || Instant::now() >= expires,
        Duration::from_secs(1)
    ));
    owner.control.block_next.store(true, Ordering::SeqCst);
    placements[0].rect.x += 100;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(2)
        ),
        "mover never started the unanswered owner probe"
    );
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    owner.control.pause_pump.store(false, Ordering::SeqCst);
    assert!(wait_for_fixture(
        || !owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let barrier = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_NULL,
            WPARAM(0),
            LPARAM(0),
            windows::Win32::UI::WindowsAndMessaging::SMTO_ABORTIFHUNG
                | windows::Win32::UI::WindowsAndMessaging::SMTO_BLOCK,
            100,
            None,
        )
        .0
    };
    assert_ne!(barrier, 0, "mover blocked on a non-answering owner");
    assert!(!owner.control.entered.load(Ordering::SeqCst));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        before
    );
}

#[test]
fn test_managed_departure_forgets_queued_move_fallback_and_pending_frame() {
    if run_in_placement_test_process("tests::display_change_regression::test_managed_departure_forgets_queued_move_fallback_and_pending_frame") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x += 100;
    let first_target = placements[0].rect;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    placements[0].rect.x += 100;
    let forgotten_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    state.take_managed_lifetime_token(owner.window_id);
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "forgetting must not hide an already executing call"
    );
    owner.control.release();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(first_target)
    );
    assert!(!owner
        .control
        .moves
        .lock()
        .unwrap()
        .contains(&forgotten_origin));
    placements[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "new lifetime inherited the queued-move fallback"
    );
    let landing = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(first_target)
    );
}

#[test]
fn test_unmarked_owner_keeps_async_animation_delivery() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_unmarked_owner_keeps_async_animation_delivery",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner = PlacementOwner::spawn();
    let state = placement_state(&[owner.window_id]);
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let started = Instant::now();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        before
    );
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    owner.resume();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2)
    ));
}

#[test]
fn test_self_moving_owner_keeps_async_animation_delivery() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_self_moving_owner_keeps_async_animation_delivery",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _desktop = FixtureDesktop::enter();
    let owner = PlacementOwner::spawn_with(Rect::new(48, 48, 160, 120), 1, false);
    let state = placement_state(&[owner.window_id]);
    let mut placements =
        state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    owner.resume();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2),
    ));
    let moved = Rect::new(90, 90, placements[0].rect.width, placements[0].rect.height);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(owner.window_id as usize as *mut _)),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 43,
            WPARAM(moved.x as usize),
            LPARAM(moved.y as isize),
        )
        .unwrap();
    }
    assert!(wait_for_fixture(
        || owner.control.sent_messages_only.load(Ordering::SeqCst)
            && leopardwm_platform_win32::get_window_chrome_rect(owner.window_id) == Some(moved),
        Duration::from_secs(2),
    ));
    thread::sleep(Duration::from_millis(300));
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    placements[0].rect.x += 80;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        !wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id) != Some(moved),
            Duration::from_millis(400),
        ),
        "self-moving owner was marked and received a synchronous mover placement"
    );
    owner
        .control
        .sent_messages_only
        .store(false, Ordering::SeqCst);
    unsafe {
        PostThreadMessageW(
            owner.thread_id,
            windows::Win32::UI::WindowsAndMessaging::WM_NULL,
            WPARAM(0),
            LPARAM(0),
        )
        .unwrap();
    }
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2),
    ));
}

#[test]
fn test_deferred_replay_survives_a_pending_landing() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = PlacementOwner::spawn();
    let (mut state, _) = pending_landing_recheck_state(&owner);
    state.deferred_moved_or_resized.insert(owner.window_id);
    state.applying_layout = true;
    let before = state.physical_request_seq;
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
    state.applying_layout = false;
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(!state.window_move_recheck_attempts[&owner.window_id].gave_up);
}

#[test]
fn test_deferred_landing_provenance_survives_suppression_and_another_correction() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let pending = PlacementOwner::spawn();
    let correction = OffscreenResizeOwner::new();
    let (mut state, _) = pending_landing_recheck_state(&pending);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(correction.hwnd, Some(300))
        .unwrap();
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let placements = state.apply_physical_projection(placements);
    let landings: Vec<_> = placements
        .iter()
        .map(|placement| leopardwm_platform_win32::PlacementLanding {
            window_id: placement.window_id,
            requested_rect: placement.rect,
            requested_visibility: placement.visibility,
            actual_visible_rect: None,
            actual_outer_rect: None,
            failed: false,
            unreadable: false,
            measurement_deferred: true,
        })
        .collect();
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(request_id, invalidation_id, &landings, &[]);
    state.applying_layout = true;
    let before = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
    assert!(!state.deferred_moved_or_resized.contains(&correction.hwnd));
    assert!(!state.deferred_moved_or_resized.contains(&pending.window_id));
    assert!(state.deferred_physical_landings.contains(&correction.hwnd));
    assert!(state
        .deferred_physical_landings
        .contains(&pending.window_id));
    state.applying_layout = false;
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        pending.window_id
    ));
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(
        correction.rect(),
        placements
            .iter()
            .find(|placement| placement.window_id == correction.hwnd)
            .unwrap()
            .rect
    );
    assert!(!state.deferred_moved_or_resized.contains(&pending.window_id));
    assert!(state
        .deferred_physical_landings
        .contains(&pending.window_id));
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        pending.window_id
    ));
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(!state
        .window_move_recheck_attempts
        .contains_key(&pending.window_id));
}

#[test]
fn test_deferred_landing_corrects_responsive_non_draining_owner_promptly() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, placements) = pending_landing_recheck_state(&owner);
    let landing_at = Instant::now();
    let before = state.physical_request_seq;
    let mut corrections = 0;
    while landing_at.elapsed() < Duration::from_secs(2) {
        state.moved_or_resized_suppression.clear();
        if state.recheck_deferred_window_moves() {
            corrections += 1;
            break;
        }
        assert_eq!(state.physical_request_seq, before);
        assert!(!state
            .window_move_recheck_attempts
            .contains_key(&owner.window_id));
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        corrections, 1,
        "responsive owner was not corrected within 2 s"
    );
    assert!(landing_at.elapsed() < Duration::from_secs(1));
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_millis(200)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    let moves = owner.control.moves.lock().unwrap().len();
    state.moved_or_resized_suppression.clear();
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 2);
    assert!(state.last_physical_presentations[&owner.window_id].confirmed);
    assert_eq!(owner.control.moves.lock().unwrap().len(), moves);
    state.moved_or_resized_suppression.clear();
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 2);
}

#[test]
fn test_deferred_landing_waits_for_latest_async_submission() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = pending_landing_recheck_state(&owner);
    thread::sleep(Duration::from_millis(350));
    placements[0].rect.x += 100;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "a fresh frame must remain pending even when the first frame is old"
    );
    let landing = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert_ne!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_active_tiling_recheck_corrects_pre_mark_moves_after_posted_drain() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, _) = pending_landing_recheck_state(&owner);
    thread::sleep(Duration::from_millis(350));
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    state.monitors.get_mut(&1).unwrap().work_area.x += 100;
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let placements = state.apply_physical_projection(placements);
    let correction = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(correction.landings[0].measurement_deferred);
    let (request, invalidation) = state.physical_request_ids();
    state.consume_physical_landings(request, invalidation, &correction.landings, &[]);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_millis(200)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    let hwnd = HWND(owner.window_id as usize as *mut _);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 42,
            WPARAM(0),
            LPARAM(0),
        )
        .unwrap();
    }
    owner.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe {
        GetPropW(hwnd, w!("LeopardWMFixtureQueueDrained"))
            .0
            .is_null()
    } {
        assert!(
            Instant::now() < deadline,
            "owner did not drain its posted queue"
        );
        thread::sleep(Duration::from_millis(10));
    }
    state.moved_or_resized_suppression.clear();
    state.deferred_moved_or_resized.insert(owner.window_id);
    assert!(state.recheck_deferred_window_moves());
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect),
            Duration::from_secs(2),
        ),
        "deferred recheck did not correct late pre-mark posted moves"
    );
}

#[test]
fn test_deferred_landing_keeps_blocked_owner_pending_after_short_wait() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = PlacementOwner::spawn();
    let (mut state, _) = pending_landing_recheck_state(&owner);
    let before = state.physical_request_seq;
    let original_rect = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    thread::sleep(Duration::from_millis(350));
    for _ in 0..3 {
        assert!(leopardwm_platform_win32::pending_async_position(
            owner.window_id
        ));
        state.moved_or_resized_suppression.clear();
        assert!(!state.recheck_deferred_window_moves());
        assert_eq!(state.physical_request_seq, before);
        assert!(!state
            .window_move_recheck_attempts
            .contains_key(&owner.window_id));
        assert!(state.deferred_physical_landings.contains(&owner.window_id));
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            original_rect
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn test_display_change_places_responsive_windows_while_owner_is_not_pumping() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let responsive = PlacementOwner::spawn();
    responsive.resume();
    let mut state = placement_state(&[blocked.window_id, responsive.window_id]);
    let before = leopardwm_platform_win32::get_window_visible_rect(blocked.window_id).unwrap();
    let start = Instant::now();
    state.handle_window_event(WindowEvent::DisplayChange);
    if let Some(retry) = state.display_change_apply_retry.as_ref() {
        let generation = retry.generation;
        let _ = state.run_display_change_apply_retry(generation);
    }
    let remained_active = !state.paused && state.pending_apply_workers.is_empty();
    let elapsed = start.elapsed();
    if !remained_active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        remained_active,
        "display reconciliation paused behind a non-pumping owner"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "display apply waited on owner: {elapsed:?}"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id).unwrap(),
        before
    );
    let expected: HashMap<_, _> = state.workspaces[&1][0]
        .compute_placements_animated(state.layout_viewport(1))
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(responsive.window_id),
        Some(expected[&responsive.window_id])
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);

    // Outlive both the production apply/retry interval and the ordinary async expiry.
    thread::sleep(Duration::from_millis(10_100).saturating_sub(start.elapsed()));
    state.apply_layout().unwrap();
    assert!(!state.paused);
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
        != Some(expected[&blocked.window_id])
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(expected[&blocked.window_id])
    );
    state.apply_layout().unwrap();
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    state.apply_layout().unwrap();
    assert!(state.last_physical_presentations[&blocked.window_id].confirmed);
    assert!(!state.paused);
}

#[test]
fn test_display_change_retry_defers_non_pumping_owner() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let responsive = PlacementOwner::spawn();
    responsive.resume();
    let mut state = placement_state(&[blocked.window_id, responsive.window_id]);
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(50),
    ));
    state.handle_window_event(WindowEvent::DisplayChange);
    let generation = pending_display_change_retry_generation(&state);
    for worker in state.pending_apply_workers.drain(..) {
        worker.join().unwrap();
    }
    state.injected_apply_placements_behavior = None;
    state.layout_apply_timeout = Duration::from_secs(5);
    let start = Instant::now();
    let result = state.run_display_change_apply_retry(generation);
    let elapsed = start.elapsed();
    let remained_active = result.is_ok() && !state.paused && state.pending_apply_workers.is_empty();
    if !remained_active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        remained_active,
        "display-change retry blocked or paused: {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "retry waited on owner: {elapsed:?}"
    );
    let expected = state.workspaces[&1][0]
        .compute_placements_animated(state.layout_viewport(1))
        .into_iter()
        .find(|placement| placement.window_id == responsive.window_id)
        .unwrap()
        .rect;
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(responsive.window_id),
        Some(expected)
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    assert!(state.display_change_apply_retry.is_none());
}

#[test]
fn test_display_change_size_only_deferral_waits_for_owner() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let template = placement_state(&[100]);
    let expected =
        template.workspaces[&1][0].compute_placements_animated(template.layout_viewport(1))[0].rect;
    let before = Rect::new(
        expected.x,
        expected.y,
        expected.width - 20,
        expected.height - 20,
    );
    let blocked = PlacementOwner::spawn_at(before);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(!state.paused);
    let start = Instant::now();
    let result = state.apply_layout();
    let elapsed = start.elapsed();
    let stayed_deferred = result.is_ok() && !state.paused;
    if !stayed_deferred {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        stayed_deferred,
        "same-origin async resize drained before owner pumped: {result:?}"
    );
    assert!(elapsed < Duration::from_secs(2));
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(before)
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id) != Some(expected)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(expected)
    );
    state.apply_layout().unwrap();
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    state.apply_layout().unwrap();
    assert!(state.last_physical_presentations[&blocked.window_id].confirmed);
}

#[test]
fn test_owner_wait_unchanged_rect_does_not_drain_while_blocked() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let template = placement_state(&[100]);
    let expected =
        template.workspaces[&1][0].compute_placements_animated(template.layout_viewport(1))[0].rect;
    let blocked = PlacementOwner::spawn_at(expected);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    let result = state.apply_layout();
    let active = result.is_ok() && !state.paused;
    if !active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        active,
        "matching geometry drained a blocked owner's deferral: {result:?}"
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
}

#[test]
fn test_owner_wait_clamped_size_returns_to_feedback_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn_with(Rect::new(48, 48, 160, 120), 360, false);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
        .unwrap()
        .width
        != 360
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
            .unwrap()
            .width,
        360
    );
    for _ in 0..3 {
        state.apply_layout().unwrap();
    }
    assert!(
        state.last_physical_presentations[&blocked.window_id].confirmed,
        "a pumping owner that clamps its size stayed measurement-deferred"
    );
    let placed =
        state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1))[0].rect;
    assert!(
        placed.width >= 360,
        "native width feedback was never restored: {placed:?}"
    );
}

fn fixture_placement(window_id: u64, visibility: Visibility) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect: if visibility == Visibility::Visible {
            Rect::new(32, 32, 320, 240)
        } else {
            Rect::new(-10_000, -10_000, 320, 240)
        },
        visibility,
        column_index: 0,
        row_index: 0,
    }
}

#[test]
fn test_owner_wait_offscreen_deferral_drains_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let state = placement_state(&[]);
    let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
    let (first, _) = leopardwm_platform_win32::apply_display_change_placements(
        std::slice::from_ref(&placement),
        &state.platform_config,
        false,
    )
    .unwrap();
    assert!(first.landings[0].measurement_deferred);
    blocked.resume();
    let mut last = first;
    for _ in 0..3 {
        last = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
    }
    assert!(
        !last.landings[0].measurement_deferred,
        "SWP_NOSIZE off-screen deferral never drained after the owner pumped"
    );
    assert!(!last.landings[0].failed);
}

#[test]
fn test_owner_wait_parked_maximized_recovery_is_classified_before_positioning() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_owner_wait_parked_maximized_recovery_is_classified_before_positioning",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn_with(Rect::new(48, 48, 320, 240), 0, true);
    assert!(leopardwm_platform_win32::is_placement_parked(
        blocked.window_id
    ));
    let state = placement_state(&[]);
    let placement = fixture_placement(blocked.window_id, Visibility::Visible);
    let config = state.platform_config.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let worker_placement = placement.clone();
    let worker = thread::spawn(move || {
        let result = leopardwm_platform_win32::apply_display_change_placements(
            std::slice::from_ref(&worker_placement),
            &config,
            false,
        );
        done_tx.send(result).unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(400));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let (result, deferred) = fast
        .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .unwrap();
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "parked-maximized recovery positioned synchronously before probing"
    );
    assert!(deferred.contains(&blocked.window_id));
    assert!(result.landings[0].measurement_deferred);
    for _ in 0..3 {
        let result = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
        if !result.landings[0].measurement_deferred {
            return;
        }
    }
    panic!("parked-maximized recovery did not drain after owner resumed");
}

#[test]
fn test_owner_wait_recovery_leaves_an_on_screen_target_untouched() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let monitor = leopardwm_platform_win32::get_primary_monitor().unwrap();
    let current = Rect::new(monitor.work_area.x + 48, monitor.work_area.y + 48, 160, 120);
    let targets = [
        Rect::new(current.x, current.y, 320, 240),
        Rect::new(monitor.work_area.x - 240, current.y, 320, 240),
    ];
    for rect in targets {
        let blocked = PlacementOwner::spawn_at(current);
        let state = placement_state(&[]);
        let mut placement = fixture_placement(blocked.window_id, Visibility::Visible);
        placement.rect = rect;
        let (_, deferred) = leopardwm_platform_win32::apply_display_change_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            false,
        )
        .unwrap();
        assert!(deferred.contains(&blocked.window_id));
        let window_id = blocked.window_id;
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(leopardwm_platform_win32::restore_windows_moved_offscreen(
                    &[window_id],
                ))
                .unwrap();
        });
        let fast = done_rx.recv_timeout(Duration::from_millis(400));
        let returned_while_blocked = fast.is_ok();
        blocked.resume();
        let restored = fast
            .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .unwrap();
        worker.join().unwrap();
        assert!(
            returned_while_blocked,
            "recovery synchronously positioned an on-screen waiting owner"
        );
        assert_eq!(
            restored, 0,
            "recovery queued a move for an on-screen target"
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            leopardwm_platform_win32::get_window_visible_rect(window_id),
            Some(rect),
            "recovery dragged an on-screen deferred window to the work-area origin"
        );
    }
}

#[test]
fn test_owner_wait_recovery_updates_size_before_a_size_preserving_park() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let primary = leopardwm_platform_win32::get_primary_monitor().unwrap();
    let monitors = leopardwm_platform_win32::enumerate_monitors().unwrap();
    let leftmost = monitors
        .iter()
        .min_by_key(|monitor| monitor.work_area.x)
        .unwrap();
    let current = Rect::new(primary.work_area.x + 48, primary.work_area.y + 48, 160, 120);
    let blocked = PlacementOwner::spawn_at(current);
    let window_id = blocked.window_id;
    let park_rect = Rect::new(
        leftmost.work_area.x - 240,
        leftmost.work_area.y + 48,
        320,
        240,
    );
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let state = placement_state(&[]);
        let mut resize = fixture_placement(window_id, Visibility::Visible);
        resize.rect = Rect::new(current.x, current.y, 320, 240);
        let (_, deferred) = leopardwm_platform_win32::apply_display_change_placements(
            &[resize],
            &state.platform_config,
            false,
        )
        .unwrap();
        leopardwm_platform_win32::queue_window_offscreen(window_id).unwrap();
        let first =
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[window_id]).unwrap();
        let mut park = fixture_placement(window_id, Visibility::OffScreenLeft);
        park.rect = park_rect;
        leopardwm_platform_win32::apply_placements(&[park], &state.platform_config, None, false)
            .unwrap();
        let second =
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[window_id]).unwrap();
        done_tx
            .send((deferred.contains(&window_id), first, second))
            .unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(600));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let (deferred, first, second) =
        fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "park/recovery sequence blocked on the owner"
    );
    assert!(deferred);
    assert_eq!(first, 1);
    assert_eq!(
        second, 1,
        "recovery misclassified a size-preserving park using the pre-restore size"
    );
    let expected = Rect::new(primary.work_area.x, primary.work_area.y, 160, 120);
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap() != expected
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap(),
        expected,
        "the size-preserving park overtook the final visible restore"
    );
}

#[test]
fn test_owner_wait_recovery_follows_a_park_that_has_not_landed() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for display_apply in [false, true] {
        let blocked = PlacementOwner::spawn();
        let state = placement_state(&[]);
        if display_apply {
            let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
            leopardwm_platform_win32::apply_display_change_placements(
                &[placement],
                &state.platform_config,
                false,
            )
            .unwrap();
        } else {
            leopardwm_platform_win32::queue_window_offscreen(blocked.window_id).unwrap();
        }
        assert!(!leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
            &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap()
        ));
        let window_id = blocked.window_id;
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(leopardwm_platform_win32::restore_windows_moved_offscreen(
                    &[window_id],
                ))
                .unwrap();
        });
        let fast = done_rx.recv_timeout(Duration::from_millis(400));
        let returned_while_blocked = fast.is_ok();
        blocked.resume();
        let restored = fast
            .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .unwrap();
        worker.join().unwrap();
        assert!(
            returned_while_blocked,
            "recovery synchronously positioned a waiting owner"
        );
        assert_eq!(
            restored, 1,
            "recovery skipped a queued park before it landed"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let rect = leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
            if rect.x >= 0 && rect.y >= 0 {
                // Allow the queued park and recovery to both run before checking final geometry.
                thread::sleep(Duration::from_millis(100));
                let final_rect =
                    leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
                assert!(final_rect.x >= 0 && final_rect.y >= 0);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let final_rect = leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
        assert!(
            final_rect.x >= 0 && final_rect.y >= 0,
            "queued park overtook recovery"
        );
    }
}

#[test]
fn test_owner_wait_omitted_window_is_probed_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    leopardwm_platform_win32::queue_window_offscreen(blocked.window_id).unwrap();
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap(),
    ) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap()
    ));
    let state = placement_state(&[]);
    leopardwm_platform_win32::apply_placements(&[], &state.platform_config, None, false).unwrap();
    let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
    let result = leopardwm_platform_win32::apply_placements(
        &[placement],
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(
        !result.landings[0].measurement_deferred,
        "an omitted responding owner retained its owner-wait marker"
    );
}

#[test]
fn test_owner_probe_budget_keeps_unprobed_windows_deferred_without_reporting_them() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owners: Vec<_> = (0..16).map(|_| PlacementOwner::spawn()).collect();
    let placements: Vec<_> = owners
        .iter()
        .map(|owner| {
            let mut placement = fixture_placement(owner.window_id, Visibility::Visible);
            placement.rect.x = 48;
            placement.rect.y = 48;
            placement
        })
        .collect();
    let state = placement_state(&[]);
    let started = Instant::now();
    let (result, unresponsive) = leopardwm_platform_win32::apply_display_change_placements(
        &placements,
        &state.platform_config,
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(result
        .landings
        .iter()
        .all(|landing| landing.measurement_deferred));
    assert!(!unresponsive.is_empty());
    assert!(
        unresponsive.len() < owners.len(),
        "budget-exhausted owners were reported as unresponsive without being probed"
    );
    let unprobed = owners
        .iter()
        .find(|owner| !unresponsive.contains(&owner.window_id))
        .unwrap();
    let window_id = unprobed.window_id;
    let config = state.platform_config.clone();
    let mut placement = fixture_placement(window_id, Visibility::Visible);
    placement.rect = Rect::new(48, 48, 360, 240);
    let (done_tx, done_rx) = mpsc::channel();
    let worker_placement = placement.clone();
    let worker = thread::spawn(move || {
        let mut cache = leopardwm_platform_win32::PlacementCache::new();
        leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&worker_placement),
            &config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        let result =
            leopardwm_platform_win32::apply_placements(&[worker_placement], &config, None, false)
                .unwrap();
        done_tx
            .send(result.landings[0].measurement_deferred)
            .unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(1600));
    let returned_while_blocked = fast.is_ok();
    unprobed.resume();
    let deferred = fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "an unprobed owner became synchronous after a same-origin resize"
    );
    assert!(
        deferred,
        "a same-origin resize drained an unprobed owner's deferral"
    );
    for _ in 0..3 {
        let result = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
        if !result.landings[0].measurement_deferred {
            assert_eq!(
                leopardwm_platform_win32::get_window_visible_rect(window_id),
                Some(placement.rect)
            );
            return;
        }
    }
    panic!("unprobed owner did not drain after responding");
}

#[test]
fn test_owner_wait_inactive_workspace_parking_does_not_block_display_change() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let window_id = blocked.window_id;
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut state = placement_state(&[]);
        state.injected_native_offscreen_enabled = true;
        let inactive = state.workspaces[&1][0].clone();
        state.workspaces.get_mut(&1).unwrap().push(inactive);
        state.workspaces.get_mut(&1).unwrap()[1]
            .insert_window(window_id, Some(320))
            .unwrap();
        state
            .injected_window_info
            .insert(window_id, make_test_window_info(window_id));
        state.handle_window_event(WindowEvent::DisplayChange);
        done_tx.send(!state.paused).unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(400));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let active = fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "display reconciliation synchronously parked an inactive-workspace window"
    );
    assert!(active);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap(),
    ) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap()
    ));
    let placement = fixture_placement(window_id, Visibility::Visible);
    let state = placement_state(&[]);
    for _ in 0..3 {
        leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(window_id),
        Some(placement.rect),
        "queued inactive parking overtook return-to-visible placement"
    );
}
