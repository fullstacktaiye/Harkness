//! The stdio implementation of [`JsonRpcTransport`].
//!
//! One connection is one child process and three threads: a reader draining
//! standard output into frames, a writer draining a bounded queue into standard
//! input, and a reader draining standard error into a [`StderrSink`]. Three,
//! because piping a stream and not draining it deadlocks as soon as the peer
//! fills the pipe buffer — the same reason `harkness-git`'s runner always starts
//! two readers, one pipe further along.
//!
//! Everything a peer controls the size of is bounded: the inbound and outbound
//! queues, the pending line, and the retained stderr tail. A peer that outruns
//! its consumer therefore blocks on its own pipe rather than growing this
//! process, which is what "backpressure" means here and why no queue is
//! unbounded.
//!
//! # Lifecycle
//!
//! ```text
//!            spawn ─────────────► running ─────────────► torn down
//!              │ (startup window)    │  (fault)              ▲
//!              │                     ▼                       │
//!              └───────────────► quarantined ────────────────┘
//! ```
//!
//! A connection leaves `running` once and never returns. The terminal state is
//! recorded the first time it is observed, so every later call gets the same
//! answer rather than a second, differently-shaped failure describing the same
//! event.

use std::{
    io::{self, Read, Write},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use harkness_git::Cancellation;

use crate::{
    error::{DesyncDetail, DisconnectKind, TransportError},
    frame::{LineSplitter, frame},
    message::Message,
    spawn::SpawnSpec,
    stderr::StderrSink,
    transport::{Counters, JsonRpcTransport, SendRejection, ShutdownOutcome, ShutdownRung},
};

/// Bytes read from a peer's pipe in one go.
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// How often a blocking loop re-checks cancellation and the startup deadline.
///
/// The workspace target is that cancellation becomes visible within 250 ms; this
/// is `harkness-git`'s own interval, an order of magnitude inside it, and small
/// enough that a caller waking to find nothing to do costs a syscall rather than
/// a stall.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Messages read from the peer and not yet taken by a caller.
const INBOUND_CAPACITY: usize = 256;

/// Messages queued for the peer and not yet written.
const OUTBOUND_CAPACITY: usize = 256;

/// How long an enqueue waits for room before it is a write failure.
///
/// The case this exists for is a peer that has stopped reading its own standard
/// input: the pipe fills, the writer thread blocks in `write_all`, the queue
/// fills behind it, and without a bound every later `send` would block forever
/// on a peer that is never coming back.
const OUTBOUND_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the peer's process group has to exit after `SIGTERM`.
///
/// Unix only, because the rung it bounds is: Windows has no `SIGTERM` to wait
/// out.
#[cfg(unix)]
const SIGNAL_GRACE: Duration = Duration::from_secs(2);

/// How long a dropped connection waits before it starts signalling.
///
/// A connection that is dropped rather than shut down still tears its child down
/// — an agent left running after the code that owned it is gone still holds the
/// workspace open — so `Drop` runs the same sequence with a grace period short
/// enough that dropping a connection is not a stall.
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long teardown waits for its own threads once the peer is gone.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// A JSON-RPC conversation with a child process over its standard streams.
pub struct StdioTransport {
    inner: Arc<Inner>,
    teardown: Mutex<Teardown>,
}

impl std::fmt::Debug for StdioTransport {
    /// Reports what the connection is doing, and nothing a peer wrote. Message
    /// payloads are never observable at this layer — redaction rules live with
    /// the adapters, where a field's meaning is known.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdioTransport")
            .field("terminal", &self.inner.terminal())
            .field(
                "handshake_done",
                &self.inner.handshake_done.load(Ordering::Relaxed),
            )
            .field("counters", &JsonRpcTransport::counters(self))
            .finish_non_exhaustive()
    }
}

/// Everything the connection's threads and its callers share.
struct Inner {
    outbound: Mutex<Option<SyncSender<WriterCommand>>>,
    inbound: Mutex<Receiver<ReaderEvent>>,
    /// Shared with the writer thread, which is the one participant that can
    /// discover a failure with no caller present to be told about it.
    state: Arc<Mutex<Option<Terminal>>>,
    counts: Arc<Counts>,
    cancel: Cancellation,
    started_at: Instant,
    startup_deadline: Duration,
    handshake_done: AtomicBool,
}

/// Everything teardown consumes, held apart so `Drop` and `shutdown` share it.
struct Teardown {
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    outcome: Option<ShutdownOutcome>,
}

#[derive(Default)]
struct Counts {
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    stderr_bytes: AtomicU64,
    outbound_depth: AtomicUsize,
    inbound_depth: AtomicUsize,
}

/// Why this connection stopped, recorded once.
///
/// A terminal state is sticky on purpose. The alternative — recomputing a
/// failure per call — reports a disconnect to the first caller and a closed
/// channel to the second, which is two different accounts of one event.
#[derive(Clone, Debug)]
enum Terminal {
    Disconnected(DisconnectKind),
    Cancelled,
    StartupDeadline(Duration),
    WriteFailed(String),
    Quarantined {
        fault_kind: &'static str,
        detail: String,
    },
}

impl Terminal {
    fn error(&self) -> TransportError {
        match self {
            Self::Disconnected(kind) => TransportError::Disconnected { kind: *kind },
            Self::Cancelled => TransportError::Cancelled,
            Self::StartupDeadline(deadline) => TransportError::StartupDeadlineExceeded {
                deadline: *deadline,
            },
            Self::WriteFailed(detail) => TransportError::WriteFailed {
                detail: detail.clone(),
            },
            Self::Quarantined { fault_kind, detail } => TransportError::Quarantined {
                fault_kind,
                detail: detail.clone(),
            },
        }
    }
}

/// What the writer thread is asked to do.
enum WriterCommand {
    /// Put one framed line on the peer's standard input.
    Write(String),
    /// Close the peer's standard input, which is the first rung of shutdown.
    Close,
}

/// What the reader thread reports.
enum ReaderEvent {
    Message(Message),
    Fault(TransportError),
    /// Standard output reached end of file. `partial` records whether the peer
    /// died part-way through a line, which is the only evidence available for
    /// [`DisconnectKind::MidResponse`].
    Closed {
        partial: bool,
    },
}

