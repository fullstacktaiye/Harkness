//! Behavioral coverage for the durable run store.
//!
//! Every test opens its own database under a temporary directory, so nothing
//! here can read or write the real Harkness data directory.

use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::domain::{
    ExecutionState, Failure, Run, RunId, Step, StepId, Task, TaskId, ToolCall, ToolCallId,
    ToolCallState, ToolCallWire,
};

use super::migration::{MIGRATIONS, Migration, SCHEMA_VERSION, apply, recorded_version};
use super::{DATABASE_FILE, MAX_INLINE_PAYLOAD_BYTES, RunCursor, RunPage, Store, guard};

/// Text one byte past the largest value any column will hold.
fn oversized_text() -> String {
    "a".repeat(MAX_INLINE_PAYLOAD_BYTES + 1)
}

/// The frozen v1 database committed beside this module.
///
/// It is written by `regenerate_the_frozen_v1_fixture` and must not be edited
/// by hand: it exists to prove that a database created by an earlier build
/// still opens, reads, and migrates forward.
const FROZEN_V1_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v1.db");

const FIXTURE_TASK_ID: &str = "11111111-1111-4111-8111-111111111111";
const FIXTURE_RUN_ID: &str = "22222222-2222-4222-8222-222222222222";
const FIXTURE_STEP_ID: &str = "33333333-3333-4333-8333-333333333333";
const FIXTURE_TOOL_CALL_ID: &str = "44444444-4444-4444-8444-444444444444";

/// A store in its own temporary data directory.
struct Fixture {
    data_dir: TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let data_dir = TempDir::new().unwrap();
        let store = Store::open(data_dir.path()).unwrap();
        Self { data_dir, store }
    }

    fn reopen(&self) -> Store {
        Store::open(self.data_dir.path()).unwrap()
    }
}

/// A deterministic instant, `offset` seconds after a fixed epoch.
fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
}

fn stored_task(store: &Store) -> Task {
    let task = Task::with_id(
        TaskId::from_str(FIXTURE_TASK_ID).unwrap(),
        "Add the run store",
        "/workspace/harkness",
        None,
        at(0),
    );
    store.insert_task(&task).unwrap();
    task
}

fn stored_run(store: &Store, task: &Task) -> Run {
    let run = Run::with_id(RunId::from_str(FIXTURE_RUN_ID).unwrap(), task.id(), at(1));
    store.insert_run(&run).unwrap();
    run
}

fn stored_step(store: &Store, run: &Run) -> Step {
    let step = Step::with_id(
        StepId::from_str(FIXTURE_STEP_ID).unwrap(),
        run.id(),
        0,
        "Read the schema",
        at(2),
    );
    store.insert_step(&step).unwrap();
    step
}

fn stored_tool_call(store: &Store, step: &Step) -> ToolCall {
    let call = ToolCall::with_id(
        ToolCallId::from_str(FIXTURE_TOOL_CALL_ID).unwrap(),
        step,
        "fs.read",
        "1.0.0",
        json!({"path": "crates/harkness-runtime/src/store/mod.rs"}),
        at(3),
    );
    store.insert_tool_call(&call).unwrap();
    call
}

/// A run created against `task` with a distinct identity and creation time.
fn queued_run(store: &Store, task: &Task, offset: i64) -> Run {
    let run = Run::new(task.id(), at(offset));
    store.insert_run(&run).unwrap();
    run
}

fn pragma<T: rusqlite::types::FromSql>(connection: &Connection, name: &str) -> T {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .unwrap()
}

// -- opening ----------------------------------------------------------------

#[test]
fn opening_creates_the_database_under_the_given_data_directory() {
    let parent = TempDir::new().unwrap();
    let data_dir = parent.path().join("nested").join("harkness");

    let store = Store::open(&data_dir).unwrap();

    assert_eq!(store.path(), data_dir.join(DATABASE_FILE));
    assert!(store.path().is_file(), "the database file was not created");
    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );
}

#[test]
fn opening_an_existing_database_reuses_it_instead_of_replacing_it() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    drop(stored_run(&fixture.store, &task));

    let reopened = fixture.reopen();

    assert_eq!(reopened.list_runs(RunPage::new(10)).unwrap().runs.len(), 1);
}

