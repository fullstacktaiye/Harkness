//! Supervised child processes for tools that shell out.
//!
//! `harkness-git`'s runner already establishes what running a child correctly
//! costs: its own process group so cancellation reaches transport and credential
//! helpers, both pipes drained concurrently so neither can deadlock against the
//! other, a poll loop that honours a deadline and a cancellation token, and only
//! the tail of the diagnostic stream retained. That is not Git-specific
//! behaviour — it is what *any* tool spawning a program owes its caller — so it
//! is generalized here rather than wrapped, and every property is restated in
//! terms the tool contract already has.
//!
//! Three things are different from the Git runner, and each follows from this
//! being a tool rather than one hard-coded program.
//!
//! - **Output goes somewhere durable.** Git's runner returns stdout to its
//!   caller because a Git verb's output is small and structured. A tool's child
//!   can emit a gigabyte, so each stream is streamed into an artifact as it
//!   arrives and only [`ExecutionContext::stream_tail_bytes`] of it is retained
//!   in memory. Peak memory is the tail plus one read buffer, whatever the child
//!   produces.
//! - **The deadline is the call's, not the command's.** A tool does not get to
//!   invent a limit; it inherits the one the executor put on the
//!   [`ExecutionContext`], so a child cannot outlive the call that started it.
//! - **Progress is typed.** Stderr segments become [`ProgressEvent::message`]
//!   through the context's sink, so a front end renders a running child the same
//!   way it renders anything else.
//!
//! # The environment is scrubbed, not inherited
//!
//! A tool's child inherits nothing that could redirect it, for the same reason
//! `harkness-git` pins its own: `harkness-cli` runs from hooks and from inside
//! other processes, so "the environment" is not a place a decision may come
//! from. Only what a tool names explicitly is passed.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use super::{
    ArtifactRef, ArtifactStream, ExecutionContext, POLL_INTERVAL, ProgressEvent, ToolError,
};

/// Bytes read from a child's pipe in one go.
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// Stderr segments held between the reader thread and the wait loop.
///
/// Bounded, so a child flooding standard error applies backpressure through its
/// own pipe rather than growing a queue in this process. The wait loop drains it
/// every [`POLL_INTERVAL`].
const SEGMENT_CHANNEL_CAPACITY: usize = 256;

/// Longest one progress segment grows before it is reported as it stands.
///
/// A segment ends at a newline or a carriage return, and a child is under no
/// obligation to emit either: a program printing a megabyte with no separator
/// would otherwise accumulate all of it in the reader thread, which is the one
/// unbounded buffer the streaming design exists to avoid. Cutting at a bound
/// costs a split message and keeps the promise.
const MAX_SEGMENT_BYTES: usize = 8 * 1024;

/// Where one of a child's output streams goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Capture {
    /// Kept only as a bounded tail, for a failure message.
    Tail,

    /// Streamed into an artifact under this name, with a tail kept as well.
    ///
    /// The artifact holds every byte; the tail is what a failure quotes.
    Artifact {
        /// Label the artifact is recorded under.
        name: String,
        /// IANA media type the content is stored as.
        media_type: String,
    },
}

impl Capture {
    /// Streams into a `text/plain` artifact named `name`.
    #[must_use]
    pub fn artifact(name: impl Into<String>) -> Self {
        Self::Artifact {
            name: name.into(),
            media_type: "text/plain".to_owned(),
        }
    }

    /// Streams into an artifact stored under an explicit media type.
    #[must_use]
    pub fn artifact_as(name: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self::Artifact {
            name: name.into(),
            media_type: media_type.into(),
        }
    }
}

/// What one of a child's streams produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedStream {
    tail: String,
    byte_len: u64,
    truncated: bool,
    artifact: Option<ArtifactRef>,
}

impl CapturedStream {
    /// The retained end of the stream, decoded lossily.
    ///
    /// Lossy because a child's output is bytes and need not be UTF-8, and a
    /// failure message is worth more than a decoding error. A cut at the tail
    /// boundary can also split a multi-byte character, which is exactly the case
    /// lossy decoding exists for.
    #[must_use]
    pub fn tail(&self) -> &str {
        &self.tail
    }

