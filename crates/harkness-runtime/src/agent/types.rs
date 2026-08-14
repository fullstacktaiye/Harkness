use std::path::PathBuf;

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize, de::Error as _};
use serde_json::Value;

use crate::{
    domain::{ApprovalOutcome, Task, TaskId, ToolCallId},
    store::{Redactor, redact_payload},
    tool::{ArtifactRef, ToolId, ToolVersion},
};

use super::ScenarioId;

/// Schema version shared by durable agent action and observation records.
pub const AGENT_TURN_RECORD_SCHEMA_VERSION: u32 = 1;

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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    /// Task identity.
    id: TaskId,
    /// User-authored task title.
    title: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRefWire {
    id: TaskId,
    title: String,
}

impl TaskRef {
    /// Projects a task through the configured redactor for the agent seam.
    #[must_use]
    pub fn from_task(task: &Task, redactor: &dyn Redactor) -> Self {
        Self {
            id: task.id(),
            title: redactor.redact_text(task.title()).into_owned(),
        }
    }

    /// Creates a redacted task projection from its stable identity and title.
    #[must_use]
    pub fn new(id: TaskId, title: impl Into<String>, redactor: &dyn Redactor) -> Self {
        let title = title.into();
        Self {
            id,
            title: redactor.redact_text(&title).into_owned(),
        }
    }

    /// Returns the stable task identity.
    #[must_use]
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the redacted task title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
}

/// Workspace identity exposed to an agent at run start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    /// Catalog project identity, when this workspace belongs to one.
    project_id: Option<ProjectId>,
    /// Root the coordinator has already associated with the run.
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRefWire {
    project_id: Option<ProjectId>,
    root: PathBuf,
}

impl WorkspaceRef {
    /// Projects a task's workspace through the configured agent redactor.
    #[must_use]
    pub fn from_task(task: &Task, redactor: &dyn Redactor) -> Self {
        Self::new(task.project_id(), task.workspace_root(), redactor)
    }

    /// Creates a redacted workspace projection.
    ///
    /// A non-UTF-8 path is retained only so the JSON-backed durable boundary can
    /// return its documented serialization error; it can never be persisted.
    #[must_use]
    pub fn new(
        project_id: Option<ProjectId>,
        root: impl Into<PathBuf>,
        redactor: &dyn Redactor,
    ) -> Self {
        let root = root.into();
        let root = root.to_str().map_or(root.clone(), |root| {
            PathBuf::from(redactor.redact_text(root).into_owned())
        });
        Self { project_id, root }
    }

    /// Returns the catalog project identity, when present.
    #[must_use]
    pub const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    /// Returns the redacted workspace root.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// Bounded, schema-validated result view delivered after a tool succeeds.
///
/// Large content remains in artifacts. `output` is the inline result already
/// accepted by the tool's output schema and the run store's inline bound. The
/// fields are private so a coordinator cannot accidentally wrap executor output
/// without first applying its store's redactor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultView {
    /// Inline structured output.
    pub(super) output: Value,
    /// References to any large result content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolResultViewWire {
    output: Value,
    #[serde(default)]
    artifacts: Vec<ArtifactRef>,
}

impl ToolResultView {
    /// Creates a redacted inline result with no artifact references.
    #[must_use]
    pub fn inline(output: Value, redactor: &dyn Redactor) -> Self {
        Self {
            output: redact_payload(redactor, &output),
            artifacts: Vec::new(),
        }
    }

    /// Creates a redacted result carrying stored artifact references.
    ///
    /// Artifact references come from the artifact store after its metadata and
    /// content redaction has run; only the inline JSON needs projecting here.
    #[must_use]
    pub fn with_artifacts(
        output: Value,
        artifacts: Vec<ArtifactRef>,
        redactor: &dyn Redactor,
    ) -> Self {
        Self {
            output: redact_payload(redactor, &output),
            artifacts,
        }
    }

    /// Borrows the redacted inline output.
    #[must_use]
    pub const fn output(&self) -> &Value {
        &self.output
    }

    /// Borrows the stored artifact references.
    #[must_use]
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }
}

/// Redacted stable failure view delivered after a tool does not complete.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolErrorView {
    /// Stable machine-readable tool error kind.
    pub(super) kind: String,
    /// Redacted user-facing detail.
    pub(super) message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolErrorViewWire {
    kind: String,
    message: String,
}

