use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use time::OffsetDateTime;

use super::record::{Lifecycle, invalid_lifecycle, invalid_timestamp, validate_utc};
use super::{
    Approval, ApprovalDecision, ExecutionState, Failure, Run, RunDomainError, RunId, Step, StepId,
    Task, TaskId, ToolCall, ToolCallId, ToolCallState,
};

/// Newest durable runtime-record schema understood by this build.
pub const RUNTIME_RECORD_SCHEMA_VERSION: u32 = 1;
/// Oldest durable runtime-record schema understood by this build.
pub const MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION: u32 = 1;

/// Strict owned representation used to deserialize a [`Task`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TaskWire {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable task identifier.
    pub id: TaskId,
    /// User-facing task title.
    pub title: String,
    /// Workspace against which the task runs.
    ///
    /// JSON serialization fails when this platform path is not valid UTF-8.
    pub workspace_root: PathBuf,
    /// Catalog project associated with the workspace, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Strict owned representation used to deserialize a [`Run`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunWire {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable run identifier.
    pub id: RunId,
    /// Task this run attempts to execute.
    pub task_id: TaskId,
    /// Current lifecycle state.
    pub state: ExecutionState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 time at which the current state was entered.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 time execution first entered `running`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 time a terminal state was entered.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail, present only in `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Durable approval audit history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
}

/// Strict owned representation used to deserialize a [`Step`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StepWire {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable step identifier.
    pub id: StepId,
    /// Run containing this step.
    pub run_id: RunId,
    /// Zero-based position within the run; persistence enforces uniqueness.
    pub ordinal: u32,
    /// User-facing step title.
    pub title: String,
    /// Current lifecycle state.
    pub state: ExecutionState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 time at which the current state was entered.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 time execution first entered `running`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 time a terminal state was entered.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail, present only in `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Durable approval audit history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
}

/// Strict owned representation used to deserialize a [`ToolCall`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolCallWire {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable tool-call identifier.
    pub id: ToolCallId,
    /// Run containing this call, denormalized for correlation and storage.
    ///
    /// Persistence must validate that the referenced step belongs to this run.
    pub run_id: RunId,
    /// Step containing this call.
    pub step_id: StepId,
    /// Stable dotted identifier of the requested tool.
    pub tool_id: String,
    /// Requested immutable tool version.
    pub tool_version: String,
    /// Raw input awaiting validation by the typed tool layer.
    pub input: Value,
    /// Current lifecycle state.
    pub state: ToolCallState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 time at which the current state was entered.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 time execution first entered `running`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 time a terminal state was entered.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail, present only in `failed` or `denied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Tool result, present only in `succeeded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Durable approval audit history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
}

/// Borrowing representation used to serialize a [`Task`] without cloning it.
#[derive(Debug, Serialize)]
pub struct TaskWireRef<'a> {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable task identifier.
    pub id: TaskId,
    /// User-facing task title.
    pub title: &'a str,
    /// Workspace against which the task runs.
    pub workspace_root: &'a Path,
    /// Catalog project associated with the workspace, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Borrowing representation used to serialize a [`Run`] without cloning it.
#[derive(Debug, Serialize)]
pub struct RunWireRef<'a> {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable run identifier.
    pub id: RunId,
    /// Task this run attempts to execute.
    pub task_id: TaskId,
    /// Current lifecycle state.
    pub state: ExecutionState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 state-update time.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 execution-start time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 terminal-state time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<&'a Failure>,
    /// Durable approval audit history.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub approvals: &'a [Approval],
}

/// Borrowing representation used to serialize a [`Step`] without cloning it.
#[derive(Debug, Serialize)]
pub struct StepWireRef<'a> {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable step identifier.
    pub id: StepId,
    /// Run containing this step.
    pub run_id: RunId,
    /// Zero-based position within the run.
    pub ordinal: u32,
    /// User-facing step title.
    pub title: &'a str,
    /// Current lifecycle state.
    pub state: ExecutionState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 state-update time.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 execution-start time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 terminal-state time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<&'a Failure>,
    /// Durable approval audit history.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub approvals: &'a [Approval],
}

