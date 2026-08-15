use std::{
    borrow::Cow,
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use harkness_runtime::{
    agent::{
        Agent, AgentAction, AgentFailure, AgentSessionState, ApprovalOutcomeView, MockAgent,
        Observation, RedactedText, Scenario, TaskRef, ToolErrorView, ToolResultView, WorkspaceRef,
    },
    domain::{Run, RunId, StepId, Task, TaskId, ToolCallId},
    store::{EventKind, PassThrough, Redactor, RunEvent, Store},
    tool::{
        ArtifactRef, ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
        ToolProcess, ToolRegistry, erase, invoke,
    },
    trust::{AllowlistedEnv, CommandSpec, PathBoundary},
};
use harkness_test_fixtures::{Fixture, child_path};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{
    Value, json,
    value::{RawValue, to_raw_value},
};
use sha2::{Digest, Sha256};
use time::macros::datetime;

const WIRE_CONTRACT_FIXTURE: &str = include_str!("../src/agent/fixtures/wire-contract-v1.json");
const SCENARIO_RUNNER_WORKSPACE: &str = "HARKNESS_SCENARIO_RUNNER_WORKSPACE";

harkness_test_fixtures::scenario_process_fixture_tests!();

#[test]
#[ignore = "re-executed to verify scenario PATH through the real process supervisor"]
fn scenario_process_real_runner_child() {
    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct EmptyInput {}

    #[derive(JsonSchema, Serialize)]
    #[serde(deny_unknown_fields)]
    struct EmptyOutput {}

    struct ProcessFixtureDescriptor;

    impl Tool for ProcessFixtureDescriptor {
        type Input = EmptyInput;
        type Output = EmptyOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.process", "1.0.0").unwrap(),
                "Fixture process",
                "Provides the descriptor whose baseline environment drives the real process path.",
                RiskLevel::Execute,
            )
        }

        fn execute(
            &self,
            _input: Self::Input,
            _context: &mut ExecutionContext,
        ) -> Result<Self::Output, ToolError> {
            Ok(EmptyOutput {})
        }
    }

    let workspace = child_path(SCENARIO_RUNNER_WORKSPACE);
    let erased = erase(ProcessFixtureDescriptor).unwrap();
    let environment = AllowlistedEnv::for_descriptor(erased.descriptor());
    let boundary = PathBoundary::new(&workspace, std::iter::empty::<&Path>()).unwrap();
    let cwd = boundary.contain(".").unwrap();
    let spec = CommandSpec::new(
        "fixture-fail",
        [
            "--exact",
            "scenario_process_fixture_failure_child",
            "--ignored",
            "--nocapture",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        cwd,
        environment,
    )
    .unwrap();
    let mut context =
        ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), &workspace)
            .unwrap();
    let output = ToolProcess::new(spec).run(&mut context).unwrap();
    assert_ne!(output.code(), Some(0));
}

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
        event_payloads: Box<RawValue>,
        actions: Vec<Box<RawValue>>,
        observations: Vec<Box<RawValue>>,
    }

    let fixture: WireContract = serde_json::from_str(WIRE_CONTRACT_FIXTURE).unwrap();
    assert_eq!(fixture.v, 1);
    assert_eq!(fixture.actions.len(), 9);
    assert_eq!(fixture.observations.len(), 7);
    assert_eq!(
        fixture.state.scenario_definition_digest(),
        Scenario::read_only_success().definition_digest()
    );
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
            vec![
                started(),
                ok(),
                ok(),
                diff_artifact(),
                result(json!({"passed": true})),
                ok(),
            ],
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
            vec![started(), result(json!({"timed_out": true}))],
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
            vec![started(), policy_denied("denied: workspace is untrusted")],
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
fn frozen_success_actions_use_the_published_tool_inputs() {
    let read_only = Scenario::read_only_success();
    let read_only_diff = call_input(&read_only, "git.diff");
    assert_eq!(read_only_diff, &json!({"target": "unstaged"}));

    let flagship = Scenario::edit_test_diff_success();
    let patch = call_input(&flagship, "fs.apply_patch");
    assert_eq!(
        patch["bases"][0]["base_sha256"],
        format!(
            "{:x}",
            Sha256::digest(b"pub const VALUE: &str = \"old\";\n")
        )
    );
    assert!(
        patch["patch"]
            .as_str()
            .unwrap()
            .contains("+pub const VALUE: &str = \"new\";")
    );
    assert_eq!(
        call_input(&flagship, "git.diff"),
        &json!({"target": "unstaged"})
    );
}

