use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::tool::{ToolId, ToolVersion};

use super::{AgentAction, AgentFailure, ApprovalOutcomeView, Observation, ObservationKind};

/// Version of the deterministic scenario fixture format.
pub const SCENARIO_FIXTURE_VERSION: u32 = 1;

const SCENARIO_DEFINITION_DIGEST_DOMAIN: &[u8] = b"harkness.agent.scenario-definition.v1";

const FLAGSHIP_SOURCE_SHA256: &str =
    "4f03383f0bbf9e30e56d77f0a1b85286436cf6df407f00ade9f115b71f382026";

/// Largest scenario fixture accepted from disk or another untrusted source.
pub const MAX_SCENARIO_BYTES: usize = 64 * 1024;

/// Largest script the mock will retain and walk.
pub const MAX_SCENARIO_STEPS: usize = 64;

/// All built-in scenario names in stable registry order.
pub(crate) const BUILTIN_SCENARIO_NAMES: &[&str] = &[
    "read_only_success",
    "edit_test_diff_success",
    "approval_denied",
    "invalid_tool_input",
    "tool_process_failure",
    "tool_timeout",
    "user_cancellation",
    "restart_recovery",
    "forbidden_path",
    "disallowed_capability",
];

/// Stable lowercase snake-case identity of one mock scenario.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ScenarioId(String);

impl ScenarioId {
    /// Parses and validates a scenario identity.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::InvalidScenarioId`] for an empty, overlong, or
    /// non-snake-case spelling.
    pub fn new(value: impl Into<String>) -> Result<Self, ScenarioError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' => true,
                b'0'..=b'9' => index > 0,
                b'_' => index > 0 && index + 1 < value.len(),
                _ => false,
            })
            && !value.contains("__");
        if !valid {
            return Err(ScenarioError::InvalidScenarioId { value });
        }
        Ok(Self(value))
    }

    /// Borrows the stable spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScenarioId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<String> for ScenarioId {
    type Error = ScenarioError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ScenarioId> for String {
    fn from(value: ScenarioId) -> Self {
        value.0
    }
}

/// Failure to load, select, or resume a deterministic scenario.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScenarioError {
    /// Scenario id is outside the stable grammar.
    #[error("invalid scenario id {value:?}; expected at most 64 lowercase snake-case characters")]
    InvalidScenarioId {
        /// Rejected value.
        value: String,
    },
    /// Registry does not contain the requested scenario.
    #[error("unknown mock-agent scenario {name:?}")]
    UnknownScenario {
        /// Requested name.
        name: String,
    },
    /// Registry contains the scenario name, but not the checkpoint's version.
    #[error("mock-agent scenario {name:?} has no retained fixture version {version}")]
    UnknownScenarioVersion {
        /// Requested scenario name.
        name: String,
        /// Requested fixture version.
        version: u32,
    },
    /// Fixture exceeded the input bound.
    #[error("scenario fixture is {bytes} bytes; the limit is {MAX_SCENARIO_BYTES}")]
    FixtureTooLarge {
        /// Actual byte length.
        bytes: usize,
    },
    /// JSON could not be decoded as the current strict wire form.
    #[error("invalid scenario fixture: {0}")]
    InvalidFixture(#[from] serde_json::Error),
    /// Fixture came from a newer Harkness build.
    #[error(
        "scenario fixture version {found} is newer than supported version {supported}; upgrade Harkness"
    )]
    FixtureTooNew {
        /// Version in the fixture.
        found: u32,
        /// Newest version this build supports.
        supported: u32,
    },
    /// Fixture version is older than the supported format floor.
    #[error("scenario fixture version {found} is not supported")]
    UnsupportedFixtureVersion {
        /// Version in the fixture.
        found: u32,
    },
    /// Structurally valid JSON described an invalid script.
    #[error("invalid scenario {scenario}: {reason}")]
    InvalidDefinition {
        /// Scenario being checked.
        scenario: ScenarioId,
        /// Stable refusal reason.
        reason: &'static str,
    },
    /// A checkpoint cannot belong to the selected script.
    #[error("checkpoint cursor {cursor} is beyond scenario {scenario}'s {steps} steps")]
    InvalidCheckpoint {
        /// Scenario named by the checkpoint.
        scenario: ScenarioId,
        /// Stored cursor.
        cursor: u32,
        /// Script length.
        steps: usize,
    },
    /// A checkpoint names different scenario bytes than the supplied script.
    #[error("checkpoint for scenario {scenario} v{version} does not match the supplied definition")]
    CheckpointDefinitionMismatch {
        /// Scenario named by the checkpoint.
        scenario: ScenarioId,
        /// Fixture version named by the checkpoint.
        version: u32,
    },
    /// A built-in checkpoint's definition digest is not retained by this build.
    #[error("no retained mock-agent scenario v{version} has definition digest {digest}")]
    UnknownCheckpointDefinition {
        /// Fixture version named by the checkpoint.
        version: u32,
        /// Exact definition identity the checkpoint retained.
        digest: String,
    },
}

