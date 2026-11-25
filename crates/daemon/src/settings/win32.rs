//! The WebView2 settings UI lives in html.rs. Rust sends scripts through
//! evaluate_script; JS sends messages through window.ipc.postMessage.

use leopardwm_ipc::log_text;
use std::sync::mpsc;
use std::sync::Mutex;

use anyhow::Result;
use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, Win32WindowHandle,
    WindowsDisplayHandle,
};
use wry::{WebContext, WebViewBuilderExtWindows as _};

use crate::config::Config;

use super::html::SETTINGS_HTML;
use super::SettingsEvent;

// DWM attribute values for Windows 11 theming.
const DWMWA_USE_IMMERSIVE_DARK_MODE_VAL: i32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE_VAL: i32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE_VAL: i32 = 38;
const DWMWCP_ROUND: u32 = 2;
const DWMSBT_MAINWINDOW: u32 = 2; // Mica

const DEFAULT_WINDOW_WIDTH: i32 = 1000;
const DEFAULT_WINDOW_HEIGHT: i32 = 720;
const MIN_WINDOW_WIDTH: i32 = 800;
const MIN_WINDOW_HEIGHT: i32 = 600;

fn scale_for_dpi(logical_pixels: i32, dpi: u32) -> i32 {
    logical_pixels * dpi as i32 / 96
}

fn clamp_to_work_area(size: (i32, i32), work_area: RECT) -> (i32, i32) {
    (
        size.0.min(work_area.right - work_area.left),
        size.1.min(work_area.bottom - work_area.top),
    )
}

fn fit_window_to_work_area(size: (i32, i32), work_area: RECT) -> (i32, i32, i32, i32) {
    let (width, height) = clamp_to_work_area(size, work_area);
    (
        work_area.left + (work_area.right - work_area.left - width) / 2,
        work_area.top + (work_area.bottom - work_area.top - height) / 2,
        width,
        height,
    )
}

unsafe fn window_size_and_work_area(
    hwnd: HWND,
    logical_size: (i32, i32),
) -> ((i32, i32), Option<RECT>) {
    let dpi = GetDpiForWindow(hwnd);
    let size = (
        scale_for_dpi(logical_size.0, dpi),
        scale_for_dpi(logical_size.1, dpi),
    );
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut monitor_info as *mut _).as_bool() {
        let work_area = monitor_info.rcWork;
        (clamp_to_work_area(size, work_area), Some(work_area))
    } else {
        (size, None)
    }
}

// Dark mode background (COLORREF = 0x00BBGGRR)
const DARK_BG: u32 = 0x00202020;

/// Custom message: ask the open settings window to refresh its rejected-hotkey
/// warning. Carries no payload; the new list is read from `PENDING_FAILED_BINDS`
/// on the window's own thread (the only thread that may touch the webview).
const WM_SETTINGS_PUSH_BINDS: u32 = WM_APP + 1;
const WM_SETTINGS_PUSH_RECORDED: u32 = WM_APP + 2;
const WM_SETTINGS_PUSH_LOCALE: u32 = WM_APP + 3;
const WM_SETTINGS_PUSH_TRAY_CONFIG: u32 = WM_APP + 4;
const WM_SETTINGS_PUSH_REPORT: u32 = WM_APP + 5;
const WM_SETTINGS_PUSH_HEALTH: u32 = WM_APP + 6;
const WM_SETTINGS_PUSH_SNAPSHOT: u32 = WM_APP + 7;
const WM_SETTINGS_PUSH_RELOAD_CONFIG: u32 = WM_APP + 9;
const WM_SETTINGS_SAVE_RESULT: u32 = WM_APP + 10;
const WM_SETTINGS_LIST_DIAGNOSTICS: u32 = WM_APP + 8;
static SETTINGS_WINDOW: Mutex<Option<isize>> = Mutex::new(None);
static PENDING_HEALTH: Mutex<Option<String>> = Mutex::new(None);
static PENDING_SNAPSHOTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static REPORT_DRAFT: Mutex<Option<super::issue_report::IssueReport>> = Mutex::new(None);
static PENDING_REPORT: Mutex<Option<String>> = Mutex::new(None);

