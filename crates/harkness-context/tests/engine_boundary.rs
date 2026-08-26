//! The context engine as an outside caller sees it.
//!
//! The unit tests beside the implementation can reach private state; these
//! cannot, which is the point. Everything [#114]–[#123] needs to plug into is a
//! public item, and a seam that only works from inside its own crate is not a
//! seam. [#115]'s half of that is the pair a surface reaches for: a scope it
//! can name and a watch it can start, stop, and ask about. This also stands where [#133] and [#136] will: no run store, no agent,
//! no model, and no network in the process.
//!
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115
//! [#123]: https://github.com/fullstacktaiye/harkness/issues/123
//! [#133]: https://github.com/fullstacktaiye/harkness/issues/133
//! [#136]: https://github.com/fullstacktaiye/harkness/issues/136

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use harkness_context::index::{
    INDEX_DATABASE_FILE, IndexAvailability, IndexCache, RecreationReason,
};
use harkness_context::watch::{ChangeHint, WatchOptions, WatchService, WatchState};
use harkness_context::{
    ChunkId, ContextEngine, ContextEngineConfig, ContextEngineError, GitContextBudget,
    InventoryRequest, MapRequest, PackRequest, ReconcileScope, RepoPath, RetrievalSource,
    SearchLimits, SearchQuery, SymbolQuery,
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

/// Git context is reached through the same public engine boundary and carries
/// the caller's capture rather than taking an unrelated one per query.
#[test]
fn git_context_is_snapshot_bound_from_outside_the_crate() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("tracked.txt"), "changed outside\n").unwrap();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    let snapshot = engine.snapshot(&cancellation).unwrap();
    let git = engine.git_context_under(&snapshot, &cancellation).unwrap();
    let diff = git
        .working_diff(&GitContextBudget::default(), &cancellation)
        .unwrap();

    assert_eq!(diff.snapshot_id, snapshot.id());
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].provenance.snapshot_id, snapshot.id());
    assert_eq!(
        diff.files[0].new_path.as_ref().unwrap().as_bytes(),
        b"tracked.txt"
    );
}

/// The whole of what [#123] plugs into: build an index, ask a question, page
/// the answer, and read where every match came from — with no run store, no
/// policy engine and no model in the process.
///
/// [#123]: https://github.com/fullstacktaiye/harkness/issues/123
#[test]
fn search_answers_paged_attributed_matches_from_outside_the_crate() {
    let workspace = Workspace::new();
    let cancellation = Cancellation::default();
    fs::write(
        workspace.root.join("alpha.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    fs::write(workspace.root.join("beta.rs"), "fn four() {}\n").unwrap();
    let engine = workspace.engine();
    engine.reindex(&cancellation).unwrap();

    // One page at a time, so the cursor is exercised rather than described.
    let mut query = SearchQuery::exact("fn ").with_limits(SearchLimits::new().with_max_results(2));
    let mut found = Vec::new();
    let mut pages = 0;
    loop {
        let page = engine.search(&query, &cancellation).unwrap();
        pages += 1;
        for matched in &page.matches {
            assert_eq!(
                matched.provenance.source,
                RetrievalSource::LexicalSearch,
                "every match names the machinery that produced it"
            );
            assert_eq!(matched.provenance.snapshot_id, page.snapshot_id);
            assert!(matched.content_sha256.is_some());
            found.push((matched.path.display(), matched.line_number.unwrap()));
        }
        assert_eq!(page.is_truncated(), page.next_cursor.is_some());
        let Some(cursor) = page.next_cursor else {
            break;
        };
        query = query.continuing(cursor);
    }

    assert!(pages > 1, "a two-match page over four matches has to page");
    assert_eq!(
        found,
        vec![
            ("alpha.rs".to_owned(), 1),
            ("alpha.rs".to_owned(), 2),
            ("alpha.rs".to_owned(), 3),
            ("beta.rs".to_owned(), 1),
        ]
    );

    // And the same universe answers about names without reading a byte of
    // content.
    let names = engine
        .search(&SearchQuery::filename("beta"), &cancellation)
        .unwrap();
    assert_eq!(names.matches.len(), 1);
    assert_eq!(names.matches[0].path.display(), "beta.rs");
    assert_eq!(
        names.matches[0].provenance.source,
        RetrievalSource::FilenameSearch
    );
    assert_eq!(names.stats.files_scanned, 0);
}

/// A worktree the cache has never seen is a question this engine refuses,
/// because "no match" and "I did not look" are different answers.
#[test]
fn searching_an_unindexed_worktree_refuses_rather_than_answering_empty() {
    let workspace = Workspace::new();
    let engine = workspace.engine();

    let refusal = engine
        .search(&SearchQuery::exact("initial"), &Cancellation::default())
        .map(|_| ())
        .unwrap_err();

    assert_eq!(refusal.kind(), "index_unavailable");
    assert!(ContextEngineError::kinds().contains(&"index_unavailable"));
}

/// Every remaining retrieval issue has a compiling seam and a typed refusal.
#[test]
fn the_unimplemented_facade_methods_refuse_by_name_from_outside_the_crate() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    let chunk = ChunkId::derive(&RepoPath::from_path(Path::new("src/lib.rs")), "0", b"");
    engine.reindex(&cancellation).unwrap();
    assert!(
        engine
            .symbols(&SymbolQuery::new("Thing"), &cancellation)
            .unwrap()
            .symbols
            .is_empty()
    );

    let refusals = [
        engine
            .read_chunk(&chunk, &cancellation)
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

/// The persistent index, reached the way a retrieval feature will reach it:
/// build, then read back through the public API alone, with no run store, no
/// agent, and no network in the process.
#[test]
fn the_index_is_built_and_read_back_from_outside_the_crate() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("lib.rs"), "fn exported() {}\n").unwrap();
    fs::create_dir_all(workspace.root.join("docs")).unwrap();
    fs::write(workspace.root.join("docs/guide.md"), "# Guide\n\nProse.\n").unwrap();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    let receipt = engine.reindex(&cancellation).unwrap();

    assert_eq!(receipt.worktree, engine.worktree_key());
    assert!(receipt.files_recorded >= 2);
    assert!(receipt.chunks_recorded >= 2);
    assert!(receipt.generation > 0);

    let source = RepoPath::from_path(Path::new("lib.rs"));
    let row = engine
        .indexed_file(&source)
        .unwrap()
        .expect("an eligible file is indexed");
    assert!(row.eligible());
    assert_eq!(row.byte_size, "fn exported() {}\n".len() as u64);
    assert!(!engine.indexed_chunks(&source).unwrap().is_empty());

    let counts = engine.index_counts().unwrap();
    assert_eq!(counts.worktrees, 1);
    assert_eq!(counts.files, receipt.files_recorded);
    assert_eq!(counts.chunks, receipt.chunks_recorded);
    assert!(counts.database_bytes > 0);

    // And it is still a cache: disposing it costs the rows and nothing else.
    engine.dispose_index(&cancellation).unwrap();
    assert!(engine.indexed_files(100).unwrap().is_empty());
    assert!(engine.snapshot(&cancellation).is_ok());
}

/// Eviction is a data-directory-wide maintenance action a front end can offer,
/// so it has to be callable without an engine and has to skip what is open.
#[test]
fn eviction_is_reachable_without_an_engine_and_spares_a_live_cache() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("a.rs"), "fn a() {}\n").unwrap();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine.reindex(&cancellation).unwrap();

    let report =
        harkness_context::index::evict_to_budget(&workspace.fixture.data_dir, 0, &cancellation)
            .unwrap();

    assert!(report.evicted.is_empty(), "a live cache is never evicted");
    assert_eq!(report.skipped_in_use, 1);
    assert!(report.bytes_before > 0);
    assert!(!engine.indexed_files(100).unwrap().is_empty());
}

