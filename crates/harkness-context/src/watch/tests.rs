//! The hint pipeline, and the two things it must never do.
//!
//! A hint may be wrong in either direction and cost nothing but work — that is
//! the reconciler's guarantee, proved beside it. What this module has to prove
//! is narrower and sharper: that a denied path never becomes a hint at all, and
//! that no burst of events can make the queue unbounded.
//!
//! Almost everything here drives [`ChangeHint`]s directly rather than through a
//! real watcher. That is deliberate: a suite whose assertions depend on when
//! `inotify` decides to deliver is a suite that fails on a loaded machine and
//! says nothing when it does. The one test that does use a backend asserts an
//! outcome with a generous deadline, and falls back to the injector where no
//! backend exists — so it proves something on every runner instead of being
//! skipped on some.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harkness_core::ProjectId;
use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, initialize_repository};

use super::{
    ChangeClass, ChangeHint, DirtySet, FilesystemChange, Normalizer, QUIESCENCE_WINDOW,
    WATCH_QUEUE_CAPACITY, WatchError, WatchEvent, WatchOptions, WatchService, WatchState,
};
use crate::engine::{ContextEngine, ContextEngineConfig};
use crate::path::RepoPath;
use crate::reconcile::ReconcileScope;

/// Long enough that a burst still coalesces, short enough that a test that
/// waits out several windows finishes in well under a second.
const TEST_QUIESCENCE: Duration = Duration::from_millis(40);

/// How long a test waits for the worker to catch up before failing.
///
/// Generous by construction: it bounds a failure rather than measuring a
/// success, and the latency target is measured by its own `#[ignore]`d test
/// under `--release`.
const TEST_DEADLINE: Duration = Duration::from_secs(20);

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

    fn engine(&self) -> Arc<ContextEngine> {
        Arc::new(
            ContextEngine::open(
                ContextEngineConfig::new(ProjectId::new(), &self.root, &self.fixture.data_dir),
                &Cancellation::default(),
            )
            .expect("the engine opens"),
        )
    }

    fn write(&self, relative: &str, body: &str) {
        let target = self.root.join(relative);
        fs::create_dir_all(target.parent().expect("a file has a parent")).unwrap();
        fs::write(target, body).unwrap();
    }

    fn path(&self, relative: &str) -> RepoPath {
        RepoPath::from_path(Path::new(relative))
    }

    fn normalizer(&self) -> Normalizer {
        Normalizer::new(&self.root).expect("the built-in rules compile")
    }

    fn change(&self, relative: &str, class: ChangeClass) -> FilesystemChange {
        FilesystemChange {
            paths: vec![self.root.join(relative)],
            class,
        }
    }
}

fn path_hint(relative: &str) -> ChangeHint {
    ChangeHint::Path(RepoPath::from_path(Path::new(relative)))
}

fn subtree_hint(relative: &str) -> ChangeHint {
    ChangeHint::Subtree(RepoPath::from_path(Path::new(relative)))
}

// -- the normalizer ----------------------------------------------------------

