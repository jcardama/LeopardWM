//! `lwm doctor windows` — read-only admission-path diagnostics.

use anyhow::Result;
use leopardwm_platform_win32::inspect::{inspect_windows, SkipReason, WindowInspection};
use leopardwm_platform_win32::{
    get_process_executable, is_dialog_like_window, window_manage_block, ManageBlock,
};
use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn handle_doctor_windows(
    watch: Option<u64>,
    delay: Option<u64>,
    include_titles: bool,
) -> Result<()> {
    if let Some(secs) = delay {
        for remaining in (1..=secs).rev() {
            println!("Waiting… {remaining}s");
            thread::sleep(Duration::from_secs(1));
        }
    }

    if let Some(secs) = watch {
        run_watch(secs, include_titles);
    } else {
        let windows = inspect_windows();
        println!(
            "LeopardWM window inspection ({} top-level window{})",
            windows.len(),
            if windows.len() == 1 { "" } else { "s" }
        );
        println!();
        for w in &windows {
            print_window(w, include_titles);
            println!();
        }
        print_footer();
    }

    Ok(())
}

fn run_watch(secs: u64, include_titles: bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen: HashSet<(u64, String, String)> = HashSet::new();
    println!(
        "LeopardWM window inspection — watching for {secs}s (100ms interval)"
    );
    println!();

    while Instant::now() < deadline {
        let windows = inspect_windows();
        for w in &windows {
            let key = (w.hwnd, w.class_name.clone(), w.title.clone());
            if seen.insert(key) {
                print_window(w, include_titles);
                println!();
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    println!(
        "Watch complete: {} unique window identity(ies) observed.",
        seen.len()
    );
    print_footer();
}

fn print_window(w: &WindowInspection, include_titles: bool) {
    println!("hwnd {:#018x}", w.hwnd);
    println!(
        "  LIVE-CREATE path (how popups are admitted): {}",
        format_verdict(w.live_create_verdict, &w.class_name, true)
    );
    println!(
        "  STARTUP/REFRESH path:                       {}",
        format_verdict(w.startup_verdict, &w.class_name, false)
    );
    println!("  class    {}", display_or_empty(&w.class_name));
    println!("  title    {}", format_title(&w.title, include_titles));
    println!(
        "  style    {:#010x}   ex-style {:#010x}",
        w.style, w.ex_style
    );
    match w.owner_hwnd {
        Some(owner) => println!("  owner    {:#018x}", owner),
        None => println!("  owner    (none)"),
    }
    let exe = get_process_executable(w.process_id).unwrap_or_else(|| "(unknown)".to_string());
    println!("  exe      {exe}");
    match w.rect {
        Some(r) => println!(
            "  rect     ({}, {}) {}x{}",
            r.x, r.y, r.width, r.height
        ),
        None => println!("  rect     (unreadable)"),
    }
    let dialog = is_dialog_like_window(w.hwnd);
    println!(
        "  dialog-shape heuristic (applies only when no user rule matched): {}",
        if dialog { "yes" } else { "no" }
    );
    println!("  elevation {}", format_elevation(w.hwnd));
}

fn format_verdict(
    verdict: Result<(), SkipReason>,
    class_name: &str,
    live_create: bool,
) -> String {
    match verdict {
        Ok(()) => "ADMIT".to_string(),
        Err(reason) => {
            let detail = reason_detail(reason, class_name, live_create);
            if detail.is_empty() {
                format!("SKIP — {reason:?}")
            } else {
                format!("SKIP — {reason:?} ({detail})")
            }
        }
    }
}

fn reason_detail(reason: SkipReason, class_name: &str, _live_create: bool) -> String {
    match reason {
        SkipReason::SkipClass if !class_name.is_empty() => {
            format!("\"{class_name}\" is in the built-in skip list")
        }
        SkipReason::SkipTitle => "title is in the built-in skip list".to_string(),
        SkipReason::ToolWindow => {
            "WS_EX_TOOLWINDOW without resizable WS_EX_APPWINDOW".to_string()
        }
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
        format!("[redacted — length {}]", title.chars().count())
    }
}

fn format_elevation(hwnd: u64) -> String {
    match window_manage_block(hwnd) {
        ManageBlock::No => "manageable (same or lower integrity)".to_string(),
        ManageBlock::HigherIntegrity => {
            "blocked — higher integrity than this process (elevated/admin or System)".to_string()
        }
        ManageBlock::Protected => {
            "blocked — protected/unreadable process token".to_string()
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

fn print_footer() {
    println!(
        "Note: verdicts are the built-in admission filters only. The daemon may still\n\
         float or ignore a window due to a user window rule, already-managed state, or\n\
         transient-popup suppression — this command cannot see those. Titles are\n\
         redacted unless --include-titles is passed; review all output before pasting\n\
         into a public issue."
    );
}