#[test]
fn pragmas_are_applied_to_every_connection() {
    let fixture = Fixture::new();

    let assert_pragmas = |connection: &Connection, label: &str| {
        assert_eq!(
            pragma::<String>(connection, "journal_mode").to_ascii_lowercase(),
            "wal",
            "{label} is not in WAL mode"
        );
        assert_eq!(
            pragma::<i64>(connection, "foreign_keys"),
            1,
            "{label} does not enforce foreign keys"
        );
        assert!(
            pragma::<i64>(connection, "busy_timeout") >= 5_000,
            "{label} has too short a busy timeout"
        );
        assert_eq!(
            pragma::<i64>(connection, "synchronous"),
            1,
            "{label} is not at synchronous=NORMAL"
        );
    };

    assert_pragmas(&guard(&fixture.store.writer), "the writer");
    fixture
        .store
        .with_reader(|connection| {
            assert_pragmas(connection, "a reader");
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_newer_schema_is_refused_as_upgrade_and_leaves_the_file_untouched() {
    let data_dir = TempDir::new().unwrap();
    let path = data_dir.path().join(DATABASE_FILE);
    {
        // A rollback-journal database, so nothing is checkpointed on close and
        // the bytes on disk are entirely under this test's control.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA journal_mode = DELETE; \
                 CREATE TABLE from_the_future (id TEXT PRIMARY KEY); \
                 PRAGMA user_version = {};",
                SCHEMA_VERSION + 1
            ))
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let error = Store::open(data_dir.path()).unwrap_err();

    assert_eq!(error.kind(), "schema_too_new");
    assert!(
        error.to_string().contains("upgrade Harkness"),
        "the refusal should read as an upgrade request: {error}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refused database must not be modified"
    );
    assert!(
        !path.with_extension("db-wal").exists(),
        "a refused database must not gain a write-ahead log"
    );
}

#[test]
fn concurrent_opens_of_a_new_database_all_succeed() {
    // Creating a database is the only moment openers contend, and the two ways
    // it can go wrong are platform-dependent: climbing the migration ladder on
    // a version read outside the write lock replays `CREATE TABLE`, and the
    // switch into WAL takes an exclusive lock that SQLite refuses outright
    // rather than waiting for on some platforms. Several fresh databases give
    // both windows more than one chance to be hit.
    for attempt in 0..4 {
        let data_dir = TempDir::new().unwrap();
        let path = Arc::new(data_dir.path().to_path_buf());

        let openers = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                thread::spawn(move || Store::open(&path).map(|_| ()))
            })
            .collect::<Vec<_>>();

        let failures = openers
            .into_iter()
            .filter_map(|opener| opener.join().unwrap().err())
            .map(|error| format!("{}: {error}", error.kind()))
            .collect::<Vec<_>>();
        assert!(
            failures.is_empty(),
            "concurrent opens failed on attempt {attempt}: {failures:?}"
        );
        assert_eq!(
            recorded_version(&Connection::open(path.join(DATABASE_FILE)).unwrap()).unwrap(),
            SCHEMA_VERSION
        );
    }
}

#[test]
fn the_write_ahead_log_transition_waits_out_an_exclusive_lock() {
    let data_dir = TempDir::new().unwrap();
    let path = data_dir.path().join(DATABASE_FILE);

    // A rollback-journal database another connection holds exclusively. Moving
    // it into WAL needs that same exclusive lock.
    let holder = Connection::open(&path).unwrap();
    holder
        .execute_batch("PRAGMA journal_mode = DELETE; BEGIN EXCLUSIVE;")
        .unwrap();

    // Disabling the busy handler makes this connection be refused outright
    // instead of being made to wait, which is what Windows does to the WAL
    // transition even with a busy timeout set. Reproducing it here keeps the
    // regression visible on every platform rather than only in Windows CI.
    let waiter = Connection::open(&path).unwrap();
    waiter.busy_timeout(Duration::ZERO).unwrap();
    let transition =
        thread::spawn(move || super::enable_wal(&waiter).map_err(|error| error.to_string()));

    thread::sleep(Duration::from_millis(300));
    holder.execute_batch("COMMIT").unwrap();
    drop(holder);

    transition
        .join()
        .unwrap()
        .expect("the transition should have waited for the lock");
    assert_eq!(
        pragma::<String>(&Connection::open(&path).unwrap(), "journal_mode").to_ascii_lowercase(),
        "wal"
    );
}

const CONCURRENT_OPEN_CHILD: &str = "store::tests::open_the_store_in_the_shared_data_dir";

/// Re-entered as a child process by the cross-process migration test.
#[test]
#[ignore = "only run as a child process by the concurrent migration test"]
fn open_the_store_in_the_shared_data_dir() {
    let data_dir = std::env::var_os(CHILD_DATA_DIR_ENV)
        .map(PathBuf::from)
        .expect("child data directory was not set");

    let store = Store::open(&data_dir).unwrap();

    // Opening is not enough: the schema has to be usable, not merely recorded.
    store.list_runs(RunPage::new(1)).unwrap();
}

#[test]
fn independent_processes_migrate_a_new_database_exactly_once() {
    let data_dir = TempDir::new().unwrap();
    let path = data_dir.path().join(DATABASE_FILE);

    // Holding the write lock over an empty database lets every child get as far
    // as reading `user_version` 0 and then block requesting the lock, so they
    // are all released into the migration at once holding the same stale
    // answer. A fresh data directory produces that interleaving on its own;
    // this only makes it reliable. A child that arrives late simply finds the
    // work done, so the test cannot fail for being too slow.
    let mut blocker = Connection::open(&path).unwrap();
    blocker.pragma_update(None, "journal_mode", "WAL").unwrap();
    let held = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();

    let children = (0..4)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(CONCURRENT_OPEN_CHILD)
                .arg("--ignored")
                .env(CHILD_DATA_DIR_ENV, data_dir.path())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(500));
    held.rollback().unwrap();
    drop(blocker);

    for mut child in children {
        assert!(
            child.wait().unwrap().success(),
            "a concurrent opener failed to migrate the shared database"
        );
    }
    assert_eq!(
        recorded_version(&Connection::open(&path).unwrap()).unwrap(),
        SCHEMA_VERSION
    );
}

