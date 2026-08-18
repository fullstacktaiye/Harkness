//! Ownership of the context engines a process holds open.
//!
//! `harkness-context` knows how to serve one workspace; this module decides how
//! many of it there are. One lazily created, `Arc`-shared [`ContextEngine`] per
//! open project means the command line and the application answer context
//! questions from the same handle, so "what the index says" cannot differ
//! between front ends — which is the failure the whole boundary exists to
//! prevent.
//!
//! # One engine per project, one cache per repository
//!
//! The two are not the same count, and both are deliberate. A Harkness-managed
//! worktree is its own catalog entry with its own [`ProjectId`], so keying the
//! registry by project gives one engine per *checkout* — which is right, because
//! a snapshot is a fact about one worktree. The expensive, content-addressed
//! half is shared anyway: every engine of one repository resolves to the same
//! `<data_dir>/context/<repository-key>` cache, and the write-ahead log and busy
//! timeout are what make several handles on it safe. Folding those handles into
//! one engine with per-worktree state inside it is [#115]'s.
//!
//! # Locking
//!
//! The registry mutex is **never held while an engine is opened**. Opening one
//! inspects a repository and may wait out the cache's busy timeout, and a
//! five-second wait for one project must not stop every other project being
//! looked up. Two callers racing for one project may therefore both build an
//! engine; the first to reach the map wins and the loser drops its own, so every
//! caller still leaves with the one handle the project shares. Nothing here
//! takes the repository lock, the catalog lock, or a store transaction, so the
//! workspace's lock ordering is untouched.
//!
//! # It records nothing
//!
//! An engine returns typed values and persists none of them. A snapshot becomes
//! evidence through
//! [`Store::record_workspace_snapshot_for_run`](crate::store::Store::record_workspace_snapshot_for_run),
//! and a cache rebuild reaches a run's timeline through
//! [`cache_recreated_event`] — both called by whoever owns the run, never by the
//! engine. That is what keeps deleting `<data_dir>/context/` lossless (ADR-0004).
//!
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use harkness_context::index::CacheRecreation;
use harkness_context::{ContextEngine, ContextEngineConfig, ContextEngineError};
use harkness_core::ProjectId;
use harkness_git::Cancellation;
use serde_json::json;
use time::OffsetDateTime;

use crate::store::{EventKind, RunEvent};

/// The context engines this process holds open, one per project.
#[derive(Debug)]
pub struct ContextEngines {
    data_dir: PathBuf,
    engines: Mutex<HashMap<ProjectId, Arc<ContextEngine>>>,
}

