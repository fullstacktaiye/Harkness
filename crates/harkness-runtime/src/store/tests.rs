//! Behavioral coverage for the durable run store.
//!
//! Every test opens its own database under a temporary directory, so nothing
//! here can read or write the real Harkness data directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harkness_context::{
    CONTEXT_RECORD_SCHEMA_VERSION, CaptureRequest, FilesystemProbe, SnapshotWireRef,
    WorkspaceSnapshot,
};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::approval::{
    ApprovalDecision, ApprovalGate, ApprovalObservation, ApprovalRequest, ApprovalScope,
    ApprovalState, CandidateCall, DecidedVia, PendingApproval, WorkspaceBinding,
    canonical_input_hash, matching_grants, matching_grants_detailed,
};
use crate::domain::{
    ArtifactId, ExecutionState, Failure, Run, RunId, Step, StepId, Task, TaskId, ToolCall,
    ToolCallId, ToolCallState, ToolCallWire,
};
use crate::integration::{IntegrationIdentity, Sha256Hash};
use crate::policy::{PolicyDecision, PolicyVerdict};
use crate::tool::{ArtifactWriter, Capability, RiskLevel, ToolIdentity};
use crate::trust::{TrustState, WorkspaceTrust};

use super::artifact::artifact_path;
use super::migration::{MIGRATIONS, Migration, SCHEMA_VERSION, apply, recorded_version};
use super::redaction::tests::{MASK, Masking, NonIdempotentValueOnly, SECRET, Shouting};
use super::{
    ARTIFACTS_DIRECTORY, Artifact, Availability, DATABASE_FILE, EventKind, EventPage, EventSeq,
    MAX_EVENT_PAGE_LIMIT, MAX_INLINE_PAYLOAD_BYTES, Redactor, RunCursor, RunEvent, RunPage, Store,
    StoreArtifacts, StoreError, guard,
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

fn policy_decision(verdict: PolicyVerdict) -> PolicyDecision {
    serde_json::from_value(json!({
        "verdict": verdict.as_str(),
        "reason": format!("fixture policy decision: {}", verdict.as_str()),
        "source": "built_in"
    }))
    .unwrap()
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

/// A read-only projection has to be able to ask a data directory what it holds
/// without writing to it. `Store::open` answers "nothing recorded" by first
/// creating the directory, the database, both WAL sidecars and every migration,
/// which is not a read.
#[test]
fn opening_only_an_existing_database_leaves_an_untouched_data_directory_alone() {
    let parent = TempDir::new().unwrap();
    let data_dir = parent.path().join("nested").join("harkness");

    assert!(Store::open_existing(&data_dir).unwrap().is_none());
    assert!(
        !data_dir.exists(),
        "a read created the data directory: {}",
        data_dir.display()
    );

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    drop(stored_run(&fixture.store, &task));
    let opened = Store::open_existing(fixture.data_dir.path())
        .unwrap()
        .expect("an existing database opens");
    assert_eq!(opened.list_runs(RunPage::new(10)).unwrap().runs.len(), 1);
}

#[test]
fn opening_an_existing_database_reuses_it_instead_of_replacing_it() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    drop(stored_run(&fixture.store, &task));

    let reopened = fixture.reopen();

    assert_eq!(reopened.list_runs(RunPage::new(10)).unwrap().runs.len(), 1);
}

// -- workspace trust -------------------------------------------------------

#[test]
fn an_undecided_workspace_is_untrusted_by_default() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();

    assert_eq!(
        fixture
            .store
            .resolve_workspace_trust(harkness_core::ProjectId::new(), workspace.path())
            .unwrap(),
        TrustState::Untrusted
    );
}

#[test]
fn workspace_trust_requires_both_project_identity_and_canonical_path() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    let trust =
        WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8)).unwrap();
    fixture.store.put_workspace_trust(&trust).unwrap();

    assert_eq!(
        fixture
            .store
            .resolve_workspace_trust(project_id, workspace.path())
            .unwrap(),
        TrustState::Trusted
    );
    assert_eq!(
        fixture
            .store
            .resolve_workspace_trust(harkness_core::ProjectId::new(), workspace.path())
            .unwrap(),
        TrustState::Untrusted,
        "a path match cannot transfer trust to a recreated catalog entry"
    );
    assert_eq!(
        fixture
            .store
            .resolve_workspace_trust(project_id, elsewhere.path())
            .unwrap(),
        TrustState::Untrusted,
        "a project-id match cannot transfer trust to a moved checkout"
    );

    let reopened = fixture.reopen();
    assert_eq!(
        reopened.workspace_trust(project_id).unwrap().unwrap(),
        trust,
        "the decision must survive a new connection"
    );
}

#[test]
fn a_later_untrusted_decision_replaces_the_earlier_trusted_one() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    for (state, at) in [(TrustState::Trusted, at(8)), (TrustState::Untrusted, at(9))] {
        fixture
            .store
            .put_workspace_trust(
                &WorkspaceTrust::decide(project_id, workspace.path(), state, at).unwrap(),
            )
            .unwrap();
    }

    assert_eq!(
        fixture
            .store
            .resolve_workspace_trust(project_id, workspace.path())
            .unwrap(),
        TrustState::Untrusted
    );
}

#[test]
fn an_older_writer_cannot_overwrite_a_newer_revocation() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    let trusted =
        WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8)).unwrap();
    let stale = trusted.clone();
    fixture.store.put_workspace_trust(&trusted).unwrap();
    fixture
        .store
        .revoke_workspace_trust(project_id, at(10))
        .unwrap();
    fixture.store.put_workspace_trust(&stale).unwrap();

    let stored = fixture.store.workspace_trust(project_id).unwrap().unwrap();
    assert_eq!(stored.state(), TrustState::Untrusted);
    assert_eq!(stored.decided_at(), at(10));
}

#[test]
fn an_untrusted_decision_wins_a_timestamp_tie() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    let trusted =
        WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8)).unwrap();
    let untrusted =
        WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Untrusted, at(8)).unwrap();

    fixture.store.put_workspace_trust(&untrusted).unwrap();
    fixture.store.put_workspace_trust(&trusted).unwrap();

    assert_eq!(
        fixture
            .store
            .workspace_trust(project_id)
            .unwrap()
            .unwrap()
            .state(),
        TrustState::Untrusted
    );
}

#[test]
fn trust_can_be_revoked_after_the_workspace_disappears() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    fixture
        .store
        .put_workspace_trust(
            &WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8))
                .unwrap(),
        )
        .unwrap();
    drop(workspace);

    fixture
        .store
        .revoke_workspace_trust(project_id, at(9))
        .unwrap();
    assert_eq!(
        fixture
            .store
            .workspace_trust(project_id)
            .unwrap()
            .unwrap()
            .state(),
        TrustState::Untrusted
    );
}

#[test]
fn a_future_workspace_trust_row_requests_an_upgrade_before_decoding_its_body() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    fixture
        .store
        .put_workspace_trust(
            &WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8))
                .unwrap(),
        )
        .unwrap();
    guard(&fixture.store.writer)
        .execute(
            "UPDATE workspace_trust SET schema_version = 99, state = 'future', \
             decided_at = 'future' WHERE project_id = ?1",
            [project_id.to_string()],
        )
        .unwrap();

    let error = fixture.store.workspace_trust(project_id).unwrap_err();
    assert_eq!(error.kind(), "invalid_record");
    assert!(error.to_string().contains("upgrade Harkness"), "{error}");
}

#[test]
fn an_unknown_current_workspace_trust_state_is_refused() {
    let fixture = Fixture::new();
    let workspace = TempDir::new().unwrap();
    let project_id = harkness_core::ProjectId::new();
    fixture
        .store
        .put_workspace_trust(
            &WorkspaceTrust::decide(project_id, workspace.path(), TrustState::Trusted, at(8))
                .unwrap(),
        )
        .unwrap();
    guard(&fixture.store.writer)
        .execute(
            "UPDATE workspace_trust SET state = 'future' WHERE project_id = ?1",
            [project_id.to_string()],
        )
        .unwrap();

    assert_eq!(
        fixture
            .store
            .workspace_trust(project_id)
            .unwrap_err()
            .kind(),
        "column_encoding"
    );
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
fn inserting_a_run_and_first_event_rolls_back_both_on_event_refusal() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let existing = stored_run(&fixture.store, &task);
    let foreign_step = stored_step(&fixture.store, &existing);
    let run = Run::new(task.id(), at(20));

    let error = fixture
        .store
        .insert_run_with_event(
            &run,
            None,
            RunEvent::new(EventKind::Diagnostic, at(20)).for_step(foreign_step.id()),
        )
        .unwrap_err();

    assert!(matches!(error.kind(), "missing_parent" | "query_failed"));
    assert_eq!(
        fixture.store.load_run(run.id()).unwrap_err().kind(),
        "not_found"
    );
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
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
        policy_decision: None,
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

    let decision = policy_decision(PolicyVerdict::Deny);
    let denied = fixture
        .store
        .apply_tool_call_policy_decision(call.id(), decision.clone(), at(10))
        .unwrap();

    assert_eq!(denied.state(), ToolCallState::Denied);
    // The point of the test: a denied row that reached the database without the
    // decision that produced it is an audit gap, not a lifecycle detail.
    assert_eq!(denied.policy_decision(), Some(&decision));
    assert_eq!(
        denied.failure(),
        Some(&Failure::new("policy", decision.reason()))
    );
    let reloaded = fixture.reopen().load_tool_call(call.id()).unwrap();
    assert_eq!(reloaded, denied);
    assert_eq!(reloaded.policy_decision(), Some(&decision));
}

#[test]
fn a_missing_external_identity_denial_is_persisted_and_terminalizes_the_call() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let decision: PolicyDecision = serde_json::from_value(json!({
        "verdict": "deny",
        "reason": "denied: invoke_mcp_tool requires observed identity evidence",
        "source": "built_in",
        "external_request": {
            "schema_version": 1,
            "capability": "invoke_mcp_tool",
            "classified_risk": "execute"
        },
        "denial_kind": "mcp_tool_schema_identity_required"
    }))
    .unwrap();

    let denied = fixture
        .store
        .apply_tool_call_policy_decision(call.id(), decision.clone(), at(10))
        .unwrap();
    assert_eq!(denied.state(), ToolCallState::Denied);
    assert_eq!(denied.policy_decision(), Some(&decision));
    assert_eq!(fixture.store.load_tool_call(call.id()).unwrap(), denied);
}

#[test]
fn an_external_declaration_mismatch_denial_is_persisted_and_terminalizes_the_call() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let decision: PolicyDecision = serde_json::from_value(json!({
        "verdict": "deny",
        "reason": "denied: external context does not match the declared capability",
        "source": "built_in",
        "external_request": {
            "schema_version": 1,
            "capability": "invoke_mcp_tool",
            "classified_risk": "execute",
            "identity": {
                "mcp_tool_schema_fingerprint": Sha256Hash::of("schema")
            }
        },
        "denial_kind": "external_identity_context_invalid"
    }))
    .unwrap();

    let denied = fixture
        .store
        .apply_tool_call_policy_decision(call.id(), decision.clone(), at(10))
        .unwrap();
    assert_eq!(denied.state(), ToolCallState::Denied);
    assert_eq!(denied.policy_decision(), Some(&decision));
    assert_eq!(fixture.reopen().load_tool_call(call.id()).unwrap(), denied);
}

