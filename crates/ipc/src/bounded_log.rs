use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Maximum size of each daemon or watchdog log file, except for an oversized event.
pub const LOG_FILE_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// An append-only current log with one previous file.
///
/// An existing file at or over the limit is moved to the previous file on open.
/// An oversized old log remains there until the next rollover replaces it.
pub struct BoundedLogFile {
    path: PathBuf,
    previous_path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
}

impl BoundedLogFile {
    pub fn open(dir: &Path, file_name: &str, max_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(file_name);
        let previous_path = match path.extension() {
            Some(extension) => {
                let mut previous_extension = std::ffi::OsString::from("old.");
                previous_extension.push(extension);
                path.with_extension(previous_extension)
            }
            None => path.with_extension("old"),
        };
        let file = Self::open_append(&path)?;
        let length = file.metadata()?.len();
        let mut log = Self {
            path,
            previous_path,
            file: Some(file),
            length,
            max_bytes,
        };
        if length >= max_bytes && length != 0 {
            log.rollover()?;
        }
        Ok(log)
    }

    fn open_append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn rollover(&mut self) -> io::Result<()> {
        drop(self.file.take());
        let remove_previous = fs::remove_file(&self.previous_path);
        let can_rename = match remove_previous {
            Ok(()) => true,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if !can_rename || fs::rename(&self.path, &self.previous_path).is_err() {
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }
        self.length = 0;
        self.file = Some(Self::open_append(&self.path)?);
        Ok(())
    }
}

impl Write for BoundedLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            let file = Self::open_append(&self.path)?;
            self.length = file.metadata()?.len();
            self.file = Some(file);
        }
        if self.length != 0 && self.length.saturating_add(buf.len() as u64) > self.max_bytes {
            self.rollover()?;
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leopardwm-bounded-log-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
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
    fn locked_previous_file_falls_back_to_truncating_current() {
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
        assert_eq!(fs::read(&current).unwrap(), b"ef");
        assert_eq!(fs::read(&previous).unwrap(), b"old");
        drop((log, locked));
        fs::remove_dir_all(dir).unwrap();
    }
}
