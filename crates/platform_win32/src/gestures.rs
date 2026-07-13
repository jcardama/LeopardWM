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
const LLMHF_INJECTED: u32 = 0x01;

/// Threshold for accumulated wheel delta before firing a swipe gesture.
/// 3 * WHEEL_DELTA (120) = 360.
const GESTURE_SCROLL_THRESHOLD: i32 = 360;
const WHEEL_DELTA: i32 = 120;
const STREAM_INTERVAL_MS: u128 = 30;
const STREAM_END_MS: u128 = 80;
const NAVIGATION_COOLDOWN_MS: u128 = 150;
const SWIPE_COOLDOWN_MS: u128 = 200;

/// Timeout in milliseconds: if no swipe event arrives within this window,
/// partial accumulators are reset.
const GESTURE_TIMEOUT_MS: u128 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelMode {
    Discrete,
    Stream,
}

#[derive(Debug, Clone, Copy)]
struct WheelGestureInput {
    now_ms: u128,
    axis: WheelAxis,
    delta: i32,
    flags: u32,
    mods_held: bool,
    swipe_candidate: bool,
}

#[derive(Debug, Clone, Copy)]
struct StreamSummary {
    emitted: u32,
    suppressed: u32,
    duration_ms: u128,
    flags_seen: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct EngineTrace {
    mode_transition: Option<(WheelMode, WheelMode)>,
    stream_end: Option<StreamSummary>,
    cooldown_suppressed: bool,
    mode: Option<WheelMode>,
    accumulation: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct WheelGestureResult {
    event: Option<GestureEvent>,
    consume: bool,
    trace: EngineTrace,
}

struct WheelGestureEngine {
    navigation_mode: WheelMode,
    navigation_burst_count: u32,
    navigation_last_event_ms: Option<u128>,
    navigation_stream_started_ms: Option<u128>,
    navigation_accum: i32,
    navigation_cooldown_until_ms: Option<u128>,
    navigation_stream_emitted: u32,
    navigation_stream_suppressed: u32,
    navigation_stream_flags: u32,
    swipe_accum_x: i32,
    swipe_accum_y: i32,
    swipe_last_event_ms: Option<u128>,
    swipe_cooldown_x_until_ms: Option<u128>,
    swipe_cooldown_y_until_ms: Option<u128>,
}

impl WheelGestureEngine {
    fn new() -> Self {
        Self {
            navigation_mode: WheelMode::Discrete,
            navigation_burst_count: 0,
            navigation_last_event_ms: None,
            navigation_stream_started_ms: None,
            navigation_accum: 0,
            navigation_cooldown_until_ms: None,
            navigation_stream_emitted: 0,
            navigation_stream_suppressed: 0,
            navigation_stream_flags: 0,
            swipe_accum_x: 0,
            swipe_accum_y: 0,
            swipe_last_event_ms: None,
            swipe_cooldown_x_until_ms: None,
            swipe_cooldown_y_until_ms: None,
        }
    }

    fn process(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        if input.axis == WheelAxis::Vertical && input.mods_held {
            return self.process_navigation(input);
        }
        if input.swipe_candidate {
            return self.process_swipe(input);
        }
        WheelGestureResult {
            event: None,
            consume: false,
            trace: EngineTrace::default(),
        }
    }

    fn process_navigation(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        let mut trace = EngineTrace::default();
        if let Some(last_event_ms) = self.navigation_last_event_ms {
            let gap_ms = input.now_ms.saturating_sub(last_event_ms);
            if gap_ms > STREAM_END_MS {
                if self.navigation_mode == WheelMode::Stream {
                    trace.stream_end = Some(StreamSummary {
                        emitted: self.navigation_stream_emitted,
                        suppressed: self.navigation_stream_suppressed,
                        duration_ms: input.now_ms.saturating_sub(
                            self.navigation_stream_started_ms.unwrap_or(last_event_ms),
                        ),
                        flags_seen: self.navigation_stream_flags,
                    });
                    trace.mode_transition = Some((WheelMode::Stream, WheelMode::Discrete));
                }
                self.reset_navigation_stream();
                self.navigation_burst_count = 1;
            } else if gap_ms < STREAM_INTERVAL_MS {
                self.navigation_burst_count += 1;
            } else {
                self.navigation_burst_count = 1;
            }
        } else {
            self.navigation_burst_count = 1;
        }
        self.navigation_last_event_ms = Some(input.now_ms);

        if self.navigation_mode == WheelMode::Discrete && self.navigation_burst_count >= 3 {
            self.navigation_mode = WheelMode::Stream;
            self.navigation_stream_started_ms = Some(input.now_ms);
            self.navigation_stream_flags = input.flags;
            trace.mode_transition = Some((WheelMode::Discrete, WheelMode::Stream));
        }

        let event = if self.navigation_mode == WheelMode::Discrete {
            Some(scroll_event(input.delta))
        } else {
            self.navigation_stream_flags |= input.flags;
            self.navigation_accum += input.delta;
            if self.navigation_accum.abs() < WHEEL_DELTA {
                None
            } else if self
                .navigation_cooldown_until_ms
                .is_some_and(|until_ms| input.now_ms < until_ms)
            {
                self.navigation_accum = 0;
                self.navigation_stream_suppressed += 1;
                trace.cooldown_suppressed = true;
                None
            } else {
                let event = scroll_event(self.navigation_accum);
                self.navigation_accum -= self.navigation_accum.signum() * WHEEL_DELTA;
                self.navigation_cooldown_until_ms = Some(input.now_ms + NAVIGATION_COOLDOWN_MS);
                self.navigation_stream_emitted += 1;
                Some(event)
            }
        };

        trace.mode = Some(self.navigation_mode);
        trace.accumulation = Some(self.navigation_accum);
        WheelGestureResult {
            event,
            consume: true,
            trace,
        }
    }

    fn process_swipe(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        let mut trace = EngineTrace::default();
        if self.swipe_last_event_ms.is_some_and(|last_event_ms| {
            input.now_ms.saturating_sub(last_event_ms) > GESTURE_TIMEOUT_MS
        }) {
            self.swipe_accum_x = 0;
            self.swipe_accum_y = 0;
        }
        self.swipe_last_event_ms = Some(input.now_ms);

        let (accum, cooldown_until) = match input.axis {
            WheelAxis::Horizontal => (&mut self.swipe_accum_x, &mut self.swipe_cooldown_x_until_ms),
            WheelAxis::Vertical => (&mut self.swipe_accum_y, &mut self.swipe_cooldown_y_until_ms),
        };

        if cooldown_until.is_some_and(|until_ms| input.now_ms < until_ms) {
            *accum = 0;
            trace.cooldown_suppressed = true;
        } else {
            *accum += input.delta;
        }

        let event = if !trace.cooldown_suppressed && accum.abs() >= GESTURE_SCROLL_THRESHOLD {
            let event = match input.axis {
                WheelAxis::Horizontal if *accum > 0 => GestureEvent::SwipeRight,
                WheelAxis::Horizontal => GestureEvent::SwipeLeft,
                WheelAxis::Vertical if *accum > 0 => GestureEvent::SwipeDown,
                WheelAxis::Vertical => GestureEvent::SwipeUp,
            };
            *accum = 0;
            *cooldown_until = Some(input.now_ms + SWIPE_COOLDOWN_MS);
            Some(event)
        } else {
            None
        };

        trace.accumulation = Some(*accum);
        WheelGestureResult {
            event,
            consume: false,
            trace,
        }
    }

    fn reset_navigation_stream(&mut self) {
        self.navigation_mode = WheelMode::Discrete;
        self.navigation_accum = 0;
        self.navigation_cooldown_until_ms = None;
        self.navigation_stream_started_ms = None;
        self.navigation_stream_emitted = 0;
        self.navigation_stream_suppressed = 0;
        self.navigation_stream_flags = 0;
    }
}

fn scroll_event(delta: i32) -> GestureEvent {
    if delta > 0 {
        GestureEvent::ScrollUp
    } else {
        GestureEvent::ScrollDown
    }
}

/// Gesture accumulator state for the low-level mouse hook.
struct GestureAccumState {
    engine: WheelGestureEngine,
    started_at: std::time::Instant,
}

/// Modifier flags for scroll wheel navigation, stored as a bitmask.
/// Bit 0 = Ctrl, Bit 1 = Alt, Bit 2 = Shift, Bit 3 = Win.
static SCROLL_MODIFIER_FLAGS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0x03); // default: Ctrl + Alt

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
    tracing::debug!(
        "Scroll modifier set to: {} (flags=0x{:02x})",
        modifier_str,
        flags
    );
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
            engine: WheelGestureEngine::new(),
            started_at: std::time::Instant::now(),
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
                let hook =
                    match SetWindowsHookExW(WH_MOUSE_LL, Some(gesture_mouse_hook_proc), None, 0) {
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

fn scroll_modifiers_held(flags: u8) -> bool {
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_SHIFT: i32 = 0x10;
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;

    flags != 0
        && (flags & 0x01 == 0 || unsafe { GetAsyncKeyState(VK_CONTROL) } < 0)
        && (flags & 0x02 == 0 || unsafe { GetAsyncKeyState(VK_MENU) } < 0)
        && (flags & 0x04 == 0 || unsafe { GetAsyncKeyState(VK_SHIFT) } < 0)
        && (flags & 0x08 == 0
            || unsafe { GetAsyncKeyState(VK_LWIN) } < 0
            || unsafe { GetAsyncKeyState(VK_RWIN) } < 0)
}

fn send_gesture_event(event: GestureEvent) {
    let sender_guard = GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(sender) = sender_guard.as_ref() {
        let _ = sender.send(event);
    }
}

/// Low-level mouse hook callback for gesture detection.
///
/// Handles WM_MOUSEWHEEL and WM_MOUSEHWHEEL to normalize modifier navigation
/// and fire swipe gesture events when the threshold is exceeded.
unsafe extern "system" fn gesture_mouse_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if ncode >= 0 {
        let msg = wparam.0 as u32;
        let axis = match msg {
            WM_MOUSEHWHEEL => Some(WheelAxis::Horizontal),
            WM_MOUSEWHEEL => Some(WheelAxis::Vertical),
            _ => None,
        };
        if let Some(axis) = axis {
            let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let delta = (mouse_struct.mouseData >> 16) as i16 as i32;
            let modifier_flags = SCROLL_MODIFIER_FLAGS.load(std::sync::atomic::Ordering::Relaxed);
            let mods_held = axis == WheelAxis::Vertical && scroll_modifiers_held(modifier_flags);
            let swipe_candidate = mouse_struct.flags & LLMHF_INJECTED != 0;

            let mut state_guard = GESTURE_STATE.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(state) = state_guard.as_mut() {
                let result = state.engine.process(WheelGestureInput {
                    now_ms: state.started_at.elapsed().as_millis(),
                    axis,
                    delta,
                    flags: mouse_struct.flags,
                    mods_held,
                    swipe_candidate,
                });

                tracing::trace!(
                    ?axis,
                    delta,
                    hook_flags = mouse_struct.flags,
                    modifier_flags,
                    mods_held,
                    swipe_candidate,
                    mode = ?result.trace.mode,
                    accumulation = ?result.trace.accumulation,
                    consume = result.consume,
                    "Wheel hook decision"
                );
                if let Some((from, to)) = result.trace.mode_transition {
                    tracing::debug!(
                        ?from,
                        ?to,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel navigation mode changed"
                    );
                }
                if let Some(summary) = result.trace.stream_end {
                    tracing::debug!(
                        emitted = summary.emitted,
                        suppressed = summary.suppressed,
                        duration_ms = summary.duration_ms,
                        flags_seen = summary.flags_seen,
                        "Wheel navigation stream ended"
                    );
                }
                if result.trace.cooldown_suppressed {
                    tracing::debug!(
                        ?axis,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel gesture suppressed during cooldown"
                    );
                }
                if let Some(event) = result.event {
                    tracing::debug!(
                        ?event,
                        ?axis,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel gesture emitted"
                    );
                    send_gesture_event(event);
                }
                if result.consume {
                    return windows::Win32::Foundation::LRESULT(1);
                }
            }
        }
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        now_ms: u128,
        axis: WheelAxis,
        delta: i32,
        mods_held: bool,
        flags: u32,
    ) -> WheelGestureInput {
        WheelGestureInput {
            now_ms,
            axis,
            delta,
            flags,
            mods_held,
            swipe_candidate: flags & LLMHF_INJECTED != 0,
        }
    }

    #[test]
    fn modifier_notches_remain_one_to_one() {
        let mut engine = WheelGestureEngine::new();

        let up = engine.process(input(0, WheelAxis::Vertical, 120, true, 0));
        let down = engine.process(input(100, WheelAxis::Vertical, -120, true, 0));
        let passthrough = engine.process(input(200, WheelAxis::Vertical, 120, false, 0));

        assert_eq!(up.event, Some(GestureEvent::ScrollUp));
        assert!(up.consume);
        assert_eq!(down.event, Some(GestureEvent::ScrollDown));
        assert!(down.consume);
        assert_eq!(passthrough.event, None);
        assert!(!passthrough.consume);
    }

    #[test]
    fn modifier_stream_is_cooldown_bound() {
        let mut engine = WheelGestureEngine::new();
        let mut emitted = 0;

        for index in 0..20 {
            let result = engine.process(input(index * 10, WheelAxis::Vertical, 120, true, 0));
            assert!(result.consume);
            emitted += usize::from(result.event.is_some());
        }

        assert!(emitted < 6, "stream emitted {emitted} navigation actions");
    }

    #[test]
    fn navigation_stream_resets_after_pause() {
        let mut engine = WheelGestureEngine::new();
        for index in 0..3 {
            engine.process(input(index * 10, WheelAxis::Vertical, 40, true, 0));
        }

        let result = engine.process(input(200, WheelAxis::Vertical, 120, true, 0));

        assert_eq!(result.event, Some(GestureEvent::ScrollUp));
        assert_eq!(result.trace.stream_end.unwrap().emitted, 0);
    }

    #[test]
    fn swipe_refractory_limits_continuous_candidate() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));
        let suppressed = engine.process(input(10, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));
        let second = engine.process(input(210, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));

        assert_eq!(first.event, Some(GestureEvent::SwipeDown));
        assert!(!first.consume);
        assert_eq!(suppressed.event, None);
        assert!(suppressed.trace.cooldown_suppressed);
        assert_eq!(second.event, Some(GestureEvent::SwipeDown));
    }

    #[test]
    fn horizontal_swipe_accumulates_independently() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 240, false, LLMHF_INJECTED));
        let result = engine.process(input(
            10,
            WheelAxis::Horizontal,
            -360,
            false,
            LLMHF_INJECTED,
        ));

        assert_eq!(result.event, Some(GestureEvent::SwipeLeft));
        assert_eq!(engine.swipe_accum_y, 240);
    }

    #[test]
    fn non_candidate_unmodified_wheel_passes_through() {
        let mut engine = WheelGestureEngine::new();

        let result = engine.process(input(0, WheelAxis::Vertical, 120, false, 0));

        assert_eq!(result.event, None);
        assert!(!result.consume);
    }

    #[test]
    fn swipe_timeout_clears_partial_accumulation() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 240, false, LLMHF_INJECTED));
        let result = engine.process(input(
            GESTURE_TIMEOUT_MS + 1,
            WheelAxis::Vertical,
            120,
            false,
            LLMHF_INJECTED,
        ));

        assert_eq!(result.event, None);
    }

    #[test]
    fn injection_flag_does_not_change_navigation_stream_mode() {
        let mut unflagged = WheelGestureEngine::new();
        let mut injected = WheelGestureEngine::new();
        let mut unflagged_events = Vec::new();
        let mut injected_events = Vec::new();

        for index in 0..6 {
            unflagged_events.push(
                unflagged
                    .process(input(index * 10, WheelAxis::Vertical, 120, true, 0))
                    .event,
            );
            injected_events.push(
                injected
                    .process(input(
                        index * 10,
                        WheelAxis::Vertical,
                        120,
                        true,
                        LLMHF_INJECTED,
                    ))
                    .event,
            );
        }

        assert_eq!(unflagged_events, injected_events);
        assert_eq!(unflagged.navigation_mode, injected.navigation_mode);
    }
}
