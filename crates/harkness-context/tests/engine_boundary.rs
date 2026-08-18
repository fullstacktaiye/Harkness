//! The context engine as an outside caller sees it.
//!
//! The unit tests beside the implementation can reach private state; these
//! cannot, which is the point. Everything [#114]–[#123] needs to plug into is a
//! public item, and a seam that only works from inside its own crate is not a
//! seam. This also stands where [#133] and [#136] will: no run store, no agent,
//! no model, and no network in the process.
//!
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114
//! [#123]: https://github.com/fullstacktaiye/harkness/issues/123
//! [#133]: https://github.com/fullstacktaiye/harkness/issues/133
//! [#136]: https://github.com/fullstacktaiye/harkness/issues/136

use std::fs;
use std::path::{Path, PathBuf};

use harkness_context::index::{
    INDEX_DATABASE_FILE, IndexAvailability, IndexCache, RecreationReason,
};
use harkness_context::{
    ChunkId, ContextEngine, ContextEngineConfig, ContextEngineError, InventoryRequest, MapRequest,
    PackRequest, RepoPath, SearchQuery, SymbolQuery,
};
use harkness_core::{CONTEXT_DIRECTORY, ProjectId};
use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, initialize_repository};

struct Workspace {
    fixture: Fixture,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let fixture = Fixture::new();
        let root = fixture.directory("workspace");
        initialize_repository(&root);
        Self { fixture, root }
    }

    fn engine(&self) -> ContextEngine {
        ContextEngine::open(
            ContextEngineConfig::new(ProjectId::new(), &self.root, &self.fixture.data_dir),
            &Cancellation::default(),
        )
        .unwrap()
    }

    fn cache_root(&self, engine: &ContextEngine) -> PathBuf {
        self.fixture
            .data_dir
            .join(CONTEXT_DIRECTORY)
            .join(engine.repository_key())
    }
}

/// The flagship claim of the boundary: an engine needs nothing else in the
/// process to answer what workspace this is.
#[test]
fn an_engine_serves_workspace_identity_with_nothing_else_in_the_process() {
    let workspace = Workspace::new();

    let engine = workspace.engine();
    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();

    assert_eq!(snapshot.project_id(), engine.project_id());
    assert_eq!(
        snapshot.worktree_root(),
        fs::canonicalize(&workspace.root).unwrap()
    );
    assert_eq!(snapshot.index_generation(), engine.index_generation());
    assert_eq!(engine.cache_root(), workspace.cache_root(&engine));
    assert!(engine.cache_root().join(INDEX_DATABASE_FILE).is_file());

    // And what that workspace offers as context, from the same standing start:
    // no runtime, no store, no model in the process.
    let inventory = engine
        .inventory(&InventoryRequest::new(), &Cancellation::default())
        .unwrap();
    assert!(
        inventory
            .entries()
            .iter()
            .any(|entry| entry.path.display() == "tracked.txt" && entry.eligible()),
        "{:?}",
        inventory.entries()
    );
    assert!(!inventory.is_truncated());
}

/// Every retrieval issue has a compiling seam today and a typed refusal until it
/// lands. Nothing panics and nothing fabricates a result.
#[test]
fn the_unimplemented_facade_methods_refuse_by_name_from_outside_the_crate() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    let chunk = ChunkId::derive(&RepoPath::from_path(Path::new("src/lib.rs")), "0", b"");

    let refusals = [
        engine
            .search(&SearchQuery::new("needle"), &cancellation)
            .map(|_| ())
            .unwrap_err(),
        engine
            .read_chunk(&chunk, &cancellation)
            .map(|_| ())
            .unwrap_err(),
        engine
            .symbols(&SymbolQuery::new("Thing"), &cancellation)
            .map(|_| ())
            .unwrap_err(),
        engine
            .repository_map(&MapRequest::new(), &cancellation)
            .map(|_| ())
            .unwrap_err(),
        engine.instructions(&cancellation).map(|_| ()).unwrap_err(),
        engine
            .build_pack(&PackRequest::new("fix the bug"), &cancellation)
            .map(|_| ())
            .unwrap_err(),
    ];

    for refusal in refusals {
        assert_eq!(refusal.kind(), "not_yet_available");
        assert!(
            matches!(refusal, ContextEngineError::NotYetAvailable { .. }),
            "{refusal}"
        );
    }
    assert!(ContextEngineError::kinds().contains(&"not_yet_available"));
}

/// Deleting the cache directory is the supported recovery action. It costs
/// warm-up time and moves the generation, and nothing else.
#[test]
fn deleting_the_cache_directory_leaves_a_working_engine_on_a_new_generation() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let generation = engine.index_generation();
    let cache_root = workspace.cache_root(&engine);
    drop(engine);

    fs::remove_dir_all(workspace.fixture.data_dir.join(CONTEXT_DIRECTORY)).unwrap();
    let engine = workspace.engine();

    assert_eq!(workspace.cache_root(&engine), cache_root);
    assert!(engine.index_generation() > generation);
    assert_eq!(engine.index_status().availability, IndexAvailability::Ready);
    engine.snapshot(&Cancellation::default()).unwrap();
}

/// The quarantine path, from outside: an unreadable cache is set aside, the
/// engine comes back up, and the recreation is visible where a UI polls.
#[test]
fn a_corrupt_cache_is_reported_through_index_status_and_replaced() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cache_root = workspace.cache_root(&engine);
    drop(engine);
    fs::write(
        cache_root.join(INDEX_DATABASE_FILE),
        &b"not a database"[..10],
    )
    .unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(cache_root.join(format!("{INDEX_DATABASE_FILE}{suffix}")));
    }

    let engine = workspace.engine();

    let status = engine.index_status();
    assert_eq!(status.availability, IndexAvailability::Ready);
    let recreation = status.last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Corrupt);
    let quarantined = recreation.quarantined_to.expect("quarantined bytes");
    assert!(quarantined.is_file());
    assert!(
        quarantined.starts_with(&cache_root),
        "a quarantined cache stays inside the cache it came from"
    );
    engine.snapshot(&Cancellation::default()).unwrap();
}

/// The cache lifecycle is usable on its own, without an engine around it —
/// which is what [#114] extends.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
#[test]
fn the_cache_lifecycle_is_reachable_without_an_engine() {
    let fixture = Fixture::new();
    let cache_root = fixture.root.path().join("standalone-cache");

    let cache = IndexCache::open_or_create(
        &cache_root,
        &harkness_context::index::ExpectedVersions::current(),
        "11111111-1111-5111-8111-111111111111",
        &Cancellation::default(),
    )
    .unwrap();

    assert_eq!(cache.path(), cache_root.join(INDEX_DATABASE_FILE));
    let report = cache.refresh(&Cancellation::default()).unwrap();
    assert_eq!(report.generation, cache.generation());
    let recreation = cache.dispose(&Cancellation::default()).unwrap();
    assert_eq!(recreation.reason, RecreationReason::Disposed);
    assert!(cache.generation() > report.generation);
}
