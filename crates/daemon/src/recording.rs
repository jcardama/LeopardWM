use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing_subscriber::{filter::LevelFilter, reload, Registry};

const FILE_NAME: &str = "leopardwm-recording.log";
const MAX_BYTES: u64 = 20 * 1024 * 1024;
const FOOTER_RESERVE: u64 = 256;
const DEFAULT_SECS: u64 = 600;

pub(crate) type FilterHandle = reload::Handle<LevelFilter, Registry>;
static RECORDING: OnceLock<Recording> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Recording {
    inner: Arc<Inner>,
}

struct Inner {
    filter: FilterHandle,
    directory: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    generation: u64,
    active: Option<Active>,
    last_path: Option<PathBuf>,
}

struct Active {
    file: File,
    path: PathBuf,
    bytes: u64,
    capped: bool,
    _timer_cancel: std::sync::mpsc::Sender<()>,
}

#[derive(Clone)]
pub(crate) struct RecordingInfo {
    pub(crate) path: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) enum StopReason {
    Stopped,
    TimedOut,
    DaemonExit,
}

impl StopReason {
    fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::TimedOut => "timed out",
            Self::DaemonExit => "daemon exit",
        }
    }
}

pub(crate) struct Guard(Recording);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.stop(StopReason::DaemonExit);
    }
}

pub(crate) fn install(recording: Recording) -> Guard {
    assert!(RECORDING.set(recording.clone()).is_ok());
    Guard(recording)
}

pub(crate) fn global() -> &'static Recording {
    RECORDING.get().expect("recording installed at startup")
}

pub(crate) fn stop_on_exit() {
    if let Some(recording) = RECORDING.get() {
        recording.stop(StopReason::DaemonExit);
    }
}

pub(crate) fn stop_on_emergency_exit() {
    if let Some(recording) = RECORDING.get() {
        recording.stop_emergency();
    }
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("Recording path is a link: {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot inspect {}: {error}", path.display())),
    }
}

impl Recording {
    pub(crate) fn new(filter: FilterHandle, directory: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                filter,
                directory,
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub(crate) fn writer(&self) -> RecordingWriter {
        RecordingWriter(self.inner.clone())
    }

    pub(crate) fn start(&self, timeout_secs: Option<u64>) -> Result<RecordingInfo, String> {
        self.start_for(Duration::from_secs(
            timeout_secs.unwrap_or(DEFAULT_SECS).clamp(1, 3600),
        ))
    }

    fn start_for(&self, timeout: Duration) -> Result<RecordingInfo, String> {
        let mut state = self.inner.state.lock().unwrap();
        if state.active.is_some() {
            return Err("A recording is already active".into());
        }
        reject_symlink(&self.inner.directory)?;
        fs::create_dir_all(&self.inner.directory).map_err(|e| e.to_string())?;
        reject_symlink(&self.inner.directory)?;
        let path = self.inner.directory.join(FILE_NAME);
        reject_symlink(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Cannot replace recording file {} (it may be in use): {error}",
                    path.display()
                ))
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(1)
            .open(&path)
            .map_err(|error| {
                format!(
                    "Cannot create recording file {} (it may be in use or already exist): {error}",
                    path.display()
                )
            })?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let header = format!(
            "LeopardWM {} pid={} start_unix={} timeout_secs={}\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            started,
            timeout.as_secs()
        );
        file.write_all(header.as_bytes())
            .map_err(|e| e.to_string())?;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.last_path = Some(path.clone());
        let (timer_cancel, timer_receiver) = std::sync::mpsc::channel();
        state.active = Some(Active {
            file,
            path: path.clone(),
            bytes: header.len() as u64,
            capped: false,
            _timer_cancel: timer_cancel,
        });
        if let Err(error) = self
            .inner
            .filter
            .modify(|filter| *filter = LevelFilter::DEBUG)
        {
            state.active = None;
            return Err(error.to_string());
        }
        let weak = Arc::downgrade(&self.inner);
        if let Err(error) = std::thread::Builder::new()
            .name("recording-timeout".into())
            .spawn(move || {
                if matches!(
                    timer_receiver.recv_timeout(timeout),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    stop_generation(weak, generation);
                }
            })
        {
            let _ = self
                .inner
                .filter
                .modify(|filter| *filter = LevelFilter::OFF);
            if let Some(mut active) = state.active.take() {
                let _ = writeln!(active.file, "Recording ended: stopped (timer error)");
                let _ = active.file.flush();
            }
            return Err(error.to_string());
        }
        Ok(RecordingInfo { path })
    }

