//! Touchpad gesture detection via low-level mouse hook.

use crate::{recover_poisoned_mutex, Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::mpsc;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_MOUSE_LL,
};

/// Gesture events detected from touchpad/pointer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureEvent {
    /// Three-finger swipe left
    SwipeLeft,
    /// Three-finger swipe right
    SwipeRight,
    /// Three-finger swipe up
    SwipeUp,
    /// Three-finger swipe down
    SwipeDown,
    /// Modifier + mouse wheel scroll up
    ScrollUp,
    /// Modifier + mouse wheel scroll down
    ScrollDown,
}

/// Wheel message constants (not all exposed by windows-rs).
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;

/// Threshold for accumulated wheel delta before firing a gesture event.
/// 3 * WHEEL_DELTA (120) = 360.
const GESTURE_SCROLL_THRESHOLD: i32 = 360;

/// Timeout in milliseconds: if no scroll event arrives within this window,
/// accumulators are reset.
const GESTURE_TIMEOUT_MS: u128 = 300;

/// Gesture accumulator state for the low-level mouse hook.
struct GestureAccumState {
    /// Accumulated horizontal wheel delta.
    accum_x: i32,
    /// Accumulated vertical wheel delta.
    accum_y: i32,
    /// Timestamp of the last scroll event.
    last_scroll_time: std::time::Instant,
}

/// Modifier flags for scroll wheel navigation, stored as a bitmask.
/// Bit 0 = Ctrl, Bit 1 = Alt, Bit 2 = Shift, Bit 3 = Win.
static SCROLL_MODIFIER_FLAGS: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0x03); // default: Ctrl + Alt

/// Global sender for gesture events.
static GESTURE_SENDER: std::sync::Mutex<Option<mpsc::Sender<GestureEvent>>> =
    std::sync::Mutex::new(None);

/// Global gesture accumulator state.
/// Initialized to `None`; `register_gestures()` sets it to `Some(...)`.
static GESTURE_STATE: std::sync::Mutex<Option<GestureAccumState>> = std::sync::Mutex::new(None);

/// Handle for gesture detection.
///
/// Dropping this handle will signal the dedicated message-pump thread to
/// unhook the low-level mouse hook and exit.
pub struct GestureHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for GestureHandle {
    fn drop(&mut self) {
        // Signal the thread to exit
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT_LLHOOK_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            // Give the thread a moment to clean up
            for _ in 0..30 {
                if thread.is_finished() {
                    let _ = thread.join();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        // Clear the global sender and state (recover from mutex poisoning)
        let mut sender = GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        *sender = None;
        drop(sender);
        let mut state = GESTURE_STATE.lock().unwrap_or_else(recover_poisoned_mutex);
        *state = None;

        tracing::debug!("Gesture detection stopped");
    }
}

/// Set the modifier keys required for scroll wheel navigation.
///
/// Parses a modifier string like "Ctrl+Alt", "Ctrl+Shift", etc.
/// Unrecognized tokens are ignored.
pub fn set_scroll_modifier(modifier_str: &str) {
    let mut flags: u8 = 0;
    for part in modifier_str.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => flags |= 0x01,
            "alt" | "menu" => flags |= 0x02,
            "shift" => flags |= 0x04,
            "win" | "super" => flags |= 0x08,
            _ => {}
        }
    }
    if flags == 0 {
        // Fallback to Ctrl+Alt if nothing valid was parsed
        flags = 0x03;
    }
    SCROLL_MODIFIER_FLAGS.store(flags, std::sync::atomic::Ordering::Relaxed);
    tracing::debug!("Scroll modifier set to: {} (flags=0x{:02x})", modifier_str, flags);
}

/// Register a low-level mouse hook for gesture detection via wheel events.
///
/// Spawns a dedicated thread with a Win32 message pump so that `WH_MOUSE_LL`
/// callbacks are dispatched promptly (low-level hooks require the installing
/// thread to pump messages).
///
/// Returns a handle that must be kept alive to receive gesture events,
/// and a channel receiver for gesture events.
pub fn register_gestures() -> Result<(GestureHandle, mpsc::Receiver<GestureEvent>), Win32Error> {
    // Create channel for events
    let (tx, rx) = mpsc::channel();

    // Store sender globally
    {
        let mut sender = GESTURE_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Gesture sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Gesture sender already initialized - drop existing GestureHandle first"
                    .to_string(),
            ));
        }
        *sender = Some(tx);
    }

    // Initialize accumulator state
    {
        let mut state = GESTURE_STATE.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Gesture state mutex poisoned".to_string())
        })?;
        *state = Some(GestureAccumState {
            accum_x: 0,
            accum_y: 0,
            last_scroll_time: std::time::Instant::now(),
        });
    }

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("gesture-hook".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();

                // Ensure message queue exists before signalling init
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Install the low-level mouse hook on this thread
                let hook = match SetWindowsHookExW(
                    WH_MOUSE_LL,
                    Some(gesture_mouse_hook_proc),
                    None,
                    0,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "SetWindowsHookExW for gesture hook failed: {}",
                            e
                        ))));
                        return;
                    }
                };

                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WH_MOUSE_LL callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_LLHOOK_THREAD {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                let _ = UnhookWindowsHookEx(hook);
            }
        })
        .map_err(|e| {
            Win32Error::HookInstallFailed(format!("Failed to spawn gesture thread: {}", e))
        })?;

    // Wait for initialization
    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Gesture thread initialization failed".to_string())
    })??;

    tracing::info!("Gesture detection registered (low-level mouse hook)");

    Ok((
        GestureHandle {
            thread_id,
            thread: Some(thread),
        },
        rx,
    ))
}

