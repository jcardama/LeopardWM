use regex::{Captures, Regex};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct HealthRow {
    pub summary: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ConfigLoadStatus {
    Loaded,
    NotFound,
    ReadFailed(String),
}

#[derive(Debug, Clone)]
pub struct Monitor {
    pub device: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub monitor: String,
    pub index: usize,
    pub rows: Vec<Vec<(i32, usize)>>,
    pub floating: usize,
}

#[derive(Debug, Clone)]
pub struct SnapshotInput {
    pub requester: String,
    pub requester_version: String,
    pub requester_integrity: Option<u32>,
    pub daemon_version: Option<String>,
    pub daemon_integrity: Option<u32>,
    pub daemon_uptime: Option<u64>,
    pub unavailable_reason: String,
    pub windows_version: String,
    pub monitors: Option<Vec<Monitor>>,
    pub health: Option<Vec<HealthRow>>,
    pub config_path: PathBuf,
    pub config_status: ConfigLoadStatus,
    pub config_text: Option<String>,
    pub workspaces: Option<Vec<Workspace>>,
    pub known_titles: Vec<String>,
    pub userprofile: Option<String>,
    pub log_dir: PathBuf,
    pub timestamp: SystemTime,
}

fn masked_config(text: &str) -> String {
    fn mask(value: &mut toml::Value) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table.iter_mut() {
                    if key == "match_title" || key == "match_class" {
                        *value = toml::Value::String("<masked>".into());
                    } else {
                        mask(value);
                    }
                }
            }
            toml::Value::Array(values) => values.iter_mut().for_each(mask),
            _ => {}
        }
    }
    match text.parse::<toml::Table>() {
        Ok(table) => {
            let mut value = toml::Value::Table(table);
            mask(&mut value);
            toml::to_string_pretty(&value)
                .unwrap_or_else(|_| "config text omitted: serialization failed".into())
        }
        Err(error) => {
            let offset = error.span().map_or(0, |span| span.start.min(text.len()));
            let prefix = &text[..text.floor_char_boundary(offset)];
            let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
            let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            format!("config text omitted: TOML parse error at line {line}, column {column}")
        }
    }
}

fn file_section(path: &Path, tail: Option<usize>, stitch: bool) -> String {
    let current = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => return format!("unavailable: {error}\n"),
    };
    let Some(limit) = tail else {
        return if current.is_empty() {
            "(empty)\n".into()
        } else {
            format!("{current}\n")
        };
    };
    let mut lines: Vec<String> = Vec::new();
    if stitch {
        if let Ok(previous) = fs::read_to_string(crate::bounded_log::previous_log_path(path)) {
            lines.extend(previous.lines().map(str::to_owned));
        }
    }
    lines.extend(current.lines().map(str::to_owned));
    let start = lines.len().saturating_sub(limit);
    let mut result = lines[start..].join("\n");
    result.push('\n');
    result
}

fn replace_known(text: &str, titles: &[String]) -> String {
    let mut unique: Vec<&str> = titles
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .collect();
    unique.sort_unstable();
    unique.dedup();
    let mut forms = Vec::new();
    for (index, title) in unique.iter().enumerate() {
        let marker = format!("<title#{}>", index + 1);
        forms.push(((*title).to_string(), marker.clone()));
        let debug = format!("{title:?}");
        let escaped = &debug[1..debug.len() - 1];
        if escaped != *title {
            forms.push((escaped.to_string(), marker));
        }
    }
    forms.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    if forms.is_empty() {
        return text.to_string();
    }
    let matcher = match aho_corasick::AhoCorasick::builder()
        .match_kind(aho_corasick::MatchKind::LeftmostLongest)
        .build(forms.iter().map(|(form, _)| form))
    {
        Ok(matcher) => matcher,
        Err(_) => return "snapshot omitted: too many titles to redact safely".into(),
    };
    let mut result = String::with_capacity(text.len());
    let mut end = 0;
    for found in matcher.find_iter(text) {
        result.push_str(&text[end..found.start()]);
        result.push_str(&forms[found.pattern().as_usize()].1);
        end = found.end();
    }
    result.push_str(&text[end..]);
    result
}

