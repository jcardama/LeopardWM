//! Global hotkey matching via a low-level keyboard hook.
//!
//! A `WH_KEYBOARD_LL` hook inspects key-downs, swallows configured chords, and
//! emits hotkey or Settings-recorder events. Matching here rather than with
//! `RegisterHotKey` distinguishes left/right modifiers, so AltGr (Left Ctrl +
//! Right Alt on international layouts) does not fire Ctrl+Alt binds.

use crate::{
    fn_mod_bit, recover_poisoned_mutex, HotkeyEvent, HotkeyId, Modifiers, Win32Error,
    WM_QUIT_LLHOOK_THREAD,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// A hotkey bind the hook matches: its modifiers, virtual-key code, and the
/// hotkey id to emit when it fires.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyBind {
    pub modifiers: Modifiers,
    pub vk: u32,
    pub id: HotkeyId,
    pub overview_action: Option<crate::overview::OverviewAction>,
}

/// Event produced by the global keyboard hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardHookEvent {
    /// A configured hotkey matched.
    Hotkey(HotkeyEvent),
    /// A Settings recorder captured a chord.
    Recorded { modifiers: Modifiers, vk: u32 },
}

static HOOK_SENDER: std::sync::Mutex<Option<mpsc::Sender<KeyboardHookEvent>>> =
    std::sync::Mutex::new(None);
static HOOK_BINDS: std::sync::Mutex<Vec<HotkeyBind>> = std::sync::Mutex::new(Vec::new());
#[derive(Default)]
struct HeldKeys {
    physical: Vec<i32>,
    injected: Vec<i32>,
}

impl HeldKeys {
    fn source(&mut self, injected: bool) -> &mut Vec<i32> {
        if injected {
            &mut self.injected
        } else {
            &mut self.physical
        }
    }

    fn press(&mut self, vk: i32, injected: bool) -> bool {
        let held = self.source(injected);
        if held.contains(&vk) {
            false
        } else {
            held.push(vk);
            true
        }
    }

    fn release(&mut self, vk: i32, injected: bool) {
        self.source(injected).retain(|&key| key != vk);
    }

    fn clear(&mut self) {
        self.physical.clear();
        self.injected.clear();
    }
}

/// Track matched and unmatched keys per source. Hardware repeat is never
/// injected, so a remapper's injected press is distinct from the physical one;
/// repeats from the same source remain guarded even if modifiers change.
static HOOK_HELD: std::sync::Mutex<HeldKeys> = std::sync::Mutex::new(HeldKeys {
    physical: Vec::new(),
    injected: Vec::new(),
});
// A hotkey consumed over the overview owns its repeats until key-up, even
// if modifiers change and the next repeat would otherwise reach WM_KEYDOWN.
static HOOK_OVERVIEW_HELD: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
/// Which F13–F24 keys any bind uses as a modifier (union of `fn_mods` across
/// all binds). Keys in this mask are swallowed and tracked rather than passed
/// through, so they act purely as modifiers and never reach the foreground app.
static HOOK_FN_MOD_MASK: std::sync::Mutex<u16> = std::sync::Mutex::new(0);
/// Which masked F13–F24 modifiers are currently held. Maintained from the hook's
/// own key-down/up events (a swallowed key never updates `GetAsyncKeyState`).
static HOOK_FN_HELD: std::sync::Mutex<u16> = std::sync::Mutex::new(0);
/// F13–F24 keys held while Settings recording is active, including keys that
/// are not configured as normal hotkey modifiers.
static HOOK_RECORDING_FN_HELD: std::sync::Mutex<u16> = std::sync::Mutex::new(0);
static HOOK_RECORDING: AtomicBool = AtomicBool::new(false);
/// Recorded key whose auto-repeat remains swallowed until its key-up arrives.
static HOOK_RECORDED_PENDING: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12; // Alt
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;
const VK_LSHIFT: i32 = 0xA0;
const VK_RSHIFT: i32 = 0xA1;
const VK_LCONTROL: i32 = 0xA2;
const VK_RCONTROL: i32 = 0xA3;
const VK_LMENU: i32 = 0xA4;
const VK_RMENU: i32 = 0xA5;

const VK_TAB: u32 = 0x09;
const VK_BACK: u32 = 0x08;
const VK_ESCAPE: u32 = 0x1B;
const VK_DELETE: u32 = 0x2E;
const VK_MENU_MASK: u16 = 0xE8;

fn is_modifier_vk(vk: i32) -> bool {
    matches!(
        vk,
        VK_SHIFT
            | VK_CONTROL
            | VK_MENU
            | VK_LWIN
            | VK_RWIN
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_LMENU
            | VK_RMENU
    )
}

