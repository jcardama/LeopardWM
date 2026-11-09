use super::*;
use crate::event_handler::{AdmissionKind, AdmitOutcome};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, IsZoomed, PostThreadMessageW,
    ShowWindow, MSG, SW_SHOWMAXIMIZED, WM_QUIT, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPEDWINDOW,
};

struct MaximizedOwner {
    window_id: u64,
    thread_id: u32,
    release: mpsc::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl MaximizedOwner {
    fn spawn() -> Self {
        Self::spawn_with_explicit_release(false)
    }

    fn spawn_with_explicit_release(explicit_release: bool) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let join = thread::spawn(move || unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                None,
                WS_OVERLAPPEDWINDOW,
                48,
                48,
                320,
                240,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            // STARTUPINFO can override the process's first ShowWindow command.
            if !IsZoomed(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            }
            ready_tx
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            if explicit_release {
                let _ = resume.recv();
            } else {
                let _ = resume.recv_timeout(Duration::from_secs(2));
            }
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).unwrap();
        });
        let (window_id, thread_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            window_id,
            thread_id,
            release,
            join: Some(join),
        }
    }
}

impl Drop for MaximizedOwner {
    fn drop(&mut self) {
        let _ = self.release.send(());
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}

// The real window cannot grow past the host desktop, which is 1024x768 on CI runners,
// so lay it out on a monitor that fits there.
fn desktop_fitting_monitors() -> Vec<MonitorInfo> {
    vec![MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 800, 600),
        work_area: Rect::new(0, 0, 800, 560),
        is_primary: true,
        device_name: "DISPLAY1".to_string(),
        scale_factor: 1.0,
    }]
}