// -- checkpointing ----------------------------------------------------------

#[test]
fn a_successful_checkpoint_empties_the_write_ahead_log() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    drop(queued_run(&fixture.store, &task, 1));
    let log = fixture.store.path().with_extension("db-wal");
    assert!(log.metadata().unwrap().len() > 0, "nothing was logged");

    fixture.store.checkpoint().unwrap();

    assert_eq!(
        log.metadata().unwrap().len(),
        0,
        "a checkpoint that reported success left frames in the log"
    );
}

#[test]
fn a_checkpoint_a_reader_prevents_is_refused_rather_than_reported_as_done() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    drop(queued_run(&fixture.store, &task, 1));

    // A second connection parked in a read transaction pins the log. SQLite
    // reports that in the checkpoint's result row instead of failing the
    // statement, so a store that discarded the row would call this a backup.
    // The wait costs the busy timeout, which is why this is the one slow test
    // in the module.
    let reader = Connection::open(fixture.store.path()).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM runs;")
        .unwrap();

    let error = fixture.store.checkpoint().unwrap_err();

    assert_eq!(error.kind(), "incomplete_checkpoint");
    assert!(
        error.to_string().contains("runtime.db-wal"),
        "the refusal should say how to take a correct backup anyway: {error}"
    );

    reader.execute_batch("COMMIT").unwrap();
    fixture.store.checkpoint().unwrap();
}

// -- round trips ------------------------------------------------------------

#[test]
fn every_record_type_round_trips_through_the_store() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    // A fresh store proves the records survived the process, not just the cache.
    let reopened = fixture.reopen();
    assert_eq!(reopened.load_task(task.id()).unwrap(), task);
    assert_eq!(reopened.load_run(run.id()).unwrap(), run);
    assert_eq!(reopened.load_step(step.id()).unwrap(), step);
    assert_eq!(reopened.load_tool_call(call.id()).unwrap(), call);
    assert_eq!(reopened.load_run_steps(run.id()).unwrap(), vec![step]);
    assert_eq!(reopened.load_run_tool_calls(run.id()).unwrap(), vec![call]);
}

#[test]
fn a_task_keeps_its_optional_project_association() {
    let fixture = Fixture::new();
    let project_id = harkness_core::ProjectId::new();
    let task = Task::new("Linked task", "/workspace/linked", Some(project_id), at(0));

    fixture.store.insert_task(&task).unwrap();

    assert_eq!(
        fixture.store.load_task(task.id()).unwrap().project_id(),
        Some(project_id)
    );
}

#[test]
fn an_absent_record_is_reported_as_missing_rather_than_empty() {
    let fixture = Fixture::new();

    let error = fixture.store.load_run(RunId::new()).unwrap_err();

    assert_eq!(error.kind(), "not_found");
}

// -- containment ------------------------------------------------------------

#[test]
fn a_run_cannot_reference_a_task_that_is_not_stored() {
    let fixture = Fixture::new();
    let run = Run::new(TaskId::new(), at(1));

    let error = fixture.store.insert_run(&run).unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(
        error.to_string().contains("task"),
        "the refusal should name the container: {error}"
    );
}

#[test]
fn a_repeated_identity_is_refused_instead_of_overwriting_history() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let error = fixture.store.insert_run(&run).unwrap_err();

    assert_eq!(error.kind(), "already_exists");
}

#[test]
fn a_second_step_cannot_reuse_an_ordinal_within_its_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let first = stored_step(&fixture.store, &run);
    let clash = Step::new(run.id(), first.ordinal(), "Same position", at(4));

    let error = fixture.store.insert_step(&clash).unwrap_err();

    assert_eq!(error.kind(), "duplicate_step_ordinal");
    assert_eq!(fixture.store.load_run_steps(run.id()).unwrap(), vec![first]);
}

#[test]
fn a_tool_call_cannot_claim_a_run_its_step_does_not_belong_to() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let other_run = queued_run(&fixture.store, &task, 5);

    // Only a wire record can disagree with itself: the in-process constructor
    // derives the run from the step it is given.
    let disagreeing = ToolCall::try_from(ToolCallWire {
        schema_version: crate::domain::RUNTIME_RECORD_SCHEMA_VERSION,
        id: ToolCallId::new(),
        run_id: other_run.id(),
        step_id: step.id(),
        tool_id: "fs.read".to_owned(),
        tool_version: "1.0.0".to_owned(),
        input: json!({}),
        state: ToolCallState::Pending,
        revision: 0,
        created_at: at(6),
        updated_at: at(6),
        started_at: None,
        finished_at: None,
        failure: None,
        output: None,
        approvals: Vec::new(),
    })
    .unwrap();

    let error = fixture.store.insert_tool_call(&disagreeing).unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(
        fixture
            .store
            .load_run_tool_calls(other_run.id())
            .unwrap()
            .is_empty(),
        "the refused call must not have been written"
    );
}

