use crate::log_text::{self, Kind};
use regex::Regex;
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
                    if matches!(
                        key.as_str(),
                        "match_title" | "match_class" | "match_executable"
                    ) {
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

fn file_section(path: &Path, tail: Option<usize>, stitch: bool, redactor: &Redactor<'_>) -> String {
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
        // Gesture capture contains only typed gesture data and canonical command names.
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
    let mut lines = redactor.records(std::iter::once(current.as_str()));
    if stitch && lines.len() < line_limit {
        if let Ok((previous, _)) =
            read_tail(&crate::bounded_log::previous_log_path(path), LOG_TAIL_BYTES)
        {
            lines = redactor.records([previous.as_str(), current.as_str()]);
        }
    }
    let start = lines.len().saturating_sub(line_limit);
    format!("{}\n", lines[start..].join("\n"))
}

static PROFILE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)[a-z]:[\\/]+users[\\/]+[^\\/"<>|?*:\x00-\x1f]+"#).unwrap());

struct Redactor<'a> {
    titles: Vec<&'a str>,
    profile: Option<Regex>,
}

impl<'a> Redactor<'a> {
    fn new(titles: &'a [String], profile: Option<&str>) -> Self {
        let mut titles: Vec<_> = titles.iter().map(String::as_str).collect();
        titles.sort_unstable();
        titles.dedup();
        let profile = profile.filter(|p| !p.is_empty()).and_then(|p| {
            let pattern = p
                .split(['\\', '/'])
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join(r"[\\/]+");
            Regex::new(&format!("(?i){pattern}")).ok()
        });
        Self { titles, profile }
    }

    fn mask_profile(&self, value: &str) -> String {
        let masked = PROFILE_PREFIX.replace_all(value, "%USERPROFILE%");
        match &self.profile {
            Some(profile) => profile.replace_all(&masked, "%USERPROFILE%").into_owned(),
            None => masked.into_owned(),
        }
    }

    fn path(&self, value: &str) -> String {
        const PIPE: &str = r"\\.\pipe\leopardwm_";
        let normalized = value.replace('/', "\\");
        if normalized
            .get(..PIPE.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PIPE))
        {
            return format!(r"{PIPE}<pipe-scope>");
        }
        let basename = value.rsplit(['\\', '/']).next().unwrap_or_default();
        if basename
            .as_bytes()
            .last_chunk::<4>()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(b".exe"))
        {
            return log_text::single_line(basename).to_string();
        }
        // Preserve Windows separators while keeping the rendered value on one line.
        let masked = self.mask_profile(value);
        let masked = log_text::single_line(&masked).to_string();
        masked
    }

    fn spans(&self, mut line: &str) -> Option<String> {
        let mut output = String::new();
        while let Some((before, span)) = log_text::before_span(line) {
            output.push_str(&self.mask_profile(before));
            let (kind, encoded, decoded, rest) = log_text::split_span(span)?;
            match kind {
                Kind::Title => match self.titles.binary_search(&decoded.as_str()) {
                    Ok(index) => output.push_str(&format!("<title#{}>", index + 1)),
                    Err(_) => output.push_str("<title>"),
                },
                Kind::Path => output.push_str(&self.path(&decoded)),
                Kind::Text => output.push_str("<redacted>"),
                Kind::Name => output.push_str(&self.mask_profile(encoded)),
            }
            line = rest;
        }
        output.push_str(&self.mask_profile(line));
        Some(output)
    }

    fn records<'b>(&self, files: impl IntoIterator<Item = &'b str>) -> Vec<String> {
        fn flush(lines: &mut Vec<String>, omitted: &mut usize) {
            if *omitted != 0 {
                lines.push(format!(
                    "<{} records omitted (older format, malformed, or incomplete)>",
                    std::mem::take(omitted)
                ));
            }
        }
        let mut lines = Vec::new();
        let mut omitted = 0;
        // This is a format marker, not authentication. A crafted old title can
        // forge a tagged-looking line of its own text; later old records still drop.
        for record in files
            .into_iter()
            .flat_map(|file| file.split_inclusive('\n'))
        {
            let body = record
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .and_then(|line| line.strip_prefix(log_text::RECORD_TAG))
                .and_then(|line| self.spans(line));
            match body {
                Some(line) => {
                    flush(&mut lines, &mut omitted);
                    lines.push(line);
                }
                None => omitted += 1,
            }
        }
        flush(&mut lines, &mut omitted);
        lines
    }
}

