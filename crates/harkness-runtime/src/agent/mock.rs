use sha2::{Digest, Sha256};

use super::scenario::BUILTIN_SCENARIO_NAMES;
use super::{
    Agent, AgentAction, AgentFailure, AgentSessionId, AgentSessionState, Observation, Scenario,
    ScenarioError,
};

const OBSERVATION_DIGEST_DOMAIN: &[u8] = b"harkness.agent.observation-history.v1";

/// Deterministic scenario-driven implementation of [`Agent`].
///
/// The mock owns no runtime component. It compares the observation supplied to
/// [`Agent::next_action`] with the next structural pattern in its script and
/// returns the scripted action. The observation history is committed to a
/// chained digest in [`AgentSessionState`], so two identical replays prove
/// byte-identical decisions without retaining workspace-derived observations.
#[derive(Clone, Debug)]
pub struct MockAgent {
    scenario: Scenario,
    session_id: AgentSessionId,
    cursor: u32,
    observation_digest: [u8; 32],
}

impl MockAgent {
    /// Every built-in scenario name in stable order.
    #[must_use]
    pub const fn scenario_names() -> &'static [&'static str] {
        BUILTIN_SCENARIO_NAMES
    }

    /// Constructs one registered scenario with a fresh session identity.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::UnknownScenario`] when `name` is not in
    /// [`scenario_names`](Self::scenario_names).
    pub fn scenario(name: &str) -> Result<Self, ScenarioError> {
        Self::scenario_version(name, super::SCENARIO_FIXTURE_VERSION)
    }

    /// Constructs one exact retained version of a registered scenario.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::UnknownScenario`] when `name` is not registered,
    /// or [`ScenarioError::UnknownScenarioVersion`] when this build no longer
    /// contains `version` for that name.
    pub fn scenario_version(name: &str, version: u32) -> Result<Self, ScenarioError> {
        Scenario::builtin(name, version).map(Self::from_scenario)
    }

    /// Constructs a mock from validated scenario data.
    #[must_use]
    pub fn from_scenario(scenario: Scenario) -> Self {
        Self {
            scenario,
            session_id: AgentSessionId::new(),
            cursor: 0,
            observation_digest: initial_digest(),
        }
    }

    /// Restores a built-in scenario from a durable checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::UnknownCheckpointDefinition`] when the exact
    /// retained definition is not built into this version, or
    /// [`ScenarioError::InvalidCheckpoint`] when its cursor is beyond the
    /// script. The checkpoint's deserializer has already validated its digest.
    pub fn from_state(state: AgentSessionState) -> Result<Self, ScenarioError> {
        let scenario = Scenario::builtin_by_definition(
            state.scenario_version(),
            state.scenario_definition_digest(),
        )?;
        Self::from_state_with_scenario(state, scenario)
    }

    /// Restores a checkpoint using an explicitly resolved scenario definition.
    ///
    /// This is the recovery path for scenarios loaded from JSON rather than the
    /// built-in registry. The fixture version and definition digest (which
    /// commits to the id) must match the checkpoint, so a restart can never
    /// silently switch scripts.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::CheckpointDefinitionMismatch`] when `scenario`
    /// is not the exact definition named by `state`, or
    /// [`ScenarioError::InvalidCheckpoint`] when the cursor is beyond it.
    pub fn from_state_with_scenario(
        state: AgentSessionState,
        scenario: Scenario,
    ) -> Result<Self, ScenarioError> {
        if state.scenario_version() != scenario.version()
            || state.scenario_definition_digest() != scenario.definition_digest()
        {
            return Err(ScenarioError::CheckpointDefinitionMismatch {
                scenario: scenario.id().clone(),
                version: state.scenario_version(),
            });
        }
        if usize::try_from(state.cursor()).unwrap_or(usize::MAX) > scenario.steps().len() {
            return Err(ScenarioError::InvalidCheckpoint {
                scenario: scenario.id().clone(),
                cursor: state.cursor(),
                steps: scenario.steps().len(),
            });
        }
        let observation_digest = decode_digest(state.observation_digest())
            .expect("AgentSessionState guarantees a canonical SHA-256 digest");
        Ok(Self {
            scenario,
            session_id: state.session_id(),
            cursor: state.cursor(),
            observation_digest,
        })
    }

    /// Scenario this mock is replaying.
    #[must_use]
    pub const fn definition(&self) -> &Scenario {
        &self.scenario
    }
}

impl Agent for MockAgent {
    fn session_id(&self) -> AgentSessionId {
        self.session_id
    }

    fn next_action(&mut self, observation: Observation) -> AgentAction {
        let Some(step) = usize::try_from(self.cursor)
            .ok()
            .and_then(|cursor| self.scenario.steps().get(cursor))
        else {
            return AgentAction::FailRun {
                reason: AgentFailure::ScenarioExhausted {
                    scenario: self.scenario.id().clone(),
                },
            };
        };

        if !step.expectation().matches(&observation) {
            return AgentAction::FailRun {
                reason: AgentFailure::ScenarioDivergence {
                    expected: step.expectation().kind(),
                    actual: observation.kind(),
                },
            };
        }

        let Ok(observation_digest) = advance_digest(self.observation_digest, &observation) else {
            return AgentAction::FailRun {
                reason: AgentFailure::AgentFailed {
                    reason: "the observation cannot be represented in the durable agent format"
                        .to_owned(),
                },
            };
        };
        self.observation_digest = observation_digest;
        self.cursor += 1;
        step.action().clone()
    }