#[test]
fn test_maximized_admission_does_not_wait_for_non_pumping_owner() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (_hooks, events) = leopardwm_platform_win32::install_event_hooks().unwrap();
    let owner = MaximizedOwner::spawn();
    let hwnd = owner.window_id;
    assert!(unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
    let token = leopardwm_platform_win32::stamp_managed_lifetime_token(hwnd).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let (verify_tx, verify_rx) = mpsc::channel::<bool>();
    let admission = thread::spawn(move || {
        let mut config = test_config();
        config.behavior.disable_snap_layouts = false;
        config.behavior.focus_new_windows = false;
        config.animation.layout_duration_ms = 0;
        config.animation.scroll_duration_ms = 0;
        let mut state = AppState::new_with_config(config, desktop_fitting_monitors());
        state.reduce_motion = true;
        state.paused = false;
        state.next_injected_lifetime_token = token;
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
        let start = Instant::now();
        let outcome = state.try_admit_window(hwnd, AdmissionKind::Automatic);
        done_tx.send((outcome, start.elapsed())).unwrap();
        if !verify_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            return;
        }
        assert!(leopardwm_platform_win32::wait_for_window_style_requests(
            Duration::from_secs(5)
        ));
        assert_eq!(outcome, AdmitOutcome::Admitted);
        assert!(!unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
        assert!(state.window_last_maximized_at.contains_key(&hwnd));
        state.layout_transition = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.window_last_maximized_at.contains_key(&hwnd) && Instant::now() < deadline {
            let Ok(event) = events.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            // Feed only worker completions, not ambient desktop hook notifications.
            if matches!(
                event,
                WindowEvent::Created(..)
                    | WindowEvent::Destroyed(..)
                    | WindowEvent::Hidden(..)
                    | WindowEvent::Focused(..)
                    | WindowEvent::Minimized(..)
                    | WindowEvent::Restored(..)
                    | WindowEvent::MovedOrResized(..)
                    | WindowEvent::MoveSizeStart(..)
                    | WindowEvent::MoveSizeEnd(..)
                    | WindowEvent::DisplayChange
                    | WindowEvent::WorkAreaChanged
                    | WindowEvent::MouseEnterWindow(..)
                    | WindowEvent::MouseLeftManaged
                    | WindowEvent::TitleChanged(..)
            ) {
                continue;
            }
            state.handle_window_event(event);
        }
        assert!(
            !state.window_last_maximized_at.contains_key(&hwnd),
            "worker completion must remove grace"
        );
        let expected = state
            .focused_workspace()
            .unwrap()
            .compute_placements_animated(state.layout_viewport(1))
            .into_iter()
            .find(|placement| placement.window_id == hwnd)
            .unwrap()
            .rect;
        assert_eq!(state.last_placed_layout_rects.get(&hwnd), Some(&expected));
        let landed = leopardwm_platform_win32::get_window_visible_rect(hwnd).unwrap();
        assert!((landed.x - expected.x).abs() <= 2 && (landed.y - expected.y).abs() <= 2);
        assert!(
            (landed.width - expected.width).abs() <= 2
                && (landed.height - expected.height).abs() <= 2
        );
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(400));
    let returned_while_blocked = fast.is_ok();
    let _ = owner.release.send(());
    let (outcome, elapsed) = fast.unwrap_or_else(|_| {
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("admission did not finish after owner resumed")
    });
    verify_tx.send(returned_while_blocked).unwrap();
    admission.join().unwrap();
    assert!(
        returned_while_blocked && elapsed < Duration::from_millis(400),
        "admission waited for non-pumping owner: {elapsed:?}"
    );
    assert_eq!(outcome, AdmitOutcome::Admitted);
}

fn native_admission_state(hwnd: u64) -> AppState {
    let mut config = test_config();
    config.behavior.disable_snap_layouts = false;
    config.behavior.focus_new_windows = false;
    config.animation.layout_duration_ms = 0;
    config.animation.scroll_duration_ms = 0;
    let mut state = AppState::new_with_config(config, desktop_fitting_monitors());
    state.reduce_motion = true;
    state.paused = false;
    state.next_injected_lifetime_token =
        leopardwm_platform_win32::stamp_managed_lifetime_token(hwnd).unwrap();
    state
        .injected_window_info
        .insert(hwnd, make_test_window_info(hwnd));
    state
}

#[test]
fn test_first_late_maximize_observes_fresh_app_fullscreen_before_restore() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for initially_zoomed in [true, false] {
        let owner = MaximizedOwner::spawn();
        let hwnd = owner.window_id;
        let mut state = native_admission_state(hwnd);
        let chrome = leopardwm_platform_win32::get_window_chrome_rect(hwnd).unwrap();
        let monitor = state.monitors.get_mut(&1).unwrap();
        monitor.rect = chrome;
        monitor.work_area = Rect::new(
            chrome.x + 100,
            chrome.y + 100,
            chrome.width - 200,
            chrome.height - 200,
        );
        state.paused = true;
        state.injected_press_count = Some(10);
        assert_eq!(
            state.try_admit_window_at_with_native_ops(
                hwnd,
                AdmissionKind::Automatic,
                None,
                |_| false,
                |_| panic!("unmaximized admission must not restore"),
            ),
            AdmitOutcome::Admitted
        );
        assert!(!state.is_application_fullscreen(hwnd));
        state.paused = false;
        if initially_zoomed {
            state.on_window_moved_or_resized_with_native_ops(
                hwnd,
                false,
                || Some(11),
                |_| panic!("user-initiated maximize must not restore"),
                |_| true,
            );
            assert!(!state.is_application_fullscreen(hwnd));
        }
        state.on_window_moved_or_resized_with_native_ops(
            hwnd,
            false,
            || Some(10 + u64::from(initially_zoomed)),
            |_| panic!("fresh app fullscreen must not be restored into its tile"),
            |_| false,
        );
        assert!(state.is_application_fullscreen(hwnd));
        assert_eq!(state.application_fullscreen[&hwnd].rect, chrome);
        assert!(!state.pending_maximized_admission_restores.contains(&hwnd));
        assert!(!state
            .post_admission_maximize_restore_eligible
            .contains_key(&hwnd));
    }
}

#[test]
fn test_late_maximize_event_wrapper_uses_press_count_to_queue_restore() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (_hooks, _events) = leopardwm_platform_win32::install_event_hooks().unwrap();
    let owner = MaximizedOwner::spawn();
    let hwnd = owner.window_id;
    let mut state = native_admission_state(hwnd);
    state.paused = true;
    state.injected_press_count = Some(10);
    assert_eq!(
        state.try_admit_window_at_with_native_ops(
            hwnd,
            AdmissionKind::Automatic,
            None,
            |_| false,
            |_| panic!("unmaximized admission must not restore"),
        ),
        AdmitOutcome::Admitted
    );
    state.paused = false;
    state
        .moved_or_resized_suppression
        .insert(hwnd, Instant::now() + Duration::from_millis(250));
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    assert!(state.pending_maximized_admission_restores.contains(&hwnd));
    let _ = owner.release.send(());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(5)
    ));
    assert!(!unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
}

