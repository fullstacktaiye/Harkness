use std::fs;
use std::path::{Path, PathBuf};

use harkness_git::Cancellation;
use harkness_test_fixtures::{Fixture, child_path, park, signal_ready, spawn_child};
use rusqlite::Connection;

use super::{
    ExpectedVersions, INDEX_DATABASE_FILE, INDEX_SCHEMA_VERSION, IndexCache, IndexComponent,
    MAX_QUARANTINED_CACHES, QUARANTINE_PREFIX, RecreationReason, next_generation,
    prune_quarantines,
};
use crate::error::ContextEngineError;

const IDENTITY: &str = "11111111-1111-5111-8111-111111111111";

const PROCESS_CHILD_TEST: &str = "index::tests::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_CONTEXT_TEST_ROLE";
const PROCESS_CACHE_ROOT_ENV: &str = "HARKNESS_CONTEXT_TEST_CACHE_ROOT";
const PROCESS_READY_FILE_ENV: &str = "HARKNESS_CONTEXT_TEST_READY_FILE";

struct CacheFixture {
    fixture: Fixture,
    root: PathBuf,
}

impl CacheFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let root = fixture.root.path().join("cache-root");
        Self { fixture, root }
    }

    fn open(&self) -> Result<IndexCache, ContextEngineError> {
        self.open_expecting(&ExpectedVersions::current())
    }

    fn open_expecting(
        &self,
        expected: &ExpectedVersions,
    ) -> Result<IndexCache, ContextEngineError> {
        IndexCache::open_or_create(&self.root, expected, IDENTITY)
    }

    fn database(&self) -> PathBuf {
        self.root.join(INDEX_DATABASE_FILE)
    }

    /// Replaces the cache on disk with bytes that are not a database.
    ///
    /// The write-ahead log goes too. A cache is three files, and truncating one
    /// of them while the other two still describe a healthy database is not a
    /// corrupt cache — it is a cache SQLite reads perfectly well out of the log.
    fn corrupt_on_disk(&self) {
        fs::write(self.database(), &b"not a database"[..10]).unwrap();
        for suffix in ["-wal", "-shm"] {
            let mut name = self.database().into_os_string();
            name.push(suffix);
            let _ = fs::remove_file(PathBuf::from(name));
        }
    }

    fn quarantined(&self) -> Vec<String> {
        quarantined_in(&self.root)
    }
}

fn quarantined_in(directory: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(QUARANTINE_PREFIX))
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// The cache root is created on demand, exactly as the artifact store's is.
#[test]
fn opening_a_missing_cache_creates_it_with_one_metadata_row() {
    let fixture = CacheFixture::new();

    let cache = fixture.open().unwrap();

    assert!(fixture.database().is_file());
    let meta = cache.meta();
    assert_eq!(meta.schema_version, INDEX_SCHEMA_VERSION);
    assert_eq!(meta.repository_identity, IDENTITY);
    assert!(meta.index_generation > 0);
    assert_eq!(cache.generation(), meta.index_generation);
    assert!(
        cache.status().last_recreation.is_none(),
        "a first build is not a recreation"
    );

    let rows: i64 = Connection::open(fixture.database())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM index_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    drop(fixture.fixture);
}

/// Reopening an intact cache keeps its generation. A generation that moved on
/// every open would invalidate every stored snapshot on every process start.
#[test]
fn reopening_an_intact_cache_keeps_its_generation() {
    let fixture = CacheFixture::new();
    let first = fixture.open().unwrap();
    let generation = first.generation();
    drop(first);

    let second = fixture.open().unwrap();

    assert_eq!(second.generation(), generation);
    assert!(second.status().last_recreation.is_none());
}

/// Deleting the whole cache directory is the supported recovery action, and the
/// generation must still move — a stale snapshot may not verify as fresh
/// against a cache that was rebuilt from nothing.
#[test]
fn a_deleted_cache_directory_reopens_with_a_greater_generation() {
    let fixture = CacheFixture::new();
    let first = fixture.open().unwrap();
    let generation = first.generation();
    drop(first);
    fs::remove_dir_all(&fixture.root).unwrap();

    let second = fixture.open().unwrap();

    assert!(
        second.generation() > generation,
        "{} did not advance past {generation}",
        second.generation()
    );
}

