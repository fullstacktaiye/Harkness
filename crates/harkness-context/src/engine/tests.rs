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
use crate::index::{
    INDEX_DATABASE_FILE, INDEX_SCHEMA_VERSION, IndexAvailability, RecreationReason,
};
use crate::inventory::GLOBAL_IGNORE_FILE;
use crate::probe::FilesystemProbe;
use crate::{
    ChunkId, ExtractionSkipReason, FreshnessState, MAX_SYMBOLS_PER_FILE, ParseHealth, RepoPath,
    SymbolKind,
};

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
        ContextEngine::open(self.config(), &Cancellation::default()).unwrap()
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

    let main = ContextEngine::open(
        ContextEngineConfig::new(project_id, &main_root, &fixture.data_dir),
        &Cancellation::default(),
    )
    .unwrap();
    let linked = ContextEngine::open(
        ContextEngineConfig::new(project_id, &linked_root, &fixture.data_dir),
        &Cancellation::default(),
    )
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

/// The facade walks the workspace it was configured for, under the policy that
/// configuration implies — including the global ignore file, whose path only the
/// engine knows how to compose.
#[test]
fn an_inventory_is_walked_under_the_engines_own_policy() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("keep.rs"), "fn main() {}\n").unwrap();
    fs::write(workspace.root.join("notes.md"), "# notes\n").unwrap();
    fs::write(workspace.root.join(".env"), "TOKEN=hunter2\n").unwrap();
    fs::create_dir_all(&workspace.fixture.data_dir).unwrap();
    fs::write(
        workspace.fixture.data_dir.join(GLOBAL_IGNORE_FILE),
        "notes.md\n",
    )
    .unwrap();

    let engine = workspace.engine();
    let inventory = engine
        .inventory(&InventoryRequest::new(), &Cancellation::default())
        .unwrap();

    let paths = inventory
        .entries()
        .iter()
        .map(|entry| entry.path.display())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"keep.rs".to_owned()), "{paths:?}");
    // The user's own layer applied, which only happens because the engine
    // joined `<data_dir>/context-ignore` — nothing else in the workspace does.
    assert!(!paths.contains(&"notes.md".to_owned()), "{paths:?}");
    assert_eq!(inventory.ignored_count(), 1);
    // And the denial layer still holds through the facade.
    assert_eq!(inventory.denied_count(), 1);
    assert!(!format!("{inventory:#?}").contains("hunter2"));
    // The id names the capture this walk was built for, and a second capture of
    // one unchanged workspace is a different id — which is what an id means.
    assert_ne!(
        inventory.snapshot(),
        engine.snapshot(&Cancellation::default()).unwrap().id()
    );
}

/// A cancelled walk reaches the caller as the engine's own `cancelled`, not as a
/// second spelling of it.
#[test]
fn a_cancelled_inventory_answers_with_the_engines_cancelled_kind() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let error = engine
        .inventory(&InventoryRequest::new(), &cancellation)
        .unwrap_err();

    assert_eq!(error.kind(), "cancelled");
    assert_eq!(
        crate::ContextEngineError::kinds()
            .iter()
            .filter(|kind| **kind == "cancelled")
            .count(),
        1,
        "one event, one spelling"
    );
}

/// Every facade method answers. None panics, and none fabricates a result for a
/// feature this build does not have.
#[test]
fn every_facade_method_either_answers_or_names_the_feature_it_is_missing() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    // The methods that are implemented.
    engine.snapshot(&cancellation).unwrap();
    engine
        .inventory(&InventoryRequest::new(), &cancellation)
        .unwrap();
    engine.reindex(&cancellation).unwrap();
    engine
        .search(&SearchQuery::exact("needle"), &cancellation)
        .unwrap();
    engine
        .symbols(&SymbolQuery::new("Thing"), &cancellation)
        .unwrap();

    let chunk = ChunkId::derive(
        &RepoPath::from_path(std::path::Path::new("src/lib.rs")),
        "0",
        b"",
    );
    let refusals: Vec<(&str, crate::ContextEngineError)> = vec![
        (
            "read_chunk",
            engine.read_chunk(&chunk, &cancellation).unwrap_err(),
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
        4,
        "eight facade methods, four of them working"
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

    engine.dispose_index(&Cancellation::default()).unwrap();
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

    let error = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &plain, &fixture.data_dir),
        &Cancellation::default(),
    )
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

    let error = ContextEngine::open(
        ContextEngineConfig::new(
            ProjectId::new(),
            fixture.root.path().join("never-created"),
            &fixture.data_dir,
        ),
        &Cancellation::default(),
    )
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

