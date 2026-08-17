use std::fs;
use std::path::PathBuf;

use harkness_core::{CONTEXT_DIRECTORY, ProjectId};
use harkness_git::git2::WorktreeAddOptions;
use harkness_git::{Cancellation, GitService};
use harkness_test_fixtures::{Fixture, initialize_repository};
use rusqlite::Connection;
use uuid::Uuid;

use super::{
    ContextEngine, ContextEngineConfig, InventoryRequest, MapRequest, PackRequest, SearchQuery,
    SettingGroup, SettingOrigin, SettingOrigins, SymbolQuery,
};
use crate::index::{INDEX_DATABASE_FILE, INDEX_SCHEMA_VERSION, IndexAvailability};
use crate::probe::FilesystemProbe;
use crate::{ChunkId, FreshnessState, RepoPath};

/// The namespace `harkness-git` keys repository locks by, restated here so the
/// test derives the cache key independently instead of asking the code under
/// test what it thinks the answer is.
const REPOSITORY_LOCK_NAMESPACE: Uuid = Uuid::from_u128(0x7f3a_9c1e_5b2d_4e6a_9c17_2f8b_41d0_a6e3);

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

    fn config(&self) -> ContextEngineConfig {
        ContextEngineConfig::new(ProjectId::new(), &self.root, &self.fixture.data_dir)
    }

    fn engine(&self) -> ContextEngine {
        ContextEngine::open(self.config()).unwrap()
    }
}

/// Derives the cache key the way ADR-0004 states it, from the canonical common
/// directory rather than from anything the engine computed.
fn expected_key(worktree_root: &std::path::Path) -> String {
    let repository = harkness_git::git2::Repository::open(worktree_root).unwrap();
    let common = fs::canonicalize(repository.commondir()).unwrap();
    Uuid::new_v5(
        &REPOSITORY_LOCK_NAMESPACE,
        common.as_os_str().as_encoded_bytes(),
    )
    .to_string()
}

#[test]
fn opening_creates_the_cache_under_the_derived_repository_key() {
    let workspace = Workspace::new();

    let engine = workspace.engine();

    let key = expected_key(&workspace.root);
    assert_eq!(engine.repository_key(), key);
    assert_eq!(
        engine.cache_root(),
        workspace
            .fixture
            .data_dir
            .join(CONTEXT_DIRECTORY)
            .join(&key)
    );
    let database = engine.cache_root().join(INDEX_DATABASE_FILE);
    assert!(database.is_file());

    let stored: (i64, String) = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT schema_version, repository_identity FROM index_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, (i64::from(INDEX_SCHEMA_VERSION), key));
    assert!(engine.index_generation() > 0);
}

/// Linked worktrees share `objects` and `refs`; indexing each one separately
/// would rebuild nearly identical content per checkout.
#[test]
fn two_linked_worktrees_of_one_repository_share_a_cache_root() {
    let fixture = Fixture::new();
    let main_root = fixture.directory("main-checkout");
    let repository = initialize_repository(&main_root);
    let linked_root = fixture.root.path().join("linked-checkout");
    repository
        .worktree("linked", &linked_root, Some(&WorktreeAddOptions::new()))
        .unwrap();
    let project_id = ProjectId::new();

    let main = ContextEngine::open(ContextEngineConfig::new(
        project_id,
        &main_root,
        &fixture.data_dir,
    ))
    .unwrap();
    let linked = ContextEngine::open(ContextEngineConfig::new(
        project_id,
        &linked_root,
        &fixture.data_dir,
    ))
    .unwrap();

    assert_eq!(main.repository_key(), linked.repository_key());
    assert_eq!(main.cache_root(), linked.cache_root());
    assert_eq!(main.index_generation(), linked.index_generation());
    assert_ne!(
        main.worktree_root(),
        linked.worktree_root(),
        "one cache, two workspaces: per-worktree isolation lives inside it"
    );
}

/// Every facade method answers. None panics, and none fabricates a result for a
/// feature this build does not have.
#[test]
fn every_facade_method_either_answers_or_names_the_feature_it_is_missing() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    // The one method that is implemented.
    engine.snapshot(&cancellation).unwrap();

    let chunk = ChunkId::derive(
        &RepoPath::from_path(std::path::Path::new("src/lib.rs")),
        "0",
        b"",
    );
    let refusals: Vec<(&str, crate::ContextEngineError)> = vec![
        (
            "inventory",
            engine
                .inventory(&InventoryRequest::new(), &cancellation)
                .unwrap_err(),
        ),
        (
            "search",
            engine
                .search(&SearchQuery::new("needle"), &cancellation)
                .unwrap_err(),
        ),
        (
            "read_chunk",
            engine.read_chunk(&chunk, &cancellation).unwrap_err(),
        ),
        (
            "symbols",
            engine
                .symbols(&SymbolQuery::new("Thing"), &cancellation)
                .unwrap_err(),
        ),
        (
            "repository_map",
            engine
                .repository_map(&MapRequest::new(), &cancellation)
                .unwrap_err(),
        ),
        (
            "instructions",
            engine.instructions(&cancellation).unwrap_err(),
        ),
        (
            "build_pack",
            engine
                .build_pack(&PackRequest::new("fix the bug"), &cancellation)
                .unwrap_err(),
        ),
    ];

    assert_eq!(
        refusals.len(),
        7,
        "eight facade methods, one of them working"
    );
    for (method, error) in refusals {
        assert_eq!(
            error.kind(),
            "not_yet_available",
            "{method} answered wrongly"
        );
        let named = match error {
            crate::ContextEngineError::NotYetAvailable { feature } => feature,
            other => panic!("{method} returned {other}"),
        };
        assert!(
            !named.is_empty(),
            "{method} must name the feature it is missing"
        );
    }
}

