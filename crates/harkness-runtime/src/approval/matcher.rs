//! Binding a live grant to the exact request it was given for.
//!
//! This is the security core of the approval module. Everything else records
//! what was asked and what was answered; [`grant_applies`] is what decides that
//! an answer covers a *new* call, and it is the only thing standing between "a
//! human approved an innocuous patch" and "an agent applied a different one".
//!
//! # There is no partial application
//!
//! The run and the workspace identity must agree whatever the scope, and each
//! scope adds the axes that give it meaning: the recorded call, the tool
//! identity and the input hash for `ExactCall`, the tool identity for
//! `ToolForRun`, the declared capabilities for `CapabilityForRun`. A single
//! mismatch on any axis a scope names is not a weaker match, it is no match —
//! there is no path here that returns "close enough".
//!
//! # The matcher reads no clock and touches no database
//!
//! Matching a run's grants is arithmetic over values the caller already has.
//! [`crate::policy::PolicyEngine::evaluate`] makes the same promise, and a
//! matcher that read a clock would make one call's verdict depend on when it was
//! evaluated rather than on what it is.

use crate::domain::{ApprovalId, RunId, ToolCallId};
use crate::integration::IntegrationIdentity;
use crate::policy::{RunGrant, RunGrantScope};
use crate::tool::{Capability, ToolIdentity};

use super::{ApprovalRequest, ApprovalScope, ApprovalState, InputHash, WorkspaceBinding};

/// A granted approval, in the shape the matcher compares against.
///
/// Projected from a stored [`ApprovalRequest`] and never constructed: a grant is
/// an authorization, so the only way to have one is to hold the request a human
/// allowed. That is also why there is no lifecycle field here. `granted` is
/// terminal, so a request that reached it cannot leave; every other state yields
/// no grant at all, which makes "a dead approval authorizes nothing" a shape the
/// type system holds rather than a check somebody has to remember.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalGrant {
    approval_id: ApprovalId,
    run_id: RunId,
    tool_call_id: ToolCallId,
    workspace: WorkspaceBinding,
    tool: ToolIdentity,
    capabilities: Vec<Capability>,
    integration_identity: IntegrationIdentity,
    input_hash: InputHash,
    scope: ApprovalScope,
}

impl ApprovalGrant {
    /// Projects a granted request, and nothing else, into a grant.
    ///
    /// The request's `expires_at` is deliberately not carried across. It is a
    /// deadline for a *human to answer*, so the only thing it can do is stop a
    /// request from ever becoming a grant; reusing it as the grant's own
    /// lifetime would make a `ToolForRun` approval given "for the remainder of
    /// the run" quietly stop applying part-way through it. A grant's lifetime is
    /// its run, which is what "every grant dies with its run" means.
    pub(super) fn of(request: &ApprovalRequest) -> Option<Self> {
        (request.state() == ApprovalState::Granted).then(|| Self {
            approval_id: request.id(),
            run_id: request.run_id(),
            tool_call_id: request.tool_call_id(),
            workspace: request.workspace().clone(),
            tool: request.tool().clone(),
            capabilities: request.capabilities().to_vec(),
            integration_identity: request.integration_identity(),
            input_hash: request.input_hash(),
            // The *effective* scope, which a decision narrowing to one call has
            // already rewritten: a grant reaches as far as what was allowed, not
            // as far as what was asked.
            scope: request.effective_scope(),
        })
    }

    /// Tool call this grant was raised for.
    #[must_use]
    pub const fn tool_call_id(&self) -> ToolCallId {
        self.tool_call_id
    }

    /// Approval this grant came from.
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    /// Run the grant dies with.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Breadth the human actually allowed.
    #[must_use]
    pub const fn scope(&self) -> ApprovalScope {
        self.scope
    }

    /// Projects this grant into policy, if and only if it covers `candidate`.
    ///
    /// The only production route to a [`RunGrant`]. Policy deliberately cannot
    /// build one, so "an approval exists for this call" is a claim only this
    /// module can make.
    #[must_use]
    pub fn matching(&self, candidate: &CandidateCall<'_>) -> Option<RunGrant> {
        match self.match_candidate(candidate) {
            GrantMatch::Matched(grant) => Some(grant),
            GrantMatch::IdentityDrift(_) | GrantMatch::NotApplicable => None,
        }
    }