// -- payload bounds ---------------------------------------------------------

#[test]
fn an_oversized_tool_call_input_is_refused_and_nothing_is_written() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let oversized = Value::String("a".repeat(MAX_INLINE_PAYLOAD_BYTES));
    let call = ToolCall::new(&step, "fs.write", "1.0.0", oversized, at(4));

    let error = fixture.store.insert_tool_call(&call).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert!(
        error
            .to_string()
            .contains(&MAX_INLINE_PAYLOAD_BYTES.to_string()),
        "the refusal should name the threshold: {error}"
    );
    assert!(
        fixture
            .store
            .load_run_tool_calls(run.id())
            .unwrap()
            .is_empty(),
        "a refused payload must leave no row behind"
    );
}

#[test]
fn an_oversized_tool_call_output_is_refused_and_leaves_the_call_running() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let running = fixture
        .store
        .transition_tool_call(call.id(), ToolCallState::Running, at(4))
        .unwrap();

    let error = fixture
        .store
        .succeed_tool_call(
            call.id(),
            Value::String("a".repeat(MAX_INLINE_PAYLOAD_BYTES)),
            at(5),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert_eq!(fixture.store.load_tool_call(call.id()).unwrap(), running);
}

#[test]
fn an_oversized_task_title_is_refused_and_nothing_is_written() {
    let fixture = Fixture::new();
    let task = Task::new(oversized_text(), "/workspace/harkness", None, at(0));

    let error = fixture.store.insert_task(&task).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert_eq!(
        fixture.store.load_task(task.id()).unwrap_err().kind(),
        "not_found"
    );
}

#[test]
fn an_oversized_workspace_path_is_refused() {
    let fixture = Fixture::new();
    let task = Task::new(
        "Long workspace",
        format!("/{}", oversized_text()),
        None,
        at(0),
    );

    let error = fixture.store.insert_task(&task).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
}

#[test]
fn an_oversized_step_title_is_refused() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = Step::new(run.id(), 0, oversized_text(), at(2));

    let error = fixture.store.insert_step(&step).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert!(fixture.store.load_run_steps(run.id()).unwrap().is_empty());
}

#[test]
fn oversized_tool_metadata_is_refused() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);

    for call in [
        ToolCall::new(&step, oversized_text(), "1.0.0", json!({}), at(4)),
        ToolCall::new(&step, "fs.read", oversized_text(), json!({}), at(4)),
    ] {
        let error = fixture.store.insert_tool_call(&call).unwrap_err();
        assert_eq!(error.kind(), "payload_too_large");
    }
    assert!(
        fixture
            .store
            .load_run_tool_calls(run.id())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_oversized_failure_message_is_refused_and_leaves_the_run_as_it_was() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let running = fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();

    let error = fixture
        .store
        .fail_run(
            run.id(),
            Failure::new("tool_failed", oversized_text()),
            at(11),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    // The caller has to summarize and retry, so the record must still be in a
    // state a retry can transition out of.
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), running);
    let failed = fixture
        .store
        .fail_run(run.id(), Failure::new("tool_failed", "summarized"), at(12))
        .unwrap();
    assert_eq!(failed.state(), ExecutionState::Failed);
}

#[test]
fn an_oversized_approval_audit_record_is_refused_and_keeps_the_run_waiting() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();
    let waiting = fixture
        .store
        .transition_run(run.id(), ExecutionState::WaitingForApproval, at(11))
        .unwrap();

    // The approval history is the one column that grows with each write rather
    // than arriving whole, so it is the one that could outgrow the threshold
    // unnoticed.
    let error = fixture
        .store
        .approve_run(run.id(), &oversized_text(), at(12))
        .unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), waiting);
}

#[test]
fn an_accumulated_approval_history_is_bounded_without_stranding_the_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();

    // Every approval on its own fits; the history they build up does not. This
    // is the only column a caller can overflow without ever handing the store
    // an oversized value, so the refusal has to arrive at the append rather
    // than at some later transition that merely rewrites the same column.
    let decided_by = "a".repeat(MAX_INLINE_PAYLOAD_BYTES / 8);
    let mut approvals = 0;
    let refusal = loop {
        let clock = at(11 + approvals * 2);
        fixture
            .store
            .transition_run(run.id(), ExecutionState::WaitingForApproval, clock)
            .unwrap();
        match fixture
            .store
            .approve_run(run.id(), &decided_by, clock + Duration::from_secs(1))
        {
            Ok(_) => approvals += 1,
            Err(error) => break error,
        }
        assert!(approvals < 100, "the approval history never hit its bound");
    };

    assert_eq!(refusal.kind(), "payload_too_large");
    assert!(
        approvals > 1,
        "the bound should be reached by accumulation, not by one oversized value"
    );

    // The run is still where the refused approval left it, and the states that
    // do not append to the history are still reachable, so a record whose
    // history filled up can always be ended rather than being stranded awaiting
    // an approval it can never record.
    let waiting = fixture.store.load_run(run.id()).unwrap();
    assert_eq!(waiting.state(), ExecutionState::WaitingForApproval);
    assert_eq!(waiting.approvals().len(), approvals as usize);
    let cancelled = fixture
        .store
        .transition_run(run.id(), ExecutionState::Cancelled, at(500))
        .unwrap();
    assert_eq!(cancelled.state(), ExecutionState::Cancelled);
}

