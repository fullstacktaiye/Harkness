use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{RunId, StepId, ToolCallId};
use crate::trust::{ContainedPath, ExecutionMode, PathBoundary};

use super::ToolError;

/// How often a waiting supervisor re-checks for a reason to stop.
///
/// The cadence `harkness-git`'s runner already uses, named once here so a
/// cancelled call, a cancelled child, and a deadline all take effect on the same
/// terms. Short enough that a stopped child dies while the user is still looking
/// at the button they pressed; long enough that waiting costs no busy spin.
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bytes of each captured output stream a call retains in memory by default.
///
/// A tool that shells out cannot know in advance how much its child will write,
/// and a failure message needs the end of that output rather than all of it —
/// the same reasoning that keeps only the last few segments of Git's stderr.
/// The full stream still reaches an artifact; this bounds only what is held to
/// explain a failure.
pub const DEFAULT_STREAM_TAIL_BYTES: usize = 64 * 1024;

/// A unit a countable progress event is measured in.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    /// Bytes transferred or written.
    Bytes,
    /// Files visited.
    Files,
    /// Git objects, the unit Git's own transfer progress counts in.
    Objects,
    /// Anything else the tool counts.
    Items,
}

impl ProgressUnit {
    /// Every unit in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Bytes, Self::Files, Self::Objects, Self::Items];

    /// Returns the stable spelling used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Files => "files",
            Self::Objects => "objects",
            Self::Items => "items",
        }
    }
}

impl fmt::Display for ProgressUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One thing a running tool reports about its own progress.
///
/// This is the typed generalization of the `impl FnMut(String)` callback Git
/// operations already take. That callback can only say *something happened*; a
/// front end wanting a determinate progress bar has to parse Git's prose to
/// find the numbers. Naming the three shapes separately means a consumer can
/// render a bar from [`Counted`](Self::Counted), a phase label from
/// [`Stage`](Self::Stage), and a log line from [`Message`](Self::Message)
/// without pattern-matching English.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum ProgressEvent {
    /// A human-readable line, the direct equivalent of one Git stderr segment.
    Message {
        /// Text to show or log.
        text: String,
    },

    /// The tool entered a named phase of its work.
    Stage {
        /// Stable name of the phase.
        name: String,
    },

    /// Countable progress, with a total when the tool knows one.
    Counted {
        /// Units completed so far.
        completed: u64,
        /// Units expected in total, when known. `None` means indeterminate.
        total: Option<u64>,
        /// What the counts are measured in.
        unit: ProgressUnit,
    },
}

impl ProgressEvent {
    /// Reports a human-readable line.
    #[must_use]
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message { text: text.into() }
    }

    /// Reports entry into a named phase.
    #[must_use]
    pub fn stage(name: impl Into<String>) -> Self {
        Self::Stage { name: name.into() }
    }

    /// Reports countable progress towards a known total.
    #[must_use]
    pub const fn counted(completed: u64, total: u64, unit: ProgressUnit) -> Self {
        Self::Counted {
            completed,
            total: Some(total),
            unit,
        }
    }

    /// Reports countable progress with no known total.
    #[must_use]
    pub const fn indeterminate(completed: u64, unit: ProgressUnit) -> Self {
        Self::Counted {
            completed,
            total: None,
            unit,
        }
    }

    /// Completion as a fraction in `0.0..=1.0`, when a total is known.
    ///
    /// Clamped, because a tool that miscounts past its own total should produce
    /// a full bar rather than an out-of-range value a front end has to guard.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        match self {
            Self::Counted {
                completed,
                total: Some(total),
                ..
            } => {
                if *total == 0 {
                    Some(1.0)
                } else {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "a progress fraction does not need more precision than f64 gives"
                    )]
                    Some((*completed as f64 / *total as f64).clamp(0.0, 1.0))
                }
            }
            _ => None,
        }
    }
}

/// Where a running tool's progress events go.
///
/// Taking a sink rather than returning a stream keeps a tool synchronous: it
/// reports as it works without owning a channel or a runtime. The semantics of
/// delivery — buffering, coalescing, persistence — belong to whoever supplies
/// the sink, not to the tool.
pub trait ProgressSink: Send {
    /// Accepts one event. Implementations must not block for long and must not
    /// panic: a tool reporting progress is not asking for a failure path.
    fn emit(&mut self, event: ProgressEvent);
}

impl<F> ProgressSink for F
where
    F: FnMut(ProgressEvent) + Send,
{
    /// Bridges the closure style Git operations already use.
    fn emit(&mut self, event: ProgressEvent) {
        self(event);
    }
}

