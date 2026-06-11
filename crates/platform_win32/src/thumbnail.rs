//! DWM thumbnail bindings for ghost-animation of swap-chain-sensitive windows.
//!
//! Chromium/Electron/Mozilla/Cascadia renderers can't keep up with per-frame
//! `SetWindowPos` cadence — their swap chains desync. This module composites
//! a DWM thumbnail of a cloaked source HWND onto a hidden host window and
//! animates the thumbnail's destination rect instead of moving the live HWND.
//!
//! See `crates/daemon/src/animation_worker.rs` for the per-frame update path
//! and `crates/daemon/src/helpers.rs::start_layout_transition` for the
//! registration site.

use crate::{window_id_to_hwnd, Win32Error};
use leopardwm_core_layout::{Rect, WindowId};
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use tracing::warn;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, HBITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, SelectObject, GetDC, ReleaseDC, AC_SRC_ALPHA,
    AC_SRC_OVER, BLENDFUNCTION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, RegisterClassW,
    SetWindowPos, UpdateLayeredWindow, CW_USEDEFAULT, HWND_NOTOPMOST, HWND_TOPMOST, MSG,
    SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, ULW_ALPHA, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

/// Class name for the singleton thumbnail host window. Listed in
/// `enumeration::should_skip_window_by_class` so we don't try to manage
/// our own overlay.
pub(crate) const THUMBNAIL_HOST_CLASS: &str = "LeopardWMThumbnailHost";

/// Process-global counter of currently-registered DWM thumbnails. Used
/// by tests and the `lwm health` IPC field to assert no leaks. Mirrors
/// `Z_ORDER_STATE.balance` for lock-free reads.
static REGISTER_BALANCE: AtomicI64 = AtomicI64::new(0);

/// Serializes register/unregister z-order side effects so concurrent
/// register/unregister can't interleave between the atomic balance update
/// and the `set_topmost` side effect.
///
/// Without this, the bad interleaving is:
///   T1 unregister: balance=1→0, about to call set_topmost(false)
///   T2 register:   balance=0→1, calls set_topmost(true) first
///   T1 unregister: calls set_topmost(false)  ← host left non-topmost with a live thumbnail
struct ZOrderState {
    balance: i64,
}
static Z_ORDER_STATE: Mutex<ZOrderState> = Mutex::new(ZOrderState { balance: 0 });

/// Return the current outstanding-registration count. Should converge to 0
/// after any animation cycle completes.
pub fn current_register_balance() -> i64 {
    REGISTER_BALANCE.load(Ordering::Relaxed)
}

/// RAII wrapper around an `HTHUMBNAIL`. Unregisters on drop unless the
/// handle has been transferred out via [`ThumbnailHandle::into_isize`].
///
/// `Send` + `Sync` safety: `HTHUMBNAIL` is a kernel-level handle managed
/// by `dwm.exe`; it has no thread affinity post-registration. Cross-thread
/// `DwmUpdateThumbnailProperties` is supported by design (Aero Flip 3D
/// used the same pattern from worker threads).
pub struct ThumbnailHandle {
    /// Raw `HTHUMBNAIL` value. Set to 0 by `into_isize` to suppress Drop.
    handle: isize,
}

// SAFETY: HTHUMBNAIL is a process-wide DWM handle, not bound to any HWND
// owner thread for updates. Codex's Microsoft-Learn check confirmed no
// apartment-affinity requirement post-registration.
unsafe impl Send for ThumbnailHandle {}
unsafe impl Sync for ThumbnailHandle {}

impl Drop for ThumbnailHandle {
    fn drop(&mut self) {
        if self.handle != 0 {
            unregister_raw(self.handle);
        }
    }
}

impl ThumbnailHandle {
    /// Consume this handle without firing Drop, returning the raw `isize`.
    /// The caller takes responsibility for eventually calling
    /// [`unregister_raw`] on the returned value (or wrapping it in a new
    /// owning type that does).
    ///
    /// Used at landing to transfer handle ownership from the daemon's
    /// `AppState.ghost_handles` into `WorkerCommand::Crossfade` entries
    /// owned by the worker thread.
    pub fn into_isize(mut self) -> isize {
        let raw = self.handle;
        self.handle = 0;
        std::mem::forget(self);
        raw
    }

    /// Raw `HTHUMBNAIL` for cross-thread `update` calls. Does NOT transfer
    /// ownership — Drop still fires when `self` is dropped.
    pub fn as_isize(&self) -> isize {
        self.handle
    }
}

