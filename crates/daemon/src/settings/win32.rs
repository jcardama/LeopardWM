//! Win32 window shell + WebView2 settings panel.
//!
//! Creates a Win32 window with DWM theming (Mica, dark title bar, rounded
//! corners), then fills the client area with a WebView2 instance via `wry`.
//! All settings UI lives in the embedded HTML/CSS/JS (see `html.rs`).
//! Communication is via IPC: Rust → JS with `evaluate_script`, JS → Rust
//! with `window.ipc.postMessage`.

use std::sync::mpsc;

use anyhow::Result;
use tracing::{info, warn};
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, Win32WindowHandle,
    WindowsDisplayHandle,
};
use wry::{WebContext, WebViewBuilderExtWindows as _};

use crate::config::Config;

use super::html::SETTINGS_HTML;
use super::SettingsEvent;

// DWM attributes for Windows 11 theming (not yet in windows crate enum)
const DWMWA_USE_IMMERSIVE_DARK_MODE_VAL: i32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE_VAL: i32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE_VAL: i32 = 38;
const DWMWCP_ROUND: u32 = 2;
const DWMSBT_MAINWINDOW: u32 = 2; // Mica

const WINDOW_WIDTH: i32 = 780;
const WINDOW_HEIGHT: i32 = 560;

// Dark mode background (COLORREF = 0x00BBGGRR)
const DARK_BG: u32 = 0x00202020;

/// Wrapper that implements `HasWindowHandle` + `HasDisplayHandle` for a raw HWND.
struct Win32Handle(isize);

impl HasWindowHandle for Win32Handle {
    fn window_handle(
        &self,
        ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let mut handle = Win32WindowHandle::new(unsafe {
            std::num::NonZero::new_unchecked(self.0)
        });
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
pub fn run_settings_window(config: Config, event_tx: mpsc::Sender<SettingsEvent>) -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let dark = is_dark_mode();

        let bg_brush = if dark {
            CreateSolidBrush(COLORREF(DARK_BG))
        } else {
            HBRUSH((COLOR_BTNFACE.0 + 1) as _)
        };

        // Register window class
        let class_name = w!("LeopardWMSettings");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: bg_brush,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // Create the window
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("LeopardWM Settings"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        // Apply Windows 11 DWM theming
        apply_win11_theming(hwnd, dark);

        // Persistent data directory so WebView2 reuses its browser profile
        // across settings opens (avoids cold-start each time).
        let data_dir = directories::ProjectDirs::from("", "", "leopardwm")
            .map(|d| d.cache_dir().join("webview2"))
            .unwrap_or_else(|| std::env::temp_dir().join("leopardwm-webview2"));
        let mut web_context = WebContext::new(Some(data_dir));

        // Create the WebView2 instance via wry
        let win_handle = Win32Handle(hwnd.0 as isize);
        let config_json =
            serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());

        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(SETTINGS_HTML)
            .with_initialization_script(&format!("window._initConfig = {};", config_json))
            .with_ipc_handler(move |req| {
                handle_ipc(req.body(), &event_tx, hwnd);
            })
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_additional_browser_args("--disable-features=msSmartScreenProtection")
            .build(&win_handle)?;

        // Populate the form with the current config
        let init_js = format!("init(window._initConfig)");
        let _ = webview.evaluate_script(&init_js);

        // Show the window
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        // Message loop
        let mut msg_buf = MSG::default();
        while GetMessageW(&mut msg_buf, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg_buf);
            DispatchMessageW(&msg_buf);
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

/// Handle IPC messages from the WebView (JS → Rust).
fn handle_ipc(body: &str, event_tx: &mpsc::Sender<SettingsEvent>, _hwnd: HWND) {
    let msg: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            warn!("Settings IPC: invalid JSON: {}", e);
            return;
        }
    };

    let action = msg.get("action").and_then(|v| v.as_str()).unwrap_or("");

    match action {
        "save" => {
            if let Some(cfg_val) = msg.get("config") {
                do_save(cfg_val, event_tx);
            }
        }
        other => {
            warn!("Settings IPC: unknown action: {}", other);
        }
    }
}

/// Deserialize config JSON, validate, save to disk, and notify daemon.
fn do_save(cfg_val: &serde_json::Value, event_tx: &mpsc::Sender<SettingsEvent>) -> bool {
    let mut cfg: Config = match serde_json::from_value(cfg_val.clone()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Settings: failed to parse config JSON: {}", e);
            return false;
        }
    };

    let warnings = cfg.validate();
    for w in &warnings {
        warn!("Config validation: {}: {}", w.field, w.message);
    }

    match cfg.save() {
        Ok(()) => {
            info!("Settings saved successfully");
            let _ = event_tx.send(SettingsEvent::Saved);
            true
        }
        Err(e) => {
            warn!("Failed to save settings: {}", e);
            false
        }
    }
}

// ── Window Procedure ─────────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE | WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

// ── Windows 11 Theming ──────────────────────────────────────────────

/// Detect whether the system is using dark mode via the registry.
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

/// Apply Windows 11 DWM attributes: dark title bar, rounded corners, Mica backdrop.
unsafe fn apply_win11_theming(hwnd: HWND, dark: bool) {
    if dark {
        let val: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE_VAL),
            &val as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }

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
