//! Window style tweaks: DWM border color and WS_MAXIMIZEBOX (snap layout) management.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use crate::window_identity::{capture_window_identity, window_identity_matches, WindowIdentity};
use leopardwm_core_layout::WindowId;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use windows::core::BOOL;
use windows::Win32::Foundation::{GetLastError, SetLastError, HWND, RECT, WIN32_ERROR};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_NCRENDERING_ENABLED,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

/// Set the DWM border color for a window (Windows 11+).
///
/// Returns Ok(true) if the border was set, Ok(false) if the API is unsupported.
pub fn set_window_border_color(hwnd: WindowId, color: u32) -> Result<bool, Win32Error> {
    let window_id = hwnd;
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        const DWMWA_BORDER_COLOR: u32 = 34;
        let colorref = color;
        let result = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(DWMWA_BORDER_COLOR as i32),
            &colorref as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        match result {
            Ok(()) => Ok(true),
            Err(e) => {
                if !IsWindow(Some(hwnd)).as_bool() {
                    return Err(Win32Error::WindowNotFound(window_id));
                }

                if is_border_color_unsupported_hresult(e.code()) {
                    return Ok(false);
                }

                Err(Win32Error::SetPositionFailed(format!(
                    "DwmSetWindowAttribute(DWMWA_BORDER_COLOR) failed for {:?}: {}",
                    hwnd, e
                )))
            }
        }
    }
}

/// Reset the DWM border color for a window to the default.
///
/// Returns Ok(true) if the border was reset, Ok(false) if the API is unsupported.
pub fn reset_window_border_color(hwnd: WindowId) -> Result<bool, Win32Error> {
    // DWMWA_COLOR_DEFAULT = 0xFFFFFFFF
    set_window_border_color(hwnd, 0xFFFFFFFF)
}

fn is_border_color_unsupported_hresult(code: windows::core::HRESULT) -> bool {
    const E_INVALIDARG_HRESULT: i32 = 0x8007_0057u32 as i32;
    const E_NOTIMPL_HRESULT: i32 = 0x8000_4001u32 as i32;
    code.0 == E_INVALIDARG_HRESULT || code.0 == E_NOTIMPL_HRESULT
}

struct ManagedFrameRefresh {
    token: u64,
    pending: Option<u64>,
}

static FRAME_REFRESHES: LazyLock<Mutex<HashMap<WindowId, ManagedFrameRefresh>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_FRAME_REFRESH: AtomicU64 = AtomicU64::new(1);

fn lock_frame_refreshes() -> MutexGuard<'static, HashMap<WindowId, ManagedFrameRefresh>> {
    FRAME_REFRESHES
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(crate) fn register_frame_refresh_lifetime(window_id: WindowId, token: u64) {
    lock_frame_refreshes().insert(
        window_id,
        ManagedFrameRefresh {
            token,
            pending: None,
        },
    );
}

/// Discard pending refreshes for a departing managed lifetime without invalidating
/// the HWND's token, which queued style restores and hidden-window suppression still use.
pub fn retire_frame_refresh_lifetime(window_id: WindowId, token: u64) {
    let mut refreshes = lock_frame_refreshes();
    if refreshes
        .get(&window_id)
        .is_some_and(|entry| entry.token == token)
    {
        refreshes.remove(&window_id);
    }
}

fn mark_frame_refresh(window_id: WindowId, hwnd: HWND, identity: WindowIdentity) {
    let mut refreshes = lock_frame_refreshes();
    let Some(entry) = refreshes.get_mut(&window_id) else {
        return;
    };
    if identity.managed_lifetime_token == Some(entry.token)
        && window_identity_matches(hwnd, identity)
    {
        entry.pending = Some(NEXT_FRAME_REFRESH.fetch_add(1, Ordering::Relaxed));
    }
}

fn frame_change_offscreen(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL).0.is_null() }
}

fn begin_frame_change(
    window_id: WindowId,
    hwnd: HWND,
    identity: WindowIdentity,
) -> (bool, Option<u64>) {
    let previous = frame_refresh(window_id, hwnd);
    let offscreen = frame_change_offscreen(hwnd);
    if offscreen {
        mark_frame_refresh(window_id, hwnd, identity);
    }
    (offscreen, previous)
}

fn finish_frame_change(
    window_id: WindowId,
    hwnd: HWND,
    identity: WindowIdentity,
    before: (bool, Option<u64>),
    completed_synchronously: bool,
) {
    if frame_change_offscreen(hwnd) {
        mark_frame_refresh(window_id, hwnd, identity);
    } else if !before.0 && completed_synchronously {
        if let Some(generation) = before.1 {
            clear_frame_refresh(window_id, generation);
        }
    }
}

pub(crate) fn frame_refresh(window_id: WindowId, hwnd: HWND) -> Option<u64> {
    let mut refreshes = lock_frame_refreshes();
    let entry = refreshes.get_mut(&window_id)?;
    let generation = entry.pending?;
    if capture_window_identity(hwnd).and_then(|identity| identity.managed_lifetime_token)
        == Some(entry.token)
    {
        Some(generation)
    } else {
        entry.pending = None;
        None
    }
}

pub(crate) fn clear_frame_refresh(window_id: WindowId, generation: u64) {
    let mut refreshes = lock_frame_refreshes();
    if let Some(entry) = refreshes.get_mut(&window_id) {
        if entry.pending == Some(generation) {
            entry.pending = None;
        }
    }
}

/// Tracks window identities queued for WS_MAXIMIZEBOX removal.
/// Available for panic recovery when AppState is poisoned or unavailable.
static SNAP_DISABLED_HWNDS: Mutex<Option<HashMap<WindowId, WindowIdentity>>> = Mutex::new(None);
static PENDING_RESTORES: OnceLock<Mutex<HashMap<(WindowId, WindowIdentity), usize>>> =
    OnceLock::new();

fn lock_snap_disabled() -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, WindowIdentity>>>
{
    SNAP_DISABLED_HWNDS
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

fn lock_pending_restores(
) -> std::sync::MutexGuard<'static, HashMap<(WindowId, WindowIdentity), usize>> {
    PENDING_RESTORES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

static IN_FLIGHT_STYLE_REQUEST: Mutex<Option<(WindowId, WindowIdentity)>> = Mutex::new(None);

fn lock_in_flight_style_request(
) -> std::sync::MutexGuard<'static, Option<(WindowId, WindowIdentity)>> {
    IN_FLIGHT_STYLE_REQUEST
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex)
}

struct InFlightStyleRequestGuard {
    request: (WindowId, WindowIdentity),
    active: bool,
}

impl InFlightStyleRequestGuard {
    fn publish(window_id: WindowId, identity: WindowIdentity) -> Self {
        let request = (window_id, identity);
        *lock_in_flight_style_request() = Some(request);
        Self {
            request,
            active: true,
        }
    }

    fn retire_if_tracked(&mut self) -> bool {
        let mut published = lock_in_flight_style_request();
        let remains_tracked = lock_snap_disabled()
            .as_ref()
            .and_then(|set| set.get(&self.request.0))
            == Some(&self.request.1);
        #[cfg(test)]
        tests::pause_after_remove_tracking_decision();

        if remains_tracked && *published == Some(self.request) {
            *published = None;
            self.active = false;
            true
        } else {
            false
        }
    }

    fn retire(&mut self) {
        if self.active {
            let mut published = lock_in_flight_style_request();
            if *published == Some(self.request) {
                *published = None;
            }
            self.active = false;
        }
    }
}

impl Drop for InFlightStyleRequestGuard {
    fn drop(&mut self) {
        self.retire();
    }
}

const MAXIMIZEBOX_GEOMETRY_TARGET: &str = "leopardwm::window_style";
const MAXIMIZEBOX_GEOMETRY_EVENT: &str = "maximizebox_geometry";

static MAXIMIZEBOX_GEOMETRY_CORRELATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MaximizeboxGeometrySnapshot {
    style: Result<i32, i32>,
    ex_style: Result<i32, i32>,
    window_rect: Result<[i32; 4], i32>,
    client_rect: Result<[i32; 4], i32>,
    dwm_frame: Result<[i32; 4], i32>,
    nc_rendering: Result<bool, i32>,
}

