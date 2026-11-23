use crate::anchored_fs::AnchoredDir;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Target maximum size of each daemon or watchdog log file.
pub const LOG_FILE_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// An append-only current log with one previous file.
///
/// An existing file at or over the limit is rolled over on open when possible.
/// An oversized old log remains there until the next rollover replaces it.
/// If renaming during rollover fails but reopening succeeds, writes keep appending
/// and the current file can exceed the limit until a later rollover succeeds.
/// A failed reopen is reported once per failure streak; later writes retry the
/// reopen and are discarded until it succeeds. A single write larger than the
/// limit can also exceed it.
pub struct BoundedLogFile {
    dir: AnchoredDir,
    name: OsString,
    previous_name: OsString,
    file: Option<File>,
    reopen_failure_reported: bool,
    length: u64,
    max_bytes: u64,
}

/// Return the previous-file path for a current log path.
pub fn previous_log_path(path: &Path) -> PathBuf {
    match path.extension() {
        Some(extension) => {
            let mut previous_extension = std::ffi::OsString::from("old.");
            previous_extension.push(extension);
            path.with_extension(previous_extension)
        }
        None => path.with_extension("old"),
    }
}

impl BoundedLogFile {
    pub fn open(dir: &Path, file_name: &str, max_bytes: u64) -> io::Result<Self> {
        let dir = AnchoredDir::for_path(dir)?;
        let name = OsString::from(file_name);
        let previous_name = previous_log_path(Path::new(file_name)).into_os_string();
        let file = dir.open_append(&name)?;
        let length = file.metadata()?.len();
        let mut log = Self {
            dir,
            name,
            previous_name,
            file: Some(file),
            reopen_failure_reported: false,
            length,
            max_bytes,
        };
        if length >= max_bytes && length != 0 {
            log.rollover()?;
        }
        Ok(log)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = self.dir.open_append(&self.name)?;
        self.length = file.metadata()?.len();
        self.file = Some(file);
        self.reopen_failure_reported = false;
        Ok(())
    }

    fn rollover(&mut self) -> io::Result<()> {
        drop(self.file.take());
        let _ = self.dir.rename_replace(&self.name, &self.previous_name);
        self.reopen()
    }

    fn reopen_error(&mut self, error: io::Error, len: usize) -> io::Result<usize> {
        if self.reopen_failure_reported {
            Ok(len)
        } else {
            self.reopen_failure_reported = true;
            Err(error)
        }
    }
}

