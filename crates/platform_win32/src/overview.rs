//! Overview overlay: a fullscreen, interactive "map" of the focused
//! monitor's non-empty workspaces (Win+Tab-like), one row per workspace
//! with placeholder cards (title bar + icon body) for each visible window.
//!
//! Mirrors `TabStripOverlay`'s structure (background message-pump thread,
//! mpsc init handshake, daemon-facing handle) but with two differences:
//! 1. The backdrop is the DESKTOP BLURRED through the window: at creation
//!    the overlay enables the undocumented `SetWindowCompositionAttribute`
//!    accent (acrylic, then plain blur). The window stays non-layered and
//!    paints through WM_PAINT: each frame is rendered into a 32bpp DIB —
//!    shapes are SDF-anti-aliased straight into the bits with per-region
//!    alpha (backdrop translucent, cards opaque), GDI draws only text and
//!    icons — and the premultiplied result is `BitBlt`-ed to the window
//!    DC. DWM composites it over the blurred backdrop using the surface
//!    alpha.
//!    When the accent call is unavailable or fails, the overlay falls
//!    back to the previous per-pixel-alpha layered pipeline
//!    (`UpdateLayeredWindow`, alpha-dim instead of blur).
//! 2. It takes the foreground when shown: the overlay handles keyboard
//!    (arrows/Enter/Esc/digits) against its local model copy.
//!
//! The overlay is dumb: the daemon sends a complete [`OverviewModel`]
//! (client-coordinate rects); the overlay only draws and hit-tests it,
//! reporting user intent back through [`OverviewEvent`]s.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{mpsc, LazyLock, Mutex, MutexGuard};

use leopardwm_core_layout::Rect;
use windows::core::BOOL;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_LEFT,
    VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::border::{clamp, rounded_rect_sdf};
use crate::thumbnail::{self, ThumbnailHandle};
use crate::Win32Error;

/// User intent reported by the overlay back to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewEvent {
    /// Activate (focus + switch to) the given window.
    ActivateWindow(u64),
    /// Switch to the given workspace index (0-based).
    SwitchWorkspace(usize),
    /// Close the given window; the overview stays open.
    CloseWindow(u64),
    /// Dismiss the overview without acting (Esc, backdrop click, focus loss).
    Dismissed,
}

/// One placeholder window card. Rects are overlay client coordinates.
#[derive(Debug, Clone)]
pub struct OverviewCard {
    pub window_id: u64,
    pub title: String,
    /// Raw HICON as `isize` (same convention as `TabLabel::icon`): the
    /// overlay does not own the icon; the source window outlives the draw.
    pub icon: Option<isize>,
    pub rect: Rect,
    /// `Some(n)` when the window is the visible tab of a tabbed column
    /// holding `n > 1` windows; the overlay draws a small count pill.
    pub tab_count: Option<usize>,
    pub selected: bool,
    /// Whether the overlay may register a live DWM thumbnail for this
    /// card's body. The daemon clears it when the window already has a
    /// thumbnail registration (ghost animation) or config says placeholder.
    pub live: bool,
}

/// One workspace row: a rounded panel with a label strip across the top
/// and the miniaturized column strip below. Rects are overlay client
/// coordinates.
#[derive(Debug, Clone)]
pub struct OverviewRow {
    pub workspace_index: usize,
    pub label: String,
    pub is_active: bool,
    pub panel: Rect,
    /// Label row across the panel's top (workspace number + name).
    pub label_strip: Rect,
    /// The portion of the miniaturized strip the workspace's real viewport
    /// currently shows (scroll position marker). Zero-sized when empty.
    pub viewport: Rect,
    pub cards: Vec<OverviewCard>,
}

/// Complete display model for one overview frame.
#[derive(Debug, Clone)]
pub struct OverviewModel {
    /// Full client-area rect (the blurred/dimmed backdrop).
    pub backdrop: Rect,
    /// Selection/active accent in COLORREF byte order (0x00BBGGRR); the
    /// daemon fills it from the configured focus-border color.
    pub accent_bgr: u32,
    /// Stroke width for the accent chrome (selected-card ring, active
    /// panel ring, viewport ring); the daemon fills it from the
    /// configured focus-border width.
    pub accent_width: u32,
    pub rows: Vec<OverviewRow>,
}

/// Win11 default accent #0078D4 in BGR, used when no config color parses.
pub const DEFAULT_ACCENT_BGR: u32 = 0x00D4_7800;

/// Default accent stroke width (matches the default focus-border width).
pub const DEFAULT_ACCENT_WIDTH: u32 = 2;

/// Breathing room between the viewport ring and the viewport-region
/// content it encircles; the daemon inflates the ring rect by this and
/// reserves matching body insets so the ring never clips the panel.
pub const VIEWPORT_RING_PAD: i32 = 6;

/// Padding between a row panel's edge (or label strip) and the viewport
/// ring. The daemon adds VIEWPORT_RING_PAD to it for the body inset; the
/// renderer folds both into [`PANEL_RADIUS`] so the rounding constants
/// stay in sync with the layout.
pub const PANEL_INNER_PAD: i32 = 10;

impl Default for OverviewModel {
    fn default() -> Self {
        Self {
            backdrop: Rect::new(0, 0, 0, 0),
            accent_bgr: DEFAULT_ACCENT_BGR,
            accent_width: DEFAULT_ACCENT_WIDTH,
            rows: Vec::new(),
        }
    }
}

// Palette (COLORREF byte order: 0x00BBGGRR).
const BACKDROP_BG: u32 = 0x001A1A1A;
const PANEL_BG: u32 = 0x00262626;
const PANEL_ACTIVE_BG: u32 = 0x00303030;
/// Label strip across each panel's top: slightly darker than the panel
/// body, mirroring the card title-bar treatment.
const LABEL_STRIP_BG: u32 = 0x001E1E1E;
const LABEL_STRIP_ACTIVE_BG: u32 = 0x00262626;
const PANEL_BORDER: u32 = 0x003A3A3A;
const CARD_BG: u32 = 0x003C3C3C;
const CARD_HOVER_BG: u32 = 0x004A4A4A;
const CARD_TITLE_BG: u32 = 0x002E2E2E;
/// Hovered title strip: DWM composites live thumbnails over the card
/// BODY, so the hover highlight must also show on the (uncovered) strip.
const CARD_TITLE_HOVER_BG: u32 = 0x003C3C3C;
const CARD_BORDER: u32 = 0x00585858;
const TEXT_PRIMARY: u32 = 0x00E6E6E6;
const TEXT_SECONDARY: u32 = 0x00A0A0A0;
const PILL_TEXT: u32 = 0x00FFFFFF;
/// Viewport ("you are here") ring: neutral glassy highlight — white at
/// ~150/255 alpha precomposed over the panel bg — bright enough to read
/// clearly against the panel without competing with the accent rings.
const VIEWPORT_RING_COLOR: u32 = 0x00A8A8A8;