fn capture_maximizebox_geometry(hwnd: HWND) -> MaximizeboxGeometrySnapshot {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowLongW, GetWindowRect, GWL_EXSTYLE, GWL_STYLE,
    };

    unsafe {
        let previous_error = GetLastError();
        // GetWindowLongW returns 0 for both failure and a real zero value.
        // SetLastError(0) first; a later nonzero last error is the failed read.
        let style_bits = |index| {
            SetLastError(WIN32_ERROR(0));
            let value = GetWindowLongW(hwnd, index);
            if value == 0 {
                let err = GetLastError().0;
                if err == 0 {
                    Ok(0)
                } else {
                    Err(windows::core::HRESULT::from_win32(err).0)
                }
            } else {
                Ok(value)
            }
        };
        let rect = |query: windows::core::Result<()>, rect: RECT| match query {
            Ok(()) => Ok([rect.left, rect.top, rect.right, rect.bottom]),
            Err(error) => Err(error.code().0),
        };

        let style = style_bits(GWL_STYLE);
        let ex_style = style_bits(GWL_EXSTYLE);
        let mut window_rect = RECT::default();
        let window_rect = rect(GetWindowRect(hwnd, &mut window_rect), window_rect);
        let mut client_rect = RECT::default();
        let client_rect = rect(GetClientRect(hwnd, &mut client_rect), client_rect);
        let mut dwm_frame = RECT::default();
        let dwm_frame = rect(
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut dwm_frame as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            ),
            dwm_frame,
        );
        let mut nc_enabled = BOOL::default();
        let nc_rendering = match DwmGetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_ENABLED,
            &mut nc_enabled as *mut BOOL as *mut c_void,
            std::mem::size_of::<BOOL>() as u32,
        ) {
            Ok(()) => Ok(nc_enabled.as_bool()),
            Err(error) => Err(error.code().0),
        };
        SetLastError(previous_error);
        MaximizeboxGeometrySnapshot {
            style,
            ex_style,
            window_rect,
            client_rect,
            dwm_frame,
            nc_rendering,
        }
    }
}

fn log_maximizebox_geometry(
    operation: &'static str,
    stage: &'static str,
    hwnd: HWND,
    correlation_id: u64,
    set_window_pos_hr: Option<i32>,
) {
    if !tracing::enabled!(target: MAXIMIZEBOX_GEOMETRY_TARGET, tracing::Level::DEBUG) {
        return;
    }

    let previous_error = unsafe { GetLastError() };
    let snapshot = capture_maximizebox_geometry(hwnd);
    let pid = unsafe { GetCurrentProcessId() };
    let tid = unsafe { GetCurrentThreadId() };
    tracing::debug!(
        target: MAXIMIZEBOX_GEOMETRY_TARGET,
        operation,
        stage,
        hwnd = hwnd.0 as usize as u64,
        pid,
        tid,
        correlation_id,
        style = ?snapshot.style,
        ex_style = ?snapshot.ex_style,
        window_rect = ?snapshot.window_rect,
        client_rect = ?snapshot.client_rect,
        dwm_frame = ?snapshot.dwm_frame,
        nc_rendering = ?snapshot.nc_rendering,
        set_window_pos_hr,
        event = MAXIMIZEBOX_GEOMETRY_EVENT,
    );
    unsafe { SetLastError(previous_error) };
}

#[derive(Clone, Copy)]
enum WindowStyleOperation {
    Remove,
    Restore,
}

enum WindowStyleRequest {
    Apply {
        window_id: WindowId,
        hwnd: usize,
        identity: WindowIdentity,
        operation: WindowStyleOperation,
        correlation_id: u64,
    },
    RestoreMaximized {
        window_id: WindowId,
        hwnd: usize,
        identity: WindowIdentity,
        completion_sender: Option<std::sync::Weak<Sender<crate::WindowEvent>>>,
    },
    Barrier(SyncSender<()>),
}

static WINDOW_STYLE_WORKER: OnceLock<Sender<WindowStyleRequest>> = OnceLock::new();

fn window_style_worker() -> &'static Sender<WindowStyleRequest> {
    WINDOW_STYLE_WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("leopardwm-window-style".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    match request {
                        WindowStyleRequest::Apply {
                            window_id,
                            hwnd,
                            identity,
                            operation,
                            correlation_id,
                        } => {
                            let mut in_flight =
                                InFlightStyleRequestGuard::publish(window_id, identity);
                            apply_window_style(
                                window_id,
                                hwnd,
                                identity,
                                operation,
                                correlation_id,
                                &mut in_flight,
                            );
                        }
                        WindowStyleRequest::RestoreMaximized {
                            window_id,
                            hwnd,
                            identity,
                            completion_sender,
                        } => {
                            restore_maximized_window_with(window_id, hwnd, identity, |event| {
                                if let Some(sender) =
                                    completion_sender.and_then(|sender| sender.upgrade())
                                {
                                    let _ = sender.send(event);
                                }
                            });
                        }
                        WindowStyleRequest::Barrier(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("failed to spawn window style worker");
        sender
    })
}

/// Queue a non-activating restore; `Ok(true)` means a completion route was captured.
pub fn queue_maximized_window_restore(window_id: WindowId) -> Result<bool, Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    let identity = capture_window_identity(hwnd).ok_or(Win32Error::WindowNotFound(window_id))?;
    if identity.managed_lifetime_token.is_none() {
        return Err(Win32Error::SetPositionFailed(format!(
            "Missing managed lifetime for maximized window {}",
            window_id
        )));
    }
    let completion_sender = crate::event_hooks::weak_event_sender();
    let will_report = completion_sender.is_some();
    window_style_worker()
        .send(WindowStyleRequest::RestoreMaximized {
            window_id,
            hwnd: hwnd.0 as usize,
            identity,
            completion_sender,
        })
        .expect("window style worker stopped unexpectedly");
    Ok(will_report)
}

fn restore_maximized_window_with(
    window_id: WindowId,
    hwnd_value: usize,
    identity: WindowIdentity,
    report: impl FnOnce(crate::WindowEvent),
) {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, IsZoomed, ShowWindow, ShowWindowAsync, SW_SHOWNOACTIVATE,
    };

    let hwnd = HWND(hwnd_value as *mut c_void);
    let Some(managed_lifetime_token) = identity.managed_lifetime_token else {
        return;
    };
    let is_zoomed = || unsafe { IsZoomed(hwnd).as_bool() };
    let is_admission_target =
        || window_identity_matches(hwnd, identity) && unsafe { IsWindowVisible(hwnd).as_bool() };
    if !is_admission_target() {
        report(crate::WindowEvent::MaximizedAdmissionRestored {
            window_id,
            managed_lifetime_token,
            still_maximized: unsafe { !IsWindow(Some(hwnd)).as_bool() } || is_zoomed(),
        });
        return;
    }
    let result = crate::focus::restore_maximized_window_no_activate_with(
        window_id,
        is_admission_target,
        is_zoomed,
        || unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        },
        || unsafe {
            let _ = ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
        },
        |ms| std::thread::sleep(std::time::Duration::from_millis(ms as u64)),
    );
    let still_maximized = maximized_admission_still_maximized(result, || {
        report_target_still_zoomed(&is_admission_target, &is_zoomed)
    });
    report(crate::WindowEvent::MaximizedAdmissionRestored {
        window_id,
        managed_lifetime_token,
        still_maximized,
    });
}

/// The zoom reading a restore failure reports. A target that is no longer
/// available cannot be read as a restored window: the handle may now name a
/// replacement, or none at all, so report the conservative outcome instead.
fn report_target_still_zoomed(
    is_admission_target: &impl Fn() -> bool,
    is_zoomed: &impl Fn() -> bool,
) -> bool {
    if !is_admission_target() {
        return true;
    }
    is_zoomed()
}

/// Decide the `still_maximized` value a restore outcome reports. `target_still_zoomed`
/// supplies the live zoom reading and is only consulted for a target that is
/// still confirmed present, so a stale handle can never be read as a restored
/// window.
fn maximized_admission_still_maximized(
    result: Result<(), Win32Error>,
    target_still_zoomed: impl FnOnce() -> bool,
) -> bool {
    match result {
        Ok(()) => false,
        Err(Win32Error::WindowNotFound(_)) => {
            // The window is gone or replaced, so its handle proves nothing
            // about zoom state. Report the conservative outcome: a restore
            // that cannot be confirmed leaves the window's placement
            // suppressed rather than tiling a window that may be maximized.
            tracing::debug!(
                "Maximized admission target is no longer available; reporting it unrestored"
            );
            true
        }
        Err(error) => {
            tracing::debug!(
                "Could not restore a maximized window without activation: {:?}",
                error
            );
            target_still_zoomed()
        }
    }
}