impl ContextEngines {
    /// Serves engines whose caches live beneath `data_dir`.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            engines: Mutex::new(HashMap::new()),
        }
    }

    /// The data directory every cache this registry opens lives beneath.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The engine for `project_id`, opening one against `worktree_root` if
    /// this is the first ask.
    ///
    /// A held engine whose worktree root is not the one being asked about is
    /// replaced rather than returned. A project's checkout can be repaired or
    /// moved, and answering about the path it used to be at would be worse than
    /// paying for a second open.
    ///
    /// # Errors
    ///
    /// The failures of [`ContextEngine::open`]: a missing worktree root, and a
    /// directory that is not a Git worktree — which is the answer a
    /// `ProjectSource::Local` folder gets in v0.4. A cache that cannot be
    /// prepared is *not* one of them; the engine opens and reports it through
    /// [`ContextEngine::index_status`].
    pub fn engine(
        &self,
        project_id: ProjectId,
        worktree_root: &Path,
        cancellation: &Cancellation,
    ) -> Result<Arc<ContextEngine>, ContextEngineError> {
        self.engine_from(
            ContextEngineConfig::new(project_id, worktree_root, &self.data_dir),
            cancellation,
        )
    }

    /// The engine for `config`'s project, opening one from `config` if this is
    /// the first ask.
    ///
    /// The configuration is used only when an engine is actually built. A
    /// project already holding one keeps the settings it was opened with, so
    /// changing them means releasing the project first — a bump to the
    /// configuration generation is a new engine, not a mutation of a live one.
    ///
    /// # Errors
    ///
    /// The failures of [`ContextEngine::open`], and
    /// [`ContextEngineError::CacheOpenFailed`] when `config` names a different
    /// data directory than this registry serves.
    pub fn engine_from(
        &self,
        config: ContextEngineConfig,
        cancellation: &Cancellation,
    ) -> Result<Arc<ContextEngine>, ContextEngineError> {
        // A configuration naming another data directory would put this
        // project's cache outside the tree `HARKNESS_DATA_DIR` covers and
        // outside the one `release`-then-delete recovers, while the registry
        // went on reporting its own `data_dir` for it. Refusing is the only
        // honest answer: silently rewriting the caller's explicit value would
        // be worse than refusing it.
        if config.data_dir() != self.data_dir {
            return Err(ContextEngineError::CacheOpenFailed {
                path: config.data_dir().join(harkness_core::CONTEXT_DIRECTORY),
                reason: format!(
                    "this registry serves '{}' and the configuration names '{}'",
                    self.data_dir.display(),
                    config.data_dir().display()
                ),
            });
        }
        let project_id = config.project_id();
        if let Some(held) = self.held(project_id, config.worktree_root()) {
            return Ok(held);
        }

        // Opened with no lock held: this inspects a repository and can wait out
        // the cache's busy timeout, and one project's slow open must not stop
        // every other project being looked up.
        let opened = Arc::new(ContextEngine::open(config, cancellation)?);
        let mut engines = lock(&self.engines);
        match engines.entry(project_id) {
            // Somebody won the race. Their handle is the one the project
            // shares, so this one is dropped rather than installed over it.
            Entry::Occupied(occupied)
                if occupied.get().worktree_root() == opened.worktree_root() =>
            {
                Ok(Arc::clone(occupied.get()))
            }
            Entry::Occupied(mut occupied) => {
                occupied.insert(Arc::clone(&opened));
                Ok(opened)
            }
            Entry::Vacant(vacant) => Ok(Arc::clone(vacant.insert(opened))),
        }
    }

    /// Drops this registry's reference to `project_id`'s engine.
    ///
    /// The engine itself lives until the last front-end reference goes too,
    /// which is what keeps a call already in flight from losing its cache
    /// underneath it. Returns whether an engine was held.
    pub fn release(&self, project_id: ProjectId) -> bool {
        lock(&self.engines).remove(&project_id).is_some()
    }

    /// Drops every engine this registry holds.
    pub fn release_all(&self) {
        lock(&self.engines).clear();
    }

    /// How many projects currently hold an engine.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.engines).len()
    }

    /// Whether no project holds an engine.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn held(&self, project_id: ProjectId, worktree_root: &Path) -> Option<Arc<ContextEngine>> {
        let engines = lock(&self.engines);
        let held = engines.get(&project_id)?;
        (held.worktree_root() == worktree_root).then(|| Arc::clone(held))
    }
}

/// The timeline entry that records a context cache being thrown away.
///
/// Nothing in `runtime.db` changed when this happened — the cache is a separate,
/// disposable store — but a generation bump changes what every later snapshot
/// digest means, so a run that spans one has to be able to say so.
///
/// The generations travel as numbers rather than as strings, so the store's
/// mandatory redaction of every JSON string value cannot rewrite them.
#[must_use]
pub fn cache_recreated_event(recreation: &CacheRecreation, at: OffsetDateTime) -> RunEvent {
    RunEvent::new(EventKind::ContextCacheRecreated, at).with_payload(json!({
        "reason": recreation.reason.as_str(),
        "detail": recreation.detail,
        "previous_generation": recreation.previous_generation,
        "generation": recreation.generation,
        "quarantined": recreation.quarantined_to.is_some(),
    }))
}

/// Takes a lock, adopting the contents even if a previous holder panicked.
///
/// A panic in a caller says nothing about the map: it holds `Arc`s and a failed
/// insert leaves it as it was, so refusing to use it would turn one failure into
/// a process that can never open a context engine again.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
