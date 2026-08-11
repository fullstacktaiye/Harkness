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
use std::path::{Component, Path, PathBuf};

use harkness_core::ProjectId;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};

use crate::tool::{RiskLevel, ToolDescriptor};

/// Variables an arbitrary tool child may inherit without an extra declaration.
pub const BASELINE_ENVIRONMENT: [&str; 5] = ["PATH", "HOME", "LANG", "LC_ALL", "TERM"];

/// Longest accepted environment-variable declaration.
pub const MAX_ENVIRONMENT_NAME_LENGTH: usize = 128;

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
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContainedPath(PathBuf);

impl ContainedPath {
    /// Canonical path, including a lexically restored not-yet-existing tail.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consumes the capability and returns its canonical path.
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for ContainedPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
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
    /// restored lexically, so a destination that will be created or a file that
    /// was just deleted remains addressable. `..` is refused before resolution:
    /// lexically folding it would disagree with filesystem traversal when the
    /// preceding component is a symlink.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::OutsideAllowedRoots`] for a path outside every
    /// root, and [`BoundaryError::SymlinkEscapes`] when a symlink reached from
    /// an allowed root resolves outside them. The latter names both the link and
    /// its resolved target for the audit trail.
    pub fn contain(&self, candidate: impl AsRef<Path>) -> Result<ContainedPath, BoundaryError> {
        let supplied = candidate.as_ref();
        if supplied
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(self.outside(supplied));
        }

        let absolute = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            self.workspace_root.join(supplied)
        };
        let resolved =
            canonicalize_with_missing_tail(&absolute).ok_or_else(|| self.outside(supplied))?;
        if let Some((link, target)) = self.escaping_symlink(&absolute) {
            return Err(BoundaryError::SymlinkEscapes { link, target });
        }
        if self.contains_resolved(&resolved) {
            return Ok(ContainedPath(resolved));
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
    fn escaping_symlink(&self, candidate: &Path) -> Option<(PathBuf, PathBuf)> {
        let mut reached = PathBuf::new();
        for component in candidate.components() {
            reached.push(component.as_os_str());
            let metadata = fs::symlink_metadata(&reached).ok()?;
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let parent = reached.parent()?;
            if !self.contains_resolved(parent) {
                return None;
            }
            let target = fs::read_link(&reached).ok()?;
            let target = if target.is_absolute() {
                target
            } else {
                parent.join(target)
            };
            let target = canonicalize_with_missing_tail(&target)?;
            if !self.contains_resolved(&target) {
                return Some((reached, target));
            }
            reached = target;
        }
        None
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf, BoundaryError> {
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

/// Canonicalizes the nearest existing ancestor and restores the missing tail.
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(existing) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
        missing.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
}

/// A filesystem-boundary refusal.
#[derive(Debug, thiserror::Error)]
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
}

impl BoundaryError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "outside_allowed_roots",
        "symlink_escapes",
        "root_unavailable",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::OutsideAllowedRoots { .. } => "outside_allowed_roots",
            Self::SymlinkEscapes { .. } => "symlink_escapes",
            Self::RootUnavailable { .. } => "root_unavailable",
        }
    }
}

/// A validated environment-variable name a tool descriptor may request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EnvironmentName(String);

impl EnvironmentName {
    /// Validates an ASCII process-environment name with no wildcard syntax.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentError::InvalidName`] for an empty, overlong, or
    /// non-identifier spelling.
    pub fn new(name: impl Into<String>) -> Result<Self, EnvironmentError> {
        let name = name.into();
        let mut bytes = name.bytes();
        let first = bytes.next();
        let valid = first.is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
        let reason = if name.is_empty() {
            Some("it must not be empty")
        } else if name.len() > MAX_ENVIRONMENT_NAME_LENGTH {
            Some("it is longer than 128 bytes")
        } else if !valid {
            Some("it must match [A-Za-z_][A-Za-z0-9_]*")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(EnvironmentError::InvalidName { name, reason });
        }
        Ok(Self(name))
    }

    /// Validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A declaration that cannot become a process-environment name.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvironmentError {
    /// The spelling is not one exact environment identifier.
    #[error("{name:?} is not a valid environment variable name: {reason}")]
    InvalidName {
        /// Refused spelling.
        name: String,
        /// Stable validation explanation.
        reason: &'static str,
    },
}

impl EnvironmentError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &["invalid_environment_name"];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "invalid_environment_name",
        }
    }
}

/// Exact environment inherited by one non-Git child process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
    #[must_use]
    pub fn new(
        program: impl AsRef<OsStr>,
        args: Vec<OsString>,
        cwd: ContainedPath,
        env: AllowlistedEnv,
    ) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args,
            cwd,
            env,
        }
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

/// Non-filesystem effects discovered in one concrete invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestFlags {
    executes: bool,
    network: bool,
    remote_write: bool,
    destructive: bool,
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
) -> RiskLevel {
    input_paths
        .iter()
        .map(|path| path.access.risk())
        .chain([descriptor.risk(), flags.risk()])
        .max()
        .unwrap_or(RiskLevel::Observe)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::process::Command;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;
    use time::macros::datetime;

    use super::*;
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
                assert_eq!(named, link);
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
                if named == link && target == absent_target
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
    }

    #[cfg(unix)]
    #[test]
    fn an_allowlisted_child_sees_exactly_the_permitted_environment() {
        let extra = std::env::vars_os().find_map(|(name, _)| {
            let name = name.into_string().ok()?;
            (!BASELINE_ENVIRONMENT.contains(&name.as_str()))
                .then(|| EnvironmentName::new(name).ok())
                .flatten()
        });
        let env = AllowlistedEnv::build(extra.iter());
        let expected = env
            .names()
            .map(OsStr::to_os_string)
            .collect::<BTreeSet<_>>();

        let output = Command::new("/usr/bin/env")
            .arg("-0")
            .env_clear()
            .envs(env.iter())
            .output()
            .unwrap();
        assert!(output.status.success());
        let actual = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let equals = entry.iter().position(|byte| *byte == b'=').unwrap();
                OsString::from(String::from_utf8(entry[..equals].to_vec()).unwrap())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Input {}

    #[derive(JsonSchema, Serialize)]
    struct Output {}

    struct FixtureTool(RiskLevel);

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
            ),
            RiskLevel::Observe
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[RequestPath::new(&path, PathAccess::Write)],
                RequestFlags::default(),
            ),
            RiskLevel::WorkspaceWrite
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().executing()
            ),
            RiskLevel::Execute
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().using_network()
            ),
            RiskLevel::Network
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[],
                RequestFlags::default().writing_remote()
            ),
            RiskLevel::RemoteWrite
        );
        assert_eq!(
            classify_request(
                observe.descriptor(),
                &[RequestPath::new(&path, PathAccess::Destructive)],
                RequestFlags::default(),
            ),
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
            ),
            RiskLevel::Network
        );
        assert_eq!(
            classify_request(
                declared.descriptor(),
                &[],
                RequestFlags::default().destructive()
            ),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn runtime_process_construction_contains_no_shell_command_path() {
        let process = include_str!("tool/process.rs");
        assert!(!process.contains("Command::new(\"sh\")"));
        assert!(!process.contains(".arg(\"-c\")"));
        assert!(!process.contains(".args([\"-c\""));
    }
}