#[test]
fn a_row_holding_more_than_a_column_may_hold_fails_to_load() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    // Only something outside Harkness can produce this row, and reading it back
    // would import exactly the cost the threshold exists to prevent.
    guard(&fixture.store.writer)
        .execute(
            "UPDATE tasks SET title = ?2 WHERE id = ?1",
            rusqlite::params![task.id().to_string(), oversized_text()],
        )
        .unwrap();

    let error = fixture.store.load_task(task.id()).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
}

// -- lifecycle --------------------------------------------------------------

#[test]
fn a_valid_run_transition_is_persisted_with_its_lifecycle() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let running = fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();

    assert_eq!(running.state(), ExecutionState::Running);
    assert_eq!(running.started_at(), Some(at(10)));
    assert_eq!(running.revision(), run.revision() + 1);
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), running);
}

#[test]
fn an_invalid_run_transition_leaves_the_row_untouched() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let error = fixture
        .store
        .transition_run(run.id(), ExecutionState::Succeeded, at(10))
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), run);
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), run);
}

#[test]
fn an_invalid_step_transition_leaves_the_row_untouched() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);

    let error = fixture
        .store
        .transition_step(step.id(), ExecutionState::Succeeded, at(10))
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    assert_eq!(fixture.store.load_step(step.id()).unwrap(), step);
}

#[test]
fn a_failed_run_keeps_its_structured_detail() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let failure = Failure::new("tool_missing", "fs.read 1.0.0 is not registered");

    let failed = fixture
        .store
        .fail_run(run.id(), failure.clone(), at(10))
        .unwrap();

    assert_eq!(failed.state(), ExecutionState::Failed);
    assert_eq!(failed.failure(), Some(&failure));
    assert_eq!(failed.finished_at(), Some(at(10)));
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), failed);
}

#[test]
fn an_approval_decision_is_persisted_with_its_audit_record() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();
    fixture
        .store
        .transition_run(run.id(), ExecutionState::WaitingForApproval, at(11))
        .unwrap();

    let approved = fixture
        .store
        .approve_run(run.id(), "maintainer", at(12))
        .unwrap();

    assert_eq!(approved.state(), ExecutionState::Running);
    assert_eq!(approved.approvals().len(), 1);
    assert_eq!(approved.approvals()[0].decided_by(), "maintainer");
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), approved);
}

#[test]
fn a_denied_approval_fails_the_run_with_its_audit_record() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();
    fixture
        .store
        .transition_run(run.id(), ExecutionState::WaitingForApproval, at(11))
        .unwrap();

    let rejected = fixture
        .store
        .reject_run_approval(
            run.id(),
            "maintainer",
            Failure::new("denied", "the plan touches production"),
            at(12),
        )
        .unwrap();

    assert_eq!(rejected.state(), ExecutionState::Failed);
    assert_eq!(rejected.approvals().len(), 1);
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), rejected);
}

#[test]
fn a_tool_call_outcome_round_trips_with_its_output() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    fixture
        .store
        .transition_tool_call(call.id(), ToolCallState::Running, at(10))
        .unwrap();

    let succeeded = fixture
        .store
        .succeed_tool_call(call.id(), json!({"bytes": 4096}), at(11))
        .unwrap();

    assert_eq!(succeeded.state(), ToolCallState::Succeeded);
    assert_eq!(succeeded.output(), Some(&json!({"bytes": 4096})));
    assert_eq!(
        fixture.reopen().load_tool_call(call.id()).unwrap(),
        succeeded
    );
}

#[test]
fn a_denied_tool_call_records_the_policy_decision() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    let denied = fixture
        .store
        .deny_tool_call(
            call.id(),
            Failure::new("policy", "fs.write is not permitted"),
            at(10),
        )
        .unwrap();

    assert_eq!(denied.state(), ToolCallState::Denied);
    assert_eq!(fixture.reopen().load_tool_call(call.id()).unwrap(), denied);
}

#[test]
fn a_step_transition_is_persisted_independently_of_its_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);

    let running = fixture
        .store
        .transition_step(step.id(), ExecutionState::Running, at(10))
        .unwrap();

    assert_eq!(running.state(), ExecutionState::Running);
    assert_eq!(
        fixture.store.load_run(run.id()).unwrap().state(),
        ExecutionState::Queued,
        "a step transition must not move its run"
    );
}

// -- interruption groundwork -------------------------------------------------

#[test]
fn run_ownership_is_recorded_and_cleared_without_touching_the_lifecycle() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    assert_eq!(fixture.store.run_owner(run.id()).unwrap(), None);

    fixture
        .store
        .set_run_owner(run.id(), Some(std::process::id()))
        .unwrap();
    assert_eq!(
        fixture.reopen().run_owner(run.id()).unwrap(),
        Some(std::process::id())
    );

    fixture.store.set_run_owner(run.id(), None).unwrap();
    assert_eq!(fixture.store.run_owner(run.id()).unwrap(), None);
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), run);
}

