//! WinEvent hook installation and dispatch.

use crate::enumeration::{
    normalize_to_root_window, should_emit_window_event_for,
    should_filter_window_event_by_manageability,
};
use crate::recover_poisoned_mutex;
use leopardwm_core_layout::WindowId;
use std::sync::mpsc;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetForegroundWindow, GetMessageW, PeekMessageW, PostThreadMessageW, MSG,
    PM_NOREMOVE, WM_USER,
};

// WinEvent constants (not all are exposed by windows-rs)
pub(crate) const EVENT_OBJECT_CREATE: u32 = 0x8000;
pub(crate) const EVENT_OBJECT_DESTROY: u32 = 0x8001;
pub(crate) const EVENT_OBJECT_SHOW: u32 = 0x8002;
pub(crate) const EVENT_OBJECT_HIDE: u32 = 0x8003;
pub(crate) const EVENT_OBJECT_FOCUS: u32 = 0x8005;
pub(crate) const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
pub(crate) const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
pub(crate) const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
pub(crate) const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000A;
pub(crate) const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
pub(crate) const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
const OBJID_WINDOW: i32 = 0;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

/// Custom message ID used to signal the WinEvent hook thread to exit.
const WM_QUIT_WINEVENT_THREAD: u32 = WM_USER + 3;

/// Window event types that the daemon needs to handle.
#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// A new window was created.
    Created(WindowId),
    /// A window was destroyed.
    Destroyed(WindowId),
    /// A window was hidden (e.g., close-to-tray apps using ShowWindow(SW_HIDE)).
    Hidden(WindowId),
    /// A window received focus.
    Focused(WindowId),
    /// A window was minimized.
    Minimized(WindowId),
    /// A window was restored from minimized state.
    Restored(WindowId),
    /// A window was moved or resized by the user.
    MovedOrResized(WindowId),
    /// User started dragging/resizing a window.
    MoveSizeStart(WindowId),
    /// User finished dragging/resizing a window.
    MoveSizeEnd(WindowId),
    /// Display configuration changed (monitors added/removed/rearranged).
    DisplayChange,
    /// Mouse cursor entered a window (for focus-follows-mouse).
    MouseEnterWindow(WindowId),
}

/// Global sender for window events from WinEvent callbacks.
///
/// This uses a thread-safe channel because WinEvent callbacks run on Windows'
/// internal thread pool and we need to forward events to the async runtime.
static EVENT_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);

pub(crate) fn set_event_sender(sender: mpsc::Sender<WindowEvent>) -> Result<(), crate::Win32Error> {
    let mut guard = EVENT_SENDER
        .lock()
        .map_err(|_| crate::Win32Error::HookInstallFailed("Event sender mutex poisoned".to_string()))?;
    if guard.is_some() {
        return Err(crate::Win32Error::HookInstallFailed(
            "Event sender already initialized - drop existing EventHookHandle first".to_string(),
        ));
    }
    *guard = Some(sender);
    Ok(())
}

pub(crate) fn clear_event_sender() {
    let mut guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    *guard = None;
}

pub(crate) fn clone_event_sender() -> Option<mpsc::Sender<WindowEvent>> {
    let guard = EVENT_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    guard.as_ref().cloned()
}

/// Handle for installed event hooks.
///
/// Dropping this handle will unhook all installed event hooks.
pub struct EventHookHandle {
    thread_id: u32,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EventHookHandle {
    fn drop(&mut self) {
        // Signal the dedicated thread to exit
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT_WINEVENT_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(handle) = self._thread.take() {
            let _ = handle.join();
        }
        clear_event_sender();
        tracing::debug!("WinEvent hook thread stopped");
    }
}

/// Install WinEvent hooks to receive window lifecycle events.
///
/// Spawns a dedicated thread with a Win32 message pump so that
/// `WINEVENT_OUTOFCONTEXT` callbacks are dispatched reliably.
///
/// Returns a handle that must be kept alive to receive events.
/// Also returns a receiver channel for the events.
///
/// # Events Hooked
/// - Window creation (EVENT_OBJECT_CREATE)
/// - Window destruction (EVENT_OBJECT_DESTROY)
/// - Foreground change (EVENT_SYSTEM_FOREGROUND)
/// - Minimize/restore (EVENT_SYSTEM_MINIMIZESTART/END)
/// - Drag start/end (EVENT_SYSTEM_MOVESIZESTART/END)
/// - Move/resize (EVENT_OBJECT_LOCATIONCHANGE)
/// - Focus within app (EVENT_OBJECT_FOCUS)
pub fn install_event_hooks() -> Result<(EventHookHandle, mpsc::Receiver<WindowEvent>), crate::Win32Error> {
    // Create channel for events
    let (tx, rx) = mpsc::channel();

    // Store sender globally for callback access
    set_event_sender(tx)?;

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, crate::Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("winevent-pump".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();

                // Ensure message queue exists before installing hooks
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Define events to hook: (min_event, max_event)
                let event_ranges = [
                    (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
                    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                    (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
                    (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
                    (EVENT_OBJECT_FOCUS, EVENT_OBJECT_FOCUS),
                ];

                let mut hooks = Vec::new();

                for (min_event, max_event) in event_ranges {
                    let hook = SetWinEventHook(
                        min_event,
                        max_event,
                        None,
                        Some(win_event_callback),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    );

                    if hook.is_invalid() {
                        for h in &hooks {
                            let _ = UnhookWinEvent(*h);
                        }
                        let _ = init_tx.send(Err(crate::Win32Error::HookInstallFailed(format!(
                            "SetWinEventHook failed for events {}-{}",
                            min_event, max_event
                        ))));
                        return;
                    }

                    hooks.push(hook);
                }

                tracing::info!("Installed {} WinEvent hooks", hooks.len());
                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WINEVENT_OUTOFCONTEXT callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_WINEVENT_THREAD {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                // Clean up hooks
                for hook in &hooks {
                    if !UnhookWinEvent(*hook).as_bool() {
                        tracing::warn!("Failed to unhook WinEvent: {:?}", hook);
                    }
                }
            }
        })
        .map_err(|e| {
            crate::Win32Error::HookInstallFailed(format!("Failed to spawn winevent-pump thread: {}", e))
        })?;

    // Wait for init result
    match init_rx.recv() {
        Ok(Ok(thread_id)) => Ok((
            EventHookHandle {
                thread_id,
                _thread: Some(thread),
            },
            rx,
        )),
        Ok(Err(e)) => {
            let _ = thread.join();
            clear_event_sender();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            clear_event_sender();
            Err(crate::Win32Error::HookInstallFailed(
                "WinEvent hook thread exited during init".to_string(),
            ))
        }
    }
}

/// Callback function for WinEvent hooks.
///
/// This runs on Windows' thread pool, so we forward events to the channel.
/// Wrapped with catch_unwind to prevent panics from crashing the application.
unsafe extern "system" fn win_event_callback(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    id_event_thread: u32,
    dwms_event_time: u32,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        win_event_callback_inner(
            hook,
            event,
            hwnd,
            id_object,
            id_child,
            id_event_thread,
            dwms_event_time,
        )
    }));

