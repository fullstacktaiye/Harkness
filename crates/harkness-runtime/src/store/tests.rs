//! Behavioral coverage for the durable run store.
//!
//! Every test opens its own database under a temporary directory, so nothing
//! here can read or write the real Harkness data directory.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::domain::{
    ArtifactId, ExecutionState, Failure, Run, RunId, Step, StepId, Task, TaskId, ToolCall,
    ToolCallId, ToolCallState, ToolCallWire,
};
use crate::tool::ArtifactWriter;

use super::artifact::artifact_path;
use super::migration::{MIGRATIONS, Migration, SCHEMA_VERSION, apply, recorded_version};
use super::redaction::tests::{MASK, Masking, SECRET, Shouting};
use super::{
    ARTIFACTS_DIRECTORY, Artifact, Availability, DATABASE_FILE, EventKind, EventSeq,
    MAX_EVENT_PAGE_LIMIT, MAX_INLINE_PAYLOAD_BYTES, Redactor, RunCursor, RunEvent, RunPage, Store,
    StoreArtifacts, guard,
};

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

    /// A store whose every recorded byte passes through `redactor`.
    fn redacting(redactor: Arc<dyn Redactor>) -> Self {
        let data_dir = TempDir::new().unwrap();
        let store = Store::open(data_dir.path()).unwrap().redacting(redactor);
        Self { data_dir, store }
    }

    fn reopen(&self) -> Store {
        Store::open(self.data_dir.path()).unwrap()
    }
}

/// The digest an artifact of `content` should carry.
fn sha256_hex(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn stored_artifact(store: &Store, run_id: RunId, name: &str, content: &[u8]) -> Artifact {
    let mut sink = store
        .create_artifact(run_id, name, "text/plain", at(20))
        .unwrap();
    sink.write_all(content).unwrap();
    sink.finish().unwrap()
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

// -- events -----------------------------------------------------------------

#[test]
fn an_event_round_trips_with_its_associations() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let event = RunEvent::new(EventKind::ToolProgress, at(20))
        .for_step(step.id())
        .for_tool_call(call.id())
        .with_payload(json!({"completed": 3, "total": 4}));

    let seq = fixture.store.append_event(run.id(), event.clone()).unwrap();

    assert_eq!(seq, EventSeq::FIRST);
    let stored = fixture.reopen().events(run.id(), None, 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].run_id, run.id());
    assert_eq!(stored[0].seq, seq);
    assert_eq!(stored[0].event, event);
}

#[test]
fn event_sequences_are_monotonic_per_run_under_concurrent_appends() {
    const PER_THREAD: u64 = 25;

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let other = queued_run(&fixture.store, &task, 5);
    let store = Arc::new(fixture.reopen());

    let appenders = (0..2)
        .map(|appender| {
            let store = Arc::clone(&store);
            let run_id = run.id();
            let other_id = other.id();
            thread::spawn(move || {
                let mut taken = Vec::new();
                for index in 0..PER_THREAD {
                    taken.push(
                        store
                            .append_event(
                                run_id,
                                RunEvent::new(EventKind::Diagnostic, at(30))
                                    .with_payload(json!({"appender": appender, "index": index})),
                            )
                            .unwrap(),
                    );
                    // A second run appending at the same time proves the counter
                    // is per run rather than global.
                    store
                        .append_event(other_id, RunEvent::new(EventKind::Diagnostic, at(30)))
                        .unwrap();
                }
                taken
            })
        })
        .collect::<Vec<_>>();

    let mut allocated = appenders
        .into_iter()
        .flat_map(|appender| appender.join().unwrap())
        .map(EventSeq::get)
        .collect::<Vec<_>>();
    allocated.sort_unstable();

    assert_eq!(
        allocated,
        (1..=PER_THREAD * 2).collect::<Vec<_>>(),
        "two appenders must never be handed the same number"
    );
    let stored = store.events(run.id(), None, MAX_EVENT_PAGE_LIMIT).unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|event| event.seq.get())
            .collect::<Vec<_>>(),
        (1..=PER_THREAD * 2).collect::<Vec<_>>()
    );
    assert_eq!(
        store
            .events(other.id(), None, MAX_EVENT_PAGE_LIMIT)
            .unwrap()[0]
            .seq,
        EventSeq::FIRST,
        "each run counts from one"
    );
}

#[test]
fn a_state_change_and_its_event_commit_atomically_or_not_at_all() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    // An event naming a step that was never stored cannot be inserted, so the
    // transition sharing its transaction must not survive either.
    let error = fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10)).for_step(StepId::new()),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), run);
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), run);
    assert!(
        fixture.store.events(run.id(), None, 10).unwrap().is_empty(),
        "a rolled-back transition must leave no event behind"
    );

    // And the successful pairing writes both.
    let (running, seq) = fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10))
                .with_payload(json!({"from": "queued", "to": "running"})),
        )
        .unwrap();
    assert_eq!(running.state(), ExecutionState::Running);
    assert_eq!(seq, EventSeq::FIRST);
    let reopened = fixture.reopen();
    assert_eq!(reopened.load_run(run.id()).unwrap(), running);
    assert_eq!(reopened.events(run.id(), None, 10).unwrap().len(), 1);
}