/// Low-level mouse hook callback for gesture detection.
///
/// Handles WM_MOUSEWHEEL and WM_MOUSEHWHEEL to accumulate scroll deltas
/// and fire swipe gesture events when the threshold is exceeded.
unsafe extern "system" fn gesture_mouse_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if ncode >= 0 {
        let msg = wparam.0 as u32;
        if msg == WM_MOUSEHWHEEL || msg == WM_MOUSEWHEEL {
            let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);

            // The high word of mouseData contains the wheel delta (signed).
            let delta = (mouse_struct.mouseData >> 16) as i16 as i32;

            // Check if this is a physical mouse wheel event (not injected by
            // touchpad driver). Physical events with Ctrl+Alt held trigger
            // modifier+scroll navigation instead of touchpad gestures.
            const LLMHF_INJECTED: u32 = 0x01;
            let is_physical = mouse_struct.flags & LLMHF_INJECTED == 0;

            if is_physical {
                // Check if the configured scroll modifier keys are held
                const VK_CONTROL: i32 = 0x11;
                const VK_MENU: i32 = 0x12; // Alt
                const VK_SHIFT: i32 = 0x10;
                const VK_LWIN: i32 = 0x5B;
                const VK_RWIN: i32 = 0x5C;

                let flags = SCROLL_MODIFIER_FLAGS
                    .load(std::sync::atomic::Ordering::Relaxed);
                let mods_held = (flags & 0x01 == 0 || GetAsyncKeyState(VK_CONTROL) < 0)
                    && (flags & 0x02 == 0 || GetAsyncKeyState(VK_MENU) < 0)
                    && (flags & 0x04 == 0 || GetAsyncKeyState(VK_SHIFT) < 0)
                    && (flags & 0x08 == 0
                        || GetAsyncKeyState(VK_LWIN) < 0
                        || GetAsyncKeyState(VK_RWIN) < 0);

                if mods_held && flags != 0 && msg == WM_MOUSEWHEEL {
                    // Modifier+scroll: emit ScrollUp/ScrollDown per wheel notch
                    let event = if delta > 0 {
                        GestureEvent::ScrollUp
                    } else {
                        GestureEvent::ScrollDown
                    };
                    let sender_guard =
                        GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
                    if let Some(sender) = sender_guard.as_ref() {
                        let _ = sender.send(event);
                    }
                    // Consume the event so it doesn't scroll the window
                    return windows::Win32::Foundation::LRESULT(1);
                }

                // Physical mouse wheel without modifier — pass through
                return CallNextHookEx(None, ncode, wparam, lparam);
            }

            // Recover from mutex poisoning for both state and sender
            let mut state_guard = GESTURE_STATE.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(state) = state_guard.as_mut() {
                let now = std::time::Instant::now();

                // Reset accumulators if timeout exceeded
                if now.duration_since(state.last_scroll_time).as_millis() > GESTURE_TIMEOUT_MS {
                    state.accum_x = 0;
                    state.accum_y = 0;
                }
                state.last_scroll_time = now;

                if msg == WM_MOUSEHWHEEL {
                    state.accum_x += delta;
                } else {
                    state.accum_y += delta;
                }

                // Check thresholds and determine gesture
                let gesture = if state.accum_x.abs() >= GESTURE_SCROLL_THRESHOLD {
                    let g = if state.accum_x > 0 {
                        GestureEvent::SwipeRight
                    } else {
                        GestureEvent::SwipeLeft
                    };
                    state.accum_x = 0;
                    Some(g)
                } else if state.accum_y.abs() >= GESTURE_SCROLL_THRESHOLD {
                    let g = if state.accum_y > 0 {
                        GestureEvent::SwipeDown
                    } else {
                        GestureEvent::SwipeUp
                    };
                    state.accum_y = 0;
                    Some(g)
                } else {
                    None
                };

                if let Some(event) = gesture {
                    let sender_guard = GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
                    if let Some(sender) = sender_guard.as_ref() {
                        let _ = sender.send(event);
                    }
                }
            }
        }
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}
