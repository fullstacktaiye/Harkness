//! The durable records held by the project catalog.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
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

/// Describes how a project entered the catalog and carries the metadata that
/// is valid for that source alone.
///
/// The internally tagged representation is flattened into [`Project`], so the
/// durable JSON remains additive: v1 managed repositories still have sibling
/// `"source"` and `"remote"` fields, while v2 worktrees add `"parent"` and
/// `"worktree_branch"` beside `"source": "worktree"`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProjectSource {
    /// A directory that already exists on the local machine.
    Local,
    /// A clone owned by Harkness and stored below its repositories directory.
    ManagedRepository {
        /// Canonical GitHub remote identity for the managed repository.
        remote: String,
    },
    /// A Git worktree owned by Harkness and linked to another catalog entry.
    Worktree {
        /// The catalog entry whose repository owns this worktree.
        parent: ProjectId,
        /// The branch recorded when this worktree was created.
        worktree_branch: String,
    },
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    /// Stable catalog identifier.
    pub id: ProjectId,
    /// Human-readable name derived from the imported directory.
    pub display_name: String,
    /// Canonical path to the project directory.
    pub root: PathBuf,
    /// How this project entered the catalog.
    pub source: ProjectSource,
    /// The last time the project was imported or successfully reopened.
    pub last_opened: OffsetDateTime,
    /// Whether the root currently exists and can be read.
    ///
    /// Derived from the filesystem on every read, so it is deliberately not
    /// persisted; a stored copy would only ever be stale.
    pub available: bool,
    /// Git metadata when the root is the working directory of a Git
    /// repository.
    ///
    /// Derived alongside [`Project::available`], and likewise not persisted.
    pub git: Option<GitStatus>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProjectSourceKind {
    Local,
    ManagedRepository,
    Worktree,
}

/// The on-disk shape stays flat even though the Rust source type carries its
/// own data. Rejecting unknown and source-inappropriate fields avoids silently
/// dropping data when a same-version writer violates the schema policy.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectWire {
    id: ProjectId,
    display_name: String,
    root: PathBuf,
    source: ProjectSourceKind,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    parent: Option<ProjectId>,
    #[serde(default)]
    worktree_branch: Option<String>,
    last_opened: OffsetDateTime,
}

#[derive(Serialize)]
struct ProjectWireRef<'a> {
    id: ProjectId,
    display_name: &'a str,
    root: &'a PathBuf,
    source: ProjectSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_branch: Option<&'a str>,
    last_opened: OffsetDateTime,
}

impl<'de> Deserialize<'de> for Project {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProjectWire::deserialize(deserializer)?;
        let source = match wire.source {
            ProjectSourceKind::Local => {
                if wire.remote.is_some() || wire.parent.is_some() || wire.worktree_branch.is_some()
                {
                    return Err(de::Error::custom(
                        "a local project cannot carry managed-repository or worktree metadata",
                    ));
                }
                ProjectSource::Local
            }
            ProjectSourceKind::ManagedRepository => {
                if wire.parent.is_some() || wire.worktree_branch.is_some() {
                    return Err(de::Error::custom(
                        "a managed repository cannot carry worktree metadata",
                    ));
                }
                let remote = wire
                    .remote
                    .ok_or_else(|| de::Error::custom("a managed repository requires a remote"))?;
                ProjectSource::ManagedRepository { remote }
            }
            ProjectSourceKind::Worktree => {
                if wire.remote.is_some() {
                    return Err(de::Error::custom(
                        "a worktree cannot carry managed-repository metadata",
                    ));
                }
                let parent = wire
                    .parent
                    .ok_or_else(|| de::Error::custom("a worktree requires a parent"))?;
                let worktree_branch = wire
                    .worktree_branch
                    .ok_or_else(|| de::Error::custom("a worktree requires a worktree_branch"))?;
                ProjectSource::Worktree {
                    parent,
                    worktree_branch,
                }
            }
        };
        Ok(Self {
            id: wire.id,
            display_name: wire.display_name,
            root: wire.root,
            source,
            last_opened: wire.last_opened,
            available: false,
            git: None,
        })
    }
}

impl Serialize for Project {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (source, remote, parent, worktree_branch) = match &self.source {
            ProjectSource::Local => (ProjectSourceKind::Local, None, None, None),
            ProjectSource::ManagedRepository { remote } => (
                ProjectSourceKind::ManagedRepository,
                Some(remote.as_str()),
                None,
                None,
            ),
            ProjectSource::Worktree {
                parent,
                worktree_branch,
            } => (
                ProjectSourceKind::Worktree,
                None,
                Some(*parent),
                Some(worktree_branch.as_str()),
            ),
        };
        ProjectWireRef {
            id: self.id,
            display_name: &self.display_name,
            root: &self.root,
            source,
            remote,
            parent,
            worktree_branch,
            last_opened: self.last_opened,
        }
        .serialize(serializer)
    }
}
