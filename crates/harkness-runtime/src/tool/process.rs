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
//! On Unix the portable supervision boundary is the process group. A hostile
//! descendant can deliberately create a new session and leave that boundary;
//! it cannot be killed portably without broader operating-system authority.
//! Output readers are nevertheless cancellable, so such a detached process can
//! neither hold the call open nor defeat its timeout by retaining a pipe.
//!
//! Three things are different from the Git runner, and each follows from this
//! being a tool rather than one hard-coded program.
//!
//! - **Output goes somewhere durable.** Git's runner returns stdout to its
//!   caller because a Git verb's output is small and structured. A tool's child
//!   can emit a gigabyte, so each stream is streamed into an artifact as it
//!   arrives and only [`ExecutionContext::stream_tail_bytes`] of it is retained
//!   in memory. A caller may also cap the artifact while the reader keeps
//!   draining and counting the complete stream. Peak memory is the tail plus one
//!   read buffer, whatever the child produces.
//! - **The deadline is the call's, not the command's.** A tool does not get to
//!   invent a limit; it inherits the one the executor put on the
//!   [`ExecutionContext`], so a child cannot outlive the call that started it.
//! - **Progress is typed.** Stderr segments become [`ProgressEvent::message`]
//!   through the context's sink, so a front end renders a running child the same
//!   way it renders anything else.
//!
//! # The environment is scrubbed, not inherited
//!
//! A tool's child starts with an empty environment, for the same reason
//! `harkness-git` pins its own: `harkness-cli` runs from hooks and from inside
//! other processes, so "the environment" is not a place a decision may come
//! from. Only the fixed baseline and exact names published by the tool are
//! copied into it.

use std::ffi::OsString;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

use super::{
    ArtifactRef, ArtifactStream, ExecutionContext, POLL_INTERVAL, ProgressEvent, ToolError,
};
use crate::observe;
use crate::trust::{AllowlistedEnv, CommandSpec, ContainedPath};

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

/// Segments the wait loop forwards before returning to its own checks.
///
/// A child that talks continuously would otherwise keep the loop inside its
/// forwarding step indefinitely, and cancellation and the deadline are only
/// tested between steps. Leaving the rest queued costs nothing — the queue is
/// bounded and backpressures the child through its own pipe — while a bound here
/// is what makes the advertised poll cadence true of a chatty child as well as a
/// quiet one.
const MAX_SEGMENTS_PER_POLL: usize = 64;

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

    /// Streams at most `max_bytes` into an artifact while draining the rest.
    BoundedArtifact {
        /// Label the artifact is recorded under.
        name: String,
        /// IANA media type the content is stored as.
        media_type: String,
        /// Most child-controlled bytes admitted to artifact storage.
        max_bytes: u64,
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

    /// Streams at most `max_bytes` into a `text/plain` artifact named `name`.
    ///
    /// Bytes beyond the cap are still drained from the child and included in
    /// [`CapturedStream::byte_len`], so applying a storage bound cannot
    /// deadlock the child on a full pipe or make its emitted-byte count lie.
    #[must_use]
    pub fn bounded_artifact(name: impl Into<String>, max_bytes: u64) -> Self {
        Self::BoundedArtifact {
            name: name.into(),
            media_type: "text/plain".to_owned(),
            max_bytes,
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
    artifact_truncated: bool,
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

    /// The artifact holding captured bytes, when the stream was stored in one.
    ///
    /// A bounded capture contains only the prefix admitted by its byte cap; use
    /// [`Self::artifact_is_truncated`] to distinguish that from a complete one.
    #[must_use]
    pub const fn artifact(&self) -> Option<&ArtifactRef> {
        self.artifact.as_ref()
    }

    /// Whether a caller-selected byte cap omitted stream bytes from the artifact.
    #[must_use]
    pub const fn artifact_is_truncated(&self) -> bool {
        self.artifact_truncated
    }
}

/// What one finished child process produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    duration: Duration,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl ProcessOutput {
    /// The status the child reported, or `None` when a signal ended it.
    #[must_use]
    pub const fn code(&self) -> Option<i32> {
        self.code
    }

    /// Signal that ended the child, on platforms that expose one.
    #[must_use]
    pub const fn signal(&self) -> Option<i32> {
        self.signal
    }

    /// Whether this process reached its tool-supplied local timeout.
    ///
    /// A call-level timeout remains [`ToolError::TimedOut`]; this flag belongs
    /// to a shorter bound the tool deliberately placed on its child so the tool
    /// can return a typed result describing that termination.
    #[must_use]
    pub const fn timed_out(&self) -> bool {
        self.timed_out
    }

    /// Wall-clock time from immediately before spawn through output draining.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
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

struct SupervisedChild {
    child: Child,
    #[cfg(windows)]
    job: WindowsJob,
}

#[cfg(windows)]
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

#[cfg(windows)]
const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

#[cfg(windows)]
#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    priority_delta: i32,
    flags: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> HANDLE;
    fn Thread32First(snapshot: HANDLE, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: HANDLE, entry: *mut ThreadEntry32) -> i32;
}

#[cfg(windows)]
struct WindowsHandle(HANDLE);

#[cfg(windows)]
impl Drop for WindowsHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollAction {
    Exited,
    Cancelled,
    Deadline,
    LocalTimeout,
    Running,
}

fn poll_action(
    exited: bool,
    cancelled: bool,
    deadline_passed: bool,
    local_timeout_passed: bool,
) -> PollAction {
    if exited {
        PollAction::Exited
    } else if cancelled {
        PollAction::Cancelled
    } else if deadline_passed {
        PollAction::Deadline
    } else if local_timeout_passed {
        PollAction::LocalTimeout
    } else {
        PollAction::Running
    }
}

impl std::ops::Deref for SupervisedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for SupervisedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: HANDLE,
}