fn apply_window_style(
    window_id: WindowId,
    hwnd_value: usize,
    identity: WindowIdentity,
    operation: WindowStyleOperation,
    correlation_id: u64,
    in_flight: &mut InFlightStyleRequestGuard,
) {
    let applied =
        apply_window_style_inner(window_id, hwnd_value, identity, operation, correlation_id);
    if matches!(operation, WindowStyleOperation::Remove)
        && applied
        && !in_flight.retire_if_tracked()
    {
        apply_window_style_inner(
            window_id,
            hwnd_value,
            identity,
            WindowStyleOperation::Restore,
            correlation_id,
        );
        in_flight.retire();
    }
    if matches!(operation, WindowStyleOperation::Restore) {
        let mut pending = lock_pending_restores();
        if let Some(count) = pending.get_mut(&(window_id, identity)) {
            *count -= 1;
            if *count == 0 {
                pending.remove(&(window_id, identity));
            }
        }
    }
}

fn apply_window_style_inner(
    window_id: WindowId,
    hwnd_value: usize,
    identity: WindowIdentity,
    operation: WindowStyleOperation,
    correlation_id: u64,
) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };

    const WS_MAXIMIZEBOX: i32 = 0x0001_0000;
    let hwnd = HWND(hwnd_value as *mut c_void);
    if !window_identity_matches(hwnd, identity) {
        if matches!(operation, WindowStyleOperation::Remove) {
            let mut tracking = lock_snap_disabled();
            if let Some(set) = tracking.as_mut() {
                if set.get(&window_id) == Some(&identity) {
                    set.remove(&window_id);
                }
            }
        }
        return false;
    }
    if matches!(operation, WindowStyleOperation::Remove)
        && lock_snap_disabled()
            .as_ref()
            .and_then(|set| set.get(&window_id))
            != Some(&identity)
    {
        return false;
    }

    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let (operation_name, new_style) = match operation {
            WindowStyleOperation::Remove if style & WS_MAXIMIZEBOX != 0 => {
                ("remove", style & !WS_MAXIMIZEBOX)
            }
            WindowStyleOperation::Restore if style & WS_MAXIMIZEBOX == 0 => {
                ("restore", style | WS_MAXIMIZEBOX)
            }
            _ => return false,
        };

        log_maximizebox_geometry(operation_name, "before_style", hwnd, correlation_id, None);
        SetWindowLongW(hwnd, GWL_STYLE, new_style);
        log_maximizebox_geometry(operation_name, "after_style", hwnd, correlation_id, None);

        // Frame recalculation remains synchronous, but runs off the daemon event loop.
        let before = begin_frame_change(window_id, hwnd, identity);
        let frame_result = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        finish_frame_change(window_id, hwnd, identity, before, frame_result.is_ok());
        log_maximizebox_geometry(
            operation_name,
            "after_frame",
            hwnd,
            correlation_id,
            Some(match &frame_result {
                Ok(()) => 0,
                Err(error) => error.code().0,
            }),
        );
    }
    true
}

/// Remove `WS_MAXIMIZEBOX` from a window to disable Windows 11 Snap Layouts.
///
/// Returns `Ok(true)` if removal was queued, `Ok(false)` if already absent and
/// no restore is pending. Registers the window for panic recovery when queued.
///
/// Uses `GetWindowLongW`/`SetWindowLongW` (32-bit) intentionally: on 64-bit
/// Windows this disables the DWM snap layout flyout while preserving the
/// maximize button and its click-to-maximize behavior.
pub fn remove_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};

    let hwnd = window_id_to_hwnd(window_id)?;
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    let identity = capture_window_identity(hwnd).ok_or(Win32Error::WindowNotFound(window_id))?;
    let pending = lock_pending_restores();
    if !window_identity_matches(hwnd, identity) {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    const WS_MAXIMIZEBOX: i32 = 0x0001_0000;
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    if style & WS_MAXIMIZEBOX == 0 && !pending.contains_key(&(window_id, identity)) {
        return Ok(false);
    }

    let correlation_id = MAXIMIZEBOX_GEOMETRY_CORRELATION.fetch_add(1, Ordering::Relaxed);
    lock_snap_disabled()
        .get_or_insert_with(HashMap::new)
        .insert(window_id, identity);
    window_style_worker()
        .send(WindowStyleRequest::Apply {
            window_id,
            hwnd: hwnd.0 as usize,
            identity,
            operation: WindowStyleOperation::Remove,
            correlation_id,
        })
        .expect("window style worker stopped unexpectedly");
    drop(pending);
    Ok(true)
}

/// Restore `WS_MAXIMIZEBOX` on a window, re-enabling Windows 11 Snap Layouts.
///
/// Removes the window from the global tracking set and queues the restore.
/// Returns `Ok(true)` when the restore request was queued.
pub fn restore_maximizebox(window_id: WindowId) -> Result<bool, Win32Error> {
    // Always remove from tracking set, even if the Win32 call fails.
    let mut pending = lock_pending_restores();
    if let Some(ref mut set) = *lock_snap_disabled() {
        set.remove(&window_id);
    }

    let hwnd = window_id_to_hwnd(window_id)?;
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    let identity = capture_window_identity(hwnd).ok_or(Win32Error::WindowNotFound(window_id))?;

    let correlation_id = MAXIMIZEBOX_GEOMETRY_CORRELATION.fetch_add(1, Ordering::Relaxed);
    *pending.entry((window_id, identity)).or_default() += 1;
    window_style_worker()
        .send(WindowStyleRequest::Apply {
            window_id,
            hwnd: hwnd.0 as usize,
            identity,
            operation: WindowStyleOperation::Restore,
            correlation_id,
        })
        .expect("window style worker stopped unexpectedly");
    drop(pending);
    Ok(true)
}

/// Wait up to `timeout` for all previously queued window style requests.
/// Returns whether the worker reached the barrier; `false` does not cancel requests.
pub fn wait_for_window_style_requests(timeout: Duration) -> bool {
    let Some(worker) = WINDOW_STYLE_WORKER.get() else {
        return true;
    };
    let (done, completed) = mpsc::sync_channel(0);
    if worker.send(WindowStyleRequest::Barrier(done)).is_err() {
        return false;
    }
    completed.recv_timeout(timeout).is_ok()
}

/// Best-effort bulk restore of `WS_MAXIMIZEBOX` for multiple windows.
/// Never panics — logs failures and continues.
pub fn restore_maximizebox_all(window_ids: &[WindowId]) {
    for &wid in window_ids {
        match restore_maximizebox(wid) {
            Ok(_) => {}
            Err(Win32Error::WindowNotFound(_)) => {
                // Window already destroyed — tracking set already cleaned up
            }
            Err(e) => {
                tracing::warn!("Failed to restore WS_MAXIMIZEBOX for window {}: {}", wid, e);
            }
        }
    }
}

fn recover_maximizebox(
    window_id: WindowId,
    hwnd: HWND,
    identity: WindowIdentity,
    deadline: Option<std::time::Instant>,
    async_unmarked: bool,
) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SET_WINDOW_POS_FLAGS,
        SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        const MAXIMIZEBOX: i32 = 0x0001_0000;
        if style & MAXIMIZEBOX != 0
            || !crate::placement::can_recover_placement_owner(window_id, hwnd, deadline)
        {
            return;
        }
        SetWindowLongW(hwnd, GWL_STYLE, style | MAXIMIZEBOX);
        let responsive = crate::placement::can_recover_placement_owner(window_id, hwnd, deadline);
        // Queue the frame refresh if the owner stopped answering after the style changed.
        let dispatch = if !responsive
            || (async_unmarked && !crate::placement::is_marked_placement_owner(window_id))
        {
            SWP_ASYNCWINDOWPOS
        } else {
            SET_WINDOW_POS_FLAGS(0)
        };
        let before = begin_frame_change(window_id, hwnd, identity);
        let result = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | dispatch | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        finish_frame_change(
            window_id,
            hwnd,
            identity,
            before,
            result.is_ok() && !dispatch.contains(SWP_ASYNCWINDOWPOS),
        );
    }
}