// -- listing ----------------------------------------------------------------

#[test]
fn run_listing_pages_by_keyset_and_survives_inserts_at_the_tip() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let ordered = (0..5)
        .map(|index| queued_run(&fixture.store, &task, 10 - index))
        .collect::<Vec<_>>();

    let first = fixture.store.list_runs(RunPage::new(2)).unwrap();
    assert_eq!(ids(&first.runs), ids(&ordered[..2]));
    let cursor = first.next_cursor.expect("a continuation should exist");
    assert_eq!(cursor.anchor(), ordered[2].id());

    // A run created after the first page is newer than every row the cursor
    // addresses, so the continuation must neither return it nor lose its place.
    let newest = queued_run(&fixture.store, &task, 20);

    let second = fixture.store.list_runs(RunPage::after(cursor, 2)).unwrap();
    assert_eq!(ids(&second.runs), ids(&ordered[2..4]));

    let third = fixture
        .store
        .list_runs(RunPage::after(second.next_cursor.unwrap(), 2))
        .unwrap();
    assert_eq!(ids(&third.runs), ids(&ordered[4..]));
    assert!(
        third.next_cursor.is_none(),
        "the last page should not offer a continuation"
    );

    let restarted = fixture.store.list_runs(RunPage::new(1)).unwrap();
    assert_eq!(ids(&restarted.runs), vec![newest.id()]);
}

#[test]
fn runs_created_in_the_same_instant_still_page_without_repeating() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let mut created = (0..4)
        .map(|_| queued_run(&fixture.store, &task, 7))
        .map(|run| run.id())
        .collect::<Vec<_>>();
    // Ties break on the identifier, descending, exactly as the index orders them.
    created.sort_unstable_by_key(|id| std::cmp::Reverse(id.to_string()));

    let mut seen = Vec::new();
    let mut page = fixture.store.list_runs(RunPage::new(1)).unwrap();
    loop {
        seen.extend(ids(&page.runs));
        let Some(cursor) = page.next_cursor else {
            break;
        };
        page = fixture.store.list_runs(RunPage::after(cursor, 1)).unwrap();
    }

    assert_eq!(seen, created);
}

#[test]
fn an_empty_history_lists_no_runs_and_offers_no_continuation() {
    let fixture = Fixture::new();

    let page = fixture.store.list_runs(RunPage::default()).unwrap();

    assert!(page.runs.is_empty());
    assert!(page.next_cursor.is_none());
}

#[test]
fn a_page_outside_the_supported_range_is_refused() {
    let fixture = Fixture::new();

    for limit in [0, super::MAX_RUN_PAGE_LIMIT + 1] {
        let error = fixture.store.list_runs(RunPage::new(limit)).unwrap_err();
        assert_eq!(error.kind(), "invalid_page_limit");
    }
}

#[test]
fn a_cursor_token_spelled_with_an_offset_continues_from_the_same_place() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let runs = (1..=5)
        .map(|offset| queued_run(&fixture.store, &task, offset))
        .collect::<Vec<_>>();

    let first = fixture.store.list_runs(RunPage::new(2)).unwrap();
    let cursor = first.next_cursor.expect("a continuation should exist");
    let canonical = fixture.store.list_runs(RunPage::after(cursor, 2)).unwrap();

    // A front end serializes this token into its own transport and hands it
    // back. Coming home spelled with an offset must not move the position it
    // names: re-encoding the offset's own clock reading under a literal `Z`
    // would silently skip every run in between.
    let shifted: RunCursor = serde_json::from_str(&token_spelled_with_an_offset(&cursor)).unwrap();
    let continued = fixture.store.list_runs(RunPage::after(shifted, 2)).unwrap();

    assert_eq!(ids(&continued.runs), ids(&canonical.runs));
    assert_eq!(
        ids(&continued.runs),
        vec![runs[2].id(), runs[1].id()],
        "the continued page should be the third and fourth newest runs"
    );
}

/// Re-spells a cursor's timestamp with a `-05:00` offset naming the same instant.
fn token_spelled_with_an_offset(cursor: &RunCursor) -> String {
    let mut token = serde_json::to_value(cursor).unwrap();
    let canonical = token["created_at"].as_str().unwrap();
    let shifted = OffsetDateTime::parse(canonical, &Rfc3339)
        .unwrap()
        .to_offset(UtcOffset::from_hms(-5, 0, 0).unwrap());
    token["created_at"] = json!(shifted.format(&Rfc3339).unwrap());
    token.to_string()
}

fn ids(runs: &[Run]) -> Vec<RunId> {
    runs.iter().map(Run::id).collect()
}

// -- stored rows are re-validated -------------------------------------------

