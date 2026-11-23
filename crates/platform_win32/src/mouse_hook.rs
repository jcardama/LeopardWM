//! Low-level mouse hook for focus-follows-mouse.

use crate::{
    normalize_to_root_window, recover_poisoned_mutex, should_emit_window_event, Win32Error,
    WindowEvent, WM_QUIT_LLHOOK_THREAD,
};
use leopardwm_core_layout::WindowId;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    WH_MOUSE_LL, WM_MOUSEMOVE, WM_USER,
};

static MOUSE_EVENT_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);
static FOCUS_LOCK_MODIFIER: AtomicU32 = AtomicU32::new(0);
const WM_FOCUS_LOCK_MASK: u32 = WM_USER + 3;

pub fn set_focus_lock_modifier(modifier: &str) {
    let vk = match modifier {
        "Ctrl" => 0x11,
        "Alt" => 0x12,
        "Shift" => 0x10,
        "Win" => 0x5b,
        _ => crate::parse_vk(modifier)
            .filter(|vk| crate::fn_mod_bit(*vk).is_some())
            .unwrap_or(0),
    };
    FOCUS_LOCK_MODIFIER.store(vk, Ordering::SeqCst);
}

pub(crate) fn configured_focus_lock_vk() -> u32 {
    FOCUS_LOCK_MODIFIER.load(Ordering::SeqCst)
}

pub fn focus_lock_blocks_focus() -> bool {
    let vk = configured_focus_lock_vk();
    let held = crate::keyboard_hook::focus_lock_held(vk);
    if held {
        crate::keyboard_hook::mask_focus_lock(vk);
    }
    held
}

static CURRENT_MOUSE_WINDOW: std::sync::Mutex<Option<WindowId>> = std::sync::Mutex::new(None);

/// Whether the currently-tracked window is one we'd manage (i.e. we emitted a
/// `MouseEnterWindow` for it). Lets us fire `MouseLeftManaged` exactly on the
/// managed -> non-manageable transition, not on every move over the taskbar.
static CURRENT_OVER_MANAGED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Handle for the low-level mouse hook.
///
/// Dropping this handle will signal the dedicated message-pump thread to
/// unhook and exit.
pub struct MouseHookHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MouseHookHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT_LLHOOK_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
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
        tracing::debug!("Mouse hook uninstalled");

        let mut sender = MOUSE_EVENT_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *sender = None;
    }
}

/// Install a low-level mouse hook for focus-follows-mouse functionality.
///
/// Spawns a dedicated thread with a Win32 message pump so that `WH_MOUSE_LL`
/// callbacks are dispatched promptly.
///
/// # Arguments
/// * `event_sender` - Receives `MouseEnterWindow` and `MouseLeftManaged` events.
pub fn install_mouse_hook(
    event_sender: mpsc::Sender<WindowEvent>,
) -> Result<MouseHookHandle, Win32Error> {
    {
        let mut sender = MOUSE_EVENT_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Mouse sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Mouse sender already initialized - drop existing MouseHookHandle first"
                    .to_string(),
            ));
        }
        *sender = Some(event_sender);
    }

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();

                // PeekMessageW forces the queue to exist before the hook installs.
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_hook_proc), None, 0) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "SetWindowsHookExW failed: {}",
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
                    if msg.message == WM_FOCUS_LOCK_MASK {
                        crate::keyboard_hook::mask_focus_lock(msg.wParam.0 as u32);
                        continue;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                let _ = UnhookWindowsHookEx(hook);
            }
        })
        .map_err(|e| {
            Win32Error::HookInstallFailed(format!("Failed to spawn mouse hook thread: {}", e))
        })?;

    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Mouse hook thread initialization failed".to_string())
    })??;

    tracing::info!("Low-level mouse hook installed for focus-follows-mouse");

    Ok(MouseHookHandle {
        thread_id,
        thread: Some(thread),
    })
}

unsafe extern "system" fn mouse_ll_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // ncode < 0: the hook must not process the event, just chain.
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    if wparam.0 as u32 == WM_MOUSEMOVE {
        let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let point = mouse_struct.pt;

        let raw_hwnd = WindowFromPoint(point);
        let candidate_hwnd = if raw_hwnd.is_invalid() {
            None
        } else {
            let normalized = normalize_to_root_window(raw_hwnd);
            if normalized.is_invalid() {
                None
            } else {
                Some(normalized)
            }
        };
        let candidate_window_id = candidate_hwnd.map(|hwnd| hwnd.0 as WindowId);

        let mut current = CURRENT_MOUSE_WINDOW
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        if *current != candidate_window_id {
            *current = candidate_window_id;
            let focus_lock_vk = configured_focus_lock_vk();
            let focus_locked = crate::keyboard_hook::focus_lock_held(focus_lock_vk);
            // Gate on window transitions so moves within one window do not repeat events.
            let candidate_managed = candidate_hwnd.is_some_and(should_emit_window_event);
            let mut over_managed = CURRENT_OVER_MANAGED
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            let was_managed = *over_managed;
            *over_managed = candidate_managed;
            drop(over_managed);

            if candidate_managed {
                if let Some(hwnd) = candidate_hwnd {
                    let sender_guard = MOUSE_EVENT_SENDER
                        .lock()
                        .unwrap_or_else(recover_poisoned_mutex);
                    if let Some(sender) = sender_guard.as_ref() {
                        if focus_locked && matches!(focus_lock_vk, 0x12 | 0x5b) {
                            if let Err(error) = PostThreadMessageW(
                                GetCurrentThreadId(),
                                WM_FOCUS_LOCK_MASK,
                                windows::Win32::Foundation::WPARAM(focus_lock_vk as usize),
                                windows::Win32::Foundation::LPARAM(0),
                            ) {
                                tracing::warn!("Failed to queue focus-lock mask: {}", error);
                            }
                        }
                        let _ = sender.send(WindowEvent::MouseEnterWindow(
                            hwnd.0 as WindowId,
                            focus_locked,
                        ));
                    }
                }
            } else if was_managed {
                // Left a managed window for the taskbar/a popup: cancel any
                // pending focus so it doesn't fire on the window we just left.
                let sender_guard = MOUSE_EVENT_SENDER
                    .lock()
                    .unwrap_or_else(recover_poisoned_mutex);
                if let Some(sender) = sender_guard.as_ref() {
                    let _ = sender.send(WindowEvent::MouseLeftManaged);
                }
            }
        }
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}