pub fn render(input: &SnapshotInput) -> String {
    let redactor = Redactor::new(&input.known_titles, input.userprofile.as_deref());
    let requester_version = semver::Version::parse(&input.requester_version)
        .map_or_else(|_| "<redacted>".into(), |version| version.to_string());
    let mut sections = Vec::new();
    let mut out = String::new();
    out.push_str(&format!(
        "Requester: {} {} (integrity {:?})\nWindows: {}\n",
        input.requester, requester_version, input.requester_integrity, input.windows_version
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
                    log_text::single_line(&m.device),
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
                let summary = redactor.spans(&row.summary);
                out.push_str(&format!(
                    "- {}\n",
                    summary.as_deref().unwrap_or("<redacted>")
                ));
                if let Some(hint) = &row.hint {
                    let hint = redactor.spans(hint);
                    out.push_str(&format!("  {}\n", hint.as_deref().unwrap_or("<redacted>")));
                }
            }
        }
    }
    sections.push(("Health", std::mem::take(&mut out)));
    out.push_str(&format!(
        "Source: {}\n",
        redactor.path(&input.config_path.to_string_lossy())
    ));
    match &input.config_status {
        ConfigLoadStatus::Loaded => out.push_str("Source status: readable\n"),
        ConfigLoadStatus::NotFound => out.push_str("Source status: not found\n"),
        ConfigLoadStatus::ReadFailed(_) => out.push_str("Source status: read failed\n"),
    }
    if let Some(text) = &input.config_text {
        out.push_str(&redactor.mask_profile(&masked_config(text)));
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
                    log_text::single_line(&ws.monitor),
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
            file_section(&input.log_dir.join(name), tail, stitch, &redactor),
        ));
    }
    let mut report = String::new();
    for (heading, body) in sections {
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&format!("## {heading}\n"));
        report.push_str(&body);
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
            requester_version: "1.0.0".into(),
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

    fn file_tracing(path: &Path, log: impl FnOnce()) {
        use tracing_subscriber::prelude::*;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let subscriber = tracing_subscriber::registry().with(log_text::file_layer(
            std::sync::Mutex::new(file),
            tracing_subscriber::filter::LevelFilter::TRACE,
        ));
        tracing::subscriber::with_default(subscriber, log);
    }

    #[test]
    fn explicit_spans_preserve_only_the_allowed_parts() {
        let (dir, mut input) = fixture();
        input.known_titles = vec!["x title=y".into()];
        input.userprofile = Some(r"D:\Profiles\PrivateHome".into());
        input.health = Some(vec![HealthRow {
            summary: format!(
                "Auto-start enabled (path: {})",
                log_text::path(r"D:\Private\App.exe")
            ),
            hint: None,
        }]);
        file_tracing(&dir.join("leopardwm-daemon.log"), || {
            tracing::info!(
                target: "leopardwm::window_rules",
                title = %log_text::title("x title=y"),
                class = %log_text::name("ExampleClass"),
                "Window created"
            );
            tracing::info!(
                target: "leopardwm::window_rules",
                "Profile-shaped name: {}",
                log_text::name(r"C:\Users\NameSpanCanary\x")
            );
            tracing::info!(
                target: "leopardwm_platform_win32::shell",
                "Sources: {} | {} | {} | {} | fallback C:/Users/Unmarked/notes.txt",
                log_text::path(r"d:/profiles/PrivateHome/first.toml"),
                log_text::path(r"C:\Users\O'Connor\second.toml"),
                log_text::path(r"\\PrivateServer\PrivateShare\Network.EXE"),
                log_text::path(crate::scoped_pipe_name_for_user(
                    r"DOMAIN\PrivateAccount.exe"
                )),
            );
        });
        let report = render(&input);
        assert!(
            report.contains("Auto-start enabled (path: App.exe)"),
            "{report}"
        );
        assert!(
            report.contains("Window created title=<title#1> class=ExampleClass"),
            "{report}"
        );
        assert!(report.contains(r"%USERPROFILE%/first.toml | %USERPROFILE%\second.toml | Network.EXE | \\.\pipe\leopardwm_<pipe-scope> | fallback %USERPROFILE%/notes.txt"), "{report}");
        assert!(
            report.contains(r"Profile-shaped name: %USERPROFILE%\\x"),
            "{report}"
        );
        for secret in [
            "NameSpanCanary",
            r"D:\Private",
            "x title=y",
            "PrivateHome",
            "O'Connor",
            "PrivateServer",
            "PrivateShare",
            "privateaccount",
            "Unmarked",
        ] {
            assert!(!report.contains(secret), "{secret} survived: {report}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formatter_keeps_multiline_titles_in_one_record_and_drops_legacy_after_tag() {
        let (dir, mut input) = fixture();
        let title = "a\n2026-11-22T00:00:00.000000Z  INFO x: secret";
        input.known_titles = vec![title.into()];
        let log = dir.join("leopardwm-daemon.log");
        file_tracing(
            &log,
            || tracing::info!(target: "leopardwm::event_handler", "Observed {}", log_text::title(title)),
        );
        let first = fs::read_to_string(&log).unwrap();
        assert!(first.starts_with("[lwm-log-v1] "), "{first}");
        assert_eq!(first.lines().count(), 1, "{first}");
        OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(
                b"2026-11-22T00:00:00Z INFO private legacy title\nprivate legacy continuation\n",
            )
            .unwrap();
        file_tracing(
            &log,
            || tracing::info!(target: "leopardwm::event_handler", "After downgrade"),
        );
        let report = render(&input);
        assert!(!report.contains("secret"));
        assert!(!report.contains("private legacy"));
        let first = report.find("Observed <title#1>").unwrap();
        let omitted = report.find("<2 records omitted").unwrap();
        let last = report.find("After downgrade").unwrap();
        assert!(first < omitted && omitted < last, "{report}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_and_incomplete_records_are_omitted_whole() {
        let (dir, input) = fixture();
        let log = dir.join("leopardwm-daemon.log");
        fs::write(
            &log,
            concat!(
                "[lwm-log-v1] INFO private-prefix @lwm:title\"bad\\q\"\n",
                "[lwm-log-v1] INFO private-prefix @lwm:unknown\"bad\"\n",
                "[lwm-log-v1] INFO private-prefix @lwm:text\"unfinished\n",
                "[lwm-log-v1] INFO safe neighboring record\n",
                "[lwm-log-v1] INFO private-prefix @lwm:name\"truncated record\"",
            ),
        )
        .unwrap();
        let report = render(&input);
        assert!(!report.contains("private-prefix"), "{report}");
        assert!(report.contains("<3 records omitted"), "{report}");
        assert!(
            report.contains("safe neighboring record\n<1 records omitted"),
            "{report}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn final_snapshot_excludes_distinct_private_inputs_from_every_source() {
        let (dir, mut input) = fixture();
        input.known_titles = vec!["canary-known-title".into()];
        input.requester_version = "canary-requester-version".into();
        input.config_path = PathBuf::from(r"C:\Users\canary-source\config.toml");
        input.config_text = Some(
            concat!(
                "[[window_rules]]\n",
                "match_title = 'canary-title-rule'\n",
                "match_class = 'canary-class-rule'\n",
                "match_executable = 'canary-exe-rule'\n",
                "action = 'float'\n",
            )
            .into(),
        );
        input.health = Some(vec![HealthRow {
            summary: format!("Worker error: {}", log_text::text("canary-health")),
            hint: Some(format!(
                "Inspect {}",
                log_text::path(r"D:\canary-health-path\Helper.exe")
            )),
        }]);
        let daemon = dir.join("leopardwm-daemon.log");
        fs::write(&daemon, "2026-11-22T00:00:00Z WARN canary-legacy\n").unwrap();
        file_tracing(&daemon, || {
            tracing::info!(
                target: "leopardwm::layout_apply",
                "Real layout event: {} {}",
                log_text::title("canary-known-title"),
                log_text::title("canary-unknown-title")
            );
            tracing::warn!(target: "leopardwm::config", error = %log_text::text("canary-traced-error"), "Config rejected");
            tracing::debug!(target: "webview_dependency", "Raw error: canary-dependency");
        });
        let location = std::panic::Location::caller();
        let panic = log_text::panic_record(
            Some(location),
            "canary-panic\n[lwm-log-v1] canary-panic-forged",
        );
        fs::write(
            dir.join("leopardwm-daemon.err.log"),
            format!("canary-stderr\n{panic}"),
        )
        .unwrap();
        file_tracing(&dir.join("leopardwm-watchdog.log"), || {
            tracing::info!(target: "leopardwm_watchdog", path = %log_text::path(r"D:\canary-watchdog\leopardwm.exe"), "Resolved daemon binary");
        });
        fs::write(
            dir.join("leopardwm-watchdog.err.log"),
            log_text::panic_record(None, "canary-watchdog-error"),
        )
        .unwrap();
        let final_path = write(&input, &dir).unwrap();
        let report = fs::read_to_string(final_path).unwrap();
        assert!(!report.contains("canary-"), "{report}");
        assert!(
            report.contains("Real layout event: <title#1> <title>"),
            "{report}"
        );
        assert!(
            report.contains(r"Source: %USERPROFILE%\config.toml"),
            "{report}"
        );
        assert!(
            report.contains(&format!(
                ":{}:{}: <redacted>",
                location.line(),
                location.column()
            )),
            "{report}"
        );
        assert!(
            report.contains("Resolved daemon binary path=leopardwm.exe"),
            "{report}"
        );
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
        assert!(local.contains("Requester: CLI 1.0.0"));
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
    fn oversized_files_include_only_bounded_complete_tail_lines() {
        let (dir, input) = fixture();
        for name in [
            "leopardwm-daemon.log",
            "leopardwm-daemon.err.log",
            "leopardwm-watchdog.log",
            "leopardwm-watchdog.err.log",
        ] {
            let content = format!(
                "old-secret\n{}\n[lwm-log-v1] 2026-01-01T00:00:01Z INFO new-tail\n",
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
                    .map(|n| format!("[lwm-log-v1] 2026-01-01T00:00:00Z INFO {name}-old-{n}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            fs::write(
                dir.join(format!("leopardwm-{name}.log")),
                format!("[lwm-log-v1] 2026-01-01T00:00:01Z INFO {name}-current\n"),
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
