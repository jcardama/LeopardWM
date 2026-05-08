//! Modern Win10/11 toast notifications via `ToastNotificationManager`.
//!
//! Win11 routes the legacy `Shell_NotifyIcon NIF_INFO` balloons to
//! Notification Center silently — they don't render the classic Win7-style
//! popup the user might expect. The proper modern path is the WinRT
//! `ToastNotificationManager` API, which renders the standard top-right
//! toast and persists in Notification Center.
//!
//! Unpackaged Win32 apps need an AppUserModelID registered before they can
//! show toasts. We do that at watchdog startup via `init()` (registry
//! entry + `SetCurrentProcessExplicitAppUserModelID`). The toast itself
//! renders on a worker thread wrapped in `catch_unwind` so a Win32 / WinRT
//! failure cannot kill the supervisor.

use anyhow::{Context, Result};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::warn;
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const AUMID: &str = "jcardama.LeopardWM.Watchdog";
const APP_NAME: &str = "LeopardWM";

#[derive(Debug, Clone, Copy)]
pub enum Severity {
    Info,
    Warning,
}

/// Register the AppUserModelID so the OS treats this process as a known
/// notification source. Must be called once, early in `main()`, before any
/// `show_toast()` call.
pub fn init() -> Result<()> {
    register_aumid_in_registry().context("AUMID registry registration failed")?;
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID))
            .context("SetCurrentProcessExplicitAppUserModelID failed")?;
    }
    Ok(())
}

fn register_aumid_in_registry() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!("Software\\Classes\\AppUserModelId\\{AUMID}");
    let (key, _) = hkcu.create_subkey(&path).context("create AUMID subkey")?;
    key.set_value("DisplayName", &APP_NAME)
        .context("set DisplayName value")?;
    Ok(())
}

/// Render a recovery toast on a worker thread. Returns a `JoinHandle` the
/// caller can drop (fire-and-forget) or join on (block until rendered).
pub fn show_toast(title: &str, body: &str, _severity: Severity) -> JoinHandle<()> {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(move || {
            // WinRT calls require COM/RT init on every thread. Use MTA so
            // we don't have message-pump requirements for this short-lived
            // worker. Ignore errors — if the apartment is already
            // initialized (S_FALSE / RPC_E_CHANGED_MODE), Show() still works.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            if let Err(err) = render(&title, &body) {
                warn!(%err, "Failed to render watchdog toast");
            }
        });
    })
}

fn render(title: &str, body: &str) -> Result<()> {
    let xml_str = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body),
    );
    let xml = XmlDocument::new().context("XmlDocument::new")?;
    xml.LoadXml(&HSTRING::from(xml_str)).context("XmlDocument.LoadXml")?;
    let toast = ToastNotification::CreateToastNotification(&xml)
        .context("CreateToastNotification")?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
        .context("CreateToastNotifierWithId")?;
    notifier.Show(&toast).context("ToastNotifier.Show")?;
    // Brief delay so the OS has a chance to deliver the toast before our
    // thread exits — Show() is asynchronous.
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(xml_escape(r#"say "hi""#), "say &quot;hi&quot;");
        assert_eq!(xml_escape("plain"), "plain");
    }
}
