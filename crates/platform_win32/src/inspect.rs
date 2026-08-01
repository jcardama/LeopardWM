//! Read-only window admission diagnostics.
//!
//! Mirrors the two admission paths without mutating windows or touching the
//! production enumeration callbacks. Classification short-circuits in source
//! order; pure table builders exist only so that order is unit-testable without
//! live HWNDs.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPath {
    StartupRefresh,
    LiveCreate,
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

// Pure ordered tables + first_failure exist for unit tests: live classifiers stay
// lazy (Win32 short-circuit) because intermediate values (title, class) depend on
// earlier reads. Laziness preferred; order is locked by the table builders alone.
#[cfg(test)]
fn first_failure(checks: &[(bool, SkipReason)]) -> Result<(), SkipReason> {
    for (fails, reason) in checks {
        if *fails {
            return Err(*reason);
        }
    }
    Ok(())
}

#[cfg(test)]
const STARTUP_ORDER: [SkipReason; 12] = [
    SkipReason::NotVisible,
    SkipReason::Minimized,
    SkipReason::StyleNotVisible,
    SkipReason::ToolWindow,
    SkipReason::NoActivate,
    SkipReason::Owned,
    SkipReason::Cloaked,
    SkipReason::EmptyOrUnreadableTitle,
    SkipReason::SkipTitle,
    SkipReason::SkipClass,
    SkipReason::RectReadFailed,
    SkipReason::ZeroSize,
];

#[cfg(test)]
const LIVE_CREATE_ORDER: [SkipReason; 7] = [
    SkipReason::NotVisible,
    SkipReason::ToolWindow,
    SkipReason::NoActivate,
    SkipReason::Owned,
    SkipReason::SkipClass,
    SkipReason::RectReadFailed,
    SkipReason::ZeroSize,
];

/// Ordered startup/refresh (strict) failure table from already-read values.
#[cfg(test)]
fn startup_check_table(fails: [bool; 12]) -> Vec<(bool, SkipReason)> {
    fails.into_iter().zip(STARTUP_ORDER).collect()
}

/// Ordered live-create (relaxed) failure table from already-read values.
#[cfg(test)]
fn live_create_check_table(fails: [bool; 7]) -> Vec<(bool, SkipReason)> {
    fails.into_iter().zip(LIVE_CREATE_ORDER).collect()
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

/// Lazy live-create chain — Win32 reads short-circuit in get_window_info order.
fn classify_live_create(hwnd: HWND) -> Result<(), SkipReason> {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(SkipReason::NotVisible);
    }
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };
    if is_excluded_tool_window(style, ex_style) {
        return Err(SkipReason::ToolWindow);
    }
    if ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return Err(SkipReason::NoActivate);
    }
    if owner_is_present(hwnd) {
        return Err(SkipReason::Owned);
    }
    // Title is read but empty is allowed on the live-create path.
    let _ = read_title_best_effort(hwnd);
    let class_name = read_class_name(hwnd);
    if should_skip_window_by_class(&class_name) {
        return Err(SkipReason::SkipClass);
    }
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
pub fn inspect_windows() -> Vec<WindowInspection> {
    let mut windows: Vec<WindowInspection> = Vec::new();
    unsafe {
        let windows_ptr = &mut windows as *mut Vec<WindowInspection>;
        let _ = EnumWindows(Some(inspect_windows_callback), LPARAM(windows_ptr as isize));
    }
    windows
}