/// The mapping table, in one place, so a backend kind that starts arriving
/// differently fails here rather than in whatever it silently stopped
/// indexing.
#[test]
fn every_event_class_maps_to_the_hint_its_path_deserves() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    workspace.write("src/nested/inner.rs", "fn inner() {}\n");
    let normalizer = workspace.normalizer();

    // A file that exists is a path hint, whatever said so — and a path hint is
    // the strong one, hashed however unchanged its metadata looks.
    for class in [
        ChangeClass::Created,
        ChangeClass::Modified,
        ChangeClass::Renamed,
    ] {
        assert_eq!(
            normalizer.normalize(&workspace.change("src/main.rs", class.clone())),
            vec![path_hint("src/main.rs")],
            "{class:?} on an existing file"
        );
    }

    // A directory is a subtree hint, so everything inside it is
    // metadata-compared rather than rehashed.
    assert_eq!(
        normalizer.normalize(&workspace.change("src/nested", ChangeClass::Created)),
        vec![subtree_hint("src/nested")]
    );

    // A removal cannot be stat-ed to find out what it was, so it is a subtree
    // hint either way: on a file that is its own row, on a directory it is
    // every row beneath it.
    assert_eq!(
        normalizer.normalize(&workspace.change("src/main.rs", ChangeClass::Removed)),
        vec![subtree_hint("src/main.rs")]
    );
    assert_eq!(
        normalizer.normalize(&workspace.change("src/vanished.rs", ChangeClass::Modified)),
        vec![subtree_hint("src/vanished.rs")],
        "a path that is already gone is the rename's source half"
    );

    // A rescan is the backend saying it lost track, and nothing about the
    // worktree may be assumed afterwards.
    assert_eq!(
        normalizer.normalize(&workspace.change("src/main.rs", ChangeClass::Rescan)),
        vec![ChangeHint::Overflow]
    );

    // A path outside the root belongs to nobody.
    assert!(
        normalizer
            .normalize(&FilesystemChange {
                paths: vec![workspace.fixture.root.path().join("elsewhere.rs")],
                class: ChangeClass::Modified,
            })
            .is_empty()
    );
}

/// Layer 1 applies before anything is queued. A denied path changing on disk
/// must produce no hint, so nothing downstream ever holds its name — the same
/// contract the walk keeps, from the same compiled list.
#[test]
fn a_denied_path_never_becomes_a_hint() {
    let workspace = Workspace::new();
    workspace.write(".env", "SECRET=1\n");
    workspace.write("config/.env", "SECRET=2\n");
    workspace.write(".ssh/id_rsa", "-----BEGIN\n");
    let normalizer = workspace.normalizer();

    for denied in [".env", "config/.env", ".ssh/id_rsa"] {
        for class in [
            ChangeClass::Created,
            ChangeClass::Modified,
            ChangeClass::Removed,
            ChangeClass::Renamed,
        ] {
            let hints = normalizer.normalize(&workspace.change(denied, class.clone()));
            assert!(
                hints.is_empty(),
                "'{denied}' produced {hints:?} for {class:?}"
            );
        }
    }

    // And the eligible file beside them still does.
    workspace.write("config/settings.toml", "a = 1\n");
    assert_eq!(
        normalizer.normalize(&workspace.change("config/settings.toml", ChangeClass::Modified)),
        vec![path_hint("config/settings.toml")]
    );
}

/// An atomic save is a temporary file, a rename onto the target, and a removal
/// of a path that no longer exists. Exactly one of those is worth reconciling.
#[test]
fn an_atomic_save_yields_one_hint_for_the_target_and_none_for_the_temporary() {
    let workspace = Workspace::new();
    workspace.write("notes.md", "# after\n");
    let normalizer = workspace.normalizer();

    let mut hints = Vec::new();
    for change in [
        workspace.change("notes.md.tmp", ChangeClass::Created),
        workspace.change("notes.md.tmp", ChangeClass::Modified),
        workspace.change(".#notes.md", ChangeClass::Created),
        workspace.change("4913", ChangeClass::Created),
        workspace.change("notes.md", ChangeClass::Renamed),
        workspace.change("notes.md.tmp", ChangeClass::Removed),
    ] {
        hints.extend(normalizer.normalize(&change));
    }

    assert_eq!(hints, vec![path_hint("notes.md")]);
}

/// The administrative directory is not content, and `HEAD` is the one thing in
/// it worth reading: rewriting it is what a branch switch does, and the ten
/// thousand working-tree events that follow are exactly the storm the dirty set
/// would collapse into this hint anyway.
#[test]
fn the_git_directory_says_nothing_except_that_head_moved() {
    let workspace = Workspace::new();
    let normalizer = workspace.normalizer();

    for quiet in [
        ".git",
        ".git/index",
        ".git/refs/heads/main",
        ".git/objects/ab/cd",
    ] {
        assert!(
            normalizer
                .normalize(&workspace.change(quiet, ChangeClass::Modified))
                .is_empty(),
            "'{quiet}' should say nothing"
        );
    }
    assert_eq!(
        normalizer.normalize(&workspace.change(".git/HEAD", ChangeClass::Modified)),
        vec![ChangeHint::Subtree(RepoPath::from_bytes(Vec::new()))],
        "a moved HEAD is a whole-worktree hint"
    );
}

