use super::*;
use leopardwm_platform_win32 as platform;
use std::sync::atomic::AtomicUsize;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, MonitorFromRect, MonitorFromWindow, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, RegisterClassW, UnregisterClassW, NCCALCSIZE_PARAMS,
    SM_CXPADDEDBORDER, SM_CXSIZEFRAME, WM_NCCALCSIZE, WNDCLASSW, WS_VISIBLE,
};

static NCCALCS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn chromium_frame_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCALCSIZE && wparam.0 != 0 {
        NCCALCS.fetch_add(1, Ordering::SeqCst);
        let rect = &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0];
        if !MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL).0.is_null()
            || !MonitorFromRect(rect, MONITOR_DEFAULTTONULL).0.is_null()
        {
            let dpi = GetDpiForWindow(hwnd);
            let frame = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            rect.left += frame;
            rect.right -= frame;
            rect.bottom -= frame;
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

struct FrameWindow {
    id: u64,
    thread_id: u32,
    join: Option<thread::JoinHandle<()>>,
}

impl FrameWindow {
    fn spawn(rect: Rect) -> Self {
        let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
        let (ready, receiver) = mpsc::channel();
        let join = thread::spawn(move || unsafe {
            assert_ne!(SetThreadDesktop(desktop as *mut _), 0);
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(chromium_frame_proc),
                hInstance: instance.into(),
                lpszClassName: w!("LeopardWMChromiumFrameFixture"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.lpszClassName,
                w!("frame fixture"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            ready
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).unwrap();
            UnregisterClassW(class.lpszClassName, Some(instance.into())).unwrap();
        });
        let (id, thread_id) = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        platform::stamp_managed_lifetime_token(id).unwrap();
        Self {
            id,
            thread_id,
            join: Some(join),
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.id as usize as *mut _)
    }

    fn insets(&self) -> (i32, i32, i32, i32) {
        unsafe {
            let mut outer = RECT::default();
            let mut client = RECT::default();
            let mut origin = POINT::default();
            GetWindowRect(self.hwnd(), &mut outer).unwrap();
            GetClientRect(self.hwnd(), &mut client).unwrap();
            assert!(ClientToScreen(self.hwnd(), &mut origin).as_bool());
            (
                origin.x - outer.left,
                origin.y - outer.top,
                outer.right - origin.x - (client.right - client.left),
                outer.bottom - origin.y - (client.bottom - client.top),
            )
        }
    }

    fn expected_insets(&self) -> (i32, i32, i32, i32) {
        let dpi = unsafe { GetDpiForWindow(self.hwnd()) };
        let f = unsafe {
            GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
        };
        (f, 0, f, f)
    }
}

impl Drop for FrameWindow {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).unwrap() };
        self.join.take().unwrap().join().unwrap();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameTest {
    CachedReturn,
    ShutdownRecovery,
    OrdinaryReturn,
}

fn run_frame_test(kind: FrameTest) {
    let _desktop = FixtureDesktop::enter();
    let monitors = platform::enumerate_monitors().unwrap();
    let primary = monitors.iter().find(|m| m.is_primary).unwrap();
    let visible = Rect::new(
        primary.work_area.x + 100,
        primary.work_area.y + 100,
        600,
        400,
    );
    let window = FrameWindow::spawn(visible);
    let config = platform::PlatformConfig {
        monitor_rects: monitors.iter().map(|m| m.rect).collect(),
    };
    let mut cache = platform::PlacementCache::new();
    let mut placement = WindowPlacement {
        window_id: window.id,
        rect: visible,
        visibility: Visibility::Visible,
        column_index: 0,
        row_index: 0,
    };
    let place = |placement: &WindowPlacement, cache: &mut platform::PlacementCache| {
        platform::apply_placements(std::slice::from_ref(placement), &config, Some(cache), false)
            .unwrap();
    };
    place(&placement, &mut cache);
    assert!(wait_for_fixture(
        || window.insets() == window.expected_insets(),
        Duration::from_secs(3)
    ));
    if kind == FrameTest::ShutdownRecovery {
        assert!(platform::remove_maximizebox(window.id).unwrap());
        assert!(platform::wait_for_window_style_requests(
            Duration::from_secs(5)
        ));
    }
    let sentinel = platform::MOVE_OFFSCREEN_SENTINEL_COORD;
    placement.rect = Rect::new(sentinel, sentinel, visible.width, visible.height);
    placement.visibility = Visibility::OffScreenLeft;
    place(&placement, &mut cache);
    assert!(wait_for_fixture(
        || platform::get_window_chrome_rect(window.id)
            .is_some_and(|r| platform::is_move_offscreen_sentinel_rect(&r)),
        Duration::from_secs(3)
    ));
    match kind {
        FrameTest::CachedReturn => assert!(platform::remove_maximizebox(window.id).unwrap()),
        FrameTest::ShutdownRecovery => assert!(platform::restore_maximizebox(window.id).unwrap()),
        FrameTest::OrdinaryReturn => {}
    }
    if kind != FrameTest::OrdinaryReturn {
        assert!(platform::wait_for_window_style_requests(
            Duration::from_secs(5)
        ));
        assert_eq!(window.insets(), (0, 0, 0, 0));
    }
    let count = NCCALCS.load(Ordering::SeqCst);
    if kind == FrameTest::ShutdownRecovery {
        crate::state::run_visibility_recovery_pass(&[window.id], "test");
    } else {
        placement.rect = visible;
        placement.visibility = Visibility::Visible;
        place(&placement, &mut cache);
    }
    assert!(wait_for_fixture(
        || platform::get_window_chrome_rect(window.id)
            .is_some_and(|r| !platform::is_move_offscreen_sentinel_rect(&r)),
        Duration::from_secs(3)
    ));
    if kind == FrameTest::OrdinaryReturn {
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            NCCALCS.load(Ordering::SeqCst),
            count,
            "ordinary return must not recalculate the frame"
        );
        place(&placement, &mut cache);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            NCCALCS.load(Ordering::SeqCst),
            count,
            "cached return must not recalculate the frame"
        );
    } else {
        assert!(
            wait_for_fixture(
                || window.insets() == window.expected_insets(),
                Duration::from_secs(3)
            ),
            "return left collapsed insets: {:?}",
            window.insets()
        );
    }
}

#[test]
fn parked_style_refreshes_on_cached_return() {
    if run_in_placement_test_process("tests::display_change_regression::parked_frame_refresh::parked_style_refreshes_on_cached_return") { return; }
    run_frame_test(FrameTest::CachedReturn);
}

#[test]
fn parked_style_refreshes_on_shutdown_recovery() {
    if run_in_placement_test_process("tests::display_change_regression::parked_frame_refresh::parked_style_refreshes_on_shutdown_recovery") { return; }
    run_frame_test(FrameTest::ShutdownRecovery);
}

#[test]
fn ordinary_parked_return_does_not_refresh() {
    if run_in_placement_test_process("tests::display_change_regression::parked_frame_refresh::ordinary_parked_return_does_not_refresh") { return; }
    run_frame_test(FrameTest::OrdinaryReturn);
}
