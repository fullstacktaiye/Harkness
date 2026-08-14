//! Workspace trust, filesystem boundaries, and process-construction safety.
//!
//! This module answers two questions and deliberately no policy question:
//! whether a concrete path resolves inside the roots granted to a workspace,
//! and exactly what an arbitrary child process is allowed to inherit. Policy
//! consumes these facts later; it does not get a second path resolver or
//! environment model of its own.
//!
//! Git is intentionally different. [`harkness_git`] runs one known program and
//! preserves most of its caller's environment so credential helpers keep
//! working, while removing variables that can redirect Git. An arbitrary tool
//! child is unknown code, so [`AllowlistedEnv`] starts empty and copies only
//! named variables. The two models sit beside each other because their trust
//! assumptions are different.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};

pub use crate::tool::{EnvironmentError, EnvironmentName, MAX_ENVIRONMENT_NAME_LENGTH};
use crate::tool::{RiskLevel, ToolDescriptor};

/// Variables an arbitrary tool child may inherit without an extra declaration.
pub const BASELINE_ENVIRONMENT: [&str; 5] = ["PATH", "HOME", "LANG", "LC_ALL", "TERM"];

/// Whether the user has decided to trust executable content in one workspace.
///
/// Trust does not turn repository text into instructions. It only records that
/// the user accepts executing code belonging to the exact project and path.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// No matching positive decision exists.
    #[default]
    Untrusted,
    /// The exact project at the exact canonical root was trusted.
    Trusted,
}

impl TrustState {
    /// Stable spelling stored in `runtime.db`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "untrusted" => Some(Self::Untrusted),
            "trusted" => Some(Self::Trusted),
            _ => None,
        }
    }
}

/// One durable trust decision, bound to project identity *and* canonical path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceTrust {
    project_id: ProjectId,
    canonical_root: PathBuf,
    state: TrustState,
    decided_at: OffsetDateTime,
}

impl WorkspaceTrust {
    /// Records a decision for an existing workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::RootUnavailable`] when the root cannot be
    /// canonicalized. A trust decision is never stored against a lexical path.
    pub fn decide(
        project_id: ProjectId,
        root: impl AsRef<Path>,
        state: TrustState,
        decided_at: OffsetDateTime,
    ) -> Result<Self, BoundaryError> {
        let canonical_root = canonical_root(root.as_ref())?;
        Ok(Self {
            project_id,
            canonical_root,
            state,
            decided_at: decided_at.to_offset(UtcOffset::UTC),
        })
    }

    pub(crate) fn from_stored(
        project_id: ProjectId,
        canonical_root: PathBuf,
        state: TrustState,
        decided_at: OffsetDateTime,
    ) -> Self {
        Self {
            project_id,
            canonical_root,
            state,
            decided_at,
        }
    }

    /// Resolves this record for a concrete catalog identity and current path.
    ///
    /// A missing path, a recreated catalog entry, or a checkout moved since the
    /// decision all resolve to [`TrustState::Untrusted`]. Path equality alone
    /// and project equality alone are each insufficient.
    #[must_use]
    pub fn resolve(&self, project_id: ProjectId, root: impl AsRef<Path>) -> TrustState {
        let Ok(root) = fs::canonicalize(root) else {
            return TrustState::Untrusted;
        };
        if self.project_id == project_id && self.canonical_root == root {
            self.state
        } else {
            TrustState::Untrusted
        }
    }

    /// Catalog identity this decision belongs to.
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Canonical workspace root this decision belongs to.
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Decision that was recorded.
    #[must_use]
    pub const fn state(&self) -> TrustState {
        self.state
    }

    /// UTC instant at which the decision was recorded.
    #[must_use]
    pub const fn decided_at(&self) -> OffsetDateTime {
        self.decided_at
    }
}

/// A canonical path proved to be inside one allowed root.
///
/// The inner path is private so tools cannot manufacture this capability from
/// unchecked input. Obtain one only through [`PathBoundary::contain`].
#[derive(Clone, Debug)]
pub struct ContainedPath {
    boundary: PathBoundary,
    supplied: PathBuf,
    resolved: PathBuf,
}

impl PartialEq for ContainedPath {
    fn eq(&self, other: &Self) -> bool {
        self.boundary == other.boundary && self.resolved == other.resolved
    }
}

impl Eq for ContainedPath {}

impl ContainedPath {
    /// Canonical path at the instant this capability was checked.
    ///
    /// Filesystem names can be replaced after any check. Code immediately
    /// before a filesystem operation must call [`Self::revalidate`] and use
    /// the fresh capability; process launch does this automatically.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.resolved
    }

    /// Resolves the caller's original spelling against the current filesystem.
    ///
    /// # Errors
    ///
    /// Returns the same typed boundary errors as [`PathBoundary::contain`].
    pub fn revalidate(&self) -> Result<Self, BoundaryError> {
        self.boundary.contain(&self.supplied)
    }
}

