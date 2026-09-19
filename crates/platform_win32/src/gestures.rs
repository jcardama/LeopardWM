//! Touchpad gesture detection via low-level mouse hook.

use crate::{recover_poisoned_mutex, Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};
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

impl GestureEvent {
    /// Stable capture vocabulary for this event. Avoids `Debug` quotes.
    pub fn as_diag_str(self) -> &'static str {
        match self {
            Self::SwipeLeft => "swipe_left",
            Self::SwipeRight => "swipe_right",
            Self::SwipeUp => "swipe_up",
            Self::SwipeDown => "swipe_down",
            Self::ScrollUp => "scroll_up",
            Self::ScrollDown => "scroll_down",
        }
    }
}

/// Dedicated tracing target for opt-in gesture diagnostic capture.
pub const GESTURE_DIAG_TARGET: &str = "leopardwm::gesture_diag";
pub const GESTURE_DIAG_STAGE_HOOK_DELIVERY: &str = "hook_delivery";
pub const GESTURE_DIAG_STAGE_CLASSIFIER: &str = "classifier";
pub const GESTURE_DIAG_STAGE_ACCUMULATION: &str = "accumulation";
pub const GESTURE_DIAG_STAGE_TIMEOUT: &str = "timeout";
pub const GESTURE_DIAG_STAGE_COOLDOWN: &str = "cooldown";
pub const GESTURE_DIAG_STAGE_RECOGNIZED: &str = "recognized";
pub const GESTURE_DIAG_STAGE_DISPATCH: &str = "dispatch";
pub const GESTURE_DIAG_STAGE_REGISTRATION: &str = "registration";

const GESTURE_DIAGNOSTIC_GATE_OPEN: u64 = 1;
const GESTURE_DIAGNOSTIC_ADMISSION_STEP: u64 = 2;

// Bit 0 is the capture gate; the remaining bits count admitted emitters. Closing
// clears the gate with one atomic operation, so no emitter can be admitted after
// the worker snapshots the count for its bounded summary.
static GESTURE_DIAGNOSTIC_CAPTURE_STATE: AtomicU64 = AtomicU64::new(0);

/// Keeps an already-admitted diagnostic emitter visible to capture shutdown.
///
/// `admit_gesture_diagnostic_capture()` is the only source, so the admission
/// count cannot be decremented by work that was never admitted. Neither the
/// original unit form nor a field literal can reconstruct it externally:
///
/// ```compile_fail
/// use leopardwm_platform_win32::GestureDiagnosticAdmission;
///
/// let _forged = GestureDiagnosticAdmission;
/// ```
///
/// ```compile_fail
/// use leopardwm_platform_win32::GestureDiagnosticAdmission;
///
/// let _forged = GestureDiagnosticAdmission { _private: () };
/// ```
pub struct GestureDiagnosticAdmission {
    _private: (),
}

impl Drop for GestureDiagnosticAdmission {
    fn drop(&mut self) {
        GESTURE_DIAGNOSTIC_CAPTURE_STATE
            .fetch_sub(GESTURE_DIAGNOSTIC_ADMISSION_STEP, Ordering::Release);
    }
}

/// Enables trace construction for the bounded daemon diagnostic capture.
pub fn begin_gesture_diagnostic_capture() {
    GESTURE_DIAGNOSTIC_CAPTURE_STATE.fetch_or(GESTURE_DIAGNOSTIC_GATE_OPEN, Ordering::Release);
}

/// Stops new trace construction and returns emitters admitted at closure.
pub fn end_gesture_diagnostic_capture() -> u64 {
    GESTURE_DIAGNOSTIC_CAPTURE_STATE.fetch_and(!GESTURE_DIAGNOSTIC_GATE_OPEN, Ordering::AcqRel)
        / GESTURE_DIAGNOSTIC_ADMISSION_STEP
}

/// Attempts to admit a diagnostic emitter without waiting on capture I/O.
pub fn admit_gesture_diagnostic_capture() -> Option<GestureDiagnosticAdmission> {
    let mut state = GESTURE_DIAGNOSTIC_CAPTURE_STATE.load(Ordering::Acquire);
    loop {
        if state & GESTURE_DIAGNOSTIC_GATE_OPEN == 0 {
            return None;
        }
        match GESTURE_DIAGNOSTIC_CAPTURE_STATE.compare_exchange_weak(
            state,
            state + GESTURE_DIAGNOSTIC_ADMISSION_STEP,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(GestureDiagnosticAdmission { _private: () }),
            Err(next) => state = next,
        }
    }
}