// -- the dirty set -----------------------------------------------------------

/// A marker swallows what it covers, so the set can only shrink when a wider
/// hint arrives. Without it, a directory hint and the thousand file hints
/// beneath it would each be carried and each be reconciled.
#[test]
fn a_subtree_marker_absorbs_the_paths_beneath_it() {
    let mut dirty = DirtySet::new();
    dirty.insert(path_hint("src/a.rs"));
    dirty.insert(path_hint("src/b.rs"));
    dirty.insert(path_hint("docs/guide.md"));
    assert_eq!(dirty.len(), 3);

    dirty.insert(subtree_hint("src"));
    assert_eq!(
        dirty.len(),
        2,
        "the two under src collapsed into the marker"
    );

    // And a path already covered is absorbed rather than added.
    dirty.insert(path_hint("src/c.rs"));
    assert_eq!(dirty.len(), 2);

    let scope = dirty.take();
    match &scope {
        ReconcileScope::Paths(paths) => {
            assert_eq!(
                paths.iter().map(RepoPath::display).collect::<Vec<_>>(),
                ["docs/guide.md", "src"]
            );
        }
        other => panic!("expected a path list, found {other:?}"),
    }
    assert!(dirty.is_empty());
}

/// A checkout touching ten thousand files must cost one reconcile and a bounded
/// amount of memory, not ten thousand of either. The set never grows past its
/// capacity because reaching it replaces the contents rather than adding to
/// them.
#[test]
fn an_event_storm_collapses_into_one_full_pass_with_bounded_memory() {
    let mut dirty = DirtySet::new();
    let mut high_water = 0;
    for index in 0..10_000 {
        dirty.insert(path_hint(&format!("src/module{}/f{index}.rs", index % 64)));
        high_water = high_water.max(dirty.len());
    }

    assert!(
        high_water <= WATCH_QUEUE_CAPACITY + 1,
        "the set reached {high_water} entries"
    );
    assert!(dirty.is_collapsed());
    assert_eq!(dirty.len(), 0, "a collapsed set carries no paths at all");
    assert!(dirty.overflows() >= 1);
    assert_eq!(dirty.take(), ReconcileScope::Full);
    assert!(dirty.is_empty());
}

/// A backend that reports a rescan is saying it lost track, and one that
/// reports an error is saying the same thing less politely. Both collapse.
#[test]
fn an_overflow_hint_discards_what_was_queued_rather_than_carrying_it() {
    let mut dirty = DirtySet::new();
    dirty.insert(path_hint("src/a.rs"));
    dirty.insert(subtree_hint("docs"));

    dirty.insert(ChangeHint::Overflow);

    assert!(dirty.is_collapsed());
    assert_eq!(dirty.len(), 0);
    assert_eq!(dirty.overflows(), 1);
    assert_eq!(dirty.take(), ReconcileScope::Full);
}

/// The worktree root is everything, whichever spelling reaches the set.
#[test]
fn a_root_subtree_hint_is_the_whole_worktree() {
    let mut dirty = DirtySet::new();
    dirty.insert(path_hint("src/a.rs"));
    dirty.insert(ChangeHint::Subtree(RepoPath::from_bytes(Vec::new())));

    assert!(dirty.is_collapsed());
    assert_eq!(dirty.take(), ReconcileScope::Full);
}

