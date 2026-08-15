//! Layered policy loading and pure tool-request evaluation.
//!
//! Policy files are read once at a run boundary. [`PolicyEngine::evaluate`]
//! performs no I/O, reads no clock, and takes no lock: identical loaded policy
//! and request values always produce the same decision.
//!
//! Three inputs to a decision are attacker-influenced, and each is narrowed to
//! something that cannot be asserted:
//!
//! - **What the request is.** [`PolicyRequest`] carries a
//!   [`RequestClassification`], not a risk level and a force-push flag, and
//!   floors it at the descriptor's declared risk. Understating a request raises
//!   no privilege.
//! - **Whether an approval exists.** [`RunGrant`] has no public constructor;
//!   only #92's matcher may decide a grant applies to a candidate.
//! - **What the repository says.** `.harkness/policy.json` is repository
//!   content, so it is resolved through the workspace boundary, refused if it
//!   is not a regular file inside the workspace, and bounded in size before it
//!   is read. Its rules can then only tighten a verdict, never weaken one.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::integration::IntegrationIdentity;
use crate::tool::{RiskLevel, ToolDescriptor, ToolId};
use crate::trust::{
    ContainedPath, ExecutionMode, ForcePush, PathBoundary, RequestClassification, TrustState,
};

mod integration;

use integration::ExternalPolicyContextWire;

pub use integration::{
    AcpPermissionOption, EXTERNAL_POLICY_CONTEXT_SCHEMA_VERSION, EXTERNAL_POLICY_DENIAL_KINDS,
    ExternalCapability, ExternalPermissionContext, ExternalPolicyContext, McpToolAnnotations,
};

/// Current version of `policy.json`.
pub const POLICY_SCHEMA_VERSION: u32 = 2;
/// Oldest `policy.json` version this build can read.
pub const MINIMUM_POLICY_SCHEMA_VERSION: u32 = 1;
/// Name of the global policy file below the Harkness data directory.
pub const USER_POLICY_FILE: &str = "policy.json";
/// Repository-relative policy path.
pub const REPOSITORY_POLICY_FILE: &str = ".harkness/policy.json";
/// Largest policy file this build will read into memory.
///
/// Rules are a handful of short keys; anything larger is a mistake or an
/// attempt to make a load allocate without bound.
pub const MAX_POLICY_FILE_BYTES: u64 = 64 * 1024;

/// Policy severity, ordered so `max` is the safe way to combine layers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// The request may proceed without another decision.
    Allow,
    /// A matching durable approval is required before execution.
    Ask,
    /// The request must not execute.
    Deny,
}

impl PolicyVerdict {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Layer responsible for the binding verdict.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// Compiled-in safety defaults or a hard built-in refusal.
    BuiltIn,
    /// Global user policy below the Harkness data directory.
    UserPolicy,
    /// Tightening repository policy.
    RepositoryPolicy,
    /// A live grant already matched to this exact candidate request.
    RunGrant,
}

impl PolicySource {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::UserPolicy => "user_policy",
            Self::RepositoryPolicy => "repository_policy",
            Self::RunGrant => "run_grant",
        }
    }
}

/// Complete inspectable result of one policy evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecision {
    verdict: PolicyVerdict,
    reason: String,
    source: PolicySource,
    #[serde(default, skip_serializing_if = "is_false")]
    one_call_only: bool,
    /// External request facts evaluated, absent for pre-v0.5 and local tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external_request: Option<ExternalPolicyContext>,
    /// Stable machine-readable refusal, present for typed external denials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    denial_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDecisionStrict {
    verdict: PolicyVerdict,
    reason: String,
    source: PolicySource,
    #[serde(default)]
    one_call_only: bool,
    #[serde(default)]
    external_request: Option<ExternalPolicyContextWire>,
    #[serde(default)]
    denial_kind: Option<String>,
}

impl<'de> Deserialize<'de> for PolicyDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decision = PolicyDecisionStrict::deserialize(deserializer)?;
        Ok(Self {
            verdict: decision.verdict,
            reason: decision.reason,
            source: decision.source,
            one_call_only: decision.one_call_only,
            external_request: decision.external_request.map(|context| context.0),
            denial_kind: decision.denial_kind,
        })
    }
}

impl PolicyDecision {
    fn new(
        verdict: PolicyVerdict,
        reason: impl Into<String>,
        source: PolicySource,
        one_call_only: bool,
    ) -> Self {
        let reason = reason.into();
        debug_assert!(!reason.trim().is_empty());
        Self {
            verdict,
            reason,
            source,
            one_call_only,
            external_request: None,
            denial_kind: None,
        }
    }

    fn for_request(mut self, request: &PolicyRequest<'_>) -> Self {
        self.external_request = request.external;
        self
    }

    fn denied_as(mut self, kind: &'static str) -> Self {
        debug_assert_eq!(self.verdict, PolicyVerdict::Deny);
        self.denial_kind = Some(kind.to_owned());
        self
    }

    /// Binding verdict.
    #[must_use]
    pub const fn verdict(&self) -> PolicyVerdict {
        self.verdict
    }

    /// Human-readable explanation suitable for an audit or approval prompt.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Layer responsible for the binding verdict.
    #[must_use]
    pub const fn source(&self) -> PolicySource {
        self.source
    }

    /// Whether an approval may cover only this exact call.
    #[must_use]
    pub const fn one_call_only(&self) -> bool {
        self.one_call_only
    }

    /// External-integration facts evaluated, when this was an external request.
    #[must_use]
    pub const fn external_request(&self) -> Option<&ExternalPolicyContext> {
        self.external_request.as_ref()
    }

    /// Stable refusal kind for a typed external denial.
    #[must_use]
    pub fn denial_kind(&self) -> Option<&str> {
        self.denial_kind.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.reason.trim().is_empty() {
            return Err("policy decisions require a non-empty reason");
        }
        if self.one_call_only && self.verdict != PolicyVerdict::Ask {
            return Err("only an approval request may be marked one-call-only");
        }
        if self.source == PolicySource::RunGrant && self.verdict != PolicyVerdict::Allow {
            return Err("a run grant may only produce an allow decision");
        }
        if self.denial_kind.is_some() && self.verdict != PolicyVerdict::Deny {
            return Err("only a denial may carry a denial kind");
        }
        if let Some(kind) = self.denial_kind.as_deref()
            && !EXTERNAL_POLICY_DENIAL_KINDS.contains(&kind)
        {
            return Err("policy decision carries an unknown external denial kind");
        }
        if let Some(external) = self.external_request.as_ref() {
            if external.schema_version() != EXTERNAL_POLICY_CONTEXT_SCHEMA_VERSION {
                return Err("external policy context was written by a newer Harkness build");
            }
            if external.validate_identity_shape().is_err() {
                if !(self.verdict == PolicyVerdict::Deny
                    && self.denial_kind.as_deref() == external.invalid_identity_denial_kind())
                {
                    return Err(
                        "external policy context carries missing or irrelevant identity evidence",
                    );
                }
            } else if let Some(kind) = self.denial_kind.as_deref()
                && kind != external.capability().noninteractive_denial_kind()
                && kind != "external_identity_context_invalid"
            {
                return Err("external denial kind does not match the evaluated request");
            }
        } else if self.denial_kind.is_some()
            && self.denial_kind.as_deref() != Some("external_identity_context_invalid")
        {
            return Err("an external denial kind requires an external request context");
        }
        Ok(())
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Scope of a grant after #92's exact-request matcher has accepted it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunGrantScope {
    /// The exact tool call, including its canonical input.
    ExactCall,
    /// This tool for the remainder of the run.
    ToolForRun,
    /// One declared capability for the remainder of the run.
    CapabilityForRun,
}

/// Narrow projection of a live grant already matched by the approval module.
///
/// This type deliberately carries no matcher inputs. #92 owns the durable grant,
/// lifecycle, workspace/run binding, and input hash; policy consumes only the
/// fact that the matcher accepted it and its effective scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RunGrant {
    scope: RunGrantScope,
    integration_identity: IntegrationIdentity,
}

impl RunGrant {
    /// Projects one live, matching approval grant into policy evaluation.
    ///
    /// Deliberately crate-private: a grant is an authorization, and only the
    /// approval module's matcher may decide that one applies to a candidate
    /// request. If any caller could mint one, every `Ask` would be one line of
    /// code away from `Allow`.
    ///
    /// [`ApprovalGrant::matching`](crate::approval::ApprovalGrant::matching) is
    /// the one production caller, and it reaches this only after every binding
    /// axis of the durable grant matched the candidate.
    #[must_use]
    pub(crate) const fn matching(
        scope: RunGrantScope,
        integration_identity: IntegrationIdentity,
    ) -> Self {
        Self {
            scope,
            integration_identity,
        }
    }

