use std::path::PathBuf;

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    domain::{ApprovalOutcome, Task, TaskId, ToolCallId},
    tool::{ArtifactRef, ToolId, ToolVersion},
};

use super::ScenarioId;

/// One user-facing step in an agent-authored plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedStep {
    /// Short description of the intended work.
    pub title: String,
}

impl PlannedStep {
    /// Creates a planned step.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

/// Stable, redacted task fields an agent may observe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    /// Task identity.
    pub id: TaskId,
    /// User-authored task title.
    pub title: String,
}

impl From<&Task> for TaskRef {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id(),
            title: task.title().to_owned(),
        }
    }
}

/// Workspace identity exposed to an agent at run start.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    /// Catalog project identity, when this workspace belongs to one.
    pub project_id: Option<ProjectId>,
    /// Root the coordinator has already associated with the run.
    pub root: PathBuf,
}

impl From<&Task> for WorkspaceRef {
    fn from(task: &Task) -> Self {
        Self {
            project_id: task.project_id(),
            root: task.workspace_root().to_owned(),
        }
    }
}

/// Bounded, schema-validated result view delivered after a tool succeeds.
///
/// Large content remains in artifacts. `output` is the inline result already
/// accepted by the tool's output schema and the run store's inline bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultView {
    /// Inline structured output.
    pub output: Value,
    /// References to any large result content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
}

impl ToolResultView {
    /// Creates an inline result with no artifact references.
    #[must_use]
    pub const fn inline(output: Value) -> Self {
        Self {
            output,
            artifacts: Vec::new(),
        }
    }

    /// Creates a result carrying explicit artifact references.
    #[must_use]
    pub const fn with_artifacts(output: Value, artifacts: Vec<ArtifactRef>) -> Self {
        Self { output, artifacts }
    }
}

/// Redacted stable failure view delivered after a tool does not complete.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolErrorView {
    /// Stable machine-readable tool error kind.
    pub kind: String,
    /// Redacted user-facing detail.
    pub message: String,
}

impl ToolErrorView {
    /// Creates a failure view from already-redacted fields.
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// Human answer to an approval request, projected for the agent seam.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcomeView {
    /// The requested call may proceed.
    Approved,
    /// The requested call must not proceed.
    Denied,
}

impl From<ApprovalOutcome> for ApprovalOutcomeView {
    fn from(outcome: ApprovalOutcome) -> Self {
        match outcome {
            ApprovalOutcome::Approved => Self::Approved,
            ApprovalOutcome::Denied => Self::Denied,
        }
    }
}

/// Stable discriminant for one [`Observation`] shape.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// A coordinator began a run.
    RunStarted,
    /// A requested tool completed successfully.
    ToolResult,
    /// A requested tool failed or was refused by its execution path.
    ToolFailed,
    /// Policy denied a requested tool before approval or execution.
    PolicyDenied,
    /// A human approval request was resolved.
    ApprovalOutcome,
    /// The user cancelled the run.
    Cancelled,
}

impl ObservationKind {
    /// Stable snake-case spelling used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run_started",
            Self::ToolResult => "tool_result",
            Self::ToolFailed => "tool_failed",
            Self::PolicyDenied => "policy_denied",
            Self::ApprovalOutcome => "approval_outcome",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Plain, redacted data the coordinator may deliver to an [`Agent`](super::Agent).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    /// The first observation of a session.
    RunStarted {
        /// Task being attempted.
        task: TaskRef,
        /// Workspace bound to the run.
        workspace: WorkspaceRef,
    },
    /// One requested call succeeded.
    ToolResult {
        /// Recorded call identity.
        call: ToolCallId,
        /// Redacted, bounded result.
        result: ToolResultView,
    },
    /// One requested call failed.
    ToolFailed {
        /// Recorded call identity.
        call: ToolCallId,
        /// Redacted structured failure.
        error: ToolErrorView,
    },
    /// Policy refused one call outright.
    PolicyDenied {
        /// Recorded call identity.
        call: ToolCallId,
        /// Redacted policy explanation.
        reason: String,
    },
    /// One approval prompt was decided.
    ApprovalOutcome {
        /// Recorded call identity.
        call: ToolCallId,
        /// Direction of the decision.
        outcome: ApprovalOutcomeView,
    },
    /// The caller cancelled the run.
    Cancelled,
}

