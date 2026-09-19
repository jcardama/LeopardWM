//! Opt-in, bounded touchpad-gesture diagnostic capture.
//!
//! Listens only for the dedicated `leopardwm::gesture_diag` tracing target and
//! writes a separate report file. The low-level hook path uses `try_send` so it
//! never waits on disk. Capture is startup-only and off by default.

use leopardwm_platform_win32::{
    GestureEvent, GESTURE_DIAG_STAGE_ACCUMULATION, GESTURE_DIAG_STAGE_CLASSIFIER,
    GESTURE_DIAG_STAGE_COOLDOWN, GESTURE_DIAG_STAGE_DISPATCH, GESTURE_DIAG_STAGE_HOOK_DELIVERY,
    GESTURE_DIAG_STAGE_RECOGNIZED, GESTURE_DIAG_STAGE_REGISTRATION, GESTURE_DIAG_STAGE_TIMEOUT,
    GESTURE_DIAG_TARGET, INTEGRITY_HIGH, INTEGRITY_MEDIUM,
};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::Layer;

const MAX_FIELD_CHARS: usize = 64;
const MAX_FIELDS: usize = 12;
const SHUTDOWN_JOIN: Duration = Duration::from_millis(400);
const WORKER_POLL: Duration = Duration::from_millis(50);
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_MAX_RECORDS: usize = 4096;
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
const DEFAULT_CHANNEL_CAPACITY: usize = 256;
const FINAL_DRAIN_MAX_RECORDS: usize = DEFAULT_CHANNEL_CAPACITY;

const ALLOWED_FIELDS: &[&str] = &[
    "axis",
    "delta",
    "flags",
    "mods_held",
    "swipe_candidate",
    "outcome",
    "accumulation",
    "event",
    "binding",
    "command",
    "state",
    "timeout",
    "cooldown",
    "mode",
];

const KNOWN_STAGES: &[&str] = &[
    GESTURE_DIAG_STAGE_HOOK_DELIVERY,
    GESTURE_DIAG_STAGE_CLASSIFIER,
    GESTURE_DIAG_STAGE_ACCUMULATION,
    GESTURE_DIAG_STAGE_TIMEOUT,
    GESTURE_DIAG_STAGE_COOLDOWN,
    GESTURE_DIAG_STAGE_RECOGNIZED,
    GESTURE_DIAG_STAGE_DISPATCH,
    GESTURE_DIAG_STAGE_REGISTRATION,
];

/// Header values written at the start of a capture file.
#[derive(Debug, Clone)]
pub struct CaptureHeader {
    pub version: String,
    pub gestures_enabled_config: bool,
    pub capture_limit_secs: u64,
    pub daemon_integrity: String,
}

/// Tunable bounds for a capture run. Production uses the defaults.
#[derive(Debug, Clone)]
pub struct CaptureLimits {
    pub duration: Duration,
    pub max_records: usize,
    pub max_bytes: usize,
    pub channel_capacity: usize,
}

impl CaptureLimits {
    pub fn from_secs(secs: u64) -> Self {
        Self {
            duration: Duration::from_secs(secs),
            max_records: DEFAULT_MAX_RECORDS,
            max_bytes: DEFAULT_MAX_BYTES,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }
}

struct CaptureRecord {
    stage: &'static str,
    line: String,
}

#[derive(Clone)]
struct CaptureState {
    dropped: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    in_flight: Arc<AtomicU64>,
    after_close: Arc<AtomicU64>,
    gate_open: Arc<AtomicBool>,
}

impl CaptureState {
    fn open_gate(&self) {
        if !self.gate_open.swap(true, Ordering::AcqRel) {
            leopardwm_platform_win32::begin_gesture_diagnostic_capture();
        }
    }