/// Canonical roots one tool invocation may address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathBoundary {
    workspace_root: PathBuf,
    extra_roots: Vec<PathBuf>,
}

impl PathBoundary {
    /// Builds a boundary from one workspace and explicit additional grants.
    ///
    /// Every root is canonicalized at construction, sorted, and deduplicated.
    /// Relative candidates are always resolved from the workspace root, never
    /// from the process's current directory.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::RootUnavailable`] for the first root that does
    /// not resolve to an existing directory.
    pub fn new(
        workspace_root: impl AsRef<Path>,
        extra_roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, BoundaryError> {
        let workspace_root = canonical_root(workspace_root.as_ref())?;
        let mut extras = extra_roots
            .into_iter()
            .map(|root| canonical_root(root.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        extras.retain(|root| root != &workspace_root);
        extras.sort_unstable();
        extras.dedup();
        Ok(Self {
            workspace_root,
            extra_roots: extras,
        })
    }

    /// Canonical workspace root relative candidates resolve from.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Every allowed canonical root, workspace first.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.workspace_root.as_path())
            .chain(self.extra_roots.iter().map(PathBuf::as_path))
    }

    /// Resolves `candidate` and proves it lies inside one allowed root.
    ///
    /// The nearest existing ancestor is canonicalized and any missing tail is
    /// restored and normalized, so a destination that will be created or a file
    /// that was just deleted remains addressable. Safe `..` traversal is
    /// accepted only when the resolved result remains in a granted root.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::OutsideAllowedRoots`] for a path outside every
    /// root, and [`BoundaryError::SymlinkEscapes`] when a symlink reached from
    /// an allowed root resolves outside them. The latter names both the link and
    /// its resolved target for the audit trail.
    pub fn contain(&self, candidate: impl AsRef<Path>) -> Result<ContainedPath, BoundaryError> {
        let supplied = candidate.as_ref();
        let absolute = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            self.workspace_root.join(supplied)
        };
        let resolved =
            harkness_git::canonicalize_with_missing_tail(&absolute).map_err(|source| {
                BoundaryError::CandidateUnavailable {
                    candidate: supplied.to_path_buf(),
                    reason: source.to_string(),
                }
            })?;
        if let Some((link, target)) = self.escaping_symlink(&absolute)? {
            return Err(BoundaryError::SymlinkEscapes { link, target });
        }
        if self.contains_resolved(&resolved) {
            return Ok(ContainedPath {
                boundary: self.clone(),
                supplied: supplied.to_path_buf(),
                resolved,
            });
        }
        Err(self.outside(supplied))
    }

    fn contains_resolved(&self, candidate: &Path) -> bool {
        self.roots().any(|root| candidate.starts_with(root))
    }

    fn outside(&self, candidate: &Path) -> BoundaryError {
        BoundaryError::OutsideAllowedRoots {
            candidate: candidate.to_path_buf(),
            roots: self.roots().map(Path::to_path_buf).collect(),
        }
    }

    /// Finds the first symlink reached *from inside* an allowed root whose
    /// resolved target leaves every root.
    fn escaping_symlink(
        &self,
        candidate: &Path,
    ) -> Result<Option<(PathBuf, PathBuf)>, BoundaryError> {
        let mut reached = PathBuf::new();
        for component in candidate.components() {
            reached.push(component.as_os_str());
            if matches!(
                component,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            ) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&reached) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(BoundaryError::CandidateUnavailable {
                        candidate: candidate.to_path_buf(),
                        reason: error.to_string(),
                    });
                }
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let Some(parent) = reached.parent() else {
                return Ok(None);
            };
            if !self.contains_resolved(parent) {
                return Ok(None);
            }
            let target =
                fs::read_link(&reached).map_err(|error| BoundaryError::CandidateUnavailable {
                    candidate: candidate.to_path_buf(),
                    reason: error.to_string(),
                })?;
            let target = if target.is_absolute() {
                target
            } else {
                parent.join(target)
            };
            let target =
                harkness_git::canonicalize_with_missing_tail(&target).map_err(|error| {
                    BoundaryError::CandidateUnavailable {
                        candidate: candidate.to_path_buf(),
                        reason: error.to_string(),
                    }
                })?;
            if !self.contains_resolved(&target) {
                return Ok(Some((reached, target)));
            }
            reached = target;
        }
        Ok(None)
    }
}

pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, BoundaryError> {
    let canonical = fs::canonicalize(root).map_err(|source| BoundaryError::RootUnavailable {
        root: root.to_path_buf(),
        reason: source.to_string(),
    })?;
    if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(BoundaryError::RootUnavailable {
            root: root.to_path_buf(),
            reason: "it is not a directory".to_owned(),
        });
    }
    Ok(canonical)
}

/// A filesystem-boundary refusal.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum BoundaryError {
    /// A resolved path lies outside every allowed root.
    #[error("{} resolves outside the allowed roots {roots:?}", .candidate.display())]
    OutsideAllowedRoots {
        /// Path as the caller supplied it.
        candidate: PathBuf,
        /// Canonical roots the check admitted.
        roots: Vec<PathBuf>,
    },
    /// A symlink inside an allowed root points outside every allowed root.
    #[error("symlink {} resolves outside the allowed roots to {}", .link.display(), .target.display())]
    SymlinkEscapes {
        /// Symlink reached from inside an allowed root.
        link: PathBuf,
        /// Canonical target outside the roots.
        target: PathBuf,
    },
    /// One configured root could not be canonicalized as a directory.
    #[error("allowed root {} is unavailable: {reason}", .root.display())]
    RootUnavailable {
        /// Root that was refused.
        root: PathBuf,
        /// Filesystem explanation.
        reason: String,
    },
    /// A candidate failed to resolve for a reason other than absence.
    #[error("candidate {} is unavailable: {reason}", .candidate.display())]
    CandidateUnavailable {
        /// Path as the caller supplied it.
        candidate: PathBuf,
        /// Filesystem explanation.
        reason: String,
    },
}

impl BoundaryError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "outside_allowed_roots",
        "symlink_escapes",
        "root_unavailable",
        "candidate_unavailable",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::OutsideAllowedRoots { .. } => "outside_allowed_roots",
            Self::SymlinkEscapes { .. } => "symlink_escapes",
            Self::RootUnavailable { .. } => "root_unavailable",
            Self::CandidateUnavailable { .. } => "candidate_unavailable",
        }
    }
}

/// Exact environment inherited by one non-Git child process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowlistedEnv(BTreeMap<OsString, OsString>);

impl AllowlistedEnv {
    /// Copies the baseline and this tool's published environment grants.
    #[must_use]
    pub fn for_descriptor(descriptor: &ToolDescriptor) -> Self {
        Self::build(descriptor.environment())
    }

    /// Copies present baseline variables and descriptor-declared extras from
    /// the parent. Every other parent variable is absent from the child.
    #[must_use]
    pub(crate) fn build<'a>(
        declared_extras: impl IntoIterator<Item = &'a EnvironmentName>,
    ) -> Self {
        let declared = declared_extras
            .into_iter()
            .map(EnvironmentName::as_str)
            .chain(BASELINE_ENVIRONMENT)
            .collect::<BTreeSet<_>>();
        let values = declared
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
            .collect();
        Self(values)
    }

    /// Replaces explicitly allowed values for one concrete process request.
    ///
    /// The fixed baseline is always eligible. Every other name must have been
    /// published by the tool descriptor, so an input map cannot enlarge the
    /// environment authority hidden behind an already-approved tool identity.
    /// Names are validated and canonicalized exactly as descriptor names are.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentOverrideError::InvalidName`] for a malformed key,
    /// [`EnvironmentOverrideError::Undeclared`] for a valid but ungranted name,
    /// and [`EnvironmentOverrideError::InvalidValue`] for a value containing a
    /// NUL byte that no operating-system process environment can represent.
    pub fn apply_overrides<'a>(
        &mut self,
        declared_extras: impl IntoIterator<Item = &'a EnvironmentName>,
        overrides: &BTreeMap<String, String>,
    ) -> Result<(), EnvironmentOverrideError> {
        let allowed = declared_extras
            .into_iter()
            .map(EnvironmentName::as_str)
            .chain(BASELINE_ENVIRONMENT)
            .collect::<BTreeSet<_>>();
        for (supplied, value) in overrides {
            let name = EnvironmentName::new(supplied.clone())
                .map_err(EnvironmentOverrideError::InvalidName)?;
            if !allowed.contains(name.as_str()) {
                return Err(EnvironmentOverrideError::Undeclared {
                    name: name.as_str().to_owned(),
                });
            }
            if value.contains('\0') {
                return Err(EnvironmentOverrideError::InvalidValue {
                    name: name.as_str().to_owned(),
                });
            }
            self.0
                .insert(OsString::from(name.as_str()), OsString::from(value));
        }
        Ok(())
    }

    /// Value copied for `name`, if the parent held it and it was allowed.
    #[must_use]
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.0.get(name.as_ref()).map(OsString::as_os_str)
    }

    /// Exact names and values to pass after `Command::env_clear`.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
    }

    /// Exact set of inherited names.
    pub fn names(&self) -> impl Iterator<Item = &OsStr> {
        self.0.keys().map(OsString::as_os_str)
    }
}

