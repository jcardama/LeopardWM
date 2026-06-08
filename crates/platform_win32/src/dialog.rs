//! Inline rename popup for tab strip.
//!
//! When the user invokes "Rename tab…" the daemon spawns this popup on
//! a dedicated thread. The popup is a borderless top-level window that
//! sits pixel-aligned over the tab cell — so the user perceives the tab
//! itself as becoming editable rather than a separate dialog opening.
//!
//! Visual contract:
//! - Borderless WS_POPUP sized to the tab rect, painted to match the
//!   tab's bg color so it blends with the strip.
//! - EDIT control fills the left/middle, pre-populated with the current
//!   title and pre-selected so typing replaces it.
//! - The right edge renders a checkmark glyph (where the close-X used
//!   to be), clickable to commit.
//!
//! Lifecycle:
//! - Enter or check-button click → commit, returns `Some(text)`.
//! - Esc, click-away (WM_ACTIVATE WA_INACTIVE), or window close →
//!   cancel, returns `None`.
//! - Over-length input (> `TAB_TITLE_MAX_BYTES`) is rejected at commit;
//!   the popup re-prompts with the text re-selected.
//!
//! Threading: the daemon spawns a dedicated short-lived thread per
//! popup, not the strip thread (which would freeze hover/animation)
//! and not a tokio task (no message pump). The popup runs its own
//! GetMessage loop and returns when WM_DESTROY fires.

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    EndPaint, FillRect, InvalidateRect, SelectObject, SetBkColor, SetTextColor, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP,
    HBRUSH, PAINTSTRUCT,
};
use windows::Win32::Graphics::Gdi::AlphaBlend;
use windows::Win32::UI::Controls::{EM_SETSEL, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, VIRTUAL_KEY, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassW, SendMessageW, SetCursor, SetLayeredWindowAttributes, SetTimer,
    SetWindowLongPtrW, SetWindowTextW, TranslateMessage, ES_AUTOHSCROLL,
    GWLP_USERDATA, GWLP_WNDPROC, HMENU, IDC_ARROW, IDC_IBEAM, LWA_ALPHA, MSG,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CHAR, WM_CLOSE, WM_CREATE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_TIMER,
    WM_USER, WNDCLASSW, WNDPROC, WS_CHILD, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE,
};

use crate::Win32Error;

/// Maximum length (in UTF-8 bytes) for a tab title override. Over-length
/// input is rejected at commit; the popup keeps the text and re-selects
/// it so the user can edit down.
pub const TAB_TITLE_MAX_BYTES: usize = 128;

const ID_EDIT: usize = 1001;

/// Custom messages the subclassed EDIT control posts back to the popup
/// to request commit/cancel. WM_USER + small offsets so they don't
/// collide with any control notifications.
const WM_INLINE_RENAME_COMMIT: u32 = WM_USER + 20;
const WM_INLINE_RENAME_CANCEL: u32 = WM_USER + 21;

/// `WM_TIMER` id used to drive the popup's fade-in animation. Tick
/// interval and duration match the tooltip's so the two reveals feel
/// consistent.
const FADE_TIMER_ID: usize = 0xB1;
const FADE_DURATION_MS: u64 = 80;
const FADE_INTERVAL_MS: u32 = 16;

/// Helpers for converting BGR u32 colors into a `COLORREF`.
fn bgr_to_colorref(bgr: u32) -> COLORREF {
    COLORREF(bgr)
}

/// Lift a 0xBBGGRR color toward white by `delta` per channel, clamping
/// at 0xFF. Used to derive the check-button hover-pill color from the
/// popup's tab bg — matches the recipe the strip uses for its close-X.
fn lighten_bgr(bgr: u32, delta: u32) -> u32 {
    let b = ((bgr & 0xFF) + delta).min(0xFF);
    let g = (((bgr >> 8) & 0xFF) + delta).min(0xFF);
    let r = (((bgr >> 16) & 0xFF) + delta).min(0xFF);
    (r << 16) | (g << 8) | b
}