#[test]
fn every_policy_verdict_is_persisted_before_its_lifecycle_consequence() {
    for (verdict, expected_state) in [
        (PolicyVerdict::Allow, ToolCallState::Pending),
        (PolicyVerdict::Ask, ToolCallState::AwaitingApproval),
        (PolicyVerdict::Deny, ToolCallState::Denied),
    ] {
        let fixture = Fixture::new();
        let task = stored_task(&fixture.store);
        let run = stored_run(&fixture.store, &task);
        let step = stored_step(&fixture.store, &run);
        let call = stored_tool_call(&fixture.store, &step);
        let decision = policy_decision(verdict);

        let governed = fixture
            .store
            .apply_tool_call_policy_decision(call.id(), decision.clone(), at(10))
            .unwrap();

        assert_eq!(governed.state(), expected_state);
        assert_eq!(governed.policy_decision(), Some(&decision));
        let reloaded = fixture.reopen().load_tool_call(call.id()).unwrap();
        assert_eq!(reloaded, governed);
        if verdict == PolicyVerdict::Deny {
            assert_eq!(reloaded.failure().unwrap().kind(), "policy");
            assert_eq!(reloaded.failure().unwrap().message(), decision.reason());
        }
    }
}

#[test]
fn a_policy_decision_cannot_be_replaced() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let allowed = fixture
        .store
        .apply_tool_call_policy_decision(call.id(), policy_decision(PolicyVerdict::Allow), at(10))
        .unwrap();

    let error = fixture
        .store
        .apply_tool_call_policy_decision(call.id(), policy_decision(PolicyVerdict::Deny), at(11))
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    assert_eq!(fixture.store.load_tool_call(call.id()).unwrap(), allowed);
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
fn coordinator_state_helpers_roll_back_when_their_event_is_refused() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let failure = Failure::new("fixture", "must roll back");
    let error = fixture
        .store
        .fail_run_with_event(
            run.id(),
            failure,
            at(10),
            RunEvent::new(EventKind::RunStateChanged, at(10)).for_step(StepId::new()),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "missing_parent");
    assert_eq!(fixture.store.load_run(run.id()).unwrap(), run);
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let error = fixture
        .store
        .transition_step_with_event(
            step.id(),
            ExecutionState::Running,
            at(10),
            RunEvent::new(EventKind::StepStarted, at(10)).for_tool_call(ToolCallId::new()),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "missing_parent");
    assert_eq!(fixture.store.load_step(step.id()).unwrap(), step);
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());

    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = stored_tool_call(&fixture.store, &step);
    let error = fixture
        .store
        .apply_tool_call_policy_decision_with_event(
            call.id(),
            policy_decision(PolicyVerdict::Ask),
            at(10),
            RunEvent::new(EventKind::PolicyDecision, at(10)).for_artifact(ArtifactId::new()),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "missing_parent");
    assert_eq!(fixture.store.load_tool_call(call.id()).unwrap(), call);
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
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
fn dispatching_a_tool_call_pins_the_version_that_ran_with_its_event() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    // Recorded without naming a version, which is what a caller that wants
    // "whichever is latest" leaves behind.
    let call = ToolCall::new(&step, "fs.read", "", json!({"path": "a.rs"}), at(3));
    fixture.store.insert_tool_call(&call).unwrap();

    let (running, seq) = fixture
        .store
        .dispatch_tool_call_with_event(
            call.id(),
            "1.4.0",
            at(10),
            RunEvent::new(EventKind::ToolCallStateChanged, at(10)).for_tool_call(call.id()),
        )
        .unwrap();

    assert_eq!(running.state(), ToolCallState::Running);
    assert_eq!(running.tool_version(), "1.4.0");

    // The version has to survive the *next* write, which is where a lifecycle
    // update that did not name the column would quietly lose it.
    let succeeded = fixture
        .store
        .succeed_tool_call(call.id(), json!({"bytes": 4}), at(11))
        .unwrap();
    assert_eq!(succeeded.tool_version(), "1.4.0");
    assert_eq!(
        fixture
            .reopen()
            .load_tool_call(call.id())
            .unwrap()
            .tool_version(),
        "1.4.0"
    );
    assert_eq!(
        fixture.store.events(run.id(), None, 10).unwrap()[0].seq,
        seq
    );
}

#[test]
fn an_approved_dispatch_commits_the_decision_the_version_and_the_event_together() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = ToolCall::new(&step, "fs.read", "", json!({"path": "a.rs"}), at(3));
    fixture.store.insert_tool_call(&call).unwrap();
    fixture
        .store
        .transition_tool_call(call.id(), ToolCallState::AwaitingApproval, at(4))
        .unwrap();

    let (running, seq) = fixture
        .store
        .dispatch_approved_tool_call_with_event(
            call.id(),
            "reviewer",
            "1.4.0",
            at(10),
            RunEvent::new(EventKind::ApprovalDecided, at(10)).for_tool_call(call.id()),
        )
        .unwrap();

    assert_eq!(running.state(), ToolCallState::Running);
    assert_eq!(running.tool_version(), "1.4.0");
    assert_eq!(running.approvals().len(), 1);

    // The version survives the next write, as it must for the approval beside it
    // to keep describing the work that was authorized.
    let reloaded = fixture.reopen().load_tool_call(call.id()).unwrap();
    assert_eq!(reloaded.tool_version(), "1.4.0");
    assert_eq!(reloaded.approvals()[0].decided_by(), "reviewer");
    assert_eq!(
        fixture.store.events(run.id(), None, 10).unwrap()[0].seq,
        seq
    );
}

#[test]
fn a_refused_approved_dispatch_writes_neither_the_decision_nor_its_event() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    // Still `pending`, so there is no decision to record.
    let call = stored_tool_call(&fixture.store, &step);

    let error = fixture
        .store
        .dispatch_approved_tool_call_with_event(
            call.id(),
            "reviewer",
            "1.0.0",
            at(10),
            RunEvent::new(EventKind::ApprovalDecided, at(10)).for_tool_call(call.id()),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    let unchanged = fixture.store.load_tool_call(call.id()).unwrap();
    assert_eq!(unchanged.state(), ToolCallState::Pending);
    assert!(unchanged.approvals().is_empty());
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
}

#[test]
fn dispatching_may_not_replace_a_version_a_caller_already_named() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    // `stored_tool_call` records `fs.read@1.0.0`, so this is a resolution
    // disagreeing with the request — a caller bug, not a version to overwrite.
    let call = stored_tool_call(&fixture.store, &step);

    let error = fixture
        .store
        .dispatch_tool_call_with_event(
            call.id(),
            "2.0.0",
            at(10),
            RunEvent::new(EventKind::ToolCallStateChanged, at(10)).for_tool_call(call.id()),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_transition");
    let unchanged = fixture.store.load_tool_call(call.id()).unwrap();
    assert_eq!(unchanged.state(), ToolCallState::Pending);
    assert_eq!(unchanged.tool_version(), "1.0.0");
    assert!(fixture.store.events(run.id(), None, 10).unwrap().is_empty());
}

#[test]
fn a_batch_of_events_is_appended_whole_or_not_at_all() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);

    let seqs = fixture
        .store
        .append_events(
            run.id(),
            (0..5).map(|index| {
                RunEvent::new(EventKind::ToolProgress, at(10))
                    .for_step(step.id())
                    .with_payload(json!({"completed": index}))
            }),
        )
        .unwrap();

    // Numbers are still allocated one at a time inside the transaction, so a
    // batch is indistinguishable from the same appends made separately.
    assert_eq!(
        seqs.iter().map(|seq| seq.get()).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    let stored = fixture.reopen().events(run.id(), None, 10).unwrap();
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[4].event.payload()["completed"], json!(4));

    // One refused event refuses the batch: a timeline that stops mid-phase for
    // no reason a reader can see is worse than one that never claims the phase.
    let orphan = StepId::new();
    let error = fixture
        .store
        .append_events(
            run.id(),
            [
                RunEvent::new(EventKind::ToolProgress, at(11)),
                RunEvent::new(EventKind::ToolProgress, at(11)).for_step(orphan),
            ],
        )
        .unwrap_err();
    assert_eq!(error.kind(), "missing_parent");
    assert_eq!(
        fixture.store.events(run.id(), None, 10).unwrap().len(),
        5,
        "a refused batch must leave the log exactly as it was"
    );

    assert!(
        fixture
            .store
            .append_events(run.id(), [])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_refused_batch_leaves_no_spilled_payload_behind() {
    // The single-event path already cleans up after a rejected write; a batch
    // has the same duty for every payload it spilled before the refusal, or a
    // caller retrying accumulates one file per attempt in a store with no
    // collector.
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let oversized = json!({"stderr": "a".repeat(MAX_INLINE_PAYLOAD_BYTES)});

    let error = fixture
        .store
        .append_events(
            run.id(),
            [
                RunEvent::new(EventKind::Diagnostic, at(10)).with_payload(oversized),
                RunEvent::new(EventKind::Diagnostic, at(10)).for_step(StepId::new()),
            ],
        )
        .unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(fixture.store.run_artifacts(run.id()).unwrap().is_empty());
    let directory = fixture
        .data_dir
        .path()
        .join(ARTIFACTS_DIRECTORY)
        .join(run.id().to_string());
    let leftover = std::fs::read_dir(&directory)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftover, 0, "a spilled payload outlived its refused batch");
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

        let error = fixture
            .store
            .event_page(run.id(), EventPage::oldest(limit))
            .unwrap_err();
        assert_eq!(error.kind(), "invalid_page_limit");
    }
}

#[test]
fn a_newest_first_page_opens_at_the_tip_and_walks_backwards() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    for index in 0..5 {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, at(20 + index)),
            )
            .unwrap();
    }

    let mut seen = Vec::new();
    let mut page = EventPage::newest(2);
    loop {
        let listing = fixture.store.event_page(run.id(), page).unwrap();
        seen.extend(listing.events.iter().map(|stored| stored.seq.get()));
        let Some(next) = listing.next_cursor else {
            break;
        };
        page = EventPage::newest(2).after(next);
    }

    assert_eq!(
        seen,
        vec![5, 4, 3, 2, 1],
        "a newest-first walk must reach every event exactly once, in reverse"
    );
}

