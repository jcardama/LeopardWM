//! Low-level mouse hook for focus-follows-mouse.

use crate::{
    normalize_to_root_window, recover_poisoned_mutex, should_emit_window_event, Win32Error,
    WindowEvent, WM_QUIT_LLHOOK_THREAD,
};
use leopardwm_core_layout::WindowId;
use std::sync::mpsc;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    WH_MOUSE_LL, WM_MOUSEMOVE,
};

/// Global sender for mouse enter events.
static MOUSE_EVENT_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);

/// Track the window the mouse is currently over.
static CURRENT_MOUSE_WINDOW: std::sync::Mutex<Option<WindowId>> = std::sync::Mutex::new(None);

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
            for _ in 0..30 {
                if thread.is_finished() {
                    let _ = thread.join();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        tracing::debug!("Mouse hook uninstalled");

        // Clear the global sender (recover from mutex poisoning)
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
/// * `event_sender` - Sender for WindowEvent (specifically MouseEnterWindow)
pub fn install_mouse_hook(
    event_sender: mpsc::Sender<WindowEvent>,
) -> Result<MouseHookHandle, Win32Error> {
    // Store sender globally
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

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();

                // Ensure message queue exists
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Install the low-level mouse hook on this thread
                let hook =
                    match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_hook_proc), None, 0) {
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
                    let _ = DispatchMessageW(&msg);
                }

                let _ = UnhookWindowsHookEx(hook);
            }
        })
        .map_err(|e| {
            Win32Error::HookInstallFailed(format!("Failed to spawn mouse hook thread: {}", e))
        })?;

    // Wait for initialization
    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Mouse hook thread initialization failed".to_string())
    })??;

    tracing::info!("Low-level mouse hook installed for focus-follows-mouse");

    Ok(MouseHookHandle {
        thread_id,
        thread: Some(thread),
    })
}

/// Low-level mouse hook callback.
///
/// Tracks mouse movement and sends MouseEnterWindow events when the cursor
/// enters a different window.
unsafe extern "system" fn mouse_ll_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // If ncode < 0, we must call CallNextHookEx without processing
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    // Only process mouse move events
    if wparam.0 as u32 == WM_MOUSEMOVE {
        // Get the mouse position from the hook struct
        let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let point = mouse_struct.pt;

        // Find the window at the cursor position
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

        // Check if this is a different top-level window than before (recover from mutex poisoning)
        let mut current = CURRENT_MOUSE_WINDOW
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        if *current != candidate_window_id {
            *current = candidate_window_id;

            if let Some(hwnd) = candidate_hwnd {
                if should_emit_window_event(hwnd) {
                    let sender_guard = MOUSE_EVENT_SENDER
                        .lock()
                        .unwrap_or_else(recover_poisoned_mutex);
                    if let Some(sender) = sender_guard.as_ref() {
                        let _ = sender.send(WindowEvent::MouseEnterWindow(hwnd.0 as WindowId));
                    }
                }
            }
        }
    }

    // Always call next hook in the chain
    CallNextHookEx(None, ncode, wparam, lparam)
}