impl Write for BoundedLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            if let Err(error) = self.reopen() {
                return self.reopen_error(error, buf.len());
            }
        }
        if self.length != 0 && self.length.saturating_add(buf.len() as u64) > self.max_bytes {
            if let Err(error) = self.rollover() {
                return self.reopen_error(error, buf.len());
            }
        }
        let written = self.file.as_mut().expect("log file is open").write(buf)?;
        self.length += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leopardwm-bounded-log-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[cfg(windows)]
    fn assert_redirected_directory_is_refused(redirect_leaf: bool) {
        crate::anchored_fs::force_refuse_for_tests();
        let root = temp_dir();
        let target = root.join("outside");
        let app = root.join("leopardwm");
        let log_dir = app.join("logs");
        fs::create_dir_all(&target).unwrap();
        let link = if redirect_leaf {
            fs::create_dir_all(&app).unwrap();
            log_dir.clone()
        } else {
            app
        };
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = BoundedLogFile::open(&log_dir, "example.log", 8);
        let refused = result.is_err();
        drop(result);
        let target_was_modified = fs::read_dir(&target).unwrap().next().is_some();
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(refused, "redirected log folder was accepted");
        assert!(!target_was_modified, "junction target was modified");
    }

    #[test]
    fn open_refuses_hard_link_to_outside_file() {
        crate::anchored_fs::force_refuse_for_tests();
        let root = temp_dir();
        let dir = root.join("logs");
        fs::create_dir_all(&dir).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"preserve").unwrap();
        fs::hard_link(&outside, dir.join("example.log")).unwrap();
        let result = BoundedLogFile::open(&dir, "example.log", 100);
        let refused = result.is_err();
        drop(result);
        let contents = fs::read(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(refused, "hard-linked log was accepted");
        assert_eq!(contents, b"preserve");
    }

    #[cfg(windows)]
    #[test]
    fn open_refuses_redirected_app_directory_without_creating_logs() {
        assert_redirected_directory_is_refused(false);
    }

    #[cfg(windows)]
    #[test]
    fn open_refuses_redirected_log_directory_without_creating_file() {
        assert_redirected_directory_is_refused(true);
    }

    #[cfg(windows)]
    #[test]
    fn rollover_preserves_current_when_previous_is_a_directory_junction() {
        crate::anchored_fs::force_refuse_for_tests();
        let root = temp_dir();
        let dir = root.join("logs");
        let target = root.join("outside");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&target).unwrap();
        let marker = target.join("marker");
        fs::write(&marker, b"untouched").unwrap();
        let previous = dir.join("example.old.log");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&previous)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut log = BoundedLogFile::open(&dir, "example.log", 4).unwrap();
        log.write_all(b"abcd").unwrap();
        log.write_all(b"ef").unwrap();
        drop(log);
        let marker_content = fs::read(&marker).unwrap();
        let current_content = fs::read(dir.join("example.log")).unwrap();
        fs::remove_dir(previous).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(marker_content, b"untouched");
        assert_eq!(current_content, b"abcdef");
    }

    #[cfg(windows)]
    #[test]
    fn junction_reopen_failure_is_reported_once_per_failure_streak() {
        crate::anchored_fs::force_refuse_for_tests();
        let root = temp_dir();
        let dir = root.join("logs");
        let target = root.join("outside");
        fs::create_dir_all(&target).unwrap();
        let mut log = BoundedLogFile::open(&dir, "example.log", 4).unwrap();
        log.write_all(b"abcd").unwrap();
        let current = dir.join("example.log");
        fs::rename(&current, dir.join("saved.log")).unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&current)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let first = log.write(b"e");
        let second = log.write(b"f");
        let third = log.write(b"g");
        drop(log);
        let target_was_modified = fs::read_dir(&target).unwrap().next().is_some();
        if current.is_dir() {
            fs::remove_dir(&current).unwrap();
        } else if current.exists() {
            fs::remove_file(&current).unwrap();
        }
        let previous = dir.join("example.old.log");
        if previous.is_dir() {
            fs::remove_dir(previous).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
        assert!(first.is_err(), "unsafe reopen was accepted");
        assert_eq!(second.unwrap(), 1);
        assert_eq!(third.unwrap(), 1);
        assert!(!target_was_modified, "junction target was modified");
    }

    #[cfg(windows)]
    #[test]
    fn reopen_retries_after_transient_sharing_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir();
        let current = dir.join("example.log");
        let mut log = BoundedLogFile::open(&dir, "example.log", 4).unwrap();
        log.write_all(b"abcd").unwrap();
        fs::rename(&current, dir.join("saved.log")).unwrap();
        fs::write(&current, b"x").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&current)
            .unwrap();
        let first = log.write(b"e");
        let second = log.write(b"f");
        drop(locked);
        let recovered = log.write(b"g");
        let contents = fs::read(&current).unwrap();
        log.write_all(b"hi").unwrap();
        fs::rename(&current, dir.join("saved-again.log")).unwrap();
        fs::write(&current, b"y").unwrap();
        let locked_again = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&current)
            .unwrap();
        let next_failure = log.write(b"j");
        drop(locked_again);
        drop(log);
        fs::remove_dir_all(dir).unwrap();
        assert!(first.is_err(), "sharing violation was not reported");
        assert_eq!(second.unwrap(), 1);
        assert_eq!(recovered.unwrap(), 1);
        assert_eq!(contents, b"xg");
        assert!(
            next_failure.is_err(),
            "a new failure streak was not reported"
        );
    }

    #[test]
    fn daemon_and_watchdog_logs_can_be_open_together() {
        let dir = temp_dir();
        let mut daemon = BoundedLogFile::open(&dir, "leopardwm.log", 8).unwrap();
        let mut watchdog = BoundedLogFile::open(&dir, "leopardwm-watchdog.log", 8).unwrap();
        daemon.write_all(b"daemon").unwrap();
        watchdog.write_all(b"watchdog").unwrap();
        daemon.write_all(b"+").unwrap();
        drop((daemon, watchdog));
        assert_eq!(fs::read(dir.join("leopardwm.log")).unwrap(), b"daemon+");
        assert_eq!(
            fs::read(dir.join("leopardwm-watchdog.log")).unwrap(),
            b"watchdog"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn appends_existing_file_and_replaces_previous_on_rollover() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("example.log");
        let previous = dir.join("example.old.log");
        fs::write(&current, b"abc").unwrap();
        fs::write(&previous, b"obsolete").unwrap();
        let mut log = BoundedLogFile::open(&dir, "example.log", 8).unwrap();
        log.write_all(b"de").unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"abcde");
        log.write_all(b"fghi").unwrap();
        assert_eq!(fs::read(&previous).unwrap(), b"abcde");
        assert_eq!(fs::read(&current).unwrap(), b"fghi");
        log.write_all(b"jklmn").unwrap();
        assert_eq!(fs::read(&previous).unwrap(), b"fghi");
        assert_eq!(fs::read(&current).unwrap(), b"jklmn");
        assert!(
            fs::metadata(&previous).unwrap().len() + fs::metadata(&current).unwrap().len() <= 16
        );
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn locked_previous_file_preserves_both_logs_and_retries_rollover() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("example.log");
        let previous = dir.join("example.old.log");
        fs::write(&previous, b"old").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&previous)
            .unwrap();
        let mut log = BoundedLogFile::open(&dir, "example.log", 4).unwrap();
        log.write_all(b"abcd").unwrap();
        log.write_all(b"ef").unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"abcdef");
        assert_eq!(fs::read(&previous).unwrap(), b"old");
        drop(locked);
        log.write_all(b"gh").unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"gh");
        assert_eq!(fs::read(&previous).unwrap(), b"abcdef");
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }
}
