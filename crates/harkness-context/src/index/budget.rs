//! What the cache subtree is allowed to cost, and what happens when it costs more.
//!
//! Two bounds, and they fail in opposite directions on purpose.
//!
//! [`MAX_INDEX_DB_BYTES`](super::MAX_INDEX_DB_BYTES) is *per repository* and it
//! **fails the batch**. A cache that quietly stopped storing rows at half a
//! gibibyte would answer "no match" for content it never held, which is worse
//! than an error: a retrieval that finds nothing looks exactly like a
//! repository that contains nothing. `index_budget_exhausted` says which cap
//! was reached and leaves the previous generation intact and usable.
//!
//! [`MAX_TOTAL_CONTEXT_BYTES`] is *across every repository* and it **evicts
//! whole caches**. Nothing partial is ever deleted — a half-emptied index is a
//! lying index, whereas a missing one is an honest cold start — so eviction
//! removes least-recently-opened repository directories entirely until the
//! subtree is back under its bound.
//!
//! # Why a lock file rather than a heuristic
//!
//! A cache another process is using must not be deleted underneath it, and
//! "another process is using it" cannot be read off the filesystem: WAL
//! sidecars survive a crash, modification times say nothing about readers, and
//! a process identifier in a file is a claim rather than a fact. Every open
//! [`IndexCache`](super::IndexCache) therefore holds a *shared* advisory lock
//! on `<cache-root>/index.lock` for its whole life, and eviction takes the
//! *exclusive* one. The kernel releases the lock however the holder ends,
//! including a `SIGKILL`, so there is no stale state to reap — which is the
//! same bargain the coordinator's lease and the repository lock already make.
//!
//! # Recency without an access clock
//!
//! `atime` is unusable: `relatime` is the default mount option on Linux, so a
//! file read twice in a day updates it once, and `noatime` never updates it at
//! all. The cache records `last_opened_at` in its own metadata instead, stamped
//! by [`IndexCache::open_or_create`](super::IndexCache::open_or_create) *after*
//! it has decided the cache is one this build may adopt — so the refusal path
//! for a newer cache still leaves the file byte-identical.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Instant;

use harkness_git::Cancellation;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::ContextEngineError;

use super::INDEX_DATABASE_FILE;

/// Name of the advisory lock every open cache holds in its own root.
pub const CACHE_LOCK_FILE: &str = "index.lock";

/// How much `<data_dir>/context/` may hold before caches are evicted.
///
/// Four gibibytes across every repository a user has opened. It is a *subtree*
/// bound rather than a per-repository one because that is the number a disk
/// actually feels: eight repositories under their individual caps are within
/// every per-repository rule and still four gibibytes of derived rows.
pub const MAX_TOTAL_CONTEXT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// One repository's cache, as the eviction sweep sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheUsage {
    /// Repository key the directory is named for.
    pub repository_key: String,
    /// Directory holding the cache.
    pub root: PathBuf,
    /// Bytes the directory occupies, sidecars and quarantines included.
    pub bytes: u64,
    /// When a build last adopted this cache, when its metadata could be read.
    pub last_opened_at: Option<OffsetDateTime>,
}

/// What one eviction sweep did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct EvictionReport {
    /// Bytes the subtree held before the sweep.
    pub bytes_before: u64,
    /// Bytes it holds now.
    pub bytes_after: u64,
    /// Caches removed, oldest first.
    pub evicted: Vec<CacheUsage>,
    /// Caches left alone because another process held them open.
    pub skipped_in_use: u64,
    /// Whether the subtree is under its bound now.
    pub within_budget: bool,
}

/// A held advisory lock over one cache root.
///
/// Shared while a cache is open, exclusive while it is being evicted. Dropping
/// the guard releases it; so does the process ending, however it ends.
#[derive(Debug)]
pub(super) struct CacheLock {
    file: File,
}

impl CacheLock {
    /// Takes the shared lock an open cache holds for its whole life.
    ///
    /// Failure is *not* fatal and the caller is expected to carry on without
    /// one: a read-only data directory, a filesystem with no locking, or a
    /// descriptor limit would otherwise take retrieval away over a bookkeeping
    /// file. What is lost is protection from eviction, and eviction is a
    /// maintenance sweep a user asks for rather than something that happens
    /// under a running build.
    pub(super) fn shared(cache_root: &Path) -> Option<Self> {
        let file = open_lock_file(cache_root)?;
        file.try_lock_shared().ok().map(|()| Self { file })
    }

    /// Takes the exclusive lock, or answers `None` when a cache is open.
    fn exclusive(cache_root: &Path) -> Option<Self> {
        let file = open_lock_file(cache_root)?;
        file.try_lock().ok().map(|()| Self { file })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // Closing the handle releases it anyway; unlocking says where the
        // critical section ends.
        let _ = self.file.unlock();
    }
}

fn open_lock_file(cache_root: &Path) -> Option<File> {
    fs::create_dir_all(cache_root).ok()?;
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache_root.join(CACHE_LOCK_FILE))
        .ok()
}

