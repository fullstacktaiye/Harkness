//! The content tables: what a batch publishes, what a version bump takes away,
//! and what the budget refuses.
//!
//! The lifecycle tests beside this one ([`super::tests`]) are about the *file* —
//! opening it, refusing it, quarantining it. These are about the rows.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harkness_git::Cancellation;
use harkness_test_fixtures::Fixture;
use rusqlite::Connection;

use super::store::{BatchScope, SymbolRecord, WorktreeKey};
use super::{
    CLASSIFY_VERSION, ExpectedVersions, IndexCache, IndexComponent, MAX_TOTAL_CONTEXT_BYTES,
    RecreationReason, evict_to_budget, survey,
};
use crate::chunk::{ChunkSet, FileVersion, chunk_file};
use crate::classify::FileClass;
use crate::error::ContextEngineError;
use crate::ids::{FileVersionId, SnapshotId, SymbolId};
use crate::inventory::InventoryEntry;
use crate::path::RepoPath;
use crate::provenance::ByteRange;
use crate::symbols::{
    ExtractionSkipReason, FileSymbols, LanguageDetection, LanguageDetectionSource, ParseHealth,
    SymbolKind,
};

const IDENTITY: &str = "22222222-2222-5222-8222-222222222222";

struct StoreFixture {
    fixture: Fixture,
    root: PathBuf,
}

impl StoreFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let root = fixture.root.path().join("cache-root");
        Self { fixture, root }
    }

    fn open(&self) -> IndexCache {
        self.open_expecting(&ExpectedVersions::current())
    }

    fn open_expecting(&self, expected: &ExpectedVersions) -> IndexCache {
        IndexCache::open_or_create(&self.root, expected, IDENTITY, &Cancellation::default())
            .expect("the cache opens")
    }

    fn database(&self) -> PathBuf {
        self.root.join(super::INDEX_DATABASE_FILE)
    }
}

fn worktree(name: &str) -> WorktreeKey {
    WorktreeKey::for_root(Path::new("/workspaces").join(name).as_path())
}

/// One eligible source entry whose recorded size matches the bytes it names.
fn entry(path: &str, bytes: &[u8]) -> InventoryEntry {
    InventoryEntry {
        path: RepoPath::from_bytes(path.as_bytes().to_vec()),
        byte_size: bytes.len() as u64,
        mtime_ns: Some(1_700_000_000_000_000_000),
        class: FileClass::Source,
        symlink: false,
        boundary: None,
        unreadable: false,
    }
}

/// One entry the walk saw and refused to read.
fn binary_entry(path: &str, size: u64) -> InventoryEntry {
    InventoryEntry {
        path: RepoPath::from_bytes(path.as_bytes().to_vec()),
        byte_size: size,
        mtime_ns: None,
        class: FileClass::Binary,
        symlink: false,
        boundary: None,
        unreadable: false,
    }
}

fn derive(entry: &InventoryEntry, bytes: &[u8]) -> (FileVersion, ChunkSet) {
    let cancellation = Cancellation::default();
    let version = FileVersion::new(
        entry,
        SnapshotId::new(),
        Arc::from(bytes.to_vec().into_boxed_slice()),
        &cancellation,
    )
    .expect("the entry is eligible and its size matches");
    let chunks = chunk_file(&version, None, &cancellation).expect("the file chunks");
    (version, chunks)
}

fn derive_language(
    entry: &InventoryEntry,
    bytes: &[u8],
    language: &str,
) -> (FileVersion, ChunkSet) {
    let cancellation = Cancellation::default();
    let version = FileVersion::new(
        entry,
        SnapshotId::new(),
        Arc::from(bytes.to_vec().into_boxed_slice()),
        &cancellation,
    )
    .expect("the entry is eligible and its size matches")
    .with_language(crate::Language::new(language).expect("the language is valid"));
    let chunks = chunk_file(&version, None, &cancellation).expect("the file chunks");
    (version, chunks)
}

/// Writes one committed full batch holding `files`, and returns the receipt.
fn commit_full(
    cache: &IndexCache,
    key: &WorktreeKey,
    root: &str,
    files: &[(&str, &[u8])],
) -> super::BatchReceipt {
    let cancellation = Cancellation::default();
    let mut batch = cache
        .begin(key, Path::new(root), BatchScope::Full, &cancellation)
        .expect("the batch opens");
    for (path, bytes) in files {
        let entry = entry(path, bytes);
        let (version, chunks) = derive(&entry, bytes);
        batch
            .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
            .expect("the file records");
    }
    batch.commit(&cancellation).expect("the batch commits")
}

/// Enough files that the batch flushes at least once before it is asked to
/// commit, so the "written and still invisible" window really exists.
const FLUSHED_FILES: u64 = 300;

/// Records `range` distinct one-line files into an open batch.
fn record_many(batch: &mut super::IndexBatch<'_>, range: std::ops::Range<u64>) {
    for index in range {
        let path = format!("src/file{index}.rs");
        let bytes = format!("fn file{index}() {{}}\n").into_bytes();
        let entry = entry(&path, &bytes);
        let (version, chunks) = derive(&entry, &bytes);
        batch
            .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
            .expect("the file records");
    }
}

fn count(database: &Path, table: &str) -> u64 {
    let connection = Connection::open(database).expect("the cache opens for counting");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|value| u64::try_from(value).unwrap_or(0))
        .expect("the table is countable")
}

// -- what a batch publishes -------------------------------------------------

/// The metadata row records every version this build compares against. A key
/// that is not stored is a skew that can never be detected.
#[test]
fn a_fresh_cache_records_all_five_versions_and_its_repository() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();

    let meta = cache.meta();
    let expected = ExpectedVersions::current();
    assert_eq!(meta.schema_version, expected.schema_version);
    assert_eq!(meta.parser_version, expected.parser_version);
    assert_eq!(meta.chunking_version, expected.chunking_version);
    assert_eq!(meta.ranking_version, expected.ranking_version);
    assert_eq!(meta.classify_version, expected.classify_version);
    assert_eq!(meta.repository_identity, IDENTITY);
    assert!(meta.index_generation > 0);
    assert!(meta.last_opened_at >= meta.created_at);
}

/// A batch is invisible until it commits, and then it is visible whole. Nothing
/// between those two states is a state a reader may observe.
///
/// Large enough to flush, deliberately: a batch that fits in the buffer never
/// touches the file, so the interesting case — rows written and still
/// unreachable — only exists past the flush threshold.
#[test]
fn rows_are_invisible_until_the_batch_commits() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    record_many(&mut batch, 0..FLUSHED_FILES);

    assert!(
        count(&fixture.database(), "pending_files") > 0,
        "at least one flush wrote rows to the file"
    );
    assert_eq!(
        count(&fixture.database(), "files"),
        0,
        "and none of them reached the table readers address"
    );
    assert!(
        cache.files(&key, 10_000).unwrap().is_empty(),
        "so no query returns any of them"
    );
    assert_eq!(cache.worktree_generation(&key).unwrap(), 0);

    let receipt = batch.commit(&cancellation).unwrap();

    assert_eq!(receipt.files_recorded, FLUSHED_FILES);
    assert_eq!(
        cache.files(&key, 10_000).unwrap().len(),
        usize::try_from(FLUSHED_FILES).unwrap()
    );
    assert_eq!(cache.worktree_generation(&key).unwrap(), receipt.generation);
}

/// A dropped batch is a killed process by another name: the rows it flushed stay
/// invisible, the watermark never moved, and the next batch clears them.
#[test]
fn an_abandoned_batch_leaves_nothing_visible_and_the_next_one_clears_it() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    let watermark = cache.worktree_generation(&key).unwrap();

    let mut abandoned = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    record_many(&mut abandoned, 0..FLUSHED_FILES);
    let orphans = count(&fixture.database(), "pending_files");
    drop(abandoned);

    assert!(orphans > 0, "the abandoned batch reached the file");
    assert_eq!(
        count(&fixture.database(), "files"),
        1,
        "and never touched the table readers address"
    );
    assert_eq!(cache.worktree_generation(&key).unwrap(), watermark);
    let visible = cache.files(&key, 10_000).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].path.display(), "kept.rs");

    // The staged rows sit there until something sweeps them, and the next
    // commit is what sweeps them — not by name, but because a batch below its
    // generation can no longer publish.
    assert_eq!(count(&fixture.database(), "pending_files"), orphans);
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    assert_eq!(count(&fixture.database(), "files"), 1);
    assert_eq!(count(&fixture.database(), "pending_files"), 0);
}