/// Borrowing representation used to serialize a [`ToolCall`] without cloning input or output.
#[derive(Debug, Serialize)]
pub struct ToolCallWireRef<'a> {
    /// Runtime-record schema version.
    pub schema_version: u32,
    /// Stable tool-call identifier.
    pub id: ToolCallId,
    /// Denormalized containing run ID.
    pub run_id: RunId,
    /// Containing step ID.
    pub step_id: StepId,
    /// Stable dotted tool identifier.
    pub tool_id: &'a str,
    /// Requested immutable tool version.
    pub tool_version: &'a str,
    /// Raw typed-tool input.
    pub input: &'a Value,
    /// Current lifecycle state.
    pub state: ToolCallState,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// UTC RFC 3339 state-update time.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// UTC RFC 3339 execution-start time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// UTC RFC 3339 terminal-state time.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub finished_at: Option<OffsetDateTime>,
    /// Structured failure detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<&'a Failure>,
    /// Tool result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<&'a Value>,
    /// Durable approval audit history.
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub approvals: &'a [Approval],
}

impl<'a> From<&'a Task> for TaskWireRef<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            schema_version: RUNTIME_RECORD_SCHEMA_VERSION,
            id: task.id(),
            title: task.title(),
            workspace_root: task.workspace_root(),
            project_id: task.project_id(),
            created_at: task.created_at(),
        }
    }
}

impl<'a> From<&'a Run> for RunWireRef<'a> {
    fn from(run: &'a Run) -> Self {
        Self {
            schema_version: RUNTIME_RECORD_SCHEMA_VERSION,
            id: run.id(),
            task_id: run.task_id(),
            state: run.state(),
            revision: run.revision(),
            created_at: run.created_at(),
            updated_at: run.updated_at(),
            started_at: run.started_at(),
            finished_at: run.finished_at(),
            failure: run.failure(),
            approvals: run.approvals(),
        }
    }
}

impl<'a> From<&'a Step> for StepWireRef<'a> {
    fn from(step: &'a Step) -> Self {
        Self {
            schema_version: RUNTIME_RECORD_SCHEMA_VERSION,
            id: step.id(),
            run_id: step.run_id(),
            ordinal: step.ordinal(),
            title: step.title(),
            state: step.state(),
            revision: step.revision(),
            created_at: step.created_at(),
            updated_at: step.updated_at(),
            started_at: step.started_at(),
            finished_at: step.finished_at(),
            failure: step.failure(),
            approvals: step.approvals(),
        }
    }
}

impl<'a> From<&'a ToolCall> for ToolCallWireRef<'a> {
    fn from(call: &'a ToolCall) -> Self {
        Self {
            schema_version: RUNTIME_RECORD_SCHEMA_VERSION,
            id: call.id(),
            run_id: call.run_id(),
            step_id: call.step_id(),
            tool_id: call.tool_id(),
            tool_version: call.tool_version(),
            input: call.input(),
            state: call.state(),
            revision: call.revision(),
            created_at: call.created_at(),
            updated_at: call.updated_at(),
            started_at: call.started_at(),
            finished_at: call.finished_at(),
            failure: call.failure(),
            output: call.output(),
            approvals: call.approvals(),
        }
    }
}

impl TryFrom<TaskWire> for Task {
    type Error = RunDomainError;

    fn try_from(wire: TaskWire) -> Result<Self, Self::Error> {
        validate_schema_version("task", wire.schema_version)?;
        validate_utc("task", "created_at", wire.created_at)?;
        Ok(Self {
            id: wire.id,
            title: wire.title,
            workspace_root: wire.workspace_root,
            project_id: wire.project_id,
            created_at: wire.created_at,
        })
    }
}

impl TryFrom<RunWire> for Run {
    type Error = RunDomainError;

    fn try_from(wire: RunWire) -> Result<Self, Self::Error> {
        validate_schema_version("run", wire.schema_version)?;
        let lifecycle = Lifecycle::from_wire(
            "run",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.revision,
        )?;
        validate_failure(
            "run",
            wire.state == ExecutionState::Failed,
            wire.failure.as_ref(),
        )?;
        validate_approvals(
            "run",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.revision,
            &wire.approvals,
        )?;
        Ok(Self {
            id: wire.id,
            task_id: wire.task_id,
            lifecycle,
            failure: wire.failure,
            approvals: wire.approvals,
        })
    }
}

impl TryFrom<StepWire> for Step {
    type Error = RunDomainError;

    fn try_from(wire: StepWire) -> Result<Self, Self::Error> {
        validate_schema_version("step", wire.schema_version)?;
        let lifecycle = Lifecycle::from_wire(
            "step",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.revision,
        )?;
        validate_failure(
            "step",
            wire.state == ExecutionState::Failed,
            wire.failure.as_ref(),
        )?;
        validate_approvals(
            "step",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.revision,
            &wire.approvals,
        )?;
        Ok(Self {
            id: wire.id,
            run_id: wire.run_id,
            ordinal: wire.ordinal,
            title: wire.title,
            lifecycle,
            failure: wire.failure,
            approvals: wire.approvals,
        })
    }
}