/// A truncated database is not a database. It is set aside rather than deleted,
/// and the engine comes back up on an empty one.
#[test]
fn a_corrupt_cache_is_quarantined_and_replaced() {
    let fixture = CacheFixture::new();
    let first = fixture.open().unwrap();
    let generation = first.generation();
    drop(first);
    fixture.corrupt_on_disk();

    let second = fixture.open().unwrap();

    let recreation = second.status().last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Corrupt);
    assert_eq!(
        recreation.previous_generation, None,
        "an unreadable cache cannot report what generation it held"
    );
    assert!(second.generation() > generation);
    let quarantined = fixture.quarantined();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(
        recreation.quarantined_to,
        Some(fixture.root.join(&quarantined[0]))
    );
    assert_eq!(
        fs::read(fixture.root.join(&quarantined[0])).unwrap(),
        b"not a data",
        "the quarantined bytes are the ones that were found"
    );
    assert_eq!(second.meta().schema_version, INDEX_SCHEMA_VERSION);
}

/// A database with no `index_meta` is as unusable as a truncated one: nothing
/// says what produced its rows, so nothing may be read out of them.
#[test]
fn a_cache_without_metadata_is_quarantined() {
    let fixture = CacheFixture::new();
    fs::create_dir_all(&fixture.root).unwrap();
    Connection::open(fixture.database())
        .unwrap()
        .execute_batch("CREATE TABLE chunks (id TEXT PRIMARY KEY) STRICT;")
        .unwrap();

    let cache = fixture.open().unwrap();

    let recreation = cache.status().last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Corrupt);
    assert_eq!(fixture.quarantined().len(), 1);
}

/// A cache that names another repository is not this repository's cache, and
/// serving one checkout's rows for another is the bleed the derived path exists
/// to prevent.
#[test]
fn a_cache_recording_another_repository_is_quarantined() {
    let fixture = CacheFixture::new();
    let other = IndexCache::open_or_create(
        &fixture.root,
        &ExpectedVersions::current(),
        "22222222-2222-5222-8222-222222222222",
    )
    .unwrap();
    let generation = other.generation();
    drop(other);

    let cache = fixture.open().unwrap();

    let recreation = cache.status().last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Corrupt);
    assert_eq!(recreation.previous_generation, Some(generation));
    assert!(recreation.detail.contains("22222222"));
    assert_eq!(cache.meta().repository_identity, IDENTITY);
}

/// The refusal has to leave the file exactly as it was found: a newer sibling
/// process is still using it, and truncating it would be this build corrupting
/// a working cache.
#[test]
fn a_newer_cache_is_refused_and_left_byte_identical() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    drop(cache);
    let connection = Connection::open(fixture.database()).unwrap();
    connection
        .execute(
            "UPDATE index_meta SET schema_version = ?1",
            [i64::from(INDEX_SCHEMA_VERSION + 1)],
        )
        .unwrap();
    drop(connection);
    let before = fs::read(fixture.database()).unwrap();

    let error = fixture.open().unwrap_err();

    assert_eq!(error.kind(), "cache_version_conflict");
    assert!(
        matches!(error, ContextEngineError::CacheVersionConflict { found, maximum, .. }
            if found == INDEX_SCHEMA_VERSION + 1 && maximum == INDEX_SCHEMA_VERSION),
        "unexpected refusal: {error}"
    );
    assert_eq!(
        fs::read(fixture.database()).unwrap(),
        before,
        "a refused cache must keep the bytes it arrived with"
    );
    assert!(fixture.quarantined().is_empty());
}

/// An older schema has no downgrade path and no incremental migration: the
/// cache is disposable, so it is replaced rather than reconciled.
#[test]
fn an_older_cache_is_replaced_rather_than_migrated() {
    let fixture = CacheFixture::new();
    let older = ExpectedVersions {
        schema_version: INDEX_SCHEMA_VERSION,
        ..ExpectedVersions::current()
    };
    let cache = fixture.open_expecting(&older).unwrap();
    let generation = cache.generation();
    drop(cache);
    let connection = Connection::open(fixture.database()).unwrap();
    connection
        .execute("UPDATE index_meta SET schema_version = 0", [])
        .unwrap();
    drop(connection);

    let replaced = fixture.open().unwrap();

    let recreation = replaced.status().last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Version);
    assert_eq!(recreation.previous_generation, Some(generation));
    assert!(replaced.generation() > generation);
    assert_eq!(fixture.quarantined().len(), 1);
}