fn redact_paths(text: &str, userprofile: Option<&str>) -> String {
    let executable = Regex::new(
        r#"(?i)(?:[a-z]:[\\/]+|[\\/]{2,}[^\\/\s"']+[\\/]+)[^"'\r\n]*?[^\\/\s"']+\.exe\b"#,
    )
    .unwrap();
    let mut text = executable
        .replace_all(text, |c: &Captures<'_>| {
            c[0].rsplit(['\\', '/']).next().unwrap_or("").to_string()
        })
        .into_owned();
    if let Some(profile) = userprofile.filter(|p| !p.is_empty()) {
        text = text.replace(profile, "%USERPROFILE%");
        text = text.replace(&profile.replace('\\', "\\\\"), "%USERPROFILE%");
        text = text.replace(&profile.replace('\\', "/"), "%USERPROFILE%");
    }
    let profile = Regex::new(r#"(?i)[a-z]:[\\/]+users[\\/]+[^\\/"'\r\n]+"#).unwrap();
    profile.replace_all(&text, "%USERPROFILE%").into_owned()
}

fn redact_structured(text: &str, titles: &[String]) -> String {
    let known = |value: &str| {
        (value.starts_with("<title#")
            && value.ends_with('>')
            && value[7..value.len() - 1]
                .chars()
                .all(|ch| ch.is_ascii_digit()))
            || titles
                .iter()
                .filter(|title| !title.trim().is_empty())
                .any(|title| {
                    let debug = format!("{title:?}");
                    title == value || &debug[1..debug.len() - 1] == value
                })
    };
    let structured = Regex::new(r#"(title\s*[:=]\s*(?:Some\()?\")((?:\\.|[^"\\])*)(\")"#).unwrap();
    let mut text = text.to_string();
    text = structured
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    let cannot_tile = Regex::new(r"(Cannot tile window ')([^\r\n]*)(')").unwrap();
    text = cannot_tile
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    text
}

/// Redact known titles and supported structured forms. Initial passes prevent
/// known titles like "Users" or "title" from breaking later recognition.
/// Unstructured titles from closed windows cannot be identified reliably.
pub fn redact(text: &str, titles: &[String], userprofile: Option<&str>) -> String {
    let text = redact_paths(text, userprofile);
    let text = redact_structured(&text, titles);
    let text = replace_known(&text, titles);
    let text = redact_structured(&text, &[]);
    redact_paths(&text, userprofile)
}

pub fn render(input: &SnapshotInput) -> String {
    let mut out = String::from("## Environment\n");
    out.push_str(&format!(
        "Requester: {} {} (integrity {:?})\nWindows: {}\n",
        input.requester, input.requester_version, input.requester_integrity, input.windows_version
    ));
    if let Some(version) = &input.daemon_version {
        out.push_str(&format!(
            "Daemon: {version} (integrity {:?}, uptime {:?} seconds)\n",
            input.daemon_integrity, input.daemon_uptime
        ));
    } else {
        out.push_str(&format!(
            "Daemon unavailable: {}\n",
            input.unavailable_reason
        ));
    }
    out.push_str("\n## Monitors\n");
    match &input.monitors {
        Some(monitors) => {
            for m in monitors {
                out.push_str(&format!(
                    "{}: {},{} {}x{} scale {} (DPI {})\n",
                    m.device,
                    m.x,
                    m.y,
                    m.width,
                    m.height,
                    m.scale,
                    m.scale * 96.0
                ));
            }
        }
        None => out.push_str(&format!("unavailable: {}\n", input.unavailable_reason)),
    }
    out.push_str("\n## Health\n");
    match &input.health {
        None => out.push_str(&format!("unavailable: {}\n", input.unavailable_reason)),
        Some(rows) if rows.is_empty() => out.push_str("No problems found\n"),
        Some(rows) => {
            for row in rows {
                out.push_str(&format!("- {}\n", row.summary));
                if let Some(hint) = &row.hint {
                    out.push_str(&format!("  {hint}\n"));
                }
            }
        }
    }
    out.push_str("\n## Config\n");
    out.push_str(&format!("Source: {}\n", input.config_path.display()));
    match &input.config_status {
        ConfigLoadStatus::Loaded => out.push_str("Source status: readable\n"),
        ConfigLoadStatus::NotFound => out.push_str("Source status: not found\n"),
        ConfigLoadStatus::ReadFailed(error) => {
            out.push_str(&format!("Source status: read failed: {error}\n"))
        }
    }
    if let Some(text) = &input.config_text {
        out.push_str(&masked_config(text));
        out.push('\n');
    } else {
        out.push_str("Config text unavailable\n");
    }
    out.push_str("\n## Workspaces\n");
    match &input.workspaces {
        None => out.push_str(&format!("unavailable: {}\n", input.unavailable_reason)),
        Some(workspaces) => {
            for ws in workspaces {
                out.push_str(&format!(
                    "{} workspace {}: floating {}\n",
                    ws.monitor,
                    ws.index + 1,
                    ws.floating
                ));
                for (index, row) in ws.rows.iter().enumerate() {
                    out.push_str(&format!("  row {}:", index + 1));
                    for (width, count) in row {
                        out.push_str(&format!(" [width {width}, windows {count}]"));
                    }
                    out.push('\n');
                }
            }
        }
    }
    for (heading, name, tail, stitch) in [
        ("Daemon Log", "leopardwm-daemon.log", Some(200), true),
        (
            "Daemon Error Log",
            "leopardwm-daemon.err.log",
            Some(200),
            false,
        ),
        ("Watchdog Log", "leopardwm-watchdog.log", Some(200), true),
        (
            "Watchdog Error Log",
            "leopardwm-watchdog.err.log",
            Some(200),
            false,
        ),
        (
            "Gesture Capture",
            crate::GESTURE_CAPTURE_LOG_FILE,
            None,
            false,
        ),
    ] {
        out.push_str(&format!("\n## {heading}\n"));
        out.push_str(&file_section(&input.log_dir.join(name), tail, stitch));
    }
    redact(&out, &input.known_titles, input.userprofile.as_deref())
}

pub fn write(input: &SnapshotInput, output_dir: &Path) -> io::Result<PathBuf> {
    let timestamp: time::OffsetDateTime = input.timestamp.into();
    let name = format!(
        "leopardwm-snapshot-{:04}{:02}{:02}-{:02}{:02}{:02}",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    );
    let contents = render(input);
    fs::create_dir_all(output_dir)?;
    for suffix in 0..1000 {
        let filename = if suffix == 0 {
            format!("{name}.txt")
        } else {
            format!("{name}-{suffix}.txt")
        };
        let path = output_dir.join(filename);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(contents.as_bytes())?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many snapshots created in one second",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, SnapshotInput) {
        let dir = std::env::temp_dir().join(format!(
            "lwm-snapshot-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let input = SnapshotInput {
            requester: "CLI".into(),
            requester_version: "1".into(),
            requester_integrity: None,
            daemon_version: None,
            daemon_integrity: None,
            daemon_uptime: None,
            unavailable_reason: "daemon not running".into(),
            windows_version: "Windows".into(),
            monitors: None,
            health: None,
            config_path: dir.join("config.toml"),
            config_status: ConfigLoadStatus::Loaded,
            config_text: Some("[behavior]\ngap = 7\n".into()),
            workspaces: None,
            known_titles: Vec::new(),
            userprofile: None,
            log_dir: dir.clone(),
            timestamp: SystemTime::now(),
        };
        (dir, input)
    }

    #[test]
    fn written_snapshot_redacts_known_and_structured_titles_and_paths() {
        let (dir, mut input) = fixture();
        let quoted = "Report \\\"private\\\" \\\\ notes";
        input.userprofile = Some(r"D:\Profiles\PrivateHome".into());
        input.known_titles = vec![
            "Secret".into(),
            "Secret longer".into(),
            quoted.into(),
            "Users".into(),
            "title".into(),
            "Cannot".into(),
        ];
        input.config_text = Some("[[window_rules]]\nmatch_title = 'hidden-rule-title'\nmatch_class = 'hidden-rule-class'\naction = 'float'\n".into());
        let logs = format!("Secret longer\nSecret\ntitle=\"Secret longer\"\n{quoted}\n{quoted:?}\ntitle=\"closed confidential\"\ntitle=Some(\"closed optional\")\ntitle: \"closed struct\"\nCannot tile window 'closed blocked'\nC:\\Users\\Alice\\Documents\\note.txt\nC:\\\\Users\\\\Alice\\\\docs\\\\note.txt\nC:/Users/Alice/docs/note.txt\nC:\\Program Files\\Vendor\\ExampleApp.exe\nD:/Program Files/HiddenApp/Forward.EXE\n\\\\PrivateServer\\PrivateShare\\Network.exe\nD:\\Profiles\\PrivateHome\\notes.txt\n");
        fs::write(dir.join("leopardwm-daemon.log"), logs).unwrap();
        let path = write(&input, &dir).unwrap();
        let result = fs::read_to_string(path).unwrap();
        for secret in [
            "Secret longer",
            "Secret",
            quoted,
            "closed confidential",
            "closed optional",
            "closed struct",
            "closed blocked",
            "Alice",
            "hidden-rule-title",
            "hidden-rule-class",
            "Program Files",
            "Vendor",
            "HiddenApp",
            "PrivateServer",
            "PrivateShare",
            "PrivateHome",
        ] {
            assert!(
                !result.contains(secret),
                "secret survived: {secret}: {result}"
            );
        }
        assert_eq!(result.matches("<title#2>").count(), 2);
        assert!(result.contains("<title#3>"));
        assert_eq!(result.matches("<title#4>").count(), 2);
        assert!(!result.contains(&format!("{quoted:?}")));
        assert!(result.contains("%USERPROFILE%"));
        assert!(result.contains("ExampleApp.exe"));
        assert!(result.contains("Forward.EXE"));
        assert!(result.contains("Network.exe"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn config_masks_rules_and_omits_invalid_input() {
        let (dir, mut input) = fixture();
        input.config_text = Some("[[window_rules]]\nmatch_title = 'private title'\nmatch_class = 'private class'\naction = 'float'\n[behavior]\ngap = 7\n".into());
        let report = render(&input);
        assert!(!report.contains("private title"));
        assert!(!report.contains("private class"));
        assert!(report.contains("<masked>"), "{report}");
        assert!(report.contains("gap = 7"), "{report}");
        input.config_text = Some("[[window_rules]]\nmatch_title = '''\nmultiline secret\n".into());
        let report = render(&input);
        assert!(!report.contains("multiline secret"));
        assert!(report.contains("config text omitted: TOML parse error at line"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_sections_are_identical_when_daemon_unavailable() {
        let (dir, mut input) = fixture();
        let local = render(&input);
        input.daemon_version = Some("1".into());
        input.monitors = Some(vec![]);
        input.health = Some(vec![]);
        input.workspaces = Some(vec![]);
        let daemon = render(&input);
        let headings = |text: &str| {
            text.lines()
                .filter(|line| line.starts_with("## "))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(headings(&local), headings(&daemon));
        assert_eq!(headings(&local).len(), 10);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotated_logs_are_stitched_and_gesture_capture_is_complete() {
        let (dir, input) = fixture();
        for name in ["daemon", "watchdog"] {
            fs::write(
                dir.join(format!("leopardwm-{name}.old.log")),
                (0..250)
                    .map(|n| format!("{name}-old-{n}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            fs::write(
                dir.join(format!("leopardwm-{name}.log")),
                format!("{name}-current\n"),
            )
            .unwrap();
        }
        fs::write(
            dir.join(crate::GESTURE_CAPTURE_LOG_FILE),
            (0..250)
                .map(|n| format!("capture-{n}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let report = render(&input);
        for name in ["daemon", "watchdog"] {
            assert!(!report.contains(&format!("{name}-old-50\n")));
            assert!(report.contains(&format!("{name}-old-51\n")));
            assert!(
                report.find(&format!("{name}-old-249\n")).unwrap()
                    < report.find(&format!("{name}-current\n")).unwrap()
            );
        }
        assert!(report.contains("capture-0\n"));
        assert!(report.contains("capture-249\n"));
        fs::remove_dir_all(dir).unwrap();
    }
}