/// Find the bind matching exactly the held modifiers and key. Exact
/// match means a superset (extra modifier held) does not fire it, matching
/// `RegisterHotKey` semantics.
fn find_bind(binds: &[HotkeyBind], held: Modifiers, vk: u32) -> Option<HotkeyBind> {
    binds
        .iter()
        .find(|b| b.vk == vk && b.modifiers == held)
        .copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyMsg {
    Down,
    Up,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Pass,
    SwallowOverview {
        input: crate::overview::OverviewInput,
        mask: Option<u16>,
    },
    RecordingControl,
    Swallow,
    SwallowHotkey {
        event: HotkeyEvent,
        emit: bool,
        mask: Option<u16>,
    },
    SwallowRecorded {
        modifiers: Modifiers,
        vk: u32,
        mask: Option<u16>,
    },
    PassRecorded {
        modifiers: Modifiers,
        vk: u32,
    },
}

// Bare Esc/Tab/Backspace/Delete must remain usable as Settings recording controls.
fn is_recording_control(vk: u32, modifiers: Modifiers) -> bool {
    modifiers == Modifiers::default() && matches!(vk, VK_ESCAPE | VK_TAB | VK_BACK | VK_DELETE)
}

fn menu_mask(modifiers: Modifiers) -> Option<u16> {
    if modifiers.win && !modifiers.ctrl && !modifiers.alt && !modifiers.shift {
        Some(VK_CONTROL as u16)
    } else if modifiers.alt && !modifiers.ctrl {
        Some(VK_MENU_MASK)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn decide(
    msg: KeyMsg,
    vk: u32,
    held: Modifiers,
    recording: bool,
    binds: &[HotkeyBind],
    fn_mask: u16,
    fn_held: u16,
    recording_fn_held: u16,
    is_new_press: bool,
    alt_gr: bool,
    pending_recorded_vk: Option<u32>,
) -> Action {
    if msg == KeyMsg::Up {
        if pending_recorded_vk == Some(vk) {
            return Action::Pass;
        }
        if recording && fn_mod_bit(vk).is_some_and(|bit| recording_fn_held & bit != 0) {
            return Action::PassRecorded {
                modifiers: Modifiers::default(),
                vk,
            };
        }
        return if fn_mod_bit(vk).is_some_and(|bit| fn_held & bit != 0) {
            Action::Swallow
        } else {
            Action::Pass
        };
    }

    if msg != KeyMsg::Down {
        return Action::Pass;
    }

    if is_modifier_vk(vk as i32) {
        return Action::Pass;
    }

    if pending_recorded_vk == Some(vk) {
        return Action::Swallow;
    }

    if recording {
        if fn_mod_bit(vk).is_some() {
            return Action::Swallow;
        }
        if is_recording_control(vk, held) {
            return Action::RecordingControl;
        }
        return if is_new_press && crate::format_hotkey(held, vk).is_some() {
            Action::SwallowRecorded {
                modifiers: held,
                vk,
                mask: menu_mask(held),
            }
        } else {
            Action::Swallow
        };
    }

    if fn_mod_bit(vk).is_some_and(|bit| fn_mask & bit != 0) {
        return Action::Swallow;
    }

    if alt_gr {
        return Action::Pass;
    }

    if let Some(bind) = find_bind(binds, held, vk) {
        return Action::SwallowHotkey {
            event: HotkeyEvent { id: bind.id },
            emit: is_new_press,
            mask: if is_new_press {
                menu_mask(bind.modifiers)
            } else {
                None
            },
        };
    }

    Action::Pass
}

fn retain_overview_press(
    msg: KeyMsg,
    vk: u32,
    action: Action,
    overview_visible: bool,
    owned_keys: &mut Vec<u32>,
) -> Action {
    if !overview_visible {
        owned_keys.clear();
    }
    if msg == KeyMsg::Up {
        owned_keys.retain(|&key| key != vk);
    } else if msg == KeyMsg::Down {
        if owned_keys.contains(&vk) {
            return Action::Swallow;
        }
        if overview_visible
            && matches!(
                action,
                Action::SwallowHotkey { .. }
                    | Action::SwallowOverview {
                        input: crate::overview::OverviewInput::Action(_),
                        ..
                    }
            )
        {
            owned_keys.push(vk);
        }
    }
    action
}

pub(crate) fn clear_overview_press_ownership() {
    HOOK_OVERVIEW_HELD
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .clear();
}

fn route_overview_action(
    native_key_down: bool,
    vk: u32,
    action: Action,
    overview_active: bool,
    overview_action: Option<crate::overview::OverviewAction>,
) -> Action {
    use crate::overview::OverviewInput;

    if !overview_active {
        return action;
    }
    match action {
        Action::SwallowHotkey {
            emit: true, mask, ..
        } => {
            if let Some(action) = overview_action {
                return Action::SwallowOverview {
                    input: OverviewInput::Action(action),
                    mask,
                };
            }
        }
        Action::Pass
            if native_key_down && matches!(vk, 0x1B | 0x0D | 0x25..=0x28 | 0x31..=0x39) =>
        {
            return Action::SwallowOverview {
                input: OverviewInput::Key(vk as u16),
                mask: None,
            };
        }
        _ => {}
    }
    action
}

/// Set whether the installed hook captures a single Settings recorder chord.
pub fn set_recording(enabled: bool) {
    HOOK_RECORDING.store(enabled, Ordering::SeqCst);
    if !enabled {
        let mut held = HOOK_RECORDING_FN_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *held = 0;
        let mut pending = HOOK_RECORDED_PENDING
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *pending = None;
    }
}

/// Handle for the keyboard hook. Dropping it signals the dedicated thread to
/// unhook and exit, then clears the global state.
pub struct KeyboardHookHandle {
    thread_id: u32,
    generation: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for KeyboardHookHandle {
    fn drop(&mut self) {
        crate::user_press::set_keyboard_observer_installed(self.generation, false);
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
        HOOK_RECORDING.store(false, Ordering::SeqCst);
        let mut sender = HOOK_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        *sender = None;
        drop(sender);
        let mut binds = HOOK_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
        binds.clear();
        drop(binds);
        let mut held = HOOK_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        held.clear();
        drop(held);
        HOOK_OVERVIEW_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .clear();
        let mut mask = HOOK_FN_MOD_MASK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *mask = 0;
        drop(mask);
        let mut fn_held = HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        *fn_held = 0;
        drop(fn_held);
        let mut recording_fn_held = HOOK_RECORDING_FN_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *recording_fn_held = 0;
        drop(recording_fn_held);
        let mut pending = HOOK_RECORDED_PENDING
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *pending = None;
        tracing::debug!("Keyboard hook stopped");
    }
}

fn reset_held_keys() -> Result<(), Win32Error> {
    {
        let mut held = HOOK_HELD
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Hook held mutex poisoned".to_string()))?;
        held.clear();
    }
    {
        let mut fn_held = HOOK_FN_HELD.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Hook fn-held mutex poisoned".to_string())
        })?;
        *fn_held = 0;
    }
    {
        let mut recording_fn_held = HOOK_RECORDING_FN_HELD.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Recording fn-held mutex poisoned".to_string())
        })?;
        *recording_fn_held = 0;
    }
    {
        let mut pending = HOOK_RECORDED_PENDING.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Recorded-pending mutex poisoned".to_string())
        })?;
        *pending = None;
    }
    HOOK_OVERVIEW_HELD
        .lock()
        .unwrap_or_else(recover_poisoned_mutex)
        .clear();
    HOOK_RECORDING.store(false, Ordering::SeqCst);
    Ok(())
}