    fn close_gate(&self) {
        if self.gate_open.swap(false, Ordering::AcqRel) {
            leopardwm_platform_win32::end_gesture_diagnostic_capture();
        }
    }
}

/// Tracing layer that handoffs allowlisted diagnostic records without blocking.
#[derive(Clone)]
pub struct GestureCaptureLayer {
    tx: SyncSender<CaptureRecord>,
    state: CaptureState,
}

#[derive(Clone)]
pub struct GestureCaptureFilter {
    state: CaptureState,
}

/// Owns the capture worker. Dropping it signals shutdown without waiting out
/// the remaining capture interval.
pub struct GestureCaptureHandle {
    state: CaptureState,
    thread: Option<JoinHandle<()>>,
    layer: GestureCaptureLayer,
}

impl GestureCaptureHandle {
    pub fn layer(&self) -> GestureCaptureLayer {
        self.layer.clone()
    }

    pub fn filter(&self) -> GestureCaptureFilter {
        self.layer.filter()
    }

    #[cfg(test)]
    fn wait_for_deadline(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GestureCaptureHandle {
    fn drop(&mut self) {
        self.state.close_gate();
        self.state.active.store(false, Ordering::Release);
        self.state.shutdown.store(true, Ordering::SeqCst);
        join_short(&mut self.thread, SHUTDOWN_JOIN);
    }
}

impl GestureCaptureLayer {
    pub fn filter(&self) -> GestureCaptureFilter {
        GestureCaptureFilter {
            state: self.state.clone(),
        }
    }
}

impl<S> Filter<S> for GestureCaptureFilter
where
    S: Subscriber,
{
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: &Context<'_, S>) -> bool {
        metadata.target() == GESTURE_DIAG_TARGET && self.state.active.load(Ordering::Acquire)
    }
}

/// Format the observed process integrity the same way doctor does.
pub fn format_integrity(rid: Option<u32>) -> String {
    match rid {
        Some(INTEGRITY_MEDIUM) => "Medium".to_string(),
        Some(INTEGRITY_HIGH) => "High".to_string(),
        Some(rid) => format!("0x{rid:X}"),
        None => "unavailable".to_string(),
    }
}

/// Path of the dedicated capture file in `log_dir`.
pub fn capture_log_path(log_dir: &Path) -> PathBuf {
    log_dir.join(leopardwm_ipc::GESTURE_CAPTURE_LOG_FILE)
}

/// Start a bounded capture worker. Returns `None` when duration is zero, spawn
/// fails, or the channel cannot be created — the window manager continues.
pub fn start_capture(
    log_dir: &Path,
    header: CaptureHeader,
    limits: CaptureLimits,
) -> Option<GestureCaptureHandle> {
    if limits.duration.is_zero() {
        return None;
    }
    let path = capture_log_path(log_dir);
    let capacity = limits.channel_capacity.clamp(1, FINAL_DRAIN_MAX_RECORDS);
    let (tx, rx) = mpsc::sync_channel(capacity);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let state = CaptureState {
        dropped: Arc::new(AtomicU64::new(0)),
        shutdown: Arc::new(AtomicBool::new(false)),
        active: Arc::new(AtomicBool::new(true)),
        in_flight: Arc::new(AtomicU64::new(0)),
        after_close: Arc::new(AtomicU64::new(0)),
        gate_open: Arc::new(AtomicBool::new(false)),
    };
    let worker_state = state.clone();
    let thread = thread::Builder::new()
        .name("gesture-diag-capture".into())
        .spawn(move || {
            let mut file = match File::create(&path) {
                Ok(file) => file,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            if let Err(e) = write_header(&mut file, &header) {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            run_worker(file, limits, rx, worker_state);
        })
        .map_err(|e| {
            eprintln!(
                "[leopardwm] Warning: gesture diagnostic capture thread failed to start: {e}"
            );
            e
        })
        .ok()?;

    match ready_rx.recv_timeout(STARTUP_READY_TIMEOUT) {
        Ok(Ok(())) => {
            state.open_gate();
            Some(GestureCaptureHandle {
                state: state.clone(),
                thread: Some(thread),
                layer: GestureCaptureLayer { tx, state },
            })
        }
        Ok(Err(e)) => {
            eprintln!("[leopardwm] Warning: gesture diagnostic capture could not start: {e}");
            let _ = thread.join();
            None
        }
        Err(e) => {
            state.active.store(false, Ordering::Release);
            state.shutdown.store(true, Ordering::SeqCst);
            eprintln!("[leopardwm] Warning: gesture diagnostic capture did not start: {e}");
            let _ = thread.join();
            None
        }
    }
}

/// Emit a dispatch-stage record using the shared diagnostic target.
pub fn emit_dispatch(event: GestureEvent, binding: &'static str, command: Option<&str>) {
    if !leopardwm_platform_win32::gesture_diagnostic_capture_active() {
        return;
    }
    if let Some(command) = command {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_DISPATCH,
            event = event.as_diag_str(),
            binding,
            command,
        );
    } else {
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_DISPATCH,
            event = event.as_diag_str(),
            binding,
        );
    }
}

impl<S> Layer<S> for GestureCaptureLayer
where
    S: Subscriber,
{
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Target matching is also checked here because tests may install this
        // layer without the production per-layer filter.
        if event.metadata().target() != GESTURE_DIAG_TARGET {
            return;
        }
        if !self.state.active.load(Ordering::Acquire) {
            self.state.after_close.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.state.in_flight.fetch_add(1, Ordering::Relaxed);
        if !self.state.active.load(Ordering::Acquire) {
            self.state.in_flight.fetch_sub(1, Ordering::Relaxed);
            self.state.after_close.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let mut sink = FieldSink::default();
        event.record(&mut sink);
        let Some(record) = sink.into_record() else {
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
            self.state.in_flight.fetch_sub(1, Ordering::Relaxed);
            return;
        };
        match self.tx.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.state.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.state.after_close.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.state.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct FieldSink {
    stage: Option<String>,
    fields: Vec<(String, String)>,
}

impl FieldSink {
    fn push(&mut self, name: &str, value: &str) {
        if name == "message" {
            return;
        }
        let Some(value) = sanitize_token(value) else {
            return;
        };
        if name == "stage" {
            self.stage = Some(value);
            return;
        }
        if !ALLOWED_FIELDS.contains(&name) || self.fields.len() >= MAX_FIELDS {
            return;
        }
        self.fields.push((name.to_string(), value));
    }

    fn into_record(self) -> Option<CaptureRecord> {
        let stage = intern_stage(self.stage.as_deref()?)?;
        let mut line = format!("stage={stage}");
        for (key, value) in self.fields {
            line.push(' ');
            line.push_str(&key);
            line.push('=');
            line.push_str(&value);
        }
        Some(CaptureRecord { stage, line })
    }
}

impl Visit for FieldSink {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field.name(), &format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field.name(), &value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field.name(), &value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field.name(), if value { "true" } else { "false" });
    }

    fn record_error(&mut self, _field: &Field, _value: &(dyn std::error::Error + 'static)) {}
}

fn intern_stage(stage: &str) -> Option<&'static str> {
    KNOWN_STAGES.iter().copied().find(|known| *known == stage)
}

fn sanitize_token(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(s);
    if s.is_empty() || s.len() > MAX_FIELD_CHARS {
        return None;
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        Some(s.to_string())
    } else {
        None
    }
}

#[derive(Default)]
struct StageCounts {
    hook_delivery: u64,
    classifier: u64,
    accumulation: u64,
    timeout: u64,
    cooldown: u64,
    recognized: u64,
    dispatch: u64,
    registration: u64,
}

struct CaptureSummary<'a> {
    end_reason: &'a str,
    records_written: u64,
    records_dropped: u64,
    records_capped: u64,
    records_in_flight_at_close: u64,
    records_after_close: u64,
    counts: &'a StageCounts,
}

impl StageCounts {
    fn bump(&mut self, stage: &str) {
        match stage {
            GESTURE_DIAG_STAGE_HOOK_DELIVERY => self.hook_delivery += 1,
            GESTURE_DIAG_STAGE_CLASSIFIER => self.classifier += 1,
            GESTURE_DIAG_STAGE_ACCUMULATION => self.accumulation += 1,
            GESTURE_DIAG_STAGE_TIMEOUT => self.timeout += 1,
            GESTURE_DIAG_STAGE_COOLDOWN => self.cooldown += 1,
            GESTURE_DIAG_STAGE_RECOGNIZED => self.recognized += 1,
            GESTURE_DIAG_STAGE_DISPATCH => self.dispatch += 1,
            GESTURE_DIAG_STAGE_REGISTRATION => self.registration += 1,
            _ => {}
        }
    }
}

fn run_worker(
    mut file: File,
    limits: CaptureLimits,
    rx: mpsc::Receiver<CaptureRecord>,
    state: CaptureState,
) {
    let deadline = Instant::now() + limits.duration;
    let mut counts = StageCounts::default();
    let mut records_written: u64 = 0;
    let mut records_capped: u64 = 0;
    let mut bytes_written: usize = 0;
    let mut end_reason = loop {
        if state.shutdown.load(Ordering::Relaxed) {
            break "shutdown";
        }
        let now = Instant::now();
        if now >= deadline {
            break "deadline";
        }
        let wait = deadline.saturating_duration_since(now).min(WORKER_POLL);
        match rx.recv_timeout(wait) {
            Ok(record) => match append_record(
                &mut file,
                record,
                &limits,
                &mut counts,
                &mut records_written,
                &mut records_capped,
                &mut bytes_written,
            ) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!(
                        "[leopardwm] Warning: gesture diagnostic capture failed to write a record ({e})"
                    );
                    break "write_error";
                }
            },
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break "shutdown",
        }
    };

