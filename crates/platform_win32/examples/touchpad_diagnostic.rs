//! Developer-only diagnostic for synthetic Windows touchpad gestures.

use std::{
    mem::size_of,
    sync::{mpsc, Mutex},
    thread,
    time::Duration,
};
use windows::{
    core::{w, PCSTR},
    Win32::{
        Foundation::{LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Gdi::HMONITOR,
        System::{
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Threading::GetCurrentThreadId,
        },
        UI::{
            Controls::{
                DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_MODE,
                POINTER_FEEDBACK_NONE, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
            },
            Input::{
                KeyboardAndMouse::{
                    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
                },
                Pointer::{
                    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN,
                    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP,
                    POINTER_FLAG_UPDATE, POINTER_TOUCH_INFO,
                },
            },
            WindowsAndMessaging::{
                CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
                SetWindowsHookExW, UnhookWindowsHookEx, LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED,
                MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, POINTER_INPUT_TYPE, PT_TOUCHPAD, WH_MOUSE_LL,
            },
        },
    },
};

const CONTACTS: usize = 3;
const WIDTH: u32 = 10_000;
const HEIGHT: u32 = 6_000;
const CENTER: (i32, i32) = (5_000, 3_000);
const CONTACT_SPACING: i32 = 500;
const TRAVEL: i32 = 2_400;
const STEPS: usize = 5;
const FRAME_DELAY: Duration = Duration::from_millis(16);
const SETTLE_DELAY: Duration = Duration::from_millis(250);
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_QUIT_DIAGNOSTIC: u32 = 0x8000 + 102;

static WHEEL_SENDER: Mutex<Option<mpsc::Sender<WheelEvent>>> = Mutex::new(None);

#[repr(transparent)]
#[derive(Clone, Copy)]
struct SyntheticDeviceCreationOptions(u32);

const SDCO_PHYSICAL_SIZE: SyntheticDeviceCreationOptions = SyntheticDeviceCreationOptions(1);

#[repr(C)]
struct SyntheticDeviceCreationParams {
    pointer_type: POINTER_INPUT_TYPE,
    max_count: u32,
    feedback_mode: POINTER_FEEDBACK_MODE,
    hmonitor: HMONITOR,
    device_width: u32,
    device_height: u32,
    options: SyntheticDeviceCreationOptions,
}

type CreateSyntheticPointerDevice2 =
    unsafe extern "system" fn(*const SyntheticDeviceCreationParams) -> HSYNTHETICPOINTERDEVICE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    fn expected_axis(self) -> WheelAxis {
        match self {
            Self::Left | Self::Right => WheelAxis::Horizontal,
            Self::Up | Self::Down => WheelAxis::Vertical,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    directions: Vec<Direction>,
    dry_run: bool,
    yes: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let (mut direction, mut dry_run, mut yes) = (None, false, false);
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--dry-run" => dry_run = true,
            "--yes" => yes = true,
            "all" if direction.is_none() => direction = Some(arg),
            _ if direction.is_none() && Direction::parse(&arg).is_some() => direction = Some(arg),
            _ => return Err(format!("unrecognized or duplicate argument: {arg}")),
        }
    }
    let directions = match direction.as_deref() {
        Some("all") => vec![
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ],
        Some(value) => vec![Direction::parse(value).expect("validated direction")],
        None => return Err("a direction is required".into()),
    };
    Ok(Some(Options {
        directions,
        dry_run,
        yes,
    }))
}

fn usage() {
    println!("Usage: cargo run -p leopardwm-platform-win32 --example touchpad_diagnostic -- <left|right|up|down|all> [--dry-run] [--yes]");
    println!("  --dry-run  Resolve and observe only; never call SendInput or InjectSyntheticPointerInput.");
    println!("  --yes      Skip the real-run warning countdown.");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug)]
struct WheelEvent {
    axis: WheelAxis,
    delta: i32,
    flags: u32,
    time: u32,
    point: POINT,
}

impl WheelEvent {
    fn injected(self) -> bool {
        self.flags & LLMHF_INJECTED != 0
    }

    fn lower_il(self) -> bool {
        self.flags & LLMHF_LOWER_IL_INJECTED != 0
    }
}