/// Context stored via `GWLP_USERDATA` so the WndProc can write the
/// final result + reach the subclassed EDIT control.
struct PopupContext {
    edit_hwnd: HWND,
    bg_brush: HBRUSH,
    bg_color: u32,
    text_color: u32,
    /// Brush used to paint the check-button hover pill — lighter than
    /// the bg by a small step so the pill reads as an interactive
    /// surface without being garish.
    hover_brush: HBRUSH,
    hover_color: u32,
    check_rect: RECT,
    initial: Vec<u16>,
    result: Option<String>,
    /// Original EDIT proc, saved during subclassing so the proc can
    /// forward unhandled messages back to the default handler.
    original_edit_proc: WNDPROC,
    /// Whether the popup has received its first WM_ACTIVATE with
    /// WA_ACTIVE. Guards against false-cancel on the initial creation
    /// WM_ACTIVATE inactive that some Win32 popups receive before
    /// taking foreground.
    activated: bool,
    /// When true, the popup has already started its DestroyWindow path
    /// (commit or cancel). Suppresses WM_ACTIVATE-driven cancel so the
    /// destroy isn't reentered.
    finishing: bool,
    /// Mouse is currently over the check button. Drives the hover-pill
    /// background so the check reads as a clickable button. Same idea
    /// as the strip's close-X hover state.
    check_hovered: bool,
    /// Whether `TrackMouseEvent` is armed (so we'll receive
    /// `WM_MOUSELEAVE`). Re-armed on every entry since Windows clears
    /// the subscription on leave.
    mouse_tracking_armed: bool,
    /// Wall-clock instant the fade-in animation started.
    fade_start: std::time::Instant,
    /// Pre-rendered Segoe Fluent Icons "Accept" (E73E) glyph bitmap.
    /// Same supersample + box-filter pipeline as the menu icons so the
    /// check matches their visual style exactly. Generated once at
    /// WM_CREATE, freed in WM_DESTROY.
    check_bitmap: HBITMAP,
    /// HFONT applied to the EDIT control — Segoe UI sized to the
    /// strip's tab-title font so the rename text matches the rest of
    /// the strip rather than the system default.
    edit_font: windows::Win32::Graphics::Gdi::HFONT,
    /// Original strip height (before active-tab inset). Drives the EDIT
    /// font sizing so the rename text matches the strip's tab title
    /// pixel-for-pixel rather than scaling down with the inset popup.
    strip_h: i32,
    /// Owned HICON copy for the tab being renamed (stored as `isize`
    /// to match the strip's `TabLabel::icon` type). Created via
    /// `CopyIcon` at popup spawn so the popup doesn't depend on the
    /// source window's icon handle staying valid. Destroyed via
    /// `DestroyIcon` when the popup tears down. `None` → text-only
    /// popup, no icon area reserved.
    icon: Option<isize>,
    /// Persistent tooltip popup HWND (raw, as `isize` for `Send` safety
    /// even though we never cross threads). Created once at WM_CREATE,
    /// re-positioned + re-rendered + ShowWindow'd on each check-button
    /// hover entry, HideWindow'd on hover exit. `0` = not created.
    check_tooltip_hwnd: isize,
    /// Tracks ShowWindow / HideWindow state so we don't redundantly
    /// re-render the tooltip bitmap on every WM_MOUSEMOVE while the
    /// cursor is already inside the check rect.
    check_tooltip_visible: bool,
}