/// An input environment map that exceeds a tool's published allowlist.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvironmentOverrideError {
    /// A supplied key is not an environment identifier.
    #[error(transparent)]
    InvalidName(EnvironmentError),
    /// The tool descriptor did not grant this otherwise-valid name.
    #[error("environment variable {name} is not declared by this tool")]
    Undeclared {
        /// Canonical spelling that was refused.
        name: String,
    },
    /// Process environments cannot carry NUL bytes.
    #[error("environment variable {name} contains a NUL byte")]
    InvalidValue {
        /// Canonical spelling whose value was refused.
        name: String,
    },
}

impl EnvironmentOverrideError {
    /// Every stable discriminant this namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_environment_name",
        "undeclared_environment",
        "invalid_environment_value",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidName(error) => error.kind(),
            Self::Undeclared { .. } => "undeclared_environment",
            Self::InvalidValue { .. } => "invalid_environment_value",
        }
    }
}

/// An argv-only description of one child process.
///
/// There is deliberately no shell-string constructor. The executable and each
/// argument remain separate [`OsString`] values, the working directory has
/// already passed [`PathBoundary`], and the environment is exact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    cwd: ContainedPath,
    env: AllowlistedEnv,
}

impl CommandSpec {
    /// Builds a command from one executable and an explicit argv vector.
    ///
    /// A relative executable must be one bare name resolved by the allowlisted
    /// `PATH`. Relative paths containing separators are ambiguous once `cwd`
    /// is applied and are refused; callers can resolve one through a boundary
    /// and pass its resulting absolute path instead.
    ///
    /// # Errors
    ///
    /// Returns [`CommandSpecError::AmbiguousProgram`] for an empty or
    /// multi-component relative executable.
    pub fn new(
        program: impl AsRef<OsStr>,
        args: Vec<OsString>,
        cwd: ContainedPath,
        env: AllowlistedEnv,
    ) -> Result<Self, CommandSpecError> {
        let program = program.as_ref();
        let path = Path::new(program);
        let bare = !program.is_empty()
            && path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
            && path.components().count() == 1;
        if !path.is_absolute() && !bare {
            return Err(CommandSpecError::AmbiguousProgram {
                program: path.to_path_buf(),
            });
        }
        Ok(Self {
            program: program.to_os_string(),
            args,
            cwd,
            env,
        })
    }

    /// Executable passed directly to [`std::process::Command::new`].
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Arguments passed without shell interpolation.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Contained working directory.
    #[must_use]
    pub const fn cwd(&self) -> &ContainedPath {
        &self.cwd
    }

    /// Exact child environment.
    #[must_use]
    pub const fn env(&self) -> &AllowlistedEnv {
        &self.env
    }

    pub(crate) fn into_parts(self) -> (OsString, Vec<OsString>, ContainedPath, AllowlistedEnv) {
        (self.program, self.args, self.cwd, self.env)
    }
}

/// A process description that cannot be interpreted unambiguously.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandSpecError {
    /// A relative program was empty or contained a path separator.
    #[error("relative executable {} must be one bare program name", .program.display())]
    AmbiguousProgram {
        /// Refused executable spelling.
        program: PathBuf,
    },
}

impl CommandSpecError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &["ambiguous_program"];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::AmbiguousProgram { .. } => "ambiguous_program",
        }
    }
}

/// Whether a call may attach to an interactive terminal.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// The child may interact with a user through a terminal owned by the front
    /// end. Policy decides whether this mode is allowed.
    Interactive,
    /// No prompt can be answered; the safe default for recorded execution.
    #[default]
    NonInteractive,
}

/// Consequence one already-contained path has within a concrete request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    /// Reads without changing the path.
    Read,
    /// Writes workspace-visible, locally recoverable content.
    Write,
    /// Discards content that may not exist anywhere else.
    Destructive,
}

impl PathAccess {
    const fn risk(self) -> RiskLevel {
        match self {
            Self::Read => RiskLevel::Observe,
            Self::Write => RiskLevel::WorkspaceWrite,
            Self::Destructive => RiskLevel::Destructive,
        }
    }
}

/// One path input after containment and with its concrete access mode.
#[derive(Clone, Copy, Debug)]
pub struct RequestPath<'a> {
    path: &'a ContainedPath,
    access: PathAccess,
}

