use std::path::PathBuf;

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use super::{
    Run, RunDomainError, RunId, RunState, Step, StepId, Task, TaskId, ToolCall, ToolCallId,
    ToolCallState,
};

/// Strict serializable representation of a [`Task`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWire {
    /// Stable task identifier.
    pub id: TaskId,
    /// User-facing task title.
    pub title: String,
    /// Workspace against which the task runs.
    pub workspace_root: PathBuf,
    /// Catalog project associated with the workspace, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// UTC RFC 3339 creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Strict serializable representation of a [`Run`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunWire {
    /// Stable run identifier.
    pub id: RunId,
    /// Task this run attempts to execute.
    pub task_id: TaskId,
    /// Current lifecycle state.
    pub state: RunState,
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
}

/// Strict serializable representation of a [`Step`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepWire {
    /// Stable step identifier.
    pub id: StepId,
    /// Run containing this step.
    pub run_id: RunId,
    /// Zero-based position within the run.
    pub ordinal: u32,
    /// User-facing step title.
    pub title: String,
    /// Current lifecycle state.
    pub state: RunState,
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
}

/// Strict serializable representation of a [`ToolCall`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallWire {
    /// Stable tool-call identifier.
    pub id: ToolCallId,
    /// Run containing this call, denormalized for correlation and storage.
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
}

impl From<&Task> for TaskWire {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id(),
            title: task.title().to_owned(),
            workspace_root: task.workspace_root().to_path_buf(),
            project_id: task.project_id(),
            created_at: task.created_at(),
        }
    }
}

impl TryFrom<TaskWire> for Task {
    type Error = RunDomainError;