#[test]
fn an_invalid_transition_writes_no_event() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let error = fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Succeeded,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10)),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), run);
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
}

#[test]
fn a_tool_call_transition_and_its_event_commit_together() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    let (running, seq) = fixture
        .store
        .transition_tool_call_with_event(
            call.id(),
            ToolCallState::Running,
            at(10),
            RunEvent::new(EventKind::ToolCallStateChanged, at(10))
                .for_step(step.id())
                .for_tool_call(call.id())
                .with_payload(json!({"to": "running"})),
        )
        .unwrap();

    assert_eq!(running.state(), ToolCallState::Running);
    let stored = fixture.reopen().events(run.id(), None, 10).unwrap();
    assert_eq!(stored[0].seq, seq);
    assert_eq!(stored[0].event.tool_call_id(), Some(call.id()));
}

#[test]
fn oversized_event_payloads_become_artifacts_with_a_reference() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let full = json!({"stderr": "a".repeat(MAX_INLINE_PAYLOAD_BYTES)});

    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(20))
                .for_step(step.id())
                .with_payload(full.clone()),
        )
        .unwrap();

    let stored = fixture.reopen().events(run.id(), None, 10).unwrap();
    let event = &stored[0].event;
    let reference = event
        .overflowed_payload()
        .expect("an oversized payload should have been spilled");
    assert_eq!(
        event.artifact_id(),
        Some(reference.id),
        "the column should point at the spilled artifact when the caller named none"
    );
    assert_eq!(reference.media_type, "application/json");
    assert!(
        serde_json::to_string(event.payload()).unwrap().len() < MAX_INLINE_PAYLOAD_BYTES,
        "the inline payload must be under the threshold"
    );

    // The full bytes round-trip through the artifact, which is the whole point
    // of spilling rather than refusing.
    let bytes = fixture.store.read_artifact(reference.id).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), full);
    let artifact = fixture.store.artifact(reference.id).unwrap();
    assert_eq!(artifact.byte_size(), reference.byte_size);
    assert_eq!(artifact.step_id(), Some(step.id()));
}

#[test]
fn an_overflow_does_not_overwrite_an_artifact_the_caller_named() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let mut sink = fixture
        .store
        .create_artifact(run.id(), "build.log", "text/plain", at(19))
        .unwrap();
    sink.write_all(b"the log the caller cared about").unwrap();
    let named = sink.finish().unwrap();

    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::ArtifactCreated, at(20))
                .for_artifact(named.id())
                .with_payload(json!({"stderr": "a".repeat(MAX_INLINE_PAYLOAD_BYTES)})),
        )
        .unwrap();

    let stored = fixture.store.events(run.id(), None, 10).unwrap();
    let event = &stored[0].event;
    assert_eq!(
        event.artifact_id(),
        Some(named.id()),
        "the caller's reference must survive the spill"
    );
    assert_ne!(
        event.overflowed_payload().unwrap().id,
        named.id(),
        "the payload should have gone to its own artifact"
    );
}

#[test]
fn an_unknown_event_kind_loads_as_opaque() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    fixture
        .store
        .append_event(run.id(), RunEvent::new(EventKind::Diagnostic, at(20)))
        .unwrap();
    // A kind a later build defines. An older binary must render it, not refuse
    // the run it belongs to.
    guard(&fixture.store.writer)
        .execute(
            "UPDATE run_events SET kind = 'sandbox_escaped' WHERE run_id = ?1",
            [run.id().to_string()],
        )
        .unwrap();

    let stored = fixture.store.events(run.id(), None, 10).unwrap();

    assert_eq!(
        stored[0].event.kind(),
        &EventKind::Unrecognized("sandbox_escaped".to_owned())
    );
    assert!(!stored[0].event.kind().is_recognized());
    assert_eq!(stored[0].event.kind().to_string(), "sandbox_escaped");
}

#[test]
fn event_pages_by_sequence_survive_concurrent_appends() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    for index in 0..5 {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, at(20 + index))
                    .with_payload(json!({"index": index})),
            )
            .unwrap();
    }

    let mut seen = Vec::new();
    let mut after = None;
    let mut appended = 0;
    loop {
        let page = fixture.store.events(run.id(), after, 2).unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(page[page.len() - 1].seq);
        seen.extend(page.iter().map(|stored| stored.seq.get()));

        // An event arriving mid-paging is newer than everything the cursor
        // addresses, so it must appear once at the end and never displace a row
        // of a page already returned. The appends stop so the loop can drain;
        // a log that never stops growing is #100's problem, not paging's.
        if appended < 3 {
            fixture
                .store
                .append_event(run.id(), RunEvent::new(EventKind::Diagnostic, at(40)))
                .unwrap();
            appended += 1;
        }
    }

    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique, seen, "a page repeated an event");
    assert_eq!(seen, (1..=5 + appended).collect::<Vec<_>>());
}