/// Register a DWM thumbnail of `source` against the singleton host window.
/// On success, returns an owning handle whose `Drop` unregisters.
pub fn register(wid: WindowId) -> Result<ThumbnailHandle, Win32Error> {
    let host_hwnd = host().hwnd();
    if host_hwnd.0.is_null() {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail host unavailable".into(),
        ));
    }
    register_to(host_hwnd, wid)
}

/// Register a DWM thumbnail of `source_wid` against an arbitrary top-level
/// window of THIS process (e.g. the overview overlay). Shares the same
/// `REGISTER_BALANCE` accounting and host z-order invariant as [`register`]
/// (the balance counts every handle we own, wherever it is composited).
pub fn register_for_window(
    dest_hwnd_raw: isize,
    source_wid: WindowId,
) -> Result<ThumbnailHandle, Win32Error> {
    if dest_hwnd_raw == 0 {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail destination hwnd is null".into(),
        ));
    }
    register_to(HWND(dest_hwnd_raw as *mut c_void), source_wid)
}

fn register_to(dest: HWND, wid: WindowId) -> Result<ThumbnailHandle, Win32Error> {
    let source = window_id_to_hwnd(wid)?;
    let raw = unsafe { DwmRegisterThumbnail(dest, source) }.map_err(|e| {
        Win32Error::SetPositionFailed(format!("DwmRegisterThumbnail({:?}): {}", source.0, e))
    })?;
    if raw == 0 {
        return Err(Win32Error::SetPositionFailed(
            "DwmRegisterThumbnail returned null handle".into(),
        ));
    }
    // Serialize the balance update with the z-order side effect so a
    // concurrent unregister can't sneak its set_topmost(false) in
    // between our increment and our set_topmost(true).
    {
        let mut z = Z_ORDER_STATE
            .lock()
            .unwrap_or_else(crate::recover_poisoned_mutex);
        let prev = z.balance;
        z.balance += 1;
        REGISTER_BALANCE.store(z.balance, Ordering::Relaxed);
        // First active thumbnail: promote the host to HWND_TOPMOST so the
        // composition sits above ordinary windows. While idle the host
        // stays non-topmost so the Windows taskbar (also topmost) can
        // animate without z-order interference.
        if prev == 0 {
            host().set_topmost(true);
        }
    }
    Ok(ThumbnailHandle { handle: raw })
}

/// Update the destination rect, opacity, and visibility of a registered
/// thumbnail. Safe to call from any thread (the worker thread does this
/// per animation frame). Destination-agnostic: only the `HTHUMBNAIL` is
/// needed, whatever window the registration targeted.
///
/// `dest_client_rect` is in CLIENT coordinates of the thumbnail's
/// DESTINATION window, NOT screen coordinates. For host-bound thumbnails
/// convert via [`screen_to_host_client`] first; overview thumbnails pass
/// overlay client coordinates directly.
pub fn update(
    handle: isize,
    dest_client_rect: Rect,
    opacity: u8,
    visible: bool,
) -> Result<(), Win32Error> {
    if handle == 0 {
        return Err(Win32Error::SetPositionFailed(
            "thumbnail::update called with null handle".into(),
        ));
    }
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE,
        rcDestination: RECT {
            left: dest_client_rect.x,
            top: dest_client_rect.y,
            right: dest_client_rect.x + dest_client_rect.width,
            bottom: dest_client_rect.y + dest_client_rect.height,
        },
        rcSource: RECT::default(),
        opacity,
        fVisible: BOOL::from(visible),
        fSourceClientAreaOnly: BOOL::from(false),
    };
    let result = unsafe { DwmUpdateThumbnailProperties(handle, &props) };
    if let Err(e) = result {
        return Err(Win32Error::SetPositionFailed(format!(
            "DwmUpdateThumbnailProperties: {}",
            e
        )));
    }
    Ok(())
}

/// Source window's true size for a registered thumbnail, used for
/// aspect-fit destination rects. `None` on null handles or DWM failure.
pub fn source_size(handle: isize) -> Option<(i32, i32)> {
    if handle == 0 {
        return None;
    }
    let size = unsafe { DwmQueryThumbnailSourceSize(handle) }.ok()?;
    if size.cx <= 0 || size.cy <= 0 {
        return None;
    }
    Some((size.cx, size.cy))
}