impl Observation {
    /// Returns the stable shape discriminant without inspecting its payload.
    #[must_use]
    pub const fn kind(&self) -> ObservationKind {
        match self {
            Self::RunStarted { .. } => ObservationKind::RunStarted,
            Self::ToolResult { .. } => ObservationKind::ToolResult,
            Self::ToolFailed { .. } => ObservationKind::ToolFailed,
            Self::PolicyDenied { .. } => ObservationKind::PolicyDenied,
            Self::ApprovalOutcome { .. } => ObservationKind::ApprovalOutcome,
            Self::Cancelled => ObservationKind::Cancelled,
        }
    }
}

/// Typed reason an agent asks the coordinator to fail a run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentFailure {
    /// The agent implementation itself could not produce another decision.
    ///
    /// This is the provider-neutral failure available to future model-backed
    /// agents; scenario-specific failures remain separately typed below.
    AgentFailed {
        /// Redacted explanation safe to persist and display.
        reason: String,
    },
    /// A deterministic scenario observed a different shape than it expected.
    ScenarioDivergence {
        /// Expected observation kind.
        expected: ObservationKind,
        /// Actual observation kind.
        actual: ObservationKind,
    },
    /// A script was called after its terminal action had already been emitted.
    ScenarioExhausted {
        /// Exhausted scenario.
        scenario: ScenarioId,
    },
    /// A human refused work required by the scenario.
    ApprovalDenied {
        /// Stable explanation recorded for the run.
        reason: String,
    },
    /// The agent acknowledges that the caller cancelled the run.
    Cancelled,
    /// The owning process stopped before the scripted operation completed.
    Interrupted {
        /// Stable explanation recorded for inspection after restart.
        reason: String,
    },
}

impl AgentFailure {
    /// Stable machine-readable failure kind.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::AgentFailed { .. } => "agent_failed",
            Self::ScenarioDivergence { .. } => "scenario_divergence",
            Self::ScenarioExhausted { .. } => "scenario_exhausted",
            Self::ApprovalDenied { .. } => "approval_denied",
            Self::Cancelled => "cancelled",
            Self::Interrupted { .. } => "interrupted",
        }
    }
}

/// One decision returned by an agent to its coordinator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentAction {
    /// Publish a user-facing plan. The coordinator records it; it does not
    /// authorize any work in the plan.
    Plan {
        /// Ordered intended steps.
        steps: Vec<PlannedStep>,
    },
    /// Request one typed tool invocation. Validation, policy, approval,
    /// persistence, execution, and cancellation remain coordinator work.
    CallTool {
        /// Stable tool identifier.
        tool_id: ToolId,
        /// Exact immutable tool version.
        tool_version: ToolVersion,
        /// Unvalidated input. Keeping this raw is what lets the real registry
        /// reject the mock's `invalid_tool_input` scenario honestly.
        input: Value,
    },
    /// Ask the coordinator to complete the run successfully.
    CompleteRun {
        /// User-facing completion summary.
        summary: String,
    },
    /// Ask the coordinator to terminate the run with typed failure detail.
    FailRun {
        /// Why the agent cannot proceed.
        reason: AgentFailure,
    },
}

impl AgentAction {
    /// Stable snake-case discriminant used by scenario tests and front ends.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::CallTool { .. } => "call_tool",
            Self::CompleteRun { .. } => "complete_run",
            Self::FailRun { .. } => "fail_run",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Observation, ObservationKind, TaskRef, WorkspaceRef};
    use crate::domain::TaskId;

    #[test]
    fn observation_wire_forms_are_internally_tagged_and_strict() {
        let observation = Observation::RunStarted {
            task: TaskRef {
                id: TaskId::new(),
                title: "inspect".to_owned(),
            },
            workspace: WorkspaceRef {
                project_id: None,
                root: PathBuf::from("/workspace"),
            },
        };
        let mut value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["kind"], "run_started");
        assert_eq!(observation.kind(), ObservationKind::RunStarted);
        value["shortcut"] = serde_json::json!("registry");
        assert!(
            serde_json::from_value::<Observation>(value)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }
}