/// Wheel message constants (not all exposed by windows-rs).
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const LLMHF_INJECTED: u32 = 0x01;

/// Threshold for accumulated wheel delta before firing a swipe gesture.
/// 3 * WHEEL_DELTA (120) = 360.
const GESTURE_SCROLL_THRESHOLD: i32 = 360;
const WHEEL_DELTA: i32 = 120;
const STREAM_END_MS: u128 = 80;
const NAVIGATION_COOLDOWN_MS: u128 = 150;

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

impl WheelAxis {
    fn as_diag_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassifierKind {
    Pass,
    Reject,
}

impl ClassifierKind {
    fn as_diag_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Reject => "reject",
        }
    }
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
    classifier: Option<ClassifierKind>,
    timeout_reset: bool,
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
            trace: EngineTrace {
                classifier: Some(ClassifierKind::Reject),
                ..EngineTrace::default()
            },
        }
    }

    fn process_navigation(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        if input.delta == 0 {
            return WheelGestureResult {
                event: None,
                consume: false,
                trace: EngineTrace {
                    classifier: Some(ClassifierKind::Reject),
                    ..EngineTrace::default()
                },
            };
        }

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
            } else {
                self.navigation_burst_count += 1;
            }
        } else {
            self.navigation_burst_count = 1;
        }
        self.navigation_last_event_ms = Some(input.now_ms);
        self.navigation_stream_flags |= input.flags;

        if self.navigation_mode == WheelMode::Discrete && self.navigation_burst_count >= 2 {
            self.navigation_mode = WheelMode::Stream;
            self.navigation_stream_started_ms = Some(input.now_ms);
            trace.mode_transition = Some((WheelMode::Discrete, WheelMode::Stream));
        }

        let event = if self
            .navigation_cooldown_until_ms
            .is_some_and(|until_ms| input.now_ms < until_ms)
        {
            if self.navigation_mode == WheelMode::Stream {
                self.navigation_stream_suppressed += 1;
                trace.cooldown_suppressed = true;
            }
            None
        } else {
            self.navigation_accum += input.delta;
            if self.navigation_accum.abs() < WHEEL_DELTA {
                None
            } else {
                let event = scroll_event(self.navigation_accum);
                self.navigation_accum %= WHEEL_DELTA;
                self.navigation_cooldown_until_ms = Some(input.now_ms + NAVIGATION_COOLDOWN_MS);
                if self.navigation_mode == WheelMode::Stream {
                    self.navigation_stream_emitted += 1;
                }
                Some(event)
            }
        };

        trace.mode = Some(self.navigation_mode);
        trace.accumulation = Some(self.navigation_accum);
        trace.classifier = Some(ClassifierKind::Pass);
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
            trace.timeout_reset = true;
        }
        self.swipe_last_event_ms = Some(input.now_ms);

        let accum = match input.axis {
            WheelAxis::Horizontal => &mut self.swipe_accum_x,
            WheelAxis::Vertical => &mut self.swipe_accum_y,
        };
        *accum += input.delta;

        let event = if accum.abs() >= GESTURE_SCROLL_THRESHOLD {
            let event = match input.axis {
                WheelAxis::Horizontal if *accum > 0 => GestureEvent::SwipeRight,
                WheelAxis::Horizontal => GestureEvent::SwipeLeft,
                WheelAxis::Vertical if *accum > 0 => GestureEvent::SwipeDown,
                WheelAxis::Vertical => GestureEvent::SwipeUp,
            };
            *accum = 0;
            Some(event)
        } else {
            None
        };

        trace.accumulation = Some(*accum);
        trace.classifier = Some(ClassifierKind::Pass);
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
        emit_gesture_registration("stopped");
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

/// Emit a registration-stage record on the dedicated diagnostic target.
pub fn emit_gesture_registration(state: &'static str) {
    let Some(_admission) = admit_gesture_diagnostic_capture() else {
        return;
    };
    emit_gesture_registration_active(state);
}

fn emit_gesture_registration_active(state: &'static str) {
    tracing::trace!(
        target: GESTURE_DIAG_TARGET,
        stage = GESTURE_DIAG_STAGE_REGISTRATION,
        state,
    );
}