impl ToolErrorView {
    /// Creates a failure view, redacting its caller-controlled detail.
    ///
    /// `kind` is a Harkness-generated stable discriminant and remains intact so
    /// matching cannot be changed by a content-redaction rule.
    #[must_use]
    pub fn new(
        kind: impl Into<String>,
        message: impl Into<String>,
        redactor: &dyn Redactor,
    ) -> Self {
        let message = message.into();
        Self {
            kind: kind.into(),
            message: redactor.redact_text(&message).into_owned(),
        }
    }

    /// Returns the stable failure kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the redacted failure detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
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

/// Caller-authored text that has crossed the agent observation redaction seam.
///
/// The inner value is private and this type has no public deserialization path,
/// so a coordinator must provide its configured redactor before the text can be
/// embedded in an [`Observation`]. Persisted records use a private wire type to
/// recover content that was redacted before it was written.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RedactedText(String);

impl RedactedText {
    /// Redacts caller-authored observation text.
    #[must_use]
    pub fn new(value: impl Into<String>, redactor: &dyn Redactor) -> Self {
        let value = value.into();
        Self(redactor.redact_text(&value).into_owned())
    }

    /// Returns the redacted text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
        reason: RedactedText,
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

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ObservationWire {
    RunStarted {
        task: TaskRefWire,
        workspace: WorkspaceRefWire,
    },
    ToolResult {
        call: ToolCallId,
        result: ToolResultViewWire,
    },
    ToolFailed {
        call: ToolCallId,
        error: ToolErrorViewWire,
    },
    PolicyDenied {
        call: ToolCallId,
        reason: String,
    },
    ApprovalOutcome {
        call: ToolCallId,
        outcome: ApprovalOutcomeView,
    },
    Cancelled,
}

impl From<ObservationWire> for Observation {
    fn from(wire: ObservationWire) -> Self {
        match wire {
            ObservationWire::RunStarted { task, workspace } => Self::RunStarted {
                task: TaskRef {
                    id: task.id,
                    title: task.title,
                },
                workspace: WorkspaceRef {
                    project_id: workspace.project_id,
                    root: workspace.root,
                },
            },
            ObservationWire::ToolResult { call, result } => Self::ToolResult {
                call,
                result: ToolResultView {
                    output: result.output,
                    artifacts: result.artifacts,
                },
            },
            ObservationWire::ToolFailed { call, error } => Self::ToolFailed {
                call,
                error: ToolErrorView {
                    kind: error.kind,
                    message: error.message,
                },
            },
            ObservationWire::PolicyDenied { call, reason } => Self::PolicyDenied {
                call,
                reason: RedactedText(reason),
            },
            ObservationWire::ApprovalOutcome { call, outcome } => {
                Self::ApprovalOutcome { call, outcome }
            }
            ObservationWire::Cancelled => Self::Cancelled,
        }
    }
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

/// Versioned durable form of one [`AgentAction`].
///
/// Scenario fixtures embed actions inside their own versioned envelope. Run
/// events use this record instead, so a future action shape probes its version
/// before the strict current body is decoded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentActionRecord {
    schema_version: u32,
    action: AgentAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentActionRecordWire {
    schema_version: u32,
    action: AgentAction,
}

impl AgentActionRecord {
    /// Redacts and wraps an action in the current durable schema.
    #[must_use]
    pub fn new(action: AgentAction, redactor: &dyn Redactor) -> Self {
        Self {
            schema_version: AGENT_TURN_RECORD_SCHEMA_VERSION,
            action: redact_action(action, redactor),
        }
    }

    /// Version of this durable record.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Borrows the recorded action.
    #[must_use]
    pub const fn action(&self) -> &AgentAction {
        &self.action
    }

    /// Unwraps the recorded action after version validation.
    #[must_use]
    pub fn into_action(self) -> AgentAction {
        self.action
    }