/// A pass the *cache* refused — another process holding the write lock, or
/// publishing this worktree first — is put back on the queue, because the scope
/// was drained when it started and dropping it would leave exactly the paths
/// something told us about unexamined. It comes back covering what it covered,
/// and at the strength it had: a retry that downgraded a file hint to a subtree
/// marker would drop the suspicion the hint existed for.
#[test]
fn a_refused_pass_comes_back_covering_what_it_covered() {
    let scope = ReconcileScope::paths([
        RepoPath::from_bytes(b"src/a.rs".to_vec()),
        RepoPath::from_bytes(b"docs".to_vec()),
    ]);

    let mut dirty = DirtySet::new();
    for hint in super::scope_hints(&scope) {
        dirty.insert(hint);
    }
    let again = dirty.take();

    assert_eq!(again, scope);
    assert!(again.names_exactly(&RepoPath::from_bytes(b"src/a.rs".to_vec())));

    // A pass that covered everything cannot be narrowed on its way back in.
    let mut dirty = DirtySet::new();
    dirty.insert(path_hint("unrelated.rs"));
    for hint in super::scope_hints(&ReconcileScope::Full) {
        dirty.insert(hint);
    }
    assert_eq!(dirty.take(), ReconcileScope::Full);

    let subtree = ReconcileScope::subtree(RepoPath::from_bytes(b"src".to_vec()));
    let mut dirty = DirtySet::new();
    for hint in super::scope_hints(&subtree) {
        dirty.insert(hint);
    }
    assert!(
        dirty
            .take()
            .covers(&RepoPath::from_bytes(b"src/deep/f.rs".to_vec()))
    );
}

// -- the error namespace -----------------------------------------------------

/// Published spellings a front end may depend on, in declaration order.
#[test]
fn every_watch_variant_maps_to_a_listed_kind_in_declaration_order() {
    let cases = [
        (
            WatchError::WatcherUnavailable {
                path: PathBuf::from("/w"),
                reason: "inotify watch limit reached".to_owned(),
            },
            "watcher_unavailable",
        ),
        (
            WatchError::WatchRootMissing {
                path: PathBuf::from("/w"),
            },
            "watch_root_missing",
        ),
        (
            WatchError::QueueOverflow { dropped: 4_096 },
            "queue_overflow",
        ),
        (WatchError::Cancelled, "cancelled"),
    ];

    let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
    assert_eq!(kinds, WatchError::KINDS);
    for (error, expected) in cases {
        assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
    }

    let mut sorted = WatchError::KINDS.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    sorted.dedup();
    assert_eq!(sorted.len(), count);
}

/// A cancelled watch and a cancelled facade call are one answer, so the
/// spelling is published once — exactly as it is for a cancelled walk.
#[test]
fn a_carried_watch_failure_keeps_its_own_kind_and_cancellation_does_not() {
    let carried = crate::ContextEngineError::from(WatchError::WatcherUnavailable {
        path: PathBuf::from("/w"),
        reason: "no backend".to_owned(),
    });
    assert_eq!(carried.kind(), "watcher_unavailable");
    assert!(crate::ContextEngineError::kinds().contains(&"watcher_unavailable"));

    let cancelled = crate::ContextEngineError::from(WatchError::Cancelled);
    assert_eq!(cancelled.kind(), "cancelled");
    assert!(matches!(cancelled, crate::ContextEngineError::Cancelled));
}

// -- the service -------------------------------------------------------------

/// Collects what a service reported, so a test can assert on passes without
/// racing the worker.
#[derive(Clone, Default)]
struct Observed {
    events: Arc<Mutex<Vec<String>>>,
    passes: Arc<AtomicUsize>,
}

