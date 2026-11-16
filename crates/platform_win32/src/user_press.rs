use crate::{Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, GetSystemMetrics, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    SM_SWAPBUTTON, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_XBUTTONDOWN,
};

pub(crate) const SELF_INJECTED_INPUT_TAG: usize = 0x4C57_4D49;

static USER_PRESS_COUNT: AtomicU64 = AtomicU64::new(0);
static NEXT_OBSERVER_GENERATION: AtomicU64 = AtomicU64::new(1);
static KEYBOARD_OBSERVER_GENERATION: AtomicU64 = AtomicU64::new(0);
static MOUSE_OBSERVER_GENERATION: AtomicU64 = AtomicU64::new(0);
static DRAG_FLOAT_ARMED: AtomicU64 = AtomicU64::new(0);
static DRAG_FLOAT_ARMED_GENERATION: AtomicU64 = AtomicU64::new(0);
static DRAG_FLOAT_REQUESTED: AtomicU64 = AtomicU64::new(0);
static SWALLOW_SECONDARY_UP: AtomicBool = AtomicBool::new(false);
static PRIMARY_BUTTON_HELD: AtomicBool = AtomicBool::new(false);
static PRIMARY_PRESS_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn capture_move_size_start(hwnd: u64) -> Option<crate::event_hooks::MoveStartCapture> {
    if !PRIMARY_BUTTON_HELD.load(Ordering::SeqCst) {
        return None;
    }
    let generation = PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst);
    let rect = crate::get_window_visible_rect(hwnd)?;
    if !PRIMARY_BUTTON_HELD.load(Ordering::SeqCst)
        || PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst) != generation
    {
        return None;
    }
    Some(crate::event_hooks::MoveStartCapture { rect, generation })
}