#[test]
fn an_event_page_outside_the_supported_range_is_refused() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    for limit in [0, MAX_EVENT_PAGE_LIMIT + 1] {
        let error = fixture.store.events(run.id(), None, limit).unwrap_err();
        assert_eq!(error.kind(), "invalid_page_limit");
    }
}

#[test]
fn an_event_against_an_unstored_run_is_refused() {
    let fixture = Fixture::new();

    let error = fixture
        .store
        .append_event(RunId::new(), RunEvent::new(EventKind::Diagnostic, at(20)))
        .unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
}

// -- artifacts ---------------------------------------------------------------

#[test]
fn artifact_hashes_and_sizes_match_streamed_content() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let content = b"diff --git a/one b/one\n@@ -1 +1 @@\n";

    let mut sink = fixture
        .store
        .create_artifact(run.id(), "change.patch", "text/x-diff", at(20))
        .unwrap();
    let id = sink.id();
    for chunk in content.chunks(7) {
        sink.write_all(chunk).unwrap();
    }
    let artifact = sink.finish().unwrap();

    assert_eq!(artifact.id(), id, "the identity is known before the bytes");
    assert_eq!(artifact.byte_size(), content.len() as u64);
    assert_eq!(artifact.sha256(), sha256_hex(content));
    assert_eq!(artifact.availability(), Availability::Available);
    assert_eq!(artifact.media_type(), "text/x-diff");
    assert_eq!(fixture.reopen().read_artifact(id).unwrap(), content);
    assert_eq!(fixture.store.artifact(id).unwrap(), artifact);
    assert_eq!(fixture.store.run_artifacts(run.id()).unwrap(), [artifact]);
}

#[test]
fn an_artifact_streams_without_accumulating_its_content() {
    // The structural guarantee is that `ArtifactSink` is a `Write` and nothing
    // else: there is no method taking a whole artifact, so no amount of content
    // can be held anywhere but the fixed buffer between the sink and the file.
    // This exercises that path with more content than any buffer in it.
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 128;

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let chunk = vec![b'x'; CHUNK];

    let mut sink = fixture
        .store
        .create_artifact(run.id(), "build.log", "text/plain", at(20))
        .unwrap();
    for _ in 0..CHUNKS {
        sink.write_all(&chunk).unwrap();
    }
    let artifact = sink.finish().unwrap();

    assert_eq!(artifact.byte_size(), (CHUNK * CHUNKS) as u64);
    assert_eq!(
        artifact.sha256(),
        sha256_hex(&vec![b'x'; CHUNK * CHUNKS]),
        "the recorded digest must describe the whole stream"
    );
    assert_eq!(artifact.availability(), Availability::Available);
}

#[test]
fn artifact_finalize_is_write_sync_rename_then_row() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let mut sink = fixture
        .store
        .create_artifact(run.id(), "notes.txt", "text/plain", at(20))
        .unwrap();
    let id = sink.id();
    sink.write_all(b"durable before it is recorded").unwrap();

    // Stand exactly where a crash between the two phases would.
    let sealed = sink.seal().unwrap();

    let path = artifact_path(fixture.data_dir.path(), run.id(), id);
    assert!(path.is_file(), "the bytes should already be durable");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"durable before it is recorded"
    );
    assert!(
        !path.with_file_name(format!(".tmp-{id}")).exists(),
        "the temporary name should be gone"
    );
    assert_eq!(
        fixture.store.artifact(id).unwrap_err().kind(),
        "not_found",
        "an orphan file must leave no dangling row"
    );
    // The store is entirely readable across that crash point.
    assert_eq!(fixture.reopen().load_run(run.id()).unwrap(), run);
    assert!(fixture.store.run_artifacts(run.id()).unwrap().is_empty());

    // Completing the second phase is all that is left.
    let artifact = fixture
        .store
        .in_write_transaction("recording an artifact", |connection| {
            super::artifact::insert_artifact(connection, &sealed)
        })
        .unwrap();
    assert_eq!(fixture.store.artifact(id).unwrap(), artifact);
}

#[test]
fn an_abandoned_artifact_leaves_no_row_and_no_file() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let id = {
        let mut sink = fixture
            .store
            .create_artifact(run.id(), "abandoned.log", "text/plain", at(20))
            .unwrap();
        sink.write_all(b"never finished").unwrap();
        sink.id()
    };

    assert!(fixture.store.run_artifacts(run.id()).unwrap().is_empty());
    let directory = fixture
        .data_dir
        .path()
        .join(ARTIFACTS_DIRECTORY)
        .join(run.id().to_string());
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        0,
        "an abandoned write should leave nothing behind"
    );
    assert!(!artifact_path(fixture.data_dir.path(), run.id(), id).exists());
}