impl<'a> RequestPath<'a> {
    /// Associates an already-contained path with how this invocation uses it.
    #[must_use]
    pub const fn new(path: &'a ContainedPath, access: PathAccess) -> Self {
        Self { path, access }
    }

    /// Contained path being classified.
    #[must_use]
    pub const fn path(self) -> &'a ContainedPath {
        self.path
    }

    /// Concrete access requested for the path.
    #[must_use]
    pub const fn access(self) -> PathAccess {
        self.access
    }
}

/// Force variant a validated remote-write request selects.
///
/// Both variants overwrite history someone else may already have fetched;
/// `--force-with-lease` only narrows the window in which that happens. Policy
/// therefore treats them alike, and this enum exists so the audit trail can
/// still name which one was requested.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ForcePush {
    /// The request performs no force update.
    #[default]
    None,
    /// `--force-with-lease`: the remote is overwritten only if it still matches
    /// the ref the caller last observed.
    WithLease,
    /// Plain `--force`: the remote is overwritten unconditionally.
    Force,
}

impl ForcePush {
    /// Stable spelling used in reasons and persisted rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WithLease => "force_with_lease",
            Self::Force => "force",
        }
    }

    /// Whether this request overwrites remote history at all.
    #[must_use]
    pub const fn is_forcing(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Non-filesystem effects discovered in one concrete invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestFlags {
    executes: bool,
    network: bool,
    remote_write: bool,
    destructive: bool,
    force_push: ForcePush,
}

impl RequestFlags {
    /// Marks that the request starts arbitrary process execution.
    #[must_use]
    pub const fn executing(mut self) -> Self {
        self.executes = true;
        self
    }

    /// Marks that the request contacts a remote.
    #[must_use]
    pub const fn using_network(mut self) -> Self {
        self.network = true;
        self
    }

    /// Marks that the request mutates remote state.
    #[must_use]
    pub const fn writing_remote(mut self) -> Self {
        self.remote_write = true;
        self
    }

    /// Marks that the request discards unrecoverable state.
    #[must_use]
    pub const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Marks the force variant a validated remote-write request selects.
    ///
    /// Forcing implies remote write, so a caller cannot describe a force push
    /// as anything less consequential by omitting [`Self::writing_remote`].
    #[must_use]
    pub const fn force_pushing(mut self, variant: ForcePush) -> Self {
        self.force_push = variant;
        if variant.is_forcing() {
            self.remote_write = true;
        }
        self
    }

    /// Force variant this request selects, if any.
    #[must_use]
    pub const fn force_push(self) -> ForcePush {
        self.force_push
    }

    const fn risk(self) -> RiskLevel {
        if self.destructive {
            RiskLevel::Destructive
        } else if self.remote_write {
            RiskLevel::RemoteWrite
        } else if self.network {
            RiskLevel::Network
        } else if self.executes {
            RiskLevel::Execute
        } else {
            RiskLevel::Observe
        }
    }
}

/// What one concrete invocation was classified as.
///
/// The fields are private and there is no public constructor, so this value can
/// only come from [`classify_request`]. That is what stops a caller from
/// declaring a request less consequential — or less forceful — than the tool
/// descriptor and the validated input make it: policy consumes the
/// classification, never separately supplied risk and force flags.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestClassification {
    risk: RiskLevel,
    force_push: ForcePush,
}

impl RequestClassification {
    /// Effective risk, never below the descriptor's declared level.
    #[must_use]
    pub const fn risk(self) -> RiskLevel {
        self.risk
    }

    /// Force variant the validated input selected.
    #[must_use]
    pub const fn force_push(self) -> ForcePush {
        self.force_push
    }
}