#[test]
fn test_maximized_admission_without_completion_route_does_not_stick_pending() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocker = MaximizedOwner::spawn_with_explicit_release(true);
    assert!(leopardwm_platform_win32::remove_maximizebox(blocker.window_id).unwrap());
    assert!(!leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_millis(20)
    ));
    let owner = MaximizedOwner::spawn();
    let hwnd = owner.window_id;
    assert!(unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
    let mut state = native_admission_state(hwnd);
    let _ = owner.release.send(());
    let outcome = state.try_admit_window(hwnd, AdmissionKind::Automatic);
    let pending = state.pending_maximized_admission_restores.contains(&hwnd);
    let grace = state.window_last_maximized_at.contains_key(&hwnd);
    let still_zoomed = unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() };
    let _ = blocker.release.send(());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(5)
    ));
    assert_eq!(outcome, AdmitOutcome::Admitted);
    assert!(
        still_zoomed,
        "restore must remain queued until after the sample"
    );
    assert!(
        !pending,
        "admission with no completion route must not remain pending"
    );
    assert!(
        grace,
        "a natively maximized admission without a report keeps settling grace"
    );
    assert!(!unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
    let old = Instant::now() - Duration::from_secs(10);
    state.window_managed_at.insert(hwnd, old);
    state.window_last_maximized_at.insert(hwnd, old);
    state.layout_transition = None;
    state.handle_window_event(WindowEvent::MovedOrResized(hwnd));
    assert!(!state.window_last_maximized_at.contains_key(&hwnd));
    let expected = state
        .focused_workspace()
        .unwrap()
        .compute_placements_animated(state.layout_viewport(1))[0]
        .rect;
    assert_eq!(state.last_placed_layout_rects.get(&hwnd), Some(&expected));
    let actual = leopardwm_platform_win32::get_window_visible_rect(hwnd).unwrap();
    assert!((actual.x - expected.x).abs() <= 2 && (actual.y - expected.y).abs() <= 2);
    assert!(
        (actual.width - expected.width).abs() <= 2 && (actual.height - expected.height).abs() <= 2
    );
}

#[test]
fn test_maximized_admission_identity_skip_reports_and_clears_pending() {
    use windows::Win32::UI::WindowsAndMessaging::RemovePropW;

    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (_hooks, events) = leopardwm_platform_win32::install_event_hooks().unwrap();
    let blocker = MaximizedOwner::spawn();
    assert!(leopardwm_platform_win32::remove_maximizebox(blocker.window_id).unwrap());
    assert!(!leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_millis(20)
    ));
    let owner = MaximizedOwner::spawn();
    let hwnd = owner.window_id;
    assert!(unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() });
    let mut state = native_admission_state(hwnd);
    assert_eq!(
        state.try_admit_window(hwnd, AdmissionKind::Automatic),
        AdmitOutcome::Admitted
    );
    assert!(state.pending_maximized_admission_restores.contains(&hwnd));
    let token = state.managed_lifetime_tokens[&hwnd];
    unsafe {
        RemovePropW(HWND(hwnd as *mut _), w!("LeopardWMManagedToken")).unwrap();
    }
    let _ = blocker.release.send(());
    let _ = owner.release.send(());
    assert!(leopardwm_platform_win32::wait_for_window_style_requests(
        Duration::from_secs(5)
    ));
    assert!(
        unsafe { IsZoomed(HWND(hwnd as *mut _)).as_bool() },
        "identity mismatch must skip ShowWindow"
    );
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut completion = None;
    while Instant::now() < deadline {
        let Ok(event) = events.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        if matches!(event, WindowEvent::MaximizedAdmissionRestored { window_id, .. } if window_id == hwnd)
        {
            completion = Some(event);
            break;
        }
    }
    let completion = completion.expect("identity skip must report a terminal completion");
    assert!(
        matches!(completion, WindowEvent::MaximizedAdmissionRestored {
        managed_lifetime_token: reported_token, still_maximized: true, ..
    } if reported_token == token)
    );
    state.handle_window_event(completion);
    assert!(!state.pending_maximized_admission_restores.contains(&hwnd));
    assert!(state.window_last_maximized_at.contains_key(&hwnd));
}