/// A cache that could not be prepared is remembered, not sealed in. The
/// commonest way to reach that state is a few seconds of contention at exactly
/// the wrong moment, and losing retrieval for the engine's whole life over it
/// would make the failure far more expensive than the cause.
#[cfg(unix)]
#[test]
fn an_engine_recovers_a_cache_that_could_not_be_prepared_at_open() {
    let workspace = Workspace::new();
    let context_root = workspace.fixture.data_dir.join(CONTEXT_DIRECTORY);
    fs::create_dir_all(&context_root).unwrap();
    let Some(sealed) = crate::index::tests::ReadOnlyDirectory::seal(&context_root) else {
        return;
    };

    let engine = workspace.engine();

    assert!(
        matches!(
            engine.index_status().availability,
            IndexAvailability::Unavailable { kind, .. } if kind == "cache_open_failed"
        ),
        "unexpected availability: {:?}",
        engine.index_status().availability
    );
    assert_eq!(engine.index_generation(), 0);
    // Identity still works while retrieval does not.
    assert_eq!(
        engine
            .snapshot(&Cancellation::default())
            .unwrap()
            .index_generation(),
        0
    );

    drop(sealed);
    let report = engine.refresh_index(&Cancellation::default()).unwrap();

    assert!(report.generation > 0);
    assert_eq!(engine.index_status().availability, IndexAvailability::Ready);
    assert_eq!(engine.index_generation(), report.generation);
    // And the documented "fix a weird index" action works from here too.
    engine.dispose_index(&Cancellation::default()).unwrap();
}

/// The action documented as the fix for a weird index has to work on the one
/// cache that cannot be opened at all — otherwise a user's only recourse is
/// deleting the data directory by hand.
#[test]
fn disposing_discards_a_cache_this_build_cannot_even_read() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let database = engine.cache_root().join(INDEX_DATABASE_FILE);
    drop(engine);
    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE index_meta SET schema_version = ?1",
            [i64::from(INDEX_SCHEMA_VERSION + 1)],
        )
        .unwrap();

    let engine = workspace.engine();
    assert_eq!(
        engine
            .refresh_index(&Cancellation::default())
            .unwrap_err()
            .kind(),
        "cache_version_conflict",
        "a refresh must not silently destroy a cache a newer build is using"
    );

    let recreation = engine.dispose_index(&Cancellation::default()).unwrap();

    assert_eq!(recreation.reason, RecreationReason::Disposed);
    assert_eq!(
        recreation.previous_generation, None,
        "a cache that could not be read cannot report what generation it held"
    );
    assert!(recreation.generation > 0);
    assert_eq!(engine.index_status().availability, IndexAvailability::Ready);
    assert_eq!(engine.index_generation(), recreation.generation);
    engine.refresh_index(&Cancellation::default()).unwrap();
}

/// Two spellings of one checkout are one workspace. An engine that recorded the
/// caller's raw path would let a registry hold two engines for it.
#[test]
fn the_worktree_root_an_engine_records_is_canonical() {
    let workspace = Workspace::new();
    let trailing = workspace.root.join("");
    let indirect = workspace.root.join("..").join(
        workspace
            .root
            .file_name()
            .expect("the fixture root is named"),
    );

    let direct = workspace.engine();
    let engine = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &indirect, &workspace.fixture.data_dir),
        &Cancellation::default(),
    )
    .unwrap();
    let slashed = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &trailing, &workspace.fixture.data_dir),
        &Cancellation::default(),
    )
    .unwrap();

    let canonical = fs::canonicalize(&workspace.root).unwrap();
    assert_eq!(direct.worktree_root(), canonical);
    assert_eq!(engine.worktree_root(), canonical);
    assert_eq!(slashed.worktree_root(), canonical);
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

    let engine = ContextEngine::open(config, &Cancellation::default()).unwrap();

    assert_eq!(engine.config().config_generation(), 7);
    assert_eq!(
        engine.config().origins().origin(SettingGroup::Ignore),
        SettingOrigin::RepositoryTightened
    );
    let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
    assert_eq!(snapshot.config_generation(), 7);
}