/// Structural expectation for one coordinator observation.
///
/// Optional fields make matching selective without admitting arbitrary code in
/// fixture data. Incidental call ids are deliberately absent, so replay does
/// not depend on random identifiers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationPattern {
    /// Match run start, optionally requiring the task title.
    RunStarted {
        /// Exact title to require.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_title: Option<String>,
    },
    /// Match a successful result, optionally requiring an artifact media type.
    ToolResult {
        /// At least one returned artifact must carry this media type.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_media_type: Option<String>,
        /// Object fields that must be present in the inline output. Nested
        /// objects are matched recursively; every other value is exact.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_contains: Option<Value>,
    },
    /// Match a tool failure, optionally requiring its stable kind.
    ToolFailed {
        /// Exact failure kind to require.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
    },
    /// Match a policy denial, optionally requiring text in its reason.
    PolicyDenied {
        /// Substring the redacted reason must contain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason_contains: Option<String>,
    },
    /// Match an approval outcome, optionally requiring one direction.
    ApprovalOutcome {
        /// Exact outcome to require.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<ApprovalOutcomeView>,
    },
    /// Match user cancellation.
    Cancelled,
}

impl ObservationPattern {
    /// Returns the observation shape this pattern expects.
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

    /// Matches the observation kind and any selected fields.
    #[must_use]
    pub fn matches(&self, observation: &Observation) -> bool {
        match (self, observation) {
            (Self::RunStarted { task_title }, Observation::RunStarted { task, .. }) => task_title
                .as_ref()
                .is_none_or(|title| task.title() == title),
            (
                Self::ToolResult {
                    artifact_media_type,
                    output_contains,
                },
                Observation::ToolResult { result, .. },
            ) => {
                artifact_media_type.as_ref().is_none_or(|media_type| {
                    result
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.media_type == *media_type)
                }) && output_contains
                    .as_ref()
                    .is_none_or(|expected| value_contains(&result.output, expected))
            }
            (Self::ToolFailed { error_kind }, Observation::ToolFailed { error, .. }) => {
                error_kind.as_ref().is_none_or(|kind| error.kind == *kind)
            }
            (Self::PolicyDenied { reason_contains }, Observation::PolicyDenied { reason, .. }) => {
                reason_contains
                    .as_ref()
                    .is_none_or(|required| reason.as_str().contains(required))
            }
            (
                Self::ApprovalOutcome { outcome },
                Observation::ApprovalOutcome {
                    outcome: observed, ..
                },
            ) => outcome.is_none_or(|expected| expected == *observed),
            (Self::Cancelled, Observation::Cancelled) => true,
            _ => false,
        }
    }
}

/// One expected observation and the action emitted when it matches.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioStep {
    expect: ObservationPattern,
    action: AgentAction,
}

impl ScenarioStep {
    /// Builds one script transition.
    #[must_use]
    pub const fn new(expect: ObservationPattern, action: AgentAction) -> Self {
        Self { expect, action }
    }

    /// Observation pattern this transition requires.
    #[must_use]
    pub const fn expectation(&self) -> &ObservationPattern {
        &self.expect
    }

    /// Action returned when the expectation matches.
    #[must_use]
    pub const fn action(&self) -> &AgentAction {
        &self.action
    }
}

/// Ordered deterministic mock-agent script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scenario {
    version: u32,
    id: ScenarioId,
    steps: Vec<ScenarioStep>,
}

#[derive(Deserialize)]
struct VersionProbe {
    v: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioWire {
    v: u32,
    id: ScenarioId,
    steps: Vec<ScenarioStep>,
}

#[derive(Serialize)]
struct ScenarioWireRef<'a> {
    v: u32,
    id: &'a ScenarioId,
    steps: &'a [ScenarioStep],
}

