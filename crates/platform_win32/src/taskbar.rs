//! Hide/show taskbar buttons for managed windows via `ITaskbarList`.
//!
//! DWM cloaking can't hide an external window's taskbar button (it returns
//! `E_ACCESSDENIED` on another process's window) and off-screen positioning
//! doesn't reliably drop the button either, so `ITaskbarList::DeleteTab` /
//! `AddTab` is the only mechanism that works. The interface is apartment-model
//! COM, so a dedicated STA thread owns it and processes requests over a channel.
//!
//! Callable from anywhere via [`taskbar_hide`] / [`taskbar_show`] (a global
//! sender, like the cloak helpers). Safety: every window we `DeleteTab` is
//! tracked and restored with `AddTab` when [`TaskbarHandle`] is dropped
//! (graceful shutdown), so a window is never left missing from the taskbar.

use crate::recover_poisoned_mutex;
use leopardwm_core_layout::WindowId;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{mpsc, Mutex};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

enum TaskbarCmd {
    Hide(WindowId),
    Show(WindowId),
    Restore(WindowId),
    Forget(WindowId),
}

/// Global sender to the taskbar thread. `None` until `init_taskbar`, and again
/// after the handle is dropped. Free functions no-op when unset.
static TASKBAR_TX: Mutex<Option<mpsc::Sender<TaskbarCmd>>> = Mutex::new(None);

/// Remove `wid`'s taskbar button (best-effort; no-op if uninitialized).
pub fn taskbar_hide(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(TaskbarCmd::Hide(wid));
    }
}

/// Restore `wid`'s taskbar button (only acts if we'd hidden it).
pub fn taskbar_show(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(TaskbarCmd::Show(wid));
    }
}

/// Unconditionally re-add `wid`'s taskbar button. Used at startup to restore
/// buttons a crashed prior daemon instance may have left deleted (those aren't
/// in this process's hidden set, so `taskbar_show` wouldn't touch them).
pub fn taskbar_restore(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(TaskbarCmd::Restore(wid));
    }
}

/// Drop `wid` from the hidden set without an `AddTab` (the window is gone).
/// Keeps the set from retaining a stale id whose HWND could be recycled, which
/// would otherwise make the change-gate skip re-hiding the new window.
pub fn taskbar_forget(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(TaskbarCmd::Forget(wid));
    }
}

/// Owns the taskbar-control thread. Dropping it restores every hidden window's
/// taskbar button (via the thread's shutdown path) before returning.
pub struct TaskbarHandle {
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the COM thread and publish the global sender. Returns `None` if the
/// thread can't be spawned; taskbar hiding is then a best-effort no-op.
pub fn init_taskbar() -> Option<TaskbarHandle> {
    let (tx, rx) = mpsc::channel::<TaskbarCmd>();
    let thread = std::thread::Builder::new()
        .name("taskbar-list".into())
        .spawn(move || run(rx))
        .map_err(|e| tracing::warn!("Failed to spawn taskbar thread: {}", e))
        .ok()?;
    *TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex) = Some(tx);
    Some(TaskbarHandle {
        thread: Some(thread),
    })
}

impl Drop for TaskbarHandle {
    fn drop(&mut self) {
        // Clear the global sender so the thread's recv() ends; it then restores
        // every hidden button and exits. Join so restore completes before we
        // return (bounded so a hung shell call can't block shutdown forever).
        *TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex) = None;
        if let Some(thread) = self.thread.take() {
            for _ in 0..50 {
                if thread.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!("Taskbar thread did not exit promptly; detaching");
            }
        }
    }
}

fn hwnd_of(wid: WindowId) -> HWND {
    HWND(wid as *mut c_void)
}

fn run(rx: mpsc::Receiver<TaskbarCmd>) {
    unsafe {
        // STA: ITaskbarList is apartment-threaded.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let taskbar: ITaskbarList = match CoCreateInstance(&TaskbarList, None, CLSCTX_ALL) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("CoCreateInstance(TaskbarList) failed: {}", e);
                CoUninitialize();
                return;
            }
        };
        if let Err(e) = taskbar.HrInit() {
            tracing::warn!("ITaskbarList::HrInit failed: {}", e);
            CoUninitialize();
            return;
        }
        tracing::info!("Taskbar controller initialized");

        let mut hidden: HashSet<WindowId> = HashSet::new();
        while let Ok(cmd) = rx.recv() {
            match cmd {
                // Change-gated so frequent focus/scroll reconciles don't spam
                // the shell with redundant DeleteTab/AddTab calls.
                TaskbarCmd::Hide(wid) => {
                    if hidden.insert(wid) {
                        let _ = taskbar.DeleteTab(hwnd_of(wid));
                    }
                }
                TaskbarCmd::Show(wid) => {
                    if hidden.remove(&wid) {
                        let _ = taskbar.AddTab(hwnd_of(wid));
                    }
                }
                TaskbarCmd::Restore(wid) => {
                    hidden.remove(&wid);
                    let _ = taskbar.AddTab(hwnd_of(wid));
                }
                TaskbarCmd::Forget(wid) => {
                    hidden.remove(&wid);
                }
            }
        }

        // Channel closed (handle dropped): restore every hidden button so
        // nothing is left missing from the taskbar after shutdown.
        for wid in hidden.drain() {
            let _ = taskbar.AddTab(hwnd_of(wid));
        }
        drop(taskbar);
        CoUninitialize();
        tracing::debug!("Taskbar controller stopped (restored all tabs)");
    }
}
