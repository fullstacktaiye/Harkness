use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::policy::{PolicyDecision, PolicyVerdict};

use super::state::LifecycleState;
use super::{ExecutionState, RunDomainError, RunId, StepId, TaskId, ToolCallId, ToolCallState};

/// Structured explanation attached to a failed or denied record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    kind: String,
    message: String,
}

impl Failure {
    /// Creates a failure with a stable machine-readable kind and user-facing message.
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// Stable machine-readable failure category.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Human-readable failure detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Replaces the detail with what the store's redactor made of it.
    ///
    /// The `kind` is untouched: it is a stable machine identifier this build
    /// chose from a fixed set, and rewriting one would break the very callers
    /// that switch on it. The message is the half that quotes a command line, a
    /// URL, or a child's last words, and is therefore the half that can carry a
    /// credential into a durable column.
    pub(crate) fn with_redacted_message(mut self, message: String) -> Self {
        self.message = message;
        self
    }
}

/// Which way one recorded approval went.
///
/// Named for the outcome rather than for the decision so it does not collide
/// with [`approval::ApprovalDecision`](crate::approval::ApprovalDecision), which
/// is a different thing entirely: the durable answer to a specific question,
/// carrying the scope it authorized and the surface it arrived from. This is one
/// line in a record's own audit history, and the only thing it says is which way
/// the answer went. Two public types with one name in one crate would be
/// mis-imported eventually, and the two compile interchangeably at the use site.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// The requested work may proceed.
    Approved,
    /// The requested work must not proceed.
    Denied,
}

/// One auditable decision made while a record awaited approval.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    decided_by: String,
    decision: ApprovalOutcome,
    #[serde(with = "time::serde::rfc3339")]
    decided_at: OffsetDateTime,
}

impl Approval {
    fn approved(decided_by: impl Into<String>, decided_at: OffsetDateTime) -> Self {
        Self::new(decided_by, ApprovalOutcome::Approved, decided_at)
    }

    fn denied(decided_by: impl Into<String>, decided_at: OffsetDateTime) -> Self {
        Self::new(decided_by, ApprovalOutcome::Denied, decided_at)
    }

    /// Reconstructs a durable approval decision with a UTC-normalized timestamp.
    #[must_use]
    pub fn new(
        decided_by: impl Into<String>,
        decision: ApprovalOutcome,
        decided_at: OffsetDateTime,
    ) -> Self {
        Self {
            decided_by: decided_by.into(),
            decision,
            decided_at: utc(decided_at),
        }
    }

    /// Stable identity of the user, policy, or service that made the decision.
    #[must_use]
    pub fn decided_by(&self) -> &str {
        &self.decided_by
    }

    /// Whether the request was approved or denied.
    #[must_use]
    pub const fn decision(&self) -> ApprovalOutcome {
        self.decision
    }

    /// UTC time at which the decision was recorded.
    #[must_use]
    pub const fn decided_at(&self) -> OffsetDateTime {
        self.decided_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Lifecycle<S> {
    state: S,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
    revision: u64,
}

impl<S> Lifecycle<S>
where
    S: LifecycleState + 'static,
{
    fn new(created_at: OffsetDateTime) -> Self {
        let created_at = utc(created_at);
        Self {
            state: S::INITIAL,
            created_at,
            updated_at: created_at,
            started_at: None,
            finished_at: None,
            revision: 0,
        }
    }

    pub(super) fn from_wire(
        record: &'static str,
        state: S,
        created_at: OffsetDateTime,
        updated_at: OffsetDateTime,
        started_at: Option<OffsetDateTime>,
        finished_at: Option<OffsetDateTime>,
        revision: u64,
    ) -> Result<Self, RunDomainError> {
        validate_utc(record, "created_at", created_at)?;
        validate_utc(record, "updated_at", updated_at)?;
        if let Some(started_at) = started_at {
            validate_utc(record, "started_at", started_at)?;
        }
        if let Some(finished_at) = finished_at {
            validate_utc(record, "finished_at", finished_at)?;
        }

        if updated_at < created_at {
            return Err(invalid_timestamp(
                record,
                "updated_at",
                "must not precede created_at",
            ));
        }
        if started_at.is_some_and(|at| at < created_at || at > updated_at) {
            return Err(invalid_timestamp(
                record,
                "started_at",
                "must fall between created_at and updated_at",
            ));
        }
        if finished_at.is_some_and(|at| at < created_at || at > updated_at) {
            return Err(invalid_timestamp(
                record,
                "finished_at",
                "must fall between created_at and updated_at",
            ));
        }
        if state.is_terminal() && finished_at.is_none() {
            return Err(invalid_lifecycle(
                record,
                "a terminal state requires finished_at",
            ));
        }
        if !state.is_terminal() && finished_at.is_some() {
            return Err(invalid_lifecycle(
                record,
                "a non-terminal state cannot carry finished_at",
            ));
        }
        if finished_at.is_some_and(|at| at != updated_at) {
            return Err(invalid_lifecycle(
                record,
                "finished_at must equal updated_at in a terminal state",
            ));
        }
        if state.requires_started_at() && started_at.is_none() {
            return Err(invalid_lifecycle(
                record,
                "the current state requires started_at",
            ));
        }
        if state.forbids_started_at() && started_at.is_some() {
            return Err(invalid_lifecycle(
                record,
                "the current state cannot carry started_at",
            ));
        }
        if state == S::INITIAL && revision != 0 {
            return Err(invalid_lifecycle(
                record,
                "the initial state requires revision zero",
            ));
        }
        if state != S::INITIAL && revision == 0 {
            return Err(invalid_lifecycle(
                record,
                "a transitioned state requires a nonzero revision",
            ));
        }
        if revision < state.minimum_revision() {
            return Err(invalid_lifecycle(
                record,
                "revision is too small for the current state",
            ));
        }
        if revision == 0 && updated_at != created_at {
            return Err(invalid_lifecycle(
                record,
                "revision zero requires updated_at to equal created_at",
            ));
        }

        Ok(Self {
            state,
            created_at,
            updated_at,
            started_at,
            finished_at,
            revision,
        })
    }

    fn transition(
        &mut self,
        record: &'static str,
        to: S,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        self.validate_edge(to)?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(RunDomainError::RevisionExhausted { record })?;
        let at = utc(at).max(self.updated_at);

        self.state = to;
        self.updated_at = at;
        self.revision = revision;
        if to == S::RUNNING && self.started_at.is_none() {
            self.started_at = Some(at);
        }
        if to.is_terminal() {
            self.finished_at = Some(at);
        }
        Ok(())
    }

    fn validate_edge(&self, to: S) -> Result<(), RunDomainError> {
        if S::transitions().contains(&(self.state, to)) {
            Ok(())
        } else {
            Err(S::invalid_transition(self.state, to))
        }
    }

    const fn state(&self) -> S {
        self.state
    }

    const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    const fn started_at(&self) -> Option<OffsetDateTime> {
        self.started_at
    }

    const fn finished_at(&self) -> Option<OffsetDateTime> {
        self.finished_at
    }

    const fn revision(&self) -> u64 {
        self.revision
    }
}

macro_rules! lifecycle_accessors {
    ($state:ty) => {
        /// Current lifecycle state.
        #[must_use]
        pub const fn state(&self) -> $state {
            self.lifecycle.state()
        }

        /// UTC creation time.
        #[must_use]
        pub const fn created_at(&self) -> OffsetDateTime {
            self.lifecycle.created_at()
        }

        /// UTC time at which the current state was entered.
        #[must_use]
        pub const fn updated_at(&self) -> OffsetDateTime {
            self.lifecycle.updated_at()
        }

        /// UTC time at which execution first entered `running`.
        #[must_use]
        pub const fn started_at(&self) -> Option<OffsetDateTime> {
            self.lifecycle.started_at()
        }

        /// UTC time at which a terminal state was entered.
        #[must_use]
        pub const fn finished_at(&self) -> Option<OffsetDateTime> {
            self.lifecycle.finished_at()
        }

        /// Optimistic-concurrency revision, incremented by every transition.
        #[must_use]
        pub const fn revision(&self) -> u64 {
            self.lifecycle.revision()
        }
    };
}

/// User-requested work associated with one workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Task {
    pub(super) id: TaskId,
    pub(super) title: String,
    pub(super) workspace_root: PathBuf,
    pub(super) project_id: Option<ProjectId>,
    pub(super) created_at: OffsetDateTime,
}

impl Task {
    /// Creates a task with a random ID and a UTC-normalized creation time.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
        project_id: Option<ProjectId>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self::with_id(TaskId::new(), title, workspace_root, project_id, created_at)
    }