    fn from_json_value(value: Value) -> Result<Self, serde_json::Error> {
        probe_turn_record_version(&value, "agent action").map_err(serde_json::Error::custom)?;
        let wire: AgentActionRecordWire = serde_json::from_value(value)?;
        Ok(Self {
            schema_version: wire.schema_version,
            action: wire.action,
        })
    }
}

/// Versioned durable form of one [`Observation`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    schema_version: u32,
    observation: Observation,
}

#[allow(dead_code, reason = "decoded by the coordinator landing in issue 97")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationRecordWire {
    schema_version: u32,
    observation: ObservationWire,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TurnRecordEventPayload {
    encoding_version: u32,
    record_bytes: Vec<u8>,
}

impl ObservationRecord {
    /// Wraps an observation in the current durable schema.
    #[must_use]
    pub const fn new(observation: Observation) -> Self {
        Self {
            schema_version: AGENT_TURN_RECORD_SCHEMA_VERSION,
            observation,
        }
    }

    /// Version of this durable record.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Borrows the recorded observation.
    #[must_use]
    pub const fn observation(&self) -> &Observation {
        &self.observation
    }

    /// Unwraps the recorded observation after version validation.
    #[must_use]
    pub fn into_observation(self) -> Observation {
        self.observation
    }

    /// Encodes an already-redacted observation for a generic run-event payload.
    ///
    /// The versioned record is stored as numeric bytes so the event store's
    /// mandatory text redactor cannot rewrite enum tags, UUID spellings, or
    /// other machine-control fields. Live tool content can enter this record
    /// only through the redaction-enforcing observation projections.
    ///
    /// # Errors
    ///
    /// Returns a serialization error when a path cannot be represented in the
    /// JSON-backed durable format, including a non-UTF-8 workspace path.
    pub fn to_event_payload(&self) -> Result<Value, serde_json::Error> {
        turn_record_event_payload(self)
    }

    #[allow(dead_code, reason = "called by the coordinator landing in issue 97")]
    pub(crate) fn from_event_payload(value: Value) -> Result<Self, serde_json::Error> {
        let bytes = turn_record_event_bytes(value)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        Self::from_json_value(value)
    }

    #[allow(dead_code, reason = "called by the coordinator landing in issue 97")]
    fn from_json_value(value: Value) -> Result<Self, serde_json::Error> {
        probe_turn_record_version(&value, "agent observation")
            .map_err(serde_json::Error::custom)?;
        let wire: ObservationRecordWire = serde_json::from_value(value)?;
        Ok(Self {
            schema_version: wire.schema_version,
            observation: wire.observation.into(),
        })
    }
}

impl From<Observation> for ObservationRecord {
    fn from(observation: Observation) -> Self {
        Self::new(observation)
    }
}

impl AgentActionRecord {
    /// Encodes this versioned action for a generic redacting run-event payload.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if this versioned action cannot be
    /// represented in the durable JSON format.
    pub fn to_event_payload(&self) -> Result<Value, serde_json::Error> {
        turn_record_event_payload(self)
    }

    #[allow(dead_code, reason = "called by the coordinator landing in issue 97")]
    pub(crate) fn from_event_payload(value: Value) -> Result<Self, serde_json::Error> {
        let bytes = turn_record_event_bytes(value)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        Self::from_json_value(value)
    }
}

fn redact_action(action: AgentAction, redactor: &dyn Redactor) -> AgentAction {
    match action {
        AgentAction::Plan { steps } => AgentAction::Plan {
            steps: steps
                .into_iter()
                .map(|step| PlannedStep::new(redactor.redact_text(&step.title).into_owned()))
                .collect(),
        },
        AgentAction::CallTool {
            tool_id,
            tool_version,
            input,
        } => AgentAction::CallTool {
            tool_id,
            tool_version,
            input: redact_payload(redactor, &input),
        },
        AgentAction::CompleteRun { summary } => AgentAction::CompleteRun {
            summary: redactor.redact_text(&summary).into_owned(),
        },
        AgentAction::FailRun { reason } => AgentAction::FailRun {
            reason: redact_agent_failure(reason, redactor),
        },
    }
}

fn redact_agent_failure(reason: AgentFailure, redactor: &dyn Redactor) -> AgentFailure {
    match reason {
        AgentFailure::AgentFailed { reason } => AgentFailure::AgentFailed {
            reason: redactor.redact_text(&reason).into_owned(),
        },
        AgentFailure::ApprovalDenied { reason } => AgentFailure::ApprovalDenied {
            reason: redactor.redact_text(&reason).into_owned(),
        },
        AgentFailure::Interrupted { reason } => AgentFailure::Interrupted {
            reason: redactor.redact_text(&reason).into_owned(),
        },
        control => control,
    }
}

fn turn_record_event_payload(record: &impl Serialize) -> Result<Value, serde_json::Error> {
    serde_json::to_value(TurnRecordEventPayload {
        encoding_version: 1,
        record_bytes: serde_json::to_vec(record)?,
    })
}

#[allow(dead_code, reason = "called by the coordinator landing in issue 97")]
fn turn_record_event_bytes(value: Value) -> Result<Vec<u8>, serde_json::Error> {
    let version = value
        .get("encoding_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            serde_json::Error::custom(
                "agent turn event payload is missing numeric encoding_version",
            )
        })?;
    if version > 1 {
        return Err(serde_json::Error::custom(format!(
            "agent turn event payload encoding {version} is newer than supported encoding 1; upgrade Harkness"
        )));
    }
    if version != 1 {
        return Err(serde_json::Error::custom(format!(
            "agent turn event payload encoding {version} is not supported"
        )));
    }
    let payload: TurnRecordEventPayload = serde_json::from_value(value)?;
    Ok(payload.record_bytes)
}

