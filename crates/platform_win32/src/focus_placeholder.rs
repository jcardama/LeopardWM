//! Transparent foreground target used when an empty workspace needs to release focus.

use crate::Win32Error;
use leopardwm_core_layout::{Rect, WindowId};
use std::ffi::c_void;
use std::sync::mpsc;
use windows::core::BOOL;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
    GetForegroundWindow, GetMessageW, GetShellWindow, GetWindowThreadProcessId, IsWindow,
    PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, SetForegroundWindow,
    SetLayeredWindowAttributes, SetWindowPos, UnregisterClassW, HWND_TOP, LWA_ALPHA, MSG,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, WM_CLOSE, WM_DESTROY, WM_QUIT, WM_USER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TRANSPARENT, WS_POPUP,
};

const WINDOW_CLASS: &str = "LeopardWMFocusPlaceholder";
const OWNER_CLASS: &str = "LeopardWMFocusPlaceholderOwner";
const WM_STOP: u32 = WM_USER + 120;

fn work_area_center(work_area: Rect) -> (i32, i32) {
    (
        work_area.x.saturating_add(work_area.width.max(1) / 2),
        work_area.y.saturating_add(work_area.height.max(1) / 2),
    )
}

fn window_class_name(hwnd: HWND) -> Option<String> {
    let mut class_name = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
    (length > 0).then(|| String::from_utf16_lossy(&class_name[..length as usize]))
}

fn is_focus_window_class(class_name: &str) -> bool {
    matches!(class_name, WINDOW_CLASS | OWNER_CLASS)
}

/// Identify the internal focus target without treating it as a managed window.
pub fn is_focus_placeholder(hwnd: WindowId) -> bool {
    window_class_name(HWND(hwnd as *mut c_void)).as_deref() == Some(WINDOW_CLASS)
}