    /// Total bytes the stream produced, whatever was retained.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Whether the stream produced more than the tail holds.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The artifact holding every byte, when the stream was captured into one.
    #[must_use]
    pub const fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }
}

/// What one finished child process produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    code: Option<i32>,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl ProcessOutput {
    /// The status the child reported, or `None` when a signal ended it.
    #[must_use]
    pub const fn code(&self) -> Option<i32> {
        self.code
    }

    /// Whether the child exited zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    /// What the child wrote to standard output.
    #[must_use]
    pub const fn stdout(&self) -> &CapturedStream {
        &self.stdout
    }

    /// What the child wrote to standard error.
    #[must_use]
    pub const fn stderr(&self) -> &CapturedStream {
        &self.stderr
    }

    /// Returns the output, or a typed failure when the child did not exit zero.
    ///
    /// The shape most tools want: an exit status is not something a tool should
    /// have to remember to check, and forgetting produces a call that succeeds
    /// while its work did not.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ProcessFailed`] carrying the status and the retained
    /// end of standard error.
    pub fn require_success(self) -> Result<Self, ToolError> {
        if self.succeeded() {
            return Ok(self);
        }
        Err(ToolError::ProcessFailed {
            code: self.code,
            stderr_tail: self.stderr.tail.clone(),
        })
    }
}

/// One child process a tool runs under its call's limits.
///
/// Built rather than spawned directly, so every invocation carries the same
/// guarantees: its own process group, both pipes drained concurrently, a
/// scrubbed environment, no inherited standard input, and a wait loop bounded by
/// the call's deadline and cancellation token.
#[derive(Clone, Debug)]
pub struct ToolProcess {
    program: OsString,
    arguments: Vec<OsString>,
    working_directory: Option<PathBuf>,
    environment: Vec<(OsString, OsString)>,
    stdout: Capture,
    stderr: Capture,
}