// Geometry (Win+Tab-like chrome).
const CARD_RADIUS: i32 = 8;
/// Panel rounding matches the viewport ring's radius (card radius + ring
/// pad): full concentric (radius + body inset) read as too round, while
/// the bare card radius read as too square on the larger panel.
const PANEL_RADIUS: i32 = CARD_RADIUS + VIEWPORT_RING_PAD;
/// Title bar strip across the top of each card (icon + window title).
const CARD_TITLE_H: i32 = 26;
/// App icon size inside the card title bar.
const TITLE_ICON: i32 = 16;
/// Minimum card size to get the title-bar/body split; smaller cards use
/// the compact one-row rendering.
const TITLED_MIN_H: i32 = 56;
const TITLED_MIN_W: i32 = 72;
/// Accent-outline breathing room: the selected-card ring and the
/// active-panel ring are drawn this many px OUTSIDE their rect,
/// Win+Tab-style. Stroke width comes from the model's `accent_width`.
/// Public so the daemon's row layout reserves clearance for the
/// active-panel ring.
pub const SELECT_PAD: i32 = 4;

/// Inset between a card's body edges and its live-thumbnail dest rect,
/// keeping a sliver of the painted body visible as the thumbnail's frame.
const THUMB_INSET: i32 = 1;

// Per-region alpha. Cards are fully opaque; the backdrop and row panels
// let the (blurred) desktop show through.
/// Backdrop dim in the no-blur layered fallback.
const BACKDROP_ALPHA: u8 = 170;
/// Dark tint stamped over plain (non-acrylic) accent blur, which ignores
/// the policy's gradient color.
const BLUR_TINT_ALPHA: u8 = 150;
const PANEL_ALPHA: u8 = 210;
const OPAQUE: u8 = 255;

/// Acrylic tint (AABBGGRR) passed in the accent policy's gradient color.
const ACRYLIC_TINT: u32 = 0xCC1A_1A1A;

/// How the backdrop translucency is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackdropMode {
    /// `ACCENT_ENABLE_ACRYLICBLURBEHIND`: blur + noise + tint from the
    /// accent policy; backdrop pixels are fully transparent.
    Acrylic,
    /// `ACCENT_ENABLE_BLURBEHIND`: plain blur; the dark tint is stamped
    /// into the frame's own alpha plane.
    Blur,
    /// Accent unavailable: per-pixel-alpha layered window, alpha dim only.
    AlphaDim,
}

/// Custom shutdown message — mirrors `WM_QUIT_TAB_STRIP_THREAD`:
/// `PostMessageW(hwnd, WM_QUIT, ...)` is documented as a no-op, so a
/// custom message breaks the loop explicitly.
const WM_QUIT_OVERVIEW_THREAD: u32 = WM_USER + 5;

/// State shared between the daemon (caller) and the overlay's WndProc.
/// Single-instance: the daemon owns at most one `OverviewOverlay`.
struct OverviewState {
    model: OverviewModel,
    visible: bool,
    /// Screen rect the overlay occupies; rendering needs the destination
    /// position/size on every re-render.
    window_rect: Rect,
    /// `(row_idx, card_idx)` under the mouse, if any.
    hovered: Option<(usize, usize)>,
    /// `(row_idx, card_idx)` of the keyboard selection, if any.
    selected: Option<(usize, usize)>,
    /// Whether `TrackMouseEvent` is armed for WM_MOUSELEAVE.
    mouse_tracking_armed: bool,
    backdrop: BackdropMode,
    event_tx: Option<mpsc::Sender<OverviewEvent>>,
    /// Live DWM thumbnails composited over card bodies, keyed by source
    /// window id. RAII: dropping an entry unregisters its thumbnail.
    /// Cleared on hide and overlay drop; re-diffed on every model push.
    thumbnails: HashMap<u64, ThumbnailHandle>,
}

static STATE: LazyLock<Mutex<OverviewState>> = LazyLock::new(|| {
    Mutex::new(OverviewState {
        model: OverviewModel::default(),
        visible: false,
        window_rect: Rect::new(0, 0, 0, 0),
        hovered: None,
        selected: None,
        mouse_tracking_armed: false,
        backdrop: BackdropMode::AlphaDim,
        event_tx: None,
        thumbnails: HashMap::new(),
    })
});

fn state() -> MutexGuard<'static, OverviewState> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn send_event(event: OverviewEvent) {
    let tx = state().event_tx.clone();
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

fn rect_contains(r: &Rect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Derive the initial keyboard selection from the model's `selected` flags.
fn selection_from_model(model: &OverviewModel) -> Option<(usize, usize)> {
    for (ri, row) in model.rows.iter().enumerate() {
        if let Some(ci) = row.cards.iter().position(|c| c.selected) {
            return Some((ri, ci));
        }
    }
    // Fall back to the active row's first card, then any first card.
    model
        .rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.is_active && !r.cards.is_empty())
        .or_else(|| model.rows.iter().enumerate().find(|(_, r)| !r.cards.is_empty()))
        .map(|(ri, _)| (ri, 0))
}

// --- Accent backdrop (undocumented SetWindowCompositionAttribute) -------

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    pv_data: *mut c_void,
    cb_data: usize,
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;

/// Enable a blurred backdrop behind the overlay: acrylic first, plain
/// blur second. Dynamically loaded from user32 — when the export is
/// missing or both calls fail, reports [`BackdropMode::AlphaDim`] so the
/// caller switches to the layered fallback. Never panics.
unsafe fn enable_backdrop_blur(hwnd: HWND) -> BackdropMode {
    let Ok(user32) = GetModuleHandleW(windows::core::w!("user32.dll")) else {
        return BackdropMode::AlphaDim;
    };
    let Some(proc_addr) = GetProcAddress(user32, windows::core::s!("SetWindowCompositionAttribute"))
    else {
        return BackdropMode::AlphaDim;
    };
    let set_attr: SetWindowCompositionAttributeFn = std::mem::transmute(proc_addr);
    for (accent_state, mode) in [
        (ACCENT_ENABLE_ACRYLICBLURBEHIND, BackdropMode::Acrylic),
        (ACCENT_ENABLE_BLURBEHIND, BackdropMode::Blur),
    ] {
        let mut policy = AccentPolicy {
            accent_state,
            accent_flags: 0,
            gradient_color: ACRYLIC_TINT,
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attrib: WCA_ACCENT_POLICY,
            pv_data: std::ptr::addr_of_mut!(policy).cast(),
            cb_data: std::mem::size_of::<AccentPolicy>(),
        };
        if set_attr(hwnd, &mut data).as_bool() {
            return mode;
        }
    }
    BackdropMode::AlphaDim
}