fn signed_delta(mouse_data: u32) -> i32 {
    (mouse_data >> 16) as i16 as i32
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let axis = match wparam.0 as u32 {
            WM_MOUSEWHEEL => Some(WheelAxis::Vertical),
            WM_MOUSEHWHEEL => Some(WheelAxis::Horizontal),
            _ => None,
        };
        if let Some(axis) = axis {
            // WH_MOUSE_LL supplies this pointer for non-negative hook codes.
            let mouse = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let event = WheelEvent {
                axis,
                delta: signed_delta(mouse.mouseData),
                flags: mouse.flags,
                time: mouse.time,
                point: mouse.pt,
            };
            if let Ok(sender) = WHEEL_SENDER.lock() {
                if let Some(sender) = sender.as_ref() {
                    let _ = sender.send(event);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct WheelHook {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl WheelHook {
    fn install() -> Result<(Self, mpsc::Receiver<WheelEvent>), String> {
        let (sender, receiver) = mpsc::channel();
        let mut shared = WHEEL_SENDER
            .lock()
            .map_err(|_| "wheel observer mutex is poisoned")?;
        if shared.is_some() {
            return Err("a touchpad diagnostic observer is already running".into());
        }
        *shared = Some(sender);
        drop(shared);

        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("touchpad-diagnostic-hook".into())
            .spawn(move || unsafe {
                let thread_id = GetCurrentThreadId();
                let mut message = MSG::default();
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("SetWindowsHookExW failed: {error}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(thread_id));
                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    if message.message == WM_QUIT_DIAGNOSTIC {
                        break;
                    }
                    let _ = DispatchMessageW(&message);
                }
                let _ = UnhookWindowsHookEx(hook);
            })
            .map_err(|error| format!("failed to start wheel observer thread: {error}"))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok((
                Self {
                    thread_id,
                    thread: Some(thread),
                },
                receiver,
            )),
            Ok(Err(error)) => {
                let _ = thread.join();
                clear_sender();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                clear_sender();
                Err("wheel observer thread exited before initialization".into())
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        if self.thread.is_none() {
            return Ok(());
        }
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT_DIAGNOSTIC, WPARAM(0), LPARAM(0)) }
            .map_err(|error| format!("failed to stop wheel observer: {error}"))?;
        self.thread
            .take()
            .expect("live hook thread")
            .join()
            .map_err(|_| "wheel observer thread panicked")?;
        clear_sender();
        Ok(())
    }
}

impl Drop for WheelHook {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn clear_sender() {
    if let Ok(mut sender) = WHEEL_SENDER.lock() {
        *sender = None;
    }
}

struct SyntheticDevice(Option<HSYNTHETICPOINTERDEVICE>);

impl SyntheticDevice {
    fn create(create: CreateSyntheticPointerDevice2) -> Result<Self, String> {
        let params = SyntheticDeviceCreationParams {
            pointer_type: PT_TOUCHPAD,
            max_count: CONTACTS as u32,
            feedback_mode: POINTER_FEEDBACK_NONE,
            hmonitor: HMONITOR(std::ptr::null_mut()),
            device_width: WIDTH,
            device_height: HEIGHT,
            options: SDCO_PHYSICAL_SIZE,
        };
        // This dynamic function is called only after its User32 export is resolved.
        let device = unsafe { create(&params) };
        (!device.is_invalid())
            .then_some(Self(Some(device)))
            .ok_or_else(|| {
                format!(
                    "CreateSyntheticPointerDevice2 failed: {}",
                    windows::core::Error::from_thread()
                )
            })
    }

    fn handle(&self) -> HSYNTHETICPOINTERDEVICE {
        self.0.expect("synthetic device is live until drop")
    }
}

impl Drop for SyntheticDevice {
    fn drop(&mut self) {
        if let Some(device) = self.0.take() {
            unsafe { DestroySyntheticPointerDevice(device) };
        }
    }
}

fn resolve_create_device() -> Result<CreateSyntheticPointerDevice2, String> {
    let user32 = unsafe { GetModuleHandleW(w!("user32.dll")) }
        .map_err(|error| format!("could not locate user32.dll: {error}"))?;
    let proc = unsafe {
        GetProcAddress(
            user32,
            PCSTR(c"CreateSyntheticPointerDevice2".as_ptr().cast()),
        )
    }
    .ok_or(
        "CreateSyntheticPointerDevice2 is not exported by this user32.dll; Windows 11 is required",
    )?;
    // GetProcAddress returns an untyped system ABI function pointer.
    Ok(unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateSyntheticPointerDevice2>(
            proc,
        )
    })
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Down,
    Update,
    Up,
}

impl Phase {
    fn flags(self) -> POINTER_FLAGS {
        match self {
            Self::Down => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            Self::Update => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            Self::Up => POINTER_FLAG_UP,
        }
    }
}

fn offsets(direction: Direction) -> [(i32, i32); STEPS + 1] {
    std::array::from_fn(|step| {
        let distance = TRAVEL * step as i32 / STEPS as i32;
        match direction {
            Direction::Left => (-distance, 0),
            Direction::Right => (distance, 0),
            Direction::Up => (0, -distance),
            Direction::Down => (0, distance),
        }
    })
}

fn point(offset: (i32, i32), contact: usize) -> POINT {
    POINT {
        x: CENTER.0 + offset.0 + (contact as i32 - 1) * CONTACT_SPACING,
        y: CENTER.1 + offset.1,
    }
}

fn inject_frame(
    device: HSYNTHETICPOINTERDEVICE,
    phase: Phase,
    offset: (i32, i32),
) -> Result<(), String> {
    let contacts: Vec<_> = (0..CONTACTS)
        .map(|contact| {
            let position = point(offset, contact);
            POINTER_TYPE_INFO {
                r#type: PT_TOUCHPAD,
                Anonymous: POINTER_TYPE_INFO_0 {
                    touchInfo: POINTER_TOUCH_INFO {
                        pointerInfo: windows::Win32::UI::Input::Pointer::POINTER_INFO {
                            pointerType: PT_TOUCHPAD,
                            pointerId: contact as u32 + 1,
                            pointerFlags: phase.flags(),
                            ptHimetricLocation: position,
                            ptHimetricLocationRaw: position,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
            }
        })
        .collect();
    unsafe { InjectSyntheticPointerInput(device, &contacts) }
        .map_err(|error| format!("InjectSyntheticPointerInput ({phase:?}) failed: {error}"))
}

fn print_plan(direction: Direction) {
    println!(
        "planned {} swipe ({} contacts):",
        direction.name(),
        CONTACTS
    );
    let frames = offsets(direction);
    for (index, offset) in frames.into_iter().enumerate() {
        let phase = if index == 0 {
            Phase::Down
        } else {
            Phase::Update
        };
        let points = [point(offset, 0), point(offset, 1), point(offset, 2)];
        println!(
            "  frame {index} {phase:?} offset=({}, {}) contacts=({}, {}), ({}, {}), ({}, {})",
            offset.0,
            offset.1,
            points[0].x,
            points[0].y,
            points[1].x,
            points[1].y,
            points[2].x,
            points[2].y
        );
    }
    let offset = *offsets(direction)
        .last()
        .expect("fixed frames are nonempty");
    let points = [point(offset, 0), point(offset, 1), point(offset, 2)];
    println!(
        "  frame {} Up offset=({}, {}) contacts=({}, {}), ({}, {}), ({}, {})",
        STEPS + 1,
        offset.0,
        offset.1,
        points[0].x,
        points[0].y,
        points[1].x,
        points[1].y,
        points[2].x,
        points[2].y
    );
}

fn inject_swipe(device: HSYNTHETICPOINTERDEVICE, direction: Direction) -> Result<(), String> {
    let frames = offsets(direction);
    for (index, offset) in frames.into_iter().enumerate() {
        inject_frame(
            device,
            if index == 0 {
                Phase::Down
            } else {
                Phase::Update
            },
            offset,
        )?;
        thread::sleep(FRAME_DELAY);
    }
    inject_frame(
        device,
        Phase::Up,
        *frames.last().expect("fixed frames are nonempty"),
    )
}

fn send_positive_control() -> Result<(), String> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                mouseData: 120,
                dwFlags: MOUSEEVENTF_WHEEL,
                ..Default::default()
            },
        },
    };
    (unsafe { SendInput(&[input], size_of::<INPUT>() as i32) } == 1)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "SendInput wheel positive control failed: {}",
                windows::core::Error::from_thread()
            )
        })
}