    if let Err(e) = result {
        // Can't use tracing here safely in all contexts, use eprintln
        eprintln!("Panic in win_event_callback: {:?}", e);
    }
}

/// Inner implementation of WinEvent callback.
fn win_event_callback_inner(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Only handle window-level events (not child objects like menus).
    // Exception: EVENT_OBJECT_FOCUS fires with OBJID_CLIENT, and
    // EVENT_SYSTEM_* events always have id_object == 0, so we allow
    // focus/foreground events regardless of id_object.
    // EVENT_SYSTEM_FOREGROUND and EVENT_OBJECT_FOCUS fire with id_object == 0
    // or OBJID_CLIENT, so allow them regardless. But EVENT_OBJECT_SHOW/HIDE
    // must be OBJID_WINDOW only — child control visibility changes should not
    // be emitted as top-level window lifecycle events.
    let is_focus_event = matches!(
        event,
        EVENT_SYSTEM_FOREGROUND | EVENT_OBJECT_FOCUS
    );
    if id_object != OBJID_WINDOW && !is_focus_event {
        return;
    }

    // Ignore invalid HWNDs
    if hwnd.0.is_null() {
        return;
    }

    // For destroy/hide events, skip normalization — the window may already be gone,
    // and GetAncestor would return null. Use the HWND as-is.
    let hwnd = if matches!(event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) {
        hwnd
    } else {
        normalize_to_root_window(hwnd)
    };

    if should_filter_window_event_by_manageability(event)
        && !should_emit_window_event_for(event, hwnd)
    {
        return;
    }

    let window_id = hwnd.0 as WindowId;

    // Suppress LOCATIONCHANGE and SHOW events for windows currently cloaked
    // by our placement system. DWM cloaking fires EVENT_OBJECT_LOCATIONCHANGE
    // on both cloak and uncloak, which would cascade into snap-back loops.
    // SHOW fires on uncloak — harmless ("already managed") but noisy.
    // HIDE is NOT suppressed: cloaking doesn't fire HIDE, and real hide
    // events (minimize, close-to-tray) must reach the daemon.
    if matches!(event, EVENT_OBJECT_SHOW | EVENT_OBJECT_LOCATIONCHANGE)
        && crate::is_placement_cloaked(window_id)
    {
        return;
    }

    // Map event to our WindowEvent type
    let window_event = match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => WindowEvent::Created(window_id),
        EVENT_OBJECT_DESTROY => WindowEvent::Destroyed(window_id),
        EVENT_OBJECT_HIDE => WindowEvent::Hidden(window_id),
        EVENT_SYSTEM_FOREGROUND => WindowEvent::Focused(window_id),
        EVENT_OBJECT_FOCUS => {
            // Only emit Focused for EVENT_OBJECT_FOCUS if the window is actually
            // the foreground window. This filters out spurious focus events from
            // Windows' "scroll inactive windows" feature — when the mouse wheel
            // is delivered to a non-foreground window (e.g., the other window in
            // a stacked column), some apps fire EVENT_OBJECT_FOCUS without the
            // window truly becoming foreground, causing the border to flicker.
            let fg = unsafe { GetForegroundWindow() };
            if fg != hwnd {
                return;
            }
            WindowEvent::Focused(window_id)
        }
        EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized(window_id),
        EVENT_SYSTEM_MINIMIZEEND => WindowEvent::Restored(window_id),
        EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MoveSizeStart(window_id),
        EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MoveSizeEnd(window_id),
        EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::MovedOrResized(window_id),
        _ => return,
    };

    // Send event through channel
    if let Some(sender) = clone_event_sender() {
        let _ = sender.send(window_event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static GLOBAL_SENDER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_event_sender_can_be_reinstalled_after_clear() {
        let _guard = GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        clear_event_sender();

        let (first_tx, _first_rx) = mpsc::channel::<WindowEvent>();
        assert!(set_event_sender(first_tx).is_ok());

        let (second_tx, _second_rx) = mpsc::channel::<WindowEvent>();
        let err = set_event_sender(second_tx).unwrap_err();
        assert!(matches!(err, crate::Win32Error::HookInstallFailed(_)));

        clear_event_sender();

        let (third_tx, _third_rx) = mpsc::channel::<WindowEvent>();
        assert!(set_event_sender(third_tx).is_ok());
        clear_event_sender();
    }
}