#[test]
fn a_page_that_reaches_the_end_of_the_log_offers_no_continuation() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    for index in 0..4 {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, at(20 + index)),
            )
            .unwrap();
    }

    // Exactly full and last: the distinction an under-full page cannot make,
    // and the reason the continuation is a probe rather than a length check.
    let exact = fixture
        .store
        .event_page(run.id(), EventPage::oldest(4))
        .unwrap();
    assert_eq!(exact.events.len(), 4);
    assert_eq!(
        exact.next_cursor, None,
        "a full page with nothing behind it must not offer a continuation"
    );

    let partial = fixture
        .store
        .event_page(run.id(), EventPage::newest(3).after(EventSeq::new(2)))
        .unwrap();
    assert_eq!(
        partial
            .events
            .iter()
            .map(|stored| stored.seq.get())
            .collect::<Vec<_>>(),
        vec![1],
        "the boundary is exclusive in the direction the order names"
    );
    assert_eq!(partial.next_cursor, None);
}

#[test]
fn a_newest_first_page_is_unmoved_by_events_appended_at_the_tip() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    for index in 0..4 {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, at(20 + index)),
            )
            .unwrap();
    }

    let first = fixture
        .store
        .event_page(run.id(), EventPage::newest(2))
        .unwrap();
    assert_eq!(
        first
            .events
            .iter()
            .map(|stored| stored.seq.get())
            .collect::<Vec<_>>(),
        vec![4, 3]
    );

    // Scrolling back is a walk towards the beginning of the log, so appends at
    // the far end must not shift what the next page contains. An offset page
    // would return event 3 twice here.
    for _ in 0..3 {
        fixture
            .store
            .append_event(run.id(), RunEvent::new(EventKind::Diagnostic, at(40)))
            .unwrap();
    }

    let older = fixture
        .store
        .event_page(
            run.id(),
            EventPage::newest(2).after(first.next_cursor.unwrap()),
        )
        .unwrap();
    assert_eq!(
        older
            .events
            .iter()
            .map(|stored| stored.seq.get())
            .collect::<Vec<_>>(),
        vec![2, 1],
        "an append at the tip displaced a page walking away from it"
    );
    assert_eq!(older.next_cursor, None);
}

#[test]
fn both_page_directions_agree_on_the_events_of_a_run() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    for index in 0..7 {
        fixture
            .store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, at(20 + index))
                    .with_payload(json!({"index": index})),
            )
            .unwrap();
    }

    let mut forwards = Vec::new();
    let mut page = EventPage::oldest(3);
    loop {
        let listing = fixture.store.event_page(run.id(), page).unwrap();
        forwards.extend(listing.events.clone());
        let Some(next) = listing.next_cursor else {
            break;
        };
        page = EventPage::oldest(3).after(next);
    }

    let mut backwards = Vec::new();
    let mut page = EventPage::newest(3);
    loop {
        let listing = fixture.store.event_page(run.id(), page).unwrap();
        backwards.extend(listing.events.clone());
        let Some(next) = listing.next_cursor else {
            break;
        };
        page = EventPage::newest(3).after(next);
    }
    backwards.reverse();

    assert_eq!(
        forwards, backwards,
        "the two directions disagreed about the same log"
    );
    assert_eq!(forwards, fixture.store.events(run.id(), None, 100).unwrap());
}

#[test]
fn the_event_page_of_an_unstored_run_is_empty_rather_than_refused() {
    let fixture = Fixture::new();

    let listing = fixture
        .store
        .event_page(RunId::new(), EventPage::newest(10))
        .unwrap();

    assert!(listing.events.is_empty());
    assert_eq!(listing.next_cursor, None);
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

/// A URL whose userinfo the standard rules recognize.
const LEAKY_URL: &str = "https://user:hunter2@example.com/repo.git";

#[test]
fn an_opened_store_scrubs_without_being_asked_to() {
    let fixture = Fixture::new();
    let task = Task::with_id(
        TaskId::from_str(FIXTURE_TASK_ID).unwrap(),
        format!("clone {LEAKY_URL}"),
        "/workspace/harkness",
        None,
        at(0),
    );

    fixture.store.insert_task(&task).unwrap();

    let stored = fixture.store.load_task(task.id()).unwrap();
    assert!(
        !stored.title().contains("hunter2"),
        "Store::open must install real rules, not PassThrough: {}",
        stored.title()
    );
    assert!(stored.title().contains("«redacted:url_userinfo»"));
    assert_eq!(
        stored.workspace_root(),
        task.workspace_root(),
        "a workspace root is a filesystem identity this store compares and canonicalizes"
    );
}

#[test]
fn a_result_and_a_failure_are_scrubbed_before_they_become_columns() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let succeeding = stored_tool_call(&fixture.store, &step);
    let failing = ToolCall::new(
        &step,
        "fs.read",
        "1.0.0",
        json!({"path": "src/lib.rs"}),
        at(4),
    );
    fixture.store.insert_tool_call(&failing).unwrap();

    fixture
        .store
        .transition_tool_call(succeeding.id(), ToolCallState::Running, at(5))
        .unwrap();
    fixture
        .store
        .succeed_tool_call(succeeding.id(), json!({"remote": LEAKY_URL}), at(6))
        .unwrap();
    fixture
        .store
        .transition_tool_call(failing.id(), ToolCallState::Running, at(7))
        .unwrap();
    fixture
        .store
        .fail_tool_call(
            failing.id(),
            Failure::new("not_found", format!("could not reach {LEAKY_URL}")),
            at(8),
        )
        .unwrap();
    fixture
        .store
        .fail_step(
            step.id(),
            Failure::new("tool", format!("at {LEAKY_URL}")),
            at(9),
        )
        .unwrap();

    let output = fixture
        .store
        .load_tool_call(succeeding.id())
        .unwrap()
        .output()
        .cloned()
        .unwrap();
    assert_eq!(
        output["remote"],
        "https://«redacted:url_userinfo»@example.com/repo.git"
    );

    let failed = fixture.store.load_tool_call(failing.id()).unwrap();
    let failure = failed.failure().unwrap();
    assert_eq!(
        failure.kind(),
        "not_found",
        "the kind is a machine identifier this build chose; only the detail can leak"
    );
    assert!(
        !failure.message().contains("hunter2"),
        "{}",
        failure.message()
    );

    let step_failure = fixture.store.load_step(step.id()).unwrap();
    assert!(
        !step_failure
            .failure()
            .unwrap()
            .message()
            .contains("hunter2")
    );
}

#[test]
fn a_tool_call_input_is_stored_exactly_as_the_caller_wrote_it() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let step = stored_step(&fixture.store, &run);
    let call = ToolCall::new(
        &step,
        "git.fetch",
        "1.0.0",
        json!({"remote": LEAKY_URL}),
        at(4),
    );
    fixture.store.insert_tool_call(&call).unwrap();

    let stored = fixture.store.load_tool_call(call.id()).unwrap();

    // This is a *boundary*, not an oversight, and it is pinned here so that
    // closing it has to be a deliberate change rather than a tidy-up. The
    // executor reads these bytes back and runs them, and an approval's hash is
    // taken over them: a rewritten input would run a different command than the
    // one that was approved, against a record that no longer matches the
    // decision made about it. `observe`'s coverage table says so where a tool
    // author reads, and the answer is a declared environment variable.
    assert_eq!(
        stored.input(),
        &json!({"remote": LEAKY_URL}),
        "redacting an input would change what the executor runs"
    );
}

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
fn structured_artifact_redacts_values_and_metadata_once_but_not_keys() {
    let data_dir = TempDir::new().unwrap();
    let store = Arc::new(
        Store::open(data_dir.path())
            .unwrap()
            .redacting(Arc::new(NonIdempotentValueOnly)),
    );
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);
    let mut artifacts = StoreArtifacts::new(Arc::clone(&store), run.id(), step.id(), call.id());

    let reference = artifacts
        .write_json(
            "diff-hunter2.json",
            "application/hunter2+json",
            &json!({"published_key": SECRET}),
        )
        .unwrap();
    let id = ArtifactId::from_str(&reference.id).unwrap();
    let metadata = store.artifact(id).unwrap();
    let payload: Value = serde_json::from_slice(&store.read_artifact(id).unwrap()).unwrap();

    assert_eq!(payload, json!({"published_key": format!("R({MASK})")}));
    assert_eq!(metadata.name(), "R(diff-[redacted].json)");
    assert_eq!(metadata.media_type(), "R(application/[redacted]+json)");
    assert!(!payload.to_string().contains("R(R("));
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

// -- approvals ---------------------------------------------------------------

/// A store with a task, a run, a step, and one recorded tool call.
struct Held {
    fixture: Fixture,
    run: Run,
    call: ToolCall,
}

impl Held {
    fn new() -> Self {
        Self::with(Fixture::new())
    }

    fn with(fixture: Fixture) -> Self {
        let task = stored_task(&fixture.store);
        let run = stored_run(&fixture.store, &task);
        let step = stored_step(&fixture.store, &run);
        let call = stored_tool_call(&fixture.store, &step);
        Self { fixture, run, call }
    }

    fn store(&self) -> &Store {
        &self.fixture.store
    }

    fn pending(&self, risk: RiskLevel) -> PendingApproval {
        PendingApproval::new(
            self.run.id(),
            self.call.id(),
            ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
            canonical_input_hash(&approval_input()).unwrap(),
            approval_workspace(),
            risk,
            at(4),
        )
        .with_capabilities([Capability::new("fs.write").unwrap()])
        .summarized_as("write 12 lines to src/lib.rs")
    }

    /// Persists one pending request together with the event announcing it.
    fn open(&self, pending: PendingApproval) -> ApprovalRequest {
        self.store()
            .open_approval(ApprovalRequest::open(pending).unwrap())
            .unwrap()
            .0
    }
}

fn approval_input() -> Value {
    json!({"path": "src/lib.rs", "contents": "fn main() {}"})
}

fn approval_workspace() -> WorkspaceBinding {
    WorkspaceBinding::new(
        Some(harkness_core::ProjectId::from_str("55555555-5555-4555-8555-555555555555").unwrap()),
        "/workspace/harkness",
    )
}

#[test]
fn an_approval_row_exists_before_the_event_that_announces_it_is_visible() {
    let held = Held::new();
    let request = ApprovalRequest::open(held.pending(RiskLevel::WorkspaceWrite)).unwrap();
    let id = request.id();

    // Nothing is observable before the write.
    assert_eq!(held.store().approval(id).unwrap_err().kind(), "not_found");
    assert!(
        held.store()
            .events(held.run.id(), None, 10)
            .unwrap()
            .is_empty()
    );

    held.store().open_approval(request).unwrap();

    // Afterwards both are, and they arrived in one transaction: an observer can
    // never see the announcement without the question behind it.
    let events = held.store().events(held.run.id(), None, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.kind().as_str(), "approval_requested");
    assert_eq!(
        held.store().approval(id).unwrap().state(),
        ApprovalState::Pending
    );
}