fn print_events(phase: &str, events: &[WheelEvent]) {
    if events.is_empty() {
        println!("phase={phase} event_count=0");
    }
    for (index, event) in events.iter().enumerate() {
        let kind = if event.axis == WheelAxis::Vertical {
            "WM_MOUSEWHEEL"
        } else {
            "WM_MOUSEHWHEEL"
        };
        println!("phase={phase} event={} kind={kind} delta={} flags=0x{:08X} injected={} lower_il_injected={} time={} point=({}, {})", index + 1, event.delta, event.flags, event.injected(), event.lower_il(), event.time, event.point.x, event.point.y);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Expected,
    PositiveControlFailure,
    NoTouchpadWheelEvents,
    UnexpectedAxis,
    InjectedFlagMismatch,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Self::Expected => "expected",
            Self::PositiveControlFailure => "positive-control failure",
            Self::NoTouchpadWheelEvents => "no touchpad wheel events",
            Self::UnexpectedAxis => "unexpected axis",
            Self::InjectedFlagMismatch => "injected-flag mismatch",
        }
    }
}

fn matches_axis(event: WheelEvent, direction: Direction) -> bool {
    event.axis == direction.expected_axis()
}

fn classify(positive: &[WheelEvent], observations: &[(Direction, Vec<WheelEvent>)]) -> Verdict {
    if !positive.iter().copied().any(|event| {
        event.axis == WheelAxis::Vertical
            && event.delta == 120
            && event.injected()
            && !event.lower_il()
    }) {
        return Verdict::PositiveControlFailure;
    }
    if observations.iter().any(|(_, events)| events.is_empty()) {
        return Verdict::NoTouchpadWheelEvents;
    }
    if observations.iter().any(|(direction, events)| {
        !events
            .iter()
            .copied()
            .any(|event| matches_axis(event, *direction))
    }) {
        return Verdict::UnexpectedAxis;
    }
    if observations.iter().any(|(direction, events)| {
        !events
            .iter()
            .copied()
            .any(|event| matches_axis(event, *direction) && event.injected() && !event.lower_il())
    }) {
        return Verdict::InjectedFlagMismatch;
    }
    Verdict::Expected
}