    // Closing admission before draining prevents post-deadline work from
    // extending the capture. The channel capacity bounds this final drain.
    state.close_gate();
    state.active.store(false, Ordering::Release);
    let records_in_flight_at_close = state.in_flight.load(Ordering::Acquire);
    for _ in 0..limits.channel_capacity.clamp(1, FINAL_DRAIN_MAX_RECORDS) {
        let Ok(record) = rx.try_recv() else {
            break;
        };
        if append_record(
            &mut file,
            record,
            &limits,
            &mut counts,
            &mut records_written,
            &mut records_capped,
            &mut bytes_written,
        )
        .is_err()
        {
            end_reason = "write_error";
            break;
        }
    }

    let records_dropped = state.dropped.load(Ordering::Relaxed);
    let records_after_close = state.after_close.load(Ordering::Relaxed);
    let summary = CaptureSummary {
        end_reason,
        records_written,
        records_dropped,
        records_capped,
        records_in_flight_at_close,
        records_after_close,
        counts: &counts,
    };
    if let Err(e) = write_summary(&mut file, summary) {
        eprintln!("[leopardwm] Warning: gesture diagnostic capture failed to write summary ({e})");
    }
    state.active.store(false, Ordering::Release);
}

fn append_record(
    file: &mut File,
    record: CaptureRecord,
    limits: &CaptureLimits,
    counts: &mut StageCounts,
    records_written: &mut u64,
    records_capped: &mut u64,
    bytes_written: &mut usize,
) -> io::Result<()> {
    let line_len = record.line.len() + 1;
    if *records_written as usize >= limits.max_records
        || *bytes_written + line_len > limits.max_bytes
    {
        *records_capped += 1;
        return Ok(());
    }
    writeln!(file, "{}", record.line)?;
    *bytes_written += line_len;
    *records_written += 1;
    counts.bump(record.stage);
    Ok(())
}

fn write_header(file: &mut File, header: &CaptureHeader) -> io::Result<()> {
    writeln!(file, "# leopardwm gesture capture")?;
    writeln!(file, "version={}", sanitize_header(&header.version))?;
    writeln!(
        file,
        "gestures_enabled_config={}",
        header.gestures_enabled_config
    )?;
    writeln!(file, "capture_limit_secs={}", header.capture_limit_secs)?;
    writeln!(
        file,
        "daemon_integrity={}",
        sanitize_header(&header.daemon_integrity)
    )?;
    writeln!(
        file,
        "# Hook/classifier/dispatch evidence only. Cannot prove finger count, device origin, or Windows gesture-setting compatibility."
    )?;
    writeln!(
        file,
        "# A new capture replaces this file. Default-off does not truncate it. This file's presence does not mean a capture is running now."
    )?;
    writeln!(file)?;
    Ok(())
}

fn write_summary(file: &mut impl Write, summary: CaptureSummary<'_>) -> io::Result<()> {
    let no_input = summary.counts.hook_delivery == 0
        && summary.records_dropped == 0
        && summary.records_capped == 0
        && summary.records_in_flight_at_close == 0
        && summary.records_after_close == 0;
    writeln!(file)?;
    writeln!(file, "# summary")?;
    writeln!(file, "end_reason={}", summary.end_reason)?;
    writeln!(file, "records_written={}", summary.records_written)?;
    writeln!(file, "records_dropped={}", summary.records_dropped)?;
    writeln!(file, "records_capped={}", summary.records_capped)?;
    writeln!(
        file,
        "records_in_flight_at_close={}",
        summary.records_in_flight_at_close
    )?;
    writeln!(file, "records_after_close={}", summary.records_after_close)?;
    writeln!(file, "no_input={no_input}")?;
    writeln!(file, "stage_hook_delivery={}", summary.counts.hook_delivery)?;
    writeln!(file, "stage_classifier={}", summary.counts.classifier)?;
    writeln!(file, "stage_accumulation={}", summary.counts.accumulation)?;
    writeln!(file, "stage_timeout={}", summary.counts.timeout)?;
    writeln!(file, "stage_cooldown={}", summary.counts.cooldown)?;
    writeln!(file, "stage_recognized={}", summary.counts.recognized)?;
    writeln!(file, "stage_dispatch={}", summary.counts.dispatch)?;
    writeln!(file, "stage_registration={}", summary.counts.registration)?;
    file.flush()
}

fn sanitize_header(value: &str) -> String {
    sanitize_token(value).unwrap_or_else(|| "unavailable".to_string())
}

fn join_short(thread: &mut Option<JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(handle) = thread.as_ref() else {
            return;
        };
        if handle.is_finished() {
            if let Some(handle) = thread.take() {
                let _ = handle.join();
            }
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leopardwm_platform_win32::emit_gesture_registration;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lwm-gdiag-{}-{}",
            std::process::id(),
            TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_header(secs: u64) -> CaptureHeader {
        CaptureHeader {
            version: "0.2.10".to_string(),
            gestures_enabled_config: true,
            capture_limit_secs: secs,
            daemon_integrity: "Medium".to_string(),
        }
    }

    fn read_capture(dir: &Path) -> String {
        std::fs::read_to_string(capture_log_path(dir)).unwrap()
    }

    fn emit_under_capture(handle: &GestureCaptureHandle, emit: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry().with(handle.layer());
        tracing::subscriber::with_default(subscriber, emit);
    }

    fn emit_diagnostic_probe(visits: &AtomicU64) {
        if !leopardwm_platform_win32::gesture_diagnostic_capture_active() {
            return;
        }
        tracing::trace!(
            target: GESTURE_DIAG_TARGET,
            stage = GESTURE_DIAG_STAGE_HOOK_DELIVERY,
            axis = "vertical",
            delta = {
                visits.fetch_add(1, Ordering::Relaxed);
                120
            },
            flags = 0u32,
            mods_held = false,
            swipe_candidate = false,
        );
    }

    fn emit_general_trace_probe(visits: &AtomicU64) {
        tracing::trace!(
            target: "leopardwm::test_general",
            visits = {
                visits.fetch_add(1, Ordering::Relaxed);
                1
            },
            "general trace after capture"
        );
    }

    #[test]
    fn default_off_does_not_mutate_existing_file() {
        let dir = test_dir();
        let path = capture_log_path(&dir);
        std::fs::write(&path, "keep-me\n").unwrap();
        let handle = start_capture(&dir, test_header(0), CaptureLimits::from_secs(0));
        assert!(handle.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep-me\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_input_completes_on_deadline_without_further_events() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(200);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(body.contains("end_reason=deadline"), "{body}");
        assert!(body.contains("no_input=true"), "{body}");
        assert!(body.contains("records_written=0"), "{body}");
        assert!(body.contains("records_dropped=0"), "{body}");
        assert!(body.contains("gestures_enabled_config=true"), "{body}");
        assert!(body.contains("daemon_integrity=Medium"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boundary_uncertainty_prevents_no_input_claim() {
        let counts = StageCounts::default();
        let mut body = Vec::new();
        write_summary(
            &mut body,
            CaptureSummary {
                end_reason: "deadline",
                records_written: 0,
                records_dropped: 0,
                records_capped: 0,
                records_in_flight_at_close: 1,
                records_after_close: 1,
                counts: &counts,
            },
        )
        .unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("no_input=false"), "{body}");
    }

    #[test]
    fn production_filter_stops_inactive_diagnostics_without_vetoing_general_trace() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(100);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        let diagnostic_visits = Arc::new(AtomicU64::new(0));
        let general_visits = Arc::new(AtomicU64::new(0));
        let general = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = Arc::clone(&general);
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || VecWriter(Arc::clone(&writer_buf)))
                    .with_filter(LevelFilter::TRACE),
            )
            .with(handle.layer().with_filter(handle.filter()));

        tracing::subscriber::with_default(subscriber, || {
            emit_diagnostic_probe(&diagnostic_visits);
            handle.wait_for_deadline();
            emit_diagnostic_probe(&diagnostic_visits);
            emit_general_trace_probe(&general_visits);
        });

        assert_eq!(diagnostic_visits.load(Ordering::Relaxed), 1);
        assert_eq!(general_visits.load(Ordering::Relaxed), 1);
        let general = String::from_utf8(general.lock().unwrap().clone()).unwrap();
        assert!(general.contains("general trace after capture"), "{general}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_capture_replaces_prior_report() {
        let dir = test_dir();
        let path = capture_log_path(&dir);
        std::fs::write(&path, "stale-report\n").unwrap();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(200);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(!body.contains("stale-report"), "{body}");
        assert!(body.contains("# leopardwm gesture capture"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deadline_does_not_require_further_input() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(250);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        emit_under_capture(&handle, || {
            emit_gesture_registration("registered");
        });
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(
            body.contains("stage=registration state=registered"),
            "{body}"
        );
        assert!(body.contains("end_reason=deadline"), "{body}");
        assert!(body.contains("stage_registration=1"), "{body}");
        assert!(body.contains("stage_hook_delivery=0"), "{body}");
        assert!(body.contains("no_input=true"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuing_input_cannot_extend_deadline_capture() {
        let dir = test_dir();
        let limits = CaptureLimits {
            duration: Duration::from_millis(120),
            max_records: DEFAULT_MAX_RECORDS,
            max_bytes: DEFAULT_MAX_BYTES,
            channel_capacity: 32,
        };
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        let layer = handle.layer();
        let producer = std::thread::spawn(move || {
            let subscriber = tracing_subscriber::registry().with(layer);
            tracing::subscriber::with_default(subscriber, || {
                let until = Instant::now() + Duration::from_millis(300);
                while Instant::now() < until {
                    tracing::trace!(
                        target: GESTURE_DIAG_TARGET,
                        stage = GESTURE_DIAG_STAGE_HOOK_DELIVERY,
                        axis = "vertical",
                        delta = 120,
                        flags = 0u32,
                        mods_held = false,
                        swipe_candidate = false,
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        });
        let started = Instant::now();
        handle.wait_for_deadline();
        let elapsed = started.elapsed();
        producer.join().unwrap();

        let body = read_capture(&dir);
        assert!(
            elapsed < Duration::from_millis(600),
            "capture took {elapsed:?}"
        );
        assert!(body.contains("end_reason=deadline"), "{body}");
        assert!(body.contains("no_input=false"), "{body}");
        assert!(body.contains("stage_hook_delivery="), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_count_reports_cap_and_does_not_claim_no_input() {
        let dir = test_dir();
        let limits = CaptureLimits {
            duration: Duration::from_millis(400),
            max_records: 2,
            max_bytes: DEFAULT_MAX_BYTES,
            channel_capacity: 32,
        };
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        emit_under_capture(&handle, || {
            for _ in 0..6 {
                emit_gesture_registration("registered");
            }
        });
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(body.contains("records_written=2"), "{body}");
        assert!(body.contains("records_capped=4"), "{body}");
        assert!(body.contains("no_input=false"), "{body}");
        let written = body
            .lines()
            .filter(|line| line.starts_with("stage=registration"))
            .count();
        assert_eq!(written, 2, "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_drop_evidence_prevents_no_input_claim() {
        let (tx, rx) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let layer = GestureCaptureLayer {
            tx,
            state: CaptureState {
                dropped: Arc::clone(&dropped),
                shutdown: Arc::new(AtomicBool::new(false)),
                active: Arc::new(AtomicBool::new(true)),
                in_flight: Arc::new(AtomicU64::new(0)),
                after_close: Arc::new(AtomicU64::new(0)),
                gate_open: Arc::new(AtomicBool::new(false)),
            },
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for state in ["registered", "disabled", "failed"] {
                tracing::trace!(
                    target: GESTURE_DIAG_TARGET,
                    stage = GESTURE_DIAG_STAGE_REGISTRATION,
                    state,
                );
            }
        });
        assert!(dropped.load(Ordering::Relaxed) >= 1);
        drop(rx);
    }

    #[test]
    fn no_action_dispatch_omits_unknown_command_text() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(250);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        emit_under_capture(&handle, || {
            emit_dispatch(GestureEvent::SwipeLeft, "no_action", None);
            emit_dispatch(
                GestureEvent::SwipeRight,
                "unknown",
                Some("not a safe command!"),
            );
            emit_dispatch(GestureEvent::SwipeUp, "known", Some("focus_up"));
        });
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(
            body.contains("stage=dispatch event=swipe_left binding=no_action"),
            "{body}"
        );
        assert!(
            body.contains("stage=dispatch event=swipe_right binding=unknown"),
            "{body}"
        );
        assert!(!body.contains("not a safe command"), "{body}");
        assert!(
            body.contains("stage=dispatch event=swipe_up binding=known command=focus_up"),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_excludes_unrelated_application_data() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(250);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        emit_under_capture(&handle, || {
            tracing::info!(
                hwnd = 0x1234u64,
                title = "SecretApp Window",
                "unrelated window event"
            );
            tracing::trace!(
                target: GESTURE_DIAG_TARGET,
                stage = GESTURE_DIAG_STAGE_HOOK_DELIVERY,
                hwnd = 0x1234u64,
                title = "SecretApp Window",
                axis = "vertical",
                delta = 120,
                flags = 0u32,
                mods_held = false,
                swipe_candidate = false,
            );
        });
        handle.wait_for_deadline();
        let body = read_capture(&dir);
        assert!(body.contains("stage=hook_delivery"), "{body}");
        assert!(body.contains("axis=vertical"), "{body}");
        assert!(!body.contains("SecretApp"), "{body}");
        assert!(!body.contains("0x1234"), "{body}");
        assert!(!body.contains("hwnd"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn info_filter_ignores_trace_diagnostics_and_capture_still_records() {
        let dir = test_dir();
        let mut limits = CaptureLimits::from_secs(1);
        limits.duration = Duration::from_millis(250);
        let handle = start_capture(&dir, test_header(1), limits).unwrap();
        let general = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = Arc::clone(&general);
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || VecWriter(Arc::clone(&writer_buf)))
                    .with_filter(LevelFilter::INFO),
            )
            .with(handle.layer().with_filter(handle.filter()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("ordinary info line");
            tracing::trace!("trace should not reach info layer");
            emit_dispatch(GestureEvent::ScrollUp, "known", Some("focus_next"));
        });
        handle.wait_for_deadline();
        let general = String::from_utf8(general.lock().unwrap().clone()).unwrap();
        assert!(
            general.contains("ordinary info line"),
            "info layer output was: {general:?}"
        );
        assert!(
            !general.contains("trace should not reach info layer"),
            "{general}"
        );
        assert!(!general.contains("stage=dispatch"), "{general}");
        let body = read_capture(&dir);
        assert!(
            body.contains("stage=dispatch event=scroll_up binding=known command=focus_next"),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_integrity_matches_observed_rids() {
        assert_eq!(format_integrity(Some(INTEGRITY_MEDIUM)), "Medium");
        assert_eq!(format_integrity(Some(INTEGRITY_HIGH)), "High");
        assert_eq!(format_integrity(Some(0x4000)), "0x4000");
        assert_eq!(format_integrity(None), "unavailable");
    }

    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
