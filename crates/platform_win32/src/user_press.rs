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
static DRAG_FLOAT_REQUESTED: AtomicU64 = AtomicU64::new(0);
static SWALLOW_SECONDARY_UP: AtomicBool = AtomicBool::new(false);

pub fn arm_drag_float(hwnd: u64) {
    DRAG_FLOAT_REQUESTED.store(0, Ordering::SeqCst);
    DRAG_FLOAT_ARMED.store(hwnd, Ordering::SeqCst);
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
    Disarm,
}

fn drag_float_decision(
    ncode: i32,
    message: u32,
    self_injected: bool,
    swapped: bool,
    armed: u64,
    swallow_up: bool,
) -> DragFloatDecision {
    if ncode < 0 || self_injected {
        return DragFloatDecision::Pass;
    }
    let (secondary_down, secondary_up, primary_up) = if swapped {
        (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_RBUTTONUP)
    } else {
        (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_LBUTTONUP)
    };
    if message == secondary_down {
        if armed == 0 {
            DragFloatDecision::ClearSwallow
        } else {
            DragFloatDecision::Request(armed)
        }
    } else if message == secondary_up && swallow_up {
        DragFloatDecision::SwallowUp
    } else if message == primary_up && armed != 0 {
        DragFloatDecision::Disarm
    } else {
        DragFloatDecision::Pass
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
        match drag_float_decision(
            ncode,
            wparam.0 as u32,
            mouse.dwExtraInfo == SELF_INJECTED_INPUT_TAG,
            GetSystemMetrics(SM_SWAPBUTTON) != 0,
            DRAG_FLOAT_ARMED.load(Ordering::SeqCst),
            SWALLOW_SECONDARY_UP.load(Ordering::SeqCst),
        ) {
            DragFloatDecision::Request(hwnd) => {
                DRAG_FLOAT_REQUESTED.store(hwnd, Ordering::SeqCst);
                SWALLOW_SECONDARY_UP.store(true, Ordering::SeqCst);
                return LRESULT(1);
            }
            DragFloatDecision::ClearSwallow => SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst),
            DragFloatDecision::SwallowUp => {
                SWALLOW_SECONDARY_UP.store(false, Ordering::SeqCst);
                return LRESULT(1);
            }
            DragFloatDecision::Disarm => disarm_drag_float(),
            DragFloatDecision::Pass => {}
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
        for (ncode, message, injected, swapped, armed, swallow, expected) in [
            (0, WM_RBUTTONDOWN, false, false, 42, false, Request(42)),
            (0, WM_RBUTTONUP, false, false, 42, true, SwallowUp),
            (0, WM_RBUTTONUP, false, false, 42, false, Pass),
            (0, WM_RBUTTONDOWN, false, false, 0, true, ClearSwallow),
            (0, WM_LBUTTONUP, false, false, 42, false, Disarm),
            (0, WM_LBUTTONDOWN, false, true, 42, false, Request(42)),
            (0, WM_LBUTTONUP, false, true, 42, true, SwallowUp),
            (0, WM_RBUTTONUP, false, true, 42, false, Disarm),
            (0, WM_RBUTTONDOWN, true, false, 42, false, Pass),
            (-1, WM_RBUTTONDOWN, false, false, 42, false, Pass),
        ] {
            assert_eq!(
                drag_float_decision(ncode, message, injected, swapped, armed, swallow),
                expected
            );
        }
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