impl Scenario {
    /// Builds and validates a scenario from Rust data.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::InvalidDefinition`] for an empty or oversized
    /// script, a non-terminal final action, or a terminal action before the end.
    pub fn new(id: ScenarioId, steps: Vec<ScenarioStep>) -> Result<Self, ScenarioError> {
        Self::from_parts(SCENARIO_FIXTURE_VERSION, id, steps)
    }

    fn from_parts(
        version: u32,
        id: ScenarioId,
        steps: Vec<ScenarioStep>,
    ) -> Result<Self, ScenarioError> {
        let scenario = Self { version, id, steps };
        scenario.validate()?;
        Ok(scenario)
    }

    /// Parses a versioned strict JSON fixture after probing its version.
    ///
    /// # Errors
    ///
    /// Returns a distinct [`ScenarioError::FixtureTooNew`] before the strict
    /// body is decoded when `v` came from a newer build. Current-version unknown
    /// fields and malformed values return [`ScenarioError::InvalidFixture`].
    pub fn from_json(bytes: &str) -> Result<Self, ScenarioError> {
        if bytes.len() > MAX_SCENARIO_BYTES {
            return Err(ScenarioError::FixtureTooLarge { bytes: bytes.len() });
        }
        let version: VersionProbe = serde_json::from_str(bytes)?;
        if version.v > SCENARIO_FIXTURE_VERSION {
            return Err(ScenarioError::FixtureTooNew {
                found: version.v,
                supported: SCENARIO_FIXTURE_VERSION,
            });
        }
        if version.v != SCENARIO_FIXTURE_VERSION {
            return Err(ScenarioError::UnsupportedFixtureVersion { found: version.v });
        }
        let wire: ScenarioWire = serde_json::from_str(bytes)?;
        debug_assert_eq!(wire.v, SCENARIO_FIXTURE_VERSION);
        Self::from_parts(wire.v, wire.id, wire.steps)
    }