impl ToolProcess {
    /// Prepares an invocation of `program`.
    #[must_use]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            arguments: Vec::new(),
            working_directory: None,
            environment: Vec::new(),
            stdout: Capture::Tail,
            stderr: Capture::Tail,
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_os_string());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.arguments.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_os_string()),
        );
        self
    }

    /// Runs the child in `directory` rather than in the calling process's.
    #[must_use]
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Sets one environment variable for the child.
    ///
    /// The child's environment is otherwise empty: nothing is inherited, so a
    /// variable a hook or a parent process exported cannot change what a tool
    /// does. A tool that needs `PATH` or `HOME` asks for it here.
    #[must_use]
    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.environment
            .push((name.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Decides what becomes of the child's standard output.
    #[must_use]
    pub fn capture_stdout(mut self, capture: Capture) -> Self {
        self.stdout = capture;
        self
    }

    /// Decides what becomes of the child's standard error.
    #[must_use]
    pub fn capture_stderr(mut self, capture: Capture) -> Self {
        self.stderr = capture;
        self
    }

    /// Runs the child to completion under the call's limits.
    ///
    /// Blocks until the child exits, the call is cancelled, or the call's
    /// deadline passes. On either of the last two the child's whole *process
    /// group* is killed, so helpers it started stop with it rather than
    /// outliving the call that is already being reported as over.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Cancelled`] or [`ToolError::TimedOut`] when the call
    /// was stopped, and [`ToolError::ExecutionFailed`] when the child could not
    /// be launched or its output could not be stored. A non-zero exit is *not*
    /// an error here — see [`ProcessOutput::require_success`] — because some
    /// programs answer a question through their status.
    pub fn run(self, context: &mut ExecutionContext) -> Result<ProcessOutput, ToolError> {
        // Checked before the spawn as well as inside the loop: a child dispatched
        // after the call was already stopped must never start, exactly as the
        // invocation pipeline gates the tool body itself.
        context.check_still_permitted()?;

        let tail_bytes = context.stream_tail_bytes();
        let deadline = context.deadline();

        // Both artifact streams are opened before anything is spawned. Opening
        // one after the child is running would mean discovering that storage is
        // unavailable with a process already writing into a pipe nobody will
        // read.
        let stdout_artifact = open_capture(context, &self.stdout)?;
        let stderr_artifact = open_capture(context, &self.stderr)?;

        let mut child = self.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (segments, incoming) = mpsc::sync_channel(SEGMENT_CHANNEL_CAPACITY);

        // Two readers, always. Piping both streams and draining only one
        // deadlocks the moment the child fills the pipe buffer of the other.
        let stdout_reader = thread::spawn(move || drain(stdout, tail_bytes, stdout_artifact, None));
        let stderr_reader =
            thread::spawn(move || drain(stderr, tail_bytes, stderr_artifact, Some(segments)));

        loop {
            forward(context, &incoming);

            if context.cancellation().is_cancelled() {
                terminate(&mut child, incoming, stdout_reader, stderr_reader);
                return Err(ToolError::Cancelled);
            }
            if let Some(deadline) = deadline
                && deadline.has_passed()
            {
                terminate(&mut child, incoming, stdout_reader, stderr_reader);
                return Err(ToolError::TimedOut {
                    limit: deadline.limit(),
                });
            }

            let waited = child.try_wait().map_err(|error| {
                ToolError::execution_failed(format!(
                    "a child process could not be waited on: {error}"
                ))
            })?;
            if let Some(status) = waited {
                // The readers are joined before anything is reported, so the
                // byte counts and the artifacts describe the whole stream rather
                // than however much had arrived when the child exited.
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                forward(context, &incoming);
                return Ok(ProcessOutput {
                    code: status.code(),
                    stdout: stdout.finish()?,
                    stderr: stderr.finish()?,
                });
            }

            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Starts the child with the invocation policy every tool process carries.
    fn spawn(&self) -> Result<Child, ToolError> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .env_clear()
            .envs(self.environment.iter().map(|(name, value)| (name, value)))
            // Closed rather than inherited: a front end has no terminal to answer
            // a prompt on, so a child that reaches for one must fail rather than
            // hang on a question nobody can see.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        // The child leads its own group, which is what makes cancellation and
        // the deadline able to stop a whole tree rather than only its root.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        command.spawn().map_err(|error| {
            ToolError::execution_failed(format!(
                "{} could not be started: {error}",
                self.program.to_string_lossy()
            ))
        })
    }
}

/// Opens the artifact a capture streams into, if it streams into one.
fn open_capture(
    context: &mut ExecutionContext,
    capture: &Capture,
) -> Result<Option<Box<dyn ArtifactStream>>, ToolError> {
    match capture {
        Capture::Tail => Ok(None),
        Capture::Artifact { name, media_type } => context.open_artifact(name, media_type).map(Some),
    }
}

/// Hands the wait loop's queued stderr segments to the call's progress sink.
fn forward(context: &mut ExecutionContext, incoming: &Receiver<String>) {
    while let Ok(segment) = incoming.try_recv() {
        context.report(ProgressEvent::message(segment));
    }
}

/// Kills the child's process group and waits for its readers to drain.
///
/// The receiver is consumed rather than borrowed, and dropped *before* the
/// joins. A reader blocked on a full segment channel would otherwise never
/// return, and joining it would hang the very path that exists to stop things:
/// dropping the receiver makes its next send fail, which is how a blocked reader
/// learns nobody is listening.
fn terminate(
    child: &mut Child,
    incoming: Receiver<String>,
    stdout_reader: JoinHandle<Drained>,
    stderr_reader: JoinHandle<Drained>,
) {
    terminate_process_group(child);
    drop(incoming);
    let _ = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child) {
    // `process_group(0)` made the child's PID its process-group ID, so a
    // negative target signals it and every helper still in the group at once.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Child) {
    let _ = child.kill();
}