    fn try_from(wire: TaskWire) -> Result<Self, Self::Error> {
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

impl From<&Run> for RunWire {
    fn from(run: &Run) -> Self {
        Self {
            id: run.id(),
            task_id: run.task_id(),
            state: run.state(),
            created_at: run.created_at(),
            updated_at: run.updated_at(),
            started_at: run.started_at(),
            finished_at: run.finished_at(),
        }
    }
}

impl TryFrom<RunWire> for Run {
    type Error = RunDomainError;

    fn try_from(wire: RunWire) -> Result<Self, Self::Error> {
        validate_lifecycle(
            "run",
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.state.is_terminal(),
            wire.state.requires_started_at(),
            wire.state.forbids_started_at(),
        )?;
        Ok(Self {
            id: wire.id,
            task_id: wire.task_id,
            state: wire.state,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
        })
    }
}

impl From<&Step> for StepWire {
    fn from(step: &Step) -> Self {
        Self {
            id: step.id(),
            run_id: step.run_id(),
            ordinal: step.ordinal(),
            title: step.title().to_owned(),
            state: step.state(),
            created_at: step.created_at(),
            updated_at: step.updated_at(),
            started_at: step.started_at(),
            finished_at: step.finished_at(),
        }
    }
}

impl TryFrom<StepWire> for Step {
    type Error = RunDomainError;

    fn try_from(wire: StepWire) -> Result<Self, Self::Error> {
        validate_lifecycle(
            "step",
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.state.is_terminal(),
            wire.state.requires_started_at(),
            wire.state.forbids_started_at(),
        )?;
        Ok(Self {
            id: wire.id,
            run_id: wire.run_id,
            ordinal: wire.ordinal,
            title: wire.title,
            state: wire.state,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
        })
    }
}

impl From<&ToolCall> for ToolCallWire {
    fn from(call: &ToolCall) -> Self {
        Self {
            id: call.id(),
            run_id: call.run_id(),
            step_id: call.step_id(),
            tool_id: call.tool_id().to_owned(),
            tool_version: call.tool_version().to_owned(),
            input: call.input().clone(),
            state: call.state(),
            created_at: call.created_at(),
            updated_at: call.updated_at(),
            started_at: call.started_at(),
            finished_at: call.finished_at(),
        }
    }
}

impl TryFrom<ToolCallWire> for ToolCall {
    type Error = RunDomainError;

    fn try_from(wire: ToolCallWire) -> Result<Self, Self::Error> {
        validate_lifecycle(
            "tool_call",
            wire.created_at,
            wire.updated_at,
            wire.started_at,
            wire.finished_at,
            wire.state.is_terminal(),
            wire.state.requires_started_at(),
            wire.state.forbids_started_at(),
        )?;
        Ok(Self {
            id: wire.id,
            run_id: wire.run_id,
            step_id: wire.step_id,
            tool_id: wire.tool_id,
            tool_version: wire.tool_version,
            input: wire.input,
            state: wire.state,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            started_at: wire.started_at,
            finished_at: wire.finished_at,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_lifecycle(
    record: &'static str,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
    terminal: bool,
    requires_started_at: bool,
    forbids_started_at: bool,
) -> Result<(), RunDomainError> {
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
    if terminal && finished_at.is_none() {
        return Err(invalid_lifecycle(
            record,
            "a terminal state requires finished_at",
        ));
    }
    if !terminal && finished_at.is_some() {
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
    if requires_started_at && started_at.is_none() {
        return Err(invalid_lifecycle(
            record,
            "the current state requires started_at",
        ));
    }
    if forbids_started_at && started_at.is_some() {
        return Err(invalid_lifecycle(
            record,
            "the current state cannot carry started_at",
        ));
    }
    Ok(())
}

fn validate_utc(
    record: &'static str,
    field: &'static str,
    timestamp: OffsetDateTime,
) -> Result<(), RunDomainError> {
    if timestamp.offset() != UtcOffset::UTC {
        return Err(invalid_timestamp(record, field, "must use the UTC offset"));
    }
    Ok(())
}

const fn invalid_timestamp(
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

const fn invalid_lifecycle(record: &'static str, reason: &'static str) -> RunDomainError {
    RunDomainError::InvalidLifecycle { record, reason }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use harkness_core::ProjectId;
    use serde_json::{Value, json};
    use time::{Duration, OffsetDateTime, UtcOffset};

    use super::{RunWire, StepWire, TaskWire, ToolCallWire};
    use crate::domain::{
        Run, RunDomainError, RunId, RunState, Step, StepId, Task, TaskId, ToolCall, ToolCallId,
        ToolCallState,
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

    fn task_wire() -> TaskWire {
        TaskWire {
            id: task_id(),
            title: "Inspect repository".to_owned(),
            workspace_root: PathBuf::from("/fixture"),
            project_id: Some(ProjectId::from_str("55555555-5555-4555-8555-555555555555").unwrap()),
            created_at: at(0),
        }
    }

    fn run_wire(state: RunState) -> RunWire {
        let started_at = state.requires_started_at().then(|| at(1));
        let finished_at = state.is_terminal().then(|| at(2));
        RunWire {
            id: run_id(),
            task_id: task_id(),
            state,
            created_at: at(0),
            updated_at: finished_at.unwrap_or_else(|| started_at.unwrap_or_else(|| at(0))),
            started_at,
            finished_at,
        }
    }

    fn step_wire(state: RunState) -> StepWire {
        let run = run_wire(state);
        StepWire {
            id: step_id(),
            run_id: run.id,
            ordinal: 0,
            title: "Inspect".to_owned(),
            state: run.state,
            created_at: run.created_at,
            updated_at: run.updated_at,
            started_at: run.started_at,
            finished_at: run.finished_at,
        }
    }

    fn call_wire(state: ToolCallState) -> ToolCallWire {
        let started_at = state.requires_started_at().then(|| at(1));
        let finished_at = state.is_terminal().then(|| at(2));
        ToolCallWire {
            id: call_id(),
            run_id: run_id(),
            step_id: step_id(),
            tool_id: "git.status".to_owned(),
            tool_version: "1.0.0".to_owned(),
            input: json!({"include_untracked": true}),
            state,
            created_at: at(0),
            updated_at: finished_at.unwrap_or_else(|| started_at.unwrap_or_else(|| at(0))),
            started_at,
            finished_at,
        }
    }

    fn with_unknown_field<T: serde::Serialize>(wire: T) -> Value {
        let mut value = serde_json::to_value(wire).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_owned(), json!(true));
        value
    }

    #[test]
    fn every_wire_type_rejects_unknown_fields() {
        assert!(serde_json::from_value::<TaskWire>(with_unknown_field(task_wire())).is_err());
        assert!(
            serde_json::from_value::<RunWire>(with_unknown_field(run_wire(RunState::Queued)))
                .is_err()
        );
        assert!(
            serde_json::from_value::<StepWire>(with_unknown_field(step_wire(RunState::Queued)))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ToolCallWire>(with_unknown_field(call_wire(
                ToolCallState::Pending
            )))
            .is_err()
        );
    }

    #[test]
    fn wire_run_rejects_a_terminal_state_without_finished_at() {
        let mut wire = run_wire(RunState::Succeeded);
        wire.finished_at = None;

        assert_eq!(
            Run::try_from(wire).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "run",
                reason: "a terminal state requires finished_at",
            }
        );
    }

    #[test]
    fn wire_tool_call_rejects_a_terminal_state_without_finished_at() {
        let mut wire = call_wire(ToolCallState::Denied);
        wire.finished_at = None;

        assert_eq!(
            ToolCall::try_from(wire).unwrap_err(),
            RunDomainError::InvalidLifecycle {
                record: "tool_call",
                reason: "a terminal state requires finished_at",
            }
        );
    }

    #[test]
    fn domain_records_round_trip_through_their_wire_types() {
        let task = Task::try_from(task_wire()).unwrap();
        let run = Run::try_from(run_wire(RunState::Succeeded)).unwrap();
        let step = Step::try_from(step_wire(RunState::WaitingForApproval)).unwrap();
        let call = ToolCall::try_from(call_wire(ToolCallState::Succeeded)).unwrap();

        assert_eq!(Task::try_from(TaskWire::from(&task)).unwrap(), task);
        assert_eq!(Run::try_from(RunWire::from(&run)).unwrap(), run);
        assert_eq!(Step::try_from(StepWire::from(&step)).unwrap(), step);
        assert_eq!(ToolCall::try_from(ToolCallWire::from(&call)).unwrap(), call);
    }

    #[test]
    fn wire_timestamps_are_rfc3339_utc_strings() {
        let value = serde_json::to_value(task_wire()).unwrap();
        assert_eq!(value["created_at"], "1970-01-01T00:00:00Z");

        let mut wire = run_wire(RunState::Queued);
        wire.created_at = at(0).to_offset(UtcOffset::from_hms(1, 0, 0).unwrap());
        assert_eq!(
            Run::try_from(wire).unwrap_err(),
            RunDomainError::InvalidTimestamp {
                record: "run",
                field: "created_at",
                reason: "must use the UTC offset",
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
}
