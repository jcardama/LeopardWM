use leopardwm_ipc::bounded_log::{BoundedLogFile, LOG_FILE_MAX_BYTES};
use leopardwm_ipc::{
    log_text::{self, RecordTime},
    DaemonLogStatus,
};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::Level;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone)]
pub(crate) struct LogHealth {
    initial: DaemonLogStatus,
    first_write_error: Arc<OnceLock<String>>,
    settings_events:
        Arc<OnceLock<std::sync::Weak<std::sync::mpsc::Sender<crate::settings::SettingsEvent>>>>,
}

impl LogHealth {
    fn new(initial: DaemonLogStatus) -> Self {
        Self {
            initial,
            first_write_error: Arc::new(OnceLock::new()),
            settings_events: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn status(&self) -> DaemonLogStatus {
        match (&self.initial, self.first_write_error.get()) {
            (DaemonLogStatus::Writing { path }, Some(error)) => DaemonLogStatus::WriteFailed {
                path: path.clone(),
                error: error.clone(),
            },
            _ => self.initial.clone(),
        }
    }

    pub(crate) fn set_settings_events(
        &self,
        sender: std::sync::Weak<std::sync::mpsc::Sender<crate::settings::SettingsEvent>>,
    ) {
        let _ = self.settings_events.set(sender);
    }

    fn record<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if self.first_write_error.set(error.to_string()).is_ok() {
                if let Some(sender) = self
                    .settings_events
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                {
                    let _ = sender.send(crate::settings::SettingsEvent::LogFailure);
                }
            }
        }
        result
    }
}

pub(crate) struct LogWriter<W> {
    inner: W,
    health: LogHealth,
}

impl<'a, W: MakeWriter<'a>> MakeWriter<'a> for LogWriter<W> {
    type Writer = LogWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            inner: self.inner.make_writer(),
            health: self.health.clone(),
        }
    }
}

impl<W: Write> Write for LogWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.health.record(self.inner.write(buf))
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.health.record(self.inner.write_all(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.health.record(self.inner.flush())
    }
}

