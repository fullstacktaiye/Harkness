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
        Scenario::builtin(name).map(Self::from_scenario)
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
    /// Returns [`ScenarioError::UnknownScenario`] when the checkpoint names a
    /// scenario this build does not contain, or
    /// [`ScenarioError::InvalidCheckpoint`] when its cursor is beyond the
    /// script. The checkpoint's deserializer has already validated its digest.
    pub fn from_state(state: AgentSessionState) -> Result<Self, ScenarioError> {
        let scenario = Scenario::builtin(state.scenario_id().as_str())?;
        if usize::try_from(state.cursor()).unwrap_or(usize::MAX) > scenario.steps().len() {
            return Err(ScenarioError::InvalidCheckpoint {
                scenario: state.scenario_id().clone(),
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

        self.observation_digest = advance_digest(self.observation_digest, &observation);
        self.cursor += 1;
        step.action().clone()
    }

    fn state(&self) -> AgentSessionState {
        AgentSessionState::new(
            self.session_id,
            self.scenario.id().clone(),
            self.cursor,
            encode_digest(self.observation_digest),
        )
    }
}

fn initial_digest() -> [u8; 32] {
    Sha256::digest(OBSERVATION_DIGEST_DOMAIN).into()
}

fn advance_digest(previous: [u8; 32], observation: &Observation) -> [u8; 32] {
    let encoded = serde_json::to_vec(observation)
        .expect("Observation contains only infallibly serializable JSON values");
    let mut digest = Sha256::new();
    digest.update(OBSERVATION_DIGEST_DOMAIN);
    digest.update(previous);
    digest.update((encoded.len() as u64).to_be_bytes());
    digest.update(encoded);
    digest.finalize().into()
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
        tool::ArtifactRef,
    };

    fn started() -> Observation {
        Observation::RunStarted {
            task: TaskRef {
                id: TaskId::new(),
                title: "test".to_owned(),
            },
            workspace: WorkspaceRef {
                project_id: None,
                root: PathBuf::from("/workspace"),
            },
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
                result: ToolResultView::inline(serde_json::json!({"files": 1})),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"text": "source"})),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"patched": true})),
            },
            Observation::ToolResult {
                call: ToolCallId::new(),
                result: ToolResultView::inline(serde_json::json!({"passed": true})),
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
            result: ToolResultView::inline(serde_json::json!({})),
        });
        assert!(matches!(next, AgentAction::CallTool { .. }));
        assert_eq!(resumed.state().cursor(), 2);
    }
}