    /// Creates a task with a caller-chosen stable ID.
    #[must_use]
    pub fn with_id(
        id: TaskId,
        title: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
        project_id: Option<ProjectId>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            workspace_root: workspace_root.into(),
            project_id,
            created_at: utc(created_at),
        }
    }

    /// Replaces the title with what the store's redactor made of it.
    ///
    /// A task is redacted on its way into the store and handed back in the form
    /// that was stored, so a caller never holds a title that differs from the
    /// row and from every observation derived from it.
    pub(crate) fn with_redacted_title(mut self, title: String) -> Self {
        self.title = title;
        self
    }

    /// Stable identifier of this task.
    #[must_use]
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// User-facing task title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Workspace against which the task runs.
    ///
    /// Durable JSON serialization currently requires this path to be UTF-8.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Catalog project associated with the workspace, when it has one.
    #[must_use]
    pub const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    /// UTC time at which the task was created.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }
}

/// One attempt to execute a [`Task`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Run {
    pub(super) id: RunId,
    pub(super) task_id: TaskId,
    pub(super) lifecycle: Lifecycle<ExecutionState>,
    pub(super) failure: Option<Failure>,
    pub(super) approvals: Vec<Approval>,
    pub(super) retry_of: Option<RunId>,
    pub(super) workspace_may_be_modified: bool,
}

impl Run {
    /// Creates a queued run with a random ID.
    #[must_use]
    pub fn new(task_id: TaskId, created_at: OffsetDateTime) -> Self {
        Self::with_id(RunId::new(), task_id, created_at)
    }

    /// Creates a queued run with a caller-chosen stable ID.
    #[must_use]
    pub fn with_id(id: RunId, task_id: TaskId, created_at: OffsetDateTime) -> Self {
        Self {
            id,
            task_id,
            lifecycle: Lifecycle::new(created_at),
            failure: None,
            approvals: Vec::new(),
            retry_of: None,
            workspace_may_be_modified: false,
        }
    }

    /// Creates a queued run that re-attempts `original` for the same task.
    ///
    /// A retry is a *new* run and never a rewrite of the one it follows: the
    /// original keeps its own timeline, its own terminal state, and its own
    /// approvals, and this record merely names it. The relationship is stored
    /// here rather than derived, because a front end reading one run has to be
    /// able to say "this is attempt two" without paging every run of the task.
    ///
    /// `workspace_may_be_modified` is the honest half of that provenance:
    /// v0.3 never rolls back or re-applies what an interrupted attempt already
    /// wrote, so a retry may be starting against a worktree the earlier attempt
    /// changed. See
    /// [`RunCoordinator::retry_run`](crate::coordinator::RunCoordinator::retry_run)
    /// for how it is computed — from persisted tool-call lifecycle, never from
    /// whether a tool "probably" finished.
    #[must_use]
    pub fn retrying(
        task_id: TaskId,
        original: RunId,
        workspace_may_be_modified: bool,
        created_at: OffsetDateTime,
    ) -> Self {
        Self::retrying_with_id(
            RunId::new(),
            task_id,
            original,
            workspace_may_be_modified,
            created_at,
        )
    }

