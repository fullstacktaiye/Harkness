//! The hints-are-not-truth model, held to the rows it produces.
//!
//! Nothing here involves a filesystem watcher. Every scope is handed to
//! [`ContextEngine::reconcile`] directly, which is the point: a reconcile
//! reached through a dropped event, a startup sweep, or a caller asking must
//! produce the same index, so the behaviour is proved where no watcher can
//! affect the timing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use harkness_core::ProjectId;
use harkness_git::Cancellation;
use harkness_git::git2::WorktreeAddOptions;
use harkness_test_fixtures::{
    Fixture, child_path, commit_all, git, initialize_repository, spawn_child,
};

use super::{MAX_PATHS_PER_RECONCILE, ReconcileScope};
use crate::engine::{ContextEngine, ContextEngineConfig};
use crate::index::WorktreeKey;
use crate::path::RepoPath;

const PROCESS_CHILD_TEST: &str = "reconcile::tests::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_RECONCILE_TEST_ROLE";
const PROCESS_DATA_DIR_ENV: &str = "HARKNESS_RECONCILE_TEST_DATA_DIR";
const PROCESS_WORKTREE_ENV: &str = "HARKNESS_RECONCILE_TEST_WORKTREE";

/// A repository, a data directory, and the engine that serves them.
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
        .expect("the engine opens")
    }

    fn write(&self, relative: &str, body: &str) {
        write_at(&self.root, relative, body, None);
    }

    /// Writes and stamps a modification time a comparison can tell apart.
    ///
    /// Filesystem clocks are coarse — a one-second `mtime` is an ordinary
    /// filesystem, and even a nanosecond one can return the same value twice
    /// inside a tick. A test that means to exercise the *metadata* comparison
    /// stamps the time rather than hoping the clock moved between two writes.
    fn write_stamped(&self, relative: &str, body: &str, epoch_seconds: u64) {
        write_at(&self.root, relative, body, Some(epoch_seconds));
    }

    fn path(&self, relative: &str) -> RepoPath {
        RepoPath::from_path(Path::new(relative))
    }
}

fn write_at(root: &Path, relative: &str, body: &str, epoch_seconds: Option<u64>) {
    let target = root.join(relative);
    fs::create_dir_all(target.parent().expect("a file has a parent")).unwrap();
    fs::write(&target, body).unwrap();
    if let Some(seconds) = epoch_seconds {
        let times = fs::FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(times)
            .unwrap();
    }
}

fn paths(scope: &ReconcileScope) -> Vec<String> {
    match scope {
        ReconcileScope::Paths(paths) => paths.iter().map(RepoPath::display).collect(),
        other => panic!("expected a path list, found {other:?}"),
    }
}

// -- the scope vocabulary ----------------------------------------------------

/// The list has to be ordered and disjoint, because the merge reads the index
/// in one forward pass over exactly these ranges. A list holding both `src` and
/// `src/main.rs` would read the second range twice and, worse, read it *after*
/// the first — so a removal decided by "no path beside this row" would fire on
/// a row the walk had already recorded.
#[test]
fn a_path_list_is_ordered_and_stripped_of_what_it_already_covers() {
    let scope = ReconcileScope::paths([
        RepoPath::from_bytes(b"src/main.rs".to_vec()),
        RepoPath::from_bytes(b"docs/guide.md".to_vec()),
        RepoPath::from_bytes(b"src".to_vec()),
        RepoPath::from_bytes(b"docs/guide.md".to_vec()),
        RepoPath::from_bytes(b"src/lib/mod.rs".to_vec()),
    ]);

    assert_eq!(paths(&scope), ["docs/guide.md", "src"]);
    assert_eq!(scope.kind(), "paths");
    assert_eq!(scope.named_paths(), 2);
    assert!(scope.covers(&RepoPath::from_bytes(b"src/lib/mod.rs".to_vec())));
    // Containment is by separator, not by byte prefix.
    assert!(!scope.covers(&RepoPath::from_bytes(b"src-generated.rs".to_vec())));

    // Covering is not naming, and the difference is the whole cost model: the
    // directory `src` is named and everything beneath it is only covered.
    assert!(scope.names_exactly(&RepoPath::from_bytes(b"src".to_vec())));
    assert!(!scope.names_exactly(&RepoPath::from_bytes(b"src/lib/mod.rs".to_vec())));
    assert!(!ReconcileScope::Full.names_exactly(&RepoPath::from_bytes(b"src".to_vec())));
    assert!(
        !ReconcileScope::subtree(RepoPath::from_bytes(b"src".to_vec()))
            .names_exactly(&RepoPath::from_bytes(b"src".to_vec()))
    );
}

