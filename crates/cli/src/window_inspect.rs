//! `lwm doctor windows` — read-only admission-path diagnostics.

use anyhow::Result;
use leopardwm_platform_win32::inspect::{inspect_windows, SkipReason, WindowInspection};
use leopardwm_platform_win32::{
    get_process_executable, is_dialog_like_window, window_manage_block, ManageBlock,
};
use std::collections::HashSet;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn handle_doctor_windows(
    watch: Option<u64>,
    delay: Option<u64>,
    include_titles: bool,
    show_all: bool,
) -> Result<()> {
    if let Some(secs) = delay {
        for remaining in (1..=secs).rev() {
            println!("Waiting… {remaining}s");
            thread::sleep(Duration::from_secs(1));
        }
    }

    if let Some(secs) = watch {
        run_watch(secs, include_titles, show_all)?;
    } else {
        let windows = inspect_windows().map_err(|e| anyhow::anyhow!(e))?;
        let (shown, omitted) = partition_for_display(&windows, show_all);
        println!(
            "LeopardWM window inspection ({} top-level window{}; showing {})",
            windows.len(),
            if windows.len() == 1 { "" } else { "s" },
            shown.len()
        );
        println!();
        for w in &shown {
            print_window(w, include_titles, false);
            println!();
        }
        if omitted > 0 {
            println!(
                "Omitted {omitted} window(s) that SKIP on both admission paths. Pass --all to show them."
            );
            println!();
        }
        print_footer();
    }

    Ok(())
}

type WatchKey = (u64, String, Result<(), SkipReason>, Result<(), SkipReason>);

fn admits_any(w: &WindowInspection) -> bool {
    w.startup_verdict.is_ok() || w.live_create_verdict.is_ok()
}

fn partition_for_display(
    windows: &[WindowInspection],
    show_all: bool,
) -> (Vec<&WindowInspection>, usize) {
    if show_all {
        return (windows.iter().collect(), 0);
    }
    let shown: Vec<&WindowInspection> = windows.iter().filter(|w| admits_any(w)).collect();
    let omitted = windows.len().saturating_sub(shown.len());
    (shown, omitted)
}