pub fn push_report_status(
    file: Option<String>,
    error: Option<String>,
    truncated: bool,
    copied: bool,
) {
    let Ok(thread) = SETTINGS_THREAD.lock() else {
        return;
    };
    let Some(thread_id) = *thread else { return };
    if let Ok(mut pending) = PENDING_REPORT.lock() {
        *pending = Some(serde_json::json!({ "file": file, "error": error, "truncated": truncated, "copied": copied }).to_string());
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_REPORT, WPARAM(0), LPARAM(0));
        }
    }
}
static PENDING_LOCALE: Mutex<Option<String>> = Mutex::new(None);
static PENDING_TRAY_CONFIG: Mutex<Option<String>> = Mutex::new(None);
static PENDING_RELOAD_CONFIG: Mutex<Option<String>> = Mutex::new(None);

/// Thread id of the open settings window's message loop, or `None` when closed.
/// We target the thread queue (not the HWND) so a destroyed or recycled window
/// can never receive a stray push.
static SETTINGS_THREAD: Mutex<Option<u32>> = Mutex::new(None);
static PENDING_FAILED_BINDS: Mutex<Option<String>> = Mutex::new(None);
/// Recorded chords awaiting evaluation on the Settings window thread.
static PENDING_RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Push an updated rejected-hotkey list to the open settings window, if one is
/// open. Safe to call from any thread and no-ops when no window is open; the
/// `evaluate_script` itself runs on the window's own thread via its message loop.
pub fn push_failed_binds(failed_binds: &[String]) {
    let thread_id = match SETTINGS_THREAD.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    let Some(thread_id) = thread_id else { return };
    let json = serde_json::to_string(failed_binds).unwrap_or_else(|_| "[]".to_string());
    if let Ok(mut pending) = PENDING_FAILED_BINDS.lock() {
        *pending = Some(json);
    }
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_BINDS, WPARAM(0), LPARAM(0));
    }
}