/// Two spellings of "everything" must not behave differently, and "nothing"
/// must not be promoted into "everything": a watcher that has been quiet
/// draining an empty set would otherwise rebuild the repository.
#[test]
fn the_root_is_the_whole_worktree_and_an_empty_list_is_nothing() {
    let root = ReconcileScope::paths([RepoPath::from_bytes(Vec::new())]);
    assert!(root.is_full());
    assert!(ReconcileScope::subtree(RepoPath::from_bytes(Vec::new())).is_full());

    let nothing = ReconcileScope::paths([]);
    assert!(nothing.is_empty());
    assert!(!nothing.is_full());
    assert_eq!(nothing.kind(), "paths");
}

/// A scope only ever widens, and it says so. Narrowing would be an update that
/// silently covered less than it was asked to.
#[test]
fn a_path_list_past_its_bound_widens_to_the_directory_that_holds_it() {
    let within = ReconcileScope::paths(
        (0..8).map(|index| RepoPath::from_bytes(format!("src/f{index}.rs").into_bytes())),
    );
    assert!(within.overflowed().is_none());

    let over = ReconcileScope::paths(
        (0..=MAX_PATHS_PER_RECONCILE)
            .map(|index| RepoPath::from_bytes(format!("src/f{index}.rs").into_bytes())),
    );
    assert_eq!(
        over.overflowed(),
        Some(ReconcileScope::Subtree(RepoPath::from_bytes(
            b"src".to_vec()
        )))
    );

    // Nothing in common: the only directory holding all of it is the root.
    let scattered = ReconcileScope::paths(
        (0..=MAX_PATHS_PER_RECONCILE)
            .map(|index| RepoPath::from_bytes(format!("d{index}/f.rs").into_bytes())),
    );
    assert_eq!(scattered.overflowed(), Some(ReconcileScope::Full));
}

/// A scoped walk descends from the root exactly as a full one does, so every
/// `.gitignore` on the way is read. Jumping straight to the scope would apply a
/// different set of rules to the same tree, and an incremental update would
/// then record a file a rebuild excludes.
#[test]
fn a_scoped_walk_reads_the_same_ignore_chain_a_full_one_does() {
    let workspace = Workspace::new();
    workspace.write(".gitignore", "generated/\n");
    workspace.write("src/keep.rs", "fn keep() {}\n");
    workspace.write("src/generated/skip.rs", "fn skip() {}\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();

    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    assert!(
        engine
            .indexed_file(&workspace.path("src/keep.rs"))
            .unwrap()
            .is_some()
    );
    assert!(
        engine
            .indexed_file(&workspace.path("src/generated/skip.rs"))
            .unwrap()
            .is_none(),
        "the root .gitignore excludes it"
    );

    // The same answer through a subtree that does not contain the rule file.
    workspace.write("src/generated/also-skipped.rs", "fn also() {}\n");
    let report = engine
        .reconcile(
            &ReconcileScope::subtree(workspace.path("src")),
            &cancellation,
        )
        .unwrap();
    assert_eq!(report.added, 0);
    assert!(
        engine
            .indexed_file(&workspace.path("src/generated/also-skipped.rs"))
            .unwrap()
            .is_none()
    );
}

// -- what a reconcile writes -------------------------------------------------

/// A worktree the cache has never published has nothing to reconcile against,
/// so a full pass over one is a cold build reached by another route — same
/// rows, one generation, nothing visible until it commits.
#[test]
fn a_full_reconcile_of_an_unindexed_worktree_is_a_cold_build() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    workspace.write("README.md", "# Title\n");
    let engine = workspace.engine();

    let report = engine
        .reconcile(&ReconcileScope::Full, &Cancellation::default())
        .unwrap();

    assert!(report.added >= 3, "{report:?}");
    assert_eq!(report.changed, 0);
    assert_eq!(report.removed, 0);
    assert!(report.generation > 0);
    assert!(!report.is_quiet());
    assert!(
        engine
            .indexed_file(&workspace.path("src/main.rs"))
            .unwrap()
            .is_some()
    );
    assert!(
        !engine
            .indexed_chunks(&workspace.path("README.md"))
            .unwrap()
            .is_empty()
    );
}

/// The headline promise: one edit costs one file's work, whatever the size of
/// the repository. `hashed` is the number that says so — a sweep that read
/// every file to discover that one changed would report the file count here.
#[test]
fn editing_one_file_reads_one_file() {
    let workspace = Workspace::new();
    for index in 0..12 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    workspace.write_stamped("src/f3.rs", "fn a() {}\nfn b() {}\n", 1_700_000_100);
    let report = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("src/f3.rs")]),
            &cancellation,
        )
        .unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.hashed, 1);
    assert_eq!(report.changed, 1);
    assert_eq!(report.added, 0);
    assert_eq!(report.removed, 0);
    assert!(report.requeued.is_empty());
}

