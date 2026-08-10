use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use harkness_git::Cancellation;
use serde::{Deserialize, Serialize};

use crate::domain::{RunId, StepId, ToolCallId};

use super::ToolError;

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
    /// # Panics
    ///
    /// Panics if a previous recording panicked while holding the lock, which
    /// cannot happen through [`ProgressSink::emit`].
    #[must_use]
    pub fn events(&self) -> Vec<ProgressEvent> {
        self.events
            .lock()
            .expect("progress recorder poisoned")
            .clone()
    }
}

impl ProgressSink for RecordedProgress {
    fn emit(&mut self, event: ProgressEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

/// A handle to content a tool stored outside its own output.
///
/// Output travels through schema validation and is persisted inline under a size
/// bound, so anything large — a full diff, a build log, a downloaded file —
/// belongs in the artifact store with only this reference in the result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// Identifier the artifact store assigned.
    pub id: String,
    /// IANA media type the content was stored under.
    pub media_type: String,
    /// Size of the stored content in bytes.
    pub byte_len: u64,
}

/// Where a running tool puts content too large for its result.
pub trait ArtifactWriter: Send {
    /// Stores `bytes` and returns a reference the tool can put in its output.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] when the content cannot be stored. A tool that
    /// cannot store its artifact has not completed its work and should report
    /// the failure rather than return a partial result.
    fn write(
        &mut self,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRef, ToolError>;
}

/// An artifact writer for contexts with no artifact store attached.
///
/// It refuses rather than discards. A tool that believes it stored a build log
/// and returns a reference to nothing is worse than one that fails saying the
/// storage is absent.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedArtifacts;

impl ArtifactWriter for UnsupportedArtifacts {
    fn write(
        &mut self,
        name: &str,
        _media_type: &str,
        _bytes: &[u8],
    ) -> Result<ArtifactRef, ToolError> {
        Err(ToolError::ExecutionFailed {
            message: format!(
                "no artifact store is attached to this execution context, so {name:?} cannot be stored"
            ),
        })
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
pub struct ExecutionContext {
    run: RunId,
    step: StepId,
    call: ToolCallId,
    workspace_root: PathBuf,
    cancellation: Cancellation,
    progress: Box<dyn ProgressSink>,
    artifacts: Box<dyn ArtifactWriter>,
}

impl ExecutionContext {
    /// Builds a context for one tool call.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ForbiddenPath`] when `workspace_root` is not
    /// absolute or is not already lexically normal. Both are refused here, once,
    /// so [`resolve`](Self::resolve) can reason about containment by prefix
    /// alone rather than re-deriving it on every path a tool touches.
    pub fn new(
        run: RunId,
        step: StepId,
        call: ToolCallId,
        workspace_root: impl Into<PathBuf>,
        cancellation: Cancellation,
        progress: Box<dyn ProgressSink>,
        artifacts: Box<dyn ArtifactWriter>,
    ) -> Result<Self, ToolError> {
        let workspace_root = workspace_root.into();
        if !workspace_root.is_absolute() {
            return Err(ToolError::ForbiddenPath {
                path: workspace_root,
                reason: "the workspace root must be an absolute path",
            });
        }
        if workspace_root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(ToolError::ForbiddenPath {
                path: workspace_root,
                reason: "the workspace root must not contain . or .. components",
            });
        }

        Ok(Self {
            run,
            step,
            call,
            workspace_root,
            cancellation,
            progress,
            artifacts,
        })
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
        &self.workspace_root
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

    /// Resolves a workspace-relative path against the workspace root.
    ///
    /// Refuses absolute paths and any path whose components would climb out of
    /// the workspace, so a tool that routes every path argument through this
    /// method cannot be talked into touching `../../.ssh/id_rsa` by its input.
    ///
    /// **This check is lexical.** It does not consult the filesystem, so it does
    /// not detect a symlink inside the workspace that points outside it.
    /// Resolving that requires touching the filesystem under the same
    /// time-of-check race every such test has, and it belongs to the trust and
    /// policy layers that evaluate an invocation against real paths. What this
    /// method guarantees is narrower and worth having on its own: no *string* a
    /// caller supplies can escape the root.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ForbiddenPath`] when the path is empty, absolute, or
    /// contains a `..` component.
    pub fn resolve(&self, relative: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        let relative = relative.as_ref();
        let forbid = |reason: &'static str| ToolError::ForbiddenPath {
            path: relative.to_path_buf(),
            reason,
        };

        if relative.as_os_str().is_empty() {
            return Err(forbid("a workspace path must not be empty"));
        }

        let mut resolved = self.workspace_root.clone();
        for component in relative.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(forbid(
                        "a workspace path must be relative to the workspace root",
                    ));
                }
                Component::ParentDir => {
                    return Err(forbid("a workspace path must not contain a .. component"));
                }
                Component::CurDir => {}
                Component::Normal(part) => resolved.push(part),
            }
        }