    pub(crate) fn stop(&self, reason: StopReason) -> Option<RecordingInfo> {
        self.stop_if(reason, None)
    }

    fn stop_if(&self, reason: StopReason, generation: Option<u64>) -> Option<RecordingInfo> {
        let mut state = self.inner.state.lock().unwrap();
        if generation.is_some_and(|generation| generation != state.generation)
            || state.active.is_none()
        {
            return None;
        }
        let _ = self
            .inner
            .filter
            .modify(|filter| *filter = LevelFilter::OFF);
        close_active(&mut state, reason)
    }

    fn stop_emergency(&self) {
        let _ = self
            .inner
            .filter
            .modify(|filter| *filter = LevelFilter::OFF);
        if let Ok(mut state) = self.inner.state.try_lock() {
            close_active(&mut state, StopReason::DaemonExit);
        }
    }

    pub(crate) fn status(&self) -> Option<RecordingInfo> {
        self.inner
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|active| RecordingInfo {
                path: active.path.clone(),
            })
    }

    pub(crate) fn last_recording_path(&self) -> Option<PathBuf> {
        self.inner.state.lock().unwrap().last_path.clone()
    }
}

fn close_active(state: &mut State, reason: StopReason) -> Option<RecordingInfo> {
    let mut active = state.active.take()?;
    let _ = writeln!(
        active.file,
        "Recording ended: {}; size cap hit: {}",
        reason.label(),
        active.capped
    );
    let _ = active.file.flush();
    Some(RecordingInfo { path: active.path })
}

fn stop_generation(inner: Weak<Inner>, generation: u64) {
    if let Some(inner) = inner.upgrade() {
        Recording { inner }.stop_if(StopReason::TimedOut, Some(generation));
    }
}

pub(crate) struct RecordingWriter(Arc<Inner>);

