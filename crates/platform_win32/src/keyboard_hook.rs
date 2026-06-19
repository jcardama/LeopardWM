//! Global hotkey matching via a low-level keyboard hook.
//!
//! A `WH_KEYBOARD_LL` hook is LeopardWM's sole hotkey matcher: it inspects every
//! key-down, and when the modifiers and key match a configured bind it swallows
//! the keystroke and re-emits it as a [`HotkeyEvent`] so the daemon runs the
//! bound command. Matching here (rather than `RegisterHotKey`) lets us tell
//! left/right modifiers apart, so AltGr (Left Ctrl + Right Alt on international
//! layouts) types normally instead of firing Ctrl+Alt binds. (#44)
//!
//! Mirrors the dedicated-thread + message-pump pattern in `gestures.rs`.

use crate::{recover_poisoned_mutex, HotkeyEvent, HotkeyId, Modifiers, Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::mpsc;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// A hotkey bind the hook matches: its modifiers, virtual-key code, and the
/// hotkey id to emit when it fires.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyBind {
    pub modifiers: Modifiers,
    pub vk: u32,
    pub id: HotkeyId,
}

/// Sender the hook proc uses to deliver matched binds to the daemon.
static HOOK_SENDER: std::sync::Mutex<Option<mpsc::Sender<HotkeyEvent>>> =
    std::sync::Mutex::new(None);
/// The set of binds the hook should match.
static HOOK_BINDS: std::sync::Mutex<Vec<HotkeyBind>> = std::sync::Mutex::new(Vec::new());
/// Virtual-keys of currently-held matched main keys. Lets us fire once per
/// physical press and swallow auto-repeat, tracking each key independently so a
/// second matched key held at the same time can't reset the first.
static HOOK_HELD: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

// Modifier virtual-key codes (both the generic and left/right variants the
// low-level hook reports).
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

fn is_modifier_vk(vk: i32) -> bool {
    matches!(
        vk,
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL
            | VK_RCONTROL | VK_LMENU | VK_RMENU
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

/// Handle for the keyboard hook. Dropping it signals the dedicated thread to
/// unhook and exit, then clears the global state.
pub struct KeyboardHookHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for KeyboardHookHandle {
    fn drop(&mut self) {
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
        let mut sender = HOOK_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        *sender = None;
        drop(sender);
        let mut binds = HOOK_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
        binds.clear();
        drop(binds);
        let mut held = HOOK_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        held.clear();
        tracing::debug!("Keyboard hook stopped");
    }
}

/// Install the keyboard hook for the given binds. Spawns a dedicated thread
/// with a message pump (required for `WH_KEYBOARD_LL`). Returns a handle that
/// must be kept alive and a receiver for matched binds.
pub fn install_keyboard_hook(
    binds: Vec<HotkeyBind>,
) -> Result<(KeyboardHookHandle, mpsc::Receiver<HotkeyEvent>), Win32Error> {
    let count = binds.len();
    let (tx, rx) = mpsc::channel();

    {
        let mut sender = HOOK_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Hook sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Keyboard hook already installed - drop existing handle first".to_string(),
            ));
        }
        *sender = Some(tx);
    }
    {
        let mut b = HOOK_BINDS
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Hook binds mutex poisoned".to_string()))?;
        *b = binds;
    }
    {
        let mut held = HOOK_HELD
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Hook held mutex poisoned".to_string()))?;
        held.clear();
    }

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

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

    let msg = wparam.0 as u32;
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = kb.vkCode as i32;

    // On key-up, drop the key from the held set so its next press fires again.
    // Always pass key-ups through.
    if msg == WM_KEYUP || msg == WM_SYSKEYUP {
        let mut held = HOOK_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        held.retain(|&k| k != vk);
        drop(held);
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    // Modifier key-downs never match a bind on their own — pass through so the
    // modifier still works (and so Windows still sees it held).
    if is_modifier_vk(vk) {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    // Track the physical down-state of every non-modifier key so a bind fires
    // once per physical press and never on auto-repeat. Recorded for matched and
    // unmatched keys alike: otherwise a key held bare (e.g. typing it), then
    // joined by modifiers, would look freshly pressed on its next auto-repeat
    // and fire the now-matching bind. The key-up handler above clears it.
    let is_new_press = {
        let mut held = HOOK_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        if held.contains(&vk) {
            false
        } else {
            held.push(vk);
            true
        }
    };

    // AltGr emits Left Ctrl + Right Alt. Treat any Right-Alt-down state as AltGr
    // and never match a bind, so AltGr combos pass through and type normally on
    // international layouts (the synthesized Left Ctrl would otherwise satisfy a
    // Ctrl bind too). (#44)
    if GetAsyncKeyState(VK_RMENU) < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    // Read the left/right-specific modifiers: Alt means Left Alt only (Right Alt
    // is AltGr, handled above); Ctrl/Shift/Win accept either side.
    let held = Modifiers {
        ctrl: GetAsyncKeyState(VK_LCONTROL) < 0 || GetAsyncKeyState(VK_RCONTROL) < 0,
        alt: GetAsyncKeyState(VK_LMENU) < 0,
        shift: GetAsyncKeyState(VK_LSHIFT) < 0 || GetAsyncKeyState(VK_RSHIFT) < 0,
        win: GetAsyncKeyState(VK_LWIN) < 0 || GetAsyncKeyState(VK_RWIN) < 0,
    };

    let matched = {
        let binds = HOOK_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
        find_bind(&binds, held, vk as u32)
    };

    if let Some(bind) = matched {
        // Fire once per physical press; swallow auto-repeat without re-firing.
        // Always swallow so the OS action (e.g. desktop switch) never leaks.
        if is_new_press {
            let sender = HOOK_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(s) = sender.as_ref() {
                let _ = s.send(HotkeyEvent { id: bind.id });
            }
            // For a bare-Win bind, swallowing the main key leaves Windows seeing
            // Win pressed and released with nothing in between, which pops the
            // Start menu on key-up. Inject a no-op modifier tap so the OS treats
            // the Win press as part of a chord and suppresses the menu. Combos
            // with another modifier already use up the Win press, so skip them.
            let m = bind.modifiers;
            if m.win && !m.ctrl && !m.alt && !m.shift {
                send_start_menu_mask();
            }
        }
        return LRESULT(1);
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}

/// Inject a Ctrl key tap to mask a bare-Win press so it doesn't pop the Start
/// menu after the hook swallows the bound key. Ctrl alone is inert, so the tap
/// has no user-visible side effect.
unsafe fn send_start_menu_mask() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_CONTROL as u16),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(modifiers: Modifiers, vk: u32) -> HotkeyBind {
        HotkeyBind {
            modifiers,
            vk,
            id: crate::Hotkey::stable_id(modifiers, vk),
        }
    }

    fn win_ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            win: true,
            ..Default::default()
        }
    }

    #[test]
    fn exact_match_fires() {
        let binds = vec![bind(win_ctrl(), 0x25)]; // Win+Ctrl+Left
        let found = find_bind(&binds, win_ctrl(), 0x25);
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, crate::Hotkey::stable_id(win_ctrl(), 0x25));
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
        assert!(find_bind(&binds, Modifiers { ctrl: true, ..Default::default() }, 0x25).is_none());
    }

    #[test]
    fn modifier_vks_are_recognized() {
        assert!(is_modifier_vk(VK_CONTROL));
        assert!(is_modifier_vk(VK_LWIN));
        assert!(is_modifier_vk(VK_RMENU));
        assert!(!is_modifier_vk(0x25)); // Left arrow
        assert!(!is_modifier_vk(0x41)); // 'A'
    }
}