/// Overview overlay handle. Created lazily by the daemon on first show.
pub struct OverviewOverlay {
    hwnd: HWND,
    /// UI thread id for the PostThreadMessageW fallback in Drop.
    thread_id: u32,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl OverviewOverlay {
    /// Create the overview overlay on a background thread. `event_tx`
    /// receives an [`OverviewEvent`] for every user action.
    #[cfg_attr(test, allow(unused_variables))] // the cfg(test) panic makes the param unused
    pub fn new(event_tx: mpsc::Sender<OverviewEvent>) -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("OverviewOverlay::new spawns a top-level interactive window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            state().event_tx = Some(event_tx);

            let (tx, rx) = mpsc::channel::<Result<(isize, u32), Win32Error>>();
            let thread = std::thread::Builder::new()
                .name("overview-overlay".into())
                .spawn(move || unsafe {
                    let thread_id = windows::Win32::System::Threading::GetCurrentThreadId();
                    let class_name: Vec<u16> = "LeopardWMOverview\0".encode_utf16().collect();
                    let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(overview_wnd_proc),
                        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                        hCursor: arrow,
                        ..Default::default()
                    };
                    RegisterClassW(&wc);

                    // Interactive: no WS_EX_TRANSPARENT, no WS_EX_NOACTIVATE —
                    // the overlay needs clicks AND keyboard focus.
                    // WS_EX_TOOLWINDOW keeps it out of Alt+Tab / taskbar.
                    // Created non-layered so the accent blur composes the
                    // backdrop; the layered bit is added only for the
                    // no-accent fallback below.
                    let ex_style = WS_EX_TOOLWINDOW | WS_EX_TOPMOST;

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
                            let mode = enable_backdrop_blur(h);
                            if mode == BackdropMode::AlphaDim {
                                // Fallback: per-pixel-alpha layered pipeline
                                // (UpdateLayeredWindow + alpha dim).
                                SetWindowLongPtrW(
                                    h,
                                    GWL_EXSTYLE,
                                    (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST).0 as isize,
                                );
                            }
                            state().backdrop = mode;
                            let _ = tx.send(Ok((h.0 as isize, thread_id)));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                if msg.message == WM_QUIT_OVERVIEW_THREAD {
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
                                "OverviewOverlay: {}",
                                e
                            ))));
                        }
                    }
                })
                .map_err(|e| {
                    Win32Error::HookInstallFailed(format!("OverviewOverlay thread: {}", e))
                })?;

            let (hwnd_raw, thread_id) = match rx.recv() {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Win32Error::HookInstallFailed(
                        "OverviewOverlay init failed".into(),
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

    /// Show the overlay sized/positioned to `monitor_rect`, displaying
    /// `model`. Takes the foreground so the keyboard works immediately.
    pub fn show(&self, monitor_rect: Rect, model: OverviewModel) {
        {
            let mut s = state();
            s.selected = selection_from_model(&model);
            s.model = model;
            s.window_rect = monitor_rect;
            s.hovered = None;
            s.visible = true;
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                monitor_rect.x,
                monitor_rect.y,
                monitor_rect.width,
                monitor_rect.height,
                SWP_SHOWWINDOW,
            );
        }
        sync_thumbnails(self.hwnd);
        render_overlay(self.hwnd);
        // Foreground (with the platform helper's focus-stealing fallbacks)
        // so Esc/arrows land in the overlay without an extra click.
        let _ = crate::set_foreground_window(self.hwnd.0 as u64);
    }

    /// Replace the displayed model in place (overlay stays visible).
    pub fn update_model(&self, model: OverviewModel) {
        {
            let mut s = state();
            // Preserve the current selection if its window survived.
            let prev_wid = s.selection_window_id();
            s.selected = prev_wid
                .and_then(|wid| {
                    model.rows.iter().enumerate().find_map(|(ri, row)| {
                        row.cards
                            .iter()
                            .position(|c| c.window_id == wid)
                            .map(|ci| (ri, ci))
                    })
                })
                .or_else(|| selection_from_model(&model));
            s.model = model;
            s.hovered = None;
        }
        sync_thumbnails(self.hwnd);
        render_overlay(self.hwnd);
    }

    /// Hide the overlay. Marks state hidden BEFORE the OS hide so the
    /// resulting WM_KILLFOCUS doesn't emit a spurious `Dismissed`.
    pub fn hide(&self) {
        state().visible = false;
        drop_all_thumbnails();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

/// Unregister every live thumbnail (each `ThumbnailHandle` drop calls
/// `unregister_raw`). DWM calls happen outside the state lock.
fn drop_all_thumbnails() {
    let handles = std::mem::take(&mut state().thumbnails);
    drop(handles);
}

impl Drop for OverviewOverlay {
    fn drop(&mut self) {
        // Unregister thumbnails BEFORE destroying their destination window
        // so DwmUnregisterThumbnail releases valid handles.
        drop_all_thumbnails();
        // Wake the UI thread out of its message loop, then join. Mirrors
        // TabStripOverlay::drop (PostThreadMessageW fallback included).
        let posted_via_window = unsafe {
            PostMessageW(
                Some(self.hwnd),
                WM_QUIT_OVERVIEW_THREAD,
                WPARAM(0),
                LPARAM(0),
            )
            .is_ok()
        };
        if !posted_via_window {
            let _ = unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT_OVERVIEW_THREAD, WPARAM(0), LPARAM(0))
            };
        }
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
    }
}

// --- Live thumbnails ------------------------------------------------------
//
// DWM composites registered thumbnails ON TOP of the window's painted
// content within their dest rects, so the painted card body stays as the
// backing and the title strip / selection ring (drawn outside the card)
// remain visible. Per-pixel-alpha UpdateLayeredWindow destinations don't
// reliably composite thumbnails, so the AlphaDim fallback skips them and
// keeps the placeholder bodies.

/// The thumbnail dest rect for a card: the body below the title strip,
/// inset by [`THUMB_INSET`]. `None` for compact (untitled) cards — a
/// thumbnail there would cover the card's only title/icon row.
fn thumb_body(card_rect: &Rect) -> Option<Rect> {
    if !card_is_titled(card_rect) {
        return None;
    }
    let body = Rect::new(
        card_rect.x + THUMB_INSET,
        card_rect.y + CARD_TITLE_H + THUMB_INSET,
        card_rect.width - 2 * THUMB_INSET,
        card_rect.height - CARD_TITLE_H - 2 * THUMB_INSET,
    );
    (body.width > 0 && body.height > 0).then_some(body)
}

/// Diff the registered thumbnails against the current model: register
/// new live cards, retarget surviving ones via `update`, drop removed
/// ones. All DWM calls happen outside the state lock.
fn sync_thumbnails(hwnd: HWND) {
    let (mode, targets) = {
        let s = state();
        let targets: Vec<(u64, Rect)> = s
            .model
            .rows
            .iter()
            .flat_map(|row| row.cards.iter())
            .filter(|c| c.live)
            .filter_map(|c| thumb_body(&c.rect).map(|body| (c.window_id, body)))
            .collect();
        (s.backdrop, targets)
    };
    let mut existing = std::mem::take(&mut state().thumbnails);
    if mode == BackdropMode::AlphaDim {
        drop(existing); // layered fallback: no thumbnails, placeholder bodies
        return;
    }
    let mut next: HashMap<u64, ThumbnailHandle> = HashMap::with_capacity(targets.len());
    for (wid, body) in targets {
        let handle = match existing.remove(&wid) {
            Some(h) => h,
            None => match thumbnail::register_for_window(hwnd.0 as isize, wid) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("overview thumbnail register({wid}) failed: {e}");
                    continue;
                }
            },
        };
        // Fill the whole body: DWM stretches the source to rcDestination,
        // and a filled card reads better than a letterboxed one even when
        // the aspect ratio is off.
        let dest = body;
        if let Err(e) = thumbnail::update(handle.as_isize(), dest, 255, true) {
            tracing::warn!("overview thumbnail update({wid}) failed: {e}");
            continue; // handle drops here -> unregistered
        }
        next.insert(wid, handle);
    }
    drop(existing); // windows gone from the model: unregister
    state().thumbnails = next;
}