#[test]
fn a_seal_that_fails_partway_still_removes_its_temporary_file() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    let mut sink = fixture
        .store
        .create_artifact(run.id(), "notes.txt", "text/plain", at(20))
        .unwrap();
    let id = sink.id();
    sink.write_all(b"never reaches its final name").unwrap();
    let staged = sink.temporary().to_path_buf();
    // A directory where the artifact wants to land, so the rename inside `seal`
    // fails after the stream has already been taken.
    std::fs::create_dir(artifact_path(fixture.data_dir.path(), run.id(), id)).unwrap();

    let error = sink.finish().unwrap_err();

    assert_eq!(error.kind(), "artifact_io");
    assert!(
        !staged.exists(),
        "a seal that gave up must not strand its temporary file at {}",
        staged.display()
    );
    assert_eq!(fixture.store.artifact(id).unwrap_err().kind(), "not_found");
}

#[test]
fn a_missing_artifact_file_degrades_to_availability_missing() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let kept = stored_artifact(&fixture.store, run.id(), "kept.txt", b"still here");
    let removed = stored_artifact(&fixture.store, run.id(), "gone.txt", b"deleted later");

    std::fs::remove_file(artifact_path(
        fixture.data_dir.path(),
        run.id(),
        removed.id(),
    ))
    .unwrap();

    let store = fixture.reopen();
    assert_eq!(
        store.artifact(removed.id()).unwrap().availability(),
        Availability::Missing
    );
    assert_eq!(
        store.artifact(kept.id()).unwrap().availability(),
        Availability::Available,
        "one artifact's absence must not change another's"
    );
    // Everything that does not need the bytes still works.
    assert_eq!(store.load_run(run.id()).unwrap(), run);
    assert!(store.events(run.id(), None, 10).unwrap().is_empty());
    assert_eq!(store.run_artifacts(run.id()).unwrap().len(), 2);
    assert_eq!(
        store.read_artifact(removed.id()).unwrap_err().kind(),
        "artifact_io"
    );
}

#[test]
fn an_artifact_whose_bytes_changed_is_reported_as_a_size_mismatch() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let artifact = stored_artifact(&fixture.store, run.id(), "notes.txt", b"original");

    std::fs::write(
        artifact_path(fixture.data_dir.path(), run.id(), artifact.id()),
        b"rewritten from outside",
    )
    .unwrap();

    assert_eq!(
        fixture
            .store
            .artifact(artifact.id())
            .unwrap()
            .availability(),
        Availability::SizeMismatch
    );
}

#[test]
fn a_storage_path_outside_the_artifacts_directory_is_refused_on_read() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let tampered = stored_artifact(&fixture.store, run.id(), "notes.txt", b"content");
    let intact = stored_artifact(&fixture.store, run.id(), "other.txt", b"content");
    guard(&fixture.store.writer)
        .execute(
            "UPDATE artifacts SET storage_path = '../../.ssh/id_rsa' WHERE id = ?1",
            [tampered.id().to_string()],
        )
        .unwrap();

    let store = fixture.reopen();
    for error in [
        store.artifact(tampered.id()).unwrap_err(),
        store.read_artifact(tampered.id()).unwrap_err(),
        store.open_artifact(tampered.id()).unwrap_err(),
    ] {
        assert_eq!(error.kind(), "forbidden_artifact_path");
        assert!(
            error.to_string().contains("../../.ssh/id_rsa"),
            "the refusal should name the path it declined: {error}"
        );
    }

    // The run itself is untouched by one tampered row, and every other artifact
    // still reads on its own.
    assert_eq!(store.load_run(run.id()).unwrap(), run);
    assert!(store.events(run.id(), None, 10).unwrap().is_empty());
    assert_eq!(store.artifact(intact.id()).unwrap(), intact);

    // A *listing* does fail, deliberately: dropping the bad row would hand back
    // a list the caller reads as complete when it is not.
    assert_eq!(
        store.run_artifacts(run.id()).unwrap_err().kind(),
        "forbidden_artifact_path"
    );
}

#[test]
fn an_artifact_against_an_unstored_run_is_refused_before_a_file_exists() {
    let fixture = Fixture::new();
    let unstored = RunId::new();

    let error = fixture
        .store
        .create_artifact(unstored, "notes.txt", "text/plain", at(20))
        .unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(
        !fixture
            .data_dir
            .path()
            .join(ARTIFACTS_DIRECTORY)
            .join(unstored.to_string())
            .exists(),
        "a refused artifact must not create its directory"
    );
}

#[test]
fn oversized_artifact_metadata_is_refused_before_anything_is_streamed() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    for (name, media_type) in [
        (oversized_text(), "text/plain".to_owned()),
        ("notes.txt".to_owned(), oversized_text()),
    ] {
        let error = fixture
            .store
            .create_artifact(run.id(), &name, &media_type, at(20))
            .unwrap_err();
        assert_eq!(error.kind(), "payload_too_large");
    }
    assert!(fixture.store.run_artifacts(run.id()).unwrap().is_empty());
}