pub fn arm_drag_float(hwnd: u64, generation: u64) {
    disarm_drag_float();
    DRAG_FLOAT_REQUESTED.store(0, Ordering::SeqCst);
    DRAG_FLOAT_ARMED_GENERATION.store(generation, Ordering::SeqCst);
    DRAG_FLOAT_ARMED.store(hwnd, Ordering::SeqCst);
    if !PRIMARY_BUTTON_HELD.load(Ordering::SeqCst)
        || PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst) != generation
    {
        let _ = DRAG_FLOAT_ARMED.compare_exchange(hwnd, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

pub fn disarm_drag_float() {
    DRAG_FLOAT_ARMED.store(0, Ordering::SeqCst);
}

pub fn drag_float_requested(hwnd: u64) -> bool {
    hwnd != 0 && DRAG_FLOAT_REQUESTED.load(Ordering::SeqCst) == hwnd
}

pub fn take_drag_float_request(hwnd: u64) -> bool {
    hwnd != 0
        && DRAG_FLOAT_REQUESTED
            .compare_exchange(hwnd, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

#[derive(Debug, PartialEq, Eq)]
enum DragFloatDecision {
    Pass,
    Request(u64),
    ClearSwallow,
    SwallowUp,
    PrimaryDown,
    PrimaryUp,
}

fn drag_float_decision(
    ncode: i32,
    message: u32,
    self_injected: bool,
    swapped: bool,
    armed: u64,
    swallow_up: bool,
    primary_held: bool,
) -> DragFloatDecision {
    if ncode < 0 || self_injected {
        return DragFloatDecision::Pass;
    }
    let (secondary_down, secondary_up, primary_down, primary_up) = if swapped {
        (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP)
    } else {
        (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_LBUTTONDOWN, WM_LBUTTONUP)
    };
    if message == secondary_down {
        if armed == 0 || !primary_held {
            DragFloatDecision::ClearSwallow
        } else {
            DragFloatDecision::Request(armed)
        }
    } else if message == secondary_up && swallow_up {
        DragFloatDecision::SwallowUp
    } else if message == primary_down {
        DragFloatDecision::PrimaryDown
    } else if message == primary_up {
        DragFloatDecision::PrimaryUp
    } else {
        DragFloatDecision::Pass
    }
}

fn handle_drag_float_mouse(ncode: i32, message: u32, self_injected: bool, swapped: bool) -> bool {
    let armed = DRAG_FLOAT_ARMED.load(Ordering::SeqCst);
    match drag_float_decision(
        ncode,
        message,
        self_injected,
        swapped,
        armed,
        SWALLOW_SECONDARY_UP.load(Ordering::SeqCst),
        PRIMARY_BUTTON_HELD.load(Ordering::SeqCst),
    ) {
        DragFloatDecision::Request(hwnd) => {
            if DRAG_FLOAT_ARMED_GENERATION.load(Ordering::SeqCst)
                != PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst)
                || DRAG_FLOAT_ARMED.load(Ordering::SeqCst) != hwnd
            {
                let _ =
                    DRAG_FLOAT_ARMED.compare_exchange(hwnd, 0, Ordering::SeqCst, Ordering::SeqCst);
                SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
                return false;
            }
            DRAG_FLOAT_REQUESTED.store(hwnd, Ordering::SeqCst);
            SWALLOW_SECONDARY_UP.store(true, Ordering::SeqCst);
            true
        }
        DragFloatDecision::ClearSwallow => {
            if armed != 0 {
                let _ =
                    DRAG_FLOAT_ARMED.compare_exchange(armed, 0, Ordering::SeqCst, Ordering::SeqCst);
            }
            SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
            false
        }
        DragFloatDecision::SwallowUp => {
            SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
            true
        }
        DragFloatDecision::PrimaryDown => {
            PRIMARY_BUTTON_HELD.store(true, Ordering::SeqCst);
            PRIMARY_PRESS_GENERATION.fetch_add(1, Ordering::SeqCst);
            disarm_drag_float();
            false
        }
        DragFloatDecision::PrimaryUp => {
            PRIMARY_BUTTON_HELD.store(false, Ordering::SeqCst);
            disarm_drag_float();
            false
        }
        DragFloatDecision::Pass => false,
    }
}

pub fn user_press_count() -> Option<u64> {
    let count = USER_PRESS_COUNT.load(Ordering::SeqCst);
    if KEYBOARD_OBSERVER_GENERATION.load(Ordering::SeqCst) == 0
        || MOUSE_OBSERVER_GENERATION.load(Ordering::SeqCst) == 0
    {
        return None;
    }
    // Reject a snapshot spanning an observer gap, even if both hooks are back by now.
    (USER_PRESS_COUNT.load(Ordering::SeqCst) == count).then_some(count)
}

pub(crate) fn next_observer_generation() -> u64 {
    NEXT_OBSERVER_GENERATION.fetch_add(1, Ordering::SeqCst)
}

pub(crate) fn set_keyboard_observer_installed(generation: u64, installed: bool) {
    set_observer_installed(&KEYBOARD_OBSERVER_GENERATION, generation, installed);
}

pub(crate) fn set_mouse_observer_installed(generation: u64, installed: bool) {
    set_observer_installed(&MOUSE_OBSERVER_GENERATION, generation, installed);
}

fn set_observer_installed(observer: &AtomicU64, generation: u64, installed: bool) {
    if installed {
        record_user_press();
        observer.store(generation, Ordering::SeqCst);
    } else if observer
        .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        record_user_press();
    }
}

pub(crate) fn record_user_press() {
    USER_PRESS_COUNT.fetch_add(1, Ordering::SeqCst);
}

pub struct MousePressHookHandle {
    thread_id: u32,
    generation: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MousePressHookHandle {
    fn drop(&mut self) {
        set_mouse_observer_installed(self.generation, false);
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT_LLHOOK_THREAD, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            for _ in 0..30 {
                if thread.is_finished() {
                    let _ = thread.join();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

pub fn install_mouse_press_hook() -> Result<MousePressHookHandle, Win32Error> {
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    let generation = next_observer_generation();
    let thread = std::thread::Builder::new()
        .name("mouse-press-hook".into())
        .spawn(move || unsafe {
            let thread_id = GetCurrentThreadId();
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_press_hook_proc), None, 0) {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                        "SetWindowsHookExW for mouse presses failed: {error}"
                    ))));
                    return;
                }
            };
            set_mouse_observer_installed(generation, true);
            let _ = init_tx.send(Ok(thread_id));
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0).0;
                if ret <= 0 || msg.message == WM_QUIT_LLHOOK_THREAD {
                    break;
                }
                let _ = DispatchMessageW(&msg);
            }
            set_mouse_observer_installed(generation, false);
            let _ = UnhookWindowsHookEx(hook);
        })
        .map_err(|error| {
            Win32Error::HookInstallFailed(format!(
                "Failed to spawn mouse press hook thread: {error}"
            ))
        })?;
    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Mouse press hook thread initialization failed".into())
    })??;
    Ok(MousePressHookHandle {
        thread_id,
        generation,
        thread: Some(thread),
    })
}