    /// Produces the canonical pretty JSON frozen by scenario fixtures.
    ///
    /// # Errors
    ///
    /// Returns a JSON encoding error, although built-in actions contain only
    /// representable `serde_json::Value` data.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let mut encoded = serde_json::to_string_pretty(&ScenarioWireRef {
            v: self.version,
            id: &self.id,
            steps: &self.steps,
        })?;
        encoded.push('\n');
        Ok(encoded)
    }

    /// Stable scenario identity.
    #[must_use]
    pub const fn id(&self) -> &ScenarioId {
        &self.id
    }

    /// Version of the frozen fixture definition this scenario represents.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Domain-separated SHA-256 of the exact versioned scenario definition.
    #[must_use]
    pub fn definition_digest(&self) -> String {
        let encoded = serde_json::to_vec(&ScenarioWireRef {
            v: self.version,
            id: &self.id,
            steps: &self.steps,
        })
        .expect("Scenario contains only infallibly serializable JSON values");
        let mut digest = Sha256::new();
        digest.update(SCENARIO_DEFINITION_DIGEST_DOMAIN);
        digest.update((encoded.len() as u64).to_be_bytes());
        digest.update(encoded);
        hex_sha256(digest.finalize().into())
    }

    /// Ordered script transitions.
    #[must_use]
    pub fn steps(&self) -> &[ScenarioStep] {
        &self.steps
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        if self.steps.is_empty() {
            return Err(self.invalid("a scenario must contain at least one step"));
        }
        if self.steps.len() > MAX_SCENARIO_STEPS {
            return Err(self.invalid("a scenario exceeds the 64-step bound"));
        }
        let terminal = |action: &AgentAction| {
            matches!(
                action,
                AgentAction::CompleteRun { .. } | AgentAction::FailRun { .. }
            )
        };
        if self.steps[..self.steps.len() - 1]
            .iter()
            .any(|step| terminal(&step.action))
        {
            return Err(self.invalid("only the final scenario step may be terminal"));
        }
        if !terminal(&self.steps.last().expect("nonempty checked above").action) {
            return Err(self.invalid("the final scenario step must complete or fail the run"));
        }
        Ok(())
    }

    fn invalid(&self, reason: &'static str) -> ScenarioError {
        ScenarioError::InvalidDefinition {
            scenario: self.id.clone(),
            reason,
        }
    }

    pub(crate) fn builtin(name: &str, version: u32) -> Result<Self, ScenarioError> {
        if version != SCENARIO_FIXTURE_VERSION {
            if BUILTIN_SCENARIO_NAMES.contains(&name) {
                return Err(ScenarioError::UnknownScenarioVersion {
                    name: name.to_owned(),
                    version,
                });
            }
            return Err(ScenarioError::UnknownScenario {
                name: name.to_owned(),
            });
        }
        match name {
            "read_only_success" => Ok(Self::read_only_success()),
            "edit_test_diff_success" => Ok(Self::edit_test_diff_success()),
            "approval_denied" => Ok(Self::approval_denied()),
            "invalid_tool_input" => Ok(Self::invalid_tool_input()),
            "tool_process_failure" => Ok(Self::tool_process_failure()),
            "tool_timeout" => Ok(Self::tool_timeout()),
            "user_cancellation" => Ok(Self::user_cancellation()),
            "restart_recovery" => Ok(Self::restart_recovery()),
            "forbidden_path" => Ok(Self::forbidden_path()),
            "disallowed_capability" => Ok(Self::disallowed_capability()),
            _ => Err(ScenarioError::UnknownScenario {
                name: name.to_owned(),
            }),
        }
    }

    pub(crate) fn builtin_by_definition(
        version: u32,
        definition_digest: &str,
    ) -> Result<Self, ScenarioError> {
        for name in BUILTIN_SCENARIO_NAMES {
            let scenario = Self::builtin(name, version)?;
            if scenario.definition_digest() == definition_digest {
                return Ok(scenario);
            }
        }
        Err(ScenarioError::UnknownCheckpointDefinition {
            version,
            digest: definition_digest.to_owned(),
        })
    }

    /// Rust-data definition of the read-only success scenario.
    #[must_use]
    pub fn read_only_success() -> Self {
        built_in(
            "read_only_success",
            vec![
                step(run_started(), call("workspace.inspect", json!({}))),
                step(
                    tool_result(),
                    call("fs.read", json!({"path": "src/lib.rs"})),
                ),
                step(
                    tool_result(),
                    call("git.diff", json!({"target": "unstaged"})),
                ),
                step(
                    tool_result(),
                    complete("Workspace inspection completed without changes."),
                ),
            ],
        )
    }

    /// Rust-data definition of the flagship edit/test/diff scenario.
    #[must_use]
    pub fn edit_test_diff_success() -> Self {
        built_in(
            "edit_test_diff_success",
            vec![
                step(run_started(), call("workspace.inspect", json!({}))),
                step(
                    tool_result(),
                    call("fs.read", json!({"path": "src/lib.rs"})),
                ),
                step(
                    tool_result(),
                    call(
                        "fs.apply_patch",
                        json!({
                            "patch": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub const VALUE: &str = \"old\";\n+pub const VALUE: &str = \"new\";\n",
                            "bases": [{
                                "path": "src/lib.rs",
                                "base_sha256": FLAGSHIP_SOURCE_SHA256
                            }]
                        }),
                    ),
                ),
                step(
                    ObservationPattern::ToolResult {
                        artifact_media_type: Some("text/x-diff".to_owned()),
                        output_contains: None,
                    },
                    call(
                        "test.run",
                        json!({"command": ["cargo", "test", "-p", "harkness-runtime"]}),
                    ),
                ),
                step(
                    tool_result(),
                    call("git.diff", json!({"target": "unstaged"})),
                ),
                step(
                    tool_result(),
                    complete("Edited the workspace, passed tests, and captured the final diff."),
                ),
            ],
        )
    }

    /// Rust-data definition of the denied-approval scenario.
    #[must_use]
    pub fn approval_denied() -> Self {
        built_in(
            "approval_denied",
            vec![
                step(
                    run_started(),
                    call(
                        "fs.apply_patch",
                        json!({
                            "patch": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub const VALUE: &str = \"old\";\n+pub const VALUE: &str = \"new\";\n",
                            "bases": [{
                                "path": "src/lib.rs",
                                "base_sha256": FLAGSHIP_SOURCE_SHA256
                            }]
                        }),
                    ),
                ),
                step(
                    ObservationPattern::ApprovalOutcome {
                        outcome: Some(ApprovalOutcomeView::Denied),
                    },
                    AgentAction::FailRun {
                        reason: AgentFailure::ApprovalDenied {
                            reason: "The requested workspace edit was denied.".to_owned(),
                        },
                    },
                ),
            ],
        )
    }

    /// Rust-data definition of the schema-invalid tool input scenario.
    #[must_use]
    pub fn invalid_tool_input() -> Self {
        built_in(
            "invalid_tool_input",
            vec![
                step(run_started(), call("fs.read", json!({"path": 42}))),
                step(
                    tool_failed("invalid_input"),
                    complete(
                        "The runtime rejected the deliberately invalid input before execution.",
                    ),
                ),
            ],
        )
    }

    /// Rust-data definition of a nonzero child-process result.
    #[must_use]
    pub fn tool_process_failure() -> Self {
        built_in(
            "tool_process_failure",
            vec![
                step(
                    run_started(),
                    call(
                        "test.run",
                        json!({"command": fixture_process_command("fixture-fail")}),
                    ),
                ),
                step(
                    tool_result_containing(json!({"passed": false})),
                    complete("Observed and reported the failed test result."),
                ),
            ],
        )
    }

    /// Rust-data definition of a tool deadline expiring.
    #[must_use]
    pub fn tool_timeout() -> Self {
        built_in(
            "tool_timeout",
            vec![
                step(
                    run_started(),
                    call(
                        "process.exec",
                        json!({
                            "argv": fixture_process_command("fixture-hang"),
                            "timeout_seconds": 1
                        }),
                    ),
                ),
                step(
                    tool_result_containing(json!({"timed_out": true})),
                    complete("Observed and reported the tool timeout."),
                ),
            ],
        )
    }

    /// Rust-data definition of cancellation while work is in flight.
    #[must_use]
    pub fn user_cancellation() -> Self {
        built_in(
            "user_cancellation",
            vec![
                step(
                    run_started(),
                    call(
                        "process.exec",
                        json!({
                            "argv": fixture_process_command("fixture-cancellable"),
                            "timeout_seconds": 120
                        }),
                    ),
                ),
                step(
                    ObservationPattern::Cancelled,
                    AgentAction::FailRun {
                        reason: AgentFailure::Cancelled,
                    },
                ),
            ],
        )
    }

    /// Rust-data definition of inspection after an interrupted process.
    #[must_use]
    pub fn restart_recovery() -> Self {
        built_in(
            "restart_recovery",
            vec![
                step(
                    run_started(),
                    call("fs.read", json!({"path": "src/lib.rs"})),
                ),
                step(
                    tool_failed("interrupted"),
                    AgentAction::FailRun {
                        reason: AgentFailure::Interrupted {
                            reason: "The recorded call was interrupted before restart.".to_owned(),
                        },
                    },
                ),
            ],
        )
    }

    /// Rust-data definition of a path-boundary refusal.
    #[must_use]
    pub fn forbidden_path() -> Self {
        built_in(
            "forbidden_path",
            vec![
                step(
                    run_started(),
                    call("fs.read", json!({"path": "../outside"})),
                ),
                step(
                    tool_failed("forbidden_path"),
                    complete("Observed and reported the workspace boundary refusal."),
                ),
            ],
        )
    }

    /// Rust-data definition of a capability policy denying a call.
    #[must_use]
    pub fn disallowed_capability() -> Self {
        built_in(
            "disallowed_capability",
            vec![
                step(
                    run_started(),
                    call(
                        "process.exec",
                        json!({
                            "argv": fixture_process_command("fixture-disallowed"),
                            "timeout_seconds": 120
                        }),
                    ),
                ),
                step(
                    ObservationPattern::PolicyDenied {
                        reason_contains: None,
                    },
                    complete("Observed and reported the disallowed capability."),
                ),
            ],
        )
    }
}