#[test]
fn a_tool_artifact_writer_records_against_its_call() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let store = Arc::new(fixture.reopen());
    let mut artifacts = StoreArtifacts::new(Arc::clone(&store), run.id(), step.id(), call.id());

    let reference = artifacts
        .write("build.log", "text/plain", b"compiling harkness")
        .unwrap();

    let id = ArtifactId::from_str(&reference.id).unwrap();
    let artifact = store.artifact(id).unwrap();
    assert_eq!(artifact.name(), "build.log");
    assert_eq!(artifact.step_id(), Some(step.id()));
    assert_eq!(artifact.tool_call_id(), Some(call.id()));
    assert_eq!(reference.byte_len, 18);
    assert_eq!(store.read_artifact(id).unwrap(), b"compiling harkness");
}

#[test]
fn a_tool_artifact_failure_is_reported_rather_than_half_stored() {
    let fixture = Fixture::new();
    let store = Arc::new(fixture.reopen());
    // A run nobody stored: the writer must refuse rather than hand the tool a
    // reference to something that was never recorded.
    let mut artifacts = StoreArtifacts::new(store, RunId::new(), StepId::new(), ToolCallId::new());

    let error = artifacts
        .write("build.log", "text/plain", b"output")
        .unwrap_err();

    assert_eq!(error.kind(), "execution_failed");
    assert!(error.to_string().contains("build.log"), "{error}");
}

#[cfg(unix)]
#[test]
fn artifact_files_are_readable_only_by_their_owner() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let artifact = stored_artifact(&fixture.store, run.id(), "secret.log", b"a token, probably");

    let path = artifact_path(fixture.data_dir.path(), run.id(), artifact.id());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "process output may contain anything; the umask is not a strong enough claim"
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

// -- redaction ---------------------------------------------------------------

#[test]
fn every_event_payload_and_artifact_byte_passes_through_the_redactor() {
    let fixture = Fixture::redacting(Arc::new(Shouting));
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);

    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(20)).with_payload(json!({"note": "quiet"})),
        )
        .unwrap();
    fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            at(21),
            RunEvent::new(EventKind::RunStateChanged, at(21))
                .with_payload(json!({"reason": "started"})),
        )
        .unwrap();
    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(22))
                .for_step(step.id())
                .with_payload(json!({"stderr": "b".repeat(MAX_INLINE_PAYLOAD_BYTES)})),
        )
        .unwrap();
    let mut sink = fixture
        .store
        .create_artifact(run.id(), "build.log", "text/plain", at(23))
        .unwrap();
    sink.write_all(b"linking harkness").unwrap();
    let artifact = sink.finish().unwrap();

    let stored = fixture.reopen().events(run.id(), None, 10).unwrap();
    assert_eq!(stored[0].event.payload(), &json!({"note": "QUIET"}));
    assert_eq!(stored[1].event.payload(), &json!({"reason": "STARTED"}));

    // A spilled payload holds exactly what the row would have held had it fit:
    // values scrubbed, keys intact. Redacting it through the stream wrapper
    // instead would scrub it twice and rewrite its keys, so the same content
    // would come back differently depending only on its size.
    let spilled = stored[2].event.overflowed_payload().unwrap();
    let bytes = fixture.store.read_artifact(spilled.id).unwrap();
    assert_eq!(
        bytes,
        format!(r#"{{"stderr":"{}"}}"#, "B".repeat(MAX_INLINE_PAYLOAD_BYTES)).into_bytes()
    );
    assert_eq!(spilled.byte_size, bytes.len() as u64);
    assert_eq!(spilled.sha256, sha256_hex(&bytes));

    // And the size and digest describe what actually landed, not what arrived.
    let content = fixture.store.read_artifact(artifact.id()).unwrap();
    assert_eq!(content, b"LINKING HARKNESS");
    assert_eq!(artifact.byte_size(), content.len() as u64);
    assert_eq!(artifact.sha256(), sha256_hex(&content));
    assert_eq!(
        fixture
            .store
            .artifact(artifact.id())
            .unwrap()
            .availability(),
        Availability::Available,
        "the recorded size must match the redacted file, or every probe would disagree"
    );
}

#[test]
fn a_redactor_that_only_scrubs_values_still_scrubs_an_oversized_payload() {
    let fixture = Fixture::redacting(Arc::new(Masking));
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    // Still oversized after the rule has run, so it takes the spill path.
    let noisy = format!("{}{SECRET}", "-".repeat(MAX_INLINE_PAYLOAD_BYTES));

    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(20)).with_payload(json!({"stderr": noisy})),
        )
        .unwrap();

    // `Masking` leaves `wrap_stream` as the identity, which the trait permits.
    // If a spilled payload relied on the stream wrapper to scrub it, the secret
    // would be durable here purely because the payload was over the threshold.
    let stored = fixture.store.events(run.id(), None, 10).unwrap();
    let spilled = stored[0].event.overflowed_payload().unwrap();
    let bytes = fixture.store.read_artifact(spilled.id).unwrap();
    let content = String::from_utf8(bytes).unwrap();
    assert!(
        !content.contains(SECRET),
        "a value scrubbed under the threshold must stay scrubbed above it"
    );
    assert!(content.contains(MASK), "the rule should have run once");
    assert!(
        content.starts_with(r#"{"stderr":"#),
        "the spilled payload should keep its published field names: {}",
        &content[..40.min(content.len())]
    );
}