/// Owns a transparent, click-through window that can safely receive foreground.
pub struct FocusPlaceholder {
    hwnd: isize,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FocusPlaceholder {
    /// Create the placeholder and its hidden owner on a dedicated message-loop thread.
    pub fn new() -> Result<Self, Win32Error> {
        let (init_tx, init_rx) = mpsc::channel::<Result<(isize, u32), Win32Error>>();
        let thread = std::thread::Builder::new()
            .name("focus-placeholder".into())
            .spawn(move || unsafe {
                let class_name: Vec<u16> = format!("{WINDOW_CLASS}\0").encode_utf16().collect();
                let owner_class_name: Vec<u16> =
                    format!("{OWNER_CLASS}\0").encode_utf16().collect();
                let class_name_ptr = windows::core::PCWSTR(class_name.as_ptr());
                let owner_class_name_ptr = windows::core::PCWSTR(owner_class_name.as_ptr());
                let class = WNDCLASSW {
                    lpfnWndProc: Some(placeholder_window_proc),
                    lpszClassName: class_name_ptr,
                    ..Default::default()
                };
                let owner_class = WNDCLASSW {
                    lpfnWndProc: Some(placeholder_window_proc),
                    lpszClassName: owner_class_name_ptr,
                    ..Default::default()
                };
                if RegisterClassW(&class) == 0 || RegisterClassW(&owner_class) == 0 {
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(
                        "Failed to register focus-placeholder window classes".into(),
                    )));
                    let _ = UnregisterClassW(class_name_ptr, None);
                    let _ = UnregisterClassW(owner_class_name_ptr, None);
                    return;
                }

                let owner = match CreateWindowExW(
                    Default::default(),
                    owner_class_name_ptr,
                    None,
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "Failed to create focus-placeholder owner: {error}"
                        ))));
                        let _ = UnregisterClassW(class_name_ptr, None);
                        let _ = UnregisterClassW(owner_class_name_ptr, None);
                        return;
                    }
                };

                let placeholder = match CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TRANSPARENT,
                    class_name_ptr,
                    None,
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    Some(owner),
                    None,
                    None,
                    None,
                ) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "Failed to create focus placeholder: {error}"
                        ))));
                        let _ = DestroyWindow(owner);
                        let _ = UnregisterClassW(class_name_ptr, None);
                        let _ = UnregisterClassW(owner_class_name_ptr, None);
                        return;
                    }
                };
                if let Err(error) =
                    SetLayeredWindowAttributes(placeholder, COLORREF(0), 0, LWA_ALPHA)
                {
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                        "Failed to make focus placeholder transparent: {error}"
                    ))));
                    let _ = DestroyWindow(placeholder);
                    let _ = DestroyWindow(owner);
                    let _ = UnregisterClassW(class_name_ptr, None);
                    let _ = UnregisterClassW(owner_class_name_ptr, None);
                    return;
                }

                // Cloak before showing so the shell never presents this focus target.
                // Keep its styles and Win32 visibility unchanged for foreground restoration.
                let cloaked = BOOL::from(true);
                if let Err(error) = DwmSetWindowAttribute(
                    placeholder,
                    DWMWA_CLOAK,
                    &cloaked as *const _ as _,
                    std::mem::size_of::<BOOL>() as u32,
                ) {
                    tracing::warn!(
                        "Failed to cloak focus placeholder; preserving empty-workspace focus without Alt+Tab suppression: {error}"
                    );
                }

                let thread_id = GetCurrentThreadId();
                if init_tx
                    .send(Ok((placeholder.0 as isize, thread_id)))
                    .is_err()
                {
                    let _ = DestroyWindow(placeholder);
                    let _ = DestroyWindow(owner);
                    let _ = UnregisterClassW(class_name_ptr, None);
                    let _ = UnregisterClassW(owner_class_name_ptr, None);
                    return;
                }

                let mut msg = MSG::default();
                loop {
                    if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }
                restore_foreground_before_destroy(placeholder);
                let _ = DestroyWindow(placeholder);
                let _ = DestroyWindow(owner);
                let _ = UnregisterClassW(class_name_ptr, None);
                let _ = UnregisterClassW(owner_class_name_ptr, None);
            })
            .map_err(|error| {
                Win32Error::HookInstallFailed(format!(
                    "Failed to spawn focus-placeholder thread: {error}"
                ))
            })?;

        match init_rx.recv() {
            Ok(Ok((hwnd, thread_id))) => Ok(Self {
                hwnd,
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Win32Error::HookInstallFailed(
                    "Focus-placeholder thread exited during initialization".into(),
                ))
            }
        }
    }

    /// Move the placeholder onto the selected monitor and transfer foreground only
    /// if the parked window remains foreground throughout the handoff.
    pub fn release_foreground(
        &self,
        expected: WindowId,
        work_area: Rect,
    ) -> Result<bool, Win32Error> {
        let expected_hwnd = crate::window_id_to_hwnd(expected)?;
        let hwnd = HWND(self.hwnd as *mut c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(Win32Error::WindowNotFound(self.hwnd as WindowId));
        }
        if unsafe { GetForegroundWindow() } != expected_hwnd {
            return Ok(false);
        }

        let (x, y) = work_area_center(work_area);
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                x,
                y,
                1,
                1,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
            )
            .map_err(|error| {
                Win32Error::SetPositionFailed(format!(
                    "Could not position focus placeholder: {error}"
                ))
            })?;
        }

        let expected_thread = unsafe { GetWindowThreadProcessId(expected_hwnd, None) };
        if expected_thread == 0 {
            return Err(Win32Error::SetPositionFailed(format!(
                "GetWindowThreadProcessId returned 0 for window {expected}"
            )));
        }
        let current_thread = unsafe { GetCurrentThreadId() };
        let mut attached = Vec::new();
        for thread_id in [expected_thread, self.thread_id] {
            if thread_id == current_thread || attached.contains(&thread_id) {
                continue;
            }
            if !unsafe { AttachThreadInput(current_thread, thread_id, true) }.as_bool() {
                detach_input_threads(current_thread, &attached);
                return Err(Win32Error::SetPositionFailed(format!(
                    "AttachThreadInput attach failed (current_thread={current_thread}, other_thread={thread_id})"
                )));
            }
            attached.push(thread_id);
        }

        let foreground = unsafe { GetForegroundWindow() };
        let foreground_set = if foreground == expected_hwnd {
            unsafe { SetForegroundWindow(hwnd).as_bool() }
        } else {
            false
        };
        detach_input_threads(current_thread, &attached);
        Ok(foreground_set)
    }
}