    /// Creates a retry run with a caller-chosen stable ID.
    #[must_use]
    pub fn retrying_with_id(
        id: RunId,
        task_id: TaskId,
        original: RunId,
        workspace_may_be_modified: bool,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            retry_of: Some(original),
            workspace_may_be_modified,
            ..Self::with_id(id, task_id, created_at)
        }
    }

    /// Stable identifier of this run.
    #[must_use]
    pub const fn id(&self) -> RunId {
        self.id
    }

    /// Task this run attempts to execute.
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// The earlier attempt this run re-attempts, when it is a retry.
    #[must_use]
    pub const fn retry_of(&self) -> Option<RunId> {
        self.retry_of
    }

    /// Whether an earlier attempt may already have changed the workspace.
    ///
    /// Only ever true on a retry, and true whenever the run it follows started
    /// any tool call that could write — which is a claim about what was
    /// recorded, not a claim that anything was actually written. A front end
    /// must surface it: nothing in v0.3 undoes a partial mutation.
    #[must_use]
    pub const fn workspace_may_be_modified(&self) -> bool {
        self.workspace_may_be_modified
    }

    lifecycle_accessors!(ExecutionState);

    /// Structured failure detail, present only in `failed`.
    #[must_use]
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// Approval decisions retained in decision order for audit.
    #[must_use]
    pub fn approvals(&self) -> &[Approval] {
        &self.approvals
    }

    /// Enters an outcome-free edge in [`super::EXECUTION_TRANSITIONS`].
    ///
    /// Use [`Self::fail`] for `failed` and [`Self::approve`] when resuming
    /// approval-gated work. The transition time is UTC-normalized and clamped
    /// to `updated_at`; a regressing clock therefore produces a zero-duration
    /// state instead of moving lifecycle time backwards.
    pub fn transition(
        &mut self,
        to: ExecutionState,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        self.lifecycle.validate_edge(to)?;
        if to == ExecutionState::Failed {
            return Err(invalid_lifecycle(
                "run",
                "failed transitions require Run::fail",
            ));
        }
        if self.state() == ExecutionState::WaitingForApproval && to == ExecutionState::Running {
            return Err(invalid_lifecycle(
                "run",
                "approval-gated work requires Run::approve",
            ));
        }
        self.lifecycle.transition("run", to, at)
    }

    /// Enters `failed` atomically with structured failure detail.
    pub fn fail(&mut self, failure: Failure, at: OffsetDateTime) -> Result<(), RunDomainError> {
        self.lifecycle
            .transition("run", ExecutionState::Failed, at)?;
        self.failure = Some(failure);
        Ok(())
    }

    /// Records an approval and resumes from `waiting_for_approval`.
    pub fn approve(
        &mut self,
        decided_by: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "run",
            self.state() == ExecutionState::WaitingForApproval,
            "approval decisions require waiting_for_approval",
        )?;
        let decided_by = approval_identity("run", decided_by)?;
        self.lifecycle
            .transition("run", ExecutionState::Running, at)?;
        self.approvals
            .push(Approval::approved(decided_by, self.updated_at()));
        Ok(())
    }

    /// Records a denied approval and terminates the run as failed.
    pub fn reject_approval(
        &mut self,
        decided_by: impl Into<String>,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "run",
            self.state() == ExecutionState::WaitingForApproval,
            "approval decisions require waiting_for_approval",
        )?;
        let decided_by = approval_identity("run", decided_by)?;
        self.lifecycle
            .transition("run", ExecutionState::Failed, at)?;
        self.failure = Some(failure);
        self.approvals
            .push(Approval::denied(decided_by, self.updated_at()));
        Ok(())
    }

    /// Records that a tool-level approval was denied and resumes orchestration.
    ///
    /// The run itself was paused only because its current tool call needed an
    /// answer. Refusing that call is an observation the agent may handle; it is
    /// not necessarily a terminal failure of the whole run.
    pub fn resume_after_denial(
        &mut self,
        decided_by: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "run",
            self.state() == ExecutionState::WaitingForApproval,
            "approval decisions require waiting_for_approval",
        )?;
        // Validate the identity even though the tool call and durable approval
        // request, rather than the run, retain the denial. Run approval history
        // has the older invariant that a denial is a terminal run outcome.
        let _ = approval_identity("run", decided_by)?;
        self.lifecycle
            .transition("run", ExecutionState::Running, at)?;
        Ok(())
    }
}

/// One ordered unit of work within a [`Run`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Step {
    pub(super) id: StepId,
    pub(super) run_id: RunId,
    pub(super) ordinal: u32,
    pub(super) title: String,
    pub(super) lifecycle: Lifecycle<ExecutionState>,
    pub(super) failure: Option<Failure>,
    pub(super) approvals: Vec<Approval>,
}

impl Step {
    /// Creates a queued step with a random ID.
    ///
    /// The persistence layer owns ordinal allocation and uniqueness within a run.
    #[must_use]
    pub fn new(
        run_id: RunId,
        ordinal: u32,
        title: impl Into<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self::with_id(StepId::new(), run_id, ordinal, title, created_at)
    }

    /// Creates a queued step with a caller-chosen stable ID.
    ///
    /// The persistence layer owns ordinal allocation and uniqueness within a run.
    #[must_use]
    pub fn with_id(
        id: StepId,
        run_id: RunId,
        ordinal: u32,
        title: impl Into<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            run_id,
            ordinal,
            title: title.into(),
            lifecycle: Lifecycle::new(created_at),
            failure: None,
            approvals: Vec::new(),
        }
    }

    /// Stable identifier of this step.
    #[must_use]
    pub const fn id(&self) -> StepId {
        self.id
    }

    /// Run containing this step.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Zero-based position within the run; persistence enforces uniqueness.
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// User-facing step title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    lifecycle_accessors!(ExecutionState);

    /// Structured failure detail, present only in `failed`.
    #[must_use]
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// Approval decisions retained in decision order for audit.
    #[must_use]
    pub fn approvals(&self) -> &[Approval] {
        &self.approvals
    }

    /// Enters an outcome-free edge in [`super::EXECUTION_TRANSITIONS`].
    ///
    /// Use [`Self::fail`] for `failed` and [`Self::approve`] when resuming
    /// approval-gated work. The transition time is UTC-normalized and clamped
    /// to `updated_at`; a regressing clock therefore produces a zero-duration
    /// state instead of moving lifecycle time backwards.
    pub fn transition(
        &mut self,
        to: ExecutionState,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        self.lifecycle.validate_edge(to)?;
        if to == ExecutionState::Failed {
            return Err(invalid_lifecycle(
                "step",
                "failed transitions require Step::fail",
            ));
        }
        if self.state() == ExecutionState::WaitingForApproval && to == ExecutionState::Running {
            return Err(invalid_lifecycle(
                "step",
                "approval-gated work requires Step::approve",
            ));
        }
        self.lifecycle.transition("step", to, at)
    }

    /// Enters `failed` atomically with structured failure detail.
    pub fn fail(&mut self, failure: Failure, at: OffsetDateTime) -> Result<(), RunDomainError> {
        self.lifecycle
            .transition("step", ExecutionState::Failed, at)?;
        self.failure = Some(failure);
        Ok(())
    }

    /// Records an approval and resumes from `waiting_for_approval`.
    pub fn approve(
        &mut self,
        decided_by: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "step",
            self.state() == ExecutionState::WaitingForApproval,
            "approval decisions require waiting_for_approval",
        )?;
        let decided_by = approval_identity("step", decided_by)?;
        self.lifecycle
            .transition("step", ExecutionState::Running, at)?;
        self.approvals
            .push(Approval::approved(decided_by, self.updated_at()));
        Ok(())
    }

    /// Records a denied approval and terminates the step as failed.
    pub fn reject_approval(
        &mut self,
        decided_by: impl Into<String>,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "step",
            self.state() == ExecutionState::WaitingForApproval,
            "approval decisions require waiting_for_approval",
        )?;
        let decided_by = approval_identity("step", decided_by)?;
        self.lifecycle
            .transition("step", ExecutionState::Failed, at)?;
        self.failure = Some(failure);
        self.approvals
            .push(Approval::denied(decided_by, self.updated_at()));
        Ok(())
    }
}