impl Observed {
    fn observer(&self) -> impl Fn(&WatchEvent) + Send + Sync + 'static {
        let events = Arc::clone(&self.events);
        let passes = Arc::clone(&self.passes);
        move |event| {
            let rendered = match event {
                WatchEvent::Started { scope } => format!("started:{}", scope.kind()),
                WatchEvent::Finished(report) => {
                    passes.fetch_add(1, Ordering::AcqRel);
                    format!(
                        "finished:{}:added={}:changed={}:removed={}",
                        report.effective_scope().kind(),
                        report.added,
                        report.changed,
                        report.removed
                    )
                }
                WatchEvent::Failed { kind, .. } => format!("failed:{kind}"),
                WatchEvent::Degraded { kind, .. } => format!("degraded:{kind}"),
            };
            events.lock().unwrap().push(rendered);
        }
    }

    fn rendered(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

/// Events are not truth, proved by taking the events away. With no backend in
/// the process at all, a change made before the watch started is still found by
/// the startup sweep and one made afterwards is still found through a hint a
/// caller supplied.
#[test]
fn a_watch_with_no_backend_still_sweeps_and_still_reconciles() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    let engine = workspace.engine();
    let observed = Observed::default();

    let service = WatchService::start(
        Arc::clone(&engine),
        WatchOptions::new()
            .without_filesystem_events()
            .with_quiescence(TEST_QUIESCENCE)
            .observed_by(observed.observer()),
    )
    .expect("a watch with no backend still starts");

    assert!(
        matches!(service.status().state, WatchState::Degraded { kind, .. } if kind == "watcher_unavailable")
    );
    assert!(
        service.wait_until_quiet(TEST_DEADLINE),
        "the sweep finished"
    );
    assert!(
        engine
            .indexed_file(&workspace.path("src/main.rs"))
            .unwrap()
            .is_some(),
        "the startup sweep indexed what was already there"
    );

    // A change nothing observed, discovered because a caller said where to
    // look.
    workspace.write("src/added.rs", "fn added() {}\n");
    service.hint(path_hint("src/added.rs"));
    assert!(service.wait_until_quiet(TEST_DEADLINE));

    assert!(
        engine
            .indexed_file(&workspace.path("src/added.rs"))
            .unwrap()
            .is_some()
    );
    let rendered = observed.rendered();
    assert!(
        rendered.contains(&"degraded:watcher_unavailable".to_owned()),
        "{rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.starts_with("finished:full:")),
        "{rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.starts_with("finished:paths:") && line.contains("added=1")),
        "{rendered:?}"
    );
}

/// A burst of hints is one pass. Without the quiescence window an editor's save
/// followed by a formatter's rewrite would be two, and a checkout would be ten
/// thousand.
#[test]
fn a_burst_of_hints_becomes_one_pass() {
    let workspace = Workspace::new();
    for index in 0..6 {
        workspace.write(&format!("src/f{index}.rs"), "fn a() {}\n");
    }
    let engine = workspace.engine();
    let observed = Observed::default();
    let service = WatchService::start(
        Arc::clone(&engine),
        WatchOptions::new()
            .without_filesystem_events()
            .with_quiescence(TEST_QUIESCENCE)
            .observed_by(observed.observer()),
    )
    .unwrap();
    assert!(service.wait_until_quiet(TEST_DEADLINE));
    let after_sweep = observed.passes.load(Ordering::Acquire);

    for index in 0..6 {
        workspace.write(&format!("src/f{index}.rs"), "fn a() {}\nfn b() {}\n");
        service.hint(path_hint(&format!("src/f{index}.rs")));
    }
    assert!(service.wait_until_quiet(TEST_DEADLINE));

    assert_eq!(
        observed.passes.load(Ordering::Acquire) - after_sweep,
        1,
        "six hints inside one window are one reconcile: {:?}",
        observed.rendered()
    );
    let rendered = observed.rendered();
    assert!(
        rendered
            .last()
            .is_some_and(|line| line.contains("changed=6")),
        "{rendered:?}"
    );
}

/// Ten thousand hints arriving at once cost one full pass, not ten thousand
/// targeted ones — and the queue never holds ten thousand of anything.
#[test]
fn a_storm_of_hints_costs_one_full_pass() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    let engine = workspace.engine();
    let observed = Observed::default();
    let service = WatchService::start(
        Arc::clone(&engine),
        WatchOptions::new()
            .without_filesystem_events()
            .with_quiescence(TEST_QUIESCENCE)
            .observed_by(observed.observer()),
    )
    .unwrap();
    assert!(service.wait_until_quiet(TEST_DEADLINE));
    let after_sweep = observed.passes.load(Ordering::Acquire);

    for index in 0..10_000 {
        service.hint(path_hint(&format!("src/module{}/f{index}.rs", index % 64)));
        assert!(
            service.status().queue_depth <= WATCH_QUEUE_CAPACITY + 1,
            "the queue grew past its capacity"
        );
    }
    assert!(service.wait_until_quiet(TEST_DEADLINE));

    assert_eq!(observed.passes.load(Ordering::Acquire) - after_sweep, 1);
    let rendered = observed.rendered();
    assert!(
        rendered
            .last()
            .is_some_and(|line| line.starts_with("finished:full:")),
        "{rendered:?}"
    );
    assert!(service.status().overflows >= 1);
}