/// The cache lifecycle is usable on its own, without an engine around it.
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

/// The incremental seam as a surface reaches it: name a scope, get a report,
/// and read the rows it published. Nothing about it needs a watcher, which is
/// the whole of the hints-are-not-truth split expressed as an API.
#[test]
fn a_scoped_reconcile_is_reachable_and_reports_what_it_did() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.root.join("src")).unwrap();
    fs::write(workspace.root.join("src/a.rs"), "fn a() {}\n").unwrap();
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    let cold = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    assert!(cold.added > 0);
    assert!(cold.generation > 0);
    assert_eq!(cold.scope, ReconcileScope::Full);
    assert!(cold.escalated.is_none());

    fs::write(workspace.root.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let path = RepoPath::from_path(Path::new("src/a.rs"));
    let update = engine
        .reconcile(&ReconcileScope::paths([path.clone()]), &cancellation)
        .unwrap();

    assert_eq!(update.examined, 1);
    assert_eq!(update.changed, 1);
    assert!(update.generation > cold.generation);
    assert_eq!(update.worktree, engine.worktree_key());
    assert!(!engine.indexed_chunks(&path).unwrap().is_empty());
}

/// A watch is started from an engine, answers its own status without waiting on
/// the worker, accepts a hint from a caller, and is stopped by being dropped.
/// Every one of those is something [#133]'s surface has to be able to do.
///
/// [#133]: https://github.com/fullstacktaiye/harkness/issues/133
#[test]
fn a_watch_is_startable_observable_and_stoppable_from_outside_the_crate() {
    let workspace = Workspace::new();
    fs::write(workspace.root.join("a.rs"), "fn a() {}\n").unwrap();
    let engine = Arc::new(workspace.engine());

    let mut service = engine
        .watch(
            WatchOptions::new()
                .without_filesystem_events()
                .with_quiescence(Duration::from_millis(40)),
        )
        .expect("the watch starts");
    assert!(service.wait_until_quiet(Duration::from_secs(20)));
    assert_eq!(service.worktree_root(), engine.worktree_root());
    assert!(matches!(
        service.status().state,
        WatchState::Degraded { .. }
    ));
    assert!(
        engine
            .indexed_file(&RepoPath::from_path(Path::new("a.rs")))
            .unwrap()
            .is_some(),
        "the startup sweep is what recovers everything missed while this process was not running"
    );

    fs::write(workspace.root.join("b.rs"), "fn b() {}\n").unwrap();
    service.hint(ChangeHint::Path(RepoPath::from_path(Path::new("b.rs"))));
    assert!(service.wait_until_quiet(Duration::from_secs(20)));
    assert!(
        engine
            .indexed_file(&RepoPath::from_path(Path::new("b.rs")))
            .unwrap()
            .is_some()
    );

    service.stop();
    assert_eq!(service.status().state, WatchState::Stopped);
    drop(service);
    assert_eq!(Arc::strong_count(&engine), 1);
}

/// A watch on a root that is not there refuses with the discriminant a caller
/// switches on, carried through the engine's own namespace.
#[test]
fn a_watch_failure_reaches_the_caller_with_its_own_discriminant() {
    let workspace = Workspace::new();
    let engine = Arc::new(workspace.engine());
    fs::remove_dir_all(&workspace.root).unwrap();

    let error = engine
        .watch(WatchOptions::new())
        .expect_err("there is nothing to watch");

    assert_eq!(error.kind(), "watch_root_missing");
    assert!(harkness_context::ContextEngineError::kinds().contains(&"watch_root_missing"));
    let _ = WatchService::start(engine, WatchOptions::new()).expect_err("and the type refuses too");
}