/// One drained stream: its tail, its size, and the artifact holding the rest.
#[derive(Default)]
struct Drained {
    tail: Tail,
    artifact: Option<Box<dyn ArtifactStream>>,
    failure: Option<String>,
}

impl Drained {
    /// Finalizes the artifact and reports what the stream produced.
    fn finish(self) -> Result<CapturedStream, ToolError> {
        if let Some(failure) = self.failure {
            return Err(ToolError::execution_failed(failure));
        }
        let artifact = self.artifact.map(ArtifactStream::finish).transpose()?;
        Ok(CapturedStream {
            tail: String::from_utf8_lossy(self.tail.retained()).into_owned(),
            byte_len: self.tail.total,
            truncated: self.tail.truncated,
            artifact,
        })
    }
}

/// Reads one stream to its end, storing every byte and retaining the last few.
///
/// A read failure ends the drain with what arrived before it. The exit status
/// and the child's own diagnostics decide the outcome of a command, never this —
/// but a *storage* failure is different, because it means the artifact does not
/// hold what a later reader will be told it holds, so it is carried out as an
/// error rather than swallowed.
fn drain(
    stream: impl Read,
    tail_bytes: usize,
    artifact: Option<Box<dyn ArtifactStream>>,
    segments: Option<SyncSender<String>>,
) -> Drained {
    use std::io::Write as _;

    let mut drained = Drained {
        tail: Tail::new(tail_bytes),
        artifact,
        failure: None,
    };
    let mut reader = std::io::BufReader::new(stream);
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    let mut segment = Vec::new();
    // Cleared when the consumer goes away, which happens while the call is being
    // stopped. Reading carries on regardless, so the artifact still receives
    // whatever the child had already written.
    let mut segments = segments;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let chunk = &buffer[..read];

        if let Some(artifact) = drained.artifact.as_mut()
            && let Err(error) = artifact.write_all(chunk)
        {
            drained.failure = Some(format!("a captured stream could not be stored: {error}"));
            drained.artifact = None;
        }
        drained.tail.push(chunk);

        if let Some(sender) = segments.as_ref()
            && !split_segments(sender, chunk, &mut segment)
        {
            segments = None;
        }
    }

    if let Some(sender) = segments.as_ref() {
        let _ = send_segment(sender, &mut segment);
    }
    drained
}

/// Splits a chunk into segments and forwards each completed one.
///
/// Returns `false` once nobody is receiving, so the caller can stop trying.
///
/// Both separators end a segment. A program that overwrites a progress line with
/// a carriage return and only emits a newline when the phase ends would
/// otherwise report nothing for the whole of its slowest phase.
fn split_segments(segments: &SyncSender<String>, chunk: &[u8], segment: &mut Vec<u8>) -> bool {
    for &byte in chunk {
        if byte == b'\n' || byte == b'\r' {
            if !send_segment(segments, segment) {
                return false;
            }
        } else {
            segment.push(byte);
            // A child that never emits a separator must not be able to make this
            // buffer as large as its output.
            if segment.len() >= MAX_SEGMENT_BYTES && !send_segment(segments, segment) {
                return false;
            }
        }
    }
    true
}

/// Sends one accumulated segment, dropping it when it is only whitespace.
///
/// Returns `false` only when the receiver has gone away.
fn send_segment(segments: &SyncSender<String>, segment: &mut Vec<u8>) -> bool {
    if segment.is_empty() {
        return true;
    }
    let message = String::from_utf8_lossy(segment).trim().to_owned();
    segment.clear();
    if message.is_empty() {
        return true;
    }
    segments.send(message).is_ok()
}

/// The last `capacity` bytes of a stream, and how many went past.
///
/// A ring would avoid the copy; a `Vec` that drops its front is chosen instead
/// because the copy is bounded by the capacity and happens once per read, while
/// a ring has to be unwound before it can be read and gets that unwinding wrong
/// silently. The bound is what matters: memory here is the capacity plus one
/// read buffer whatever the child emits.
#[derive(Debug, Default)]
struct Tail {
    retained: Vec<u8>,
    capacity: usize,
    total: u64,
    truncated: bool,
}