#[test]
fn a_request_answered_before_it_was_recorded_cannot_be_opened() {
    // Deciding a record in memory and only then handing it to the store would
    // land a live grant whose timeline says a question was asked and never
    // answered. Approval-before-execution is a claim about what the store
    // witnessed, so the only thing that may be opened is a question.
    let held = Held::new();
    let mut request = ApprovalRequest::open(held.pending(RiskLevel::Destructive)).unwrap();
    request
        .decide(ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ExactCall,
            DecidedVia::Cli,
            at(5),
        ))
        .unwrap();
    let id = request.id();

    let error = held.store().open_approval(request).unwrap_err();

    assert!(
        matches!(&error, StoreError::Approval(inner)
            if inner.kind() == "approval_already_resolved"),
        "unexpected refusal: {error}"
    );
    assert_eq!(held.store().approval(id).unwrap_err().kind(), "not_found");
    assert!(held.store().run_grants(held.run.id()).unwrap().is_empty());
    assert!(
        held.store()
            .events(held.run.id(), None, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_approval_event_carries_the_summary_and_never_the_input() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::RemoteWrite)
            .requesting(ApprovalScope::ToolForRun),
    );
    let events = held.store().events(held.run.id(), None, 10).unwrap();

    let payload = events[0].event.payload();
    assert_eq!(payload["summary"], json!("write 12 lines to src/lib.rs"));
    assert_eq!(payload["approval_id"], json!(request.id().to_string()));
    assert_eq!(payload["tool"], json!("fs.write@1.2.0"));
    assert_eq!(payload["risk"], json!("remote_write"));
    // Both spellings, so a reader of the timeline alone can see the downgrade.
    assert_eq!(payload["requested_scope"], json!("tool_for_run"));
    assert_eq!(payload["effective_scope"], json!("exact_call"));
    assert_eq!(events[0].event.tool_call_id(), Some(held.call.id()));

    // The event is derived from the record, so there is no route by which the
    // raw input reaches the hot event stream at all.
    let rendered = payload.to_string();
    assert!(
        !rendered.contains("fn main"),
        "the tool input must not reach the timeline: {rendered}"
    );

    held.store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(6),
            )
            .because("reviewed"),
        )
        .unwrap();
    let events = held.store().events(held.run.id(), None, 10).unwrap();
    assert_eq!(events[1].event.kind().as_str(), "approval_decided");
    assert_eq!(events[1].event.payload()["state"], json!("granted"));
    assert_eq!(events[1].event.payload()["verdict"], json!("granted"));
    assert_eq!(events[1].event.payload()["scope"], json!("exact_call"));
    assert_eq!(events[1].event.payload()["decided_via"], json!("cli"));
    assert_eq!(events[1].event.payload()["reason"], json!("reviewed"));
}

#[test]
fn an_unanswered_resolution_records_no_verdict_in_the_timeline() {
    let held = Held::new();
    let request = held.open(held.pending(RiskLevel::Execute));

    held.store()
        .resolve_approval(request.id(), ApprovalState::Cancelled, at(8))
        .unwrap();

    let events = held.store().events(held.run.id(), None, 10).unwrap();
    let payload = events[1].event.payload();
    assert_eq!(payload["state"], json!("cancelled"));
    assert_eq!(
        payload.get("verdict"),
        None,
        "nobody answered, so the timeline must not report a verdict"
    );
}

#[test]
fn an_approval_may_only_hold_a_tool_call_of_its_own_run() {
    let held = Held::new();

    // A second run of the same task, with its own step and call.
    let sibling = queued_run(
        held.store(),
        &held.store().load_task(held.run.task_id()).unwrap(),
        40,
    );
    let sibling_step = Step::new(sibling.id(), 0, "Sibling step", at(41));
    held.store().insert_step(&sibling_step).unwrap();
    let sibling_call = ToolCall::new(&sibling_step, "fs.write", "1.2.0", approval_input(), at(42));
    held.store().insert_tool_call(&sibling_call).unwrap();

    for (run_id, tool_call_id) in [
        // A call of a different run, claimed by this one.
        (held.run.id(), sibling_call.id()),
        // A call that was never stored at all.
        (held.run.id(), ToolCallId::new()),
    ] {
        let request = ApprovalRequest::open(PendingApproval::new(
            run_id,
            tool_call_id,
            ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
            canonical_input_hash(&approval_input()).unwrap(),
            approval_workspace(),
            RiskLevel::Execute,
            at(4),
        ))
        .unwrap();

        let error = held.store().open_approval(request).unwrap_err();
        assert_eq!(error.kind(), "missing_parent", "{run_id}/{tool_call_id}");
    }
}

#[test]
fn either_front_end_resolves_the_same_pending_request() {
    for via in [DecidedVia::Cli, DecidedVia::Gui] {
        let held = Held::new();
        let request = held.open(held.pending(RiskLevel::WorkspaceWrite));

        let (resolved, seq) = held
            .store()
            .decide_approval(
                request.id(),
                ApprovalDecision::grant(request.id(), ApprovalScope::ExactCall, via, at(6))
                    .because("looks right"),
            )
            .unwrap();

        assert_eq!(resolved.state(), ApprovalState::Granted, "{via}");
        assert_eq!(resolved.decision().unwrap().decided_via(), via);
        assert_eq!(seq, EventSeq::new(2));
        // Reloading proves the decision is durable rather than only returned.
        let reloaded = held.store().approval(request.id()).unwrap();
        assert_eq!(reloaded, resolved);
        assert_eq!(reloaded.decision().unwrap().reason(), Some("looks right"));
    }
}

#[test]
fn the_second_decision_on_a_resolved_request_is_refused_by_name() {
    let held = Held::new();
    let request = held.open(held.pending(RiskLevel::WorkspaceWrite));
    held.store()
        .decide_approval(
            request.id(),
            ApprovalDecision::deny(request.id(), DecidedVia::Gui, at(6)),
        )
        .unwrap();

    let error = held
        .store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(7),
            ),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "approval_refused");
    assert!(
        matches!(&error, StoreError::Approval(inner)
            if inner.kind() == "approval_already_resolved"),
        "unexpected refusal: {error}"
    );
    // The refused write left neither a changed state nor a second event.
    let reloaded = held.store().approval(request.id()).unwrap();
    assert_eq!(reloaded.state(), ApprovalState::Denied);
    assert_eq!(
        held.store().events(held.run.id(), None, 10).unwrap().len(),
        2
    );
}

#[test]
fn two_threads_deciding_one_request_produce_exactly_one_winner() {
    let held = Arc::new(Held::new());
    let request = held.open(held.pending(RiskLevel::Execute));
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let deciders = [DecidedVia::Cli, DecidedVia::Gui].map(|via| {
        let held = Arc::clone(&held);
        let barrier = Arc::clone(&barrier);
        let id = request.id();
        thread::spawn(move || {
            barrier.wait();
            held.store().decide_approval(
                id,
                ApprovalDecision::grant(id, ApprovalScope::ExactCall, via, at(6)),
            )
        })
    });

    let outcomes = deciders.map(|decider| decider.join().unwrap());
    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(winners, 1, "exactly one decision may resolve a request");
    let loser = outcomes
        .into_iter()
        .find_map(Result::err)
        .expect("the other decision must be refused");
    assert_eq!(loser.kind(), "approval_refused");
    assert_eq!(
        held.store().events(held.run.id(), None, 10).unwrap().len(),
        2,
        "the losing decision must not have appended its event either"
    );
}

#[test]
fn a_timeout_and_a_cancellation_resolve_a_request_without_a_decision() {
    for (state, offset) in [
        (ApprovalState::Expired, 8),
        (ApprovalState::Cancelled, 9),
        (ApprovalState::Superseded, 10),
    ] {
        let held = Held::new();
        let request = held.open(held.pending(RiskLevel::Execute));

        let (resolved, _) = held
            .store()
            .resolve_approval(request.id(), state, at(offset))
            .unwrap();

        assert_eq!(resolved.state(), state);
        assert_eq!(resolved.resolved_at(), Some(at(offset)));
        assert!(
            resolved.decision().is_none(),
            "nobody answered, so no decision may be recorded"
        );
        assert_eq!(held.store().approval(request.id()).unwrap(), resolved);

        // The waiting call observes a denial rather than hanging.
        let observation = ApprovalObservation::of(&resolved).unwrap();
        assert!(!observation.is_granted());
        assert_eq!(observation.state(), state);
    }
}

#[test]
fn a_resolved_request_cannot_be_resolved_a_second_time() {
    let held = Held::new();
    let request = held.open(held.pending(RiskLevel::Execute));
    held.store()
        .resolve_approval(request.id(), ApprovalState::Cancelled, at(8))
        .unwrap();

    let error = held
        .store()
        .resolve_approval(request.id(), ApprovalState::Expired, at(9))
        .unwrap_err();

    assert!(
        matches!(&error, StoreError::Approval(inner)
            if inner.kind() == "approval_invalid_transition"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn no_transaction_spans_the_period_a_request_is_pending() {
    let held = Held::new();
    let request = held.open(held.pending(RiskLevel::Execute));
    let gate = Arc::new(ApprovalGate::new());
    let ticket = gate.ticket(request.id()).unwrap();

    // The call is parked. If `open_approval` had left a transaction open, this
    // unrelated write would block until the busy timeout and fail.
    let started = std::time::Instant::now();
    held.store()
        .append_event(
            held.run.id(),
            RunEvent::new(EventKind::Diagnostic, at(6)).with_payload(json!({"note": "unrelated"})),
        )
        .unwrap();
    held.store()
        .transition_run(held.run.id(), ExecutionState::Running, at(6))
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "a concurrent writer waited {elapsed:?}, so a transaction was held across the wait"
    );

    // A second connection to the same file commits too, so the claim holds
    // between processes and not merely within this one.
    let separate = held.fixture.reopen();
    separate
        .append_event(
            held.run.id(),
            RunEvent::new(EventKind::Diagnostic, at(7)).with_payload(json!({"note": "another"})),
        )
        .unwrap();

    let (resolved, _) = held
        .store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(8),
            ),
        )
        .unwrap();
    gate.resolve_from(&resolved);
    assert!(ticket.wait().is_granted());
}

#[test]
fn a_pending_request_survives_a_crash_with_every_binding_field_intact() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::Network)
            .requesting(ApprovalScope::ToolForRun)
            .expiring_at(at(600)),
    );

    // Reopening the database is what a restart does; nothing closed the store
    // cleanly, and the pending row is simply still there.
    let reopened = held.fixture.reopen();

    let pending = reopened.pending_approvals().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0], request);
    assert_eq!(pending[0].state(), ApprovalState::Pending);
    assert_eq!(pending[0].run_id(), held.run.id());
    assert_eq!(pending[0].tool_call_id(), held.call.id());
    assert_eq!(pending[0].tool().id.as_str(), "fs.write");
    assert_eq!(pending[0].tool().version.to_string(), "1.2.0");
    assert_eq!(
        pending[0].input_hash(),
        canonical_input_hash(&approval_input()).unwrap()
    );
    assert_eq!(pending[0].workspace(), &approval_workspace());
    assert_eq!(pending[0].risk(), RiskLevel::Network);
    assert_eq!(pending[0].requested_scope(), ApprovalScope::ToolForRun);
    assert_eq!(pending[0].effective_scope(), ApprovalScope::ToolForRun);
    assert_eq!(pending[0].expires_at(), Some(at(600)));
    assert_eq!(
        pending[0]
            .capabilities()
            .iter()
            .map(Capability::as_str)
            .collect::<Vec<_>>(),
        ["fs.write"]
    );

    // Answering it after the restart is an ordinary decision.
    reopened
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ToolForRun,
                DecidedVia::Cli,
                at(11),
            ),
        )
        .unwrap();
    assert!(reopened.pending_approvals().unwrap().is_empty());
}