impl OverviewState {
    fn selection_window_id(&self) -> Option<u64> {
        let (ri, ci) = self.selected?;
        self.model
            .rows
            .get(ri)
            .and_then(|r| r.cards.get(ci))
            .map(|c| c.window_id)
    }

    /// Hit-test a client point against cards (title bar included — the
    /// card rect covers both the title strip and the body).
    fn card_at(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        for (ri, row) in self.model.rows.iter().enumerate() {
            for (ci, card) in row.cards.iter().enumerate() {
                if rect_contains(&card.rect, x, y) {
                    return Some((ri, ci));
                }
            }
        }
        None
    }

    /// Hit-test a client point against row panels (the label strip is
    /// inside the panel).
    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        self.model
            .rows
            .iter()
            .position(|r| rect_contains(&r.panel, x, y))
    }

    /// Move the selection one card left/right within the current row
    /// (x-order), or up/down to the nearest-x card of the adjacent
    /// non-empty row. Returns true when the selection changed.
    fn move_selection(&mut self, key: u16) -> bool {
        let Some((ri, ci)) = self.selected else {
            self.selected = selection_from_model(&self.model);
            return self.selected.is_some();
        };
        match next_selection(&self.model, ri, ci, key) {
            Some(new) if new != (ri, ci) => {
                self.selected = Some(new);
                true
            }
            _ => false,
        }
    }
}

/// Compute the next `(row, card)` for an arrow key, or `None` when the
/// selection cannot move that way (edge of the map).
fn next_selection(
    model: &OverviewModel,
    ri: usize,
    ci: usize,
    key: u16,
) -> Option<(usize, usize)> {
    let rows = &model.rows;
    let cur = rows.get(ri)?.cards.get(ci)?;
    let cur_cx = cur.rect.x + cur.rect.width / 2;
    if key == VK_LEFT.0 || key == VK_RIGHT.0 {
        // Order this row's cards by x and step within it.
        let row = &rows[ri];
        let mut order: Vec<usize> = (0..row.cards.len()).collect();
        order.sort_by_key(|&i| row.cards[i].rect.x);
        let pos = order.iter().position(|&i| i == ci)?;
        let next = if key == VK_LEFT.0 {
            pos.checked_sub(1)?
        } else {
            pos + 1
        };
        order.get(next).map(|&i| (ri, i))
    } else {
        // Up/Down: nearest non-empty row in that direction, card with
        // the closest x-center.
        let mut candidates: Box<dyn Iterator<Item = usize>> = if key == VK_UP.0 {
            Box::new((0..ri).rev())
        } else {
            Box::new(ri + 1..rows.len())
        };
        let target_row = candidates.find(|&i| !rows[i].cards.is_empty())?;
        let best = rows[target_row]
            .cards
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| (c.rect.x + c.rect.width / 2 - cur_cx).abs())?
            .0;
        Some((target_row, best))
    }
}

/// Handle WM_KEYDOWN. Returns true when the key was consumed.
fn handle_key_down(hwnd: HWND, vk: u16) -> bool {
    if vk == VK_ESCAPE.0 {
        send_event(OverviewEvent::Dismissed);
        return true;
    }
    if vk == VK_RETURN.0 {
        let wid = state().selection_window_id();
        if let Some(wid) = wid {
            send_event(OverviewEvent::ActivateWindow(wid));
        }
        return true;
    }
    // Digits 1-9 ('1' = 0x31).
    if (0x31..=0x39).contains(&vk) {
        send_event(OverviewEvent::SwitchWorkspace((vk - 0x31) as usize));
        return true;
    }
    if vk == VK_LEFT.0 || vk == VK_RIGHT.0 || vk == VK_UP.0 || vk == VK_DOWN.0 {
        let changed = state().move_selection(vk);
        if changed {
            render_overlay(hwnd);
        }
        return true;
    }
    false
}

/// Handle mouse buttons. `middle` selects the close gesture.
fn handle_mouse_button(x: i32, y: i32, middle: bool) {
    let (card_hit, row_hit) = {
        let s = state();
        if !s.visible {
            return;
        }
        (
            s.card_at(x, y)
                .and_then(|(ri, ci)| s.model.rows.get(ri).and_then(|r| r.cards.get(ci)))
                .map(|c| c.window_id),
            s.row_at(x, y)
                .and_then(|ri| s.model.rows.get(ri))
                .map(|r| r.workspace_index),
        )
    };
    if middle {
        if let Some(wid) = card_hit {
            send_event(OverviewEvent::CloseWindow(wid));
        }
        return;
    }
    match (card_hit, row_hit) {
        (Some(wid), _) => send_event(OverviewEvent::ActivateWindow(wid)),
        (None, Some(ws_idx)) => send_event(OverviewEvent::SwitchWorkspace(ws_idx)),
        (None, None) => send_event(OverviewEvent::Dismissed),
    }
}

fn handle_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let mut s = state();
    if !s.visible {
        return;
    }
    if !s.mouse_tracking_armed {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
            s.mouse_tracking_armed = true;
        }
    }
    let hit = s.card_at(x, y);
    if hit != s.hovered {
        s.hovered = hit;
        drop(s);
        render_overlay(hwnd);
    }
}