/// Unregister a thumbnail by raw `HTHUMBNAIL` value. Used by the worker
/// thread when consuming `CrossfadeEntry` values (whose Drop calls this).
///
/// Idempotent on null/zero handles — does nothing.
pub fn unregister_raw(handle: isize) {
    if handle == 0 {
        return;
    }
    // A failed DwmUnregisterThumbnail leaks the DWM handle (the caller
    // already gave up its owning reference, so we can't retry). Decrement
    // anyway: the balance tracks handles WE account for, and pinning it
    // above zero on a transient failure would strand the host topmost for
    // the rest of the session and make the health metric lie. Clamp at
    // zero so a double-unregister or failure run can't go negative.
    if let Err(e) = unsafe { DwmUnregisterThumbnail(handle) } {
        warn!("DwmUnregisterThumbnail({}) failed (handle leaked): {}", handle, e);
    }
    // Serialize the balance update with the z-order side effect.
    let mut z = Z_ORDER_STATE
        .lock()
        .unwrap_or_else(crate::recover_poisoned_mutex);
    let prev = z.balance;
    z.balance = (z.balance - 1).max(0);
    REGISTER_BALANCE.store(z.balance, Ordering::Relaxed);
    // Last accounted thumbnail just went away: drop the host back to
    // non-topmost so it stops interfering with the taskbar's auto-hide
    // z-order. Guard on prev >= 1 so a clamped underflow can't skip it.
    if prev >= 1 && z.balance == 0 {
        host().set_topmost(false);
    }
}

/// Convert a screen-coordinate rect to client coordinates of the host
/// window. The host is positioned at the virtual-screen origin
/// (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`), so the conversion is a
/// simple subtraction.
///
/// CRITICAL: `SM_XVIRTUALSCREEN` can be negative when a secondary monitor
/// extends to the left of the primary. The single most likely bug class
/// in this whole module is a sign error here. Unit-tested.
pub fn screen_to_host_client(screen: Rect, host_origin: (i32, i32)) -> Rect {
    Rect {
        x: screen.x - host_origin.0,
        y: screen.y - host_origin.1,
        width: screen.width,
        height: screen.height,
    }
}

/// Predicate: does this window's class name match a swap-chain-sensitive
/// window family that benefits from ghost animation?
///
/// Matches:
/// - `Chrome_WidgetWin_*` — Chromium / Electron (Chrome, Edge, Slack,
///   Discord, Beeper, Spotify, VS Code, Cursor, Notion, ...)
/// - `MozillaWindowClass` — Firefox, Zen
/// - `CASCADIA_HOSTING_WINDOW_CLASS` — Windows Terminal Preview
pub fn is_swap_chain_class(wid: WindowId) -> bool {
    is_swap_chain_class_str(&class_name(wid))
}

/// String variant of [`is_swap_chain_class`] for callers that have
/// already read the class name (avoids a redundant `GetClassNameW` call).
pub fn is_swap_chain_class_str(class: &str) -> bool {
    class.starts_with("Chrome_WidgetWin_")
        || class == "MozillaWindowClass"
        || class == "CASCADIA_HOSTING_WINDOW_CLASS"
}

/// Read the class name of a window. Returns empty string on failure
/// (unknown class, dead HWND, or invalid window ID).
pub fn class_name(wid: WindowId) -> String {
    let Ok(hwnd) = window_id_to_hwnd(wid) else {
        return String::new();
    };
    class_name_hwnd(hwnd)
}