#[test]
fn a_downgraded_remote_write_request_is_stored_and_granted_as_one_call() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::RemoteWrite)
            .requesting(ApprovalScope::ToolForRun),
    );

    let stored = held.store().approval(request.id()).unwrap();
    assert_eq!(stored.requested_scope(), ApprovalScope::ToolForRun);
    assert_eq!(stored.effective_scope(), ApprovalScope::ExactCall);
    assert!(stored.was_downgraded());

    // The surface cannot restore the breadth the ceiling removed.
    let error = held
        .store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ToolForRun,
                DecidedVia::Gui,
                at(6),
            ),
        )
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Approval(inner)
            if inner.kind() == "approval_scope_exceeds_request"),
        "unexpected refusal: {error}"
    );

    let (granted, _) = held
        .store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(7),
            ),
        )
        .unwrap();
    assert_eq!(granted.state(), ApprovalState::Granted);
    assert_eq!(
        held.store().run_grants(held.run.id()).unwrap().len(),
        1,
        "the one-call grant is still a grant"
    );
}

#[test]
fn only_granted_requests_become_grants_and_they_bind_to_their_own_call() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::WorkspaceWrite)
            .requesting(ApprovalScope::ExactCall),
    );
    assert!(held.store().run_grants(held.run.id()).unwrap().is_empty());

    held.store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(6),
            ),
        )
        .unwrap();

    let grants = held.store().run_grants(held.run.id()).unwrap();
    assert_eq!(grants.len(), 1);
    let tool = ToolIdentity::parse("fs.write", "1.2.0").unwrap();
    let workspace = approval_workspace();

    // The call the human actually approved.
    let approved = CandidateCall::new(
        held.run.id(),
        held.call.id(),
        &workspace,
        &tool,
        canonical_input_hash(&approval_input()).unwrap(),
    );
    assert_eq!(matching_grants(&grants, &approved).len(), 1);

    // The same tool, one byte of input different.
    let altered = CandidateCall::new(
        held.run.id(),
        held.call.id(),
        &workspace,
        &tool,
        canonical_input_hash(&json!({"path": "src/lib.rs", "contents": "fn main() { rm() }"}))
            .unwrap(),
    );
    assert!(matching_grants(&grants, &altered).is_empty());

    // The same call, in a later run.
    let replayed = CandidateCall::new(
        RunId::new(),
        held.call.id(),
        &workspace,
        &tool,
        canonical_input_hash(&approval_input()).unwrap(),
    );
    assert!(matching_grants(&grants, &replayed).is_empty());
}

#[test]
fn a_cancelled_grant_stops_covering_the_call_it_covered() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::Execute)
            .requesting(ApprovalScope::ToolForRun),
    );
    held.store()
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ToolForRun,
                DecidedVia::Cli,
                at(6),
            ),
        )
        .unwrap();
    assert_eq!(held.store().run_grants(held.run.id()).unwrap().len(), 1);

    // A granted request is terminal, so the grant dies with its run rather than
    // by being revoked: the listing is what stops returning it.
    let second = held.open(held.pending(RiskLevel::Execute));
    held.store()
        .resolve_approval(second.id(), ApprovalState::Cancelled, at(8))
        .unwrap();

    let grants = held.store().run_grants(held.run.id()).unwrap();
    assert_eq!(grants.len(), 1, "only the granted request is a grant");
    assert_eq!(held.store().run_approvals(held.run.id()).unwrap().len(), 2);
}

#[test]
fn an_approval_summary_and_reason_pass_through_the_redactor() {
    let held = Held::with(Fixture::redacting(Arc::new(Masking)));
    let request = held.open(
        held.pending(RiskLevel::Execute)
            .summarized_as(format!("push using {SECRET}")),
    );

    assert_eq!(request.input_summary(), format!("push using {MASK}"));
    assert_eq!(
        held.store().approval(request.id()).unwrap().input_summary(),
        format!("push using {MASK}")
    );

    let (decided, _) = held
        .store()
        .decide_approval(
            request.id(),
            ApprovalDecision::deny(request.id(), DecidedVia::Cli, at(6))
                .because(format!("the token {SECRET} must not leave this machine")),
        )
        .unwrap();
    assert_eq!(
        decided.decision().unwrap().reason(),
        Some(format!("the token {MASK} must not leave this machine").as_str())
    );
    assert_eq!(
        held.store()
            .approval(request.id())
            .unwrap()
            .decision()
            .unwrap()
            .reason(),
        decided.decision().unwrap().reason()
    );
}

#[test]
fn an_oversized_approval_summary_is_refused_and_records_nothing() {
    let held = Held::new();
    let request = ApprovalRequest::open(
        held.pending(RiskLevel::Execute)
            .summarized_as(oversized_text()),
    )
    .unwrap();
    let id = request.id();

    let error = held.store().open_approval(request).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert_eq!(held.store().approval(id).unwrap_err().kind(), "not_found");
    assert!(
        held.store()
            .events(held.run.id(), None, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_hand_edited_approval_row_fails_to_load_instead_of_becoming_a_grant() {
    for (column, value, field) in [
        // A verdict with no surface beside it: half a decision, which is a
        // corrupt row rather than a partial answer.
        ("decision_verdict", "granted", "decision_verdict"),
        ("risk", "catastrophic", "risk"),
        ("effective_scope", "everything", "effective_scope"),
        ("state", "approved", "state"),
        // A decision cleared down to its remnants: the request would otherwise
        // read as unanswered while the decider's words sat in the next column.
        ("decision_reason", "I said yes", "decision_verdict"),
        ("decision_scope", "exact_call", "decision_verdict"),
        ("input_hash", "not-a-hash", "input_hash"),
        // Exactly 64 *bytes*, but not 64 characters. The length check counts
        // bytes and hexadecimal pairs are read as bytes, so this is a refusal
        // rather than a slice boundary landing inside a character.
        (
            "input_hash",
            "aé000000000000000000000000000000000000000000000000000000000000",
            "input_hash",
        ),
        ("tool_version", "one point two", "tool"),
        ("capabilities_json", "fs.write", "capabilities_json"),
    ] {
        let held = Held::new();
        let request = held.open(held.pending(RiskLevel::Execute));
        guard(&held.store().writer)
            .execute(
                &format!("UPDATE approvals SET {column} = ?1 WHERE id = ?2"),
                rusqlite::params![value, request.id().to_string()],
            )
            .unwrap();

        let error = held.store().approval(request.id()).unwrap_err();
        assert_eq!(error.kind(), "column_encoding", "{column} = {value}");
        assert!(
            error.to_string().contains(field),
            "the refusal should name {field}: {error}"
        );
        // The listing refuses the row rather than quietly skipping it, so a
        // tampered row can never reach the matcher as a grant either way.
        assert!(
            !matches!(held.store().run_grants(held.run.id()), Ok(grants) if !grants.is_empty()),
            "{column} = {value} produced a grant"
        );
    }
}

#[test]
fn a_row_edited_to_widen_its_scope_or_forge_a_decision_fails_to_load() {
    // These edits all use spellings this build understands, so nothing catches
    // them at the column level. They are refused because the record they
    // describe cannot be true — and `effective_scope` in particular is what the
    // matcher grants, so nothing downstream would re-derive it.
    for (risk, requested, edits, reason) in [
        (
            RiskLevel::RemoteWrite,
            ApprovalScope::ToolForRun,
            vec![("effective_scope", "tool_for_run")],
            "broader than its requested scope and risk",
        ),
        (
            RiskLevel::WorkspaceWrite,
            ApprovalScope::ExactCall,
            vec![("effective_scope", "tool_for_run")],
            "broader than its requested scope and risk",
        ),
        (
            RiskLevel::Execute,
            ApprovalScope::ExactCall,
            vec![("state", "granted"), ("resolved_at", RESOLVED_AT)],
            "records the decision that resolved it",
        ),
        (
            RiskLevel::Execute,
            ApprovalScope::ExactCall,
            vec![
                ("state", "granted"),
                ("resolved_at", RESOLVED_AT),
                ("decision_verdict", "denied"),
                ("decided_via", "cli"),
            ],
            "disagrees with the verdict",
        ),
        (
            RiskLevel::Execute,
            ApprovalScope::ExactCall,
            vec![("resolved_at", RESOLVED_AT)],
            "records when it was resolved",
        ),
    ] {
        let held = Held::new();
        let request = held.open(held.pending(risk).requesting(requested));
        for (column, value) in &edits {
            guard(&held.store().writer)
                .execute(
                    &format!("UPDATE approvals SET {column} = ?1 WHERE id = ?2"),
                    rusqlite::params![value, request.id().to_string()],
                )
                .unwrap();
        }

        let error = held.store().approval(request.id()).unwrap_err();
        assert_eq!(error.kind(), "approval_refused", "{edits:?}");
        assert!(
            error.to_string().contains(reason),
            "the refusal should name the rule broken by {edits:?}: {error}"
        );
        assert!(
            !matches!(held.store().run_grants(held.run.id()), Ok(grants) if !grants.is_empty()),
            "{edits:?} produced a grant"
        );
    }
}

/// The canonical spelling of `at(9)`, for a hand-edited `resolved_at`.
const RESOLVED_AT: &str = "2023-11-14T22:13:29.000000000Z";

#[test]
fn a_granted_row_may_not_claim_a_breadth_its_decision_did_not_authorize() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::Execute)
            .requesting(ApprovalScope::ToolForRun),
    );
    held.store()
        .decide_approval(
            request.id(),
            // The human narrowed to one call, so the record and its grant are
            // exact-call.
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(6),
            ),
        )
        .unwrap();
    assert_eq!(held.store().run_grants(held.run.id()).unwrap().len(), 1);

    // Widening the record back to what was *asked for* is exactly the edit the
    // ceiling check alone would allow, because `tool_for_run` is what the
    // request wanted. The decision is what refuses it.
    guard(&held.store().writer)
        .execute(
            "UPDATE approvals SET effective_scope = 'tool_for_run' WHERE id = ?1",
            rusqlite::params![request.id().to_string()],
        )
        .unwrap();

    let error = held.store().approval(request.id()).unwrap_err();
    assert_eq!(error.kind(), "approval_refused");
    assert!(
        error.to_string().contains("its decision authorized"),
        "{error}"
    );
}