/// The two strengths of hint, apart. A scope that *names* a file hashes it
/// whatever its metadata says; a scope that names the directory above it does
/// not, because a checkout touching ten thousand files moved ten thousand
/// modification times and rehashing all of them is the rebuild this exists to
/// avoid. A coalesced watcher scope holds both kinds in one list, so the
/// question has to be asked per path.
#[test]
fn a_named_file_is_hashed_and_a_named_directory_is_only_swept() {
    let workspace = Workspace::new();
    for index in 0..6 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    // The directory: everything under it is examined and nothing is read.
    let swept = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("src")]),
            &cancellation,
        )
        .unwrap();
    assert_eq!(swept.examined, 6);
    assert_eq!(swept.hashed, 0, "{swept:?}");

    // A subtree scope says the same thing in the other spelling.
    let subtree = engine
        .reconcile(
            &ReconcileScope::subtree(workspace.path("src")),
            &cancellation,
        )
        .unwrap();
    assert_eq!(subtree.hashed, 0);

    // The files, named one by one: every one is a suspect.
    let named = engine
        .reconcile(
            &ReconcileScope::paths((0..6).map(|index| workspace.path(&format!("src/f{index}.rs")))),
            &cancellation,
        )
        .unwrap();
    assert_eq!(named.examined, 6);
    assert_eq!(named.hashed, 6);
    assert!(named.is_quiet(), "and none of them actually moved");
}

/// Duplicate and spurious events must cost work and never a row. The hash is
/// the short-circuit: the bytes are the ones the row already names, so its
/// chunk set is still correct and nothing is written.
#[test]
fn a_spurious_hint_for_an_unchanged_file_changes_no_row() {
    let workspace = Workspace::new();
    workspace.write_stamped("src/main.rs", "fn main() {}\n", 1_700_000_000);
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    let before = engine
        .indexed_file(&workspace.path("src/main.rs"))
        .unwrap()
        .expect("the file is indexed");

    let scope = ReconcileScope::paths([workspace.path("src/main.rs")]);
    for _ in 0..3 {
        let report = engine.reconcile(&scope, &cancellation).unwrap();
        assert_eq!(report.examined, 1);
        // Hinted, so it is hashed however unchanged its metadata looks.
        assert_eq!(report.hashed, 1);
        assert_eq!(report.changed, 0);
        assert_eq!(report.added, 0);
        assert_eq!(report.removed, 0);
        assert!(report.is_quiet());
    }

    let after = engine
        .indexed_file(&workspace.path("src/main.rs"))
        .unwrap()
        .expect("the file is still indexed");
    assert_eq!(after.file_version, before.file_version);
    assert_eq!(
        after.generation, before.generation,
        "an unchanged file's row must not be rewritten at a new generation"
    );
}

/// A sweep over a repository nothing touched reads no file at all. This is the
/// other half of the promise: the startup recovery is a metadata comparison,
/// not a rebuild, so opening a project costs a walk rather than a re-hash.
#[test]
fn a_sweep_over_an_unchanged_worktree_reads_nothing() {
    let workspace = Workspace::new();
    for index in 0..12 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    let report = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    assert!(report.examined >= 13, "{report:?}");
    assert_eq!(report.hashed, 0, "an unhinted sweep metadata-compares only");
    assert!(report.is_quiet());
}

/// A file that changed while nothing was watching is found by the metadata
/// comparison, and only that file is read. This is what makes a watcher's
/// events optional rather than load-bearing.
#[test]
fn a_change_made_with_no_hint_at_all_is_found_by_the_sweep() {
    let workspace = Workspace::new();
    for index in 0..8 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    workspace.write_stamped("src/f5.rs", "fn a() {}\nfn changed() {}\n", 1_700_000_500);

    let report = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    assert_eq!(report.hashed, 1, "only the moved file is a suspect");
    assert_eq!(report.changed, 1);
    let chunks = engine.indexed_chunks(&workspace.path("src/f5.rs")).unwrap();
    assert!(
        chunks.iter().any(|chunk| chunk.byte_range.end > 12),
        "the chunks describe the new bytes rather than the old ones"
    );
}

