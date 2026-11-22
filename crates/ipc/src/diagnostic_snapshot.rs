use regex::{Captures, Regex};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
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

const LOG_TAIL_BYTES: u64 = 256 * 1024;
const CAPTURE_TAIL_BYTES: u64 = 1024 * 1024;

fn read_tail(path: &Path, limit: u64) -> io::Result<(String, bool)> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit);
    let mut partial = false;
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        partial = previous[0] != b'\n';
    }
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    if partial {
        let first = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        bytes.drain(..first);
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), start > 0))
}

fn complete_log_entries(text: String, truncated: bool) -> String {
    if !truncated {
        return text;
    }
    // A bounded read may begin within a multiline diagnostic, whose first line
    // carries the label needed to redact its remaining lines.
    static ENTRY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\d{4}-\d{2}-\d{2}T\S+ +(?:TRACE|DEBUG|INFO|WARN|ERROR) ").unwrap()
    });
    match ENTRY.find(&text) {
        Some(found) => text[found.start()..].to_string(),
        None => String::new(),
    }
}

fn file_section(path: &Path, tail: Option<usize>, stitch: bool) -> String {
    let limit = if tail.is_some() {
        LOG_TAIL_BYTES
    } else {
        CAPTURE_TAIL_BYTES
    };
    let (current, truncated) = match read_tail(path, limit) {
        Ok(result) => result,
        Err(error) => return format!("unavailable: {error}\n"),
    };
    let Some(line_limit) = tail else {
        let note = if truncated {
            "(capture truncated to its last 1 MiB)\n"
        } else {
            ""
        };
        return if current.is_empty() {
            format!("{note}(empty)\n")
        } else {
            format!("{note}{current}\n")
        };
    };
    let current = if stitch {
        complete_log_entries(current, truncated)
    } else {
        current
    };
    let current_lines: Vec<_> = current.lines().collect();
    let mut lines = Vec::new();
    let previous = if stitch && current_lines.len() < line_limit {
        read_tail(&crate::bounded_log::previous_log_path(path), LOG_TAIL_BYTES)
            .ok()
            .map(|(text, truncated)| complete_log_entries(text, truncated))
    } else {
        None
    };
    if let Some(previous) = &previous {
        lines.extend(previous.lines());
    }
    lines.extend(current_lines);
    let start = lines.len().saturating_sub(line_limit);
    format!("{}\n", lines[start..].join("\n"))
}

fn title_forms(titles: &[String], userprofile: Option<&str>) -> Vec<(String, String)> {
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
        let debug = format!("{title:?}");
        let escaped = &debug[1..debug.len() - 1];
        for raw in [*title, escaped] {
            forms.push((raw.to_string(), marker.clone()));
            let prepared = redact_structured(&redact_paths(raw, userprofile), titles);
            if !prepared.is_empty() {
                forms.push((prepared, marker.clone()));
            }
        }
    }
    forms.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    forms.dedup();
    forms
}

fn replace_known(text: &str, forms: &[(String, String)]) -> String {
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
    static EXECUTABLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(?:[a-z]:[\\/]+|[\\/]{2,}[^\\/\s"]+[\\/]+)[^"\r\n]*?[^\\/\s"]+\.exe\b"#)
            .unwrap()
    });
    let mut text = EXECUTABLE
        .replace_all(text, |c: &Captures<'_>| {
            c[0].rsplit(['\\', '/']).next().unwrap_or("").to_string()
        })
        .into_owned();
    if let Some(profile) = userprofile.filter(|p| !p.is_empty()) {
        text = text.replace(profile, "%USERPROFILE%");
        text = text.replace(&profile.replace('\\', "\\\\"), "%USERPROFILE%");
        text = text.replace(&profile.replace('\\', "/"), "%USERPROFILE%");
    }
    static PROFILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)[a-z]:[\\/]+users[\\/]+[^\\/"\r\n]+"#).unwrap());
    text = PROFILE.replace_all(&text, "%USERPROFILE%").into_owned();
    static PIPE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)([\\/]{2,}\.[\\/]+pipe[\\/]+leopardwm)_[a-z0-9_.-]+").unwrap()
    });
    PIPE.replace_all(&text, "${1}_<pipe-scope>").into_owned()
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
    static INVALID_RULE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?s)Invalid regex in window rule match_(?:class|title) .*?(?:\. Skipping rule\.\r?(?m:$)|\z)",
        )
        .unwrap()
    });
    let mut text = INVALID_RULE
        .replace_all(
            text,
            "Invalid regex in window rule: <masked>. Skipping rule.",
        )
        .into_owned();
    // These are the legacy title-bearing log messages; older daemon logs remain in captures.
    static PLAIN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)((?:Window created|Ignoring shell-cloaked window|Ignoring window by rule|Recovering suppressed window|Skipping transient console-host window with exe-path title|Recovering console-host window with real title|Added floating window|Added tiled window|Floating fixed-size caption window): |Leaving [^\r\n]*? window unmanaged: )([^\r\n]*)( \([^\r\n]*\))").unwrap()
    });
    text = PLAIN
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    static OVERRIDE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(Pruning stale tab title override for dead HWND \d+: \")((?:\\.|[^"\\])*)(\")"#,
        )
        .unwrap()
    });
    text = OVERRIDE_TITLE
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    static STRUCTURED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(title\s*[:=]\s*(?:Some\()?\")((?:\\.|[^"\\])*)(\")"#).unwrap()
    });
    text = STRUCTURED
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    static CANNOT_TILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)(Cannot tile window ')([^\r\n]*)('(?: \(|$))").unwrap());
    text = CANNOT_TILE
        .replace_all(&text, |c: &Captures<'_>| {
            if known(&c[2]) {
                c[0].to_string()
            } else {
                format!("{}<title>{}", &c[1], &c[3])
            }
        })
        .into_owned();
    static PLAIN_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:Window created|Ignoring shell-cloaked window|Ignoring window by rule|Recovering suppressed window|Skipping transient console-host window with exe-path title|Recovering console-host window with real title|Added floating window|Added tiled window|Floating fixed-size caption window): |Leaving [^\r\n]*? window unmanaged: ").unwrap()
    });
    let mut offset = 0;
    let mut truncate_at = None;
    for line in text.split_inclusive('\n') {
        let incomplete = PLAIN_PREFIX
            .find(line)
            .filter(|found| !PLAIN.is_match(&line[found.start()..]))
            .map(|found| found.end())
            .or_else(|| {
                line.find("Cannot tile window '")
                    .filter(|start| !CANNOT_TILE.is_match(&line[*start..]))
                    .map(|start| start + "Cannot tile window '".len())
            });
        if let Some(end) = incomplete {
            truncate_at = Some(offset + end);
            break;
        }
        offset += line.len();
    }
    if let Some(index) = truncate_at {
        text.truncate(index);
        text.push_str("<title> (remaining output omitted after incomplete title)\n");
    }
    text
}