#[test]
fn a_future_approval_row_reads_as_an_upgrade_request() {
    let held = Held::new();
    let request = held.open(held.pending(RiskLevel::Execute));
    guard(&held.store().writer)
        .execute(
            "UPDATE approvals SET schema_version = ?1 WHERE id = ?2",
            rusqlite::params![
                i64::from(crate::domain::RUNTIME_RECORD_SCHEMA_VERSION) + 1,
                request.id().to_string()
            ],
        )
        .unwrap();

    let error = held.store().approval(request.id()).unwrap_err();
    assert_eq!(error.kind(), "invalid_record");
    assert!(error.to_string().contains("upgrade Harkness"), "{error}");
}

#[test]
fn a_v2_approval_row_cannot_claim_v3_external_identity_fields() {
    let held = Held::new();
    let request = held.open(
        held.pending(RiskLevel::Execute)
            .with_capabilities([Capability::new("invoke_mcp_tool").unwrap()])
            .with_integration_identity(
                IntegrationIdentity::none()
                    .with_mcp_tool_schema_fingerprint(Sha256Hash::of("schema")),
            ),
    );
    guard(&held.store().writer)
        .execute(
            "UPDATE approvals SET schema_version = 2 WHERE id = ?1",
            rusqlite::params![request.id().to_string()],
        )
        .unwrap();

    let error = held.store().approval(request.id()).unwrap_err();
    assert_eq!(error.kind(), "approval_refused");
    assert!(
        error
            .to_string()
            .contains("schema versions before 3 cannot carry external integration identity"),
        "{error}"
    );
    assert!(!matches!(held.store().run_grants(held.run.id()), Ok(grants) if !grants.is_empty()));
}

// -- workspace snapshots -----------------------------------------------------

/// A hermetic worktree and one capture of it.
///
/// [`WorkspaceSnapshot`] has exactly one constructor — reading a real
/// workspace — so this builds one rather than hand-writing a wire form. Doing
/// it by hand would also mean hand-writing the composite digest the load path
/// re-derives, which is the very check the store depends on.
struct CapturedWorkspace {
    fixture: harkness_test_fixtures::Fixture,
    root: PathBuf,
    snapshot: WorkspaceSnapshot,
}

impl CapturedWorkspace {
    fn new() -> Self {
        let fixture = harkness_test_fixtures::Fixture::new();
        let root = fixture.directory("workspace");
        harkness_test_fixtures::initialize_repository(&root);
        let snapshot = capture_at(&fixture, &root);
        Self {
            fixture,
            root,
            snapshot,
        }
    }

    /// Reads the same workspace again, after a test has changed it.
    fn recapture(&self) -> WorkspaceSnapshot {
        capture_at(&self.fixture, &self.root)
    }
}

fn capture_at(fixture: &harkness_test_fixtures::Fixture, root: &Path) -> WorkspaceSnapshot {
    WorkspaceSnapshot::capture(
        &CaptureRequest::new(harkness_core::ProjectId::new()).with_index_generation(9),
        &harkness_git::GitService::new(root, &fixture.data_dir),
        &FilesystemProbe::new(root),
        &harkness_git::Cancellation::default(),
    )
    .unwrap()
}

fn stored_payload(store: &Store, snapshot: &WorkspaceSnapshot) -> String {
    store
        .writer_for_test()
        .query_row(
            "SELECT payload_json FROM workspace_snapshots WHERE id = ?1",
            [snapshot.id().to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
}

/// The row and the event that announces it commit together. A capture the
/// timeline does not mention is a run whose context arrived from nowhere.
#[test]
fn recording_a_run_snapshot_writes_its_row_and_its_event_together() {
    let fixture = Fixture::new();
    let task = stored_task(&fixture.store);
    let run = stored_run(&fixture.store, &task);
    let workspace = CapturedWorkspace::new();

    let seq = fixture
        .store
        .record_workspace_snapshot_for_run(run.id(), &workspace.snapshot)
        .unwrap();

    assert_eq!(seq, EventSeq::FIRST);
    let events = fixture.store.events(run.id(), None, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.kind(), &EventKind::SnapshotCaptured);
    assert_eq!(
        events[0].event.payload(),
        &json!({
            "snapshot_id": workspace.snapshot.id().to_string(),
            "snapshot_digest": workspace.snapshot.digest().to_string(),
        })
    );

    let stored = fixture
        .store
        .workspace_snapshot(workspace.snapshot.id())
        .unwrap()
        .unwrap();
    assert_eq!(stored.run_id, Some(run.id()));
    assert_eq!(stored.snapshot, workspace.snapshot);
    assert_eq!(
        fixture.store.run_workspace_snapshots(run.id()).unwrap(),
        vec![stored]
    );
}

/// A capture that belongs to nobody has no timeline to be announced on, and
/// inventing a run to hold one would be worse than recording none.
#[test]
fn a_standalone_snapshot_is_recorded_without_a_run_or_an_event() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();

    fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap();

    let stored = fixture
        .store
        .workspace_snapshot(workspace.snapshot.id())
        .unwrap()
        .unwrap();
    assert_eq!(stored.run_id, None);
    assert_eq!(stored.snapshot.digest(), workspace.snapshot.digest());
}

/// The column is the frozen `harkness-context` wire form, so a value written by
/// this build and read back by it must be the same bytes — that is what makes a
/// future format change a migration rather than a silent rewrite.
#[test]
fn a_snapshot_payload_round_trips_byte_identically() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();
    fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap();

    let stored = fixture
        .store
        .workspace_snapshot(workspace.snapshot.id())
        .unwrap()
        .unwrap();

    let payload = stored_payload(&fixture.store, &workspace.snapshot);
    assert_eq!(
        payload,
        serde_json::to_string(&SnapshotWireRef::from(&stored.snapshot)).unwrap()
    );
    assert_eq!(
        payload,
        serde_json::to_string(&SnapshotWireRef::from(&workspace.snapshot)).unwrap()
    );
}

#[test]
fn a_snapshot_naming_an_unstored_run_is_refused() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();

    let error = fixture
        .store
        .record_workspace_snapshot_for_run(
            RunId::from_str(FIXTURE_RUN_ID).unwrap(),
            &workspace.snapshot,
        )
        .unwrap_err();

    assert_eq!(error.kind(), "missing_parent");
    assert!(
        fixture
            .store
            .workspace_snapshot(workspace.snapshot.id())
            .unwrap()
            .is_none(),
        "a refused write must leave no row behind"
    );
}

#[test]
fn recording_one_capture_twice_is_refused() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();
    fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap();

    let error = fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap_err();

    assert_eq!(error.kind(), "already_exists");
}

/// The denormalized columns are compared against the payload, never trusted. A
/// hand-edited digest would otherwise make a search by workspace identity
/// return a capture of a different workspace.
#[test]
fn a_snapshot_row_whose_digest_column_was_edited_fails_to_load() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();
    fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap();

    fixture
        .store
        .writer_for_test()
        .execute(
            "UPDATE workspace_snapshots SET snapshot_digest = ?1",
            [format!("{:0>64}", "b")],
        )
        .unwrap();

    let error = fixture
        .store
        .workspace_snapshot(workspace.snapshot.id())
        .unwrap_err();

    assert_eq!(error.kind(), "column_encoding");
    assert!(error.to_string().contains("snapshot_digest"), "{error}");
}

/// The payload carries its own schema version, probed before its body, so a
/// document from a newer build reads as an upgrade request rather than as a
/// corrupt column.
#[test]
fn a_snapshot_payload_from_a_newer_build_reads_as_an_upgrade_request() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();
    fixture
        .store
        .record_workspace_snapshot(&workspace.snapshot)
        .unwrap();
    let mut payload: Value =
        serde_json::from_str(&stored_payload(&fixture.store, &workspace.snapshot)).unwrap();
    payload["schema_version"] = json!(CONTEXT_RECORD_SCHEMA_VERSION + 1);
    fixture
        .store
        .writer_for_test()
        .execute(
            "UPDATE workspace_snapshots SET payload_json = ?1",
            [payload.to_string()],
        )
        .unwrap();

    let error = fixture
        .store
        .workspace_snapshot(workspace.snapshot.id())
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_context_record");
    assert!(error.to_string().contains("upgrade Harkness"), "{error}");
}

/// A snapshot is caller data in a column, so it is held to the same inline
/// bound every other one keeps. Refusing is the honest answer: a snapshot that
/// recorded only some of a workspace's paths would claim an identity the
/// workspace never had.
#[test]
fn an_oversized_snapshot_is_refused_rather_than_truncated() {
    let fixture = Fixture::new();
    let workspace = CapturedWorkspace::new();
    for index in 0..900 {
        std::fs::write(workspace.root.join(format!("f{index:04}")), b"x").unwrap();
    }
    let large = workspace.recapture();
    assert!(
        serde_json::to_string(&SnapshotWireRef::from(&large))
            .unwrap()
            .len()
            > MAX_INLINE_PAYLOAD_BYTES,
        "the fixture workspace must produce an oversized payload"
    );

    let error = fixture.store.record_workspace_snapshot(&large).unwrap_err();

    assert_eq!(error.kind(), "payload_too_large");
    assert!(
        fixture
            .store
            .workspace_snapshot(large.id())
            .unwrap()
            .is_none(),
        "a refused write must leave no row behind"
    );
}

// -- migration from a frozen database ---------------------------------------

/// The frozen v2 database committed beside this module.
///
/// Written by `regenerate_the_frozen_v2_fixture`. Its artifact file is
/// deliberately *not* committed beside it: an artifact whose bytes are gone is
/// exactly the case a restored database has to survive.
const FROZEN_V2_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v2.db");

/// The frozen v3 database carrying one exact workspace trust decision.
const FROZEN_V3_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v3.db");

/// The frozen v4 database carrying one binding policy decision.
const FROZEN_V4_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v4.db");

/// The frozen v5 database carrying one pending approval and one grant.
const FROZEN_V5_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v5.db");

/// The frozen v6 database carrying an external identity-bound approval.
const FROZEN_V6_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v6.db");

/// The frozen v7 database carrying a dead lease and the retry that followed it.
const FROZEN_V7_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v7.db");

/// The frozen v8 database carrying one run's workspace snapshot.
const FROZEN_V8_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v8.db");
/// The frozen v9 database carrying one trusted agent and what was observed of it.
const FROZEN_V9_DATABASE: &[u8] = include_bytes!("fixtures/runtime-v9.db");

/// The agent registration and trust-record identities the v9 fixture was
/// written with.
const FIXTURE_AGENT_ID: &str = "gemini-cli";
const FIXTURE_TRUST_RECORD_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
/// The digest the v9 fixture's grant is bound to.
const FIXTURE_AGENT_DIGEST: &str = "frozen agent executable";

/// The lease and retry identities the v7 fixture was written with.
const FIXTURE_LEASE_ID: &str = "99999999-9999-4999-8999-999999999999";
const FIXTURE_RETRY_RUN_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