/// Install the keyboard hook for the given binds. Spawns a dedicated thread
/// with a message pump (required for `WH_KEYBOARD_LL`). Returns a handle that
/// must be kept alive and a receiver for hotkey and recorder events.
pub fn install_keyboard_hook(
    binds: Vec<HotkeyBind>,
) -> Result<(KeyboardHookHandle, mpsc::Receiver<KeyboardHookEvent>), Win32Error> {
    let count = binds.len();
    let (tx, rx) = mpsc::channel();

    {
        let mut sender = HOOK_SENDER
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Hook sender mutex poisoned".to_string()))?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Keyboard hook already installed - drop existing handle first".to_string(),
            ));
        }
        *sender = Some(tx);
    }
    let fn_mod_mask = binds
        .iter()
        .fold(0u16, |mask, b| mask | b.modifiers.fn_mods);
    {
        let mut b = HOOK_BINDS
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Hook binds mutex poisoned".to_string()))?;
        *b = binds;
    }
    {
        let mut mask = HOOK_FN_MOD_MASK.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Hook fn-mod mask mutex poisoned".to_string())
        })?;
        *mask = fn_mod_mask;
    }
    reset_held_keys()?;

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let generation = crate::user_press::next_observer_generation();
    let thread = std::thread::Builder::new()
        .name("hotkey-hook".into())
        .spawn(move || unsafe {
            let thread_id = GetCurrentThreadId();

            // Ensure the message queue exists before signalling init.
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_hook_proc), None, 0)
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                        "SetWindowsHookExW for keyboard hook failed: {}",
                        e
                    ))));
                    return;
                }
            };

            crate::user_press::set_keyboard_observer_installed(generation, true);
            let _ = init_tx.send(Ok(thread_id));

            // Message pump — required for WH_KEYBOARD_LL callbacks.
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

            crate::user_press::set_keyboard_observer_installed(generation, false);
            let _ = UnhookWindowsHookEx(hook);
        })
        .map_err(|e| {
            Win32Error::HookInstallFailed(format!("Failed to spawn hotkey hook thread: {}", e))
        })?;

    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Keyboard hook thread initialization failed".to_string())
    })??;

    tracing::info!("Keyboard hook installed ({} hotkeys)", count);

    Ok((
        KeyboardHookHandle {
            thread_id,
            generation,
            thread: Some(thread),
        },
        rx,
    ))
}

/// Low-level keyboard hook callback. Wrapped in `catch_unwind` so a panic can't
/// unwind across the FFI boundary.
unsafe extern "system" fn keyboard_ll_hook_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        keyboard_ll_hook_inner(ncode, wparam, lparam)
    }));
    match result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!("Panic in keyboard_ll_hook_proc");
            CallNextHookEx(None, ncode, wparam, lparam)
        }
    }
}

unsafe fn keyboard_ll_hook_inner(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    let msg = match wparam.0 as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => KeyMsg::Down,
        WM_KEYUP | WM_SYSKEYUP => KeyMsg::Up,
        _ => KeyMsg::Other,
    };
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = kb.vkCode;
    let recording = HOOK_RECORDING.load(Ordering::SeqCst);
    if crate::overview::input_target().is_none() {
        clear_overview_press_ownership();
    }

    if msg == KeyMsg::Up {
        let fn_held = *HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        let recording_fn_held = *HOOK_RECORDING_FN_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let pending_recorded_vk = *HOOK_RECORDED_PENDING
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let action = decide(
            msg,
            vk,
            Modifiers::default(),
            recording,
            &[],
            0,
            fn_held,
            recording_fn_held,
            false,
            false,
            pending_recorded_vk,
        );
        if pending_recorded_vk == Some(vk) {
            let mut pending = HOOK_RECORDED_PENDING
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *pending = None;
        }
        if let Some(bit) = fn_mod_bit(vk) {
            let mut held = HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
            *held &= !bit;
            drop(held);
            let mut recording_held = HOOK_RECORDING_FN_HELD
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *recording_held &= !bit;
        }
        let mut held = HOOK_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        held.release(vk as i32, kb.flags.contains(LLKHF_INJECTED));
        drop(held);
        return apply_action(action, ncode, wparam, lparam);
    }

    if msg != KeyMsg::Down {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    let held = Modifiers {
        ctrl: GetAsyncKeyState(VK_LCONTROL) < 0 || GetAsyncKeyState(VK_RCONTROL) < 0,
        alt: GetAsyncKeyState(VK_LMENU) < 0,
        shift: GetAsyncKeyState(VK_LSHIFT) < 0 || GetAsyncKeyState(VK_RSHIFT) < 0,
        win: GetAsyncKeyState(VK_LWIN) < 0 || GetAsyncKeyState(VK_RWIN) < 0,
        fn_mods: 0,
    };
    let action = decide_key_down(kb, recording, held, GetAsyncKeyState(VK_RMENU) < 0);
    apply_action(action, ncode, wparam, lparam)
}