#[test]
fn a_row_holding_an_unknown_state_spelling_fails_to_load() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    guard(&fixture.store.writer)
        .execute(
            "UPDATE runs SET state = 'Running' WHERE id = ?1",
            [run.id().to_string()],
        )
        .unwrap();

    let error = fixture.store.load_run(run.id()).unwrap_err();

    assert_eq!(error.kind(), "column_encoding");
}

#[test]
fn a_row_breaking_a_lifecycle_rule_fails_to_load() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    // Terminal without a finish time: a combination the domain forbids.
    guard(&fixture.store.writer)
        .execute(
            "UPDATE runs SET state = 'succeeded' WHERE id = ?1",
            [run.id().to_string()],
        )
        .unwrap();

    let error = fixture.store.load_run(run.id()).unwrap_err();

    assert_eq!(error.kind(), "invalid_record");
}

#[test]
fn a_row_holding_a_noncanonical_timestamp_fails_to_load() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    // A real instant, spelled a way that sorts differently from the canonical
    // form. Accepting it would not fail; the recency index would just stop
    // agreeing with chronological order.
    guard(&fixture.store.writer)
        .execute(
            "UPDATE runs SET created_at = '2026-08-10T12:34:56Z' WHERE id = ?1",
            [run.id().to_string()],
        )
        .unwrap();

    let error = fixture.store.load_run(run.id()).unwrap_err();

    assert_eq!(error.kind(), "column_encoding");
    assert!(
        error.to_string().contains("YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ"),
        "the refusal should name the spelling the store writes: {error}"
    );
}

#[test]
fn a_row_from_a_newer_record_schema_fails_to_load_as_an_upgrade() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);

    guard(&fixture.store.writer)
        .execute(
            "UPDATE tasks SET schema_version = 99 WHERE id = ?1",
            [task.id().to_string()],
        )
        .unwrap();

    let error = fixture.store.load_task(task.id()).unwrap_err();

    assert_eq!(error.kind(), "invalid_record");
    assert!(
        error.to_string().contains("upgrade Harkness"),
        "the refusal should read as an upgrade request: {error}"
    );
}

#[test]
fn a_future_row_is_an_upgrade_request_even_when_its_body_is_unreadable() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    // The reason a version is worth recording is that a future build may spell
    // a column in a way this one cannot parse. That row has to read as "upgrade
    // Harkness", not as a corrupt column, so the version is probed before
    // anything else in the row is decoded.
    guard(&fixture.store.writer)
        .execute(
            "UPDATE tasks SET schema_version = 99, created_at = 'a future spelling' \
             WHERE id = ?1",
            [task.id().to_string()],
        )
        .unwrap();

    let error = fixture.store.load_task(task.id()).unwrap_err();

    assert_eq!(error.kind(), "invalid_record");
    assert!(
        error.to_string().contains("upgrade Harkness"),
        "an undecodable future row should still read as an upgrade request: {error}"
    );
}

// -- concurrency ------------------------------------------------------------

#[test]
fn concurrent_writers_serialize_through_the_store() {
    const PER_THREAD: i64 = 25;

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let store = Arc::new(fixture.reopen());

    let writers = (0..2)
        .map(|writer| {
            let store = Arc::clone(&store);
            let task_id = task.id();
            thread::spawn(move || {
                for index in 0..PER_THREAD {
                    let run = Run::new(task_id, at(writer * PER_THREAD + index));
                    store.insert_run(&run).unwrap();
                    store
                        .transition_run(run.id(), ExecutionState::Running, at(100 + index))
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }

    let stored = store
        .list_runs(RunPage::new(super::MAX_RUN_PAGE_LIMIT))
        .unwrap();
    assert_eq!(stored.runs.len(), usize::try_from(PER_THREAD * 2).unwrap());
    assert!(
        stored
            .runs
            .iter()
            .all(|run| run.state() == ExecutionState::Running),
        "every concurrent transition should have been recorded"
    );
}

#[test]
fn a_reader_sees_a_write_committed_by_the_writer_connection() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);

    // Warm a pooled reader first, so the read below reuses a connection that
    // was opened before the run existed.
    assert!(
        fixture
            .store
            .list_runs(RunPage::new(1))
            .unwrap()
            .runs
            .is_empty()
    );
    let run = stored_run(&fixture.store, &task);

    assert_eq!(
        ids(&fixture.store.list_runs(RunPage::new(1)).unwrap().runs),
        vec![run.id()]
    );
}

// -- the default data directory ---------------------------------------------

const OPEN_DEFAULT_CHILD: &str = "store::tests::the_default_store_lands_in_the_overridden_data_dir";
const CHILD_DATA_DIR_ENV: &str = "HARKNESS_STORE_TEST_DATA_DIR";

/// Re-entered as a child process so the environment override can be set for a
/// whole process instead of racing every other test in this binary.
#[test]
#[ignore = "only run as a child process by the data directory override test"]
fn the_default_store_lands_in_the_overridden_data_dir() {
    let data_dir = std::env::var_os(CHILD_DATA_DIR_ENV)
        .map(std::path::PathBuf::from)
        .expect("child data directory was not set");

    let store = Store::open_default().unwrap();

    assert_eq!(store.path(), data_dir.join(DATABASE_FILE));
    assert!(store.list_runs(RunPage::new(1)).unwrap().runs.is_empty());
}

#[test]
fn the_data_directory_override_redirects_the_default_store() {
    let data_dir = TempDir::new().unwrap();

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(OPEN_DEFAULT_CHILD)
        .arg("--ignored")
        .env("HARKNESS_DATA_DIR", data_dir.path())
        .env(CHILD_DATA_DIR_ENV, data_dir.path())
        .status()
        .unwrap();

    assert!(status.success(), "the child process failed: {status}");
    assert!(
        data_dir.path().join(DATABASE_FILE).is_file(),
        "the default store did not follow the override"
    );
}

// -- migration from a frozen database ---------------------------------------

/// A later migration, standing in for the ones #88 and #92 will add.
const LATER_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: include_str!("migrations/001_initial_schema.sql"),
    },
    Migration {
        version: 2,
        statements: "ALTER TABLE runs ADD COLUMN cancelled_by TEXT;",
    },
];