impl Tail {
    fn new(capacity: usize) -> Self {
        Self {
            retained: Vec::new(),
            capacity,
            total: 0,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if self.capacity == 0 {
            self.truncated |= !chunk.is_empty();
            return;
        }

        if chunk.len() >= self.capacity {
            self.truncated = true;
            self.retained.clear();
            self.retained
                .extend_from_slice(&chunk[chunk.len() - self.capacity..]);
            return;
        }

        let overflow = (self.retained.len() + chunk.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.truncated = true;
            self.retained.drain(..overflow);
        }
        self.retained.extend_from_slice(chunk);
    }

    fn retained(&self) -> &[u8] {
        &self.retained
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{MAX_SEGMENT_BYTES, Tail, split_segments};

    #[test]
    fn a_child_that_never_emits_a_separator_does_not_grow_an_unbounded_segment() {
        // The one buffer in the reader that a child controls the size of. Left
        // unbounded, a program printing a megabyte on one line would hold all of
        // it in memory while the artifact beside it streams perfectly happily.
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut segment = Vec::new();

        for _ in 0..4 {
            assert!(split_segments(&sender, &[b'x'; 8 * 1024], &mut segment));
            assert!(
                segment.len() <= MAX_SEGMENT_BYTES,
                "the segment grew to {} bytes",
                segment.len()
            );
        }
        drop(sender);

        let reported = receiver.iter().collect::<Vec<_>>();
        assert_eq!(reported.len(), 4, "each full segment should be reported");
        assert!(
            reported
                .iter()
                .all(|message| message.len() == MAX_SEGMENT_BYTES)
        );
    }

    #[test]
    fn both_separators_end_a_segment_and_blank_ones_are_dropped() {
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut segment = Vec::new();

        // A carriage return has to end a segment too: a program overwriting its
        // progress line emits a newline only when the phase ends, so a
        // line-oriented reader reports nothing for the whole of the slowest one.
        assert!(split_segments(
            &sender,
            b"cloning...\nreceiving:  50%\rreceiving: 100%\n\n  \n",
            &mut segment
        ));
        drop(sender);

        assert_eq!(
            receiver.iter().collect::<Vec<_>>(),
            ["cloning...", "receiving:  50%", "receiving: 100%"]
        );
    }

    #[test]
    fn a_tail_keeps_the_end_of_a_stream_and_counts_all_of_it() {
        let mut tail = Tail::new(8);
        for _ in 0..1_000 {
            tail.push(b"0123456789");
        }
        tail.push(b"END");

        assert_eq!(tail.retained(), b"56789END");
        assert_eq!(tail.retained().len(), 8, "the tail is exactly its capacity");
        assert_eq!(tail.total, 10_003);
        assert!(tail.truncated);
    }

    #[test]
    fn a_stream_within_the_bound_is_retained_whole_and_not_marked_truncated() {
        let mut tail = Tail::new(16);
        tail.push(b"short");
        tail.push(b" output");

        assert_eq!(tail.retained(), b"short output");
        assert_eq!(tail.total, 12);
        assert!(!tail.truncated);
    }

    #[test]
    fn one_chunk_larger_than_the_bound_keeps_only_its_end() {
        // The case a naive implementation gets wrong by appending first: a
        // single read larger than the whole capacity must not be assembled
        // before it is cut, or the bound is a bound on the average rather than
        // on the peak.
        let mut tail = Tail::new(4);
        tail.push(&[b'x'; 4096]);

        assert_eq!(tail.retained(), b"xxxx");
        assert_eq!(tail.total, 4096);
        assert!(tail.truncated);
    }

    #[test]
    fn a_zero_capacity_tail_retains_nothing_but_still_counts() {
        let mut tail = Tail::new(0);
        tail.push(b"discarded");

        assert!(tail.retained().is_empty());
        assert_eq!(tail.total, 9);
        assert!(tail.truncated);
    }
}