fn decide_key_down(
    kb: &KBDLLHOOKSTRUCT,
    recording: bool,
    mut held: Modifiers,
    alt_gr: bool,
) -> Action {
    let vk = kb.vkCode;
    let injected = kb.flags.contains(LLKHF_INJECTED);
    let fn_mask = *HOOK_FN_MOD_MASK
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    let is_new_press = if is_modifier_vk(vk as i32)
        || (fn_mod_bit(vk).is_some()
            && (recording || fn_mod_bit(vk).is_some_and(|bit| fn_mask & bit != 0)))
    {
        false
    } else {
        HOOK_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .press(vk as i32, injected)
    };

    if is_new_press && kb.dwExtraInfo != crate::user_press::SELF_INJECTED_INPUT_TAG {
        crate::user_press::record_user_press();
    }

    if recording && fn_mod_bit(vk).is_some() {
        let bit = fn_mod_bit(vk).unwrap();
        let mut recording_held = HOOK_RECORDING_FN_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *recording_held |= bit;
        drop(recording_held);
        if fn_mask & bit != 0 {
            let mut fn_held = HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
            *fn_held |= bit;
        }
    } else if !recording && fn_mod_bit(vk).is_some_and(|bit| fn_mask & bit != 0) {
        let bit = fn_mod_bit(vk).unwrap();
        let mut fn_held = HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        *fn_held |= bit;
    }

    let fn_held = *HOOK_FN_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
    let recording_fn_held = *HOOK_RECORDING_FN_HELD
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    held.fn_mods = fn_held | recording_fn_held;
    let binds = HOOK_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
    let pending_recorded_vk = *HOOK_RECORDED_PENDING
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    let action = decide(
        KeyMsg::Down,
        vk,
        held,
        recording,
        &binds,
        fn_mask,
        fn_held,
        recording_fn_held,
        is_new_press,
        alt_gr,
        pending_recorded_vk,
    );
    drop(binds);
    if matches!(action, Action::SwallowHotkey { emit: false, .. }) {
        tracing::debug!(
            vk = format_args!("{vk:#x}"),
            source = if injected { "injected" } else { "physical" },
            "Hotkey dropped as repeat"
        );
    }
    action
}

unsafe fn apply_action(action: Action, ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let msg = match wparam.0 as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => KeyMsg::Down,
        WM_KEYUP | WM_SYSKEYUP => KeyMsg::Up,
        _ => KeyMsg::Other,
    };
    let vk = (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode;
    let target = crate::overview::input_target();
    let overview_active = target.is_some() && !HOOK_RECORDING.load(Ordering::SeqCst);
    let overview_action = if let Action::SwallowHotkey { event, .. } = action {
        HOOK_BINDS
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .iter()
            .find(|bind| bind.id == event.id)
            .and_then(|bind| bind.overview_action)
    } else {
        None
    };
    let action = route_overview_action(
        wparam.0 as u32 == WM_KEYDOWN,
        vk,
        action,
        overview_active,
        overview_action,
    );
    let action = retain_overview_press(
        msg,
        vk,
        action,
        overview_active,
        &mut HOOK_OVERVIEW_HELD
            .lock()
            .unwrap_or_else(recover_poisoned_mutex),
    );
    match action {
        Action::SwallowOverview { input, mask } => {
            if let Some(target) = target {
                crate::overview::post_input(target, input);
            }
            if let Some(vk) = mask {
                send_menu_mask(vk);
            }
            LRESULT(1)
        }
        Action::Pass | Action::RecordingControl => CallNextHookEx(None, ncode, wparam, lparam),
        Action::Swallow => LRESULT(1),
        Action::SwallowHotkey { event, emit, mask } => {
            if emit {
                send_event(KeyboardHookEvent::Hotkey(event));
            }
            if let Some(vk) = mask {
                send_menu_mask(vk);
            }
            LRESULT(1)
        }
        Action::SwallowRecorded {
            modifiers,
            vk,
            mask,
        } => {
            send_event(KeyboardHookEvent::Recorded { modifiers, vk });
            // One-shot capture closes the swallow window before JS posts its stop event.
            set_recording(false);
            let mut pending = HOOK_RECORDED_PENDING
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *pending = Some(vk);
            drop(pending);
            if let Some(vk) = mask {
                send_menu_mask(vk);
            }
            LRESULT(1)
        }
        Action::PassRecorded { modifiers, vk } => {
            send_event(KeyboardHookEvent::Recorded { modifiers, vk });
            set_recording(false);
            CallNextHookEx(None, ncode, wparam, lparam)
        }
    }
}

fn send_event(event: KeyboardHookEvent) {
    let sender = HOOK_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(event);
    }
}