/// One typed tool request contained by a [`Step`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolCall {
    pub(super) id: ToolCallId,
    pub(super) run_id: RunId,
    pub(super) step_id: StepId,
    pub(super) tool_id: String,
    pub(super) tool_version: String,
    pub(super) input: Value,
    pub(super) lifecycle: Lifecycle<ToolCallState>,
    pub(super) failure: Option<Failure>,
    pub(super) output: Option<Value>,
    pub(super) approvals: Vec<Approval>,
    pub(super) policy_decision: Option<PolicyDecision>,
}

impl ToolCall {
    /// Creates a pending tool request with a random ID.
    ///
    /// Both containment IDs are derived from `step`, so a caller cannot create
    /// a call whose denormalized run ID disagrees with its step.
    #[must_use]
    pub fn new(
        step: &Step,
        tool_id: impl Into<String>,
        tool_version: impl Into<String>,
        input: Value,
        created_at: OffsetDateTime,
    ) -> Self {
        Self::with_id(
            ToolCallId::new(),
            step,
            tool_id,
            tool_version,
            input,
            created_at,
        )
    }

    /// Creates a pending tool request with a caller-chosen stable ID.
    #[must_use]
    pub fn with_id(
        id: ToolCallId,
        step: &Step,
        tool_id: impl Into<String>,
        tool_version: impl Into<String>,
        input: Value,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            run_id: step.run_id(),
            step_id: step.id(),
            tool_id: tool_id.into(),
            tool_version: tool_version.into(),
            input,
            lifecycle: Lifecycle::new(created_at),
            failure: None,
            output: None,
            approvals: Vec::new(),
            policy_decision: None,
        }
    }

    /// Stable identifier of this tool call.
    #[must_use]
    pub const fn id(&self) -> ToolCallId {
        self.id
    }

    /// Run containing this call, derived from its step at construction.
    ///
    /// Deserialization cannot resolve the referenced step, so persistence must
    /// verify `step_id -> run_id` before accepting a wire record.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Step containing this call.
    #[must_use]
    pub const fn step_id(&self) -> StepId {
        self.step_id
    }

    /// Stable dotted identifier of the requested tool.
    #[must_use]
    pub fn tool_id(&self) -> &str {
        &self.tool_id
    }

    /// Requested immutable tool version.
    #[must_use]
    pub fn tool_version(&self) -> &str {
        &self.tool_version
    }

    /// Raw typed-tool input awaiting schema validation by the tool layer.
    #[must_use]
    pub const fn input(&self) -> &Value {
        &self.input
    }

    lifecycle_accessors!(ToolCallState);

    /// Structured failure detail, present only in `failed` or `denied`.
    #[must_use]
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// Tool result, present only in `succeeded`.
    #[must_use]
    pub const fn output(&self) -> Option<&Value> {
        self.output.as_ref()
    }

    /// Approval decisions retained in decision order for audit.
    #[must_use]
    pub fn approvals(&self) -> &[Approval] {
        &self.approvals
    }

    /// Policy decision recorded before this call was allowed to proceed.
    #[must_use]
    pub const fn policy_decision(&self) -> Option<&PolicyDecision> {
        self.policy_decision.as_ref()
    }

    /// Records policy and applies its immediate lifecycle consequence.
    ///
    /// `Allow` leaves the call pending for dispatch, `Ask` parks it in
    /// `awaiting_approval`, and `Deny` terminates it with policy failure detail.
    /// The store persists this whole mutation in one transaction, so a call can
    /// never be observed in the governed state without the decision that put it
    /// there.
    ///
    /// This is the only edge from `pending` to `denied`. There is deliberately
    /// no decision-free denial to reach for: a denied call that carried no
    /// verdict, reason, or source would be exactly the audit gap this record
    /// exists to close. A refusal after an approval prompt uses
    /// [`Self::reject_approval`], which records who refused it.
    pub fn apply_policy_decision(
        &mut self,
        decision: PolicyDecision,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "tool_call",
            self.state() == ToolCallState::Pending,
            "policy evaluation requires pending",
        )?;
        require_state(
            "tool_call",
            self.policy_decision.is_none(),
            "a tool call may carry only one policy decision",
        )?;
        if let Err(reason) = decision.validate() {
            return Err(invalid_lifecycle("tool_call", reason));
        }

        match decision.verdict() {
            PolicyVerdict::Allow => {}
            PolicyVerdict::Ask => {
                self.lifecycle
                    .transition("tool_call", ToolCallState::AwaitingApproval, at)?;
            }
            PolicyVerdict::Deny => {
                self.lifecycle
                    .transition("tool_call", ToolCallState::Denied, at)?;
                self.failure = Some(Failure::new("policy", decision.reason()));
            }
        }
        self.policy_decision = Some(decision);
        Ok(())
    }

    /// Enters an outcome-free edge in [`super::TOOL_CALL_TRANSITIONS`].
    ///
    /// Use the outcome-specific methods for `succeeded`, `failed`, and
    /// `denied`, and [`Self::approve`] when resuming approval-gated work. The
    /// transition time is UTC-normalized and clamped to `updated_at`; a
    /// regressing clock therefore produces a zero-duration state instead of
    /// moving lifecycle time backwards.
    pub fn transition(
        &mut self,
        to: ToolCallState,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        self.lifecycle.validate_edge(to)?;
        if matches!(
            to,
            ToolCallState::Succeeded | ToolCallState::Failed | ToolCallState::Denied
        ) {
            return Err(invalid_lifecycle(
                "tool_call",
                "outcome states require an outcome-specific transition method",
            ));
        }
        if self.state() == ToolCallState::AwaitingApproval && to == ToolCallState::Running {
            return Err(invalid_lifecycle(
                "tool_call",
                "approval-gated work requires ToolCall::approve",
            ));
        }
        self.lifecycle.transition("tool_call", to, at)
    }

    /// Enters `running`, pinning the tool version that was actually resolved.
    ///
    /// A call may be recorded without naming a version — "the latest `fs.read`"
    /// — and which version that turned out to be has to be written down at the
    /// moment it is chosen. Resolving again later is a second lookup, and a
    /// second lookup can disagree with the first: a version registered in
    /// between would make the record name something other than what ran, while
    /// an approval bound to the recorded version would no longer describe the
    /// work it authorized. Pinning is therefore part of this transition rather
    /// than a separate write that could be skipped, reordered, or lost.
    ///
    /// # Errors
    ///
    /// Returns [`RunDomainError`] when the call is not `pending`, and when the
    /// record already names a *different* version — a resolution disagreeing
    /// with a recorded request is a caller bug, not a version to overwrite.
    pub fn dispatch(
        &mut self,
        tool_version: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        let resolved = tool_version.into();
        require_state(
            "tool_call",
            self.state() == ToolCallState::Pending,
            "ToolCall::dispatch begins execution and requires pending",
        )?;
        require_state(
            "tool_call",
            self.tool_version.is_empty() || self.tool_version == resolved,
            "a recorded tool version may not be replaced by a different one",
        )?;
        self.lifecycle
            .transition("tool_call", ToolCallState::Running, at)?;
        self.tool_version = resolved;
        Ok(())
    }

    /// Records an approval, pins the resolved version, and enters `running`.
    ///
    /// [`Self::approve`] already resumes approval-gated work, so this adds one
    /// thing: the version. An approval is a decision about `(id, version)` —
    /// that pair is what an audit reads back and what a policy scope is matched
    /// against — so recording the decision and recording what it authorized have
    /// to be the same step. Doing them separately would leave a window in which
    /// the call is approved and running while the row still says which version
    /// was *asked for* rather than which one is executing.
    ///
    /// # Errors
    ///
    /// Returns [`RunDomainError`] when the call is not `awaiting_approval`, when
    /// the identity is blank, and when the record already names a *different*
    /// version. Nothing is mutated unless all three hold.
    pub fn dispatch_approved(
        &mut self,
        decided_by: impl Into<String>,
        tool_version: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        let resolved = tool_version.into();
        require_state(
            "tool_call",
            self.state() == ToolCallState::AwaitingApproval,
            "approval decisions require awaiting_approval",
        )?;
        require_state(
            "tool_call",
            self.tool_version.is_empty() || self.tool_version == resolved,
            "a recorded tool version may not be replaced by a different one",
        )?;
        // Validated before the transition, like every other refusal here: a
        // rejected dispatch leaves the record exactly as it was.
        let decided_by = approval_identity("tool_call", decided_by)?;
        self.lifecycle
            .transition("tool_call", ToolCallState::Running, at)?;
        self.tool_version = resolved;
        self.approvals
            .push(Approval::approved(decided_by, self.updated_at()));
        Ok(())
    }

    /// Enters `succeeded` atomically with the tool output.
    pub fn succeed(&mut self, output: Value, at: OffsetDateTime) -> Result<(), RunDomainError> {
        self.lifecycle
            .transition("tool_call", ToolCallState::Succeeded, at)?;
        self.output = Some(output);
        Ok(())
    }

    /// Enters `failed` atomically with structured failure detail.
    pub fn fail(&mut self, failure: Failure, at: OffsetDateTime) -> Result<(), RunDomainError> {
        self.lifecycle
            .transition("tool_call", ToolCallState::Failed, at)?;
        self.failure = Some(failure);
        Ok(())
    }

    /// Records an approval and resumes from `awaiting_approval`.
    pub fn approve(
        &mut self,
        decided_by: impl Into<String>,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "tool_call",
            self.state() == ToolCallState::AwaitingApproval,
            "approval decisions require awaiting_approval",
        )?;
        let decided_by = approval_identity("tool_call", decided_by)?;
        self.lifecycle
            .transition("tool_call", ToolCallState::Running, at)?;
        self.approvals
            .push(Approval::approved(decided_by, self.updated_at()));
        Ok(())
    }

    /// Records a denied approval and enters `denied` with structured detail.
    pub fn reject_approval(
        &mut self,
        decided_by: impl Into<String>,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<(), RunDomainError> {
        require_state(
            "tool_call",
            self.state() == ToolCallState::AwaitingApproval,
            "approval decisions require awaiting_approval",
        )?;
        let decided_by = approval_identity("tool_call", decided_by)?;
        self.lifecycle
            .transition("tool_call", ToolCallState::Denied, at)?;
        self.failure = Some(failure);
        self.approvals
            .push(Approval::denied(decided_by, self.updated_at()));
        Ok(())
    }
}