    /// Effective scope after approval scope ceilings were applied.
    #[must_use]
    pub const fn scope(self) -> RunGrantScope {
        self.scope
    }

    /// External identity evidence the approval matcher accepted.
    #[must_use]
    pub const fn integration_identity(self) -> IntegrationIdentity {
        self.integration_identity
    }
}

/// Concrete facts policy consumes after validation and boundary checking.
///
/// Every field is private, and the two a caller could use to understate a
/// request — its risk and its force-push variant — are not fields at all. They
/// are read from a [`RequestClassification`], which only
/// [`classify_request`](crate::trust::classify_request) can produce, and
/// [`Self::risk`] additionally floors the result at the descriptor's declared
/// level so a classification built against a different tool cannot lower it.
#[derive(Clone, Copy, Debug)]
pub struct PolicyRequest<'a> {
    descriptor: &'a ToolDescriptor,
    classification: RequestClassification,
    trust: TrustState,
    mode: ExecutionMode,
    paths: &'a [ContainedPath],
    grants: &'a [RunGrant],
    external: Option<ExternalPolicyContext>,
}

impl<'a> PolicyRequest<'a> {
    /// Builds a request from a published descriptor and its classification.
    ///
    /// Paths and grants default to empty; add them with [`Self::with_paths`]
    /// and [`Self::with_grants`].
    #[must_use]
    pub const fn new(
        descriptor: &'a ToolDescriptor,
        classification: RequestClassification,
        trust: TrustState,
        mode: ExecutionMode,
    ) -> Self {
        Self {
            descriptor,
            classification,
            trust,
            mode,
            paths: &[],
            grants: &[],
            external: None,
        }
    }

    /// Attaches every filesystem input, already checked by the boundary.
    #[must_use]
    pub const fn with_paths(mut self, paths: &'a [ContainedPath]) -> Self {
        self.paths = paths;
        self
    }

    /// Attaches grants the approval module already matched to this candidate.
    #[must_use]
    pub const fn with_grants(mut self, grants: &'a [RunGrant]) -> Self {
        self.grants = grants;
        self
    }

    /// Attaches the external-integration subject and identity being evaluated.
    #[must_use]
    #[allow(
        dead_code,
        reason = "reserved for runtime-owned integration coordination"
    )]
    pub(super) const fn with_external_context(mut self, external: ExternalPolicyContext) -> Self {
        self.external = Some(external);
        self
    }

    /// Immutable published tool contract.
    #[must_use]
    pub const fn descriptor(&self) -> &'a ToolDescriptor {
        self.descriptor
    }

    /// Effective risk: the classification, floored at the declared risk.
    ///
    /// A tool may never be evaluated below the level its descriptor publishes,
    /// so an understated classification raises no privilege.
    #[must_use]
    pub fn risk(&self) -> RiskLevel {
        let base = self.classification.risk().max(self.descriptor.risk());
        self.external
            .map_or(base, |external| base.max(external.risk_floor()))
    }

    /// Force variant the validated input selected, if any.
    #[must_use]
    pub const fn force_push(&self) -> ForcePush {
        self.classification.force_push()
    }

    /// Trust resolved for this project identity and canonical workspace root.
    #[must_use]
    pub const fn trust(&self) -> TrustState {
        self.trust
    }

    /// Whether a human can answer a new prompt.
    #[must_use]
    pub const fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Every filesystem input, already checked by the workspace boundary.
    #[must_use]
    pub const fn paths(&self) -> &'a [ContainedPath] {
        self.paths
    }

    /// Live grants already matched to this candidate by the approval module.
    #[must_use]
    pub const fn grants(&self) -> &'a [RunGrant] {
        self.grants
    }

    /// External-integration context, when this call crosses that boundary.
    #[must_use]
    pub const fn external(&self) -> Option<ExternalPolicyContext> {
        self.external
    }
}

/// Versioned policy rules shared by global and repository files.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    risks: BTreeMap<RiskLevel, PolicyVerdict>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tools: BTreeMap<String, PolicyVerdict>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    external_capabilities: BTreeMap<ExternalCapability, PolicyVerdict>,
}

impl PolicyFile {
    fn empty() -> Self {
        Self {
            version: POLICY_SCHEMA_VERSION,
            risks: BTreeMap::new(),
            tools: BTreeMap::new(),
            external_capabilities: BTreeMap::new(),
        }
    }

    /// The strictest rule in this file that matches the request.
    ///
    /// A tool rule and a risk rule can both match. They are combined with the
    /// same `max` the layers use rather than letting the more specific one win:
    /// a file that denies a risk level has denied it, and a permissive rule for
    /// one tool inside the same file must not carve an exception out of it.
    fn selected(&self, request: &PolicyRequest<'_>) -> Option<(PolicyVerdict, String)> {
        let risk = request.risk();
        let tool = self
            .tools
            .get(request.descriptor().id().as_str())
            .copied()
            .map(|verdict| {
                (
                    verdict,
                    format!("tool {}", request.descriptor().id().as_str()),
                )
            });
        let risk = self
            .risks
            .get(&risk)
            .copied()
            .map(|verdict| (verdict, format!("risk {}", risk.as_str())));
        let external = request.external().and_then(|context| {
            self.external_capabilities
                .get(&context.capability())
                .copied()
                .map(|verdict| {
                    (
                        verdict,
                        format!("external capability {}", context.capability().as_str()),
                    )
                })
        });
        [tool, risk, external]
            .into_iter()
            .flatten()
            .max_by_key(|selected| selected.0)
    }

    fn validate(&self, path: &Path) -> Result<(), PolicyLoadError> {
        if self.version < MINIMUM_POLICY_SCHEMA_VERSION {
            return Err(PolicyLoadError::Malformed {
                path: path.to_path_buf(),
                reason: format!(
                    "unsupported policy version {}; the minimum supported version is {}",
                    self.version, MINIMUM_POLICY_SCHEMA_VERSION
                ),
            });
        }
        if self.version == 1 && !self.external_capabilities.is_empty() {
            return Err(PolicyLoadError::Malformed {
                path: path.to_path_buf(),
                reason: "policy version 1 cannot carry external capability rules".to_owned(),
            });
        }
        for id in self.tools.keys() {
            id.parse::<ToolId>()
                .map_err(|error| PolicyLoadError::Malformed {
                    path: path.to_path_buf(),
                    reason: format!("invalid tool policy key {id:?}: {error}"),
                })?;
        }
        Ok(())
    }

    fn validate_repository(&self, path: &Path) -> Result<(), PolicyLoadError> {
        if let Some((capability, _)) = self
            .external_capabilities
            .iter()
            .find(|(_, verdict)| **verdict == PolicyVerdict::Allow)
        {
            return Err(PolicyLoadError::Malformed {
                path: path.to_path_buf(),
                reason: format!(
                    "repository policy may not grant allow for external capability {}",
                    capability.as_str()
                ),
            });
        }
        Ok(())
    }
}

/// Global user policy loaded once for a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserPolicy(PolicyFile);

impl Default for UserPolicy {
    fn default() -> Self {
        Self(PolicyFile::empty())
    }
}

impl UserPolicy {
    /// Adds or replaces a risk-level rule.
    #[must_use]
    pub fn with_risk(mut self, risk: RiskLevel, verdict: PolicyVerdict) -> Self {
        self.0.risks.insert(risk, verdict);
        self
    }

    /// Adds or replaces a tool-specific rule.
    #[must_use]
    pub fn with_tool(mut self, tool: &ToolId, verdict: PolicyVerdict) -> Self {
        self.0.tools.insert(tool.to_string(), verdict);
        self
    }

    /// Adds or replaces a rule for one typed external capability.
    #[must_use]
    pub fn with_external_capability(
        mut self,
        capability: ExternalCapability,
        verdict: PolicyVerdict,
    ) -> Self {
        self.0.external_capabilities.insert(capability, verdict);
        self
    }

    /// Loads a strict v1 file. Absence means no user overrides.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PolicyLoadError> {
        load_file(path.as_ref()).map(Self)
    }

    /// Atomically replaces the file with this policy.
    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), PolicyLoadError> {
        persist_file(path.as_ref(), &self.0)
    }
}

/// Tightening-only policy supplied by repository content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryPolicy(PolicyFile);

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self(PolicyFile::empty())
    }
}

impl RepositoryPolicy {
    /// Adds or replaces a risk-level rule.
    #[must_use]
    pub fn with_risk(mut self, risk: RiskLevel, verdict: PolicyVerdict) -> Self {
        self.0.risks.insert(risk, verdict);
        self
    }

    /// Adds or replaces a tool-specific rule.
    #[must_use]
    pub fn with_tool(mut self, tool: &ToolId, verdict: PolicyVerdict) -> Self {
        self.0.tools.insert(tool.to_string(), verdict);
        self
    }