/// Tap a key after swallowing a bind: Ctrl masks bare Win so Start stays closed,
/// and the unassigned VK 0xE8 masks bare Alt so the app's menu bar stays inactive.
/// Alt uses an unused key rather than Ctrl because Ctrl+Alt acts as AltGr on some layouts.
unsafe fn send_menu_mask(vk: u16) {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: crate::user_press::SELF_INJECTED_INPUT_TAG,
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    static HOOK_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn keyboard_hook_counts_only_new_non_modifier_presses() {
        let _guard = HOOK_STATE_TEST_LOCK.lock().unwrap();
        let _counter_guard = crate::user_press::PRESS_COUNT_TEST_LOCK.lock().unwrap();
        reset_held_keys().unwrap();
        let keyboard_generation = crate::user_press::next_observer_generation();
        let mouse_generation = crate::user_press::next_observer_generation();
        crate::user_press::set_keyboard_observer_installed(keyboard_generation, true);
        crate::user_press::set_mouse_observer_installed(mouse_generation, true);
        let mut expected = crate::user_press_count().unwrap();
        for (message, vk, flags, extra_info, counts) in [
            (WM_KEYUP, 0x41, Default::default(), 0, false),
            (WM_KEYDOWN, 0x41, Default::default(), 0, true),
            (WM_KEYDOWN, 0x41, Default::default(), 0, false),
            (WM_KEYUP, 0x41, Default::default(), 0, false),
            (WM_SYSKEYDOWN, 0x41, Default::default(), 0, true),
            (WM_SYSKEYUP, 0x41, Default::default(), 0, false),
            (WM_KEYDOWN, VK_LSHIFT as u32, Default::default(), 0, false),
            (WM_KEYUP, VK_LSHIFT as u32, Default::default(), 0, false),
            (
                WM_KEYDOWN,
                0xE8,
                LLKHF_INJECTED,
                crate::user_press::SELF_INJECTED_INPUT_TAG,
                false,
            ),
            (WM_KEYDOWN, 0x42, LLKHF_INJECTED, 0, true),
            (WM_KEYDOWN, 0x43, LLKHF_INJECTED, 123, true),
        ] {
            let kb = KBDLLHOOKSTRUCT {
                vkCode: vk,
                flags,
                dwExtraInfo: extra_info,
                ..Default::default()
            };
            unsafe {
                keyboard_ll_hook_inner(
                    0,
                    WPARAM(message as usize),
                    LPARAM(&kb as *const _ as isize),
                )
            };
            expected += u64::from(counts);
            assert_eq!(
                crate::user_press_count(),
                Some(expected),
                "message={message}, vk={vk}, flags={flags:?}, extra_info={extra_info}"
            );
        }
        reset_held_keys().unwrap();
        crate::user_press::set_keyboard_observer_installed(keyboard_generation, false);
        crate::user_press::set_mouse_observer_installed(mouse_generation, false);
    }

    fn bind(modifiers: Modifiers, vk: u32) -> HotkeyBind {
        HotkeyBind {
            modifiers,
            vk,
            id: crate::Hotkey::stable_id(modifiers, vk),
            overview_action: None,
        }
    }

    fn test_key(vk: u32, injected: bool) -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT {
            vkCode: vk,
            flags: if injected {
                LLKHF_INJECTED
            } else {
                Default::default()
            },
            ..Default::default()
        }
    }

    fn release_key(kb: &KBDLLHOOKSTRUCT) {
        unsafe {
            keyboard_ll_hook_inner(
                0,
                WPARAM(WM_KEYUP as usize),
                LPARAM(kb as *const _ as isize),
            );
        }
    }

    #[test]
    fn physical_hold_does_not_block_injected_chord() {
        let _guard = HOOK_STATE_TEST_LOCK.lock().unwrap();
        let _counter_guard = crate::user_press::PRESS_COUNT_TEST_LOCK.lock().unwrap();
        reset_held_keys().unwrap();
        let modifiers = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let bind = bind(modifiers, 0x48);
        *HOOK_BINDS.lock().unwrap() = vec![bind];
        let physical = test_key(0x48, false);
        let injected = test_key(0x48, true);
        assert_eq!(
            decide_key_down(&physical, false, Modifiers::default(), false),
            Action::Pass
        );
        assert_eq!(
            decide_key_down(&injected, false, modifiers, false),
            Action::SwallowHotkey {
                event: HotkeyEvent { id: bind.id },
                emit: true,
                mask: None,
            }
        );
        release_key(&physical);
        assert!(matches!(
            decide_key_down(&injected, false, modifiers, false),
            Action::SwallowHotkey { emit: false, .. }
        ));
        release_key(&injected);
        assert!(matches!(
            decide_key_down(&injected, false, modifiers, false),
            Action::SwallowHotkey { emit: true, .. }
        ));
        release_key(&injected);
        assert_eq!(
            decide_key_down(&injected, false, Modifiers::default(), false),
            Action::Pass
        );
        assert!(matches!(
            decide_key_down(&injected, false, modifiers, false),
            Action::SwallowHotkey { emit: false, .. }
        ));
        HOOK_BINDS.lock().unwrap().clear();
        reset_held_keys().unwrap();
    }

    #[test]
    fn physical_repeat_fires_only_after_its_own_release() {
        let _guard = HOOK_STATE_TEST_LOCK.lock().unwrap();
        let _counter_guard = crate::user_press::PRESS_COUNT_TEST_LOCK.lock().unwrap();
        reset_held_keys().unwrap();
        let modifiers = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        *HOOK_BINDS.lock().unwrap() = vec![bind(modifiers, 0x48)];
        let physical = test_key(0x48, false);
        let injected = test_key(0x48, true);
        assert!(matches!(
            decide_key_down(&physical, false, modifiers, false),
            Action::SwallowHotkey { emit: true, .. }
        ));
        for _ in 0..2 {
            assert!(matches!(
                decide_key_down(&physical, false, modifiers, false),
                Action::SwallowHotkey { emit: false, .. }
            ));
        }
        release_key(&injected);
        assert!(matches!(
            decide_key_down(&physical, false, modifiers, false),
            Action::SwallowHotkey { emit: false, .. }
        ));
        release_key(&physical);
        assert!(matches!(
            decide_key_down(&physical, false, modifiers, false),
            Action::SwallowHotkey { emit: true, .. }
        ));
        HOOK_BINDS.lock().unwrap().clear();
        reset_held_keys().unwrap();
    }

    fn win_ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            win: true,
            ..Default::default()
        }
    }

    fn win() -> Modifiers {
        Modifiers {
            win: true,
            ..Default::default()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    #[test]
    fn overview_release_preserves_held_key_until_key_up() {
        let mut owned = vec![0x48];
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x48, Action::Pass, false, &mut owned),
            Action::Pass
        );
        assert!(owned.is_empty());

        let _guard = HOOK_STATE_TEST_LOCK.lock().unwrap();
        reset_held_keys().unwrap();
        HOOK_HELD.lock().unwrap().physical.extend([0x48, 0x49]);
        HOOK_OVERVIEW_HELD.lock().unwrap().push(0x48);
        clear_overview_press_ownership();
        assert_eq!(HOOK_HELD.lock().unwrap().physical, [0x48, 0x49]);
        assert!(HOOK_OVERVIEW_HELD.lock().unwrap().is_empty());

        let modifiers = win_ctrl();
        let binds = [bind(modifiers, 0x48)];
        for expected_emit in [false, true] {
            let is_new_press = !HOOK_HELD.lock().unwrap().physical.contains(&0x48);
            let action = decide(
                KeyMsg::Down,
                0x48,
                modifiers,
                false,
                &binds,
                0,
                0,
                0,
                is_new_press,
                false,
                None,
            );
            let routed = route_overview_action(
                true,
                0x48,
                action,
                false,
                Some(crate::overview::OverviewAction::Navigate(
                    crate::overview::OverviewDirection::Right,
                )),
            );
            assert_eq!(
                retain_overview_press(
                    KeyMsg::Down,
                    0x48,
                    routed,
                    false,
                    &mut HOOK_OVERVIEW_HELD.lock().unwrap(),
                ),
                Action::SwallowHotkey {
                    event: HotkeyEvent { id: binds[0].id },
                    emit: expected_emit,
                    mask: None,
                }
            );
            assert_eq!(
                retain_overview_press(
                    KeyMsg::Up,
                    0x48,
                    Action::Pass,
                    false,
                    &mut HOOK_OVERVIEW_HELD.lock().unwrap(),
                ),
                Action::Pass
            );
            HOOK_HELD.lock().unwrap().release(0x48, false);
        }
        reset_held_keys().unwrap();
    }

    #[test]
    fn navigation_and_enter_share_ordered_overview_ingress() {
        use crate::overview::{OverviewAction, OverviewDirection, OverviewInput};
        let navigate = OverviewAction::Navigate(OverviewDirection::Right);
        let hotkey = Action::SwallowHotkey {
            event: HotkeyEvent { id: 123 },
            emit: true,
            mask: None,
        };
        for mapped in [
            navigate,
            OverviewAction::MoveUp,
            OverviewAction::MoveDown,
            OverviewAction::Noop,
        ] {
            let mut queued = Vec::new();
            for (vk, action, metadata) in [(0x48, hotkey, Some(mapped)), (0x0D, Action::Pass, None)]
            {
                match route_overview_action(true, vk, action, true, metadata) {
                    Action::SwallowOverview { input, .. } => queued.push(input),
                    other => panic!("overview input escaped the ordered queue: {other:?}"),
                }
            }
            assert_eq!(
                queued,
                [OverviewInput::Action(mapped), OverviewInput::Key(0x0D)]
            );
            assert_eq!(
                route_overview_action(true, 0x48, hotkey, false, Some(mapped)),
                hotkey,
                "hidden, closing, or non-foreground overview keeps normal command dispatch"
            );
        }
        assert_eq!(OverviewInput::Action(navigate).key(), 0x27);
        assert_eq!(
            route_overview_action(true, 0x48, hotkey, true, None),
            hotkey
        );
        assert_eq!(
            route_overview_action(false, 0x0D, Action::Pass, true, None),
            Action::Pass
        );
    }

    #[test]
    fn native_overview_keys_pass_without_visible_foreground_overlay() {
        for (hwnd, foreground, expected_active) in [(0, 99, false), (42, 99, false), (42, 42, true)]
        {
            let active = crate::overview::foreground_input_target(hwnd, foreground).is_some();
            assert_eq!(active, expected_active);
            for vk in [0x0D, 0x1B, 0x25, 0x28, 0x31] {
                let routed = route_overview_action(true, vk, Action::Pass, active, None);
                if expected_active {
                    assert!(matches!(routed, Action::SwallowOverview { .. }));
                } else {
                    assert_eq!(routed, Action::Pass);
                }
            }
        }
    }

    #[test]
    fn hook_install_reset_clears_every_held_key_tracker() {
        let _guard = HOOK_STATE_TEST_LOCK.lock().unwrap();
        reset_held_keys().unwrap();
        HOOK_HELD.lock().unwrap().press(0x48, false);
        HOOK_HELD.lock().unwrap().press(0x49, true);
        HOOK_OVERVIEW_HELD.lock().unwrap().push(0x48);
        *HOOK_FN_HELD.lock().unwrap() = 1;
        *HOOK_RECORDING_FN_HELD.lock().unwrap() = 2;
        *HOOK_RECORDED_PENDING.lock().unwrap() = Some(0x49);
        HOOK_RECORDING.store(true, Ordering::SeqCst);
        reset_held_keys().unwrap();
        assert!(HOOK_HELD.lock().unwrap().physical.is_empty());
        assert!(HOOK_HELD.lock().unwrap().injected.is_empty());
        assert!(HOOK_OVERVIEW_HELD.lock().unwrap().is_empty());
        assert_eq!(*HOOK_FN_HELD.lock().unwrap(), 0);
        assert_eq!(*HOOK_RECORDING_FN_HELD.lock().unwrap(), 0);
        assert_eq!(*HOOK_RECORDED_PENDING.lock().unwrap(), None);
        assert!(!HOOK_RECORDING.load(Ordering::SeqCst));
    }

    #[test]
    fn overview_hotkey_press_owns_repeats_until_release() {
        let modifiers = win_ctrl();
        let binds = [bind(modifiers, 0x25), bind(alt(), 0x25)];
        let mut owned = Vec::new();
        let first = decide(
            KeyMsg::Down,
            0x25,
            modifiers,
            false,
            &binds,
            0,
            0,
            0,
            true,
            false,
            None,
        );
        assert!(matches!(
            retain_overview_press(KeyMsg::Down, 0x25, first, true, &mut owned),
            Action::SwallowHotkey { emit: true, .. }
        ));
        for held in [modifiers, Modifiers::default(), alt()] {
            let repeat = decide(
                KeyMsg::Down,
                0x25,
                held,
                false,
                &binds,
                0,
                0,
                0,
                false,
                false,
                None,
            );
            assert!(matches!(
                retain_overview_press(KeyMsg::Down, 0x25, repeat, true, &mut owned),
                Action::Swallow
                    | Action::SwallowHotkey {
                        emit: false,
                        mask: None,
                        ..
                    }
            ));
        }
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x25, Action::Pass, false, &mut owned),
            Action::Pass,
            "closing the overview releases ownership even if key-up was missed"
        );
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x27, Action::Pass, true, &mut owned),
            Action::Pass,
            "unrelated arrows still reach the window"
        );
        assert_eq!(
            retain_overview_press(KeyMsg::Up, 0x25, Action::Pass, true, &mut owned),
            Action::Pass
        );
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x25, Action::Pass, true, &mut owned),
            Action::Pass,
            "a fresh bare arrow works after release"
        );
        assert!(matches!(
            retain_overview_press(KeyMsg::Down, 0x25, first, true, &mut owned),
            Action::SwallowHotkey { emit: true, .. }
        ));
    }

    #[test]
    fn closed_overview_does_not_change_hotkey_repeat_policy() {
        let mut owned = Vec::new();
        let action = Action::SwallowHotkey {
            event: HotkeyEvent { id: 123 },
            emit: true,
            mask: None,
        };
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x25, action, false, &mut owned),
            action
        );
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x25, Action::Pass, false, &mut owned),
            Action::Pass
        );
        assert_eq!(
            retain_overview_press(KeyMsg::Down, 0x25, Action::Pass, true, &mut owned),
            Action::Pass,
            "unmatched input is never claimed on open"
        );
    }

    #[test]
    fn exact_match_fires() {
        let binds = vec![bind(win_ctrl(), 0x25)]; // Win+Ctrl+Left
        let found = find_bind(&binds, win_ctrl(), 0x25);
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().id,
            crate::Hotkey::stable_id(win_ctrl(), 0x25)
        );
    }

    #[test]
    fn superset_modifiers_do_not_match() {
        let binds = vec![bind(win_ctrl(), 0x25)];
        let held = Modifiers {
            ctrl: true,
            win: true,
            shift: true, // extra modifier held
            ..Default::default()
        };
        assert!(find_bind(&binds, held, 0x25).is_none());
    }

    #[test]
    fn wrong_key_or_mods_does_not_match() {
        let binds = vec![bind(win_ctrl(), 0x25)];
        assert!(find_bind(&binds, win_ctrl(), 0x27).is_none()); // Right, not Left
        assert!(find_bind(
            &binds,
            Modifiers {
                ctrl: true,
                ..Default::default()
            },
            0x25
        )
        .is_none());
    }

    #[test]
    fn modifier_vks_are_recognized() {
        assert!(is_modifier_vk(VK_CONTROL));
        assert!(is_modifier_vk(VK_LWIN));
        assert!(is_modifier_vk(VK_RMENU));
        assert!(!is_modifier_vk(0x25)); // Left arrow
        assert!(!is_modifier_vk(0x41)); // 'A'
    }

    #[test]
    fn fn_modifier_matches_exactly() {
        let f13 = Modifiers {
            fn_mods: fn_mod_bit(0x7C).unwrap(),
            ..Default::default()
        };
        let binds = vec![bind(f13, 0x48)]; // F13+H
                                           // Exact F-modifier held -> fires.
        assert!(find_bind(&binds, f13, 0x48).is_some());
        assert!(find_bind(&binds, Modifiers::default(), 0x48).is_none());
        // A superset F-modifier (F13+F14) does not fire an F13-only bind.
        let f13_f14 = Modifiers {
            fn_mods: fn_mod_bit(0x7C).unwrap() | fn_mod_bit(0x7D).unwrap(),
            ..Default::default()
        };
        assert!(find_bind(&binds, f13_f14, 0x48).is_none());
        // A standard modifier held instead of the F-key does not fire.
        assert!(find_bind(
            &binds,
            Modifiers {
                ctrl: true,
                ..Default::default()
            },
            0x48
        )
        .is_none());
    }

    #[test]
    fn recording_swallows_once_and_never_matches_binds() {
        let binds = vec![bind(win_ctrl(), 0x48)];
        let held = win_ctrl();
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x48,
                held,
                true,
                &binds,
                0,
                0,
                0,
                true,
                false,
                None,
            ),
            Action::SwallowRecorded {
                modifiers: held,
                vk: 0x48,
                mask: None,
            }
        );
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x48,
                held,
                true,
                &binds,
                0,
                0,
                0,
                false,
                false,
                None,
            ),
            Action::Swallow
        );
    }

    #[test]
    fn recording_passes_modifiers_and_bare_controls() {
        let empty = Modifiers::default();
        assert_eq!(
            decide(
                KeyMsg::Down,
                VK_LWIN as u32,
                empty,
                true,
                &[],
                0,
                0,
                0,
                false,
                false,
                None
            ),
            Action::Pass
        );
        for vk in [VK_ESCAPE, VK_TAB, VK_BACK, VK_DELETE] {
            assert_eq!(
                decide(
                    KeyMsg::Down,
                    vk,
                    empty,
                    true,
                    &[],
                    0,
                    0,
                    0,
                    true,
                    false,
                    None,
                ),
                Action::RecordingControl
            );
        }
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert!(matches!(
            decide(
                KeyMsg::Down,
                VK_TAB,
                shift,
                true,
                &[],
                0,
                0,
                0,
                true,
                false,
                None,
            ),
            Action::SwallowRecorded { .. }
        ));
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(matches!(
            decide(
                KeyMsg::Down,
                VK_BACK,
                ctrl,
                true,
                &[],
                0,
                0,
                0,
                true,
                false,
                None,
            ),
            Action::Swallow
        ));
    }

    #[test]
    fn recording_chords_mask_start_and_alt_menus() {
        for (held, vk, mask) in [(win(), 0x24, Some(0x11)), (alt(), 0x4A, Some(0xE8))] {
            assert_eq!(
                decide(KeyMsg::Down, vk, held, true, &[], 0, 0, 0, true, true, None,),
                Action::SwallowRecorded {
                    modifiers: held,
                    vk,
                    mask,
                }
            );
        }
    }

    #[test]
    fn hotkeys_mask_menus_only_on_new_presses_without_ctrl() {
        for (modifiers, mask) in [
            (alt(), Some(0xE8)),
            (
                Modifiers {
                    shift: true,
                    ..alt()
                },
                Some(0xE8),
            ),
            (Modifiers { win: true, ..alt() }, Some(0xE8)),
            (
                Modifiers {
                    fn_mods: fn_mod_bit(0x7C).unwrap(),
                    ..alt()
                },
                Some(0xE8),
            ),
            (
                Modifiers {
                    ctrl: true,
                    ..alt()
                },
                None,
            ),
            (win(), Some(0x11)),
            (win_ctrl(), None),
            (
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                None,
            ),
            (Modifiers::default(), None),
        ] {
            let binds = [bind(modifiers, 0x4A)];
            for is_new_press in [true, false] {
                assert_eq!(
                    decide(
                        KeyMsg::Down,
                        0x4A,
                        modifiers,
                        false,
                        &binds,
                        0,
                        0,
                        0,
                        is_new_press,
                        false,
                        None,
                    ),
                    Action::SwallowHotkey {
                        event: HotkeyEvent { id: binds[0].id },
                        emit: is_new_press,
                        mask: if is_new_press { mask } else { None },
                    },
                    "modifiers={modifiers:?}, is_new_press={is_new_press}"
                );
            }
        }
    }

    #[test]
    fn recording_fn_chords_and_bare_key_are_emitted() {
        let f13 = fn_mod_bit(0x7C).unwrap();
        let held = Modifiers {
            fn_mods: f13,
            ..Default::default()
        };
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x48,
                held,
                true,
                &[],
                0,
                0,
                f13,
                true,
                false,
                None,
            ),
            Action::SwallowRecorded {
                modifiers: held,
                vk: 0x48,
                mask: None,
            }
        );
        assert_eq!(
            decide(
                KeyMsg::Up,
                0x7C,
                held,
                true,
                &[],
                0,
                0,
                f13,
                false,
                false,
                None,
            ),
            Action::PassRecorded {
                modifiers: Modifiers::default(),
                vk: 0x7C,
            }
        );
    }

    #[test]
    fn recorded_key_repeat_stays_swallowed_until_key_up() {
        let held = win();
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x24,
                held,
                false,
                &[],
                0,
                0,
                0,
                false,
                false,
                Some(0x24),
            ),
            Action::Swallow
        );
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x23,
                held,
                false,
                &[],
                0,
                0,
                0,
                true,
                false,
                Some(0x24),
            ),
            Action::Pass
        );
        assert_eq!(
            decide(
                KeyMsg::Up,
                0x24,
                Modifiers::default(),
                false,
                &[],
                0,
                0,
                0,
                false,
                false,
                Some(0x24),
            ),
            Action::Pass
        );
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x24,
                held,
                false,
                &[],
                0,
                0,
                0,
                true,
                false,
                None,
            ),
            Action::Pass
        );
    }

    #[test]
    fn non_recording_altgr_and_bind_behavior_is_unchanged() {
        let binds = vec![bind(win_ctrl(), 0x48)];
        assert_eq!(
            decide(
                KeyMsg::Down,
                0x48,
                win_ctrl(),
                false,
                &binds,
                0,
                0,
                0,
                true,
                true,
                None
            ),
            Action::Pass
        );
        assert!(matches!(
            decide(
                KeyMsg::Down,
                0x48,
                win_ctrl(),
                false,
                &binds,
                0,
                0,
                0,
                true,
                false,
                None
            ),
            Action::SwallowHotkey { .. }
        ));
    }
}