/// Brings `<data_dir>/context/` back under [`MAX_TOTAL_CONTEXT_BYTES`].
///
/// Removes least-recently-opened repository caches whole, skipping any a
/// process still holds open, and stops as soon as the subtree fits. A subtree
/// that is already within its bound is scanned and left alone.
///
/// This is safe to run at any moment against any data directory: everything it
/// deletes is derived (ADR-0004), so the cost is warm-up time and never
/// evidence. It is not safe to run against a directory that is *not* a Harkness
/// context subtree, which is why it composes the path itself rather than taking
/// one.
///
/// # Errors
///
/// [`ContextEngineError::Cancelled`] when the token is observed. A directory
/// that cannot be read or removed is *reported* rather than fatal: the sweep
/// skips it and keeps going, because one unreadable cache must not stop the
/// disk being reclaimed.
pub fn evict_to_budget(
    data_dir: &Path,
    limit: u64,
    cancellation: &Cancellation,
) -> Result<EvictionReport, ContextEngineError> {
    if cancellation.is_cancelled() {
        return Err(ContextEngineError::Cancelled);
    }
    let started = Instant::now();
    let span = tracing::debug_span!("context.index.evict", limit);
    let _entered = span.enter();

    let context_root = data_dir.join(harkness_core::CONTEXT_DIRECTORY);
    let mut caches = survey(&context_root, cancellation)?;
    let bytes_before: u64 = caches.iter().map(|cache| cache.bytes).sum();
    if bytes_before <= limit {
        return Ok(EvictionReport {
            bytes_before,
            bytes_after: bytes_before,
            evicted: Vec::new(),
            skipped_in_use: 0,
            within_budget: true,
        });
    }

    // Oldest first, and a cache whose metadata could not be read sorts oldest
    // of all: it is either corrupt or foreign, and either way it is the one
    // whose removal costs the least. `None` ordering before `Some` is what
    // `Option`'s own comparison already gives.
    caches.sort_by(|left, right| {
        left.last_opened_at
            .cmp(&right.last_opened_at)
            .then_with(|| left.repository_key.cmp(&right.repository_key))
    });

    let mut bytes = bytes_before;
    let mut evicted = Vec::new();
    let mut skipped_in_use = 0;
    for cache in caches {
        if bytes <= limit {
            break;
        }
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let Some(guard) = CacheLock::exclusive(&cache.root) else {
            skipped_in_use += 1;
            continue;
        };
        if remove_cache(&cache.root, guard) {
            bytes = bytes.saturating_sub(cache.bytes);
            evicted.push(cache);
        } else {
            skipped_in_use += 1;
        }
    }

    let report = EvictionReport {
        bytes_before,
        bytes_after: bytes,
        within_budget: bytes <= limit,
        evicted,
        skipped_in_use,
    };
    tracing::debug!(
        bytes_before = report.bytes_before,
        bytes_after = report.bytes_after,
        evicted = report.evicted.len(),
        skipped_in_use = report.skipped_in_use,
        duration_ms = started.elapsed().as_millis(),
        "context index eviction swept"
    );
    Ok(report)
}

/// Every repository cache beneath `context_root`, with its size and recency.
///
/// # Errors
///
/// [`ContextEngineError::Cancelled`] when the token is observed. A missing
/// subtree is not an error — it is a data directory nothing has indexed yet —
/// and answers an empty survey.
pub fn survey(
    context_root: &Path,
    cancellation: &Cancellation,
) -> Result<Vec<CacheUsage>, ContextEngineError> {
    let Ok(entries) = fs::read_dir(context_root) else {
        return Ok(Vec::new());
    };
    let mut caches = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let root = entry.path();
        let Some(repository_key) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        // Read the stamp *before* measuring, because reading it costs bytes: a
        // read-only connection to a write-ahead-logged database creates the
        // shared-memory file and cannot delete it again on close. Measuring
        // first would make two surveys of one unchanged subtree disagree, and a
        // budget that moves when it is looked at is not a budget.
        let last_opened_at = last_opened_at(&root.join(INDEX_DATABASE_FILE));
        caches.push(CacheUsage {
            bytes: directory_bytes(&root),
            last_opened_at,
            repository_key,
            root,
        });
    }
    Ok(caches)
}

/// Deletes a cache directory while holding its lock.
///
/// The lock file goes last and separately. Windows refuses to unlink a file any
/// handle has open, and the handle in question is the one proving nobody else
/// is here — so the contents are removed under the lock, the guard is dropped,
/// and only then is the marker itself unlinked. A directory left holding an
/// empty lock file is inert and the next open adopts it.
fn remove_cache(root: &Path, guard: CacheLock) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    let mut removed_everything = true;
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name() == CACHE_LOCK_FILE {
            continue;
        }
        let path = entry.path();
        let outcome = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed_everything &= outcome.is_ok();
    }
    drop(guard);
    let _ = fs::remove_file(root.join(CACHE_LOCK_FILE));
    let _ = fs::remove_dir(root);
    removed_everything
}

/// Bytes one cache directory occupies, sidecars and quarantines included.
fn directory_bytes(root: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// Reads `last_opened_at` without writing a byte of the cache.
///
/// Read-only for the same reason the metadata probe is: eviction inspects
/// caches this build may not be able to address at all, and opening one for
/// writing would recover its write-ahead log.
fn last_opened_at(database: &Path) -> Option<OffsetDateTime> {
    let connection =
        Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let stamp: Option<String> = connection
        .query_row(
            "SELECT last_opened_at FROM index_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    OffsetDateTime::parse(&stamp?, &Rfc3339).ok()
}
