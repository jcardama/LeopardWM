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

use leopardwm_core_layout::{Easing, Rect};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewCard {
    pub window_id: u64,
    pub title: String,
    /// Raw HICON as `isize` (same convention as `TabLabel::icon`): the
    /// overlay does not own the icon; the source window outlives the draw.
    pub icon: Option<isize>,
    pub rect: Rect,
    /// The window's real on-screen rect in overlay client coordinates
    /// (screen minus overlay origin), for the open/close zoom animation:
    /// the card's live DWM thumbnail glides between this and the card
    /// body. The daemon fills it for every row (all rows glide in
    /// tandem; the close animation may target any workspace).
    pub from_rect: Option<Rect>,
    /// `Some(n)` when the window is the visible tab of a tabbed column
    /// holding `n > 1` windows; the overlay draws a small count pill.
    pub tab_count: Option<usize>,
    pub selected: bool,
    /// Whether the overlay may register a live DWM thumbnail for this
    /// card's body. The daemon clears it when the window already has a
    /// thumbnail registration (ghost animation) or config says placeholder.
    pub live: bool,
    /// Whether the card body draws the cached capture-on-hide snapshot
    /// (config `render = "snapshot"`); falls back to the icon placeholder
    /// when no snapshot is cached. Mutually exclusive with `live`.
    pub snapshot: bool,
}

/// One workspace row: a rounded panel with a label strip across the top
/// and the miniaturized column strip below. Rects are overlay client
/// coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Open/close zoom-animation duration in milliseconds; 0 snaps
    /// instantly (daemon maps config + reduce-motion into this).
    pub anim_ms: u32,
    /// Easing curve for the zoom glide (daemon fills it from the
    /// configured `[animation] easing`).
    pub easing: Easing,
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
            anim_ms: 0,
            easing: Easing::default(),
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

/// Posted (cross-thread) to start the zoom-animation timer: `SetTimer`
/// must run on the overlay's own thread so WM_TIMER lands in its queue.
const WM_OVERVIEW_START_ANIM: u32 = WM_USER + 6;

/// WM_TIMER id for the zoom-animation driver.
const ANIM_TIMER_ID: usize = 1;

/// Animation tick interval (ms). The OS clamps to USER_TIMER_MINIMUM
/// (10ms) — close enough to a 100Hz-ish redraw cadence.
const ANIM_TICK_MS: u32 = 8;

/// Global alpha factors of the pre-rendered staged chrome-fade frames
/// (see [`AnimPhase`]): the chrome steps up through these on open and
/// back down on close. The last MUST be 1.0 — it doubles as the settled
/// frame. Four steps read as a fade over the 150ms default glide; each
/// frame holds a full-monitor 32bpp DIB (~28MB at 5120x1440). The faded
/// copies live only while a glide runs (freed at open-settle, rebuilt
/// from the retained full frame at close-start); everything is freed on
/// hide.
const CHROME_FADE_FACTORS: [f32; 4] = [0.15, 0.45, 0.75, 1.0];

/// Pick the chrome-fade step for motion parameter `k`: thresholds at
/// 0.25 / 0.5 / 0.75. Pure and phase-agnostic — the open (k 0 -> 1)
/// walks the steps up, the close (k 1 -> 0) walks the SAME steps back
/// down, and an Esc-mid-open handover (which keeps `k` continuous)
/// keeps the picker consistent for free.
fn chrome_step_for_k(k: f32) -> usize {
    if k < 0.25 {
        0
    } else if k < 0.5 {
        1
    } else if k < 0.75 {
        2
    } else {
        3
    }
}

/// Which way the zoom animation is running.
///
/// The zoom is carried ENTIRELY by the live DWM thumbnails: retargeting
/// a thumbnail (`thumbnail::update`) is a near-free DWM property change,
/// so the glide runs at the full timer cadence. The painted chrome
/// cannot re-render per tick (the full-monitor software SDF render
/// costs tens of ms in debug builds — re-rendering it per tick is what
/// reduced the old animation to 3-4 visible frames), so it fades in
/// STEPS instead: `show()` renders the frame ONCE and derives faded
/// copies at the [`CHROME_FADE_FACTORS`] global alpha factors BEFORE
/// the animation clock starts (the first tick restarts the clock, so
/// the pre-render cost never eats into the glide), and each tick
/// cheaply `BitBlt`s the
/// prebuilt DIB for the current motion parameter `k`
/// ([`chrome_step_for_k`]) — chrome fades in under the gliding
/// thumbnails on open and back out on close. Caveats: in Acrylic mode
/// the OS-side blur/tint lives in the accent policy and cannot fade
/// (only our painted chrome does; in Blur mode the painted backdrop
/// TINT scales with the steps, approximating a backdrop fade), and the
/// step frames snapshot the show-time model — a refresh landing
/// mid-animation shows its new chrome only when the open settles. Cards
/// without a thumbnail (compact, placeholder/snapshot mode) fade with
/// the chrome steps; the AlphaDim fallback never animates and skips the
/// staged fade entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimPhase {
    /// Overlay opening: every row's thumbnails glide from their windows'
    /// would-be on-screen rects to their card bodies over the static
    /// chrome; inactive rows additionally fade in (0 -> 255) while
    /// gliding (active row stays 255).
    Opening,
    /// Overlay closing: every row's thumbnails glide back toward their
    /// would-be rects; the target row stays at 255 while the other rows
    /// fade out (255 -> 0), the chrome stays, then the window hides.
    Closing,
}

/// One in-flight open/close animation.
#[derive(Debug, Clone, Copy)]
struct AnimState {
    phase: AnimPhase,
    started: std::time::Instant,
    duration_ms: u32,
    /// Easing curve for the glide (from the model at start time).
    easing: Easing,
    /// Index of the row whose thumbnails stay fully opaque while gliding
    /// (open: active row; close: the activated workspace's row); the
    /// other rows opacity-ramp on top of the same glide.
    row: usize,
    /// Timer ticks consumed so far (diagnostics: logged on finish).
    ticks: u32,
    /// Send `OverviewEvent::Dismissed` when the close finishes: set for
    /// overlay-initiated closes (Esc, backdrop click, focus loss) so the
    /// daemon's bookkeeping runs only AFTER the animation — its hide()
    /// then no-ops on the already-hidden window instead of stomping the
    /// zoom. Daemon-initiated closes (false) already did their own.
    notify: bool,
}

/// Linear progress of `anim` in `[0, 1]`.
fn anim_progress(anim: &AnimState) -> f32 {
    if anim.duration_ms == 0 {
        return 1.0;
    }
    let elapsed = anim.started.elapsed().as_secs_f32() * 1000.0;
    (elapsed / anim.duration_ms as f32).clamp(0.0, 1.0)
}

/// Apply the configured easing curve to a progress value (f32 shim over
/// the f64 [`Easing::apply`]).
fn apply_easing(easing: Easing, t: f32) -> f32 {
    easing.apply(f64::from(t)) as f32
}

/// Inverse of [`apply_easing`] on `[0, 1]`: the `t` with `ease(t) = y`.
/// Every [`Easing`] variant is strictly monotonic, so the inverse is
/// well-defined; used by the Esc-mid-open close handover to back-date
/// the close's start time so the motion parameter stays continuous.
fn ease_inverse(easing: Easing, y: f32) -> f32 {
    let y = y.clamp(0.0, 1.0);
    match easing {
        Easing::Linear => y,
        Easing::EaseOut => 1.0 - (1.0 - y).cbrt(),
        Easing::EaseIn => y.cbrt(),
        Easing::EaseInOut => {
            if y < 0.5 {
                (y / 4.0).cbrt()
            } else {
                1.0 - (2.0 * (1.0 - y)).cbrt() / 2.0
            }
        }
    }
}

fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}

fn lerp_rect(a: &Rect, b: &Rect, t: f32) -> Rect {
    Rect::new(
        lerp_i32(a.x, b.x, t),
        lerp_i32(a.y, b.y, t),
        lerp_i32(a.width, b.width, t).max(1),
        lerp_i32(a.height, b.height, t).max(1),
    )
}

/// Motion parameter `k` in `[0, 1]` of the animation: every gliding
/// thumbnail sits at `lerp(from_rect, card_body, k)`. Opening runs
/// 0 -> 1, closing 1 -> 0 — that shared parameter is what makes an
/// interrupted open hand over to the close seamlessly (see
/// [`start_close`]).
fn anim_k(anim: &AnimState) -> f32 {
    eased_k(anim.phase, anim.easing, anim_progress(anim))
}

/// Pure core of [`anim_k`]: ease `progress` with the configured curve
/// and orient it for the phase (open 0 -> 1, close 1 -> 0).
fn eased_k(phase: AnimPhase, easing: Easing, progress: f32) -> f32 {
    let t = apply_easing(easing, progress);
    match phase {
        AnimPhase::Opening => t,
        AnimPhase::Closing => 1.0 - t,
    }
}

/// `k` for thumbnail (re)registration outside a timer tick. An Opening
/// anim that hasn't ticked yet pins to 0: the first tick restarts the
/// clock (the show-time paint may have eaten the whole duration), so
/// wall-clock `k` here would register at the final card bodies and flash
/// the settled layout before the glide.
fn sync_k(anim: &AnimState) -> f32 {
    if anim.phase == AnimPhase::Opening && anim.ticks == 0 {
        0.0
    } else {
        anim_k(anim)
    }
}

/// Thumbnail opacity for INACTIVE (non-target) rows at motion parameter
/// `k`: they opacity-ramp (open 0 -> 255, close 255 -> 0) ON TOP of the
/// shared glide — every workspace's from_rects cover the same screen
/// area, so inactive rows at full opacity would pile up chaotically.
fn inactive_row_opacity(k: f32) -> u8 {
    (k.clamp(0.0, 1.0) * 255.0).round() as u8
}

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
    /// In-flight open/close zoom animation, if any.
    anim: Option<AnimState>,
    /// Pre-rendered staged chrome-fade frames, one per
    /// [`CHROME_FADE_FACTORS`] entry, built in `show()` and freed via
    /// [`free_chrome_steps`] on hide (each ~28MB at 5120x1440). Empty
    /// when the open didn't animate (AlphaDim, anim_ms == 0) or the
    /// pre-render failed — chrome then shows at full strength at once.
    /// Also emptied at open-settle (the faded copies are freed, the
    /// full-strength frame moves to `chrome_full`) and refilled at
    /// close-start by [`rebuild_faded_chrome_steps`].
    chrome_steps: Vec<ChromeStep>,
    /// Full-strength chrome frame retained at open-settle after the
    /// faded step DIBs are freed ([`take_faded_steps`]): the close-start
    /// rebuild derives fresh faded copies from it instead of holding all
    /// four DIBs (~85MB of faded copies at 5120x1440) while the overview
    /// sits open. Freed with the steps in [`free_chrome_steps`].
    chrome_full: Option<ChromeStep>,
    /// Hover cache pre-built from the show-time chrome render, adopted
    /// at open-settle when the state snapshot still matches
    /// ([`adopt_pending_hover_cache`]) — eliminating the second
    /// full-monitor SDF render per animated open. Freed on hide/close
    /// finish/drop via [`drop_pending_hover_cache`].
    pending_hover_cache: Option<PendingHoverCache>,
    /// Last fade step blitted to the window, so ticks skip redundant
    /// blits (the step only changes 3 times per open/close).
    chrome_last_step: Option<usize>,
    /// Cached settled frame for cheap hover repaints (accent modes only):
    /// rebuilt on every full render, freed on hide. See [`HoverCache`].
    hover_cache: Option<HoverCache>,
    /// Set by [`render_overlay`] before invalidating: the next WM_PAINT
    /// must re-render (model/selection changed) instead of re-blitting
    /// the cached working frame.
    needs_full_render: bool,
    /// Corner-cap mask layer HWND (as `isize`; 0 = not created): a
    /// click-through layered companion window that rounds off the live
    /// DWM thumbnails' bottom corners. Created on the overlay thread,
    /// shown only on the settled map (see [`sync_mask`]).
    mask_hwnd: isize,
    /// What the mask layer currently shows (window rect + caps), so
    /// [`sync_mask`] skips pushes that would change nothing.
    mask_pushed: (Rect, Vec<CornerCap>),
}

/// Cached settled frame so a hover change repaints two cards instead of
/// re-running the full-monitor SDF render (~40-50ms in debug — the old
/// per-mouse-move hitch/flicker). `work_*` is the premultiplied DIB the
/// window shows (kept selected into its memory DC for rect blits);
/// `base_*` is the same frame BEFORE the hover and the premultiply
/// (straight color + region-alpha plane), so a repaint resets a card
/// rect from the base and redoes only that card's chrome. Handles are
/// `isize` like [`ChromeStep`]; all access runs under the state lock.
struct HoverCache {
    work_dc: isize,
    work_bmp: isize,
    work_old: isize,
    /// Raw working-DIB bits (`w*h` premultiplied u32), valid while
    /// `work_bmp` lives.
    work_bits: isize,
    /// Region-alpha plane matching the working frame (repaints reset
    /// card rects from `base_alpha` before re-premultiplying).
    work_alpha: Vec<u8>,
    /// Hover-free straight-color pixels (pre-premultiply).
    base_pixels: Vec<u32>,
    /// Region-alpha plane matching `base_pixels`.
    base_alpha: Vec<u8>,
    w: i32,
    h: i32,
}

/// A hover cache built from the show-time chrome render plus the state
/// snapshot it reflects. [`adopt_pending_hover_cache`] installs it at
/// open-settle only when the snapshot still matches the live state — a
/// mid-open refresh (which carries the anim across) invalidates it and
/// the settle falls back to the render-at-settle path.
struct PendingHoverCache {
    cache: HoverCache,
    model: OverviewModel,
    selected: Option<(usize, usize)>,
    /// Thumbnail registrations baked into the render (body icons are
    /// skipped for covered cards), compared against the settled set.
    thumb_wids: HashSet<u64>,
}