#[test]
fn a_rejected_write_leaves_no_spilled_artifact_behind() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let oversized = json!({"stderr": "a".repeat(MAX_INLINE_PAYLOAD_BYTES)});

    // An invalid transition is an ordinary refusal, not a crash, so the spill
    // written on the way in has to be cleaned up: an agent retrying would
    // otherwise leave one file per attempt in a store with no collector.
    for attempt in 0..3 {
        let error = fixture
            .store
            .transition_run_with_event(
                run.id(),
                ExecutionState::Succeeded,
                at(10 + attempt),
                RunEvent::new(EventKind::RunStateChanged, at(10 + attempt))
                    .with_payload(oversized.clone()),
            )
            .unwrap_err();
        assert_eq!(error.kind(), "invalid_transition");
    }

    assert!(fixture.store.run_artifacts(run.id()).unwrap().is_empty());
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
    let directory = fixture
        .data_dir
        .path()
        .join(ARTIFACTS_DIRECTORY)
        .join(run.id().to_string());
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        0,
        "a retried rejection must not accumulate spilled payloads"
    );
}

#[test]
fn an_event_cannot_associate_a_step_or_call_from_another_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let elsewhere = queued_run(&fixture.store, &task, 5);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    // Both records exist; neither belongs to `elsewhere`. A timeline that could
    // name another run's step is worse than a refused write, because nothing
    // downstream re-checks it — the wrong step is simply rendered.
    for event in [
        RunEvent::new(EventKind::StepStarted, at(20)).for_step(step.id()),
        RunEvent::new(EventKind::ToolProgress, at(20)).for_tool_call(call.id()),
    ] {
        let error = fixture
            .store
            .append_event(elsewhere.id(), event)
            .unwrap_err();
        assert_eq!(error.kind(), "missing_parent");
    }
    assert!(
        fixture
            .store
            .events(elsewhere.id(), None, 10)
            .unwrap()
            .is_empty()
    );

    // The same association against its own run is accepted.
    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::StepStarted, at(20)).for_step(step.id()),
        )
        .unwrap();
}

#[test]
fn an_artifact_cannot_be_attributed_to_a_step_from_another_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let elsewhere = queued_run(&fixture.store, &task, 5);
    let step = stored_step(&fixture.store, &run);

    let mut sink = fixture
        .store
        .create_artifact(elsewhere.id(), "notes.txt", "text/plain", at(20))
        .unwrap()
        .for_step(step.id());
    sink.write_all(b"attributed to the wrong run").unwrap();
    let id = sink.id();
    let error = sink.finish().unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(
        fixture
            .store
            .run_artifacts(elsewhere.id())
            .unwrap()
            .is_empty()
    );
    // A refused insert is an ordinary rejection, not the crash the orphan-file
    // trade-off is about: a tool retrying a failing artifact write must not
    // leave a full copy of its content behind each time.
    assert!(
        !artifact_path(fixture.data_dir.path(), elsewhere.id(), id).exists(),
        "a refused artifact must not leave its content behind"
    );
}

#[test]
fn a_retried_tool_artifact_write_does_not_accumulate_content() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let elsewhere = queued_run(&fixture.store, &task, 5);
    let store = Arc::new(fixture.reopen());
    // The run exists, so the content is streamed and sealed in full; the step
    // belongs to a different run, so the insert is refused afterwards. That is
    // the window where a whole build log can be left behind once per attempt.
    let mut artifacts =
        StoreArtifacts::new(Arc::clone(&store), elsewhere.id(), step.id(), call.id());

    for _ in 0..3 {
        assert_eq!(
            artifacts
                .write("build.log", "text/plain", b"compiling harkness")
                .unwrap_err()
                .kind(),
            "execution_failed"
        );
    }

    let directory = fixture
        .data_dir
        .path()
        .join(ARTIFACTS_DIRECTORY)
        .join(elsewhere.id().to_string());
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        0,
        "a retried tool artifact write accumulated content"
    );
    assert!(store.run_artifacts(elsewhere.id()).unwrap().is_empty());
}