// -- the cold build ---------------------------------------------------------

/// Writes `files` into a fresh workspace and returns the engine that serves it.
fn workspace_with(files: &[(&str, &str)]) -> (Workspace, ContextEngine) {
    let workspace = Workspace::new();
    for (path, body) in files {
        let target = workspace.root.join(path);
        fs::create_dir_all(target.parent().expect("a file has a parent")).unwrap();
        fs::write(target, body).unwrap();
    }
    let engine = workspace.engine();
    (workspace, engine)
}

/// The whole point of the cache: a repository walked once answers from rows
/// afterwards, and the rows say what the walk found.
#[test]
fn reindexing_writes_every_eligible_file_and_reopening_reads_it_back() {
    let (workspace, engine) = workspace_with(&[
        ("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n"),
        ("README.md", "# Title\n\nSome prose.\n"),
        ("assets/blob.bin", "\u{0}\u{1}\u{2}binary\u{0}"),
    ]);
    let cancellation = Cancellation::default();

    let receipt = engine.reindex(&cancellation).unwrap();

    assert_eq!(receipt.scope.as_str(), "full");
    assert!(receipt.files_recorded >= 3);
    assert!(receipt.chunks_recorded >= 2);

    let source = RepoPath::from_path(std::path::Path::new("src/main.rs"));
    let row = engine
        .indexed_file(&source)
        .unwrap()
        .expect("the source is indexed");
    assert!(row.eligible());
    assert!(row.file_version.is_some());
    assert_eq!(row.classify_version, crate::CLASSIFY_VERSION);
    assert!(!engine.indexed_chunks(&source).unwrap().is_empty());

    let binary = RepoPath::from_path(std::path::Path::new("assets/blob.bin"));
    let blob = engine
        .indexed_file(&binary)
        .unwrap()
        .expect("the blob is recorded");
    assert!(!blob.eligible(), "recorded, and never read");
    assert!(blob.file_version.is_none());

    // A second engine over the same data directory reads the warm cache rather
    // than an empty one, which is what "reopening a project is fast" reduces to.
    let reopened = ContextEngine::open(
        ContextEngineConfig::new(
            ProjectId::new(),
            &workspace.root,
            &workspace.fixture.data_dir,
        ),
        &cancellation,
    )
    .unwrap();
    assert_eq!(
        reopened.indexed_files(1_000).unwrap().len(),
        engine.indexed_files(1_000).unwrap().len()
    );
    // Adopting a warm cache deliberately does not count it — six table scans on
    // the path a user reached by opening a project would spend the whole open
    // budget on a number nothing has asked for. Asking is what counts it.
    assert!(reopened.index_status().counts.is_none());
    assert_eq!(
        reopened.index_counts().unwrap().files,
        receipt.files_recorded
    );
    assert!(reopened.refresh_index(&cancellation).is_ok());
    assert_eq!(
        reopened
            .index_status()
            .counts
            .expect("a refresh publishes what it found")
            .files,
        receipt.files_recorded
    );
}

