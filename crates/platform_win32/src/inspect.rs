//! Read-only window admission diagnostics.
//!
//! Mirrors the two admission paths without mutating windows or touching the
//! production enumeration callbacks. Classification short-circuits in source
//! order. Live-create first mirrors the create/show WinEvent pre-gate
//! (`should_emit_window_event_with_policy` with require_visible=true,
//! require_title=false, filter_cloaked=false), then the residual
//! `get_window_info` checks. Relative to startup/refresh it still omits cloak,
//! empty/unreadable title, and skip-title — that divergence is the contract
//! that admits transient popups, but it is not enforced by unit tests here
//! because the classifiers require a live HWND.

use crate::enumeration::{
    is_excluded_tool_window, is_window_cloaked, should_skip_window_by_class,
    should_skip_window_by_title,
};
use leopardwm_core_layout::{Rect, WindowId};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE,
    GW_OWNER, WS_EX_NOACTIVATE, WS_VISIBLE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    NotVisible,
    Minimized,
    StyleNotVisible,
    ToolWindow,
    NoActivate,
    Owned,
    Cloaked,
    EmptyOrUnreadableTitle,
    SkipTitle,
    SkipClass,
    RectReadFailed,
    ZeroSize,
}

pub struct WindowInspection {
    pub hwnd: WindowId,
    pub style: u32,
    pub ex_style: u32,
    pub owner_hwnd: Option<WindowId>,
    pub title: String,
    pub class_name: String,
    pub process_id: u32,
    pub rect: Option<Rect>,
    pub startup_verdict: Result<(), SkipReason>,
    pub live_create_verdict: Result<(), SkipReason>,
}

fn owner_is_present(hwnd: HWND) -> bool {
    unsafe {
        match GetWindow(hwnd, GW_OWNER) {
            Ok(owner) => !owner.is_invalid(),
            Err(_) => false,
        }
    }
}

fn read_class_name(hwnd: HWND) -> String {
    let mut class_buf: Vec<u16> = vec![0; 256];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize])
    } else {
        String::new()
    }
}

fn read_title_strict(hwnd: HWND) -> Result<String, SkipReason> {
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len == 0 {
        return Err(SkipReason::EmptyOrUnreadableTitle);
    }
    let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
    let actual_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
    if actual_len == 0 {
        return Err(SkipReason::EmptyOrUnreadableTitle);
    }
    Ok(String::from_utf16_lossy(&title_buf[..actual_len as usize]))
}

fn read_rect(hwnd: HWND) -> Result<Rect, SkipReason> {
    let mut win_rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut win_rect) }.is_err() {
        return Err(SkipReason::RectReadFailed);
    }
    let rect = Rect::new(
        win_rect.left,
        win_rect.top,
        win_rect.right - win_rect.left,
        win_rect.bottom - win_rect.top,
    );
    if rect.width == 0 || rect.height == 0 {
        return Err(SkipReason::ZeroSize);
    }
    Ok(rect)
}

/// Lazy startup/refresh chain — Win32 reads short-circuit in callback order.
fn classify_startup(hwnd: HWND) -> Result<(), SkipReason> {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(SkipReason::NotVisible);
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(SkipReason::Minimized);
    }
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };
    if style & WS_VISIBLE.0 == 0 {
        return Err(SkipReason::StyleNotVisible);
    }
    if is_excluded_tool_window(style, ex_style) {
        return Err(SkipReason::ToolWindow);
    }
    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return Err(SkipReason::NoActivate);
    }
    if owner_is_present(hwnd) {
        return Err(SkipReason::Owned);
    }
    if is_window_cloaked(hwnd) {
        return Err(SkipReason::Cloaked);
    }
    let title = read_title_strict(hwnd)?;
    if should_skip_window_by_title(&title) {
        return Err(SkipReason::SkipTitle);
    }
    let class_name = read_class_name(hwnd);
    if should_skip_window_by_class(&class_name) {
        return Err(SkipReason::SkipClass);
    }
    let _ = read_rect(hwnd)?;
    Ok(())
}

