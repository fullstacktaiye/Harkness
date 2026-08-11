//! Layered policy loading and pure tool-request evaluation.
//!
//! Policy files are read once at a run boundary. [`PolicyEngine::evaluate`]
//! performs no I/O, reads no clock, and takes no lock: identical loaded policy
//! and request values always produce the same decision.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::tool::{RiskLevel, ToolDescriptor, ToolId};
use crate::trust::{ContainedPath, ExecutionMode, TrustState};

/// Current version of `policy.json`.
pub const POLICY_SCHEMA_VERSION: u32 = 1;
/// Name of the global policy file below the Harkness data directory.
pub const USER_POLICY_FILE: &str = "policy.json";
/// Repository-relative policy path.
pub const REPOSITORY_POLICY_FILE: &str = ".harkness/policy.json";

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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecision {
    verdict: PolicyVerdict,
    reason: String,
    source: PolicySource,
    #[serde(default, skip_serializing_if = "is_false")]
    one_call_only: bool,
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
        }
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
}

impl RunGrant {
    /// Projects one live, matching approval grant into policy evaluation.
    #[must_use]
    pub const fn matching(scope: RunGrantScope) -> Self {
        Self { scope }
    }

    /// Effective scope after approval scope ceilings were applied.
    #[must_use]
    pub const fn scope(self) -> RunGrantScope {
        self.scope
    }
}

/// Concrete facts policy consumes after validation and boundary checking.
pub struct PolicyRequest<'a> {
    /// Immutable published tool contract.
    pub descriptor: &'a ToolDescriptor,
    /// Risk after request-specific classification.
    pub risk: RiskLevel,
    /// Trust resolved for this project identity and canonical workspace root.
    pub trust: TrustState,
    /// Whether a human can answer a new prompt.
    pub mode: ExecutionMode,
    /// Every filesystem input, already checked by the workspace boundary.
    pub paths: &'a [ContainedPath],
    /// Live grants already matched to this candidate by the approval module.
    pub grants: &'a [RunGrant],
    /// Whether the validated request selects any force-push variant.
    pub force_push: bool,
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
}

impl PolicyFile {
    fn empty() -> Self {
        Self {
            version: POLICY_SCHEMA_VERSION,
            risks: BTreeMap::new(),
            tools: BTreeMap::new(),
        }
    }

    fn selected(&self, request: &PolicyRequest<'_>) -> Option<(PolicyVerdict, String)> {
        self.tools
            .get(request.descriptor.id().as_str())
            .copied()
            .map(|verdict| {
                (
                    verdict,
                    format!("tool {}", request.descriptor.id().as_str()),
                )
            })
            .or_else(|| {
                self.risks
                    .get(&request.risk)
                    .copied()
                    .map(|verdict| (verdict, format!("risk {}", request.risk.as_str())))
            })
    }