#[test]
fn artifact_metadata_passes_through_the_redactor_like_its_content() {
    let fixture = Fixture::redacting(Arc::new(Masking));
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);

    // A label is caller text that becomes durable in a column, so a tool naming
    // its artifact after the credential it just leaked must not persist it in
    // the one place redaction does not look.
    let mut sink = fixture
        .store
        .create_artifact(
            run.id(),
            &format!("token-{SECRET}.log"),
            "text/plain",
            at(20),
        )
        .unwrap();
    sink.write_all(b"content").unwrap();
    let artifact = sink.finish().unwrap();

    assert_eq!(artifact.name(), format!("token-{MASK}.log"));
    assert!(!artifact.name().contains(SECRET));
    assert_eq!(
        fixture.store.artifact(artifact.id()).unwrap().name(),
        artifact.name()
    );

    // A store-generated value is left exactly as this module wrote it, so the
    // spilled-payload media type still reads back as its published constant.
    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(21))
                .with_payload(json!({"stderr": "-".repeat(MAX_INLINE_PAYLOAD_BYTES)})),
        )
        .unwrap();
    let spilled = fixture.store.events(run.id(), None, 10).unwrap()[0]
        .event
        .overflowed_payload()
        .unwrap();
    assert_eq!(spilled.media_type, super::OVERFLOW_PAYLOAD_MEDIA_TYPE);
    assert_eq!(
        fixture.store.artifact(spilled.id).unwrap().name(),
        super::OVERFLOW_PAYLOAD_NAME
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

/// The frozen v2 database committed beside this module.
///
/// Written by `regenerate_the_frozen_v2_fixture`. Its artifact file is
/// deliberately *not* committed beside it: an artifact whose bytes are gone is
/// exactly the case a restored database has to survive.
const FROZEN_V2_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v2.db");

/// A later migration, standing in for the one #92 will add.
const LATER_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: include_str!("migrations/001_initial_schema.sql"),
    },
    Migration {
        version: 2,
        statements: include_str!("migrations/002_events_and_artifacts.sql"),
    },
    Migration {
        version: 3,
        statements: "ALTER TABLE runs ADD COLUMN cancelled_by TEXT;",
    },
];

fn restore_frozen_database(bytes: &[u8]) -> TempDir {
    let data_dir = TempDir::new().unwrap();
    std::fs::write(data_dir.path().join(DATABASE_FILE), bytes).unwrap();
    data_dir
}

#[test]
fn a_v1_database_migrates_to_v2_and_still_reads_its_existing_runs() {
    let data_dir = restore_frozen_database(FROZEN_V1_DATABASE);

    let store = Store::open(data_dir.path()).unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION,
        "opening a v1 database should climb the ladder"
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

    // A run that predates the event log has an empty one rather than none: the
    // new tables must be usable against inherited rows, not only against rows
    // this build created.
    assert!(store.events(run.id(), None, 10).unwrap().is_empty());
    assert!(store.run_artifacts(run.id()).unwrap().is_empty());
    let seq = store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::Diagnostic, at(20)).with_payload(json!({"note": "migrated"})),
        )
        .unwrap();
    assert_eq!(seq, EventSeq::FIRST);
}

#[test]
fn a_frozen_v2_database_opens_and_reads_its_log_and_artifacts() {
    let data_dir = restore_frozen_database(FROZEN_V2_DATABASE);

    let store = Store::open(data_dir.path()).unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        2,
        "opening must not invent a migration"
    );
    let run_id = RunId::from_str(FIXTURE_RUN_ID).unwrap();
    let events = store.events(run_id, None, 10).unwrap();
    assert_eq!(
        events
            .iter()
            .map(|stored| stored.event.kind().as_str())
            .collect::<Vec<_>>(),
        ["run_state_changed", "artifact_created"]
    );
    assert_eq!(events[0].seq, EventSeq::FIRST);

    // The fixture ships without its artifact file, which is the shape a restored
    // backup has: the metadata reads, the content does not, and nothing fails.
    let artifacts = store.run_artifacts(run_id).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].availability(), Availability::Missing);
    assert_eq!(artifacts[0].name(), "notes.txt");
    assert_eq!(
        store.read_artifact(artifacts[0].id()).unwrap_err().kind(),
        "artifact_io"
    );
}

#[test]
fn a_frozen_v2_database_opens_after_future_migrations() {
    let data_dir = restore_frozen_database(FROZEN_V2_DATABASE);
    let path = data_dir.path().join(DATABASE_FILE);
    let mut connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();

    apply(&mut connection, LATER_MIGRATIONS).unwrap();

    assert_eq!(recorded_version(&connection).unwrap(), 3);
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
    assert_eq!(
        super::event::events(&connection, run.id(), None, 10)
            .unwrap()
            .len(),
        2,
        "a forward migration must preserve the event log it inherits"
    );
}