/// One pre-rendered chrome frame: a premultiplied 32bpp DIB already
/// faded to a fixed [`CHROME_FADE_FACTORS`] factor, kept selected into
/// its memory DC so a tick can `BitBlt` it straight to the window.
/// Handles are stored as `isize` (created on the daemon thread in
/// `show()`, blitted on the overlay thread; never used concurrently —
/// blits and frees both run under the state lock). No `Drop` impl:
/// freeing is explicit through [`free_chrome_steps`].
struct ChromeStep {
    mem_dc: isize,
    hbitmap: isize,
    old_bmp: isize,
    /// Raw DIB bits (`w*h` premultiplied u32), valid while `hbitmap`
    /// lives; the close-start rebuild copies the retained full frame
    /// from here. 0 only in test fakes.
    bits: isize,
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
        anim: None,
        chrome_steps: Vec::new(),
        chrome_full: None,
        pending_hover_cache: None,
        chrome_last_step: None,
        hover_cache: None,
        needs_full_render: true,
        mask_hwnd: 0,
        mask_pushed: (Rect::new(0, 0, 0, 0), Vec::new()),
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

/// Locate a window's card in `model` as `(row_idx, card_idx)`.
fn locate_card(model: &OverviewModel, wid: u64) -> Option<(usize, usize)> {
    model.rows.iter().enumerate().find_map(|(ri, row)| {
        row.cards
            .iter()
            .position(|c| c.window_id == wid)
            .map(|ci| (ri, ci))
    })
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

// --- Corner-cap mask layer ----------------------------------------------
//
// Live DWM thumbnails composite ABOVE everything the overlay paints, so
// their square corners overflow the cards' rounded BOTTOM corners and
// nothing in the frame DIB can clip them. A companion click-through
// layered window owned by the overlay (always above its owner) paints
// small anti-aliased caps over each thumbnail-carrying card's bottom two
// corners. A cap restores everything the square preview corner covers,
// in the overlay's own layer order: panel background outside the card's
// rounding, the card's 1px frame arc, and (selected card only) the
// accent ring's corner arc — alpha 0 over the preview interior (inside
// the frame's inner rounding) and everywhere else. The squares sit at
// the THUMBNAIL dest rect corners (the preview body, [`thumb_body`]),
// mirroring the snapshot path's [`mask_snapshot_corners`], so the cap
// never repaints pixels the preview doesn't cover. Shown only on the
// SETTLED map (the caps sit on fixed card corners; mid-glide they'd
// float over moving thumbnails).

/// One corner cap: pixels covering everything the square preview corner
/// overlaps within `square` (alpha = 1 - coverage of the frame's INNER
/// rounding), colored panel-bg / frame arc / accent ring per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CornerCap {
    square: Rect,
    /// The card rect whose bottom rounding the cap follows.
    card: Rect,
    /// The live thumbnail's dest rect ([`thumb_body`] of `card`): the
    /// preview pixels the cap may repaint.
    body: Rect,
    /// Card corner radius, stored as rounded f32 via [`card_radius`].
    radius_px: i32,
    /// Panel background under the card (COLORREF order).
    color: u32,
    /// Selected card: accent ring `(color COLORREF, stroke px)` — the cap
    /// re-draws the ring's corner arc segment over its panel/frame fill.
    ring: Option<(u32, u32)>,
}

impl CornerCap {
    /// The cap translated by `(dx, dy)`: the SDF math is
    /// translation-invariant, so painting into a sub-rect DIB just
    /// shifts every rect into the DIB's local space.
    fn offset(&self, dx: i32, dy: i32) -> CornerCap {
        let shift = |r: &Rect| Rect::new(r.x + dx, r.y + dy, r.width, r.height);
        CornerCap {
            square: shift(&self.square),
            card: shift(&self.card),
            body: shift(&self.body),
            ..self.clone()
        }
    }
}

/// Bounding union of the caps' squares, padded by 1px and clipped to the
/// `w` x `h` overlay client area: the mask layer allocates (and zeroes)
/// only this sub-rect instead of a full-monitor DIB on every push.
/// `None` when no cap survives the clip.
fn caps_bounds(caps: &[CornerCap], w: i32, h: i32) -> Option<Rect> {
    if caps.is_empty() {
        return None;
    }
    let union = bounding_rect(caps.iter().map(|c| c.square));
    let padded = Rect::new(union.x - 1, union.y - 1, union.width + 2, union.height + 2);
    let (x0, x1, y0, y1) = clip_rect(&padded, w, h);
    (x0 < x1 && y0 < y1).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Bounding box of a non-empty rect iterator.
fn bounding_rect(rects: impl Iterator<Item = Rect>) -> Rect {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for r in rects {
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_x = max_x.max(r.x + r.width);
        max_y = max_y.max(r.y + r.height);
    }
    Rect::new(min_x, min_y, (max_x - min_x).max(0), (max_y - min_y).max(0))
}

/// Mask window class name (also in the daemon's enumeration skip list).
fn mask_class_name() -> Vec<u16> {
    "LeopardWMOverviewMask\0".encode_utf16().collect()
}

unsafe extern "system" fn mask_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Create the corner-cap mask window on the overlay thread, owned by
/// `owner` (the overlay HWND) so it always z-orders above it. Layered +
/// transparent + no-activate: per-pixel alpha, click-through, never
/// takes focus. Returns the HWND as `isize` (0 on failure — the overlay
/// then simply shows square thumbnail corners).
unsafe fn create_mask_window(owner: HWND) -> isize {
    let class_name = mask_class_name();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(mask_wnd_proc),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    RegisterClassW(&wc);
    match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(class_name.as_ptr()),
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
        Ok(h) => h.0 as isize,
        Err(e) => {
            tracing::warn!("overview corner-cap mask window creation failed: {e}");
            0
        }
    }
}

/// Compute the caps for the current model: one pair per titled card
/// whose live thumbnail is registered (`thumb_wids`). The keyboard
/// selection adds the accent ring to its card's caps. Pure for tests.
fn corner_caps(
    model: &OverviewModel,
    thumb_wids: &HashSet<u64>,
    selected: Option<(usize, usize)>,
) -> Vec<CornerCap> {
    let mut caps = Vec::new();
    for (ri, row) in model.rows.iter().enumerate() {
        let color = if row.is_active { PANEL_ACTIVE_BG } else { PANEL_BG };
        for (ci, card) in row.cards.iter().enumerate() {
            if !thumb_wids.contains(&card.window_id) {
                continue;
            }
            let Some(body) = thumb_body(&card.rect) else {
                continue;
            };
            let radius = card_radius(&card.rect);
            let inner_rad = (radius - THUMB_INSET as f32).max(0.0);
            let ring = (selected == Some((ri, ci)))
                .then_some((model.accent_bgr, model.accent_width.max(1)));
            // Square span: inner radius + frame + ring stroke, so the cap
            // covers the full square the preview corner can overlap even
            // when a wide ring dips inside the card.
            let span_rad = inner_rad + ring.map_or(0.0, |(_, w)| w as f32);
            for square in bottom_corner_squares(&body, span_rad) {
                caps.push(CornerCap {
                    square,
                    card: card.rect,
                    body,
                    radius_px: radius.round() as i32,
                    color,
                    ring,
                });
            }
        }
    }
    caps
}

/// Show, refresh, or hide the mask layer to match the current state:
/// painted and shown only when the map is settled (visible, no anim)
/// with live thumbnails to round off. Cross-thread-safe
/// (`UpdateLayeredWindow` + `SetWindowPos`, like the border overlay).
fn sync_mask() {
    let (mask, rect, caps) = {
        let mut s = state();
        if s.mask_hwnd == 0 {
            return;
        }
        let settled = s.visible && s.anim.is_none() && s.backdrop != BackdropMode::AlphaDim;
        let caps = if settled {
            let wids: HashSet<u64> = s.thumbnails.keys().copied().collect();
            corner_caps(&s.model, &wids, s.selected)
        } else {
            Vec::new()
        };
        // The layer already shows exactly this: skip the push. Redundant
        // UpdateLayeredWindow + SetWindowPos churn makes the OS post
        // synthetic WM_MOUSEMOVEs to the overlay under a stationary
        // cursor (hover churn) for a pixel-identical mask.
        if s.mask_pushed.0 == s.window_rect && s.mask_pushed.1 == caps {
            return;
        }
        s.mask_pushed = (s.window_rect, caps.clone());
        (s.mask_hwnd, s.window_rect, caps)
    };
    let hwnd = HWND(mask as *mut c_void);
    if caps.is_empty() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        return;
    }
    unsafe { update_mask_window(hwnd, rect, &caps) };
}

/// Paint the cap layer into a transparent DIB sized to the caps'
/// bounding union ([`caps_bounds`] — a thin band, not the full monitor),
/// push it with `UpdateLayeredWindow` at that sub-rect, and show the
/// window in the topmost band (the ownership chain keeps it above the
/// overlay). Never activates.
unsafe fn update_mask_window(hwnd: HWND, rect: Rect, caps: &[CornerCap]) {
    let Some(bounds) = caps_bounds(caps, rect.width, rect.height) else {
        // Caps entirely clipped out: nothing to show.
        let _ = ShowWindow(hwnd, SW_HIDE);
        return;
    };
    let (w, h) = (bounds.width, bounds.height);
    let bmi = top_down_bmi(w, h);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let Ok(hbitmap) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        return;
    };
    let hdc_screen = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(hdc_screen));
    let old_bmp = SelectObject(mem_dc, hbitmap.into());
    {
        // CreateDIBSection zero-initializes: everything not painted
        // below stays fully transparent (premultiplied 0). Caps shift
        // into the sub-rect's local space before painting.
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        for cap in caps {
            paint_corner_cap(pixels, w, h, &cap.offset(-bounds.x, -bounds.y));
        }
    }
    let pt_dst = POINT {
        x: rect.x + bounds.x,
        y: rect.y + bounds.y,
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
        Some(hdc_screen),
        Some(&pt_dst),
        Some(&sz),
        Some(mem_dc),
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    SelectObject(mem_dc, old_bmp);
    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, hdc_screen);
    let _ = DeleteObject(hbitmap.into());
}

/// Rasterize one cap, premultiplied for `ULW_ALPHA`. Alpha = 1 -
/// coverage of the frame's INNER rounding (the preview keeps showing
/// inside it); the covered band is colored in the overlay's own layer
/// order: panel bg outside the card's rounding, the card's 1px frame arc
/// between the card and inner roundings, and the accent ring arc on top
/// for the selected card.
fn paint_corner_cap(pixels: &mut [u32], w: i32, h: i32, cap: &CornerCap) {
    let panel = bgr_to_pixel(cap.color);
    let border = bgr_to_pixel(CARD_BORDER);
    let radius = cap.radius_px as f32;
    let inner_rad = (radius - THUMB_INSET as f32).max(0.0);
    let (x0, x1, y0, y1) = clip_rect(&cap.square, w, h);
    for y in y0..y1 {
        for x in x0..x1 {
            let cov_inner = bottom_round_cov(x, y, &cap.body, inner_rad);
            let a = ((1.0 - cov_inner) * 255.0).round() as u32;
            if a == 0 {
                continue;
            }
            let cov_card = bottom_round_cov(x, y, &cap.card, radius);
            // The covered band splits into frame arc (inside the card's
            // rounding) and panel bg (outside it); the weights sum to the
            // cap's alpha, so normalizing yields the straight color.
            let frame_w = (cov_card - cov_inner).max(0.0);
            let panel_w = 1.0 - cov_card;
            let total = frame_w + panel_w;
            if total <= 0.0 {
                continue;
            }
            let mut color = lerp_color(panel, border, frame_w / total);
            if let Some((accent, stroke)) = cap.ring {
                let cov_ring = ring_cov(x, y, &cap.card, radius, stroke as f32);
                if cov_ring > 0.0 {
                    color = lerp_color(color, bgr_to_pixel(accent), cov_ring);
                }
            }
            let (cr, cg, cb) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);
            pixels[(y * w + x) as usize] =
                a << 24 | (cr * a / 255) << 16 | (cg * a / 255) << 8 | (cb * a / 255);
        }
    }
}