/// A deleted path loses its row by name rather than by a sweep, and a rename
/// keeps the content it moved: the bytes are content-addressed, so the new path
/// finds the digest already stored.
#[test]
fn a_deleted_file_loses_its_row_and_a_renamed_one_keeps_its_content() {
    let workspace = Workspace::new();
    workspace.write("src/gone.rs", "fn gone() {}\n");
    workspace.write("src/moved.rs", "fn moved() {}\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    let original = engine
        .indexed_file(&workspace.path("src/moved.rs"))
        .unwrap()
        .expect("the file is indexed");

    fs::remove_file(workspace.root.join("src/gone.rs")).unwrap();
    fs::rename(
        workspace.root.join("src/moved.rs"),
        workspace.root.join("src/elsewhere.rs"),
    )
    .unwrap();

    let report = engine
        .reconcile(
            &ReconcileScope::paths([
                workspace.path("src/gone.rs"),
                workspace.path("src/moved.rs"),
                workspace.path("src/elsewhere.rs"),
            ]),
            &cancellation,
        )
        .unwrap();

    assert_eq!(report.removed, 2, "{report:?}");
    assert_eq!(report.added, 1);
    assert!(
        engine
            .indexed_file(&workspace.path("src/gone.rs"))
            .unwrap()
            .is_none()
    );
    assert!(
        engine
            .indexed_file(&workspace.path("src/moved.rs"))
            .unwrap()
            .is_none()
    );
    let moved = engine
        .indexed_file(&workspace.path("src/elsewhere.rs"))
        .unwrap()
        .expect("the renamed path is indexed");
    assert_eq!(
        moved.content_sha256, original.content_sha256,
        "the bytes did not change, so the content row is the same one"
    );
    assert_ne!(
        moved.file_version, original.file_version,
        "a file version absorbs its path, because chunking depends on it"
    );
}

/// A directory and a file named after it — `src` and `src.rs` — are the case
/// that makes the merge read a scope as a *point and an interval per path*
/// rather than one range per path. `src`'s descendants begin at `src/`, which
/// sorts after `src.rs`, so reading one whole path and then the next hands the
/// merge a stream that goes backwards. A backwards stream makes it stage a
/// removal and a record for the same path in one batch, and the removal wins
/// at the commit — an existing file deleted from the index because two of its
/// neighbours were named together.
#[test]
fn a_directory_and_a_file_named_after_it_are_reconciled_in_order() {
    let workspace = Workspace::new();
    workspace.write("src/a.rs", "fn a() {}\n");
    workspace.write("src/b.rs", "fn b() {}\n");
    workspace.write("src.rs", "fn root() {}\n");
    workspace.write("src-generated.rs", "fn generated() {}\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    let report = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("src"), workspace.path("src.rs")]),
            &cancellation,
        )
        .unwrap();

    assert_eq!(report.removed, 0, "{report:?}");
    assert_eq!(report.added, 0);
    for present in ["src/a.rs", "src/b.rs", "src.rs", "src-generated.rs"] {
        assert!(
            engine
                .indexed_file(&workspace.path(present))
                .unwrap()
                .is_some(),
            "'{present}' left the index"
        );
    }

    // And the scope really did cover both, rather than passing by leaving both
    // untouched: deleting one file under each is noticed.
    fs::remove_file(workspace.root.join("src/a.rs")).unwrap();
    fs::remove_file(workspace.root.join("src.rs")).unwrap();
    let swept = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("src"), workspace.path("src.rs")]),
            &cancellation,
        )
        .unwrap();

    assert_eq!(swept.removed, 2, "{swept:?}");
    assert!(
        engine
            .indexed_file(&workspace.path("src/b.rs"))
            .unwrap()
            .is_some()
    );
    assert!(
        engine
            .indexed_file(&workspace.path("src-generated.rs"))
            .unwrap()
            .is_some(),
        "a neighbour whose name merely begins with the scope is not in it"
    );
}