/// Show the inline rename popup over the given screen-coord tab rect.
///
/// `bg_color` and `text_color` are 0xBBGGRR (the strip's color format)
/// so the popup paints itself to look like an active tab. The popup
/// blocks the calling thread until the user commits or cancels.
pub fn show_rename_inline_popup(
    initial: String,
    screen_rect: (i32, i32, i32, i32),
    bg_color: u32,
    text_color: u32,
    icon: Option<isize>,
) -> Result<Option<String>, Win32Error> {
    let (px_outer, py_outer, pw_outer, ph_outer) = screen_rect;
    if pw_outer <= 0 || ph_outer <= 0 {
        return Err(Win32Error::HookInstallFailed(
            "Rename popup: zero-area tab rect".into(),
        ));
    }
    unsafe {
        // Inset the popup so its outline matches the strip's active-tab
        // pill geometry — same h_inset / v_inset as `render_strip_inner`.
        // Without this the popup spans the full strip height and reads
        // as a separate rectangle rather than an in-place tab edit.
        let strip_h = ph_outer;
        let strip_h_inset = (strip_h / 6).max(4);
        let v_inset = (strip_h / 8).max(3);
        let px = px_outer + strip_h_inset;
        let py = py_outer + v_inset;
        let pw = (pw_outer - 2 * strip_h_inset).max(20);
        let ph = (ph_outer - 2 * v_inset).max(12);
        let class_name: Vec<u16> = "LeopardWMInlineRename\0".encode_utf16().collect();
        // I-beam cursor reads as "editable text" — keeps the popup
        // feeling like an inline edit rather than a button.
        let cursor = LoadCursorW(None, IDC_IBEAM).unwrap_or_default();
        let bg_brush = CreateSolidBrush(bgr_to_colorref(bg_color));
        // Hover-pill bg is the tab bg lifted ~12% toward white
        // (clamped per channel). Same recipe the close-X uses for its
        // pill background so the two read as one visual system.
        let hover_color = lighten_bgr(bg_color, 28);
        let hover_brush = CreateSolidBrush(bgr_to_colorref(hover_color));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(inline_rename_proc),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            hCursor: cursor,
            hbrBackground: bg_brush,
            ..Default::default()
        };
        // RegisterClass returns 0 if the class already exists from a
        // previous popup run; that's fine — we just re-use it. Errors
        // surface from CreateWindowExW below.
        let _ = RegisterClassW(&wc);

        // Pre-compute the check-button rect in popup-local coords —
        // identical math to the strip's close-X pill (see
        // `tab_strip::show`) so the rename check sits at the same
        // pixel position and has the same dimensions as the X button
        // it visually replaces.
        let h_inset = (ph / 6).max(4);
        let pill_size_target = (ph * 2 / 3).max(16).min(ph - 4);
        let pill_size = if (pill_size_target & 1) == (ph & 1) {
            pill_size_target
        } else {
            pill_size_target + 1
        };
        let right_breathing = ((pill_size - ((pill_size * 4 / 9).max(8))) / 2).max(h_inset);
        let pill_right = pw - right_breathing;
        let pill_left = pill_right - pill_size;
        let pill_top = (ph - pill_size) / 2;
        let pill_bottom = pill_top + pill_size;
        let check_rect = RECT {
            left: pill_left,
            top: pill_top,
            right: pill_right,
            bottom: pill_bottom,
        };

        let mut initial_utf16: Vec<u16> = initial.encode_utf16().collect();
        initial_utf16.push(0);

        // Take an owned copy of the source HICON. The handle the daemon
        // passed us is borrowed from the live window — if that window
        // changes its icon or exits while the popup is open, the
        // original handle becomes invalid. `CopyIcon` gives us our own
        // handle whose lifetime we control; we destroy it on teardown.
        let owned_icon: Option<isize> = icon.and_then(|h| {
            if h == 0 {
                return None;
            }
            let src = windows::Win32::UI::WindowsAndMessaging::HICON(h as *mut c_void);
            match windows::Win32::UI::WindowsAndMessaging::CopyIcon(src) {
                Ok(copy) if !copy.is_invalid() => Some(copy.0 as isize),
                _ => None,
            }
        });

        let ctx = Box::new(PopupContext {
            edit_hwnd: HWND(std::ptr::null_mut()),
            bg_brush,
            bg_color,
            text_color,
            hover_brush,
            hover_color,
            check_rect,
            initial: initial_utf16,
            result: None,
            original_edit_proc: None,
            activated: false,
            finishing: false,
            check_hovered: false,
            mouse_tracking_armed: false,
            fade_start: std::time::Instant::now(),
            check_bitmap: HBITMAP::default(),
            edit_font: windows::Win32::Graphics::Gdi::HFONT::default(),
            strip_h,
            icon: owned_icon,
            check_tooltip_hwnd: 0,
            check_tooltip_visible: false,
        });
        let ctx_ptr = Box::into_raw(ctx);

        let title: Vec<u16> = "\0".encode_utf16().collect();
        // `WS_EX_LAYERED` with `SetLayeredWindowAttributes` (uniform
        // alpha) is required so the child EDIT control renders — per-
        // pixel alpha via `UpdateLayeredWindow` would make the EDIT
        // invisible. DWM corner rounding (set after CreateWindowExW)
        // gives the popup AA-rounded corners that match the strip's
        // active tab.
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            WS_POPUP | WS_VISIBLE,
            px,
            py,
            pw,
            ph,
            None,
            None,
            None,
            Some(ctx_ptr as *const c_void),
        )
        .map_err(|e| {
            let _ = Box::from_raw(ctx_ptr);
            Win32Error::HookInstallFailed(format!("Rename popup create: {}", e))
        })?;

        // DWM rounds + AA-smooths the popup's outer corners.
        // `DWMWCP_ROUNDSMALL` (~4 px) lands closer to the strip's
        // `strip_corner - 2` (6 px) than the larger `DWMWCP_ROUND`
        // (~8 px) preset. Win11-only; older OSes silently no-op.
        let pref = DWMWCP_ROUNDSMALL;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const c_void,
            std::mem::size_of_val(&pref) as u32,
        );
        // Subtle DWM border. Default Win11 popup border is near-white
        // and reads too prominently against the tab pill; a mid-grey
        // (~30% lift toward white from the strip bg) gives definition
        // without standing out. COLORREF = 0x00BBGGRR.
        let border: u32 = 0x0050_5050;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as *const c_void,
            std::mem::size_of_val(&border) as u32,
        );

        // Start invisible, then ramp via WM_TIMER.
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        let _ = SetTimer(Some(hwnd), FADE_TIMER_ID, FADE_INTERVAL_MS, None);
        // Force activation so the EDIT receives keyboard input. The
        // popup thread isn't the foreground thread at spawn time, but
        // the user just clicked a menu item (or pressed Enter on a
        // right-click), so Windows is still in the interactive grace
        // period and SetForegroundWindow will succeed.
        let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Reclaim ownership; clean up the brushes we allocated up front.
        let ctx_owned: Box<PopupContext> = Box::from_raw(ctx_ptr);
        let _ = DeleteObject(ctx_owned.bg_brush.into());
        let _ = DeleteObject(ctx_owned.hover_brush.into());
        if !ctx_owned.check_bitmap.is_invalid() {
            let _ = DeleteObject(ctx_owned.check_bitmap.into());
        }
        if !ctx_owned.edit_font.is_invalid() {
            let _ = DeleteObject(ctx_owned.edit_font.into());
        }
        if ctx_owned.check_tooltip_hwnd != 0 {
            let _ = DestroyWindow(HWND(ctx_owned.check_tooltip_hwnd as *mut c_void));
        }
        // Release our owned HICON copy.
        if let Some(h) = ctx_owned.icon {
            if h != 0 {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(
                    windows::Win32::UI::WindowsAndMessaging::HICON(h as *mut c_void),
                );
            }
        }
        Ok(ctx_owned.result)
    }
}