    /// Explains whether this grant matches, including identity drift that must
    /// be recorded separately from an ordinary unrelated grant.
    #[must_use]
    pub fn match_candidate(&self, candidate: &CandidateCall<'_>) -> GrantMatch {
        if !scope_applies_except_identity(self, candidate) {
            return GrantMatch::NotApplicable;
        }
        if self.integration_identity != candidate.integration_identity {
            return GrantMatch::IdentityDrift(IntegrationIdentityDrift::between(
                self.approval_id,
                self.integration_identity,
                candidate.integration_identity,
            ));
        }
        GrantMatch::Matched(RunGrant::matching(
            policy_scope(self.scope),
            self.integration_identity,
        ))
    }
}

/// One identity component that changed after an approval was granted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IntegrationIdentityField {
    /// ACP-agent or MCP-server executable content changed.
    AgentExecutableSha256,
    /// Imported MCP tool schema changed.
    McpToolSchemaFingerprint,
    /// Compiled workflow recipe content changed.
    RecipeContentHash,
}

impl IntegrationIdentityField {
    /// Stable spelling for the drift event payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentExecutableSha256 => "agent_executable_sha256",
            Self::McpToolSchemaFingerprint => "mcp_tool_schema_fingerprint",
            Self::RecipeContentHash => "recipe_content_hash",
        }
    }
}

/// Observable identity drift for an otherwise applicable approval grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationIdentityDrift {
    approval_id: ApprovalId,
    changed_fields: Vec<IntegrationIdentityField>,
}

impl IntegrationIdentityDrift {
    fn between(
        approval_id: ApprovalId,
        approved: IntegrationIdentity,
        observed: IntegrationIdentity,
    ) -> Self {
        let mut changed_fields = Vec::new();
        if approved.agent_executable_sha256() != observed.agent_executable_sha256() {
            changed_fields.push(IntegrationIdentityField::AgentExecutableSha256);
        }
        if approved.mcp_tool_schema_fingerprint() != observed.mcp_tool_schema_fingerprint() {
            changed_fields.push(IntegrationIdentityField::McpToolSchemaFingerprint);
        }
        if approved.recipe_content_hash() != observed.recipe_content_hash() {
            changed_fields.push(IntegrationIdentityField::RecipeContentHash);
        }
        Self {
            approval_id,
            changed_fields,
        }
    }

    /// Approval invalidated by drift.
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    /// Exact hash fields whose observed values no longer match.
    #[must_use]
    pub fn changed_fields(&self) -> &[IntegrationIdentityField] {
        &self.changed_fields
    }
}

/// Result of matching one grant to one candidate call.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GrantMatch {
    /// The grant authorizes the candidate and is ready for policy evaluation.
    Matched(RunGrant),
    /// All non-identity axes match, but one or more identity hashes changed.
    IdentityDrift(IntegrationIdentityDrift),
    /// The grant belongs to another request, scope, run, or workspace.
    NotApplicable,
}

/// Complete result of matching a run's grants to one candidate.
///
/// Identity drift is retained beside applicable grants so the coordinator can
/// append one `approval_identity_drift` event per invalidated approval before
/// it opens a replacement prompt. It is never collapsed into an ordinary
/// non-match.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GrantMatches {
    grants: Vec<RunGrant>,
    identity_drifts: Vec<IntegrationIdentityDrift>,
}

impl GrantMatches {
    /// Grants policy may consume for this candidate.
    #[must_use]
    pub fn grants(&self) -> &[RunGrant] {
        &self.grants
    }

    /// Otherwise-applicable approvals invalidated by changed identity hashes.
    #[must_use]
    pub fn identity_drifts(&self) -> &[IntegrationIdentityDrift] {
        &self.identity_drifts
    }
}

/// The call being evaluated, in the shape the matcher compares against.
///
/// Borrowing rather than owning, because a candidate is built per call from
/// values the coordinator already holds and is discarded as soon as the verdict
/// is known.
#[derive(Clone, Copy, Debug)]
pub struct CandidateCall<'a> {
    run_id: RunId,
    tool_call_id: ToolCallId,
    workspace: &'a WorkspaceBinding,
    tool: &'a ToolIdentity,
    capabilities: &'a [Capability],
    integration_identity: IntegrationIdentity,
    input_hash: InputHash,
}