/// A walk stopped by its own budget did not see the whole scope, so a row it
/// has no path for is a row about a file nobody looked at. Removing it would be
/// the index deleting files that exist.
#[test]
fn a_truncated_walk_removes_nothing() {
    let workspace = Workspace::new();
    for index in 0..8 {
        workspace.write(&format!("src/f{index}.rs"), "fn a() {}\n");
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    let indexed = engine.indexed_files(1_000).unwrap().rows.len();

    // A second handle on the same cache, driven with a walk budget the facade
    // never offers: reaching truncation through `reconcile` would mean building
    // a repository past `MAX_INVENTORY_FILES`, so the rule is exercised where
    // the budget can be set instead of by a fixture nobody can afford to write.
    let cache = crate::index::IndexCache::open_or_create(
        engine.cache_root(),
        &crate::index::ExpectedVersions::current(),
        engine.repository_key(),
        &cancellation,
    )
    .unwrap();
    let root = fs::canonicalize(&workspace.root).unwrap();
    let policy = crate::InventoryPolicy::new().with_max_files(2);
    let report = super::Reconciler {
        cache: &cache,
        worktree: engine.worktree_key(),
        root: &root,
        policy: &policy,
        head_marker: None,
    }
    .run(&ReconcileScope::Full, &cancellation)
    .unwrap();

    assert!(report.truncated);
    assert_eq!(report.removed, 0);
    assert_eq!(engine.indexed_files(1_000).unwrap().rows.len(), indexed);
}

/// A cancelled pass leaves the previous generation answering, because nothing
/// it staged was ever visible. That is the crash-consistency guarantee reused:
/// giving up and being killed end the same way.
#[test]
fn a_cancelled_reconcile_leaves_the_previous_generation_answering() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    let before = engine.indexed_files(1_000).unwrap().rows;

    workspace.write("src/added.rs", "fn added() {}\n");
    cancellation.cancel();
    let error = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .expect_err("a cancelled pass refuses");

    assert_eq!(error.kind(), "cancelled");
    assert_eq!(engine.indexed_files(1_000).unwrap().rows, before);
}

/// A component version bump widens the suspect set rather than triggering a
/// rebuild. The row's own marker is what says so, which is why the file rows
/// survive an invalidation that empties the chunks.
#[test]
fn a_chunking_bump_makes_every_file_a_suspect_without_a_rebuild() {
    let workspace = Workspace::new();
    for index in 0..4 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    let cancellation = Cancellation::default();
    let engine = workspace.engine();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    // A build whose chunker moved on. Its `refresh` empties the chunks and
    // nulls the marker; the reconcile that follows is what refills them.
    let mut versions = crate::index::ExpectedVersions::current();
    versions.chunking_version = "999".to_owned();
    let bumped = ContextEngine::open(
        ContextEngineConfig::new(
            ProjectId::new(),
            &workspace.root,
            &workspace.fixture.data_dir,
        )
        .with_expected_versions(versions),
        &cancellation,
    )
    .unwrap();

    let report = bumped
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    assert!(
        report.hashed >= 4,
        "every eligible file is a suspect: {report:?}"
    );
    assert!(report.changed >= 4);
    assert_eq!(
        report.removed, 0,
        "a version bump is not a reason to lose a path"
    );
    assert!(
        !bumped
            .indexed_chunks(&workspace.path("src/f0.rs"))
            .unwrap()
            .is_empty()
    );
}

/// A path that was a file and is now a directory is a removal and an unknown
/// number of additions. A scope naming only the path would record the removal
/// and leave the tree beneath it invisible.
#[test]
fn a_file_that_became_a_directory_is_removed_and_its_contents_indexed() {
    let workspace = Workspace::new();
    workspace.write("thing", "I am a file\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    assert!(
        engine
            .indexed_file(&workspace.path("thing"))
            .unwrap()
            .is_some()
    );

    fs::remove_file(workspace.root.join("thing")).unwrap();
    workspace.write("thing/inner.rs", "fn inner() {}\n");

    let report = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("thing")]),
            &cancellation,
        )
        .unwrap();

    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(report.added, 1);
    assert!(
        engine
            .indexed_file(&workspace.path("thing"))
            .unwrap()
            .is_none()
    );
    assert!(
        engine
            .indexed_file(&workspace.path("thing/inner.rs"))
            .unwrap()
            .is_some()
    );
}

// -- worktree isolation ------------------------------------------------------

