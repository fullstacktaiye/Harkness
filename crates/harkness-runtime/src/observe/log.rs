//! The bounded diagnostic log under the data directory.
//!
//! One JSON object per line, one file being written and at most
//! [`MAX_LOG_FILES`] on disk, each capped at [`MAX_LOG_FILE_BYTES`]. The cap is
//! the point: a diagnostic log is written by every run, on a machine whose owner
//! never asked for it, and one that grows without bound is a bug report waiting
//! to be filed against the wrong component.
//!
//! # Why the writer, not the formatter, owns redaction
//!
//! `tracing` fields are structured, so a span carrying `run_id` cannot be forged
//! into two log lines by a tool that emits a newline — the JSON formatter
//! escapes it. What structure does not protect against is *content*: an
//! instrumentation site that interpolates a tool's stderr into a message writes
//! whatever the tool said. Wrapping the writer rather than trusting each call
//! site means the diagnostic log is covered by the same rules the store is, and
//! a new `tracing::info!` cannot become the one channel nobody scrubbed.
//!
//! # Failure is degradation, never an outcome
//!
//! Nothing here can fail a run. A `logs/` directory that cannot be created, a
//! file that cannot be opened, a rotation that loses a race with another process
//! — each leaves the process writing its diagnostics to standard error instead,
//! for good, and carrying on. A diagnostic that took down the work it was
//! describing would be the worst possible trade.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

use crate::store::Redactor;

use super::redactor::StandardRedactor;

/// Directory the diagnostic log lives in, under the Harkness data directory.
pub const LOGS_DIRECTORY: &str = "logs";

/// Name of the file currently being written.
///
/// Rotated copies are this name with `.1` through `.{MAX_LOG_FILES - 1}`
/// appended, oldest highest — the shape `logrotate` and every operator already
/// expect, so nobody has to learn a scheme to read their own logs.
pub const LOG_FILE_NAME: &str = "harkness.log";

/// Bytes one log file may reach before it is rotated.
pub const MAX_LOG_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Files kept, counting the one being written.
///
/// Four archives plus the live file, so the log costs at most
/// `MAX_LOG_FILES * MAX_LOG_FILE_BYTES` — 20 MiB — however long Harkness runs.
pub const MAX_LOG_FILES: usize = 5;

/// Where the diagnostic log for `data_dir` lives.
#[must_use]
pub fn log_directory(data_dir: &Path) -> PathBuf {
    data_dir.join(LOGS_DIRECTORY)
}

/// Where the file currently being written lives.
#[must_use]
pub fn log_file(data_dir: &Path) -> PathBuf {
    log_directory(data_dir).join(LOG_FILE_NAME)
}

/// The archive name for generation `index`, counting from one.
fn archive(directory: &Path, index: usize) -> PathBuf {
    directory.join(format!("{LOG_FILE_NAME}.{index}"))
}

/// Creates a directory only its owner may read, on platforms that can say so.
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Applied after creation rather than through the umask, which a caller
        // may have widened. Diagnostic lines quote process output and Git
        // stderr; the redactor is the first defence and the mode is the second.
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Opens the live log file for appending, creating it `0600`.
fn open_log(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // An existing file keeps the mode it was created with, so a log left
        // behind by an older, more permissive build is tightened on reopen
        // rather than trusted.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Where a log's bytes are currently going.
///
/// Three states rather than a `Result` held somewhere, because the transition
/// out of [`Unopened`](Openness::Unopened) is one-way in both directions: a file
/// that opened stays open, and a directory that refused once is not retried on
/// every line. Retrying would turn one broken permission into a syscall per
/// event for the life of the process.
#[derive(Debug)]
enum Openness {
    /// Nothing has been written yet, so nothing has been created yet.
    Unopened,
    /// The live file is open; `written` is how much of the cap it has used.
    Open { file: File, written: u64 },
    /// The directory or the file refused, and every line goes to the fallback.
    Failed,
}

/// A size-bounded, generation-rotated append-only file.
///
/// # Created by the first line, not by `init`
///
/// Opening is deferred until something is actually logged. The alternative —
/// creating `logs/` when the subscriber is installed — would make every
/// read-only command leave a directory behind in a data directory it was only
/// asked to *read*, which is precisely the property
/// [`Store::open_existing`](crate::store::Store::open_existing) exists to
/// protect. At the default `info` level a command that records nothing creates
/// nothing.
///
/// # A directory that refuses does not fail the work
///
/// If the directory or the file cannot be opened, the log falls back to its
/// sink of last resort — standard error in production — permanently, and the
/// process carries on. Losing diagnostics is a bad day; failing the run that
/// was producing them would be a worse one.
pub(super) struct RotatingLog {
    directory: PathBuf,
    path: PathBuf,
    openness: Openness,
    fallback: Box<dyn Write + Send>,
}

impl std::fmt::Debug for RotatingLog {
    /// Names the destination and its state, never a line that passed through.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RotatingLog")
            .field("path", &self.path)
            .field("openness", &self.openness)
            .finish_non_exhaustive()
    }
}