fn class_name_hwnd(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf: [u16; 256] = [0; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

// ----------------------------------------------------------------------
// ThumbnailHost — singleton invisible host window covering the virtual screen.
// ----------------------------------------------------------------------

/// Singleton invisible host window used as the destination for all
/// thumbnails. Created lazily on first `host()` call. Lives until process
/// exit.
///
/// Style choice: `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
/// | WS_EX_TRANSPARENT` with a 1×1 fully-transparent UpdateLayeredWindow
/// backing, mirroring `border.rs`. This is the proven-working pattern for
/// click-through composite overlays in our codebase. If Intel iGPU
/// hardware shows flicker artifacts, fall back to variants discussed in
/// the plan's "ThumbnailHost — singleton host window" section.
pub struct ThumbnailHost {
    hwnd_raw: isize,
    /// Virtual-screen origin captured at host creation, updated by
    /// `resize_to_virtual_screen` on display change. Wrapped in a Mutex
    /// for cross-thread reads (animation worker reads on every frame).
    origin: std::sync::Mutex<(i32, i32)>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

static HOST: OnceLock<ThumbnailHost> = OnceLock::new();

/// Get (or lazily create) the global thumbnail host.
pub fn host() -> &'static ThumbnailHost {
    HOST.get_or_init(|| match ThumbnailHost::new() {
        Ok(h) => h,
        Err(e) => {
            // Construct-failure path: panic in dev, but in production
            // surface a recoverable host with a null HWND so callers can
            // detect and fall back to legacy animation.
            warn!("ThumbnailHost::new failed: {} — ghost animation disabled", e);
            ThumbnailHost {
                hwnd_raw: 0,
                origin: std::sync::Mutex::new(virtual_screen_origin()),
                _thread: None,
            }
        }
    })
}

impl ThumbnailHost {
    fn new() -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("ThumbnailHost::new spawns a DWM host window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            let origin = virtual_screen_origin();
            let (vw, vh) = virtual_screen_size();
            let (tx, rx) = mpsc::channel::<Result<isize, Win32Error>>();

            let thread = std::thread::Builder::new()
                .name("leopardwm-thumbnail-host".into())
                .spawn(move || unsafe {
                    let class_name: Vec<u16> =
                        format!("{}\0", THUMBNAIL_HOST_CLASS).encode_utf16().collect();
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(thumbnail_host_proc),
                        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                        ..Default::default()
                    };
                    RegisterClassW(&wc);

                    let ex_style = WS_EX_LAYERED
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE
                        | WS_EX_TRANSPARENT;

                    match CreateWindowExW(
                        ex_style,
                        windows::core::PCWSTR(class_name.as_ptr()),
                        None,
                        WS_POPUP | WS_VISIBLE,
                        origin.0,
                        origin.1,
                        vw,
                        vh,
                        None,
                        None,
                        None,
                        None,
                    ) {
                        Ok(h) => {
                            // Initialize layered window with a 1×1 fully-transparent
                            // bitmap so DWM treats it as a valid layered surface.
                            // Without this, the host has no backing and DWM
                            // composition of the thumbnail behaves inconsistently.
                            init_layered_transparent(h);
                            // Idle z-order: non-topmost so the Windows taskbar
                            // (also topmost) can show in front during auto-hide
                            // animation. `register` promotes us to topmost when
                            // at least one thumbnail is alive.
                            let _ = SetWindowPos(
                                h,
                                Some(HWND_NOTOPMOST),
                                origin.0,
                                origin.1,
                                vw,
                                vh,
                                SWP_NOACTIVATE,
                            );
                            let _ = tx.send(Ok(h.0 as isize));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                let _ = DispatchMessageW(&msg);
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(Win32Error::HookInstallFailed(format!(
                                "ThumbnailHost: {}",
                                e
                            ))));
                        }
                    }
                })
                .map_err(|e| {
                    Win32Error::HookInstallFailed(format!("ThumbnailHost thread: {}", e))
                })?;

            let hwnd_raw = match rx.recv() {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Win32Error::HookInstallFailed(
                        "ThumbnailHost init failed".into(),
                    ))
                }
            };

            Ok(Self {
                hwnd_raw,
                origin: std::sync::Mutex::new(origin),
                _thread: Some(thread),
            })
        }
    }

    /// HWND of the host window. `HWND(0)` if construction failed (e.g.
    /// under cfg(test) or genuine init failure).
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd_raw as *mut c_void)
    }

    /// Origin of the host's client area in screen coordinates (matches
    /// `(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)` at the most recent
    /// `resize_to_virtual_screen` call, or host creation if no resize
    /// has happened).
    pub fn origin(&self) -> (i32, i32) {
        *self.origin.lock().unwrap_or_else(crate::recover_poisoned_mutex)
    }

    /// `true` if the host construction succeeded. Callers should check
    /// this before attempting registration.
    pub fn is_available(&self) -> bool {
        self.hwnd_raw != 0
    }

    /// Resize and reposition the host to the current virtual-screen
    /// geometry. Called from the daemon's display-change recovery so
    /// thumbnail destination rects use post-change coordinates. Subsequent
    /// `register` calls see the new origin via `origin()`.
    pub fn resize_to_virtual_screen(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let new_origin = virtual_screen_origin();
        let (vw, vh) = virtual_screen_size();
        {
            let mut g = self
                .origin
                .lock()
                .unwrap_or_else(crate::recover_poisoned_mutex);
            *g = new_origin;
        }
        let hwnd = self.hwnd();
        // Preserve current z-order: if thumbnails are live we're topmost,
        // otherwise non-topmost. Pass SWP_NOZORDER to leave it untouched.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                new_origin.0,
                new_origin.1,
                vw,
                vh,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }

    /// Toggle the host's z-order between topmost (while thumbnails are
    /// active) and non-topmost (idle). Idle non-topmost lets the taskbar
    /// auto-hide animation appear correctly in front of windows; topmost
    /// during animation ensures the thumbnail composites above the live
    /// HWNDs that may be cloaked underneath.
    fn set_topmost(&self, topmost: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = self.hwnd();
        let z = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(z),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }
}