/// The isolation contract, and the sharing that pays for the repository-keyed
/// cache, in one test because they are one design: `files` is per-worktree and
/// everything beneath it is content-addressed.
#[test]
fn two_worktrees_share_content_and_never_see_each_others_edits() {
    let fixture = Fixture::new();
    let main_root = fixture.directory("main-checkout");
    let repository = initialize_repository(&main_root);
    write_at(&main_root, "src/shared.rs", "fn shared() {}\n", None);
    commit_all(&repository, "shared");
    let linked_root = fixture.root.path().join("linked-checkout");
    repository
        .worktree("linked", &linked_root, Some(&WorktreeAddOptions::new()))
        .unwrap();

    let cancellation = Cancellation::default();
    let main = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &main_root, &fixture.data_dir),
        &cancellation,
    )
    .unwrap();
    let linked = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &linked_root, &fixture.data_dir),
        &cancellation,
    )
    .unwrap();
    assert_eq!(main.cache_root(), linked.cache_root());
    assert_ne!(main.worktree_key(), linked.worktree_key());

    main.reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    linked
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    let path = RepoPath::from_path(Path::new("src/shared.rs"));
    let before_main = main.indexed_file(&path).unwrap().unwrap();
    let before_linked = linked.indexed_file(&path).unwrap().unwrap();
    assert_eq!(
        before_main.content_sha256, before_linked.content_sha256,
        "identical files share one content row"
    );
    assert_eq!(before_main.file_version, before_linked.file_version);
    let shared_rows = content_rows(&main);

    // An uncommitted edit in one checkout, and a secret only it holds.
    write_at(
        &main_root,
        "src/shared.rs",
        "fn shared() { edited() }\n",
        None,
    );
    write_at(
        &main_root,
        "src/only-here.rs",
        "const KEY: &str = \"s3cret\";\n",
        None,
    );
    let report = main
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    assert_eq!(report.changed, 1);
    assert_eq!(report.added, 1);

    let after_main = main.indexed_file(&path).unwrap().unwrap();
    let after_linked = linked.indexed_file(&path).unwrap().unwrap();
    assert_ne!(after_main.content_sha256, before_main.content_sha256);
    assert_eq!(
        after_linked.content_sha256, before_linked.content_sha256,
        "the sibling checkout's row must not move because this one was edited"
    );
    assert!(
        linked
            .indexed_file(&RepoPath::from_path(Path::new("src/only-here.rs")))
            .unwrap()
            .is_none(),
        "one worktree's uncommitted file must be unreachable from the other"
    );
    assert!(
        linked
            .indexed_files(1_000)
            .unwrap()
            .rows
            .iter()
            .all(|row| row.path.display() != "src/only-here.rs")
    );
    assert!(
        content_rows(&main) > shared_rows,
        "the edited bytes are a new content row beside the shared one"
    );
}

/// Removing a checkout takes its rows and leaves everything a sibling still
/// names. A collection that ignored the sibling would delete the content rows
/// out from under it.
#[test]
fn forgetting_a_worktree_keeps_what_its_sibling_still_uses() {
    let fixture = Fixture::new();
    let main_root = fixture.directory("main-checkout");
    let repository = initialize_repository(&main_root);
    write_at(&main_root, "src/shared.rs", "fn shared() {}\n", None);
    commit_all(&repository, "shared");
    let linked_root = fixture.root.path().join("linked-checkout");
    repository
        .worktree("linked", &linked_root, Some(&WorktreeAddOptions::new()))
        .unwrap();

    let cancellation = Cancellation::default();
    let main = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &main_root, &fixture.data_dir),
        &cancellation,
    )
    .unwrap();
    let linked = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &linked_root, &fixture.data_dir),
        &cancellation,
    )
    .unwrap();
    main.reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    linked
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    // One file only the linked checkout holds, so forgetting it has something
    // to collect as well as something to keep.
    write_at(&linked_root, "src/linked-only.rs", "fn only() {}\n", None);
    linked
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    let before = content_rows(&main);

    let key = linked.worktree_key();
    drop(linked);
    let report = main.forget_worktree(&key, &cancellation).unwrap();

    assert!(report.files_removed >= 2, "{report:?}");
    assert!(
        report.rows_collected >= 1,
        "the file only it held is collected"
    );
    assert!(content_rows(&main) < before);
    let shared = RepoPath::from_path(Path::new("src/shared.rs"));
    assert!(
        main.indexed_file(&shared).unwrap().is_some(),
        "the surviving checkout keeps the content row it still names"
    );
    assert!(!main.indexed_chunks(&shared).unwrap().is_empty());
}