/// The failure the staging table exists for: a batch that re-records a path
/// must not make the committed row invisible while it runs, and abandoning it
/// must leave that row exactly where it was. An in-place upsert tagged the live
/// row with an uncommitted generation, which took it out of every query for the
/// length of the batch and let the next `begin` delete it outright.
#[test]
fn a_batch_that_re_records_a_path_never_hides_the_committed_row() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let path = RepoPath::from_bytes(b"kept.rs".to_vec());
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    let before = cache.file(&key, &path).unwrap();
    assert!(before.is_some());

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    let changed = entry("kept.rs", b"fn kept() { changed(); }\n");
    let (version, chunks) = derive(&changed, b"fn kept() { changed(); }\n");
    batch
        .record_chunked(&changed, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    // Past the flush threshold, so the staged row really is on disk.
    record_many(&mut batch, 0..FLUSHED_FILES);

    assert_eq!(
        cache.file(&key, &path).unwrap(),
        before,
        "the committed row answers unchanged while the batch is in flight"
    );

    drop(batch);

    assert_eq!(
        cache.file(&key, &path).unwrap(),
        before,
        "and an abandoned batch leaves it exactly as it was"
    );
    let next = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    next.commit(&cancellation).unwrap();
    assert_eq!(
        cache.file(&key, &path).unwrap(),
        before,
        "nor does the batch after it, which is where the file was lost for good"
    );
}

/// Two batches on one worktree is what two front ends indexing one repository
/// looks like. Neither may erase the other's staged rows, and the loser is
/// refused rather than allowed to drag the watermark backwards — a commit that
/// hid every row the winner published would be a success that shrank the index.
#[test]
fn a_batch_that_lost_the_race_is_refused_rather_than_moving_the_watermark_back() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let root = Path::new("/workspaces/alpha");

    let mut first = cache
        .begin(&key, root, BatchScope::Full, &cancellation)
        .unwrap();
    record_many(&mut first, 0..FLUSHED_FILES);
    let staged = count(&fixture.database(), "pending_files");
    let mut second = cache
        .begin(&key, root, BatchScope::Full, &cancellation)
        .unwrap();
    assert!(second.generation() > first.generation());
    assert_eq!(
        count(&fixture.database(), "pending_files"),
        staged,
        "opening a second batch leaves the first one's staged rows alone"
    );
    record_many(&mut second, 0..FLUSHED_FILES);

    let winner = second.commit(&cancellation).unwrap();
    let published = cache.files(&key, 10_000).unwrap().len();
    assert_eq!(published, usize::try_from(FLUSHED_FILES).unwrap());

    let error = first.commit(&cancellation).unwrap_err();

    assert_eq!(error.kind(), "index_batch_superseded");
    assert_eq!(
        cache.worktree_generation(&key).unwrap(),
        winner.generation,
        "the watermark only ever moves forward"
    );
    assert_eq!(
        cache.files(&key, 10_000).unwrap().len(),
        published,
        "and every row the winner published is still readable"
    );
}

/// Symbols may be attached before the file version they belong to is recorded.
/// A flush boundary falling between the two calls is not something a caller can
/// see coming, so the store carries them rather than failing on a foreign key.
#[test]
fn symbols_recorded_before_their_file_version_are_carried_to_it() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"fn late() {}\n";
    let entry = entry("src/late.rs", bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let symbol = SymbolRecord {
        id: SymbolId::derive(&entry.path, "rust", "late", "function"),
        name: "late".to_owned(),
        qualified_path: "late".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 12),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    // Symbols first, then enough files to force a flush before the version they
    // name has been recorded at all.
    batch
        .record_symbols(version.id(), "1", std::slice::from_ref(&symbol))
        .unwrap();
    record_many(&mut batch, 0..FLUSHED_FILES);
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch.commit(&cancellation).unwrap();

    let found = cache.symbols_named(&key, "late", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, symbol.id);
}

/// A file version that never arrives is a caller mistake, and it is spelled as
/// one rather than as a broken cache — a front end must not tell a user their
/// index is corrupt when the code above it built the batch wrong.
#[test]
fn symbols_whose_file_version_never_arrives_are_refused_by_name() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let path = RepoPath::from_bytes(b"src/absent.rs".to_vec());
    let orphan = FileVersionId::derive(&path, b"nothing records this\n");

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_symbols(
            &orphan,
            "1",
            &[SymbolRecord {
                id: SymbolId::derive(&path, "rust", "absent", "function"),
                name: "absent".to_owned(),
                qualified_path: "absent".to_owned(),
                kind: "function".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 1),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }],
        )
        .unwrap();

    let error = batch.commit(&cancellation).unwrap_err();

    assert_eq!(error.kind(), "index_batch_invalid");
    assert_eq!(cache.worktree_generation(&key).unwrap(), 0);
}

/// A file that changed under the walk keeps whatever the last successful pass
/// derived. Clearing it would unlink the file from its chunks and the commit's
/// collection would delete them — one unreadable moment costing the file its
/// whole entry in the index.
#[test]
fn a_file_that_became_unreadable_keeps_the_derivation_it_had() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let path = RepoPath::from_bytes(b"a.rs".to_vec());
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    let indexed = cache.file(&key, &path).unwrap().unwrap();
    let chunks = cache.chunks(&key, &path).unwrap();
    assert!(!chunks.is_empty());

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_unreadable(&entry("a.rs", b"fn a() {}\n"), CLASSIFY_VERSION)
        .unwrap();
    batch.commit(&cancellation).unwrap();

    let after = cache.file(&key, &path).unwrap().unwrap();
    assert_eq!(after.file_version, indexed.file_version);
    assert!(after.unreadable, "the row says the walk could not read it");
    assert_eq!(
        cache.chunks(&key, &path).unwrap(),
        chunks,
        "and its chunks are still there to retrieve"
    );
}

/// A dead batch's generation is never handed out again. If it were, a targeted
/// batch — which sweeps nothing — would publish an abandoned batch's rows as
/// part of its own commit.
#[test]
fn a_generation_is_allocated_once_and_never_reissued() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let root = Path::new("/workspaces/alpha");

    let first = cache
        .begin(&key, root, BatchScope::Full, &cancellation)
        .unwrap()
        .generation();
    let second = cache
        .begin(&key, root, BatchScope::Full, &cancellation)
        .unwrap()
        .generation();
    let third = cache
        .begin(&key, root, BatchScope::Full, &cancellation)
        .unwrap()
        .generation();

    assert!(first < second && second < third);
}

/// A full batch is the whole worktree, so a path it did not present is a path
/// that is gone.
#[test]
fn a_full_batch_sweeps_what_it_did_not_confirm() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n")],
    );

    let receipt = commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );

    assert_eq!(receipt.rows_swept, 1);
    let visible = cache.files(&key, 100).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].path.display(), "a.rs");
    // The swept file's derived rows went with it, because nothing else held them.
    assert_eq!(count(&fixture.database(), "file_versions"), 1);
    assert_eq!(count(&fixture.database(), "contents"), 1);
}

/// The acceptance criterion a single-file update exists for: one file's rows
/// change and no other file's row is written, read back, or deleted.
#[test]
fn a_targeted_batch_touches_only_the_file_it_names() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[
            ("a.rs", b"fn a() {}\n"),
            ("b.rs", b"fn b() {}\n"),
            ("c.rs", b"fn c() {}\n"),
        ],
    );
    let untouched = cache
        .file(&key, &RepoPath::from_bytes(b"b.rs".to_vec()))
        .unwrap()
        .unwrap();

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    let changed = entry("a.rs", b"fn a() { changed(); }\n");
    let (version, chunks) = derive(&changed, b"fn a() { changed(); }\n");
    batch
        .record_chunked(&changed, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    let receipt = batch.commit(&cancellation).unwrap();

    assert_eq!(receipt.files_recorded, 1);
    assert_eq!(receipt.rows_swept, 0, "a targeted batch sweeps nothing");
    assert_eq!(cache.files(&key, 100).unwrap().len(), 3);
    let after = cache
        .file(&key, &RepoPath::from_bytes(b"b.rs".to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(
        after, untouched,
        "an unrelated file's row must be byte-for-byte what it was, generation included"
    );
    // The displaced version of `a.rs` was collected, and only it.
    assert_eq!(count(&fixture.database(), "file_versions"), 3);
}

/// A path removed from a worktree stops being visible, and its content goes
/// with it when nothing else holds it.
#[test]
fn a_targeted_removal_drops_the_row_and_collects_its_content() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n")],
    );

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    batch
        .remove(&RepoPath::from_bytes(b"a.rs".to_vec()))
        .unwrap();
    let receipt = batch.commit(&cancellation).unwrap();

    assert_eq!(receipt.files_removed, 1);
    assert_eq!(cache.files(&key, 100).unwrap().len(), 1);
    assert_eq!(count(&fixture.database(), "file_versions"), 1);
    assert_eq!(count(&fixture.database(), "contents"), 1);
}