/// The end-to-end promise, with a real backend where one exists: edit a file
/// and the index catches up on its own. The deadline is generous because it
/// bounds a failure rather than measuring a success; the latency target is a
/// separate, `#[ignore]`d measurement.
#[test]
fn an_edit_reaches_the_index_without_anybody_asking() {
    let workspace = Workspace::new();
    workspace.write("src/main.rs", "fn main() {}\n");
    let engine = workspace.engine();
    let service = WatchService::start(
        Arc::clone(&engine),
        WatchOptions::new().with_quiescence(TEST_QUIESCENCE),
    )
    .expect("the watch starts");
    assert!(service.wait_until_quiet(TEST_DEADLINE));
    let before = engine
        .indexed_file(&workspace.path("src/main.rs"))
        .unwrap()
        .expect("the sweep indexed it");

    workspace.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    // A runner with no notification backend — an exhausted inotify table, a
    // filesystem that does not support it — degrades rather than fails, and the
    // caller-supplied hint is the supported way to work there. Asserting the
    // outcome on every runner is worth more than skipping the test on some.
    if matches!(service.status().state, WatchState::Degraded { .. }) {
        service.hint(path_hint("src/main.rs"));
    }
    let deadline = std::time::Instant::now() + TEST_DEADLINE;
    let after = loop {
        assert!(service.wait_until_quiet(TEST_DEADLINE));
        let row = engine
            .indexed_file(&workspace.path("src/main.rs"))
            .unwrap()
            .expect("the file is still indexed");
        if row.content_sha256 != before.content_sha256 {
            break row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edit never reached the index"
        );
        std::thread::yield_now();
    };

    assert_ne!(after.content_sha256, before.content_sha256);
    assert!(after.byte_size > before.byte_size);
}

/// Stopping is idempotent, and dropping is stopping. A service that needed an
/// explicit stop would leave a worker holding an engine — and a cache handle —
/// for the life of the process.
#[test]
fn stopping_is_idempotent_and_dropping_is_stopping() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    let mut service = WatchService::start(
        Arc::clone(&engine),
        WatchOptions::new()
            .without_filesystem_events()
            .with_quiescence(TEST_QUIESCENCE),
    )
    .unwrap();
    assert!(service.wait_until_quiet(TEST_DEADLINE));

    service.stop();
    assert_eq!(service.status().state, WatchState::Stopped);
    service.stop();
    assert_eq!(service.status().state, WatchState::Stopped);
    // A hint offered to a stopped service is dropped rather than queued for a
    // worker that will never read it.
    service.hint(path_hint("src/main.rs"));
    assert_eq!(service.status().queue_depth, 0);
    drop(service);

    assert_eq!(
        Arc::strong_count(&engine),
        1,
        "the worker released the engine"
    );
}

/// A watch on a root that is not there is the one refusal, because it leaves
/// nothing to watch *and* nothing to sweep.
#[test]
fn a_missing_worktree_root_is_the_one_refusal() {
    let workspace = Workspace::new();
    let engine = workspace.engine();
    fs::remove_dir_all(&workspace.root).unwrap();

    let error =
        WatchService::start(engine, WatchOptions::new()).expect_err("there is nothing to watch");

    assert_eq!(error.kind(), "watch_root_missing");
}

/// The emission bound, held to the constant that produces it rather than to a
/// throttle that could silently drop the one event a surface was waiting for.
/// At most one pass starts and one finishes per window.
#[test]
fn the_quiescence_window_bounds_the_event_rate() {
    let per_pass = 2_u32;
    let windows_per_second = 1_000 / u32::try_from(QUIESCENCE_WINDOW.as_millis()).unwrap();
    assert!(
        per_pass * windows_per_second <= 4,
        "the window admits more than four events a second"
    );
}