impl StdioTransport {
    /// Launches `spec`'s program and starts the conversation.
    ///
    /// Returns as soon as the child exists and its threads are running: the peer
    /// has not been spoken to yet, and the startup deadline is now counting
    /// against the adapter's handshake.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidSpawnSpec`] when the description cannot
    /// produce a hermetic invocation, [`TransportError::SpawnFailed`] when the
    /// operating system refuses to launch it, and [`TransportError::Cancelled`]
    /// when the token was already tripped — in which case nothing is launched at
    /// all.
    pub fn spawn(spec: SpawnSpec, cancel: Cancellation) -> Result<Self, TransportError> {
        spec.validate()?;
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }

        let (mut command, sink, limits) = spec.into_parts();
        let mut child = command
            .spawn()
            .map_err(|source| TransportError::SpawnFailed {
                program: limits.program.clone(),
                source,
            })?;

        let stdin = child.stdin.take().expect("standard input is piped");
        let stdout = child.stdout.take().expect("standard output is piped");
        let stderr = child.stderr.take().expect("standard error is piped");

        let counts = Arc::new(Counts::default());
        let (outbound, outbound_queue) = mpsc::sync_channel(OUTBOUND_CAPACITY);
        let (inbound_sender, inbound) = mpsc::sync_channel(INBOUND_CAPACITY);
        let state = Arc::new(Mutex::new(None));

        let reader = thread::spawn({
            let counts = Arc::clone(&counts);
            let limit = limits.max_message_bytes;
            move || read_messages(stdout, limit, &counts, &inbound_sender)
        });
        let writer = thread::spawn({
            let counts = Arc::clone(&counts);
            let state = Arc::clone(&state);
            move || write_messages(stdin, &outbound_queue, &counts, &state)
        });
        let stderr = thread::spawn({
            let counts = Arc::clone(&counts);
            move || drain_stderr(stderr, sink, &counts)
        });

        Ok(Self {
            inner: Arc::new(Inner {
                outbound: Mutex::new(Some(outbound)),
                inbound: Mutex::new(inbound),
                state,
                counts,
                cancel,
                started_at: Instant::now(),
                startup_deadline: limits.startup_deadline,
                handshake_done: AtomicBool::new(false),
            }),
            teardown: Mutex::new(Teardown {
                child: Some(child),
                reader: Some(reader),
                writer: Some(writer),
                stderr: Some(stderr),
                outcome: None,
            }),
        })
    }
}

impl Inner {
    /// The recorded terminal state, if this connection has one.
    fn terminal(&self) -> Option<Terminal> {
        self.state
            .lock()
            .expect("transport state is not poisoned")
            .clone()
    }

    /// Records `terminal` if nothing is recorded yet.
    fn record(&self, terminal: Terminal) {
        let mut state = self.state.lock().expect("transport state is not poisoned");
        if state.is_none() {
            *state = Some(terminal);
        }
    }

    /// Refuses the call when the connection has already ended.
    fn check_open(&self) -> Result<(), TransportError> {
        match self.terminal() {
            Some(terminal) => Err(terminal.error()),
            None => Ok(()),
        }
    }

    /// Refuses the call when cancellation has been requested.
    fn check_cancelled(&self) -> Result<(), TransportError> {
        if self.cancel.is_cancelled() {
            self.record(Terminal::Cancelled);
            self.close_stdin();
            return Err(TransportError::Cancelled);
        }
        Ok(())
    }

    /// Refuses the call when the peer never finished its handshake in time.
    fn check_startup(&self) -> Result<(), TransportError> {
        if self.handshake_done.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.started_at.elapsed() < self.startup_deadline {
            return Ok(());
        }
        self.record(Terminal::StartupDeadline(self.startup_deadline));
        self.close_stdin();
        Err(TransportError::StartupDeadlineExceeded {
            deadline: self.startup_deadline,
        })
    }

    /// Runs the three checks every blocking iteration owes its caller.
    fn check_all(&self) -> Result<(), TransportError> {
        self.check_open()?;
        self.check_cancelled()?;
        self.check_startup()
    }

    /// Closes the peer's standard input, which is shutdown's first rung.
    ///
    /// Best effort by design: the command is enqueued if there is room, and the
    /// transport's own sender is dropped either way, so a writer thread with a
    /// full queue still ends when it drains. A peer that ignores a closed
    /// standard input is what the signal rungs are for.
    fn close_stdin(&self) {
        let sender = self
            .outbound
            .lock()
            .expect("transport outbound queue is not poisoned")
            .take();
        if let Some(sender) = sender {
            let _ = sender.try_send(WriterCommand::Close);
        }
    }
}