pub(super) fn validate_utc(
    record: &'static str,
    field: &'static str,
    timestamp: OffsetDateTime,
) -> Result<(), RunDomainError> {
    if timestamp.offset() != UtcOffset::UTC {
        return Err(invalid_timestamp(record, field, "must use the UTC offset"));
    }
    Ok(())
}

pub(super) const fn invalid_timestamp(
    record: &'static str,
    field: &'static str,
    reason: &'static str,
) -> RunDomainError {
    RunDomainError::InvalidTimestamp {
        record,
        field,
        reason,
    }
}

pub(super) const fn invalid_lifecycle(
    record: &'static str,
    reason: &'static str,
) -> RunDomainError {
    RunDomainError::InvalidLifecycle { record, reason }
}

fn require_state(
    record: &'static str,
    condition: bool,
    reason: &'static str,
) -> Result<(), RunDomainError> {
    if condition {
        Ok(())
    } else {
        Err(invalid_lifecycle(record, reason))
    }
}

fn approval_identity(
    record: &'static str,
    decided_by: impl Into<String>,
) -> Result<String, RunDomainError> {
    let decided_by = decided_by.into();
    if decided_by.trim().is_empty() {
        Err(invalid_lifecycle(
            record,
            "approval decisions require decided_by",
        ))
    } else {
        Ok(decided_by)
    }
}

