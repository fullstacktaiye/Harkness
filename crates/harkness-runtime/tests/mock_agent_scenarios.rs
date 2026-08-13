use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use harkness_runtime::{
    agent::{
        Agent, AgentAction, AgentFailure, AgentSessionState, ApprovalOutcomeView, MockAgent,
        Observation, Scenario, TaskRef, ToolErrorView, ToolResultView, WorkspaceRef,
    },
    domain::{Run, RunId, StepId, Task, TaskId, ToolCallId},
    store::{EventKind, RunEvent, Store},
    tool::{
        ArtifactRef, ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
        ToolRegistry, invoke,
    },
};
use harkness_test_fixtures::Fixture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::to_raw_value};
use time::macros::datetime;

const WIRE_CONTRACT_FIXTURE: &str = include_str!("../src/agent/fixtures/wire-contract-v1.json");

const SCENARIO_FIXTURES: &[(&str, &str)] = &[
    (
        "read_only_success",
        include_str!("../src/agent/fixtures/read-only-success-v1.json"),
    ),
    (
        "edit_test_diff_success",
        include_str!("../src/agent/fixtures/edit-test-diff-success-v1.json"),
    ),
    (
        "approval_denied",
        include_str!("../src/agent/fixtures/approval-denied-v1.json"),
    ),
    (
        "invalid_tool_input",
        include_str!("../src/agent/fixtures/invalid-tool-input-v1.json"),
    ),
    (
        "tool_process_failure",
        include_str!("../src/agent/fixtures/tool-process-failure-v1.json"),
    ),
    (
        "tool_timeout",
        include_str!("../src/agent/fixtures/tool-timeout-v1.json"),
    ),
    (
        "user_cancellation",
        include_str!("../src/agent/fixtures/user-cancellation-v1.json"),
    ),
    (
        "restart_recovery",
        include_str!("../src/agent/fixtures/restart-recovery-v1.json"),
    ),
    (
        "forbidden_path",
        include_str!("../src/agent/fixtures/forbidden-path-v1.json"),
    ),
    (
        "disallowed_capability",
        include_str!("../src/agent/fixtures/disallowed-capability-v1.json"),
    ),
];

#[test]
fn all_ten_scenarios_are_registered_in_stable_order() {
    assert_eq!(
        MockAgent::scenario_names(),
        [
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
        ]
    );
    assert_eq!(MockAgent::scenario_names(), fixture_names());
    for name in MockAgent::scenario_names() {
        assert_eq!(
            MockAgent::scenario(name)
                .unwrap()
                .definition()
                .id()
                .as_str(),
            *name
        );
    }
}

#[test]
fn rust_scenarios_and_frozen_json_fixtures_are_byte_compatible() {
    for (name, fixture) in SCENARIO_FIXTURES {
        let rust = MockAgent::scenario(name).unwrap().definition().clone();
        let loaded = Scenario::from_json(fixture).unwrap();
        assert_eq!(loaded, rust, "{name} fixture changed meaning");
        assert_eq!(
            rust.to_json_pretty().unwrap(),
            *fixture,
            "{name} fixture is not the canonical v1 encoding"
        );
    }
}

#[test]
fn action_observation_and_session_wire_forms_match_the_frozen_v1_fixture() {
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct WireContract {
        v: u32,
        state: AgentSessionState,
        actions: Vec<AgentAction>,
        observations: Vec<Observation>,
    }

    let fixture: WireContract = serde_json::from_str(WIRE_CONTRACT_FIXTURE).unwrap();
    assert_eq!(fixture.v, 1);
    assert_eq!(fixture.actions.len(), 4);
    assert_eq!(fixture.observations.len(), 6);
    let mut encoded = serde_json::to_string_pretty(&fixture).unwrap();
    encoded.push('\n');
    assert_eq!(encoded, WIRE_CONTRACT_FIXTURE);
}