/// A path the walk saw and did not read is still a path the index knows about.
/// Dropping it would make the next full batch sweep it and the walk after that
/// find it again.
#[test]
fn an_ineligible_entry_is_recorded_without_content() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_entry(&binary_entry("assets/logo.png", 4096), CLASSIFY_VERSION)
        .unwrap();
    batch.commit(&cancellation).unwrap();

    let row = cache
        .file(&key, &RepoPath::from_bytes(b"assets/logo.png".to_vec()))
        .unwrap()
        .expect("the entry is recorded");
    assert_eq!(row.class, FileClass::Binary);
    assert!(row.file_version.is_none());
    assert!(
        !row.eligible(),
        "eligibility is derived from what is stored"
    );
    assert_eq!(count(&fixture.database(), "contents"), 0);
}

/// The store keeps the bytes Git reported. A path that is not valid UTF-8 has to
/// survive a write and a read unchanged, or the file it names cannot be opened.
#[test]
fn a_path_that_is_not_utf8_round_trips_exactly() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let raw = b"src/\xff\xfe.rs".to_vec();

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_entry(
            &InventoryEntry {
                path: RepoPath::from_bytes(raw.clone()),
                byte_size: 1,
                mtime_ns: None,
                class: FileClass::UnknownText,
                symlink: false,
                boundary: None,
                unreadable: false,
            },
            CLASSIFY_VERSION,
        )
        .unwrap();
    batch.commit(&cancellation).unwrap();

    let row = cache
        .file(&key, &RepoPath::from_bytes(raw.clone()))
        .unwrap()
        .expect("the entry is addressable by its exact bytes");
    assert_eq!(row.path.as_bytes(), raw.as_slice());
}

/// Chunk rows come back as the records that produced them, in order.
#[test]
fn chunks_are_readable_through_the_worktree_that_holds_the_file() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let bytes =
        b"# Title\n\nBody text that is long enough to be its own section.\n\n## Second\n\nMore.\n";
    commit_full(&cache, &key, "/workspaces/alpha", &[("README.md", bytes)]);

    let path = RepoPath::from_bytes(b"README.md".to_vec());
    let stored = cache.chunks(&key, &path).unwrap();
    let (_, produced) = derive(&entry("README.md", bytes), bytes);

    assert_eq!(stored.len(), produced.chunks.len());
    assert!(!stored.is_empty());
    for (row, record) in stored.iter().zip(&produced.chunks) {
        assert_eq!(row.id, record.id);
        assert_eq!(row.anchor, record.anchor);
        assert_eq!(row.ordinal, record.ordinal);
        assert_eq!(row.byte_range, record.byte_range);
        assert_eq!(row.chunk_sha256, record.chunk_sha256);
        assert_eq!(row.path, record_path(record.id.clone(), &stored));
    }
    // Reachable by identity as well as by path, and only through this worktree.
    let by_id = cache.chunk(&key, &stored[0].id).unwrap().unwrap();
    assert_eq!(by_id, stored[0]);
    assert!(
        cache
            .chunk(&worktree("beta"), &stored[0].id)
            .unwrap()
            .is_none()
    );
}

fn record_path(id: crate::ids::ChunkId, rows: &[super::IndexedChunk]) -> RepoPath {
    rows.iter()
        .find(|row| row.id == id)
        .map(|row| row.path.clone())
        .expect("the row is one of the rows it came from")
}

fn commit_symbol_rows(
    cache: &IndexCache,
    key: &WorktreeKey,
    root: &str,
    path: &str,
    bytes: &[u8],
    symbols: &[SymbolRecord],
) -> FileVersionId {
    let cancellation = Cancellation::default();
    let entry = entry(path, bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let id = version.id().clone();
    let mut batch = cache
        .begin(key, Path::new(root), BatchScope::Full, &cancellation)
        .unwrap();
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch.record_symbols(&id, "rust-1", symbols).unwrap();
    batch.commit(&cancellation).unwrap();
    id
}

/// Symbols have no producer in this build, so the store's own acceptance of
/// them is what [#117] plugs into. A table nothing can write is a table the next
/// issue redesigns.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
#[test]
fn symbol_rows_are_stored_and_looked_up_by_name() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"fn interesting() {}\n";
    let entry = entry("src/lib.rs", bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let symbol = SymbolRecord {
        id: SymbolId::derive(&entry.path, "rust", "interesting", "function"),
        name: "interesting".to_owned(),
        qualified_path: "interesting".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 19).with_lines(1, 1),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(version.id(), "test-parser-1", std::slice::from_ref(&symbol))
        .unwrap();
    let receipt = batch.commit(&cancellation).unwrap();

    assert_eq!(receipt.symbols_recorded, 1);
    let found = cache.symbols_named(&key, "interesting", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, symbol.id);
    assert_eq!(found[0].kind, SymbolKind::Function);
    assert_eq!(found[0].parser_version, "test-parser-1");
    assert!(cache.symbols_named(&key, "absent", 10).unwrap().is_empty());
    assert!(
        cache
            .symbols_named(&worktree("beta"), "interesting", 10)
            .unwrap()
            .is_empty(),
        "a symbol is reachable only through a worktree holding the file it is in"
    );
}

#[test]
fn symbol_ids_are_rederived_and_tampering_is_refused() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let path = RepoPath::from_bytes(b"src/lib.rs".to_vec());
    let symbol = SymbolRecord {
        id: SymbolId::derive(&path, "rust", "interesting", "function"),
        name: "interesting".to_owned(),
        qualified_path: "interesting".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 19),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };
    commit_symbol_rows(
        &cache,
        &key,
        "/workspaces/alpha",
        "src/lib.rs",
        b"fn interesting() {}\n",
        std::slice::from_ref(&symbol),
    );

    let forged = SymbolId::derive(&path, "rust", "different", "function");
    Connection::open(fixture.database())
        .unwrap()
        .execute(
            "UPDATE symbols SET symbol_id = ?1 WHERE symbol_id = ?2",
            [forged.to_string(), symbol.id.to_string()],
        )
        .unwrap();

    assert!(cache.symbols_named(&key, "interesting", 10).is_err());
}

#[test]
fn symbol_parent_association_is_revalidated_on_read() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let path = RepoPath::from_bytes(b"src/lib.rs".to_vec());
    let parent_id = SymbolId::derive(&path, "rust", "outer", "function");
    let child_id = SymbolId::derive(&path, "rust", "outer::inner", "function");
    let symbols = [
        SymbolRecord {
            id: parent_id.clone(),
            name: "outer".to_owned(),
            qualified_path: "outer".to_owned(),
            kind: "function".to_owned(),
            ordinal: 0,
            byte_range: ByteRange::new(0, 33),
            parent: None,
            is_test: false,
            name_is_lossy: false,
        },
        SymbolRecord {
            id: child_id,
            name: "inner".to_owned(),
            qualified_path: "outer::inner".to_owned(),
            kind: "function".to_owned(),
            ordinal: 0,
            byte_range: ByteRange::new(17, 30),
            parent: Some(parent_id.clone()),
            is_test: false,
            name_is_lossy: false,
        },
    ];
    commit_symbol_rows(
        &cache,
        &key,
        "/workspaces/alpha",
        "src/lib.rs",
        b"fn outer() {\n    fn inner() {}\n}\n",
        &symbols,
    );

    Connection::open(fixture.database())
        .unwrap()
        .execute(
            "UPDATE symbols SET start_byte = 18 WHERE symbol_id = ?1",
            [parent_id.to_string()],
        )
        .unwrap();

    assert!(cache.symbols_named(&key, "inner", 10).is_err());
}

#[test]
fn stored_ranges_are_bounded_and_schema_checks_reject_impossible_integers() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let path = RepoPath::from_bytes(b"src/lib.rs".to_vec());
    let symbol = SymbolRecord {
        id: SymbolId::derive(&path, "rust", "item", "function"),
        name: "item".to_owned(),
        qualified_path: "item".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 12),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };
    let version = commit_symbol_rows(
        &cache,
        &key,
        "/workspaces/alpha",
        "src/lib.rs",
        b"fn item() {}\n",
        std::slice::from_ref(&symbol),
    );
    let connection = Connection::open(fixture.database()).unwrap();

    assert!(
        connection
            .execute("UPDATE symbols SET start_byte = -1", [])
            .is_err()
    );
    assert!(
        connection
            .execute("UPDATE symbols SET start_byte = 12, end_byte = 1", [])
            .is_err()
    );
    connection
        .execute("UPDATE symbols SET start_byte = 0, end_byte = 100", [])
        .unwrap();
    assert!(cache.symbols_named(&key, "item", 10).is_err());
    connection
        .execute(
            "INSERT INTO symbol_references \
             (file_version_id, ordinal, name, start_byte, end_byte, start_line, end_line, name_is_lossy) \
             VALUES (?1, 0, 'item', 0, 100, NULL, NULL, 0)",
            [version.to_string()],
        )
        .unwrap();
    assert!(cache.symbol_references_in_file(&key, &path, 10).is_err());

    connection
        .execute(
            "UPDATE parse_health SET status = 'partial', reason = NULL, \
             error_ranges_json = '[{\"start\":0,\"end\":100,\"first_line\":null,\"last_line\":null}]' \
             WHERE file_version_id = ?1",
            [version.to_string()],
        )
        .unwrap();
    assert!(cache.parse_health(&key, &path).is_err());
}

