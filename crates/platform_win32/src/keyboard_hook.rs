//! Reclaim OS-reserved keyboard shortcuts via a low-level keyboard hook.
//!
//! When `RegisterHotKey` can't claim a combo because Windows owns it (e.g.
//! `Win+Ctrl+Arrow` for virtual-desktop switching), this opt-in `WH_KEYBOARD_LL`
//! hook swallows the keystroke and re-emits it as a [`HotkeyEvent`] so the daemon
//! runs the bound command. It only matches a curated set of combos the caller
//! passes in, so it never steals shortcuts owned by other apps.
//!
//! Mirrors the dedicated-thread + message-pump pattern in `gestures.rs`.

use crate::{recover_poisoned_mutex, HotkeyEvent, HotkeyId, Modifiers, Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::mpsc;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// A combo to reclaim: its modifiers, virtual-key code, and the hotkey id to
/// emit when it fires.
#[derive(Debug, Clone, Copy)]
pub struct ReclaimBind {
    pub modifiers: Modifiers,
    pub vk: u32,
    pub id: HotkeyId,
}

/// Sender the hook proc uses to deliver reclaimed combos to the daemon.
static RECLAIM_SENDER: std::sync::Mutex<Option<mpsc::Sender<HotkeyEvent>>> =
    std::sync::Mutex::new(None);
/// The set of combos the hook should reclaim.
static RECLAIM_BINDS: std::sync::Mutex<Vec<ReclaimBind>> = std::sync::Mutex::new(Vec::new());
/// Virtual-keys of currently-held reclaimed main keys. Lets us fire once per
/// physical press and swallow auto-repeat, tracking each key independently so a
/// second reclaimed key held at the same time can't reset the first.
static RECLAIM_HELD: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

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

/// Find the reclaim bind matching exactly the held modifiers and key. Exact
/// match means a superset (extra modifier held) does not fire it, matching
/// `RegisterHotKey` semantics.
fn find_reclaim(binds: &[ReclaimBind], held: Modifiers, vk: u32) -> Option<ReclaimBind> {
    binds
        .iter()
        .find(|b| b.vk == vk && b.modifiers == held)
        .copied()
}

/// Handle for the reclaim hook. Dropping it signals the dedicated thread to
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
        let mut sender = RECLAIM_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        *sender = None;
        drop(sender);
        let mut binds = RECLAIM_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
        binds.clear();
        drop(binds);
        let mut held = RECLAIM_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
        held.clear();
        tracing::debug!("OS-shortcut reclaim hook stopped");
    }
}

/// Install the reclaim hook for the given combos. Spawns a dedicated thread
/// with a message pump (required for `WH_KEYBOARD_LL`). Returns a handle that
/// must be kept alive and a receiver for reclaimed combos.
pub fn install_keyboard_hook(
    binds: Vec<ReclaimBind>,
) -> Result<(KeyboardHookHandle, mpsc::Receiver<HotkeyEvent>), Win32Error> {
    let count = binds.len();
    let (tx, rx) = mpsc::channel();

    {
        let mut sender = RECLAIM_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Reclaim sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Reclaim hook already installed - drop existing handle first".to_string(),
            ));
        }
        *sender = Some(tx);
    }
    {
        let mut b = RECLAIM_BINDS
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Reclaim binds mutex poisoned".to_string()))?;
        *b = binds;
    }
    {
        let mut held = RECLAIM_HELD
            .lock()
            .map_err(|_| Win32Error::HookInstallFailed("Reclaim held mutex poisoned".to_string()))?;
        held.clear();
    }

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("reclaim-hook".into())
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
            Win32Error::HookInstallFailed(format!("Failed to spawn reclaim thread: {}", e))
        })?;

    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Reclaim thread initialization failed".to_string())
    })??;

    tracing::info!("OS-shortcut reclaim hook installed ({} combos)", count);

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
        let mut held = RECLAIM_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
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

    let held = Modifiers {
        ctrl: GetAsyncKeyState(VK_CONTROL) < 0,
        alt: GetAsyncKeyState(VK_MENU) < 0,
        shift: GetAsyncKeyState(VK_SHIFT) < 0,
        win: GetAsyncKeyState(VK_LWIN) < 0 || GetAsyncKeyState(VK_RWIN) < 0,
    };

    let matched = {
        let binds = RECLAIM_BINDS.lock().unwrap_or_else(recover_poisoned_mutex);
        find_reclaim(&binds, held, vk as u32)
    };

    if let Some(bind) = matched {
        // Fire once per physical press; swallow auto-repeat without re-firing.
        // Always swallow so the OS action (e.g. desktop switch) never leaks.
        let is_new_press = {
            let mut held = RECLAIM_HELD.lock().unwrap_or_else(recover_poisoned_mutex);
            if held.contains(&vk) {
                false
            } else {
                held.push(vk);
                true
            }
        };
        if is_new_press {
            let sender = RECLAIM_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(s) = sender.as_ref() {
                let _ = s.send(HotkeyEvent { id: bind.id });
            }
        }
        return LRESULT(1);
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(modifiers: Modifiers, vk: u32) -> ReclaimBind {
        ReclaimBind {
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
        let found = find_reclaim(&binds, win_ctrl(), 0x25);
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
        assert!(find_reclaim(&binds, held, 0x25).is_none());
    }

    #[test]
    fn wrong_key_or_mods_does_not_match() {
        let binds = vec![bind(win_ctrl(), 0x25)];
        assert!(find_reclaim(&binds, win_ctrl(), 0x27).is_none()); // Right, not Left
        assert!(find_reclaim(&binds, Modifiers { ctrl: true, ..Default::default() }, 0x25).is_none());
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