/// A sink that drops every event.
///
/// Named for what it does. A tool run from a test or from a batch command has
/// nobody watching, and the alternative — an `Option<Box<dyn ProgressSink>>` —
/// would put a branch at every report site.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardedProgress;

impl ProgressSink for DiscardedProgress {
    fn emit(&mut self, _event: ProgressEvent) {}
}

/// A sink that retains every event for later inspection.
///
/// The handle is shareable and the events survive the [`ExecutionContext`] that
/// consumed the sink, which is what a test asserting on progress needs.
#[derive(Clone, Debug, Default)]
pub struct RecordedProgress {
    events: Arc<Mutex<Vec<ProgressEvent>>>,
}

impl RecordedProgress {
    /// Creates an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of the events recorded so far, in arrival order.
    ///
    /// Recovers from a poisoned lock rather than panicking. Nothing this type does
    /// while holding the lock can panic, so poisoning would have to come from a
    /// panic elsewhere in the same call — and losing a progress log is not a reason
    /// to turn that into a second failure.
    #[must_use]
    pub fn events(&self) -> Vec<ProgressEvent> {
        self.locked().clone()
    }

    /// Borrows the recorded events, recovering from poisoning.
    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<ProgressEvent>> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ProgressSink for RecordedProgress {
    fn emit(&mut self, event: ProgressEvent) {
        self.locked().push(event);
    }
}

/// Progress events a bounded channel holds before the consumer falls behind.
///
/// Small on purpose. The queue is not a buffer to be filled: it exists so a tool
/// reporting a burst of events does not block on every one of them, and a
/// consumer polling on a short interval empties it long before it fills. Sizing
/// it generously would only convert a slow consumer from a visible stall into
/// invisible memory growth.
pub const DEFAULT_PROGRESS_CAPACITY: usize = 64;

/// Creates a bounded channel a running tool reports progress through.
///
/// # Backpressure, not buffering
///
/// The channel is a [`SyncSender`](std::sync::mpsc::SyncSender), so a tool that
/// outruns its consumer *blocks* rather than queueing. That is the intended
/// behaviour and not a limitation to work around: progress describes work in
/// flight, so a queue that grows without bound is one that reports the past
/// while consuming memory in the present. A tool held up by its own reporting is
/// a tool that has been told to slow down.
///
/// Dropping the receiver is not an error. A consumer that has given up — because
/// the call timed out, or its run ended — leaves the tool free to run to its own
/// conclusion instead of blocking forever on a sink nobody reads.
#[must_use]
pub fn progress_channel(capacity: usize) -> (ProgressChannel, ProgressReceiver) {
    let (sender, receiver) = mpsc::sync_channel(capacity);
    (ProgressChannel { sender }, ProgressReceiver { receiver })
}

/// The sending half of a bounded progress channel.
#[derive(Clone, Debug)]
pub struct ProgressChannel {
    sender: mpsc::SyncSender<ProgressEvent>,
}

impl ProgressSink for ProgressChannel {
    /// Blocks while the channel is full, and discards once nobody is receiving.
    fn emit(&mut self, event: ProgressEvent) {
        let _ = self.sender.send(event);
    }
}

/// The receiving half of a bounded progress channel.
#[derive(Debug)]
pub struct ProgressReceiver {
    receiver: mpsc::Receiver<ProgressEvent>,
}

impl ProgressReceiver {
    /// Takes every event that has arrived, without waiting for more.
    ///
    /// Returns an empty vector when nothing is queued, including after every
    /// sender has been dropped — a consumer draining one last time as a call
    /// ends is asking what is left, not whether the tool is still running.
    #[must_use]
    pub fn drain(&self) -> Vec<ProgressEvent> {
        self.receiver.try_iter().collect()
    }
}

/// A handle to content a tool stored outside its own output.
///
/// Output travels through schema validation and is persisted inline under a size
/// bound, so anything large — a full diff, a build log, a downloaded file —
/// belongs in the artifact store with only this reference in the result.
///
/// It derives [`JsonSchema`](schemars::JsonSchema) because that is the whole point
/// of it: a tool returns this *inside* its `Output`, and an `Output` must have a
/// generated schema. Without the derive the only documented route for returning
/// stored content would force a tool author to hand-write a schema or mirror the
/// struct — reintroducing exactly the type/schema divergence this module exists to
/// make impossible.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// Identifier the artifact store assigned.
    pub id: String,
    /// IANA media type the content was stored under.
    pub media_type: String,
    /// Size of the stored content in bytes.
    pub byte_len: u64,
}