impl RotatingLog {
    /// Names the log under `data_dir`, creating nothing.
    pub(super) fn new(data_dir: &Path) -> Self {
        Self::with_fallback(data_dir, Box::new(io::stderr()))
    }

    /// The same, against a sink a test can read back.
    pub(super) fn with_fallback(data_dir: &Path, fallback: Box<dyn Write + Send>) -> Self {
        let directory = log_directory(data_dir);
        let path = directory.join(LOG_FILE_NAME);
        Self {
            directory,
            path,
            openness: Openness::Unopened,
            fallback,
        }
    }

    /// Opens the live file on the first line, once.
    fn ensure_open(&mut self) {
        if !matches!(self.openness, Openness::Unopened) {
            return;
        }
        self.openness =
            match create_private_dir(&self.directory).and_then(|()| open_log(&self.path)) {
                Ok(file) => {
                    let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                    Openness::Open { file, written }
                }
                Err(_) => Openness::Failed,
            };
    }

    /// Renames the live file down the generations and starts a new one.
    ///
    /// The oldest archive is removed first, so the cap holds even if a later
    /// rename fails: a missed rotation costs one oversized file, while removing
    /// last would let the count creep upwards every time one did.
    fn rotate(&mut self) -> io::Result<()> {
        let oldest = archive(&self.directory, MAX_LOG_FILES - 1);
        if let Err(error) = fs::remove_file(&oldest)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error);
        }
        for index in (1..MAX_LOG_FILES - 1).rev() {
            let from = archive(&self.directory, index);
            let to = archive(&self.directory, index + 1);
            if from.exists() {
                fs::rename(&from, &to)?;
            }
        }
        // The handle is replaced rather than closed first: on Unix the rename
        // moves the name and not the open file, and on Windows a rename over an
        // open handle is refused — so the new file is opened only after the old
        // name has moved, and the previous handle is dropped when the state is
        // overwritten.
        fs::rename(&self.path, archive(&self.directory, 1))?;
        let file = open_log(&self.path)?;
        self.openness = Openness::Open { file, written: 0 };
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.ensure_open();
        let Openness::Open { written, .. } = &self.openness else {
            self.fallback.write_all(buffer)?;
            return Ok(buffer.len());
        };
        let used = *written;
        // Rotation is decided before the write rather than after it, so a line
        // is never split across two files: a JSON-lines log whose records can
        // straddle a boundary is not parseable by the tool that reads it.
        if used > 0 && used + buffer.len() as u64 > MAX_LOG_FILE_BYTES && self.rotate().is_err() {
            self.openness = Openness::Failed;
            self.fallback.write_all(buffer)?;
            return Ok(buffer.len());
        }
        let Openness::Open { file, written } = &mut self.openness else {
            unreachable!("the state was Open a moment ago and rotation restores it")
        };
        match file.write(buffer) {
            Ok(count) => {
                *written += count as u64;
                Ok(count)
            }
            Err(_) => {
                self.openness = Openness::Failed;
                self.fallback.write_all(buffer)?;
                Ok(buffer.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.openness {
            Openness::Open { file, .. } => file.flush(),
            Openness::Unopened | Openness::Failed => self.fallback.flush(),
        }
    }
}

/// A destination every formatted line is redacted on its way into.
///
/// Generic over the sink so the file and the stderr mirror share one
/// implementation; a rule that applied to one and not the other would make
/// `--verbose` a way to see what the log was hiding.
pub(super) struct RedactedSink<W: Write> {
    redactor: StandardRedactor,
    sink: W,
}

impl<W: Write> Write for RedactedSink<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // The `fmt` layer formats one whole event and writes it once, so a
        // buffer is a complete line and applying a line-oriented rule to it is
        // exact. Lossy decoding is safe here for the same reason: the formatter
        // produced this text, and everything it produces is UTF-8.
        let line = String::from_utf8_lossy(buffer);
        match self.redactor.redact_text(&line) {
            std::borrow::Cow::Borrowed(_) => self.sink.write_all(buffer)?,
            std::borrow::Cow::Owned(redacted) => self.sink.write_all(redacted.as_bytes())?,
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

/// The `MakeWriter` behind the diagnostic file.
#[derive(Debug)]
pub(super) struct LogWriter {
    log: Mutex<RotatingLog>,
    redactor: StandardRedactor,
}

impl LogWriter {
    pub(super) fn new(log: RotatingLog, redactor: StandardRedactor) -> Self {
        Self {
            log: Mutex::new(log),
            redactor,
        }
    }
}

/// One event's exclusive hold on the rotating file.
///
/// A `MutexGuard` is not itself a writer, and the lock has to live exactly as
/// long as the formatted line takes to reach disk — which is what a `MakeWriter`
/// borrowing from `&self` expresses and what keeps two threads from interleaving
/// halves of two JSON lines into one unparseable one.
pub(super) struct Guarded<'log>(MutexGuard<'log, RotatingLog>);

impl Write for Guarded<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'writer> MakeWriter<'writer> for LogWriter {
    type Writer = RedactedSink<Guarded<'writer>>;

    fn make_writer(&'writer self) -> Self::Writer {
        RedactedSink {
            redactor: self.redactor.clone(),
            // A poisoned log mutex means some thread panicked while writing a
            // line, which is a reason to keep logging rather than to stop.
            sink: Guarded(self.log.lock().unwrap_or_else(PoisonError::into_inner)),
        }
    }
}

/// The `MakeWriter` behind the `--verbose` stderr mirror.
#[derive(Clone, Debug)]
pub(super) struct StderrWriter {
    redactor: StandardRedactor,
}

impl StderrWriter {
    pub(super) fn new(redactor: StandardRedactor) -> Self {
        Self { redactor }
    }
}

impl<'writer> MakeWriter<'writer> for StderrWriter {
    type Writer = RedactedSink<io::Stderr>;

    fn make_writer(&'writer self) -> Self::Writer {
        RedactedSink {
            redactor: self.redactor.clone(),
            sink: io::stderr(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        LOG_FILE_NAME, MAX_LOG_FILE_BYTES, MAX_LOG_FILES, RotatingLog, log_directory, log_file,
    };

    fn names(directory: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn the_directory_is_created_by_the_first_line_and_not_before() {
        let data_dir = TempDir::new().unwrap();

        let mut log = RotatingLog::new(data_dir.path());
        assert!(
            !log_directory(data_dir.path()).exists(),
            "naming a log must not write to a data directory nobody asked to change"
        );

        log.write_all(b"the first line\n").unwrap();
        drop(log);

        assert!(log_file(data_dir.path()).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(log_directory(data_dir.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(log_file(data_dir.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "a diagnostic line quotes process output; the umask is not a strong enough claim"
            );
        }
    }

    #[test]
    fn writing_past_the_cap_rotates_and_deletes_the_oldest() {
        let data_dir = TempDir::new().unwrap();
        let mut log = RotatingLog::new(data_dir.path());
        let line = vec![b'x'; 64 * 1024];

        // Enough to fill every generation and then some, so the oldest has to be
        // deleted rather than merely renamed.
        let per_file = MAX_LOG_FILE_BYTES / line.len() as u64;
        for _ in 0..per_file * (MAX_LOG_FILES as u64 + 2) {
            log.write_all(&line).unwrap();
        }
        log.flush().unwrap();
        drop(log);

        let directory = log_directory(data_dir.path());
        let found = names(&directory);
        assert_eq!(
            found.len(),
            MAX_LOG_FILES,
            "the cap is a count as well as a size: {found:?}"
        );
        assert!(found.contains(&LOG_FILE_NAME.to_owned()));
        for index in 1..MAX_LOG_FILES {
            assert!(
                found.contains(&format!("{LOG_FILE_NAME}.{index}")),
                "generation {index} is missing from {found:?}"
            );
        }
        for name in found {
            let size = fs::metadata(directory.join(&name)).unwrap().len();
            assert!(
                size <= MAX_LOG_FILE_BYTES,
                "{name} grew past the documented bound at {size} bytes"
            );
        }
    }

    #[test]
    fn a_line_is_never_split_across_two_files() {
        let data_dir = TempDir::new().unwrap();
        let mut log = RotatingLog::new(data_dir.path());
        let line = vec![b'y'; 1024 * 1024];

        for _ in 0..6 {
            log.write_all(&line).unwrap();
        }
        log.flush().unwrap();
        drop(log);

        let directory = log_directory(data_dir.path());
        for name in names(&directory) {
            let size = fs::metadata(directory.join(&name)).unwrap().len();
            assert_eq!(
                size % line.len() as u64,
                0,
                "{name} holds a partial line at {size} bytes"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_directory_that_cannot_be_created_falls_back_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Collected(Arc<Mutex<Vec<u8>>>);

        impl Write for Collected {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let collected = Collected::default();
        let mut log = RotatingLog::with_fallback(&data_dir, Box::new(collected.clone()));
        log.write_all(b"a line nobody could file\n").unwrap();
        log.flush().unwrap();

        assert_eq!(
            String::from_utf8(collected.0.lock().unwrap().clone()).unwrap(),
            "a line nobody could file\n",
            "an unwritable directory degrades the destination, never the caller"
        );
        assert!(!log_directory(&data_dir).exists());

        // Restored so the temporary directory can be removed on drop.
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn reopening_appends_rather_than_truncating() {
        let data_dir = TempDir::new().unwrap();
        let mut first = RotatingLog::new(data_dir.path());
        first.write_all(b"one\n").unwrap();
        drop(first);

        let mut second = RotatingLog::new(data_dir.path());
        second.write_all(b"two\n").unwrap();
        drop(second);

        assert_eq!(
            fs::read_to_string(log_file(data_dir.path())).unwrap(),
            "one\ntwo\n",
            "a second process must not erase the first one's evidence"
        );
    }
}