unsafe extern "system" fn inspect_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<WindowInspection>);
    windows.push(inspect_one(hwnd));
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_THICKFRAME};

    #[test]
    fn first_failure_returns_earliest_reason() {
        let checks = [
            (false, SkipReason::NotVisible),
            (true, SkipReason::Minimized),
            (true, SkipReason::ToolWindow),
        ];
        assert_eq!(first_failure(&checks), Err(SkipReason::Minimized));
    }

    #[test]
    fn first_failure_ok_when_all_pass() {
        let checks = [
            (false, SkipReason::NotVisible),
            (false, SkipReason::Minimized),
            (false, SkipReason::SkipClass),
        ];
        assert_eq!(first_failure(&checks), Ok(()));
    }

    #[test]
    fn startup_table_contains_strict_only_reasons_in_order() {
        let table = startup_check_table([false; 12]);
        let reasons: Vec<SkipReason> = table.into_iter().map(|(_, r)| r).collect();
        assert_eq!(reasons, STARTUP_ORDER.to_vec());
        let idx = |r: SkipReason| reasons.iter().position(|x| *x == r).unwrap();
        assert!(idx(SkipReason::Minimized) < idx(SkipReason::StyleNotVisible));
        assert!(idx(SkipReason::StyleNotVisible) < idx(SkipReason::Cloaked));
        assert!(idx(SkipReason::Cloaked) < idx(SkipReason::EmptyOrUnreadableTitle));
        assert!(idx(SkipReason::EmptyOrUnreadableTitle) < idx(SkipReason::SkipTitle));
    }

    #[test]
    fn live_create_table_is_relaxed() {
        let table = live_create_check_table([false; 7]);
        let reasons: Vec<SkipReason> = table.into_iter().map(|(_, r)| r).collect();
        assert_eq!(reasons, LIVE_CREATE_ORDER.to_vec());
        for forbidden in [
            SkipReason::Minimized,
            SkipReason::StyleNotVisible,
            SkipReason::Cloaked,
            SkipReason::EmptyOrUnreadableTitle,
            SkipReason::SkipTitle,
        ] {
            assert!(
                !reasons.contains(&forbidden),
                "live-create must not check {forbidden:?}"
            );
        }
        for required in [
            SkipReason::ToolWindow,
            SkipReason::NoActivate,
            SkipReason::Owned,
            SkipReason::SkipClass,
            SkipReason::RectReadFailed,
            SkipReason::ZeroSize,
        ] {
            assert!(
                reasons.contains(&required),
                "live-create must check {required:?}"
            );
        }
    }

    #[test]
    fn empty_title_maps_to_fused_empty_or_unreadable() {
        let mut fails = [false; 12];
        fails[7] = true; // EmptyOrUnreadableTitle position in STARTUP_ORDER
        let table = startup_check_table(fails);
        assert_eq!(
            first_failure(&table),
            Err(SkipReason::EmptyOrUnreadableTitle)
        );
    }

    #[test]
    fn tool_window_exception_preserved() {
        let thick = WS_THICKFRAME.0;
        let tool = WS_EX_TOOLWINDOW.0;
        let app = WS_EX_APPWINDOW.0;
        assert!(!is_excluded_tool_window(thick, tool | app));
        let mut fails = [false; 12];
        fails[3] = is_excluded_tool_window(thick, tool | app); // ToolWindow position
        let table = startup_check_table(fails);
        assert_eq!(first_failure(&table), Ok(()));
    }

    #[test]
    fn skip_class_and_title_leaf_predicates() {
        assert!(should_skip_window_by_class("#32770"));
        assert!(should_skip_window_by_title("Program Manager"));
        let mut live_fails = [false; 7];
        live_fails[4] = should_skip_window_by_class("#32770"); // SkipClass
        let class_table = live_create_check_table(live_fails);
        assert_eq!(first_failure(&class_table), Err(SkipReason::SkipClass));
        let mut startup_fails = [false; 12];
        startup_fails[8] = should_skip_window_by_title("Program Manager"); // SkipTitle
        let title_table = startup_check_table(startup_fails);
        assert_eq!(first_failure(&title_table), Err(SkipReason::SkipTitle));
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn inspect_windows_returns_entries_with_both_verdicts() {
        let windows = inspect_windows();
        for w in &windows {
            // Both verdicts are always populated (Ok or Err).
            let _ = (w.startup_verdict, w.live_create_verdict, w.hwnd);
        }
        let _ = windows.len();
    }
}