impl TryFrom<ToolCallWire> for ToolCall {
    type Error = RunDomainError;

    fn try_from(wire: ToolCallWire) -> Result<Self, Self::Error> {
        validate_schema_version("tool_call", wire.schema_version)?;
        let lifecycle = Lifecycle::from_wire(
            "tool_call",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.revision,
        )?;
        validate_failure(
            "tool_call",
            matches!(wire.state, ToolCallState::Failed | ToolCallState::Denied),
            wire.failure.as_ref(),
        )?;
        validate_output(wire.state, wire.output.as_ref())?;
        validate_approvals(
            "tool_call",
            wire.state,
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.revision,
            &wire.approvals,
        )?;
        Ok(Self {
            id: wire.id,
            run_id: wire.run_id,
            step_id: wire.step_id,
            tool_id: wire.tool_id,
            tool_version: wire.tool_version,
            input: wire.input,
            lifecycle,
            failure: wire.failure,
            output: wire.output,
            approvals: wire.approvals,
        })
    }
}

/// Refuses a record schema version this build cannot read.
///
/// Exposed so a persistence layer can probe a row's version before it decodes
/// anything else. A future record may spell a field in a way this build cannot
/// parse, and the caller should learn that it needs an upgrade rather than that
/// some column looked corrupt.
pub fn validate_record_schema_version(
    record: &'static str,
    found: u32,
) -> Result<(), RunDomainError> {
    validate_schema_version(record, found)
}