#[test]
fn process_scenarios_resolve_to_hermetic_cross_platform_fixture_children() {
    let fixture = Fixture::new();

    let failure = scenario_process_command("tool_process_failure", "command");
    let status = fixture_command(&fixture, &failure)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the failure fixture exited successfully");

    let workspace = fixture.directory("process-workspace");
    let mut real_runner = Command::new(std::env::current_exe().unwrap());
    real_runner.args([
        "--exact",
        "scenario_process_real_runner_child",
        "--ignored",
        "--nocapture",
    ]);
    real_runner.env_clear();
    real_runner.env(SCENARIO_RUNNER_WORKSPACE, &workspace);
    fixture.configure_scenario_process_path(&mut real_runner);
    assert!(real_runner.status().unwrap().success());

    for scenario in ["tool_timeout", "user_cancellation"] {
        let argv = scenario_process_command(scenario, "argv");
        let mut child = fixture_command(&fixture, &argv)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().expect("fixture stdout is piped");
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let ready = BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .any(|line| line.contains("HARKNESS_SCENARIO_FIXTURE_READY"));
            let _ = sender.send(ready);
        });
        assert_eq!(receiver.recv_timeout(Duration::from_secs(10)), Ok(true));
        child.kill().unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
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
        error: ToolErrorView::new(error.kind(), error.to_string(), &PassThrough),
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
                .with_payload(state.to_event_payload()),
        )
        .unwrap();

    let events = store.events(run.id(), None, 10).unwrap();
    assert_eq!(events.len(), 1);
    let restored =
        AgentSessionState::from_event_payload(events[0].event.payload().clone()).unwrap();
    assert_eq!(restored, state);

    let resumed = MockAgent::from_state(restored).unwrap();
    assert_eq!(resumed.session_id(), agent.session_id());
    assert_eq!(resumed.state(), agent.state());
}

#[derive(Debug)]
struct RewriteEveryString;

impl Redactor for RewriteEveryString {
    fn redact_text<'a>(&self, _text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed("[redacted]")
    }

    fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
        sink
    }
}

#[test]
fn session_state_survives_a_mutating_event_redactor() {
    let fixture = Fixture::new();
    let workspace = fixture.directory("redacted-workspace");
    let store = Store::open(&fixture.data_dir)
        .unwrap()
        .redacting(Arc::new(RewriteEveryString));
    let task = Task::new(
        "agent checkpoint",
        &workspace,
        None,
        datetime!(2026-08-13 13:00 UTC),
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), datetime!(2026-08-13 13:01 UTC));
    store.insert_run(&run).unwrap();

    let mut agent = MockAgent::scenario("restart_recovery").unwrap();
    let _requested_read = agent.next_action(started_at(task.id(), &workspace));
    let state = agent.state();
    store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, datetime!(2026-08-13 13:02 UTC))
                .with_payload(state.to_event_payload()),
        )
        .unwrap();

    let events = store.events(run.id(), None, 10).unwrap();
    let restored =
        AgentSessionState::from_event_payload(events[0].event.payload().clone()).unwrap();
    assert_eq!(restored, state);
    assert_eq!(MockAgent::from_state(restored).unwrap().state(), state);
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

fn call_input<'a>(scenario: &'a Scenario, tool: &str) -> &'a Value {
    scenario
        .steps()
        .iter()
        .find_map(|step| match step.action() {
            AgentAction::CallTool { tool_id, input, .. } if tool_id.as_str() == tool => Some(input),
            _ => None,
        })
        .unwrap_or_else(|| panic!("scenario does not call {tool}"))
}

fn scenario_process_command(scenario: &str, field: &str) -> Vec<String> {
    let mut agent = MockAgent::scenario(scenario).unwrap();
    let AgentAction::CallTool { input, .. } = agent.next_action(started()) else {
        panic!("{scenario} did not begin with a tool call");
    };
    serde_json::from_value(input[field].clone()).unwrap()
}

fn fixture_command(fixture: &Fixture, argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.env_clear();
    fixture.configure_scenario_process_path(&mut command);
    assert!(
        fixture.scenario_process_program(&argv[0]).is_file(),
        "missing fixture program {}",
        argv[0]
    );
    command
}

fn started() -> Observation {
    started_at(TaskId::new(), Path::new("/workspace"))
}

fn started_at(task_id: TaskId, workspace: &Path) -> Observation {
    Observation::RunStarted {
        task: TaskRef::new(task_id, "fixture task", &PassThrough),
        workspace: WorkspaceRef::new(None, workspace, &PassThrough),
    }
}

fn ok() -> Observation {
    result(Value::Null)
}

fn result(output: Value) -> Observation {
    Observation::ToolResult {
        call: ToolCallId::new(),
        result: ToolResultView::inline(output, &PassThrough),
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
            &PassThrough,
        ),
    }
}

fn failed(kind: &str) -> Observation {
    Observation::ToolFailed {
        call: ToolCallId::new(),
        error: ToolErrorView::new(kind, format!("fixture {kind}"), &PassThrough),
    }
}

fn policy_denied(reason: &str) -> Observation {
    Observation::PolicyDenied {
        call: ToolCallId::new(),
        reason: RedactedText::new(reason, &PassThrough),
    }
}

fn approval(outcome: ApprovalOutcomeView) -> Observation {
    Observation::ApprovalOutcome {
        call: ToolCallId::new(),
        outcome,
    }
}