#[test]
fn every_scenario_replays_its_complete_action_sequence_through_the_agent_trait() {
    let cases = [
        Case::new(
            "read_only_success",
            vec![started(), ok(), ok(), ok()],
            &[
                "call:workspace.inspect",
                "call:fs.read",
                "call:git.diff",
                "complete",
            ],
        ),
        Case::new(
            "edit_test_diff_success",
            vec![started(), ok(), ok(), ok(), ok(), diff_artifact()],
            &[
                "call:workspace.inspect",
                "call:fs.read",
                "call:fs.apply_patch",
                "call:test.run",
                "call:git.diff",
                "complete",
            ],
        ),
        Case::new(
            "approval_denied",
            vec![started(), approval(ApprovalOutcomeView::Denied)],
            &["call:fs.apply_patch", "fail:approval_denied"],
        ),
        Case::new(
            "invalid_tool_input",
            vec![started(), failed("invalid_input")],
            &["call:fs.read", "complete"],
        ),
        Case::new(
            "tool_process_failure",
            vec![started(), result(json!({"passed": false, "exit_code": 1}))],
            &["call:test.run", "complete"],
        ),
        Case::new(
            "tool_timeout",
            vec![started(), failed("timed_out")],
            &["call:process.exec", "complete"],
        ),
        Case::new(
            "user_cancellation",
            vec![started(), Observation::Cancelled],
            &["call:process.exec", "fail:cancelled"],
        ),
        Case::new(
            "restart_recovery",
            vec![started(), failed("interrupted")],
            &["call:fs.read", "fail:interrupted"],
        ),
        Case::new(
            "forbidden_path",
            vec![started(), failed("forbidden_path")],
            &["call:fs.read", "complete"],
        ),
        Case::new(
            "disallowed_capability",
            vec![
                started(),
                policy_denied("the execute capability is disabled"),
            ],
            &["call:process.exec", "complete"],
        ),
    ];

    let _fixture = Fixture::new();
    for case in cases {
        let mut agent: Box<dyn Agent> = Box::new(MockAgent::scenario(case.name).unwrap());
        let actions = case
            .observations
            .into_iter()
            .map(|observation| agent.next_action(observation))
            .collect::<Vec<_>>();
        assert_eq!(
            labels(&actions),
            case.expected,
            "{} action sequence",
            case.name
        );
        assert_eq!(
            usize::try_from(agent.state().cursor()).unwrap(),
            actions.len(),
            "{} did not consume its full script",
            case.name
        );
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                AgentAction::FailRun {
                    reason: AgentFailure::ScenarioDivergence { .. }
                }
            )),
            "{} diverged from its own fixture",
            case.name
        );
    }
}

#[test]
fn invalid_tool_input_is_rejected_by_the_real_registry_before_the_body_runs() {
    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct ReadInput {
        path: String,
    }

    #[derive(JsonSchema, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ReadOutput {
        text: String,
    }

    struct ReadTool {
        executions: Arc<AtomicUsize>,
    }

    impl Tool for ReadTool {
        type Input = ReadInput;
        type Output = ReadOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fs.read", "1.0.0").unwrap(),
                "Read a file",
                "Reads one contained workspace file.",
                RiskLevel::Observe,
            )
        }

        fn execute(
            &self,
            input: ReadInput,
            _context: &mut ExecutionContext,
        ) -> Result<ReadOutput, ToolError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ReadOutput { text: input.path })
        }
    }

    let executions = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(ReadTool {
            executions: Arc::clone(&executions),
        })
        .unwrap();
    let fixture = Fixture::new();
    let workspace = fixture.directory("workspace");
    let mut agent = MockAgent::scenario("invalid_tool_input").unwrap();
    let action = agent.next_action(started());
    let AgentAction::CallTool {
        tool_id,
        tool_version,
        input,
    } = action
    else {
        panic!("invalid_tool_input did not request a tool");
    };
    assert_eq!(
        input,
        json!({"path": 42}),
        "the mock must emit the bad input verbatim"
    );

    let raw = to_raw_value(&input).unwrap();
    let mut context =
        ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), workspace)
            .unwrap();
    let error = invoke(&registry, &tool_id, Some(&tool_version), &raw, &mut context).unwrap_err();
    assert_eq!(error.kind(), "invalid_input");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "schema-invalid input reached the tool body"
    );

    let terminal = agent.next_action(Observation::ToolFailed {
        call: ToolCallId::new(),
        error: ToolErrorView::new(error.kind(), error.to_string()),
    });
    assert!(matches!(terminal, AgentAction::CompleteRun { .. }));
}