fn built_in(id: &str, steps: Vec<ScenarioStep>) -> Scenario {
    Scenario::from_parts(
        SCENARIO_FIXTURE_VERSION,
        ScenarioId::new(id).expect("valid built-in id"),
        steps,
    )
    .expect("valid built-in scenario")
}

fn fixture_process_command(program: &str) -> Vec<&str> {
    let test = match program {
        "fixture-fail" => "scenario_process_fixture_failure_child",
        "fixture-hang" => "scenario_process_fixture_hang_child",
        "fixture-cancellable" => "scenario_process_fixture_cancellable_child",
        "fixture-disallowed" => "scenario_process_fixture_disallowed_child",
        _ => unreachable!("built-in scenarios name only registered fixture processes"),
    };
    vec![program, "--exact", test, "--ignored", "--nocapture"]
}

fn hex_sha256(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn step(expect: ObservationPattern, action: AgentAction) -> ScenarioStep {
    ScenarioStep::new(expect, action)
}

fn run_started() -> ObservationPattern {
    ObservationPattern::RunStarted { task_title: None }
}

fn tool_result() -> ObservationPattern {
    ObservationPattern::ToolResult {
        artifact_media_type: None,
        output_contains: None,
    }
}

fn tool_result_containing(expected: Value) -> ObservationPattern {
    ObservationPattern::ToolResult {
        artifact_media_type: None,
        output_contains: Some(expected),
    }
}

fn value_contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| value_contains(actual, value))
        }),
        _ => actual == expected,
    }
}

