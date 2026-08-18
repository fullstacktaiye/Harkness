use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use harkness_context::index::{IndexAvailability, RecreationReason};
use harkness_core::ProjectId;
use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, initialize_repository};
use time::OffsetDateTime;

use super::{ContextEngines, cache_recreated_event};
use crate::domain::{Run, Task};
use crate::store::{EventKind, Store};

struct Workspace {
    fixture: Fixture,
    root: PathBuf,
    project_id: ProjectId,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let fixture = Fixture::new();
        let root = fixture.directory(name);
        initialize_repository(&root);
        Self {
            fixture,
            root,
            project_id: ProjectId::new(),
        }
    }

    fn registry(&self) -> ContextEngines {
        ContextEngines::new(&self.fixture.data_dir)
    }
}

/// One project, one handle. Two front ends asking twice must not end up
/// answering context questions from two different index states.
#[test]
fn a_project_gets_one_engine_however_many_times_it_is_asked_for() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();

    let first = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    let second = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(registry.len(), 1);
    assert_eq!(first.index_generation(), second.index_generation());
}

#[test]
fn concurrent_callers_converge_on_one_engine() {
    let workspace = Workspace::new("workspace");
    let registry = Arc::new(workspace.registry());

    let engines = thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let root = workspace.root.clone();
                let project_id = workspace.project_id;
                scope.spawn(move || {
                    registry
                        .engine(project_id, &root, &Cancellation::default())
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    assert_eq!(registry.len(), 1);
    let first = &engines[0];
    for engine in &engines {
        assert!(
            Arc::ptr_eq(first, engine),
            "every caller must leave with the handle the project shares"
        );
    }
}

/// The two worktrees are two catalog entries, so they are two engines. What
/// they must share is the expensive half — one cache per repository.
#[test]
fn two_worktrees_of_one_repository_get_two_engines_over_one_cache() {
    let fixture = Fixture::new();
    let main_root = fixture.directory("main-checkout");
    let repository = initialize_repository(&main_root);
    let linked_root = fixture.root.path().join("linked-checkout");
    repository
        .worktree(
            "linked",
            &linked_root,
            Some(&harkness_git::git2::WorktreeAddOptions::new()),
        )
        .unwrap();
    let registry = ContextEngines::new(&fixture.data_dir);

    let main = registry
        .engine(ProjectId::new(), &main_root, &Cancellation::default())
        .unwrap();
    let linked = registry
        .engine(ProjectId::new(), &linked_root, &Cancellation::default())
        .unwrap();

    assert_eq!(registry.len(), 2);
    assert!(!Arc::ptr_eq(&main, &linked));
    assert_eq!(main.cache_root(), linked.cache_root());
    assert_eq!(main.index_generation(), linked.index_generation());
}

/// A checkout that was repaired or moved must not be answered about at the path
/// it used to be at.
#[test]
fn a_project_whose_worktree_moved_gets_a_fresh_engine() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();
    let held = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    let moved = workspace.fixture.directory("moved-workspace");
    initialize_repository(&moved);

    let reopened = registry
        .engine(workspace.project_id, &moved, &Cancellation::default())
        .unwrap();

    assert!(!Arc::ptr_eq(&held, &reopened));
    assert_eq!(reopened.worktree_root(), moved);
    assert_eq!(registry.len(), 1);
    assert!(
        Arc::ptr_eq(
            &reopened,
            &registry
                .engine(workspace.project_id, &moved, &Cancellation::default())
                .unwrap()
        ),
        "the replacement is the handle the project now shares"
    );
}

/// Releasing gives up the registry's reference; a caller already holding one
/// keeps working, which is what stops a call in flight losing its cache.
#[test]
fn releasing_a_project_drops_the_registrys_reference_and_nothing_else() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();
    let held = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();

    assert!(registry.release(workspace.project_id));
    assert!(!registry.release(workspace.project_id));
    assert!(registry.is_empty());

    // The caller's handle still answers.
    held.snapshot(&Cancellation::default()).unwrap();
    let reopened = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    assert!(!Arc::ptr_eq(&held, &reopened));
    assert_eq!(
        reopened.index_generation(),
        held.index_generation(),
        "reopening a cache is not rebuilding it"
    );
}

/// Two spellings of one checkout are one workspace. Keyed on the raw path they
/// would be two engines evicting each other, and a caller holding the earlier
/// `Arc` would answer from a different handle — the disagreement between front
/// ends this registry exists to prevent.
#[test]
fn two_spellings_of_one_worktree_share_the_projects_engine() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();
    let trailing = workspace.root.join("");
    let indirect = workspace.root.join("..").join("workspace");

    let held = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    let through_slash = registry
        .engine(workspace.project_id, &trailing, &Cancellation::default())
        .unwrap();
    let through_parent = registry
        .engine(workspace.project_id, &indirect, &Cancellation::default())
        .unwrap();

    assert!(Arc::ptr_eq(&held, &through_slash));
    assert!(Arc::ptr_eq(&held, &through_parent));
    assert_eq!(registry.len(), 1);
}

/// A configuration naming another data directory would put the cache outside
/// the tree `HARKNESS_DATA_DIR` covers while the registry went on reporting its
/// own for it.
#[test]
fn a_configuration_naming_another_data_directory_is_refused() {
    let workspace = Workspace::new("workspace");
    let elsewhere = workspace.fixture.directory("elsewhere");
    let registry = workspace.registry();

    let error = registry
        .engine_from(
            harkness_context::ContextEngineConfig::new(
                workspace.project_id,
                &workspace.root,
                &elsewhere,
            ),
            &Cancellation::default(),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "cache_open_failed");
    assert!(registry.is_empty());
    assert!(
        !elsewhere.join(harkness_core::CONTEXT_DIRECTORY).exists(),
        "a refused configuration must not leave a cache behind"
    );
}