#[test]
fn symbol_buffer_flushes_by_rows_before_commit_without_publishing() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"x";
    let entry = entry("src/lib.rs", bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let symbols = (0..600)
        .map(|ordinal| {
            let name = format!("item_{ordinal}");
            SymbolRecord {
                id: SymbolId::derive(&entry.path, "rust", &name, "function"),
                name: name.clone(),
                qualified_path: name,
                kind: "function".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 1),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }
        })
        .collect::<Vec<_>>();
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(version.id(), "rust-1", &symbols)
        .unwrap();

    assert_eq!(count(&fixture.database(), "symbols"), 600);
    assert!(cache.symbols_named(&key, "item_0", 10).unwrap().is_empty());
}

#[test]
fn symbol_buffer_flushes_by_dynamic_bytes_before_commit() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"x";
    let entry = entry("src/lib.rs", bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let symbols = (0..64)
        .map(|ordinal| {
            let name = format!("{}_{ordinal}", "x".repeat(36_000));
            SymbolRecord {
                id: SymbolId::derive(&entry.path, "rust", &name, "function"),
                name: name.clone(),
                qualified_path: name,
                kind: "function".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 1),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }
        })
        .collect::<Vec<_>>();
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(version.id(), "rust-1", &symbols)
        .unwrap();

    assert_eq!(count(&fixture.database(), "symbols"), 64);
    assert!(
        cache
            .symbols_named(&key, &symbols[0].name, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn health_only_rows_round_trip_without_a_language_and_keep_transcoded_reason() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"plain text\n";
    let notes_entry = entry("notes", bytes);
    let (version, chunks) = derive(&notes_entry, bytes);
    let extraction = FileSymbols::skipped(
        LanguageDetection {
            language: None,
            source: None,
        },
        "",
        ExtractionSkipReason::UnknownLanguage,
    );
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&notes_entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch.record_extraction(version.id(), &extraction).unwrap();
    batch.commit(&cancellation).unwrap();
    assert!(matches!(
        cache
            .parse_health(&key, &notes_entry.path)
            .unwrap()
            .unwrap()
            .health,
        ParseHealth::Skipped {
            reason: ExtractionSkipReason::UnknownLanguage
        }
    ));

    let rust_entry = entry("src/lib.rs", b"fn item() {}\n");
    let (rust_version, rust_chunks) = derive_language(&rust_entry, b"fn item() {}\n", "rust");
    let transcoded = FileSymbols::skipped(
        LanguageDetection {
            language: crate::Language::new("rust").ok(),
            source: Some(LanguageDetectionSource::Extension),
        },
        "rust-1",
        ExtractionSkipReason::TranscodedInput,
    );
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&rust_entry, &rust_version, &rust_chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_extraction(rust_version.id(), &transcoded)
        .unwrap();
    batch.commit(&cancellation).unwrap();
    assert!(matches!(
        cache
            .parse_health(&key, &rust_entry.path)
            .unwrap()
            .unwrap()
            .health,
        ParseHealth::Skipped {
            reason: ExtractionSkipReason::TranscodedInput
        }
    ));
}

#[test]
fn incomplete_symbol_files_counts_only_visible_budget_failures_in_one_worktree() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let alpha = worktree("alpha");
    let beta = worktree("beta");
    let alpha_path = RepoPath::from_bytes(b"src/alpha.rs".to_vec());
    let beta_path = RepoPath::from_bytes(b"src/beta.rs".to_vec());
    let alpha_symbol = SymbolRecord {
        id: SymbolId::derive(&alpha_path, "rust", "alpha", "function"),
        name: "alpha".to_owned(),
        qualified_path: "alpha".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 13),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };
    let beta_symbol = SymbolRecord {
        id: SymbolId::derive(&beta_path, "rust", "beta", "function"),
        name: "beta".to_owned(),
        qualified_path: "beta".to_owned(),
        kind: "function".to_owned(),
        ordinal: 0,
        byte_range: ByteRange::new(0, 12),
        parent: None,
        is_test: false,
        name_is_lossy: false,
    };
    let alpha_version = commit_symbol_rows(
        &cache,
        &alpha,
        "/workspaces/alpha",
        "src/alpha.rs",
        b"fn alpha() {}\n",
        &[alpha_symbol],
    );
    let beta_version = commit_symbol_rows(
        &cache,
        &beta,
        "/workspaces/beta",
        "src/beta.rs",
        b"fn beta() {}\n",
        &[beta_symbol],
    );
    let connection = Connection::open(fixture.database()).unwrap();
    connection
        .execute(
            "UPDATE parse_health SET status = 'failed', reason = 'symbol_budget_exhausted', \
             error_ranges_json = '[]' WHERE file_version_id = ?1",
            [alpha_version.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE parse_health SET status = 'failed', reason = 'parser_failed', \
             error_ranges_json = '[]' WHERE file_version_id = ?1",
            [beta_version.to_string()],
        )
        .unwrap();

    assert_eq!(cache.incomplete_symbol_files(&alpha).unwrap(), 1);
    assert_eq!(cache.incomplete_symbol_files(&beta).unwrap(), 0);
    connection
        .execute(
            "UPDATE parse_health SET reason = 'reference_budget_exhausted' \
             WHERE file_version_id = ?1",
            [beta_version.to_string()],
        )
        .unwrap();
    assert_eq!(cache.incomplete_symbol_files(&beta).unwrap(), 1);
}

#[test]
fn a_grammar_bump_invalidates_only_that_languages_rows() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    cache
        .refresh_grammar_versions(&[
            ("rust".to_owned(), "rust-1".to_owned()),
            ("toml".to_owned(), "toml-1".to_owned()),
        ])
        .unwrap();

    let rust_entry = entry("src/lib.rs", b"fn rust_item() {}\n");
    let rust_version = FileVersion::new(
        &rust_entry,
        SnapshotId::new(),
        Arc::from(&b"fn rust_item() {}\n"[..]),
        &cancellation,
    )
    .unwrap()
    .with_language(crate::Language::new("rust").unwrap());
    let rust_chunks = chunk_file(&rust_version, None, &cancellation).unwrap();
    let toml_entry = InventoryEntry {
        class: FileClass::Configuration,
        ..entry("Cargo.toml", b"[package]\nname = \"demo\"\n")
    };
    let toml_version = FileVersion::new(
        &toml_entry,
        SnapshotId::new(),
        Arc::from(&b"[package]\nname = \"demo\"\n"[..]),
        &cancellation,
    )
    .unwrap()
    .with_language(crate::Language::new("toml").unwrap());
    let toml_chunks = chunk_file(&toml_version, None, &cancellation).unwrap();

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&rust_entry, &rust_version, &rust_chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(
            rust_version.id(),
            "rust-1",
            &[SymbolRecord {
                id: SymbolId::derive(&rust_entry.path, "rust", "rust_item", "function"),
                name: "rust_item".to_owned(),
                qualified_path: "rust_item".to_owned(),
                kind: "function".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 17),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }],
        )
        .unwrap();
    batch
        .record_chunked(&toml_entry, &toml_version, &toml_chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(
            toml_version.id(),
            "toml-1",
            &[SymbolRecord {
                id: SymbolId::derive(&toml_entry.path, "toml", "package", "module"),
                name: "package".to_owned(),
                qualified_path: "package".to_owned(),
                kind: "module".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 9),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }],
        )
        .unwrap();
    batch.commit(&cancellation).unwrap();

    let toml_before = cache.symbols_named(&key, "package", 10).unwrap();
    assert_eq!(cache.status().counts.unwrap().symbols, 2);
    let invalidated = cache
        .refresh_grammar_versions(&[
            ("rust".to_owned(), "rust-2".to_owned()),
            ("toml".to_owned(), "toml-1".to_owned()),
        ])
        .unwrap();

    assert!(invalidated > 0);
    assert!(
        cache
            .symbols_named(&key, "rust_item", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cache.symbols_named(&key, "package", 10).unwrap(),
        toml_before
    );
    assert_eq!(cache.status().counts.unwrap().symbols, 1);
    assert_eq!(
        cache
            .file(&key, &rust_entry.path)
            .unwrap()
            .unwrap()
            .parser_version,
        None
    );
    assert_eq!(
        cache
            .file(&key, &toml_entry.path)
            .unwrap()
            .unwrap()
            .parser_version
            .as_deref(),
        Some("toml-1")
    );
}

// -- a process that stops in the middle -------------------------------------