/// A component version says what produced the rows, not where they sit, so the
/// rows are kept and the disagreement is reported for incremental reconciliation.
#[test]
fn a_component_version_mismatch_keeps_the_cache_and_reports_the_skew() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    let generation = cache.generation();
    drop(cache);

    let upgraded = ExpectedVersions {
        parser_version: "2".to_owned(),
        ranking_version: "3".to_owned(),
        ..ExpectedVersions::current()
    };
    let cache = fixture.open_expecting(&upgraded).unwrap();

    assert_eq!(
        cache.generation(),
        generation,
        "a parser upgrade is not a rebuild"
    );
    assert!(cache.status().last_recreation.is_none());
    let skew = cache.status().stale_components;
    assert_eq!(
        skew.iter().map(|entry| entry.component).collect::<Vec<_>>(),
        [IndexComponent::Parser, IndexComponent::Ranking]
    );
    assert_eq!(skew[0].stored, "0");
    assert_eq!(skew[0].expected, "2");
    assert_eq!(
        cache.meta().parser_version,
        "0",
        "the stored version is what reconciliation needs; overwriting it would erase the work"
    );
}

/// Disposal is what "delete this to reclaim disk" resolves to inside a live
/// process, and it must move the generation exactly as a wipe does.
#[test]
fn disposing_replaces_the_cache_and_advances_the_generation() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    let generation = cache.generation();

    let recreation = cache.dispose().unwrap();

    assert_eq!(recreation.reason, RecreationReason::Disposed);
    assert_eq!(recreation.previous_generation, Some(generation));
    assert!(recreation.generation > generation);
    assert_eq!(cache.generation(), recreation.generation);
    assert!(
        recreation.quarantined_to.is_none(),
        "a caller asking to be rid of the cache is not reporting a fault"
    );
    assert!(fixture.quarantined().is_empty());
    assert_eq!(cache.meta().repository_identity, IDENTITY);
}

#[test]
fn refreshing_reports_the_generation_and_the_stale_components() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();

    let report = cache.refresh(&Cancellation::default()).unwrap();

    assert_eq!(report.generation, cache.generation());
    assert!(report.stale_components.is_empty());
    assert_eq!(report.entries_reconciled, 0);
    assert!(cache.status().last_refreshed_at.is_some());
    assert!(
        cache.status().in_progress.is_none(),
        "the operation must be cleared when it finishes"
    );
}

/// An already-cancelled token launches nothing at all.
#[test]
fn refreshing_under_a_cancelled_token_starts_nothing() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let error = cache.refresh(&cancellation).unwrap_err();

    assert_eq!(error.kind(), "cancelled");
    assert!(cache.status().last_refreshed_at.is_none());
}

/// A cache that stops being a cache while it is open is set aside and replaced,
/// and the call that met the fault fails — the cache it addressed is gone even
/// though the engine is healthy again.
#[test]
fn a_cache_that_faults_mid_life_is_quarantined_and_the_call_fails() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    let generation = cache.generation();
    // What another process, or a filesystem, can do to the files underneath a
    // live handle. Refresh re-reads them rather than this process's page cache,
    // which is what makes it notice.
    fixture.corrupt_on_disk();

    let error = cache.refresh(&Cancellation::default()).unwrap_err();

    assert_eq!(error.kind(), "cache_corrupt_quarantined");
    assert_eq!(fixture.quarantined().len(), 1);
    assert!(cache.generation() > generation);
    let recreation = cache.status().last_recreation.expect("a recreation");
    assert_eq!(recreation.reason, RecreationReason::Corrupt);
    // The replacement is healthy, so the next refresh succeeds.
    cache.refresh(&Cancellation::default()).unwrap();
    assert!(cache.status().last_refreshed_at.is_some());
}

/// Two front ends share one cache, so the file behind a live handle can be
/// rebuilt by somebody else. Refresh is where that is noticed.
#[test]
fn refreshing_adopts_a_cache_another_process_rebuilt() {
    let fixture = CacheFixture::new();
    let held = fixture.open().unwrap();
    let generation = held.generation();
    let other = fixture.open().unwrap();

    let rebuilt = other.dispose().unwrap();
    assert!(rebuilt.generation > generation);
    assert_eq!(
        held.generation(),
        generation,
        "the other handle has not looked yet"
    );

    let report = held.refresh(&Cancellation::default()).unwrap();

    assert_eq!(report.generation, rebuilt.generation);
    assert_eq!(held.generation(), rebuilt.generation);
}

/// Rotation keeps the newest two. A repeatedly failing cache must not be able
/// to fill a disk with copies of itself.
#[test]
fn quarantine_rotation_keeps_the_newest_two() {
    let fixture = Fixture::new();
    let directory = fixture.directory("rotation");
    let database = directory.join(INDEX_DATABASE_FILE);
    for index in 0..5 {
        fs::write(
            directory.join(format!(
                "{QUARANTINE_PREFIX}2026010{index}T000000000000000Z"
            )),
            [],
        )
        .unwrap();
    }

    prune_quarantines(&database);

    assert_eq!(
        quarantined_in(&directory),
        [
            format!("{QUARANTINE_PREFIX}20260103T000000000000000Z"),
            format!("{QUARANTINE_PREFIX}20260104T000000000000000Z"),
        ]
    );
    assert_eq!(quarantined_in(&directory).len(), MAX_QUARANTINED_CACHES);
}