/// [#63]: a worktree's identity is its path, so a checkout deleted and
/// re-created there is the same key holding another branch's rows. Metadata
/// alone cannot tell — a restore that preserved sizes and modification times
/// would have every row verify — so the committed base is recorded and its
/// divergence makes every row a suspect.
///
/// [#63]: https://github.com/fullstacktaiye/harkness/issues/63
#[test]
fn a_re_created_worktree_on_another_branch_distrusts_every_row() {
    let workspace = Workspace::new();
    let cancellation = Cancellation::default();
    let source = workspace.path("src/main.rs");
    workspace.write("src/main.rs", "fn main() { on_aaaa() }\n");
    commit(&workspace, "main");

    let engine = workspace.engine();
    let first = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();
    assert!(!first.head_changed, "the first pass records the base");
    let recorded = engine.indexed_file(&source).unwrap().unwrap();

    // Another branch holding a file of exactly the same size, restored to
    // exactly the modification time the row recorded. Nothing about the
    // filesystem can tell the two apart; only the base can.
    git(&workspace.root, ["checkout", "-b", "other"]);
    workspace.write("src/main.rs", "fn main() { on_bbbb() }\n");
    commit(&workspace, "other");
    stamp(
        &workspace.root.join("src/main.rs"),
        recorded.mtime_ns.unwrap(),
    );
    let metadata = fs::metadata(workspace.root.join("src/main.rs")).unwrap();
    assert_eq!(metadata.len(), recorded.byte_size);

    let report = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    assert!(report.head_changed, "{report:?}");
    assert!(
        report.hashed >= 1,
        "a diverged base is hashed rather than trusted: {report:?}"
    );
    assert_eq!(report.changed, 1);
    let indexed = engine.indexed_file(&source).unwrap().unwrap();
    assert_ne!(
        indexed.content_sha256, recorded.content_sha256,
        "the row must describe the branch that is actually checked out"
    );

    // And a narrower scope widens to a full pass rather than covering less.
    git(&workspace.root, ["checkout", "main"]);
    let widened = engine
        .reconcile(&ReconcileScope::paths([source.clone()]), &cancellation)
        .unwrap();
    assert!(widened.head_changed);
    assert_eq!(widened.escalated, Some(ReconcileScope::Full));
    assert_eq!(
        engine
            .indexed_file(&source)
            .unwrap()
            .unwrap()
            .content_sha256,
        recorded.content_sha256,
        "and the widened pass ends consistent with the branch it is on"
    );
}

/// The cost half of the recorded base, and the reason it is the branch rather
/// than the commit: a commit does not touch the working tree, so making one a
/// divergence would rehash the whole repository to discover that nothing moved
/// — on the single most frequent operation there is.
#[test]
fn an_ordinary_commit_is_not_a_divergence() {
    let workspace = Workspace::new();
    let cancellation = Cancellation::default();
    for index in 0..6 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }
    commit(&workspace, "base");
    let engine = workspace.engine();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    commit_empty(&workspace, "no changes to the working tree");
    let report = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    assert!(!report.head_changed, "{report:?}");
    assert_eq!(report.hashed, 0);
    assert!(report.is_quiet());
}

/// A scope naming nothing reconciles nothing, whatever else is true. A watcher
/// whose queue drained to empty must not be able to ask for a rebuild.
#[test]
fn a_scope_that_names_nothing_opens_no_batch() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    let cold = engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    // Including when the recorded base has moved, which is the case that would
    // otherwise widen it.
    git(&workspace.root, ["checkout", "-b", "other"]);
    let report = engine
        .reconcile(&ReconcileScope::paths([]), &cancellation)
        .unwrap();

    assert_eq!(report.examined, 0);
    assert_eq!(report.hashed, 0);
    assert!(!report.head_changed);
    assert!(report.escalated.is_none());
    assert!(report.is_quiet());
    assert_eq!(
        report.generation, cold.generation,
        "a pass that wrote nothing reports the generation that is answering"
    );
}

/// Commits every path in the worktree through the system Git the fixtures run.
fn commit(workspace: &Workspace, message: &str) {
    git(&workspace.root, ["add", "--all"]);
    commit_staged(workspace, message, &[]);
}

/// Records a commit that changes nothing, which is what "a commit does not
/// touch the working tree" needs in order to be asserted at all.
fn commit_empty(workspace: &Workspace, message: &str) {
    commit_staged(workspace, message, &["--allow-empty"]);
}

fn commit_staged(workspace: &Workspace, message: &str, extra: &[&str]) {
    let mut arguments = vec![
        "-c",
        "user.email=tests@harkness.invalid",
        "-c",
        "user.name=Harkness Tests",
        "commit",
    ];
    arguments.extend_from_slice(extra);
    arguments.extend_from_slice(&["-m", message]);
    git(&workspace.root, arguments);
}

/// Sets one file's modification time to an exact nanosecond value.
fn stamp(path: &Path, modified_ns: i64) {
    let nanos = u64::try_from(modified_ns).expect("fixture times are after the epoch");
    let times =
        fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos));
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(times)
        .unwrap();
}

// -- across a process boundary -----------------------------------------------

/// The child that indexes a worktree and exits, so the parent's changes are
/// made with nothing watching and no cache handle open.
#[test]
#[ignore = "only run as a child process by the stopped-process discovery test"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let data_dir = child_path(PROCESS_DATA_DIR_ENV);
    let worktree = child_path(PROCESS_WORKTREE_ENV);
    assert_eq!(role, "cold-build", "unknown test child role: {role}");
    let engine = ContextEngine::open(
        ContextEngineConfig::new(ProjectId::new(), &worktree, &data_dir),
        &Cancellation::default(),
    )
    .unwrap();
    let report = engine
        .reconcile(&ReconcileScope::Full, &Cancellation::default())
        .unwrap();
    assert!(report.added > 0, "the child indexed nothing: {report:?}");
}