#[cfg(windows)]
impl WindowsJob {
    fn new() -> Result<Self, ToolError> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(ToolError::execution_failed(format!(
                "a Windows Job Object could not be created: {}",
                io::Error::last_os_error()
            )));
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX),
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(ToolError::execution_failed(format!(
                "a Windows Job Object could not be configured: {error}"
            )));
        }
        Ok(Self { handle })
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        if unsafe { AssignProcessToJobObject(self.handle, process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn resume(&self, child: &Child) -> io::Result<()> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = WindowsHandle(snapshot);
        let mut entry = ThreadEntry32 {
            size: u32::try_from(std::mem::size_of::<ThreadEntry32>()).unwrap_or(u32::MAX),
            usage: 0,
            thread_id: 0,
            owner_process_id: 0,
            base_priority: 0,
            priority_delta: 0,
            flags: 0,
        };
        let mut found = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
        while found {
            if entry.owner_process_id == child.id() {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = WindowsHandle(thread);
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            found = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the suspended child's primary thread was not present in the system snapshot",
        ))
    }

    fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
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
    working_directory: ContainedPath,
    environment: AllowlistedEnv,
    stdout: Capture,
    stderr: Capture,
    local_timeout: Option<Duration>,
}

impl ToolProcess {
    /// Prepares an invocation from an argv-only, contained command description.
    #[must_use]
    pub fn new(spec: CommandSpec) -> Self {
        let (program, arguments, working_directory, environment) = spec.into_parts();
        Self {
            program,
            arguments,
            working_directory,
            environment,
            stdout: Capture::Tail,
            stderr: Capture::Tail,
            local_timeout: None,
        }
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

    /// Adds a process-local timeout inside the enclosing call's deadline.
    ///
    /// Reaching this bound kills the supervised process unit and returns a
    /// [`ProcessOutput`] with [`ProcessOutput::timed_out`] set. The enclosing
    /// execution-context deadline still wins when it is earlier and remains a
    /// [`ToolError::TimedOut`], because that limit ends the whole tool call.
    #[must_use]
    pub const fn within(mut self, timeout: Duration) -> Self {
        self.local_timeout = Some(timeout);
        self
    }

    /// Runs the child to completion under the call's limits.
    ///
    /// Blocks until the child exits, the call is cancelled, or the call's
    /// deadline passes. On either of the last two the child's supervised unit
    /// is killed: its process group on Unix and its Job Object on Windows. Unix
    /// descendants that deliberately detach into another session are outside
    /// that portable boundary, but cannot hold this call open through inherited
    /// output pipes.
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

        // Artifact setup is allowed to block and can be implemented by an
        // external store. Re-check at the last point before launch so a cancel
        // or deadline observed while opening either stream cannot start work
        // after the executor has already decided the call is terminal.
        context.check_still_permitted()?;
        let local_deadline = self
            .local_timeout
            .and_then(|limit| Instant::now().checked_add(limit).map(|at| (limit, at)));
        let started = Instant::now();
        let mut child = self.spawn(context)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        if let Err(error) =
            set_pipe_nonblocking(&stdout).and_then(|()| set_pipe_nonblocking(&stderr))
        {
            terminate_process_group(&mut child);
            drop(stdout);
            drop(stderr);
            let _ = child.wait();
            return Err(error);
        }
        let (segments, incoming) = mpsc::sync_channel(SEGMENT_CHANNEL_CAPACITY);
        let stop_readers = Arc::new(AtomicBool::new(false));

        // Two readers, always. Piping both streams and draining only one
        // deadlocks the moment the child fills the pipe buffer of the other.
        let stdout_stop = Arc::clone(&stop_readers);
        let stderr_stop = Arc::clone(&stop_readers);
        let stdout_reader =
            thread::spawn(move || drain(stdout, tail_bytes, stdout_artifact, None, stdout_stop));
        let stderr_reader = thread::spawn(move || {
            drain(
                stderr,
                tail_bytes,
                stderr_artifact,
                Some(segments),
                stderr_stop,
            )
        });

        loop {
            forward(context, &incoming, MAX_SEGMENTS_PER_POLL);

            let waited = match child.try_wait() {
                Ok(waited) => waited,
                Err(error) => {
                    // The one path that must not simply return: a child this
                    // process has lost track of is still running, and dropping
                    // its handle neither kills nor reaps it. It would outlive
                    // the call it was bounded by while that call is recorded as
                    // failed.
                    terminate(
                        &mut child,
                        incoming,
                        stdout_reader,
                        stderr_reader,
                        &stop_readers,
                    );
                    return Err(ToolError::execution_failed(format!(
                        "a child process could not be waited on: {error}"
                    )));
                }
            };
            let action = poll_action(
                waited.is_some(),
                context.cancellation().is_cancelled(),
                deadline.is_some_and(|deadline| deadline.has_passed()),
                local_deadline.is_some_and(|(_, at)| Instant::now() >= at),
            );
            if let Some(status) = waited {
                // The group is ended before its output is collected, and that
                // ordering is the whole of what makes this return promptly. A
                // pipe reaches end of file only when *every* write end is
                // closed, so a child that started a background helper leaves one
                // open behind it: waiting for the readers would mean waiting for
                // however long the helper runs, long past the call. The group is
                // the unit of execution, so it is the unit that ends.
                //
                // Signalling after the child has been reaped is sound while any
                // member of the group is alive — the group keeps the identifier
                // reserved, so it cannot name anything else — and is a harmless
                // `ESRCH` once none is.
                terminate_process_group(&mut child);
                stop_readers.store(true, Ordering::Release);

                // The readers are then drained *while* they are waited on, never
                // after. A child can exit with its pipe still full, and the
                // reader turning that residue into segments blocks as soon as
                // the bounded queue fills — so joining first would wait on a
                // thread that is waiting on this one. The symptom is not a slow
                // call but one that never ends.
                //
                // They still finish before anything is reported, so the byte
                // counts and the artifacts describe the whole stream rather than
                // however much had arrived when the child exited.
                let (stdout, stderr) =
                    drain_until_read(context, &incoming, stdout_reader, stderr_reader);
                return Ok(ProcessOutput {
                    code: status.code(),
                    signal: exit_signal(&status),
                    timed_out: false,
                    duration: started.elapsed(),
                    stdout: finish_stream(stdout, context)?,
                    stderr: finish_stream(stderr, context)?,
                });
            }

            // A status already produced belongs to the child even when this
            // poll also observes a cancellation or deadline. Checking it first
            // prevents a completed exit-zero process from becoming a timeout.
            match action {
                PollAction::Exited => unreachable!("an observed exit returned above"),
                PollAction::Cancelled => {
                    terminate(
                        &mut child,
                        incoming,
                        stdout_reader,
                        stderr_reader,
                        &stop_readers,
                    );
                    return Err(ToolError::Cancelled);
                }
                PollAction::Deadline => {
                    terminate(
                        &mut child,
                        incoming,
                        stdout_reader,
                        stderr_reader,
                        &stop_readers,
                    );
                    return Err(ToolError::TimedOut {
                        limit: deadline.expect("the action requires a deadline").limit(),
                    });
                }
                PollAction::LocalTimeout => {
                    let (status, stdout, stderr) = terminate_and_collect(
                        &mut child,
                        incoming,
                        stdout_reader,
                        stderr_reader,
                        &stop_readers,
                    );
                    return Ok(ProcessOutput {
                        code: status.as_ref().and_then(ExitStatus::code),
                        signal: status.as_ref().and_then(exit_signal),
                        timed_out: true,
                        duration: started.elapsed(),
                        stdout: finish_stream(stdout, context)?,
                        stderr: finish_stream(stderr, context)?,
                    });
                }
                PollAction::Running => {}
            }

            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Starts the child with the invocation policy every tool process carries.
    fn spawn(&self, context: &ExecutionContext) -> Result<SupervisedChild, ToolError> {
        // This is the moment a secret leaves Harkness's control: from here the
        // value can come back in the child's stdout, its stderr, an error
        // quoting the command line, or the tool's own result — all of which are
        // persisted. Declaring it before the spawn rather than after is what
        // makes every one of those channels covered by the time there is
        // anything to cover.
        //
        // Only the *names* are recorded. A name identifies which grant was
        // exercised; the value is the thing this whole mechanism exists to keep
        // out of the record.
        let declared = observe::declare_environment_secrets(
            &observe::SecretRegistry::process(),
            self.environment.iter(),
        );
        if !declared.is_empty() {
            tracing::debug!(
                run_id = %context.run(),
                step_id = %context.step(),
                tool_call_id = %context.call(),
                secrets = ?declared,
                "declared environment secrets before spawning a child"
            );
        }
        // `ContainedPath` is a point-in-time proof. Re-resolve immediately
        // before launch so a symlink retargeted while a call awaited approval
        // cannot redirect the child's working directory outside its grant.
        let working_directory = self
            .working_directory
            .revalidate()
            .map_err(ToolError::from)?;
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .env_clear()
            .envs(self.environment.iter())
            // Closed rather than inherited: a front end has no terminal to answer
            // a prompt on, so a child that reaches for one must fail rather than
            // hang on a question nobody can see.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.current_dir(working_directory.as_path());
        // The child leads its own group, which is what makes cancellation and
        // the deadline able to stop a whole tree rather than only its root.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_SUSPENDED);

        #[cfg(windows)]
        let job = WindowsJob::new()?;
        context.check_still_permitted()?;
        let child = command.spawn().map_err(|error| {
            ToolError::execution_failed(format!(
                "{} could not be started: {error}",
                self.program.to_string_lossy()
            ))
        })?;
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::execution_failed(format!(
                "{} could not be assigned to its Windows Job Object: {error}",
                self.program.to_string_lossy()
            )));
        }
        #[cfg(windows)]
        if let Err(error) = job.resume(&child) {
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::execution_failed(format!(
                "{} could not resume after assignment to its Windows Job Object: {error}",
                self.program.to_string_lossy()
            )));
        }
        Ok(SupervisedChild {
            child,
            #[cfg(windows)]
            job,
        })
    }
}