impl Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.0.state.lock().unwrap();
        if let Some(active) = state.active.as_mut() {
            if !active.capped
                && active.bytes.saturating_add(buf.len() as u64) <= MAX_BYTES - FOOTER_RESERVE
            {
                let written = active.file.write(buf)?;
                active.bytes += written as u64;
                return Ok(written);
            } else {
                active.capped = true;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(active) = self.0.state.lock().unwrap().active.as_mut() {
            active.file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tracing_subscriber::prelude::*;

    fn directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leopardwm-recording-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn emit_debug() {
        tracing::debug!("capture-marker");
    }

    fn join_bounded(handle: std::thread::JoinHandle<()>) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        if handle.is_finished() {
            handle.join().unwrap();
            true
        } else {
            false
        }
    }

    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn start_replaces_hard_link_without_modifying_its_target() {
        let root = directory();
        let dir = root.join("diagnostics");
        fs::create_dir_all(&dir).unwrap();
        let outside = root.join("outside.txt");
        fs::write(&outside, b"preserve me").unwrap();
        fs::hard_link(&outside, dir.join(FILE_NAME)).unwrap();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle, dir);
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            recording.start(Some(60)).unwrap();
            recording.stop(StopReason::Stopped);
        });
        assert_eq!(fs::read(outside).unwrap(), b"preserve me");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_refuses_file_open_without_delete_sharing() {
        let dir = directory();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, b"existing capture").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle.clone(), dir.clone());
        let other = Recording::new(handle, dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            let error = recording.start(Some(60)).err().unwrap();
            assert!(error.contains("in use"), "{error}");
            assert_eq!(fs::read(&path).unwrap(), b"existing capture");
            drop(held);
            recording.start(Some(60)).unwrap();
            let error = other.start(Some(60)).err().unwrap();
            assert!(error.contains("in use"), "{error}");
            recording.stop(StopReason::Stopped);
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn start_rejects_junction_directory() {
        let root = directory();
        let target = root.join("outside");
        let dir = root.join("diagnostics");
        fs::create_dir_all(&target).unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&dir)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fs::symlink_metadata(&dir).unwrap().file_type().is_symlink());
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle, dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            assert!(recording.start(Some(60)).is_err());
        });
        assert!(!target.join(FILE_NAME).exists());
        fs::remove_dir(dir).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emergency_stop_does_not_wait_for_recording_state() {
        let dir = directory();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle.clone(), dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            let path = recording.start(Some(60)).unwrap().path;
            let (locked_tx, locked_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder_recording = recording.clone();
            let holder = std::thread::spawn(move || {
                let guard = holder_recording.inner.state.lock().unwrap();
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(guard);
            });
            locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let emergency_recording = recording.clone();
            let emergency = std::thread::spawn(move || {
                emergency_recording.stop_emergency();
                done_tx.send(()).unwrap();
            });
            let returned = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            let filter_off = returned && handle.with_current(|f| *f).unwrap() == LevelFilter::OFF;
            let footer_skipped = !fs::read_to_string(&path)
                .unwrap()
                .contains("Recording ended:");
            release_tx.send(()).unwrap();
            let holder_finished = join_bounded(holder);
            let emergency_finished = join_bounded(emergency);
            assert!(
                holder_finished && emergency_finished,
                "test threads did not finish"
            );
            assert!(returned, "emergency stop waited for the state lock");
            assert!(filter_off);
            assert!(footer_skipped);
            recording.stop_emergency();
            assert!(fs::read_to_string(path)
                .unwrap()
                .contains("Recording ended: daemon exit"));
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capture_reloads_debug_without_changing_info_layer() {
        let dir = directory();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle.clone(), dir.clone());
        let writer = recording.clone();
        let normal = Buffer(Arc::new(Mutex::new(Vec::new())));
        let normal_writer = normal.clone();
        let subscriber = Registry::default()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_writer(move || writer.writer())
                    .with_filter(filter),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_writer(move || normal_writer.clone())
                    .with_filter(LevelFilter::INFO),
            );
        tracing::subscriber::with_default(subscriber, || {
            emit_debug();
            let info = recording.start(Some(60)).unwrap();
            assert!(recording.start(Some(60)).is_err());
            emit_debug();
            recording.stop(StopReason::Stopped).unwrap();
            emit_debug();
            let contents = fs::read_to_string(info.path).unwrap();
            assert_eq!(contents.matches("capture-marker").count(), 1);
            assert!(contents.contains("Recording ended: stopped"));
            assert_eq!(handle.with_current(|f| *f).unwrap(), LevelFilter::OFF);
            assert!(!String::from_utf8_lossy(&normal.0.lock().unwrap()).contains("capture-marker"));
            assert!(recording.stop(StopReason::Stopped).is_none());
            for (timeout, expected) in [(None, 600), (Some(0), 1), (Some(u64::MAX), 3600)] {
                let info = recording.start(timeout).unwrap();
                let header = fs::read_to_string(info.path).unwrap();
                assert!(header.contains(&format!("timeout_secs={expected}")));
                recording.stop(StopReason::Stopped);
            }
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timeout_and_guard_drop_disable_capture_and_close_file() {
        let dir = directory();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle.clone(), dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            let path = recording.start_for(Duration::from_millis(10)).unwrap().path;
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while recording.status().is_some() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(recording.status().is_none());
            assert_eq!(handle.with_current(|f| *f).unwrap(), LevelFilter::OFF);
            assert!(fs::read_to_string(&path)
                .unwrap()
                .contains("Recording ended: timed out"));
            let guard = Guard(recording.clone());
            recording.start(Some(60)).unwrap();
            drop(guard);
            assert_eq!(handle.with_current(|f| *f).unwrap(), LevelFilter::OFF);
            assert!(fs::read_to_string(&path)
                .unwrap()
                .contains("Recording ended: daemon exit"));
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_timeout_does_not_stop_new_recording() {
        let dir = directory();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle, dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            recording.start(Some(60)).unwrap();
            let generation = recording.inner.state.lock().unwrap().generation;
            recording.stop(StopReason::Stopped);
            recording.start(Some(60)).unwrap();
            stop_generation(Arc::downgrade(&recording.inner), generation);
            assert!(recording.status().is_some());
            recording.stop(StopReason::Stopped);
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn size_cap_leaves_room_for_footer() {
        let dir = directory();
        let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
        let recording = Recording::new(handle, dir.clone());
        let writer = recording.clone();
        let subscriber = Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.writer())
                .with_filter(filter),
        );
        tracing::subscriber::with_default(subscriber, || {
            let path = recording.start(Some(60)).unwrap().path;
            let mut writer = recording.writer();
            writer.write_all(&vec![b'x'; MAX_BYTES as usize]).unwrap();
            writer.write_all(b"should-not-appear").unwrap();
            recording.stop(StopReason::Stopped);
            assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
            let contents = fs::read_to_string(path).unwrap();
            assert!(contents.contains("size cap hit: true"));
            assert!(!contents.contains("should-not-appear"));
        });
        fs::remove_dir_all(dir).unwrap();
    }
}