// SetWindowLongW can block on a hung owner; call only inside the exit controller's budget.
pub(crate) fn emergency_restore_maximizebox(window_ids: &[WindowId], deadline: std::time::Instant) {
    let entries = {
        let Ok(in_flight) = IN_FLIGHT_STYLE_REQUEST.try_lock() else {
            return;
        };
        let pending = PENDING_RESTORES.get().map(|pending| pending.try_lock());
        let pending = match pending {
            Some(Ok(guard)) => Some(guard),
            Some(Err(_)) => return,
            None => None,
        };
        let Ok(tracking) = SNAP_DISABLED_HWNDS.try_lock() else {
            return;
        };
        let mut entries: Vec<_> = tracking
            .as_ref()
            .map(|set| set.iter().map(|(&id, &identity)| (id, identity)).collect())
            .unwrap_or_default();
        if let Some(pending) = pending {
            for key in pending.keys() {
                if !entries.contains(key) {
                    entries.push(*key);
                }
            }
        }
        entries.retain(|entry| Some(*entry) != *in_flight && window_ids.contains(&entry.0));
        entries
    };
    let mut entries = entries;
    entries.sort_by_key(|&(id, _)| crate::placement::is_marked_placement_owner(id));
    for (id, identity) in entries {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let Ok(hwnd) = window_id_to_hwnd(id) else {
            continue;
        };
        if !window_identity_matches(hwnd, identity) {
            continue;
        }
        recover_maximizebox(id, hwnd, identity, Some(deadline), true);
    }
}