/// Read the current EDIT text as a `String` (UTF-16 → UTF-8).
unsafe fn read_edit_text(edit: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{WM_GETTEXT, WM_GETTEXTLENGTH};
    let len = SendMessageW(edit, WM_GETTEXTLENGTH, None, None).0 as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf: Vec<u16> = vec![0; len + 1];
    let copied = SendMessageW(
        edit,
        WM_GETTEXT,
        Some(WPARAM(buf.len())),
        Some(LPARAM(buf.as_mut_ptr() as isize)),
    )
    .0 as usize;
    let slice = &buf[..copied.min(buf.len())];
    String::from_utf16_lossy(slice)
}

unsafe fn ctx_from_hwnd(hwnd: HWND) -> Option<&'static mut PopupContext> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupContext;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

#[allow(clippy::too_many_lines)] // TODO: decompose (~313 lines, grandfathered)
unsafe extern "system" fn inline_rename_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;

    if msg == WM_CREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        let ctx_ptr = (*cs).lpCreateParams as isize;
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr);

        let ctx = &mut *(ctx_ptr as *mut PopupContext);

        // Font + EDIT geometry: char height matches the strip's tab
        // title (same `0.43 * strip_h` ratio, floor 8) so the rename
        // text is pixel-for-pixel the same size as a live tab title.
        // Single-line Win32 EDIT controls do NOT auto-center text
        // vertically — they render text starting at `edit_top +
        // internal_top_pad` (the pad is ~2 px). Position the EDIT so
        // the rendered cell sits centered: cell center at client_h/2.
        // Visual left pad set to match top/bottom for balance.
        let mut client = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
        let client_h = (client.bottom - client.top).max(1);
        let font_char = ((ctx.strip_h as f32 * 0.43).round() as i32).max(8);
        let cell_h = (font_char * 14 / 10).max(font_char + 2);
        // Visible text occupies the font's ascent (≈ `font_char`) — the
        // descender area below the baseline is empty for most glyphs.
        // Center the ASCENT, not the full cell, so the visible text
        // sits at the popup's true vertical midpoint.
        let edit_internal_top_pad = 2;
        let ascent = font_char;
        let edit_top = ((client_h / 2) - (ascent / 2) - edit_internal_top_pad).max(0);
        let edit_height = (client_h - edit_top - 1).max(cell_h + 2);
        let visual_pad = ((client_h - cell_h) / 2).max(3);
        // Reserve space for the icon on the left if one was supplied —
        // same `0.57 * strip_h` recipe `render_strip_inner` uses so the
        // icon position matches the live tab pixel-for-pixel.
        let icon_size = ((ctx.strip_h as f32 * 0.57).round() as i32).max(10);
        let icon_inside_pad = (ctx.strip_h / 6).max(4);
        let edit_left = if ctx.icon.is_some() {
            visual_pad + icon_size + icon_inside_pad
        } else {
            visual_pad
        };
        let edit_right = ctx.check_rect.left - visual_pad;

        let edit_class: Vec<u16> = "EDIT\0".encode_utf16().collect();
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(edit_class.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            edit_left,
            edit_top,
            (edit_right - edit_left).max(20),
            edit_height,
            Some(hwnd),
            Some(HMENU(ID_EDIT as *mut c_void)),
            None,
            None,
        );

        if let Ok(edit) = edit {
            ctx.edit_hwnd = edit;
            // Subclass the EDIT control so we can intercept Esc/Enter
            // (which the default proc would otherwise consume as a beep
            // and as a "no-op input" respectively).
            let original = SetWindowLongPtrW(
                edit,
                GWLP_WNDPROC,
                edit_subclass_proc as *const () as isize,
            );
            if original != 0 {
                ctx.original_edit_proc = Some(std::mem::transmute::<
                    isize,
                    unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                >(original));
            }
            let _ = SetWindowTextW(edit, windows::core::PCWSTR(ctx.initial.as_ptr()));
            // Match the strip's tab title font: Segoe UI, char height
            // = `0.43 * strip_h` (≈ 12 px at default DPI) floor 8 —
            // identical to `render_strip_inner`. Pixel-for-pixel match
            // with a live tab title.
            let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
            let height = -(((ctx.strip_h as f32 * 0.43).round() as i32).max(8));
            let font = windows::Win32::Graphics::Gdi::CreateFontW(
                height,
                0,
                0,
                0,
                400, // FW_NORMAL
                0,
                0,
                0,
                windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
                windows::Win32::Graphics::Gdi::OUT_DEFAULT_PRECIS,
                windows::Win32::Graphics::Gdi::CLIP_DEFAULT_PRECIS,
                windows::Win32::Graphics::Gdi::CLEARTYPE_QUALITY,
                0,
                windows::core::PCWSTR(face.as_ptr()),
            );
            if !font.is_invalid() {
                SendMessageW(
                    edit,
                    windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
                ctx.edit_font = font;
            }
            // Zero the EDIT control's internal left/right text margins
            // (default is a few px) so the visual padding inside the
            // pill matches the geometric `edit_left` / `edit_right`
            // exactly — otherwise the text appears shifted right.
            use windows::Win32::UI::Controls::EM_SETMARGINS;
            use windows::Win32::UI::WindowsAndMessaging::{EC_LEFTMARGIN, EC_RIGHTMARGIN};
            SendMessageW(
                edit,
                EM_SETMARGINS,
                Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
                Some(LPARAM(0)),
            );
            let _ = SetFocus(Some(edit));
            // Select-all so typing replaces the seeded text immediately.
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }

        // Pre-render the check glyph using Segoe Fluent Icons "Accept"
        // (E73E) at the check rect's exact pixel size — avoids the
        // double-rescale (render-at-16-then-stretch) that produced a
        // visibly fuzzy result. Same supersample pipeline as the menu
        // icons, just sized for *this* tab's check button.
        let check_size = (ctx.check_rect.right - ctx.check_rect.left)
            .min(ctx.check_rect.bottom - ctx.check_rect.top);
        ctx.check_bitmap =
            crate::tab_strip::create_glyph_bitmap_at_size(0xE73E, ctx.text_color, check_size);

        // Pre-create the tooltip popup so hover-show can just position
        // + render + ShowWindow without paying class-register or
        // window-create cost on every hover entry.
        let tt = create_check_tooltip_popup();
        if !tt.is_invalid() {
            ctx.check_tooltip_hwnd = tt.0 as isize;
        }

        return LRESULT(0);
    }

    if msg == WM_ERASEBKGND {
        // We paint the bg in WM_PAINT to avoid a default-color flash.
        return LRESULT(1);
    }

    // Show the arrow cursor (not I-beam) when the mouse is over the
    // check button. Default IDC_IBEAM was set on the popup class for
    // the text-edit area, so we override only when over the check.
    if msg == WM_SETCURSOR {
        let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let cr = ctx.check_rect;
            if pt.x >= cr.left && pt.x < cr.right && pt.y >= cr.top && pt.y < cr.bottom {
                if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                    let _ = SetCursor(Some(arrow));
                    return LRESULT(1);
                }
            }
        }
    }

    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let mut client = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(
                hwnd, &mut client,
            );
            // Fill the whole popup with the tab bg. DWM clips the
            // visible region to a rounded shape so the corners outside
            // the rounded area are never painted to screen.
            let _ = FillRect(hdc, &client, ctx.bg_brush);
            // Tab icon — drawn at the same position as the live strip
            // so the transition tab → rename feels in-place. Geometry
            // mirrors `render_strip_inner`'s icon block.
            if let Some(icon_handle) = ctx.icon {
                if icon_handle != 0 {
                    let client_h = (client.bottom - client.top).max(1);
                    let icon_size =
                        ((ctx.strip_h as f32 * 0.57).round() as i32).max(10);
                    let cell_h_local = {
                        let fc = ((ctx.strip_h as f32 * 0.43).round() as i32).max(8);
                        (fc * 14 / 10).max(fc + 2)
                    };
                    let visual_pad_local = ((client_h - cell_h_local) / 2).max(3);
                    let icon_left = visual_pad_local;
                    let icon_top = (client_h - icon_size) / 2;
                    let hicon = windows::Win32::UI::WindowsAndMessaging::HICON(
                        icon_handle as *mut c_void,
                    );
                    let _ = windows::Win32::UI::WindowsAndMessaging::DrawIconEx(
                        hdc,
                        icon_left,
                        icon_top,
                        hicon,
                        icon_size,
                        icon_size,
                        0,
                        None,
                        windows::Win32::UI::WindowsAndMessaging::DI_NORMAL,
                    );
                }
            }
            // Hover pill + check glyph — AA-rendered into a temp DIB
            // and composited via AlphaBlend so the glyph treatment
            // matches the close-X in the strip.
            paint_check_glyph(hdc, ctx);
        }
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }

    if msg == WM_MOUSEMOVE {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            if !ctx.mouse_tracking_armed {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                ctx.mouse_tracking_armed = true;
            }
            let raw = lparam.0 as u32;
            let mx = (raw & 0xFFFF) as i16 as i32;
            let my = ((raw >> 16) & 0xFFFF) as i16 as i32;
            let cr = ctx.check_rect;
            let over = mx >= cr.left && mx < cr.right && my >= cr.top && my < cr.bottom;
            if over != ctx.check_hovered {
                ctx.check_hovered = over;
                let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
                // Show tooltip on hover-in, hide on hover-out. Same
                // visual style as the close-X tooltip (rendered via
                // the strip's shared tooltip pipeline).
                if ctx.check_tooltip_hwnd != 0 {
                    let tt = HWND(ctx.check_tooltip_hwnd as *mut c_void);
                    if over && !ctx.check_tooltip_visible {
                        show_check_tooltip(tt, hwnd, cr, CHECK_TOOLTIP_TEXT);
                        ctx.check_tooltip_visible = true;
                    } else if !over && ctx.check_tooltip_visible {
                        hide_check_tooltip(tt);
                        ctx.check_tooltip_visible = false;
                    }
                }
            }
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    if msg == WM_MOUSELEAVE {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            ctx.mouse_tracking_armed = false;
            if ctx.check_hovered {
                ctx.check_hovered = false;
                let cr = ctx.check_rect;
                let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
            }
            if ctx.check_tooltip_visible && ctx.check_tooltip_hwnd != 0 {
                hide_check_tooltip(HWND(ctx.check_tooltip_hwnd as *mut c_void));
                ctx.check_tooltip_visible = false;
            }
        }
        return LRESULT(0);
    }

    if msg == WM_TIMER && wparam.0 == FADE_TIMER_ID {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let elapsed = ctx.fade_start.elapsed().as_millis() as u64;
            let t = (elapsed.min(FADE_DURATION_MS) as f32) / FADE_DURATION_MS as f32;
            // Ease-out cubic — matches the tooltip's fade curve.
            let eased = 1.0 - (1.0 - t).powi(3);
            let alpha = (eased * 255.0).round().clamp(0.0, 255.0) as u8;
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
            if elapsed >= FADE_DURATION_MS {
                let _ = KillTimer(Some(hwnd), FADE_TIMER_ID);
            }
        } else {
            let _ = KillTimer(Some(hwnd), FADE_TIMER_ID);
        }
        return LRESULT(0);
    }

    if msg == WM_LBUTTONDOWN {
        let raw = lparam.0 as u32;
        let cx = (raw & 0xFFFF) as i16 as i32;
        let cy = ((raw >> 16) & 0xFFFF) as i16 as i32;
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let cr = ctx.check_rect;
            if cx >= cr.left && cx < cr.right && cy >= cr.top && cy < cr.bottom {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_INLINE_RENAME_COMMIT,
                    WPARAM(0),
                    LPARAM(0),
                );
                return LRESULT(0);
            }
        }
        return LRESULT(0);
    }

    if msg == WM_ACTIVATE {
        // WA_INACTIVE = 0; any non-zero state means we're activating.
        let activation = (wparam.0 & 0xFFFF) as u32;
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            if activation != 0 {
                ctx.activated = true;
                // Make sure focus is on the EDIT once we're active —
                // SetFocus during WM_CREATE doesn't always stick if
                // the popup wasn't foreground at creation time.
                if !ctx.edit_hwnd.is_invalid() {
                    let _ = SetFocus(Some(ctx.edit_hwnd));
                    SendMessageW(
                        ctx.edit_hwnd,
                        EM_SETSEL,
                        Some(WPARAM(0)),
                        Some(LPARAM(-1)),
                    );
                }
            } else if ctx.activated && !ctx.finishing {
                // Lost activation after being active → user clicked
                // elsewhere. Discard. Guard against the initial
                // creation WA_INACTIVE that fires before WA_ACTIVE on
                // some Win32 paths.
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_INLINE_RENAME_CANCEL,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    // EDIT control bg + text color routing. WM_CTLCOLOREDIT lets the
    // parent override the default white-on-system colors so the EDIT
    // blends with the tab.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLOREDIT {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, bgr_to_colorref(ctx.text_color));
            SetBkColor(hdc, bgr_to_colorref(ctx.bg_color));
            return LRESULT(ctx.bg_brush.0 as isize);
        }
    }

    if msg == WM_INLINE_RENAME_COMMIT {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            let text = read_edit_text(ctx.edit_hwnd);
            if text.len() > TAB_TITLE_MAX_BYTES {
                // Reject and re-select so the user can edit down.
                SendMessageW(
                    ctx.edit_hwnd,
                    EM_SETSEL,
                    Some(WPARAM(0)),
                    Some(LPARAM(-1)),
                );
                let _ = SetFocus(Some(ctx.edit_hwnd));
                return LRESULT(0);
            }
            ctx.result = Some(text);
            ctx.finishing = true;
        }
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }

    if msg == WM_INLINE_RENAME_CANCEL {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            ctx.finishing = true;
        }
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }

    if msg == WM_CLOSE {
        if let Some(ctx) = ctx_from_hwnd(hwnd) {
            ctx.finishing = true;
        }
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }

    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Subclassed EDIT proc. Intercepts Esc/Enter and routes them back to