/// Content on its way into artifact storage, one chunk at a time.
///
/// This is the shape a tool that supervises a child process needs, and the
/// reason [`ArtifactWriter`] is not a single buffer-shaped method: the output of
/// a build or a clone is exactly the thing that must never be assembled in
/// memory in order to be stored. Bytes written here are durable only once
/// [`finish`](Self::finish) returns; an abandoned stream records nothing.
pub trait ArtifactStream: Write + Send {
    /// Makes the bytes durable and returns the reference naming them.
    ///
    /// Consuming a `Box<Self>` rather than `self` keeps the trait
    /// object-safe, which is what lets a stream be opened here and handed to
    /// the reader thread that fills it.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] when the content cannot be finalized.
    fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError>;
}

/// Where a running tool puts content too large for its result.
pub trait ArtifactWriter: Send {
    /// Redacts a bounded text value that will be returned inline beside an
    /// artifact produced by this writer.
    ///
    /// The default is pass-through for detached and test writers. Durable
    /// writers that redact their streams must apply the corresponding text rule
    /// here so an inline excerpt cannot bypass the storage policy.
    fn redact_text(&self, text: &str) -> String {
        text.to_owned()
    }

    /// Opens a stream that stores content under `name` as `media_type`.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] when storage cannot be opened. A tool that
    /// cannot store its artifact has not completed its work and should report
    /// the failure rather than return a partial result.
    fn open(&mut self, name: &str, media_type: &str) -> Result<Box<dyn ArtifactStream>, ToolError>;

    /// Stores `bytes` and returns a reference the tool can put in its output.
    ///
    /// The convenience shape for content a tool already holds. It is a provided
    /// method rather than a second thing to implement, so a writer cannot store
    /// buffered content one way and streamed content another.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open), plus a failure to write or finalize the content.
    fn write(
        &mut self,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRef, ToolError> {
        let mut stream = self.open(name, media_type)?;
        stream.write_all(bytes).map_err(|error| {
            ToolError::execution_failed(format!("{name:?} could not be written: {error}"))
        })?;
        stream.finish()
    }
}

/// An artifact writer for contexts with no artifact store attached.
///
/// It refuses rather than discards. A tool that believes it stored a build log
/// and returns a reference to nothing is worse than one that fails saying the
/// storage is absent.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedArtifacts;

impl ArtifactWriter for UnsupportedArtifacts {
    fn open(
        &mut self,
        name: &str,
        _media_type: &str,
    ) -> Result<Box<dyn ArtifactStream>, ToolError> {
        Err(ToolError::ExecutionFailed {
            message: format!(
                "no artifact store is attached to this execution context, so {name:?} cannot be stored"
            ),
        })
    }
}

/// The wall-clock bound on one tool call.
///
/// Both halves are carried because both are needed and neither derives the
/// other after the fact: the instant answers "is it over", and the limit is what
/// [`ToolError::TimedOut`] has to report. Recomputing the limit from the instant
/// once it has passed would report how *late* the call is rather than what it
/// was allowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    limit: Duration,
    at: Instant,
}

impl Deadline {
    /// Bounds a call that starts now to `limit`.
    ///
    /// Returns `None` when the limit lands further into the future than an
    /// [`Instant`] can represent — a call that would outlive the machine, and so
    /// one bounded by cancellation alone. An `Option` rather than a panic
    /// because the caller is an executor that has already moved a record to
    /// `running`: unwinding there would strand the call in the one state the
    /// execution layer promises is impossible, over a limit nobody could reach.
    #[must_use]
    pub fn starting_now(limit: Duration) -> Option<Self> {
        Instant::now()
            .checked_add(limit)
            .map(|at| Self { limit, at })
    }

    /// The limit the call was given.
    #[must_use]
    pub const fn limit(self) -> Duration {
        self.limit
    }

    /// The instant after which the call is over its limit.
    #[must_use]
    pub const fn at(self) -> Instant {
        self.at
    }

    /// Whether the limit has already elapsed.
    #[must_use]
    pub fn has_passed(self) -> bool {
        Instant::now() >= self.at
    }
}