fn countdown() {
    println!("WARNING: this injects three-contact PT_TOUCHPAD input. Windows system gestures and a running LeopardWM daemon may move focus, workspaces, or virtual desktops. No daemon configuration will be inspected or changed.");
    for seconds in (1..=3).rev() {
        println!("Starting in {seconds}...");
        thread::sleep(Duration::from_secs(1));
    }
}

fn run(options: Options) -> Result<i32, String> {
    let create = resolve_create_device()?;
    println!("resolved CreateSyntheticPointerDevice2 from user32.dll");
    let device = SyntheticDevice::create(create)?;
    println!("created PT_TOUCHPAD: max_count=3 feedback=NONE hmonitor=NULL physical_size={WIDTH}x{HEIGHT} himetric options=SDCO_PHYSICAL_SIZE (gesture-only omitted)");
    let (mut hook, receiver) = WheelHook::install()?;
    println!("installed dedicated WH_MOUSE_LL observer");
    for direction in options.directions.iter().copied() {
        print_plan(direction);
    }

    let result = (|| {
        if options.dry_run {
            println!("dry-run: no SendInput or InjectSyntheticPointerInput calls were made");
            return Ok(0);
        }
        if !options.yes {
            countdown();
        }
        println!("running SendInput wheel positive control");
        if let Err(error) = send_positive_control() {
            println!(
                "verdict: {} ({error})",
                Verdict::PositiveControlFailure.name()
            );
            return Ok(1);
        }
        thread::sleep(SETTLE_DELAY);
        let positive: Vec<_> = receiver.try_iter().collect();
        print_events("positive-control", &positive);
        if classify(&positive, &[]) == Verdict::PositiveControlFailure {
            println!("verdict: {}", Verdict::PositiveControlFailure.name());
            return Ok(1);
        }
        let mut observations = Vec::with_capacity(options.directions.len());
        for direction in options.directions.iter().copied() {
            println!("injecting {} swipe", direction.name());
            inject_swipe(device.handle(), direction)?;
            thread::sleep(SETTLE_DELAY);
            let events: Vec<_> = receiver.try_iter().collect();
            print_events(direction.name(), &events);
            observations.push((direction, events));
        }
        let verdict = classify(&positive, &observations);
        println!("verdict: {}", verdict.name());
        Ok(if verdict == Verdict::Expected { 0 } else { 1 })
    })();
    let cleanup = hook.shutdown();
    drop(device);
    cleanup?;
    result
}