/// Lazy live-create chain.
///
/// Stage 1 mirrors `should_emit_window_event_with_policy(hwnd, true, false, false)`
/// — the create/show WinEvent pre-gate that runs before `get_window_info`.
/// Stage 2 mirrors the residual `get_window_info` checks (title allowed empty;
/// rect required).
fn classify_live_create(hwnd: HWND) -> Result<(), SkipReason> {
    // --- Stage 1: create/show event pre-gate ---
    // Mirrors should_emit_window_event_with_policy (require_visible=true,
    // require_title=false, filter_cloaked=false).
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };

    // require_visible: IsWindowVisible
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(SkipReason::NotVisible);
    }
    // require_visible: WS_VISIBLE style bit
    if style & WS_VISIBLE.0 == 0 {
        return Err(SkipReason::StyleNotVisible);
    }
    // require_visible: IsIconic
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(SkipReason::Minimized);
    }
    if is_excluded_tool_window(style, ex_style) {
        return Err(SkipReason::ToolWindow);
    }
    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return Err(SkipReason::NoActivate);
    }
    if owner_is_present(hwnd) {
        return Err(SkipReason::Owned);
    }
    // filter_cloaked=false: cloak not checked.
    // require_title=false: empty/skip title allowed.

    let class_name = read_class_name(hwnd);
    if should_skip_window_by_class(&class_name) {
        return Err(SkipReason::SkipClass);
    }

    // --- Stage 2: residual get_window_info checks ---
    // Title is read but empty is allowed on the live-create path.
    let _ = read_title_best_effort(hwnd);
    let _ = read_rect(hwnd)?;
    Ok(())
}

fn read_title_best_effort(hwnd: HWND) -> String {
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len <= 0 {
        return String::new();
    }
    let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
    let actual_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
    if actual_len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&title_buf[..actual_len as usize])
}

fn inspect_one(hwnd: HWND) -> WindowInspection {
    let hwnd_id = hwnd.0 as WindowId;
    // Live-create first: it is the path that admits transient popups.
    let live_create_verdict = classify_live_create(hwnd);
    let startup_verdict = classify_startup(hwnd);

    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };
    let owner_hwnd = unsafe {
        match GetWindow(hwnd, GW_OWNER) {
            Ok(owner) if !owner.is_invalid() => Some(owner.0 as WindowId),
            _ => None,
        }
    };
    let title = read_title_best_effort(hwnd);
    let class_name = read_class_name(hwnd);
    let mut process_id: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    let rect = {
        let mut win_rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut win_rect) }.is_ok() {
            Some(Rect::new(
                win_rect.left,
                win_rect.top,
                win_rect.right - win_rect.left,
                win_rect.bottom - win_rect.top,
            ))
        } else {
            None
        }
    };

    WindowInspection {
        hwnd: hwnd_id,
        style,
        ex_style,
        owner_hwnd,
        title,
        class_name,
        process_id,
        rect,
        startup_verdict,
        live_create_verdict,
    }
}

/// Enumerate top-level windows and classify each under both admission paths.
///
/// Own EnumWindows callback — does not reuse or modify `enum_windows_callback`.
/// Strictly read-only: no SetWindowPos/ShowWindow/SetForegroundWindow.
pub fn inspect_windows() -> Result<Vec<WindowInspection>, String> {
    let mut windows: Vec<WindowInspection> = Vec::new();
    unsafe {
        let windows_ptr = &mut windows as *mut Vec<WindowInspection>;
        EnumWindows(Some(inspect_windows_callback), LPARAM(windows_ptr as isize))
            .map_err(|e| format!("EnumWindows failed: {e}"))?;
    }
    Ok(windows)
}

unsafe extern "system" fn inspect_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<WindowInspection>);
    windows.push(inspect_one(hwnd));
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_THICKFRAME,
    };

    #[test]
    fn tool_window_exception_preserved() {
        let thick = WS_THICKFRAME.0;
        let tool = WS_EX_TOOLWINDOW.0;
        let app = WS_EX_APPWINDOW.0;
        assert!(!is_excluded_tool_window(thick, tool | app));
        assert!(is_excluded_tool_window(0, tool));
    }

    #[test]
    fn skip_class_and_title_leaf_predicates() {
        assert!(should_skip_window_by_class("#32770"));
        assert!(should_skip_window_by_title("Program Manager"));
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn inspect_windows_returns_entries_with_both_verdicts() {
        let windows = inspect_windows().expect("EnumWindows should succeed");
        for w in &windows {
            // Both verdicts are always populated (Ok or Err).
            let _ = (w.startup_verdict, w.live_create_verdict, w.hwnd);
        }
        let _ = windows.len();
    }
}