        if resolved == self.workspace_root {
            return Err(forbid(
                "a workspace path must name something inside the root",
            ));
        }
        Ok(resolved)
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
            .field("workspace_root", &self.workspace_root)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use harkness_git::Cancellation;

    use super::{
        ArtifactRef, ArtifactWriter, DiscardedProgress, ExecutionContext, ProgressEvent,
        ProgressSink, ProgressUnit, RecordedProgress, UnsupportedArtifacts,
    };
    use crate::domain::{RunId, StepId, ToolCallId};
    use crate::tool::ToolError;

    const ROOT: &str = if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    };

    fn context() -> ExecutionContext {
        ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), ROOT).unwrap()
    }

    #[test]
    fn a_relative_path_resolves_under_the_workspace_root() {
        let context = context();
        let resolved = context.resolve("src/main.rs").unwrap();
        assert_eq!(resolved, Path::new(ROOT).join("src").join("main.rs"));
        assert!(resolved.starts_with(context.workspace_root()));

        // A leading `./` is noise, not an escape.
        assert_eq!(context.resolve("./src/./main.rs").unwrap(), resolved);
    }

    #[test]
    fn no_supplied_string_resolves_outside_the_workspace_root() {
        let context = context();
        let rejected = [
            "",
            "..",
            "../secrets",
            "src/../../secrets",
            "./..",
            ".",
            "./",
        ];
        for path in rejected {
            let error = context.resolve(path).unwrap_err();
            assert_eq!(error.kind(), "forbidden_path", "accepted {path:?}");
            assert!(
                error.happened_before_execution(),
                "a refused path must promise nothing ran"
            );
        }

        let absolute = if cfg!(windows) {
            r"C:\workspace\src"
        } else {
            "/workspace/src"
        };
        assert_eq!(
            context.resolve(absolute).unwrap_err().kind(),
            "forbidden_path",
            "an absolute path inside the root is still absolute"
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

        let unnormalized = Path::new(ROOT).join("..").join("elsewhere");
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
    fn the_context_reports_its_identities_and_shares_one_cancellation_token() {
        let run = RunId::new();
        let step = StepId::new();
        let call = ToolCallId::new();
        let cancellation = Cancellation::default();
        let context = ExecutionContext::new(
            run,
            step,
            call,
            ROOT,
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
    fn recorded_progress_outlives_the_context_that_consumed_the_sink() {
        let recorder = RecordedProgress::new();
        let mut context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            ROOT,
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
        let mut context = context();
        let error = context
            .write_artifact("build.log", "text/plain", b"output")
            .unwrap_err();
        assert_eq!(error.kind(), "execution_failed");
        assert!(error.to_string().contains("build.log"), "{error}");
    }

    #[test]
    fn an_attached_artifact_store_receives_the_content() {
        #[derive(Default)]
        struct Recording(Vec<(String, String, usize)>);

        impl ArtifactWriter for Recording {
            fn write(
                &mut self,
                name: &str,
                media_type: &str,
                bytes: &[u8],
            ) -> Result<ArtifactRef, ToolError> {
                self.0
                    .push((name.to_owned(), media_type.to_owned(), bytes.len()));
                Ok(ArtifactRef {
                    id: format!("artifact-{}", self.0.len()),
                    media_type: media_type.to_owned(),
                    byte_len: bytes.len() as u64,
                })
            }
        }

        let mut context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            ROOT,
            Cancellation::default(),
            Box::new(DiscardedProgress),
            Box::new(Recording::default()),
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
    }

    #[test]
    fn the_debug_form_names_the_identities_and_omits_the_opaque_sinks() {
        let rendered = format!("{:?}", context());
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