/// A snapshot is taken *against* a cache generation, so a rebuilt cache has to
/// make an old snapshot stale even when not one byte of the workspace moved.
#[test]
fn a_snapshot_carries_the_cache_generation_and_a_rebuild_makes_it_stale() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    let before = engine.snapshot(&cancellation).unwrap();
    assert_eq!(before.index_generation(), engine.index_generation());

    let git = GitService::new(&workspace.root, &workspace.fixture.data_dir);
    let probe = FilesystemProbe::new(&workspace.root);
    assert_eq!(
        before.verify(&git, &probe, &cancellation).unwrap(),
        FreshnessState::Fresh
    );

    engine.dispose_index().unwrap();
    let after = engine.snapshot(&cancellation).unwrap();

    assert!(after.index_generation() > before.index_generation());
    assert_ne!(
        after.digest(),
        before.digest(),
        "a pack built against a rebuilt index must not be taken for one built against the old"
    );
}

/// Capture must not hand back a half-built identity, so an already-cancelled
/// token starts nothing.
#[test]
fn a_cancelled_token_starts_no_capture() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let error = engine.snapshot(&cancellation).unwrap_err();

    assert_eq!(error.kind(), "cancelled");
}

/// v0.4 gives a folder that is not a Git worktree no context engine at all, and
/// says so by name rather than by a cache failure.
#[test]
fn a_directory_that_is_not_a_repository_gets_no_engine() {
    let fixture = Fixture::new();
    let plain = fixture.directory("just-a-folder");

    let error = ContextEngine::open(ContextEngineConfig::new(
        ProjectId::new(),
        &plain,
        &fixture.data_dir,
    ))
    .unwrap_err();

    assert_eq!(error.kind(), "repository_unavailable");
    assert!(
        !fixture.data_dir.join(CONTEXT_DIRECTORY).exists(),
        "a refused engine must not leave a cache behind"
    );
}

#[test]
fn a_missing_worktree_root_is_named_as_such() {
    let fixture = Fixture::new();

    let error = ContextEngine::open(ContextEngineConfig::new(
        ProjectId::new(),
        fixture.root.path().join("never-created"),
        &fixture.data_dir,
    ))
    .unwrap_err();

    assert_eq!(error.kind(), "worktree_root_missing");
}

/// A cache this build cannot address costs retrieval, not workspace identity.
/// The engine still opens and `snapshot` still answers.
#[test]
fn a_cache_written_by_a_newer_build_degrades_retrieval_and_nothing_else() {
    let workspace = Workspace::new();
    let key = expected_key(&workspace.root);
    let cache_root = workspace
        .fixture
        .data_dir
        .join(CONTEXT_DIRECTORY)
        .join(&key);
    let engine = workspace.engine();
    let generation = engine.index_generation();
    drop(engine);
    let database = cache_root.join(INDEX_DATABASE_FILE);
    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE index_meta SET schema_version = ?1",
            [i64::from(INDEX_SCHEMA_VERSION + 1)],
        )
        .unwrap();
    let before = fs::read(&database).unwrap();

    let engine = workspace.engine();

    let status = engine.index_status();
    assert!(
        matches!(status.availability, IndexAvailability::Unavailable { kind, .. }
            if kind == "cache_version_conflict"),
        "unexpected availability: {:?}",
        status.availability
    );
    assert_eq!(status.generation, 0);
    assert_eq!(
        engine
            .refresh_index(&Cancellation::default())
            .unwrap_err()
            .kind(),
        "cache_version_conflict"
    );

    // Identity still works, and the snapshot honestly reports that it was taken
    // against no index at all rather than against the one it could not read.
    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
    assert_eq!(snapshot.index_generation(), 0);
    assert_ne!(snapshot.index_generation(), generation);
    assert_eq!(
        fs::read(&database).unwrap(),
        before,
        "a refused cache must keep the bytes it arrived with"
    );
}

#[test]
fn setting_origins_record_a_refused_widening_per_group() {
    let origins = SettingOrigins::default()
        .recording(SettingGroup::Ignore, SettingOrigin::RepositoryTightened)
        .recording(
            SettingGroup::Retrieval,
            SettingOrigin::RepositoryWideningRefused,
        );

    assert!(origins.tightened_only(SettingGroup::Ignore));
    assert!(origins.tightened_only(SettingGroup::Instructions));
    assert!(!origins.tightened_only(SettingGroup::Retrieval));
    assert_eq!(
        origins.origin(SettingGroup::Instructions),
        SettingOrigin::Global
    );
    assert_eq!(SettingGroup::ALL.len(), 3);
}

#[test]
fn a_configuration_carries_its_provenance_into_the_engine() {
    let workspace = Workspace::new();
    let config = workspace
        .config()
        .with_config_generation(7)
        .with_setting_origin(SettingGroup::Ignore, SettingOrigin::RepositoryTightened);

    let engine = ContextEngine::open(config).unwrap();

    assert_eq!(engine.config().config_generation(), 7);
    assert_eq!(
        engine.config().origins().origin(SettingGroup::Ignore),
        SettingOrigin::RepositoryTightened
    );
    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
    assert_eq!(snapshot.config_generation(), 7);
}