fn main() {
    match parse_args(std::env::args().skip(1)) {
        Ok(None) => usage(),
        Ok(Some(options)) => match run(options) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("diagnostic error: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("argument error: {error}");
            usage();
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(axis: WheelAxis, delta: i32, flags: u32) -> WheelEvent {
        WheelEvent {
            axis,
            delta,
            flags,
            time: 0,
            point: POINT::default(),
        }
    }

    #[test]
    fn parses_cli() {
        assert_eq!(
            parse_args(["up", "--dry-run", "--yes"].map(String::from)),
            Ok(Some(Options {
                directions: vec![Direction::Up],
                dry_run: true,
                yes: true
            }))
        );
        assert!(parse_args(["--dry-run"].map(String::from)).is_err());
        assert!(parse_args(["all", "left"].map(String::from)).is_err());
    }

    #[test]
    fn decodes_signed_delta() {
        assert_eq!(signed_delta(120 << 16), 120);
        assert_eq!(signed_delta((-120i16 as u16 as u32) << 16), -120);
    }

    #[test]
    fn uses_fixed_frame_offsets() {
        assert_eq!(
            offsets(Direction::Up),
            [
                (0, 0),
                (0, -480),
                (0, -960),
                (0, -1440),
                (0, -1920),
                (0, -2400)
            ]
        );
        assert_eq!(offsets(Direction::Right).last(), Some(&(2400, 0)));
    }

    #[test]
    fn classifies_outcomes() {
        let positive = [event(WheelAxis::Vertical, 120, LLMHF_INJECTED)];
        assert_eq!(classify(&[], &[]), Verdict::PositiveControlFailure);
        assert_eq!(
            classify(&positive, &[(Direction::Up, vec![])]),
            Verdict::NoTouchpadWheelEvents
        );
        assert_eq!(
            classify(
                &positive,
                &[(
                    Direction::Up,
                    vec![event(WheelAxis::Horizontal, -120, LLMHF_INJECTED)]
                )]
            ),
            Verdict::UnexpectedAxis
        );
        assert_eq!(
            classify(
                &positive,
                &[
                    (
                        Direction::Left,
                        vec![event(WheelAxis::Horizontal, 120, LLMHF_INJECTED)],
                    ),
                    (Direction::Right, vec![]),
                ]
            ),
            Verdict::NoTouchpadWheelEvents
        );
        assert_eq!(
            classify(
                &positive,
                &[(Direction::Up, vec![event(WheelAxis::Vertical, -120, 0)])]
            ),
            Verdict::InjectedFlagMismatch
        );
        assert_eq!(
            classify(
                &positive,
                &[(
                    Direction::Up,
                    vec![event(WheelAxis::Vertical, 120, LLMHF_INJECTED)]
                )]
            ),
            Verdict::Expected
        );
    }

    #[test]
    fn creation_params_match_windows_abi() {
        assert_eq!(size_of::<POINTER_INPUT_TYPE>(), size_of::<i32>());
        assert_eq!(size_of::<POINTER_FEEDBACK_MODE>(), size_of::<i32>());
        assert_eq!(size_of::<HMONITOR>(), size_of::<*mut core::ffi::c_void>());
        assert_eq!(
            std::mem::align_of::<SyntheticDeviceCreationParams>(),
            std::mem::align_of::<HMONITOR>()
        );
        assert_eq!(
            size_of::<SyntheticDeviceCreationParams>(),
            if cfg!(target_pointer_width = "64") {
                40
            } else {
                28
            }
        );
    }
}