/// The approval identities the v5 fixture was written with.
const FIXTURE_PENDING_APPROVAL_ID: &str = "77777777-7777-4777-8777-777777777777";
const FIXTURE_GRANTED_APPROVAL_ID: &str = "88888888-8888-4888-8888-888888888888";

/// A later migration, standing in for a future store feature.
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
        statements: include_str!("migrations/003_workspace_trust.sql"),
    },
    Migration {
        version: 4,
        statements: include_str!("migrations/004_policy_decisions.sql"),
    },
    Migration {
        version: 5,
        statements: include_str!("migrations/005_approvals.sql"),
    },
    Migration {
        version: 6,
        statements: include_str!("migrations/006_approval_integration_identity.sql"),
    },
    Migration {
        version: 7,
        statements: include_str!("migrations/007_run_leases_and_retry.sql"),
    },
    Migration {
        version: 8,
        statements: include_str!("migrations/008_workspace_snapshots.sql"),
    },
    Migration {
        version: 9,
        statements: include_str!("migrations/009_agent_registry.sql"),
    },
    Migration {
        version: 10,
        statements: "ALTER TABLE runs ADD COLUMN cancelled_by TEXT;",
    },
];

fn restore_frozen_database(bytes: &[u8]) -> TempDir {
    let data_dir = TempDir::new().unwrap();
    std::fs::write(data_dir.path().join(DATABASE_FILE), bytes).unwrap();
    data_dir
}

#[test]
fn a_v1_database_migrates_to_current_and_still_reads_its_existing_runs() {
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
        SCHEMA_VERSION,
        "opening must apply the recorded migration ladder"
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
fn a_frozen_v3_database_opens_and_reads_its_workspace_trust() {
    let data_dir = restore_frozen_database(FROZEN_V3_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();
    let project_id =
        harkness_core::ProjectId::from_str("55555555-5555-4555-8555-555555555555").unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );
    let trust = store.workspace_trust(project_id).unwrap().unwrap();
    assert_eq!(trust.project_id(), project_id);
    assert_eq!(trust.state(), TrustState::Trusted);
    assert_eq!(trust.canonical_root(), Path::new("/workspace/harkness"));
    assert_eq!(
        trust.resolve(project_id, trust.canonical_root()),
        TrustState::Untrusted,
        "the fixture's temporary workspace is gone, so its old path is not an active trust grant"
    );
}

#[test]
fn a_frozen_v4_database_opens_and_reads_its_policy_decision() {
    let data_dir = restore_frozen_database(FROZEN_V4_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );
    let call = store
        .load_tool_call(ToolCallId::from_str(FIXTURE_TOOL_CALL_ID).unwrap())
        .unwrap();
    let decision = call.policy_decision().unwrap();
    assert_eq!(call.state(), ToolCallState::AwaitingApproval);
    assert_eq!(decision.verdict(), PolicyVerdict::Ask);
    assert_eq!(decision.reason(), "fixture policy decision: ask");
}

#[test]
fn a_frozen_v5_database_opens_and_reads_its_approvals() {
    let data_dir = restore_frozen_database(FROZEN_V5_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();

    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );

    // The pending question survived, with the binding fields a grant is matched
    // on. This is the shape a restart finds.
    let pending = store.pending_approvals().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id().to_string(), FIXTURE_PENDING_APPROVAL_ID);
    assert_eq!(pending[0].state(), ApprovalState::Pending);
    assert_eq!(pending[0].risk(), RiskLevel::RemoteWrite);
    assert_eq!(pending[0].requested_scope(), ApprovalScope::ToolForRun);
    assert_eq!(
        pending[0].effective_scope(),
        ApprovalScope::ExactCall,
        "the stored record must still show the risk ceiling's downgrade"
    );
    assert!(pending[0].decision().is_none());

    let run_id = RunId::from_str(FIXTURE_RUN_ID).unwrap();
    let granted = store
        .approval(FIXTURE_GRANTED_APPROVAL_ID.parse().unwrap())
        .unwrap();
    assert_eq!(granted.state(), ApprovalState::Granted);
    assert_eq!(granted.decision().unwrap().decided_via(), DecidedVia::Gui);
    assert_eq!(
        granted.decision().unwrap().reason(),
        Some("frozen fixture grant")
    );

    // The grant still covers exactly the call it was given for.
    let grants = store.run_grants(run_id).unwrap();
    assert_eq!(grants.len(), 1);
    let tool = ToolIdentity::parse("fs.write", "1.2.0").unwrap();
    let workspace = approval_workspace();
    let covered = CandidateCall::new(
        run_id,
        ToolCallId::from_str(FIXTURE_TOOL_CALL_ID).unwrap(),
        &workspace,
        &tool,
        canonical_input_hash(&approval_input()).unwrap(),
    );
    assert_eq!(matching_grants(&grants, &covered).len(), 1);
    assert_eq!(store.run_approvals(run_id).unwrap().len(), 2);
}

#[test]
fn a_frozen_v6_database_opens_and_reads_its_external_identity_binding() {
    let data_dir = restore_frozen_database(FROZEN_V6_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();
    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );

    let request = store
        .approval(FIXTURE_GRANTED_APPROVAL_ID.parse().unwrap())
        .unwrap();
    let identity = IntegrationIdentity::none()
        .with_mcp_tool_schema_fingerprint(Sha256Hash::of("frozen schema"));
    assert_eq!(request.integration_identity(), identity);

    let grants = store.run_grants(request.run_id()).unwrap();
    let workspace = approval_workspace();
    let tool = ToolIdentity::parse("fs.write", "1.2.0").unwrap();
    let capabilities = [Capability::new("invoke_mcp_tool").unwrap()];
    let candidate = CandidateCall::new(
        request.run_id(),
        request.tool_call_id(),
        &workspace,
        &tool,
        canonical_input_hash(&approval_input()).unwrap(),
    )
    .with_capabilities(&capabilities)
    .with_integration_identity(identity);
    assert_eq!(matching_grants(&grants, &candidate).len(), 1);
    let drifted = matching_grants_detailed(
        &grants,
        &candidate.with_integration_identity(
            IntegrationIdentity::none()
                .with_mcp_tool_schema_fingerprint(Sha256Hash::of("changed schema")),
        ),
    );
    assert!(drifted.grants().is_empty());
    assert_eq!(drifted.identity_drifts().len(), 1);
}

#[test]
fn a_frozen_v7_database_opens_and_reads_its_lease_and_retry_provenance() {
    let data_dir = restore_frozen_database(FROZEN_V7_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();
    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );

    // The interrupted attempt kept its lease, so a reader can still say which
    // process drove it even though that process is long gone.
    let original = RunId::from_str(FIXTURE_RUN_ID).unwrap();
    let lease_id = FIXTURE_LEASE_ID.parse::<crate::domain::LeaseId>().unwrap();
    let lease = store.lease(lease_id).unwrap().unwrap();
    assert_eq!(lease.id(), lease_id);
    assert!(
        lease.is_released(),
        "the sweep that ended its run must have written the claim off"
    );
    let interrupted = store.load_run(original).unwrap();
    assert_eq!(interrupted.state(), ExecutionState::Interrupted);
    assert_eq!(interrupted.retry_of(), None);
    assert!(!interrupted.workspace_may_be_modified());

    let retry = store
        .load_run(RunId::from_str(FIXTURE_RETRY_RUN_ID).unwrap())
        .unwrap();
    assert_eq!(retry.retry_of(), Some(original));
    assert!(
        retry.workspace_may_be_modified(),
        "the frozen retry pins the warning a front end has to surface"
    );
    assert_eq!(store.retries_of(original).unwrap(), vec![retry.id()]);

    // A released lease is a claim nobody is making, so nothing about it makes
    // the retry look owned.
    assert!(store.live_leases().unwrap().is_empty());
    assert_eq!(
        store.unfinished_runs().unwrap(),
        vec![(retry.id(), None)],
        "only the queued retry is still unfinished, and it names no live claim"
    );
}

/// The evidence half of ADR-0004's split, pinned: a snapshot recorded by an
/// earlier build still loads, still re-derives its own identity, and is still
/// tied to the run and the timeline entry that announced it.
#[test]
fn a_frozen_v8_database_opens_and_reads_its_workspace_snapshot() {
    let data_dir = restore_frozen_database(FROZEN_V8_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();
    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );

    let run_id = RunId::from_str(FIXTURE_RUN_ID).unwrap();
    let recorded = store.run_workspace_snapshots(run_id).unwrap();
    assert_eq!(recorded.len(), 1);
    let stored = &recorded[0];
    assert_eq!(stored.run_id, Some(run_id));

    // Loading re-derives all three content digests and the composite from the
    // entry lists, so reaching this line is the assertion: the frozen payload
    // still supports the identity it claims.
    let snapshot = &stored.snapshot;
    assert_eq!(snapshot.index_generation(), 9);
    assert!(snapshot.head().is_some());
    assert_eq!(
        store.workspace_snapshot(snapshot.id()).unwrap().as_ref(),
        Some(stored)
    );

    let kinds = store
        .events(run_id, None, 10)
        .unwrap()
        .into_iter()
        .map(|stored| stored.event.kind().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["snapshot_captured"]);

    // The other side of the split: deleting the whole context cache is what a
    // user is told to do, and it must leave this database untouched. There is
    // nothing to delete here because the engine never wrote to it — which is
    // the property, stated as an assertion.
    assert!(
        !data_dir
            .path()
            .join(harkness_core::CONTEXT_DIRECTORY)
            .exists(),
        "the run store must hold no context cache"
    );
}

