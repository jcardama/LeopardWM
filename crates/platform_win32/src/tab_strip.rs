//! Tab strip overlay rendered above the focused tabbed column.
//!
//! Mirrors `BorderFrame`'s structure (background thread, layered window,
//! `UpdateLayeredWindow`) but with two key differences:
//! 1. Drops `WS_EX_TRANSPARENT` so mouse clicks on the strip are delivered
//!    to this window's WndProc — clicking a tab activates it.
//! 2. Uses GDI text rendering (`CreateFontW` + `TextOutW`) for tab labels.
//!    Uses `ConstantAlpha` blend mode (`AlphaFormat=0`) so per-pixel alpha
//!    isn't required — keeps the rendering path simple while still getting
//!    a translucent strip via `SourceConstantAlpha`.
//!
//! Single-instance: one strip overlay tracks "the focused workspace's
//! focused column when that column is Tabbed". The daemon hides it when
//! the focused column is Vertical, fullscreen, paused, etc.

use std::ffi::c_void;
use std::sync::{mpsc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::Win32Error;

/// Default tab strip height in pixels at 96 DPI.
pub const DEFAULT_STRIP_HEIGHT: u32 = 28;

/// One tab label rendered in the strip.
///
/// `icon` is the raw HICON handle (passed as `isize` to keep this struct
/// `Send`/`Sync` — actual `HICON` is `!Send`). `None` means render text only.
/// The strip does NOT take ownership of the icon; the daemon must keep it
/// alive for the strip's lifetime, which is trivially true since icons
/// belong to the windows that already outlive the strip's render call.
#[derive(Debug, Clone)]
pub struct TabLabel {
    pub title: String,
    pub icon: Option<isize>,
}

/// Click event sent from the overlay back to the daemon.
///
/// Carries the (monitor, workspace, column) identity captured at the
/// moment `show()` last drew the strip, so the daemon's click handler
/// can route the activation to the column the user actually saw — not
/// whatever happens to be focused at dispatch time. This matters because
/// focus can change between `WM_LBUTTONDOWN` arriving and the main loop
/// processing the resulting `DaemonEvent::TabClicked`.
#[derive(Debug, Clone, Copy)]
pub struct TabClickEvent {
    pub monitor: isize,
    pub workspace_idx: usize,
    pub column_idx: usize,
    /// 0-based tab index that was clicked.
    pub tab_idx: usize,
}

/// Color/style configuration for the tab strip.
#[derive(Debug, Clone, Copy)]
pub struct TabStripColors {
    /// Strip background color as 0xBBGGRR.
    pub bg: u32,
    /// Active-tab background color as 0xBBGGRR.
    pub active_bg: u32,
    /// Active-tab text color as 0xBBGGRR.
    pub active_text: u32,
    /// Inactive-tab text color as 0xBBGGRR.
    pub inactive_text: u32,
    /// Overall strip opacity (0..=255). 255 = fully opaque.
    pub opacity: u8,
}

impl Default for TabStripColors {
    fn default() -> Self {
        Self {
            bg: 0x1F1F1F,           // dark gray
            active_bg: 0x303030,     // slightly lighter for active
            active_text: 0xFFFFFF,   // white
            inactive_text: 0xA0A0A0, // light gray
            opacity: 230,             // ~90% opaque
        }
    }
}

/// State shared between the daemon (caller) and the overlay's WndProc thread.
struct TabStripState {
    /// Last shown tabs (used for re-rendering and hit-test cache).
    tabs: Vec<TabLabel>,
    /// Currently active tab index (highlighted).
    active_idx: usize,
    /// Strip dimensions (window-relative).
    strip_w: i32,
    strip_h: i32,
    /// Strip screen-coord origin (top-left of the layered window).
    /// Used by `hit_test_screen` to convert drop-point screen coords to
    /// strip-relative coords for the drag-onto-tab feature.
    strip_screen_x: i32,
    strip_screen_y: i32,
    /// Whether the strip is currently shown. Hit-tests fail when hidden.
    visible: bool,
    /// Tab hit rects (window-relative, in pixels). Parallel to `tabs`.
    hit_rects: Vec<RECT>,
    /// Colors used on last render.
    colors: TabStripColors,
    /// Identity of the column this strip is currently rendered for.
    /// Captured at click time so the daemon doesn't race focus changes.
    target_monitor: isize,
    target_workspace_idx: usize,
    target_column_idx: usize,
    /// Animation state for sliding the highlight when `active_idx` changes.
    /// `None` while the strip is settled; `Some` during a ~150ms transition.
    transition: Option<HighlightTransition>,
    /// Channel back to the daemon for click events.
    click_tx: Option<mpsc::Sender<TabClickEvent>>,
}

/// In-flight highlight slide state. Used by the `WM_TIMER`-driven
/// animation loop to interpolate the highlight rect between the
/// previous and new active tab positions.
#[derive(Debug, Clone, Copy)]
struct HighlightTransition {
    /// Active idx at the moment the transition started.
    from_idx: usize,
    /// Target active idx the transition is sliding toward.
    to_idx: usize,
    /// Monotonic timestamp captured at transition start.
    started_at: std::time::Instant,
}

/// Animation duration in milliseconds. 150ms is short enough to feel
/// snappy, long enough for the slide to register visually.
const HIGHLIGHT_ANIM_MS: u64 = 150;
/// `WM_TIMER` id used to drive the animation. `WM_USER+N` is fine since
/// the strip's WndProc doesn't use any other timers.
const HIGHLIGHT_ANIM_TIMER_ID: usize = 0xA1;
/// Custom shutdown message — mirrors the `WM_QUIT_HOTKEY_THREAD` /
/// `WM_QUIT_WINEVENT_THREAD` pattern used elsewhere in the crate.
/// `PostMessageW(hwnd, WM_QUIT, ...)` is documented as a no-op (MSDN:
/// "Do not post the WM_QUIT message using PostMessage"), so we use a
/// custom message and break the loop explicitly when it lands.
const WM_QUIT_TAB_STRIP_THREAD: u32 = WM_USER + 4;

static TAB_STRIP_STATE: Mutex<TabStripState> = Mutex::new(TabStripState {
    tabs: Vec::new(),
    active_idx: 0,
    strip_w: 0,
    strip_h: 0,
    strip_screen_x: 0,
    strip_screen_y: 0,
    visible: false,
    hit_rects: Vec::new(),
    colors: TabStripColors {
        bg: 0x1F1F1F,
        active_bg: 0x303030,
        active_text: 0xFFFFFF,
        inactive_text: 0xA0A0A0,
        opacity: 230,
    },
    target_monitor: 0,
    target_workspace_idx: 0,
    target_column_idx: 0,
    transition: None,
    click_tx: None,
});

/// A drop-point hit against the visible tab strip. Used by the drag
/// system to decide "the user dropped onto tab N of column X" instead
/// of "the user dropped onto column X" — letting drag-merge insert at
/// the precise tab slot rather than always appending.
#[derive(Debug, Clone, Copy)]
pub struct TabStripHit {
    pub monitor: isize,
    pub workspace_idx: usize,
    pub column_idx: usize,
    pub tab_idx: usize,
}

/// Hit-test the strip against a screen-coordinate point. Returns `None`
/// if the strip isn't currently visible, or the point isn't over any
/// tab rect. Safe to call from any thread.
pub fn hit_test_screen(screen_x: i32, screen_y: i32) -> Option<TabStripHit> {
    let state = TAB_STRIP_STATE.lock().ok()?;
    if !state.visible {
        return None;
    }
    let rel_x = screen_x - state.strip_screen_x;
    let rel_y = screen_y - state.strip_screen_y;
    if rel_x < 0 || rel_y < 0 || rel_x >= state.strip_w || rel_y >= state.strip_h {
        return None;
    }
    state.hit_rects.iter().enumerate().find_map(|(i, r)| {
        if rel_x >= r.left && rel_x < r.right && rel_y >= r.top && rel_y < r.bottom {
            Some(TabStripHit {
                monitor: state.target_monitor,
                workspace_idx: state.target_workspace_idx,
                column_idx: state.target_column_idx,
                tab_idx: i,
            })
        } else {
            None
        }
    })
}

/// Tab strip overlay handle.
///
/// Created once per daemon lifetime. The daemon calls `show(...)` whenever
/// the focused workspace's focused column is Tabbed (every `apply_layout`
/// pass and every animation frame, mirroring `BorderFrame`'s
/// continuously-repositioned lifecycle).
pub struct TabStripOverlay {
    hwnd: HWND,
    /// Win32 thread id of the UI thread — kept for the PostThreadMessageW
    /// fallback in Drop, used when PostMessageW on the hwnd fails (e.g.,
    /// if the window has already been destroyed by an external path).
    thread_id: u32,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl TabStripOverlay {
    /// Create the tab strip overlay on a background thread.
    /// `click_tx` receives `TabClickEvent` whenever the user clicks a tab.
    pub fn new(click_tx: mpsc::Sender<TabClickEvent>) -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("TabStripOverlay::new spawns a layered DWM window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            // Stash the channel up front so the WndProc can use it on
            // WM_LBUTTONDOWN before any show() call has run.
            if let Ok(mut state) = TAB_STRIP_STATE.lock() {
                state.click_tx = Some(click_tx);
            }

            let (tx, rx) = mpsc::channel::<Result<(isize, u32), Win32Error>>();
            let thread = std::thread::Builder::new()
                .name("tab-strip".into())
                .spawn(move || unsafe {
                    let thread_id = windows::Win32::System::Threading::GetCurrentThreadId();
                    let class_name: Vec<u16> = "LeopardWMTabStrip\0".encode_utf16().collect();
                    // Load the standard arrow cursor. Without an
                    // explicit `hCursor`, Windows falls back to whatever
                    // cursor the spawning thread last used — which on
                    // some systems is the wait/loading cursor (since
                    // our background thread does no cursor management).
                    // Setting IDC_ARROW makes the strip always show the
                    // normal pointer regardless of thread state.
                    let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(tab_strip_wnd_proc),
                        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                        hCursor: arrow,
                        ..Default::default()
                    };
                    RegisterClassW(&wc);

                    // No WS_EX_TRANSPARENT: clicks must land on this window.
                    let ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;

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
                            let _ = tx.send(Ok((h.0 as isize, thread_id)));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                if msg.message == WM_QUIT_TAB_STRIP_THREAD {
                                    break;
                                }
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
                                "TabStripOverlay: {}",
                                e
                            ))));
                        }
                    }
                })
                .map_err(|e| {
                    Win32Error::HookInstallFailed(format!("TabStripOverlay thread: {}", e))
                })?;

            let (hwnd_raw, thread_id) = match rx.recv() {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Win32Error::HookInstallFailed(
                        "TabStripOverlay init failed".into(),
                    ))
                }
            };

            Ok(Self {
                hwnd: HWND(hwnd_raw as *mut c_void),
                thread_id,
                _thread: Some(thread),
            })
        }
    }

    /// Show the tab strip above the given target rect (the column rect).
    /// The strip is rendered at `target_rect.y - strip_h`, full column
    /// width, at `strip_h` pixels tall (DPI-scaled by the caller if needed).
    ///
    /// The `(target_monitor, target_workspace_idx, target_column_idx)`
    /// triplet is captured for click events — the click handler routes
    /// the activation to this column even if focus changed between the
    /// strip being drawn and the user clicking it.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &self,
        target_rect: leopardwm_core_layout::Rect,
        tabs: Vec<TabLabel>,
        active_idx: usize,
        colors: TabStripColors,
        strip_height: u32,
        bottom_gap_px: u32,
        target_monitor: isize,
        target_workspace_idx: usize,
        target_column_idx: usize,
    ) {
        if tabs.is_empty() {
            self.hide();
            return;
        }
        let strip_w = target_rect.width.max(1);
        let strip_h = strip_height as i32;
        let strip_x = target_rect.x;
        // Lift the strip by `bottom_gap_px` so its bottom edge is that many
        // pixels above the active tab — gives a visible breathing room
        // between the strip and the window content. The daemon adds the
        // same value to `tab_strip_reserve_px` so the column geometry
        // accommodates the gap rather than overlapping content.
        let strip_y = target_rect.y - strip_h - bottom_gap_px as i32;

        // Compute hit rects (window-relative).
        let n = tabs.len();
        let tab_w = strip_w / n as i32;
        let mut hit_rects = Vec::with_capacity(n);
        for i in 0..n {
            let left = (i as i32) * tab_w;
            // Last tab absorbs rounding remainder so hit-test covers the
            // full strip width exactly.
            let right = if i == n - 1 { strip_w } else { left + tab_w };
            hit_rects.push(RECT {
                left,
                top: 0,
                right,
                bottom: strip_h,
            });
        }

        // Detect active-tab change to start a highlight slide animation.
        // The window swap itself is instant; only the strip's highlight
        // bar slides between tabs.
        let new_active = active_idx.min(n.saturating_sub(1));
        let prev_active = TAB_STRIP_STATE
            .lock()
            .ok()
            .map(|s| s.active_idx)
            .unwrap_or(new_active);
        let prev_visible = TAB_STRIP_STATE
            .lock()
            .ok()
            .map(|s| s.visible)
            .unwrap_or(false);
        let transition = if prev_visible && prev_active != new_active {
            Some(HighlightTransition {
                from_idx: prev_active,
                to_idx: new_active,
                started_at: std::time::Instant::now(),
            })
        } else {
            None
        };

        // Update shared state (used by WndProc for hit testing).
        if let Ok(mut state) = TAB_STRIP_STATE.lock() {
            state.tabs = tabs.clone();
            state.active_idx = new_active;
            state.strip_w = strip_w;
            state.strip_h = strip_h;
            state.strip_screen_x = strip_x;
            state.strip_screen_y = strip_y;
            state.visible = true;
            state.hit_rects = hit_rects;
            state.colors = colors;
            state.target_monitor = target_monitor;
            state.target_workspace_idx = target_workspace_idx;
            state.target_column_idx = target_column_idx;
            state.transition = transition;
        }

        // Kick off animation timer if we started a transition. The WndProc
        // ticks every ~16ms, re-renders the strip with an interpolated
        // highlight, and kills the timer when the slide completes.
        if transition.is_some() {
            unsafe {
                let _ = SetTimer(Some(self.hwnd), HIGHLIGHT_ANIM_TIMER_ID, 16, None);
            }
        }

        render_strip_now(self.hwnd, strip_x, strip_y, strip_w, strip_h);
    }

    /// Hide the tab strip overlay.
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        if let Ok(mut state) = TAB_STRIP_STATE.lock() {
            state.visible = false;
        }
    }

    /// Re-pin the strip to the top of the z-order without re-rendering.
    ///
    /// `show()` already pins on every paint, but during an OS-driven drag
    /// the dragged window gets continuously raised to `HWND_TOP` between
    /// our repaints, pushing the strip behind it. The drag handler calls
    /// this on every mouse move so the strip stays visible without
    /// triggering a (much more expensive) full re-render per move.
    ///
    /// No-op when the strip isn't currently visible — we don't want to
    /// resurrect a hidden strip during e.g. a drag over a Vertical column.
    pub fn raise(&self) {
        let visible = TAB_STRIP_STATE
            .lock()
            .ok()
            .map(|s| s.visible)
            .unwrap_or(false);
        if !visible {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

}

/// Coverage of one pixel against a rounded-rect shape, using 4x4
/// sub-pixel supersampling. Used by `aa_fill_rounded` to produce smooth
/// curve edges that GDI's aliased `RoundRect` can't deliver.
///
/// Returns 1.0 for pixels fully inside the rectangle (away from corners),
/// 0.0 for pixels fully outside the rounded curve, and a fractional value
/// for pixels straddling a corner edge.
fn pixel_coverage_rounded(
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
) -> f32 {
    if x < left || x >= right || y < top || y >= bottom {
        return 0.0;
    }
    if radius <= 0 {
        return 1.0;
    }
    let in_left = x < left + radius;
    let in_right = x >= right - radius;
    let in_top = y < top + radius;
    let in_bottom = y >= bottom - radius;

    if !((in_left || in_right) && (in_top || in_bottom)) {
        return 1.0;
    }

    // Corner-center coords are the inner anchor of the radius — the
    // pixel grid points where the curve transitions from straight edge
    // to arc. Sub-pixel samples within this pixel get tested against
    // `dx² + dy² <= r²`.
    let cx = if in_left { (left + radius) as f32 } else { (right - radius) as f32 };
    let cy = if in_top { (top + radius) as f32 } else { (bottom - radius) as f32 };
    let r2 = (radius * radius) as f32;

    const SAMPLES: i32 = 4;
    let mut covered = 0;
    for sy in 0..SAMPLES {
        for sx in 0..SAMPLES {
            let fx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - cx;
            let fy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - cy;
            // Outside corner-radius arc only if the sample is in the
            // outer quadrant; inside the inner straight region we
            // always count as covered (handled by SAMPLES iteration
            // arriving at points where dx/dy are negative).
            let test_x = if in_left { -fx } else { fx };
            let test_y = if in_top { -fy } else { fy };
            let dx = test_x.max(0.0);
            let dy = test_y.max(0.0);
            if dx * dx + dy * dy <= r2 {
                covered += 1;
            }
        }
    }
    covered as f32 / (SAMPLES * SAMPLES) as f32
}

/// Source-over composite an AA rounded-rect fill into a 32-bpp BGRA
/// pixel buffer. RGB is written non-premultiplied; the caller is
/// responsible for premultiplying at the end before
/// `UpdateLayeredWindow` (since `AC_SRC_ALPHA` expects premultiplied
/// data).
///
/// `color_bgra` is packed as `0xBBGGRR` matching `COLORREF`.
#[allow(clippy::too_many_arguments)]
fn aa_fill_rounded(
    pixels: &mut [u8],
    bmp_w: i32,
    bmp_h: i32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    color_bgra: u32,
) {
    let radius = radius
        .min((right - left) / 2)
        .min((bottom - top) / 2)
        .max(0);
    let src_b = (color_bgra & 0xFF) as f32;
    let src_g = ((color_bgra >> 8) & 0xFF) as f32;
    let src_r = ((color_bgra >> 16) & 0xFF) as f32;

    let y_start = top.max(0);
    let y_end = bottom.min(bmp_h);
    let x_start = left.max(0);
    let x_end = right.min(bmp_w);

    for y in y_start..y_end {
        for x in x_start..x_end {
            let coverage = pixel_coverage_rounded(x, y, left, top, right, bottom, radius);
            if coverage <= 0.0 {
                continue;
            }
            let idx = ((y * bmp_w + x) * 4) as usize;
            let src_a = coverage;
            let dst_a = pixels[idx + 3] as f32 / 255.0;
            // Source-over with non-premultiplied src and dst:
            //   out_a = src_a + dst_a * (1 - src_a)
            //   out_c = (src_c * src_a + dst_c * dst_a * (1 - src_a)) / out_a
            let inv_src = 1.0 - src_a;
            let out_a = src_a + dst_a * inv_src;
            if out_a <= 0.0 {
                continue;
            }
            let dst_b = pixels[idx] as f32;
            let dst_g = pixels[idx + 1] as f32;
            let dst_r = pixels[idx + 2] as f32;
            let out_b = (src_b * src_a + dst_b * dst_a * inv_src) / out_a;
            let out_g = (src_g * src_a + dst_g * dst_a * inv_src) / out_a;
            let out_r = (src_r * src_a + dst_r * dst_a * inv_src) / out_a;
            pixels[idx] = out_b.round().clamp(0.0, 255.0) as u8;
            pixels[idx + 1] = out_g.round().clamp(0.0, 255.0) as u8;
            pixels[idx + 2] = out_r.round().clamp(0.0, 255.0) as u8;
            pixels[idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Render the tab strip bitmap and update the layered window. Reads
/// tabs/active_idx/colors/transition from `TAB_STRIP_STATE`, computes
/// the interpolated highlight rect, and pushes the result via
/// `UpdateLayeredWindow`. Called both from `TabStripOverlay::show` and
/// from the WndProc's `WM_TIMER` handler during a highlight slide.
fn render_strip_now(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (tabs, active_idx, colors, transition) = match TAB_STRIP_STATE.lock() {
        Ok(s) => (
            s.tabs.clone(),
            s.active_idx,
            s.colors,
            s.transition,
        ),
        Err(_) => return,
    };
    if tabs.is_empty() {
        return;
    }
    render_strip_inner(hwnd, x, y, w, h, &tabs, active_idx, colors, transition);
}

/// Internal helper that does the actual GDI rendering. Separated from
/// `render_strip_now` so callers (`show()`) that already hold the data
/// don't pay an extra mutex acquisition.
#[allow(clippy::too_many_arguments)]
fn render_strip_inner(
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    tabs: &[TabLabel],
    active_idx: usize,
    colors: TabStripColors,
    transition: Option<HighlightTransition>,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
            // 32-bit top-down DIB. We use per-pixel alpha (AC_SRC_ALPHA)
            // for the layered-window blend so the strip's outer corners can
            // round into genuine transparency — the corners' alpha byte
            // stays at 0 and DWM composites the window/desktop through.
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
            let Ok(hbitmap) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                return;
            };

            // Fill bitmap on a memory DC so we can draw text into it.
            let hdc_screen = GetDC(None);
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let old_bm = SelectObject(hdc_mem, hbitmap.into());

            // Zero the bitmap. `CreateDIBSection` doesn't guarantee
            // zero-init, and our AA fill below SKIPS pixels with zero
            // coverage rather than writing zeros — so any heap garbage
            // in the corners would composite as visible junk without
            // this explicit clear.
            let pixels =
                std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            pixels.fill(0);

            // Strip outer corner radius — tuned to match Win11's
            // standard window corner radius (~8 px at 100% DPI).
            // `h * 2 / 7` gives 8 at h=28 (default strip height) and
            // scales linearly with the strip's DPI-scaled height, so a
            // 200% display gets a 16px radius — same proportion as
            // Win11's own scaled corners. Visually the strip sits as a
            // sibling to the active window rather than a different
            // design language.
            let strip_corner = (h * 2 / 7).max(6);

            // AA fill the strip background. GDI's RoundRect is aliased
            // and produces stair-stepped corner pixels; we rasterize
            // manually with 4x4 sub-pixel sampling so the curve edges
            // composite smoothly into the surrounding transparency.
            aa_fill_rounded(pixels, w, h, 0, 0, w, h, strip_corner, colors.bg);

            // Active tab background highlight. When a transition is
            // in-flight, the highlight slides from the previous tab's
            // x to the new tab's x over `HIGHLIGHT_ANIM_MS`, using cubic
            // ease-out so it lands snappy.
            //
            // The highlight is drawn as a *rounded* rect inset from the
            // tab cell: horizontal inset creates a visual gap between
            // adjacent tabs; vertical inset keeps the rounded shape from
            // butting against the strip's top/bottom edges. Corner radius
            // and insets scale with strip height so the look stays
            // proportional under DPI changes.
            let n = tabs.len() as i32;
            let tab_w = w / n;
            let ai = active_idx.min((n as usize).saturating_sub(1)) as i32;
            let active_x_settled = ai * tab_w;
            let (active_left, active_right) = if let Some(t) = transition {
                let from = t.from_idx.min((n as usize).saturating_sub(1)) as i32;
                let to = t.to_idx.min((n as usize).saturating_sub(1)) as i32;
                let elapsed = t.started_at.elapsed().as_millis() as f64;
                let dur = HIGHLIGHT_ANIM_MS as f64;
                let raw_t = (elapsed / dur).clamp(0.0, 1.0);
                // easeOutCubic: 1 - (1-t)^3
                let eased = 1.0 - (1.0 - raw_t).powi(3);
                let from_x = (from * tab_w) as f64;
                let to_x = (to * tab_w) as f64;
                let interp_x = (from_x + (to_x - from_x) * eased).round() as i32;
                (interp_x, interp_x + tab_w)
            } else {
                let right = if ai == n - 1 { w } else { active_x_settled + tab_w };
                (active_x_settled, right)
            };

            // Inset and corner-radius derived from strip height — keeps
            // the look proportional across DPIs without separate pixel
            // constants. Horizontal inset gives adjacent tabs room to
            // read as separated pills. The active-tab corner radius
            // sits 2 px tighter than the strip's outer radius so the
            // inner curve is consistently smaller (outer > inner) —
            // matches the visual hierarchy Win11 uses for nested
            // rounded shapes (e.g. context menus inside flyouts).
            let h_inset = (h / 6).max(4);
            let v_inset = (h / 8).max(3);
            let corner = strip_corner.saturating_sub(2).max(4);
            let hr_left = active_left + h_inset;
            let hr_right = (active_right - h_inset).max(hr_left + 1);
            let hr_top = v_inset;
            let hr_bottom = (h - v_inset).max(hr_top + 1);

            // AA-fill the active highlight on top of the strip bg —
            // same supersampled rasterizer, source-over composites the
            // highlight over the bg so the highlight's rounded corners
            // are smooth.
            aa_fill_rounded(
                pixels,
                w,
                h,
                hr_left,
                hr_top,
                hr_right,
                hr_bottom,
                corner,
                colors.active_bg,
            );

            // Font: Segoe UI 9pt = ~12 px at 96 DPI. height < 0 means
            // character height (font-cell ascender), > 0 means cell height.
            let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
            let hfont = CreateFontW(
                -((h as f32 * 0.5) as i32).max(-12), // ~half strip height, capped
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                FONT_PIPELINE_DEFAULT_PITCH_AND_FAMILY,
                windows::core::PCWSTR(face.as_ptr()),
            );
            let old_font = SelectObject(hdc_mem, hfont.into());
            SetBkMode(hdc_mem, TRANSPARENT);

            for (i, tab) in tabs.iter().enumerate() {
                let i_i32 = i as i32;
                let left = i_i32 * tab_w;
                let right = if i_i32 == n - 1 { w } else { left + tab_w };
                let tab_rect = RECT {
                    left,
                    top: 0,
                    right,
                    bottom: h,
                };
                let color = if i == active_idx {
                    colors.active_text
                } else {
                    colors.inactive_text
                };
                SetTextColor(hdc_mem, windows::Win32::Foundation::COLORREF(color));

                // Icon geometry:
                //   * `icon_size`: shrunk so the icon doesn't kiss the
                //     highlight's vertical edges — gives breathing room
                //     so the icon reads as content sitting *inside* the
                //     button rather than filling it.
                //   * `icon_top`: arithmetic vertical centerline so the
                //     icon's center sits at the strip's center,
                //     regardless of the active-highlight's v_inset.
                //   * `icon_left`: `h_inset` puts us at the highlight's
                //     left edge; `icon_inside_pad` then nudges us
                //     further right so the icon has visible padding
                //     INSIDE the highlight (otherwise it sits flush
                //     against the highlight's left edge).
                //
                // DrawIconEx writes its own premultiplied alpha; the
                // post-render fixup only promotes alpha=0 pixels with
                // non-zero RGB to 0xFF, so the icon's anti-aliased
                // edges (alpha != 0) are left untouched.
                let icon_inside_pad = (h / 6).max(4);
                let icon_size = (h - 2 * v_inset - 4).max(8);
                let icon_top = (h - icon_size) / 2;
                let icon_left = tab_rect.left + h_inset + icon_inside_pad;
                let mut text_left = tab_rect.left + h_inset + icon_inside_pad;
                if let Some(icon_handle) = tab.icon {
                    if icon_size > 0 && icon_handle != 0 {
                        let hicon = windows::Win32::UI::WindowsAndMessaging::HICON(
                            icon_handle as *mut c_void,
                        );
                        let _ = windows::Win32::UI::WindowsAndMessaging::DrawIconEx(
                            hdc_mem,
                            icon_left,
                            icon_top,
                            hicon,
                            icon_size,
                            icon_size,
                            0,
                            None,
                            windows::Win32::UI::WindowsAndMessaging::DI_NORMAL,
                        );
                        text_left = icon_left + icon_size + icon_inside_pad;
                    }
                }

                // Convert title to UTF-16 and draw with horizontal padding.
                let mut wide: Vec<u16> = tab.title.encode_utf16().collect();
                if wide.is_empty() {
                    wide.push(0);
                }

                // Use DrawTextW with DT_END_ELLIPSIS so over-long titles ellipsize
                // automatically rather than overflow into the next tab.
                // Right padding mirrors the icon's inside-padding so the
                // text has matching breathing room from the highlight's
                // right edge — keeps the tab visually symmetric.
                let mut padded = tab_rect;
                padded.left = text_left;
                padded.right = tab_rect
                    .right
                    .saturating_sub(h_inset + icon_inside_pad);
                let _ = DrawTextW(
                    hdc_mem,
                    &mut wide,
                    &mut padded,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }

            SelectObject(hdc_mem, old_font);
            let _ = DeleteObject(hfont.into());

            // Alpha-channel fixup + premultiplication for per-pixel
            // `AC_SRC_ALPHA` compositing.
            //
            // After this point the buffer holds a mix of:
            //   * AA-rasterized strip/highlight pixels with correct
            //     non-premultiplied RGB and the right alpha already set.
            //   * GDI-rendered text pixels (RGB blended over the
            //     underlying bg color but alpha still at whatever the
            //     AA fill left it).
            //   * GDI-rendered icon pixels (DrawIconEx writes BGRA
            //     including its own alpha; treated as premultiplied
            //     output by the OS).
            //
            // Two passes:
            //   1. For any pixel where alpha is still 0 but RGB is
            //      non-zero, GDI wrote color without touching alpha —
            //      promote alpha to 0xFF so the pixel becomes
            //      hit-testable and opaque in the composite.
            //   2. Premultiply RGB by alpha so the buffer is in the
            //      premultiplied form `AC_SRC_ALPHA` expects.
            //
            // Bytes are BGRA little-endian → alpha is byte 3 of each
            // 4-byte pixel.
            let mut i = 0usize;
            while i < pixels.len() {
                let mut a = pixels[i + 3];
                if a == 0
                    && (pixels[i] != 0 || pixels[i + 1] != 0 || pixels[i + 2] != 0)
                {
                    a = 0xFF;
                    pixels[i + 3] = a;
                }
                if a < 0xFF {
                    // Premultiply: stored = color * alpha / 255. At a=0
                    // RGB stays at whatever it was, but the composite
                    // ignores those bytes since the layer is transparent.
                    let af = a as u32;
                    pixels[i] = ((pixels[i] as u32 * af) / 255) as u8;
                    pixels[i + 1] = ((pixels[i + 1] as u32 * af) / 255) as u8;
                    pixels[i + 2] = ((pixels[i + 2] as u32 * af) / 255) as u8;
                }
                i += 4;
            }

            // Push to the layered window with per-pixel-alpha blending.
            // `AC_SRC_ALPHA` requires the bitmap to be premultiplied; we
            // satisfy this because all of our explicitly-drawn pixels are
            // fully opaque (alpha=0xFF after the fixup above, and
            // premultiplication is a no-op at full alpha). Outer-corner
            // pixels are (0,0,0,0), the valid premultiplied representation
            // of fully transparent.
            //
            // `SourceConstantAlpha` still applies as a uniform multiplier
            // on top of per-pixel alpha, so the configured strip opacity
            // setting keeps working — the strip stays translucent over
            // the underlying window/desktop.
            let pt_dst = POINT { x, y };
            let sz = SIZE { cx: w, cy: h };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: colors.opacity,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                hwnd,
                Some(hdc_screen),
                Some(&pt_dst),
                Some(&sz),
                Some(hdc_mem),
                Some(&pt_src),
                windows::Win32::Foundation::COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(hdc_mem, old_bm);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);
            let _ = DeleteObject(hbitmap.into());

            // Pin the strip to the top of the z-order on every paint.
            // Without this, OS-driven SetWindowPos calls during a drag
            // (the dragged window gets raised to HWND_TOP) push the
            // strip behind the active window, making it appear to vanish
            // mid-drag and only re-appear on the next focus change. The
            // `SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE` combo
            // mirrors `BorderFrame`'s pattern: stay on top, stay visible,
            // don't steal focus, keep current size.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
}

impl Drop for TabStripOverlay {
    fn drop(&mut self) {
        // Try PostMessageW first (the hwnd is still alive in the common
        // case); fall back to PostThreadMessageW if the window has been
        // destroyed externally. Mirrors the pattern in `hotkeys.rs`.
        let posted_via_window = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd),
                WM_QUIT_TAB_STRIP_THREAD,
                WPARAM(0),
                LPARAM(0),
            )
            .is_ok()
        };
        if !posted_via_window {
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread_id,
                    WM_QUIT_TAB_STRIP_THREAD,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
        if let Ok(mut state) = TAB_STRIP_STATE.lock() {
            state.click_tx = None;
            state.hit_rects.clear();
            state.tabs.clear();
        }
    }
}

/// WndProc: handles WM_LBUTTONDOWN to translate clicks into TabClickEvents.
/// Layered windows with `UpdateLayeredWindow` skip WM_PAINT entirely; we
/// only care about input here.
unsafe extern "system" fn tab_strip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Defensive: prevent the strip from stealing activation focus when
    // clicked. Win32 may try to activate the popup if this isn't said
    // explicitly, even with WS_EX_NOACTIVATE in some edge cases.
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // Highlight slide animation tick. Re-renders the strip with the
    // interpolated highlight position; clears the transition (and kills
    // the timer) when the slide reaches its target.
    if msg == WM_TIMER && wparam.0 == HIGHLIGHT_ANIM_TIMER_ID {
        let (strip_x, strip_y, strip_w, strip_h, finished) = {
            let mut state = match TAB_STRIP_STATE.lock() {
                Ok(s) => s,
                Err(_) => return LRESULT(0),
            };
            let finished = state.transition.is_none_or(|t| {
                t.started_at.elapsed().as_millis() as u64 >= HIGHLIGHT_ANIM_MS
            });
            if finished {
                state.transition = None;
            }
            (
                state.strip_screen_x,
                state.strip_screen_y,
                state.strip_w,
                state.strip_h,
                finished,
            )
        };
        render_strip_now(hwnd, strip_x, strip_y, strip_w, strip_h);
        if finished {
            let _ = KillTimer(Some(hwnd), HIGHLIGHT_ANIM_TIMER_ID);
        }
        return LRESULT(0);
    }
    if msg == WM_LBUTTONDOWN {
        // LPARAM low word = x, high word = y, both window-relative.
        let raw = lparam.0 as u32;
        let click_x = (raw & 0xFFFF) as i16 as i32;
        let click_y = ((raw >> 16) & 0xFFFF) as i16 as i32;
        if let Ok(state) = TAB_STRIP_STATE.lock() {
            for (idx, rect) in state.hit_rects.iter().enumerate() {
                if click_x >= rect.left
                    && click_x < rect.right
                    && click_y >= rect.top
                    && click_y < rect.bottom
                {
                    if let Some(tx) = &state.click_tx {
                        // Don't block the WndProc on a dead receiver — the
                        // strip should keep working even if the daemon's
                        // click drainer is gone.
                        let _ = tx.send(TabClickEvent {
                            monitor: state.target_monitor,
                            workspace_idx: state.target_workspace_idx,
                            column_idx: state.target_column_idx,
                            tab_idx: idx,
                        });
                    }
                    return LRESULT(0);
                }
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// Win32 API constant for default-pitch+family font lookup. The `windows`
// crate exposes the components but not the combined OR'd value as a named
// constant in our crate's import surface, so we name it locally.
// `CreateFontW` expects u32 for `ipitchandfamily`.
const FONT_PIPELINE_DEFAULT_PITCH_AND_FAMILY: u32 = 0; // DEFAULT_PITCH | FF_DONTCARE
