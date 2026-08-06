//! The durable records held by the project catalog.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A stable identifier for a project catalog entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// Generates a new random project identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for ProjectId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Describes how a project entered the catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// A directory that already exists on the local machine.
    Local,
    /// A clone owned by Harkness and stored below its repositories directory.
    ManagedRepository,
}

/// Where a branch stands relative to the branch it tracks.
///
/// Resolved from local refs only. The counts answer "how far apart are the two
/// refs this machine already has", never "what does the remote have now", so
/// producing one never touches the network.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpstreamStatus {
    /// The tracked branch, as `origin/main` rather than as a full ref name.
    pub name: String,
    /// Commits on the local branch that the upstream does not have.
    pub ahead: usize,
    /// Commits on the upstream that the local branch does not have.
    pub behind: usize,
}

/// Git information collected from a project directory.
///
/// The cheap status tier: everything here comes from one in-process walk, so it
/// can be recomputed for every catalog entry on every read. A caller that needs
/// per-path detail asks [`GitService::detailed_status`] for one project.
///
/// [`GitService::detailed_status`]: crate::GitService::detailed_status
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitStatus {
    /// The checked-out branch, or `None` for a detached head.
    pub branch: Option<String>,
    /// Whether the worktree contains tracked or untracked changes.
    pub dirty: bool,
    /// The tracked branch and the divergence from it, when one is configured.
    pub upstream: Option<UpstreamStatus>,
    /// How many paths differ between the index and HEAD.
    pub staged: usize,
    /// How many tracked paths differ between the working tree and the index.
    ///
    /// Untracked paths are excluded: they are not recursed into during the
    /// walk, so counting them would count directories rather than files.
    pub unstaged: usize,
}

/// A durable project catalog entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Project {
    /// Stable catalog identifier.
    pub id: ProjectId,
    /// Human-readable name derived from the imported directory.
    pub display_name: String,
    /// Canonical path to the project directory.
    pub root: PathBuf,
    /// How this project entered the catalog.
    pub source: ProjectSource,
    /// Canonical GitHub remote identity for managed repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The last time the project was imported or successfully reopened.
    pub last_opened: OffsetDateTime,
    /// Whether the root currently exists and can be read.
    ///
    /// Derived from the filesystem on every read, so it is deliberately not
    /// persisted; a stored copy would only ever be stale.
    #[serde(skip)]
    pub available: bool,
    /// Git metadata when the root is the working directory of a Git
    /// repository.
    ///
    /// Derived alongside [`Project::available`], and likewise not persisted.
    #[serde(skip)]
    pub git: Option<GitStatus>,
}