const PROCESS_CHILD_TEST: &str = "index::store_tests::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_CONTEXT_STORE_TEST_ROLE";
const PROCESS_CACHE_ROOT_ENV: &str = "HARKNESS_CONTEXT_STORE_TEST_CACHE_ROOT";
const PROCESS_READY_FILE_ENV: &str = "HARKNESS_CONTEXT_STORE_TEST_READY_FILE";

/// The worktree the crash test's parent and child both address.
fn crash_worktree() -> WorktreeKey {
    worktree("crash")
}

/// Re-entered by the crash test, and killed by it.
///
/// It flushes a batch and then parks *without committing*, which is the state a
/// `SIGKILL` during a cold build leaves behind. Nothing here cleans up,
/// deliberately: the recovery under test is the one that happens when the
/// process that wrote the rows is gone.
#[test]
#[ignore = "only run as a child process by the interrupted-batch test"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let cache_root = harkness_test_fixtures::child_path(PROCESS_CACHE_ROOT_ENV);
    match role.as_str() {
        "flush-then-park" => {
            let cancellation = Cancellation::default();
            let cache = IndexCache::open_or_create(
                &cache_root,
                &ExpectedVersions::current(),
                IDENTITY,
                &cancellation,
            )
            .unwrap();
            let mut batch = cache
                .begin(
                    &crash_worktree(),
                    Path::new("/workspaces/crash"),
                    BatchScope::Full,
                    &cancellation,
                )
                .unwrap();
            record_many(&mut batch, 0..FLUSHED_FILES);
            harkness_test_fixtures::signal_ready(PROCESS_READY_FILE_ENV);
            harkness_test_fixtures::park();
        }
        _ => panic!("unknown test child role: {role}"),
    }
}

/// A process killed mid-batch leaves the cache openable, the watermark where it
/// was, and no partial row visible. This is the guarantee the pending
/// generation exists for, and the only way to test it honestly is to kill a
/// real process — an in-process drop unwinds, and an unwind is not a crash.
#[test]
fn a_process_killed_mid_batch_leaves_the_previous_generation_answering() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = crash_worktree();
    commit_full(
        &cache,
        &key,
        "/workspaces/crash",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    let watermark = cache.worktree_generation(&key).unwrap();
    drop(cache);

    let ready = fixture.fixture.root.path().join("child-ready");
    let mut child = harkness_test_fixtures::spawn_child(
        PROCESS_CHILD_TEST,
        PROCESS_ROLE_ENV,
        "flush-then-park",
        PROCESS_CACHE_ROOT_ENV,
        &fixture.root,
    )
    .env(PROCESS_READY_FILE_ENV, &ready)
    .spawn()
    .unwrap();
    harkness_test_fixtures::wait_for_child_signal(&mut child, &ready);
    child.kill().unwrap();
    child.wait().unwrap();

    let cache = fixture.open();
    assert!(
        cache.status().last_recreation.is_none(),
        "an interrupted batch is not a corrupt cache"
    );
    assert_eq!(cache.worktree_generation(&key).unwrap(), watermark);
    let visible = cache.files(&key, 10_000).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].path.display(), "kept.rs");
    assert!(
        count(&fixture.database(), "pending_files") > 0,
        "the killed batch's rows are on disk, staged where no query returns them"
    );
    assert_eq!(
        count(&fixture.database(), "files"),
        1,
        "and the table readers address holds only what was committed"
    );

    // Redone, not resumed: the next full batch sweeps everything it did not
    // confirm, which is exactly the abandoned work.
    commit_full(
        &cache,
        &key,
        "/workspaces/crash",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    assert_eq!(count(&fixture.database(), "files"), 1);
    assert_eq!(count(&fixture.database(), "pending_files"), 0);
}

/// Sustained contention on the write lock is `index_busy` and never a
/// quarantine, so a caller can tell "come back later" from "this is broken" and
/// fall back to reading the workspace live.
#[test]
fn a_batch_that_cannot_take_the_write_lock_reports_the_cache_busy() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );

    let blocker = Connection::open(fixture.database()).unwrap();
    blocker
        .busy_timeout(std::time::Duration::from_millis(1))
        .unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let error = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &Cancellation::default(),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "index_busy");
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);

    // Contention clears, and the fallback was a delay rather than a loss.
    assert_eq!(cache.files(&key, 100).unwrap().len(), 1);
    assert!(
        fixture
            .root
            .join("index.db.corrupt-0")
            .symlink_metadata()
            .is_err()
    );
}

// -- worktree isolation and deduplication -----------------------------------

/// The isolation contract [#115] builds on, and the deduplication that pays for
/// keying the cache by repository: two worktrees see only their own files and
/// share every content-addressed row for the files they agree about.
///
/// [#115]: https://github.com/fullstacktaiye/harkness/issues/115
#[test]
fn two_worktrees_share_content_and_never_see_each_others_files() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let alpha = worktree("alpha");
    let beta = worktree("beta");
    let shared: &[u8] = b"fn shared() {}\n";

    commit_full(
        &cache,
        &alpha,
        "/workspaces/alpha",
        &[("shared.rs", shared), ("only-alpha.rs", b"fn alpha() {}\n")],
    );
    commit_full(
        &cache,
        &beta,
        "/workspaces/beta",
        &[("shared.rs", shared), ("only-beta.rs", b"fn beta() {}\n")],
    );

    let alpha_paths = cache
        .files(&alpha, 100)
        .unwrap()
        .iter()
        .map(|row| row.path.display())
        .collect::<Vec<_>>();
    let beta_paths = cache
        .files(&beta, 100)
        .unwrap()
        .iter()
        .map(|row| row.path.display())
        .collect::<Vec<_>>();
    assert_eq!(alpha_paths, ["only-alpha.rs", "shared.rs"]);
    assert_eq!(beta_paths, ["only-beta.rs", "shared.rs"]);

    // Four file rows over three file versions and three contents: the file both
    // worktrees hold at one path with one content is stored once.
    assert_eq!(count(&fixture.database(), "files"), 4);
    assert_eq!(count(&fixture.database(), "file_versions"), 3);
    assert_eq!(count(&fixture.database(), "contents"), 3);

    let alpha_chunks = cache
        .chunks(&alpha, &RepoPath::from_bytes(b"shared.rs".to_vec()))
        .unwrap();
    let beta_chunks = cache
        .chunks(&beta, &RepoPath::from_bytes(b"shared.rs".to_vec()))
        .unwrap();
    assert_eq!(alpha_chunks, beta_chunks);
    assert_eq!(
        count(&fixture.database(), "chunks"),
        u64::try_from(alpha_chunks.len()).unwrap() + 2,
        "the shared file's chunks are stored once, beside one chunk per unique file"
    );
}

/// One worktree sweeping its own rows must not take the other's shared content
/// with it — the row is only collectable once nobody points at it.
#[test]
fn collecting_content_leaves_what_another_worktree_still_holds() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let alpha = worktree("alpha");
    let beta = worktree("beta");
    let shared: &[u8] = b"fn shared() {}\n";
    commit_full(
        &cache,
        &alpha,
        "/workspaces/alpha",
        &[("shared.rs", shared)],
    );
    commit_full(&cache, &beta, "/workspaces/beta", &[("shared.rs", shared)]);

    // Alpha now holds nothing at all.
    let cancellation = Cancellation::default();
    let batch = cache
        .begin(
            &alpha,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch.commit(&cancellation).unwrap();

    assert!(cache.files(&alpha, 100).unwrap().is_empty());
    assert_eq!(cache.files(&beta, 100).unwrap().len(), 1);
    assert_eq!(
        count(&fixture.database(), "contents"),
        1,
        "beta still holds the content, so it is not collectable"
    );
    assert!(
        !cache
            .chunks(&beta, &RepoPath::from_bytes(b"shared.rs".to_vec()))
            .unwrap()
            .is_empty()
    );
}

/// Every read names a worktree, so a worktree with no rows answers empty rather
/// than reaching the content tables directly.
#[test]
fn an_unknown_worktree_reads_empty_rather_than_the_whole_cache() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    commit_full(
        &cache,
        &worktree("alpha"),
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );

    let stranger = worktree("never-indexed");
    assert!(cache.files(&stranger, 100).unwrap().is_empty());
    assert!(
        cache
            .chunks(&stranger, &RepoPath::from_bytes(b"a.rs".to_vec()))
            .unwrap()
            .is_empty()
    );
    assert_eq!(cache.worktree_generation(&stranger).unwrap(), 0);
    assert!(
        count(&fixture.database(), "chunks") > 0,
        "the rows exist; they are simply not this worktree's"
    );
}

// -- the invalidation matrix ------------------------------------------------

