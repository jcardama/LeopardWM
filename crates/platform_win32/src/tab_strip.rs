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
//! Multi-instance: the daemon owns one `TabStripOverlay` per visible
//! tabbed column (across all monitors / workspaces). Each instance has
//! its own hwnd, its own background message-pump thread, and its own
//! tooltip popup. State is stored in a shared global registry keyed by
//! the strip's hwnd — the WndProc looks up its instance's state by
//! `hwnd`, and the tooltip popup's WndProc looks up its parent strip's
//! state via a `tooltip_hwnd → strip_hwnd` mapping populated at create.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{mpsc, LazyLock, Mutex, MutexGuard};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::Win32Error;

/// Default tab strip height in pixels at 96 DPI.
pub const DEFAULT_STRIP_HEIGHT: u32 = 28;

/// Round `target` up by 1 if needed so its parity (even/odd) matches
/// `parity_of`. Used to keep the close-X pill's height parity aligned
/// with the strip height, so `(strip_h - pill_h) / 2` is an exact
/// integer — eliminates the 0.5px upward bias that creeps in when
/// dividing odd numbers by 2.
fn snap_to_parity(target: i32, parity_of: i32) -> i32 {
    if (target & 1) == (parity_of & 1) {
        target
    } else {
        target + 1
    }
}

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

/// Discrete user-initiated action targeting a specific tab.
///
/// Emitted by the strip's WndProc on click/middle-click/right-click and
/// dispatched by the daemon. `Activate` is the v0.1.14 left-click flow;
/// `Close`/`Untab`/`Rename` are the v0.1.15 affordances wired in later
/// phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    Activate,
    Close,
    Untab,
    Rename,
}

/// Behavior for the implicit "close tab" gestures (X-button click,
/// middle-click). Passed in via `show()` so the strip's WndProc emits
/// the right action without needing access to user config. The
/// right-click menu items always carry their literal action and never
/// consult this toggle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TabCloseAction {
    #[default]
    CloseWindow,
    Untab,
}