fn emit_wheel_diagnostics(
    axis: WheelAxis,
    delta: i32,
    flags: u32,
    mods_held: bool,
    swipe_candidate: bool,
    result: &WheelGestureResult,
) {
    let Some(_admission) = admit_gesture_diagnostic_capture() else {
        return;
    };
    emit_wheel_diagnostics_active(axis, delta, flags, mods_held, swipe_candidate, result);
}

fn emit_wheel_diagnostics_active(
    axis: WheelAxis,
    delta: i32,
    flags: u32,
    mods_held: bool,
    swipe_candidate: bool,
    result: &WheelGestureResult,
) {
    tracing::trace!(
        target: GESTURE_DIAG_TARGET,
        stage = GESTURE_DIAG_STAGE_HOOK_DELIVERY,
        axis = axis.as_diag_str(),
        delta,
        flags,
        mods_held,
        swipe_candidate,
    );
    if let Some(kind) = result.trace.classifier {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_CLASSIFIER,
            outcome = kind.as_diag_str(),
        );
    }
    if let Some(accumulation) = result.trace.accumulation {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_ACCUMULATION,
            accumulation,
        );
    }
    if result.trace.timeout_reset {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_TIMEOUT,
        );
    }
    if result.trace.cooldown_suppressed {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_COOLDOWN,
        );
    }
    if let Some(event) = result.event {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_RECOGNIZED,
            event = event.as_diag_str(),
        );
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

                emit_wheel_diagnostics(
                    axis,
                    delta,
                    mouse_struct.flags,
                    mods_held,
                    swipe_candidate,
                    &result,
                );

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
                    tracing::trace!(
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

    static DIAGNOSTIC_GATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn modifier_stream_at_40ms_is_cooldown_bound() {
        let mut engine = WheelGestureEngine::new();
        let mut emitted = 0;

        for index in 0..9 {
            let result = engine.process(input(index * 40, WheelAxis::Vertical, 120, true, 0));
            assert!(result.consume);
            emitted += usize::from(result.event.is_some());
        }

        assert_eq!(emitted, 3);
    }

    #[test]
    fn partial_ticks_do_not_emit_before_reaching_a_notch() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 10, true, 0));
        let second = engine.process(input(10, WheelAxis::Vertical, 10, true, 0));
        let third = engine.process(input(20, WheelAxis::Vertical, 10, true, 0));

        assert_eq!(first.event, None);
        assert_eq!(second.event, None);
        assert_eq!(third.event, None);
        assert_eq!(engine.navigation_mode, WheelMode::Stream);
        assert_eq!(engine.navigation_accum, 30);
    }

    #[test]
    fn navigation_stream_retains_only_partial_remainder_during_cooldown() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 240, true, 0));
        assert_eq!(first.event, Some(GestureEvent::ScrollUp));
        assert_eq!(engine.navigation_accum, 0);

        for now_ms in [40, 80, 120] {
            let suppressed = engine.process(input(now_ms, WheelAxis::Vertical, 1, true, 0));
            assert_eq!(suppressed.event, None);
            assert!(suppressed.trace.cooldown_suppressed);
            assert_eq!(engine.navigation_accum, 0);
        }
        let residual = engine.process(input(160, WheelAxis::Vertical, 1, true, 0));
        assert_eq!(residual.event, None);
        assert_eq!(engine.navigation_accum, 1);

        let mut multi_notch_engine = WheelGestureEngine::new();
        let multi_notch = multi_notch_engine.process(input(0, WheelAxis::Vertical, 250, true, 0));
        assert_eq!(multi_notch.event, Some(GestureEvent::ScrollUp));
        assert_eq!(multi_notch_engine.navigation_accum, 10);

        let partial_suppressed =
            multi_notch_engine.process(input(40, WheelAxis::Vertical, 1, true, 0));
        assert_eq!(partial_suppressed.event, None);
        assert!(partial_suppressed.trace.cooldown_suppressed);
        assert_eq!(multi_notch_engine.navigation_accum, 10);
    }

    #[test]
    fn navigation_sign_reversal_cancels_partial_accumulation() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 80, true, 0));
        let result = engine.process(input(40, WheelAxis::Vertical, -80, true, 0));

        assert_eq!(result.event, None);
        assert_eq!(engine.navigation_accum, 0);
    }

    #[test]
    fn zero_delta_navigation_passes_through() {
        let mut engine = WheelGestureEngine::new();

        let result = engine.process(input(0, WheelAxis::Vertical, 0, true, 0));

        assert_eq!(result.event, None);
        assert!(!result.consume);
        assert_eq!(engine.navigation_last_event_ms, None);
    }

    #[test]
    fn navigation_stream_resets_after_pause() {
        let mut engine = WheelGestureEngine::new();
        for index in 0..3 {
            engine.process(input(index * 10, WheelAxis::Vertical, 40, true, 0));
        }

        let result = engine.process(input(200, WheelAxis::Vertical, 120, true, 0));

        assert_eq!(result.event, Some(GestureEvent::ScrollUp));
        assert_eq!(result.trace.stream_end.unwrap().emitted, 1);
    }

    #[test]
    fn rapid_swipes_remain_independent() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));
        let second = engine.process(input(10, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));

        assert_eq!(first.event, Some(GestureEvent::SwipeDown));
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
                    .process(input(index * 40, WheelAxis::Vertical, 120, true, 0))
                    .event,
            );
            injected_events.push(
                injected
                    .process(input(
                        index * 40,
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

    fn capture_diag(emit: impl FnOnce()) -> Vec<String> {
        use std::sync::{Arc, Mutex};
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = DiagSubscriber {
            events: events.clone(),
        };
        tracing::subscriber::with_default(subscriber, emit);
        let lines = events.lock().unwrap().clone();
        lines
    }

    fn process_and_capture(
        engine: &mut WheelGestureEngine,
        sample: WheelGestureInput,
    ) -> (WheelGestureResult, Vec<String>) {
        let _gate_guard = DIAGNOSTIC_GATE_TEST_LOCK.lock().unwrap();
        let result = engine.process(sample);
        begin_gesture_diagnostic_capture();
        let lines = capture_diag(|| {
            emit_wheel_diagnostics(
                sample.axis,
                sample.delta,
                sample.flags,
                sample.mods_held,
                sample.swipe_candidate,
                &result,
            );
        });
        end_gesture_diagnostic_capture();
        (result, lines)
    }

    #[derive(Default)]
    struct DiagLine {
        stage: Option<String>,
        fields: Vec<(String, String)>,
    }

    impl DiagLine {
        fn rendered(&self) -> String {
            let mut line = format!("stage={}", self.stage.as_deref().unwrap_or(""));
            for (key, value) in &self.fields {
                line.push(' ');
                line.push_str(key);
                line.push('=');
                line.push_str(value);
            }
            line
        }
    }

    struct DiagVisitor<'a>(&'a mut DiagLine);

    impl tracing::field::Visit for DiagVisitor<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "stage" {
                self.0.stage = Some(value.to_string());
            } else if field.name() != "message" {
                self.0
                    .fields
                    .push((field.name().to_string(), value.to_string()));
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.record_str(field, &format!("{value:?}"));
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0
                .fields
                .push((field.name().to_string(), value.to_string()));
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0
                .fields
                .push((field.name().to_string(), value.to_string()));
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.fields.push((
                field.name().to_string(),
                if value { "true" } else { "false" }.to_string(),
            ));
        }
    }

    struct DiagSubscriber {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for DiagSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == GESTURE_DIAG_TARGET
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut line = DiagLine::default();
            event.record(&mut DiagVisitor(&mut line));
            self.events.lock().unwrap().push(line.rendered());
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn passthrough_emits_hook_delivery_and_classifier_reject() {
        let mut engine = WheelGestureEngine::new();
        let (result, lines) =
            process_and_capture(&mut engine, input(0, WheelAxis::Vertical, 120, false, 0));
        assert_eq!(result.event, None);
        assert!(!result.consume);
        assert!(lines.iter().any(|line| line.contains("stage=hook_delivery")
            && line.contains("axis=vertical")
            && line.contains("delta=120")
            && line.contains("flags=0")
            && line.contains("mods_held=false")
            && line.contains("swipe_candidate=false")));
        assert!(lines
            .iter()
            .any(|line| line.contains("stage=classifier") && line.contains("outcome=reject")));
        assert!(!lines.iter().any(|line| line.contains("stage=recognized")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("hwnd") || line.contains("title")));
    }

    #[test]
    fn injected_swipe_emits_pass_accumulation_and_recognized() {
        let mut engine = WheelGestureEngine::new();
        let (result, lines) = process_and_capture(
            &mut engine,
            input(0, WheelAxis::Horizontal, -360, false, LLMHF_INJECTED),
        );
        assert_eq!(result.event, Some(GestureEvent::SwipeLeft));
        assert!(!result.consume);
        assert!(lines
            .iter()
            .any(|line| line.contains("swipe_candidate=true") && line.contains("flags=1")));
        assert!(lines
            .iter()
            .any(|line| line.contains("stage=classifier") && line.contains("outcome=pass")));
        assert!(lines.iter().any(|line| line.contains("stage=accumulation")));
        assert!(lines
            .iter()
            .any(|line| line.contains("stage=recognized") && line.contains("event=swipe_left")));
    }

    #[test]
    fn swipe_timeout_emits_timeout_stage() {
        let mut engine = WheelGestureEngine::new();
        engine.process(input(0, WheelAxis::Vertical, 240, false, LLMHF_INJECTED));
        let (result, lines) = process_and_capture(
            &mut engine,
            input(
                GESTURE_TIMEOUT_MS + 1,
                WheelAxis::Vertical,
                120,
                false,
                LLMHF_INJECTED,
            ),
        );
        assert_eq!(result.event, None);
        assert!(result.trace.timeout_reset);
        assert!(lines.iter().any(|line| line.contains("stage=timeout")));
        assert!(lines
            .iter()
            .any(|line| line.contains("stage=classifier") && line.contains("outcome=pass")));
    }

    #[test]
    fn navigation_cooldown_emits_cooldown_stage() {
        let mut engine = WheelGestureEngine::new();
        let (first, first_lines) =
            process_and_capture(&mut engine, input(0, WheelAxis::Vertical, 120, true, 0));
        assert_eq!(first.event, Some(GestureEvent::ScrollUp));
        assert!(first_lines
            .iter()
            .any(|line| line.contains("stage=recognized") && line.contains("event=scroll_up")));
        let (suppressed, lines) =
            process_and_capture(&mut engine, input(40, WheelAxis::Vertical, 120, true, 0));
        assert_eq!(suppressed.event, None);
        assert!(suppressed.trace.cooldown_suppressed);
        assert!(lines.iter().any(|line| line.contains("stage=cooldown")));
        assert!(lines
            .iter()
            .any(|line| line.contains("mods_held=true") && line.contains("swipe_candidate=false")));
    }

    #[test]
    fn capture_gate_controls_production_registration_emitter() {
        let _gate_guard = DIAGNOSTIC_GATE_TEST_LOCK.lock().unwrap();
        end_gesture_diagnostic_capture();
        let closed = capture_diag(|| emit_gesture_registration("registered"));
        assert!(closed.is_empty());

        begin_gesture_diagnostic_capture();
        let open = capture_diag(|| emit_gesture_registration("registered"));
        end_gesture_diagnostic_capture();
        assert_eq!(open, vec!["stage=registration state=registered"]);
    }

    #[test]
    fn capture_gate_controls_production_wheel_emitter() {
        let _gate_guard = DIAGNOSTIC_GATE_TEST_LOCK.lock().unwrap();
        let mut engine = WheelGestureEngine::new();
        let sample = input(0, WheelAxis::Vertical, 120, false, 0);
        let result = engine.process(sample);

        end_gesture_diagnostic_capture();
        let closed = capture_diag(|| {
            emit_wheel_diagnostics(
                sample.axis,
                sample.delta,
                sample.flags,
                sample.mods_held,
                sample.swipe_candidate,
                &result,
            );
        });
        assert!(closed.is_empty());

        begin_gesture_diagnostic_capture();
        let open = capture_diag(|| {
            emit_wheel_diagnostics(
                sample.axis,
                sample.delta,
                sample.flags,
                sample.mods_held,
                sample.swipe_candidate,
                &result,
            );
        });
        end_gesture_diagnostic_capture();
        assert!(open.iter().any(|line| line.contains("stage=hook_delivery")));
    }

    #[test]
    fn registration_emit_uses_fixed_vocabulary() {
        let lines = capture_diag(|| {
            emit_gesture_registration_active("disabled");
            emit_gesture_registration_active("failed");
            emit_gesture_registration_active("registered");
            emit_gesture_registration_active("stopped");
        });
        assert_eq!(
            lines,
            vec![
                "stage=registration state=disabled".to_string(),
                "stage=registration state=failed".to_string(),
                "stage=registration state=registered".to_string(),
                "stage=registration state=stopped".to_string(),
            ]
        );
    }
}