/// A chunking upgrade takes the chunks and nothing else: the walk that produced
/// the file rows is still valid, and re-walking a repository because a
/// boundary rule moved would make every retrieval improvement a cold rebuild.
#[test]
fn a_chunking_bump_empties_only_the_chunks() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n")],
    );
    let files_before = count(&fixture.database(), "files");
    let versions_before = count(&fixture.database(), "file_versions");
    assert!(count(&fixture.database(), "chunks") > 0);
    drop(cache);

    let cache = fixture.open_expecting(&ExpectedVersions {
        chunking_version: "99".to_owned(),
        ..ExpectedVersions::current()
    });
    let report = cache.refresh(&Cancellation::default()).unwrap();

    assert_eq!(count(&fixture.database(), "chunks"), 0);
    assert_eq!(count(&fixture.database(), "files"), files_before);
    assert_eq!(count(&fixture.database(), "file_versions"), versions_before);
    assert_eq!(
        report
            .invalidated
            .iter()
            .map(|applied| applied.component)
            .collect::<Vec<_>>(),
        [IndexComponent::Chunking]
    );
    assert!(report.entries_reconciled > 0);
    assert!(
        report.stale_components.is_empty(),
        "acting on the skew is what clears it"
    );
    assert_eq!(cache.meta().chunking_version, "99");
    // The version each file version was chunked under is cleared too, so a
    // reconciler is not told that files with no chunks are already chunked.
    let unchunked: i64 = Connection::open(fixture.database())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM file_versions WHERE chunking_version IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(u64::try_from(unchunked).unwrap(), versions_before);
}

/// A parser upgrade takes the symbols and leaves the chunks, which is the
/// mirror image and the reason the two are versioned apart.
#[test]
fn a_parser_bump_empties_only_the_symbols() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    let bytes = b"fn interesting() {}\n";
    let entry = entry("src/lib.rs", bytes);
    let (version, chunks) = derive_language(&entry, bytes, "rust");
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    batch
        .record_symbols(
            version.id(),
            "1",
            &[SymbolRecord {
                id: SymbolId::derive(&entry.path, "rust", "interesting", "function"),
                name: "interesting".to_owned(),
                qualified_path: "interesting".to_owned(),
                kind: "function".to_owned(),
                ordinal: 0,
                byte_range: ByteRange::new(0, 19),
                parent: None,
                is_test: false,
                name_is_lossy: false,
            }],
        )
        .unwrap();
    batch.commit(&cancellation).unwrap();
    let chunks_before = count(&fixture.database(), "chunks");
    drop(cache);

    let cache = fixture.open_expecting(&ExpectedVersions {
        parser_version: "2".to_owned(),
        ..ExpectedVersions::current()
    });
    let report = cache.refresh(&cancellation).unwrap();

    assert_eq!(count(&fixture.database(), "symbols"), 0);
    assert_eq!(count(&fixture.database(), "chunks"), chunks_before);
    assert_eq!(count(&fixture.database(), "files"), 1);
    assert_eq!(
        report
            .invalidated
            .iter()
            .map(|applied| applied.component)
            .collect::<Vec<_>>(),
        [IndexComponent::Parser]
    );
    assert_eq!(cache.meta().parser_version, "2");
}

/// A classification upgrade deletes nothing. A file row is a true record that a
/// path existed at a size; only its class was decided by the rules that moved,
/// and the row's own version is what says which of them need looking at again.
#[test]
fn a_classify_bump_keeps_every_row_and_marks_them() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n")],
    );
    assert_eq!(
        cache.stale_classifications(&key, CLASSIFY_VERSION).unwrap(),
        0
    );
    drop(cache);

    let cache = fixture.open_expecting(&ExpectedVersions {
        classify_version: "7".to_owned(),
        ..ExpectedVersions::current()
    });
    let skew = cache.status().stale_components;
    assert_eq!(skew.len(), 1);
    assert_eq!(skew[0].component, IndexComponent::Classify);

    let report = cache.refresh(&Cancellation::default()).unwrap();

    assert_eq!(report.entries_reconciled, 0, "nothing is deleted");
    assert_eq!(count(&fixture.database(), "files"), 2);
    assert!(count(&fixture.database(), "chunks") > 0);
    assert_eq!(cache.meta().classify_version, "7");
    assert_eq!(
        cache.stale_classifications(&key, 7).unwrap(),
        2,
        "the rows are kept and marked, which is what a reconciler reads"
    );
}

/// A schema bump has no reconciliation to do: the cache is disposable, so the
/// old layout is quarantined and a new one built. There is no downgrade path
/// and none may be added.
#[test]
fn a_schema_bump_rebuilds_rather_than_invalidating() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    let generation = cache.generation();
    drop(cache);

    let cache = fixture.open_expecting(&ExpectedVersions {
        schema_version: super::INDEX_SCHEMA_VERSION + 1,
        ..ExpectedVersions::current()
    });

    let recreation = cache
        .status()
        .last_recreation
        .expect("the cache was replaced");
    assert_eq!(recreation.reason, RecreationReason::Version);
    assert_eq!(recreation.previous_generation, Some(generation));
    assert!(cache.generation() > generation);
    assert!(cache.files(&key, 100).unwrap().is_empty());
    assert_eq!(count(&fixture.database(), "files"), 0);
}

/// Every stale component is acted on in one refresh, not one per call.
#[test]
fn a_refresh_acts_on_every_skewed_component_at_once() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    commit_full(
        &cache,
        &worktree("alpha"),
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    drop(cache);

    let cache = fixture.open_expecting(&ExpectedVersions {
        parser_version: "5".to_owned(),
        chunking_version: "6".to_owned(),
        ranking_version: "7".to_owned(),
        classify_version: "8".to_owned(),
        ..ExpectedVersions::current()
    });
    assert_eq!(cache.status().stale_components.len(), 4);

    let report = cache.refresh(&Cancellation::default()).unwrap();

    assert_eq!(
        report
            .invalidated
            .iter()
            .map(|applied| applied.component)
            .collect::<Vec<_>>(),
        [
            IndexComponent::Parser,
            IndexComponent::Chunking,
            IndexComponent::Ranking,
            IndexComponent::Classify,
        ]
    );
    assert!(report.stale_components.is_empty());
    assert!(cache.status().stale_components.is_empty());
}

/// Disposal throws away the content tables too. "Reclaim disk" that left the
/// chunks behind would reclaim nothing worth having.
#[test]
fn disposing_empties_the_content_tables_as_well_as_the_metadata() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    assert!(count(&fixture.database(), "chunks") > 0);

    cache.dispose(&Cancellation::default()).unwrap();

    assert_eq!(count(&fixture.database(), "files"), 0);
    assert_eq!(count(&fixture.database(), "chunks"), 0);
    assert_eq!(count(&fixture.database(), "contents"), 0);
    assert_eq!(count(&fixture.database(), "worktrees"), 0);
    assert!(cache.files(&key, 100).unwrap().is_empty());
}

/// A file the chunker stopped short of is only partly indexed, and nothing
/// short of re-chunking it could tell. The row says so, which is the difference
/// between "there is no match here" and "there is no match in the part that was
/// indexed".
#[test]
fn a_partly_chunked_file_is_recorded_as_truncated() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    // Many small sections rather than one huge file: a file past
    // `OVERSIZED_FILE_THRESHOLD` is not eligible at all, so the only way to
    // exhaust the per-file chunk budget is structurally.
    let mut document = String::new();
    for index in 0..(crate::MAX_CHUNKS_PER_FILE + 64) {
        document.push_str(&format!("## Section {index}\n\nBody {index}.\n\n"));
    }
    let bytes = document.into_bytes();
    let entry = entry("docs/big.md", &bytes);
    let (_, produced) = derive(&entry, &bytes);
    assert!(
        produced.truncation.is_some(),
        "the fixture must actually exhaust the chunk budget"
    );

    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("docs/big.md", &bytes)],
    );

    let row = cache
        .file(&key, &RepoPath::from_bytes(b"docs/big.md".to_vec()))
        .unwrap()
        .expect("the file is indexed");
    assert!(row.truncated);
    assert!(
        !cache
            .file(&key, &RepoPath::from_bytes(b"docs/big.md".to_vec()))
            .unwrap()
            .is_none()
    );
}

/// A file version left behind by a batch that never committed is collected by
/// the next batch, even a targeted one — which collects only what it displaced,
/// and would otherwise let a repository updated incrementally accumulate the
/// derived rows of every interruption.
#[test]
fn an_abandoned_batchs_content_is_collected_by_the_next_one() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("kept.rs", b"fn kept() {}\n")],
    );
    let versions = count(&fixture.database(), "file_versions");

    let mut abandoned = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    record_many(&mut abandoned, 0..FLUSHED_FILES);
    drop(abandoned);
    assert!(count(&fixture.database(), "file_versions") > versions);

    let next = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    let receipt = next.commit(&cancellation).unwrap();

    assert!(receipt.rows_collected > 0);
    assert_eq!(count(&fixture.database(), "file_versions"), versions);
    assert_eq!(count(&fixture.database(), "files"), 1);
}

