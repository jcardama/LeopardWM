//! Active window border frame overlay.
//!
//! Renders a colored frame around the focused window using a layered window
//! with per-pixel alpha for anti-aliased rounded corners. Uses a signed
//! distance field to produce smooth edges without GDI+ or Direct2D.

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::IsWindow;
use crate::Win32Error;

/// Position of the border relative to the window frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderPosition {
    Outside,
    Inside,
}

/// Windows 11 corner radius (pixels).
const WIN11_CORNER_RADIUS: f32 = 8.0;

/// Signed distance field for a rounded rectangle.
///
/// Returns negative values inside, positive outside, zero on the boundary.
fn rounded_rect_sdf(px: f32, py: f32, rx: f32, ry: f32, rw: f32, rh: f32, radius: f32) -> f32 {
    let cx = rx + rw / 2.0;
    let cy = ry + rh / 2.0;
    let hx = rw / 2.0;
    let hy = rh / 2.0;

    let dx = (px - cx).abs() - hx + radius;
    let dy = (py - cy).abs() - hy + radius;

    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    outside + inside - radius
}

fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    v.max(lo).min(hi)
}

/// Cached rendering state to avoid re-rendering when only position changes.
struct BorderState {
    color_bgr: u32,
    cached_w: i32,
    cached_h: i32,
    cached_width: u32,
    cached_position: BorderPosition,
    cached_color: u32,
}

static BORDER_STATE: Mutex<BorderState> = Mutex::new(BorderState {
    color_bgr: 0x00F48542,
    cached_w: 0,
    cached_h: 0,
    cached_width: 0,
    cached_position: BorderPosition::Outside,
    cached_color: 0,
});

