//! What instrumentation costs a tool call, with the subscriber actually running.
//!
//! `tool::execution_tests`'s overhead target measures an executor in a process
//! where no subscriber is installed, and `tracing` in that state is close to
//! free — so it proves nothing about the arrangement Harkness ships. This binary
//! installs the real one: the JSON formatter, the redacting writer, and the
//! rotating file under a temporary data directory, at the default `info` level.
//!
//! Its own binary because the subscriber is process-global, and `#[ignore]`d for
//! the reason every latency target here is: a debug build measures the optimizer
//! being off, not the code being slow.
//!
//! ```sh
//! cargo test --release -p harkness-runtime --test diagnostics_overhead -- --ignored
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use harkness_git::Cancellation;
use harkness_runtime::domain::{Run, Step, Task, ToolCall};
use harkness_runtime::observe;
use harkness_runtime::store::Store;
use harkness_runtime::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolExecutor, ToolIdentity, ToolMetadata,
    ToolRegistry,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

/// The budget the tool runtime publishes, excluding the tool's own work.
const BUDGET: Duration = Duration::from_millis(10);

/// Calls measured after the warm one, so a single scheduling hiccup cannot
/// decide the result.
const MEASURED_CALLS: u32 = 32;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    message: String,
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct EchoOutput {
    echoed: String,
}

/// A tool that does nothing, so what is measured is the runtime around it.
struct Echo;

impl Tool for Echo {
    type Input = EchoInput;
    type Output = EchoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.echo", "1.0.0").unwrap(),
            "Echo fixture",
            "Returns its input.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: EchoInput,
        _context: &mut ExecutionContext,
    ) -> Result<EchoOutput, ToolError> {
        Ok(EchoOutput {
            echoed: input.message,
        })
    }
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn per_call_overhead_stays_inside_the_budget_with_the_subscriber_installed() {
    let data_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();

    let outcome = observe::init(
        Some(data_dir.path()),
        observe::Options::default().with_default_filter(observe::DEFAULT_FILTER),
    );
    assert!(
        matches!(outcome, observe::InitOutcome::Logging { .. }),
        "the point of this measurement is that lines are really being written: {outcome:?}"
    );

    let store = Arc::new(Store::open(data_dir.path()).unwrap());
    let mut registry = ToolRegistry::new();
    registry.register(Echo).unwrap();
    let executor = ToolExecutor::new(Arc::clone(&store), Arc::new(registry));

    let task = Task::new(
        "overhead",
        workspace.path(),
        Some(ProjectId::new()),
        OffsetDateTime::now_utc(),
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), OffsetDateTime::now_utc());
    store.insert_run(&run).unwrap();
    let step = Step::new(run.id(), 0, "measure", OffsetDateTime::now_utc());
    store.insert_step(&step).unwrap();

    let pending = |message: &str| {
        let call = ToolCall::new(
            &step,
            "fixture.echo",
            "1.0.0",
            json!({ "message": message }),
            OffsetDateTime::now_utc(),
        );
        let id = call.id();
        store.insert_tool_call(&call).unwrap();
        id
    };

    // One warm call, so the measurement is not paying for the reader pool, the
    // prepared statements, or the first line opening the log file.
    let warm = pending("warm");
    executor
        .execute(warm, workspace.path(), &Cancellation::default())
        .unwrap();

    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    for index in 0..MEASURED_CALLS {
        let call = pending(&format!("measured {index}"));
        let began = Instant::now();
        let completed = executor
            .execute(call, workspace.path(), &Cancellation::default())
            .unwrap();
        let elapsed = began.elapsed();
        assert!(completed.outcome().succeeded());
        worst = worst.max(elapsed);
        total += elapsed;
    }

    let mean = total / MEASURED_CALLS;
    println!("with tracing at info: mean {mean:?} over {MEASURED_CALLS} calls");
    // The worst call rather than the mean: an instrumentation cost that is
    // usually free and occasionally not is exactly the regression this exists
    // to catch.
    harkness_test_fixtures::latency::record(
        "observe::per_call_overhead_with_the_subscriber_installed",
        worst,
        BUDGET,
    );

    // And the lines really were written, so the measurement was not of a
    // subscriber that filtered everything out.
    let log = std::fs::read_to_string(observe::log_file(data_dir.path())).unwrap();
    assert!(
        log.lines()
            .filter(|line| line.contains("tool call finished"))
            .count()
            >= MEASURED_CALLS as usize,
        "every measured call should have produced a line"
    );
}