    fn validate(&self, path: &Path) -> Result<(), PolicyLoadError> {
        if self.version != POLICY_SCHEMA_VERSION {
            return Err(PolicyLoadError::Malformed {
                path: path.to_path_buf(),
                reason: format!(
                    "unsupported policy version {}; the minimum supported version is {}",
                    self.version, POLICY_SCHEMA_VERSION
                ),
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
        load_file(path.as_ref()).map(Self)
    }

    /// Atomically replaces the file with this policy.
    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), PolicyLoadError> {
        persist_file(path.as_ref(), &self.0)
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
    /// a persisted, human-readable policy decision.
    #[must_use]
    pub fn load(data_dir: impl AsRef<Path>, workspace: impl AsRef<Path>) -> Self {
        let user_path = data_dir.as_ref().join(USER_POLICY_FILE);
        let repository_path = workspace.as_ref().join(REPOSITORY_POLICY_FILE);
        let user = match UserPolicy::load(&user_path) {
            Ok(policy) => LoadedPolicy::Loaded(policy),
            Err(error) => LoadedPolicy::Failed(error),
        };
        let repository = match load_optional_file(&repository_path) {
            Ok(policy) => LoadedPolicy::Loaded(policy.map(RepositoryPolicy)),
            Err(error) => LoadedPolicy::Failed(error),
        };
        Self { user, repository }
    }

    /// Evaluates a fully classified request without I/O, clock reads, or locks.
    ///
    /// Built-in, user, and repository rules combine with `max` on
    /// `Deny > Ask > Allow`. A repository can therefore tighten a decision and
    /// cannot weaken it. A live matching grant answers only `Ask`; it can never
    /// override `Deny`, and broad grants cannot answer remote-write or
    /// destructive requests.
    #[must_use]
    pub fn evaluate(&self, request: &PolicyRequest<'_>) -> PolicyDecision {
        if request.force_push {
            return PolicyDecision::new(
                PolicyVerdict::Deny,
                "denied: force push is not permitted in v0.3",
                PolicySource::BuiltIn,
                false,
            );
        }

        let user = match &self.user {
            LoadedPolicy::Loaded(policy) => policy,
            LoadedPolicy::Failed(error) => return load_failure(error, PolicySource::UserPolicy),
        };
        let repository = match &self.repository {
            LoadedPolicy::Loaded(policy) => policy.as_ref(),
            LoadedPolicy::Failed(error) => {
                return load_failure(error, PolicySource::RepositoryPolicy);
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
            request.risk,
            RiskLevel::RemoteWrite | RiskLevel::Destructive
        );
        let matching_grant = request
            .grants
            .iter()
            .any(|grant| !exact_only || grant.scope == RunGrantScope::ExactCall);
        if verdict == PolicyVerdict::Ask && matching_grant {
            return PolicyDecision::new(
                PolicyVerdict::Allow,
                "allowed: a live run-scoped grant matches this request",
                PolicySource::RunGrant,
                false,
            );
        }
        if verdict == PolicyVerdict::Ask && request.mode == ExecutionMode::NonInteractive {
            return PolicyDecision::new(
                PolicyVerdict::Deny,
                format!("denied: noninteractive execution cannot answer approval; {reason}"),
                source,
                false,
            );
        }

        PolicyDecision::new(
            verdict,
            reason,
            source,
            verdict == PolicyVerdict::Ask && exact_only,
        )
    }
}

fn built_in(request: &PolicyRequest<'_>) -> (PolicyVerdict, PolicySource, String) {
    let verdict = match (request.trust, request.risk) {
        (TrustState::Trusted, RiskLevel::Observe) => PolicyVerdict::Allow,
        (TrustState::Trusted, _) => PolicyVerdict::Ask,
        (TrustState::Untrusted, RiskLevel::Observe) => PolicyVerdict::Ask,
        (TrustState::Untrusted, _) => PolicyVerdict::Deny,
    };
    let reason = match (request.trust, verdict) {
        (TrustState::Untrusted, PolicyVerdict::Deny) => "denied: workspace is untrusted".to_owned(),
        (TrustState::Untrusted, PolicyVerdict::Ask) => {
            "workspace observation requires approval because the workspace is untrusted".to_owned()
        }
        (_, PolicyVerdict::Allow) => {
            format!(
                "{} is allowed by the trusted-workspace default",
                request.risk
            )
        }
        (_, PolicyVerdict::Ask) => format!(
            "{} requires approval (built-in default for {})",
            risk_label(request.risk),
            request.risk
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

fn load_optional_file(path: &Path) -> Result<Option<PolicyFile>, PolicyLoadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(PolicyLoadError::Unreadable {
                path: path.to_path_buf(),
                reason: error.to_string(),
            });
        }
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
    let policy: PolicyFile =
        serde_json::from_slice(&bytes).map_err(|error| PolicyLoadError::Malformed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    policy.validate(path)?;
    Ok(Some(policy))
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
    use std::time::Instant;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;
    use crate::tool::{ExecutionContext, Tool, ToolError, ToolIdentity, ToolMetadata, erase};

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

    fn descriptor(risk: RiskLevel) -> ToolDescriptor {
        erase(FixtureTool(risk)).unwrap().descriptor().clone()
    }

    fn request<'a>(
        descriptor: &'a ToolDescriptor,
        risk: RiskLevel,
        trust: TrustState,
        mode: ExecutionMode,
        grants: &'a [RunGrant],
    ) -> PolicyRequest<'a> {
        PolicyRequest {
            descriptor,
            risk,
            trust,
            mode,
            paths: &[],
            grants,
            force_push: false,
        }
    }

    #[test]
    fn built_in_table_covers_every_risk_and_trust_branch() {
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        for trust in [TrustState::Trusted, TrustState::Untrusted] {
            for risk in RiskLevel::ALL.iter().copied() {
                let descriptor = descriptor(risk);
                let decision = engine.evaluate(&request(
                    &descriptor,
                    risk,
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
            RiskLevel::Observe,
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
            RiskLevel::WorkspaceWrite,
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
            RiskLevel::Observe,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        ));
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::UserPolicy);
        assert!(decision.reason().contains("fixture.policy"));
    }

    #[test]
    fn every_repository_rule_preserves_or_raises_severity() {
        for risk in RiskLevel::ALL.iter().copied() {
            let descriptor = descriptor(risk);
            for trust in [TrustState::Trusted, TrustState::Untrusted] {
                let baseline = PolicyEngine::new(UserPolicy::default(), None).evaluate(&request(
                    &descriptor,
                    risk,
                    trust,
                    ExecutionMode::Interactive,
                    &[],
                ));
                for candidate in [
                    PolicyVerdict::Allow,
                    PolicyVerdict::Ask,
                    PolicyVerdict::Deny,
                ] {
                    let with_repository = PolicyEngine::new(
                        UserPolicy::default(),
                        Some(RepositoryPolicy::default().with_risk(risk, candidate)),
                    )
                    .evaluate(&request(
                        &descriptor,
                        risk,
                        trust,
                        ExecutionMode::Interactive,
                        &[],
                    ));
                    assert!(with_repository.verdict() >= baseline.verdict());
                }
            }
        }
    }

    #[test]
    fn force_push_is_a_non_overridable_built_in_denial() {
        let descriptor = descriptor(RiskLevel::RemoteWrite);
        let grant = [RunGrant::matching(RunGrantScope::ExactCall)];
        let engine = PolicyEngine::new(
            UserPolicy::default().with_risk(RiskLevel::RemoteWrite, PolicyVerdict::Allow),
            Some(
                RepositoryPolicy::default().with_risk(RiskLevel::RemoteWrite, PolicyVerdict::Allow),
            ),
        );
        let mut request = request(
            &descriptor,
            RiskLevel::RemoteWrite,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &grant,
        );
        request.force_push = true;
        let decision = engine.evaluate(&request);
        assert_eq!(decision.verdict(), PolicyVerdict::Deny);
        assert_eq!(decision.source(), PolicySource::BuiltIn);
        assert!(decision.reason().contains("force push"));
    }

    #[test]
    fn noninteractive_ask_requires_a_live_matching_grant() {
        let descriptor = descriptor(RiskLevel::WorkspaceWrite);
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        let denied = engine.evaluate(&request(
            &descriptor,
            RiskLevel::WorkspaceWrite,
            TrustState::Trusted,
            ExecutionMode::NonInteractive,
            &[],
        ));
        assert_eq!(denied.verdict(), PolicyVerdict::Deny);
        assert!(denied.reason().contains("noninteractive"));

        let grants = [RunGrant::matching(RunGrantScope::ToolForRun)];
        let allowed = engine.evaluate(&request(
            &descriptor,
            RiskLevel::WorkspaceWrite,
            TrustState::Trusted,
            ExecutionMode::NonInteractive,
            &grants,
        ));
        assert_eq!(allowed.verdict(), PolicyVerdict::Allow);
        assert_eq!(allowed.source(), PolicySource::RunGrant);
    }

    #[test]
    fn remote_and_destructive_approvals_are_one_call_only() {
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        for risk in [RiskLevel::RemoteWrite, RiskLevel::Destructive] {
            let descriptor = descriptor(risk);
            let decision = engine.evaluate(&request(
                &descriptor,
                risk,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &[],
            ));
            assert_eq!(decision.verdict(), PolicyVerdict::Ask);
            assert!(decision.one_call_only());

            let broad = [RunGrant::matching(RunGrantScope::ToolForRun)];
            assert_eq!(
                engine
                    .evaluate(&request(
                        &descriptor,
                        risk,
                        TrustState::Trusted,
                        ExecutionMode::NonInteractive,
                        &broad,
                    ))
                    .verdict(),
                PolicyVerdict::Deny
            );

            let exact = [RunGrant::matching(RunGrantScope::ExactCall)];
            let allowed = engine.evaluate(&request(
                &descriptor,
                risk,
                TrustState::Trusted,
                ExecutionMode::NonInteractive,
                &exact,
            ));
            assert_eq!(allowed.verdict(), PolicyVerdict::Allow);
            assert_eq!(allowed.source(), PolicySource::RunGrant);
        }
    }

    #[test]
    fn strict_versioned_file_round_trips_and_is_frozen() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(USER_POLICY_FILE);
        let tool = "fixture.policy".parse::<ToolId>().unwrap();
        let policy = UserPolicy::default()
            .with_risk(RiskLevel::Observe, PolicyVerdict::Ask)
            .with_tool(&tool, PolicyVerdict::Deny);
        policy.persist(&path).unwrap();
        assert_eq!(UserPolicy::load(&path).unwrap(), policy);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            include_str!("fixtures/policy-v1.json")
        );
    }

    #[test]
    fn malformed_unknown_and_future_files_fail_closed_by_name() {
        for (name, contents, kind) in [
            ("malformed.json", "{", "policy_malformed"),
            (
                "unknown.json",
                r#"{"version":1,"surprise":true}"#,
                "policy_malformed",
            ),
            ("future.json", r#"{"version":2}"#, "policy_version_too_new"),
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
            RiskLevel::Observe,
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
            (r#"{"version":2}"#, PolicySource::UserPolicy),
        ] {
            let data = TempDir::new().unwrap();
            let workspace = TempDir::new().unwrap();
            let policy_path = data.path().join(USER_POLICY_FILE);
            fs::write(&policy_path, contents).unwrap();
            let engine = PolicyEngine::load(data.path(), workspace.path());
            let descriptor = descriptor(RiskLevel::Observe);
            let decision = engine.evaluate(&request(
                &descriptor,
                RiskLevel::Observe,
                TrustState::Trusted,
                ExecutionMode::Interactive,
                &[],
            ));
            assert_eq!(decision.verdict(), PolicyVerdict::Deny);
            assert_eq!(decision.source(), source);
            assert!(decision.reason().contains(policy_path.to_str().unwrap()));
        }
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
                found: 2,
                maximum: 1,
            },
        ];
        assert_eq!(
            errors.map(|error| error.kind()).as_slice(),
            PolicyLoadError::KINDS
        );
    }

    #[test]
    fn release_policy_evaluation_stays_below_five_milliseconds() {
        if cfg!(debug_assertions) {
            return;
        }
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
            RiskLevel::WorkspaceWrite,
            TrustState::Trusted,
            ExecutionMode::Interactive,
            &[],
        );
        let iterations = 10_000u32;
        let started = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(engine.evaluate(std::hint::black_box(&request)));
        }
        let average = started.elapsed() / iterations;
        eprintln!("policy evaluation averaged {average:?} over {iterations} iterations");
        assert!(average < std::time::Duration::from_millis(5));
    }
}