#[test]
fn reindexing_extracts_queries_and_explains_symbol_health() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.root.join("src")).unwrap();
    let mut rust = String::from(
        "pub struct ProjectService;\n\
         impl ProjectService {\n\
             pub fn create_worktree(&self) {}\n\
         }\n",
    );
    rust.push_str(&"// structural padding\n".repeat(160));
    fs::write(workspace.root.join("src/project.rs"), rust).unwrap();
    fs::write(
        workspace.root.join("src/other.rs"),
        "struct OtherService;\nimpl OtherService { fn create_worktree(&self) {} }\n",
    )
    .unwrap();
    fs::write(
        workspace.root.join("src/broken.rs"),
        "fn before() {}\nfn broken( {\nfn after() {}\n",
    )
    .unwrap();
    fs::write(
        workspace.root.join("script.py"),
        "def answer():\n    return 42\n",
    )
    .unwrap();
    fs::write(workspace.root.join(".env"), "fn must_never_parse() {}\n").unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    let exact = engine
        .symbols(
            &SymbolQuery::exact_name("create_worktree"),
            &Cancellation::default(),
        )
        .unwrap();
    let repeated_exact = engine
        .symbols(
            &SymbolQuery::exact_name("create_worktree"),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(exact, repeated_exact, "exact lookup order is deterministic");
    assert_eq!(exact.symbols.len(), 2);
    assert_eq!(
        exact
            .symbols
            .iter()
            .map(|symbol| symbol.qualified_path.as_str())
            .collect::<Vec<_>>(),
        [
            "OtherService::create_worktree",
            "ProjectService::create_worktree"
        ]
    );
    let project_method = exact
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_path == "ProjectService::create_worktree")
        .unwrap();
    assert_eq!(project_method.kind, SymbolKind::Method);
    let suffix = engine
        .symbols(
            &SymbolQuery::qualified_suffix("ProjectService::create_worktree"),
            &Cancellation::default(),
        )
        .unwrap();
    let repeated_suffix = engine
        .symbols(
            &SymbolQuery::qualified_suffix("ProjectService::create_worktree"),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(
        suffix, repeated_suffix,
        "suffix lookup order is deterministic"
    );
    assert_eq!(
        suffix.symbols.as_slice(),
        std::slice::from_ref(project_method)
    );

    let project = RepoPath::from_path(std::path::Path::new("src/project.rs"));
    let first = engine
        .symbols(
            &SymbolQuery::file(project.clone()),
            &Cancellation::default(),
        )
        .unwrap();
    let second = engine
        .symbols(
            &SymbolQuery::file(project.clone()),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(first, second, "two runs are byte-for-byte deterministic");
    assert!(
        engine
            .indexed_chunks(&project)
            .unwrap()
            .iter()
            .any(|chunk| chunk.symbol.as_ref() == Some(&project_method.id)),
        "the parser outline feeds symbol identity into structural chunks"
    );
    assert_eq!(
        engine.symbol_health(&project).unwrap().unwrap().health,
        ParseHealth::Complete
    );

    let broken = RepoPath::from_path(std::path::Path::new("src/broken.rs"));
    assert!(matches!(
        engine.symbol_health(&broken).unwrap().unwrap().health,
        ParseHealth::Partial { .. }
    ));
    let python = RepoPath::from_path(std::path::Path::new("script.py"));
    assert!(matches!(
        engine.symbol_health(&python).unwrap().unwrap().health,
        ParseHealth::Skipped {
            reason: ExtractionSkipReason::UnsupportedLanguage
        }
    ));
    let secret = RepoPath::from_path(std::path::Path::new(".env"));
    assert!(engine.symbol_health(&secret).unwrap().is_none());
    assert!(
        engine
            .symbols(
                &SymbolQuery::exact_name("must_never_parse"),
                &Cancellation::default(),
            )
            .unwrap()
            .symbols
            .is_empty()
    );
}

#[test]
fn symbol_signatures_never_enter_the_index_database() {
    const SENTINEL: &[u8] = b"raw-signature-sentinel-117";
    let workspace = Workspace::new();
    fs::write(
        workspace.root.join("sentinel.rs"),
        "pub fn harmless() -> &'static str { \"raw-signature-sentinel-117\" }\n",
    )
    .unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    for entry in fs::read_dir(engine.cache_root()).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(SENTINEL.len())
                .any(|window| window == SENTINEL),
            "raw declaration content leaked into {}",
            path.display()
        );
    }
}

#[test]
fn nested_rust_items_remain_indexable() {
    let workspace = Workspace::new();
    let mut source = String::from("fn outer() {\n    fn inner() {}\n");
    source.push_str(&"    let padding = 1;\n".repeat(160));
    source.push_str("}\n");
    fs::write(workspace.root.join("nested.rs"), source).unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    let path = RepoPath::from_path(std::path::Path::new("nested.rs"));
    let file = engine.indexed_file(&path).unwrap().unwrap();
    assert!(!file.unreadable, "valid nested declarations stay readable");
    assert_eq!(
        engine.symbol_health(&path).unwrap().unwrap().health,
        ParseHealth::Complete
    );
    let inner = engine
        .symbols(&SymbolQuery::exact_name("inner"), &Cancellation::default())
        .unwrap();
    assert_eq!(inner.symbols.len(), 1);
    assert_eq!(inner.symbols[0].qualified_path, "outer::inner");
    assert!(
        !engine.indexed_chunks(&path).unwrap().is_empty(),
        "valid source must retain fallback chunks even if an outline is refused"
    );
}