/// Everything a tool is given besides its typed input.
///
/// The context carries the identities the work is recorded under, the workspace
/// it may touch, the cancellation token it must honour, and the two sinks it
/// reports through. It is passed as `&mut` because both sinks are stateful and
/// because a tool must not be able to clone a context and outlive the call it
/// belongs to.
///
/// The cancellation token is `harkness_git`'s, not a second mechanism. A tool
/// that shells out to Git hands the very same token down, so one cancel request
/// reaches the whole tree instead of stopping at a translation layer.
///
/// The deadline and the retained-output bound live here for the same reason the
/// token does: a tool that supervises a child has to enforce them on that child,
/// and it can only do that if the limits reached it. An executor sets both when
/// it dispatches the call; a context built by hand has no deadline and the
/// default bound.
pub struct ExecutionContext {
    run: RunId,
    step: StepId,
    call: ToolCallId,
    boundary: PathBoundary,
    mode: ExecutionMode,
    cancellation: Cancellation,
    progress: Box<dyn ProgressSink>,
    artifacts: Box<dyn ArtifactWriter>,
    deadline: Option<Deadline>,
    stream_tail_bytes: usize,
}

impl ExecutionContext {
    /// Builds a context for one tool call.
    ///
    /// The root is canonicalized into a [`PathBoundary`]. Every later path
    /// resolution therefore follows symlinks and checks containment against the
    /// same canonical identity rather than only comparing path strings.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ForbiddenPath`] when `workspace_root` is not an
    /// absolute, available directory.
    pub fn new(
        run: RunId,
        step: StepId,
        call: ToolCallId,
        workspace_root: impl Into<PathBuf>,
        cancellation: Cancellation,
        progress: Box<dyn ProgressSink>,
        artifacts: Box<dyn ArtifactWriter>,
    ) -> Result<Self, ToolError> {
        let supplied = workspace_root.into();
        if !supplied.is_absolute() {
            return Err(ToolError::ForbiddenPath {
                path: supplied,
                reason: "the workspace root must be an absolute path".to_owned(),
            });
        }
        if supplied
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(ToolError::ForbiddenPath {
                path: supplied,
                reason: "the workspace root must not contain a .. component".to_owned(),
            });
        }
        let boundary = PathBoundary::new(&supplied, std::iter::empty::<&Path>())?;
        Ok(Self::for_boundary(
            run,
            step,
            call,
            boundary,
            cancellation,
            progress,
            artifacts,
        ))
    }

    /// Builds a context from a pre-validated boundary with explicit extra roots.
    ///
    /// Policy can construct the boundary after evaluating grants and hand the
    /// exact same capability set to the tool. No path is reinterpreted later.
    #[must_use]
    pub fn for_boundary(
        run: RunId,
        step: StepId,
        call: ToolCallId,
        boundary: PathBoundary,
        cancellation: Cancellation,
        progress: Box<dyn ProgressSink>,
        artifacts: Box<dyn ArtifactWriter>,
    ) -> Self {
        Self {
            run,
            step,
            call,
            boundary,
            mode: ExecutionMode::NonInteractive,
            cancellation,
            progress,
            artifacts,
            deadline: None,
            stream_tail_bytes: DEFAULT_STREAM_TAIL_BYTES,
        }
    }

    /// Bounds this call's running time.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Bounds how much of each captured output stream is retained in memory.
    ///
    /// Zero is accepted and means no tail is kept at all, which is what a call
    /// whose output only ever belongs in an artifact wants.
    #[must_use]
    pub const fn with_stream_tail_bytes(mut self, bytes: usize) -> Self {
        self.stream_tail_bytes = bytes;
        self
    }

    /// Carries the front end's interaction mode into policy and tool execution.
    #[must_use]
    pub const fn with_execution_mode(mut self, mode: ExecutionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Builds a context with no progress reporting and no artifact storage.
    ///
    /// The shape a test or a one-shot CLI invocation wants.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn detached(
        run: RunId,
        step: StepId,
        call: ToolCallId,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Self, ToolError> {
        Self::new(
            run,
            step,
            call,
            workspace_root,
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(UnsupportedArtifacts),
        )
    }

    /// Run this call belongs to.
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }

    /// Step this call belongs to.
    #[must_use]
    pub const fn step(&self) -> StepId {
        self.step
    }

    /// Identity of this call, the record every result and failure attaches to.
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.call
    }

    /// Absolute root of the workspace this call may touch.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        self.boundary.workspace_root()
    }

    /// Canonical filesystem roots this call may address.
    #[must_use]
    pub const fn boundary(&self) -> &PathBoundary {
        &self.boundary
    }

    /// Whether this invocation is attached to an interactive front end.
    #[must_use]
    pub const fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Cancellation token shared with everything this call starts.
    #[must_use]
    pub const fn cancellation(&self) -> &Cancellation {
        &self.cancellation
    }

    /// Returns an error if cancellation has been requested.
    ///
    /// A long-running tool calls this between units of work. Cooperative
    /// cancellation is the only kind available to a synchronous tool, so a tool
    /// that never checks is a tool that cannot be stopped.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Cancelled`] once the token has been cancelled.
    pub fn check_cancelled(&self) -> Result<(), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        Ok(())
    }

    /// The wall-clock bound on this call, when it has one.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// Bytes of each captured output stream to retain in memory.
    #[must_use]
    pub const fn stream_tail_bytes(&self) -> usize {
        self.stream_tail_bytes
    }

    /// Returns an error if cancellation was requested or the deadline passed.
    ///
    /// The check a tool between units of work actually wants: an executor
    /// enforces the deadline from outside, but a body that notices first stops
    /// sooner and gets to unwind its own work rather than being abandoned.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Cancelled`] or [`ToolError::TimedOut`]. Cancellation
    /// is reported first, because a cancelled call that also ran out of time was
    /// stopped by its caller and saying otherwise would misattribute it.
    pub fn check_still_permitted(&self) -> Result<(), ToolError> {
        self.check_cancelled()?;
        if let Some(deadline) = self.deadline
            && deadline.has_passed()
        {
            return Err(ToolError::TimedOut {
                limit: deadline.limit(),
            });
        }
        Ok(())
    }

    /// Reports one progress event.
    pub fn report(&mut self, event: ProgressEvent) {
        self.progress.emit(event);
    }

    /// Stores content outside the tool's result and returns a reference to it.
    ///
    /// # Errors
    ///
    /// Returns whatever the attached [`ArtifactWriter`] reports, or an
    /// [`ToolError::ExecutionFailed`] naming the absent store when none is
    /// attached.
    pub fn write_artifact(
        &mut self,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRef, ToolError> {
        self.artifacts.write(name, media_type, bytes)
    }

    /// Opens a stream storing content outside the tool's result.
    ///
    /// What a tool uses when the content is a stream rather than a value it
    /// already holds — the output of a child process, most of all.
    ///
    /// # Errors
    ///
    /// As [`ArtifactWriter::open`].
    pub fn open_artifact(
        &mut self,
        name: &str,
        media_type: &str,
    ) -> Result<Box<dyn ArtifactStream>, ToolError> {
        self.artifacts.open(name, media_type)
    }

    /// Redacts bounded inline text through the same policy as this call's
    /// artifact store.
    #[must_use]
    pub fn redact_text(&self, text: &str) -> String {
        self.artifacts.redact_text(text)
    }

    /// Resolves a path through the call's canonical filesystem boundary.
    ///
    /// Relative paths start at the workspace root; absolute paths are accepted
    /// only when they resolve inside the workspace or an explicitly granted
    /// extra root. The returned capability cannot be constructed unchecked.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ForbiddenPath`] when the path is empty or the
    /// boundary refuses its canonical resolution.
    pub fn resolve(&self, candidate: impl AsRef<Path>) -> Result<ContainedPath, ToolError> {
        let candidate = candidate.as_ref();
        if candidate.as_os_str().is_empty() {
            return Err(ToolError::ForbiddenPath {
                path: candidate.to_path_buf(),
                reason: "a workspace path must not be empty".to_owned(),
            });
        }
        self.boundary.contain(candidate).map_err(ToolError::from)
    }
}