fn tool_failed(kind: &str) -> ObservationPattern {
    ObservationPattern::ToolFailed {
        error_kind: Some(kind.to_owned()),
    }
}

fn call(tool_id: &str, input: Value) -> AgentAction {
    AgentAction::CallTool {
        tool_id: ToolId::new(tool_id).expect("valid built-in tool id"),
        tool_version: ToolVersion::new("1.0.0").expect("valid built-in tool version"),
        input,
    }
}

fn complete(summary: &str) -> AgentAction {
    AgentAction::CompleteRun {
        summary: summary.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ObservationPattern, Scenario, ScenarioError, ScenarioId, ScenarioStep};
    use crate::{
        agent::{AgentAction, Observation, TaskRef, ToolErrorView, ToolResultView, WorkspaceRef},
        domain::{TaskId, ToolCallId},
        store::PassThrough,
    };

    #[test]
    fn patterns_match_selected_fields_without_matching_incidental_ids() {
        let pattern = ObservationPattern::ToolFailed {
            error_kind: Some("timed_out".to_owned()),
        };
        for call in [ToolCallId::new(), ToolCallId::new()] {
            assert!(pattern.matches(&Observation::ToolFailed {
                call,
                error: ToolErrorView::new("timed_out", "deadline", &PassThrough),
            }));
        }
        assert!(!pattern.matches(&Observation::ToolFailed {
            call: ToolCallId::new(),
            error: ToolErrorView::new("process_failed", "exit 1", &PassThrough),
        }));

        let failed_test = ObservationPattern::ToolResult {
            artifact_media_type: None,
            output_contains: Some(serde_json::json!({"passed": false})),
        };
        assert!(failed_test.matches(&Observation::ToolResult {
            call: ToolCallId::new(),
            result: ToolResultView::inline(
                serde_json::json!({
                    "passed": false,
                    "exit_code": 1
                }),
                &PassThrough,
            ),
        }));
        assert!(!failed_test.matches(&Observation::ToolResult {
            call: ToolCallId::new(),
            result: ToolResultView::inline(serde_json::json!({"passed": true}), &PassThrough,),
        }));

        let any_start = ObservationPattern::RunStarted { task_title: None };
        assert!(any_start.matches(&Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "incidental", &PassThrough),
            workspace: WorkspaceRef::new(None, "/workspace", &PassThrough),
        }));
    }

    #[test]
    fn fixtures_probe_versions_before_the_strict_body() {
        let too_new = r#"{"v":99,"id":"future","future_shape":{"anything":true}}"#;
        assert!(matches!(
            Scenario::from_json(too_new),
            Err(ScenarioError::FixtureTooNew {
                found: 99,
                supported: 1
            })
        ));

        let unknown = r#"{
            "v": 1,
            "id": "current",
            "steps": [],
            "future_shape": true
        }"#;
        assert!(matches!(
            Scenario::from_json(unknown),
            Err(ScenarioError::InvalidFixture(_))
        ));
    }

    #[test]
    fn definitions_require_one_terminal_action_at_the_end() {
        let id = ScenarioId::new("bad_scenario").unwrap();
        let error = Scenario::new(
            id,
            vec![ScenarioStep::new(
                ObservationPattern::Cancelled,
                AgentAction::Plan { steps: Vec::new() },
            )],
        )
        .unwrap_err();
        assert!(matches!(error, ScenarioError::InvalidDefinition { .. }));
    }
}