#[test]
fn nested_markdown_headings_round_trip_through_the_index() {
    let workspace = Workspace::new();
    fs::write(
        workspace.root.join("guide.md"),
        "# Install\n\nIntro.\n\n## Linux\n\nDetails.\n\n### Fedora\n\nMore.\n",
    )
    .unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    let path = RepoPath::from_path(std::path::Path::new("guide.md"));
    let headings = engine
        .symbols(&SymbolQuery::file(path), &Cancellation::default())
        .unwrap();
    assert_eq!(
        headings
            .symbols
            .iter()
            .map(|symbol| symbol.qualified_path.as_str())
            .collect::<Vec<_>>(),
        ["Install", "Install::Linux", "Install::Linux::Fedora"]
    );
}

#[test]
fn transcoded_source_records_named_symbol_skip() {
    let workspace = Workspace::new();
    let mut utf16 = vec![0xff, 0xfe];
    for unit in "fn transcoded() {}\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(workspace.root.join("transcoded.rs"), utf16).unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    let path = RepoPath::from_path(std::path::Path::new("transcoded.rs"));
    assert!(matches!(
        engine.symbol_health(&path).unwrap().unwrap().health,
        ParseHealth::Skipped {
            reason: ExtractionSkipReason::TranscodedInput
        }
    ));
    let chunks = engine.indexed_chunks(&path).unwrap();
    assert!(
        !chunks.is_empty(),
        "transcoded text still uses line chunking"
    );
    assert!(chunks.iter().all(|chunk| chunk.transcoded));
    assert!(
        engine
            .symbols(
                &SymbolQuery::exact_name("transcoded"),
                &Cancellation::default(),
            )
            .unwrap()
            .symbols
            .is_empty()
    );
}

#[test]
fn symbol_budget_exhaustion_is_visible_to_lookup() {
    let workspace = Workspace::new();
    let mut source = String::new();
    for index in 0..=MAX_SYMBOLS_PER_FILE {
        source.push_str(&format!("fn item_{index}() {{}}\n"));
    }
    fs::write(workspace.root.join("too-many.rs"), source).unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();

    let path = RepoPath::from_path(std::path::Path::new("too-many.rs"));
    assert!(matches!(
        engine.symbol_health(&path).unwrap().unwrap().health,
        ParseHealth::Failed { ref reason } if reason == "symbol_budget_exhausted"
    ));
    let answer = engine
        .symbols(&SymbolQuery::exact_name("item_0"), &Cancellation::default())
        .unwrap();
    assert!(
        answer.symbols.is_empty(),
        "a failed parse exposes no prefix"
    );
    assert_eq!(answer.incomplete_files, 1);
    assert!(
        !engine.indexed_chunks(&path).unwrap().is_empty(),
        "failed structural extraction falls back to bounded line chunks"
    );
}

#[test]
#[ignore = "release-mode warm symbol lookup benchmark"]
fn warm_symbol_lookup_meets_the_latency_target() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.root.join("src")).unwrap();
    let mut source = String::new();
    for index in 0..2_500 {
        source.push_str(&format!(
            "pub fn item_{index}() {{ let value = {index}; }}\n"
        ));
    }
    fs::write(workspace.root.join("src/medium.rs"), &source).unwrap();

    let engine = workspace.engine();
    engine.reindex(&Cancellation::default()).unwrap();
    let query = SymbolQuery::exact_name("item_1731");
    assert_eq!(
        engine
            .symbols(&query, &Cancellation::default())
            .unwrap()
            .symbols
            .len(),
        1
    );

    let mut samples = Vec::with_capacity(100);
    for _ in 0..100 {
        let started = std::time::Instant::now();
        let answer = engine.symbols(&query, &Cancellation::default()).unwrap();
        samples.push(started.elapsed());
        assert_eq!(answer.symbols.len(), 1);
    }
    samples.sort_unstable();
    let p95 = samples[94];
    eprintln!(
        "warm symbol lookup: 2500 declarations, 100 samples, p95 {:.3} ms",
        p95.as_secs_f64() * 1_000.0
    );
    harkness_test_fixtures::latency::record(
        "symbols::warm_lookup_p95",
        p95,
        std::time::Duration::from_millis(100),
    );
}