    /// Loads a strict v1 file. Absence means no repository policy.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PolicyLoadError> {
        let path = path.as_ref();
        let policy = load_file(path)?;
        policy.validate_repository(path)?;
        Ok(Self(policy))
    }

    /// Atomically replaces the file with this policy.
    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), PolicyLoadError> {
        let path = path.as_ref();
        self.0.validate_repository(path)?;
        persist_file(path, &self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LoadedPolicy<T> {
    Loaded(T),
    Failed(PolicyLoadError),
}

/// Policy layers loaded once at the edge of a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyEngine {
    user: LoadedPolicy<UserPolicy>,
    repository: LoadedPolicy<Option<RepositoryPolicy>>,
}

impl PolicyEngine {
    /// Builds an engine from already loaded policy, with no possible I/O error.
    #[must_use]
    pub fn new(user: UserPolicy, repository: Option<RepositoryPolicy>) -> Self {
        Self {
            user: LoadedPolicy::Loaded(user),
            repository: LoadedPolicy::Loaded(repository),
        }
    }

    /// Loads `<data_dir>/policy.json` and `<workspace>/.harkness/policy.json`.
    ///
    /// Errors are retained rather than returned so evaluation fails closed with
    /// a persisted, human-readable policy decision. That includes a repository
    /// policy that does not resolve to a regular file inside the workspace: a
    /// symlink out of it is a refusal, not a file to read.
    #[must_use]
    pub fn load(data_dir: impl AsRef<Path>, workspace: impl AsRef<Path>) -> Self {
        let user_path = data_dir.as_ref().join(USER_POLICY_FILE);
        let user = match UserPolicy::load(&user_path) {
            Ok(policy) => LoadedPolicy::Loaded(policy),
            Err(error) => LoadedPolicy::Failed(error),
        };
        let repository = match load_repository_policy(workspace.as_ref()) {
            Ok(policy) => LoadedPolicy::Loaded(policy.map(RepositoryPolicy)),
            Err(error) => LoadedPolicy::Failed(error),
        };
        Self { user, repository }
    }

    /// Reuses the already-loaded user layer while loading repository policy
    /// from the workspace this run will actually execute in.
    #[must_use]
    pub fn for_workspace(&self, workspace: impl AsRef<Path>) -> Self {
        let repository = match load_repository_policy(workspace.as_ref()) {
            Ok(policy) => LoadedPolicy::Loaded(policy.map(RepositoryPolicy)),
            Err(error) => LoadedPolicy::Failed(error),
        };
        Self {
            user: self.user.clone(),
            repository,
        }
    }

    /// Evaluates a fully classified request without I/O, clock reads, or locks.
    ///
    /// Built-in, user, and repository rules combine with `max` on
    /// `Deny > Ask > Allow` — across layers *and* within one file, where a
    /// tool rule and a risk rule can both match. A repository can therefore
    /// tighten a decision and cannot weaken it. A live matching grant answers
    /// only `Ask`; it can never override `Deny`, and broad grants cannot answer
    /// remote-write or destructive requests.
    ///
    /// Every rule is selected against [`PolicyRequest::risk`], which is floored
    /// at the descriptor's declared risk, so no classification reaches a weaker
    /// rule than the tool's own contract allows. The force-push refusal is
    /// checked first and reads the same classification, so it cannot be
    /// sidestepped by any layer, any grant, or a descriptor that declares
    /// something milder.
    #[must_use]
    pub fn evaluate(&self, request: &PolicyRequest<'_>) -> PolicyDecision {
        let declared_external = request
            .descriptor()
            .capabilities()
            .iter()
            .filter_map(ExternalCapability::from_capability)
            .collect::<Vec<_>>();
        if declared_external.len() > 1 {
            return PolicyDecision::new(
                PolicyVerdict::Deny,
                "denied: a tool call must declare exactly one external operation",
                PolicySource::BuiltIn,
                false,
            )
            .for_request(request)
            .denied_as("external_identity_context_invalid");
        }
        if declared_external.len() == 1 && request.external().is_none() {
            return PolicyDecision::new(
                PolicyVerdict::Deny,
                format!(
                    "denied: {} requires external policy context",
                    declared_external[0].as_str()
                ),
                PolicySource::BuiltIn,
                false,
            )
            .for_request(request)
            .denied_as("external_identity_context_invalid");
        }
        if let Some(external) = request.external() {
            if let Err(reason) = external.validate_declaration(request.descriptor().capabilities())
            {
                return PolicyDecision::new(
                    PolicyVerdict::Deny,
                    format!("denied: {reason}"),
                    PolicySource::BuiltIn,
                    false,
                )
                .for_request(request)
                .denied_as("external_identity_context_invalid");
            }
            if let Some(kind) = external.invalid_identity_denial_kind() {
                return PolicyDecision::new(
                    PolicyVerdict::Deny,
                    format!(
                        "denied: {} requires valid observed identity evidence before evaluation",
                        external.capability().as_str()
                    ),
                    PolicySource::BuiltIn,
                    false,
                )
                .for_request(request)
                .denied_as(kind);
            }
        }

        let force_push = request.force_push();
        if force_push.is_forcing() {
            return PolicyDecision::new(
                PolicyVerdict::Deny,
                format!(
                    "denied: force push is not permitted in v0.3 ({})",
                    force_push.as_str()
                ),
                PolicySource::BuiltIn,
                false,
            )
            .for_request(request);
        }

        let user = match &self.user {
            LoadedPolicy::Loaded(policy) => policy,
            LoadedPolicy::Failed(error) => {
                return load_failure(error, PolicySource::UserPolicy).for_request(request);
            }
        };
        let repository = match &self.repository {
            LoadedPolicy::Loaded(policy) => policy.as_ref(),
            LoadedPolicy::Failed(error) => {
                return load_failure(error, PolicySource::RepositoryPolicy).for_request(request);
            }
        };

        let (mut verdict, mut source, mut reason) = built_in(request);
        fold_layer(
            &mut verdict,
            &mut source,
            &mut reason,
            user.0.selected(request),
            PolicySource::UserPolicy,
        );
        if let Some(repository) = repository {
            fold_layer(
                &mut verdict,
                &mut source,
                &mut reason,
                repository.0.selected(request),
                PolicySource::RepositoryPolicy,
            );
        }

        let exact_only = matches!(
            request.risk(),
            RiskLevel::RemoteWrite | RiskLevel::Destructive
        );
        let matching_grant = request.grants().iter().any(|grant| {
            (!exact_only || grant.scope == RunGrantScope::ExactCall)
                && request.external().map_or_else(
                    || grant.integration_identity().is_empty(),
                    |external| grant.integration_identity() == external.identity(),
                )
        });
        if verdict == PolicyVerdict::Ask && matching_grant {
            return PolicyDecision::new(
                PolicyVerdict::Allow,
                "allowed: a live run-scoped grant matches this request",
                PolicySource::RunGrant,
                false,
            )
            .for_request(request);
        }
        if verdict == PolicyVerdict::Ask && request.mode() == ExecutionMode::NonInteractive {
            let decision = PolicyDecision::new(
                PolicyVerdict::Deny,
                format!("denied: noninteractive execution cannot answer approval; {reason}"),
                source,
                false,
            )
            .for_request(request);
            return request.external().map_or(decision.clone(), |external| {
                decision.denied_as(external.capability().noninteractive_denial_kind())
            });
        }

        PolicyDecision::new(
            verdict,
            reason,
            source,
            verdict == PolicyVerdict::Ask && exact_only,
        )
        .for_request(request)
    }
}

fn built_in(request: &PolicyRequest<'_>) -> (PolicyVerdict, PolicySource, String) {
    let trust = request.trust();
    let risk = request.risk();
    let verdict = match (trust, risk) {
        (TrustState::Trusted, RiskLevel::Observe) => PolicyVerdict::Allow,
        (TrustState::Trusted, _) => PolicyVerdict::Ask,
        (TrustState::Untrusted, RiskLevel::Observe) => PolicyVerdict::Ask,
        (TrustState::Untrusted, _) => PolicyVerdict::Deny,
    };
    let reason = match (trust, verdict) {
        (TrustState::Untrusted, PolicyVerdict::Deny) => "denied: workspace is untrusted".to_owned(),
        (TrustState::Untrusted, PolicyVerdict::Ask) => {
            "workspace observation requires approval because the workspace is untrusted".to_owned()
        }
        (_, PolicyVerdict::Allow) => {
            format!("{risk} is allowed by the trusted-workspace default")
        }
        (_, PolicyVerdict::Ask) => format!(
            "{} requires approval (built-in default for {risk})",
            risk_label(risk),
        ),
        (_, PolicyVerdict::Deny) => unreachable!(),
    };
    (verdict, PolicySource::BuiltIn, reason)
}

fn risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Observe => "observation",
        RiskLevel::WorkspaceWrite => "workspace write",
        RiskLevel::Execute => "execution",
        RiskLevel::Network => "network access",
        RiskLevel::RemoteWrite => "remote write",
        RiskLevel::Destructive => "destructive work",
    }
}