/// the popup parent; forwards everything else to the default EDIT proc.
/// Without this, Esc would beep and Enter would do nothing visible
/// (default EDIT just ignores it in single-line mode).
unsafe extern "system" fn edit_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::GetParent;

    if msg == WM_KEYDOWN || msg == WM_CHAR {
        let vk = VIRTUAL_KEY(wparam.0 as u16);
        if vk == VK_ESCAPE {
            if let Ok(parent) = GetParent(hwnd) {
                let _ = PostMessageW(
                    Some(parent),
                    WM_INLINE_RENAME_CANCEL,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            return LRESULT(0);
        }
        if vk == VK_RETURN {
            if let Ok(parent) = GetParent(hwnd) {
                let _ = PostMessageW(
                    Some(parent),
                    WM_INLINE_RENAME_COMMIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            return LRESULT(0);
        }
    }

    // Fall through to the original EDIT proc. The parent stashes the
    // original handler on the popup context; we look it up via the
    // parent's GWLP_USERDATA.
    let parent = match windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
        Ok(p) => p,
        Err(_) => return DefWindowProcW(hwnd, msg, wparam, lparam),
    };
    if let Some(ctx) = ctx_from_hwnd(parent) {
        if let Some(original) = ctx.original_edit_proc {
            return CallWindowProcW(Some(original), hwnd, msg, wparam, lparam);
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Render the check button: optional rounded hover pill (AA), then the
/// pre-rendered Segoe Fluent Icons "Accept" glyph bitmap, both
/// composited via `AlphaBlend` onto the popup's screen DC. The glyph
/// bitmap is built once at WM_CREATE using the same supersample +
/// box-filter pipeline the menu icons use, so visual treatment matches.
unsafe fn paint_check_glyph(hdc: windows::Win32::Graphics::Gdi::HDC, ctx: &PopupContext) {
    let cr = ctx.check_rect;
    let w = cr.right - cr.left;
    let h = cr.bottom - cr.top;
    if w <= 0 || h <= 0 {
        return;
    }

    if ctx.check_hovered {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        if let Ok(pill_bmp) =
            CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            let pill_dc = CreateCompatibleDC(Some(hdc));
            let old_bm = SelectObject(pill_dc, pill_bmp.into());
            let pixels =
                std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            pixels.fill(0);
            let radius = (w.min(h) / 3).clamp(3, w.min(h) / 2);
            crate::tab_strip::aa_fill_rounded(
                pixels,
                w,
                h,
                0,
                0,
                w,
                h,
                radius,
                ctx.hover_color,
            );
            let mut i = 0usize;
            while i < pixels.len() {
                let a = pixels[i + 3];
                if a < 0xFF {
                    let af = a as u32;
                    pixels[i] = ((pixels[i] as u32 * af) / 255) as u8;
                    pixels[i + 1] = ((pixels[i + 1] as u32 * af) / 255) as u8;
                    pixels[i + 2] = ((pixels[i + 2] as u32 * af) / 255) as u8;
                }
                i += 4;
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = AlphaBlend(hdc, cr.left, cr.top, w, h, pill_dc, 0, 0, w, h, blend);
            SelectObject(pill_dc, old_bm);
            let _ = DeleteDC(pill_dc);
            let _ = DeleteObject(pill_bmp.into());
        }
    }

    if !ctx.check_bitmap.is_invalid() {
        use windows::Win32::Graphics::Gdi::GetObjectW;
        use windows::Win32::Graphics::Gdi::BITMAP as GdiBitmap;
        let mut bm_info = GdiBitmap::default();
        let got = GetObjectW(
            ctx.check_bitmap.into(),
            std::mem::size_of::<GdiBitmap>() as i32,
            Some(&mut bm_info as *mut _ as *mut c_void),
        );
        if got != 0 {
            let src_w = bm_info.bmWidth;
            let src_h = bm_info.bmHeight;
            let glyph_dc = CreateCompatibleDC(Some(hdc));
            let old_bm = SelectObject(glyph_dc, ctx.check_bitmap.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let dx = cr.left + (w - src_w) / 2;
            let dy = cr.top + (h - src_h) / 2;
            let _ = AlphaBlend(
                hdc, dx, dy, src_w, src_h, glyph_dc, 0, 0, src_w, src_h, blend,
            );
            SelectObject(glyph_dc, old_bm);
            let _ = DeleteDC(glyph_dc);
        }
    }
}

/// Tooltip text shown on hover of the rename popup's check button.
/// Matches the close-X tooltip's terse style.
const CHECK_TOOLTIP_TEXT: &str = "Save";

/// Create the tooltip popup window (one per rename popup, kept alive
/// for the popup's lifetime so we don't pay create/destroy cost on
/// every hover). Uses `DefWindowProcW` for its WndProc — the tooltip
/// has no behavior of its own, it's just a layered surface we draw
/// into and show/hide.
unsafe extern "system" fn check_tooltip_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn create_check_tooltip_popup() -> HWND {
    let class_name: Vec<u16> = "LeopardWMRenameTooltip\0".encode_utf16().collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(check_tooltip_proc),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_TOPMOST
            | WS_EX_TOOLWINDOW
            | windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE
            | WS_EX_LAYERED,
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
        Ok(h) => h,
        Err(_) => return HWND(std::ptr::null_mut()),
    };
    // Opt out of DWM's compositor shadow + corner rounding — the
    // bitmap bakes in its own AA shadow/corners. Same opt-outs the
    // strip's tooltip uses.
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute as DwmSet, DWMNCRP_DISABLED,
        DWMWA_NCRENDERING_POLICY as DwmNcRender,
        DWMWA_WINDOW_CORNER_PREFERENCE as DwmCornerPref, DWMWCP_DONOTROUND,
    };
    let pref = DWMWCP_DONOTROUND;
    let _ = DwmSet(
        hwnd,
        DwmCornerPref,
        &pref as *const _ as *const c_void,
        std::mem::size_of_val(&pref) as u32,
    );
    let policy = DWMNCRP_DISABLED;
    let _ = DwmSet(
        hwnd,
        DwmNcRender,
        &policy as *const _ as *const c_void,
        std::mem::size_of_val(&policy) as u32,
    );
    hwnd
}

/// Measure the tooltip text + position the tooltip popup centered
/// horizontally under the check button, then render via the strip's
/// shared tooltip pipeline and `ShowWindow` it. The render function
/// drives `UpdateLayeredWindow` with the popup's screen coords.
unsafe fn show_check_tooltip(
    tt_hwnd: HWND,
    parent_hwnd: HWND,
    check_rect: RECT,
    text: &str,
) {
    use windows::Win32::Graphics::Gdi::{DrawTextW, GetDC, ReleaseDC, DT_CALCRECT,
        DT_NOPREFIX, DT_SINGLELINE};
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};

    // Compute the check button's screen-space anchor rect — popup +
    // check_rect (local) → screen coords.
    let mut win_rect = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(parent_hwnd, &mut win_rect);
    let anchor = RECT {
        left: win_rect.left + check_rect.left,
        top: win_rect.top + check_rect.top,
        right: win_rect.left + check_rect.right,
        bottom: win_rect.top + check_rect.bottom,
    };

    // Measure text with the strip's tooltip font so popup width is exact.
    let hdc_screen = GetDC(None);
    let font = crate::tab_strip::create_tooltip_font();
    let old_font = SelectObject(hdc_screen, font.into());
    let mut text_utf16: Vec<u16> = text.encode_utf16().collect();
    if text_utf16.is_empty() {
        text_utf16.push(0);
    }
    let mut measure = RECT::default();
    let _ = DrawTextW(
        hdc_screen,
        &mut text_utf16,
        &mut measure,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc_screen, old_font);
    let _ = DeleteObject(font.into());
    ReleaseDC(None, hdc_screen);

    // Same chrome padding the strip tooltip uses — matches Win11
    // Terminal tooltip proportions.
    let pad_x = 14i32;
    let pad_y = 8i32;
    let popup_w = (measure.right - measure.left) + pad_x * 2;
    let popup_h = (measure.bottom - measure.top) + pad_y * 2;

    let anchor_w = anchor.right - anchor.left;
    let anchor_cx = anchor.left + anchor_w / 2;
    let popup_x = anchor_cx - popup_w / 2;
    let popup_y = anchor.bottom + 6;

    crate::tab_strip::render_tooltip_layered(
        tt_hwnd, popup_x, popup_y, popup_w, popup_h, text, 255,
    );
    let _ = ShowWindow(tt_hwnd, SW_SHOWNOACTIVATE);
}

unsafe fn hide_check_tooltip(tt_hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    let _ = ShowWindow(tt_hwnd, SW_HIDE);
}