#[test]
#[ignore = "release-mode symbol index growth benchmark"]
fn symbol_index_growth_stays_within_the_medium_profile_target() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let empty_bytes = engine.index_counts().unwrap().database_bytes;

    let mut source = String::new();
    for index in 0..2_500 {
        source.push_str(&format!(
            "pub fn item_{index}() {{ let value = {index}; }}\n"
        ));
    }
    fs::write(workspace.root.join("medium.rs"), &source).unwrap();
    engine.reindex(&Cancellation::default()).unwrap();

    let populated_bytes = engine.index_counts().unwrap().database_bytes;
    let growth = populated_bytes.saturating_sub(empty_bytes);
    let connection = Connection::open(engine.cache_root().join(INDEX_DATABASE_FILE)).unwrap();
    let symbol_bytes = connection
        .query_row(
            "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat WHERE name = 'symbols'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let symbol_bytes = u64::try_from(symbol_bytes).unwrap();
    let ratio = symbol_bytes as f64 / source.len() as f64;
    println!(
        "harkness-resource target=symbol-index-growth source_bytes={} symbol_bytes={symbol_bytes} database_growth_bytes={growth} ratio={ratio:.3} profile={}",
        source.len(),
        harkness_test_fixtures::latency::profile(),
    );
    // The frozen medium-profile baseline is 6.0 bytes of symbols-table pages
    // per source byte. Issue #117 permits at most twice that recorded ratio so
    // an index-layout change cannot quietly double the feature's footprint.
    const MAX_RATIO: f64 = 12.0;
    assert!(
        ratio <= MAX_RATIO,
        "symbol index grew by {ratio:.3}x source bytes, over the {MAX_RATIO:.1}x ceiling"
    );
}

/// A file that is gone stops being indexed. A full batch is the whole worktree,
/// so the sweep is what makes the second build describe the repository rather
/// than its history.
#[test]
fn a_second_reindex_sweeps_what_the_worktree_no_longer_has() {
    let (workspace, engine) = workspace_with(&[
        ("keep.rs", "fn keep() {}\n"),
        ("remove.rs", "fn remove() {}\n"),
    ]);
    let cancellation = Cancellation::default();
    engine.reindex(&cancellation).unwrap();

    fs::remove_file(workspace.root.join("remove.rs")).unwrap();
    fs::write(workspace.root.join("keep.rs"), "fn keep() { changed(); }\n").unwrap();
    let receipt = engine.reindex(&cancellation).unwrap();

    assert_eq!(receipt.rows_swept, 1);
    let paths = engine
        .indexed_files(100)
        .unwrap()
        .iter()
        .map(|row| row.path.display())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"keep.rs".to_owned()));
    assert!(!paths.contains(&"remove.rs".to_owned()));
}

/// Two linked worktrees of one repository share a cache and keep their own
/// files. Getting this wrong is how one checkout answers the other's questions.
#[test]
fn two_linked_worktrees_share_one_cache_and_keep_their_own_file_rows() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("shared.rs"), "fn shared() {}\n").unwrap();
    let repository = harkness_git::git2::Repository::open(&workspace.root).unwrap();
    harkness_test_fixtures::commit_all(&repository, "base");
    let linked = workspace.fixture.root.path().join("linked");
    repository
        .worktree(
            "linked",
            &linked,
            Some(WorktreeAddOptions::new().reference(None)),
        )
        .unwrap();
    fs::write(linked.join("only-linked.rs"), "fn linked() {}\n").unwrap();

    let cancellation = Cancellation::default();
    let primary = workspace.engine();
    let secondary = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &linked, &workspace.fixture.data_dir),
        &cancellation,
    )
    .unwrap();
    assert_eq!(primary.cache_root(), secondary.cache_root());
    assert_ne!(primary.worktree_key(), secondary.worktree_key());

    primary.reindex(&cancellation).unwrap();
    secondary.reindex(&cancellation).unwrap();

    let primary_paths = primary
        .indexed_files(100)
        .unwrap()
        .iter()
        .map(|row| row.path.display())
        .collect::<Vec<_>>();
    let secondary_paths = secondary
        .indexed_files(100)
        .unwrap()
        .iter()
        .map(|row| row.path.display())
        .collect::<Vec<_>>();
    assert!(primary_paths.contains(&"shared.rs".to_owned()));
    assert!(!primary_paths.contains(&"only-linked.rs".to_owned()));
    assert!(secondary_paths.contains(&"shared.rs".to_owned()));
    assert!(secondary_paths.contains(&"only-linked.rs".to_owned()));

    // The deduplication is by *content*, so what the two checkouts actually
    // hold is what decides the row count. Git may rewrite line endings on the
    // way into a worktree — on Windows it routinely does — and a file that came
    // out with different bytes is genuinely a different version rather than a
    // dedup failure. Reading both is the honest way to say which case this is.
    let primary_bytes = fs::read(workspace.root.join("shared.rs")).unwrap();
    let secondary_bytes = fs::read(linked.join("shared.rs")).unwrap();
    let expected = i64::from(primary_bytes != secondary_bytes) + 1;

    let connection = Connection::open(primary.cache_root().join(INDEX_DATABASE_FILE)).unwrap();
    let versions: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM file_versions WHERE path = ?1",
            [&b"shared.rs"[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        versions, expected,
        "one path with one content is one file version, and one path with two \
         contents is two"
    );
}

