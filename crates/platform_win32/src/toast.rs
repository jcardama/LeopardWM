//! Modern WinRT toast notifications, shared by the daemon and the watchdog.
//!
//! Win11 routes the legacy `Shell_NotifyIcon NIF_INFO` balloons to Notification
//! Center silently; the WinRT `ToastNotificationManager` renders the standard
//! top-right toast. Unpackaged Win32 apps must register an AppUserModelID before
//! they can show toasts — [`init`] does that (registry `DisplayName` +
//! `SetCurrentProcessExplicitAppUserModelID`).
//!
//! Toasts go through a single long-lived worker thread fed by a bounded channel,
//! so a burst of [`show_toast`] calls can never spawn an unbounded number of
//! threads — excess is dropped once the queue fills. [`show_toast_blocking`]
//! renders on the caller's thread for shutdown-critical notices where the
//! process may exit before the async worker would deliver.

use anyhow::{Context, Result};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, warn};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

struct ToastMsg {
    title: String,
    body: String,
}

/// Bounded queue depth. A burst beyond this drops the overflow rather than
/// spawning threads or blocking the caller — toasts are advisory.
const QUEUE_DEPTH: usize = 32;

static SENDER: OnceLock<SyncSender<ToastMsg>> = OnceLock::new();
static AUMID: OnceLock<String> = OnceLock::new();

/// Register the AppUserModelID and start the toast worker. Call once, early in
/// `main()`, before any toast. `aumid` is the per-app identity (distinct per
/// process, e.g. `jcardama.LeopardWM`); `app_name` is its Notification Center
/// display name.
pub fn init(aumid: &str, app_name: &str) -> Result<()> {
    register_aumid(aumid, app_name)?;
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(aumid))
            .context("SetCurrentProcessExplicitAppUserModelID failed")?;
    }
    let _ = AUMID.set(aumid.to_string());

    let (tx, rx) = sync_channel::<ToastMsg>(QUEUE_DEPTH);
    let aumid_owned = aumid.to_string();
    std::thread::spawn(move || {
        // One COM init for the worker's lifetime. `recv` blocks until a toast is
        // queued, so the thread idles cheaply between (rare) toasts.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        while let Ok(msg) = rx.recv() {
            // A WinRT failure must not kill the worker, or all later toasts are
            // silently lost.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Err(err) = render(&aumid_owned, &msg.title, &msg.body) {
                    warn!(%err, "Failed to render toast");
                }
            }));
        }
        // Only balance a successful init (skip on RPC_E_CHANGED_MODE). Reached
        // only if all senders drop (process teardown).
        if hr.is_ok() {
            unsafe { CoUninitialize() };
        }
    });
    let _ = SENDER.set(tx);
    Ok(())
}

/// Queue a toast on the worker thread (fire-and-forget). No-op if [`init`] was
/// not called; drops the toast if the worker is backed up (bounded queue).
pub fn show_toast(title: &str, body: &str) {
    if let Some(tx) = SENDER.get() {
        if tx
            .try_send(ToastMsg {
                title: title.to_string(),
                body: body.to_string(),
            })
            .is_err()
        {
            debug!("Toast queue full; dropping notification: {title}");
        }
    }
}

/// Render a toast synchronously on the calling thread and wait briefly for the
/// OS to pick it up, for shutdown-critical notices where the process may exit
/// before the async worker would deliver. No-op if [`init`] was not called.
pub fn show_toast_blocking(title: &str, body: &str) {
    let Some(aumid) = AUMID.get() else { return };
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Catch a render panic so we still reach CoUninitialize below — losing
        // the toast is fine, leaking a COM init on the caller thread is not.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Err(err) = render(aumid, title, body) {
                warn!(%err, "Failed to render blocking toast");
            }
            // Show() is asynchronous; give the OS a moment to deliver before the
            // caller (typically) exits the process.
            std::thread::sleep(Duration::from_millis(500));
        }));
        if hr.is_ok() {
            CoUninitialize();
        }
    }
}

fn register_aumid(aumid: &str, app_name: &str) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!("Software\\Classes\\AppUserModelId\\{aumid}");
    let (key, _) = hkcu.create_subkey(&path).context("create AUMID subkey")?;
    key.set_value("DisplayName", &app_name)
        .context("set DisplayName value")?;
    Ok(())
}

fn render(aumid: &str, title: &str, body: &str) -> Result<()> {
    let xml_str = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body),
    );
    let xml = XmlDocument::new().context("XmlDocument::new")?;
    xml.LoadXml(&HSTRING::from(xml_str))
        .context("XmlDocument.LoadXml")?;
    let toast =
        ToastNotification::CreateToastNotification(&xml).context("CreateToastNotification")?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))
        .context("CreateToastNotifierWithId")?;
    notifier.Show(&toast).context("ToastNotifier.Show")?;
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
            // Drop XML 1.0-illegal control chars (only tab/LF/CR are legal); a
            // stray NUL etc. in a window title would make LoadXml reject the
            // whole payload and silently swallow the toast.
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
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

    #[test]
    fn xml_escape_drops_illegal_control_chars_but_keeps_whitespace() {
        assert_eq!(xml_escape("a\u{0}b\u{1}c"), "abc");
        assert_eq!(xml_escape("line\tone\r\ntwo"), "line\tone\r\ntwo");
    }
}