// -- status and counts ------------------------------------------------------

/// Status is a poll a UI makes during a cold build, so it reports what the last
/// commit published rather than taking the writer's connection to find out.
#[test]
fn status_reports_the_counts_the_last_commit_left() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");

    let empty = cache
        .status()
        .counts
        .expect("a cache this call created is empty");
    assert_eq!(empty.files, 0);
    assert_eq!(empty.chunks, 0);
    assert!(empty.database_bytes > 0);

    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n")],
    );

    let counts = cache.status().counts.expect("the cache can be counted");
    assert_eq!(counts.worktrees, 1);
    assert_eq!(counts.files, 2);
    assert_eq!(counts.contents, 2);
    assert_eq!(counts.file_versions, 2);
    assert!(counts.chunks >= 2);
    assert_eq!(counts.symbols, 0);
    assert_eq!(cache.counts().unwrap(), counts);
}

/// Adopting a warm cache does not count it. Six table scans on the path a user
/// reached by opening a project would spend the whole open budget on a number
/// nothing has asked for, and `None` is the honest word for "nobody has
/// counted" — which is not the same as "there is nothing here".
#[test]
fn adopting_a_warm_cache_reports_no_counts_until_something_asks() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    drop(cache);

    let reopened = fixture.open();

    assert!(reopened.status().counts.is_none());
    assert_eq!(reopened.counts().unwrap().files, 1);
    assert!(
        reopened.status().counts.is_none(),
        "an on-demand count answers the caller rather than publishing to a poll"
    );

    reopened.refresh(&Cancellation::default()).unwrap();

    assert_eq!(
        reopened
            .status()
            .counts
            .expect("a refresh publishes what it found")
            .files,
        1
    );
}

// -- budgets and eviction ---------------------------------------------------

/// A cache at its cap refuses the batch whole. Storing what fits would leave
/// retrieval answering "no match" for content the cache never held, which a
/// caller cannot tell from a repository that does not contain it.
#[test]
fn a_batch_past_the_per_repository_cap_is_refused_and_the_cache_still_serves() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    let watermark = cache.worktree_generation(&key).unwrap();

    // The cap is a compiled constant, so the file is grown past it rather than
    // the constant being lowered: the refusal has to come from the same
    // measurement production uses.
    grow_database_past_cap(&fixture.database());

    let cancellation = Cancellation::default();
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    let entry = entry("b.rs", b"fn b() {}\n");
    let (version, chunks) = derive(&entry, b"fn b() {}\n");
    let error = batch
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .and_then(|()| batch.commit(&cancellation).map(|_| ()))
        .unwrap_err();

    assert_eq!(error.kind(), "index_budget_exhausted");
    assert!(matches!(
        error,
        ContextEngineError::IndexBudgetExhausted { limit, .. } if limit == super::MAX_INDEX_DB_BYTES
    ));
    assert_eq!(
        cache.worktree_generation(&key).unwrap(),
        watermark,
        "the previous generation still answers"
    );
    assert_eq!(cache.files(&key, 100).unwrap().len(), 1);
}

/// Fills the database with pages until it is past the per-repository cap.
///
/// A real table rather than appended bytes, because the measurement is of the
/// file SQLite maintains and a database with junk on the end is a corrupt one.
fn grow_database_past_cap(database: &Path) {
    let connection = Connection::open(database).expect("the cache opens");
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS ballast (block BLOB)")
        .expect("the ballast table is creatable");
    let block = vec![0_u8; 1024 * 1024];
    while fs::metadata(database).map(|meta| meta.len()).unwrap_or(0) <= super::MAX_INDEX_DB_BYTES {
        connection
            .execute("INSERT INTO ballast (block) VALUES (?1)", [&block])
            .expect("the ballast row is insertable");
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("the log is checkpointable");
    }
}

/// Eviction removes whole caches, oldest first, and stops as soon as the
/// subtree fits. A half-emptied index is a lying index; a missing one is an
/// honest cold start.
#[test]
fn eviction_removes_whole_caches_least_recently_opened_first() {
    let fixture = Fixture::new();
    let data_dir = fixture.root.path();
    let context = data_dir.join(harkness_core::CONTEXT_DIRECTORY);
    let cancellation = Cancellation::default();

    // Opened in order, so the first is the least recently opened. Each is
    // padded so that the subtree is over a bound of two caches' worth.
    let mut roots = Vec::new();
    for index in 0..3 {
        let key = format!("0000000{index}-0000-5000-8000-000000000000");
        let root = context.join(&key);
        let cache =
            IndexCache::open_or_create(&root, &ExpectedVersions::current(), &key, &cancellation)
                .unwrap();
        commit_full(
            &cache,
            &worktree("alpha"),
            "/workspaces/alpha",
            &[("a.rs", b"fn a() {}\n")],
        );
        drop(cache);
        roots.push(root);
    }

    let before = survey(&context, &cancellation).unwrap();
    assert_eq!(before.len(), 3);
    let total: u64 = before.iter().map(|cache| cache.bytes).sum();
    let smallest = before.iter().map(|cache| cache.bytes).min().unwrap();

    let report = evict_to_budget(data_dir, total - smallest, &cancellation).unwrap();

    assert!(report.within_budget);
    assert_eq!(report.evicted.len(), 1, "only as many as the bound needs");
    assert_eq!(
        report.evicted[0].root, roots[0],
        "the least recently opened goes first"
    );
    assert!(!roots[0].join(super::INDEX_DATABASE_FILE).exists());
    assert!(roots[1].join(super::INDEX_DATABASE_FILE).exists());
    assert!(roots[2].join(super::INDEX_DATABASE_FILE).exists());
}

/// A cache a process holds open is not deleted underneath it, however old it is.
#[test]
fn eviction_skips_a_cache_that_is_still_open() {
    let fixture = Fixture::new();
    let data_dir = fixture.root.path();
    let context = data_dir.join(harkness_core::CONTEXT_DIRECTORY);
    let cancellation = Cancellation::default();
    let key = "00000001-0000-5000-8000-000000000000";
    let root = context.join(key);
    let held = IndexCache::open_or_create(&root, &ExpectedVersions::current(), key, &cancellation)
        .unwrap();

    // A bound of zero asks for everything to go; the open cache is what stops it.
    let report = evict_to_budget(data_dir, 0, &cancellation).unwrap();

    assert!(report.evicted.is_empty());
    assert_eq!(report.skipped_in_use, 1);
    assert!(!report.within_budget, "an honest report says it could not");
    assert!(root.join(super::INDEX_DATABASE_FILE).exists());
    assert_eq!(held.meta().repository_identity, key);
}

/// A subtree already inside its bound is surveyed and left alone.
#[test]
fn eviction_within_budget_removes_nothing() {
    let fixture = Fixture::new();
    let data_dir = fixture.root.path();
    let cancellation = Cancellation::default();
    let key = "00000002-0000-5000-8000-000000000000";
    let root = data_dir.join(harkness_core::CONTEXT_DIRECTORY).join(key);
    drop(
        IndexCache::open_or_create(&root, &ExpectedVersions::current(), key, &cancellation)
            .unwrap(),
    );

    let report = evict_to_budget(data_dir, MAX_TOTAL_CONTEXT_BYTES, &cancellation).unwrap();

    assert!(report.within_budget);
    assert!(report.evicted.is_empty());
    assert_eq!(report.bytes_before, report.bytes_after);
    assert!(root.join(super::INDEX_DATABASE_FILE).exists());
}

/// A data directory nothing has indexed is not a failure, and neither is a
/// cancelled sweep — but they are different answers.
#[test]
fn eviction_answers_for_an_empty_subtree_and_refuses_a_cancelled_one() {
    let fixture = Fixture::new();
    let cancellation = Cancellation::default();

    let report = evict_to_budget(fixture.root.path(), 0, &cancellation).unwrap();
    assert_eq!(report.bytes_before, 0);
    assert!(report.within_budget);

    cancellation.cancel();
    assert_eq!(
        evict_to_budget(fixture.root.path(), 0, &cancellation)
            .unwrap_err()
            .kind(),
        "cancelled"
    );
}

// -- keys -------------------------------------------------------------------

/// A read that stopped at its bound says so. A full page and a repository that
/// happens to hold exactly that many rows are otherwise the same answer, and a
/// caller assembling a repository map reads the first as the whole tree.
#[test]
fn a_read_that_reached_its_bound_says_there_is_more() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[
            ("a.rs", b"fn a() {}\n"),
            ("b.rs", b"fn b() {}\n"),
            ("c.rs", b"fn c() {}\n"),
        ],
    );

    let bounded = cache.files(&key, 2).unwrap();
    assert_eq!(bounded.len(), 2);
    assert!(bounded.more);

    let whole = cache.files(&key, 3).unwrap();
    assert_eq!(whole.len(), 3);
    assert!(
        !whole.more,
        "stopping because there is nothing left is not truncation"
    );

    // A caller asking for nothing gets nothing. Clamping zero up to one would
    // answer a question nobody asked, and a paging loop whose budget reached
    // zero would never end.
    let none = cache.files(&key, 0).unwrap();
    assert!(none.is_empty());
    assert!(!none.more);
}