pub(crate) fn open(
    log_dir: &Path,
    log_level: Level,
) -> (Option<LogWriter<Mutex<BoundedLogFile>>>, LogHealth) {
    let path = log_dir.join("leopardwm-daemon.log").display().to_string();
    match BoundedLogFile::open(log_dir, "leopardwm-daemon.log", LOG_FILE_MAX_BYTES) {
        Ok(inner) => {
            let health = LogHealth::new(DaemonLogStatus::Writing { path });
            let writer = LogWriter {
                inner: Mutex::new(inner),
                health: health.clone(),
            };
            let mut timestamp = String::new();
            RecordTime
                .format_time(&mut Writer::new(&mut timestamp))
                .expect("formatting a timestamp into a String cannot fail");
            let startup = format!(
                "{timestamp} LeopardWM daemon {} (pid {}) opened this log at log level {}\n",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                log_level.as_str().to_ascii_lowercase()
            );
            let _ = writer.make_writer().write_all(startup.as_bytes());
            (Some(writer), health)
        }
        Err(error) => {
            eprint!(
                "{}",
                log_text::error_record(format_args!(
                    "Cannot open daemon log {}: {}",
                    log_text::path(&path),
                    error
                ))
            );
            (
                None,
                LogHealth::new(DaemonLogStatus::OpenFailed {
                    path,
                    error: error.to_string(),
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failure_is_reported_without_panicking() {
        let path =
            std::env::temp_dir().join(format!("leopardwm-log-blocked-{}", std::process::id()));
        std::fs::write(&path, b"not a directory").unwrap();
        let (writer, health) = open(&path, Level::INFO);
        std::fs::remove_file(&path).unwrap();
        assert!(writer.is_none());
        assert!(
            matches!(health.status(), DaemonLogStatus::OpenFailed { path: reported, error } if reported == path.join("leopardwm-daemon.log").display().to_string() && !error.is_empty())
        );
    }

    #[test]
    fn startup_and_events_use_the_exact_daemon_filename() {
        let dir =
            std::env::temp_dir().join(format!("leopardwm-log-writing-{}", std::process::id()));
        let (writer, health) = open(&dir, Level::WARN);
        let writer = writer.unwrap();
        let startup = std::fs::read_to_string(dir.join("leopardwm-daemon.log")).unwrap();
        let (timestamp, message) = startup
            .strip_prefix("[lwm-log-v1] ")
            .unwrap()
            .split_once(' ')
            .unwrap();
        assert!(timestamp.ends_with('Z'));
        assert_eq!(
            message,
            format!(
                "LeopardWM daemon {} (pid {}) opened this log at log level warn\n",
                env!("CARGO_PKG_VERSION"),
                std::process::id()
            )
        );
        writer.make_writer().write_all(b"log entry\n").unwrap();
        drop(writer);
        assert_eq!(
            std::fs::read_to_string(dir.join("leopardwm-daemon.log")).unwrap(),
            format!("{startup}log entry\n")
        );
        assert!(matches!(health.status(), DaemonLogStatus::Writing { .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_existing_log_moves_aside_before_startup_line() {
        let dir = std::env::temp_dir().join(format!(
            "leopardwm-log-upgrade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("leopardwm-daemon.log");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"old log marker").unwrap();
        file.set_len(LOG_FILE_MAX_BYTES + 1).unwrap();
        drop(file);

        let (writer, health) = open(&dir, Level::INFO);
        let writer = writer.unwrap();
        let previous = dir.join("leopardwm-daemon.old.log");
        assert_eq!(
            std::fs::metadata(&previous).unwrap().len(),
            LOG_FILE_MAX_BYTES + 1
        );
        use std::io::Read;
        let mut marker = [0; 14];
        std::fs::File::open(previous)
            .unwrap()
            .read_exact(&mut marker)
            .unwrap();
        assert_eq!(&marker, b"old log marker");
        let startup = std::fs::read_to_string(path).unwrap();
        assert!(startup.contains("LeopardWM daemon "));
        assert!(startup.ends_with("opened this log at log level info\n"));
        assert!(matches!(health.status(), DaemonLogStatus::Writing { .. }));
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_line_rolls_over_without_splitting() {
        let dir = std::env::temp_dir().join(format!(
            "leopardwm-log-startup-rollover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("leopardwm-daemon.log");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(LOG_FILE_MAX_BYTES - 1).unwrap();
        drop(file);

        let (writer, _) = open(&dir, Level::INFO);
        let writer = writer.unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("leopardwm-daemon.old.log"))
                .unwrap()
                .len(),
            LOG_FILE_MAX_BYTES - 1
        );
        let startup = std::fs::read_to_string(path).unwrap();
        let (timestamp, message) = startup
            .strip_prefix("[lwm-log-v1] ")
            .unwrap()
            .split_once(' ')
            .unwrap();
        assert!(timestamp.ends_with('Z'));
        assert!(message.starts_with("LeopardWM daemon "));
        assert!(message.ends_with("opened this log at log level info\n"));
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn first_write_failure_notifies_settings_once() {
        let health = LogHealth::new(DaemonLogStatus::Writing {
            path: "daemon.log".into(),
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        let sender = Arc::new(sender);
        health.set_settings_events(Arc::downgrade(&sender));
        assert!(health.record::<()>(Err(io::Error::other("first"))).is_err());
        assert!(matches!(
            receiver.try_recv(),
            Ok(crate::settings::SettingsEvent::LogFailure)
        ));
        assert!(health
            .record::<()>(Err(io::Error::other("second")))
            .is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn writer_preserves_first_error_and_keeps_attempting_writes() {
        struct FailingWriter(usize);
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                Err(io::Error::other(format!("failure {}", self.0)))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let health = LogHealth::new(DaemonLogStatus::Writing {
            path: "daemon.log".into(),
        });
        let mut writer = LogWriter {
            inner: FailingWriter(0),
            health: health.clone(),
        };
        assert!(writer.write_all(b"first").is_err());
        assert!(writer.write_all(b"second").is_err());
        assert_eq!(writer.inner.0, 2);
        assert_eq!(
            health.status(),
            DaemonLogStatus::WriteFailed {
                path: "daemon.log".into(),
                error: "failure 1".into()
            }
        );
    }
}