fn fold_layer(
    verdict: &mut PolicyVerdict,
    source: &mut PolicySource,
    reason: &mut String,
    selected: Option<(PolicyVerdict, String)>,
    layer: PolicySource,
) {
    let Some((candidate, selector)) = selected else {
        return;
    };
    if candidate > *verdict {
        *verdict = candidate;
        *source = layer;
        *reason = format!(
            "{} by {} rule for {selector}",
            candidate.as_str(),
            layer.as_str()
        );
    }
}

fn load_failure(error: &PolicyLoadError, source: PolicySource) -> PolicyDecision {
    PolicyDecision::new(
        PolicyVerdict::Deny,
        format!("denied: {error}"),
        source,
        false,
    )
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

fn load_file(path: &Path) -> Result<PolicyFile, PolicyLoadError> {
    Ok(load_optional_file(path)?.unwrap_or_else(PolicyFile::empty))
}

/// Reads `<workspace>/.harkness/policy.json` through the workspace boundary.
///
/// The file is repository content, so its *name* is attacker-controlled even
/// when its bytes are reviewed: committing `.harkness/policy.json` as a symlink
/// would otherwise make Harkness read — and apply — a file the workspace does
/// not contain. Resolving through [`PathBoundary`] refuses that, and refuses it
/// as a load failure, so evaluation fails closed instead of silently falling
/// back to defaults.
fn load_repository_policy(workspace: &Path) -> Result<Option<PolicyFile>, PolicyLoadError> {
    let nominal = workspace.join(REPOSITORY_POLICY_FILE);
    let boundary = PathBoundary::new(workspace, std::iter::empty::<&Path>()).map_err(|error| {
        PolicyLoadError::Unreadable {
            path: nominal.clone(),
            reason: error.to_string(),
        }
    })?;
    let contained =
        boundary
            .contain(REPOSITORY_POLICY_FILE)
            .map_err(|error| PolicyLoadError::Unreadable {
                path: nominal,
                reason: error.to_string(),
            })?;
    let policy = load_optional_file(contained.as_path())?;
    if let Some(policy) = policy.as_ref() {
        policy.validate_repository(contained.as_path())?;
    }
    Ok(policy)
}

fn load_optional_file(path: &Path) -> Result<Option<PolicyFile>, PolicyLoadError> {
    let bytes = match read_bounded(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) => return Err(error),
    };
    let probe: VersionProbe =
        serde_json::from_slice(&bytes).map_err(|error| PolicyLoadError::Malformed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if probe.version > POLICY_SCHEMA_VERSION {
        return Err(PolicyLoadError::VersionTooNew {
            path: path.to_path_buf(),
            found: probe.version,
            maximum: POLICY_SCHEMA_VERSION,
        });
    }
    let mut policy: PolicyFile =
        serde_json::from_slice(&bytes).map_err(|error| PolicyLoadError::Malformed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    policy.validate(path)?;
    // Keep old files readable without rewriting them. Any later explicit
    // persistence writes the newest schema, so v1 can never acquire a v2-only
    // field while still claiming the old version.
    policy.version = POLICY_SCHEMA_VERSION;
    Ok(Some(policy))
}

/// Reads a policy file without letting its size choose this process's memory.
///
/// A missing file is `Ok(None)`; anything that is not a regular file, and
/// anything past [`MAX_POLICY_FILE_BYTES`], is refused. The cap is enforced on
/// the read itself rather than on the metadata that preceded it, so a file that
/// grows between the two checks is still bounded.
fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>, PolicyLoadError> {
    let unreadable = |reason: String| PolicyLoadError::Unreadable {
        path: path.to_path_buf(),
        reason,
    };
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error.to_string())),
    };
    if !metadata.is_file() {
        return Err(unreadable("it is not a regular file".to_owned()));
    }
    let file = fs::File::open(path).map_err(|error| unreadable(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_POLICY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(error.to_string()))?;
    if bytes.len() as u64 > MAX_POLICY_FILE_BYTES {
        return Err(unreadable(format!(
            "it exceeds the {MAX_POLICY_FILE_BYTES}-byte maximum policy size"
        )));
    }
    Ok(Some(bytes))
}

fn persist_file(path: &Path, policy: &PolicyFile) -> Result<(), PolicyLoadError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).map_err(|error| PolicyLoadError::Unreadable {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut temporary =
        NamedTempFile::new_in(directory).map_err(|error| PolicyLoadError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    serde_json::to_writer_pretty(&mut temporary, policy).map_err(|error| {
        PolicyLoadError::Malformed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }
    })?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|error| PolicyLoadError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    temporary
        .persist(path)
        .map_err(|error| PolicyLoadError::Unreadable {
            path: path.to_path_buf(),
            reason: error.error.to_string(),
        })?;
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| PolicyLoadError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    Ok(())
}

/// Typed failure while loading or persisting a policy file.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyLoadError {
    /// The path could not be read or atomically replaced.
    #[error("policy file {} is unreadable: {reason}", .path.display())]
    Unreadable {
        /// Policy path.
        path: PathBuf,
        /// Filesystem explanation.
        reason: String,
    },
    /// The current-version body is invalid or strict parsing failed.
    #[error("policy file {} is malformed: {reason}", .path.display())]
    Malformed {
        /// Policy path.
        path: PathBuf,
        /// Parse or validation explanation.
        reason: String,
    },
    /// The file requires a newer Harkness policy schema.
    #[error(
        "policy file {} uses version {found}, newer than supported version {maximum}; upgrade Harkness",
        .path.display()
    )]
    VersionTooNew {
        /// Policy path.
        path: PathBuf,
        /// Version found by the probe.
        found: u32,
        /// Newest version understood by this build.
        maximum: u32,
    },
}