/// A cold build reconciles before it writes. Adding rows beside the ones a
/// version bump invalidated would leave the cache half in each world.
#[test]
fn reindexing_reconciles_a_version_skew_before_it_writes() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("a.rs"), "fn a() {}\n").unwrap();
    let cancellation = Cancellation::default();
    workspace.engine().reindex(&cancellation).unwrap();

    let upgraded = ContextEngine::open(
        workspace
            .config()
            .with_expected_versions(crate::index::ExpectedVersions {
                chunking_version: "99".to_owned(),
                ..crate::index::ExpectedVersions::current()
            }),
        &cancellation,
    )
    .unwrap();
    assert_eq!(upgraded.index_status().stale_components.len(), 1);

    upgraded.reindex(&cancellation).unwrap();

    assert!(upgraded.index_status().stale_components.is_empty());
    let reachable: usize = upgraded
        .indexed_files(1_000)
        .unwrap()
        .iter()
        .map(|row| upgraded.indexed_chunks(&row.path).unwrap().len())
        .sum();
    assert!(reachable > 0);
    assert_eq!(
        upgraded.index_counts().unwrap().chunks,
        reachable as u64,
        "every chunk left in the cache is one this build wrote and can reach"
    );
}

/// A cache another process is writing costs *retrieval* and nothing else. The
/// engine's Git-backed half — workspace identity and the walk above all — is
/// what a run cannot proceed without, so a caller met by `index_busy` degrades
/// to reading the workspace live rather than stopping.
#[test]
fn a_contended_cache_costs_the_index_and_not_the_workspace() {
    let (workspace, engine) = workspace_with(&[("a.rs", "fn a() {}\n")]);
    let cancellation = Cancellation::default();
    engine.reindex(&cancellation).unwrap();

    // The write lock, held by somebody else. In WAL mode this is exactly what
    // one front end indexing while another wants to looks like: readers are
    // untouched and the *writer* is the one that has to wait or give up.
    let blocker = Connection::open(engine.cache_root().join(INDEX_DATABASE_FILE)).unwrap();
    blocker
        .busy_timeout(std::time::Duration::from_millis(1))
        .unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let refused = engine.reindex(&cancellation).unwrap_err();
    assert_eq!(refused.kind(), "index_busy");

    // The fallback: the same question answered from the filesystem, with the
    // cache held by somebody else for the whole of it.
    let live = engine
        .inventory(&InventoryRequest::new(), &cancellation)
        .unwrap();
    assert!(
        live.entries()
            .iter()
            .any(|entry| entry.path.display() == "a.rs")
    );
    assert!(engine.snapshot(&cancellation).is_ok());

    // What was already indexed is still readable throughout: contention is a
    // delay to the *update*, not a loss of the cache.
    assert!(!engine.indexed_files(100).unwrap().is_empty());

    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);
    assert!(engine.reindex(&cancellation).is_ok());
    let _ = workspace;
}