impl JsonRpcTransport for StdioTransport {
    fn send(&self, message: Message, deadline: Instant) -> Result<(), TransportError> {
        // Whichever comes first. The caller's deadline is what it actually asked
        // for; the backstop bounds a caller that named a distant one, since a
        // peer that has stopped reading is never coming back and there is
        // nothing to wait for.
        let backstop = Instant::now() + OUTBOUND_ENQUEUE_TIMEOUT;
        let give_up_at = deadline.min(backstop);
        let mut message = message;
        loop {
            match self.try_send(message) {
                Ok(()) => return Ok(()),
                Err(SendRejection::NoRoom(returned)) => message = returned,
                Err(SendRejection::Failed(error)) => return Err(error),
            }
            if Instant::now() >= give_up_at {
                // Which bound ran out decides which failure this is. The
                // backstop expiring means the peer has not read a byte for
                // thirty seconds, which nothing but a broken peer explains, so
                // the connection ends. The caller's own deadline expiring means
                // only that this call was in a hurry — recording a terminal
                // state on its behalf would end a working session over one
                // impatient call.
                if Instant::now() < backstop {
                    return Err(TransportError::SendTimedOut);
                }
                let detail = "the peer is not reading its standard input".to_owned();
                self.inner.record(Terminal::WriteFailed(detail.clone()));
                return Err(TransportError::WriteFailed { detail });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn try_send(&self, message: Message) -> Result<(), SendRejection> {
        if let Err(error) = self.inner.check_all() {
            return Err(SendRejection::Failed(error));
        }

        // Asked before the message is encoded. A peer that has stopped reading
        // is retried against every poll interval, and serializing a large
        // message each time to discover there is still nowhere to put it is
        // work proportional to how long the peer stays wedged.
        if self.inner.counts.outbound_depth.load(Ordering::Relaxed) >= OUTBOUND_CAPACITY {
            return Err(SendRejection::NoRoom(message));
        }

        let sender = {
            let guard = self
                .inner
                .outbound
                .lock()
                .expect("transport outbound queue is not poisoned");
            match guard.as_ref() {
                Some(sender) => sender.clone(),
                None => return Err(SendRejection::Failed(self.closed_input())),
            }
        };
        let framed = match message.encode().and_then(|encoded| frame(&encoded)) {
            Ok(framed) => framed,
            Err(error) => return Err(SendRejection::Failed(error)),
        };

        // Counted *before* the handover and undone if it does not happen. The
        // writer subtracts the moment it dequeues, and it routinely wins that
        // race against a `fetch_add` placed after a successful send — which
        // takes an unsigned counter below zero and makes the depth a front end
        // shows for a stuck connection read `usize::MAX`.
        self.inner
            .counts
            .outbound_depth
            .fetch_add(1, Ordering::Relaxed);
        match sender.try_send(WriterCommand::Write(framed)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.inner
                    .counts
                    .outbound_depth
                    .fetch_sub(1, Ordering::Relaxed);
                Err(SendRejection::NoRoom(message))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner
                    .counts
                    .outbound_depth
                    .fetch_sub(1, Ordering::Relaxed);
                Err(SendRejection::Failed(self.closed_input()))
            }
        }
    }

    fn recv_deadline(&self, deadline: Instant) -> Result<Option<Message>, TransportError> {
        self.inner.check_open()?;

        loop {
            // Re-checked every iteration rather than only on entry: another
            // thread may quarantine the connection while this one waits, and a
            // caller left blocked until its deadline on a connection that is
            // already over learns the wrong thing about why.
            self.inner.check_open()?;
            self.inner.check_cancelled()?;
            self.inner.check_startup()?;

            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let slice = POLL_INTERVAL.min(deadline - now);
            let event = {
                let inbound = self
                    .inner
                    .inbound
                    .lock()
                    .expect("transport inbound queue is not poisoned");
                inbound.recv_timeout(slice)
            };
            match event {
                Ok(ReaderEvent::Message(message)) => {
                    self.inner
                        .counts
                        .inbound_depth
                        .fetch_sub(1, Ordering::Relaxed);
                    return Ok(Some(message));
                }
                Ok(ReaderEvent::Fault(fault)) => {
                    self.inner.record(Terminal::Quarantined {
                        fault_kind: fault.kind(),
                        detail: fault.to_string(),
                    });
                    self.inner.close_stdin();
                    return Err(fault);
                }
                Ok(ReaderEvent::Closed { partial }) => {
                    let kind = if partial {
                        DisconnectKind::MidResponse
                    } else {
                        DisconnectKind::Idle
                    };
                    self.inner.record(Terminal::Disconnected(kind));
                    return Err(TransportError::Disconnected { kind });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // The reader thread ended without reporting, which only
                    // happens when the connection is already being torn down.
                    self.inner
                        .record(Terminal::Disconnected(DisconnectKind::Idle));
                    return Err(self
                        .inner
                        .terminal()
                        .expect("a terminal state was just recorded")
                        .error());
                }
            }
        }
    }

    fn quarantine(&self, fault: &TransportError) {
        self.inner.record(Terminal::Quarantined {
            fault_kind: fault.kind(),
            detail: fault.to_string(),
        });
        self.inner.close_stdin();
    }

    fn handshake_complete(&self) {
        self.inner.handshake_done.store(true, Ordering::Release);
    }

    fn counters(&self) -> Counters {
        let counts = &self.inner.counts;
        Counters {
            bytes_read: counts.bytes_read.load(Ordering::Relaxed),
            bytes_written: counts.bytes_written.load(Ordering::Relaxed),
            outbound_depth: counts.outbound_depth.load(Ordering::Relaxed),
            inbound_depth: counts.inbound_depth.load(Ordering::Relaxed),
            outstanding_requests: 0,
            peer_depth: 0,
        }
    }

    fn shutdown(self: Box<Self>, grace: Duration) -> ShutdownOutcome {
        self.tear_down(grace)
    }
}

impl StdioTransport {
    /// Runs the shutdown sequence, once.
    ///
    /// `close stdin → wait → SIGTERM → wait → SIGKILL` is the MCP
    /// specification's sequence and generalizes `GitCommand`'s process-group
    /// termination: every signal targets the *group*, so a peer's own helpers
    /// stop with it rather than outliving the connection that started them.
    ///
    /// Idempotent, because `shutdown` consumes the transport and `Drop` then
    /// runs over the same fields; the second call returns the first one's
    /// answer rather than signalling a process that no longer exists.
    fn tear_down(&self, grace: Duration) -> ShutdownOutcome {
        let mut teardown = self
            .teardown
            .lock()
            .expect("transport teardown is not poisoned");
        if let Some(outcome) = &teardown.outcome {
            return outcome.clone();
        }

        // Unconditionally, and before the exit is inspected. The writer thread
        // is parked on its queue until every sender is gone, so a peer that had
        // already exited would otherwise leave a thread this loop then waits on
        // forever.
        self.inner.close_stdin();

        let mut child = teardown.child.take().expect("the child is taken once");
        let (rung, exit_code) = match child.try_wait() {
            Ok(Some(status)) => (ShutdownRung::AlreadyExited, status.code()),
            _ => match wait_for_exit(&mut child, grace) {
                Some(status) => (ShutdownRung::ClosedStdin, status),
                None => escalate(&mut child),
            },
        };

        // The direct child being gone is not the group being gone. A peer that
        // backgrounded a language server and then exited politely on a closed
        // standard input leaves that helper running on the workspace *and*
        // holding the standard-output pipe open, so the reader would never see
        // end of file either. Both rungs above can reach here with the group
        // still populated, which is why this is unconditional rather than part
        // of the escalation.
        //
        // The signal follows the child's reaping because that reaping is how the
        // exit was detected in the first place. While any member is alive the
        // group keeps its identifier, so the target is exact — that is the case
        // this exists for. Where the child *was* the last member the group is
        // already gone and the call is a harmless `ESRCH`, with a residual race
        // that the pid was recycled as another group's leader in between: a
        // window of microseconds against sequential pid allocation, stated here
        // rather than argued away, and the same trade `harkness-runtime` takes
        // when it signals a reaped tool child's group.
        reap_group(&child);

        // The reader can be parked on a full inbound queue, and nothing is
        // draining it now that the connection is over. Joining without draining
        // would wait for a consumer that is never coming.
        self.drain_inbound();
        let join_deadline = Instant::now() + JOIN_TIMEOUT;
        for handle in [
            teardown.reader.take(),
            teardown.writer.take(),
            teardown.stderr.take(),
        ]
        .into_iter()
        .flatten()
        {
            // One budget for all three, not one each. A per-handle deadline
            // makes the worst case three times the constant, and a blocking
            // `StderrSink` — which the trait only asks not to block for *long* —
            // is enough to reach it, on whatever thread happened to drop the
            // connection.
            while !handle.is_finished() && Instant::now() < join_deadline {
                self.drain_inbound();
                thread::sleep(POLL_INTERVAL);
            }
            // A thread still blocked on a descriptor is a situation with no good
            // answer, and hanging teardown is the worst of them: the caller is
            // told the connection is over and the thread is abandoned, exactly
            // as the tool executor abandons a body that outran its deadline.
            // Every process this connection started is already gone by here —
            // `reap_group` above is what makes that true — so this bound should
            // never be reached rather than being a routine outcome.
            if handle.is_finished() {
                let _ = handle.join();
            }
        }

        // Nothing is queued for a peer that is gone, and by here the writer's
        // receiver has been dropped too, so a `send` racing this teardown fails
        // and undoes its own count. Setting the depth rather than decrementing
        // toward it is what makes that exact: a message handed over between the
        // writer's own drain and its receiver disappearing is counted, dequeued
        // by nobody, and would otherwise leave a dead connection reporting work
        // in flight forever.
        self.inner.counts.outbound_depth.store(0, Ordering::Relaxed);

        let outcome = ShutdownOutcome {
            rung,
            exit_code,
            stderr_bytes: self.inner.counts.stderr_bytes.load(Ordering::Relaxed),
        };
        teardown.outcome = Some(outcome.clone());
        outcome
    }

    /// What a caller is told when the peer's standard input is no longer there.
    fn closed_input(&self) -> TransportError {
        self.inner.terminal().map_or_else(
            || TransportError::WriteFailed {
                detail: "the peer's standard input is closed".to_owned(),
            },
            |terminal| terminal.error(),
        )
    }

    /// Discards anything the reader has queued, so it can finish and be joined.
    ///
    /// Only a queued *message* was ever counted into the depth, so only a queued
    /// message is counted back out. Subtracting for a fault or an end-of-file
    /// marker as well would wrap the counter below zero.
    fn drain_inbound(&self) {
        let inbound = self
            .inner
            .inbound
            .lock()
            .expect("transport inbound queue is not poisoned");
        while let Ok(event) = inbound.try_recv() {
            if matches!(event, ReaderEvent::Message(_)) {
                self.inner
                    .counts
                    .inbound_depth
                    .fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.tear_down(DEFAULT_SHUTDOWN_GRACE);
    }
}

/// Polls for the child's exit until `grace` runs out.
///
/// A `try_wait` that *fails* answers `None`, the same as one that never saw the
/// child exit, so the caller escalates. Reporting a failed wait as a clean exit
/// would claim the peer left politely on the strength of the one call that could
/// not find out.
fn wait_for_exit(child: &mut Child, grace: Duration) -> Option<Option<i32>> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code()),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Kills whatever is left in the peer's process group once the peer is gone.
#[cfg(unix)]
fn reap_group(child: &Child) {
    signal_group(child, libc::SIGKILL);
}

/// Windows has no process group here, so a peer's grandchildren are not
/// reachable from this side. Stated rather than implied away, per ADR-0017.
#[cfg(not(unix))]
fn reap_group(_child: &Child) {}

/// Signals the peer's process group, escalating until it exits.
#[cfg(unix)]
fn escalate(child: &mut Child) -> (ShutdownRung, Option<i32>) {
    signal_group(child, libc::SIGTERM);
    if let Some(code) = wait_for_exit(child, SIGNAL_GRACE) {
        return (ShutdownRung::Signalled, code);
    }
    signal_group(child, libc::SIGKILL);
    let code = child.wait().ok().and_then(|status| status.code());
    (ShutdownRung::Killed, code)
}

/// Windows has no `SIGTERM`, so there is no rung between closing standard input
/// and killing. Reporting `Killed` for a peer that was terminated is the honest
/// answer rather than claiming an escalation that did not happen.
#[cfg(not(unix))]
fn escalate(child: &mut Child) -> (ShutdownRung, Option<i32>) {
    let _ = child.kill();
    let code = child.wait().ok().and_then(|status| status.code());
    (ShutdownRung::Killed, code)
}

/// Signals every process in the peer's group.
///
/// `process_group(0)` made the child's PID its group id, so a negative target
/// reaches the peer and everything it started. `ESRCH` from a group whose last
/// member has exited is expected and ignored.
#[cfg(unix)]
fn signal_group(child: &Child, signal: libc::c_int) {
    // SAFETY: `kill` takes no pointers, and the negated pid names the process
    // group `process_group(0)` created for this child and nothing else.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), signal);
    }
}

/// Frames the peer's standard output into messages.
///
/// Every outcome is an event, including the ones a peer causes on purpose: a
/// panic here would take the process down over somebody else's bad line, so
/// there is nothing in this loop that can panic on input.
fn read_messages(
    stdout: ChildStdout,
    limit: usize,
    counts: &Counts,
    events: &SyncSender<ReaderEvent>,
) {
    let mut stdout = stdout;
    let mut splitter = LineSplitter::new(limit);
    let mut buffer = [0u8; READ_BUFFER_BYTES];

    loop {
        let read = match stdout.read(&mut buffer) {
            Ok(0) => {
                let _ = events.send(ReaderEvent::Closed {
                    partial: splitter.has_partial_line(),
                });
                return;
            }
            Ok(read) => read,
            // A signal that arrived while this thread was parked in `read` says
            // nothing at all about the peer. `Read::read` does not retry it —
            // only the convenience methods built on it do — so a handler
            // installed without `SA_RESTART` anywhere in the process, by Qt or a
            // profiler or whatever embeds Harkness, would otherwise make a
            // healthy peer look like it had exited, and the disconnect is
            // sticky.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Any other read error means the peer's end of the pipe is gone; the
            // peer decides that, so it is a disconnect rather than a fault here.
            Err(_) => {
                let _ = events.send(ReaderEvent::Closed {
                    partial: splitter.has_partial_line(),
                });
                return;
            }
        };
        counts.bytes_read.fetch_add(read as u64, Ordering::Relaxed);

        let mut lines = Vec::new();
        let overflow = splitter
            .feed(&buffer[..read], &mut |line| lines.push(line.to_vec()))
            .err();

        for line in lines {
            let decoded = std::str::from_utf8(&line)
                .map_err(|source| TransportError::Desynchronized {
                    detail: DesyncDetail::NonJsonLine {
                        detail: format!("the line is not UTF-8: {source}"),
                    },
                })
                .and_then(Message::decode);
            match decoded {
                Ok(message) => {
                    counts.inbound_depth.fetch_add(1, Ordering::Relaxed);
                    if events.send(ReaderEvent::Message(message)).is_err() {
                        return;
                    }
                }
                Err(fault) => {
                    let _ = events.send(ReaderEvent::Fault(fault));
                    return;
                }
            }
        }

        if let Some(fault) = overflow {
            let _ = events.send(ReaderEvent::Fault(fault));
            return;
        }
    }
}

/// Writes framed lines to the peer's standard input.
fn write_messages(
    stdin: ChildStdin,
    commands: &Receiver<WriterCommand>,
    counts: &Counts,
    state: &Mutex<Option<Terminal>>,
) {
    let mut stdin = stdin;
    while let Ok(command) = commands.recv() {
        let line = match command {
            WriterCommand::Write(line) => line,
            WriterCommand::Close => break,
        };
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush());
        counts.outbound_depth.fetch_sub(1, Ordering::Relaxed);
        match written {
            Ok(()) => {
                counts
                    .bytes_written
                    .fetch_add(line.len() as u64, Ordering::Relaxed);
            }
            Err(source) => {
                let mut state = state.lock().expect("transport state is not poisoned");
                if state.is_none() {
                    *state = Some(Terminal::WriteFailed(source.to_string()));
                }
                break;
            }
        }
    }
    // Whatever is still queued will never be written, so it is no longer
    // "waiting to be written" either. Left counted, the depth a diagnostic reads
    // would stay permanently non-zero on a connection with nothing in flight.
    //
    // The whole queue is drained rather than its leading run of writes: `send`
    // clones the outbound sender and holds the clone across its retry loop, so a
    // `Close` can end up *ahead* of a write, and stopping at the first one would
    // leave the trailing writes counted forever.
    while let Ok(command) = commands.try_recv() {
        if matches!(command, WriterCommand::Write(_)) {
            counts.outbound_depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
    // Dropping the handle closes the peer's standard input, which is what the
    // first rung of shutdown means and what tells a well-behaved peer to exit.
    drop(stdin);
}

/// Streams the peer's standard error into its sink.
///
/// Nothing here can fail a request: the MCP specification reserves this stream
/// for free-form logging a client must not read as errors, and an ACP agent's is
/// no different.
fn drain_stderr(stderr: ChildStderr, sink: Box<dyn StderrSink>, counts: &Counts) {
    let mut stderr = stderr;
    let mut sink = sink;
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    loop {
        match stderr.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => break,
            Ok(read) => {
                counts
                    .stderr_bytes
                    .fetch_add(read as u64, Ordering::Relaxed);
                sink.write(&buffer[..read]);
            }
        }
    }
    sink.finish();
}

/// Every test here drives a real child process through `Fixture::shim`, which
/// writes a `#!/bin/sh` script and marks it executable — so the module is gated
/// rather than each of its tests, matching how `harkness-git`'s runner tests are
/// arranged. Windows coverage of this crate is the platform-independent half:
/// framing, correlation against a scripted transport, the error tables, and the
/// spawn-description checks, which is where the `Path::is_absolute` difference
/// that actually varies by platform lives.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::{
        io,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use harkness_git::Cancellation;
    use harkness_test_fixtures::Fixture;
    use serde_json::json;

    use super::{POLL_INTERVAL, StdioTransport};
    use crate::{
        error::{DisconnectKind, TransportError},
        message::{Message, RequestId},
        spawn::SpawnSpec,
        stderr::StderrTail,
        transport::{JsonRpcTransport, ShutdownRung},
    };

    /// The cancellation-visibility target every blocking phase is sized against.
    const VISIBILITY_TARGET: Duration = Duration::from_millis(250);

    /// A shim's `PATH`, since nothing is inherited. Anything a peer script needs
    /// beyond its shell's builtins has to be reachable, and naming it here is
    /// also what makes the canary test meaningful: the allowlist is exhaustive,
    /// so what is absent from it is absent from the child.
    const SHIM_PATH: &str = "/usr/bin:/bin";

    fn spec(program: &Path, working_dir: &Path) -> SpawnSpec {
        SpawnSpec::new(program, working_dir)
            .env("PATH", SHIM_PATH)
            .startup_deadline(Duration::from_secs(10))
    }

    fn connect(program: &Path, working_dir: &Path) -> Box<StdioTransport> {
        launch(|| spec(program, working_dir), Cancellation::default())
    }

    /// Launches a shim, retrying the one failure this test binary causes itself.
    ///
    /// These tests write executables and fork concurrently, and a `fork` in one
    /// thread inherits the write descriptor another thread holds on a shim it is
    /// still creating — so the `exec` fails `ETXTBSY` for as long as that
    /// descriptor lives. It is an artifact of the fixtures, not of the engine,
    /// which is why it is answered here rather than by a retry inside
    /// `StdioTransport::spawn`: a production `spawn_failed` naming the operating
    /// system's reason is the diagnosis a user needs, and quietly retrying it
    /// would hide a genuinely unusable agent binary.
    fn launch(
        mut describe: impl FnMut() -> SpawnSpec,
        cancellation: Cancellation,
    ) -> Box<StdioTransport> {
        for _ in 0..50 {
            match StdioTransport::spawn(describe(), cancellation.clone()) {
                Ok(transport) => return Box::new(transport),
                Err(TransportError::SpawnFailed { source, .. })
                    if source.kind() == io::ErrorKind::ExecutableFileBusy =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("failed to launch the peer: {error}"),
            }
        }
        panic!("the peer's executable stayed busy");
    }

    /// Waits for one message, failing rather than hanging.
    fn next(transport: &StdioTransport) -> Result<Message, TransportError> {
        match transport.recv_deadline(Instant::now() + Duration::from_secs(10)) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => panic!("the peer sent nothing within 10 seconds"),
            Err(error) => Err(error),
        }
    }