fn validate_schema_version(record: &'static str, found: u32) -> Result<(), RunDomainError> {
    if found < MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION {
        return Err(RunDomainError::SchemaVersionTooOld {
            record,
            found,
            minimum: MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION,
        });
    }
    if found > RUNTIME_RECORD_SCHEMA_VERSION {
        return Err(RunDomainError::SchemaVersionTooNew {
            record,
            found,
            maximum: RUNTIME_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn validate_failure(
    record: &'static str,
    required: bool,
    failure: Option<&Failure>,
) -> Result<(), RunDomainError> {
    match (required, failure) {
        (true, None) => Err(invalid_lifecycle(
            record,
            "failed or denied states require failure detail",
        )),
        (false, Some(_)) => Err(invalid_lifecycle(
            record,
            "failure detail is permitted only in failed or denied states",
        )),
        _ => Ok(()),
    }
}

fn validate_output(state: ToolCallState, output: Option<&Value>) -> Result<(), RunDomainError> {
    match (state == ToolCallState::Succeeded, output) {
        (true, None) => Err(invalid_lifecycle("tool_call", "succeeded requires output")),
        (false, Some(_)) => Err(invalid_lifecycle(
            "tool_call",
            "output is permitted only in succeeded",
        )),
        _ => Ok(()),
    }
}

trait ApprovalState {
    fn is_initial(&self) -> bool;
    fn allows_denied_approval(&self) -> bool;
    fn validate_approval_shape(
        self,
        record: &'static str,
        started_at: Option<OffsetDateTime>,
        revision: u64,
        approvals: &[Approval],
    ) -> Result<(), RunDomainError>;
}

impl ApprovalState for ExecutionState {
    fn is_initial(&self) -> bool {
        *self == Self::Queued
    }

    fn allows_denied_approval(&self) -> bool {
        *self == Self::Failed
    }

    fn validate_approval_shape(
        self,
        record: &'static str,
        started_at: Option<OffsetDateTime>,
        revision: u64,
        approvals: &[Approval],
    ) -> Result<(), RunDomainError> {
        if approvals.is_empty() {
            return Ok(());
        }
        if started_at.is_none() {
            return Err(invalid_lifecycle(
                record,
                "execution approval history requires started_at",
            ));
        }
        let count = u64::try_from(approvals.len()).map_err(|_| {
            invalid_lifecycle(record, "approval count cannot be represented by revision")
        })?;
        let minimum_revision = count
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                invalid_lifecycle(record, "approval count cannot be represented by revision")
            })?;
        if revision < minimum_revision {
            return Err(invalid_lifecycle(
                record,
                "execution approval history requires more lifecycle revisions",
            ));
        }
        Ok(())
    }
}

impl ApprovalState for ToolCallState {
    fn is_initial(&self) -> bool {
        *self == Self::Pending
    }

    fn allows_denied_approval(&self) -> bool {
        *self == Self::Denied
    }

    fn validate_approval_shape(
        self,
        record: &'static str,
        _started_at: Option<OffsetDateTime>,
        revision: u64,
        approvals: &[Approval],
    ) -> Result<(), RunDomainError> {
        if approvals.len() > 1 {
            return Err(invalid_lifecycle(
                record,
                "a tool call can carry at most one approval decision",
            ));
        }
        let Some(approval) = approvals.first() else {
            return Ok(());
        };
        if matches!(self, Self::Pending | Self::AwaitingApproval) {
            return Err(invalid_lifecycle(
                record,
                "an undecided tool-call state cannot carry an approval decision",
            ));
        }
        if revision < 2 {
            return Err(invalid_lifecycle(
                record,
                "a tool-call approval decision requires two lifecycle revisions",
            ));
        }
        if self == Self::Denied && approval.decision() != ApprovalDecision::Denied {
            return Err(invalid_lifecycle(
                record,
                "a denied tool call cannot carry an approved decision",
            ));
        }
        Ok(())
    }
}

fn validate_approvals<S>(
    record: &'static str,
    state: S,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    revision: u64,
    approvals: &[Approval],
) -> Result<(), RunDomainError>
where
    S: ApprovalState + Copy,
{
    if state.is_initial() && !approvals.is_empty() {
        return Err(invalid_lifecycle(
            record,
            "the initial state cannot carry approval decisions",
        ));
    }
    state.validate_approval_shape(record, started_at, revision, approvals)?;
    if u64::try_from(approvals.len()).is_ok_and(|count| count > revision) {
        return Err(invalid_lifecycle(
            record,
            "approval count cannot exceed revision",
        ));
    }

    let mut previous = None;
    let mut denied_index = None;
    for (index, approval) in approvals.iter().enumerate() {
        validate_utc(record, "approvals.decided_at", approval.decided_at())?;
        if approval.decided_at() < created_at || approval.decided_at() > updated_at {
            return Err(invalid_timestamp(
                record,
                "approvals.decided_at",
                "must fall between created_at and updated_at",
            ));
        }
        if previous.is_some_and(|at| approval.decided_at() < at) {
            return Err(invalid_lifecycle(
                record,
                "approval decisions must be ordered by decided_at",
            ));
        }
        if approval.decided_by().trim().is_empty() {
            return Err(invalid_lifecycle(
                record,
                "approval decisions require decided_by",
            ));
        }
        if approval.decision() == ApprovalDecision::Denied && denied_index.replace(index).is_some()
        {
            return Err(invalid_lifecycle(
                record,
                "approval history cannot contain multiple denials",
            ));
        }
        previous = Some(approval.decided_at());
    }

    if let Some(index) = denied_index
        && (index + 1 != approvals.len() || !state.allows_denied_approval())
    {
        return Err(invalid_lifecycle(
            record,
            "a denied approval must be the final decision and terminal outcome",
        ));
    }
    Ok(())
}

fn slice_is_empty<T>(values: &&[T]) -> bool {
    values.is_empty()
}

#[derive(Deserialize)]
struct SchemaVersionProbe {
    schema_version: u32,
}

macro_rules! impl_versioned_deserialize {
    ($wire:ty, $strict:ty, $record:literal) => {
        impl<'de> Deserialize<'de> for $wire {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = Value::deserialize(deserializer)?;
                let probe = SchemaVersionProbe::deserialize(&value).map_err(de::Error::custom)?;
                validate_schema_version($record, probe.schema_version)
                    .map_err(de::Error::custom)?;
                <$strict>::deserialize(value)
                    .map(Into::into)
                    .map_err(de::Error::custom)
            }
        }
    };
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskWireStrict {
    schema_version: u32,
    id: TaskId,
    title: String,
    workspace_root: PathBuf,
    #[serde(default)]
    project_id: Option<ProjectId>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl From<TaskWireStrict> for TaskWire {
    fn from(wire: TaskWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            id: wire.id,
            title: wire.title,
            workspace_root: wire.workspace_root,
            project_id: wire.project_id,
            created_at: wire.created_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunWireStrict {
    schema_version: u32,
    id: RunId,
    task_id: TaskId,
    state: ExecutionState,
    revision: u64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    finished_at: Option<OffsetDateTime>,
    #[serde(default)]
    failure: Option<Failure>,
    #[serde(default)]
    approvals: Vec<Approval>,
}

impl From<RunWireStrict> for RunWire {
    fn from(wire: RunWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            id: wire.id,
            task_id: wire.task_id,
            state: wire.state,
            revision: wire.revision,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
            failure: wire.failure,
            approvals: wire.approvals,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepWireStrict {
    schema_version: u32,
    id: StepId,
    run_id: RunId,
    ordinal: u32,
    title: String,
    state: ExecutionState,
    revision: u64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    finished_at: Option<OffsetDateTime>,
    #[serde(default)]
    failure: Option<Failure>,
    #[serde(default)]
    approvals: Vec<Approval>,
}

impl From<StepWireStrict> for StepWire {
    fn from(wire: StepWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            id: wire.id,
            run_id: wire.run_id,
            ordinal: wire.ordinal,
            title: wire.title,
            state: wire.state,
            revision: wire.revision,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
            failure: wire.failure,
            approvals: wire.approvals,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallWireStrict {
    schema_version: u32,
    id: ToolCallId,
    run_id: RunId,
    step_id: StepId,
    tool_id: String,
    tool_version: String,
    input: Value,
    state: ToolCallState,
    revision: u64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    finished_at: Option<OffsetDateTime>,
    #[serde(default)]
    failure: Option<Failure>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    approvals: Vec<Approval>,
}

impl From<ToolCallWireStrict> for ToolCallWire {
    fn from(wire: ToolCallWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            id: wire.id,
            run_id: wire.run_id,
            step_id: wire.step_id,
            tool_id: wire.tool_id,
            tool_version: wire.tool_version,
            input: wire.input,
            state: wire.state,
            revision: wire.revision,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
            failure: wire.failure,
            output: wire.output,
            approvals: wire.approvals,
        }
    }
}

impl_versioned_deserialize!(TaskWire, TaskWireStrict, "task");
impl_versioned_deserialize!(RunWire, RunWireStrict, "run");
impl_versioned_deserialize!(StepWire, StepWireStrict, "step");
impl_versioned_deserialize!(ToolCallWire, ToolCallWireStrict, "tool_call");

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf, str::FromStr};

    use harkness_core::ProjectId;
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};
    use time::{Duration, OffsetDateTime, UtcOffset};

    use super::{
        MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION, RUNTIME_RECORD_SCHEMA_VERSION, RunWire, RunWireRef,
        StepWire, StepWireRef, TaskWire, TaskWireRef, ToolCallWire, ToolCallWireRef,
    };
    use crate::domain::{
        Approval, ApprovalDecision, EXECUTION_TRANSITIONS, ExecutionState, Failure, Run,
        RunDomainError, RunId, Step, StepId, TOOL_CALL_TRANSITIONS, Task, TaskId, ToolCall,
        ToolCallId, ToolCallState,
    };

    fn at(second: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(second)
    }

    fn task_id() -> TaskId {
        TaskId::from_str("11111111-1111-4111-8111-111111111111").unwrap()
    }

    fn run_id() -> RunId {
        RunId::from_str("22222222-2222-4222-8222-222222222222").unwrap()
    }

    fn step_id() -> StepId {
        StepId::from_str("33333333-3333-4333-8333-333333333333").unwrap()
    }

    fn call_id() -> ToolCallId {
        ToolCallId::from_str("44444444-4444-4444-8444-444444444444").unwrap()
    }

    fn failure() -> Failure {
        Failure::new("fixture_failure", "fixture failed")
    }

    fn task_wire() -> TaskWire {
        TaskWire {
            schema_version: RUNTIME_RECORD_SCHEMA_VERSION,
            id: task_id(),
            title: "Inspect repository".to_owned(),
            workspace_root: PathBuf::from("/fixture"),
            project_id: Some(ProjectId::from_str("55555555-5555-4555-8555-555555555555").unwrap()),
            created_at: at(0),
        }
    }

    fn run_wire(state: ExecutionState) -> RunWire {
        let task = Task::with_id(task_id(), "fixture", "/fixture", None, at(0));
        let mut run = Run::with_id(run_id(), task.id(), at(0));
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
        serde_json::from_value(serde_json::to_value(RunWireRef::from(&run)).unwrap()).unwrap()
    }

    fn step_wire(state: ExecutionState) -> StepWire {
        let mut step = Step::with_id(step_id(), run_id(), 0, "Inspect", at(0));
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
        serde_json::from_value(serde_json::to_value(StepWireRef::from(&step)).unwrap()).unwrap()
    }

    fn call_wire(state: ToolCallState) -> ToolCallWire {
        let step = Step::with_id(step_id(), run_id(), 0, "Inspect", at(0));
        let mut call = ToolCall::with_id(
            call_id(),
            &step,
            "git.status",
            "1.0.0",
            json!({"include_untracked": true}),
            at(0),
        );
        match state {
            ToolCallState::Pending => {}
            ToolCallState::AwaitingApproval | ToolCallState::Running => {
                call.transition(state, at(1)).unwrap();
            }
            ToolCallState::Succeeded => {
                call.transition(ToolCallState::Running, at(1)).unwrap();
                call.succeed(json!({"clean": true}), at(2)).unwrap();
            }
            ToolCallState::Failed => call.fail(failure(), at(1)).unwrap(),
            ToolCallState::Denied => call.deny(failure(), at(1)).unwrap(),
            ToolCallState::Cancelled | ToolCallState::Interrupted => {
                call.transition(state, at(1)).unwrap();
            }
        }
        serde_json::from_value(serde_json::to_value(ToolCallWireRef::from(&call)).unwrap()).unwrap()
    }

    fn with_unknown_field<T: Serialize>(wire: T) -> Value {
        let mut value = serde_json::to_value(wire).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_owned(), json!(true));
        value
    }

    fn assert_unknown_field(error: serde_json::Error) {
        let message = error.to_string();
        assert!(
            message.contains("unknown field `unexpected`"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn every_wire_type_rejects_unknown_fields_for_the_current_schema() {
        assert_unknown_field(
            serde_json::from_value::<TaskWire>(with_unknown_field(task_wire())).unwrap_err(),
        );
        assert_unknown_field(
            serde_json::from_value::<RunWire>(with_unknown_field(run_wire(ExecutionState::Queued)))
                .unwrap_err(),
        );
        assert_unknown_field(
            serde_json::from_value::<StepWire>(with_unknown_field(step_wire(
                ExecutionState::Queued,
            )))
            .unwrap_err(),
        );
        assert_unknown_field(
            serde_json::from_value::<ToolCallWire>(with_unknown_field(call_wire(
                ToolCallState::Pending,
            )))
            .unwrap_err(),
        );
    }

    #[test]
    fn future_schema_is_reported_before_future_fields_are_parsed() {
        let mut value = with_unknown_field(run_wire(ExecutionState::Queued));
        value["schema_version"] = json!(RUNTIME_RECORD_SCHEMA_VERSION + 1);

        let message = serde_json::from_value::<RunWire>(value)
            .unwrap_err()
            .to_string();
        assert!(message.contains("is newer than the maximum supported version"));
        assert!(message.contains("upgrade Harkness"));
        assert!(!message.contains("unknown field"));
    }

    #[test]
    fn old_and_programmatically_constructed_future_versions_are_typed() {
        let mut old = task_wire();
        old.schema_version = 0;
        assert_eq!(
            Task::try_from(old).unwrap_err(),
            RunDomainError::SchemaVersionTooOld {
                record: "task",
                found: 0,
                minimum: MINIMUM_RUNTIME_RECORD_SCHEMA_VERSION,
            }
        );

        let mut future = step_wire(ExecutionState::Queued);
        future.schema_version = RUNTIME_RECORD_SCHEMA_VERSION + 1;
        assert_eq!(
            Step::try_from(future).unwrap_err(),
            RunDomainError::SchemaVersionTooNew {
                record: "step",
                found: RUNTIME_RECORD_SCHEMA_VERSION + 1,
                maximum: RUNTIME_RECORD_SCHEMA_VERSION,
            }
        );
    }

    #[test]
    fn frozen_v1_json_fixtures_cover_every_record_type() {
        let task = Task::try_from(task_wire()).unwrap();

        let mut run = Run::with_id(run_id(), task.id(), at(0));
        run.fail(
            Failure::new("workspace_missing", "workspace no longer exists"),
            at(1),
        )
        .unwrap();

        let mut step = Step::with_id(step_id(), run.id(), 0, "Inspect repository", at(0));
        step.transition(ExecutionState::Running, at(1)).unwrap();
        step.transition(ExecutionState::WaitingForApproval, at(2))
            .unwrap();
        step.approve("user:42", at(3)).unwrap();

        let mut call = ToolCall::with_id(
            call_id(),
            &step,
            "git.status",
            "1.0.0",
            json!({"include_untracked": true}),
            at(0),
        );
        call.transition(ToolCallState::AwaitingApproval, at(1))
            .unwrap();
        call.approve("user:42", at(2)).unwrap();
        call.succeed(json!({"clean": true}), at(3)).unwrap();

        assert_fixture(
            TaskWireRef::from(&task),
            include_str!("fixtures/task-v1.json"),
        );
        assert_fixture(RunWireRef::from(&run), include_str!("fixtures/run-v1.json"));
        assert_fixture(
            StepWireRef::from(&step),
            include_str!("fixtures/step-v1.json"),
        );
        assert_fixture(
            ToolCallWireRef::from(&call),
            include_str!("fixtures/tool-call-v1.json"),
        );

        assert_owned_fixture::<TaskWire>(include_str!("fixtures/task-v1.json"));
        assert_owned_fixture::<RunWire>(include_str!("fixtures/run-v1.json"));
        assert_owned_fixture::<StepWire>(include_str!("fixtures/step-v1.json"));
        assert_owned_fixture::<ToolCallWire>(include_str!("fixtures/tool-call-v1.json"));
    }

    fn assert_fixture(wire: impl Serialize, fixture: &str) {
        let actual = format!("{}\n", serde_json::to_string_pretty(&wire).unwrap());
        assert_eq!(actual, fixture);
    }

    fn assert_owned_fixture<T>(fixture: &str)
    where
        T: DeserializeOwned + Serialize,
    {
        let wire = serde_json::from_str::<T>(fixture).unwrap();
        assert_fixture(wire, fixture);
    }

    #[test]
    fn transition_engine_and_wire_validator_agree_for_every_reachable_record() {
        let task = Task::with_id(task_id(), "fixture", "/fixture", None, at(0));
        let run = Run::with_id(run_id(), task.id(), at(0));
        walk_run(run, 0, &mut HashSet::new());

        let step = Step::with_id(step_id(), run_id(), 0, "fixture", at(0));
        walk_step(step, 0, &mut HashSet::new());

        let step = Step::with_id(step_id(), run_id(), 0, "fixture", at(0));
        let call = ToolCall::with_id(call_id(), &step, "fixture.tool", "1.0.0", json!({}), at(0));
        walk_call(call, 0, &mut HashSet::new());
    }

    fn walk_run(run: Run, clock: i64, seen: &mut HashSet<(ExecutionState, bool, bool)>) {
        if !seen.insert((
            run.state(),
            run.started_at().is_some(),
            !run.approvals().is_empty(),
        )) {
            return;
        }
        let wire: RunWire =
            serde_json::from_value(serde_json::to_value(RunWireRef::from(&run)).unwrap()).unwrap();
        assert_eq!(Run::try_from(wire).unwrap(), run);

        for &(_, to) in EXECUTION_TRANSITIONS
            .iter()
            .filter(|(from, _)| *from == run.state())
        {
            let mut next = run.clone();
            apply_run_edge(&mut next, to, at(clock + 1));
            walk_run(next, clock + 1, seen);
        }
    }

    fn walk_step(step: Step, clock: i64, seen: &mut HashSet<(ExecutionState, bool, bool)>) {
        if !seen.insert((
            step.state(),
            step.started_at().is_some(),
            !step.approvals().is_empty(),
        )) {
            return;
        }
        let wire: StepWire =
            serde_json::from_value(serde_json::to_value(StepWireRef::from(&step)).unwrap())
                .unwrap();
        assert_eq!(Step::try_from(wire).unwrap(), step);

        for &(_, to) in EXECUTION_TRANSITIONS
            .iter()
            .filter(|(from, _)| *from == step.state())
        {
            let mut next = step.clone();
            apply_step_edge(&mut next, to, at(clock + 1));
            walk_step(next, clock + 1, seen);
        }
    }

    fn walk_call(call: ToolCall, clock: i64, seen: &mut HashSet<(ToolCallState, bool, bool)>) {
        if !seen.insert((
            call.state(),
            call.started_at().is_some(),
            !call.approvals().is_empty(),
        )) {
            return;
        }
        let wire: ToolCallWire =
            serde_json::from_value(serde_json::to_value(ToolCallWireRef::from(&call)).unwrap())
                .unwrap();
        assert_eq!(ToolCall::try_from(wire).unwrap(), call);

        for &(_, to) in TOOL_CALL_TRANSITIONS
            .iter()
            .filter(|(from, _)| *from == call.state())
        {
            let mut next = call.clone();
            apply_call_edge(&mut next, to, at(clock + 1));
            walk_call(next, clock + 1, seen);
        }
    }

    fn apply_run_edge(run: &mut Run, to: ExecutionState, at: OffsetDateTime) {
        if to == ExecutionState::Failed {
            run.fail(failure(), at).unwrap();
        } else if run.state() == ExecutionState::WaitingForApproval && to == ExecutionState::Running
        {
            run.approve("fixture-user", at).unwrap();
        } else {
            run.transition(to, at).unwrap();
        }
    }

    fn apply_step_edge(step: &mut Step, to: ExecutionState, at: OffsetDateTime) {
        if to == ExecutionState::Failed {
            step.fail(failure(), at).unwrap();
        } else if step.state() == ExecutionState::WaitingForApproval
            && to == ExecutionState::Running
        {
            step.approve("fixture-user", at).unwrap();
        } else {
            step.transition(to, at).unwrap();
        }
    }

    fn apply_call_edge(call: &mut ToolCall, to: ToolCallState, at: OffsetDateTime) {
        match (call.state(), to) {
            (ToolCallState::AwaitingApproval, ToolCallState::Running) => {
                call.approve("fixture-user", at).unwrap();
            }
            (ToolCallState::AwaitingApproval, ToolCallState::Denied) => {
                call.reject_approval("fixture-user", failure(), at).unwrap();
            }
            (_, ToolCallState::Succeeded) => call.succeed(json!({"ok": true}), at).unwrap(),
            (_, ToolCallState::Failed) => call.fail(failure(), at).unwrap(),
            (_, ToolCallState::Denied) => call.deny(failure(), at).unwrap(),
            _ => call.transition(to, at).unwrap(),
        }
    }

    #[test]
    fn task_run_step_and_tool_call_timestamp_errors_keep_their_record_labels() {
        let mut task = task_wire();
        task.created_at = at(0).to_offset(UtcOffset::from_hms(1, 0, 0).unwrap());
        assert_eq!(
            Task::try_from(task).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "task",
                field: "created_at",
                reason: "must use the UTC offset",
            }
        );

        let mut run = run_wire(ExecutionState::Running);
        run.updated_at = at(1).to_offset(UtcOffset::from_hms(1, 0, 0).unwrap());
        assert_eq!(
            Run::try_from(run).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "run",
                field: "updated_at",
                reason: "must use the UTC offset",
            }
        );

        let mut step = step_wire(ExecutionState::Running);
        step.started_at = Some(at(1).to_offset(UtcOffset::from_hms(1, 0, 0).unwrap()));
        assert_eq!(
            Step::try_from(step).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "step",
                field: "started_at",
                reason: "must use the UTC offset",
            }
        );

        let mut call = call_wire(ToolCallState::Cancelled);
        call.finished_at = Some(at(1).to_offset(UtcOffset::from_hms(1, 0, 0).unwrap()));
        assert_eq!(
            ToolCall::try_from(call).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "tool_call",
                field: "finished_at",
                reason: "must use the UTC offset",
            }
        );
    }

    #[test]
    fn wire_outcome_payloads_are_required_and_state_gated() {
        let mut run = run_wire(ExecutionState::Failed);
        run.failure = None;
        assert_eq!(
            Run::try_from(run).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "run",
                reason: "failed or denied states require failure detail",
            }
        );

        let mut step = step_wire(ExecutionState::Queued);
        step.failure = Some(failure());
        assert_eq!(
            Step::try_from(step).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "step",
                reason: "failure detail is permitted only in failed or denied states",
            }
        );

        let mut call = call_wire(ToolCallState::Succeeded);
        call.output = None;
        assert_eq!(
            ToolCall::try_from(call).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "tool_call",
                reason: "succeeded requires output",
            }
        );
    }

    #[test]
    fn wire_approval_history_rejects_unreachable_shapes() {
        let mut call = call_wire(ToolCallState::Running);
        call.approvals
            .push(Approval::new("user:42", ApprovalDecision::Approved, at(1)));
        assert_eq!(
            ToolCall::try_from(call).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "tool_call",
                reason: "a tool-call approval decision requires two lifecycle revisions",
            }
        );

        let mut run = run_wire(ExecutionState::Failed);
        run.approvals
            .push(Approval::new("user:42", ApprovalDecision::Denied, at(1)));
        assert_eq!(
            Run::try_from(run).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "run",
                reason: "execution approval history requires started_at",
            }
        );
    }

    #[test]
    fn wire_lifecycle_rejects_impossible_timestamp_ordering() {
        let mut wire = call_wire(ToolCallState::Running);
        wire.started_at = Some(at(2));
        wire.updated_at = at(1);

        assert_eq!(
            ToolCall::try_from(wire).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "tool_call",
                field: "started_at",
                reason: "must fall between created_at and updated_at",
            }
        );
    }

    #[test]
    fn wire_lifecycle_rejects_a_revision_too_small_for_its_state() {
        let mut wire = run_wire(ExecutionState::Succeeded);
        wire.revision = 1;

        assert_eq!(
            Run::try_from(wire).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "run",
                reason: "revision is too small for the current state",
            }
        );
    }
}