#[test]
fn session_state_round_trips_through_the_real_run_event_store() {
    let fixture = Fixture::new();
    let workspace = fixture.directory("workspace");
    let store = Store::open(&fixture.data_dir).unwrap();
    let task = Task::new(
        "agent checkpoint",
        &workspace,
        None,
        datetime!(2026-08-13 12:00 UTC),
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), datetime!(2026-08-13 12:01 UTC));
    store.insert_run(&run).unwrap();

    let mut agent = MockAgent::scenario("restart_recovery").unwrap();
    let _requested_read = agent.next_action(started_at(task.id(), &workspace));
    let state = agent.state();
    store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, datetime!(2026-08-13 12:02 UTC))
                .with_payload(serde_json::to_value(&state).unwrap()),
        )
        .unwrap();

    let events = store.events(run.id(), None, 10).unwrap();
    assert_eq!(events.len(), 1);
    let restored: AgentSessionState =
        serde_json::from_value(events[0].event.payload().clone()).unwrap();
    assert_eq!(restored, state);

    let resumed = MockAgent::from_state(restored).unwrap();
    assert_eq!(resumed.session_id(), agent.session_id());
    assert_eq!(resumed.state(), agent.state());
}

struct Case {
    name: &'static str,
    observations: Vec<Observation>,
    expected: &'static [&'static str],
}

impl Case {
    const fn new(
        name: &'static str,
        observations: Vec<Observation>,
        expected: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            observations,
            expected,
        }
    }
}

fn fixture_names() -> Vec<&'static str> {
    SCENARIO_FIXTURES.iter().map(|(name, _)| *name).collect()
}

fn labels(actions: &[AgentAction]) -> Vec<String> {
    actions
        .iter()
        .map(|action| match action {
            AgentAction::Plan { .. } => "plan".to_owned(),
            AgentAction::CallTool { tool_id, .. } => format!("call:{tool_id}"),
            AgentAction::CompleteRun { .. } => "complete".to_owned(),
            AgentAction::FailRun { reason } => format!("fail:{}", reason.kind()),
        })
        .collect()
}

fn started() -> Observation {
    started_at(TaskId::new(), Path::new("/workspace"))
}

fn started_at(task_id: TaskId, workspace: &Path) -> Observation {
    Observation::RunStarted {
        task: TaskRef {
            id: task_id,
            title: "fixture task".to_owned(),
        },
        workspace: WorkspaceRef {
            project_id: None,
            root: PathBuf::from(workspace),
        },
    }
}

fn ok() -> Observation {
    result(Value::Null)
}

fn result(output: Value) -> Observation {
    Observation::ToolResult {
        call: ToolCallId::new(),
        result: ToolResultView::inline(output),
    }
}

fn diff_artifact() -> Observation {
    Observation::ToolResult {
        call: ToolCallId::new(),
        result: ToolResultView::with_artifacts(
            json!({"artifact": "diff"}),
            vec![ArtifactRef {
                id: "artifact-fixture".to_owned(),
                media_type: "text/x-diff".to_owned(),
                byte_len: 42,
            }],
        ),
    }
}

fn failed(kind: &str) -> Observation {
    Observation::ToolFailed {
        call: ToolCallId::new(),
        error: ToolErrorView::new(kind, format!("fixture {kind}")),
    }
}

fn policy_denied(reason: &str) -> Observation {
    Observation::PolicyDenied {
        call: ToolCallId::new(),
        reason: reason.to_owned(),
    }
}

fn approval(outcome: ApprovalOutcomeView) -> Observation {
    Observation::ApprovalOutcome {
        call: ToolCallId::new(),
        outcome,
    }
}
