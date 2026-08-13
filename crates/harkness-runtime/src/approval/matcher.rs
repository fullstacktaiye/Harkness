//! Binding a live grant to the exact request it was given for.
//!
//! This is the security core of the approval module. Everything else records
//! what was asked and what was answered; [`grant_applies`] is what decides that
//! an answer covers a *new* call, and it is the only thing standing between "a
//! human approved an innocuous patch" and "an agent applied a different one".
//!
//! # There is no partial application
//!
//! Every axis must agree, whatever the scope: the run, the workspace identity,
//! the tool id, the tool version, and a live lifecycle. `ExactCall` additionally
//! requires the canonical input hash, and `CapabilityForRun` swaps the tool id
//! for the declared capabilities. A single mismatch on any axis is not a weaker
//! match, it is no match — there is no path here that returns "close enough".
//!
//! # The matcher reads no clock and touches no database
//!
//! Liveness is decided against an instant the *candidate* carries, so matching a
//! run's grants is arithmetic over values the caller already has.
//! [`crate::policy::PolicyEngine::evaluate`] makes the same promise, and a
//! matcher that read a clock would make one call's verdict depend on when it was
//! evaluated rather than on what it is.

use time::OffsetDateTime;

use crate::domain::{ApprovalId, RunId};
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
    workspace: WorkspaceBinding,
    tool: ToolIdentity,
    capabilities: Vec<Capability>,
    input_hash: InputHash,
    scope: ApprovalScope,
    expires_at: Option<OffsetDateTime>,
}

impl ApprovalGrant {
    /// Projects a granted request, and nothing else, into a grant.
    pub(super) fn of(request: &ApprovalRequest) -> Option<Self> {
        (request.state() == ApprovalState::Granted).then(|| Self {
            approval_id: request.id(),
            run_id: request.run_id(),
            workspace: request.workspace().clone(),
            tool: request.tool().clone(),
            capabilities: request.capabilities().to_vec(),
            input_hash: request.input_hash(),
            // The *effective* scope, which a decision narrowing to one call has
            // already rewritten: a grant reaches as far as what was allowed, not
            // as far as what was asked.
            scope: request.effective_scope(),
            expires_at: request.expires_at(),
        })
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

    /// Whether this grant can still authorize anything at `at`.
    ///
    /// Existing at all already means the request was granted, so the only
    /// remaining question is its expiry — read against an instant the caller
    /// supplies rather than a clock, so one call's verdict never depends on when
    /// it happened to be evaluated.
    #[must_use]
    pub fn is_live(&self, at: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|expiry| at < expiry)
    }

    /// Projects this grant into policy, if and only if it covers `candidate`.
    ///
    /// The only production route to a [`RunGrant`]. Policy deliberately cannot
    /// build one, so "an approval exists for this call" is a claim only this
    /// module can make.
    #[must_use]
    pub fn matching(&self, candidate: &CandidateCall<'_>) -> Option<RunGrant> {
        grant_applies(self, candidate).then(|| RunGrant::matching(policy_scope(self.scope)))
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
    workspace: &'a WorkspaceBinding,
    tool: &'a ToolIdentity,
    capabilities: &'a [Capability],
    input_hash: InputHash,
    at: OffsetDateTime,
}

impl<'a> CandidateCall<'a> {
    /// Describes one call about to be evaluated at `at`.
    #[must_use]
    pub const fn new(
        run_id: RunId,
        workspace: &'a WorkspaceBinding,
        tool: &'a ToolIdentity,
        input_hash: InputHash,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            run_id,
            workspace,
            tool,
            capabilities: &[],
            input_hash,
            at,
        }
    }