/// The key names a checkout, so one checkout is one row however it was reached.
#[test]
fn a_worktree_key_is_derived_from_the_root_it_names() {
    assert_eq!(
        WorktreeKey::for_root(Path::new("/workspaces/alpha")),
        WorktreeKey::for_root(Path::new("/workspaces/alpha"))
    );
    assert_ne!(
        WorktreeKey::for_root(Path::new("/workspaces/alpha")),
        WorktreeKey::for_root(Path::new("/workspaces/beta"))
    );
    assert_eq!(
        WorktreeKey::for_root(Path::new("/workspaces/alpha"))
            .as_str()
            .len(),
        36,
        "the key is a UUID, which is what the column is sized for"
    );
}

/// A version this build never produced is still readable, because the store
/// keeps identifiers as the strings they print as rather than re-deriving them.
#[test]
fn a_file_version_identifier_round_trips_through_the_column() {
    let path = RepoPath::from_bytes(b"src/main.rs".to_vec());
    let id = FileVersionId::derive(&path, b"fn main() {}\n");
    assert_eq!(id.to_string().parse::<FileVersionId>().unwrap(), id);
}

// -- reading and refreshing part of a worktree ------------------------------

/// The scoped read [#115]'s merge walks, and the containment rule it must obey.
/// A prefix match on the stored bytes alone would return `src-generated.rs` for
/// a subtree scope of `src`, and the merge would then decide that a file it
/// never walked has no path beside it — a removal of a file that exists.
///
/// [#115]: https://github.com/fullstacktaiye/harkness/issues/115
#[test]
fn a_scoped_read_stops_at_the_separator_and_pages_in_path_order() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[
            ("src", b"a directory-shaped name that is a file here\n"),
            ("src-generated.rs", b"fn generated() {}\n"),
            ("src/a.rs", b"fn a() {}\n"),
            ("src/nested/b.rs", b"fn b() {}\n"),
            ("srcs/c.rs", b"fn c() {}\n"),
        ],
    );

    let scoped = cache
        .files_under(&key, &RepoPath::from_bytes(b"src".to_vec()), None, 100)
        .expect("the scoped read succeeds");
    assert_eq!(
        scoped
            .rows
            .iter()
            .map(|row| row.path.display())
            .collect::<Vec<_>>(),
        ["src", "src/a.rs", "src/nested/b.rs"]
    );
    assert!(!scoped.more);

    // The empty prefix is the worktree root, which is what makes one method
    // serve a whole-tree sweep and a one-directory one.
    let everything = cache
        .files_under(&key, &RepoPath::from_bytes(Vec::new()), None, 100)
        .expect("the whole-tree read succeeds");
    assert_eq!(everything.rows.len(), 5);

    // Paged, and continuing after a path rather than by offset — the merge
    // reads forward and never re-reads a row it has already decided about.
    let first = cache
        .files_under(&key, &RepoPath::from_bytes(b"src".to_vec()), None, 1)
        .expect("the first page succeeds");
    assert_eq!(first.rows[0].path.display(), "src");
    assert!(first.more);
    let second = cache
        .files_under(
            &key,
            &RepoPath::from_bytes(b"src".to_vec()),
            Some(&first.rows[0].path),
            100,
        )
        .expect("the second page succeeds");
    assert_eq!(
        second
            .rows
            .iter()
            .map(|row| row.path.display())
            .collect::<Vec<_>>(),
        ["src/a.rs", "src/nested/b.rs"]
    );
}

/// A file whose bytes turned out to be the ones its row already names is
/// refreshed rather than re-derived. Recording it as a plain entry would clear
/// `files.file_version_id`, and the commit's collection would then delete a
/// chunk set nothing was wrong with.
#[test]
fn a_refreshed_file_keeps_its_derivation_and_updates_its_metadata() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let bytes = b"fn a() {}\n";
    commit_full(&cache, &key, "/workspaces/alpha", &[("src/a.rs", bytes)]);
    let path = RepoPath::from_bytes(b"src/a.rs".to_vec());
    let before = cache
        .file(&key, &path)
        .unwrap()
        .expect("the file is stored");
    let chunks = cache.chunks(&key, &path).unwrap();
    assert!(!chunks.is_empty());

    let cancellation = Cancellation::default();
    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    let mut touched = entry("src/a.rs", bytes);
    touched.mtime_ns = Some(1_800_000_000_000_000_000);
    batch
        .record_refreshed(&touched, CLASSIFY_VERSION)
        .expect("the refresh records");
    batch.commit(&cancellation).expect("the batch commits");

    let after = cache
        .file(&key, &path)
        .unwrap()
        .expect("the file is stored");
    assert_eq!(after.mtime_ns, Some(1_800_000_000_000_000_000));
    assert_eq!(after.file_version, before.file_version);
    assert_eq!(after.content_sha256, before.content_sha256);
    assert_eq!(after.chunking_version, before.chunking_version);
    assert_eq!(
        cache.chunks(&key, &path).unwrap(),
        chunks,
        "a refresh must not cost a file its chunks"
    );
}

/// The committed base a full pass verified against, written by the same
/// statement that moves the watermark. A targeted batch records none, because a
/// checkout is not verified by looking at one file of it.
#[test]
fn only_a_batch_that_says_so_records_the_committed_base() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let key = worktree("alpha");
    let cancellation = Cancellation::default();
    assert_eq!(cache.worktree_marker(&key).unwrap(), None);

    commit_full(
        &cache,
        &key,
        "/workspaces/alpha",
        &[("a.rs", b"fn a() {}\n")],
    );
    assert_eq!(
        cache.worktree_marker(&key).unwrap(),
        None,
        "a batch that never claimed a base must not have one recorded for it"
    );

    let mut batch = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Full,
            &cancellation,
        )
        .unwrap();
    batch.record_head_marker(Some("main@abc123"));
    batch.commit(&cancellation).unwrap();
    assert_eq!(
        cache.worktree_marker(&key).unwrap(),
        Some("main@abc123".to_owned())
    );

    // A later targeted batch leaves it alone rather than reasserting a base it
    // did not check.
    let mut targeted = cache
        .begin(
            &key,
            Path::new("/workspaces/alpha"),
            BatchScope::Targeted,
            &cancellation,
        )
        .unwrap();
    let bytes = b"fn a() { changed() }\n";
    let entry = entry("a.rs", bytes);
    let (version, chunks) = derive(&entry, bytes);
    targeted
        .record_chunked(&entry, &version, &chunks, CLASSIFY_VERSION)
        .unwrap();
    targeted.commit(&cancellation).unwrap();
    assert_eq!(
        cache.worktree_marker(&key).unwrap(),
        Some("main@abc123".to_owned())
    );

    // And a checkout the cache has never seen has no base rather than an empty
    // one, because "cannot be told" and "verified against nothing" are
    // different answers.
    assert_eq!(cache.worktree_marker(&worktree("beta")).unwrap(), None);
}

/// Forgetting a checkout takes its rows and collects what nothing else names,
/// while a sibling's shared content survives untouched.
#[test]
fn forgetting_a_worktree_collects_only_what_nothing_else_names() {
    let fixture = StoreFixture::new();
    let cache = fixture.open();
    let alpha = worktree("alpha");
    let beta = worktree("beta");
    let shared: &[u8] = b"fn shared() {}\n";
    commit_full(
        &cache,
        &alpha,
        "/workspaces/alpha",
        &[("shared.rs", shared), ("alpha-only.rs", b"fn alpha() {}\n")],
    );
    commit_full(&cache, &beta, "/workspaces/beta", &[("shared.rs", shared)]);
    let before = cache.counts().unwrap();
    assert_eq!(before.worktrees, 2);

    let report = cache
        .forget_worktree(&alpha, &Cancellation::default())
        .expect("the checkout is forgotten");

    assert_eq!(report.files_removed, 2);
    assert!(report.rows_collected >= 2, "{report:?}");
    let after = cache.counts().unwrap();
    assert_eq!(after.worktrees, 1);
    assert_eq!(after.files, 1);
    let path = RepoPath::from_bytes(b"shared.rs".to_vec());
    assert!(
        cache.file(&beta, &path).unwrap().is_some(),
        "the surviving checkout keeps its row"
    );
    assert!(
        !cache.chunks(&beta, &path).unwrap().is_empty(),
        "and the content rows it still names"
    );
    assert!(cache.file(&alpha, &path).unwrap().is_none());
}