pub(crate) fn push_health(health: &super::troubleshooting::Health) {
    let window = SETTINGS_WINDOW
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(hwnd) = *window else { return };
    if let (Ok(json), Ok(mut pending)) = (serde_json::to_string(health), PENDING_HEALTH.lock()) {
        *pending = Some(json);
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_SETTINGS_PUSH_HEALTH,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub(crate) fn push_snapshot_result(id: u64, result: Result<std::path::PathBuf>) {
    let window = SETTINGS_WINDOW
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(hwnd) = *window else { return };
    let result = match result {
        Ok(path) => serde_json::json!({"id": id, "ok": true, "path": path.display().to_string()}),
        Err(error) => serde_json::json!({"id": id, "ok": false, "error": format!("{error:#}")}),
    };
    if let Ok(mut pending) = PENDING_SNAPSHOTS.lock() {
        pending.push(result.to_string());
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_SETTINGS_PUSH_SNAPSHOT,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn push_locale(language: &str) {
    let settings_thread = match SETTINGS_THREAD.lock() {
        Ok(thread) => thread,
        Err(_) => return,
    };
    let Some(thread_id) = *settings_thread else {
        return;
    };
    if let Ok(mut pending) = PENDING_LOCALE.lock() {
        *pending = Some(language.to_string());
    }
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_LOCALE, WPARAM(0), LPARAM(0));
    }
}

pub fn push_tray_config(config: &Config) {
    let settings_thread = match SETTINGS_THREAD.lock() {
        Ok(thread) => thread,
        Err(_) => return,
    };
    let Some(thread_id) = *settings_thread else {
        return;
    };
    let Ok(script) = super::html::tray_config_script(config) else {
        return;
    };
    if let Ok(mut pending) = PENDING_TRAY_CONFIG.lock() {
        *pending = Some(script);
        unsafe {
            let _ = PostThreadMessageW(
                thread_id,
                WM_SETTINGS_PUSH_TRAY_CONFIG,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn push_reload_config(config: &Config) {
    let Ok(settings_thread) = SETTINGS_THREAD.lock() else {
        return;
    };
    let Some(thread_id) = *settings_thread else {
        return;
    };
    let Ok(script) = super::html::reload_config_script(config) else {
        return;
    };
    if let Ok(mut pending) = PENDING_RELOAD_CONFIG.lock() {
        *pending = Some(script);
        unsafe {
            let _ = PostThreadMessageW(
                thread_id,
                WM_SETTINGS_PUSH_RELOAD_CONFIG,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn push_recorded_chord(chord: &str) {
    let settings_thread = match SETTINGS_THREAD.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(thread_id) = *settings_thread else {
        return;
    };
    if let Ok(mut pending) = PENDING_RECORDED.lock() {
        pending.push(chord.to_string());
    }
    drop(settings_thread);
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_SETTINGS_PUSH_RECORDED, WPARAM(0), LPARAM(0));
    }
}

struct Win32Handle(isize);

impl HasWindowHandle for Win32Handle {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let mut handle =
            Win32WindowHandle::new(unsafe { std::num::NonZero::new_unchecked(self.0) });
        handle.hinstance = None;
        let raw = RawWindowHandle::Win32(handle);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Win32Handle {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let raw = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(raw) })
    }
}

/// Build and run the settings window. Blocks until the window is closed.
pub fn run_settings_window(
    config: Config,
    event_tx: std::sync::Weak<mpsc::Sender<SettingsEvent>>,
    initial_section: Option<&str>,
    high_contrast: bool,
    failed_binds: Vec<String>,
) -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let dark = is_dark_mode();

        let bg_brush = if dark {
            CreateSolidBrush(COLORREF(DARK_BG))
        } else {
            HBRUSH((COLOR_BTNFACE.0 + 1) as _)
        };

        let icon = LoadIconW(Some(hinstance.into()), PCWSTR(1 as _)).ok();
        let icon_sm = LoadImageW(
            Some(hinstance.into()),
            PCWSTR(1 as _),
            IMAGE_ICON,
            16,
            16,
            LR_DEFAULTCOLOR,
        )
        .ok()
        .map(|h| HICON(h.0));

        let class_name = w!("LeopardWMSettings");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: bg_brush,
            lpszClassName: class_name,
            hIcon: icon.unwrap_or_default(),
            hIconSm: icon_sm.unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            &windows::core::HSTRING::from(crate::locale::text(
                &config.appearance.language,
                "settings.text.leopardwm_settings",
            )),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            DEFAULT_WINDOW_WIDTH,
            DEFAULT_WINDOW_HEIGHT,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;
        let (size, work_area) =
            window_size_and_work_area(hwnd, (DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT));
        let (x, y, width, height, flags) = match work_area {
            Some(work_area) => {
                let (x, y, width, height) = fit_window_to_work_area(size, work_area);
                (x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE)
            }
            None => (
                0,
                0,
                size.0,
                size.1,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            ),
        };
        let _ = SetWindowPos(hwnd, None, x, y, width, height, flags);

        if let Ok(mut g) = SETTINGS_THREAD.lock() {
            *g = Some(GetCurrentThreadId());
            if let Ok(mut pending) = PENDING_RECORDED.lock() {
                pending.clear();
            }
        }

        *SETTINGS_WINDOW
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(hwnd.0 as isize);

        apply_win11_theming(hwnd, dark);

        // Extend the DWM frame into the entire client area so Mica renders behind content
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);

        // Persistent data directory so WebView2 reuses its browser profile
        // across settings opens (avoids cold-start each time).
        let data_dir = directories::ProjectDirs::from("", "", "leopardwm")
            .map(|d| d.cache_dir().join("webview2"))
            .unwrap_or_else(|| std::env::temp_dir().join("leopardwm-webview2"));
        let mut web_context = WebContext::new(Some(data_dir));

        let win_handle = Win32Handle(hwnd.0 as isize);
        let auto_start = leopardwm_platform_win32::autostart::get_autostart().unwrap_or(false);
        let config_json = {
            let mut val = serde_json::to_value(&config)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(ref mut map) = val {
                map.insert(
                    "high_contrast".to_string(),
                    serde_json::Value::Bool(high_contrast),
                );
                map.insert(
                    "auto_start".to_string(),
                    serde_json::Value::Bool(auto_start),
                );
            }
            serde_json::to_string(&val).unwrap_or_else(|_| "{}".to_string())
        };
        // The settings UI derives its hotkey-list labels, order, and
        // reset-to-defaults from this single catalog (see ipc::hotkeys).
        let catalog_json = serde_json::to_string(&leopardwm_ipc::hotkeys::hotkey_catalog())
            .unwrap_or_else(|_| "[]".to_string());
        let categories_json = serde_json::to_string(&leopardwm_ipc::hotkeys::hotkey_categories())
            .unwrap_or_else(|_| "[]".to_string());
        let failed_binds_json =
            serde_json::to_string(&failed_binds).unwrap_or_else(|_| "[]".to_string());

        let close_tx = event_tx.clone();

        let settings_html = SETTINGS_HTML;
        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(settings_html)
            .with_initialization_script(format!(
                "window._initConfig = {}; window._hotkeyCatalog = {}; window._hotkeyCategories = {}; window._failedHotkeys = {}; window._localeStrings = {}; window._language = {}; window._version = {}; window._languages = {};",
                config_json, catalog_json, categories_json, failed_binds_json, crate::locale::page_json(&config.appearance.language), serde_json::to_string(&config.appearance.language)?, serde_json::to_string(env!("CARGO_PKG_VERSION"))?, crate::locale::languages_json()
            ))
            .with_initialization_script(format!(
                "window._reportDraft = {}; window._windowsVersion = {};",
                serde_json::to_string(&REPORT_DRAFT.lock().ok().and_then(|draft| draft.clone()))?,
                serde_json::to_string(&leopardwm_platform_win32::get_windows_version())?
            ))
            .with_ipc_handler(move |req| {
                if let Some(event_tx) = event_tx.upgrade() {
                    handle_ipc(req.body(), &event_tx, hwnd);
                }
            })
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_additional_browser_args("--disable-features=msSmartScreenProtection")
            .build(&win_handle)
            .inspect_err(|_| {
                let _ = DestroyWindow(hwnd);
                if let Ok(mut thread) = SETTINGS_THREAD.lock() { *thread = None; }
                if let Ok(mut pending) = PENDING_HEALTH.lock() { *pending = None; }
                if let Ok(mut pending) = PENDING_SNAPSHOTS.lock() { pending.clear(); }
            })?;

        // The WebView remains at this address until WM_DESTROY clears the pointer.
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            &webview as *const wry::WebView as isize,
        );
        let _ = PostMessageW(Some(hwnd), WM_SETTINGS_PUSH_HEALTH, WPARAM(0), LPARAM(0));
        let _ = PostMessageW(Some(hwnd), WM_SETTINGS_PUSH_SNAPSHOT, WPARAM(0), LPARAM(0));
        let _ = PostMessageW(
            Some(hwnd),
            WM_SETTINGS_LIST_DIAGNOSTICS,
            WPARAM(0),
            LPARAM(0),
        );
        if let Some(sender) = close_tx.upgrade() {
            let _ = sender.send(SettingsEvent::RequestHealth);
        }

        let init_js = "init(window._initConfig)".to_string();
        let _ = webview.evaluate_script(&init_js);

        if let Some(section) = initial_section {
            let nav_js = format!(
                "document.querySelector('.nav-item[data-section=\"{}\"]').click()",
                section
            );
            let _ = webview.evaluate_script(&nav_js);
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        // Message loop. GetMessageW returns >0 for a message, 0 for WM_QUIT,
        // and -1 on error; break on anything <= 0 (matches the hotkey loop).
        let mut msg_buf = MSG::default();
        loop {
            let rc = GetMessageW(&mut msg_buf, None, 0, 0).0;
            if rc <= 0 {
                break;
            }
            // Daemon-initiated live refresh of the rejected-hotkey warning. The
            // webview is only valid on this thread, so we apply it here rather
            // than in the (static) window proc.
            if msg_buf.message == WM_SETTINGS_PUSH_BINDS {
                let json = PENDING_FAILED_BINDS.lock().ok().and_then(|mut p| p.take());
                if let Some(json) = json {
                    let js = format!(
                        "window._failedHotkeys = {}; if (typeof renderFailedHotkeys === 'function') renderFailedHotkeys();",
                        json
                    );
                    let _ = webview.evaluate_script(&js);
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_PUSH_LOCALE {
                let language = PENDING_LOCALE
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take());
                if let Some(language) = language {
                    let js = format!(
                        "refreshLocale({}, {})",
                        crate::locale::page_json(&language),
                        serde_json::to_string(&language)?
                    );
                    let _ = webview.evaluate_script(&js);
                    let title = windows::core::HSTRING::from(crate::locale::text(
                        &language,
                        "settings.text.leopardwm_settings",
                    ));
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(hwnd, &title);
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_PUSH_REPORT {
                if let Some(json) = PENDING_REPORT
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take())
                {
                    let _ = webview.evaluate_script(&format!(
                        "if (window.reportStatus) window.reportStatus({json});"
                    ));
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_PUSH_TRAY_CONFIG {
                let script = PENDING_TRAY_CONFIG
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take());
                if let Some(script) = script {
                    let _ = webview.evaluate_script(&script);
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_PUSH_RELOAD_CONFIG {
                let script = PENDING_RELOAD_CONFIG
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take());
                if let Some(script) = script {
                    let _ = webview.evaluate_script(&script);
                }
                continue;
            }
            if msg_buf.message == WM_SETTINGS_PUSH_RECORDED {
                let chords = PENDING_RECORDED
                    .lock()
                    .map(|mut pending| std::mem::take(&mut *pending))
                    .unwrap_or_default();
                for chord in chords {
                    let chord =
                        serde_json::to_string(&chord).unwrap_or_else(|_| "\"\"".to_string());
                    let js = format!(
                        "if (typeof onRecordedChord === 'function') onRecordedChord({});",
                        chord
                    );
                    let _ = webview.evaluate_script(&js);
                }
                continue;
            }
            let _ = TranslateMessage(&msg_buf);
            DispatchMessageW(&msg_buf);
        }

        let window_still_open = {
            let mut window = SETTINGS_WINDOW
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *window == Some(hwnd.0 as isize) {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                *window = None;
                true
            } else {
                false
            }
        };
        if window_still_open {
            let _ = DestroyWindow(hwnd);
        }

        // The window is gone; stop the daemon from posting to a dead thread.
        if let Ok(mut g) = SETTINGS_THREAD.lock() {
            *g = None;
            if let Ok(mut pending) = PENDING_TRAY_CONFIG.lock() {
                *pending = None;
            }
            if let Ok(mut pending) = PENDING_RELOAD_CONFIG.lock() {
                *pending = None;
            }
            if let Ok(mut pending) = PENDING_HEALTH.lock() {
                *pending = None;
            }
            if let Ok(mut pending) = PENDING_SNAPSHOTS.lock() {
                pending.clear();
            }
            if let Ok(mut pending) = PENDING_RECORDED.lock() {
                pending.clear();
            }
        }
        // Let the daemon resume hotkeys if the window closed mid-recording.
        if let Some(close_tx) = close_tx.upgrade() {
            let _ = close_tx.send(SettingsEvent::Closed);
        }

        // Hide window before tearing down WebView2 to prevent white flash.
        let _ = ShowWindow(hwnd, SW_HIDE);
        drop(webview);
        drop(web_context);
        if dark {
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));
        }
        let _ = UnregisterClassW(class_name, Some(hinstance.into()));
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct DiagnosticFile {
    name: String,
    size: u64,
    modified: Option<u64>,
}

fn list_diagnostics(dir: &std::path::Path) -> std::io::Result<Vec<DiagnosticFile>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        // Only snapshots are redacted; anything else in the folder stays off the list.
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("leopardwm-snapshot-") && name.ends_with(".txt")) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().ok();
        files.push((
            modified,
            DiagnosticFile {
                name,
                size: metadata.len(),
                modified: modified
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|time| time.as_secs()),
            },
        ));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    Ok(files.into_iter().map(|(_, file)| file).collect())
}

fn is_allowed_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap();
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !authority.is_empty()
        && !authority.chars().any(char::is_whitespace)
}

fn handle_ipc(body: &str, event_tx: &mpsc::Sender<SettingsEvent>, hwnd: HWND) {
    let msg: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            warn!("Settings IPC: invalid JSON: {}", log_text::text(e));
            return;
        }
    };

    let action = msg.get("action").and_then(|v| v.as_str()).unwrap_or("");

    match action {
        "report_draft" => {
            if let Some(draft) = msg
                .get("draft")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
            {
                if let Ok(mut stored) = REPORT_DRAFT.lock() {
                    *stored = Some(draft);
                }
            }
        }
        "report_issue" => {
            let Some(report): Option<super::issue_report::IssueReport> = msg
                .get("report")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
            else {
                warn!("Settings: invalid issue report");
                return;
            };
            if report.title.trim().is_empty() || report.actual.trim().is_empty() {
                warn!("Settings: issue report is missing required fields");
                return;
            }
            if let Ok(mut stored) = REPORT_DRAFT.lock() {
                *stored = Some(report.clone());
            }
            let version = env!("CARGO_PKG_VERSION");
            let windows = leopardwm_platform_win32::get_windows_version();
            let (url, truncated) = super::issue_report::issue_url(&report, version, &windows);
            let copied = if truncated {
                match leopardwm_platform_win32::clipboard::set_text(
                    hwnd.0 as isize,
                    &super::issue_report::full_text(&report, version, &windows),
                ) {
                    Ok(()) => true,
                    Err(error) => {
                        warn!("Settings: clipboard copy failed: {}", log_text::text(error));
                        false
                    }
                }
            } else {
                false
            };
            let _ = event_tx.send(SettingsEvent::ReportIssue {
                url,
                truncated,
                copied,
            });
        }
        "save" => {
            let saved = msg
                .get("config")
                .is_some_and(|cfg_val| do_save(cfg_val, event_tx));
            unsafe {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_SETTINGS_SAVE_RESULT,
                    WPARAM(usize::from(saved)),
                    LPARAM(0),
                );
            }
        }
        "request_health" => {
            let _ = event_tx.send(SettingsEvent::RequestHealth);
        }
        "create_snapshot" => {
            if let Some(id) = msg.get("id").and_then(|value| value.as_u64()) {
                let _ = event_tx.send(SettingsEvent::CreateSnapshot(id));
            }
        }
        "list_diagnostics" => unsafe {
            let _ = PostMessageW(
                Some(hwnd),
                WM_SETTINGS_LIST_DIAGNOSTICS,
                WPARAM(0),
                LPARAM(0),
            );
        },
        "reveal_diagnostics" => {
            let path = leopardwm_ipc::diagnostics_dir();
            if let Err(error) = leopardwm_ipc::anchored_fs::AnchoredDir::for_path(&path) {
                warn!(
                    "Cannot create diagnostics folder {}: {}",
                    log_text::path(path.display()),
                    log_text::text(error)
                );
            } else {
                leopardwm_platform_win32::shell::open(&path);
            }
        }
        "reveal_config" => {
            let path = crate::config::config_rejection()
                .map(|rejection| rejection.path)
                .or_else(|| {
                    crate::config::config_paths()
                        .into_iter()
                        .find(|path| path.exists())
                })
                .or_else(|| crate::config::config_paths().into_iter().next());
            if let Some(folder) = path.as_deref().and_then(std::path::Path::parent) {
                leopardwm_platform_win32::shell::open(folder);
            }
        }
        "set_recording" => {
            let recording = msg
                .get("recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let _ = event_tx.send(SettingsEvent::SetRecording(recording));
        }
        "set_auto_start" => {
            let Some(enabled) = msg.get("enabled").and_then(|v| v.as_bool()) else {
                warn!("Settings IPC: set_auto_start missing or non-bool 'enabled' field; ignoring");
                return;
            };
            use leopardwm_platform_win32::autostart;
            let result = if enabled {
                match std::env::current_exe() {
                    Ok(exe) => autostart::enable_autostart(&exe).map(|()| Some(exe)),
                    Err(e) => Err(anyhow::anyhow!("resolve daemon executable: {}", e)),
                }
            } else {
                autostart::disable_autostart().map(|()| None)
            };
            match result {
                Ok(Some(exe)) => info!(
                    "Auto-start enabled via Settings (path: {})",
                    log_text::path(exe.display())
                ),
                Ok(None) => info!("Auto-start disabled via Settings"),
                Err(e) => warn!(
                    "Settings: failed to update auto-start: {}",
                    log_text::text(e)
                ),
            }
        }
        "open_url" => {
            let Some(url) = msg.get("url").and_then(|v| v.as_str()) else {
                warn!("Settings IPC: rejected open_url with missing or non-string 'url'");
                return;
            };
            if !is_allowed_url(url) {
                warn!("Settings IPC: rejected open_url: {}", log_text::text(url));
                return;
            }
            leopardwm_platform_win32::shell::open(url);
        }
        other => {
            warn!("Settings IPC: unknown action: {}", log_text::text(other));
        }
    }
}

fn do_save(cfg_val: &serde_json::Value, event_tx: &mpsc::Sender<SettingsEvent>) -> bool {
    let mut cfg: Config = match serde_json::from_value(cfg_val.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "Settings: failed to parse config JSON: {}",
                log_text::text(e)
            );
            return false;
        }
    };

    let warnings = cfg.validate();
    for w in &warnings {
        warn!("Config: {} - {}", w.field, log_text::text(&w.message));
    }

    match cfg.save() {
        Ok(()) => {
            info!("Settings saved successfully");
            let _ = event_tx.send(SettingsEvent::Saved);
            true
        }
        Err(e) => {
            warn!("Failed to save settings: {}", log_text::text(e));
            false
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SETTINGS_PUSH_HEALTH
        | WM_SETTINGS_PUSH_SNAPSHOT
        | WM_SETTINGS_LIST_DIAGNOSTICS
        | WM_SETTINGS_SAVE_RESULT => {
            let webview = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const wry::WebView;
            if let Some(webview) = webview.as_ref() {
                match message {
                    WM_SETTINGS_SAVE_RESULT => {
                        let _ = webview
                            .evaluate_script(&format!("window.lwmSaveResult({});", wparam.0 != 0));
                    }
                    WM_SETTINGS_PUSH_HEALTH => {
                        if let Some(json) = PENDING_HEALTH
                            .lock()
                            .ok()
                            .and_then(|mut pending| pending.take())
                        {
                            let _ = webview
                                .evaluate_script(&format!("window.lwmReceiveHealth({json})"));
                        }
                    }
                    WM_SETTINGS_PUSH_SNAPSHOT => {
                        let results = PENDING_SNAPSHOTS
                            .lock()
                            .map(|mut pending| std::mem::take(&mut *pending))
                            .unwrap_or_default();
                        for result in results {
                            let _ = webview
                                .evaluate_script(&format!("window.lwmReceiveSnapshot({result})"));
                        }
                    }
                    WM_SETTINGS_LIST_DIAGNOSTICS => {
                        let listing = match list_diagnostics(&leopardwm_ipc::diagnostics_dir()) {
                            Ok(files) => serde_json::json!({"ok": true, "files": files}),
                            Err(error) => {
                                serde_json::json!({"ok": false, "error": error.to_string()})
                            }
                        };
                        let _ = webview
                            .evaluate_script(&format!("window.lwmReceiveDiagnostics({listing})"));
                    }
                    _ => unreachable!(),
                }
            }
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let minmax = &mut *(lparam.0 as *mut MINMAXINFO);
            let ((width, height), _) =
                window_size_and_work_area(hwnd, (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT));
            minmax.ptMinTrackSize.x = width;
            minmax.ptMinTrackSize.y = height;
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Paint black — DWM treats black in the extended frame as transparent,
            // letting the Mica backdrop show through.
            let hdc = HDC(wparam.0 as *mut _);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            LRESULT(1)
        }
        WM_SETTINGCHANGE => {
            // Re-apply DWM theming on any system setting change (theme toggle, etc.).
            // Cheap and idempotent — avoids unsafe lparam string parsing.
            apply_win11_theming(hwnd, is_dark_mode());
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            *SETTINGS_WINDOW
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn is_dark_mode() -> bool {
    unsafe {
        use windows::Win32::System::Registry::*;

        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, Some(0), KEY_READ, &mut key).is_err() {
            return false;
        }

        let value_name = w!("AppsUseLightTheme");
        let mut data: u32 = 1;
        let mut data_size = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            value_name,
            None,
            None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_size),
        )
        .is_ok();
        let _ = RegCloseKey(key);

        ok && data == 0
    }
}