impl fmt::Debug for ExecutionContext {
    /// Omits the two sinks, which are opaque trait objects, and names the
    /// identities a failure report needs.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("run", &self.run)
            .field("step", &self.step)
            .field("call", &self.call)
            .field("workspace_root", &self.boundary.workspace_root())
            .field("execution_mode", &self.mode)
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("timeout", &self.deadline.map(Deadline::limit))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use harkness_git::Cancellation;
    use tempfile::TempDir;

    use super::{
        ArtifactRef, ArtifactStream, ArtifactWriter, DEFAULT_STREAM_TAIL_BYTES, Deadline,
        DiscardedProgress, ExecutionContext, ProgressEvent, ProgressSink, ProgressUnit,
        RecordedProgress, UnsupportedArtifacts,
    };
    use crate::domain::{RunId, StepId, ToolCallId};
    use crate::tool::ToolError;
    use crate::trust::ExecutionMode;

    fn context() -> (TempDir, ExecutionContext) {
        let workspace = TempDir::new().unwrap();
        let context = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
        )
        .unwrap();
        (workspace, context)
    }

    #[test]
    fn a_relative_path_resolves_under_the_workspace_root() {
        let (workspace, context) = context();
        let resolved = context.resolve("src/main.rs").unwrap();
        assert_eq!(
            resolved.as_path(),
            std::fs::canonicalize(workspace.path())
                .unwrap()
                .join("src")
                .join("main.rs")
        );
        assert!(resolved.as_path().starts_with(context.workspace_root()));

        // A leading `./` is noise, not an escape.
        assert_eq!(context.resolve("./src/./main.rs").unwrap(), resolved);
    }

    #[test]
    fn no_supplied_string_resolves_outside_the_workspace_root() {
        let (_workspace, context) = context();
        let rejected = ["", "..", "../secrets", "src/../../secrets", "./.."];
        for path in rejected {
            let error = context.resolve(path).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    "forbidden_path" | "outside_allowed_roots" | "candidate_unavailable"
                ),
                "accepted {path:?}: {error}"
            );
            // Deliberately *not* `happened_before_execution`: tools call this
            // mid-body, so a refused second path says nothing about whether an
            // earlier one was already written.
            assert!(
                !error.happened_before_execution(),
                "a mid-body path refusal must not claim the tool never ran"
            );
        }

        let absolute = context.workspace_root().join("src");
        assert_eq!(
            context.resolve(&absolute).unwrap().as_path(),
            absolute,
            "an absolute path inside the root stays containable"
        );
    }

    #[test]
    fn a_workspace_root_that_is_not_absolute_and_normal_is_refused() {
        for root in ["relative/path", ""] {
            let error =
                ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), root)
                    .unwrap_err();
            assert_eq!(error.kind(), "forbidden_path", "accepted {root:?}");
        }

        let workspace = TempDir::new().unwrap();
        let unnormalized = workspace.path().join("..").join("elsewhere");
        let error = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            unnormalized,
        )
        .unwrap_err();
        assert!(error.to_string().contains(".."), "{error}");
    }

    #[test]
    fn the_workspace_root_is_stored_lexically_normalized() {
        // `.` is dropped rather than refused: `Path::components` already elides it
        // for an absolute path, so refusing it would need a check that cannot fire.
        // Normalizing means `workspace_root()` can be compared against a canonical
        // project path as a string, which a consumer will do.
        let workspace = TempDir::new().unwrap();
        let expected = workspace.path().join("sub");
        std::fs::create_dir(&expected).unwrap();
        let noisy = workspace.path().join(".").join("sub");
        let context =
            ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), noisy)
                .unwrap();

        let expected = std::fs::canonicalize(expected).unwrap();
        assert_eq!(context.workspace_root(), expected);
        assert!(
            !context.workspace_root().to_string_lossy().contains("/./"),
            "the stored root kept a redundant component: {:?}",
            context.workspace_root()
        );
        assert_eq!(
            context.resolve("a.txt").unwrap().as_path(),
            expected.join("a.txt")
        );

        // A trailing separator is likewise normalized away.
        let trailing = format!("{}{}", expected.display(), std::path::MAIN_SEPARATOR);
        let context =
            ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), trailing)
                .unwrap();
        assert_eq!(context.workspace_root(), expected);
    }

    #[test]
    fn the_context_reports_its_identities_and_shares_one_cancellation_token() {
        let run = RunId::new();
        let step = StepId::new();
        let call = ToolCallId::new();
        let cancellation = Cancellation::default();
        let workspace = TempDir::new().unwrap();
        let context = ExecutionContext::new(
            run,
            step,
            call,
            workspace.path(),
            cancellation.clone(),
            Box::new(DiscardedProgress),
            Box::new(UnsupportedArtifacts),
        )
        .unwrap();

        assert_eq!(context.run(), run);
        assert_eq!(context.step(), step);
        assert_eq!(context.call(), call);
        assert!(context.check_cancelled().is_ok());

        // Cancelling the caller's clone is visible through the context, which is
        // the property that lets one token stop a whole tree of work.
        cancellation.cancel();
        assert_eq!(context.check_cancelled().unwrap_err(), ToolError::Cancelled);
        assert!(context.cancellation().is_cancelled());
        assert!(format!("{context:?}").contains("cancelled: true"));
    }

    #[test]
    fn execution_is_noninteractive_until_the_front_end_says_otherwise() {
        let (_workspace, context) = context();
        assert_eq!(context.execution_mode(), ExecutionMode::NonInteractive);
        assert_eq!(
            context
                .with_execution_mode(ExecutionMode::Interactive)
                .execution_mode(),
            ExecutionMode::Interactive
        );
    }

    #[test]
    fn recorded_progress_outlives_the_context_that_consumed_the_sink() {
        let recorder = RecordedProgress::new();
        let workspace = TempDir::new().unwrap();
        let mut context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
            Cancellation::default(),
            Box::new(recorder.clone()),
            Box::new(UnsupportedArtifacts),
        )
        .unwrap();

        context.report(ProgressEvent::stage("reading"));
        context.report(ProgressEvent::counted(3, 4, ProgressUnit::Files));
        context.report(ProgressEvent::message("done"));
        drop(context);

        assert_eq!(
            recorder.events(),
            [
                ProgressEvent::stage("reading"),
                ProgressEvent::counted(3, 4, ProgressUnit::Files),
                ProgressEvent::message("done"),
            ]
        );
    }

    #[test]
    fn a_closure_is_a_progress_sink() {
        let mut seen = Vec::new();
        {
            let mut sink = |event: ProgressEvent| seen.push(event);
            sink.emit(ProgressEvent::message("from a closure"));
        }
        assert_eq!(seen, [ProgressEvent::message("from a closure")]);
    }

    #[test]
    fn a_progress_fraction_is_clamped_and_only_defined_when_counted() {
        assert_eq!(
            ProgressEvent::counted(1, 4, ProgressUnit::Bytes).fraction(),
            Some(0.25)
        );
        assert_eq!(
            ProgressEvent::counted(9, 4, ProgressUnit::Bytes).fraction(),
            Some(1.0),
            "a miscounted total must not escape 0.0..=1.0"
        );
        assert_eq!(
            ProgressEvent::counted(0, 0, ProgressUnit::Items).fraction(),
            Some(1.0),
            "nothing to do is done"
        );
        assert_eq!(
            ProgressEvent::indeterminate(7, ProgressUnit::Objects).fraction(),
            None
        );
        assert_eq!(ProgressEvent::message("text").fraction(), None);
        assert_eq!(ProgressEvent::stage("phase").fraction(), None);
    }

    #[test]
    fn progress_events_serialize_with_a_tagged_event_discriminant() {
        assert_eq!(
            serde_json::to_string(&ProgressEvent::counted(2, 5, ProgressUnit::Objects)).unwrap(),
            r#"{"event":"counted","completed":2,"total":5,"unit":"objects"}"#
        );
        assert_eq!(
            serde_json::to_string(&ProgressEvent::message("hi")).unwrap(),
            r#"{"event":"message","text":"hi"}"#
        );
        let spellings = ProgressUnit::ALL
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(spellings, ["bytes", "files", "objects", "items"]);
    }

    #[test]
    fn an_absent_artifact_store_refuses_instead_of_discarding() {
        let (_workspace, mut context) = context();
        let error = context
            .write_artifact("build.log", "text/plain", b"output")
            .unwrap_err();
        assert_eq!(error.kind(), "execution_failed");
        assert!(error.to_string().contains("build.log"), "{error}");
    }

    /// One artifact an in-memory writer was handed.
    type Stored = (String, String, Vec<u8>);

    /// An in-memory artifact writer that records what it was handed.
    #[derive(Clone, Debug, Default)]
    struct Recording(Arc<Mutex<Vec<Stored>>>);

    impl Recording {
        fn stored(&self) -> Vec<Stored> {
            self.0.lock().unwrap().clone()
        }
    }

    struct RecordingStream {
        recording: Recording,
        name: String,
        media_type: String,
        bytes: Vec<u8>,
    }

    impl std::io::Write for RecordingStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl ArtifactStream for RecordingStream {
        fn finish(self: Box<Self>) -> Result<ArtifactRef, ToolError> {
            let mut stored = self.recording.0.lock().unwrap();
            stored.push((self.name, self.media_type.clone(), self.bytes.clone()));
            Ok(ArtifactRef {
                id: format!("artifact-{}", stored.len()),
                media_type: self.media_type,
                byte_len: self.bytes.len() as u64,
            })
        }
    }

    impl ArtifactWriter for Recording {
        fn open(
            &mut self,
            name: &str,
            media_type: &str,
        ) -> Result<Box<dyn ArtifactStream>, ToolError> {
            Ok(Box::new(RecordingStream {
                recording: self.clone(),
                name: name.to_owned(),
                media_type: media_type.to_owned(),
                bytes: Vec::new(),
            }))
        }
    }

    #[test]
    fn an_attached_artifact_store_receives_the_content() {
        let recording = Recording::default();
        let workspace = TempDir::new().unwrap();
        let mut context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(recording.clone()),
        )
        .unwrap();

        let reference = context
            .write_artifact("diff.patch", "text/x-diff", b"@@")
            .unwrap();
        assert_eq!(
            reference,
            ArtifactRef {
                id: "artifact-1".to_owned(),
                media_type: "text/x-diff".to_owned(),
                byte_len: 2,
            }
        );
        assert_eq!(
            recording.stored(),
            [(
                "diff.patch".to_owned(),
                "text/x-diff".to_owned(),
                b"@@".to_vec()
            )]
        );
    }

    #[test]
    fn buffered_and_streamed_artifacts_take_the_same_route() {
        // `write` is a provided method built on `open`, so a writer cannot store
        // buffered content one way and streamed content another — which is what
        // would let redaction, hashing, or naming disagree between them.
        let recording = Recording::default();
        let workspace = TempDir::new().unwrap();
        let mut context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(recording.clone()),
        )
        .unwrap();

        let mut stream = context.open_artifact("build.log", "text/plain").unwrap();
        stream.write_all(b"first ").unwrap();
        stream.write_all(b"second").unwrap();
        let streamed = stream.finish().unwrap();

        assert_eq!(streamed.byte_len, 12);
        assert_eq!(
            recording.stored()[0],
            (
                "build.log".to_owned(),
                "text/plain".to_owned(),
                b"first second".to_vec()
            )
        );
    }

    #[test]
    fn a_deadline_reports_its_limit_and_a_context_without_one_never_times_out() {
        let (workspace, context) = context();
        assert_eq!(context.deadline(), None);
        assert_eq!(context.stream_tail_bytes(), DEFAULT_STREAM_TAIL_BYTES);
        assert!(context.check_still_permitted().is_ok());

        let deadline = Deadline::starting_now(Duration::from_millis(0)).unwrap();
        assert_eq!(deadline.limit(), Duration::from_millis(0));
        assert!(deadline.has_passed());

        // A limit further away than an `Instant` can express is reported rather
        // than panicked over: the executor asking for one has already moved a
        // record to `running`, and unwinding there would strand the call in the
        // one state the execution layer promises is impossible.
        assert_eq!(Deadline::starting_now(Duration::MAX), None);

        let bounded = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
        )
        .unwrap()
        .with_deadline(deadline)
        .with_stream_tail_bytes(16);
        assert_eq!(bounded.stream_tail_bytes(), 16);
        assert_eq!(
            bounded.check_still_permitted().unwrap_err(),
            ToolError::TimedOut {
                limit: Duration::from_millis(0)
            }
        );
        assert!(format!("{bounded:?}").contains("timeout"));
    }

    #[test]
    fn a_cancelled_call_that_also_ran_out_of_time_reports_the_cancellation() {
        // Both are true, and only one of them is why the call stopped. Reporting
        // the deadline would tell a user their work was too slow when in fact
        // they asked for it to stop.
        let cancellation = Cancellation::default();
        let workspace = TempDir::new().unwrap();
        let context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
            cancellation.clone(),
            Box::new(DiscardedProgress),
            Box::new(UnsupportedArtifacts),
        )
        .unwrap()
        .with_deadline(Deadline::starting_now(Duration::ZERO).unwrap());

        cancellation.cancel();

        assert_eq!(
            context.check_still_permitted().unwrap_err(),
            ToolError::Cancelled
        );
    }

    #[test]
    fn the_debug_form_names_the_identities_and_omits_the_opaque_sinks() {
        let (_workspace, context) = context();
        let rendered = format!("{context:?}");
        assert!(rendered.contains("run"), "{rendered}");
        assert!(rendered.contains("call"), "{rendered}");
        assert!(rendered.contains("workspace_root"), "{rendered}");
        assert!(rendered.contains("cancelled: false"), "{rendered}");
        // The two trait objects have no useful rendering, and a context is the
        // thing a failure report prints; naming a sink there would be noise.
        assert!(!rendered.contains("progress"), "{rendered}");
        assert!(!rendered.contains("artifacts"), "{rendered}");
        assert!(
            rendered.ends_with(".. }"),
            "the form must stay non-exhaustive: {rendered}"
        );
    }
}
