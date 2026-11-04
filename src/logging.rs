//! The service log file: one stable name, rotated by size.
//!
//! # Why not `tracing-appender`
//!
//! `tracing-appender` rolls on a clock (`hourly`, `daily`, or never), not on a size. The
//! operational contract this service inherited is the source service's
//! `RotatingFileHandler`: `logs/data_service.log`, rolled at 10 MB, with five numbered
//! backups. Keeping the bare name stable matters because `start.bat`, `refresh.bat` and the
//! install scripts point operators at it; a midnight rename would make yesterday's failure
//! disappear from the file they know to open. So this module owns the small amount of
//! rotation logic instead of bending a time-based appender into a size-based rule.
//!
//! # What is written, and what is not
//!
//! Every event goes to the file *and* to stderr, because a console run and a service run must
//! show the same thing. Nothing here formats the vendor key: callers log only
//! [`crate::config::KeySource`], and [`crate::secrets`] never logs the value.
//!
//! # Rotation
//!
//! A write that would take the file past the limit rotates *first*, so the current file never
//! exceeds the limit except for a single oversized line. Rotation closes the file, removes the
//! oldest backup, shifts `.1` through `.backups - 1` up one place, renames the current file to
//! `.1`, and reopens. Closing first is not cosmetic: Windows refuses to rename a file that is
//! still open.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The file operators know to open.
pub const LOG_FILE_NAME: &str = "data_service.log";

/// The source service's `maxBytes`.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// The source service's `backupCount`.
pub const LOG_BACKUPS: u32 = 5;

/// The rotating file itself. Cloneable through the layers that share it.
pub struct LogFile {
    state: Arc<LogFileState>,
}

impl LogFile {
    /// Open `logs/data_service.log` with the service's documented limits.
    ///
    /// The file is opened eagerly so an unwritable log directory fails at startup rather than
    /// on the first line of output.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::with_limits(path, MAX_LOG_BYTES, LOG_BACKUPS)
    }

    /// Open with explicit limits. Tests use this to exercise rotation without writing 50 MB.
    pub fn with_limits(path: &Path, max_bytes: u64, backups: u32) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the log size limit must be positive",
            ));
        }
        if backups == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one backup is required, or rotation would delete the current log",
            ));
        }
        create_parent(path)?;
        let state = Arc::new(LogFileState {
            path: path.to_path_buf(),
            max_bytes,
            backups,
            file: Mutex::new(Some(open_append(path)?)),
        });
        Ok(Self { state })
    }

    /// The file being written, for the startup line that tells an operator where to look.
    pub fn path(&self) -> &Path {
        &self.state.path
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = LogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            state: &self.state,
            file: lock(&self.state.file),
        }
    }
}

/// One event's hold on the file.
///
/// The mutex guard lives for the whole formatted event, not for each `write` call, so two
/// threads cannot interleave the halves of two lines.
pub struct LogWriter<'a> {
    state: &'a LogFileState,
    file: MutexGuard<'a, Option<File>>,
}

impl Write for LogWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.state.append(&mut self.file, buffer)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

struct LogFileState {
    path: PathBuf,
    max_bytes: u64,
    backups: u32,
    file: Mutex<Option<File>>,
}

impl LogFileState {
    /// Append one complete chunk, rotating first when it would not fit.
    fn append(&self, slot: &mut Option<File>, bytes: &[u8]) -> io::Result<()> {
        if slot.is_none() {
            *slot = Some(open_append(&self.path)?);
        }
        if self.would_exceed(slot, bytes.len()) {
            self.rotate(slot)?;
        }
        match slot.as_mut() {
            Some(file) => file.write_all(bytes),
            None => Err(io::Error::other("the log file could not be reopened")),
        }
    }

    fn would_exceed(&self, slot: &Option<File>, incoming: usize) -> bool {
        let length = slot
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let incoming = u64::try_from(incoming).unwrap_or(u64::MAX);
        length > 0 && length.saturating_add(incoming) > self.max_bytes
    }

    /// Close, shift the numbered history up, then rename the current file to `.1` and reopen.
    fn rotate(&self, slot: &mut Option<File>) -> io::Result<()> {
        slot.take();
        remove_if_present(&self.numbered(self.backups))?;
        for index in (1..self.backups).rev() {
            let from = self.numbered(index);
            if from.exists() {
                fs::rename(&from, self.numbered(index + 1))?;
            }
        }
        fs::rename(&self.path, self.numbered(1))?;
        *slot = Some(open_append(&self.path)?);
        Ok(())
    }

    fn numbered(&self, index: u32) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.path.with_file_name(format!("{name}.{index}"))
    }
}

