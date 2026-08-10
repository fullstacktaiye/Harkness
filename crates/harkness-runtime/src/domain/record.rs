use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use super::{
    InvalidTransition, RUN_TRANSITIONS, RunId, RunState, StepId, TOOL_CALL_TRANSITIONS, TaskId,
    ToolCallId, ToolCallState,
};

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
        Self {
            id: TaskId::new(),
            title: title.into(),
            workspace_root: workspace_root.into(),
            project_id,
            created_at: utc(created_at),
        }
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
    pub(super) state: RunState,
    pub(super) created_at: OffsetDateTime,
    pub(super) updated_at: OffsetDateTime,
    pub(super) started_at: Option<OffsetDateTime>,
    pub(super) finished_at: Option<OffsetDateTime>,
}

impl Run {
    /// Creates a queued run with a random ID.
    #[must_use]
    pub fn new(task_id: TaskId, created_at: OffsetDateTime) -> Self {
        let created_at = utc(created_at);
        Self {
            id: RunId::new(),
            task_id,
            state: RunState::Queued,
            created_at,
            updated_at: created_at,
            started_at: None,
            finished_at: None,
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

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> RunState {
        self.state
    }

    /// UTC creation time.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    /// UTC time at which the current state was entered.
    #[must_use]
    pub const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    /// UTC time at which execution first entered `running`.
    #[must_use]
    pub const fn started_at(&self) -> Option<OffsetDateTime> {
        self.started_at
    }

    /// UTC time at which a terminal state was entered.
    #[must_use]
    pub const fn finished_at(&self) -> Option<OffsetDateTime> {
        self.finished_at
    }

    /// Enters a state only when its edge appears in [`RUN_TRANSITIONS`].
    ///
    /// The transition time is normalized to UTC. A timestamp older than the
    /// current state timestamp is clamped to it so lifecycle time never moves
    /// backwards when wall clocks are adjusted.
    pub fn transition(
        &mut self,
        to: RunState,
        at: OffsetDateTime,
    ) -> Result<(), InvalidTransition<RunState>> {
        apply_transition(
            &mut self.state,
            &mut self.updated_at,
            &mut self.started_at,
            &mut self.finished_at,
            to,
            at,
            RunState::Running,
            RUN_TRANSITIONS,
            RunState::is_terminal,
        )
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
    pub(super) state: RunState,
    pub(super) created_at: OffsetDateTime,
    pub(super) updated_at: OffsetDateTime,
    pub(super) started_at: Option<OffsetDateTime>,
    pub(super) finished_at: Option<OffsetDateTime>,
}

impl Step {
    /// Creates a queued step with a random ID.
    #[must_use]
    pub fn new(
        run_id: RunId,
        ordinal: u32,
        title: impl Into<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        let created_at = utc(created_at);
        Self {
            id: StepId::new(),
            run_id,
            ordinal,
            title: title.into(),
            state: RunState::Queued,
            created_at,
            updated_at: created_at,
            started_at: None,
            finished_at: None,
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

    /// Zero-based position of this step within its run.
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// User-facing step title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> RunState {
        self.state
    }

    /// UTC creation time.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    /// UTC time at which the current state was entered.
    #[must_use]
    pub const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    /// UTC time at which execution first entered `running`.
    #[must_use]
    pub const fn started_at(&self) -> Option<OffsetDateTime> {
        self.started_at
    }

    /// UTC time at which a terminal state was entered.
    #[must_use]
    pub const fn finished_at(&self) -> Option<OffsetDateTime> {
        self.finished_at
    }

    /// Enters a state only when its edge appears in [`RUN_TRANSITIONS`].
    pub fn transition(
        &mut self,
        to: RunState,
        at: OffsetDateTime,
    ) -> Result<(), InvalidTransition<RunState>> {
        apply_transition(
            &mut self.state,
            &mut self.updated_at,
            &mut self.started_at,
            &mut self.finished_at,
            to,
            at,
            RunState::Running,
            RUN_TRANSITIONS,
            RunState::is_terminal,
        )
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
    pub(super) state: ToolCallState,
    pub(super) created_at: OffsetDateTime,
    pub(super) updated_at: OffsetDateTime,
    pub(super) started_at: Option<OffsetDateTime>,
    pub(super) finished_at: Option<OffsetDateTime>,
}

impl ToolCall {
    /// Creates a pending tool request with a random ID.
    #[must_use]
    pub fn new(
        run_id: RunId,
        step_id: StepId,
        tool_id: impl Into<String>,
        tool_version: impl Into<String>,
        input: Value,
        created_at: OffsetDateTime,
    ) -> Self {
        let created_at = utc(created_at);
        Self {
            id: ToolCallId::new(),
            run_id,
            step_id,
            tool_id: tool_id.into(),
            tool_version: tool_version.into(),
            input,
            state: ToolCallState::Pending,
            created_at,
            updated_at: created_at,
            started_at: None,
            finished_at: None,
        }
    }

    /// Stable identifier of this tool call.
    #[must_use]
    pub const fn id(&self) -> ToolCallId {
        self.id
    }

    /// Run containing this call, denormalized for correlation and storage.
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

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ToolCallState {
        self.state
    }

    /// UTC creation time.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }

    /// UTC time at which the current state was entered.
    #[must_use]
    pub const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    /// UTC time at which execution first entered `running`.
    #[must_use]
    pub const fn started_at(&self) -> Option<OffsetDateTime> {
        self.started_at
    }

    /// UTC time at which a terminal state was entered.
    #[must_use]
    pub const fn finished_at(&self) -> Option<OffsetDateTime> {
        self.finished_at
    }

    /// Enters a state only when its edge appears in [`TOOL_CALL_TRANSITIONS`].
    pub fn transition(
        &mut self,
        to: ToolCallState,
        at: OffsetDateTime,
    ) -> Result<(), InvalidTransition<ToolCallState>> {
        apply_transition(
            &mut self.state,
            &mut self.updated_at,
            &mut self.started_at,
            &mut self.finished_at,
            to,
            at,
            ToolCallState::Running,
            TOOL_CALL_TRANSITIONS,
            ToolCallState::is_terminal,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_transition<S>(
    state: &mut S,
    updated_at: &mut OffsetDateTime,
    started_at: &mut Option<OffsetDateTime>,
    finished_at: &mut Option<OffsetDateTime>,
    to: S,
    at: OffsetDateTime,
    running: S,
    transitions: &[(S, S)],
    is_terminal: impl FnOnce(S) -> bool,
) -> Result<(), InvalidTransition<S>>
where
    S: Copy + Eq,
{
    let from = *state;
    if !transitions.contains(&(from, to)) {
        return Err(InvalidTransition { from, to });
    }

    let at = utc(at).max(*updated_at);
    *state = to;
    *updated_at = at;
    if to == running && started_at.is_none() {
        *started_at = Some(at);
    }
    if is_terminal(to) {
        *finished_at = Some(at);
    }
    Ok(())
}

fn utc(timestamp: OffsetDateTime) -> OffsetDateTime {
    timestamp.to_offset(UtcOffset::UTC)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::{Duration, OffsetDateTime};

    use super::{Run, Step, Task, ToolCall};
    use crate::domain::{
        InvalidTransition, RUN_TRANSITIONS, RunState, TOOL_CALL_TRANSITIONS, ToolCallState,
    };

    fn at(second: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(second)
    }

    fn run_in(state: RunState) -> Run {
        let task = Task::new("fixture", "/fixture", None, at(0));
        let mut run = Run::new(task.id(), at(0));
        run.state = state;
        if state.requires_started_at() {
            run.started_at = Some(at(1));
        }
        if state.is_terminal() {
            run.finished_at = Some(at(2));
            run.updated_at = at(2);
        }
        run
    }

    fn step_in(state: RunState) -> Step {
        let run = run_in(RunState::Queued);
        let mut step = Step::new(run.id(), 0, "fixture", at(0));
        step.state = state;
        if state.requires_started_at() {
            step.started_at = Some(at(1));
        }
        if state.is_terminal() {
            step.finished_at = Some(at(2));
            step.updated_at = at(2);
        }
        step
    }

    fn call_in(state: ToolCallState) -> ToolCall {
        let run = run_in(RunState::Queued);
        let step = Step::new(run.id(), 0, "fixture", at(0));
        let mut call = ToolCall::new(
            run.id(),
            step.id(),
            "fixture.tool",
            "1.0.0",
            json!({}),
            at(0),
        );
        call.state = state;
        if state.requires_started_at() {
            call.started_at = Some(at(1));
        }
        if state.is_terminal() {
            call.finished_at = Some(at(2));
            call.updated_at = at(2);
        }
        call
    }

    #[test]
    fn every_declared_run_transition_succeeds_and_every_other_pair_is_invalid() {
        for &from in RunState::ALL {
            for &to in RunState::ALL {
                let expected = RUN_TRANSITIONS.contains(&(from, to));
                let mut run = run_in(from);
                let mut step = step_in(from);
                let run_result = run.transition(to, at(3));
                let step_result = step.transition(to, at(3));

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
                } else {
                    let error = InvalidTransition { from, to };
                    assert_eq!(run_result.unwrap_err(), error);
                    assert_eq!(step_result.unwrap_err(), error);
                    assert_eq!(run.state(), from);
                    assert_eq!(step.state(), from);
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
                let result = call.transition(to, at(3));

                assert_eq!(
                    result.is_ok(),
                    expected,
                    "unexpected tool-call edge {from} -> {to}"
                );
                if expected {
                    assert_eq!(call.state(), to);
                } else {
                    assert_eq!(result.unwrap_err(), InvalidTransition { from, to });
                    assert_eq!(call.state(), from);
                }
            }
        }
    }

    #[test]
    fn terminal_run_states_reject_all_outgoing_transitions() {
        for &from in RunState::ALL.iter().filter(|state| state.is_terminal()) {
            for &to in RunState::ALL {
                assert_eq!(
                    run_in(from).transition(to, at(3)).unwrap_err(),
                    InvalidTransition { from, to }
                );
            }
        }
    }

    #[test]
    fn a_tool_call_cannot_enter_running_from_any_state_but_pending_or_awaiting_approval() {
        for &from in ToolCallState::ALL {
            let can_enter = matches!(
                from,
                ToolCallState::Pending | ToolCallState::AwaitingApproval
            );
            assert_eq!(
                call_in(from)
                    .transition(ToolCallState::Running, at(3))
                    .is_ok(),
                can_enter,
                "unexpected edge {from} -> running"
            );
        }

        let run = run_in(RunState::Queued);
        let step = Step::new(run.id(), 0, "fixture", at(0));
        let fresh = ToolCall::new(
            run.id(),
            step.id(),
            "fixture.tool",
            "1.0.0",
            json!({}),
            at(0),
        );
        assert_eq!(fresh.state(), ToolCallState::Pending);
    }

    #[test]
    fn transitions_record_started_updated_and_finished_times_in_utc() {
        let task = Task::new("fixture", "/fixture", None, at(0));
        let mut run = Run::new(task.id(), at(0));
        let non_utc = at(1).to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());

        run.transition(RunState::Running, non_utc).unwrap();
        assert_eq!(run.started_at(), Some(at(1)));
        assert_eq!(run.updated_at(), at(1));
        assert_eq!(run.updated_at().offset(), time::UtcOffset::UTC);
        assert_eq!(run.finished_at(), None);

        run.transition(RunState::Succeeded, at(2)).unwrap();
        assert_eq!(run.finished_at(), Some(at(2)));
        assert_eq!(run.updated_at(), at(2));
    }

    #[test]
    fn transition_timestamps_never_move_backwards() {
        let task = Task::new("fixture", "/fixture", None, at(10));
        let mut run = Run::new(task.id(), at(10));

        run.transition(RunState::Running, at(5)).unwrap();
        assert_eq!(run.started_at(), Some(at(10)));
        assert_eq!(run.updated_at(), at(10));
    }
}