/// The rule a truncated walk turns on, held to directly.
///
/// A full batch deletes every row it did not confirm. A walk stopped by its own
/// budget did not see the whole worktree, and a walk asked about part of it
/// never intended to — sweeping on either would delete rows for files that
/// exist. Reaching the truncated branch through `reindex` would mean building a
/// repository past `MAX_INVENTORY_FILES`, so the decision reads the inventory's
/// own account of what it covered and this is where that is proved.
#[test]
fn a_walk_that_saw_less_than_the_worktree_commits_as_targeted() {
    let workspace = Workspace::new();
    for name in ["a.rs", "b.rs"] {
        fs::write(workspace.root.join(name), "fn x() {}\n").unwrap();
    }
    let cancellation = Cancellation::default();
    let engine = workspace.engine();
    let snapshot = engine.snapshot(&cancellation).unwrap();

    let complete =
        crate::InventoryBuilder::build(&snapshot, &crate::InventoryPolicy::new(), &cancellation)
            .unwrap();
    assert!(!complete.is_truncated());
    assert_eq!(
        super::batch_scope(&complete),
        crate::index::BatchScope::Full
    );

    let truncated = crate::InventoryBuilder::build(
        &snapshot,
        &crate::InventoryPolicy::new().with_max_files(1),
        &cancellation,
    )
    .unwrap();
    assert!(truncated.is_truncated());
    assert_eq!(
        super::batch_scope(&truncated),
        crate::index::BatchScope::Targeted
    );

    let scoped = crate::InventoryBuilder::build_scoped(
        snapshot.id(),
        snapshot.worktree_root(),
        &crate::InventoryPolicy::new(),
        &crate::ReconcileScope::paths([RepoPath::from_path(std::path::Path::new("a.rs"))]),
        &cancellation,
    )
    .unwrap();
    assert!(!scoped.is_truncated());
    assert_eq!(
        super::batch_scope(&scoped),
        crate::index::BatchScope::Targeted,
        "an inventory that only ever looked at part of the tree must never sweep"
    );
}

/// A file that changes under a running build keeps whatever the last successful
/// pass derived, rather than being recorded as content-less — which would
/// unlink it from its chunks and have the commit collect them.
#[test]
fn a_file_that_changes_under_the_build_keeps_its_previous_chunks() {
    let (workspace, engine) = workspace_with(&[("a.rs", "fn a() {}\n")]);
    let cancellation = Cancellation::default();
    engine.reindex(&cancellation).unwrap();
    let path = RepoPath::from_path(std::path::Path::new("a.rs"));
    let chunks = engine.indexed_chunks(&path).unwrap();
    assert!(!chunks.is_empty());

    // The inventory records one size and the file is a different one by the
    // time the bytes are read, which is what `FileVersion::new` refuses.
    let inventory = engine
        .inventory(&InventoryRequest::new(), &cancellation)
        .unwrap();
    let entry = inventory
        .entries()
        .iter()
        .find(|entry| entry.path == path)
        .expect("the file is in the inventory")
        .clone();
    fs::write(
        workspace.root.join("a.rs"),
        "fn a() { much_longer_now(); }\n",
    )
    .unwrap();

    let key = engine.worktree_key();
    let receipt = {
        let cache_root = engine.cache_root().to_path_buf();
        let cache = crate::index::IndexCache::open_or_create(
            &cache_root,
            &crate::index::ExpectedVersions::current(),
            engine.repository_key(),
            &cancellation,
        )
        .unwrap();
        let mut batch = cache
            .begin(
                &key,
                engine.worktree_root(),
                crate::index::BatchScope::Targeted,
                &cancellation,
            )
            .unwrap();
        batch
            .record_unreadable(&entry, crate::CLASSIFY_VERSION)
            .unwrap();
        batch.commit(&cancellation).unwrap()
    };

    assert_eq!(receipt.files_recorded, 1);
    let row = engine
        .indexed_file(&path)
        .unwrap()
        .expect("the row is still there");
    assert!(row.unreadable);
    assert!(row.file_version.is_some(), "still linked to what it had");
    assert_eq!(
        engine.indexed_chunks(&path).unwrap(),
        chunks,
        "and its chunks survived the pass that could not read it"
    );
}

/// An already-cancelled token launches nothing at all.
#[test]
fn a_cancelled_reindex_writes_nothing_visible() {
    let (_workspace, engine) = workspace_with(&[("a.rs", "fn a() {}\n")]);
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let error = engine.reindex(&cancellation).unwrap_err();

    assert_eq!(error.kind(), "cancelled");
    assert!(engine.indexed_files(100).unwrap().is_empty());
}