#[test]
fn a_frozen_v9_database_opens_and_reads_its_agent_grant_and_observations() {
    use crate::agent_registry::{AgentId, AuthStatus, CompatibilityStatus, HealthStatus};
    use crate::integration::{SubjectKind, TrustCheck, TrustState};

    let data_dir = restore_frozen_database(FROZEN_V9_DATABASE);
    let store = Store::open(data_dir.path()).unwrap();
    assert_eq!(
        recorded_version(&guard(&store.writer)).unwrap(),
        SCHEMA_VERSION
    );

    let stored = store
        .latest_trust_record(SubjectKind::AgentExecutable, FIXTURE_AGENT_ID)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.id(),
        FIXTURE_TRUST_RECORD_ID
            .parse::<crate::integration::TrustRecordId>()
            .unwrap()
    );
    assert_eq!(stored.subject_ref(), FIXTURE_AGENT_ID);
    let record = stored.record();
    assert_eq!(record.state(), TrustState::Trusted);
    assert_eq!(
        record
            .identity_basis()
            .executable()
            .map(crate::integration::ExecutableIdentity::sha256),
        Some(Sha256Hash::of(FIXTURE_AGENT_DIGEST))
    );

    // The frozen grant still answers the question it exists to answer: the same
    // bytes are valid, and different bytes at the same path are not.
    let unchanged = crate::integration::ObservedIdentity::new(
        crate::integration::IdentityBasis::new(
            "Gemini CLI",
            crate::integration::ConfigurationSource::User,
        )
        .unwrap()
        .launched_from(
            crate::integration::ExecutableIdentity::new(
                "/usr/bin/gemini",
                Sha256Hash::of(FIXTURE_AGENT_DIGEST),
            )
            .unwrap(),
        ),
    );
    assert_eq!(record.check(&unchanged), TrustCheck::Valid);
    let replaced = crate::integration::ObservedIdentity::new(
        crate::integration::IdentityBasis::new(
            "Gemini CLI",
            crate::integration::ConfigurationSource::User,
        )
        .unwrap()
        .launched_from(
            crate::integration::ExecutableIdentity::new(
                "/usr/bin/gemini",
                Sha256Hash::of("a different program"),
            )
            .unwrap(),
        ),
    );
    assert_eq!(
        record.check(&replaced),
        TrustCheck::Invalidate(crate::integration::InvalidationReason::ExecutableHashChanged)
    );

    let observations = store
        .agent_observations(&AgentId::new(FIXTURE_AGENT_ID).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(observations.auth_status(), AuthStatus::NotRequired);
    assert_eq!(
        observations.compatibility(),
        CompatibilityStatus::Compatible
    );
    let initialize = observations.last_initialize().unwrap();
    assert_eq!(initialize.protocol_version(), 1);
    assert!(initialize.capabilities().load_session);
    assert!(initialize.capabilities().session_resume);
    assert!(!initialize.capabilities().session_close);
    assert_eq!(
        observations.last_health().unwrap().status(),
        HealthStatus::Healthy
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

    assert_eq!(recorded_version(&connection).unwrap(), 10);
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
    super::repository::insert_run(&connection, &run, None).unwrap();
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
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..2]);
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

/// Writes the frozen v3 fixture, including one exact trust decision.
///
/// Run deliberately, and only when migration 3 itself changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v3_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 3 changes"]
fn regenerate_the_frozen_v3_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..3]);
    let project_id =
        harkness_core::ProjectId::from_str("55555555-5555-4555-8555-555555555555").unwrap();
    store
        .put_workspace_trust(&WorkspaceTrust::from_stored(
            project_id,
            PathBuf::from("/workspace/harkness"),
            TrustState::Trusted,
            at(14),
        ))
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v3.db");
}

/// Writes the frozen v4 fixture, including one binding policy decision.
///
/// Run deliberately, and only when migration 4 changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v4_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 4 changes"]
fn regenerate_the_frozen_v4_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..4]);
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);
    store
        .apply_tool_call_policy_decision(call.id(), policy_decision(PolicyVerdict::Ask), at(15))
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v4.db");
}

/// Writes the frozen v5 fixture: one pending approval and one live grant.
///
/// Run deliberately, and only when migration 5 changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v5_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 5 changes"]
fn regenerate_the_frozen_v5_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..5]);
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);

    let held = |id: &str, risk, scope| {
        ApprovalRequest::open_with_id(
            id.parse().unwrap(),
            PendingApproval::new(
                run.id(),
                call.id(),
                ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
                canonical_input_hash(&approval_input()).unwrap(),
                approval_workspace(),
                risk,
                at(4),
            )
            .requesting(scope)
            .with_capabilities([Capability::new("fs.write").unwrap()])
            .summarized_as("write 12 lines to src/lib.rs"),
        )
        .unwrap()
    };

    // A remote write that asked for a run-wide scope, so the fixture pins the
    // ceiling's downgrade as well as the pending state.
    store
        .open_approval(held(
            FIXTURE_PENDING_APPROVAL_ID,
            RiskLevel::RemoteWrite,
            ApprovalScope::ToolForRun,
        ))
        .unwrap();

    let granted = held(
        FIXTURE_GRANTED_APPROVAL_ID,
        RiskLevel::WorkspaceWrite,
        ApprovalScope::ExactCall,
    );
    store.open_approval(granted.clone()).unwrap();
    store
        .decide_approval(
            granted.id(),
            ApprovalDecision::grant(
                granted.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(7),
            )
            .because("frozen fixture grant"),
        )
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v5.db");
}

/// Writes the frozen v6 fixture with an MCP schema-bound grant.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 6 changes"]
fn regenerate_the_frozen_v6_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..6]);
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let step = stored_step(&store, &run);
    let call = stored_tool_call(&store, &step);
    let identity = IntegrationIdentity::none()
        .with_mcp_tool_schema_fingerprint(Sha256Hash::of("frozen schema"));
    let request = ApprovalRequest::open_with_id(
        FIXTURE_GRANTED_APPROVAL_ID.parse().unwrap(),
        PendingApproval::new(
            run.id(),
            call.id(),
            ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
            canonical_input_hash(&approval_input()).unwrap(),
            approval_workspace(),
            RiskLevel::Execute,
            at(4),
        )
        .with_capabilities([Capability::new("invoke_mcp_tool").unwrap()])
        .with_integration_identity(identity)
        .summarized_as("invoke an imported MCP tool"),
    )
    .unwrap();
    store.open_approval(request.clone()).unwrap();
    store
        .decide_approval(
            request.id(),
            ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(7),
            ),
        )
        .unwrap();
    freeze(&guard(&store.writer), "runtime-v6.db");
}

/// Writes the frozen v7 fixture: a dead lease, the run it abandoned, and the
/// retry that followed.
///
/// Run deliberately, and only when migration 7 changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v7_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 7 changes"]
fn regenerate_the_frozen_v7_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..7]);
    let task = stored_task(&store);
    let lease =
        crate::store::LeaseRecord::acquired(FIXTURE_LEASE_ID.parse().unwrap(), 4_242, at(1));
    let run = Run::with_id(RunId::from_str(FIXTURE_RUN_ID).unwrap(), task.id(), at(1));
    store
        .insert_run_with_event(
            &run,
            Some(&lease),
            RunEvent::new(EventKind::RunStateChanged, at(1))
                .with_payload(json!({"state": "queued"})),
        )
        .unwrap();
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

    // Exactly the shape a restart finds and ends.
    store
        .interrupt_run(
            run.id(),
            Some(lease.id()),
            crate::store::InterruptionReason::LeaseLockReleased,
            at(20),
        )
        .unwrap()
        .unwrap();

    let retry = Run::retrying_with_id(
        RunId::from_str(FIXTURE_RETRY_RUN_ID).unwrap(),
        task.id(),
        run.id(),
        true,
        at(30),
    );
    store
        .insert_run_with_event(
            &retry,
            None,
            RunEvent::new(EventKind::RunStateChanged, at(30))
                .with_payload(json!({"state": "queued"})),
        )
        .unwrap();
    store
        .append_event(
            run.id(),
            RunEvent::new(EventKind::RunRetried, at(30)).with_payload(json!({
                "retry_run_id": retry.id().to_string(),
                "workspace_may_be_modified": true,
            })),
        )
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v7.db");
}

/// Writes the frozen v8 fixture: one run and the workspace it read.
///
/// Run deliberately, and only when migration 8 changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v8_fixture -- --ignored`.
///
/// The capture's identity is not pinned by a constant the way a task or a lease
/// id is, and cannot be: a snapshot's id is minted per capture and its digest
/// covers a temporary worktree root. That is what the fixture is *for* — the
/// load path re-derives the digest from the payload, so a frozen document that
/// no longer supports its own identity fails the test whatever the values are.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 8 changes"]
fn regenerate_the_frozen_v8_fixture() {
    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..8]);
    let task = stored_task(&store);
    let run = stored_run(&store, &task);
    let workspace = CapturedWorkspace::new();
    store
        .record_workspace_snapshot_for_run(run.id(), &workspace.snapshot)
        .unwrap();

    freeze(&guard(&store.writer), "runtime-v8.db");
}

/// Writes the frozen v9 fixture: one trusted agent, and what was observed of it.
///
/// Run deliberately, and only when migration 9 changes:
/// `cargo test -p harkness-runtime regenerate_the_frozen_v9_fixture -- --ignored`.
#[test]
#[ignore = "rewrites a committed fixture; run only when migration 9 changes"]
fn regenerate_the_frozen_v9_fixture() {
    use crate::agent_registry::{
        AgentCapabilitySnapshot, AgentId, AgentObservations, AgentTeardown, HealthRecord,
        HealthStatus, InitializeRecord,
    };
    use crate::integration::{
        ConfigurationSource, ExecutableIdentity, IdentityBasis, SubjectKind, TrustRecord,
        TrustRecordId, TrustScope,
    };

    let data_dir = TempDir::new().unwrap();
    let store = store_with_migrations(data_dir.path(), &MIGRATIONS[..9]);

    let executable =
        ExecutableIdentity::new("/usr/bin/gemini", Sha256Hash::of(FIXTURE_AGENT_DIGEST)).unwrap();
    let basis = IdentityBasis::new("Gemini CLI", ConfigurationSource::User)
        .unwrap()
        .launched_from(executable);
    let record = TrustRecord::grant(
        SubjectKind::AgentExecutable,
        basis,
        TrustScope::Global,
        at(1),
    )
    .unwrap();
    store
        .insert_trust_record(
            TrustRecordId::from_str(FIXTURE_TRUST_RECORD_ID).unwrap(),
            SubjectKind::AgentExecutable,
            FIXTURE_AGENT_ID,
            &record,
            at(1),
        )
        .unwrap();

    let agent = AgentId::new(FIXTURE_AGENT_ID).unwrap();
    let mut observations = AgentObservations::unobserved(at(1));
    observations.record_initialize(
        InitializeRecord::new(
            None,
            1,
            AgentCapabilitySnapshot {
                load_session: true,
                session_resume: true,
                ..AgentCapabilitySnapshot::default()
            },
            at(2),
        ),
        at(2),
    );
    observations.record_health(
        HealthRecord::succeeded(
            HealthStatus::Healthy,
            std::time::Duration::from_millis(120),
            at(2),
        )
        .torn_down(AgentTeardown::ClosedStdin),
        at(2),
    );
    store.put_agent_observations(&agent, &observations).unwrap();

    freeze(&guard(&store.writer), "runtime-v9.db");
}

/// Builds a store stopped at an older migration for fixture regeneration.
fn store_with_migrations(data_dir: &std::path::Path, migrations: &[Migration]) -> Store {
    std::fs::create_dir_all(data_dir).unwrap();
    let path = data_dir.join(DATABASE_FILE);
    let mut connection = super::connect(&path).unwrap();
    super::enable_wal(&connection).unwrap();
    apply(&mut connection, migrations).unwrap();
    Store {
        data_dir: data_dir.to_path_buf(),
        path,
        writer: Mutex::new(connection),
        readers: Mutex::new(Vec::new()),
        redactor: Arc::new(super::PassThrough),
    }
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
    assert_eq!(MIGRATIONS.len(), 9, "add coverage for a new migration");
    assert_eq!(SCHEMA_VERSION, 9);
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