/// Opens the artifact a capture streams into, if it streams into one.
fn open_capture(
    context: &mut ExecutionContext,
    capture: &Capture,
) -> Result<OpenedCapture, ToolError> {
    match capture {
        Capture::Tail => Ok(OpenedCapture::default()),
        Capture::Artifact { name, media_type } => {
            context
                .open_artifact(name, media_type)
                .map(|artifact| OpenedCapture {
                    artifact: Some(artifact),
                    byte_limit: None,
                })
        }
        Capture::BoundedArtifact {
            name,
            media_type,
            max_bytes,
        } => context
            .open_artifact(name, media_type)
            .map(|artifact| OpenedCapture {
                artifact: Some(artifact),
                byte_limit: Some(*max_bytes),
            }),
    }
}

#[derive(Default)]
struct OpenedCapture {
    artifact: Option<Box<dyn ArtifactStream>>,
    byte_limit: Option<u64>,
}

#[cfg(unix)]
fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> Result<(), ToolError> {
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(ToolError::execution_failed(format!(
            "a child output pipe could not be made cancellable: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_pipe_nonblocking<T>(_pipe: &T) -> Result<(), ToolError> {
    Ok(())
}

/// Hands up to `most` queued stderr segments to the call's progress sink.
///
/// Bounded because both halves of this can block: a child flooding standard
/// error refills the queue as fast as it is read, and each report can wait on
/// the call's own bounded progress channel. An unbounded drain would therefore
/// keep the wait loop out of its cancellation and deadline checks for as long as
/// the child cares to talk, which is precisely when a user is most likely to be
/// pressing cancel. Returns how many it forwarded, so a caller can tell a
/// quiet queue from a busy one.
fn forward(context: &mut ExecutionContext, incoming: &Receiver<String>, most: usize) -> usize {
    let mut forwarded = 0;
    while forwarded < most {
        let Ok(segment) = incoming.try_recv() else {
            break;
        };
        context.report(ProgressEvent::message(segment));
        forwarded += 1;
    }
    forwarded
}

/// Drains the segment queue until both readers have finished, then joins them.
///
/// The child has already exited by the time this runs, so there is no deadline
/// left to honour and no reason to stop early: what remains is to move the
/// residue of both pipes out of the readers, and the only way to do that without
/// deadlocking against the bounded queue is to keep draining while they work.
///
/// Sleeping only when nothing was forwarded keeps a quiet child — the common
/// case, where both readers are already done — from paying a poll interval for
/// nothing.
fn drain_until_read(
    context: &mut ExecutionContext,
    incoming: &Receiver<String>,
    stdout_reader: JoinHandle<Drained>,
    stderr_reader: JoinHandle<Drained>,
) -> (Drained, Drained) {
    loop {
        let forwarded = forward(context, incoming, usize::MAX);
        if stdout_reader.is_finished() && stderr_reader.is_finished() {
            break;
        }
        if forwarded == 0 {
            thread::sleep(POLL_INTERVAL);
        }
    }

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    // Both readers are gone, so whatever they sent last is in the queue and
    // nothing more can arrive. `is_finished` says a thread has returned, not
    // that this one has observed everything it sent, which is why this final
    // drain is after the joins rather than before them.
    forward(context, incoming, usize::MAX);
    (stdout, stderr)
}

/// Kills the child's process group and keeps whatever it had already produced.
///
/// The receiver is consumed rather than borrowed, and dropped *before* the
/// joins. A reader blocked on a full segment channel would otherwise never
/// return, and joining it would hang the very path that exists to stop things:
/// dropping the receiver makes its next send fail, which is how a blocked reader
/// learns nobody is listening.
///
/// The captured streams are *finished*, not discarded. A build log is at its
/// most useful precisely when the build was killed, and an unfinished
/// [`ArtifactStream`] deletes the bytes it staged — so discarding here would
/// destroy the diagnostic in the one case somebody needs it. What arrived before
/// the kill is already complete and correct; only the rest is missing.
fn terminate(
    child: &mut SupervisedChild,
    incoming: Receiver<String>,
    stdout_reader: JoinHandle<Drained>,
    stderr_reader: JoinHandle<Drained>,
    stop_readers: &AtomicBool,
) {
    let (_, stdout, stderr) =
        terminate_and_collect(child, incoming, stdout_reader, stderr_reader, stop_readers);
    stdout.preserve();
    stderr.preserve();
}

/// Kills, reaps, and drains one process group without discarding captured bytes.
fn terminate_and_collect(
    child: &mut SupervisedChild,
    incoming: Receiver<String>,
    stdout_reader: JoinHandle<Drained>,
    stderr_reader: JoinHandle<Drained>,
    stop_readers: &AtomicBool,
) -> (Option<ExitStatus>, Drained, Drained) {
    terminate_process_group(child);
    stop_readers.store(true, Ordering::Release);
    // A reader may be blocked sending a stderr segment. Dropping the receiver
    // releases it before either join, while artifact capture continues.
    drop(incoming);
    let status = child.wait().ok();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    (status, stdout, stderr)
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

#[cfg(unix)]
fn terminate_process_group(child: &mut SupervisedChild) {
    // `process_group(0)` made the child's PID its process-group ID, so a
    // negative target signals it and every helper still in the group at once.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn terminate_process_group(child: &mut SupervisedChild) {
    child.job.terminate();
    let _ = child.kill();
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_group(child: &mut SupervisedChild) {
    let _ = child.kill();
}

/// One drained stream: its tail, its size, and the artifact holding the rest.
#[derive(Default)]
struct Drained {
    tail: Tail,
    artifact: Option<Box<dyn ArtifactStream>>,
    artifact_bytes_remaining: Option<u64>,
    artifact_truncated: bool,
    failure: Option<String>,
}

impl Drained {
    /// Records the artifact and discards everything else.
    ///
    /// What a stopped call wants: the bytes that did arrive are worth keeping,
    /// and there is no caller left to hand a [`CapturedStream`] to. Best effort,
    /// because the call is already failing for a reason worth more than this.
    fn preserve(self) {
        if let Some(artifact) = self.artifact {
            let _ = artifact.finish();
        }
    }

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
            artifact_truncated: self.artifact_truncated,
        })
    }
}

fn finish_stream(
    drained: Drained,
    context: &ExecutionContext,
) -> Result<CapturedStream, ToolError> {
    let mut stream = drained.finish()?;
    stream.tail = context.redact_text(&stream.tail);
    Ok(stream)
}

/// Reads one stream to its end, storing its admitted prefix and retaining its tail.
///
/// A read failure ends the drain with what arrived before it. The exit status
/// and the child's own diagnostics decide the outcome of a command, never this —
/// but a *storage* failure is different, because it means the artifact does not
/// hold what a later reader will be told it holds, so it is carried out as an
/// error rather than swallowed.
fn drain(
    stream: impl Read,
    tail_bytes: usize,
    capture: OpenedCapture,
    segments: Option<SyncSender<String>>,
    stop: Arc<AtomicBool>,
) -> Drained {
    use std::io::Write as _;

    let mut drained = Drained {
        tail: Tail::new(tail_bytes),
        artifact: capture.artifact,
        artifact_bytes_remaining: capture.byte_limit,
        artifact_truncated: false,
        failure: None,
    };
    let mut reader = std::io::BufReader::new(stream);
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    let mut segment = Vec::new();
    // Cleared when the consumer goes away, which happens while the call is being
    // stopped. Reading carries on regardless, so the artifact still receives
    // whatever the child had already written.
    let mut segments = segments;
    // Once supervision ends, drain a generous bounded residue. A killed group
    // leaves at most its pipe buffers behind, while an escaped session can keep
    // producing forever; the bound preserves the former without letting the
    // latter turn a timeout back into an unbounded join.
    let mut reads_after_stop = 64usize;

    loop {
        if stop.load(Ordering::Acquire) {
            if reads_after_stop == 0 {
                break;
            }
            reads_after_stop -= 1;
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            // A signal arriving mid-read is not the end of the stream, and
            // treating it as one would silently shorten the artifact.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                // Any other read failure means the recorded size and the
                // artifact describe a prefix, while `is_truncated` would say the
                // tail is the whole stream. A consumer cannot tell that from a
                // complete capture, so it is reported rather than absorbed.
                drained.failure = Some(format!("a captured stream could not be read: {error}"));
                break;
            }
        };
        let chunk = &buffer[..read];

        if let Some(artifact) = drained.artifact.as_mut() {
            let admitted = drained
                .artifact_bytes_remaining
                .map_or(chunk.len(), |remaining| {
                    usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(chunk.len())
                });
            if admitted < chunk.len() {
                drained.artifact_truncated = true;
            }
            if admitted > 0
                && let Err(error) = artifact.write_all(&chunk[..admitted])
            {
                drained.failure = Some(format!("a captured stream could not be stored: {error}"));
                drained.artifact = None;
            }
            if let Some(remaining) = drained.artifact_bytes_remaining.as_mut() {
                *remaining = remaining.saturating_sub(admitted as u64);
            }
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
            // Only bytes actually dropped make a stream truncated. A single read
            // of exactly the capacity, arriving first, loses nothing — and
            // `is_truncated` is read to decide whether the tail can be shown as
            // the whole output, so claiming otherwise would send a reader to an
            // artifact that says the same thing.
            self.truncated |= chunk.len() > self.capacity || !self.retained.is_empty();
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
    use std::io::{self, Cursor, Write};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, mpsc};

    use super::{
        ArtifactRef, ArtifactStream, MAX_SEGMENT_BYTES, OpenedCapture, PollAction,
        READ_BUFFER_BYTES, Tail, ToolError, drain, poll_action, split_segments,
    };

    #[derive(Clone, Default)]
    struct RecordedBytes(Arc<Mutex<Vec<u8>>>);

    struct RecordingArtifact(RecordedBytes);

    impl Write for RecordingArtifact {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ArtifactStream for RecordingArtifact {
        fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError> {
            let byte_len = self.0.0.lock().unwrap().len() as u64;
            Ok(ArtifactRef {
                id: "recorded".to_owned(),
                media_type: "text/plain".to_owned(),
                byte_len,
            })
        }
    }

    #[test]
    fn an_observed_exit_outranks_every_stop_signal_from_the_same_poll() {
        assert_eq!(poll_action(true, true, true, true), PollAction::Exited);
        assert_eq!(
            poll_action(false, false, false, true),
            PollAction::LocalTimeout
        );
    }

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
    fn a_stream_of_exactly_the_bound_is_not_reported_as_truncated() {
        // Nothing was dropped, so the tail *is* the whole stream — and
        // `is_truncated` is what decides whether it can be shown as such.
        let mut tail = Tail::new(4);
        tail.push(b"abcd");

        assert_eq!(tail.retained(), b"abcd");
        assert_eq!(tail.total, 4);
        assert!(!tail.truncated);

        // One byte more, and it is.
        tail.push(b"e");
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

    #[test]
    fn bounded_artifact_stores_only_the_prefix_but_drains_and_counts_every_byte() {
        let mut input = vec![b'x'; READ_BUFFER_BYTES * 3 + 17];
        input[..6].copy_from_slice(b"012345");
        let recorded = RecordedBytes::default();
        let drained = drain(
            Cursor::new(&input),
            4,
            OpenedCapture {
                artifact: Some(Box::new(RecordingArtifact(recorded.clone()))),
                byte_limit: Some(6),
            },
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let stream = drained.finish().unwrap();

        assert_eq!(recorded.0.lock().unwrap().as_slice(), b"012345");
        assert_eq!(stream.artifact().unwrap().byte_len, 6);
        assert_eq!(stream.byte_len(), input.len() as u64);
        assert_eq!(stream.tail(), "xxxx");
        assert!(stream.is_truncated());
        assert!(stream.artifact_is_truncated());
    }

    #[test]
    fn output_exactly_at_the_artifact_cap_is_complete() {
        let recorded = RecordedBytes::default();
        let stream = drain(
            Cursor::new(b"abcdef"),
            8,
            OpenedCapture {
                artifact: Some(Box::new(RecordingArtifact(recorded.clone()))),
                byte_limit: Some(6),
            },
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .finish()
        .unwrap();

        assert_eq!(recorded.0.lock().unwrap().as_slice(), b"abcdef");
        assert_eq!(stream.byte_len(), 6);
        assert!(!stream.artifact_is_truncated());
    }
}