    /// Trips `cancellation` shortly, reporting *when* it did.
    ///
    /// The instant matters because the target is detection latency, not thread
    /// scheduling. Measuring from the start of the test folds in however long a
    /// loaded runner took to get the tripping thread onto a core, which on a
    /// busy macOS runner is most of the budget and none of the property.
    fn cancel_shortly(cancellation: &Cancellation) -> std::sync::Arc<Barrier> {
        let requested = std::sync::Arc::new(Barrier::default());
        let stamp = std::sync::Arc::clone(&requested);
        let tripping = cancellation.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            // Stamped before the token is tripped, so an observer that sees the
            // cancellation is guaranteed to find the instant already recorded.
            stamp.stamp();
            tripping.cancel();
        });
        requested
    }

    /// The instant a cancellation was asked for.
    #[derive(Default)]
    struct Barrier(std::sync::Mutex<Option<Instant>>);

    impl Barrier {
        fn stamp(&self) {
            *self.0.lock().unwrap() = Some(Instant::now());
        }

        /// How long after the request the caller noticed.
        fn since_request(&self) -> Duration {
            self.0
                .lock()
                .unwrap()
                .expect("the token was tripped before this was observed")
                .elapsed()
        }
    }

    /// A peer that answers every request by echoing its parameters back.
    fn echo_peer(fixture: &Fixture) -> PathBuf {
        fixture.shim(
            "echo-peer",
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"echoed":true}}\n' "$id"
done
"#,
        )
    }

    #[test]
    fn a_request_and_its_response_cross_the_connection() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("echo");
        let transport = connect(&echo_peer(&fixture), &workspace);

        transport
            .send(
                Message::request(
                    RequestId::Number(7),
                    "initialize",
                    Some(json!({"protocolVersion": 1})),
                ),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();

        assert_eq!(
            next(&transport).unwrap(),
            Message::result(RequestId::Number(7), json!({"echoed": true}))
        );
        assert!(transport.counters().bytes_written > 0);
        assert!(transport.counters().bytes_read > 0);
    }

    /// Deny-by-default is the whole difference from the Git runner's
    /// inherit-and-scrub, and a canary in this process's own environment is the
    /// only way to prove the child's environment was built rather than filtered.
    #[test]
    fn nothing_reaches_the_child_that_was_not_allowlisted() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("allowlist");
        let reporting_peer = fixture.shim(
            "reporting-peer",
            r#"#!/bin/sh
printf '{"jsonrpc":"2.0","method":"env","params":{"canary":"%s","allowed":"%s","cwd":"%s"}}\n' \
  "${HARKNESS_TRANSPORT_CANARY-absent}" "${HARKNESS_ALLOWED-absent}" "$PWD"
"#,
        );

        let transport = launch(
            || spec(&reporting_peer, &workspace).env("HARKNESS_ALLOWED", "yes"),
            Cancellation::default(),
        );

        // Set on the child that runs the shim rather than on this process:
        // `std::env::set_var` is unsound in a multithreaded test binary under
        // Rust 2024, and the point stands either way — the allowlist is
        // exhaustive, so a variable nobody named cannot arrive.
        let Message::Notification(reported) = next(&transport).unwrap() else {
            panic!("the peer sends a notification");
        };
        let params = reported.params.unwrap();
        assert_eq!(params["canary"], "absent");
        assert_eq!(params["allowed"], "yes");
        assert_eq!(
            std::fs::canonicalize(params["cwd"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&workspace).unwrap(),
            "the working directory is pinned by the spec"
        );
    }

    #[test]
    fn standard_error_is_captured_and_is_not_a_failure() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("chatty");
        let chatty_peer = fixture.shim(
            "chatty-peer",
            r#"#!/bin/sh
echo 'server: starting up' >&2
echo 'server: ERROR this is only logging' >&2
printf '{"jsonrpc":"2.0","method":"ready"}\n'
while IFS= read -r line; do :; done
"#,
        );
        let tail = StderrTail::new(4096);
        let transport = launch(
            || spec(&chatty_peer, &workspace).stderr_sink(tail.clone()),
            Cancellation::default(),
        );

        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );
        let outcome = transport.shutdown(Duration::from_secs(5));

        assert!(outcome.stderr_bytes > 0);
        assert!(tail.text().contains("this is only logging"));
    }

    #[test]
    fn non_protocol_output_desynchronizes_and_quarantines_the_connection() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("garbage");
        let garbage_peer = fixture.shim(
            "garbage-peer",
            "#!/bin/sh\necho 'Listening on stdio'\nwhile IFS= read -r line; do :; done\n",
        );
        let transport = connect(&garbage_peer, &workspace);

        let error = next(&transport).unwrap_err();
        assert_eq!(error.kind(), "desynchronized");

        // Quarantine means no further I/O, and every later caller is told which
        // fault ended the connection rather than being handed a second one.
        let again = next(&transport).unwrap_err();
        assert!(matches!(
            again,
            TransportError::Quarantined {
                fault_kind: "desynchronized",
                ..
            }
        ));
        assert_eq!(
            transport
                .send(
                    Message::notification("anything", None),
                    Instant::now() + Duration::from_secs(5),
                )
                .unwrap_err()
                .kind(),
            "quarantined"
        );
    }

    #[test]
    fn an_oversized_line_is_refused_without_being_buffered() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("oversized");
        let flooding_peer = fixture.shim(
            "flooding-peer",
            "#!/bin/sh\nyes harkness | tr -d '\\n' | head -c 200000\nprintf '\\n'\n",
        );
        let transport = launch(
            || spec(&flooding_peer, &workspace).max_message_bytes(1024),
            Cancellation::default(),
        );

        let error = next(&transport).unwrap_err();
        assert!(
            matches!(
                error,
                TransportError::MessageTooLarge {
                    bytes: 1025,
                    limit: 1024
                }
            ),
            "unexpected error {error:?}"
        );
    }

    #[test]
    fn an_idle_exit_is_told_apart_from_one_mid_message() {
        let fixture = Fixture::new();

        let idle_workspace = fixture.directory("idle");
        let idle_peer = fixture.shim("idle-peer", "#!/bin/sh\nexit 0\n");
        let idle = connect(&idle_peer, &idle_workspace);
        assert!(matches!(
            next(&idle).unwrap_err(),
            TransportError::Disconnected {
                kind: DisconnectKind::Idle
            }
        ));

        let partial_workspace = fixture.directory("partial");
        let partial_peer = fixture.shim(
            "partial-peer",
            "#!/bin/sh\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"resu'\nexit 0\n",
        );
        let partial = connect(&partial_peer, &partial_workspace);
        assert!(matches!(
            next(&partial).unwrap_err(),
            TransportError::Disconnected {
                kind: DisconnectKind::MidResponse
            }
        ));
    }

    /// The escalation exists for a peer that ignores the polite rungs, and the
    /// outcome records which rung was reached because "this agent had to be
    /// killed" is a bug report rather than an implementation detail.
    #[test]
    fn a_peer_ignoring_sigterm_is_killed_and_the_outcome_says_so() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("stubborn");
        let stubborn_peer = fixture.shim(
            "stubborn-peer",
            r#"#!/bin/sh
trap '' TERM
printf '{"jsonrpc":"2.0","method":"ready"}\n'
while true; do sleep 0.05; done
"#,
        );
        let transport = connect(&stubborn_peer, &workspace);
        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );

        let started = Instant::now();
        let outcome = transport.shutdown(Duration::from_millis(200));

        assert_eq!(outcome.rung, ShutdownRung::Killed);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_peer_that_exits_on_a_closed_stdin_needs_no_signal() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("polite");
        let polite_peer = fixture.shim(
            "polite-peer",
            "#!/bin/sh\nprintf '{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}\\n'\nwhile IFS= read -r line; do :; done\nexit 3\n",
        );
        let transport = connect(&polite_peer, &workspace);
        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );

        let outcome = transport.shutdown(Duration::from_secs(5));

        assert_eq!(outcome.rung, ShutdownRung::ClosedStdin);
        assert_eq!(outcome.exit_code, Some(3));
    }

    /// A teardown has to reach the peer's helpers too, or an agent's language
    /// server keeps the workspace open after the connection that started it is
    /// gone. The activity file is the only evidence once the group is dead.
    #[test]
    fn teardown_leaves_no_process_in_the_peer_group() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("helpers");
        let activity = fixture.root.path().join("helper-activity");
        let helper_peer = fixture.shim(
            "helper-peer",
            &format!(
                r#"#!/bin/sh
(while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &
printf '{{"jsonrpc":"2.0","method":"ready"}}\n'
trap '' TERM
wait
"#,
                activity.display()
            ),
        );
        let transport = connect(&helper_peer, &workspace);
        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );
        harkness_test_fixtures::wait_for_file(&activity);

        transport.shutdown(Duration::from_millis(200));

        let at_teardown = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::read(&activity).unwrap(),
            at_teardown,
            "a helper survived the teardown"
        );
    }

    /// The case a stubborn peer hides: this one exits *politely* on a closed
    /// standard input, so teardown never escalates — and the helper it
    /// backgrounded is still holding the workspace, and the standard-output
    /// pipe, after the direct child is reaped. The group has to be signalled on
    /// every rung, not only on the one that reaches `SIGTERM`.
    #[test]
    fn a_helper_outliving_a_polite_peer_is_still_reaped() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("polite-helper");
        let activity = fixture.root.path().join("polite-helper-activity");
        let peer = fixture.shim(
            "polite-helper-peer",
            &format!(
                r#"#!/bin/sh
(while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &
printf '{{"jsonrpc":"2.0","method":"ready"}}\n'
while IFS= read -r line; do :; done
exit 0
"#,
                activity.display()
            ),
        );
        let transport = connect(&peer, &workspace);
        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );
        harkness_test_fixtures::wait_for_file(&activity);

        let started = Instant::now();
        let outcome = transport.shutdown(Duration::from_secs(5));

        assert_eq!(
            outcome.rung,
            ShutdownRung::ClosedStdin,
            "the peer itself left politely"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "teardown waited {:?}, so the helper was still holding the pipe",
            started.elapsed()
        );
        let at_teardown = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::read(&activity).unwrap(),
            at_teardown,
            "a helper survived a peer that exited politely"
        );
    }

    #[test]
    fn the_startup_deadline_expires_and_leaves_no_live_child() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("slow-start");
        let activity = fixture.root.path().join("startup-activity");
        let slow_peer = fixture.shim(
            "slow-peer",
            &format!(
                "#!/bin/sh\nwhile true; do printf x >> '{}'; sleep 0.01; done\n",
                activity.display()
            ),
        );
        let transport = launch(
            || {
                SpawnSpec::new(&slow_peer, &workspace)
                    .env("PATH", SHIM_PATH)
                    .startup_deadline(Duration::from_millis(150))
            },
            Cancellation::default(),
        );

        let error = transport
            .recv_deadline(Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(error.kind(), "startup_deadline_exceeded");

        transport.shutdown(Duration::from_millis(100));
        let at_teardown = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(std::fs::read(&activity).unwrap(), at_teardown);
    }

    /// A handshake that finishes stops the deadline, so a long-lived session is
    /// not torn down thirty seconds after it started working.
    #[test]
    fn a_completed_handshake_ends_the_startup_window() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("handshake");
        let quiet_peer = fixture.shim(
            "quiet-peer",
            "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        );
        let transport = launch(
            || {
                SpawnSpec::new(&quiet_peer, &workspace)
                    .env("PATH", SHIM_PATH)
                    .startup_deadline(Duration::from_millis(50))
            },
            Cancellation::default(),
        );
        transport.handshake_complete();

        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            transport
                .recv_deadline(Instant::now() + POLL_INTERVAL)
                .unwrap(),
            None,
            "a quiet peer inside a closed startup window is not a failure"
        );
    }

    #[test]
    fn cancellation_becomes_visible_while_waiting_for_a_peer() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("cancellation");
        let quiet_peer = fixture.shim(
            "quiet-peer-2",
            "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        );
        let cancellation = Cancellation::default();
        let transport = launch(|| spec(&quiet_peer, &workspace), cancellation.clone());
        transport.handshake_complete();

        let requested = cancel_shortly(&cancellation);

        let error = transport
            .recv_deadline(Instant::now() + Duration::from_secs(30))
            .unwrap_err();
        let noticed = requested.since_request();

        assert_eq!(error.kind(), "cancelled");
        assert!(
            noticed < VISIBILITY_TARGET,
            "cancellation took {noticed:?} to become visible"
        );
    }

    /// The other blocking phase: a peer that never stops talking. A reader loop
    /// that only checked its token between messages would be at the mercy of the
    /// peer's pace, so the check is per poll slice rather than per message.
    #[test]
    fn cancellation_becomes_visible_while_a_peer_is_streaming() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("streaming-cancellation");
        let streaming_peer = fixture.shim(
            "streaming-cancel-peer",
            r#"#!/bin/sh
while true; do
  printf '{"jsonrpc":"2.0","method":"tick"}\n'
done
"#,
        );
        let cancellation = Cancellation::default();
        let transport = launch(|| spec(&streaming_peer, &workspace), cancellation.clone());
        transport.handshake_complete();

        let requested = cancel_shortly(&cancellation);

        let deadline = Instant::now() + Duration::from_secs(30);
        let error = loop {
            match transport.recv_deadline(deadline) {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the peer never stops talking"),
                Err(error) => break error,
            }
        };
        let noticed = requested.since_request();

        assert_eq!(error.kind(), "cancelled");
        assert!(
            noticed < VISIBILITY_TARGET,
            "cancellation took {noticed:?} to become visible"
        );
    }

    /// A peer that stops reading its own standard input fills its pipe and then
    /// the queue behind it. The caller's deadline is what bounds the wait —
    /// without it the enqueue would run to the transport's own 30-second
    /// backstop, thirty times what a one-second caller asked for.
    #[test]
    fn an_enqueue_gives_up_at_the_callers_deadline() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("deaf");
        // Never reads standard input, and holds its standard output open so the
        // connection stays up while the pipe fills.
        // One `sleep` rather than a loop of them: this runs beside the rest of
        // the suite, and a peer that forks a process a second is pressure the
        // test does not need.
        let deaf_peer = fixture.shim(
            "deaf-peer",
            "#!/bin/sh\nprintf '{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}\\n'\nsleep 60\n",
        );
        let transport = connect(&deaf_peer, &workspace);
        assert_eq!(
            next(&transport).unwrap(),
            Message::notification("ready", None)
        );
        transport.handshake_complete();

        let filler = "x".repeat(64 * 1024);
        let started = Instant::now();
        let error = loop {
            let deadline = Instant::now() + Duration::from_millis(200);
            match transport.send(
                Message::notification("flood", Some(json!({ "text": filler }))),
                deadline,
            ) {
                Ok(()) => assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "the peer's pipe never filled"
                ),
                Err(error) => break error,
            }
        };

        assert_eq!(
            error.kind(),
            "send_timed_out",
            "the caller's own deadline is not evidence that the peer is gone"
        );
        assert!(!error.is_terminal());
        // And the connection is still there, because nothing about a short
        // deadline says otherwise.
        assert_eq!(
            transport
                .recv_deadline(Instant::now() + Duration::from_millis(50))
                .unwrap(),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the enqueue ran past the caller's deadline to its own backstop"
        );
    }

    #[test]
    fn an_already_cancelled_token_launches_nothing() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("pre-cancelled");
        let marker = fixture.root.path().join("launched");
        let peer = fixture.shim(
            "launch-marker-peer",
            &format!("#!/bin/sh\nprintf x > '{}'\n", marker.display()),
        );
        let cancellation = Cancellation::default();
        cancellation.cancel();

        let error = StdioTransport::spawn(spec(&peer, &workspace), cancellation).unwrap_err();

        assert_eq!(error.kind(), "cancelled");
        assert!(!marker.exists(), "the peer was launched anyway");
    }

    #[test]
    fn a_missing_program_fails_before_anything_is_running() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("missing");
        let error = StdioTransport::spawn(
            spec(&fixture.root.path().join("no-such-peer"), &workspace),
            Cancellation::default(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), "spawn_failed");
    }

    /// One peer's fault is one peer's problem. Quarantine is per connection, and
    /// nothing about it is shared with another.
    #[test]
    fn quarantine_is_confined_to_the_connection_that_faulted() {
        let fixture = Fixture::new();
        let good_workspace = fixture.directory("isolated-good");
        let bad_workspace = fixture.directory("isolated-bad");
        let good = connect(&echo_peer(&fixture), &good_workspace);
        let bad = connect(
            &fixture.shim(
                "bad-peer",
                "#!/bin/sh\necho not-json\nwhile IFS= read -r line; do :; done\n",
            ),
            &bad_workspace,
        );

        assert_eq!(next(&bad).unwrap_err().kind(), "desynchronized");

        good.send(
            Message::request(RequestId::Number(1), "ping", None),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            next(&good).unwrap(),
            Message::result(RequestId::Number(1), json!({"echoed": true}))
        );
    }

    /// The bound on the inbound queue is what turns a peer that outruns its
    /// consumer into a peer blocked on its own pipe. Nothing here asserts a
    /// number of bytes — the property is that the connection stays usable and
    /// the queue stays bounded while a chatty peer runs unread.
    #[test]
    fn a_peer_outrunning_its_consumer_is_bounded_rather_than_buffered() {
        let fixture = Fixture::new();
        let workspace = fixture.directory("backpressure");
        let streaming_peer = fixture.shim(
            "streaming-peer",
            r#"#!/bin/sh
i=0
while [ $i -lt 100000 ]; do
  printf '{"jsonrpc":"2.0","method":"tick","params":{"n":%s}}\n' "$i"
  i=$((i + 1))
done
"#,
        );
        let transport = connect(&streaming_peer, &workspace);
        transport.handshake_complete();

        std::thread::sleep(Duration::from_millis(200));
        // The bound is the queue plus the one message the reader is blocked
        // handing over: the reader counts a message before it offers it, so the
        // depth includes the one that has nowhere to go yet.
        let depth = transport.counters().inbound_depth;
        assert!(
            depth <= super::INBOUND_CAPACITY + 1,
            "the inbound queue grew to {depth}, past its {} capacity",
            super::INBOUND_CAPACITY
        );

        // The consumer catches up on exactly what it asks for, in order.
        for expected in 0..16 {
            let Message::Notification(notification) = next(&transport).unwrap() else {
                panic!("the peer only sends notifications");
            };
            assert_eq!(notification.params.unwrap()["n"], expected);
        }
    }
}