impl PolicyLoadError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "policy_unreadable",
        "policy_malformed",
        "policy_version_too_new",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => "policy_unreadable",
            Self::Malformed { .. } => "policy_malformed",
            Self::VersionTooNew { .. } => "policy_version_too_new",
        }
    }

    /// Policy path named by the failure.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Unreadable { path, .. }
            | Self::Malformed { path, .. }
            | Self::VersionTooNew { path, .. } => path,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;
    use crate::integration::{IntegrationIdentity, Sha256Hash};
    use crate::tool::{ExecutionContext, Tool, ToolError, ToolIdentity, ToolMetadata, erase};
    use crate::trust::{RequestFlags, classify_request};

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct EmptyInput {}

    #[derive(JsonSchema, Serialize)]
    struct EmptyOutput {}

    struct FixtureTool(RiskLevel);

    impl Tool for FixtureTool {
        type Input = EmptyInput;
        type Output = EmptyOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.policy", "1.0.0").unwrap(),
                "Policy fixture",
                "Provides a descriptor for policy evaluator tests.",
                self.0,
            )
        }

        fn execute(
            &self,
            _input: Self::Input,
            _context: &mut ExecutionContext,
        ) -> Result<Self::Output, ToolError> {
            Ok(EmptyOutput {})
        }
    }

    struct ExternalFixtureTool(ExternalCapability, RiskLevel);

    impl Tool for ExternalFixtureTool {
        type Input = EmptyInput;
        type Output = EmptyOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.external", "1.0.0").unwrap(),
                "External policy fixture",
                "Provides an external descriptor for policy evaluator tests.",
                self.1,
            )
            .with_capabilities([self.0.capability().unwrap()])
        }

        fn execute(
            &self,
            _input: Self::Input,
            _context: &mut ExecutionContext,
        ) -> Result<Self::Output, ToolError> {
            Ok(EmptyOutput {})
        }
    }

    /// Enough of the repository policy path to identify it in a reason on any
    /// platform, since canonical spellings differ between them.
    const REPOSITORY_POLICY_FILE_NAME: &str = "policy.json";

    fn descriptor(risk: RiskLevel) -> ToolDescriptor {
        erase(FixtureTool(risk)).unwrap().descriptor().clone()
    }

    fn external_descriptor(capability: ExternalCapability, risk: RiskLevel) -> ToolDescriptor {
        erase(ExternalFixtureTool(capability, risk))
            .unwrap()
            .descriptor()
            .clone()
    }

    fn identity_for(capability: ExternalCapability) -> IntegrationIdentity {
        match capability {
            ExternalCapability::LaunchExternalAgent | ExternalCapability::ConnectMcpServer => {
                IntegrationIdentity::none()
                    .with_agent_executable_sha256(Sha256Hash::of("executable"))
            }
            ExternalCapability::InvokeMcpTool => IntegrationIdentity::none()
                .with_mcp_tool_schema_fingerprint(Sha256Hash::of("schema")),
            ExternalCapability::ExecuteWorkflowRecipe => {
                IntegrationIdentity::none().with_recipe_content_hash(Sha256Hash::of("recipe"))
            }
            _ => IntegrationIdentity::none(),
        }
    }

    fn external_context(capability: ExternalCapability) -> ExternalPolicyContext {
        match capability {
            ExternalCapability::LaunchExternalAgent => {
                ExternalPolicyContext::launch_external_agent(
                    identity_for(capability).agent_executable_sha256(),
                )
            }
            ExternalCapability::ConnectMcpServer => ExternalPolicyContext::connect_mcp_server(
                identity_for(capability).agent_executable_sha256(),
            ),
            ExternalCapability::InvokeMcpTool => ExternalPolicyContext::invoke_mcp_tool(
                declared(&descriptor(RiskLevel::Observe)),
                identity_for(capability).mcp_tool_schema_fingerprint(),
            ),
            ExternalCapability::ReadForgeResource => ExternalPolicyContext::read_forge_resource(),
            ExternalCapability::PushRemoteBranch => ExternalPolicyContext::push_remote_branch(),
            ExternalCapability::CreatePullRequest => ExternalPolicyContext::create_pull_request(),
            ExternalCapability::ModifyForgeResource => {
                ExternalPolicyContext::modify_forge_resource()
            }
            ExternalCapability::ExecuteWorkflowRecipe => {
                ExternalPolicyContext::execute_workflow_recipe(
                    [RiskLevel::Observe],
                    identity_for(capability).recipe_content_hash(),
                )
            }
        }
    }

    fn external_context_without_identity(capability: ExternalCapability) -> ExternalPolicyContext {
        match capability {
            ExternalCapability::LaunchExternalAgent => {
                ExternalPolicyContext::launch_external_agent(None)
            }
            ExternalCapability::ConnectMcpServer => ExternalPolicyContext::connect_mcp_server(None),
            ExternalCapability::InvokeMcpTool => ExternalPolicyContext::invoke_mcp_tool(
                declared(&descriptor(RiskLevel::Observe)),
                None,
            ),
            ExternalCapability::ExecuteWorkflowRecipe => {
                ExternalPolicyContext::execute_workflow_recipe([RiskLevel::Observe], None)
            }
            _ => external_context(capability),
        }
    }

    /// The classification a request with no extra paths or flags produces.
    fn declared(descriptor: &ToolDescriptor) -> RequestClassification {
        classify_request(descriptor, &[], RequestFlags::default())
    }

    fn request<'a>(
        descriptor: &'a ToolDescriptor,
        trust: TrustState,
        mode: ExecutionMode,
        grants: &'a [RunGrant],
    ) -> PolicyRequest<'a> {
        PolicyRequest::new(descriptor, declared(descriptor), trust, mode).with_grants(grants)
    }

    fn write_repository_policy(workspace: &Path, contents: &str) {
        let path = workspace.join(REPOSITORY_POLICY_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn repository_decision(workspace: &Path) -> PolicyDecision {
        let data = TempDir::new().unwrap();
        let engine = PolicyEngine::load(data.path(), workspace);
        let descriptor = descriptor(RiskLevel::Observe);
        engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ))
    }

    // -- built-in defaults and the severity lattice --------------------------

    #[test]
    fn built_in_table_covers_every_risk_and_trust_branch() {
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        for trust in [TrustState::Trusted, TrustState::Untrusted] {
            for risk in RiskLevel::ALL.iter().copied() {
                let descriptor = descriptor(risk);
                let decision = engine.evaluate(&request(
                    &descriptor,
                    trust,
                    ExecutionMode::Interactive,
                    &[],
                ));
                let expected = match (trust, risk) {
                    (TrustState::Trusted, RiskLevel::Observe) => PolicyVerdict::Allow,
                    (TrustState::Trusted, _) => PolicyVerdict::Ask,
                    (TrustState::Untrusted, RiskLevel::Observe) => PolicyVerdict::Ask,
                    (TrustState::Untrusted, _) => PolicyVerdict::Deny,
                };
                assert_eq!(decision.verdict(), expected, "{trust:?} x {risk:?}");
                assert!(!decision.reason().trim().is_empty());
            }
        }
    }

    #[test]
    fn repository_policy_can_raise_and_cannot_lower_a_verdict() {
        let observe = descriptor(RiskLevel::Observe);
        let engine = PolicyEngine::new(
            UserPolicy::default(),
            Some(RepositoryPolicy::default().with_risk(RiskLevel::Observe, PolicyVerdict::Ask)),
        );
        let raised = engine.evaluate(&request(
            &observe,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(raised.verdict(), PolicyVerdict::Ask);
        assert_eq!(raised.source(), PolicySource::RepositoryPolicy);

        let write = descriptor(RiskLevel::WorkspaceWrite);
        let engine = PolicyEngine::new(
            UserPolicy::default(),
            Some(
                RepositoryPolicy::default()
                    .with_risk(RiskLevel::WorkspaceWrite, PolicyVerdict::Allow),
            ),
        );
        let unchanged = engine.evaluate(&request(
            &write,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(unchanged.verdict(), PolicyVerdict::Ask);
        assert_eq!(unchanged.source(), PolicySource::BuiltIn);
    }

    #[test]
    fn user_and_tool_rules_participate_in_the_same_severity_lattice() {
        let descriptor = descriptor(RiskLevel::Observe);
        let engine = PolicyEngine::new(
            UserPolicy::default().with_tool(descriptor.id(), PolicyVerdict::Deny),
            None,
        );
        let decision = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::UserPolicy);
        assert!(decision.reason().contains("fixture.policy"));
    }

    #[test]
    fn a_tool_rule_never_weakens_a_risk_rule_in_the_same_file() {
        let descriptor = descriptor(RiskLevel::Observe);
        // The permissive tool rule is the more specific of the two, and is
        // still folded with `max` rather than shadowing the risk rule.
        let engine = PolicyEngine::new(
            UserPolicy::default()
                .with_risk(RiskLevel::Observe, PolicyVerdict::Deny)
                .with_tool(descriptor.id(), PolicyVerdict::Allow),
            None,
        );
        let decision = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert!(decision.reason().contains("risk observe"));

        // The reverse pairing is what makes the rule a `max` and not a
        // "risk rule always wins": a stricter tool rule still binds.
        let engine = PolicyEngine::new(
            UserPolicy::default()
                .with_risk(RiskLevel::Observe, PolicyVerdict::Ask)
                .with_tool(descriptor.id(), PolicyVerdict::Deny),
            None,
        );
        let decision = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert!(decision.reason().contains("tool fixture.policy"));
    }

    #[test]
    fn no_repository_policy_input_can_lower_any_verdict() {
        let verdicts = [
            None,
            Some(PolicyVerdict::Allow),
            Some(PolicyVerdict::Ask),
            Some(PolicyVerdict::Deny),
        ];
        let other_tool = "other.tool".parse::<ToolId>().unwrap();

        for risk in RiskLevel::ALL.iter().copied() {
            let descriptor = descriptor(risk);
            // A different risk level, so a rule that does not match this
            // request is exercised alongside the ones that do.
            let unrelated_risk = if risk == RiskLevel::Observe {
                RiskLevel::Destructive
            } else {
                RiskLevel::Observe
            };
            for user in [
                UserPolicy::default(),
                UserPolicy::default().with_risk(risk, PolicyVerdict::Allow),
                UserPolicy::default().with_tool(descriptor.id(), PolicyVerdict::Ask),
            ] {
                for trust in [TrustState::Trusted, TrustState::Untrusted] {
                    for mode in [ExecutionMode::Interactive, ExecutionMode::NonInteractive] {
                        let baseline = PolicyEngine::new(user.clone(), None).evaluate(&request(
                            &descriptor,
                            trust,
                            mode,
                            &[],
                        ));
                        for matching_risk in verdicts {
                            for unrelated in verdicts {
                                for matching_tool in verdicts {
                                    for unrelated_tool in verdicts {
                                        let mut repository = RepositoryPolicy::default();
                                        if let Some(verdict) = matching_risk {
                                            repository = repository.with_risk(risk, verdict);
                                        }
                                        if let Some(verdict) = unrelated {
                                            repository =
                                                repository.with_risk(unrelated_risk, verdict);
                                        }
                                        if let Some(verdict) = matching_tool {
                                            repository =
                                                repository.with_tool(descriptor.id(), verdict);
                                        }
                                        if let Some(verdict) = unrelated_tool {
                                            repository = repository.with_tool(&other_tool, verdict);
                                        }
                                        let decision =
                                            PolicyEngine::new(user.clone(), Some(repository))
                                                .evaluate(&request(&descriptor, trust, mode, &[]));
                                        assert!(
                                            decision.verdict() >= baseline.verdict(),
                                            "{risk:?}/{trust:?}/{mode:?} lowered \
                                             {:?} to {:?}",
                                            baseline.verdict(),
                                            decision.verdict(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // -- classification cannot understate a request --------------------------

    #[test]
    fn an_understated_classification_cannot_lower_the_declared_risk() {
        for declared_risk in RiskLevel::ALL.iter().copied() {
            let declared_descriptor = descriptor(declared_risk);
            let baseline = PolicyEngine::new(UserPolicy::default(), None).evaluate(&request(
                &declared_descriptor,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &[],
            ));
            for lower in RiskLevel::ALL
                .iter()
                .copied()
                .take_while(|risk| *risk < declared_risk)
            {
                let understated = declared(&descriptor(lower));
                let request = PolicyRequest::new(
                    &declared_descriptor,
                    understated,
                    TrustState::Trusted,
                    ExecutionMode::Interactive,
                );
                assert_eq!(
                    request.risk(),
                    declared_risk,
                    "{lower:?} must not lower {declared_risk:?}"
                );

                let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(&request);
                assert_eq!(decision.verdict(), baseline.verdict());
                assert_eq!(decision.one_call_only(), baseline.one_call_only());
            }
        }
    }

    #[test]
    fn an_understated_classification_keeps_the_one_call_only_ceiling() {
        // The specific downgrade that would otherwise buy both a weaker verdict
        // and a run-wide grant: a destructive tool classified as observation.
        let destructive = descriptor(RiskLevel::Destructive);
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        let understated = PolicyRequest::new(
            &destructive,
            declared(&descriptor(RiskLevel::Observe)),
            TrustState::Trusted,
            ExecutionMode::Interactive,
        );
        let decision = engine.evaluate(&understated);
        assert_eq!(decision.verdict(), PolicyVerdict::Ask);
        assert!(decision.one_call_only());

        let broad = [RunGrant::matching(
            RunGrantScope::ToolForRun,
            IntegrationIdentity::none(),
        )];
        let with_broad_grant = PolicyRequest::new(
            &destructive,
            declared(&descriptor(RiskLevel::Observe)),
            TrustState::Trusted,
            ExecutionMode::NonInteractive,
        )
        .with_grants(&broad);
        assert_eq!(
            engine.evaluate(&with_broad_grant).verdict(),
            PolicyVerdict::Deny
        );
    }

    #[test]
    fn an_understated_classification_cannot_escape_a_user_risk_rule() {
        let destructive = descriptor(RiskLevel::Destructive);
        let engine = PolicyEngine::new(
            UserPolicy::default().with_risk(RiskLevel::Destructive, PolicyVerdict::Deny),
            None,
        );
        let decision = engine.evaluate(&PolicyRequest::new(
            &destructive,
            declared(&descriptor(RiskLevel::Observe)),
            TrustState::Trusted,
            ExecutionMode::Interactive,
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::UserPolicy);
    }

    // -- force push ----------------------------------------------------------

    #[test]
    fn every_force_variant_is_a_non_overridable_built_in_denial() {
        let descriptor = descriptor(RiskLevel::RemoteWrite);
        let grants = [
            RunGrant::matching(RunGrantScope::ExactCall, IntegrationIdentity::none()),
            RunGrant::matching(RunGrantScope::ToolForRun, IntegrationIdentity::none()),
            RunGrant::matching(RunGrantScope::CapabilityForRun, IntegrationIdentity::none()),
        ];
        let engine = PolicyEngine::new(
            UserPolicy::default()
                .with_risk(RiskLevel::RemoteWrite, PolicyVerdict::Allow)
                .with_tool(descriptor.id(), PolicyVerdict::Allow),
            Some(
                RepositoryPolicy::default()
                    .with_risk(RiskLevel::RemoteWrite, PolicyVerdict::Allow)
                    .with_tool(descriptor.id(), PolicyVerdict::Allow),
            ),
        );

        for variant in [ForcePush::Force, ForcePush::WithLease] {
            for mode in [ExecutionMode::Interactive, ExecutionMode::NonInteractive] {
                let classification = classify_request(
                    &descriptor,
                    &[],
                    RequestFlags::default().force_pushing(variant),
                );
                let decision = engine.evaluate(
                    &PolicyRequest::new(&descriptor, classification, TrustState::Trusted, mode)
                        .with_grants(&grants),
                );
                assert_eq!(decision.verdict(), PolicyVerdict::Deny, "{variant:?}");
                assert_eq!(decision.source(), PolicySource::BuiltIn);
                assert!(decision.reason().contains("force push"));
                assert!(decision.reason().contains(variant.as_str()));
            }
        }
    }

    #[test]
    fn a_force_push_is_denied_even_when_its_descriptor_looks_harmless() {
        // The force variant travels with the classification, so a descriptor
        // that declares mere observation cannot hide one.
        let descriptor = descriptor(RiskLevel::Observe);
        let classification = classify_request(
            &descriptor,
            &[],
            RequestFlags::default().force_pushing(ForcePush::WithLease),
        );
        assert_eq!(classification.risk(), RiskLevel::RemoteWrite);

        let decision =
            PolicyEngine::new(UserPolicy::default(), None).evaluate(&PolicyRequest::new(
                &descriptor,
                classification,
                TrustState::Trusted,
                ExecutionMode::Interactive,
            ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::BuiltIn);
        assert!(decision.reason().contains("force push"));
    }

    // -- external-integration contracts --------------------------------------

    #[test]
    fn every_external_capability_has_the_normative_risk_floor() {
        for capability in ExternalCapability::ALL.iter().copied() {
            let descriptor = external_descriptor(capability, RiskLevel::Observe);
            let context = external_context(capability);
            let request = PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::Interactive,
            )
            .with_external_context(context);
            let expected = match capability {
                ExternalCapability::LaunchExternalAgent
                | ExternalCapability::ConnectMcpServer
                | ExternalCapability::InvokeMcpTool => RiskLevel::Execute,
                ExternalCapability::ReadForgeResource => RiskLevel::Network,
                ExternalCapability::PushRemoteBranch
                | ExternalCapability::CreatePullRequest
                | ExternalCapability::ModifyForgeResource => RiskLevel::RemoteWrite,
                ExternalCapability::ExecuteWorkflowRecipe => RiskLevel::Observe,
            };
            assert_eq!(request.risk(), expected, "{capability:?}");

            let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(&request);
            assert_eq!(decision.external_request(), Some(&context));
            assert_eq!(
                decision.one_call_only(),
                matches!(
                    capability,
                    ExternalCapability::PushRemoteBranch
                        | ExternalCapability::CreatePullRequest
                        | ExternalCapability::ModifyForgeResource
                ),
                "{capability:?}"
            );
        }
    }

    #[test]
    fn recipe_risk_is_the_maximum_compiled_step_risk() {
        let capability = ExternalCapability::ExecuteWorkflowRecipe;
        let descriptor = external_descriptor(capability, RiskLevel::Observe);
        for risk in RiskLevel::ALL.iter().copied() {
            let request = PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::Interactive,
            )
            .with_external_context(ExternalPolicyContext::execute_workflow_recipe(
                [RiskLevel::Observe, risk],
                identity_for(capability).recipe_content_hash(),
            ));
            assert_eq!(request.risk(), risk);
        }
    }

    #[test]
    fn required_external_identity_is_fail_closed_by_kind() {
        for (capability, kind) in [
            (
                ExternalCapability::LaunchExternalAgent,
                "agent_executable_identity_required",
            ),
            (
                ExternalCapability::ConnectMcpServer,
                "agent_executable_identity_required",
            ),
            (
                ExternalCapability::InvokeMcpTool,
                "mcp_tool_schema_identity_required",
            ),
            (
                ExternalCapability::ExecuteWorkflowRecipe,
                "recipe_content_identity_required",
            ),
        ] {
            let descriptor = external_descriptor(capability, RiskLevel::Observe);
            let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(
                &PolicyRequest::new(
                    &descriptor,
                    declared(&descriptor),
                    TrustState::Trusted,
                    ExecutionMode::Interactive,
                )
                .with_external_context(external_context_without_identity(capability)),
            );
            assert_eq!(decision.verdict(), PolicyVerdict::Deny);
            assert_eq!(decision.denial_kind(), Some(kind));
            assert!(decision.validate().is_ok(), "{capability:?}");
        }
    }

    #[test]
    fn a_mismatched_external_declaration_produces_a_valid_durable_denial() {
        let descriptor =
            external_descriptor(ExternalCapability::LaunchExternalAgent, RiskLevel::Observe);
        let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(
            &PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::Interactive,
            )
            .with_external_context(external_context(ExternalCapability::InvokeMcpTool)),
        );

        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(
            decision.denial_kind(),
            Some("external_identity_context_invalid")
        );
        assert!(decision.validate().is_ok());
        let round_trip: PolicyDecision =
            serde_json::from_value(serde_json::to_value(&decision).unwrap()).unwrap();
        assert_eq!(round_trip, decision);
        assert!(round_trip.validate().is_ok());
    }

    #[test]
    fn declaring_an_external_capability_requires_external_context() {
        let capability = ExternalCapability::LaunchExternalAgent;
        let descriptor = external_descriptor(capability, RiskLevel::Observe);
        let decision =
            PolicyEngine::new(UserPolicy::default(), None).evaluate(&PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::Interactive,
            ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(
            decision.denial_kind(),
            Some("external_identity_context_invalid")
        );
        assert!(decision.validate().is_ok());
    }

    #[test]
    fn a_grant_matched_to_an_old_identity_cannot_answer_policy_for_a_new_one() {
        let capability = ExternalCapability::LaunchExternalAgent;
        let descriptor = external_descriptor(capability, RiskLevel::Observe);
        let approved =
            IntegrationIdentity::none().with_agent_executable_sha256(Sha256Hash::of("agent-v1"));
        let grants = [RunGrant::matching(RunGrantScope::ToolForRun, approved)];
        let current =
            ExternalPolicyContext::launch_external_agent(Some(Sha256Hash::of("agent-v2")));
        let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(
            &PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::NonInteractive,
            )
            .with_grants(&grants)
            .with_external_context(current),
        );
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(
            decision.denial_kind(),
            Some("noninteractive_external_agent_launch_denied")
        );
    }

    #[test]
    fn external_permission_context_is_advisory_and_non_decisive() {
        let capability = ExternalCapability::InvokeMcpTool;
        let descriptor = external_descriptor(capability, RiskLevel::Observe);
        let decide = |context| {
            PolicyEngine::new(UserPolicy::default(), None).evaluate(
                &PolicyRequest::new(
                    &descriptor,
                    declared(&descriptor),
                    TrustState::Trusted,
                    ExecutionMode::Interactive,
                )
                .with_external_context(
                    external_context(capability).with_permission_context(context),
                ),
            )
        };
        let allow = decide(ExternalPermissionContext {
            acp_option: Some(AcpPermissionOption::AllowAlways),
            mcp_annotations: Some(McpToolAnnotations {
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
            }),
        });
        let reject = decide(ExternalPermissionContext {
            acp_option: Some(AcpPermissionOption::RejectAlways),
            mcp_annotations: Some(McpToolAnnotations {
                read_only_hint: Some(false),
                destructive_hint: Some(true),
                idempotent_hint: Some(false),
                open_world_hint: Some(true),
            }),
        });
        assert_eq!(allow.verdict(), reject.verdict());
        assert_eq!(allow.source(), reject.source());
        assert_eq!(allow.reason(), reject.reason());
        assert_ne!(allow.external_request(), reject.external_request());
    }

    #[test]
    fn every_external_noninteractive_ask_has_a_registered_denial_kind() {
        for capability in ExternalCapability::ALL.iter().copied() {
            let descriptor = external_descriptor(capability, RiskLevel::Observe);
            let context = if capability == ExternalCapability::ExecuteWorkflowRecipe {
                ExternalPolicyContext::execute_workflow_recipe(
                    [RiskLevel::WorkspaceWrite],
                    identity_for(capability).recipe_content_hash(),
                )
            } else {
                external_context(capability)
            };
            let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(
                &PolicyRequest::new(
                    &descriptor,
                    declared(&descriptor),
                    TrustState::Trusted,
                    ExecutionMode::NonInteractive,
                )
                .with_external_context(context),
            );
            assert_eq!(decision.verdict(), PolicyVerdict::Deny, "{capability:?}");
            assert_eq!(
                decision.denial_kind(),
                Some(capability.noninteractive_denial_kind())
            );
            assert!(
                EXTERNAL_POLICY_DENIAL_KINDS.contains(&decision.denial_kind().unwrap()),
                "{capability:?}"
            );
        }
    }

    #[test]
    fn repository_configuration_cannot_grant_external_execution() {
        let workspace = TempDir::new().unwrap();
        write_repository_policy(
            workspace.path(),
            r#"{
  "version": 2,
  "external_capabilities": {
    "launch_external_agent": "allow"
  }
}"#,
        );
        let data = TempDir::new().unwrap();
        let engine = PolicyEngine::load(data.path(), workspace.path());
        let capability = ExternalCapability::LaunchExternalAgent;
        let descriptor = external_descriptor(capability, RiskLevel::Observe);
        let decision = engine.evaluate(
            &PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::Interactive,
            )
            .with_external_context(external_context(capability)),
        );
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
        assert!(decision.reason().contains("may not grant allow"));
    }

    #[test]
    fn external_policy_context_wire_is_strict_and_omits_absent_optional_fields() {
        let context = external_context(ExternalCapability::InvokeMcpTool);
        let encoded = serde_json::to_value(context).unwrap();
        assert_eq!(encoded["schema_version"], 1);
        assert!(encoded.get("external_permission_context").is_none());
        let mut unknown = encoded;
        unknown["future"] = serde_json::json!(true);
        let wrapped = serde_json::json!({
            "verdict": "ask",
            "reason": "fixture",
            "source": "built_in",
            "external_request": unknown
        });
        assert!(serde_json::from_value::<PolicyDecision>(wrapped).is_err());

        let future = serde_json::json!({
            "schema_version": 2,
            "capability": "invoke_mcp_tool",
            "classified_risk": "execute",
            "future": true
        });
        let wrapped = serde_json::json!({
            "verdict": "ask",
            "reason": "fixture",
            "source": "built_in",
            "external_request": future
        });
        let error = serde_json::from_value::<PolicyDecision>(wrapped).unwrap_err();
        assert!(error.to_string().contains("newer Harkness build"));
        assert!(!error.to_string().contains("unknown field"));
    }

    #[test]
    fn external_request_and_decision_fixtures_are_frozen() {
        let context = external_context(ExternalCapability::InvokeMcpTool);
        let request = format!("{}\n", serde_json::to_string_pretty(&context).unwrap());
        assert_eq!(
            request,
            include_str!("policy/fixtures/external-request-v1.json")
        );

        let descriptor = external_descriptor(ExternalCapability::InvokeMcpTool, RiskLevel::Observe);
        let decision = PolicyEngine::new(UserPolicy::default(), None).evaluate(
            &PolicyRequest::new(
                &descriptor,
                declared(&descriptor),
                TrustState::Trusted,
                ExecutionMode::NonInteractive,
            )
            .with_external_context(context),
        );
        let decision = format!("{}\n", serde_json::to_string_pretty(&decision).unwrap());
        assert_eq!(
            decision,
            include_str!("policy/fixtures/external-decision-v1.json")
        );
    }

    // -- grants and noninteractive resolution --------------------------------

    #[test]
    fn noninteractive_ask_requires_a_live_matching_grant() {
        let descriptor = descriptor(RiskLevel::WorkspaceWrite);
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        let denied = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::NonInteractive,
            &[],
        ));
        assert_eq!(denied.verdict(), PolicyVerdict::Deny);
        assert!(denied.reason().contains("noninteractive"));

        let grants = [RunGrant::matching(
            RunGrantScope::ToolForRun,
            IntegrationIdentity::none(),
        )];
        let allowed = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::NonInteractive,
            &grants,
        ));
        assert_eq!(allowed.verdict(), PolicyVerdict::Allow);
        assert_eq!(allowed.source(), PolicySource::RunGrant);
    }

    #[test]
    fn no_grant_can_answer_a_denial() {
        let descriptor = descriptor(RiskLevel::Observe);
        let engine = PolicyEngine::new(
            UserPolicy::default().with_risk(RiskLevel::Observe, PolicyVerdict::Deny),
            None,
        );
        for scope in [
            RunGrantScope::ExactCall,
            RunGrantScope::ToolForRun,
            RunGrantScope::CapabilityForRun,
        ] {
            let grants = [RunGrant::matching(scope, IntegrationIdentity::none())];
            let decision = engine.evaluate(&request(
                &descriptor,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &grants,
            ));
            assert_eq!(decision.verdict(), PolicyVerdict::Deny, "{scope:?}");
        }
    }

    #[test]
    fn remote_and_destructive_approvals_are_one_call_only() {
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        for risk in [RiskLevel::RemoteWrite, RiskLevel::Destructive] {
            let descriptor = descriptor(risk);
            let decision = engine.evaluate(&request(
                &descriptor,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &[],
            ));
            assert_eq!(decision.verdict(), PolicyVerdict::Ask);
            assert!(decision.one_call_only());

            let broad = [RunGrant::matching(
                RunGrantScope::ToolForRun,
                IntegrationIdentity::none(),
            )];
            assert_eq!(
                engine
                    .evaluate(&request(
                        &descriptor,
                        TrustState::Trusted,
                        ExecutionMode::NonInteractive,
                        &broad,
                    ))
                    .verdict(),
                PolicyVerdict::Deny
            );

            let exact = [RunGrant::matching(
                RunGrantScope::ExactCall,
                IntegrationIdentity::none(),
            )];
            let allowed = engine.evaluate(&request(
                &descriptor,
                TrustState::Trusted,
                ExecutionMode::NonInteractive,
                &exact,
            ));
            assert_eq!(allowed.verdict(), PolicyVerdict::Allow);
            assert_eq!(allowed.source(), PolicySource::RunGrant);
        }
    }

    // -- versioned files -----------------------------------------------------

    #[test]
    fn strict_versioned_file_round_trips_and_is_frozen() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(USER_POLICY_FILE);
        let tool = "fixture.policy".parse::<ToolId>().unwrap();
        let policy = UserPolicy::default()
            .with_risk(RiskLevel::Observe, PolicyVerdict::Ask)
            .with_tool(&tool, PolicyVerdict::Deny)
            .with_external_capability(ExternalCapability::InvokeMcpTool, PolicyVerdict::Deny);
        policy.persist(&path).unwrap();
        assert_eq!(UserPolicy::load(&path).unwrap(), policy);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            include_str!("fixtures/policy-v2.json")
        );
    }

    #[test]
    fn a_v1_policy_loads_without_rewrite_and_upgrades_on_explicit_persist() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(USER_POLICY_FILE);
        let frozen = include_str!("fixtures/policy-v1.json");
        fs::write(&path, frozen).unwrap();

        let policy = UserPolicy::load(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), frozen);
        policy.persist(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path).unwrap()).unwrap()
                ["version"],
            2
        );
    }

    #[test]
    fn malformed_unknown_and_future_files_fail_closed_by_name() {
        for (name, contents, kind) in [
            ("malformed.json", "{", "policy_malformed"),
            (
                "unknown.json",
                r#"{"version":2,"surprise":true}"#,
                "policy_malformed",
            ),
            ("future.json", r#"{"version":3}"#, "policy_version_too_new"),
            (
                "v1-external.json",
                r#"{"version":1,"external_capabilities":{"invoke_mcp_tool":"deny"}}"#,
                "policy_malformed",
            ),
        ] {
            let directory = TempDir::new().unwrap();
            let path = directory.path().join(name);
            fs::write(&path, contents).unwrap();
            let error = UserPolicy::load(&path).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.path(), path);
        }
    }

    #[test]
    fn load_errors_become_denials_that_name_the_file() {
        let data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let policy_path = data.path().join(USER_POLICY_FILE);
        fs::create_dir(&policy_path).unwrap();
        let engine = PolicyEngine::load(data.path(), workspace.path());
        let descriptor = descriptor(RiskLevel::Observe);
        let decision = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::UserPolicy);
        assert!(decision.reason().contains(policy_path.to_str().unwrap()));
    }

    #[test]
    fn malformed_and_future_files_also_fail_closed_during_evaluation() {
        for (contents, source) in [
            ("{", PolicySource::UserPolicy),
            (r#"{"version":3}"#, PolicySource::UserPolicy),
        ] {
            let data = TempDir::new().unwrap();
            let workspace = TempDir::new().unwrap();
            let policy_path = data.path().join(USER_POLICY_FILE);
            fs::write(&policy_path, contents).unwrap();
            let engine = PolicyEngine::load(data.path(), workspace.path());
            let descriptor = descriptor(RiskLevel::Observe);
            let decision = engine.evaluate(&request(
                &descriptor,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &[],
            ));
            assert_eq!(decision.verdict(), PolicyVerdict::Deny);
            assert_eq!(decision.source(), source);
            assert!(decision.reason().contains(policy_path.to_str().unwrap()));
        }
    }

    // -- repository policy is repository-controlled content ------------------

    #[test]
    fn a_repository_policy_that_cannot_be_read_denies_and_names_itself() {
        for contents in ["{", r#"{"version":3}"#, r#"{"version":2,"surprise":true}"#] {
            let workspace = TempDir::new().unwrap();
            write_repository_policy(workspace.path(), contents);
            let decision = repository_decision(workspace.path());
            assert_eq!(decision.verdict(), PolicyVerdict::Deny, "{contents}");
            assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
            assert!(decision.reason().contains(REPOSITORY_POLICY_FILE_NAME));
        }
    }

    #[test]
    fn a_repository_policy_that_is_not_a_regular_file_is_refused() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join(REPOSITORY_POLICY_FILE)).unwrap();
        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
        assert!(decision.reason().contains("not a regular file"));
    }

    #[test]
    fn an_oversized_policy_file_is_refused_before_it_is_parsed() {
        let workspace = TempDir::new().unwrap();
        let oversized = usize::try_from(MAX_POLICY_FILE_BYTES).unwrap() + 1;
        let mut contents = String::from(r#"{"version":1,"tools":{}}"#);
        contents.push_str(&" ".repeat(oversized));
        write_repository_policy(workspace.path(), &contents);

        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
        assert!(decision.reason().contains("maximum policy size"));
    }

    #[test]
    fn a_policy_file_at_exactly_the_maximum_size_still_loads() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(USER_POLICY_FILE);
        let head = r#"{"version":1,"tools":{}}"#;
        let padding = usize::try_from(MAX_POLICY_FILE_BYTES).unwrap() - head.len();
        fs::write(&path, format!("{head}{}", " ".repeat(padding))).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            MAX_POLICY_FILE_BYTES,
            "the fixture must sit exactly on the boundary"
        );
        assert_eq!(UserPolicy::load(&path).unwrap(), UserPolicy::default());
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_policy_symlinked_out_of_the_workspace_is_refused() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let planted = outside.path().join("hostile-policy.json");
        fs::write(&planted, r#"{"version":1,"risks":{"destructive":"allow"}}"#).unwrap();

        let path = workspace.path().join(REPOSITORY_POLICY_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&planted, &path).unwrap();

        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
        assert!(decision.reason().contains(REPOSITORY_POLICY_FILE_NAME));
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_policy_directory_symlinked_out_of_the_workspace_is_refused() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(
            outside.path().join("policy.json"),
            r#"{"version":1,"risks":{"destructive":"allow"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join(".harkness")).unwrap();

        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_policy_symlinked_inside_the_workspace_still_loads() {
        let workspace = TempDir::new().unwrap();
        let inside = workspace.path().join("shared-policy.json");
        fs::write(&inside, r#"{"version":1,"risks":{"observe":"ask"}}"#).unwrap();
        let path = workspace.path().join(REPOSITORY_POLICY_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&inside, &path).unwrap();

        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Ask);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
    }

    #[test]
    fn a_workspace_that_cannot_be_resolved_fails_closed() {
        let data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let missing = workspace.path().join("gone");
        let engine = PolicyEngine::load(data.path(), &missing);
        let descriptor = descriptor(RiskLevel::Observe);
        let decision = engine.evaluate(&request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::RepositoryPolicy);
    }

    #[test]
    fn an_absent_repository_policy_leaves_the_defaults_in_place() {
        let workspace = TempDir::new().unwrap();
        let decision = repository_decision(workspace.path());
        assert_eq!(decision.verdict(), PolicyVerdict::Allow);
        assert_eq!(decision.source(), PolicySource::BuiltIn);
    }

    #[test]
    fn policy_error_kinds_are_stable_and_distinct() {
        let path = PathBuf::from("policy.json");
        let errors = [
            PolicyLoadError::Unreadable {
                path: path.clone(),
                reason: "no access".to_owned(),
            },
            PolicyLoadError::Malformed {
                path: path.clone(),
                reason: "bad json".to_owned(),
            },
            PolicyLoadError::VersionTooNew {
                path,
                found: 3,
                maximum: 2,
            },
        ];
        assert_eq!(
            errors.map(|error| error.kind()).as_slice(),
            PolicyLoadError::KINDS
        );
    }

    // -- latency -------------------------------------------------------------

    /// Latency targets are meaningful only in a release build, so debug and CI
    /// runs skip them; run with `--release ... -- --ignored` to record numbers.
    #[test]
    #[ignore = "latency target; meaningful only in a release build"]
    fn policy_evaluation_meets_the_latency_target() {
        let descriptor = descriptor(RiskLevel::WorkspaceWrite);
        let engine = PolicyEngine::new(
            UserPolicy::default().with_risk(RiskLevel::WorkspaceWrite, PolicyVerdict::Ask),
            Some(
                RepositoryPolicy::default()
                    .with_risk(RiskLevel::WorkspaceWrite, PolicyVerdict::Ask),
            ),
        );
        let request = request(
            &descriptor,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        );
        let iterations = 10_000u32;
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(engine.evaluate(std::hint::black_box(&request)));
        }
        let average = started.elapsed() / iterations;
        eprintln!("policy evaluation averaged {average:?} over {iterations} iterations");
        assert!(average < std::time::Duration::from_millis(5));
    }
}