/// Redact known titles and supported structured forms. Initial passes prevent
/// known titles like "Users" or "title" from breaking later recognition.
/// Unstructured titles from closed windows cannot be identified reliably.
pub fn redact(text: &str, titles: &[String], userprofile: Option<&str>) -> String {
    let forms = title_forms(titles, userprofile);
    let known: Vec<String> = forms.iter().map(|(form, _)| form.clone()).collect();
    redact_body(text, &forms, &known, userprofile)
}

fn redact_body(
    text: &str,
    forms: &[(String, String)],
    known: &[String],
    userprofile: Option<&str>,
) -> String {
    let text = redact_structured(text, known);
    let text = redact_paths(&text, userprofile);
    let text = redact_structured(&text, known);
    let text = replace_known(&text, forms);
    let text = redact_structured(&text, &[]);
    redact_paths(&text, userprofile)
}

pub fn render(input: &SnapshotInput) -> String {
    let mut sections = Vec::new();
    let mut out = String::new();
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
    sections.push(("Environment", std::mem::take(&mut out)));
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
    sections.push(("Monitors", std::mem::take(&mut out)));
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
    sections.push(("Health", std::mem::take(&mut out)));
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
    sections.push(("Config", std::mem::take(&mut out)));
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
    sections.push(("Workspaces", std::mem::take(&mut out)));
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
        sections.push((
            heading,
            file_section(&input.log_dir.join(name), tail, stitch),
        ));
    }
    let forms = title_forms(&input.known_titles, input.userprofile.as_deref());
    let known: Vec<String> = forms.iter().map(|(form, _)| form.clone()).collect();
    let mut report = String::new();
    for (heading, body) in sections {
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&format!("## {heading}\n"));
        report.push_str(&redact_body(
            &body,
            &forms,
            &known,
            input.userprofile.as_deref(),
        ));
    }
    report
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
        input.known_titles = vec!["Config".into(), "## Health".into()];
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
    fn unknown_titles_are_removed_from_existing_log_formats() {
        let lines = [
            "Cannot tile window 'Private Window' (AppClass): Protected — leaving it floating and ignoring it for this session.",
            "Ignoring shell-cloaked window: Private Window (AppClass) current desktop: true",
            "Skipping transient console-host window with exe-path title: Private Window (AppClass)",
            "Ignoring window by rule: Private Window (AppClass)",
            "Leaving dialog-like window unmanaged: Private Window (AppClass)",
            "Window created: Private Window (AppClass) - added to monitor 1 workspace 2 as Tile",
            "Window created: Private Window (inner) (App(Class)) - added to monitor 1 workspace 2 as Tile",
            "Window created: Private Window\nmore secret (AppClass) - added to monitor 1 workspace 2 as Tile",
            "Cannot tile window 'Private Window\nmore secret' (AppClass): Protected",
            "Recovering suppressed window: Private Window (AppClass) - user focused it",
            "Recovering console-host window with real title: Private Window (AppClass) - user focused it",
            "Added floating window: Private Window (AppClass) to monitor 1 - 800x600",
            "Added tiled window: Private Window (AppClass) to monitor 1 - 800x600",
            "Floating fixed-size caption window: Private Window (AppClass)",
            "Pruning stale tab title override for dead HWND 42: \"Private Window\"",
            "candidate: title=\"Private Window\"",
            "candidate: title=Some(\"Private Window\")",
            "WindowInfo { title: \"Private Window\" }",
        ];
        for line in lines {
            let output = redact(line, &[], None);
            assert!(!output.contains("Private Window"), "{line} => {output}");
            assert!(!output.contains("more secret"), "{line} => {output}");
        }
    }

    #[test]
    fn path_bearing_known_titles_keep_their_number() {
        let titles = vec![
            r"Notes - C:\Users\Alice\Tax Return.pdf - Notepad".to_string(),
            r"Launch D:\Apps\Secret Tool.exe - Admin".to_string(),
        ];
        for (index, title) in titles.iter().enumerate() {
            let output = redact(
                &format!(
                    "{title}\n{title:?}\nWindow created: {title} (AppClass) - added to monitor 1"
                ),
                &titles,
                None,
            );
            let marker = if index == 0 { "<title#2>" } else { "<title#1>" };
            assert_eq!(output.matches(marker).count(), 3, "{output}");
            assert!(!output.contains("Tax Return.pdf"));
            assert!(!output.contains("Secret Tool"));
        }
    }

    #[test]
    fn old_invalid_rule_warnings_never_expose_pattern_or_diagnostic() {
        for field in ["match_class", "match_title"] {
            let line = format!("Invalid regex in window rule {field} 'PrivatePattern(': regex parse error:\n PrivatePattern(\n ^\nerror: unclosed group. Skipping rule.");
            let output = redact(&line, &[], None);
            assert!(!output.contains("PrivatePattern"), "{output}");
            let partial = format!("Invalid regex in window rule {field} 'PrivatePattern(");
            let output = redact(&partial, &[], None);
            assert!(!output.contains("PrivatePattern"), "{output}");
            let embedded = format!("Invalid regex in window rule {field} 'PrivatePattern. Skipping rule.(': regex parse error: PrivatePattern. Skipping rule.()\nerror: invalid. Skipping rule.\nnext entry");
            let output = redact(&embedded, &[], None);
            assert!(!output.contains("PrivatePattern"), "{output}");
            assert!(output.contains("next entry"), "{output}");
        }
    }

    #[test]
    fn apostrophes_in_paths_are_redacted_in_every_separator_form() {
        for (exe, profile) in [
            (r"D:\O'Connor\lwm.exe", r"C:\Users\O'Connor\notes.txt"),
            (r"D:\\O'Connor\\lwm.exe", r"C:\\Users\\O'Connor\\notes.txt"),
            (r"D:/O'Connor/lwm.exe", r"C:/Users/O'Connor/notes.txt"),
        ] {
            let output = redact(
                &format!("{exe}\n{profile}"),
                &[],
                Some(r"C:\Users\O'CONNOR"),
            );
            assert!(!output.contains("O'Connor"), "{output}");
            assert!(output.contains("lwm.exe"), "{output}");
            assert!(output.contains("%USERPROFILE%"), "{output}");
        }
    }

    #[test]
    fn scoped_pipe_names_do_not_expose_account_names() {
        for pipe in [
            r"\\.\pipe\leopardwm_domain_user",
            r"\\\\.\\pipe\\leopardwm_domain_user",
        ] {
            let output = redact(&format!("IPC server listening on {pipe}"), &[], None);
            assert!(!output.contains("domain_user"), "{output}");
            assert!(output.contains("<pipe-scope>"), "{output}");
        }
    }

    #[test]
    fn truncated_log_never_starts_inside_a_multiline_rule_warning() {
        let (dir, input) = fixture();
        let mut content = String::from("2026-01-01T00:00:00Z WARN Invalid regex in window rule match_title 'PrivatePattern(': regex parse error:\n");
        for _ in 0..30000 {
            content.push_str("PrivatePattern\n");
        }
        content.push_str(
            "error: unclosed group. Skipping rule.\n2026-01-01T00:00:01Z INFO safe entry\n",
        );
        fs::write(dir.join("leopardwm-daemon.log"), content).unwrap();
        let report = render(&input);
        assert!(
            !report.contains("PrivatePattern"),
            "truncated warning leaked"
        );
        assert!(report.contains("safe entry"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_files_include_only_bounded_complete_tail_lines() {
        let (dir, input) = fixture();
        for name in [
            "leopardwm-daemon.log",
            "leopardwm-daemon.err.log",
            "leopardwm-watchdog.log",
            "leopardwm-watchdog.err.log",
        ] {
            let content = format!(
                "old-secret\n{}\n2026-01-01T00:00:01Z INFO new-tail\n",
                "x".repeat(256 * 1024)
            );
            fs::write(dir.join(name), content).unwrap();
        }
        let capture = format!("capture-old\n{}\ncapture-new\n", "y".repeat(1024 * 1024));
        fs::write(dir.join(crate::GESTURE_CAPTURE_LOG_FILE), capture).unwrap();
        let report = render(&input);
        assert!(!report.contains("old-secret"));
        assert_eq!(report.matches("new-tail").count(), 4);
        assert!(!report.contains("capture-old"));
        assert!(report.contains("capture-new"));
        assert!(report.contains("capture truncated to its last 1 MiB"));
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