unsafe fn apply_win11_theming(hwnd: HWND, dark: bool) {
    let val: i32 = if dark { 1 } else { 0 };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE_VAL),
        &val as *const i32 as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );

    let corner: u32 = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE_VAL),
        &corner as *const u32 as *const std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );

    let backdrop: u32 = DWMSBT_MAINWINDOW;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_SYSTEMBACKDROP_TYPE_VAL),
        &backdrop as *const u32 as *const std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_requests_config_reload() {
        let mut config = Config::default();
        config.layout.gap = -1;
        let (sender, receiver) = mpsc::channel();
        assert!(do_save(&serde_json::to_value(config).unwrap(), &sender));
        assert!(matches!(receiver.try_recv(), Ok(SettingsEvent::Saved)));
    }

    #[test]
    fn diagnostics_listing_sorts_by_full_modification_time_and_includes_metadata() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let dir = std::env::temp_dir().join(format!(
            "leopardwm-diagnostics-list-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        for (name, bytes, nanos) in [
            ("leopardwm-snapshot-a-older.txt", &b"old"[..], 100_000_000),
            ("leopardwm-snapshot-z-newer.txt", &b"newer"[..], 800_000_000),
            ("leopardwm-recording.log", &b"unredacted"[..], 900_000_000),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::new(1_700_000_000, nanos)),
            )
            .unwrap();
        }
        std::fs::create_dir(dir.join("subfolder")).unwrap();
        let files = list_diagnostics(&dir).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| (file.name.as_str(), file.size, file.modified))
                .collect::<Vec<_>>(),
            vec![
                ("leopardwm-snapshot-z-newer.txt", 5, Some(1_700_000_000)),
                ("leopardwm-snapshot-a-older.txt", 3, Some(1_700_000_000))
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn window_sizes_scale_from_96_dpi_logical_pixels() {
        assert_eq!(scale_for_dpi(DEFAULT_WINDOW_WIDTH, 96), 1000);
        assert_eq!(scale_for_dpi(DEFAULT_WINDOW_WIDTH, 120), 1250);
        assert_eq!(scale_for_dpi(MIN_WINDOW_HEIGHT, 144), 900);
    }

    #[test]
    fn window_sizes_do_not_exceed_monitor_work_area() {
        let work_area = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        assert_eq!(clamp_to_work_area((2000, 1200), work_area), (1280, 720));
        assert_eq!(clamp_to_work_area((1000, 600), work_area), (1000, 600));
    }

    #[test]
    fn fitted_window_is_centered_within_work_area() {
        let work_area = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        assert_eq!(
            fit_window_to_work_area((1600, 900), work_area),
            (-1760, 90, 1600, 900)
        );
        assert_eq!(
            fit_window_to_work_area((2400, 1200), work_area),
            (-1920, 0, 1920, 1080)
        );
    }

    #[test]
    fn allowed_urls_include_settings_links_and_mixed_case_schemes() {
        for url in [
            "https://github.com/jcardama/LeopardWM/graphs/contributors",
            "https://github.com/jcardama/LeopardWM",
            "https://buymeacoffee.com/jcardama",
            "hTtPs://example.com",
            "HtTp://example.com",
        ] {
            assert!(is_allowed_url(url), "expected URL to be allowed: {url}");
        }
    }

    #[test]
    fn disallowed_urls_reject_invalid_schemes_and_authorities() {
        for url in [
            "file:///C:/config.toml",
            "custom://example.com",
            "example.com",
            "://example.com",
            "",
            "https://",
            "https:// ",
            "https://exam ple.com",
            "https://example.com extra",
            "https:///x",
            "https:/x",
            " https://example.com",
        ] {
            assert!(!is_allowed_url(url), "expected URL to be rejected: {url:?}");
        }
    }
}