fn run_watch(secs: u64, include_titles: bool, show_all: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    // Full key includes verdicts so a stable hwnd+class re-prints when a verdict
    // flips. Title is intentionally excluded: browser/editor titles thrash.
    let mut seen: HashSet<WatchKey> = HashSet::new();
    let mut known_identities: HashSet<(u64, String)> = HashSet::new();
    let mut omitted_unique = 0usize;
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut consecutive_failing = false;
    println!("LeopardWM window inspection — watching for {secs}s (100ms interval)");
    if !show_all {
        println!("Default filter: only windows that ADMIT on at least one path (pass --all for every window).");
    }
    println!();

    while Instant::now() < deadline {
        let windows = match inspect_windows() {
            Ok(w) => {
                if let Some(msg) =
                    watch_sample_status_message(consecutive_failing, true, None, failure_count)
                {
                    let _ = writeln!(io::stderr(), "{msg}");
                }
                consecutive_failing = false;
                success_count += 1;
                w
            }
            Err(e) => {
                failure_count += 1;
                if let Some(msg) = watch_sample_status_message(
                    consecutive_failing,
                    false,
                    Some(e.as_str()),
                    failure_count,
                ) {
                    let _ = writeln!(io::stderr(), "{msg}");
                }
                consecutive_failing = true;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        for w in &windows {
            let class = sanitize_diag(&w.class_name);
            let key = (
                w.hwnd,
                class.clone(),
                w.startup_verdict,
                w.live_create_verdict,
            );
            if !seen.insert(key) {
                continue;
            }
            if !show_all && !admits_any(w) {
                omitted_unique += 1;
                continue;
            }
            let identity = (w.hwnd, class);
            let is_update = !known_identities.insert(identity);
            print_window(w, include_titles, is_update);
            println!();
        }
        thread::sleep(Duration::from_millis(100));
    }

    if success_count == 0 {
        return Err(anyhow::anyhow!(
            "Watch failed: 0 successful samples ({} failed)",
            failure_count
        ));
    }

    println!(
        "Watch complete: {} unique window state(s) observed.",
        seen.len()
    );
    if failure_count > 0 {
        println!("{failure_count} sample(s) failed during the watch.");
    }
    if omitted_unique > 0 {
        println!(
            "Omitted {omitted_unique} unique window state(s) that SKIP on both admission paths. Pass --all to show them."
        );
    }
    print_footer();
    Ok(())
}

/// Status line for a watch sample, if one should be emitted.
///
/// First failure reports; consecutive failures stay quiet; a success after
/// failures reports recovery. Pure so the throttle/recovery rules can be unit
/// tested without a live HWND.
fn watch_sample_status_message(
    was_failing: bool,
    succeeded: bool,
    err: Option<&str>,
    failure_count: usize,
) -> Option<String> {
    match (was_failing, succeeded) {
        (false, false) => Some(format!(
            "EnumWindows failed during watch sample: {}",
            err.unwrap_or("unknown error")
        )),
        (true, false) => None,
        (true, true) => Some(format!(
            "EnumWindows recovered ({failure_count} failed sample(s) so far this watch)"
        )),
        (false, true) => None,
    }
}

fn print_window(w: &WindowInspection, include_titles: bool, is_update: bool) {
    let class = sanitize_diag(&w.class_name);
    let title = sanitize_diag(&w.title);
    if is_update {
        println!("hwnd {:#018x}  (changed since first sighting)", w.hwnd);
    } else {
        println!("hwnd {:#018x}", w.hwnd);
    }
    println!(
        "  LIVE-CREATE path (how popups are admitted): {}",
        format_verdict(w.live_create_verdict, &class)
    );
    println!(
        "  STARTUP/REFRESH path:                       {}",
        format_verdict(w.startup_verdict, &class)
    );
    println!("  class    {}", display_or_empty(&class));
    println!("  title    {}", format_title(&title, include_titles));
    println!(
        "  style    {:#010x}   ex-style {:#010x}",
        w.style, w.ex_style
    );
    match w.owner_hwnd {
        Some(owner) => println!("  owner    {:#018x}", owner),
        None => println!("  owner    (none)"),
    }
    let exe = sanitize_diag(
        &get_process_executable(w.process_id).unwrap_or_else(|| "(unknown)".to_string()),
    );
    println!("  exe      {exe}");
    match w.rect {
        Some(r) => println!("  rect     ({}, {}) {}x{}", r.x, r.y, r.width, r.height),
        None => println!("  rect     (unreadable)"),
    }
    let dialog = is_dialog_like_window(w.hwnd);
    println!(
        "  dialog-shape heuristic (applies only when no user rule matched): {}",
        if dialog {
            "yes (this heuristic would leave it floating, if admitted)"
        } else {
            "no (this heuristic would not float it)"
        }
    );
    println!("  elevation {}", format_elevation(w.hwnd));
}

fn format_verdict(verdict: Result<(), SkipReason>, class_name: &str) -> String {
    match verdict {
        Ok(()) => "ADMIT".to_string(),
        Err(reason) => {
            let detail = reason_detail(reason, class_name);
            if detail.is_empty() {
                format!("SKIP — {reason:?}")
            } else {
                format!("SKIP — {reason:?} ({detail})")
            }
        }
    }
}

fn reason_detail(reason: SkipReason, class_name: &str) -> String {
    match reason {
        SkipReason::SkipClass if !class_name.is_empty() => {
            format!("\"{class_name}\" is in the built-in skip list")
        }
        SkipReason::SkipTitle => "title is in the built-in skip list".to_string(),
        SkipReason::ToolWindow => "WS_EX_TOOLWINDOW without resizable WS_EX_APPWINDOW".to_string(),
        SkipReason::NoActivate => "WS_EX_NOACTIVATE is set".to_string(),
        SkipReason::Owned => "window has a non-null GW_OWNER".to_string(),
        SkipReason::EmptyOrUnreadableTitle => {
            "title length is 0 or GetWindowTextW failed".to_string()
        }
        SkipReason::Cloaked => "DWM shell-cloaked (other virtual desktop)".to_string(),
        SkipReason::Minimized => "IsIconic".to_string(),
        SkipReason::StyleNotVisible => "WS_VISIBLE style bit clear".to_string(),
        SkipReason::NotVisible => "IsWindowVisible is false".to_string(),
        SkipReason::RectReadFailed => "GetWindowRect failed".to_string(),
        SkipReason::ZeroSize => "width or height is 0".to_string(),
        _ => String::new(),
    }
}

fn format_title(title: &str, include_titles: bool) -> String {
    if include_titles {
        if title.is_empty() {
            "(empty)".to_string()
        } else {
            format!("\"{title}\"")
        }
    } else {
        "[redacted]".to_string()
    }
}

fn format_elevation(hwnd: u64) -> String {
    match window_manage_block(hwnd) {
        ManageBlock::No => {
            "manageable relative to this CLI process (same or lower integrity; matches the daemon only when both run at the same privilege level)".to_string()
        }
        ManageBlock::HigherIntegrity => {
            "blocked relative to this CLI process — higher integrity (elevated/admin or System); may still be manageable by an elevated daemon".to_string()
        }
        ManageBlock::Protected => {
            "blocked relative to this CLI process — protected/unreadable process token".to_string()
        }
    }
}

fn display_or_empty(s: &str) -> &str {
    if s.is_empty() {
        "(empty)"
    } else {
        s
    }
}

/// Single-line, control- and format-character-free form of an untrusted Win32
/// string for report pasting. Escapes C0/C1 controls, Unicode format (Cf)
/// characters, and U+2028/U+2029 line/paragraph separators; leaves ordinary
/// class names intact.
fn sanitize_diag(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_format_or_line_sep(c) => {
                out.push_str(&format!("\\u{{{:04x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Unicode general category Cf (format) plus Zl/Zp line/paragraph separators.
/// `char::is_control()` is Cc-only and misses these.
fn is_format_or_line_sep(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn print_footer() {
    println!(
        "Note: verdicts are the built-in admission filters only. The daemon may still\n\
         float or ignore a window due to a user window rule, already-managed state,\n\
         or transient-popup suppression — this command cannot model those\n\
         post-enumeration decisions. Shell-cloaked windows, ConsoleWindowClass hosts\n\
         whose title is just an exe path, and elevated windows a non-elevated daemon\n\
         cannot reposition (UIPI) can still ADMIT here yet be dropped later by the\n\
         daemon's post-lookup shell-cloak, console-host, and elevation filters; what\n\
         this command cannot see is historical admission under different\n\
         styles/visibility/cloak/owner state. Titles are redacted unless\n\
         --include-titles is passed; review all output before pasting into a\n\
         public issue."
    );
}

#[cfg(test)]
mod tests {
    use super::{sanitize_diag, watch_sample_status_message};

    #[test]
    fn sanitize_diag_escapes_controls_and_newlines() {
        assert_eq!(sanitize_diag("plain.Class"), "plain.Class");
        assert_eq!(sanitize_diag("a\nb"), "a\\nb");
        assert_eq!(sanitize_diag("a\rb\tc"), "a\\rb\\tc");
        assert_eq!(sanitize_diag("x\u{1b}[31my"), "x\\u{001b}[31my");
        assert_eq!(sanitize_diag("del\u{7f}"), "del\\u{007f}");
    }

    #[test]
    fn sanitize_diag_escapes_format_and_line_separators() {
        assert_eq!(sanitize_diag("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_diag("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(sanitize_diag("a\u{2029}b"), "a\\u{2029}b");
        assert_eq!(sanitize_diag("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(sanitize_diag("Shell_TrayWnd"), "Shell_TrayWnd");
    }

    #[test]
    fn watch_sample_status_reports_first_failure_only() {
        let first = watch_sample_status_message(false, false, Some("boom"), 1);
        assert_eq!(
            first.as_deref(),
            Some("EnumWindows failed during watch sample: boom")
        );
        assert!(watch_sample_status_message(true, false, Some("boom"), 2).is_none());
        assert!(watch_sample_status_message(true, false, Some("boom"), 3).is_none());
    }

    #[test]
    fn watch_sample_status_reports_recovery_after_failures() {
        assert!(watch_sample_status_message(false, true, None, 0).is_none());
        let recovered = watch_sample_status_message(true, true, None, 4);
        assert_eq!(
            recovered.as_deref(),
            Some("EnumWindows recovered (4 failed sample(s) so far this watch)")
        );
    }
}