fn utc(timestamp: OffsetDateTime) -> OffsetDateTime {
    timestamp.to_offset(UtcOffset::UTC)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::json;
    use time::{Duration, OffsetDateTime};

    use super::{Failure, Run, Step, Task, ToolCall};
    use crate::domain::{
        EXECUTION_TRANSITIONS, ExecutionState, InvalidTransition, RunDomainError, RunId, StepId,
        TOOL_CALL_TRANSITIONS, TaskId, ToolCallId, ToolCallState,
    };
    use crate::policy::PolicyDecision;

    fn at(second: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(second)
    }

    fn failure() -> Failure {
        Failure::new("fixture_failure", "fixture failed")
    }

    /// The only way a pending call reaches `denied` is a recorded denial.
    fn denial() -> PolicyDecision {
        serde_json::from_value(json!({
            "verdict": "deny",
            "reason": "denied: workspace is untrusted",
            "source": "built_in"
        }))
        .unwrap()
    }

    fn task() -> Task {
        Task::new("fixture", "/fixture", None, at(0))
    }

    fn fresh_run() -> Run {
        Run::new(task().id(), at(0))
    }

    fn fresh_step() -> Step {
        Step::new(fresh_run().id(), 0, "fixture", at(0))
    }

    fn fresh_call() -> ToolCall {
        ToolCall::new(&fresh_step(), "fixture.tool", "1.0.0", json!({}), at(0))
    }

    fn run_in(state: ExecutionState) -> Run {
        let mut run = fresh_run();
        match state {
            ExecutionState::Queued => {}
            ExecutionState::Running => run.transition(state, at(1)).unwrap(),
            ExecutionState::WaitingForApproval => {
                run.transition(ExecutionState::Running, at(1)).unwrap();
                run.transition(state, at(2)).unwrap();
            }
            ExecutionState::Succeeded => {
                run.transition(ExecutionState::Running, at(1)).unwrap();
                run.transition(state, at(2)).unwrap();
            }
            ExecutionState::Failed => run.fail(failure(), at(1)).unwrap(),
            ExecutionState::Cancelled | ExecutionState::Interrupted => {
                run.transition(state, at(1)).unwrap();
            }
        }
        run
    }

    fn step_in(state: ExecutionState) -> Step {
        let mut step = fresh_step();
        match state {
            ExecutionState::Queued => {}
            ExecutionState::Running => step.transition(state, at(1)).unwrap(),
            ExecutionState::WaitingForApproval => {
                step.transition(ExecutionState::Running, at(1)).unwrap();
                step.transition(state, at(2)).unwrap();
            }
            ExecutionState::Succeeded => {
                step.transition(ExecutionState::Running, at(1)).unwrap();
                step.transition(state, at(2)).unwrap();
            }
            ExecutionState::Failed => step.fail(failure(), at(1)).unwrap(),
            ExecutionState::Cancelled | ExecutionState::Interrupted => {
                step.transition(state, at(1)).unwrap();
            }
        }
        step
    }

    fn call_in(state: ToolCallState) -> ToolCall {
        let mut call = fresh_call();
        match state {
            ToolCallState::Pending => {}
            ToolCallState::AwaitingApproval | ToolCallState::Running => {
                call.transition(state, at(1)).unwrap();
            }
            ToolCallState::Succeeded => {
                call.transition(ToolCallState::Running, at(1)).unwrap();
                call.succeed(json!({"ok": true}), at(2)).unwrap();
            }
            ToolCallState::Failed => call.fail(failure(), at(1)).unwrap(),
            ToolCallState::Denied => call.apply_policy_decision(denial(), at(1)).unwrap(),
            ToolCallState::Cancelled | ToolCallState::Interrupted => {
                call.transition(state, at(1)).unwrap();
            }
        }
        call
    }

    fn apply_run_edge(run: &mut Run, to: ExecutionState) -> Result<(), RunDomainError> {
        if to == ExecutionState::Failed {
            run.fail(failure(), at(3))
        } else if run.state() == ExecutionState::WaitingForApproval && to == ExecutionState::Running
        {
            run.approve("fixture-user", at(3))
        } else {
            run.transition(to, at(3))
        }
    }

    fn apply_step_edge(step: &mut Step, to: ExecutionState) -> Result<(), RunDomainError> {
        if to == ExecutionState::Failed {
            step.fail(failure(), at(3))
        } else if step.state() == ExecutionState::WaitingForApproval
            && to == ExecutionState::Running
        {
            step.approve("fixture-user", at(3))
        } else {
            step.transition(to, at(3))
        }
    }

    fn apply_call_edge(call: &mut ToolCall, to: ToolCallState) -> Result<(), RunDomainError> {
        match (call.state(), to) {
            (ToolCallState::AwaitingApproval, ToolCallState::Running) => {
                call.approve("fixture-user", at(3))
            }
            (ToolCallState::AwaitingApproval, ToolCallState::Denied) => {
                call.reject_approval("fixture-user", failure(), at(3))
            }
            (ToolCallState::Pending, ToolCallState::Denied) => {
                call.apply_policy_decision(denial(), at(3))
            }
            (_, ToolCallState::Succeeded) => call.succeed(json!({"ok": true}), at(3)),
            (_, ToolCallState::Failed) => call.fail(failure(), at(3)),
            _ => call.transition(to, at(3)),
        }
    }

    #[test]
    fn every_declared_execution_transition_succeeds_and_every_other_pair_is_invalid() {
        for &from in ExecutionState::ALL {
            for &to in ExecutionState::ALL {
                let expected = EXECUTION_TRANSITIONS.contains(&(from, to));
                let mut run = run_in(from);
                let run_before = run.clone();
                let mut step = step_in(from);
                let step_before = step.clone();
                let run_result = apply_run_edge(&mut run, to);
                let step_result = apply_step_edge(&mut step, to);

                assert_eq!(
                    run_result.is_ok(),
                    expected,
                    "unexpected run edge {from} -> {to}"
                );
                assert_eq!(
                    step_result.is_ok(),
                    expected,
                    "unexpected step edge {from} -> {to}"
                );
                if expected {
                    assert_eq!(run.state(), to);
                    assert_eq!(step.state(), to);
                    assert_eq!(run.revision(), run_before.revision() + 1);
                    assert_eq!(step.revision(), step_before.revision() + 1);
                } else {
                    let error =
                        RunDomainError::InvalidExecutionTransition(InvalidTransition { from, to });
                    assert_eq!(run_result.unwrap_err(), error);
                    assert_eq!(step_result.unwrap_err(), error);
                    assert_eq!(run, run_before);
                    assert_eq!(step, step_before);
                }
            }
        }
    }

    #[test]
    fn every_declared_tool_call_transition_succeeds_and_every_other_pair_is_invalid() {
        for &from in ToolCallState::ALL {
            for &to in ToolCallState::ALL {
                let expected = TOOL_CALL_TRANSITIONS.contains(&(from, to));
                let mut call = call_in(from);
                let before = call.clone();
                let result = apply_call_edge(&mut call, to);

                assert_eq!(
                    result.is_ok(),
                    expected,
                    "unexpected tool-call edge {from} -> {to}"
                );
                if expected {
                    assert_eq!(call.state(), to);
                    assert_eq!(call.revision(), before.revision() + 1);
                } else {
                    assert_eq!(
                        result.unwrap_err(),
                        RunDomainError::InvalidToolCallTransition(InvalidTransition { from, to })
                    );
                    assert_eq!(call, before);
                }
            }
        }
    }

    #[test]
    fn queued_run_can_fail_without_fabricating_a_start_time() {
        let mut run = fresh_run();
        run.fail(
            Failure::new("workspace_missing", "workspace disappeared"),
            at(1),
        )
        .unwrap();

        assert_eq!(run.state(), ExecutionState::Failed);
        assert_eq!(run.started_at(), None);
        assert_eq!(run.finished_at(), Some(at(1)));
        assert_eq!(run.failure().unwrap().kind(), "workspace_missing");
    }

    #[test]
    fn pending_tool_call_can_fail_schema_validation() {
        let mut call = fresh_call();
        call.fail(
            Failure::new("invalid_input", "missing required field"),
            at(1),
        )
        .unwrap();

        assert_eq!(call.state(), ToolCallState::Failed);
        assert_eq!(call.started_at(), None);
        assert_eq!(call.failure().unwrap().kind(), "invalid_input");
    }

    #[test]
    fn dispatch_pins_the_resolved_version_as_part_of_starting() {
        // A call may be recorded without naming a version, and which one won has
        // to be written at the moment execution starts: resolving again later is
        // a second lookup that can disagree with the first.
        let step = fresh_step();
        let mut unpinned = ToolCall::new(&step, "fixture.tool", "", json!({}), at(0));
        assert_eq!(unpinned.tool_version(), "");

        unpinned.dispatch("1.10.0", at(1)).unwrap();

        assert_eq!(unpinned.state(), ToolCallState::Running);
        assert_eq!(unpinned.tool_version(), "1.10.0");
        assert_eq!(unpinned.started_at(), Some(at(1)));

        // Restating what a caller already asked for is fine; contradicting it is
        // a caller bug, and the record is left exactly as it was.
        let mut pinned = fresh_call();
        pinned.dispatch("1.0.0", at(1)).unwrap();
        assert_eq!(pinned.state(), ToolCallState::Running);

        let mut conflicting = fresh_call();
        let error = conflicting.dispatch("2.0.0", at(1)).unwrap_err();
        assert!(error.to_string().contains("may not be replaced"), "{error}");
        assert_eq!(conflicting.state(), ToolCallState::Pending);
        assert_eq!(conflicting.tool_version(), "1.0.0");
    }

    #[test]
    fn an_approved_dispatch_records_the_decision_and_the_version_together() {
        // An approval is a decision about `(id, version)`. Recording the two in
        // separate steps would leave a window in which the call is approved and
        // running while the row still names the version that was *asked for*.
        let mut call = call_in(ToolCallState::AwaitingApproval);
        assert_eq!(call.tool_version(), "1.0.0");

        call.dispatch_approved("reviewer", "1.0.0", at(5)).unwrap();

        assert_eq!(call.state(), ToolCallState::Running);
        assert_eq!(call.tool_version(), "1.0.0");
        assert_eq!(call.approvals().len(), 1);
        assert_eq!(call.approvals()[0].decided_by(), "reviewer");
        assert_eq!(
            call.approvals()[0].decision(),
            crate::domain::ApprovalOutcome::Approved
        );

        // An unpinned call is pinned by the approval, which is the case the
        // pairing exists for.
        let step = fresh_step();
        let mut unpinned = ToolCall::new(&step, "fixture.tool", "", json!({}), at(0));
        unpinned
            .transition(ToolCallState::AwaitingApproval, at(1))
            .unwrap();
        unpinned
            .dispatch_approved("reviewer", "2.3.0", at(2))
            .unwrap();
        assert_eq!(unpinned.tool_version(), "2.3.0");
    }

    #[test]
    fn a_refused_approved_dispatch_leaves_the_record_untouched() {
        // Every refusal is checked before the transition, so a rejected dispatch
        // cannot leave a call running with no approval, or approved with the
        // wrong version.
        for (decided_by, version) in [("", "1.0.0"), ("   ", "1.0.0"), ("reviewer", "2.0.0")] {
            let mut call = call_in(ToolCallState::AwaitingApproval);
            assert!(
                call.dispatch_approved(decided_by, version, at(5)).is_err(),
                "accepted {decided_by:?} at {version}"
            );
            assert_eq!(call.state(), ToolCallState::AwaitingApproval);
            assert_eq!(call.tool_version(), "1.0.0");
            assert!(call.approvals().is_empty());
        }

        // And it is reserved for a call that is actually waiting on a decision.
        for state in [
            ToolCallState::Pending,
            ToolCallState::Running,
            ToolCallState::Succeeded,
            ToolCallState::Failed,
            ToolCallState::Denied,
            ToolCallState::Cancelled,
            ToolCallState::Interrupted,
        ] {
            let mut call = call_in(state);
            assert!(
                call.dispatch_approved("reviewer", "1.0.0", at(9)).is_err(),
                "a {state} call was approved into running"
            );
            assert_eq!(call.state(), state);
        }
    }

    #[test]
    fn only_a_pending_call_can_be_dispatched() {
        for state in [
            ToolCallState::AwaitingApproval,
            ToolCallState::Running,
            ToolCallState::Succeeded,
            ToolCallState::Failed,
            ToolCallState::Denied,
            ToolCallState::Cancelled,
            ToolCallState::Interrupted,
        ] {
            let mut call = call_in(state);
            assert!(
                call.dispatch("1.0.0", at(9)).is_err(),
                "a {state} call was dispatched"
            );
            assert_eq!(call.state(), state);
        }
    }

    #[test]
    fn queued_cancellation_finishes_without_starting() {
        let mut run = fresh_run();
        run.transition(ExecutionState::Cancelled, at(1)).unwrap();

        assert_eq!(run.started_at(), None);
        assert_eq!(run.finished_at(), Some(at(1)));
    }

    #[test]
    fn started_at_survives_approval_round_trip_and_decision_is_audited() {
        let mut run = fresh_run();
        run.transition(ExecutionState::Running, at(1)).unwrap();
        run.transition(ExecutionState::WaitingForApproval, at(2))
            .unwrap();
        run.approve("user:42", at(3)).unwrap();

        assert_eq!(run.started_at(), Some(at(1)));
        assert_eq!(run.approvals().len(), 1);
        assert_eq!(run.approvals()[0].decided_by(), "user:42");
        assert_eq!(run.approvals()[0].decided_at(), at(3));
    }

    #[test]
    fn repeated_and_denied_approval_decisions_remain_auditable() {
        let mut run = fresh_run();
        run.transition(ExecutionState::Running, at(1)).unwrap();
        run.transition(ExecutionState::WaitingForApproval, at(2))
            .unwrap();
        run.approve("user:one", at(3)).unwrap();
        run.transition(ExecutionState::WaitingForApproval, at(4))
            .unwrap();
        run.reject_approval("user:two", failure(), at(5)).unwrap();

        assert_eq!(run.state(), ExecutionState::Failed);
        assert_eq!(run.approvals().len(), 2);
        assert_eq!(
            run.approvals()[0].decision(),
            super::ApprovalOutcome::Approved
        );
        assert_eq!(
            run.approvals()[1].decision(),
            super::ApprovalOutcome::Denied
        );
        assert_eq!(run.approvals()[1].decided_by(), "user:two");
    }

    #[test]
    fn an_empty_approval_identity_is_rejected_without_mutation() {
        let mut call = fresh_call();
        call.transition(ToolCallState::AwaitingApproval, at(1))
            .unwrap();
        let before = call.clone();

        assert_eq!(
            call.approve("  ", at(2)).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "tool_call",
                reason: "approval decisions require decided_by",
            }
        );
        assert_eq!(call, before);
    }

    #[test]
    fn transitions_record_utc_times_revisions_and_tool_output() {
        let mut call = fresh_call();
        let non_utc = at(1).to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());

        call.transition(ToolCallState::Running, non_utc).unwrap();
        assert_eq!(call.started_at(), Some(at(1)));
        assert_eq!(call.updated_at(), at(1));
        assert_eq!(call.updated_at().offset(), time::UtcOffset::UTC);
        assert_eq!(call.revision(), 1);

        call.succeed(json!({"commit": "abc123"}), at(2)).unwrap();
        assert_eq!(call.finished_at(), Some(at(2)));
        assert_eq!(call.revision(), 2);
        assert_eq!(call.output(), Some(&json!({"commit": "abc123"})));
    }

    #[test]
    fn transition_timestamps_never_move_backwards() {
        let mut step = Step::new(fresh_run().id(), 0, "fixture", at(10));

        step.transition(ExecutionState::Running, at(5)).unwrap();
        assert_eq!(step.started_at(), Some(at(10)));
        assert_eq!(step.updated_at(), at(10));
    }

    #[test]
    fn caller_chosen_ids_and_tool_call_containment_are_preserved() {
        let task_id = TaskId::from_str("11111111-1111-4111-8111-111111111111").unwrap();
        let run_id = RunId::from_str("22222222-2222-4222-8222-222222222222").unwrap();
        let step_id = StepId::from_str("33333333-3333-4333-8333-333333333333").unwrap();
        let call_id = ToolCallId::from_str("44444444-4444-4444-8444-444444444444").unwrap();
        let task = Task::with_id(task_id, "fixture", "/fixture", None, at(0));
        let run = Run::with_id(run_id, task.id(), at(0));
        let step = Step::with_id(step_id, run.id(), 0, "fixture", at(0));
        let call = ToolCall::with_id(call_id, &step, "fixture.tool", "1.0.0", json!({}), at(0));

        assert_eq!(task.id(), task_id);
        assert_eq!(run.id(), run_id);
        assert_eq!(step.id(), step_id);
        assert_eq!(call.id(), call_id);
        assert_eq!(call.step_id(), step.id());
        assert_eq!(call.run_id(), step.run_id());
    }

    #[test]
    fn revision_exhaustion_does_not_mutate_the_record() {
        let mut run = fresh_run();
        run.lifecycle.revision = u64::MAX;
        let before = run.clone();

        assert_eq!(
            run.transition(ExecutionState::Running, at(1)).unwrap_err(),
            RunDomainError::RevisionExhausted { record: "run" }
        );
        assert_eq!(run, before);
    }
}