/// Manages a transparent overlay window that draws a colored border frame
/// around the focused window with anti-aliased rounded corners.
pub struct BorderFrame {
    hwnd: HWND,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl BorderFrame {
    /// Create the border frame overlay on a background thread.
    pub fn new() -> Result<Self, Win32Error> {
        let (tx, rx) = mpsc::channel::<Result<isize, Win32Error>>();

        let thread = std::thread::Builder::new()
            .name("border-frame".into())
            .spawn(move || unsafe {
                let class_name: Vec<u16> = "LeopardWMBorderFrame\0".encode_utf16().collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(border_frame_proc),
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
                    Ok(h) => {
                        let _ = tx.send(Ok(h.0 as isize));
                        let mut msg = MSG::default();
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            let _ = DispatchMessageW(&msg);
                        }
                        let _ = DestroyWindow(h);
                        let _ = UnregisterClassW(
                            windows::core::PCWSTR(class_name.as_ptr()),
                            None,
                        );
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "BorderFrame: {}",
                            e
                        ))));
                    }
                }
            })
            .map_err(|e| Win32Error::HookInstallFailed(format!("BorderFrame thread: {}", e)))?;

        let hwnd_raw = match rx.recv() {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(Win32Error::HookInstallFailed(
                    "BorderFrame init failed".into(),
                ))
            }
        };

        Ok(Self {
            hwnd: HWND(hwnd_raw as *mut c_void),
            _thread: Some(thread),
        })
    }

    /// Show the border frame around the target window.
    pub fn show(&self, target_hwnd: u64, width: u32, position: BorderPosition, color_bgr: u32) {
        if let Ok(mut state) = BORDER_STATE.lock() {
            state.color_bgr = color_bgr;
        }
        self.reposition(target_hwnd, width, position);
    }

    /// Hide the border frame.
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Reposition the border frame to track the target window.
    ///
    /// If the overlay dimensions and config haven't changed since the last
    /// render, only the position is updated (fast path for scrolling).
    pub fn reposition(&self, target_hwnd: u64, width: u32, position: BorderPosition) {
        let target = HWND(target_hwnd as *mut c_void);
        let bw = width as i32;

        unsafe {
            if !IsWindow(Some(target)).as_bool() {
                return;
            }

            // Get actual visible window bounds (excludes DWM shadow padding).
            let mut rect = RECT::default();
            const DWMWA_EXTENDED_FRAME_BOUNDS: i32 = 9;
            let got_dwm = DwmGetWindowAttribute(
                target,
                DWMWINDOWATTRIBUTE(DWMWA_EXTENDED_FRAME_BOUNDS),
                &mut rect as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok();
            if !got_dwm && GetWindowRect(target, &mut rect).is_err() {
                return;
            }

            // For outside borders, DWMWA_EXTENDED_FRAME_BOUNDS includes a
            // transparent resize border. Shrink by 1px to match the actual
            // visual window edge so the border sits flush with the content.
            if position == BorderPosition::Outside {
                rect.left += 1;
                rect.top += 1;
                rect.right -= 1;
                rect.bottom -= 1;
            }

            let tw = rect.right - rect.left;
            let th = rect.bottom - rect.top;

            let (x, y, w, h) = match position {
                BorderPosition::Outside => (
                    rect.left - bw,
                    rect.top - bw,
                    tw + 2 * bw,
                    th + 2 * bw,
                ),
                BorderPosition::Inside => (rect.left, rect.top, tw, th),
            };

            // Check if we can just move (same size/config = bitmap is cached)
            let needs_render = {
                let state = BORDER_STATE.lock().unwrap();
                w != state.cached_w
                    || h != state.cached_h
                    || width != state.cached_width
                    || position != state.cached_position
                    || state.color_bgr != state.cached_color
            };

            if needs_render {
                self.render_and_update(x, y, w, h, width, position);
            } else {
                // Fast path: just move the overlay (bitmap is retained).
                // Position just above the target in z-order.
                let insert_after = match GetWindow(target, GW_HWNDPREV) {
                    Ok(prev) if prev != self.hwnd => Some(prev),
                    _ => Some(HWND_TOP),
                };
                let _ = SetWindowPos(
                    self.hwnd,
                    insert_after,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE,
                );
                let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            }
        }
    }

    /// Render the anti-aliased border frame bitmap and update the layered window.
    fn render_and_update(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        width: u32,
        position: BorderPosition,
    ) {
        let bw = width as f32;

        let color_bgr = {
            let mut state = BORDER_STATE.lock().unwrap();
            state.cached_w = w;
            state.cached_h = h;
            state.cached_width = width;
            state.cached_position = position;
            state.cached_color = state.color_bgr;
            state.color_bgr
        };

        // Extract color components (BGR → individual channels)
        let cb = ((color_bgr >> 16) & 0xFF) as u8;
        let cg = ((color_bgr >> 8) & 0xFF) as u8;
        let cr = (color_bgr & 0xFF) as u8;

        // Compute corner radii.
        let (outer_r, inner_r) = match position {
            BorderPosition::Outside => {
                // Rect was shrunk by 1px, so visual radius is WIN11_CORNER_RADIUS - 1.
                let visual_r = (WIN11_CORNER_RADIUS - 1.0).max(0.0);
                let outer = visual_r + bw;
                let inner = visual_r;
                (outer, inner)
            }
            BorderPosition::Inside => {
                // No rect shrink, use full corner radius.
                let outer = WIN11_CORNER_RADIUS;
                let inner = (WIN11_CORNER_RADIUS - bw).max(0.0);
                (outer, inner)
            }
        };

        let wf = w as f32;
        let hf = h as f32;

        unsafe {
            // Create a 32-bit top-down DIB section for per-pixel alpha
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // negative = top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let hbitmap =
                CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            let Ok(hbitmap) = hbitmap else {
                return;
            };

            let pixels =
                std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);

            // Only iterate over pixels near the border edges (optimization).
            // The band must be wide enough to capture corner rounding where the
            // inner SDF curves away, extending border pixels further inward.
            let margin = 1.5f32;
            let band = bw + inner_r + margin;

            for py in 0..h {
                let pyf = py as f32 + 0.5;
                let in_top_band = pyf < band;
                let in_bottom_band = pyf > hf - band;
                let in_vert_band = in_top_band || in_bottom_band;

                for px in 0..w {
                    let pxf = px as f32 + 0.5;

                    // Skip pixels far from any edge (interior of the window)
                    if !in_vert_band {
                        let in_left_band = pxf < band;
                        let in_right_band = pxf > wf - band;
                        if !in_left_band && !in_right_band {
                            continue;
                        }
                    }

                    // Signed distance to outer and inner rounded rects
                    let sdf_outer = rounded_rect_sdf(pxf, pyf, 0.0, 0.0, wf, hf, outer_r);
                    let sdf_inner = rounded_rect_sdf(
                        pxf,
                        pyf,
                        bw,
                        bw,
                        wf - 2.0 * bw,
                        hf - 2.0 * bw,
                        inner_r,
                    );

                    // Anti-aliased alpha: smooth transition at both edges
                    let alpha_outer = clamp(0.5 - sdf_outer, 0.0, 1.0);
                    let alpha_inner = clamp(sdf_inner + 0.5, 0.0, 1.0);
                    let alpha = alpha_outer * alpha_inner;

                    if alpha > 0.0 {
                        let a = (alpha * 255.0) as u8;
                        // Premultiplied alpha (required by UpdateLayeredWindow + AC_SRC_ALPHA)
                        let pr = (cr as u32 * a as u32 / 255) as u8;
                        let pg = (cg as u32 * a as u32 / 255) as u8;
                        let pb = (cb as u32 * a as u32 / 255) as u8;
                        // BGRA pixel (little-endian: B, G, R, A)
                        pixels[(py * w + px) as usize] =
                            (a as u32) << 24 | (pr as u32) << 16 | (pg as u32) << 8 | pb as u32;
                    }
                }
            }

            // Apply the bitmap to the layered window
            let hdc_screen = GetDC(None);
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let old = SelectObject(hdc_mem, hbitmap.into());

            let pt_dst = POINT { x, y };
            let sz = SIZE { cx: w, cy: h };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(hdc_screen),
                Some(&pt_dst),
                Some(&sz),
                Some(hdc_mem),
                Some(&pt_src),
                windows::Win32::Foundation::COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(hdc_mem, old);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);
            let _ = DeleteObject(hbitmap.into());

            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
    }
}

impl Drop for BorderFrame {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
    }
}

/// Minimal window proc — layered windows with UpdateLayeredWindow don't use WM_PAINT.
unsafe extern "system" fn border_frame_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