fn restore_frozen_database() -> TempDir {
    let data_dir = TempDir::new().unwrap();
    std::fs::write(data_dir.path().join(DATABASE_FILE), FROZEN_V1_DATABASE).unwrap();
    data_dir
}

#[test]
fn a_frozen_v1_database_opens_and_reads_back() {
    let data_dir = restore_frozen_database();

    let store = Store::open(data_dir.path()).unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        1,
        "opening must not invent a migration"
    );
    let run = store
        .load_run(RunId::from_str(FIXTURE_RUN_ID).unwrap())
        .unwrap();
    assert_eq!(run.state(), ExecutionState::Running);
    assert_eq!(
        store
            .load_task(TaskId::from_str(FIXTURE_TASK_ID).unwrap())
            .unwrap()
            .title(),
        "Add the run store"
    );
    assert_eq!(
        store
            .load_tool_call(ToolCallId::from_str(FIXTURE_TOOL_CALL_ID).unwrap())
            .unwrap()
            .tool_id(),
        "fs.read"
    );
}

#[test]
fn a_frozen_v1_database_opens_after_future_migrations() {
    let data_dir = restore_frozen_database();
    let path = data_dir.path().join(DATABASE_FILE);
    let mut connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();

    apply(&mut connection, LATER_MIGRATIONS).unwrap();

    assert_eq!(recorded_version(&connection).unwrap(), 2);
    let run =
        super::repository::load_run(&connection, RunId::from_str(FIXTURE_RUN_ID).unwrap()).unwrap();
    assert_eq!(run.state(), ExecutionState::Running);
    assert_eq!(
        super::repository::load_run_steps(&connection, run.id())
            .unwrap()
            .len(),
        1,
        "a forward migration must preserve the rows it inherits"
    );
}

/// Rewrites the frozen v1 fixture from the current migration 1.
///
/// Run deliberately, and only when migration 1 itself changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v1_fixture -- --ignored`.
/// `VACUUM INTO` writes a compact rollback-journal database, so the committed
/// file needs no write-ahead log beside it.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 1 changes"]
fn regenerate_the_frozen_v1_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = Store::open(data_dir.path()).unwrap();
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);
    store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();
    store
        .transition_step(step.id(), ExecutionState::Running, at(11))
        .unwrap();
    store
        .transition_tool_call(call.id(), ToolCallState::Running, at(12))
        .unwrap();

    let destination =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/fixtures/runtime-v1.db");
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&destination);
    guard(&store.writer)
        .execute("VACUUM INTO ?1", [destination.to_str().unwrap()])
        .unwrap();
}

#[test]
fn every_migration_in_this_build_is_recorded_in_order() {
    assert_eq!(MIGRATIONS.len(), 1, "add coverage for a new migration");
    assert_eq!(SCHEMA_VERSION, 1);
}

// -- performance -------------------------------------------------------------

/// Latency targets are meaningful only in a release build, so debug and CI runs
/// skip them; run with `--release ... -- --ignored` to record numbers.
#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn persisting_a_state_change_batch_meets_the_latency_target() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    let started = std::time::Instant::now();
    fixture
        .store
        .transition_run(run.id(), ExecutionState::Running, at(10))
        .unwrap();
    fixture
        .store
        .transition_step(step.id(), ExecutionState::Running, at(11))
        .unwrap();
    fixture
        .store
        .transition_tool_call(call.id(), ToolCallState::Running, at(12))
        .unwrap();
    fixture
        .store
        .succeed_tool_call(call.id(), json!({"bytes": 4096}), at(13))
        .unwrap();
    let elapsed = started.elapsed();

    println!("persisting an ordinary state-change batch took {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(10),
        "persisting a state-change batch took {elapsed:?}"
    );
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn listing_one_hundred_runs_meets_the_latency_target() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    for index in 0..1_000 {
        queued_run(&fixture.store, &task, index);
    }

    let started = std::time::Instant::now();
    let page = fixture.store.list_runs(RunPage::new(100)).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(page.runs.len(), 100);
    println!("listing 100 of 1000 runs took {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "listing 100 runs took {elapsed:?}"
    );
}