impl<'a> CandidateCall<'a> {
    /// Describes one recorded call about to be evaluated.
    #[must_use]
    pub const fn new(
        run_id: RunId,
        tool_call_id: ToolCallId,
        workspace: &'a WorkspaceBinding,
        tool: &'a ToolIdentity,
        input_hash: InputHash,
    ) -> Self {
        Self {
            run_id,
            tool_call_id,
            workspace,
            tool,
            capabilities: &[],
            integration_identity: IntegrationIdentity::none(),
            input_hash,
        }
    }

    /// Attaches the capabilities the candidate's descriptor declares.
    #[must_use]
    pub const fn with_capabilities(mut self, capabilities: &'a [Capability]) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Attaches the external identities observed for the candidate call.
    #[must_use]
    pub const fn with_integration_identity(mut self, identity: IntegrationIdentity) -> Self {
        self.integration_identity = identity;
        self
    }

    /// Run this call belongs to.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Recorded call being evaluated.
    #[must_use]
    pub const fn tool_call_id(&self) -> ToolCallId {
        self.tool_call_id
    }
}

/// Whether `grant` authorizes `candidate`.
///
/// # The scope-independent axes
///
/// The same run and the same workspace identity are required by every scope.
/// Together they are what stop a grant replaying into another attempt of the
/// same task or into another checkout of the same project. Existing at all is
/// the third: only a granted request becomes an [`ApprovalGrant`].
///
/// # What each scope adds
///
/// - `ExactCall` requires the recorded tool call, the tool identity *including
///   its version*, and the canonical input hash. Binding to the call is what
///   makes it one call: the ceiling reduces every remote-write and destructive
///   request to this scope precisely so that authorizing one force push does not
///   authorize a second, byte-identical one later in the same run. The input
///   hash is kept beside it rather than made redundant by it, so an authorization
///   still cannot survive the input being re-derived differently.
/// - `ToolForRun` requires the tool identity, again including the version, and
///   ignores both the call and the input — which is exactly what "allow this
///   tool for the rest of the run" means. A new version is new code the approver
///   never saw, so it needs its own approval.
/// - `CapabilityForRun` requires that the candidate declare at least one
///   capability and that **every** capability it declares is covered by the
///   grant. It compares no tool identity at all, because a capability grant is
///   an answer about a *capability* and not about a tool: comparing a version
///   here would mean matching one tool's version string against another's, so
///   whether a grant covered a call would turn on two unrelated tools happening
///   to share a version number.
///
/// The capability rule is a subset test rather than the equality a
/// single-capability tool would suggest, and both halves of it matter. Testing
/// for overlap instead would let a tool requiring `{network, fs.write}` run
/// under a grant for `network` alone, handing out the capability nobody
/// approved. Refusing a candidate that declares nothing keeps a capability grant
/// from being the broadest scope in the system by accident: a tool with no
/// declared capabilities has none the grant can be about.
///
/// Nothing here reads a clock. A grant's lifetime is its run, and the answer
/// deadline a request may carry can only stop one from being granted.
#[must_use]
pub fn grant_applies(grant: &ApprovalGrant, candidate: &CandidateCall<'_>) -> bool {
    matches!(grant.match_candidate(candidate), GrantMatch::Matched(_))
}

fn scope_applies_except_identity(grant: &ApprovalGrant, candidate: &CandidateCall<'_>) -> bool {
    grant.run_id == candidate.run_id
        && grant.workspace == *candidate.workspace
        && match grant.scope {
            ApprovalScope::ExactCall => {
                grant.tool_call_id == candidate.tool_call_id
                    && grant.tool == *candidate.tool
                    && grant.input_hash == candidate.input_hash
            }
            ApprovalScope::ToolForRun => grant.tool == *candidate.tool,
            ApprovalScope::CapabilityForRun => {
                !candidate.capabilities.is_empty()
                    && candidate
                        .capabilities
                        .iter()
                        .all(|required| grant.capabilities.contains(required))
            }
        }
}

/// Every applicable grant and every identity drift observed for `candidate`.
#[must_use]
pub fn matching_grants(grants: &[ApprovalGrant], candidate: &CandidateCall<'_>) -> GrantMatches {
    let mut matches = GrantMatches::default();
    for grant in grants {
        match grant.match_candidate(candidate) {
            GrantMatch::Matched(grant) => matches.grants.push(grant),
            GrantMatch::IdentityDrift(drift) => matches.identity_drifts.push(drift),
            GrantMatch::NotApplicable => {}
        }
    }
    matches
}

/// Narrows a durable scope to the projection policy consumes.
const fn policy_scope(scope: ApprovalScope) -> RunGrantScope {
    match scope {
        ApprovalScope::ExactCall => RunGrantScope::ExactCall,
        ApprovalScope::ToolForRun => RunGrantScope::ToolForRun,
        ApprovalScope::CapabilityForRun => RunGrantScope::CapabilityForRun,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use crate::approval::record::tests::{at, workspace};
    use crate::approval::{
        ApprovalDecision, ApprovalRequest, DecidedVia, PendingApproval, canonical_input_hash,
    };
    use crate::domain::{RunId, ToolCallId};
    use crate::integration::{IntegrationIdentity, Sha256Hash};
    use crate::policy::{
        PolicyEngine, PolicyRequest, PolicyVerdict, RunGrant, RunGrantScope, UserPolicy,
    };
    use crate::tool::{
        Capability, ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, erase,
    };
    use crate::trust::{ExecutionMode, RequestFlags, TrustState, classify_request};

    use super::super::{ApprovalScope, ApprovalState, InputHash, WorkspaceBinding};
    use super::{
        ApprovalGrant, CandidateCall, GrantMatch, IntegrationIdentityField, grant_applies,
        matching_grants,
    };

    fn tool() -> ToolIdentity {
        ToolIdentity::parse("fs.write", "1.2.0").unwrap()
    }

    fn input_hash() -> InputHash {
        canonical_input_hash(&json!({"path": "src/lib.rs", "contents": "fn main() {}"})).unwrap()
    }

    fn capabilities(names: &[&str]) -> Vec<Capability> {
        names
            .iter()
            .map(|name| Capability::new(*name).unwrap())
            .collect()
    }

    /// Opens a request and grants it, which is the only way to hold a grant.
    ///
    /// `Execute` risk throughout: the ceiling reduces remote-write and
    /// destructive requests to one call, and these tests are about the matcher
    /// rather than about the ceiling.
    fn granted(
        run_id: RunId,
        tool_call_id: ToolCallId,
        workspace: &WorkspaceBinding,
        tool: &ToolIdentity,
        capabilities: &[Capability],
        input_hash: InputHash,
        scope: ApprovalScope,
    ) -> ApprovalGrant {
        let mut request = ApprovalRequest::open(request_for(
            run_id,
            tool_call_id,
            workspace,
            tool,
            capabilities,
            input_hash,
            scope,
        ))
        .unwrap();
        request
            .decide(ApprovalDecision::grant(
                request.id(),
                scope,
                DecidedVia::Cli,
                at(1),
            ))
            .unwrap();
        request.grant().expect("a granted request is a grant")
    }

    fn request_for(
        run_id: RunId,
        tool_call_id: ToolCallId,
        workspace: &WorkspaceBinding,
        tool: &ToolIdentity,
        capabilities: &[Capability],
        input_hash: InputHash,
        scope: ApprovalScope,
    ) -> PendingApproval {
        PendingApproval::new(
            run_id,
            tool_call_id,
            tool.clone(),
            input_hash,
            workspace.clone(),
            RiskLevel::Execute,
            at(0),
        )
        .requesting(scope)
        .with_capabilities(capabilities.iter().cloned())
    }

    /// A grant and a candidate that agree on every axis.
    struct Pair {
        grant: ApprovalGrant,
        run_id: RunId,
        tool_call_id: ToolCallId,
        workspace: WorkspaceBinding,
        tool: ToolIdentity,
        capabilities: Vec<Capability>,
        input_hash: InputHash,
    }

    impl Pair {
        /// Grants a request at `scope`, then keeps the values it was granted for
        /// so a test can vary the *candidate* alone.
        fn new(scope: ApprovalScope) -> Self {
            let run_id = RunId::new();
            let tool_call_id = ToolCallId::new();
            let workspace = workspace();
            let tool = tool();
            let capabilities = capabilities(&["fs.write"]);
            let input_hash = input_hash();
            Self {
                grant: granted(
                    run_id,
                    tool_call_id,
                    &workspace,
                    &tool,
                    &capabilities,
                    input_hash,
                    scope,
                ),
                run_id,
                tool_call_id,
                workspace,
                tool,
                capabilities,
                input_hash,
            }
        }

        fn candidate(&self) -> CandidateCall<'_> {
            CandidateCall::new(
                self.run_id,
                self.tool_call_id,
                &self.workspace,
                &self.tool,
                self.input_hash,
            )
            .with_capabilities(&self.capabilities)
        }

        fn applies(&self) -> bool {
            grant_applies(&self.grant, &self.candidate())
        }
    }

    // -- every scope crossed with every mismatch axis -------------------------

    #[test]
    fn a_grant_that_agrees_on_every_axis_applies_at_every_scope() {
        for scope in ApprovalScope::ALL.iter().copied() {
            assert!(Pair::new(scope).applies(), "{scope}");
        }
    }

    #[test]
    fn one_mismatched_run_defeats_every_scope() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut pair = Pair::new(scope);
            pair.run_id = RunId::new();
            assert!(!pair.applies(), "{scope}");
        }
    }

    #[test]
    fn one_mismatched_workspace_defeats_every_scope() {
        let other_project = "66666666-6666-4666-8666-666666666666"
            .parse::<harkness_core::ProjectId>()
            .unwrap();
        for scope in ApprovalScope::ALL.iter().copied() {
            // A different checkout of the same project.
            let mut moved = Pair::new(scope);
            moved.workspace =
                WorkspaceBinding::new(moved.workspace.project_id(), "/workspace/harkness-worktree");
            assert!(!moved.applies(), "{scope} moved root");

            // The same path, claimed by a different project.
            let mut reused = Pair::new(scope);
            reused.workspace =
                WorkspaceBinding::new(Some(other_project), reused.workspace.canonical_root());
            assert!(!reused.applies(), "{scope} reused path");

            // A workspace the catalog no longer knows.
            let mut anonymous = Pair::new(scope);
            anonymous.workspace = WorkspaceBinding::new(None, anonymous.workspace.canonical_root());
            assert!(!anonymous.applies(), "{scope} absent project");
        }
    }

    #[test]
    fn one_mismatched_tool_version_defeats_the_scopes_that_name_a_tool() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut pair = Pair::new(scope);
            pair.tool = ToolIdentity::parse("fs.write", "1.3.0").unwrap();
            assert_eq!(
                pair.applies(),
                scope == ApprovalScope::CapabilityForRun,
                "{scope}: a new version is code the approver never saw, but a \
                 capability grant names no tool to have a version"
            );
        }
    }

    #[test]
    fn a_capability_grant_never_compares_versions_across_unrelated_tools() {
        // Comparing the grant's tool version against a *different* tool's would
        // make coverage turn on two unrelated tools happening to share a number:
        // `net.fetch@1.2.0` covered and `net.fetch@2.0.0` not, for no reason the
        // approver stated.
        let pair = Pair::new(ApprovalScope::CapabilityForRun);
        for version in ["1.2.0", "2.0.0", "0.1.0-rc.1"] {
            let other = ToolIdentity::parse("net.fetch", version).unwrap();
            let candidate = CandidateCall::new(
                pair.run_id,
                ToolCallId::new(),
                &pair.workspace,
                &other,
                input_hash(),
            )
            .with_capabilities(&pair.capabilities);
            assert!(grant_applies(&pair.grant, &candidate), "{version}");
        }
    }

    #[test]
    fn a_dead_lifecycle_yields_no_grant_at_all() {
        // A grant is unconstructible from anything but a granted request, so a
        // dead approval does not lose the match — it never reaches the matcher.
        for scope in ApprovalScope::ALL.iter().copied() {
            for state in ApprovalState::ALL.iter().copied() {
                let workspace = workspace();
                let tool = tool();
                let declared = capabilities(&["fs.write"]);
                let mut request = ApprovalRequest::open(request_for(
                    RunId::new(),
                    ToolCallId::new(),
                    &workspace,
                    &tool,
                    &declared,
                    input_hash(),
                    scope,
                ))
                .unwrap();
                match state {
                    ApprovalState::Pending => {}
                    ApprovalState::Granted => request
                        .decide(ApprovalDecision::grant(
                            request.id(),
                            scope,
                            DecidedVia::Cli,
                            at(1),
                        ))
                        .unwrap(),
                    ApprovalState::Denied => request
                        .decide(ApprovalDecision::deny(request.id(), DecidedVia::Cli, at(1)))
                        .unwrap(),
                    other => request.resolve(other, at(1)).unwrap(),
                }

                assert_eq!(
                    request.grant().is_some(),
                    state == ApprovalState::Granted,
                    "{scope}/{state}"
                );
            }
        }
    }

    #[test]
    fn an_answer_deadline_never_becomes_the_lifetime_of_the_grant_it_produced() {
        // `expires_at` is a deadline for a human to answer. Carrying it into the
        // grant would make an approval given "for the remainder of the run" stop
        // applying part-way through it, for a reason nobody stated.
        for scope in ApprovalScope::ALL.iter().copied() {
            let run_id = RunId::new();
            let tool_call_id = ToolCallId::new();
            let workspace = workspace();
            let tool = tool();
            let declared = capabilities(&["fs.write"]);
            let mut request = ApprovalRequest::open(
                request_for(
                    run_id,
                    tool_call_id,
                    &workspace,
                    &tool,
                    &declared,
                    input_hash(),
                    scope,
                )
                .expiring_at(at(10)),
            )
            .unwrap();
            request
                .decide(ApprovalDecision::grant(
                    request.id(),
                    scope,
                    DecidedVia::Cli,
                    at(5),
                ))
                .unwrap();

            let grant = request.grant().unwrap();
            let candidate =
                CandidateCall::new(run_id, tool_call_id, &workspace, &tool, input_hash())
                    .with_capabilities(&declared);
            assert!(
                grant_applies(&grant, &candidate),
                "{scope}: the grant outlives the deadline for answering its request"
            );

            // The deadline still does the one job it has: an *unanswered*
            // request past it can be resolved, and never becomes a grant.
            let mut lapsed = ApprovalRequest::open(
                request_for(
                    run_id,
                    tool_call_id,
                    &workspace,
                    &tool,
                    &declared,
                    input_hash(),
                    scope,
                )
                .expiring_at(at(10)),
            )
            .unwrap();
            assert!(lapsed.is_expired(at(10)));
            lapsed.expire(at(10)).unwrap();
            assert!(lapsed.grant().is_none());
        }
    }

    #[test]
    fn an_exact_call_grant_authorizes_only_the_call_it_was_raised_for() {
        // The ceiling reduces every remote-write and destructive request to this
        // scope on the strength of it being *one* call. A second, byte-identical
        // force push later in the same run is a second call.
        let pair = Pair::new(ApprovalScope::ExactCall);
        let repeat = CandidateCall::new(
            pair.run_id,
            ToolCallId::new(),
            &pair.workspace,
            &pair.tool,
            pair.input_hash,
        )
        .with_capabilities(&pair.capabilities);

        assert!(pair.applies(), "the approved call is authorized");
        assert!(
            !grant_applies(&pair.grant, &repeat),
            "an identical repeat is a second call and needs its own approval"
        );
    }

    #[test]
    fn a_run_wide_grant_covers_calls_other_than_the_one_that_raised_it() {
        // The counterpart: `ToolForRun` and `CapabilityForRun` mean what they
        // say, so binding the call must not have narrowed them too.
        for scope in [ApprovalScope::ToolForRun, ApprovalScope::CapabilityForRun] {
            let pair = Pair::new(scope);
            let later = CandidateCall::new(
                pair.run_id,
                ToolCallId::new(),
                &pair.workspace,
                &pair.tool,
                canonical_input_hash(&json!({"path": "src/other.rs"})).unwrap(),
            )
            .with_capabilities(&pair.capabilities);
            assert!(grant_applies(&pair.grant, &later), "{scope}");
        }
    }

    #[test]
    fn a_mismatched_tool_id_defeats_the_scopes_that_name_one() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut pair = Pair::new(scope);
            pair.tool = ToolIdentity::parse("fs.read", "1.2.0").unwrap();
            assert_eq!(
                pair.applies(),
                scope == ApprovalScope::CapabilityForRun,
                "{scope}: only a capability grant is about something other than a tool id"
            );
        }
    }

    #[test]
    fn a_mismatched_input_hash_defeats_only_the_exact_call_scope() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut pair = Pair::new(scope);
            pair.input_hash = canonical_input_hash(&json!({
                "path": "src/lib.rs",
                "contents": "fn main() { launch() }"
            }))
            .unwrap();
            assert_eq!(pair.applies(), scope != ApprovalScope::ExactCall, "{scope}");
        }
    }

    #[test]
    fn every_external_identity_hash_is_bound_at_every_scope() {
        let identities = [
            (
                "launch_external_agent",
                IntegrationIdentity::none()
                    .with_agent_executable_sha256(Sha256Hash::of("agent-v1")),
                IntegrationIdentity::none()
                    .with_agent_executable_sha256(Sha256Hash::of("agent-v2")),
                IntegrationIdentityField::AgentExecutableSha256,
            ),
            (
                "invoke_mcp_tool",
                IntegrationIdentity::none()
                    .with_mcp_tool_schema_fingerprint(Sha256Hash::of("schema-v1")),
                IntegrationIdentity::none()
                    .with_mcp_tool_schema_fingerprint(Sha256Hash::of("schema-v2")),
                IntegrationIdentityField::McpToolSchemaFingerprint,
            ),
            (
                "execute_workflow_recipe",
                IntegrationIdentity::none().with_recipe_content_hash(Sha256Hash::of("recipe-v1")),
                IntegrationIdentity::none().with_recipe_content_hash(Sha256Hash::of("recipe-v2")),
                IntegrationIdentityField::RecipeContentHash,
            ),
        ];

        for scope in ApprovalScope::ALL.iter().copied() {
            for (capability, approved, different, changed_field) in identities {
                let pair = Pair::new(scope);
                let external_capabilities = capabilities(&[capability]);
                let mut request = ApprovalRequest::open(
                    request_for(
                        pair.run_id,
                        pair.tool_call_id,
                        &pair.workspace,
                        &pair.tool,
                        &external_capabilities,
                        pair.input_hash,
                        scope,
                    )
                    .with_integration_identity(approved),
                )
                .unwrap();
                request
                    .decide(ApprovalDecision::grant(
                        request.id(),
                        scope,
                        DecidedVia::Cli,
                        at(1),
                    ))
                    .unwrap();
                let grant = request.grant().unwrap();
                let candidate = pair
                    .candidate()
                    .with_capabilities(&external_capabilities)
                    .with_integration_identity(approved);
                assert!(grant_applies(&grant, &candidate), "{scope}/{approved:?}");
                let drifted = pair
                    .candidate()
                    .with_capabilities(&external_capabilities)
                    .with_integration_identity(different);
                let GrantMatch::IdentityDrift(drift) = grant.match_candidate(&drifted) else {
                    panic!("identity drift must be observable at {scope}");
                };
                assert_eq!(drift.changed_fields(), &[changed_field]);
                assert_eq!(drift.changed_fields()[0].as_str(), changed_field.as_str());
                assert!(
                    !grant_applies(
                        &grant,
                        &pair.candidate().with_capabilities(&external_capabilities)
                    ),
                    "present versus absent must not match at {scope}"
                );
            }
        }
    }

    // -- capability grants ----------------------------------------------------

    #[test]
    fn a_capability_grant_covers_a_subset_and_refuses_anything_extra() {
        let run_id = RunId::new();
        let workspace = workspace();
        let grant = granted(
            run_id,
            ToolCallId::new(),
            &workspace,
            &tool(),
            &capabilities(&["fs.write", "network"]),
            input_hash(),
            ApprovalScope::CapabilityForRun,
        );
        // A different tool entirely, which is the point of the scope.
        let other_tool = ToolIdentity::parse("net.fetch", "3.1.4").unwrap();

        for (required, expected) in [
            (vec!["network"], true),
            (vec!["fs.write"], true),
            (vec!["fs.write", "network"], true),
            (vec!["network", "process.spawn"], false),
            (vec!["process.spawn"], false),
            (vec![], false),
        ] {
            let required = capabilities(&required);
            let candidate = CandidateCall::new(
                run_id,
                ToolCallId::new(),
                &workspace,
                &other_tool,
                canonical_input_hash(&json!({"url": "https://example.invalid"})).unwrap(),
            )
            .with_capabilities(&required);
            assert_eq!(
                grant_applies(&grant, &candidate),
                expected,
                "{required:?} against a grant for fs.write and network"
            );
        }
    }

    // -- projection into policy ----------------------------------------------

    #[test]
    fn a_matching_grant_projects_its_scope_and_a_mismatched_one_projects_nothing() {
        for (scope, expected) in [
            (ApprovalScope::ExactCall, RunGrantScope::ExactCall),
            (ApprovalScope::ToolForRun, RunGrantScope::ToolForRun),
            (
                ApprovalScope::CapabilityForRun,
                RunGrantScope::CapabilityForRun,
            ),
        ] {
            let pair = Pair::new(scope);
            assert_eq!(
                pair.grant.matching(&pair.candidate()).map(RunGrant::scope),
                Some(expected)
            );

            let mut mismatched = Pair::new(scope);
            mismatched.run_id = RunId::new();
            assert!(mismatched.grant.matching(&mismatched.candidate()).is_none());
        }
    }

    #[test]
    fn only_the_grants_that_cover_a_candidate_reach_policy() {
        let pair = Pair::new(ApprovalScope::ExactCall);
        // A grant that would cover this call but belongs to another run.
        let unrelated = granted(
            RunId::new(),
            ToolCallId::new(),
            &pair.workspace,
            &pair.tool,
            &pair.capabilities,
            pair.input_hash,
            ApprovalScope::ToolForRun,
        );
        // A grant of this run, for a different checkout of the same project.
        let elsewhere = granted(
            pair.run_id,
            ToolCallId::new(),
            &WorkspaceBinding::new(pair.workspace.project_id(), "/workspace/other"),
            &pair.tool,
            &pair.capabilities,
            pair.input_hash,
            ApprovalScope::ToolForRun,
        );

        let grants = [unrelated, pair.grant.clone(), elsewhere];
        let matched = matching_grants(&grants, &pair.candidate());

        assert_eq!(matched.grants().len(), 1);
        assert_eq!(matched.grants()[0].scope(), RunGrantScope::ExactCall);
        assert!(matched.identity_drifts().is_empty());
    }

    /// A tool whose only job is to publish a descriptor at a chosen risk.
    struct FixtureTool(RiskLevel);

    impl Tool for FixtureTool {
        type Input = FixtureInput;
        type Output = FixtureOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
                "Approval fixture",
                "Provides a descriptor for approval matcher tests.",
                self.0,
            )
        }

        fn execute(
            &self,
            _input: Self::Input,
            _context: &mut ExecutionContext,
        ) -> Result<Self::Output, ToolError> {
            Ok(FixtureOutput {})
        }
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct FixtureInput {}

    #[derive(JsonSchema, Serialize)]
    struct FixtureOutput {}

    #[test]
    fn a_matched_grant_is_what_turns_a_policy_ask_into_an_allow() {
        // The whole point of the projection: policy cannot mint a grant, so an
        // `Ask` only becomes an `Allow` because the matcher accepted one.
        let descriptor = erase(FixtureTool(RiskLevel::WorkspaceWrite))
            .unwrap()
            .descriptor()
            .clone();
        let engine = PolicyEngine::new(UserPolicy::default(), None);
        let classification = classify_request(&descriptor, &[], RequestFlags::default());
        let evaluate = |grants: &[RunGrant]| {
            engine
                .evaluate(
                    &PolicyRequest::new(
                        &descriptor,
                        classification,
                        TrustState::Trusted,
                        ExecutionMode::NonInteractive,
                    )
                    .with_grants(grants),
                )
                .verdict()
        };

        // Without a grant, noninteractive execution cannot answer the question.
        assert_eq!(evaluate(&[]), PolicyVerdict::Deny);

        let pair = Pair::new(ApprovalScope::ToolForRun);
        assert_eq!(
            evaluate(
                matching_grants(std::slice::from_ref(&pair.grant), &pair.candidate()).grants()
            ),
            PolicyVerdict::Allow
        );

        // The same grant against a call it does not cover projects nothing, so
        // the verdict falls straight back to the refusal.
        let mut other_run = Pair::new(ApprovalScope::ToolForRun);
        other_run.run_id = RunId::new();
        assert_eq!(
            evaluate(
                matching_grants(
                    std::slice::from_ref(&other_run.grant),
                    &other_run.candidate(),
                )
                .grants(),
            ),
            PolicyVerdict::Deny
        );
    }
}