extern "system" fn thumbnail_host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Set up the layered host with a 1×1 fully-transparent backing so DWM
/// composes its layered surface correctly. Without this step,
/// `WS_EX_LAYERED` windows that never call `UpdateLayeredWindow` may not
/// composite thumbnails reliably on all GPUs.
unsafe fn init_layered_transparent(hwnd: HWND) {
    let screen_dc: HDC = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));

    // 1×1 BGRA bitmap, alpha = 0 (fully transparent).
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 1,
            biHeight: 1,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp_result = CreateDIBSection(
        Some(mem_dc),
        &bmi,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    );
    let bmp: HBITMAP = match bmp_result {
        Ok(h) => h,
        Err(_) => {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            return;
        }
    };
    // Zero the pixel (alpha = 0).
    if !bits.is_null() {
        std::ptr::write_bytes(bits as *mut u8, 0, 4);
    }
    let old = SelectObject(mem_dc, bmp.into());

    let src_pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    let size = windows::Win32::Foundation::SIZE { cx: 1, cy: 1 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        None,
        None,
        Some(&size),
        Some(mem_dc),
        Some(&src_pt),
        windows::Win32::Foundation::COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(mem_dc, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
}

fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

fn virtual_screen_size() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

// Silence unused-warning hint: SWP_NOZORDER, SET_WINDOW_POS_FLAGS,
// CW_USEDEFAULT, WS_POPUP are kept for completeness even when not all
// are used directly.
#[allow(dead_code)]
const _UNUSED_IMPORTS: (SET_WINDOW_POS_FLAGS, i32) = (SWP_NOZORDER, CW_USEDEFAULT);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screen_to_host_client_zero_origin() {
        let screen = Rect {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        let client = screen_to_host_client(screen, (0, 0));
        assert_eq!(client.x, 100);
        assert_eq!(client.y, 200);
        assert_eq!(client.width, 800);
        assert_eq!(client.height, 600);
    }

    #[test]
    fn test_screen_to_host_client_negative_origin() {
        // Secondary monitor LEFT of primary: SM_XVIRTUALSCREEN is negative.
        // A window at screen x=-1000 should map to client x=0 when the host
        // origin is also -1000.
        let screen = Rect {
            x: -1000,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let client = screen_to_host_client(screen, (-1000, 0));
        assert_eq!(client.x, 0);
        assert_eq!(client.y, 0);
        assert_eq!(client.width, 1920);
        assert_eq!(client.height, 1080);

        // A window on the primary (at screen x=0) with the host at x=-1000
        // should map to client x=1000.
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let client2 = screen_to_host_client(primary, (-1000, 0));
        assert_eq!(client2.x, 1000);
        assert_eq!(client2.y, 0);
    }

    #[test]
    fn test_is_swap_chain_class_str() {
        assert!(is_swap_chain_class_str("Chrome_WidgetWin_1"));
        assert!(is_swap_chain_class_str("Chrome_WidgetWin_2"));
        assert!(is_swap_chain_class_str("Chrome_WidgetWin_100"));
        assert!(is_swap_chain_class_str("MozillaWindowClass"));
        assert!(is_swap_chain_class_str("CASCADIA_HOSTING_WINDOW_CLASS"));

        assert!(!is_swap_chain_class_str("Notepad"));
        assert!(!is_swap_chain_class_str(""));
        assert!(!is_swap_chain_class_str("Chrome_RenderWidgetHostHWND")); // internal widget; skipped earlier
        assert!(!is_swap_chain_class_str("Chrome_Widget")); // prefix-only match avoided
        assert!(!is_swap_chain_class_str("CASCADIA")); // partial match avoided
    }

    #[test]
    fn test_register_balance_starts_at_zero() {
        // Process-global; may have non-zero state from other tests in the
        // same binary, but we can at least observe the read API.
        let _initial = current_register_balance();
    }
}