/// Action event sent from the overlay back to the daemon.
///
/// Carries the (monitor, workspace, column) identity captured at the
/// moment `show()` last drew the strip, so the daemon's handler can
/// route the action to the column the user actually saw — not whatever
/// happens to be focused at dispatch time. This matters because focus
/// can change between the originating WM_* arriving and the main loop
/// processing the resulting `DaemonEvent::TabAction`.
#[derive(Debug, Clone, Copy)]
pub struct TabActionEvent {
    pub monitor: isize,
    pub workspace_idx: usize,
    pub column_idx: usize,
    /// 0-based tab index targeted by the action.
    pub tab_idx: usize,
    pub action: TabAction,
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
    /// Channel back to the daemon for action events.
    action_tx: Option<mpsc::Sender<TabActionEvent>>,
    /// Currently hovered tab (under the mouse). Used to render the
    /// per-tab close-X glyph only on the active hover target.
    hovered_tab_idx: Option<usize>,
    /// Currently hovered close-X glyph (mouse over the X itself, not
    /// just over the tab). Used to render a button-style hover bg
    /// behind the glyph so the X reads as a clickable button rather
    /// than incidental decoration.
    hovered_close_idx: Option<usize>,
    /// Close-X hit rects (window-relative). Parallel to `tabs` when
    /// the strip is wide enough that the X is rendered; empty when
    /// every tab is too narrow to fit the glyph.
    close_hit_rects: Vec<RECT>,
    /// Whether `TrackMouseEvent` is currently armed for WM_MOUSELEAVE.
    /// Windows auto-clears the subscription when it fires the leave
    /// event, so we re-arm on the next WM_MOUSEMOVE.
    mouse_tracking_armed: bool,
    /// Physical-pixel threshold above which the close-X is rendered and
    /// hit-tested. Computed from the scale factor passed into `show()`.
    close_min_tab_w_px: i32,
    /// Physical-pixel threshold above which the title text is drawn.
    /// Below this width tabs render icon-only.
    title_min_tab_w_px: i32,
    /// Behavior for implicit close gestures (X-button click, middle-click).
    /// Updated on every `show()` from `BehaviorConfig.tab_close_action`.
    close_action: TabCloseAction,
    /// Custom tooltip popup HWND as a raw `isize`. Stored as integer
    /// so this static `Mutex<TabStripState>` stays `Sync` (HWND wraps
    /// `*mut c_void`, which is !Send by default). The popup is created
    /// once on the strip's UI thread alongside the strip itself, hidden
    /// by default, and shown/repositioned by the strip's WndProc when
    /// the user hovers a close-X for longer than `TOOLTIP_DELAY_MS`.
    /// `0` means the popup hasn't been created yet.
    tooltip_hwnd_raw: isize,
    /// Tooltip text currently rendered into the popup (the popup's
    /// `WM_PAINT` reads from here). Updated before each show.
    tooltip_text: String,
    /// Tab index that started the current hover countdown, if any.
    /// Tracked so the timer callback can verify the user is still over
    /// the same close-X when the delay elapses (mouse may have moved
    /// to a different tab in the meantime).
    tooltip_pending_tab: Option<usize>,
    /// Anchor rect (screen coords) used to re-render the tooltip during
    /// the fade animation. Set when the tooltip is first shown, cleared
    /// when hidden. `RECT` is plain `i32` fields so it's `Send + Sync`.
    tooltip_anchor_rect: Option<RECT>,
    /// Wall-clock instant the fade-in animation started. `None` means
    /// not animating (tooltip is either fully shown or hidden).
    tooltip_fade_start: Option<std::time::Instant>,
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
/// Logical (96-DPI) minimum tab width at which the close-X is rendered
/// AND hit-tested. Scaled by the strip's DPI factor at runtime.
const CLOSE_BTN_MIN_TAB_W_LOGICAL: i32 = 80;
/// Logical (96-DPI) minimum tab width at which the title text is drawn.
/// Below this, tabs render icon-only.
const TITLE_VISIBLE_MIN_TAB_W_LOGICAL: i32 = 60;
/// Context-menu item ids. Returned by `TrackPopupMenu` and translated
/// back into `TabAction` variants. Distinct from the dialog module's
/// own IDs (`crate::dialog::*`) because each WndProc has its own
/// namespace.
const MENU_ID_CLOSE: usize = 1001;
const MENU_ID_UNTAB: usize = 1002;
const MENU_ID_RENAME: usize = 1003;
/// `WM_TIMER` id used to drive the animation. `WM_USER+N` is fine since
/// the strip's WndProc doesn't use any other timers.
const HIGHLIGHT_ANIM_TIMER_ID: usize = 0xA1;
/// `WM_TIMER` id used to delay the custom tooltip popup. One-shot.
const TOOLTIP_DELAY_TIMER_ID: usize = 0xA2;
/// `WM_TIMER` id used to drive the tooltip fade-in animation.
const TOOLTIP_FADE_TIMER_ID: usize = 0xA3;
/// Tooltip popup appears this many ms after the user enters a close-X.
/// Matches the "fast reshow" timing WinUI XAML uses inside an already-
/// hovered window: snappy enough to feel instant, just long enough to
/// suppress flashes during quick fly-over.
const TOOLTIP_DELAY_MS: u32 = 30;
/// Duration of the tooltip fade-in animation in ms.
const TOOLTIP_FADE_MS: u64 = 80;
/// Frame interval for the fade animation. 60-ish fps.
const TOOLTIP_FADE_INTERVAL_MS: u32 = 16;
/// Custom shutdown message — mirrors the `WM_QUIT_HOTKEY_THREAD` /
/// `WM_QUIT_WINEVENT_THREAD` pattern used elsewhere in the crate.
/// `PostMessageW(hwnd, WM_QUIT, ...)` is documented as a no-op (MSDN:
/// "Do not post the WM_QUIT message using PostMessage"), so we use a
/// custom message and break the loop explicitly when it lands.
const WM_QUIT_TAB_STRIP_THREAD: u32 = WM_USER + 4;

fn default_tab_strip_state() -> TabStripState {
    TabStripState {
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
        action_tx: None,
        hovered_tab_idx: None,
        hovered_close_idx: None,
        close_hit_rects: Vec::new(),
        mouse_tracking_armed: false,
        close_min_tab_w_px: 0,
        title_min_tab_w_px: 0,
        close_action: TabCloseAction::CloseWindow,
        tooltip_hwnd_raw: 0,
        tooltip_text: String::new(),
        tooltip_pending_tab: None,
        tooltip_anchor_rect: None,
        tooltip_fade_start: None,
    }
}

/// Global registry: every live strip's state keyed by its hwnd, plus a
/// reverse map from tooltip-popup hwnd to its owning strip's hwnd so the
/// tooltip WndProc can find its parent's state.
struct StripRegistry {
    states: HashMap<isize, TabStripState>,
    tooltip_owners: HashMap<isize, isize>,
}

static REGISTRY: LazyLock<Mutex<StripRegistry>> = LazyLock::new(|| {
    Mutex::new(StripRegistry {
        states: HashMap::new(),
        tooltip_owners: HashMap::new(),
    })
});

fn registry() -> MutexGuard<'static, StripRegistry> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

/// Resolve the strip hwnd that owns the given hwnd. If the hwnd is a
/// strip hwnd, returns it as-is. If it's a tooltip popup hwnd, returns
/// the strip it belongs to. Returns 0 if neither.
fn owner_strip_isize(hwnd: HWND, reg: &StripRegistry) -> isize {
    let h = hwnd.0 as isize;
    if reg.states.contains_key(&h) {
        h
    } else {
        reg.tooltip_owners.get(&h).copied().unwrap_or(0)
    }
}

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

// `tab_screen_rect`, `current_colors`, and `hit_test_screen` are now
// methods on `TabStripOverlay` — see the `impl` block below. With
// multiple strips alive, "the strip" is ambiguous without a handle.

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
    /// `action_tx` receives `TabActionEvent` whenever the user invokes a
    /// tab action (click, middle-click, right-click menu, etc.).
    pub fn new(action_tx: mpsc::Sender<TabActionEvent>) -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("TabStripOverlay::new spawns a layered DWM window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        {
            // Channel stashed inside the per-instance state once the
            // strip hwnd is allocated below. `action_tx` is moved into
            // the spawned closure verbatim.
            let action_tx_for_thread = action_tx;

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
                    // `CS_DBLCLKS` enables `WM_LBUTTONDBLCLK` so the
                    // WndProc can promote a quick second click into a
                    // rename gesture. Without it, the second click is
                    // delivered as a regular `WM_LBUTTONDOWN`.
                    let wc = WNDCLASSW {
                        style: CS_DBLCLKS,
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
                            // Register the per-instance state keyed by
                            // this strip's hwnd. The WndProc and the
                            // daemon-side methods both look it up here.
                            let strip_hwnd_isize = h.0 as isize;
                            {
                                let mut reg = registry();
                                let mut state = default_tab_strip_state();
                                state.action_tx = Some(action_tx_for_thread);
                                reg.states.insert(strip_hwnd_isize, state);
                            }
                            // Create the custom tooltip popup on the
                            // strip's UI thread so its WndProc receives
                            // messages from the same message loop. The
                            // popup stays hidden until WM_TIMER decides
                            // to show it (after the hover delay). Its
                            // hwnd is mapped back to the parent strip
                            // via the registry's `tooltip_owners` so
                            // the popup proc can find its parent state.
                            create_tooltip_popup(h);
                            let _ = tx.send(Ok((strip_hwnd_isize, thread_id)));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                if msg.message == WM_QUIT_TAB_STRIP_THREAD {
                                    break;
                                }
                                let _ = DispatchMessageW(&msg);
                            }
                            // Destroy the tooltip popup first, then
                            // the strip hwnd. Both must be torn down on
                            // this same UI thread (the one that created
                            // them) so their WndProcs unwind cleanly.
                            let tooltip_hwnd_raw = registry()
                                .states
                                .get(&strip_hwnd_isize)
                                .map(|s| s.tooltip_hwnd_raw)
                                .unwrap_or(0);
                            if tooltip_hwnd_raw != 0 {
                                let popup = HWND(tooltip_hwnd_raw as *mut c_void);
                                let _ = DestroyWindow(popup);
                                let _ = registry()
                                    .tooltip_owners
                                    .remove(&tooltip_hwnd_raw);
                            }
                            let _ = DestroyWindow(h);
                            let _ = UnregisterClassW(
                                windows::core::PCWSTR(class_name.as_ptr()),
                                None,
                            );
                            // Drop the per-instance state. UnregisterClass
                            // for the tooltip class is intentionally not
                            // called: many strip instances may share it,
                            // and Windows handles class cleanup on
                            // process exit.
                            let _ = registry().states.remove(&strip_hwnd_isize);
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
        scale_factor: f64,
        close_action: TabCloseAction,
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

        // DPI-scaled thresholds for narrow-tab rendering.
        let close_min_tab_w_px =
            (CLOSE_BTN_MIN_TAB_W_LOGICAL as f64 * scale_factor).round() as i32;
        let title_min_tab_w_px =
            (TITLE_VISIBLE_MIN_TAB_W_LOGICAL as f64 * scale_factor).round() as i32;

        // Close-X hit rects — only populated when the strip is wide
        // enough to render the glyph. Empty vec when every tab is below
        // the threshold; that keeps the WM_LBUTTONDOWN close-first path
        // a no-op without an extra guard.
        //
        // Pill and glyph dimensions are snapped to the same parity as
        // `strip_h` so `(strip_h - bg_h) / 2` yields an exact integer
        // — without parity matching, integer rounding biases the pill
        // 0.5px upward and the X looks visibly off-center.
        //
        // Even spacing: `right_breathing` (pill→tab edge) matches the
        // inside `bg_pad` (glyph→pill edge) so the X reads as evenly
        // padded both inside its button and from the tab boundary.
        // Geometry must mirror render_strip_inner exactly.
        let h_inset = (strip_h / 6).max(4);
        let pill_size_target = (strip_h * 2 / 3).max(16).min(strip_h - 4);
        let pill_size = snap_to_parity(pill_size_target, strip_h);
        let glyph_target = (pill_size * 4 / 9).max(8);
        let glyph_render = snap_to_parity(glyph_target, pill_size);
        let bg_pad = (pill_size - glyph_render) / 2;
        let right_breathing = bg_pad.max(h_inset);
        let mut close_hit_rects = Vec::new();
        if tab_w >= close_min_tab_w_px {
            close_hit_rects.reserve(n);
            for i in 0..n {
                let left = (i as i32) * tab_w;
                let right = if i == n - 1 { strip_w } else { left + tab_w };
                let bg_right = right - right_breathing;
                let bg_w = pill_size;
                let bg_h = pill_size;
                let bg_left = bg_right - bg_w;
                // Both even-or-both-odd, so exact integer center.
                let bg_top = (strip_h - bg_h) / 2;
                let bg_bottom = bg_top + bg_h;
                close_hit_rects.push(RECT {
                    left: bg_left,
                    top: bg_top,
                    right: bg_right,
                    bottom: bg_bottom,
                });
            }
        }
        let _ = bg_pad; // computed for parity assertion; render path re-derives it

        // Detect active-tab change to start a highlight slide animation.
        // The window swap itself is instant; only the strip's highlight
        // bar slides between tabs.
        let new_active = active_idx.min(n.saturating_sub(1));
        let (prev_active, prev_visible) = {
            let reg = registry();
            reg.states
                .get(&(self.hwnd.0 as isize))
                .map(|s| (s.active_idx, s.visible))
                .unwrap_or((new_active, false))
        };
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
        let mut reg = registry();
        if let Some(state) = reg.states.get_mut(&(self.hwnd.0 as isize)) {
            // Preserve hover state across re-renders triggered by
            // icon-poll / title-change ticks (otherwise the X flickers
            // off every couple of seconds while the user is hovering
            // a tab). Only clear when the strip's identity changes
            // (different monitor/workspace/column) OR when the tab
            // count shrinks below the hovered index — both indicate
            // the old hover target is no longer meaningful.
            let identity_changed = state.target_monitor != target_monitor
                || state.target_workspace_idx != target_workspace_idx
                || state.target_column_idx != target_column_idx;
            let hover_out_of_range = state
                .hovered_tab_idx
                .is_some_and(|i| i >= tabs.len());
            if identity_changed || hover_out_of_range {
                state.hovered_tab_idx = None;
                state.hovered_close_idx = None;
            }

            state.tabs = tabs.clone();
            state.active_idx = new_active;
            state.strip_w = strip_w;
            state.strip_h = strip_h;
            state.strip_screen_x = strip_x;
            state.strip_screen_y = strip_y;
            state.visible = true;
            state.hit_rects = hit_rects;
            state.close_hit_rects = close_hit_rects;
            state.colors = colors;
            state.target_monitor = target_monitor;
            state.target_workspace_idx = target_workspace_idx;
            state.target_column_idx = target_column_idx;
            state.transition = transition;
            state.close_min_tab_w_px = close_min_tab_w_px;
            state.title_min_tab_w_px = title_min_tab_w_px;
            state.close_action = close_action;
        }
        drop(reg);

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
        let mut reg = registry();
        if let Some(state) = reg.states.get_mut(&(self.hwnd.0 as isize)) {
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
        let visible = {
            let reg = registry();
            reg.states
                .get(&(self.hwnd.0 as isize))
                .map(|s| s.visible)
                .unwrap_or(false)
        };
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

    /// Resolve a tab's screen-coord rect (top-left x, y + width, height).
    /// Used by the daemon to position the inline rename popup precisely
    /// over the tab cell. Returns `None` if the strip is hidden or the
    /// index is out of range. Safe to call from any thread.
    pub fn tab_screen_rect(&self, tab_idx: usize) -> Option<(i32, i32, i32, i32)> {
        let reg = registry();
        let state = reg.states.get(&(self.hwnd.0 as isize))?;
        if !state.visible {
            return None;
        }
        let rect = state.hit_rects.get(tab_idx)?;
        Some((
            state.strip_screen_x + rect.left,
            state.strip_screen_y + rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        ))
    }

    /// Current colors used by the strip. Used by the inline rename popup
    /// to match the tab's appearance.
    pub fn current_colors(&self) -> Option<TabStripColors> {
        let reg = registry();
        reg.states.get(&(self.hwnd.0 as isize)).map(|s| s.colors)
    }

    /// Hit-test this strip against a screen-coordinate point. Returns
    /// `None` if the strip isn't currently visible, or the point isn't
    /// over any tab rect. Drag-and-drop callers iterate over every live
    /// strip to find the first hit.
    pub fn hit_test_screen(
        &self,
        screen_x: i32,
        screen_y: i32,
    ) -> Option<TabStripHit> {
        let reg = registry();
        let state = reg.states.get(&(self.hwnd.0 as isize))?;
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
pub(crate) fn aa_fill_rounded(
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

/// Bake a soft drop shadow into the BGRA pixel buffer underneath where
/// a rounded-rectangle content area will be drawn. Win11's DWM gives
/// top-level popups a compositor shadow that wraps the *window
/// rectangle* and ignores layered alpha — so any rounded layered
/// content has a visible square-cornered shadow leaking out at the
/// corners. We disable that compositor shadow and bake our own here:
/// rasterize the rounded rect into an alpha mask, slightly offset
/// downward to suggest light from above, then box-blur it for soft
/// edges and composite as premultiplied black at low opacity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bake_soft_shadow(
    pixels: &mut [u8],
    bmp_w: i32,
    bmp_h: i32,
    content_x: i32,
    content_y: i32,
    content_w: i32,
    content_h: i32,
    radius: i32,
) {
    const SHADOW_OFFSET_Y: i32 = 2;
    const BLUR_RADIUS: i32 = 5;
    const SHADOW_OPACITY: u32 = 90; // ~35% of 255

    let n = (bmp_w * bmp_h) as usize;
    let mut alpha = vec![0u8; n];

    let left = content_x;
    let top = content_y + SHADOW_OFFSET_Y;
    let right = content_x + content_w;
    let bottom = content_y + content_h + SHADOW_OFFSET_Y;
    let r = radius
        .min((right - left) / 2)
        .min((bottom - top) / 2)
        .max(0);

    for y in top.max(0)..bottom.min(bmp_h) {
        for x in left.max(0)..right.min(bmp_w) {
            let coverage = pixel_coverage_rounded(x, y, left, top, right, bottom, r);
            if coverage > 0.0 {
                let idx = (y * bmp_w + x) as usize;
                alpha[idx] = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    let mut tmp = vec![0u8; n];
    box_blur_alpha_horizontal(&alpha, &mut tmp, bmp_w, bmp_h, BLUR_RADIUS);
    box_blur_alpha_vertical(&tmp, &mut alpha, bmp_w, bmp_h, BLUR_RADIUS);

    for (i, &a) in alpha.iter().enumerate() {
        if a == 0 {
            continue;
        }
        let scaled = (a as u32 * SHADOW_OPACITY / 255) as u8;
        let pi = i * 4;
        // Premultiplied black: RGB = 0, A = shadow alpha.
        pixels[pi] = 0;
        pixels[pi + 1] = 0;
        pixels[pi + 2] = 0;
        pixels[pi + 3] = scaled;
    }
}

fn box_blur_alpha_horizontal(src: &[u8], dst: &mut [u8], w: i32, h: i32, r: i32) {
    for y in 0..h {
        let row = (y * w) as usize;
        for x in 0..w {
            let lo = (x - r).max(0);
            let hi = (x + r).min(w - 1);
            let mut sum: u32 = 0;
            for nx in lo..=hi {
                sum += src[row + nx as usize] as u32;
            }
            let count = (hi - lo + 1) as u32;
            dst[row + x as usize] = (sum / count) as u8;
        }
    }
}

fn box_blur_alpha_vertical(src: &[u8], dst: &mut [u8], w: i32, h: i32, r: i32) {
    for x in 0..w {
        for y in 0..h {
            let lo = (y - r).max(0);
            let hi = (y + r).min(h - 1);
            let mut sum: u32 = 0;
            for ny in lo..=hi {
                sum += src[(ny * w + x) as usize] as u32;
            }
            let count = (hi - lo + 1) as u32;
            dst[(y * w + x) as usize] = (sum / count) as u8;
        }
    }
}

/// Anti-aliased diagonal-line rasterizer used to draw the close-X
/// glyph. GDI's `CreatePen` only supports integer stroke widths, so a
/// 1.5px-equivalent stroke requires manual sub-pixel coverage. This
/// function computes pixel coverage by perpendicular distance from the
/// line segment and composites with source-over (lerp toward the pen
/// color in the existing buffer).
///
/// `half_width` is the stroke half-width in pixels (use 0.75 for a
/// 1.5px line). `color_bgra` is packed `0xBBGGRR`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn aa_fill_line_stroke(
    pixels: &mut [u8],
    bmp_w: i32,
    bmp_h: i32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    half_width: f32,
    color_bgra: u32,
) {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let line_len_sq = dx * dx + dy * dy;
    if line_len_sq < 1.0e-3 {
        return;
    }

    let pad = half_width + 1.0;
    let bx_min = ((x1.min(x2) - pad).floor() as i32).max(0);
    let bx_max = ((x1.max(x2) + pad).ceil() as i32).min(bmp_w);
    let by_min = ((y1.min(y2) - pad).floor() as i32).max(0);
    let by_max = ((y1.max(y2) + pad).ceil() as i32).min(bmp_h);

    let src_b = (color_bgra & 0xFF) as f32;
    let src_g = ((color_bgra >> 8) & 0xFF) as f32;
    let src_r = ((color_bgra >> 16) & 0xFF) as f32;

    for y in by_min..by_max {
        for x in bx_min..bx_max {
            // Pixel-center sample.
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            // Project (px, py) onto the line; clamp t to [0, 1] so the
            // stroke has flat (not infinitely long) endpoints.
            let t = (((px - x1) * dx + (py - y1) * dy) / line_len_sq).clamp(0.0, 1.0);
            let qx = x1 + dx * t;
            let qy = y1 + dy * t;
            let dxp = px - qx;
            let dyp = py - qy;
            let dist = (dxp * dxp + dyp * dyp).sqrt();
            // Linear falloff over the last pixel of the stroke edge.
            let coverage = (half_width + 0.5 - dist).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let idx = ((y * bmp_w + x) * 4) as usize;
            let cur_b = pixels[idx] as f32;
            let cur_g = pixels[idx + 1] as f32;
            let cur_r = pixels[idx + 2] as f32;
            let cur_a = pixels[idx + 3] as f32;
            // Lerp toward source by coverage (non-premultiplied RGB).
            pixels[idx] = (src_b * coverage + cur_b * (1.0 - coverage))
                .round()
                .clamp(0.0, 255.0) as u8;
            pixels[idx + 1] = (src_g * coverage + cur_g * (1.0 - coverage))
                .round()
                .clamp(0.0, 255.0) as u8;
            pixels[idx + 2] = (src_r * coverage + cur_r * (1.0 - coverage))
                .round()
                .clamp(0.0, 255.0) as u8;
            // Bump alpha up if we're drawing into an under-alpha region
            // (e.g. just outside the pill bg). Otherwise the existing
            // (fully opaque) bg pixel stays at 255.
            pixels[idx + 3] = (255.0 * coverage + cur_a * (1.0 - coverage))
                .round()
                .clamp(0.0, 255.0) as u8;
        }
    }
}

/// Render the tab strip bitmap and update the layered window. Reads
/// tabs/active_idx/colors/transition from the strip's registry entry,
/// computes the interpolated highlight rect, and pushes the result via
/// `UpdateLayeredWindow`. Called both from `TabStripOverlay::show` and
/// from the WndProc's `WM_TIMER` handler during a highlight slide.
fn render_strip_now(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (tabs, active_idx, colors, transition, hovered, hovered_close, close_min, title_min) = {
        let reg = registry();
        let Some(s) = reg.states.get(&(hwnd.0 as isize)) else { return };
        (
            s.tabs.clone(),
            s.active_idx,
            s.colors,
            s.transition,
            s.hovered_tab_idx,
            s.hovered_close_idx,
            s.close_min_tab_w_px,
            s.title_min_tab_w_px,
        )
    };
    if tabs.is_empty() {
        return;
    }
    render_strip_inner(
        hwnd, x, y, w, h, &tabs, active_idx, colors, transition, hovered, hovered_close,
        close_min, title_min,
    );
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
    hovered_tab_idx: Option<usize>,
    hovered_close_idx: Option<usize>,
    close_min_tab_w_px: i32,
    title_min_tab_w_px: i32,
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

            // Font: Segoe UI ~12 px char height (matches Win11
            // Terminal / Edge tab title) at default DPI. Scales
            // linearly with strip height; floor of 8 px keeps it
            // legible at small DPIs. `height < 0` in CreateFontW means
            // character height (cell ascender), > 0 means cell height.
            let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
            let hfont = CreateFontW(
                -(((h as f32 * 0.43).round() as i32).max(8)),
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
                // Icon size: `0.57 * strip_h` (≈ 16 px at 28 px strip)
                // matches Win11 Terminal / Edge tab icon proportions.
                // Floor of 10 px keeps it recognizable at small DPIs.
                let icon_size = ((h as f32 * 0.57).round() as i32).max(10);
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

                // Title is gated on the narrow-tab threshold — below it,
                // tabs render icon-only so the strip stays legible when
                // many tabs share a column.
                if tab_w >= title_min_tab_w_px {
                    let mut wide: Vec<u16> = tab.title.encode_utf16().collect();
                    if wide.is_empty() {
                        wide.push(0);
                    }

                    // Use DrawTextW with DT_END_ELLIPSIS so over-long titles
                    // ellipsize automatically rather than overflow into the
                    // next tab. Right padding mirrors the icon's
                    // inside-padding so the text has matching breathing
                    // room from the highlight's right edge.
                    //
                    // Reserve room on the right for the close-X glyph
                    // (only when this tab is the hover target AND the
                    // strip is wide enough to draw it) — otherwise the
                    // ellipsis lands underneath the X.
                    let mut padded = tab_rect;
                    padded.left = text_left;
                    let mut right_pad = h_inset + icon_inside_pad;
                    if tab_w >= close_min_tab_w_px
                        && hovered_tab_idx == Some(i)
                    {
                        // Reserve space for the bg pill + its outer
                        // breathing room so the title ellipsis lands
                        // well clear of the button.
                        let pill_target = (h * 2 / 3).max(16).min(h - 4);
                        let pill = snap_to_parity(pill_target, h);
                        let glyph_target = (pill * 4 / 9).max(8);
                        let glyph = snap_to_parity(glyph_target, pill);
                        let bg_pad = (pill - glyph) / 2;
                        let right_breathing = bg_pad.max(h_inset);
                        right_pad = right_pad.max(right_breathing + pill + (h_inset / 2).max(3));
                    }
                    padded.right = tab_rect.right.saturating_sub(right_pad);
                    let _ = DrawTextW(
                        hdc_mem,
                        &mut wide,
                        &mut padded,
                        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }

                // Close-X glyph on hover. Geometry mirrors
                // `close_hit_rects` in `show()` exactly — parity-snapped
                // pill so the pill centers cleanly in the strip and the
                // glyph centers cleanly in the pill. Inside pad (glyph
                // → pill edge) equals outside pad (pill → tab edge) for
                // visually even spacing on both sides.
                if tab_w >= close_min_tab_w_px && hovered_tab_idx == Some(i) {
                    let pill_target = (h * 2 / 3).max(16).min(h - 4);
                    let pill = snap_to_parity(pill_target, h);
                    // Glyph is a touch smaller than 1/2 the pill so the
                    // X has room to breathe inside the button.
                    let glyph_target = (pill * 4 / 9).max(8);
                    let glyph = snap_to_parity(glyph_target, pill);
                    let bg_pad = (pill - glyph) / 2;
                    let right_breathing = bg_pad.max(h_inset);
                    let bg_right = tab_rect.right - right_breathing;
                    let bg_left = bg_right - pill;
                    let bg_top = (h - pill) / 2;
                    let bg_bottom = bg_top + pill;
                    let glyph_right = bg_right - bg_pad;
                    let glyph_left = glyph_right - glyph;
                    let glyph_top = bg_top + bg_pad;
                    let glyph_bottom = glyph_top + glyph;

                    // Rounded-square hover background (Zen-style). Only
                    // drawn when the mouse is directly over the pill —
                    // when the user is just hovering the tab body, the
                    // X stays glyph-only for a lighter feel.
                    if hovered_close_idx == Some(i) {
                        let bg_radius = (pill / 3).clamp(3, pill / 2);
                        let bg_color: u32 = 0x3A3A3A;
                        aa_fill_rounded(
                            pixels, w, h, bg_left, bg_top, bg_right, bg_bottom, bg_radius,
                            bg_color,
                        );
                    }

                    // Stroke half-width scales with strip height for
                    // DPI-aware thickness. At default h=28, this is
                    // 28/37 ≈ 0.76, yielding a ~1.5px visual stroke.
                    // At higher DPI strip heights (e.g. h=42 / 150%),
                    // scales to ~1.14 → ~2.3px stroke.
                    let half_width = (h as f32 / 37.0).max(0.6);
                    // Center the line on the half-pixel boundary so the
                    // 1.5px stroke samples symmetrically around the
                    // pill's geometric diagonal.
                    let gl = glyph_left as f32 + 0.5;
                    let gt = glyph_top as f32 + 0.5;
                    let gr = glyph_right as f32 - 0.5;
                    let gb = glyph_bottom as f32 - 0.5;
                    aa_fill_line_stroke(pixels, w, h, gl, gt, gr, gb, half_width, color);
                    aa_fill_line_stroke(pixels, w, h, gr, gt, gl, gb, half_width, color);
                }
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
        // Signal the strip thread to exit its message loop. It cleans
        // up its own hwnd, the tooltip popup, and its registry entries
        // before returning, so Drop just needs to wake it and join.
        //
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
    }
}

/// Tooltip text the daemon wants displayed for a given close-action.
/// Mirrors the menu-item wording from the right-click context menu so
/// the user sees consistent vocabulary across surfaces.
fn tooltip_text_for(close_action: TabCloseAction) -> &'static str {
    match close_action {
        TabCloseAction::CloseWindow => "Close window",
        TabCloseAction::Untab => "Untab this window",
    }
}

/// Whether the user has the system "Apps use dark theme" toggle on.
/// Read from the standard Personalize registry key — the same value
/// File Explorer / Settings / WinUI apps check. Cached cheaply; lookup
/// happens once per tooltip show.
fn is_dark_mode() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// `GetSysColor` returns a `COLORREF` packed as `0x00BBGGRR`. Our pixel
/// buffer uses the same BGR layout, so the value passes through.
unsafe fn sys_color_bgr(idx: windows::Win32::Graphics::Gdi::SYS_COLOR_INDEX) -> u32 {
    windows::Win32::Graphics::Gdi::GetSysColor(idx)
}

/// Resolve the tooltip palette from the live system theme:
/// - High contrast: use system theme colors directly (mandatory for
///   accessibility — hardcoded palettes are illegal under HC).
/// - Dark mode: Win11 Fluent dark tooltip palette.
/// - Light mode: Win11 Fluent light tooltip palette.
///
/// Returns `(bg, text, border)` packed as `0xBBGGRR`.
pub(crate) unsafe fn tooltip_theme_colors() -> (u32, u32, u32) {
    use windows::Win32::Graphics::Gdi::{
        COLOR_WINDOW, COLOR_WINDOWFRAME, COLOR_WINDOWTEXT,
    };
    if crate::is_high_contrast_enabled() {
        return (
            sys_color_bgr(COLOR_WINDOW),
            sys_color_bgr(COLOR_WINDOWTEXT),
            sys_color_bgr(COLOR_WINDOWFRAME),
        );
    }
    if is_dark_mode() {
        // Matches Terminal / Win11 system tooltip in dark mode:
        // - bg is dark gray (not pure black)
        // - border is near-black, barely perceptible — separation
        //   comes from the drop shadow, not the border line
        // - text is near-white with a hair of warmth for legibility
        (0x202020, 0xE6E6E6, 0x0F0F0F)
    } else {
        // Matches Win11 system tooltip in light mode.
        (0xF9F9F9, 0x1C1C1C, 0xC8C8C8)
    }
}

/// Create the custom tooltip popup window on the strip's UI thread.
/// The popup is a borderless, non-layered, top-level window — a
/// `WS_EX_LAYERED` parent doesn't reliably route mouse events into the
/// comctl32 `TOOLTIPS_CLASS` control, so this avoids that path
/// entirely. The popup stays hidden until the strip's WndProc decides
/// it should appear (see `TOOLTIP_DELAY_TIMER_ID`).
unsafe fn create_tooltip_popup(strip_hwnd: HWND) {
    let class_name: Vec<u16> = "LeopardWMTabTooltip\0".encode_utf16().collect();
    let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
    // No hbrBackground — `UpdateLayeredWindow` writes the pixel buffer
    // directly. CS_DROPSHADOW is intentionally NOT set: it draws a
    // classic rectangular shadow that ignores the layered window's
    // transparent rounded corners, so the bottom-right corner of the
    // rectangle pokes through as a visible artifact. Win11 already
    // gives top-level popups a proper DWM compositor shadow.
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(tooltip_popup_proc),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        hCursor: arrow,
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);

    let popup = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
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
    );
    match popup {
        Ok(popup) => {
            // Opt out of Win11's automatic DWM rounded-corner shadow.
            // DWM draws the shadow around the *window rectangle*, ignoring
            // our layered-alpha rounded corners — that's the source of the
            // rectangular shadow artifact at the bottom-right. We bake our
            // own soft shadow into the bitmap instead.
            use windows::Win32::Graphics::Dwm::{
                DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
            };
            let pref = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(
                popup,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const c_void,
                std::mem::size_of_val(&pref) as u32,
            );
            let policy = DWMNCRP_DISABLED;
            let _ = DwmSetWindowAttribute(
                popup,
                DWMWA_NCRENDERING_POLICY,
                &policy as *const _ as *const c_void,
                std::mem::size_of_val(&policy) as u32,
            );
            // Stash the tooltip's hwnd in the parent strip's state and
            // register the reverse `tooltip_hwnd → strip_hwnd` mapping
            // so the tooltip's WndProc can find its parent state.
            let strip_hwnd_isize = strip_hwnd.0 as isize;
            let popup_isize = popup.0 as isize;
            let mut reg = registry();
            if let Some(state) = reg.states.get_mut(&strip_hwnd_isize) {
                state.tooltip_hwnd_raw = popup_isize;
            }
            reg.tooltip_owners.insert(popup_isize, strip_hwnd_isize);
            tracing::debug!("Tab tooltip popup created: hwnd={:?}", popup.0);
        }
        Err(e) => {
            tracing::warn!("Failed to create tab tooltip popup: {}", e);
        }
    }
}

/// Create a font matching the system tooltip / message dialog look.
/// On Win11 this resolves to Segoe UI Variable Text at the user's
/// chosen size — exactly the same font and weight Terminal / Settings
/// use for their tooltips. Falls back to Segoe UI on older Windows.
pub(crate) unsafe fn create_tooltip_font() -> HFONT {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    if ok.is_ok() {
        // `lfMessageFont` is the system message-area font — what
        // dialogs / WinUI tooltips render with. Use it verbatim so we
        // inherit the user's font preference + DPI scaling.
        let lf = &ncm.lfMessageFont;
        return windows::Win32::Graphics::Gdi::CreateFontIndirectW(lf as *const _);
    }
    // Fallback: Segoe UI at a sensible size.
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem();
    let height_px = (14i32 * dpi as i32 / 96).max(11);
    CreateFontW(
        -height_px,
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
    )
}

/// Show the tooltip popup beneath the given close-X hit rect, sized to
/// fit the supplied text. Coordinates are in screen space. Safe to
/// call when the popup hasn't been created (no-op).
unsafe fn show_tooltip_popup(strip_hwnd: HWND, text: &str, anchor_screen_rect: RECT) {
    let popup_hwnd_raw = {
        let mut reg = registry();
        let Some(state) = reg.states.get_mut(&(strip_hwnd.0 as isize)) else {
            return;
        };
        state.tooltip_text = text.to_string();
        state.tooltip_hwnd_raw
    };
    if popup_hwnd_raw == 0 {
        return;
    }
    let popup = HWND(popup_hwnd_raw as *mut c_void);

    // Measure the text with DT_CALCRECT against the same font we'll
    // render with so the popup width is exact.
    let hdc_screen = GetDC(None);
    let font = create_tooltip_font();
    let old_font = SelectObject(hdc_screen, font.into());
    let mut text_utf16: Vec<u16> = text.encode_utf16().collect();
    if text_utf16.is_empty() {
        text_utf16.push(0);
    }
    let mut measure = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = DrawTextW(
        hdc_screen,
        &mut text_utf16,
        &mut measure,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc_screen, old_font);
    let _ = DeleteObject(font.into());
    ReleaseDC(None, hdc_screen);

    // Padding to match Win11 Terminal tooltip chrome — generous
    // horizontal pad, modest vertical pad. 1px AA border is rendered
    // into the bitmap as the outer rounded shape minus an inset
    // rounded fill (no extra space needed inside the padding).
    let pad_x = 14i32;
    let pad_y = 8i32;
    let popup_w = (measure.right - measure.left) + pad_x * 2;
    let popup_h = (measure.bottom - measure.top) + pad_y * 2;

    // Center horizontally under the anchor; sit a few px below it.
    let anchor_w = anchor_screen_rect.right - anchor_screen_rect.left;
    let anchor_cx = anchor_screen_rect.left + anchor_w / 2;
    let popup_x = anchor_cx - popup_w / 2;
    let popup_y = anchor_screen_rect.bottom + 6;

    // Layered + per-pixel alpha — gives smooth AA rounded corners with
    // a properly-shaped drop shadow baked into the bitmap. `SetWindowRgn`
    // would clip at pixel boundaries (visible jaggies), and DWM's own
    // compositor shadow wraps the *window rectangle* (visible square
    // shadow artifact at the corners) — so we draw our own.
    //
    // Start at SCA=0 (invisible) then drive the fade via a WM_TIMER
    // that re-renders with progressively higher SCA. The bitmap itself
    // doesn't change between frames, only the layered blend's alpha.
    render_tooltip_layered(popup, popup_x, popup_y, popup_w, popup_h, text, 0);
    let _ = ShowWindow(popup, SW_SHOWNOACTIVATE);

    // Record the anchor + fade start in state so the fade timer can
    // recompute the same position+size on each tick.
    {
        let mut reg = registry();
        if let Some(state) = reg.states.get_mut(&(strip_hwnd.0 as isize)) {
            state.tooltip_anchor_rect = Some(anchor_screen_rect);
            state.tooltip_fade_start = Some(std::time::Instant::now());
        }
    }
    // Drive the fade from the strip's WndProc, where WM_TIMER is
    // already pumped. The strip HWND is the popup's owner conceptually,
    // but `SetTimer` here uses the popup hwnd so the timer message
    // routes into `tooltip_popup_proc`.
    let _ = SetTimer(
        Some(popup),
        TOOLTIP_FADE_TIMER_ID,
        TOOLTIP_FADE_INTERVAL_MS,
        None,
    );
}

/// Render the tooltip into a 32-bpp BGRA DIB and push it via
/// `UpdateLayeredWindow`. Mirrors the strip's render pipeline: AA
/// rounded fill for bg + border, GDI text overlay, alpha fixup,
/// premultiplied composite. Drop shadow follows the rendered alpha.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn render_tooltip_layered(
    popup: HWND,
    content_x: i32,
    content_y: i32,
    content_w: i32,
    content_h: i32,
    text: &str,
    overall_alpha: u8,
) {
    if content_w <= 0 || content_h <= 0 {
        return;
    }
    let (bg, text_color, _border) = tooltip_theme_colors();
    let is_hc = crate::is_high_contrast_enabled();

    // Shadow padding around the content rect. Zero in HC mode (HC
    // themes don't render decorative shadows — they use solid borders
    // via system colors). Otherwise reserves space on all sides for a
    // soft baked-in drop shadow.
    let shadow_margin: i32 = if is_hc { 0 } else { 14 };
    let w = content_w + shadow_margin * 2;
    let h = content_h + shadow_margin * 2;
    let cx = shadow_margin; // content origin inside bitmap
    let cy = shadow_margin;
    let window_x = content_x - shadow_margin;
    let window_y = content_y - shadow_margin;

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
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

    let hdc_screen = GetDC(None);
    let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
    let old_bm = SelectObject(hdc_mem, hbitmap.into());

    let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    pixels.fill(0);

    let radius = if is_hc { 2 } else { (content_h / 5).max(5) };

    // Bake a soft drop shadow into the bitmap underneath where the
    // content rect will land. DWM's own compositor shadow tracks the
    // *window rectangle* (square) and ignores layered alpha — so we
    // disabled it via `DwmSetWindowAttribute` and draw our own here.
    if !is_hc {
        bake_soft_shadow(pixels, w, h, cx, cy, content_w, content_h, radius);
    }

    // Single rounded fill — no border. The drop shadow above gives
    // separation. High contrast themes use sharper near-square corners
    // by convention.
    aa_fill_rounded(
        pixels,
        w,
        h,
        cx,
        cy,
        cx + content_w,
        cy + content_h,
        radius,
        bg,
    );

    // Text — same font we measured with, themed text color. Drawn
    // through GDI inside the content rect (offset by shadow margin).
    let font = create_tooltip_font();
    let old_font = SelectObject(hdc_mem, font.into());
    SetTextColor(hdc_mem, windows::Win32::Foundation::COLORREF(text_color));
    SetBkMode(hdc_mem, TRANSPARENT);
    let mut text_utf16: Vec<u16> = text.encode_utf16().collect();
    if text_utf16.is_empty() {
        text_utf16.push(0);
    }
    let mut text_rect = RECT {
        left: cx,
        top: cy,
        right: cx + content_w,
        bottom: cy + content_h,
    };
    let _ = DrawTextW(
        hdc_mem,
        &mut text_utf16,
        &mut text_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc_mem, old_font);
    let _ = DeleteObject(font.into());

    // Same alpha-fixup pass the strip uses: promote GDI-written pixels
    // (alpha=0, RGB!=0) to opaque, then premultiply for `AC_SRC_ALPHA`.
    let mut i = 0usize;
    while i < pixels.len() {
        let mut a = pixels[i + 3];
        if a == 0 && (pixels[i] != 0 || pixels[i + 1] != 0 || pixels[i + 2] != 0) {
            a = 0xFF;
            pixels[i + 3] = a;
        }
        if a < 0xFF {
            let af = a as u32;
            pixels[i] = ((pixels[i] as u32 * af) / 255) as u8;
            pixels[i + 1] = ((pixels[i + 1] as u32 * af) / 255) as u8;
            pixels[i + 2] = ((pixels[i + 2] as u32 * af) / 255) as u8;
        }
        i += 4;
    }

    // Bake a subtle ~6% translucency directly into the bitmap. Doing
    // this via SourceConstantAlpha confuses DWM's drop shadow (it
    // samples the layered alpha twice and produces a dark gradient
    // artifact in the corners), so we scale RGBA together to preserve
    // the premultiplied relationship. Skipped in High Contrast — those
    // themes mandate solid, fully-opaque backgrounds for legibility.
    if !is_hc {
        const BLEED_SCALE: u32 = 240; // ~94%
        let mut i = 0usize;
        while i < pixels.len() {
            pixels[i] = ((pixels[i] as u32 * BLEED_SCALE) / 255) as u8;
            pixels[i + 1] = ((pixels[i + 1] as u32 * BLEED_SCALE) / 255) as u8;
            pixels[i + 2] = ((pixels[i + 2] as u32 * BLEED_SCALE) / 255) as u8;
            pixels[i + 3] = ((pixels[i + 3] as u32 * BLEED_SCALE) / 255) as u8;
            i += 4;
        }
    }

    let pt_dst = POINT { x: window_x, y: window_y };
    let sz = SIZE { cx: w, cy: h };
    let pt_src = POINT { x: 0, y: 0 };
    // SourceConstantAlpha scales the per-pixel alpha uniformly — drives
    // the fade-in. The DWM compositor shadow is disabled on this popup
    // (see `create_tooltip_popup`), so the SCA-vs-shadow interaction
    // bug we saw earlier doesn't apply: SCA < 255 only fades the
    // already-baked-in content + shadow.
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: overall_alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let _ = UpdateLayeredWindow(
        popup,
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
}

unsafe fn hide_tooltip_popup(strip_hwnd: HWND) {
    let popup_hwnd_raw = {
        let mut reg = registry();
        let Some(s) = reg.states.get_mut(&(strip_hwnd.0 as isize)) else {
            return;
        };
        // Clear fade state so any in-flight fade tick exits cleanly
        // and the next show starts a fresh animation.
        s.tooltip_fade_start = None;
        s.tooltip_anchor_rect = None;
        s.tooltip_hwnd_raw
    };
    if popup_hwnd_raw == 0 {
        return;
    }
    let popup = HWND(popup_hwnd_raw as *mut c_void);
    let _ = KillTimer(Some(popup), TOOLTIP_FADE_TIMER_ID);
    let _ = ShowWindow(popup, SW_HIDE);
}

/// WndProc for the custom tooltip popup. Renders a dark rounded
/// rectangle with white text via GDI; opts out of activation so it
/// doesn't steal focus when shown.
unsafe extern "system" fn tooltip_popup_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if msg == WM_ERASEBKGND {
        // We paint everything in WM_PAINT.
        return LRESULT(1);
    }
    // Fade-in animation: scale `SourceConstantAlpha` from 0 → 255 over
    // `TOOLTIP_FADE_MS`, re-rendering the layered bitmap each tick.
    // The bitmap rasterization is cheap (a few thousand pixels) so it
    // costs less than caching the DIB section across frames.
    if msg == WM_TIMER && wparam.0 == TOOLTIP_FADE_TIMER_ID {
        let snapshot = {
            let reg = registry();
            let key = owner_strip_isize(hwnd, &reg);
            let Some(state) = reg.states.get(&key) else {
                let _ = KillTimer(Some(hwnd), TOOLTIP_FADE_TIMER_ID);
                return LRESULT(0);
            };
            match (state.tooltip_fade_start, state.tooltip_anchor_rect) {
                (Some(start), Some(rect)) => Some((start, rect, state.tooltip_text.clone())),
                _ => None,
            }
        };
        let Some((start, anchor, text)) = snapshot else {
            let _ = KillTimer(Some(hwnd), TOOLTIP_FADE_TIMER_ID);
            return LRESULT(0);
        };
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let progress = (elapsed_ms.min(TOOLTIP_FADE_MS) as f32) / TOOLTIP_FADE_MS as f32;
        // Ease-out cubic for a snappy reveal that settles smoothly.
        let eased = 1.0 - (1.0 - progress).powi(3);
        let alpha = (eased * 255.0).round().clamp(0.0, 255.0) as u8;

        // Recompute position + size from the anchor (cheap text measure).
        render_tooltip_at_alpha(hwnd, &text, &anchor, alpha);

        if elapsed_ms >= TOOLTIP_FADE_MS {
            let _ = KillTimer(Some(hwnd), TOOLTIP_FADE_TIMER_ID);
            let mut reg = registry();
            let key = owner_strip_isize(hwnd, &reg);
            if let Some(state) = reg.states.get_mut(&key) {
                state.tooltip_fade_start = None;
            }
        }
        return LRESULT(0);
    }
    // Layered windows render via `UpdateLayeredWindow` only; WM_PAINT
    // doesn't fire. No paint handler needed.
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Re-measure, position, and render the tooltip at a given overall
/// alpha — used by the fade-in WM_TIMER to drive the animation. Factored
/// out of `show_tooltip_popup` so the initial show and each fade tick
/// can share the same measurement and positioning code path.
unsafe fn render_tooltip_at_alpha(
    popup: HWND,
    text: &str,
    anchor_screen_rect: &RECT,
    alpha: u8,
) {
    let hdc_screen = GetDC(None);
    let font = create_tooltip_font();
    let old_font = SelectObject(hdc_screen, font.into());
    let mut text_utf16: Vec<u16> = text.encode_utf16().collect();
    if text_utf16.is_empty() {
        text_utf16.push(0);
    }
    let mut measure = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = DrawTextW(
        hdc_screen,
        &mut text_utf16,
        &mut measure,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc_screen, old_font);
    let _ = DeleteObject(font.into());
    ReleaseDC(None, hdc_screen);

    let pad_x = 14i32;
    let pad_y = 8i32;
    let popup_w = (measure.right - measure.left) + pad_x * 2;
    let popup_h = (measure.bottom - measure.top) + pad_y * 2;
    let anchor_w = anchor_screen_rect.right - anchor_screen_rect.left;
    let anchor_cx = anchor_screen_rect.left + anchor_w / 2;
    let popup_x = anchor_cx - popup_w / 2;
    let popup_y = anchor_screen_rect.bottom + 6;

    render_tooltip_layered(popup, popup_x, popup_y, popup_w, popup_h, text, alpha);
}

/// Generate a 16-logical-pixel-square (DPI-scaled) PARGB bitmap with a
/// Segoe Fluent Icons glyph rendered at `color_bgr`. Renders at 3× the
/// target size with greyscale AA, then downsamples to the target via a
/// 3×3 box filter. The supersample is what produces clean curves at
/// menu-icon resolution — direct 16px rendering produces visible stair-
/// step edges no matter how good the AA mode.
///
/// Output is premultiplied 32-bpp BGRA so the Win11 modern menu
/// compositor renders it cleanly through `MIIM_BITMAP` — same path
/// Edge / Explorer use for their menu icons.
pub(crate) unsafe fn create_menu_glyph_bitmap(glyph: u16, color_bgr: u32) -> HBITMAP {
    let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem();
    let size = ((16i32 * dpi as i32) / 96).max(16);
    create_glyph_bitmap_at_size(glyph, color_bgr, size)
}

/// Like `create_menu_glyph_bitmap` but renders at a caller-specified
/// pixel size. Used by the rename popup's check button, where the
/// target rect is computed from the tab cell rather than the menu's
/// fixed 16-logical-pixel icon column.
pub(crate) unsafe fn create_glyph_bitmap_at_size(
    glyph: u16,
    color_bgr: u32,
    size: i32,
) -> HBITMAP {
    if size <= 0 {
        return HBITMAP::default();
    }
    const SS: i32 = 3; // supersample factor
    let big = size * SS;

    // Render the glyph as white text into a `big × big` temp DIB.
    // We read its green channel as the alpha mask later.
    let bmi_big = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: big,
            biHeight: -big,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut big_bits: *mut c_void = std::ptr::null_mut();
    let Ok(big_bmp) =
        CreateDIBSection(None, &bmi_big, DIB_RGB_COLORS, &mut big_bits, None, 0)
    else {
        return HBITMAP::default();
    };
    let hdc_screen = GetDC(None);
    let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
    let old_bm = SelectObject(hdc_mem, big_bmp.into());

    let big_pixels =
        std::slice::from_raw_parts_mut(big_bits as *mut u8, (big * big * 4) as usize);
    big_pixels.fill(0);

    let face: Vec<u16> = "Segoe Fluent Icons\0".encode_utf16().collect();
    // Glyph fills ~80% of the cell — matches Terminal's visual weight.
    let glyph_h_px = (big * 8) / 10;
    let font = CreateFontW(
        -glyph_h_px,
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
        ANTIALIASED_QUALITY,
        FONT_PIPELINE_DEFAULT_PITCH_AND_FAMILY,
        windows::core::PCWSTR(face.as_ptr()),
    );
    let old_font = SelectObject(hdc_mem, font.into());
    SetTextColor(hdc_mem, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
    SetBkMode(hdc_mem, TRANSPARENT);
    let mut text = [glyph, 0];
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: big,
        bottom: big,
    };
    let _ = DrawTextW(
        hdc_mem,
        &mut text[..1],
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );

    SelectObject(hdc_mem, old_font);
    let _ = DeleteObject(font.into());
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(None, hdc_screen);

    // Snapshot the supersampled green channel as the alpha mask, then
    // free the big DIB before we allocate the target one.
    let mut mask: Vec<u8> = Vec::with_capacity((big * big) as usize);
    {
        let mut i = 0usize;
        while i < big_pixels.len() {
            mask.push(big_pixels[i + 1]);
            i += 4;
        }
    }
    let _ = DeleteObject(big_bmp.into());

    // Allocate the target-size PARGB bitmap and downsample with an
    // unweighted box filter — average of SS×SS source pixels per
    // destination pixel.
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
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
        return HBITMAP::default();
    };
    let pixels =
        std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
    pixels.fill(0);

    let src_b = color_bgr & 0xFF;
    let src_g = (color_bgr >> 8) & 0xFF;
    let src_r = (color_bgr >> 16) & 0xFF;
    let divisor = (SS * SS) as u32;
    for dy in 0..size {
        for dx in 0..size {
            let mut sum: u32 = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let bx = dx * SS + sx;
                    let by = dy * SS + sy;
                    sum += mask[(by * big + bx) as usize] as u32;
                }
            }
            let alpha = sum / divisor;
            let idx = ((dy * size + dx) * 4) as usize;
            pixels[idx] = ((src_b * alpha) / 255) as u8;
            pixels[idx + 1] = ((src_g * alpha) / 255) as u8;
            pixels[idx + 2] = ((src_r * alpha) / 255) as u8;
            pixels[idx + 3] = alpha as u8;
        }
    }

    hbitmap
}

/// WndProc: handles WM_LBUTTONDOWN to translate clicks into TabActionEvents.
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
            let mut reg = registry();
            let Some(state) = reg.states.get_mut(&(hwnd.0 as isize)) else {
                return LRESULT(0);
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
    // Hover tracking. Windows clears the TME_LEAVE subscription each
    // time it fires WM_MOUSELEAVE, so re-arm on the next mouse entry.
    if msg == WM_MOUSEMOVE {
        let raw = lparam.0 as u32;
        let mouse_x = (raw & 0xFFFF) as i16 as i32;
        let mouse_y = ((raw >> 16) & 0xFFFF) as i16 as i32;
        let mut needs_repaint = false;
        let (strip_x, strip_y, strip_w, strip_h, close_hover_changed, now_over_close) = {
            let mut reg = registry();
            let Some(state) = reg.states.get_mut(&(hwnd.0 as isize)) else {
                return LRESULT(0);
            };
            if !state.mouse_tracking_armed {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                state.mouse_tracking_armed = true;
            }
            let new_hover = state.hit_rects.iter().enumerate().find_map(|(i, r)| {
                if mouse_x >= r.left
                    && mouse_x < r.right
                    && mouse_y >= r.top
                    && mouse_y < r.bottom
                {
                    Some(i)
                } else {
                    None
                }
            });
            // Whether the mouse is over a close-X glyph specifically.
            // Drives the button-style hover bg behind the X — so the X
            // reads as a clickable button when targeted, plain when the
            // user is just hovering the tab body.
            let new_close_hover =
                state.close_hit_rects.iter().enumerate().find_map(|(i, r)| {
                    if mouse_x >= r.left
                        && mouse_x < r.right
                        && mouse_y >= r.top
                        && mouse_y < r.bottom
                    {
                        Some(i)
                    } else {
                        None
                    }
                });
            if state.hovered_tab_idx != new_hover {
                state.hovered_tab_idx = new_hover;
                needs_repaint = true;
            }
            let close_hover_changed = state.hovered_close_idx != new_close_hover;
            if close_hover_changed {
                state.hovered_close_idx = new_close_hover;
                state.tooltip_pending_tab = new_close_hover;
                needs_repaint = true;
            }
            (
                state.strip_screen_x,
                state.strip_screen_y,
                state.strip_w,
                state.strip_h,
                close_hover_changed,
                new_close_hover.is_some(),
            )
        };
        if needs_repaint {
            render_strip_now(hwnd, strip_x, strip_y, strip_w, strip_h);
        }
        // Tooltip-delay timer: on every close-X hover transition,
        // (1) hide any currently-showing tooltip so it doesn't linger
        //     on the wrong X, (2) kill any pending one-shot, (3) start
        //     a fresh one-shot if we're now over a close-X.
        if close_hover_changed {
            let _ = KillTimer(Some(hwnd), TOOLTIP_DELAY_TIMER_ID);
            hide_tooltip_popup(hwnd);
            if now_over_close {
                let _ = SetTimer(
                    Some(hwnd),
                    TOOLTIP_DELAY_TIMER_ID,
                    TOOLTIP_DELAY_MS,
                    None,
                );
            }
        }
        return LRESULT(0);
    }
    if msg == WM_MOUSELEAVE {
        let (strip_x, strip_y, strip_w, strip_h, had_hover) = {
            let mut reg = registry();
            let Some(state) = reg.states.get_mut(&(hwnd.0 as isize)) else {
                return LRESULT(0);
            };
            let had_hover = state.hovered_tab_idx.is_some()
                || state.hovered_close_idx.is_some();
            state.hovered_tab_idx = None;
            state.hovered_close_idx = None;
            state.tooltip_pending_tab = None;
            state.mouse_tracking_armed = false;
            (
                state.strip_screen_x,
                state.strip_screen_y,
                state.strip_w,
                state.strip_h,
                had_hover,
            )
        };
        let _ = KillTimer(Some(hwnd), TOOLTIP_DELAY_TIMER_ID);
        hide_tooltip_popup(hwnd);
        if had_hover {
            render_strip_now(hwnd, strip_x, strip_y, strip_w, strip_h);
        }
        return LRESULT(0);
    }
    // Tooltip-delay one-shot. Fires `TOOLTIP_DELAY_MS` after the user
    // first enters a close-X; shows the popup if they're still parked
    // on the same X.
    if msg == WM_TIMER && wparam.0 == TOOLTIP_DELAY_TIMER_ID {
        let _ = KillTimer(Some(hwnd), TOOLTIP_DELAY_TIMER_ID);
        // Snapshot what we need to position the popup, then drop the
        // lock before SetWindowPos calls.
        let snapshot = {
            let reg = registry();
            let Some(state) = reg.states.get(&(hwnd.0 as isize)) else {
                return LRESULT(0);
            };
            // Verify the user is still over the same close-X we armed
            // the timer for. If pending was cleared or hover diverged,
            // bail without showing.
            let idx = match state.tooltip_pending_tab {
                Some(i) if state.hovered_close_idx == Some(i) => i,
                _ => return LRESULT(0),
            };
            let rect = match state.close_hit_rects.get(idx) {
                Some(r) => *r,
                None => return LRESULT(0),
            };
            let screen_rect = RECT {
                left: state.strip_screen_x + rect.left,
                top: state.strip_screen_y + rect.top,
                right: state.strip_screen_x + rect.right,
                bottom: state.strip_screen_y + rect.bottom,
            };
            (tooltip_text_for(state.close_action).to_string(), screen_rect)
        };
        let (text, screen_rect) = snapshot;
        show_tooltip_popup(hwnd, &text, screen_rect);
        return LRESULT(0);
    }
    if msg == WM_LBUTTONDOWN {
        // LPARAM low word = x, high word = y, both window-relative.
        let raw = lparam.0 as u32;
        let click_x = (raw & 0xFFFF) as i16 as i32;
        let click_y = ((raw >> 16) & 0xFFFF) as i16 as i32;
        // Decide what to dispatch under the lock, then drop it before
        // calling helpers that re-acquire the registry.
        enum LClick {
            Close { ev: TabActionEvent },
            Activate { ev: TabActionEvent },
        }
        let decision: Option<LClick> = {
            let reg = registry();
            let Some(state) = reg.states.get(&(hwnd.0 as isize)) else {
                return LRESULT(0);
            };
            let mut hit: Option<LClick> = None;
            for (idx, rect) in state.close_hit_rects.iter().enumerate() {
                if click_x >= rect.left
                    && click_x < rect.right
                    && click_y >= rect.top
                    && click_y < rect.bottom
                {
                    let action = match state.close_action {
                        TabCloseAction::CloseWindow => TabAction::Close,
                        TabCloseAction::Untab => TabAction::Untab,
                    };
                    hit = Some(LClick::Close {
                        ev: TabActionEvent {
                            monitor: state.target_monitor,
                            workspace_idx: state.target_workspace_idx,
                            column_idx: state.target_column_idx,
                            tab_idx: idx,
                            action,
                        },
                    });
                    break;
                }
            }
            if hit.is_none() {
                for (idx, rect) in state.hit_rects.iter().enumerate() {
                    if click_x >= rect.left
                        && click_x < rect.right
                        && click_y >= rect.top
                        && click_y < rect.bottom
                    {
                        hit = Some(LClick::Activate {
                            ev: TabActionEvent {
                                monitor: state.target_monitor,
                                workspace_idx: state.target_workspace_idx,
                                column_idx: state.target_column_idx,
                                tab_idx: idx,
                                action: TabAction::Activate,
                            },
                        });
                        break;
                    }
                }
            }
            // Send via channel while we still hold a borrow of state.tx.
            if let Some(ref h) = hit {
                let ev = match h {
                    LClick::Close { ev, .. } => *ev,
                    LClick::Activate { ev } => *ev,
                };
                if let Some(tx) = &state.action_tx {
                    let _ = tx.send(ev);
                }
            }
            hit
        };
        if matches!(decision, Some(LClick::Close { .. })) {
            let _ = KillTimer(Some(hwnd), TOOLTIP_DELAY_TIMER_ID);
            hide_tooltip_popup(hwnd);
        }
        return LRESULT(0);
    }
    if msg == WM_LBUTTONDBLCLK {
        // Double-click on a tab title → rename. Ignore double-clicks on
        // the close-X (single-click already closes/untabs there); ignore
        // double-clicks outside any tab hit rect.
        let raw = lparam.0 as u32;
        let click_x = (raw & 0xFFFF) as i16 as i32;
        let click_y = ((raw >> 16) & 0xFFFF) as i16 as i32;
        let reg = registry();
        if let Some(state) = reg.states.get(&(hwnd.0 as isize)) {
            // Skip if the click landed in any close-X hit rect.
            for rect in state.close_hit_rects.iter() {
                if click_x >= rect.left
                    && click_x < rect.right
                    && click_y >= rect.top
                    && click_y < rect.bottom
                {
                    return LRESULT(0);
                }
            }
            for (idx, rect) in state.hit_rects.iter().enumerate() {
                if click_x >= rect.left
                    && click_x < rect.right
                    && click_y >= rect.top
                    && click_y < rect.bottom
                {
                    if let Some(tx) = &state.action_tx {
                        let _ = tx.send(TabActionEvent {
                            monitor: state.target_monitor,
                            workspace_idx: state.target_workspace_idx,
                            column_idx: state.target_column_idx,
                            tab_idx: idx,
                            action: TabAction::Rename,
                        });
                    }
                    return LRESULT(0);
                }
            }
        }
        return LRESULT(0);
    }
    if msg == WM_MBUTTONUP {
        let raw = lparam.0 as u32;
        let click_x = (raw & 0xFFFF) as i16 as i32;
        let click_y = ((raw >> 16) & 0xFFFF) as i16 as i32;
        let reg = registry();
        if let Some(state) = reg.states.get(&(hwnd.0 as isize)) {
            for (idx, rect) in state.hit_rects.iter().enumerate() {
                if click_x >= rect.left
                    && click_x < rect.right
                    && click_y >= rect.top
                    && click_y < rect.bottom
                {
                    if let Some(tx) = &state.action_tx {
                        let action = match state.close_action {
                            TabCloseAction::CloseWindow => TabAction::Close,
                            TabCloseAction::Untab => TabAction::Untab,
                        };
                        let _ = tx.send(TabActionEvent {
                            monitor: state.target_monitor,
                            workspace_idx: state.target_workspace_idx,
                            column_idx: state.target_column_idx,
                            tab_idx: idx,
                            action,
                        });
                    }
                    return LRESULT(0);
                }
            }
        }
        return LRESULT(0);
    }
    if msg == WM_RBUTTONUP {
        let raw = lparam.0 as u32;
        let click_x = (raw & 0xFFFF) as i16 as i32;
        let click_y = ((raw >> 16) & 0xFFFF) as i16 as i32;

        // Resolve the target tab index + identity under one lock, drop
        // before TrackPopupMenu (which runs a nested modal loop and may
        // re-enter the WndProc for WM_TIMER).
        let (target_idx, monitor, ws_idx, col_idx) = {
            let reg = registry();
            let Some(state) = reg.states.get(&(hwnd.0 as isize)) else {
                return LRESULT(0);
            };
            let idx = state.hit_rects.iter().enumerate().find_map(|(i, r)| {
                if click_x >= r.left
                    && click_x < r.right
                    && click_y >= r.top
                    && click_y < r.bottom
                {
                    Some(i)
                } else {
                    None
                }
            });
            (
                idx,
                state.target_monitor,
                state.target_workspace_idx,
                state.target_column_idx,
            )
        };
        let Some(idx) = target_idx else {
            return LRESULT(0);
        };

        // Plain `AppendMenuW` so Win11's modern Fluent compositor takes
        // over (rounded corners, themed chrome, hover animations). Icon
        // gutter gets filled below via `MIIM_BITMAP` with properly
        // rendered Segoe Fluent Icons glyphs (greyscale AA → PARGB).
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return LRESULT(0),
        };
        let close_text: Vec<u16> = "Close window\0".encode_utf16().collect();
        let untab_text: Vec<u16> = "Untab this window\0".encode_utf16().collect();
        let rename_text: Vec<u16> = "Rename tab…\0".encode_utf16().collect();
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_CLOSE,
            windows::core::PCWSTR(close_text.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_UNTAB,
            windows::core::PCWSTR(untab_text.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_SEPARATOR,
            0,
            windows::core::PCWSTR(std::ptr::null()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_RENAME,
            windows::core::PCWSTR(rename_text.as_ptr()),
        );

        // Attach PARGB icon bitmaps via `MIIM_BITMAP`. Glyph codepoints
        // verified from `microsoft/terminal` (Tab.cpp::_CreateContextMenu):
        //   E711 Cancel        → Close (Terminal: closeSymbol)
        //   E8A7 OpenInNewWin  → Untab (Terminal: moveTabToNewWindowTabSymbol)
        //   E8AC Rename        → Rename (Terminal: renameTabSymbol)
        let glyph_color = if crate::is_high_contrast_enabled() {
            sys_color_bgr(windows::Win32::Graphics::Gdi::COLOR_MENUTEXT)
        } else if is_dark_mode() {
            0xE6E6E6
        } else {
            0x1C1C1C
        };
        let icon_close = create_menu_glyph_bitmap(0xE711, glyph_color);
        let icon_untab = create_menu_glyph_bitmap(0xE8A7, glyph_color);
        let icon_rename = create_menu_glyph_bitmap(0xE8AC, glyph_color);
        for (id, hbmp) in [
            (MENU_ID_CLOSE, icon_close),
            (MENU_ID_UNTAB, icon_untab),
            (MENU_ID_RENAME, icon_rename),
        ] {
            if hbmp.is_invalid() {
                continue;
            }
            let mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_BITMAP,
                hbmpItem: hbmp,
                ..Default::default()
            };
            let _ = SetMenuItemInfoW(menu, id as u32, false, &mii);
        }

        let mut screen_pt = POINT {
            x: click_x,
            y: click_y,
        };
        let _ = ClientToScreen(hwnd, &mut screen_pt);

        // MSDN-documented workaround: without SetForegroundWindow the
        // first click outside the menu may not dismiss it.
        let _ = SetForegroundWindow(hwnd);

        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            screen_pt.x,
            screen_pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        // Bitmaps must outlive `TrackPopupMenu`; safe to delete now.
        for hbmp in [icon_close, icon_untab, icon_rename] {
            if !hbmp.is_invalid() {
                let _ = DeleteObject(hbmp.into());
            }
        }

        let action = match cmd.0 as usize {
            MENU_ID_CLOSE => Some(TabAction::Close),
            MENU_ID_UNTAB => Some(TabAction::Untab),
            MENU_ID_RENAME => Some(TabAction::Rename),
            _ => None,
        };

        if let Some(action) = action {
            let reg = registry();
            if let Some(state) = reg.states.get(&(hwnd.0 as isize)) {
                if let Some(tx) = &state.action_tx {
                    let _ = tx.send(TabActionEvent {
                        monitor,
                        workspace_idx: ws_idx,
                        column_idx: col_idx,
                        tab_idx: idx,
                        action,
                    });
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