/// Install the process subscriber: the rotating file and stderr, at INFO.
///
/// Returns the file's path for the startup line. A subscriber that is already installed is
/// left alone rather than treated as an error; only a log file that cannot be opened stops
/// the caller.
pub fn init(log_dir: &Path) -> io::Result<PathBuf> {
    let path = log_dir.join(LOG_FILE_NAME);
    let file = LogFile::open(&path)?;
    let subscriber = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .with_target(true),
        )
        .with(fmt::layer().with_writer(io::stderr).with_ansi(false))
        .with(LevelFilter::INFO);
    let _ = subscriber.try_init();
    Ok(path)
}

/// Console-only logging for the command-line jobs.
///
/// `mmds-daily` is run by a person or by Task Scheduler, not by NSSM, and the source job
/// logged to stderr only. Pointing a second process at the service's rotating file would let
/// two processes race for the same rename on Windows, and a daily run failing to rotate would
/// be a stranger failure than a daily run with no file.
pub fn init_stderr() {
    let subscriber = tracing_subscriber::registry()
        .with(fmt::layer().with_writer(io::stderr).with_ansi(false))
        .with(LevelFilter::INFO);
    let _ = subscriber.try_init();
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn create_parent(path: &Path) -> io::Result<()> {
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn lock(file: &Mutex<Option<File>>) -> MutexGuard<'_, Option<File>> {
    file.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn backup(dir: &tempfile::TempDir, index: u32) -> PathBuf {
        dir.path().join(format!("{LOG_FILE_NAME}.{index}"))
    }

    #[test]
    fn the_documented_limits_are_what_the_service_uses() {
        assert_eq!(LOG_FILE_NAME, "data_service.log");
        assert_eq!(MAX_LOG_BYTES, 10 * 1024 * 1024);
        assert_eq!(LOG_BACKUPS, 5);
    }

    #[test]
    fn opening_creates_the_directory_and_keeps_the_stable_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(LOG_FILE_NAME);
        let log = LogFile::open(&path).unwrap();

        assert_eq!(log.path(), path);
        assert!(path.exists());
    }

    #[test]
    fn writes_append_so_a_restart_keeps_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        {
            let log = LogFile::open(&path).unwrap();
            log.make_writer().write_all(b"first\n").unwrap();
        }
        {
            let log = LogFile::open(&path).unwrap();
            log.make_writer().write_all(b"second\n").unwrap();
        }

        assert_eq!(read(&path), "first\nsecond\n");
    }

    #[test]
    fn a_write_that_would_pass_the_limit_rotates_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let log = LogFile::with_limits(&path, 25, 3).unwrap();

        log.make_writer().write_all(&[b'a'; 10]).unwrap();
        assert!(!backup(&dir, 1).exists());

        log.make_writer().write_all(&[b'b'; 10]).unwrap();
        log.make_writer().write_all(&[b'c'; 10]).unwrap();

        assert_eq!(read(&path), "cccccccccc");
        assert_eq!(read(&backup(&dir, 1)), "aaaaaaaaaabbbbbbbbbb");
    }

    #[test]
    fn numbered_backups_keep_the_newest_history_and_drop_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let log = LogFile::with_limits(&path, 5, 3).unwrap();

        for byte in b'A'..=b'G' {
            log.make_writer().write_all(&[byte; 5]).unwrap();
        }

        assert_eq!(read(&path), "GGGGG");
        assert_eq!(read(&backup(&dir, 1)), "FFFFF");
        assert_eq!(read(&backup(&dir, 2)), "EEEEE");
        assert_eq!(read(&backup(&dir, 3)), "DDDDD");
        assert!(!backup(&dir, 4).exists());
    }

    #[test]
    fn a_zero_limit_or_no_backups_is_refused_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);

        assert!(LogFile::with_limits(&path, 0, 5).is_err());
        assert!(LogFile::with_limits(&path, 10, 0).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn one_event_holds_the_file_for_all_of_its_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let log = Arc::new(LogFile::with_limits(&path, 1_000_000, 1).unwrap());

        let threads: Vec<_> = (0..8)
            .map(|index| {
                let log = Arc::clone(&log);
                let index = u8::try_from(index).unwrap();
                std::thread::spawn(move || {
                    let line = char::from(b'A' + index).to_string().repeat(400);
                    let mut writer = log.make_writer();
                    // Split so an interleaved event would visibly corrupt the line.
                    writer.write_all(&line.as_bytes()[..100]).unwrap();
                    writer.write_all(&line.as_bytes()[100..]).unwrap();
                    writer.write_all(b"\n").unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let text = read(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8);
        for line in lines {
            assert_eq!(line.len(), 400);
            let first = line.chars().next().unwrap();
            assert!(
                line.chars().all(|character| character == first),
                "interleaved line: {line:?}"
            );
        }
    }
}