    fn state(&self) -> AgentSessionState {
        AgentSessionState::new(
            self.session_id,
            self.scenario.version(),
            self.scenario.definition_digest(),
            self.cursor,
            encode_digest(self.observation_digest),
        )
    }
}

fn initial_digest() -> [u8; 32] {
    Sha256::digest(OBSERVATION_DIGEST_DOMAIN).into()
}

fn advance_digest(
    previous: [u8; 32],
    observation: &Observation,
) -> Result<[u8; 32], serde_json::Error> {
    let encoded = serde_json::to_vec(observation)?;
    let mut digest = Sha256::new();
    digest.update(OBSERVATION_DIGEST_DOMAIN);
    digest.update(previous);
    digest.update((encoded.len() as u64).to_be_bytes());
    digest.update(encoded);
    Ok(digest.finalize().into())
}

fn encode_digest(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        digest[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(digest)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::MockAgent;
    use crate::{
        agent::{
            Agent, AgentAction, AgentFailure, Observation, TaskRef, ToolResultView, WorkspaceRef,
        },
        domain::{TaskId, ToolCallId},
        store::PassThrough,
        tool::ArtifactRef,
    };

    fn started() -> Observation {
        Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "test", &PassThrough),
            workspace: WorkspaceRef::new(None, "/workspace", &PassThrough),
        }
    }

    #[test]
    fn divergence_names_expected_and_actual_observation_kinds() {
        let mut agent = MockAgent::scenario("read_only_success").unwrap();
        let digest = agent.state().observation_digest().to_owned();
        let action = agent.next_action(Observation::Cancelled);
        assert_eq!(agent.state().cursor(), 0);
        assert_eq!(agent.state().observation_digest(), digest);
        assert!(matches!(
            action,
            AgentAction::FailRun {
                reason: AgentFailure::ScenarioDivergence {
                    expected: crate::agent::ObservationKind::RunStarted,
                    actual: crate::agent::ObservationKind::Cancelled,
                }
            }
        ));
    }

    #[test]
    fn identical_replays_produce_identical_actions_and_history_digests() {
        let observations = [
            started(),
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"files": 1}), &PassThrough),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"text": "source"}), &PassThrough),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"patched": true}), &PassThrough),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"passed": true}), &PassThrough),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::with_artifacts(
                    serde_json::json!({"artifact": "diff"}),
                    vec![ArtifactRef {
                        id: "diff-fixture".to_owned(),
                        media_type: "text/x-diff".to_owned(),
                        byte_len: 8,
                    }],
                    &PassThrough,
                ),
            },
        ];
        let mut first = MockAgent::scenario("edit_test_diff_success").unwrap();
        let mut second = MockAgent::scenario("edit_test_diff_success").unwrap();
        let first_actions = observations
            .iter()
            .cloned()
            .map(|observation| first.next_action(observation))
            .collect::<Vec<_>>();
        let second_actions = observations
            .iter()
            .cloned()
            .map(|observation| second.next_action(observation))
            .collect::<Vec<_>>();
        assert_eq!(first_actions, second_actions);
        assert_eq!(
            first.state().observation_digest(),
            second.state().observation_digest()
        );
    }

    #[test]
    fn a_checkpoint_resumes_at_the_next_script_transition() {
        let mut agent = MockAgent::scenario("read_only_success").unwrap();
        assert_eq!(agent.next_action(started()).kind(), "call_tool");
        let checkpoint = agent.state();
        let session = checkpoint.session_id();
        let digest = checkpoint.observation_digest().to_owned();

        let mut resumed = MockAgent::from_state(checkpoint).unwrap();
        assert_eq!(resumed.session_id(), session);
        assert_eq!(resumed.state().observation_digest(), digest);
        let next = resumed.next_action(Observation::ToolResult {
            call: ToolCallId::new(),
            result: ToolResultView::inline(serde_json::json!({}), &PassThrough),
        });
        assert!(matches!(next, AgentAction::CallTool { .. }));
        assert_eq!(resumed.state().cursor(), 2);
    }

    #[test]
    fn a_checkpoint_refuses_a_different_scenario_definition() {
        let mut agent = MockAgent::scenario("read_only_success").unwrap();
        assert_eq!(agent.next_action(started()).kind(), "call_tool");
        let checkpoint = agent.state();

        let replacement = crate::agent::Scenario::new(
            crate::agent::ScenarioId::new("read_only_success").unwrap(),
            vec![crate::agent::ScenarioStep::new(
                crate::agent::ObservationPattern::Cancelled,
                AgentAction::FailRun {
                    reason: AgentFailure::Cancelled,
                },
            )],
        )
        .unwrap();
        assert!(matches!(
            MockAgent::from_state_with_scenario(checkpoint, replacement),
            Err(crate::agent::ScenarioError::CheckpointDefinitionMismatch { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_workspace_observation_fails_without_advancing_or_panicking() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let mut root = b"/workspace-".to_vec();
        root.push(0xff);
        let observation = Observation::RunStarted {
            task: TaskRef::new(TaskId::new(), "test", &PassThrough),
            workspace: WorkspaceRef::new(
                None,
                PathBuf::from(OsString::from_vec(root)),
                &PassThrough,
            ),
        };
        let mut agent = MockAgent::scenario("read_only_success").unwrap();
        let before = agent.state();

        assert!(matches!(
            agent.next_action(observation),
            AgentAction::FailRun {
                reason: AgentFailure::AgentFailed { reason }
            } if reason == "the observation cannot be represented in the durable agent format"
        ));
        assert_eq!(agent.state().cursor(), before.cursor());
        assert_eq!(
            agent.state().observation_digest(),
            before.observation_digest()
        );
    }
}