/// Rewrites the frozen v1 fixture from the current migration 1.
///
/// Run deliberately, and only when migration 1 itself changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v1_fixture -- --ignored`.
/// `VACUUM INTO` writes a compact rollback-journal database, so the committed
/// file needs no write-ahead log beside it.
///
/// It cannot use [`Store`], which now climbs to the newest schema on open: the
/// point of this fixture is a database that stopped at version 1, so the ladder
/// is truncated and the rows are written through the repository directly.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 1 changes"]
fn regenerate_the_frozen_v1_fixture() {
    let data_dir = TempDir::new().unwrap();
    let mut connection = Connection::open(data_dir.path().join(DATABASE_FILE)).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();
    apply(&mut connection, &MIGRATIONS[..1]).unwrap();

    let task = Task::with_id(
        TaskId::from_str(FIXTURE_TASK_ID).unwrap(),
        "Add the run store",
        "/workspace/harkness",
        None,
        at(0),
    );
    super::repository::insert_task(&connection, &task).unwrap();
    let mut run = Run::with_id(RunId::from_str(FIXTURE_RUN_ID).unwrap(), task.id(), at(1));
    super::repository::insert_run(&connection, &run).unwrap();
    let mut step = Step::with_id(
        StepId::from_str(FIXTURE_STEP_ID).unwrap(),
        run.id(),
        0,
        "Read the schema",
        at(2),
    );
    super::repository::insert_step(&connection, &step).unwrap();
    let mut call = ToolCall::with_id(
        ToolCallId::from_str(FIXTURE_TOOL_CALL_ID).unwrap(),
        &step,
        "fs.read",
        "1.0.0",
        json!({"path": "crates/harkness-runtime/src/store/mod.rs"}),
        at(3),
    );
    super::repository::insert_tool_call(&connection, &call).unwrap();

    run.transition(ExecutionState::Running, at(10)).unwrap();
    super::repository::update_run(&connection, &run).unwrap();
    step.transition(ExecutionState::Running, at(11)).unwrap();
    super::repository::update_step(&connection, &step).unwrap();
    call.transition(ToolCallState::Running, at(12)).unwrap();
    super::repository::update_tool_call(&connection, &call).unwrap();

    freeze(&connection, "runtime-v1.db");
}

/// Rewrites the frozen v2 fixture from the current migration ladder.
///
/// Run deliberately, and only when migration 2 itself changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v2_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 2 changes"]
fn regenerate_the_frozen_v2_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = Store::open(data_dir.path()).unwrap();
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);
    store
        .transition_step(step.id(), ExecutionState::Running, at(11))
        .unwrap();
    store
        .transition_tool_call(call.id(), ToolCallState::Running, at(12))
        .unwrap();
    store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10))
                .with_payload(json!({"from": "queued", "to": "running"})),
        )
        .unwrap();

    let mut sink = store
        .create_artifact(run.id(), "notes.txt", "text/plain", at(13))
        .unwrap()
        .for_step(step.id());
    sink.write_all(b"frozen fixture content\n").unwrap();
    let artifact = sink.finish().unwrap();
    store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::ArtifactCreated, at(13))
                .for_step(step.id())
                .for_artifact(artifact.id())
                .with_payload(json!({"name": artifact.name()})),
        )
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v2.db");
}

/// Writes a compact rollback-journal copy of `connection` into the fixtures.
fn freeze(connection: &Connection, name: &str) {
    let destination = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/store/fixtures")
        .join(name);
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&destination);
    connection
        .execute("VACUUM INTO ?1", [destination.to_str().unwrap()])
        .unwrap();
}

#[test]
fn every_migration_in_this_build_is_recorded_in_order() {
    assert_eq!(MIGRATIONS.len(), 2, "add coverage for a new migration");
    assert_eq!(SCHEMA_VERSION, 2);
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
fn persisting_a_state_change_batch_with_its_events_meets_the_latency_target() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);

    // An event rides the same transaction as the state change it describes, so
    // the pairing has to stay inside the budget the state change alone had.
    let started = std::time::Instant::now();
    fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10))
                .with_payload(json!({"to": "running"})),
        )
        .unwrap();
    fixture
        .store
        .transition_step(step.id(), ExecutionState::Running, at(11))
        .unwrap();
    fixture
        .store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::StepStarted, at(11)).for_step(step.id()),
        )
        .unwrap();
    fixture
        .store
        .transition_tool_call_with_event(
            call.id(),
            ToolCallState::Running,
            at(12),
            RunEvent::new(EventKind::ToolCallStateChanged, at(12)).for_tool_call(call.id()),
        )
        .unwrap();
    fixture
        .store
        .succeed_tool_call(call.id(), json!({"bytes": 4096}), at(13))
        .unwrap();
    let elapsed = started.elapsed();

    println!("persisting a state-change batch with its events took {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(10),
        "persisting a state-change batch with its events took {elapsed:?}"
    );
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn loading_a_thousand_event_run_meets_the_latency_target() {
    const EVENTS: usize = 1_000;

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    for index in 0..EVENTS {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::ToolProgress, at(20 + index as i64))
                    .for_step(step.id())
                    .with_payload(json!({"completed": index, "total": EVENTS})),
            )
            .unwrap();
    }
    // A fresh store, so the measurement is of loading the log rather than of
    // reading a warm page cache in the process that wrote it.
    let store = fixture.reopen();

    let started = std::time::Instant::now();
    let events = store.events(run.id(), None, MAX_EVENT_PAGE_LIMIT).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(events.len(), EVENTS);
    println!("loading a {EVENTS}-event run took {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "loading a {EVENTS}-event run took {elapsed:?}"
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