/// Emergency restore of `WS_MAXIMIZEBOX` for tracked and pending-restore windows.
/// Drains tracking and restores styles best-effort, leaving the in-flight request to its worker.
/// Safe to call from panic hooks (no AppState needed).
pub fn restore_maximizebox_panic_recovery() {
    let (window_ids, in_flight): (Vec<(WindowId, WindowIdentity)>, _) = {
        let in_flight = lock_in_flight_style_request();
        let pending = lock_pending_restores();
        let mut tracking = lock_snap_disabled();
        let mut window_ids: Vec<_> = tracking
            .as_mut()
            .map(|set| set.drain().collect())
            .unwrap_or_default();
        for key in pending.keys().copied() {
            if !window_ids.contains(&key) {
                window_ids.push(key);
            }
        }
        (window_ids, *in_flight)
    };
    let mut window_ids: Vec<_> = window_ids
        .into_iter()
        .filter(|entry| Some(*entry) != in_flight)
        .collect();
    window_ids.sort_by_key(|&(id, _)| crate::placement::is_marked_placement_owner(id));

    if window_ids.is_empty() {
        return;
    }

    eprintln!(
        "[leopardwm] Restoring WS_MAXIMIZEBOX for {} window(s) in panic recovery",
        window_ids.len()
    );

    let mut recovered_unmarked = false;
    for &(wid, identity) in &window_ids {
        if !recovered_unmarked && crate::placement::is_marked_placement_owner(wid) {
            crate::visibility::uncloak_unmarked_windows_for_recovery();
            recovered_unmarked = true;
        }
        // Direct Win32 call — don't use restore_maximizebox since tracking set is already drained
        let Ok(hwnd) = window_id_to_hwnd(wid) else {
            continue;
        };
        if !window_identity_matches(hwnd, identity) {
            continue;
        }
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            continue;
        }
        recover_maximizebox(wid, hwnd, identity, None, false);
    }

    eprintln!(
        "[leopardwm] WS_MAXIMIZEBOX panic recovery complete ({} windows processed)",
        window_ids.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_target_is_reported_as_not_restored() {
        let reads = std::cell::Cell::new(0);
        let still_maximized =
            maximized_admission_still_maximized(Err(Win32Error::WindowNotFound(42)), || {
                reads.set(reads.get() + 1);
                false
            });

        assert!(
            still_maximized,
            "an unconfirmable restore must not look restored"
        );
        assert_eq!(reads.get(), 0, "the stale handle must not be inspected");
    }

    #[test]
    fn timed_out_restore_reports_a_target_that_vanished_before_reporting() {
        // The restore timed out, then the window was destroyed, replaced or
        // hidden before the completion event was built.
        let available = std::cell::Cell::new(false);
        let zoom_reads = std::cell::Cell::new(0);
        let is_admission_target = || available.get();
        let is_zoomed = || {
            zoom_reads.set(zoom_reads.get() + 1);
            false
        };

        let still_maximized = maximized_admission_still_maximized(
            Err(Win32Error::SetPositionFailed("timed out".to_string())),
            || report_target_still_zoomed(&is_admission_target, &is_zoomed),
        );

        assert!(still_maximized, "a vanished target must not look restored");
        assert_eq!(zoom_reads.get(), 0, "the stale handle must not be read");
    }

    #[test]
    fn failed_restore_reports_a_live_target_that_is_still_zoomed() {
        let result = Err(Win32Error::SetPositionFailed("ignored".to_string()));
        assert!(maximized_admission_still_maximized(result, || true));
        assert!(!maximized_admission_still_maximized(
            Err(Win32Error::SetPositionFailed("ignored".to_string())),
            || false
        ));
    }

    #[test]
    fn successful_restore_reports_the_window_as_not_maximized() {
        let reads = std::cell::Cell::new(0);
        assert!(!maximized_admission_still_maximized(Ok(()), || {
            reads.set(reads.get() + 1);
            true
        }));
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn test_is_border_color_unsupported_hresult_mapping() {
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8007_0057u32 as i32
        )));
        assert!(is_border_color_unsupported_hresult(windows::core::HRESULT(
            0x8000_4001u32 as i32
        )));
        assert!(!is_border_color_unsupported_hresult(
            windows::core::HRESULT(0x8000_4005u32 as i32)
        ));
    }

    #[test]
    fn test_set_window_border_color_zero_fails() {
        let result = set_window_border_color(0, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_window_border_color_invalid_hwnd_fails() {
        let result = set_window_border_color(u64::MAX, 0x4285F4);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_reset_window_border_color_zero_fails() {
        let result = reset_window_border_color(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_remove_maximizebox_zero_fails() {
        let result = remove_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_remove_maximizebox_invalid_hwnd_fails() {
        let result = remove_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_restore_maximizebox_zero_fails() {
        let result = restore_maximizebox(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_restore_maximizebox_invalid_hwnd_fails() {
        let result = restore_maximizebox(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    const WS_MAXIMIZEBOX_BIT: i32 = 0x0001_0000;

    static SNAP_TRACKING_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

    fn lock_snap_tracking_fixture() -> std::sync::MutexGuard<'static, ()> {
        SNAP_TRACKING_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct HiddenFramedFixture {
        hwnd: Option<HWND>,
    }

    impl HiddenFramedFixture {
        fn create() -> Self {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
            };

            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    None,
                    WS_OVERLAPPEDWINDOW,
                    48,
                    48,
                    416,
                    312,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("failed to create hidden framed STATIC fixture");
            Self { hwnd: Some(hwnd) }
        }

        fn hwnd(&self) -> HWND {
            self.hwnd.expect("fixture already destroyed")
        }

        fn window_id(&self) -> WindowId {
            self.hwnd().0 as usize as u64
        }
    }

    impl Drop for HiddenFramedFixture {
        fn drop(&mut self) {
            if let Some(hwnd) = self.hwnd.take() {
                let _ = restore_maximizebox(hwnd.0 as usize as u64);
                pump_until_window_style_idle();
                let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            }
        }
    }

    #[test]
    fn departing_window_discards_pending_frame_refresh() {
        let _guard = lock_snap_tracking_fixture();
        let fixture = HiddenFramedFixture::create();
        let id = fixture.window_id();
        let hwnd = fixture.hwnd();
        let token = crate::stamp_managed_lifetime_token(id).unwrap();
        let identity = capture_window_identity(hwnd).unwrap();
        mark_frame_refresh(id, hwnd, identity);
        assert!(frame_refresh(id, hwnd).is_some());

        retire_frame_refresh_lifetime(id, token);

        assert_eq!(frame_refresh(id, hwnd), None);
    }

    #[test]
    fn late_frame_refresh_is_rejected_after_departure() {
        let _guard = lock_snap_tracking_fixture();
        let fixture = HiddenFramedFixture::create();
        let id = fixture.window_id();
        let hwnd = fixture.hwnd();
        let token = crate::stamp_managed_lifetime_token(id).unwrap();
        let identity = capture_window_identity(hwnd).unwrap();

        retire_frame_refresh_lifetime(id, token);
        assert!(window_identity_matches(hwnd, identity));
        mark_frame_refresh(id, hwnd, identity);
        assert_eq!(frame_refresh(id, hwnd), None);

        let new_token = crate::stamp_managed_lifetime_token(id).unwrap();
        retire_frame_refresh_lifetime(id, token);
        mark_frame_refresh(id, hwnd, capture_window_identity(hwnd).unwrap());
        assert!(frame_refresh(id, hwnd).is_some());
        retire_frame_refresh_lifetime(id, new_token);
    }

    #[derive(Default)]
    struct RecordedGeometryEvent {
        hwnd: Option<u64>,
        event: Option<String>,
        operation: Option<String>,
        stage: Option<String>,
        correlation_id: Option<u64>,
    }

    struct GeometryEventVisitor<'a>(&'a mut RecordedGeometryEvent);

    impl tracing::field::Visit for GeometryEventVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "operation" if self.0.operation.is_none() => {
                    self.0.operation = Some(format!("{value:?}"));
                }
                "stage" if self.0.stage.is_none() => {
                    self.0.stage = Some(format!("{value:?}"));
                }
                _ => {}
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "event" => self.0.event = Some(value.to_string()),
                "operation" => self.0.operation = Some(value.to_string()),
                "stage" => self.0.stage = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            match field.name() {
                "correlation_id" => self.0.correlation_id = Some(value),
                "hwnd" => self.0.hwnd = Some(value),
                _ => {}
            }
        }
    }

    struct GeometryEventSubscriber {
        events: std::sync::Arc<Mutex<Vec<RecordedGeometryEvent>>>,
    }

    impl tracing::Subscriber for GeometryEventSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == MAXIMIZEBOX_GEOMETRY_TARGET
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::DEBUG)
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut recorded = RecordedGeometryEvent::default();
            event.record(&mut GeometryEventVisitor(&mut recorded));
            self.events.lock().unwrap().push(recorded);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn pump_until_window_style_idle() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if wait_for_window_style_requests(Duration::from_millis(10)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "style worker did not become idle"
            );
        }
    }

    struct RemoveDecisionPause {
        reached_tx: mpsc::Sender<()>,
        release_tx: mpsc::Sender<()>,
        release_rx: Mutex<mpsc::Receiver<()>>,
    }

    static REMOVE_DECISION_PAUSE: Mutex<Option<std::sync::Arc<RemoveDecisionPause>>> =
        Mutex::new(None);

    pub(super) fn pause_after_remove_tracking_decision() {
        let pause = REMOVE_DECISION_PAUSE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(pause) = pause {
            let _ = pause.reached_tx.send(());
            let _ = pause
                .release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
    }

    struct RemoveDecisionPauseGuard(std::sync::Arc<RemoveDecisionPause>);

    impl RemoveDecisionPauseGuard {
        fn install() -> (Self, mpsc::Receiver<()>) {
            let (reached_tx, reached_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let pause = std::sync::Arc::new(RemoveDecisionPause {
                reached_tx,
                release_tx,
                release_rx: Mutex::new(release_rx),
            });
            *REMOVE_DECISION_PAUSE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pause.clone());
            (Self(pause), reached_rx)
        }

        fn release(&self) {
            let _ = self.0.release_tx.send(());
        }
    }

    impl Drop for RemoveDecisionPauseGuard {
        fn drop(&mut self) {
            self.release();
            let mut pause = REMOVE_DECISION_PAUSE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pause
                .as_ref()
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &self.0))
            {
                *pause = None;
            }
        }
    }

    struct SinglePendingRestoreOwner(PendingRestoreOwner);

    impl Drop for SinglePendingRestoreOwner {
        fn drop(&mut self) {
            self.0.release();
            let _ = wait_for_window_style_requests(Duration::from_secs(2));
            self.0.stop();
            self.0.join_bounded(Duration::from_secs(2));
        }
    }

    #[test]
    fn test_remove_retirement_and_recovery_are_atomic() {
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};

        let _guard = lock_snap_tracking_fixture();
        let owner = PendingRestoreOwner::spawn("LeopardWMRemoveRetirementRace", false);
        let _owner_cleanup = SinglePendingRestoreOwner(owner);
        let window_id = _owner_cleanup.0.hwnd.0 as usize as u64;
        assert!(remove_maximizebox(window_id).unwrap());
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));
        assert!(restore_maximizebox(window_id).unwrap());
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));

        let (pause_guard, reached_rx) = RemoveDecisionPauseGuard::install();
        assert!(remove_maximizebox(window_id).unwrap());
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker did not reach the post-remove tracking decision");

        let (recovery_done_tx, recovery_done_rx) = mpsc::channel();
        let recovery_thread = std::thread::spawn(move || {
            restore_maximizebox_panic_recovery();
            let _ = recovery_done_tx.send(());
        });
        let mut recovery = BoundedRecoveryCall {
            in_flight: _owner_cleanup.0.state.clone(),
            done_rx: recovery_done_rx,
            thread: Some(recovery_thread),
        };
        let recovery_finished_before_release = recovery.wait(Duration::from_millis(100));
        pause_guard.release();
        if !recovery_finished_before_release {
            assert!(
                recovery.wait(Duration::from_secs(2)),
                "recovery remained blocked after the worker retired its request"
            );
        }
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));
        assert_eq!(
            unsafe { GetWindowLongW(_owner_cleanup.0.hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            WS_MAXIMIZEBOX_BIT,
            "recovery and worker retirement must not jointly skip the restored window"
        );
    }

    struct PendingRestoreControl {
        block_next: std::sync::atomic::AtomicBool,
        entered_tx: mpsc::Sender<()>,
        entered_rx: Mutex<Option<mpsc::Receiver<()>>>,
        release_tx: mpsc::Sender<()>,
        release_rx: Mutex<mpsc::Receiver<()>>,
    }

    unsafe extern "system" fn pending_restore_owner_proc(
        hwnd: HWND,
        message: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, GetWindowLongPtrW, SetWindowLongPtrW, CREATESTRUCTW, GWLP_USERDATA,
            WM_NCCREATE, WM_STYLECHANGING,
        };

        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        } else if message == WM_STYLECHANGING {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PendingRestoreControl;
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                if state
                    .block_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = state.entered_tx.send(());
                    let _ = state
                        .release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    struct PendingRestoreOwner {
        hwnd: HWND,
        thread_id: u32,
        state: std::sync::Arc<PendingRestoreControl>,
        done_rx: mpsc::Receiver<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl PendingRestoreOwner {
        fn spawn(class_name: &'static str, block_next: bool) -> Self {
            use windows::Win32::System::Threading::GetCurrentThreadId;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, RegisterClassW,
                WNDCLASSW, WS_OVERLAPPEDWINDOW,
            };

            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let state = std::sync::Arc::new(PendingRestoreControl {
                block_next: std::sync::atomic::AtomicBool::new(block_next),
                entered_tx,
                entered_rx: Mutex::new(Some(entered_rx)),
                release_tx,
                release_rx: Mutex::new(release_rx),
            });
            let owner_state = state.clone();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (done_tx, done_rx) = mpsc::channel();
            let class_name = class_name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let thread = std::thread::spawn(move || unsafe {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(pending_restore_owner_proc),
                    lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&class), 0);
                let hwnd = CreateWindowExW(
                    Default::default(),
                    windows::core::PCWSTR(class_name.as_ptr()),
                    None,
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    320,
                    240,
                    None,
                    None,
                    None,
                    Some(std::sync::Arc::as_ptr(&owner_state) as *const c_void),
                )
                .expect("failed to create pending-restore owner window");
                ready_tx
                    .send((hwnd.0 as usize, GetCurrentThreadId()))
                    .expect("pending-restore test stopped receiving its window");
                let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    DispatchMessageW(&message);
                }
                DestroyWindow(hwnd).expect("failed to destroy pending-restore owner window");
                let _ = done_tx.send(());
            });
            let (hwnd, thread_id) = ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("pending-restore owner did not start");
            Self {
                hwnd: HWND(hwnd as *mut c_void),
                thread_id,
                state,
                done_rx,
                thread: Some(thread),
            }
        }

        fn release(&self) {
            let _ = self.state.release_tx.send(());
        }

        fn stop(&self) {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }

        fn join_bounded(&mut self, timeout: Duration) {
            if self.done_rx.recv_timeout(timeout).is_ok() {
                if let Some(thread) = self.thread.take() {
                    thread.join().expect("pending-restore owner panicked");
                }
            } else {
                self.thread.take();
            }
        }
    }

    struct BoundedRecoveryCall {
        in_flight: std::sync::Arc<PendingRestoreControl>,
        done_rx: mpsc::Receiver<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl BoundedRecoveryCall {
        fn wait(&mut self, timeout: Duration) -> bool {
            if self.done_rx.recv_timeout(timeout).is_err() {
                return false;
            }
            if let Some(thread) = self.thread.take() {
                thread.join().expect("recovery call panicked");
            }
            true
        }
    }

    impl Drop for BoundedRecoveryCall {
        fn drop(&mut self) {
            if self.thread.is_none() {
                return;
            }
            let _ = self.in_flight.release_tx.send(());
            let _ = wait_for_window_style_requests(Duration::from_secs(2));
            if self.done_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            } else {
                self.thread.take();
            }
        }
    }

    struct PendingRestoreRaceOwners(PendingRestoreOwner, PendingRestoreOwner);

    impl Drop for PendingRestoreRaceOwners {
        fn drop(&mut self) {
            self.0.release();
            self.1.release();
            let _ = wait_for_window_style_requests(Duration::from_secs(2));
            self.0.stop();
            self.1.stop();
            self.0.join_bounded(Duration::from_secs(2));
            self.1.join_bounded(Duration::from_secs(2));
        }
    }

    #[test]
    fn test_panic_recovery_restores_pending_windows_except_in_flight() {
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};

        let _guard = lock_snap_tracking_fixture();
        let a = PendingRestoreOwner::spawn("LeopardWMPendingRestoreA", false);
        let b = PendingRestoreOwner::spawn("LeopardWMPendingRestoreB", false);
        let _owners = PendingRestoreRaceOwners(a, b);
        let a_id = _owners.0.hwnd.0 as usize as u64;
        let b_id = _owners.1.hwnd.0 as usize as u64;

        assert!(remove_maximizebox(a_id).unwrap());
        assert!(remove_maximizebox(b_id).unwrap());
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));
        assert_eq!(
            unsafe { GetWindowLongW(_owners.0.hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            0
        );
        assert_eq!(
            unsafe { GetWindowLongW(_owners.1.hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            0
        );

        _owners
            .0
            .state
            .block_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(restore_maximizebox(a_id).unwrap());
        assert!(restore_maximizebox(b_id).unwrap());
        _owners
            .0
            .state
            .entered_rx
            .lock()
            .unwrap()
            .take()
            .expect("owner entry receiver available")
            .recv_timeout(Duration::from_secs(5))
            .expect("restore(A) did not block in WM_STYLECHANGING");

        let (recovery_done_tx, recovery_done_rx) = mpsc::channel();
        let recovery_thread = std::thread::spawn(move || {
            restore_maximizebox_panic_recovery();
            let _ = recovery_done_tx.send(());
        });
        let mut recovery = BoundedRecoveryCall {
            in_flight: _owners.0.state.clone(),
            done_rx: recovery_done_rx,
            thread: Some(recovery_thread),
        };
        assert!(
            recovery.wait(Duration::from_secs(2)),
            "panic recovery must return within its bound"
        );
        assert_eq!(
            unsafe { GetWindowLongW(_owners.1.hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            WS_MAXIMIZEBOX_BIT,
            "recovery must restore responsive B while skipping in-flight A"
        );

        _owners.0.release();
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));
        assert_eq!(
            unsafe { GetWindowLongW(_owners.0.hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            WS_MAXIMIZEBOX_BIT,
            "the worker must complete A's queued restore after it is released"
        );
    }

    struct StyleChangeBlock {
        entered_tx: mpsc::SyncSender<()>,
        entered_rx: Mutex<Option<mpsc::Receiver<()>>>,
        release_tx: mpsc::Sender<()>,
        release_rx: Mutex<mpsc::Receiver<()>>,
        blocked: std::sync::atomic::AtomicBool,
    }

    static STYLE_CHANGE_BLOCK: OnceLock<StyleChangeBlock> = OnceLock::new();
    static STYLE_CHANGE_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "system" fn style_change_count_proc(
        hwnd: HWND,
        message: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_STYLECHANGING};

        if message == WM_STYLECHANGING {
            STYLE_CHANGE_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    unsafe extern "system" fn style_change_block_proc(
        hwnd: HWND,
        message: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_STYLECHANGING};

        if message == WM_STYLECHANGING {
            let block = STYLE_CHANGE_BLOCK.get().expect("style block initialized");
            if !block
                .blocked
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let _ = block.entered_tx.send(());
                let _ = block
                    .release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(8));
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    #[test]
    fn test_remove_compensates_when_recovery_drains_tracking_in_flight() {
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, PostThreadMessageW,
            RegisterClassW, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };

        let _guard = lock_snap_tracking_fixture();
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::channel();
        let block = STYLE_CHANGE_BLOCK.get_or_init(|| StyleChangeBlock {
            entered_tx,
            entered_rx: Mutex::new(Some(entered_rx)),
            release_tx,
            release_rx: Mutex::new(release_rx),
            blocked: std::sync::atomic::AtomicBool::new(false),
        });
        assert!(!block.blocked.load(std::sync::atomic::Ordering::SeqCst));

        let (window_tx, window_rx) = mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(style_change_block_proc),
                lpszClassName: w!("LeopardWMSnapStyleRaceFixture"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("LeopardWMSnapStyleRaceFixture"),
                None,
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                None,
                None,
            )
            .expect("failed to create style-race fixture");
            window_tx
                .send((hwnd.0 as usize, GetCurrentThreadId()))
                .expect("style-race test stopped receiving fixture");
            let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).expect("failed to destroy style-race fixture");
        });
        let (hwnd_value, owner_thread_id) = window_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("style-race fixture did not start");
        struct OwnerCleanup {
            thread_id: u32,
            owner: Option<std::thread::JoinHandle<()>>,
            release: mpsc::Sender<()>,
        }
        impl Drop for OwnerCleanup {
            fn drop(&mut self) {
                let _ = self.release.send(());
                unsafe {
                    let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                if let Some(owner) = self.owner.take() {
                    owner.join().expect("style-race fixture owner panicked");
                }
            }
        }
        let _owner_cleanup = OwnerCleanup {
            thread_id: owner_thread_id,
            owner: Some(owner),
            release: block.release_tx.clone(),
        };
        let window_id = hwnd_value as u64;
        let hwnd = HWND(hwnd_value as *mut c_void);
        assert!(remove_maximizebox(window_id).unwrap());
        block
            .entered_rx
            .lock()
            .unwrap()
            .take()
            .expect("style-change entry receiver available")
            .recv_timeout(Duration::from_secs(5))
            .expect("remove did not block in WM_STYLECHANGING");

        assert_ne!(
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                ) & WS_MAXIMIZEBOX_BIT
            },
            0
        );
        restore_maximizebox_panic_recovery();
        block
            .release_tx
            .send(())
            .expect("release blocked style change");
        assert!(wait_for_window_style_requests(Duration::from_secs(5)));
        assert_eq!(
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                ) & WS_MAXIMIZEBOX_BIT
            },
            WS_MAXIMIZEBOX_BIT,
            "in-flight remove must compensate after recovery drains tracking"
        );
    }

    fn block_style_worker(class_name: &'static str) -> SinglePendingRestoreOwner {
        let owner = SinglePendingRestoreOwner(PendingRestoreOwner::spawn(class_name, true));
        assert!(remove_maximizebox(owner.0.hwnd.0 as usize as u64).unwrap());
        owner
            .0
            .state
            .entered_rx
            .lock()
            .unwrap()
            .take()
            .expect("owner entry receiver available")
            .recv_timeout(Duration::from_secs(5))
            .expect("style worker did not block in WM_STYLECHANGING");
        owner
    }

    struct CompletionSenderFixture {
        previous: Option<Sender<crate::WindowEvent>>,
    }

    impl CompletionSenderFixture {
        fn install(sender: Sender<crate::WindowEvent>) -> Self {
            let previous = crate::event_hooks::clone_event_sender();
            crate::event_hooks::clear_event_sender();
            crate::event_hooks::set_event_sender(sender).unwrap();
            Self { previous }
        }
    }

    impl Drop for CompletionSenderFixture {
        fn drop(&mut self) {
            crate::event_hooks::clear_event_sender();
            if let Some(previous) = self.previous.take() {
                crate::event_hooks::set_event_sender(previous).unwrap();
            }
        }
    }

    #[test]
    fn test_queued_maximized_restore_keeps_subsequently_hidden_window_hidden() {
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindowVisible, IsZoomed, ShowWindow, SW_HIDE, SW_SHOWMAXIMIZED,
        };

        let _sender_lock = crate::event_hooks::GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _guard = lock_snap_tracking_fixture();
        let (sender, events) = mpsc::channel();
        let _sender_fixture = CompletionSenderFixture::install(sender);
        let fixture = HiddenFramedFixture::create();
        let hwnd = fixture.hwnd();
        let window_id = fixture.window_id();
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        }
        assert!(unsafe { IsWindowVisible(hwnd).as_bool() && IsZoomed(hwnd).as_bool() });
        let token = crate::window_identity::stamp_managed_lifetime_token(window_id).unwrap();
        let blocker = block_style_worker("LeopardWMMaximizedWindowHiddenBeforeRestore");
        queue_maximized_window_restore(window_id).unwrap();
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        assert!(unsafe { !IsWindowVisible(hwnd).as_bool() && IsZoomed(hwnd).as_bool() });
        blocker.0.release();
        pump_until_window_style_idle();
        let completion = events.recv_timeout(Duration::from_millis(200));
        let re_shown = unsafe { IsWindowVisible(hwnd).as_bool() };
        if re_shown {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        assert!(
            !re_shown,
            "queued restore must not re-show a window hidden by its app"
        );
        assert!(
            matches!(completion, Ok(crate::WindowEvent::MaximizedAdmissionRestored {
            window_id: reported_id,
            managed_lifetime_token: reported_token,
            still_maximized: true,
        }) if reported_id == window_id && reported_token == token)
        );
    }

    #[test]
    fn test_queued_maximized_restore_does_not_hold_channel_open_after_shutdown() {
        let _sender_lock = crate::event_hooks::GLOBAL_SENDER_TEST_LOCK
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let _guard = lock_snap_tracking_fixture();
        let (sender, events) = mpsc::channel();
        let _sender_fixture = CompletionSenderFixture::install(sender);
        let fixture = HiddenFramedFixture::create();
        let window_id = fixture.window_id();
        crate::window_identity::stamp_managed_lifetime_token(window_id).unwrap();
        let blocker = block_style_worker("LeopardWMCompletionSenderCleared");
        queue_maximized_window_restore(window_id).unwrap();
        crate::event_hooks::clear_event_sender();
        let completion = events.recv_timeout(Duration::from_secs(1));
        blocker.0.release();
        pump_until_window_style_idle();
        assert_eq!(
            completion.unwrap_err(),
            mpsc::RecvTimeoutError::Disconnected
        );
    }

    #[test]
    fn test_queued_maximized_restore_rejects_stale_identity_without_style_tracking() {
        use crate::window_identity::stamp_managed_lifetime_token;
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, IsZoomed, ShowWindow, SW_SHOWMAXIMIZED,
        };

        let _guard = lock_snap_tracking_fixture();
        for (class_name, destroyed) in [
            ("LeopardWMMaximizedTokenChanged", false),
            ("LeopardWMMaximizedOwnerGone", true),
        ] {
            let mut fixture = HiddenFramedFixture::create();
            let hwnd = fixture.hwnd();
            let window_id = fixture.window_id();
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            }
            assert!(unsafe { IsZoomed(hwnd).as_bool() });
            let token = stamp_managed_lifetime_token(window_id).unwrap();
            let identity = capture_window_identity(hwnd).unwrap();
            let blocker = block_style_worker(class_name);
            let in_flight = *lock_in_flight_style_request();
            queue_maximized_window_restore(window_id).unwrap();
            assert!(!lock_pending_restores()
                .keys()
                .any(|(id, _)| *id == window_id));
            assert!(!lock_snap_disabled()
                .as_ref()
                .unwrap()
                .contains_key(&window_id));
            assert_eq!(*lock_in_flight_style_request(), in_flight);
            restore_maximizebox_panic_recovery();
            if destroyed {
                unsafe {
                    DestroyWindow(fixture.hwnd.take().unwrap()).unwrap();
                }
            } else {
                assert_ne!(stamp_managed_lifetime_token(window_id).unwrap(), token);
            }
            let report = std::cell::RefCell::new(None);
            restore_maximized_window_with(window_id, hwnd.0 as usize, identity, |event| {
                *report.borrow_mut() = Some(event);
            });
            assert!(
                matches!(*report.borrow(), Some(crate::WindowEvent::MaximizedAdmissionRestored {
                window_id: reported_id,
                managed_lifetime_token: reported_token,
                still_maximized: true,
            }) if reported_id == window_id && reported_token == token)
            );
            blocker.0.release();
            pump_until_window_style_idle();
            if !destroyed {
                assert!(
                    unsafe { IsZoomed(hwnd).as_bool() },
                    "a queued restore must not restore a replacement lifetime"
                );
            }
            assert!(!lock_pending_restores()
                .keys()
                .any(|(id, _)| *id == window_id));
            assert!(lock_in_flight_style_request().is_none());
        }
    }

    #[test]
    fn test_queued_style_changes_reject_changed_managed_token() {
        use crate::window_identity::stamp_managed_lifetime_token;
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, RemovePropW, SetWindowLongW, GWL_STYLE,
        };

        let _guard = lock_snap_tracking_fixture();
        for (class_name, restore, missing) in [
            ("LeopardWMTokenRemoveChanged", false, false),
            ("LeopardWMTokenRemoveMissing", false, true),
            ("LeopardWMTokenRestoreChanged", true, false),
            ("LeopardWMTokenRestoreMissing", true, true),
        ] {
            let fixture = HiddenFramedFixture::create();
            let hwnd = fixture.hwnd();
            let window_id = fixture.window_id();
            let token = stamp_managed_lifetime_token(window_id).unwrap();
            let identity = capture_window_identity(hwnd).expect("live fixture identity");
            if restore {
                unsafe {
                    let style = GetWindowLongW(hwnd, GWL_STYLE);
                    SetWindowLongW(hwnd, GWL_STYLE, style & !WS_MAXIMIZEBOX_BIT);
                }
            }
            let blocker = block_style_worker(class_name);
            if restore {
                assert!(restore_maximizebox(window_id).unwrap());
            } else {
                assert!(remove_maximizebox(window_id).unwrap());
            }
            if missing {
                unsafe { RemovePropW(hwnd, w!("LeopardWMManagedToken")) }.unwrap();
            } else {
                assert_ne!(stamp_managed_lifetime_token(window_id).unwrap(), token);
            }
            blocker.0.release();
            pump_until_window_style_idle();
            assert_eq!(
                unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
                if restore { 0 } else { WS_MAXIMIZEBOX_BIT },
                "a queued style change must not touch a replaced managed lifetime: {class_name}"
            );
            assert_ne!(
                lock_snap_disabled()
                    .as_ref()
                    .and_then(|set| set.get(&window_id)),
                Some(&identity),
                "stale remove tracking must be retired"
            );
            assert!(
                !lock_pending_restores().contains_key(&(window_id, identity)),
                "skipped restores must retire their pending count"
            );
        }
    }

    #[test]
    fn test_queued_style_changes_without_managed_token_use_pid_tid_fallback() {
        use crate::window_identity::{read_managed_lifetime_token, stamp_managed_lifetime_token};
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW, GWL_STYLE};

        let _guard = lock_snap_tracking_fixture();
        for (class_name, restore, stamp_later) in [
            ("LeopardWMNoTokenRemove", false, false),
            ("LeopardWMNoTokenRestore", true, false),
            ("LeopardWMLateTokenRemove", false, true),
            ("LeopardWMLateTokenRestore", true, true),
        ] {
            let fixture = HiddenFramedFixture::create();
            let hwnd = fixture.hwnd();
            let window_id = fixture.window_id();
            assert_eq!(read_managed_lifetime_token(window_id).unwrap(), None);
            if restore {
                unsafe {
                    let style = GetWindowLongW(hwnd, GWL_STYLE);
                    SetWindowLongW(hwnd, GWL_STYLE, style & !WS_MAXIMIZEBOX_BIT);
                }
            }
            let blocker = block_style_worker(class_name);
            if restore {
                assert!(restore_maximizebox(window_id).unwrap());
            } else {
                assert!(remove_maximizebox(window_id).unwrap());
            }
            if stamp_later {
                stamp_managed_lifetime_token(window_id).unwrap();
            }
            blocker.0.release();
            pump_until_window_style_idle();
            assert_eq!(
                unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
                if restore { WS_MAXIMIZEBOX_BIT } else { 0 },
                "an unstamped queued request must use PID/TID fallback: {class_name}"
            );
        }
    }

    #[test]
    fn test_queued_style_request_rejects_changed_window_identity() {
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, PostThreadMessageW,
            RegisterClassW, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };

        let _guard = lock_snap_tracking_fixture();
        STYLE_CHANGE_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        let (window_tx, window_rx) = mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(style_change_count_proc),
                lpszClassName: w!("LeopardWMSnapIdentityFixture"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("LeopardWMSnapIdentityFixture"),
                None,
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                None,
                None,
            )
            .expect("failed to create identity fixture");
            window_tx
                .send((hwnd.0 as usize, GetCurrentThreadId()))
                .expect("identity test stopped receiving fixture");
            let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).expect("failed to destroy identity fixture");
        });
        let (hwnd_value, owner_thread_id) = window_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("identity fixture did not start");
        struct OwnerCleanup {
            thread_id: u32,
            owner: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for OwnerCleanup {
            fn drop(&mut self) {
                unsafe {
                    let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                if let Some(owner) = self.owner.take() {
                    owner.join().expect("identity fixture owner panicked");
                }
            }
        }
        let _owner_cleanup = OwnerCleanup {
            thread_id: owner_thread_id,
            owner: Some(owner),
        };
        let hwnd = HWND(hwnd_value as *mut c_void);
        let window_id = hwnd_value as u64;
        let actual_identity = capture_window_identity(hwnd).expect("live fixture identity");
        let stale_identity = WindowIdentity {
            thread_id: actual_identity.thread_id.wrapping_add(1),
            ..actual_identity
        };
        lock_snap_disabled()
            .get_or_insert_with(HashMap::new)
            .insert(window_id, stale_identity);
        struct TrackingCleanup(WindowId);
        impl Drop for TrackingCleanup {
            fn drop(&mut self) {
                if let Some(set) = lock_snap_disabled().as_mut() {
                    set.remove(&self.0);
                }
            }
        }
        let _tracking_cleanup = TrackingCleanup(window_id);

        window_style_worker()
            .send(WindowStyleRequest::Apply {
                window_id,
                hwnd: hwnd_value,
                identity: stale_identity,
                operation: WindowStyleOperation::Remove,
                correlation_id: MAXIMIZEBOX_GEOMETRY_CORRELATION.fetch_add(1, Ordering::Relaxed),
            })
            .unwrap();
        pump_until_window_style_idle();
        assert_eq!(
            STYLE_CHANGE_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a deferred remove for a stale HWND identity must not change its style"
        );
        assert_ne!(
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                ) & WS_MAXIMIZEBOX_BIT
            },
            0
        );
    }

    #[test]
    fn test_queued_restore_rejects_changed_window_identity() {
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW, GWL_STYLE};

        let _guard = lock_snap_tracking_fixture();
        let fixture = HiddenFramedFixture::create();
        let hwnd = fixture.hwnd();
        let window_id = fixture.window_id();
        let actual_identity = capture_window_identity(hwnd).expect("live fixture identity");
        let stale_identity = WindowIdentity {
            thread_id: actual_identity.thread_id.wrapping_add(1),
            ..actual_identity
        };
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            SetWindowLongW(hwnd, GWL_STYLE, style & !WS_MAXIMIZEBOX_BIT);
        }
        assert_eq!(
            unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            0
        );

        window_style_worker()
            .send(WindowStyleRequest::Apply {
                window_id,
                hwnd: hwnd.0 as usize,
                identity: stale_identity,
                operation: WindowStyleOperation::Restore,
                correlation_id: MAXIMIZEBOX_GEOMETRY_CORRELATION.fetch_add(1, Ordering::Relaxed),
            })
            .unwrap();
        pump_until_window_style_idle();
        assert_eq!(
            unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & WS_MAXIMIZEBOX_BIT,
            0,
            "a deferred restore for a stale HWND identity must not touch the live window"
        );
    }

    #[test]
    fn test_remove_restore_maximizebox_diagnostics_and_semantics() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let _guard = lock_snap_tracking_fixture();
        let fixture = HiddenFramedFixture::create();
        assert!(!unsafe { IsWindowVisible(fixture.hwnd()) }.as_bool());

        let before = capture_maximizebox_geometry(fixture.hwnd());
        let style = before.style.expect("live style should be readable");
        assert_ne!(style & WS_MAXIMIZEBOX_BIT, 0);
        let window_rect = before
            .window_rect
            .expect("live outer rect should be readable");
        assert!(window_rect[2] > window_rect[0]);
        assert!(window_rect[3] > window_rect[1]);
        let client_rect = before
            .client_rect
            .expect("live client rect should be readable");
        assert!(client_rect[2] > client_rect[0]);
        assert!(client_rect[3] > client_rect[1]);

        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let subscriber = GeometryEventSubscriber {
            events: events.clone(),
        };
        let window_id = fixture.window_id();
        let dispatcher = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::set_global_default(dispatcher)
            .expect("geometry test must install the only global subscriber");
        assert!(remove_maximizebox(window_id).unwrap());
        pump_until_window_style_idle();
        assert!(restore_maximizebox(window_id).unwrap());
        pump_until_window_style_idle();

        let all_events = events.lock().unwrap();
        let recorded: Vec<_> = all_events
            .iter()
            .filter(|event| event.hwnd == Some(window_id))
            .collect();
        assert_eq!(recorded.len(), 6);
        assert!(recorded
            .iter()
            .all(|event| event.event.as_deref() == Some(MAXIMIZEBOX_GEOMETRY_EVENT)));
        assert_eq!(
            recorded
                .iter()
                .map(|event| event.stage.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("before_style"),
                Some("after_style"),
                Some("after_frame"),
                Some("before_style"),
                Some("after_style"),
                Some("after_frame"),
            ]
        );
        assert!(recorded[..3]
            .iter()
            .all(|event| event.operation.as_deref() == Some("remove")));
        assert!(recorded[3..]
            .iter()
            .all(|event| event.operation.as_deref() == Some("restore")));
        let remove_id = recorded[0].correlation_id.expect("remove correlation id");
        let restore_id = recorded[3].correlation_id.expect("restore correlation id");
        assert!(recorded[..3]
            .iter()
            .all(|event| event.correlation_id == Some(remove_id)));
        assert!(recorded[3..]
            .iter()
            .all(|event| event.correlation_id == Some(restore_id)));
        drop(all_events);

        let after_restore = capture_maximizebox_geometry(fixture.hwnd());
        assert_eq!(
            after_restore.style.expect("restored style") & WS_MAXIMIZEBOX_BIT,
            WS_MAXIMIZEBOX_BIT
        );
        assert_eq!(after_restore.window_rect, Ok(window_rect));

        assert!(remove_maximizebox(window_id).unwrap());
        pump_until_window_style_idle();
        let after_remove = capture_maximizebox_geometry(fixture.hwnd());
        assert_eq!(
            after_remove.style.expect("removed style") & WS_MAXIMIZEBOX_BIT,
            0
        );
        assert_eq!(after_remove.window_rect, Ok(window_rect));
        assert!(!remove_maximizebox(window_id).unwrap());
        assert_eq!(
            capture_maximizebox_geometry(fixture.hwnd()).window_rect,
            Ok(window_rect)
        );

        assert!(restore_maximizebox(window_id).unwrap());
        assert!(restore_maximizebox(window_id).unwrap());
        pump_until_window_style_idle();
        assert_eq!(
            capture_maximizebox_geometry(fixture.hwnd()).window_rect,
            Ok(window_rect)
        );
    }

    #[test]
    fn test_maximizebox_geometry_capture_zero_ex_style_is_ok() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, IsWindowVisible, WINDOW_EX_STYLE, WS_POPUP,
        };

        struct HiddenPopup(HWND);
        impl Drop for HiddenPopup {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_POPUP,
                48,
                48,
                64,
                64,
                None,
                None,
                None,
                None,
            )
        }
        .expect("failed to create hidden popup STATIC fixture");
        let _fixture = HiddenPopup(hwnd);
        assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
        assert_eq!(capture_maximizebox_geometry(hwnd).ex_style, Ok(0));
    }

    #[test]
    fn test_maximizebox_geometry_capture_invalid_hwnd_is_err() {
        use windows::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE;

        let dead = capture_maximizebox_geometry(HWND::default());
        let invalid_hwnd = windows::core::HRESULT::from_win32(ERROR_INVALID_WINDOW_HANDLE.0).0;
        assert_eq!(dead.style, Err(invalid_hwnd));
        assert_eq!(dead.ex_style, Err(invalid_hwnd));
        assert!(dead.window_rect.is_err());
        assert!(dead.client_rect.is_err());
        assert!(dead.dwm_frame.is_err());
        assert!(dead.nc_rendering.is_err());
    }
}