unsafe extern "system" fn overview_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            // Pull keyboard focus to the overlay when it becomes the
            // foreground window (SetFocus is same-thread-legal here).
            if wparam.0 & 0xFFFF != WA_INACTIVE as usize {
                let _ = SetFocus(Some(hwnd));
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            // Click-away: dismiss, but only while logically visible —
            // hide() clears the flag first so daemon-driven hides don't
            // echo a second Dismissed.
            if state().visible {
                send_event(OverviewEvent::Dismissed);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // double-buffered WM_PAINT, no erase
        WM_PAINT => {
            paint_frame(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if handle_key_down(hwnd, wparam.0 as u16) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_LBUTTONDOWN | WM_MBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            handle_mouse_button(x, y, msg == WM_MBUTTONDOWN);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            handle_mouse_move(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let mut s = state();
            s.mouse_tracking_armed = false;
            if s.hovered.take().is_some() {
                drop(s);
                render_overlay(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Re-render the current state. Accent modes invalidate the window so the
/// UI thread repaints through WM_PAINT; the layered fallback re-renders
/// the DIB and pushes it with `UpdateLayeredWindow` directly (safe from
/// any thread, same contract as `BorderFrame::render_and_update`). No-op
/// while hidden.
fn render_overlay(hwnd: HWND) {
    let (mode, visible) = {
        let s = state();
        (s.backdrop, s.visible)
    };
    if !visible {
        return;
    }
    if mode != BackdropMode::AlphaDim {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        return;
    }
    let (model, hovered, selected, rect) = {
        let s = state();
        if !s.visible {
            return;
        }
        (s.model.clone(), s.hovered, s.selected, s.window_rect)
    };
    if rect.width <= 0 || rect.height <= 0 {
        return;
    }
    unsafe { render_and_update(hwnd, rect, &model, hovered, selected) }
}

/// A frame rendered into a premultiplied 32bpp top-down DIB, selected
/// into a memory DC and ready to push (`UpdateLayeredWindow` or `BitBlt`).
struct FrameDib {
    hdc_screen: HDC,
    mem_dc: HDC,
    hbitmap: HBITMAP,
    old_bmp: HGDIOBJ,
}

/// Render the frame into a top-down 32bpp DIB: SDF-anti-aliased shapes
/// straight into the bits (color plane + region-alpha plane), GDI for
/// text and icons only, then premultiply. Caller must pass the result to
/// [`release_frame_dib`] after use.
unsafe fn build_frame_dib(
    w: i32,
    h: i32,
    model: &OverviewModel,
    hovered: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
    mode: BackdropMode,
    thumb_wids: &HashSet<u64>,
) -> Option<FrameDib> {
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
    let hbitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;

    let hdc_screen = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(hdc_screen));
    let old_bmp = SelectObject(mem_dc, hbitmap.into());

    // 1. Backdrop clear (GDI), flushed before the direct-bit shape pass.
    fill(mem_dc, &Rect::new(0, 0, w, h), BACKDROP_BG);
    let _ = GdiFlush();

    let backdrop_alpha = match mode {
        BackdropMode::Acrylic => 0, // tint comes from the accent policy
        BackdropMode::Blur => BLUR_TINT_ALPHA,
        BackdropMode::AlphaDim => BACKDROP_ALPHA,
    };
    let mut alpha = vec![backdrop_alpha; (w * h) as usize];

    // 2. Shapes: anti-aliased rounded panels/cards/outline into the bits.
    {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        draw_shapes(pixels, &mut alpha, w, h, model, hovered, selected);
    }

    // 3. Text + icons via GDI, flushed before the alpha finish below.
    draw_content(mem_dc, model, thumb_wids);
    let _ = GdiFlush();

    // 4. Label-text opacity fix-up + premultiply.
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
    finish_alpha(pixels, &mut alpha, w, h, model);

    Some(FrameDib {
        hdc_screen,
        mem_dc,
        hbitmap,
        old_bmp,
    })
}

unsafe fn release_frame_dib(dib: FrameDib) {
    SelectObject(dib.mem_dc, dib.old_bmp);
    let _ = DeleteDC(dib.mem_dc);
    ReleaseDC(None, dib.hdc_screen);
    let _ = DeleteObject(dib.hbitmap.into());
}

/// WM_PAINT for the accent (blur) modes: double-buffer the frame in the
/// DIB and `BitBlt` it to the window DC — SRCCOPY carries the alpha
/// bytes, so DWM composites the chrome over the blurred backdrop.
unsafe fn paint_frame(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (model, hovered, selected, rect, mode, visible, thumb_wids) = {
        let s = state();
        (
            s.model.clone(),
            s.hovered,
            s.selected,
            s.window_rect,
            s.backdrop,
            s.visible,
            s.thumbnails.keys().copied().collect::<HashSet<u64>>(),
        )
    };
    if visible && mode != BackdropMode::AlphaDim && rect.width > 0 && rect.height > 0 {
        if let Some(dib) = build_frame_dib(
            rect.width,
            rect.height,
            &model,
            hovered,
            selected,
            mode,
            &thumb_wids,
        ) {
            let _ = BitBlt(
                hdc,
                0,
                0,
                rect.width,
                rect.height,
                Some(dib.mem_dc),
                0,
                0,
                SRCCOPY,
            );
            release_frame_dib(dib);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Layered fallback: render the DIB and push it with `UpdateLayeredWindow`.
unsafe fn render_and_update(
    hwnd: HWND,
    rect: Rect,
    model: &OverviewModel,
    hovered: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
) {
    let (w, h) = (rect.width, rect.height);
    // AlphaDim never registers thumbnails: pass an empty set so every
    // card keeps its placeholder body icon.
    let no_thumbs = HashSet::new();
    let Some(dib) =
        build_frame_dib(w, h, model, hovered, selected, BackdropMode::AlphaDim, &no_thumbs)
    else {
        return;
    };
    let pt_dst = POINT {
        x: rect.x,
        y: rect.y,
    };
    let sz = SIZE { cx: w, cy: h };
    let pt_src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        Some(dib.hdc_screen),
        Some(&pt_dst),
        Some(&sz),
        Some(dib.mem_dc),
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    release_frame_dib(dib);
}

// --- Software (SDF) shape renderer --------------------------------------
//
// GDI's RoundRect is hard-edged, so every rounded shape (panel and card
// fills/frames, label and title strips, the selection outline) is
// rasterized directly into the DIB with the same anti-aliased
// rounded-rect SDF the border overlay uses. The color plane holds
// straight (non-premultiplied) pixels in DIB order — 0x00RRGGBB as u32,
// i.e. R in bits 16..24 (BGRA in memory) — so COLORREF values must go
// through [`bgr_to_pixel`] first (the grey palette constants are
// channel-symmetric and need no swap). A parallel alpha plane
// accumulates coverage-weighted region alpha. GDI is kept only for text
// and icons.

/// COLORREF byte order (0x00BBGGRR) -> DIB pixel order (0x00RRGGBB).
/// Same channel mapping as `border.rs`, which splits its `color_bgr`
/// into components and writes R into bits 16..24 of the DIB pixel.
fn bgr_to_pixel(color: u32) -> u32 {
    (color & 0x0000_FF00) | ((color & 0xFF) << 16) | ((color >> 16) & 0xFF)
}

/// Clip `rect` to the bitmap; returns `(x0, x1, y0, y1)` half-open bounds.
fn clip_rect(rect: &Rect, w: i32, h: i32) -> (i32, i32, i32, i32) {
    (
        rect.x.clamp(0, w),
        (rect.x + rect.width).clamp(0, w),
        rect.y.clamp(0, h),
        (rect.y + rect.height).clamp(0, h),
    )
}

/// Composite `color` over one pixel at `cov` coverage: the color plane
/// lerps toward `color`, the alpha plane source-overs `region_alpha`.
fn blend_pixel(
    pixels: &mut [u32],
    alpha: &mut [u8],
    i: usize,
    color: u32,
    region_alpha: u8,
    cov: f32,
) {
    if cov >= 1.0 {
        pixels[i] = color;
        alpha[i] = region_alpha;
        return;
    }
    let dst = pixels[i];
    let lerp =
        |s: u32, d: u32| ((s as f32) * cov + (d as f32) * (1.0 - cov)).round() as u32;
    let r = lerp((color >> 16) & 0xFF, (dst >> 16) & 0xFF);
    let g = lerp((color >> 8) & 0xFF, (dst >> 8) & 0xFF);
    let b = lerp(color & 0xFF, dst & 0xFF);
    pixels[i] = r << 16 | g << 8 | b;
    alpha[i] =
        (f32::from(region_alpha) * cov + f32::from(alpha[i]) * (1.0 - cov)).round() as u8;
}

/// Fill a rounded rect with anti-aliased corners. `round_top` /
/// `round_bottom` square off that edge's corner pair by extending the SDF
/// rect past the squared edge (iteration stays clipped to `rect`), so
/// inner strips clipped by an outer rounding stay flush — card title bars
/// and panel label strips round on top, sit square on the bottom.
#[allow(clippy::too_many_arguments)] // flat pixel-pipeline params
fn sdf_fill_round(
    pixels: &mut [u32],
    alpha: &mut [u8],
    w: i32,
    h: i32,
    rect: &Rect,
    radius: f32,
    round_top: bool,
    round_bottom: bool,
    color: u32,
    region_alpha: u8,
) {
    let (x0, x1, y0, y1) = clip_rect(rect, w, h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    // A squared edge extends the SDF rect past itself (see `ry`/`rh`
    // below), so only edges that actually round constrain the radius:
    // with one squared edge the effective height is ~2x the rect's.
    let max_rad_y = if round_top && round_bottom {
        rect.height as f32 / 2.0
    } else {
        rect.height as f32
    };
    let rad = radius
        .min(rect.width as f32 / 2.0)
        .min(max_rad_y)
        .max(0.0);
    let ext = rad.ceil() + 1.0;
    let ry = if round_top { rect.y as f32 } else { rect.y as f32 - ext };
    let rh = rect.height as f32
        + if round_top { 0.0 } else { ext }
        + if round_bottom { 0.0 } else { ext };
    // Rows beyond the corner reach are exact (integer-aligned straight
    // edges have full or zero coverage); only corner rows need the SDF.
    let band = rad.ceil() as i32 + 1;
    for y in y0..y1 {
        let row = (y * w) as usize;
        let in_top = round_top && y < rect.y + band;
        let in_bottom = round_bottom && y >= rect.y + rect.height - band;
        if !in_top && !in_bottom {
            for i in row + x0 as usize..row + x1 as usize {
                pixels[i] = color;
                alpha[i] = region_alpha;
            }
            continue;
        }
        let pyf = y as f32 + 0.5;
        for x in x0..x1 {
            let sdf =
                rounded_rect_sdf(x as f32 + 0.5, pyf, rect.x as f32, ry, rect.width as f32, rh, rad);
            let cov = clamp(0.5 - sdf, 0.0, 1.0);
            if cov > 0.0 {
                blend_pixel(pixels, alpha, row + x as usize, color, region_alpha, cov);
            }
        }
    }
}

/// Stroke a rounded-rect outline `stroke` px thick, anti-aliased on both
/// edges, kept inside `rect` (PS_INSIDEFRAME semantics).
#[allow(clippy::too_many_arguments)] // flat pixel-pipeline params
fn sdf_stroke_round(
    pixels: &mut [u32],
    alpha: &mut [u8],
    w: i32,
    h: i32,
    rect: &Rect,
    radius: f32,
    stroke: f32,
    color: u32,
    region_alpha: u8,
) {
    let (x0, x1, y0, y1) = clip_rect(rect, w, h);
    if x0 >= x1 || y0 >= y1 || stroke <= 0.0 {
        return;
    }
    let rad = radius
        .min(rect.width as f32 / 2.0)
        .min(rect.height as f32 / 2.0)
        .max(0.0);
    let inner_rad = (rad - stroke).max(0.0);
    let (rxf, ryf) = (rect.x as f32, rect.y as f32);
    let (rwf, rhf) = (rect.width as f32, rect.height as f32);
    // Skip interior pixels farther from every edge than the ring reaches.
    let band = (stroke + rad).ceil() as i32 + 1;
    for y in y0..y1 {
        let row = (y * w) as usize;
        let pyf = y as f32 + 0.5;
        let edge_row = y < rect.y + band || y >= rect.y + rect.height - band;
        for x in x0..x1 {
            if !edge_row && x >= rect.x + band && x < rect.x + rect.width - band {
                continue;
            }
            let pxf = x as f32 + 0.5;
            let sdf_outer = rounded_rect_sdf(pxf, pyf, rxf, ryf, rwf, rhf, rad);
            let sdf_inner = rounded_rect_sdf(
                pxf,
                pyf,
                rxf + stroke,
                ryf + stroke,
                rwf - 2.0 * stroke,
                rhf - 2.0 * stroke,
                inner_rad,
            );
            let cov = clamp(0.5 - sdf_outer, 0.0, 1.0) * clamp(sdf_inner + 0.5, 0.0, 1.0);
            if cov > 0.0 {
                blend_pixel(pixels, alpha, row + x as usize, color, region_alpha, cov);
            }
        }
    }
}

/// Whether the card is large enough for the title-bar/body split.
fn card_is_titled(r: &Rect) -> bool {
    r.height >= TITLED_MIN_H && r.width >= TITLED_MIN_W
}

/// Corner radius for a card, clamped so tiny cards stay well-formed.
fn card_radius(r: &Rect) -> f32 {
    (CARD_RADIUS as f32)
        .min(r.width as f32 / 2.0)
        .min(r.height as f32 / 2.0)
}

/// Card chrome: rounded two-tone fill (title strip squared at its bottom
/// so the card's rounding clips only the top corners) and a 1px frame.
fn draw_card_shape(
    pixels: &mut [u32],
    alpha: &mut [u8],
    w: i32,
    h: i32,
    card: &OverviewCard,
    hovered: bool,
) {
    let r = &card.rect;
    let radius = card_radius(r);
    let body_bg = if hovered { CARD_HOVER_BG } else { CARD_BG };
    // The hover highlight must also lighten the title strip: a live
    // thumbnail composites over the body, hiding the body highlight.
    let title_bg = if hovered { CARD_TITLE_HOVER_BG } else { CARD_TITLE_BG };
    if card_is_titled(r) {
        let title = Rect::new(r.x, r.y, r.width, CARD_TITLE_H);
        let body = Rect::new(r.x, r.y + CARD_TITLE_H, r.width, r.height - CARD_TITLE_H);
        sdf_fill_round(pixels, alpha, w, h, &title, radius, true, false, title_bg, OPAQUE);
        sdf_fill_round(pixels, alpha, w, h, &body, radius, false, true, body_bg, OPAQUE);
    } else {
        sdf_fill_round(pixels, alpha, w, h, r, radius, true, true, body_bg, OPAQUE);
    }
    sdf_stroke_round(pixels, alpha, w, h, r, radius, 1.0, CARD_BORDER, OPAQUE);
}

/// Rasterize every shape of the frame: panels (fill, label strip, frame,
/// viewport marker), cards, and the keyboard selection's accent outline.
fn draw_shapes(
    pixels: &mut [u32],
    alpha: &mut [u8],
    w: i32,
    h: i32,
    model: &OverviewModel,
    hovered: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
) {
    // The SDF path writes the pixel plane directly: convert the COLORREF
    // accent to DIB pixel order (GDI passes below keep the raw BGR).
    let accent = bgr_to_pixel(model.accent_bgr);
    let accent_w = model.accent_width.max(1) as f32;
    let panel_rad = PANEL_RADIUS as f32;
    for row in &model.rows {
        let (panel_bg, strip_bg) = if row.is_active {
            (PANEL_ACTIVE_BG, LABEL_STRIP_ACTIVE_BG)
        } else {
            (PANEL_BG, LABEL_STRIP_BG)
        };
        sdf_fill_round(pixels, alpha, w, h, &row.panel, panel_rad, true, true, panel_bg, PANEL_ALPHA);
        // Label strip: darker band across the panel top, top corners
        // following the panel rounding, bottom edge square.
        sdf_fill_round(
            pixels, alpha, w, h, &row.label_strip, panel_rad, true, false, strip_bg, PANEL_ALPHA,
        );
        // Every panel gets the neutral 1px frame; the active panel is
        // marked by an accent ring SELECT_PAD px OUTSIDE it (below),
        // mirroring the selected-card ring, so the indicator gets the
        // same breathing room instead of stroking the panel's own edge.
        sdf_stroke_round(pixels, alpha, w, h, &row.panel, panel_rad, 1.0, PANEL_BORDER, PANEL_ALPHA);
        if row.is_active {
            let ring = Rect::new(
                row.panel.x - SELECT_PAD,
                row.panel.y - SELECT_PAD,
                row.panel.width + 2 * SELECT_PAD,
                row.panel.height + 2 * SELECT_PAD,
            );
            sdf_stroke_round(
                pixels,
                alpha,
                w,
                h,
                &ring,
                (PANEL_RADIUS + SELECT_PAD) as f32,
                accent_w,
                accent,
                OPAQUE,
            );
        }
        // Viewport ring: rounded neutral border around the in-viewport
        // cards (the daemon pre-inflates the rect by VIEWPORT_RING_PAD);
        // radius = card radius + pad so the curves feel concentric with
        // the cards inside. Drawn before the cards so partially scrolled
        // cards ride over it.
        if row.viewport.width > 0 && row.viewport.height > 0 {
            sdf_stroke_round(
                pixels,
                alpha,
                w,
                h,
                &row.viewport,
                (CARD_RADIUS + VIEWPORT_RING_PAD) as f32,
                accent_w,
                VIEWPORT_RING_COLOR,
                PANEL_ALPHA,
            );
        }
    }
    for (ri, row) in model.rows.iter().enumerate() {
        for (ci, card) in row.cards.iter().enumerate() {
            draw_card_shape(pixels, alpha, w, h, card, hovered == Some((ri, ci)));
        }
    }
    // Selection outline last so it rides over neighboring card edges:
    // an accent ring SELECT_PAD px outside the card at the configured
    // focus-border width, its radius enlarged by the same pad so the
    // curves stay concentric.
    if let Some(card) = selected.and_then(|(ri, ci)| model.rows.get(ri)?.cards.get(ci)) {
        let r = &card.rect;
        let outline = Rect::new(
            r.x - SELECT_PAD,
            r.y - SELECT_PAD,
            r.width + 2 * SELECT_PAD,
            r.height + 2 * SELECT_PAD,
        );
        sdf_stroke_round(
            pixels,
            alpha,
            w,
            h,
            &outline,
            card_radius(r) + SELECT_PAD as f32,
            accent_w,
            accent,
            OPAQUE,
        );
    }
}

/// Force label-strip text pixels opaque (they'd dim with the panel
/// otherwise), then premultiply every pixel for the alpha-aware
/// composition (`AC_SRC_ALPHA` / DWM accent).
fn finish_alpha(pixels: &mut [u32], alpha: &mut [u8], w: i32, h: i32, model: &OverviewModel) {
    for row in &model.rows {
        let bg = if row.is_active { LABEL_STRIP_ACTIVE_BG } else { LABEL_STRIP_BG };
        let (x0, x1, y0, y1) = clip_rect(&row.label_strip, w, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * w + x) as usize;
                // The exact-PANEL_ALPHA guard skips anti-aliased corner
                // and edge pixels so they aren't mistaken for text.
                if alpha[i] == PANEL_ALPHA && pixels[i] & 0x00FF_FFFF != bg {
                    alpha[i] = OPAQUE;
                }
            }
        }
    }
    for (px, &a) in pixels.iter_mut().zip(alpha.iter()) {
        let a32 = u32::from(a);
        let r = (*px >> 16) & 0xFF;
        let g = (*px >> 8) & 0xFF;
        let b = *px & 0xFF;
        *px = a32 << 24 | (r * a32 / 255) << 16 | (g * a32 / 255) << 8 | (b * a32 / 255);
    }
}

fn to_win_rect(r: &Rect) -> windows::Win32::Foundation::RECT {
    windows::Win32::Foundation::RECT {
        left: r.x,
        top: r.y,
        right: r.x + r.width,
        bottom: r.y + r.height,
    }
}

unsafe fn fill(hdc: HDC, r: &Rect, color: u32) {
    let brush = CreateSolidBrush(COLORREF(color));
    let _ = FillRect(hdc, &to_win_rect(r), brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn make_font(height_px: i32, weight: i32) -> HFONT {
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    CreateFontW(
        -height_px.max(8),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        crate::tab_strip::FONT_PIPELINE_DEFAULT_PITCH_AND_FAMILY,
        windows::core::PCWSTR(face.as_ptr()),
    )
}

unsafe fn draw_text_in(hdc: HDC, text: &str, r: &Rect, color: u32, format: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        wide.push(0);
    }
    SetTextColor(hdc, COLORREF(color));
    let mut win_rect = to_win_rect(r);
    let _ = DrawTextW(hdc, &mut wide, &mut win_rect, format);
}

/// GDI content pass: text and icons only — every shape is rasterized by
/// [`draw_shapes`] before this runs.
unsafe fn draw_content(hdc: HDC, model: &OverviewModel, thumb_wids: &HashSet<u64>) {
    SetBkMode(hdc, TRANSPARENT);
    let accent = model.accent_bgr;
    let label_font = make_font(14, FW_SEMIBOLD.0 as i32);
    let card_font = make_font(13, FW_NORMAL.0 as i32);
    let pill_font = make_font(11, FW_SEMIBOLD.0 as i32);

    for row in &model.rows {
        draw_row_label(hdc, row, label_font);
        for card in &row.cards {
            if card_is_titled(&card.rect) {
                draw_card_title_strip(hdc, card, card_font, pill_font, accent);
                // A live thumbnail composites over the body: the painted
                // placeholder icon would bleed around its letterboxing.
                if !thumb_wids.contains(&card.window_id) {
                    draw_card_body_icon(hdc, card);
                }
            } else {
                draw_card_compact(hdc, card, card_font, pill_font, accent);
            }
        }
    }

    let _ = DeleteObject(label_font.into());
    let _ = DeleteObject(card_font.into());
    let _ = DeleteObject(pill_font.into());
}

/// Label row: workspace number + name across the panel's top strip.
unsafe fn draw_row_label(hdc: HDC, row: &OverviewRow, label_font: HFONT) {
    let old_font = SelectObject(hdc, label_font.into());
    let text_color = if row.is_active { TEXT_PRIMARY } else { TEXT_SECONDARY };
    let inset = Rect::new(
        row.label_strip.x + 12,
        row.label_strip.y,
        (row.label_strip.width - 16).max(1),
        row.label_strip.height,
    );
    draw_text_in(
        hdc,
        &row.label,
        &inset,
        text_color,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    SelectObject(hdc, old_font);
}

/// Tab-count pill (`n` windows behind the visible tab) inside `pill`.
unsafe fn draw_tab_pill(hdc: HDC, pill: &Rect, n: usize, pill_font: HFONT, accent: u32) {
    fill(hdc, pill, accent);
    let old_font = SelectObject(hdc, pill_font.into());
    draw_text_in(
        hdc,
        &n.to_string(),
        pill,
        PILL_TEXT,
        DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(hdc, old_font);
}

/// Card title bar: 16px app icon + left-aligned window title, with the
/// tab-count pill on the right when the column is tabbed.
unsafe fn draw_card_title_strip(
    hdc: HDC,
    card: &OverviewCard,
    font: HFONT,
    pill_font: HFONT,
    accent: u32,
) {
    let r = &card.rect;
    let pad = 8;
    let mut text_left = r.x + pad;
    if let Some(icon_raw) = card.icon {
        if icon_raw != 0 {
            let hicon = HICON(icon_raw as *mut c_void);
            let _ = DrawIconEx(
                hdc,
                text_left,
                r.y + (CARD_TITLE_H - TITLE_ICON) / 2,
                hicon,
                TITLE_ICON,
                TITLE_ICON,
                0,
                None,
                DI_NORMAL,
            );
            text_left += TITLE_ICON + 6;
        }
    }
    let mut text_right = r.x + r.width - pad;
    if let Some(n) = card.tab_count {
        let pill_w = if n >= 10 { 24 } else { 18 };
        let pill_h = 14;
        let pill = Rect::new(
            r.x + r.width - pill_w - 6,
            r.y + (CARD_TITLE_H - pill_h) / 2,
            pill_w,
            pill_h,
        );
        draw_tab_pill(hdc, &pill, n, pill_font, accent);
        text_right = pill.x - 4;
    }
    let text_w = text_right - text_left;
    if text_w > 16 {
        let old_font = SelectObject(hdc, font.into());
        let text_rect = Rect::new(text_left, r.y, text_w, CARD_TITLE_H);
        draw_text_in(
            hdc,
            &card.title,
            &text_rect,
            TEXT_PRIMARY,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
    }
}

/// Card body placeholder: a larger centered app icon. Only drawn when no
/// live thumbnail is registered for the card (the thumbnail composites
/// over this area).
unsafe fn draw_card_body_icon(hdc: HDC, card: &OverviewCard) {
    let r = &card.rect;
    let body = Rect::new(r.x, r.y + CARD_TITLE_H, r.width, r.height - CARD_TITLE_H);
    let Some(icon_raw) = card.icon else { return };
    if icon_raw == 0 {
        return;
    }
    let size = (body.height * 3 / 5).min(48).min(body.width - 8);
    if size < 16 {
        return;
    }
    let hicon = HICON(icon_raw as *mut c_void);
    let _ = DrawIconEx(
        hdc,
        body.x + (body.width - size) / 2,
        body.y + (body.height - size) / 2,
        hicon,
        size,
        size,
        0,
        None,
        DI_NORMAL,
    );
}

/// Compact rendering for cards too small for the title-bar split: one
/// vertically centered icon + title row (the pre-split layout).
unsafe fn draw_card_compact(
    hdc: HDC,
    card: &OverviewCard,
    font: HFONT,
    pill_font: HFONT,
    accent: u32,
) {
    let r = &card.rect;
    let pad = 6;
    let inner_h = r.height - 2 * pad;
    let mut text_left = r.x + pad;

    // Icon: only when the card is tall enough to show one legibly.
    if let Some(icon_raw) = card.icon {
        let icon_size = (inner_h * 3 / 5).clamp(0, 32);
        if icon_raw != 0 && icon_size >= 10 {
            let hicon = HICON(icon_raw as *mut c_void);
            let icon_top = r.y + (r.height - icon_size) / 2;
            let _ = DrawIconEx(
                hdc,
                text_left,
                icon_top,
                hicon,
                icon_size,
                icon_size,
                0,
                None,
                DI_NORMAL,
            );
            text_left += icon_size + pad;
        }
    }

    // Title: skip on slivers where even a couple of characters won't fit.
    let text_w = r.x + r.width - pad - text_left;
    if text_w > 24 && r.height >= 18 {
        let old_font = SelectObject(hdc, font.into());
        let text_rect = Rect::new(text_left, r.y, text_w, r.height);
        draw_text_in(
            hdc,
            &card.title,
            &text_rect,
            TEXT_PRIMARY,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
    }

    // Tabbed column: small count pill in the top-right corner.
    if let Some(n) = card.tab_count {
        let pill_w = if n >= 10 { 24 } else { 18 };
        let pill_h = 14;
        if r.width >= pill_w + 12 && r.height >= pill_h + 8 {
            let pill = Rect::new(r.x + r.width - pill_w - 4, r.y + 4, pill_w, pill_h);
            draw_tab_pill(hdc, &pill, n, pill_font, accent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thumb_body_titled_card_insets_below_title_strip() {
        let card = Rect::new(100, 50, 200, 150); // titled: >= 72x56
        let body = thumb_body(&card).expect("titled card gets a thumb body");
        assert_eq!(body.x, 100 + THUMB_INSET);
        assert_eq!(body.y, 50 + CARD_TITLE_H + THUMB_INSET);
        assert_eq!(body.width, 200 - 2 * THUMB_INSET);
        assert_eq!(body.height, 150 - CARD_TITLE_H - 2 * THUMB_INSET);
    }

    #[test]
    fn test_thumb_body_compact_card_gets_none() {
        // Below TITLED_MIN_H / TITLED_MIN_W: compact rendering, no thumbnail.
        assert!(thumb_body(&Rect::new(0, 0, 200, 40)).is_none());
        assert!(thumb_body(&Rect::new(0, 0, 60, 200)).is_none());
    }




}