/// Classifies one validated invocation without making an allow/deny decision.
///
/// The declared descriptor risk is a floor. Concrete path access and flags may
/// raise it and can never lower it. Paths arrive as [`ContainedPath`]
/// capabilities, so an outside write is refused before this function can be
/// called rather than misclassified as an ordinary workspace write.
#[must_use]
pub fn classify_request(
    descriptor: &ToolDescriptor,
    input_paths: &[RequestPath<'_>],
    flags: RequestFlags,
) -> RequestClassification {
    let risk = input_paths
        .iter()
        .map(|path| path.access.risk())
        .chain([descriptor.risk(), flags.risk()])
        .max()
        .unwrap_or(RiskLevel::Observe);
    RequestClassification {
        risk,
        force_push: flags.force_push(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;
    use time::macros::datetime;

    use super::*;
    #[cfg(unix)]
    use crate::domain::{RunId, StepId, ToolCallId};
    #[cfg(unix)]
    use crate::tool::{Capture, ToolProcess};
    use crate::tool::{ExecutionContext, Tool, ToolError, ToolIdentity, ToolMetadata, erase};

    fn boundary() -> (TempDir, PathBoundary) {
        let directory = TempDir::new().unwrap();
        let boundary = PathBoundary::new(directory.path(), std::iter::empty::<&Path>()).unwrap();
        (directory, boundary)
    }

    #[test]
    fn a_missing_leaf_inside_the_workspace_is_still_containable() {
        let (directory, boundary) = boundary();
        let contained = boundary.contain("new/subtree/file.txt").unwrap();
        assert_eq!(
            contained.as_path(),
            fs::canonicalize(directory.path())
                .unwrap()
                .join("new/subtree/file.txt")
        );
    }

    #[test]
    fn absolute_outside_paths_and_dot_dot_are_refused() {
        let (directory, boundary) = boundary();
        let outside = directory.path().parent().unwrap().join("outside.txt");
        for candidate in [outside, PathBuf::from("../outside.txt")] {
            let error = boundary.contain(&candidate).unwrap_err();
            assert_eq!(error.kind(), "outside_allowed_roots");
            assert!(matches!(error, BoundaryError::OutsideAllowedRoots { .. }));
        }
    }

    #[test]
    fn dot_dot_that_resolves_inside_the_workspace_is_accepted() {
        let (directory, boundary) = boundary();
        fs::create_dir(directory.path().join("subdirectory")).unwrap();
        assert_eq!(
            boundary
                .contain("subdirectory/../inside.txt")
                .unwrap()
                .as_path(),
            fs::canonicalize(directory.path())
                .unwrap()
                .join("inside.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_workspace_is_refused_by_name() {
        use std::os::unix::fs::symlink;

        let outside = TempDir::new().unwrap();
        let (directory, boundary) = boundary();
        let link = directory.path().join("escape");
        symlink(outside.path(), &link).unwrap();

        let error = boundary.contain("escape/secret.txt").unwrap_err();
        assert_eq!(error.kind(), "symlink_escapes");
        match error {
            BoundaryError::SymlinkEscapes {
                link: named,
                target,
            } => {
                assert_eq!(
                    named,
                    fs::canonicalize(directory.path()).unwrap().join("escape")
                );
                assert_eq!(target, fs::canonicalize(outside.path()).unwrap());
            }
            other => panic!("unexpected refusal: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_cannot_become_a_later_escape() {
        use std::os::unix::fs::symlink;

        let outside = TempDir::new().unwrap();
        let absent_target = outside.path().join("not-created");
        let (directory, boundary) = boundary();
        let link = directory.path().join("escape");
        symlink(&absent_target, &link).unwrap();

        let error = boundary.contain("escape/file.txt").unwrap_err();
        assert_eq!(error.kind(), "symlink_escapes");
        assert!(matches!(
            error,
            BoundaryError::SymlinkEscapes { link: named, target }
                if named == fs::canonicalize(directory.path()).unwrap().join("escape")
                    && target
                        == fs::canonicalize(outside.path()).unwrap().join("not-created")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_capability_is_refused_after_its_symlink_is_retargeted_outside() {
        use std::os::unix::fs::symlink;

        let outside = TempDir::new().unwrap();
        let (directory, boundary) = boundary();
        let inside = directory.path().join("inside");
        fs::create_dir(&inside).unwrap();
        let link = directory.path().join("current");
        symlink(&inside, &link).unwrap();
        let capability = boundary.contain("current").unwrap();

        fs::remove_file(&link).unwrap();
        symlink(outside.path(), &link).unwrap();

        assert!(matches!(
            capability.revalidate(),
            Err(BoundaryError::SymlinkEscapes { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_fails_closed_instead_of_becoming_a_missing_tail() {
        use std::os::unix::fs::symlink;

        let (directory, boundary) = boundary();
        symlink("loop", directory.path().join("loop")).unwrap();
        assert!(matches!(
            boundary.contain("loop/file"),
            Err(BoundaryError::CandidateUnavailable { .. })
        ));
    }

    #[test]
    fn explicit_extra_roots_are_containable() {
        let workspace = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let boundary = PathBoundary::new(workspace.path(), [extra.path()]).unwrap();
        let candidate = extra.path().join("granted.txt");
        assert_eq!(
            boundary.contain(&candidate).unwrap().as_path(),
            fs::canonicalize(extra.path()).unwrap().join("granted.txt")
        );
    }

    #[test]
    fn an_unavailable_root_is_refused_by_name() {
        let directory = TempDir::new().unwrap();
        let missing = directory.path().join("gone");
        let error = PathBoundary::new(&missing, std::iter::empty::<&Path>()).unwrap_err();
        assert_eq!(error.kind(), "root_unavailable");
        assert!(matches!(error, BoundaryError::RootUnavailable { root, .. } if root == missing));
    }

    #[test]
    fn unicode_and_spaced_paths_survive_containment_exactly() {
        let (directory, boundary) = boundary();
        let relative = Path::new("a spaced directory/資料.txt");
        let contained = boundary.contain(relative).unwrap();
        assert_eq!(
            contained.as_path(),
            fs::canonicalize(directory.path()).unwrap().join(relative)
        );
    }

    #[test]
    fn boundary_error_kinds_round_trip() {
        let errors = [
            BoundaryError::OutsideAllowedRoots {
                candidate: "outside".into(),
                roots: vec!["root".into()],
            },
            BoundaryError::SymlinkEscapes {
                link: "link".into(),
                target: "target".into(),
            },
            BoundaryError::RootUnavailable {
                root: "root".into(),
                reason: "gone".to_owned(),
            },
            BoundaryError::CandidateUnavailable {
                candidate: "candidate".into(),
                reason: "unreadable".to_owned(),
            },
        ];
        assert_eq!(
            errors.iter().map(BoundaryError::kind).collect::<Vec<_>>(),
            BoundaryError::KINDS
        );
    }

    #[test]
    fn trust_requires_both_the_project_identity_and_canonical_path() {
        let directory = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let project = ProjectId::new();
        let record = WorkspaceTrust::decide(
            project,
            directory.path(),
            TrustState::Trusted,
            datetime!(2026-08-11 12:00 UTC),
        )
        .unwrap();

        assert_eq!(
            record.resolve(project, directory.path()),
            TrustState::Trusted
        );
        assert_eq!(
            record.resolve(ProjectId::new(), directory.path()),
            TrustState::Untrusted
        );
        assert_eq!(
            record.resolve(project, elsewhere.path()),
            TrustState::Untrusted
        );
    }

    #[test]
    fn environment_names_are_exact_identifiers_not_patterns() {
        for invalid in ["", "*", "GIT_*", "9TOKEN", "A=B", "A-B"] {
            let error = EnvironmentName::new(invalid).unwrap_err();
            assert_eq!(error.kind(), "invalid_environment_name");
        }
        assert_eq!(
            EnvironmentName::new("HARKNESS_TOKEN_2").unwrap().as_str(),
            "HARKNESS_TOKEN_2"
        );
        assert_eq!(EnvironmentName::new("path").unwrap().as_str(), "PATH");
    }

    #[test]
    fn environment_overrides_cannot_enlarge_the_published_allowlist() {
        let declared = EnvironmentName::new("HARKNESS_ALLOWED").unwrap();
        let mut env = AllowlistedEnv::build([&declared]);
        env.apply_overrides(
            [&declared],
            &BTreeMap::from([("HARKNESS_ALLOWED".to_owned(), "value".to_owned())]),
        )
        .unwrap();
        assert_eq!(env.get("HARKNESS_ALLOWED"), Some(OsStr::new("value")));

        let error = env
            .apply_overrides(
                [&declared],
                &BTreeMap::from([("HARKNESS_SECRET".to_owned(), "leak".to_owned())]),
            )
            .unwrap_err();
        assert_eq!(error.kind(), "undeclared_environment");
        assert!(env.get("HARKNESS_SECRET").is_none());
    }

    #[test]
    fn environment_override_error_kinds_round_trip() {
        let errors = [
            EnvironmentOverrideError::InvalidName(EnvironmentName::new("NOT-VALID").unwrap_err()),
            EnvironmentOverrideError::Undeclared {
                name: "HARKNESS_SECRET".to_owned(),
            },
            EnvironmentOverrideError::InvalidValue {
                name: "PATH".to_owned(),
            },
        ];
        assert_eq!(
            errors.map(|error| error.kind()),
            EnvironmentOverrideError::KINDS
        );
    }

    #[test]
    fn relative_program_paths_with_separators_are_refused() {
        let (_directory, boundary) = boundary();
        let cwd = boundary.contain(".").unwrap();
        let env = AllowlistedEnv::build(std::iter::empty::<&EnvironmentName>());
        let error = CommandSpec::new("bin/tool", Vec::new(), cwd, env).unwrap_err();
        assert_eq!(error.kind(), "ambiguous_program");
    }

    #[cfg(unix)]
    #[test]
    fn an_allowlisted_child_sees_exactly_the_permitted_environment() {
        let extra = EnvironmentName::new("CARGO_MANIFEST_DIR").unwrap();
        assert!(std::env::var_os(extra.as_str()).is_some());
        let tool = erase(FixtureEnvTool(extra.clone())).unwrap();
        let env = AllowlistedEnv::for_descriptor(tool.descriptor());
        let expected = env
            .names()
            .map(OsStr::to_os_string)
            .collect::<BTreeSet<_>>();

        let (workspace, boundary) = boundary();
        let cwd = boundary.contain(".").unwrap();
        let spec = CommandSpec::new("/usr/bin/env", Vec::new(), cwd, env).unwrap();
        let mut context = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path().to_path_buf(),
        )
        .unwrap();
        let output = ToolProcess::new(spec)
            .capture_stdout(Capture::Tail)
            .run(&mut context)
            .unwrap()
            .require_success()
            .unwrap();
        let actual = output
            .stdout()
            .tail()
            .lines()
            .filter_map(|entry| entry.split_once('=').map(|(name, _)| OsString::from(name)))
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert!(actual.contains(OsStr::new(extra.as_str())));
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Input {}

    #[derive(JsonSchema, Serialize)]
    struct Output {}

    struct FixtureTool(RiskLevel);

    #[cfg(unix)]
    struct FixtureEnvTool(EnvironmentName);

    #[cfg(unix)]
    impl Tool for FixtureEnvTool {
        type Input = Input;
        type Output = Output;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.environment", "1.0.0").unwrap(),
                "Environment",
                "Exercises the published environment contract.",
                RiskLevel::Execute,
            )
            .with_environment([self.0.clone()])
        }

        fn execute(
            &self,
            _input: Input,
            _context: &mut ExecutionContext,
        ) -> Result<Output, ToolError> {
            Ok(Output {})
        }
    }

    impl Tool for FixtureTool {
        type Input = Input;
        type Output = Output;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.classify", "1.0.0").unwrap(),
                "Classify",
                "Classifies one fixture request.",
                self.0,
            )
        }

        fn execute(
            &self,
            _input: Input,
            _context: &mut ExecutionContext,
        ) -> Result<Output, ToolError> {
            Ok(Output {})
        }
    }

    fn descriptor(risk: RiskLevel) -> std::sync::Arc<dyn crate::tool::ErasedTool> {
        erase(FixtureTool(risk)).unwrap()
    }

    #[test]
    fn concrete_requests_cover_all_six_risk_levels() {
        let (_directory, boundary) = boundary();
        let path = boundary.contain("file.txt").unwrap();
        let observe = descriptor(RiskLevel::Observe);

        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[RequestPath::new(&path, PathAccess::Read)],
                RequestFlags::default(),
            )
            .risk(),
            RiskLevel::Observe
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[RequestPath::new(&path, PathAccess::Write)],
                RequestFlags::default(),
            )
            .risk(),
            RiskLevel::WorkspaceWrite
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().executing()
            )
            .risk(),
            RiskLevel::Execute
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().using_network()
            )
            .risk(),
            RiskLevel::Network
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().writing_remote()
            )
            .risk(),
            RiskLevel::RemoteWrite
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[RequestPath::new(&path, PathAccess::Destructive)],
                RequestFlags::default(),
            )
            .risk(),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn concrete_classification_can_raise_but_never_lower_the_descriptor() {
        let declared = descriptor(RiskLevel::Network);
        assert_eq!(
            classify_request(
                declared.descriptor(),
                &[],
                RequestFlags::default().executing()
            )
            .risk(),
            RiskLevel::Network
        );
        assert_eq!(
            classify_request(
                declared.descriptor(),
                &[],
                RequestFlags::default().destructive()
            )
            .risk(),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn every_force_variant_is_carried_and_implies_remote_write() {
        let observe = descriptor(RiskLevel::Observe);
        for variant in [ForcePush::WithLease, ForcePush::Force] {
            let classification = classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().force_pushing(variant),
            );
            assert_eq!(classification.force_push(), variant);
            assert!(variant.is_forcing());
            assert_eq!(
                classification.risk(),
                RiskLevel::RemoteWrite,
                "{variant:?} must classify at least as a remote write"
            );
        }

        let plain = classify_request(observe.descriptor(), &[], RequestFlags::default());
        assert_eq!(plain.force_push(), ForcePush::None);
        assert!(!ForcePush::None.is_forcing());
        assert_eq!(plain.risk(), RiskLevel::Observe);
    }

    #[test]
    fn a_destructive_force_push_keeps_the_higher_risk_and_the_variant() {
        let declared = descriptor(RiskLevel::Destructive);
        let classification = classify_request(
            declared.descriptor(),
            &[],
            RequestFlags::default().force_pushing(ForcePush::Force),
        );
        assert_eq!(classification.risk(), RiskLevel::Destructive);
        assert_eq!(classification.force_push(), ForcePush::Force);
    }
}