unsafe extern "system" fn mouse_press_hook_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if ncode >= 0
        && matches!(
            wparam.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    {
        let mouse = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if mouse.dwExtraInfo != SELF_INJECTED_INPUT_TAG {
            record_user_press();
        }
    }
    if ncode >= 0
        && matches!(
            wparam.0 as u32,
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
        )
    {
        let mouse = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if handle_drag_float_mouse(
            ncode,
            wparam.0 as u32,
            mouse.dwExtraInfo == SELF_INJECTED_INPUT_TAG,
            GetSystemMetrics(SM_SWAPBUTTON) != 0,
        ) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
pub(crate) static PRESS_COUNT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        LLMHF_INJECTED, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    };

    #[test]
    fn drag_float_mouse_decisions_respect_buttons_and_hook_boundaries() {
        use DragFloatDecision::*;
        for (ncode, message, injected, swapped, armed, swallow, held, expected) in [
            (
                0,
                WM_RBUTTONDOWN,
                false,
                false,
                42,
                false,
                true,
                Request(42),
            ),
            (
                0,
                WM_RBUTTONDOWN,
                false,
                false,
                42,
                true,
                false,
                ClearSwallow,
            ),
            (0, WM_RBUTTONUP, false, false, 42, true, true, SwallowUp),
            (0, WM_RBUTTONUP, false, false, 42, false, true, Pass),
            (0, WM_RBUTTONDOWN, false, false, 0, true, true, ClearSwallow),
            (
                0,
                WM_LBUTTONDOWN,
                false,
                false,
                0,
                false,
                false,
                PrimaryDown,
            ),
            (0, WM_LBUTTONUP, false, false, 42, false, true, PrimaryUp),
            (0, WM_LBUTTONDOWN, false, true, 42, false, true, Request(42)),
            (
                0,
                WM_LBUTTONDOWN,
                false,
                true,
                42,
                true,
                false,
                ClearSwallow,
            ),
            (0, WM_LBUTTONUP, false, true, 42, true, true, SwallowUp),
            (0, WM_RBUTTONDOWN, false, true, 0, false, false, PrimaryDown),
            (0, WM_RBUTTONUP, false, true, 42, false, true, PrimaryUp),
            (0, WM_RBUTTONDOWN, true, false, 42, false, true, Pass),
            (-1, WM_RBUTTONDOWN, false, false, 42, false, true, Pass),
        ] {
            assert_eq!(
                drag_float_decision(ncode, message, injected, swapped, armed, swallow, held),
                expected
            );
        }
    }

    #[test]
    fn late_arming_after_primary_release_does_not_request_or_swallow() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        disarm_drag_float();
        DRAG_FLOAT_REQUESTED.store(0, Ordering::SeqCst);
        PRIMARY_BUTTON_HELD.store(false, Ordering::SeqCst);
        SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONDOWN, false, false));
        let original_generation = PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst);
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONUP, false, false));
        arm_drag_float(42, original_generation);
        assert_eq!(DRAG_FLOAT_ARMED.load(Ordering::SeqCst), 0);
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONDOWN, false, false));
        arm_drag_float(42, original_generation);
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONDOWN, false, false));
        assert!(!drag_float_requested(42));
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONUP, false, false));
        let current_generation = PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst);
        arm_drag_float(42, current_generation);
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONDOWN, false, false));
        assert_eq!(DRAG_FLOAT_ARMED.load(Ordering::SeqCst), 0);
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONDOWN, false, false));
        assert!(!drag_float_requested(42));
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONUP, false, false));
        arm_drag_float(42, PRIMARY_PRESS_GENERATION.load(Ordering::SeqCst));
        assert!(handle_drag_float_mouse(0, WM_RBUTTONDOWN, false, false));
        assert!(take_drag_float_request(42));
        assert!(handle_drag_float_mouse(0, WM_RBUTTONUP, false, false));
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONUP, false, false));
        disarm_drag_float();
    }

    #[test]
    fn same_hwnd_drag_start_keeps_its_own_press() {
        use crate::WindowEvent;
        use windows::core::w;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        };
        struct OwnedWindow(HWND);
        impl Drop for OwnedWindow {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        let window = OwnedWindow(unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                100,
                100,
                300,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        });
        let hwnd = window.0 .0 as usize as u64;
        disarm_drag_float();
        DRAG_FLOAT_REQUESTED.store(0, Ordering::SeqCst);
        PRIMARY_BUTTON_HELD.store(false, Ordering::SeqCst);
        SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
        assert!(capture_move_size_start(hwnd).is_none());
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONDOWN, false, false));
        let event_a = WindowEvent::MoveSizeStart(hwnd, capture_move_size_start(hwnd));
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONUP, false, false));
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONDOWN, false, false));
        let event_b = WindowEvent::MoveSizeStart(hwnd, capture_move_size_start(hwnd));
        let WindowEvent::MoveSizeStart(_, Some(capture_a)) = event_a else {
            panic!("missing first drag capture");
        };
        let WindowEvent::MoveSizeStart(_, Some(capture_b)) = event_b else {
            panic!("missing second drag capture");
        };
        assert_ne!(capture_a.generation, capture_b.generation);
        arm_drag_float(hwnd, capture_a.generation);
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONDOWN, false, false));
        assert!(!drag_float_requested(hwnd));
        assert!(!handle_drag_float_mouse(0, WM_RBUTTONUP, false, false));
        arm_drag_float(hwnd, capture_b.generation);
        assert!(handle_drag_float_mouse(0, WM_RBUTTONDOWN, false, false));
        assert!(take_drag_float_request(hwnd));
        assert!(handle_drag_float_mouse(0, WM_RBUTTONUP, false, false));
        assert!(!handle_drag_float_mouse(0, WM_LBUTTONUP, false, false));
    }

    #[test]
    fn press_count_requires_both_observers_and_invalidates_observation_gaps() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        let mut keyboard_generation = next_observer_generation();
        let mut mouse_generation = next_observer_generation();
        assert_eq!(user_press_count(), None);
        for (keyboard, installed, available) in [
            (true, true, false),
            (false, true, true),
            (true, false, false),
            (true, true, true),
            (false, false, false),
            (false, true, true),
            (true, false, false),
            (false, false, false),
        ] {
            let before = USER_PRESS_COUNT.load(Ordering::SeqCst);
            if keyboard {
                if installed {
                    keyboard_generation = next_observer_generation();
                }
                set_keyboard_observer_installed(keyboard_generation, installed);
            } else {
                if installed {
                    mouse_generation = next_observer_generation();
                }
                set_mouse_observer_installed(mouse_generation, installed);
            }
            let after = USER_PRESS_COUNT.load(Ordering::SeqCst);
            assert_eq!(after, before + 1);
            assert_eq!(user_press_count(), available.then_some(after));
        }
    }

    #[test]
    fn stale_observer_clear_preserves_replacement_availability() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        let setters: [fn(u64, bool); 2] = [
            set_keyboard_observer_installed,
            set_mouse_observer_installed,
        ];
        for (index, set_observer) in setters.into_iter().enumerate() {
            let set_other = setters[1 - index];
            let other_generation = next_observer_generation();
            let old_generation = next_observer_generation();
            let current_generation = next_observer_generation();
            set_other(other_generation, true);
            set_observer(old_generation, true);
            set_observer(old_generation, false);
            set_observer(current_generation, true);
            let count = user_press_count().unwrap();
            set_observer(old_generation, false);
            assert_eq!(user_press_count(), Some(count));
            set_observer(current_generation, false);
            assert_eq!(user_press_count(), None);
            assert_eq!(USER_PRESS_COUNT.load(Ordering::SeqCst), count + 1);
            set_other(other_generation, false);
        }
    }

    #[test]
    fn mouse_hook_counts_button_downs_but_not_motion_wheel_or_release() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        let keyboard_generation = next_observer_generation();
        let mouse_generation = next_observer_generation();
        set_keyboard_observer_installed(keyboard_generation, true);
        set_mouse_observer_installed(mouse_generation, true);
        let mut expected = user_press_count().unwrap();
        for (ncode, message, flags, extra_info, counts) in [
            (0, WM_MOUSEMOVE, 0, 0, false),
            (0, WM_MOUSEWHEEL, 0, 0, false),
            (0, WM_MOUSEHWHEEL, 0, 0, false),
            (0, WM_LBUTTONUP, 0, 0, false),
            (-1, WM_LBUTTONDOWN, 0, 0, false),
            (0, WM_LBUTTONDOWN, 0, 0, true),
            (0, WM_RBUTTONDOWN, 0, 0, true),
            (0, WM_MBUTTONDOWN, 0, 0, true),
            (0, WM_XBUTTONDOWN, 0, 0, true),
            (
                0,
                WM_LBUTTONDOWN,
                LLMHF_INJECTED,
                SELF_INJECTED_INPUT_TAG,
                false,
            ),
            (0, WM_LBUTTONDOWN, LLMHF_INJECTED, 0, true),
            (0, WM_LBUTTONDOWN, LLMHF_INJECTED, 123, true),
        ] {
            let mouse = MSLLHOOKSTRUCT {
                flags,
                dwExtraInfo: extra_info,
                ..Default::default()
            };
            unsafe {
                mouse_press_hook_proc(
                    ncode,
                    WPARAM(message as usize),
                    LPARAM(&mouse as *const _ as isize),
                )
            };
            expected += u64::from(counts);
            assert_eq!(
                user_press_count(),
                Some(expected),
                "message={message}, ncode={ncode}, flags={flags}, extra_info={extra_info}"
            );
        }
        set_keyboard_observer_installed(keyboard_generation, false);
        set_mouse_observer_installed(mouse_generation, false);
    }
}