#[test]
fn a_folder_that_is_not_a_repository_is_refused_and_not_remembered() {
    let fixture = Fixture::new();
    let plain = fixture.directory("just-a-folder");
    let registry = ContextEngines::new(&fixture.data_dir);

    let error = registry
        .engine(ProjectId::new(), &plain, &Cancellation::default())
        .unwrap_err();

    assert_eq!(error.kind(), "repository_unavailable");
    assert!(registry.is_empty());
}

/// The engine's caches all live beneath the registry's data directory, which is
/// what makes `HARKNESS_DATA_DIR` cover them and what makes deleting one
/// directory the whole recovery action.
#[test]
fn every_cache_lives_beneath_the_registrys_data_directory() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();

    let engine = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();

    assert!(engine.cache_root().starts_with(registry.data_dir()));
    assert_eq!(engine.index_status().availability, IndexAvailability::Ready);
}

/// The property ADR-0004 exists to guarantee, asserted across both stores:
/// deleting `<data_dir>/context/` costs warm-up time and nothing else.
#[test]
fn deleting_the_whole_context_directory_loses_no_run_evidence() {
    let workspace = Workspace::new("workspace");
    let store = Store::open(&workspace.fixture.data_dir).unwrap();
    let task = Task::new(
        "Read the workspace",
        &workspace.root,
        None,
        OffsetDateTime::UNIX_EPOCH,
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), OffsetDateTime::UNIX_EPOCH);
    store.insert_run(&run).unwrap();

    let registry = workspace.registry();
    let engine = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    let generation = engine.index_generation();
    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
    store
        .record_workspace_snapshot_for_run(run.id(), &snapshot)
        .unwrap();

    // Close every handle, then do what a user is told to do to reclaim disk.
    registry.release_all();
    drop(engine);
    std::fs::remove_dir_all(
        workspace
            .fixture
            .data_dir
            .join(harkness_core::CONTEXT_DIRECTORY),
    )
    .unwrap();

    let reopened = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();

    assert!(
        reopened.index_generation() > generation,
        "a cache rebuilt from nothing must not reissue a generation a stored snapshot recorded"
    );
    assert_eq!(
        reopened.index_status().availability,
        IndexAvailability::Ready
    );

    // Not one row of evidence moved.
    let recorded = store.run_workspace_snapshots(run.id()).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].snapshot, snapshot);
    assert_eq!(store.load_run(run.id()).unwrap().id(), run.id());
    assert_eq!(
        store
            .events(run.id(), None, 10)
            .unwrap()
            .into_iter()
            .map(|stored| stored.event.kind().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["snapshot_captured"]
    );

    // And the snapshot taken against the old cache is honestly stale now.
    let after = reopened.snapshot(&Cancellation::default()).unwrap();
    assert_ne!(after.digest(), snapshot.digest());
}

/// Capture reads the workspace and writes nothing. The event is the persistence
/// path's, so an engine that emitted one would be claiming an audit trail it
/// never stored.
#[test]
fn capturing_a_snapshot_persists_nothing_until_the_runtime_records_it() {
    let workspace = Workspace::new("workspace");
    let store = Store::open(&workspace.fixture.data_dir).unwrap();
    let task = Task::new(
        "Read the workspace",
        &workspace.root,
        None,
        OffsetDateTime::UNIX_EPOCH,
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), OffsetDateTime::UNIX_EPOCH);
    store.insert_run(&run).unwrap();
    let registry = workspace.registry();
    let engine = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();

    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();

    assert!(store.workspace_snapshot(snapshot.id()).unwrap().is_none());
    assert!(store.run_workspace_snapshots(run.id()).unwrap().is_empty());
    assert!(store.events(run.id(), None, 10).unwrap().is_empty());

    store
        .record_workspace_snapshot_for_run(run.id(), &snapshot)
        .unwrap();

    assert_eq!(
        store
            .events(run.id(), None, 10)
            .unwrap()
            .into_iter()
            .map(|stored| stored.event.kind().clone())
            .collect::<Vec<_>>(),
        [EventKind::SnapshotCaptured]
    );
}

#[test]
fn a_cache_rebuild_becomes_a_timeline_entry_with_both_generations() {
    let workspace = Workspace::new("workspace");
    let registry = workspace.registry();
    let engine = registry
        .engine(
            workspace.project_id,
            &workspace.root,
            &Cancellation::default(),
        )
        .unwrap();
    let previous = engine.index_generation();

    let recreation = engine.dispose_index(&Cancellation::default()).unwrap();
    let event = cache_recreated_event(&recreation, OffsetDateTime::UNIX_EPOCH);

    assert_eq!(event.kind(), &EventKind::ContextCacheRecreated);
    assert_eq!(
        event.payload()["reason"],
        serde_json::json!(RecreationReason::Disposed.as_str())
    );
    assert_eq!(
        event.payload()["previous_generation"],
        serde_json::json!(previous)
    );
    assert_eq!(
        event.payload()["generation"],
        serde_json::json!(engine.index_generation())
    );
    assert_eq!(event.payload()["quarantined"], serde_json::json!(false));
    assert!(
        event.payload()["generation"].is_number(),
        "a generation must not travel as a string the store's redaction may rewrite"
    );
}