    /// Attaches the capabilities the candidate's descriptor declares.
    #[must_use]
    pub const fn with_capabilities(mut self, capabilities: &'a [Capability]) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Run this call belongs to.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Instant liveness is decided against.
    #[must_use]
    pub const fn at(&self) -> OffsetDateTime {
        self.at
    }
}

/// Whether `grant` authorizes `candidate`.
///
/// # The scope-independent axes
///
/// A live lifecycle, the same run, the same workspace identity, and the same
/// tool version are required by every scope. Run and workspace binding are what
/// stop a grant replaying into another attempt or another checkout; the version
/// is required even by `CapabilityForRun` because a capability describes what a
/// tool may do, not what a particular build of it *does*, and a new version is
/// new code the approver never saw.
///
/// # What each scope adds
///
/// - `ExactCall` requires the tool id and the canonical input hash. Changing any
///   byte of any input field changes the hash, so the grant cannot transfer.
/// - `ToolForRun` requires the tool id and ignores the input, which is exactly
///   what "allow this tool for the rest of the run" means.
/// - `CapabilityForRun` ignores the tool id and requires that the candidate
///   declare at least one capability and that **every** capability it declares
///   is covered by the grant.
///
/// That last rule is a subset test rather than the equality a single-capability
/// tool would suggest, and both halves of it matter. Testing for overlap instead
/// would let a tool requiring `{network, fs.write}` run under a grant for
/// `network` alone, handing out the capability nobody approved. Refusing a
/// candidate that declares nothing keeps a capability grant from being the
/// broadest scope in the system by accident: a tool with no declared
/// capabilities has none the grant can be about.
#[must_use]
pub fn grant_applies(grant: &ApprovalGrant, candidate: &CandidateCall<'_>) -> bool {
    grant.is_live(candidate.at)
        && grant.run_id == candidate.run_id
        && grant.workspace == *candidate.workspace
        && grant.tool.version == candidate.tool.version
        && match grant.scope {
            ApprovalScope::ExactCall => {
                grant.tool.id == candidate.tool.id && grant.input_hash == candidate.input_hash
            }
            ApprovalScope::ToolForRun => grant.tool.id == candidate.tool.id,
            ApprovalScope::CapabilityForRun => {
                !candidate.capabilities.is_empty()
                    && candidate
                        .capabilities
                        .iter()
                        .all(|required| grant.capabilities.contains(required))
            }
        }
}

/// Every grant that covers `candidate`, projected for policy evaluation.
#[must_use]
pub fn matching_grants(grants: &[ApprovalGrant], candidate: &CandidateCall<'_>) -> Vec<RunGrant> {
    grants
        .iter()
        .filter_map(|grant| grant.matching(candidate))
        .collect()
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
    use time::OffsetDateTime;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use crate::approval::record::tests::{at, workspace};
    use crate::approval::{
        ApprovalDecision, ApprovalRequest, DecidedVia, PendingApproval, canonical_input_hash,
    };
    use crate::domain::{RunId, ToolCallId};
    use crate::policy::{
        PolicyEngine, PolicyRequest, PolicyVerdict, RunGrant, RunGrantScope, UserPolicy,
    };
    use crate::tool::{
        Capability, ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, erase,
    };
    use crate::trust::{ExecutionMode, RequestFlags, TrustState, classify_request};

    use super::super::{ApprovalScope, ApprovalState, InputHash, WorkspaceBinding};
    use super::{ApprovalGrant, CandidateCall, grant_applies, matching_grants};

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
        workspace: &WorkspaceBinding,
        tool: &ToolIdentity,
        capabilities: &[Capability],
        input_hash: InputHash,
        scope: ApprovalScope,
        expires_at: Option<OffsetDateTime>,
    ) -> ApprovalGrant {
        let mut request = ApprovalRequest::open(request_for(
            run_id,
            workspace,
            tool,
            capabilities,
            input_hash,
            scope,
            expires_at,
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
        workspace: &WorkspaceBinding,
        tool: &ToolIdentity,
        capabilities: &[Capability],
        input_hash: InputHash,
        scope: ApprovalScope,
        expires_at: Option<OffsetDateTime>,
    ) -> PendingApproval {
        let pending = PendingApproval::new(
            run_id,
            ToolCallId::new(),
            tool.clone(),
            input_hash,
            workspace.clone(),
            RiskLevel::Execute,
            at(0),
        )
        .requesting(scope)
        .with_capabilities(capabilities.iter().cloned());
        match expires_at {
            Some(expiry) => pending.expiring_at(expiry),
            None => pending,
        }
    }

    /// A grant and a candidate that agree on every axis.
    struct Pair {
        grant: ApprovalGrant,
        run_id: RunId,
        workspace: WorkspaceBinding,
        tool: ToolIdentity,
        capabilities: Vec<Capability>,
        input_hash: InputHash,
    }

    impl Pair {
        fn new(scope: ApprovalScope) -> Self {
            Self::expiring(scope, None)
        }

        /// Grants a request at `scope`, then keeps the values it was granted for
        /// so a test can vary the *candidate* alone.
        fn expiring(scope: ApprovalScope, expires_at: Option<OffsetDateTime>) -> Self {
            let run_id = RunId::new();
            let workspace = workspace();
            let tool = tool();
            let capabilities = capabilities(&["fs.write"]);
            let input_hash = input_hash();
            Self {
                grant: granted(
                    run_id,
                    &workspace,
                    &tool,
                    &capabilities,
                    input_hash,
                    scope,
                    expires_at,
                ),
                run_id,
                workspace,
                tool,
                capabilities,
                input_hash,
            }
        }

        fn candidate(&self) -> CandidateCall<'_> {
            CandidateCall::new(
                self.run_id,
                &self.workspace,
                &self.tool,
                self.input_hash,
                at(10),
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
    fn one_mismatched_tool_version_defeats_every_scope() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut pair = Pair::new(scope);
            pair.tool = ToolIdentity::parse("fs.write", "1.3.0").unwrap();
            assert!(
                !pair.applies(),
                "{scope}: a new version is code the approver never saw"
            );
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
                    &workspace,
                    &tool,
                    &declared,
                    input_hash(),
                    scope,
                    None,
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
    fn an_expired_grant_defeats_every_scope_from_the_instant_it_expires() {
        for scope in ApprovalScope::ALL.iter().copied() {
            let pair = Pair::expiring(scope, Some(at(10)));
            let candidate_at = |when: OffsetDateTime| {
                CandidateCall::new(
                    pair.run_id,
                    &pair.workspace,
                    &pair.tool,
                    pair.input_hash,
                    when,
                )
                .with_capabilities(&pair.capabilities)
            };

            assert!(grant_applies(&pair.grant, &candidate_at(at(9))), "{scope}");
            assert!(
                !grant_applies(&pair.grant, &candidate_at(at(10))),
                "{scope}"
            );
            assert!(
                !grant_applies(&pair.grant, &candidate_at(at(11))),
                "{scope}"
            );
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

    // -- capability grants ----------------------------------------------------

    #[test]
    fn a_capability_grant_covers_a_subset_and_refuses_anything_extra() {
        let run_id = RunId::new();
        let workspace = workspace();
        let grant = granted(
            run_id,
            &workspace,
            &tool(),
            &capabilities(&["fs.write", "network"]),
            input_hash(),
            ApprovalScope::CapabilityForRun,
            None,
        );
        // A different tool of the same version, which is the point of the scope.
        let other_tool = ToolIdentity::parse("net.fetch", "1.2.0").unwrap();

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
                &workspace,
                &other_tool,
                canonical_input_hash(&json!({"url": "https://example.invalid"})).unwrap(),
                at(10),
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
            &pair.workspace,
            &pair.tool,
            &pair.capabilities,
            pair.input_hash,
            ApprovalScope::ToolForRun,
            None,
        );
        // A grant of this run that has already expired.
        let expired = granted(
            pair.run_id,
            &pair.workspace,
            &pair.tool,
            &pair.capabilities,
            pair.input_hash,
            ApprovalScope::ToolForRun,
            Some(at(5)),
        );

        let grants = [unrelated, pair.grant.clone(), expired];
        let matched = matching_grants(&grants, &pair.candidate());

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].scope(), RunGrantScope::ExactCall);
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
            evaluate(&matching_grants(
                std::slice::from_ref(&pair.grant),
                &pair.candidate()
            )),
            PolicyVerdict::Allow
        );

        // The same grant against a call it does not cover projects nothing, so
        // the verdict falls straight back to the refusal.
        let mut other_run = Pair::new(ApprovalScope::ToolForRun);
        other_run.run_id = RunId::new();
        assert_eq!(
            evaluate(&matching_grants(
                std::slice::from_ref(&other_run.grant),
                &other_run.candidate()
            )),
            PolicyVerdict::Deny
        );
    }
}