/// Everything a watcher cannot see: the process was not running. The startup
/// sweep is the recovery, and it is incremental — the file count is what a
/// rebuild would hash, and one changed file is what this hashes.
#[test]
fn changes_made_while_the_process_was_stopped_are_found_without_a_rebuild() {
    let workspace = Workspace::new();
    for index in 0..10 {
        workspace.write_stamped(&format!("src/f{index}.rs"), "fn a() {}\n", 1_700_000_000);
    }

    let status = spawn_child(
        PROCESS_CHILD_TEST,
        PROCESS_ROLE_ENV,
        "cold-build",
        PROCESS_DATA_DIR_ENV,
        &workspace.fixture.data_dir,
    )
    .env(PROCESS_WORKTREE_ENV, &workspace.root)
    .status()
    .unwrap();
    assert!(status.success(), "the indexing child failed: {status}");

    // Nothing in this process has ever opened the cache, and nothing observed
    // these writes.
    workspace.write_stamped("src/f7.rs", "fn a() {}\nfn later() {}\n", 1_700_000_900);
    fs::remove_file(workspace.root.join("src/f2.rs")).unwrap();
    workspace.write_stamped("src/new.rs", "fn new() {}\n", 1_700_000_900);

    let engine = workspace.engine();
    let report = engine
        .reconcile(&ReconcileScope::Full, &Cancellation::default())
        .unwrap();

    assert_eq!(report.changed, 1, "{report:?}");
    assert_eq!(report.added, 1);
    assert_eq!(report.removed, 1);
    assert!(
        report.hashed <= 2,
        "the sweep must hash the suspects rather than the tree: {report:?}"
    );
    assert!(report.examined >= 10);
}

/// The incremental-update latency target: an edit to the generation that
/// publishes it, inside one second at the ninety-fifth percentile.
///
/// The measurement is the reconcile plus [`QUIESCENCE_WINDOW`], because that
/// window is the rest of the promise — a watcher does not hand a scope over
/// until the tree has been quiet for it, so a number that left it out would be
/// measuring half of what a user waits for.
///
/// `#[ignore]`d because it means nothing in a debug build; the correctness the
/// same path guarantees is asserted by the tests above with no clock in them.
#[test]
#[ignore = "latency target; meaningful only under --release"]
fn a_single_file_update_meets_the_incremental_latency_target() {
    let workspace = Workspace::new();
    for index in 0..2_000 {
        workspace.write_stamped(
            &format!("src/module{}/f{index}.rs", index % 40),
            "fn a() {\n    let value = 1;\n}\n",
            1_700_000_000,
        );
    }
    let engine = workspace.engine();
    let cancellation = Cancellation::default();
    engine
        .reconcile(&ReconcileScope::Full, &cancellation)
        .unwrap();

    workspace.write_stamped(
        "src/module3/f43.rs",
        "fn a() {\n    let value = 2;\n}\n",
        1_700_000_900,
    );
    let started = std::time::Instant::now();
    let report = engine
        .reconcile(
            &ReconcileScope::paths([workspace.path("src/module3/f43.rs")]),
            &cancellation,
        )
        .unwrap();
    let measured = started.elapsed() + crate::watch::QUIESCENCE_WINDOW;

    assert_eq!(report.changed, 1);
    harkness_test_fixtures::latency::record(
        "context::incremental_update",
        measured,
        Duration::from_millis(1_000),
    );
}

/// How many content-addressed rows the repository's cache holds.
fn content_rows(engine: &ContextEngine) -> u64 {
    let counts = engine.index_counts().expect("the cache is readable");
    counts.file_versions + counts.contents
}

/// Restated rather than imported, so a key derivation that changed would fail
/// here instead of agreeing with itself.
#[test]
fn a_worktree_key_is_derived_from_the_checkout_rather_than_the_project() {
    let workspace = Workspace::new();
    let one = ContextEngine::open(
        ContextEngineConfig::new(
            ProjectId::new(),
            &workspace.root,
            &workspace.fixture.data_dir,
        ),
        &Cancellation::default(),
    )
    .unwrap();
    let two = ContextEngine::open(
        ContextEngineConfig::new(
            ProjectId::new(),
            &workspace.root,
            &workspace.fixture.data_dir,
        ),
        &Cancellation::default(),
    )
    .unwrap();

    assert_ne!(one.project_id(), two.project_id());
    assert_eq!(one.worktree_key(), two.worktree_key());
    assert_eq!(
        one.worktree_key(),
        WorktreeKey::for_root(&fs::canonicalize(&workspace.root).unwrap())
    );
}