/// The floor is what keeps a clock that stepped backwards from reissuing a
/// number some stored snapshot already recorded.
#[test]
fn a_generation_never_repeats_even_when_the_clock_goes_backwards() {
    let far_future = u64::MAX - 1;

    assert_eq!(next_generation(Some(far_future)), u64::MAX);
    assert!(next_generation(None) > 0);
    assert!(next_generation(Some(0)) > 1);
}

/// Two handles share one cache; WAL and the busy timeout are what make that
/// safe, and both must come up rather than one of them deciding the other's
/// file is broken.
#[test]
fn two_connections_open_one_cache_without_disturbing_it() {
    let fixture = CacheFixture::new();
    let first = fixture.open().unwrap();
    let second = fixture.open().unwrap();

    assert_eq!(first.generation(), second.generation());
    assert!(first.status().last_recreation.is_none());
    assert!(second.status().last_recreation.is_none());
    assert!(fixture.quarantined().is_empty());
    assert_eq!(integrity_of(&fixture.database()), "ok");
}

/// Re-entered by the concurrent-access test so the second reader is a genuinely
/// separate process — the shape a command line beside a running application has.
#[test]
#[ignore = "only run as a child process by the concurrent cache-access test"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let cache_root = child_path(PROCESS_CACHE_ROOT_ENV);
    match role.as_str() {
        "hold-open-cache" => {
            let cache =
                IndexCache::open_or_create(&cache_root, &ExpectedVersions::current(), IDENTITY)
                    .unwrap();
            cache.refresh(&Cancellation::default()).unwrap();
            signal_ready(PROCESS_READY_FILE_ENV);
            park();
        }
        _ => panic!("unknown test child role: {role}"),
    }
}

/// The application and the command line both reading one repository's cache.
/// Neither may conclude that the other's file is broken, and the file must
/// still verify afterwards.
#[test]
fn a_second_process_reads_one_cache_without_corrupting_it() {
    let fixture = CacheFixture::new();
    let owner = fixture.open().unwrap();
    let generation = owner.generation();
    let ready = fixture.fixture.root.path().join("child-ready");

    let mut child = spawn_child(
        PROCESS_CHILD_TEST,
        PROCESS_ROLE_ENV,
        "hold-open-cache",
        PROCESS_CACHE_ROOT_ENV,
        &fixture.root,
    )
    .env(PROCESS_READY_FILE_ENV, &ready)
    .spawn()
    .unwrap();
    harkness_test_fixtures::wait_for_child_signal(&mut child, &ready);

    // The child is holding the same cache open. This process must still read it,
    // and must not decide the generation moved.
    let report = owner.refresh(&Cancellation::default()).unwrap();
    assert_eq!(report.generation, generation);
    let reader = fixture.open().unwrap();
    assert_eq!(reader.generation(), generation);
    assert!(reader.status().last_recreation.is_none());
    assert!(fixture.quarantined().is_empty());

    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(integrity_of(&fixture.database()), "ok");
}

/// Contention resolves by waiting out the busy timeout and then by a typed
/// refusal. What it must never do is conclude the file is broken: a cache
/// somebody else is writing is one to come back to, and quarantining it would
/// let one front end destroy the other's index by being slow.
#[test]
fn a_cache_another_writer_holds_is_refused_rather_than_quarantined() {
    let fixture = CacheFixture::new();
    let cache = fixture.open().unwrap();
    drop(cache);
    let blocker = Connection::open(fixture.database()).unwrap();
    // Out of WAL first: a WAL reader would not be blocked at all, which is the
    // whole point of the journal mode. Contention is what is under test here.
    blocker
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let before = fs::read(fixture.database()).unwrap();

    let error = fixture.open().unwrap_err();

    assert_eq!(error.kind(), "cache_open_failed");
    assert!(
        fixture.quarantined().is_empty(),
        "a busy cache is not a corrupt cache"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);
    assert_eq!(fs::read(fixture.database()).unwrap(), before);
    assert_eq!(integrity_of(&fixture.database()), "ok");
}

fn integrity_of(database: &Path) -> String {
    Connection::open(database)
        .unwrap()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap()
}