fn probe_turn_record_version(value: &Value, record: &str) -> Result<(), String> {
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{record} record is missing numeric schema_version"))?;
    if version > u64::from(AGENT_TURN_RECORD_SCHEMA_VERSION) {
        return Err(format!(
            "{record} record schema {version} is newer than supported schema {AGENT_TURN_RECORD_SCHEMA_VERSION}; upgrade Harkness"
        ));
    }
    if version != u64::from(AGENT_TURN_RECORD_SCHEMA_VERSION) {
        return Err(format!("{record} record schema {version} is not supported"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, io::Write, path::PathBuf};

    use super::{
        AGENT_TURN_RECORD_SCHEMA_VERSION, AgentAction, AgentActionRecord, Observation,
        ObservationKind, ObservationRecord, ObservationWire, TaskRef, ToolErrorView,
        ToolResultView, WorkspaceRef,
    };
    use crate::{
        domain::TaskId,
        store::{PassThrough, Redactor, redact_payload},
        tool::ArtifactRef,
    };

    #[derive(Debug)]
    struct MaskEveryString;

    impl Redactor for MaskEveryString {
        fn redact_text<'a>(&self, _text: &'a str) -> Cow<'a, str> {
            Cow::Borrowed("[redacted]")
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            sink
        }
    }

    #[test]
    fn observation_wire_forms_are_internally_tagged_and_strict() {
        let observation = Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "inspect", &PassThrough),
            workspace: WorkspaceRef::new(None, "/workspace", &PassThrough),
        };
        let mut value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["kind"], "run_started");
        assert_eq!(observation.kind(), ObservationKind::RunStarted);
        value["shortcut"] = serde_json::json!("registry");
        assert!(
            serde_json::from_value::<ObservationWire>(value)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn durable_turn_records_probe_versions_before_their_strict_bodies() {
        let action = AgentActionRecord::new(
            AgentAction::CompleteRun {
                summary: "done".to_owned(),
            },
            &PassThrough,
        );
        let observation = ObservationRecord::new(Observation::Cancelled);
        assert_eq!(
            serde_json::to_value(&action).unwrap()["schema_version"],
            AGENT_TURN_RECORD_SCHEMA_VERSION
        );
        assert_eq!(
            serde_json::to_value(&observation).unwrap()["schema_version"],
            AGENT_TURN_RECORD_SCHEMA_VERSION
        );

        for mut future in [
            serde_json::to_value(action).unwrap(),
            serde_json::to_value(observation).unwrap(),
        ] {
            future["schema_version"] = serde_json::json!(99);
            future["future_shape"] = serde_json::json!({"unknown": true});
            let action_error = AgentActionRecord::from_json_value(future.clone()).unwrap_err();
            let observation_error = ObservationRecord::from_json_value(future).unwrap_err();
            assert!(action_error.to_string().contains("newer than supported"));
            assert!(
                observation_error
                    .to_string()
                    .contains("newer than supported")
            );
        }
    }

    #[test]
    fn frozen_turn_records_decode_through_the_private_persistence_path() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/wire-contract-v1.json")).unwrap();
        for action in fixture["actions"].as_array().unwrap() {
            AgentActionRecord::from_json_value(action.clone()).unwrap();
        }
        for observation in fixture["observations"].as_array().unwrap() {
            ObservationRecord::from_json_value(observation.clone()).unwrap();
        }

        let action = AgentActionRecord::from_json_value(fixture["actions"][0].clone()).unwrap();
        assert_eq!(
            action.to_event_payload().unwrap(),
            fixture["event_payloads"]["action"]
        );
        let observation =
            ObservationRecord::from_json_value(fixture["observations"][0].clone()).unwrap();
        assert_eq!(
            observation.to_event_payload().unwrap(),
            fixture["event_payloads"]["observation"]
        );
    }

    #[test]
    fn turn_event_payloads_survive_a_mutating_store_redactor() {
        let action = AgentActionRecord::new(
            AgentAction::CompleteRun {
                summary: "already redacted".to_owned(),
            },
            &PassThrough,
        );
        let observation = ObservationRecord::new(Observation::Cancelled);

        let action_payload = redact_payload(&MaskEveryString, &action.to_event_payload().unwrap());
        let observation_payload =
            redact_payload(&MaskEveryString, &observation.to_event_payload().unwrap());
        assert_eq!(
            AgentActionRecord::from_event_payload(action_payload).unwrap(),
            action
        );
        assert_eq!(
            ObservationRecord::from_event_payload(observation_payload).unwrap(),
            observation
        );

        let run_started = ObservationRecord::new(Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "secret task", &MaskEveryString),
            workspace: WorkspaceRef::new(None, "/work/secret", &MaskEveryString),
        });
        let persisted = ObservationRecord::from_event_payload(redact_payload(
            &MaskEveryString,
            &run_started.to_event_payload().unwrap(),
        ))
        .unwrap();
        let Observation::RunStarted { task, workspace } = persisted.observation() else {
            panic!("record changed the observation kind");
        };
        assert_eq!(task.title(), "[redacted]");
        assert_eq!(workspace.root(), std::path::Path::new("[redacted]"));
    }

    #[test]
    fn tool_observation_views_redact_content_at_their_construction_boundary() {
        let task = TaskRef::new(TaskId::new(), "secret task", &MaskEveryString);
        assert_eq!(task.title(), "[redacted]");
        let reason = super::RedactedText::new("secret denial", &MaskEveryString);
        assert_eq!(reason.as_str(), "[redacted]");

        let artifact = ArtifactRef {
            id: "stored-artifact".to_owned(),
            media_type: "text/plain".to_owned(),
            byte_len: 7,
        };
        let result = ToolResultView::with_artifacts(
            serde_json::json!({
                "credential": "secret",
                "nested": ["also secret"],
                "count": 1
            }),
            vec![artifact.clone()],
            &MaskEveryString,
        );
        assert_eq!(
            result.output(),
            &serde_json::json!({
                "credential": "[redacted]",
                "nested": ["[redacted]"],
                "count": 1
            })
        );
        assert_eq!(result.artifacts(), &[artifact]);

        let error = ToolErrorView::new("process_failed", "secret stderr", &MaskEveryString);
        assert_eq!(error.kind(), "process_failed");
        assert_eq!(error.message(), "[redacted]");

        let action = AgentActionRecord::new(
            AgentAction::CallTool {
                tool_id: crate::tool::ToolId::new("fs.read").unwrap(),
                tool_version: crate::tool::ToolVersion::new("1.0.0").unwrap(),
                input: serde_json::json!({"path": "secret"}),
            },
            &MaskEveryString,
        );
        let AgentAction::CallTool { input, .. } = action.action() else {
            panic!("record changed the action kind");
        };
        assert_eq!(input, &serde_json::json!({"path": "[redacted]"}));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_workspace_path_is_a_turn_record_serialization_error() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let observation = ObservationRecord::new(Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "inspect", &PassThrough),
            workspace: WorkspaceRef::new(
                None,
                PathBuf::from(OsString::from_vec(vec![b'/', 0xff])),
                &PassThrough,
            ),
        });
        assert!(observation.to_event_payload().is_err());
    }
}