fn restore_foreground_before_destroy(placeholder: HWND) {
    if unsafe { GetForegroundWindow() } != placeholder {
        return;
    }
    let shell = unsafe { GetShellWindow() };
    if shell.0.is_null() {
        tracing::debug!("Could not release focus placeholder during shutdown: no shell window");
        return;
    }
    if unsafe { SetForegroundWindow(shell) }.as_bool() {
        return;
    }

    let current_thread = unsafe { GetCurrentThreadId() };
    let shell_thread = unsafe { GetWindowThreadProcessId(shell, None) };
    if shell_thread == 0 {
        tracing::debug!(
            "Could not release focus placeholder during shutdown: shell thread unavailable"
        );
        return;
    }
    if shell_thread != current_thread
        && !unsafe { AttachThreadInput(current_thread, shell_thread, true) }.as_bool()
    {
        tracing::debug!("Could not attach to shell input thread during focus-placeholder shutdown");
        return;
    }

    let released = unsafe { SetForegroundWindow(shell) }.as_bool();
    if shell_thread != current_thread
        && !unsafe { AttachThreadInput(current_thread, shell_thread, false) }.as_bool()
    {
        tracing::debug!(
            "Could not detach from shell input thread during focus-placeholder shutdown"
        );
    }
    if !released {
        tracing::debug!("Windows refused the focus-placeholder shutdown handoff to the shell");
    }
}

impl Drop for FocusPlaceholder {
    fn drop(&mut self) {
        let thread_quit_posted =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok() };
        if let Some(thread) = self.thread.take() {
            let window_quit_posted = if thread_quit_posted || thread.is_finished() {
                false
            } else {
                unsafe {
                    PostMessageW(
                        Some(HWND(self.hwnd as *mut c_void)),
                        WM_STOP,
                        WPARAM(0),
                        LPARAM(0),
                    )
                    .is_ok()
                }
            };
            if thread_quit_posted || window_quit_posted || thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!(
                    "Could not signal focus-placeholder thread {} to stop; detaching it",
                    self.thread_id
                );
            }
        }
    }
}

fn detach_input_threads(current_thread: u32, attached: &[u32]) {
    for thread_id in attached.iter().rev() {
        if !unsafe { AttachThreadInput(current_thread, *thread_id, false) }.as_bool() {
            tracing::warn!(
                "AttachThreadInput detach failed (current_thread={}, other_thread={})",
                current_thread,
                thread_id
            );
        }
    }
}

unsafe extern "system" fn placeholder_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => LRESULT(0),
        WM_STOP => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_DESTROY if window_class_name(hwnd).is_some_and(|name| is_focus_window_class(&name)) => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_focus_window_class, work_area_center, OWNER_CLASS, WINDOW_CLASS};
    use leopardwm_core_layout::Rect;

    #[test]
    fn placeholder_position_uses_monitor_work_area_center() {
        assert_eq!(
            work_area_center(Rect::new(-1920, 100, 1920, 800)),
            (-960, 500)
        );
    }

    #[test]
    fn either_focus_window_class_stops_its_message_loop_on_destroy() {
        assert!(is_focus_window_class(WINDOW_CLASS));
        assert!(is_focus_window_class(OWNER_CLASS));
        assert!(!is_focus_window_class("UnrelatedWindow"));
    }
}