/// Coverage of the selected-card accent ring at pixel `(px, py)`: a
/// `stroke`-px band inset from a rect [`SELECT_PAD`] outside the card at
/// radius + pad. Must mirror [`draw_shapes`]' selection outline exactly
/// (same rect, radius, and SDF math) so the cap's restored arc segment
/// meets the overlay's own ring without a seam.
fn ring_cov(px: i32, py: i32, card: &Rect, radius: f32, stroke: f32) -> f32 {
    let ring = Rect::new(
        card.x - SELECT_PAD,
        card.y - SELECT_PAD,
        card.width + 2 * SELECT_PAD,
        card.height + 2 * SELECT_PAD,
    );
    let rad = (radius + SELECT_PAD as f32)
        .min(ring.width as f32 / 2.0)
        .min(ring.height as f32 / 2.0)
        .max(0.0);
    let inner_rad = (rad - stroke).max(0.0);
    let (pxf, pyf) = (px as f32 + 0.5, py as f32 + 0.5);
    let sdf_outer = rounded_rect_sdf(
        pxf,
        pyf,
        ring.x as f32,
        ring.y as f32,
        ring.width as f32,
        ring.height as f32,
        rad,
    );
    let sdf_inner = rounded_rect_sdf(
        pxf,
        pyf,
        ring.x as f32 + stroke,
        ring.y as f32 + stroke,
        ring.width as f32 - 2.0 * stroke,
        ring.height as f32 - 2.0 * stroke,
        inner_rad,
    );
    clamp(0.5 - sdf_outer, 0.0, 1.0) * clamp(sdf_inner + 0.5, 0.0, 1.0)
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
                            // Create the mask BEFORE taking the lock:
                            // window creation can dispatch same-thread
                            // sent messages whose handlers lock state.
                            let mask = create_mask_window(h);
                            {
                                let mut s = state();
                                s.backdrop = mode;
                                s.mask_hwnd = mask;
                                // Fresh mask window: it shows nothing yet.
                                s.mask_pushed = (Rect::new(0, 0, 0, 0), Vec::new());
                            }
                            let _ = tx.send(Ok((h.0 as isize, thread_id)));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                if msg.message == WM_QUIT_OVERVIEW_THREAD {
                                    break;
                                }
                                let _ = DispatchMessageW(&msg);
                            }
                            // DestroyWindow(h) also destroys the owned
                            // mask window; clear the stale handle first.
                            let mask = std::mem::take(&mut state().mask_hwnd);
                            if mask != 0 {
                                let _ = DestroyWindow(HWND(mask as *mut c_void));
                            }
                            let _ = DestroyWindow(h);
                            let _ = UnregisterClassW(
                                windows::core::PCWSTR(class_name.as_ptr()),
                                None,
                            );
                            let _ = UnregisterClassW(
                                windows::core::PCWSTR(mask_class_name().as_ptr()),
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
    ///
    /// When `model.anim_ms > 0` and any row will carry live thumbnails,
    /// the open is animated: the chrome fades in over pre-rendered alpha
    /// steps while every row's thumbnails glide from their windows'
    /// would-be on-screen rects to their card bodies, inactive rows
    /// fading in on top of the glide (see [`AnimPhase`]). The step
    /// pre-render happens here, BEFORE the window shows and the clock
    /// starts, so its cost never eats into the glide.
    pub fn show(&self, monitor_rect: Rect, model: OverviewModel) {
        let animate = {
            let mut s = state();
            s.selected = selection_from_model(&model);
            // Only live DWM thumbnails can glide (the chrome is static):
            // without any — AlphaDim fallback, placeholder/snapshot mode,
            // compact-only rows — the overlay just appears. The anim row
            // (kept at full opacity) is the active row.
            let anim_row = if s.backdrop == BackdropMode::AlphaDim
                || !model.rows.iter().any(|r| r.cards.iter().any(card_can_glide))
            {
                None
            } else {
                Some(model.rows.iter().position(|r| r.is_active).unwrap_or(0))
            };
            s.anim = match anim_row {
                Some(row) if model.anim_ms > 0 => {
                    let gliders = model
                        .rows
                        .iter()
                        .flat_map(|r| r.cards.iter())
                        .filter(|c| card_can_glide(c))
                        .count();
                    tracing::debug!(
                        "overview zoom start: Opening, {}ms, row {}, {} gliding thumbnails",
                        model.anim_ms,
                        row,
                        gliders
                    );
                    Some(AnimState {
                        phase: AnimPhase::Opening,
                        started: std::time::Instant::now(),
                        duration_ms: model.anim_ms,
                        easing: model.easing,
                        row,
                        ticks: 0,
                        notify: false,
                    })
                }
                _ => {
                    tracing::debug!(
                        "overview open: no zoom (anim_ms={}, animatable active row: {})",
                        model.anim_ms,
                        anim_row.is_some()
                    );
                    None
                }
            };
            s.model = model;
            s.window_rect = monitor_rect;
            s.hovered = None;
            s.visible = true;
            s.anim.is_some()
        };
        // Register thumbnails BEFORE the window shows so the very first
        // composited frame already has every gliding card at its
        // from_rect (k=0, inactive rows at opacity 0) instead of flashing
        // the settled layout.
        sync_thumbnails(self.hwnd);
        // Pre-render the staged chrome-fade frames, also BEFORE the
        // window shows: the first WM_PAINT then blits the faintest step
        // instead of full-strength chrome, and the first timer tick
        // restarts the animation clock so this one-time cost (ONE
        // full-monitor SDF render, ~40-50ms at 5120x1440 in debug, plus
        // a few ms of parallel faded copies) is hidden from the glide.
        if animate {
            build_chrome_steps(monitor_rect.width, monitor_rect.height);
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
        render_overlay(self.hwnd);
        // Foreground (with the platform helper's focus-stealing fallbacks)
        // so Esc/arrows land in the overlay without an extra click.
        let _ = crate::set_foreground_window(self.hwnd.0 as u64);
        // Re-assert HWND_TOPMOST after the foreground dance: the
        // AttachThreadInput/BringWindowToTop fallbacks can reshuffle z and
        // leave the overlay under the app windows while the open glide
        // runs (map visible behind the real windows).
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        if animate {
            unsafe {
                let _ =
                    PostMessageW(Some(self.hwnd), WM_OVERVIEW_START_ANIM, WPARAM(0), LPARAM(0));
            }
        }
        // Non-animated open is already settled: show the corner caps now
        // (no-op while an open glide runs — the settle tick shows them).
        sync_mask();
    }

    /// Replace the displayed model in place (overlay stays visible).
    /// Strict no-op while the window is hidden or a close animation is
    /// in flight (see [`apply_model_update`]).
    pub fn update_model(&self, model: OverviewModel) {
        if !apply_model_update(&mut state(), model) {
            return;
        }
        sync_thumbnails(self.hwnd);
        render_overlay(self.hwnd);
        // Card rects/thumbnails may have moved: refresh the corner caps.
        sync_mask();
    }

    /// Whether the overlay is mid-close or already hidden: a close
    /// animation is in flight, or the window hid itself and the daemon's
    /// `Dismissed` bookkeeping (which clears `overview_open`) hasn't run
    /// yet. The daemon skips model refreshes in this state.
    pub fn is_closing(&self) -> bool {
        is_closing_state(&state())
    }

    /// Hide the overlay. Marks state hidden BEFORE the OS hide so the
    /// resulting WM_KILLFOCUS doesn't emit a spurious `Dismissed`.
    /// `SW_HIDE` runs BEFORE the thumbnail teardown: unregistering first
    /// makes DWM recomposite the still-visible window with the bare map
    /// chrome (the close-flash). Also frees the pre-rendered chrome-fade
    /// step DIBs (~28MB each at 5120x1440).
    pub fn hide(&self) {
        {
            let mut s = state();
            s.visible = false;
            s.anim = None;
        }
        sync_mask();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            // Drop any queued repaint so a later composition can't
            // flash the stale map frame (cross-thread-safe, like
            // InvalidateRect).
            let _ = ValidateRect(Some(self.hwnd), None);
        }
        drop_all_thumbnails();
        free_chrome_steps();
        drop_hover_cache();
        drop_pending_hover_cache();
    }

    /// Request an ANIMATED close: every row's live thumbnails glide back
    /// toward their windows' would-be rects — the row for
    /// `target_workspace` (the activated workspace; `None` = the active
    /// row) at full opacity, the other rows fading out — while the chrome
    /// stays, then the window hides — all driven by the overlay's own
    /// WM_TIMER ticks.
    ///
    /// Deliberately non-blocking: the daemon proceeds with its
    /// focus/switch flow immediately while the overlay closes over the
    /// changing windows underneath — that compose-over-the-switch look is
    /// exactly Task View's. Falls back to an instant [`Self::hide`] when
    /// there is nothing to animate (`anim_ms == 0` or no live
    /// thumbnails); no-op when a close is already in flight.
    pub fn hide_animated(&self, target_workspace: Option<usize>) {
        let start = {
            let mut s = state();
            if !s.visible {
                return;
            }
            start_close(&mut s, target_workspace, false)
        };
        match start {
            CloseStart::Started => {
                // Re-derive the faded chrome steps from the retained
                // full frame (freed at open-settle); ticks skip step
                // blits until they exist, so this never races the timer.
                rebuild_faded_chrome_steps();
                // The glide is starting: the caps' card corners are about
                // to move out from under them.
                sync_mask();
                unsafe {
                    let _ = PostMessageW(
                        Some(self.hwnd),
                        WM_OVERVIEW_START_ANIM,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            CloseStart::AlreadyClosing => {}
            CloseStart::NotAnimatable => self.hide(),
        }
    }
}

/// Hidden or mid-close: model refreshes must leave the overlay alone.
/// During an overlay-owned close (Esc/backdrop/focus loss) the daemon's
/// `overview_open` stays true until `Dismissed` lands, so refreshes keep
/// arriving while the zoom-out runs and even after the window hid.
fn is_closing_state(s: &OverviewState) -> bool {
    !s.visible || matches!(s.anim, Some(a) if a.phase == AnimPhase::Closing)
}

/// Apply a model refresh for [`OverviewOverlay::update_model`]. Returns
/// `false` — state untouched, nothing to render — while the overlay is
/// closing or hidden ([`is_closing_state`]): a refresh landing then would
/// re-register every thumbnail at full opacity on the settled map (or on
/// the already-hidden window) and repaint the chrome, flashing the map
/// right as (or after) the zoom-out hides it.
fn apply_model_update(s: &mut OverviewState, model: OverviewModel) -> bool {
    if is_closing_state(s) {
        return false;
    }
    // Unchanged refresh: the daemon re-pushes on every window event, so
    // an identical model must be a strict no-op. Re-applying it repainted
    // a hover-free frame and re-pushed the mask layer per event — the
    // stationary-mouse hover-flicker storm.
    if s.model == model {
        return false;
    }
    // Preserve the current selection and hover if their windows survived.
    let prev_wid = s.selection_window_id();
    let hover_wid = s
        .hovered
        .and_then(|(ri, ci)| s.model.rows.get(ri)?.cards.get(ci))
        .map(|c| c.window_id);
    // Carry an in-flight zoom across the refresh: every frame
    // re-reads the model, so the new geometry simply takes over
    // mid-flight. Re-locate the animating row by workspace index
    // (rows may have been inserted/removed); the zoom is dropped
    // only when its row is gone. Without this, ANY window event
    // arriving during the 150ms open (browser title ticks, app
    // launches) snapped the animation to its final state.
    let anim_ws = s
        .anim
        .and_then(|a| s.model.rows.get(a.row).map(|r| r.workspace_index));
    s.selected = prev_wid
        .and_then(|wid| locate_card(&model, wid))
        .or_else(|| selection_from_model(&model));
    // The mouse hasn't moved: the full repaint this refresh triggers
    // rebuilds the frame from `s.hovered`, so clearing it here painted a
    // hover-free frame that the next (synthetic) WM_MOUSEMOVE patched
    // back — the visible hover flicker on every refresh.
    s.hovered = hover_wid.and_then(|wid| locate_card(&model, wid));
    s.model = model;
    let next_anim = match (s.anim, anim_ws) {
        (Some(mut a), Some(ws)) => s
            .model
            .rows
            .iter()
            .position(|r| r.workspace_index == ws)
            .map(|row| {
                a.row = row;
                a
            }),
        _ => None,
    };
    s.anim = next_anim;
    true
}

/// Outcome of [`start_close`].
enum CloseStart {
    /// A closing [`AnimState`] is installed; post `WM_OVERVIEW_START_ANIM`.
    Started,
    /// A close is already in flight: do nothing (double-close guard).
    AlreadyClosing,
    /// Nothing to animate (`anim_ms == 0` or no live thumbnails on the
    /// target row): the caller closes instantly instead.
    NotAnimatable,
}

/// Install a Closing [`AnimState`] toward `target_workspace`'s row
/// (`None` = the active row). `notify` makes the finished close send
/// `Dismissed` (overlay-initiated closes). Caller holds the state lock.
fn start_close(
    s: &mut OverviewState,
    target_workspace: Option<usize>,
    notify: bool,
) -> CloseStart {
    if matches!(s.anim, Some(a) if a.phase == AnimPhase::Closing) {
        return CloseStart::AlreadyClosing;
    }
    if s.model.anim_ms == 0 {
        tracing::debug!("overview close: anim_ms == 0, instant hide");
        return CloseStart::NotAnimatable;
    }
    let row = target_workspace
        .and_then(|ws| s.model.rows.iter().position(|r| r.workspace_index == ws))
        .or_else(|| s.model.rows.iter().position(|r| r.is_active))
        .unwrap_or(0);
    // Only registered thumbnails can glide; without any (AlphaDim
    // fallback, placeholder/snapshot mode) the close is instant.
    let gliders = s
        .model
        .rows
        .iter()
        .flat_map(|r| r.cards.iter())
        .filter(|c| card_can_glide(c) && s.thumbnails.contains_key(&c.window_id))
        .count();
    if gliders == 0 {
        tracing::debug!("overview close: no gliding thumbnails, instant hide");
        return CloseStart::NotAnimatable;
    }
    // Staged chrome fade: a close from the settled map starts at the
    // top step (the live full-strength frame is on screen, so the first
    // blit happens only when k drops below the top threshold); an
    // Esc-mid-open handover keeps the open's current step — same picker,
    // continuous k. From the settled map only the retained full frame is
    // alive (`chrome_full`); the caller rebuilds the faded copies.
    if !matches!(s.anim, Some(a) if a.phase == AnimPhase::Opening)
        && (!s.chrome_steps.is_empty() || s.chrome_full.is_some())
    {
        s.chrome_last_step = Some(CHROME_FADE_FACTORS.len() - 1);
    }
    // Esc mid-open: pick the close's start time so its shared motion
    // parameter `k = 1 - ease(t)` equals the open's current
    // `k = ease(t_open)` — thumbnail positions hand over without a jump.
    // ease(t0) = 1 - e  =>  t0 = ease_inverse(1 - e).
    let started = match s.anim {
        Some(a) if a.phase == AnimPhase::Opening => {
            let e = apply_easing(a.easing, anim_progress(&a));
            let t0 = ease_inverse(s.model.easing, 1.0 - e);
            let offset = std::time::Duration::from_secs_f32(
                s.model.anim_ms as f32 * t0.clamp(0.0, 1.0) / 1000.0,
            );
            std::time::Instant::now()
                .checked_sub(offset)
                .unwrap_or_else(std::time::Instant::now)
        }
        _ => std::time::Instant::now(),
    };
    tracing::debug!(
        "overview zoom start: Closing, {}ms, row {}, {} gliding thumbnails",
        s.model.anim_ms,
        row,
        gliders
    );
    s.anim = Some(AnimState {
        phase: AnimPhase::Closing,
        started,
        duration_ms: s.model.anim_ms,
        easing: s.model.easing,
        row,
        ticks: 0,
        notify,
    });
    CloseStart::Started
}

/// Overlay-initiated dismissal (Esc, backdrop click, focus loss): run the
/// close animation to completion and only THEN send `Dismissed`, so the
/// daemon's subsequent `hide()` finds the window already hidden and just
/// does its bookkeeping. Sends `Dismissed` immediately when there is
/// nothing to animate (the daemon then hides instantly, as before).
fn user_close(hwnd: HWND) {
    let start = {
        let mut s = state();
        if !s.visible {
            return;
        }
        start_close(&mut s, None, true)
    };
    match start {
        CloseStart::Started => {
            // Faded chrome steps were freed at open-settle: rebuild them
            // from the retained full frame before the fade-down starts.
            rebuild_faded_chrome_steps();
            // Hide the caps before the glide moves the thumbnails.
            sync_mask();
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_OVERVIEW_START_ANIM, WPARAM(0), LPARAM(0));
            }
        }
        CloseStart::AlreadyClosing => {}
        CloseStart::NotAnimatable => send_event(OverviewEvent::Dismissed),
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
        free_chrome_steps();
        drop_hover_cache();
        drop_pending_hover_cache();
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

/// Whether a card's preview can carry the zoom animation: it requests a
/// live thumbnail, knows its window's real rect, and is large enough for
/// a thumbnail body.
fn card_can_glide(card: &OverviewCard) -> bool {
    card.live && card.from_rect.is_some() && thumb_body(&card.rect).is_some()
}

/// Diff the registered thumbnails against the current model: register
/// new live cards, retarget surviving ones via `update`, drop removed
/// ones. All DWM calls happen outside the state lock.
///
/// Animation-aware: while a zoom animation runs, every card with a
/// `from_rect` gets its dest rect interpolated between the window's
/// would-be rect and its card body at the animation's current `k` (so a
/// model refresh mid-flight re-registers/retargets without snapping).
/// Rows other than the anim row additionally register at
/// [`inactive_row_opacity`]`(k)` so they fade on top of the glide;
/// cards without a `from_rect` keep static dest rects (and the same
/// fade when inactive). The painted chrome fades separately through the
/// pre-rendered step frames, see [`AnimPhase`].
fn sync_thumbnails(hwnd: HWND) {
    let (mode, targets) = {
        let s = state();
        let anim = s.anim;
        let k = anim.map(|a| sync_k(&a));
        let targets: Vec<(u64, Rect, u8)> = s
            .model
            .rows
            .iter()
            .enumerate()
            .flat_map(|(ri, row)| row.cards.iter().map(move |c| (ri, c)))
            .filter(|(_, c)| c.live)
            .filter_map(|(ri, c)| {
                // Fill the whole body: DWM stretches the source to
                // rcDestination, and a filled card reads better than a
                // letterboxed one even when the aspect ratio is off.
                let body = thumb_body(&c.rect)?;
                let (dest, opacity) = match (anim, k) {
                    (Some(a), Some(k)) => {
                        let dest = match c.from_rect {
                            Some(from) => lerp_rect(&from, &body, k),
                            None => body,
                        };
                        let opacity =
                            if a.row == ri { OPAQUE } else { inactive_row_opacity(k) };
                        (dest, opacity)
                    }
                    _ => (body, OPAQUE),
                };
                Some((c.window_id, dest, opacity))
            })
            .collect();
        (s.backdrop, targets)
    };
    let mut existing = std::mem::take(&mut state().thumbnails);
    if mode == BackdropMode::AlphaDim {
        drop(existing); // layered fallback: no thumbnails, placeholder bodies
        return;
    }
    let mut next: HashMap<u64, ThumbnailHandle> = HashMap::with_capacity(targets.len());
    for (wid, dest, opacity) in targets {
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
        if let Err(e) = thumbnail::update(handle.as_isize(), dest, opacity, true) {
            tracing::warn!("overview thumbnail update({wid}) failed: {e}");
            continue; // handle drops here -> unregistered
        }
        next.insert(wid, handle);
    }
    drop(existing); // windows gone from the model: unregister
    state().thumbnails = next;
}

/// One zoom-animation timer tick: retarget every gliding thumbnail to
/// its interpolated rect and opacity-ramp the non-anim rows' thumbnails
/// at [`inactive_row_opacity`]`(k)` on top (anim row stays opaque). The
/// painted chrome NEVER re-renders here — when `k` crosses into a new
/// [`chrome_step_for_k`] step, the tick `BitBlt`s that pre-rendered
/// fade frame (a cheap blit, no SDF pass), so the glide actually runs
/// at the timer cadence instead of being throttled to a few
/// full-monitor software renders.
///
/// On completion (`t >= 1`) the timer is killed and the phase resolves:
/// an open settles every thumbnail on its exact card body; a close hides
/// the window and, for overlay-initiated closes, sends `Dismissed` only
/// now (see [`user_close`]).
fn on_anim_tick(hwnd: HWND) {
    let (anim, updates) = {
        let mut s = state();
        let anim = match s.anim.as_mut() {
            Some(a) => {
                // First tick of an open: the initial full-chrome WM_PAINT
                // (the one expensive software render, processed before any
                // WM_TIMER) may have eaten tens of ms; restart the clock so
                // the glide plays its full duration. Closes never restart —
                // an Esc-mid-open close carries a back-dated start time
                // that keeps `k` continuous.
                if a.ticks == 0 && a.phase == AnimPhase::Opening {
                    a.started = std::time::Instant::now();
                }
                a.ticks += 1;
                *a
            }
            None => {
                // Animation cancelled (instant hide): stop ticking.
                drop(s);
                tracing::debug!("overview zoom cancelled: timer stopped");
                unsafe {
                    let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
                }
                return;
            }
        };
        let k = anim_k(&anim);
        // Staged chrome fade: blit the pre-rendered step for this k when
        // it changes. Done under the state lock so a concurrent hide()'s
        // free_chrome_steps (which also takes the lock) can never delete
        // the DIB mid-blit.
        let step = chrome_step_for_k(k);
        if s.chrome_last_step != Some(step) {
            if let Some(dc) = s.chrome_steps.get(step).map(|cs| cs.mem_dc) {
                s.chrome_last_step = Some(step);
                unsafe {
                    let hdc = GetDC(Some(hwnd));
                    let _ = BitBlt(
                        hdc,
                        0,
                        0,
                        s.window_rect.width,
                        s.window_rect.height,
                        Some(HDC(dc as *mut c_void)),
                        0,
                        0,
                        SRCCOPY,
                    );
                    ReleaseDC(Some(hwnd), hdc);
                }
            }
        }
        let fade = inactive_row_opacity(k);
        let updates: Vec<(isize, Rect, u8)> = s
            .model
            .rows
            .iter()
            .enumerate()
            .flat_map(|(ri, row)| row.cards.iter().map(move |c| (ri, c)))
            .filter_map(|(ri, c)| {
                let body = thumb_body(&c.rect)?;
                let handle = s.thumbnails.get(&c.window_id)?.as_isize();
                // Every card with a from_rect glides at the shared `k`;
                // cards without one stay static.
                let dest = match c.from_rect {
                    Some(from) => lerp_rect(&from, &body, k),
                    None => body,
                };
                // Non-anim rows: opacity rides `k` on top of the glide.
                let opacity = if ri == anim.row { OPAQUE } else { fade };
                Some((handle, dest, opacity))
            })
            .collect();
        (anim, updates)
    };
    // DWM calls outside the state lock.
    for (handle, dest, opacity) in updates {
        let _ = thumbnail::update(handle, dest, opacity, true);
    }
    if anim_progress(&anim) < 1.0 {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
    }
    tracing::debug!(
        "overview zoom finished: {:?} after {} ticks",
        anim.phase,
        anim.ticks
    );
    match anim.phase {
        AnimPhase::Opening => {
            {
                let mut s = state();
                s.anim = None;
                s.chrome_last_step = None;
            }
            // Settle every thumbnail on its exact card body.
            sync_thumbnails(hwnd);
            // Adopt the show-time hover cache when the state is
            // unchanged — the settled full-strength frame is already on
            // screen, so the settle costs no SDF render at all. A
            // refresh that landed mid-open invalidates it: repaint the
            // live model at full strength (which rebuilds the cache).
            if !adopt_pending_hover_cache() {
                render_overlay(hwnd);
            }
            // Keep only the full-strength frame for the close-start
            // rebuild; the faded step DIBs are freed here (~85MB at
            // 5120x1440 saved while the overview sits open).
            demote_chrome_steps_to_full();
            // Settled: the corner caps are valid now.
            sync_mask();
        }
        AnimPhase::Closing => {
            // Clear the anim and the visible flag under ONE lock: a
            // daemon refresh sneaking between the two would see "open,
            // settled" and snap the full map back (thumbnails at the
            // card bodies, full opacity) right before the hide.
            {
                let mut s = state();
                s.anim = None;
                s.visible = false;
            }
            // Hide FIRST, then unregister: tearing the thumbnails down on
            // the still-visible window made DWM recomposite the bare map
            // chrome for a few ms — the "map reappears then hides" flash.
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
                // Drop any repaint queued during the close so a later
                // composition can't flash the stale map frame.
                let _ = ValidateRect(Some(hwnd), None);
            }
            drop_all_thumbnails();
            free_chrome_steps();
            drop_hover_cache();
            drop_pending_hover_cache();
            sync_mask();
            if anim.notify {
                send_event(OverviewEvent::Dismissed);
            }
        }
    }
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
    // Close animation: input is ignored (the close is committed).
    // Open animation: only Esc may interrupt (jumps into the close).
    match state().anim.map(|a| a.phase) {
        Some(AnimPhase::Closing) => return true,
        Some(AnimPhase::Opening) if vk != VK_ESCAPE.0 => return true,
        _ => {}
    }
    if vk == VK_ESCAPE.0 {
        // Overlay-owned close: animate out, send Dismissed at the end.
        user_close(hwnd);
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
            // The accent ring moved cards: its corner-arc caps move too.
            sync_mask();
        }
        return true;
    }
    false
}

/// Handle mouse buttons. `middle` selects the close gesture.
fn handle_mouse_button(hwnd: HWND, x: i32, y: i32, middle: bool) {
    let (card_hit, row_hit) = {
        let s = state();
        // Mid-animation clicks are ignored: the thumbnails are still in
        // flight (open) or the close is already committed.
        if !s.visible || s.anim.is_some() {
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
        // Backdrop click: overlay-owned animated close (like Esc).
        (None, None) => user_close(hwnd),
    }
}

/// Hover guard: the `(old, new)` card pair to repaint for a hit-test
/// result, or `None` when the hovered card did not change — plain mouse
/// movement inside (or outside) the same card must repaint nothing.
type CardIdx = Option<(usize, usize)>;
fn hover_transition(prev: CardIdx, hit: CardIdx) -> Option<(CardIdx, CardIdx)> {
    (hit != prev).then_some((prev, hit))
}

fn handle_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let (old, new) = {
        let mut s = state();
        if !s.visible || s.anim.is_some() {
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
        let Some(transition) = hover_transition(s.hovered, hit) else {
            return;
        };
        s.hovered = hit;
        transition
    };
    // Cached-base repaint of just the two affected cards; the full
    // render only happens when the cache isn't available (AlphaDim).
    if !hover_repaint(hwnd, old, new) {
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
            // Click-away: overlay-owned animated close, but only while
            // logically visible — hide() clears the flag first so
            // daemon-driven hides don't echo a second Dismissed. During a
            // close the daemon's own focus switch steals focus: not a
            // dismissal (start_close returns AlreadyClosing).
            user_close(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // double-buffered WM_PAINT, no erase
        WM_OVERVIEW_START_ANIM => {
            // SetTimer on the overlay's own thread so WM_TIMER lands here.
            unsafe {
                SetTimer(Some(hwnd), ANIM_TIMER_ID, ANIM_TICK_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == ANIM_TIMER_ID {
                on_anim_tick(hwnd);
            }
            LRESULT(0)
        }
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
            handle_mouse_button(hwnd, x, y, msg == WM_MBUTTONDOWN);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            handle_mouse_move(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let old = {
                let mut s = state();
                s.mouse_tracking_armed = false;
                s.hovered.take()
            };
            if old.is_some() && !hover_repaint(hwnd, old, None) {
                render_overlay(hwnd);
            }
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGING => {
            // Pin the overlay to the monitor rect it was shown at: a third-party
            // Alt-drag tool (e.g. AltSnap) can otherwise move this interactive
            // overlay. Override the requested move/size back to `window_rect`
            // while visible. `try_lock` avoids any chance of a re-entrant
            // deadlock if a SetWindowPos on this window fires while STATE is
            // held; SWP_NOMOVE/NOSIZE callers (the topmost re-assert) are
            // unaffected because the system ignores the coords then.
            let rect = STATE.try_lock().ok().filter(|s| s.visible).map(|s| s.window_rect);
            if let (Some(r), Some(wp)) = (rect, (lparam.0 as *mut WINDOWPOS).as_mut()) {
                wp.x = r.x;
                wp.y = r.y;
                wp.cx = r.width;
                wp.cy = r.height;
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
        // Model/selection changed: the cached working frame is stale —
        // the next WM_PAINT must re-render (and rebuild the cache)
        // instead of re-blitting it.
        state().needs_full_render = true;
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
    /// Raw DIB bits (`w*h` u32 pixels), valid while `hbitmap` lives.
    /// Lets [`build_chrome_steps`] derive the faded step copies from the
    /// rendered frame without another SDF pass.
    bits: *mut u32,
}

/// Everything one frame render needs besides the bitmap dimensions.
struct FrameInput<'a> {
    model: &'a OverviewModel,
    hovered: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
    mode: BackdropMode,
    thumb_wids: &'a HashSet<u64>,
}

/// Render the frame up to (but not including) the premultiply: shapes
/// and GDI content settled in the DIB (straight color), region alpha in
/// the returned plane. Snapshot bodies get their bottom corners masked
/// back to the card rounding (a square `StretchDIBits` would overflow
/// it). Callers finish with [`finish_alpha`] and release with
/// [`release_frame_dib`].
unsafe fn render_frame_straight(
    w: i32,
    h: i32,
    input: &FrameInput,
) -> Option<(FrameDib, Vec<u8>)> {
    let bmi = top_down_bmi(w, h);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;

    let hdc_screen = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(hdc_screen));
    let old_bmp = SelectObject(mem_dc, hbitmap.into());

    // 1. Backdrop clear (GDI), flushed before the direct-bit shape pass.
    fill(mem_dc, &Rect::new(0, 0, w, h), BACKDROP_BG);
    let _ = GdiFlush();

    let backdrop_alpha = match input.mode {
        BackdropMode::Acrylic => 0, // tint comes from the accent policy
        BackdropMode::Blur => BLUR_TINT_ALPHA,
        BackdropMode::AlphaDim => BACKDROP_ALPHA,
    };
    let mut alpha = vec![backdrop_alpha; (w * h) as usize];

    // 2. Shapes: anti-aliased rounded panels/cards/outline into the bits.
    let saves = {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        draw_shapes(pixels, &mut alpha, w, h, input);
        // Save the pixels a square snapshot blit is about to overflow.
        save_snapshot_corners(pixels, w, h, input)
    };

    // 3. Text + icons via GDI, flushed before the direct-bit passes below.
    draw_content(mem_dc, input.model, input.thumb_wids);
    let _ = GdiFlush();

    // 4. Mask the snapshots' bottom corners back to the card rounding.
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
    mask_snapshot_corners(pixels, w, h, &saves);

    Some((
        FrameDib {
            hdc_screen,
            mem_dc,
            hbitmap,
            old_bmp,
            bits: bits as *mut u32,
        },
        alpha,
    ))
}

/// Render the frame into a top-down 32bpp DIB: SDF-anti-aliased shapes
/// straight into the bits (color plane + region-alpha plane), GDI for
/// text and icons only, then premultiply. Caller must pass the result to
/// [`release_frame_dib`] after use.
unsafe fn build_frame_dib(w: i32, h: i32, input: &FrameInput) -> Option<FrameDib> {
    let (dib, mut alpha) = render_frame_straight(w, h, input)?;
    let pixels = std::slice::from_raw_parts_mut(dib.bits, (w * h) as usize);
    finish_alpha(pixels, &mut alpha, w, h, input.model);
    Some(dib)
}

/// Saved straight-color pixels under one snapshot card's bottom-corner
/// squares, captured before the GDI pass draws the (square) snapshot.
struct CornerSave {
    /// Snapshot dest rect (the card's thumb body).
    body: Rect,
    /// Body rounding radius: card radius minus the body inset, so the
    /// mask follows the inner edge of the card frame.
    radius: f32,
    squares: [Rect; 2],
    saved: [Vec<u32>; 2],
}

/// Copy the bottom-corner pixels of every snapshot card out of the
/// frame before the GDI snapshot blits overwrite them. Targets mirror
/// [`draw_content`]'s snapshot condition, minus the cache-hit check —
/// saving a corner that ends up untouched restores identical pixels.
fn save_snapshot_corners(pixels: &[u32], w: i32, h: i32, input: &FrameInput) -> Vec<CornerSave> {
    let mut saves = Vec::new();
    let cards = input
        .model
        .rows
        .iter()
        .flat_map(|r| r.cards.iter())
        .filter(|c| c.snapshot && !input.thumb_wids.contains(&c.window_id));
    for card in cards {
        let Some(body) = thumb_body(&card.rect) else {
            continue;
        };
        let radius = (card_radius(&card.rect) - THUMB_INSET as f32).max(0.0);
        let squares = bottom_corner_squares(&body, radius);
        let saved = squares.map(|sq| {
            let (x0, x1, y0, y1) = clip_rect(&sq, w, h);
            let mut buf = Vec::with_capacity(((x1 - x0).max(0) * (y1 - y0).max(0)) as usize);
            for y in y0..y1 {
                let row = (y * w) as usize;
                buf.extend_from_slice(&pixels[row + x0 as usize..row + x1 as usize]);
            }
            buf
        });
        saves.push(CornerSave {
            body,
            radius,
            squares,
            saved,
        });
    }
    saves
}

/// Blend the snapshot corners back toward the saved pixels outside the
/// body's bottom rounding: `pixel = lerp(saved, snapshot, coverage)` —
/// the square overflow disappears, anti-aliased along the arc.
fn mask_snapshot_corners(pixels: &mut [u32], w: i32, h: i32, saves: &[CornerSave]) {
    for save in saves {
        for (sq, saved) in save.squares.iter().zip(&save.saved) {
            let (x0, x1, y0, y1) = clip_rect(sq, w, h);
            let mut s = saved.iter();
            for y in y0..y1 {
                let row = (y * w) as usize;
                for x in x0..x1 {
                    let &base = s.next().expect("saved buffer matches clip");
                    let cov = bottom_round_cov(x, y, &save.body, save.radius);
                    if cov < 1.0 {
                        let i = row + x as usize;
                        pixels[i] = lerp_color(base, pixels[i], cov);
                    }
                }
            }
        }
    }
}

/// 32bpp top-down BITMAPINFO for the frame and step DIB sections.
fn top_down_bmi(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
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
    }
}

unsafe fn release_frame_dib(dib: FrameDib) {
    SelectObject(dib.mem_dc, dib.old_bmp);
    let _ = DeleteDC(dib.mem_dc);
    ReleaseDC(None, dib.hdc_screen);
    let _ = DeleteObject(dib.hbitmap.into());
}

/// Scale a PREMULTIPLIED frame by a global fade factor: all four
/// channels multiply by the same factor, which uniformly dims every
/// region's contribution to the DWM composition (translucent backdrop
/// tint and panels included). No-op at `fade >= 1.0`.
fn apply_frame_fade(pixels: &mut [u32], fade: f32) {
    let f = (fade.clamp(0.0, 1.0) * 256.0).round() as u32;
    if f >= 256 {
        return;
    }
    for px in pixels.iter_mut() {
        let a = ((*px >> 24) * f) >> 8;
        let r = (((*px >> 16) & 0xFF) * f) >> 8;
        let g = (((*px >> 8) & 0xFF) * f) >> 8;
        let b = ((*px & 0xFF) * f) >> 8;
        *px = a << 24 | r << 16 | g << 8 | b;
    }
}

/// Pre-render the staged chrome-fade DIBs, one per
/// [`CHROME_FADE_FACTORS`] entry, from the current state. Called from
/// `show()` BEFORE the window shows and the animation clock starts.
/// The expensive full-monitor SDF render (~40-50ms at 5120x1440 in
/// debug) runs exactly ONCE — the full-strength last step; every other
/// step is a copy of that premultiplied frame scaled by its fade factor
/// ([`apply_frame_fade`]), built on parallel threads. The copies are
/// memory-bandwidth bound (a few ms for ~28MB each), so the whole
/// pre-render adds roughly one SDF pass to time-to-first-frame.
/// All-or-nothing: a failed step frees the built ones and the open
/// falls back to full-strength chrome at once. Memory: w*h*4 bytes per
/// step (~28MB at 5120x1440); the faded copies live only while the
/// open glide runs (freed at settle, rebuilt at close-start), the full
/// frame and the primed hover cache persist until hide.
///
/// The same render also primes the hover cache ([`PendingHoverCache`]):
/// the straight pixel/alpha buffers are kept and a copy of the
/// premultiplied frame becomes the cache's working DIB, so the
/// open-settle needs no second SDF pass when the model is unchanged.
fn build_chrome_steps(w: i32, h: i32) {
    free_chrome_steps(); // defensive: a re-show without hide would leak
    drop_pending_hover_cache();
    if w <= 0 || h <= 0 {
        return;
    }
    let begun = std::time::Instant::now();
    let (model, selected, mode, thumb_wids) = {
        let s = state();
        (
            s.model.clone(),
            s.selected,
            s.backdrop,
            s.thumbnails.keys().copied().collect::<HashSet<u64>>(),
        )
    };
    let input = FrameInput {
        model: &model,
        hovered: None,
        selected,
        mode,
        thumb_wids: &thumb_wids,
    };
    let Some((full, mut alpha)) = (unsafe { render_frame_straight(w, h, &input) }) else {
        tracing::warn!(
            "overview chrome fade: step pre-render failed, falling back to chrome-at-once"
        );
        return;
    };
    let len = w as usize * h as usize;
    // Keep the straight buffers (pre-premultiply pixels + region alpha):
    // the hover cache adopts them at open-settle, eliminating the second
    // full-monitor SDF render per open.
    let pixels = unsafe { std::slice::from_raw_parts_mut(full.bits, len) };
    let base_pixels = pixels.to_vec();
    let base_alpha = alpha.clone();
    finish_alpha(pixels, &mut alpha, w, h, &model);
    // Keep the DIB selected into its memory DC for the tick blits; only
    // the screen DC is released.
    unsafe { ReleaseDC(None, full.hdc_screen) };
    let rendered = begun.elapsed();
    // GdiFlush already ran in render_frame_straight: the bits are settled.
    let src: &[u32] = unsafe { std::slice::from_raw_parts(full.bits, len) };

    // The last factor is 1.0 (test-enforced) — that's `full` itself;
    // every earlier factor gets a blank DIB filled by copy + fade below.
    let (_, faded_factors) = CHROME_FADE_FACTORS
        .split_last()
        .expect("CHROME_FADE_FACTORS is non-empty");
    let full_step = ChromeStep {
        mem_dc: full.mem_dc.0 as isize,
        hbitmap: full.hbitmap.0 as isize,
        old_bmp: full.old_bmp.0 as isize,
        bits: full.bits as isize,
    };
    let mut steps = Vec::with_capacity(CHROME_FADE_FACTORS.len());
    for _ in faded_factors {
        match unsafe { create_step_dib(w, h) } {
            Some(st) => steps.push(st),
            None => {
                steps.push(full_step);
                for st in steps {
                    unsafe { free_chrome_step(st) };
                }
                tracing::warn!(
                    "overview chrome fade: step pre-render failed, falling back to chrome-at-once"
                );
                return;
            }
        }
    }
    // One more copy of the full premultiplied frame: the hover cache's
    // working DIB (mutated by hover repaints once settled — it can't
    // share the full step, which must stay pristine for close blits).
    let pending_dib = unsafe { create_step_dib(w, h) };
    // Each thread owns one destination slice; the source is shared
    // read-only. Bandwidth-bound, so this is a few ms even in debug.
    std::thread::scope(|scope| {
        let copies = steps
            .iter()
            .zip(faded_factors.iter().copied())
            .chain(pending_dib.iter().map(|st| (st, 1.0)));
        for (st, fade) in copies {
            let buf = unsafe { std::slice::from_raw_parts_mut(st.bits as *mut u32, len) };
            scope.spawn(move || {
                buf.copy_from_slice(src);
                apply_frame_fade(buf, fade);
            });
        }
    });
    steps.push(full_step);
    let pending = pending_dib.map(|st| PendingHoverCache {
        cache: HoverCache {
            work_dc: st.mem_dc,
            work_bmp: st.hbitmap,
            work_old: st.old_bmp,
            work_bits: st.bits,
            work_alpha: alpha,
            base_pixels,
            base_alpha,
            w,
            h,
        },
        model,
        selected,
        thumb_wids,
    });
    tracing::debug!(
        "overview chrome fade: {} steps pre-rendered in {:?} (SDF render {:?}, faded copies {:?}, hover cache primed: {})",
        steps.len(),
        begun.elapsed(),
        rendered,
        begun.elapsed() - rendered,
        pending.is_some()
    );
    let mut s = state();
    s.chrome_last_step = Some(0); // step 0 is what the first WM_PAINT blits
    s.chrome_steps = steps;
    s.pending_hover_cache = pending;
}

/// Create one blank step DIB selected into its own memory DC; the raw
/// bits pointer for the copy + fade fill rides in `ChromeStep::bits`.
unsafe fn create_step_dib(w: i32, h: i32) -> Option<ChromeStep> {
    let bmi = top_down_bmi(w, h);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let mem_dc = CreateCompatibleDC(None);
    let old_bmp = SelectObject(mem_dc, hbitmap.into());
    Some(ChromeStep {
        mem_dc: mem_dc.0 as isize,
        hbitmap: hbitmap.0 as isize,
        old_bmp: old_bmp.0 as isize,
        bits: bits as isize,
    })
}

/// Free one fade step's GDI objects.
unsafe fn free_chrome_step(st: ChromeStep) {
    let dc = HDC(st.mem_dc as *mut c_void);
    SelectObject(dc, HGDIOBJ(st.old_bmp as *mut c_void));
    let _ = DeleteDC(dc);
    let _ = DeleteObject(HGDIOBJ(st.hbitmap as *mut c_void));
}

/// Free every pre-rendered fade step, the retained full-strength frame
/// included (hide, close finish, overlay drop). Everything is taken
/// under the state lock and every blit also runs under it, so the GDI
/// deletes can never race an in-flight blit.
fn free_chrome_steps() {
    let (steps, full) = {
        let mut s = state();
        s.chrome_last_step = None;
        (std::mem::take(&mut s.chrome_steps), s.chrome_full.take())
    };
    for st in steps.into_iter().chain(full) {
        unsafe { free_chrome_step(st) };
    }
}

/// At open-settle, pop the full-strength frame into `chrome_full` and
/// return the faded steps for freeing (pure bookkeeping for tests; the
/// caller frees). Holding only the full frame while the overview sits
/// open saves 3x w*h*4 bytes (~85MB at 5120x1440); the close-start
/// rebuild re-derives the faded copies. No-op unless the full step set
/// is present.
fn take_faded_steps(s: &mut OverviewState) -> Vec<ChromeStep> {
    if s.chrome_steps.len() != CHROME_FADE_FACTORS.len() {
        return Vec::new();
    }
    let mut steps = std::mem::take(&mut s.chrome_steps);
    let full = steps.pop().expect("step set is non-empty");
    if let Some(old) = s.chrome_full.replace(full) {
        steps.push(old); // defensive: free a stale retained frame too
    }
    steps
}

/// [`take_faded_steps`] against the global state, freeing the result.
fn demote_chrome_steps_to_full() {
    let faded = take_faded_steps(&mut state());
    for st in faded {
        unsafe { free_chrome_step(st) };
    }
}

/// Regenerate the faded step DIBs from the full-strength frame retained
/// at open-settle so the close can run its staged fade-down: the same
/// parallel memcpy+fade as the show-time pre-render (a few ms,
/// bandwidth-bound). All-or-nothing — on failure everything is freed
/// and the close keeps full-strength chrome (mirrors the open's
/// chrome-at-once fallback). No-op when the show-time set is still
/// alive (Esc-mid-open) or nothing was retained. Runs entirely under
/// the state lock, so blits and frees can never race it.
fn rebuild_faded_chrome_steps() {
    let mut s = state();
    if !s.chrome_steps.is_empty() {
        return;
    }
    let Some(full) = s.chrome_full.take() else {
        return;
    };
    let len = s.window_rect.width as usize * s.window_rect.height as usize;
    if full.bits == 0 || len == 0 {
        s.chrome_full = Some(full);
        return;
    }
    let (w, h) = (s.window_rect.width, s.window_rect.height);
    let src: &[u32] = unsafe { std::slice::from_raw_parts(full.bits as *const u32, len) };
    let (_, faded_factors) = CHROME_FADE_FACTORS
        .split_last()
        .expect("CHROME_FADE_FACTORS is non-empty");
    let mut steps = Vec::with_capacity(CHROME_FADE_FACTORS.len());
    for _ in faded_factors {
        match unsafe { create_step_dib(w, h) } {
            Some(st) => steps.push(st),
            None => {
                steps.push(full);
                for st in steps {
                    unsafe { free_chrome_step(st) };
                }
                tracing::warn!(
                    "overview chrome fade: close-start step rebuild failed, chrome stays at full strength"
                );
                return;
            }
        }
    }
    std::thread::scope(|scope| {
        for (st, &fade) in steps.iter().zip(faded_factors) {
            let buf = unsafe { std::slice::from_raw_parts_mut(st.bits as *mut u32, len) };
            scope.spawn(move || {
                buf.copy_from_slice(src);
                apply_frame_fade(buf, fade);
            });
        }
    });
    steps.push(full);
    s.chrome_steps = steps;
}

/// WM_PAINT for the accent (blur) modes: double-buffer the frame in the
/// DIB and `BitBlt` it to the window DC — SRCCOPY carries the alpha
/// bytes, so DWM composites the chrome over the blurred backdrop.
/// While a zoom runs with pre-rendered fade steps, the paint blits the
/// step for the current `k` instead — a stray WM_PAINT mid-glide (model
/// refresh, occlusion) must not flash full-strength chrome.
unsafe fn paint_frame(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    // Staged path first. The blit runs under the state lock so a
    // concurrent free_chrome_steps can't delete the DIB mid-blit.
    let staged = {
        let mut s = state();
        let step_dc = match s.anim {
            Some(a) if s.visible && !s.chrome_steps.is_empty() => {
                let step = chrome_step_for_k(sync_k(&a));
                s.chrome_steps.get(step).map(|cs| (step, cs.mem_dc))
            }
            _ => None,
        };
        if let Some((step, dc)) = step_dc {
            let _ = BitBlt(
                hdc,
                0,
                0,
                s.window_rect.width,
                s.window_rect.height,
                Some(HDC(dc as *mut c_void)),
                0,
                0,
                SRCCOPY,
            );
            s.chrome_last_step = Some(step);
            true
        } else {
            false
        }
    };
    if staged {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    // Settled fast path: the cached working frame mirrors the window
    // content — a stray WM_PAINT (occlusion, z churn) re-blits it
    // instead of re-running the SDF render.
    let reblitted = {
        let s = state();
        match &s.hover_cache {
            Some(c)
                if s.visible
                    && !s.needs_full_render
                    && c.w == s.window_rect.width
                    && c.h == s.window_rect.height =>
            {
                let _ = BitBlt(
                    hdc,
                    0,
                    0,
                    c.w,
                    c.h,
                    Some(HDC(c.work_dc as *mut c_void)),
                    0,
                    0,
                    SRCCOPY,
                );
                true
            }
            _ => false,
        }
    };
    if reblitted {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let (model, selected, rect, mode, visible, thumb_wids) = {
        let s = state();
        (
            s.model.clone(),
            s.selected,
            s.window_rect,
            s.backdrop,
            s.visible,
            s.thumbnails.keys().copied().collect::<HashSet<u64>>(),
        )
    };
    if visible && mode != BackdropMode::AlphaDim && rect.width > 0 && rect.height > 0 {
        // The cache base must be hover-free: the live hover is re-applied
        // as a card repaint inside install_hover_cache_and_blit.
        let input = FrameInput {
            model: &model,
            hovered: None,
            selected,
            mode,
            thumb_wids: &thumb_wids,
        };
        if let Some(cache) = build_hover_cache(rect.width, rect.height, &input) {
            install_hover_cache_and_blit(hdc, cache);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Install a freshly-built hover cache, re-apply the live hover state
/// (the cache base is hover-free), and blit the working frame — all
/// under one state lock so a concurrent hide can't free it mid-blit.
unsafe fn install_hover_cache_and_blit(hdc: HDC, mut cache: HoverCache) {
    let stale = {
        let mut guard = state();
        let s = &mut *guard;
        if s.visible {
            if let Some(card) = s
                .hovered
                .and_then(|(ri, ci)| s.model.rows.get(ri)?.cards.get(ci))
            {
                let covered = body_is_covered(card, &s.thumbnails);
                repaint_card_in_cache(&mut cache, card, true, covered, s.model.accent_bgr);
            }
            let _ = BitBlt(
                hdc,
                0,
                0,
                cache.w,
                cache.h,
                Some(HDC(cache.work_dc as *mut c_void)),
                0,
                0,
                SRCCOPY,
            );
            s.needs_full_render = false;
            s.hover_cache.replace(cache)
        } else {
            // Hidden while rendering: don't resurrect 28MB of cache.
            Some(cache)
        }
    };
    if let Some(old) = stale {
        free_hover_cache(old);
    }
}

/// Build the hover cache from one full render: base = the hover-free
/// straight frame, working DIB = its premultiplied copy. The DIB stays
/// selected into its memory DC for the rect blits; only the screen DC
/// is released here.
unsafe fn build_hover_cache(w: i32, h: i32, input: &FrameInput) -> Option<HoverCache> {
    debug_assert!(input.hovered.is_none(), "cache base must be hover-free");
    let (dib, mut alpha) = render_frame_straight(w, h, input)?;
    let len = (w * h) as usize;
    let pixels = std::slice::from_raw_parts_mut(dib.bits, len);
    let base_pixels = pixels.to_vec();
    let base_alpha = alpha.clone();
    finish_alpha(pixels, &mut alpha, w, h, input.model);
    ReleaseDC(None, dib.hdc_screen);
    Some(HoverCache {
        work_dc: dib.mem_dc.0 as isize,
        work_bmp: dib.hbitmap.0 as isize,
        work_old: dib.old_bmp.0 as isize,
        work_bits: dib.bits as isize,
        work_alpha: alpha,
        base_pixels,
        base_alpha,
        w,
        h,
    })
}

/// Free a hover cache's GDI objects (same handle shape as a chrome step).
unsafe fn free_hover_cache(cache: HoverCache) {
    free_chrome_step(ChromeStep {
        mem_dc: cache.work_dc,
        hbitmap: cache.work_bmp,
        old_bmp: cache.work_old,
        bits: cache.work_bits,
    });
}

/// Take and free the cached frame (hide, close finish, overlay drop).
/// Taken under the state lock — every blit also runs under it, so the
/// GDI deletes can never race an in-flight blit.
fn drop_hover_cache() {
    let cache = state().hover_cache.take();
    if let Some(c) = cache {
        unsafe { free_hover_cache(c) };
    }
}

/// Take and free the show-time pending hover cache (hide, close finish,
/// overlay drop, re-show).
fn drop_pending_hover_cache() {
    let pending = state().pending_hover_cache.take();
    if let Some(p) = pending {
        unsafe { free_hover_cache(p.cache) };
    }
}

/// Whether the show-time pending hover cache still reflects the live
/// state at open-settle: same model, selection, thumbnail set, and
/// frame size, with no hover baked anywhere (mouse moves are blocked
/// mid-anim, so `hovered` should be `None`). Pure for tests.
fn pending_matches_state(p: &PendingHoverCache, s: &OverviewState) -> bool {
    let wids: HashSet<u64> = s.thumbnails.keys().copied().collect();
    s.visible
        && s.hovered.is_none()
        && p.model == s.model
        && p.selected == s.selected
        && p.thumb_wids == wids
        && p.cache.w == s.window_rect.width
        && p.cache.h == s.window_rect.height
}

/// Install the show-time hover cache at open-settle when it still
/// matches the live state: the settled full-strength frame is already
/// on screen (the final tick blitted the top chrome step, whose pixels
/// the cache duplicates), so no re-render OR blit is needed. Returns
/// false — pending freed, nothing installed — when the snapshot went
/// stale (mid-open refresh); the caller re-renders.
fn adopt_pending_hover_cache() -> bool {
    let (stale, adopted) = {
        let mut s = state();
        let Some(p) = s.pending_hover_cache.take() else {
            return false;
        };
        if pending_matches_state(&p, &s) {
            s.needs_full_render = false;
            (s.hover_cache.replace(p.cache), true)
        } else {
            (Some(p.cache), false)
        }
    };
    if let Some(c) = stale {
        unsafe { free_hover_cache(c) };
    }
    adopted
}

/// Whether something composites over (live DWM thumbnail) or fills
/// (cached snapshot) the card body, making a body repaint moot.
fn body_is_covered(card: &OverviewCard, thumbnails: &HashMap<u64, ThumbnailHandle>) -> bool {
    thumbnails.contains_key(&card.window_id)
        || (card.snapshot && crate::snapshot::snapshot_get(card.window_id).is_some())
}

/// Repaint one card in the cached working frame: reset its rect from the
/// hover-free base (bits + alpha), redo the card chrome at `hovered`,
/// redraw its GDI content, re-premultiply the rect. Returns the dirty
/// rect to blit, `None` when the card is fully clipped.
unsafe fn repaint_card_in_cache(
    cache: &mut HoverCache,
    card: &OverviewCard,
    hovered: bool,
    body_covered: bool,
    accent_bgr: u32,
) -> Option<Rect> {
    let (w, h) = (cache.w, cache.h);
    let (x0, x1, y0, y1) = clip_rect(&card.rect, w, h);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    // Settle any pending GDI writes before touching the bits directly.
    let _ = GdiFlush();
    let len = (w * h) as usize;
    let pixels = std::slice::from_raw_parts_mut(cache.work_bits as *mut u32, len);
    for y in y0..y1 {
        let row = (y * w) as usize;
        let (a, b) = (row + x0 as usize, row + x1 as usize);
        pixels[a..b].copy_from_slice(&cache.base_pixels[a..b]);
        cache.work_alpha[a..b].copy_from_slice(&cache.base_alpha[a..b]);
    }
    let refill_body = !body_covered;
    draw_card_chrome(pixels, &mut cache.work_alpha, w, h, card, hovered, refill_body);
    // GDI content for the repainted areas (fonts mirror draw_content).
    let hdc = HDC(cache.work_dc as *mut c_void);
    SetBkMode(hdc, TRANSPARENT);
    let card_font = make_font(13, FW_NORMAL.0 as i32);
    let pill_font = make_font(11, FW_SEMIBOLD.0 as i32);
    if card_is_titled(&card.rect) {
        draw_card_title_strip(hdc, card, card_font, pill_font, accent_bgr);
        if refill_body {
            draw_card_body_icon(hdc, card);
        }
    } else {
        draw_card_compact(hdc, card, card_font, pill_font, accent_bgr);
    }
    let _ = DeleteObject(card_font.into());
    let _ = DeleteObject(pill_font.into());
    let _ = GdiFlush();
    premultiply_rect(pixels, &cache.work_alpha, w, h, &card.rect);
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Fast path for a hover change: repaint only the affected cards (old +
/// new) in the cached working frame and blit just those dirty rects —
/// no full SDF re-render on mouse movement. Returns false when the
/// cache isn't usable (no cache, stale size, full render pending,
/// AlphaDim fallback) and the caller must fall back to a full render.
fn hover_repaint(hwnd: HWND, old: Option<(usize, usize)>, new: Option<(usize, usize)>) -> bool {
    let mut s = state();
    let s = &mut *s;
    if s.needs_full_render || s.backdrop == BackdropMode::AlphaDim {
        return false;
    }
    let Some(cache) = s.hover_cache.as_mut() else {
        return false;
    };
    if cache.w != s.window_rect.width || cache.h != s.window_rect.height {
        return false;
    }
    let mut dirty = Vec::new();
    for (idx, hov) in [(old, false), (new, true)] {
        let Some(card) = idx.and_then(|(ri, ci)| s.model.rows.get(ri)?.cards.get(ci)) else {
            continue;
        };
        let covered = body_is_covered(card, &s.thumbnails);
        if let Some(r) =
            unsafe { repaint_card_in_cache(cache, card, hov, covered, s.model.accent_bgr) }
        {
            dirty.push(r);
        }
    }
    // Blit under the lock: a concurrent drop_hover_cache (which also
    // takes the lock) can never delete the DIB mid-blit.
    if !dirty.is_empty() {
        unsafe {
            let hdc = GetDC(Some(hwnd));
            for r in &dirty {
                let _ = BitBlt(
                    hdc,
                    r.x,
                    r.y,
                    r.width,
                    r.height,
                    Some(HDC(cache.work_dc as *mut c_void)),
                    r.x,
                    r.y,
                    SRCCOPY,
                );
            }
            ReleaseDC(Some(hwnd), hdc);
        }
    }
    true
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
    let input = FrameInput {
        model,
        hovered,
        selected,
        mode: BackdropMode::AlphaDim,
        thumb_wids: &no_thumbs,
    };
    let Some(dib) = build_frame_dib(w, h, &input) else {
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

/// The two bottom-corner squares of `rect` that a square preview
/// overflows when the bottom corners round at `radius`: one SDF band
/// (radius + 1px AA) per side, clamped to the rect.
fn bottom_corner_squares(rect: &Rect, radius: f32) -> [Rect; 2] {
    let span = (radius.ceil() as i32 + 1)
        .min(rect.width.max(0))
        .min(rect.height.max(0));
    let y = rect.y + rect.height - span;
    [
        Rect::new(rect.x, y, span, span),
        Rect::new(rect.x + rect.width - span, y, span, span),
    ]
}

/// SDF coverage of pixel `(px, py)` inside `rect` rounded at the BOTTOM
/// corners by `radius` (top corners square — mirrors the squared-edge
/// trick in [`sdf_fill_round`]). 1 inside, 0 outside, anti-aliased
/// along the bottom arcs.
fn bottom_round_cov(px: i32, py: i32, rect: &Rect, radius: f32) -> f32 {
    let rad = radius
        .min(rect.width as f32 / 2.0)
        .min(rect.height as f32 / 2.0)
        .max(0.0);
    let ext = rad.ceil() + 1.0;
    let sdf = rounded_rect_sdf(
        px as f32 + 0.5,
        py as f32 + 0.5,
        rect.x as f32,
        rect.y as f32 - ext,
        rect.width as f32,
        rect.height as f32 + ext,
        rad,
    );
    clamp(0.5 - sdf, 0.0, 1.0)
}

/// Lerp two straight 0x00RRGGBB pixels: `b` at weight `t` over `a`.
fn lerp_color(a: u32, b: u32, t: f32) -> u32 {
    let lerp = |ac: u32, bc: u32| {
        ((bc as f32) * t + (ac as f32) * (1.0 - t)).round() as u32
    };
    lerp((a >> 16) & 0xFF, (b >> 16) & 0xFF) << 16
        | lerp((a >> 8) & 0xFF, (b >> 8) & 0xFF) << 8
        | lerp(a & 0xFF, b & 0xFF)
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
    draw_card_chrome(pixels, alpha, w, h, card, hovered, true);
}

/// [`draw_card_shape`] with control over the body refill: hover repaints
/// skip it when a live thumbnail or snapshot occupies the body (the
/// cached base copy already carries those pixels).
fn draw_card_chrome(
    pixels: &mut [u32],
    alpha: &mut [u8],
    w: i32,
    h: i32,
    card: &OverviewCard,
    hovered: bool,
    refill_body: bool,
) {
    let r = &card.rect;
    let radius = card_radius(r);
    let body_bg = if hovered { CARD_HOVER_BG } else { CARD_BG };
    // The hover highlight must also lighten the title strip: a live
    // thumbnail composites over the body, hiding the body highlight.
    let title_bg = if hovered { CARD_TITLE_HOVER_BG } else { CARD_TITLE_BG };
    if card_is_titled(r) {
        let title = Rect::new(r.x, r.y, r.width, CARD_TITLE_H);
        sdf_fill_round(pixels, alpha, w, h, &title, radius, true, false, title_bg, OPAQUE);
        if refill_body {
            let body = Rect::new(r.x, r.y + CARD_TITLE_H, r.width, r.height - CARD_TITLE_H);
            sdf_fill_round(pixels, alpha, w, h, &body, radius, false, true, body_bg, OPAQUE);
        }
    } else {
        sdf_fill_round(pixels, alpha, w, h, r, radius, true, true, body_bg, OPAQUE);
    }
    sdf_stroke_round(pixels, alpha, w, h, r, radius, 1.0, CARD_BORDER, OPAQUE);
}

/// Rasterize every shape of the frame: panels (fill, label strip, frame,
/// viewport marker), cards, and the keyboard selection's accent outline.
fn draw_shapes(pixels: &mut [u32], alpha: &mut [u8], w: i32, h: i32, input: &FrameInput) {
    let model = input.model;
    let (hovered, selected) = (input.hovered, input.selected);
    let panel_a = PANEL_ALPHA;
    let chrome_a = OPAQUE;
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
        sdf_fill_round(pixels, alpha, w, h, &row.panel, panel_rad, true, true, panel_bg, panel_a);
        // Label strip: darker band across the panel top, top corners
        // following the panel rounding, bottom edge square.
        sdf_fill_round(
            pixels, alpha, w, h, &row.label_strip, panel_rad, true, false, strip_bg, panel_a,
        );
        // Every panel gets the neutral 1px frame; the active panel is
        // marked by an accent ring SELECT_PAD px OUTSIDE it (below),
        // mirroring the selected-card ring, so the indicator gets the
        // same breathing room instead of stroking the panel's own edge.
        sdf_stroke_round(pixels, alpha, w, h, &row.panel, panel_rad, 1.0, PANEL_BORDER, panel_a);
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
                chrome_a,
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
                panel_a,
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
            chrome_a,
        );
    }
}

/// Force label-strip text pixels opaque (they'd dim with the panel
/// otherwise), then premultiply every pixel for the alpha-aware
/// composition (`AC_SRC_ALPHA` / DWM accent).
fn finish_alpha(pixels: &mut [u32], alpha: &mut [u8], w: i32, h: i32, model: &OverviewModel) {
    let strip_a = PANEL_ALPHA;
    let text_a = OPAQUE;
    for row in &model.rows {
        let bg = if row.is_active { LABEL_STRIP_ACTIVE_BG } else { LABEL_STRIP_BG };
        let (x0, x1, y0, y1) = clip_rect(&row.label_strip, w, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * w + x) as usize;
                // The exact-strip-alpha guard skips anti-aliased corner
                // and edge pixels so they aren't mistaken for text.
                if alpha[i] == strip_a && pixels[i] & 0x00FF_FFFF != bg {
                    alpha[i] = text_a;
                }
            }
        }
    }
    premultiply_rect(pixels, alpha, w, h, &Rect::new(0, 0, w, h));
}

/// Premultiply one rect of straight pixels with the region-alpha plane
/// (the whole-frame case is [`finish_alpha`]'s tail; hover repaints
/// re-premultiply only the repainted card rect).
fn premultiply_rect(pixels: &mut [u32], alpha: &[u8], w: i32, h: i32, rect: &Rect) {
    let (x0, x1, y0, y1) = clip_rect(rect, w, h);
    for y in y0..y1 {
        let row = (y * w) as usize;
        for i in row + x0 as usize..row + x1 as usize {
            let a32 = u32::from(alpha[i]);
            let px = pixels[i];
            let r = (px >> 16) & 0xFF;
            let g = (px >> 8) & 0xFF;
            let b = px & 0xFF;
            pixels[i] = a32 << 24 | (r * a32 / 255) << 16 | (g * a32 / 255) << 8 | (b * a32 / 255);
        }
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
                    let drew_snapshot = card.snapshot && draw_card_snapshot(hdc, card);
                    if !drew_snapshot {
                        draw_card_body_icon(hdc, card);
                    }
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

/// Card body in snapshot mode: stretch the cached capture-on-hide bitmap
/// into the body (same dest rect as a live thumbnail). The pixels land in
/// the GDI pass, so the region-alpha plane (already card-opaque from the
/// shape pass) carries them through the premultiply. Returns false when
/// nothing is cached.
unsafe fn draw_card_snapshot(hdc: HDC, card: &OverviewCard) -> bool {
    let Some(body) = thumb_body(&card.rect) else {
        return false;
    };
    let Some(snap) = crate::snapshot::snapshot_get(card.window_id) else {
        return false;
    };
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: snap.width,
            biHeight: snap.height, // bottom-up, matching the cached bits
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    SetStretchBltMode(hdc, HALFTONE);
    let _ = SetBrushOrgEx(hdc, 0, 0, None);
    // Fill the whole body (like the live thumbnails): DWM-less stretch,
    // a filled card reads better than a letterboxed one.
    let copied = StretchDIBits(
        hdc,
        body.x,
        body.y,
        body.width,
        body.height,
        0,
        0,
        snap.width,
        snap.height,
        Some(snap.bits.as_ptr().cast()),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    copied > 0
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

    fn glide_card(live: bool) -> OverviewCard {
        OverviewCard {
            window_id: 7,
            title: "win".into(),
            icon: None,
            rect: Rect::new(100, 100, 200, 150), // titled-size card
            from_rect: Some(Rect::new(0, 0, 960, 1080)),
            tab_count: None,
            selected: false,
            live,
            snapshot: false,
        }
    }

    fn glide_model(anim_ms: u32) -> OverviewModel {
        OverviewModel {
            backdrop: Rect::new(0, 0, 1920, 1080),
            anim_ms,
            rows: vec![OverviewRow {
                workspace_index: 0,
                label: "1".into(),
                is_active: true,
                panel: Rect::new(50, 50, 800, 300),
                label_strip: Rect::new(50, 50, 800, 24),
                viewport: Rect::new(0, 0, 0, 0),
                cards: vec![glide_card(true)],
            }],
            ..Default::default()
        }
    }

    fn overlay_state(model: OverviewModel) -> OverviewState {
        OverviewState {
            model,
            visible: true,
            window_rect: Rect::new(0, 0, 1920, 1080),
            hovered: None,
            selected: None,
            mouse_tracking_armed: false,
            backdrop: BackdropMode::Acrylic,
            event_tx: None,
            thumbnails: HashMap::new(),
            anim: None,
            chrome_steps: Vec::new(),
            chrome_full: None,
            pending_hover_cache: None,
            chrome_last_step: None,
            hover_cache: None,
            needs_full_render: true,
            mask_hwnd: 0,
            mask_pushed: (Rect::new(0, 0, 0, 0), Vec::new()),
        }
    }

    fn fake_step() -> ChromeStep {
        ChromeStep {
            mem_dc: 0,
            hbitmap: 0,
            old_bmp: 0,
            bits: 0,
        }
    }

    /// `CHROME_FADE_FACTORS.len()` fake steps (null GDI handles): fine
    /// for the pure bookkeeping under test, which never blits them.
    fn fake_chrome_steps() -> Vec<ChromeStep> {
        (0..CHROME_FADE_FACTORS.len()).map(|_| fake_step()).collect()
    }

    /// Fake hover cache (null GDI handles, empty planes) for the pure
    /// pending-cache match logic, which never blits or frees it.
    fn fake_hover_cache(w: i32, h: i32) -> HoverCache {
        HoverCache {
            work_dc: 0,
            work_bmp: 0,
            work_old: 0,
            work_bits: 0,
            work_alpha: Vec::new(),
            base_pixels: Vec::new(),
            base_alpha: Vec::new(),
            w,
            h,
        }
    }

    #[test]
    fn test_card_can_glide_requires_live_from_rect_and_titled_size() {
        assert!(card_can_glide(&glide_card(true)));
        assert!(!card_can_glide(&glide_card(false)), "placeholder cards never glide");
        let mut no_from = glide_card(true);
        no_from.from_rect = None;
        assert!(!card_can_glide(&no_from));
        let mut compact = glide_card(true);
        compact.rect = Rect::new(100, 100, 60, 40); // below TITLED_MIN
        assert!(!card_can_glide(&compact));
    }

    #[test]
    fn test_start_close_not_animatable_when_anim_ms_zero() {
        let mut s = overlay_state(glide_model(0));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        assert!(matches!(start_close(&mut s, None, true), CloseStart::NotAnimatable));
        assert!(s.anim.is_none());
    }

    #[test]
    fn test_start_close_not_animatable_without_registered_thumbnails() {
        // anim_ms > 0 but no thumbnail registered (AlphaDim fallback,
        // placeholder/snapshot mode): nothing can glide.
        let mut s = overlay_state(glide_model(150));
        assert!(matches!(start_close(&mut s, None, true), CloseStart::NotAnimatable));
        assert!(s.anim.is_none());
    }

    #[test]
    fn test_start_close_starts_and_guards_double_close() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        let anim = s.anim.expect("closing anim installed");
        assert_eq!(anim.phase, AnimPhase::Closing);
        assert!(anim.notify, "overlay-initiated close must report Dismissed");
        // Second close while one is in flight: no-op, state untouched.
        assert!(matches!(start_close(&mut s, None, false), CloseStart::AlreadyClosing));
        assert!(s.anim.expect("anim kept").notify, "in-flight close must not be re-armed");
    }

    #[test]
    fn test_sync_k_unticked_open_pins_to_start() {
        // Show-time paint can burn the whole duration before the first
        // tick (which restarts the clock): sync must register at k=0.
        let open = AnimState {
            phase: AnimPhase::Opening,
            started: std::time::Instant::now() - std::time::Duration::from_millis(500),
            duration_ms: 150,
            easing: Easing::default(),
            row: 0,
            ticks: 0,
            notify: false,
        };
        assert_eq!(sync_k(&open), 0.0);
        let ticked = AnimState { ticks: 1, ..open };
        assert!((sync_k(&ticked) - anim_k(&ticked)).abs() < f32::EPSILON);
        // Closes never restart the clock: sync follows the real k even
        // before the first tick (back-dated Esc-mid-open handover).
        let closing = AnimState {
            phase: AnimPhase::Closing,
            started: std::time::Instant::now() - std::time::Duration::from_millis(75),
            duration_ms: 150,
            easing: Easing::default(),
            row: 0,
            ticks: 0,
            notify: true,
        };
        assert!((sync_k(&closing) - anim_k(&closing)).abs() < 0.05);
        assert!(sync_k(&closing) < 0.6, "mid-flight close must not pin to an endpoint");
    }

    #[test]
    fn test_inactive_row_opacity_rides_k() {
        // Un-ticked open registers at k=0 -> invisible; settle at 255.
        assert_eq!(inactive_row_opacity(0.0), 0);
        assert_eq!(inactive_row_opacity(1.0), 255);
        assert_eq!(inactive_row_opacity(0.5), 128);
        // Out-of-range k clamps instead of wrapping the u8.
        assert_eq!(inactive_row_opacity(-0.2), 0);
        assert_eq!(inactive_row_opacity(1.7), 255);
    }

    #[test]
    fn test_apply_model_update_applies_while_open() {
        let mut s = overlay_state(glide_model(150));
        let mut refreshed = glide_model(150);
        refreshed.rows[0].label = "renamed".into();
        assert!(apply_model_update(&mut s, refreshed));
        assert_eq!(s.model.rows[0].label, "renamed");
    }

    #[test]
    fn test_apply_model_update_noop_while_closing() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        assert!(is_closing_state(&s));
        let mut refreshed = glide_model(150);
        refreshed.rows[0].label = "renamed".into();
        assert!(!apply_model_update(&mut s, refreshed));
        assert_eq!(s.model.rows[0].label, "1", "model untouched during close");
        assert!(
            matches!(s.anim, Some(a) if a.phase == AnimPhase::Closing),
            "closing anim kept"
        );
    }

    #[test]
    fn test_apply_model_update_noop_while_hidden() {
        // Close finished and the window hid itself, but the daemon's
        // Dismissed bookkeeping hasn't run yet: refreshes still arrive.
        let mut s = overlay_state(glide_model(150));
        s.visible = false;
        assert!(is_closing_state(&s));
        assert!(!apply_model_update(&mut s, glide_model(0)));
        assert_eq!(s.model.anim_ms, 150, "model untouched while hidden");
    }

    #[test]
    fn test_apply_model_update_carries_opening_anim() {
        // Refresh mid-open must still apply and keep the glide running.
        let mut s = overlay_state(glide_model(150));
        s.anim = Some(AnimState {
            phase: AnimPhase::Opening,
            started: std::time::Instant::now(),
            duration_ms: 150,
            easing: Easing::default(),
            row: 0,
            ticks: 1,
            notify: false,
        });
        assert!(!is_closing_state(&s));
        let mut refreshed = glide_model(150);
        refreshed.rows[0].label = "renamed".into();
        assert!(apply_model_update(&mut s, refreshed));
        assert!(
            matches!(s.anim, Some(a) if a.phase == AnimPhase::Opening),
            "opening anim carried across the refresh"
        );
    }

    #[test]
    fn test_apply_model_update_identical_model_is_noop() {
        // The daemon re-pushes on every window event: a model identical
        // to the displayed one must not trigger a repaint (it reset the
        // hover and re-pushed the mask — the hover-flicker storm).
        let mut s = overlay_state(glide_model(150));
        s.hovered = Some((0, 0));
        assert!(!apply_model_update(&mut s, glide_model(150)));
        assert_eq!(s.hovered, Some((0, 0)), "no-op refresh keeps the hover");
    }

    #[test]
    fn test_apply_model_update_preserves_hover_by_window_id() {
        // A genuine refresh with a stationary mouse must carry the hover
        // to the hovered window's card in the new model — the repaint it
        // triggers rebuilds the frame from `hovered`.
        let mut s = overlay_state(glide_model(150));
        s.hovered = Some((0, 0)); // card window_id 7
        let mut refreshed = glide_model(150);
        refreshed.rows[0].label = "renamed".into();
        assert!(apply_model_update(&mut s, refreshed));
        assert_eq!(s.hovered, Some((0, 0)), "hover relocated by window id");
        // Hovered window gone: hover clears instead of pointing at a
        // different window's card.
        let mut gone = glide_model(150);
        gone.rows[0].label = "renamed twice".into();
        gone.rows[0].cards[0].window_id = 99;
        assert!(apply_model_update(&mut s, gone));
        assert_eq!(s.hovered, None, "hover dropped with its window");
    }

    #[test]
    fn test_start_close_mid_open_hands_over_motion_parameter() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        // An open ~half way through its 150ms.
        let open = AnimState {
            phase: AnimPhase::Opening,
            started: std::time::Instant::now() - std::time::Duration::from_millis(75),
            duration_ms: 150,
            easing: Easing::default(),
            row: 0,
            ticks: 9,
            notify: false,
        };
        let k_open = anim_k(&open);
        s.anim = Some(open);
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        let k_close = anim_k(&s.anim.expect("closing anim"));
        assert!(
            (k_open - k_close).abs() < 0.05,
            "close must take over at the open's k: open {k_open} vs close {k_close}"
        );
    }

    #[test]
    fn test_chrome_step_for_k_thresholds() {
        assert_eq!(chrome_step_for_k(0.0), 0);
        assert_eq!(chrome_step_for_k(0.24), 0);
        assert_eq!(chrome_step_for_k(0.25), 1);
        assert_eq!(chrome_step_for_k(0.49), 1);
        assert_eq!(chrome_step_for_k(0.5), 2);
        assert_eq!(chrome_step_for_k(0.74), 2);
        assert_eq!(chrome_step_for_k(0.75), 3);
        assert_eq!(chrome_step_for_k(1.0), 3);
        // Out-of-range k (defensive): pins to the end steps.
        assert_eq!(chrome_step_for_k(-0.5), 0);
        assert_eq!(chrome_step_for_k(1.5), 3);
    }

    #[test]
    fn test_chrome_steps_reverse_on_close() {
        // Close runs k 1 -> 0 through the SAME picker: the steps walk
        // back down monotonically — the reverse fade comes for free.
        let ks = [1.0, 0.8, 0.6, 0.4, 0.2, 0.0];
        let steps: Vec<usize> = ks.iter().map(|&k| chrome_step_for_k(k)).collect();
        assert_eq!(steps, vec![3, 3, 2, 1, 0, 0]);
        assert!(steps.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn test_chrome_fade_factors_ascend_to_full() {
        assert!(CHROME_FADE_FACTORS.windows(2).all(|w| w[0] < w[1]));
        let last = *CHROME_FADE_FACTORS.last().unwrap();
        assert!((last - 1.0).abs() < f32::EPSILON, "settled frame must be full strength");
        // Every k the picker can produce maps inside the step table.
        assert!(chrome_step_for_k(f32::MAX) < CHROME_FADE_FACTORS.len());
    }

    #[test]
    fn test_apply_frame_fade_scales_premultiplied_channels() {
        let mut px = vec![0xFF_FF_FF_FF_u32, 0x80_40_20_10, 0x0000_0000];
        apply_frame_fade(&mut px, 0.5);
        assert_eq!(px[0], 0x7F_7F_7F_7F);
        assert_eq!(px[1], 0x40_20_10_08);
        assert_eq!(px[2], 0);
        // fade >= 1.0 is a strict no-op (live frames).
        let mut noop = vec![0x80_40_20_10_u32];
        apply_frame_fade(&mut noop, 1.0);
        assert_eq!(noop[0], 0x80_40_20_10);
    }

    #[test]
    fn test_start_close_from_settled_map_starts_at_top_step() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        s.chrome_steps = fake_chrome_steps();
        s.chrome_last_step = None; // settled: the live full frame is on screen
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        assert_eq!(
            s.chrome_last_step,
            Some(CHROME_FADE_FACTORS.len() - 1),
            "first close blit must wait until k drops below the top threshold"
        );
    }

    #[test]
    fn test_start_close_mid_open_keeps_current_step() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        s.chrome_steps = fake_chrome_steps();
        s.chrome_last_step = Some(1);
        s.anim = Some(AnimState {
            phase: AnimPhase::Opening,
            started: std::time::Instant::now() - std::time::Duration::from_millis(75),
            duration_ms: 150,
            easing: Easing::default(),
            row: 0,
            ticks: 9,
            notify: false,
        });
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        assert_eq!(
            s.chrome_last_step,
            Some(1),
            "Esc-mid-open handover keeps the open's step (same picker, continuous k)"
        );
    }

    #[test]
    fn test_ease_inverse_roundtrips_every_curve() {
        for easing in [
            Easing::Linear,
            Easing::EaseOut,
            Easing::EaseIn,
            Easing::EaseInOut,
        ] {
            for i in 0..=10 {
                let y = i as f32 / 10.0;
                let t = ease_inverse(easing, y);
                assert!(
                    (0.0..=1.0).contains(&t),
                    "{easing:?}: inverse({y}) = {t} out of range"
                );
                let back = apply_easing(easing, t);
                assert!(
                    (back - y).abs() < 1e-3,
                    "{easing:?}: ease(inverse({y})) = {back}"
                );
            }
        }
    }

    #[test]
    fn test_eased_k_uses_configured_easing() {
        // Linear: progress passes straight through.
        assert!((eased_k(AnimPhase::Opening, Easing::Linear, 0.5) - 0.5).abs() < 1e-6);
        assert!((eased_k(AnimPhase::Closing, Easing::Linear, 0.25) - 0.75).abs() < 1e-6);
        // EaseOut (the old hardcoded cubic): 1 - (1-t)^3.
        assert!((eased_k(AnimPhase::Opening, Easing::EaseOut, 0.5) - 0.875).abs() < 1e-6);
        // EaseInOut differs from EaseOut mid-flight: the curve is wired
        // through, not hardcoded.
        assert!(
            (eased_k(AnimPhase::Opening, Easing::EaseInOut, 0.25)
                - eased_k(AnimPhase::Opening, Easing::EaseOut, 0.25))
            .abs()
                > 0.1
        );
    }

    #[test]
    fn test_start_close_mid_open_hands_over_with_ease_in_out() {
        let mut model = glide_model(150);
        model.easing = Easing::EaseInOut;
        let mut s = overlay_state(model);
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        let open = AnimState {
            phase: AnimPhase::Opening,
            started: std::time::Instant::now() - std::time::Duration::from_millis(60),
            duration_ms: 150,
            easing: Easing::EaseInOut,
            row: 0,
            ticks: 9,
            notify: false,
        };
        let k_open = anim_k(&open);
        s.anim = Some(open);
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        let close = s.anim.expect("closing anim");
        assert_eq!(close.easing, Easing::EaseInOut);
        let k_close = anim_k(&close);
        assert!(
            (k_open - k_close).abs() < 0.05,
            "non-default easing must still hand over: open {k_open} vs close {k_close}"
        );
    }

    #[test]
    fn test_hover_transition_repaints_only_on_change() {
        // Same hover (or none at all): nothing to repaint.
        assert_eq!(hover_transition(None, None), None);
        assert_eq!(hover_transition(Some((0, 1)), Some((0, 1))), None);
        // Changes repaint exactly the old and new cards.
        assert_eq!(
            hover_transition(None, Some((1, 2))),
            Some((None, Some((1, 2))))
        );
        assert_eq!(
            hover_transition(Some((0, 0)), None),
            Some((Some((0, 0)), None))
        );
        assert_eq!(
            hover_transition(Some((0, 0)), Some((1, 1))),
            Some((Some((0, 0)), Some((1, 1))))
        );
    }

    #[test]
    fn test_bottom_corner_squares_geometry() {
        let rect = Rect::new(100, 100, 200, 150);
        let [bl, br] = bottom_corner_squares(&rect, 8.0);
        // One SDF band per corner: radius + 1px AA.
        assert_eq!(bl, Rect::new(100, 241, 9, 9));
        assert_eq!(br, Rect::new(291, 241, 9, 9));
        // Both hug the rect's bottom edge.
        assert_eq!(bl.y + bl.height, rect.y + rect.height);
        assert_eq!(br.x + br.width, rect.x + rect.width);
        // Tiny rects clamp the span instead of escaping the rect.
        let [tl, tr] = bottom_corner_squares(&Rect::new(0, 0, 6, 4), 8.0);
        assert!(tl.width <= 4 && tl.height <= 4);
        assert!(tr.x + tr.width <= 6);
    }

    #[test]
    fn test_bottom_round_cov_masks_outside_bottom_corners_only() {
        let rect = Rect::new(0, 0, 100, 100);
        // Deep inside: full coverage.
        assert!((bottom_round_cov(50, 50, &rect, 8.0) - 1.0).abs() < f32::EPSILON);
        // Outermost bottom-corner pixels: outside the rounding.
        assert!(bottom_round_cov(0, 99, &rect, 8.0) < 0.01);
        assert!(bottom_round_cov(99, 99, &rect, 8.0) < 0.01);
        // Top corners square off (they sit under the title strip).
        assert!((bottom_round_cov(0, 0, &rect, 8.0) - 1.0).abs() < f32::EPSILON);
        assert!((bottom_round_cov(99, 0, &rect, 8.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_corner_caps_only_for_thumbnail_carrying_cards() {
        let model = glide_model(150);
        // No registered thumbnail: nothing to cap.
        assert!(corner_caps(&model, &HashSet::new(), None).is_empty());
        // Registered: two caps (bottom-left + bottom-right), panel-bg
        // colored for the active row, squares at the THUMBNAIL body's
        // bottom corners (not the card's — off-by-THUMB_INSET otherwise).
        let wids: HashSet<u64> = [7].into_iter().collect();
        let caps = corner_caps(&model, &wids, None);
        assert_eq!(caps.len(), 2);
        let card = model.rows[0].cards[0].rect;
        let body = thumb_body(&card).expect("titled card has a thumb body");
        let inner_rad = card_radius(&card) - THUMB_INSET as f32;
        let expected = bottom_corner_squares(&body, inner_rad);
        for (cap, square) in caps.iter().zip(expected) {
            assert_eq!(cap.square, square);
            assert_eq!(cap.card, card);
            assert_eq!(cap.body, body);
            assert_eq!(cap.color, PANEL_ACTIVE_BG, "active row uses the active panel bg");
            assert_eq!(cap.ring, None, "unselected card gets no ring");
        }
    }

    #[test]
    fn test_corner_caps_selected_card_carries_the_accent_ring() {
        // Changed caps must compare unequal so sync_mask's dedup still
        // pushes when the keyboard selection moves.
        let model = glide_model(150);
        let wids: HashSet<u64> = [7].into_iter().collect();
        let plain = corner_caps(&model, &wids, None);
        let selected = corner_caps(&model, &wids, Some((0, 0)));
        assert_ne!(selected, plain, "selection change must produce different caps");
        for cap in &selected {
            assert_eq!(cap.ring, Some((model.accent_bgr, model.accent_width.max(1))));
        }
        // Off-model selection index: no card matches, no ring.
        let stale = corner_caps(&model, &wids, Some((5, 5)));
        assert_eq!(stale, plain);
    }

    /// Paint one bottom-left cap for a 100x100 card at the origin into a
    /// 100x100 DIB and return the pixel buffer. `card_radius` = 8, inner
    /// rounding = 7 at the thumb body (1, 27, 98, 72); the inner arc's
    /// corner center is (8, 92).
    fn painted_cap(ring: Option<(u32, u32)>) -> Vec<u32> {
        let card = Rect::new(0, 0, 100, 100);
        let body = thumb_body(&card).expect("titled");
        let inner_rad = card_radius(&card) - THUMB_INSET as f32;
        let span_rad = inner_rad + ring.map_or(0.0, |(_, w)| w as f32);
        let [bl, _] = bottom_corner_squares(&body, span_rad);
        let cap = CornerCap {
            square: bl,
            card,
            body,
            radius_px: card_radius(&card).round() as i32,
            color: 0x00112233, // BGR: R=0x33 G=0x22 B=0x11
            ring,
        };
        let mut pixels = vec![0u32; 100 * 100];
        paint_corner_cap(&mut pixels, 100, 100, &cap);
        pixels
    }

    #[test]
    fn test_paint_corner_cap_layers_panel_frame_and_interior() {
        let pixels = painted_cap(None);
        // Body corner pixel (1, 98): outside the card rounding entirely —
        // opaque panel bg, channels in DIB (RGB) order.
        assert_eq!(pixels[98 * 100 + 1], 0xFF33_2211);
        // Deep inside the inner rounding: the preview keeps showing.
        assert_eq!(pixels[92 * 100 + 7], 0, "cap must not cover the preview interior");
        // Outside the cap square: untouched.
        assert_eq!(pixels[98 * 100 + 20], 0);
        // Frame arc pixel (3, 97): distance ~7.1 from the corner center,
        // i.e. between the inner (7) and card (8) roundings — the cap
        // restores the 1px CARD_BORDER arc there (partial alpha, pure
        // border color).
        let px = pixels[97 * 100 + 3];
        let a = px >> 24;
        assert!(a > 0 && a < 255, "frame arc pixel must be anti-aliased, got alpha {a}");
        let border = bgr_to_pixel(CARD_BORDER);
        for shift in [16, 8, 0] {
            assert_eq!(
                (px >> shift) & 0xFF,
                ((border >> shift) & 0xFF) * a / 255,
                "frame arc pixel must be premultiplied CARD_BORDER"
            );
        }
    }

    #[test]
    fn test_paint_corner_cap_draws_the_accent_ring_arc_when_selected() {
        // Accent 0x000000FF (red COLORREF), stroke 4: the ring band
        // reaches 8..12 px from the corner-arc center, so the body corner
        // pixel (1, 98) at distance ~9.2 sits fully inside it.
        let pixels = painted_cap(Some((0x0000_00FF, 4)));
        assert_eq!(
            pixels[98 * 100 + 1],
            0xFFFF_0000,
            "ring arc must override the panel fill on the selected card"
        );
        // Without the ring the same pixel is panel bg (see the layering
        // test); a thin default ring (stroke 2, band 10..12) never
        // reaches the preview, so the pixel stays panel bg.
        let thin = painted_cap(Some((0x0000_00FF, 2)));
        assert_eq!(thin[98 * 100 + 1], 0xFF33_2211);
    }

    #[test]
    fn test_lerp_color_endpoints_and_midpoint() {
        let a = 0x0020_4060;
        let b = 0x00A0_80FF;
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
        assert_eq!(lerp_color(a, b, 0.5), 0x0060_60B0);
    }

    #[test]
    fn test_take_faded_steps_keeps_only_the_full_frame() {
        let mut s = overlay_state(glide_model(150));
        s.chrome_steps = fake_chrome_steps();
        let faded = take_faded_steps(&mut s);
        assert_eq!(faded.len(), CHROME_FADE_FACTORS.len() - 1);
        assert!(s.chrome_steps.is_empty(), "faded steps leave the live set");
        assert!(s.chrome_full.is_some(), "full frame retained for the close");
    }

    #[test]
    fn test_take_faded_steps_noop_without_full_set() {
        // Pre-render failed (empty) or already demoted: nothing to free.
        let mut s = overlay_state(glide_model(150));
        assert!(take_faded_steps(&mut s).is_empty());
        assert!(s.chrome_full.is_none());
        s.chrome_full = Some(fake_step());
        s.chrome_steps = Vec::new();
        assert!(take_faded_steps(&mut s).is_empty());
        assert!(s.chrome_full.is_some(), "retained frame untouched");
    }

    #[test]
    fn test_start_close_from_settled_retained_full_frame_starts_at_top_step() {
        // Post-settle state: faded steps freed, only chrome_full alive.
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        s.chrome_full = Some(fake_step());
        s.chrome_last_step = None;
        assert!(matches!(start_close(&mut s, None, true), CloseStart::Started));
        assert_eq!(
            s.chrome_last_step,
            Some(CHROME_FADE_FACTORS.len() - 1),
            "settled close must wait for k to drop below the top threshold"
        );
    }

    fn pending_for(s: &OverviewState) -> PendingHoverCache {
        PendingHoverCache {
            cache: fake_hover_cache(s.window_rect.width, s.window_rect.height),
            model: s.model.clone(),
            selected: s.selected,
            thumb_wids: s.thumbnails.keys().copied().collect(),
        }
    }

    #[test]
    fn test_pending_hover_cache_matches_unchanged_state() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        s.selected = Some((0, 0));
        let p = pending_for(&s);
        assert!(pending_matches_state(&p, &s));
    }

    #[test]
    fn test_pending_hover_cache_stale_on_any_drift() {
        let mut s = overlay_state(glide_model(150));
        s.thumbnails.insert(7, ThumbnailHandle::fake());
        s.selected = Some((0, 0));
        let p = pending_for(&s);

        // Mid-open refresh changed the model.
        let mut model_drift = overlay_state(glide_model(150));
        model_drift.model.rows[0].label = "renamed".into();
        model_drift.thumbnails.insert(7, ThumbnailHandle::fake());
        model_drift.selected = Some((0, 0));
        assert!(!pending_matches_state(&p, &model_drift));

        // Thumbnail set drifted (a registration failed at settle).
        let mut thumb_drift = overlay_state(glide_model(150));
        thumb_drift.selected = Some((0, 0));
        assert!(!pending_matches_state(&p, &thumb_drift));

        // Selection drifted.
        let mut sel_drift = overlay_state(glide_model(150));
        sel_drift.thumbnails.insert(7, ThumbnailHandle::fake());
        sel_drift.selected = None;
        assert!(!pending_matches_state(&p, &sel_drift));

        // Hidden mid-settle: never resurrect the cache.
        let mut hidden = overlay_state(glide_model(150));
        hidden.thumbnails.insert(7, ThumbnailHandle::fake());
        hidden.selected = Some((0, 0));
        hidden.visible = false;
        assert!(!pending_matches_state(&p, &hidden));
    }

    #[test]
    fn test_caps_bounds_union_pads_and_clips() {
        let cap = |x: i32, y: i32| CornerCap {
            square: Rect::new(x, y, 9, 9),
            card: Rect::new(0, 0, 100, 100),
            body: Rect::new(1, 27, 98, 72),
            radius_px: 8,
            color: PANEL_BG,
            ring: None,
        };
        assert_eq!(caps_bounds(&[], 1920, 1080), None);
        // Single cap: its square padded by 1px.
        let b = caps_bounds(&[cap(100, 200)], 1920, 1080).expect("bounds");
        assert_eq!(b, Rect::new(99, 199, 11, 11));
        // Two caps: bounding union of both squares (+pad).
        let b = caps_bounds(&[cap(100, 200), cap(400, 200)], 1920, 1080).expect("bounds");
        assert_eq!(b, Rect::new(99, 199, 311, 11));
        // Clip to the overlay client area.
        let b = caps_bounds(&[cap(0, 0)], 1920, 1080).expect("bounds");
        assert_eq!(b, Rect::new(0, 0, 10, 10), "pad must clip at the origin");
        // Entirely outside: nothing survives.
        assert_eq!(caps_bounds(&[cap(-50, -50)], 30, 30), None);
    }

    #[test]
    fn test_paint_corner_cap_translation_invariant() {
        // Painting an offset cap into a sub-rect DIB must produce the
        // same pixels as painting the original into a full-size DIB —
        // the mask layer's sub-rect allocation relies on it.
        let card = Rect::new(40, 30, 100, 100);
        let body = thumb_body(&card).expect("titled");
        let inner_rad = card_radius(&card) - THUMB_INSET as f32;
        let [bl, _] = bottom_corner_squares(&body, inner_rad + 4.0);
        let cap = CornerCap {
            square: bl,
            card,
            body,
            radius_px: card_radius(&card).round() as i32,
            color: 0x00112233,
            ring: Some((0x0000_00FF, 4)),
        };
        let (fw, fh) = (200, 200);
        let mut full = vec![0u32; (fw * fh) as usize];
        paint_corner_cap(&mut full, fw, fh, &cap);

        let bounds = caps_bounds(std::slice::from_ref(&cap), fw, fh).expect("bounds");
        let mut sub = vec![0u32; (bounds.width * bounds.height) as usize];
        let local = cap.offset(-bounds.x, -bounds.y);
        paint_corner_cap(&mut sub, bounds.width, bounds.height, &local);

        for y in 0..bounds.height {
            for x in 0..bounds.width {
                let full_px = full[((bounds.y + y) * fw + bounds.x + x) as usize];
                let sub_px = sub[(y * bounds.width + x) as usize];
                assert_eq!(sub_px, full_px, "pixel ({x},{y}) differs after translation");
            }
        }
    }

    #[test]
    fn test_premultiply_rect_touches_only_the_rect() {
        let (w, h) = (4, 4);
        let mut pixels = vec![0x00FF_FFFF_u32; (w * h) as usize];
        let alpha = vec![128_u8; (w * h) as usize];
        premultiply_rect(&mut pixels, &alpha, w, h, &Rect::new(1, 1, 2, 2));
        let inside = 128 << 24 | (255 * 128 / 255) << 16 | (255 * 128 / 255) << 8 | (255 * 128 / 255);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if (1..3).contains(&x) && (1..3).contains(&y) {
                    assert_eq!(pixels[i], inside, "({x},{y}) premultiplied");
                } else {
                    assert_eq!(pixels[i], 0x00FF_FFFF, "({x},{y}) untouched");
                }
            }
        }
    }
}
