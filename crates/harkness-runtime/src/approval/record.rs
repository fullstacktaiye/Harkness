//! The durable approval request, its lifecycle, and the decision that ends it.

use std::fmt;
use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use time::{OffsetDateTime, UtcOffset};

use crate::domain::{ApprovalId, RunId, ToolCallId};
use crate::integration::IntegrationIdentity;
use crate::policy::ExternalCapability;
use crate::tool::{Capability, RiskLevel, ToolIdentity};

use super::{ApprovalError, ApprovalGrant, InputHash};

/// Lifecycle state of one durable approval request.
///
/// Only [`Pending`](Self::Pending) has outgoing edges. Every other state is
/// final, which is what makes "approval is granted before execution, never
/// retroactively" checkable: a resolved request can never become a grant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ApprovalState {
    /// Recorded and waiting for an answer. The only non-final state.
    Pending,
    /// A human allowed the request, at the scope the decision names.
    Granted,
    /// A human refused the request.
    Denied,
    /// The request outlived its `expires_at` without an answer.
    Expired,
    /// The run the request belongs to was cancelled while it waited.
    Cancelled,
    /// The run will not resume, so the question no longer has a subject.
    ///
    /// This is what a pending request left behind by an interrupted run becomes
    /// when it is answered after the run was abandoned: the answer is recorded
    /// as having arrived, and it authorizes nothing.
    Superseded,
}

impl ApprovalState {
    /// Every state in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Granted,
        Self::Denied,
        Self::Expired,
        Self::Cancelled,
        Self::Superseded,
    ];

    /// The stable spelling stored in `approvals.state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }

    /// Interprets a stored spelling, refusing one this build does not define.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == value)
    }

    /// Whether no later state may follow this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

impl fmt::Display for ApprovalState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every legal approval state edge. An absent edge is invalid.
///
/// The table is deliberately a star out of `pending`: an approval answers one
/// question once, and every way of ending it — decided, expired, cancelled,
/// superseded — is a way of ending it for good.
pub const APPROVAL_TRANSITIONS: &[(ApprovalState, ApprovalState)] = &[
    (ApprovalState::Pending, ApprovalState::Granted),
    (ApprovalState::Pending, ApprovalState::Denied),
    (ApprovalState::Pending, ApprovalState::Expired),
    (ApprovalState::Pending, ApprovalState::Cancelled),
    (ApprovalState::Pending, ApprovalState::Superseded),
];

/// How far a grant reaches once it exists.
///
/// This is the durable vocabulary a request is stored with and a front end
/// renders. [`crate::policy::RunGrantScope`] is its in-memory projection: policy
/// is told only that the matcher accepted a grant and at what breadth, never the
/// binding fields the matcher used to decide.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApprovalScope {
    /// This tool, this version, this exact canonical input, this run.
    ExactCall,
    /// This tool and version for the remainder of the run, whatever the input.
    ToolForRun,
    /// The declared capabilities of this request, for the remainder of the run.
    CapabilityForRun,
}

impl ApprovalScope {
    /// Every scope in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::ExactCall, Self::ToolForRun, Self::CapabilityForRun];

    /// The stable spelling stored in `approvals.requested_scope`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactCall => "exact_call",
            Self::ToolForRun => "tool_for_run",
            Self::CapabilityForRun => "capability_for_run",
        }
    }

    /// Interprets a stored spelling, refusing one this build does not define.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|scope| scope.as_str() == value)
    }

    /// The scope a request at `risk` may actually be stored with.
    ///
    /// Remote writes and destructive work are one-call approvals whatever was
    /// asked for. The ceiling is applied when the request is *created*, not when
    /// a grant is matched, so the stored record shows the downgrade instead of
    /// recording a breadth that was never honored.
    #[must_use]
    pub fn ceiling(self, risk: RiskLevel) -> Self {
        if matches!(risk, RiskLevel::RemoteWrite | RiskLevel::Destructive) {
            return Self::ExactCall;
        }
        self
    }
}

impl fmt::Display for ApprovalScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a human answered.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApprovalVerdict {
    /// The work may proceed, at the scope the decision names.
    Granted,
    /// The work must not proceed.
    Denied,
}

impl ApprovalVerdict {
    /// Every verdict in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Granted, Self::Denied];

    /// The stable spelling stored in `approvals.decision_verdict`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
        }
    }

    /// Interprets a stored spelling, refusing one this build does not define.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|verdict| verdict.as_str() == value)
    }
}

impl fmt::Display for ApprovalVerdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which surface a decision arrived from.
///
/// Recorded because an audit of who authorized what is not answered by the
/// verdict alone, and because either front end can answer any pending request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum DecidedVia {
    /// The `harkness` command line.
    Cli,
    /// The Kirigami application.
    Gui,
}

impl DecidedVia {
    /// Every surface in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Cli, Self::Gui];

    /// The stable spelling stored in `approvals.decided_via`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Gui => "gui",
        }
    }

    /// Interprets a stored spelling, refusing one this build does not define.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|via| via.as_str() == value)
    }
}

impl fmt::Display for DecidedVia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The workspace identity an approval is bound to.
///
/// Both halves matter, for the same reason they do in
/// [`WorkspaceTrust`](crate::trust::WorkspaceTrust): a catalog identity alone
/// survives the checkout being moved, and a path alone is reused by whatever
/// project occupies it next. A grant that matched on either half by itself would
/// replay across checkouts.
///
/// `project_id` mirrors [`Task::project_id`](crate::domain::Task::project_id)
/// and is optional for the same reason a task's is: a run may target a workspace
/// the catalog does not know. Absent matches only absent, so a workspace with no
/// catalog identity is bound by its canonical root alone and never by accident
/// to a project that later claims that path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceBinding {
    project_id: Option<ProjectId>,
    canonical_root: PathBuf,
}

impl WorkspaceBinding {
    /// Binds to a catalog identity and an already-canonical workspace root.
    ///
    /// The root is stored as given rather than canonicalized here: an approval
    /// is created while a run is executing, and the run resolved its canonical
    /// root once at its own boundary. Re-canonicalizing would consult the
    /// filesystem on a path that may have moved *since*, turning a binding into
    /// a fresh observation.
    #[must_use]
    pub fn new(project_id: Option<ProjectId>, canonical_root: impl Into<PathBuf>) -> Self {
        Self {
            project_id,
            canonical_root: canonical_root.into(),
        }
    }

    /// Catalog identity of the workspace, when it has one.
    #[must_use]
    pub const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    /// Canonical root the run resolved.
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

/// The answer that resolves one pending approval.
///
/// Distinct from [`domain::Approval`](crate::domain::Approval) and its
/// [`ApprovalOutcome`](crate::domain::ApprovalOutcome), which are the audit
/// entry appended to a run, step, or tool-call record and the direction it went.
/// That entry says a record was approved; this is the durable answer to a
/// specific question, carrying the scope it authorizes and the surface it
/// arrived from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalDecision {
    approval_id: ApprovalId,
    verdict: ApprovalVerdict,
    scope: Option<ApprovalScope>,
    decided_at: OffsetDateTime,
    decided_via: DecidedVia,
    reason: Option<String>,
}

impl ApprovalDecision {
    /// Allows the request, authorizing exactly `scope`.
    ///
    /// The scope still has to survive [`ApprovalRequest::decide`], which refuses
    /// anything broader than the request was stored with.
    #[must_use]
    pub fn grant(
        approval_id: ApprovalId,
        scope: ApprovalScope,
        decided_via: DecidedVia,
        decided_at: OffsetDateTime,
    ) -> Self {
        Self {
            approval_id,
            verdict: ApprovalVerdict::Granted,
            scope: Some(scope),
            decided_at: decided_at.to_offset(UtcOffset::UTC),
            decided_via,
            reason: None,
        }
    }

    /// Refuses the request.
    ///
    /// A denial carries no scope: there is nothing for one to describe, and a
    /// stored scope beside a denial would read as a grant to anything skimming
    /// the row.
    #[must_use]
    pub fn deny(
        approval_id: ApprovalId,
        decided_via: DecidedVia,
        decided_at: OffsetDateTime,
    ) -> Self {
        Self {
            approval_id,
            verdict: ApprovalVerdict::Denied,
            scope: None,
            decided_at: decided_at.to_offset(UtcOffset::UTC),
            decided_via,
            reason: None,
        }
    }

    /// Attaches the decider's explanation.
    #[must_use]
    pub fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Replaces the decider's explanation with what the store's redactor made
    /// of it, so the value that becomes durable is the value the record carries.
    pub(crate) fn with_redacted_reason(mut self, reason: Option<String>) -> Self {
        self.reason = reason;
        self
    }

    pub(crate) fn from_stored(
        approval_id: ApprovalId,
        verdict: ApprovalVerdict,
        scope: Option<ApprovalScope>,
        decided_via: DecidedVia,
        decided_at: OffsetDateTime,
        reason: Option<String>,
    ) -> Self {
        Self {
            approval_id,
            verdict,
            scope,
            decided_at,
            decided_via,
            reason,
        }
    }

    /// Approval this decision answers.
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    /// Whether the request was granted or denied.
    #[must_use]
    pub const fn verdict(&self) -> ApprovalVerdict {
        self.verdict
    }

    /// Scope authorized, present only on a grant.
    #[must_use]
    pub const fn scope(&self) -> Option<ApprovalScope> {
        self.scope
    }

    /// UTC time the decision was made.
    #[must_use]
    pub const fn decided_at(&self) -> OffsetDateTime {
        self.decided_at
    }

    /// Surface the decision arrived from.
    #[must_use]
    pub const fn decided_via(&self) -> DecidedVia {
        self.decided_via
    }

    /// The decider's explanation, when one was given.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// Everything known about a call that needs a human answer, before one exists.
///
/// A separate type from [`ApprovalRequest`] because the two differ in exactly
/// the things this issue is about: a request has an identity, an effective scope
/// the risk ceiling may have reduced, and a lifecycle. Building one out of the
/// other in [`ApprovalRequest::open`] is what makes those three impossible to
/// set by hand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingApproval {
    run_id: RunId,
    tool_call_id: ToolCallId,
    tool: ToolIdentity,
    capabilities: Vec<Capability>,
    integration_identity: IntegrationIdentity,
    input_hash: InputHash,
    input_summary: String,
    workspace: WorkspaceBinding,
    risk: RiskLevel,
    requested_scope: ApprovalScope,
    expires_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl PendingApproval {
    /// Describes the narrowest possible request: this exact call, no expiry.
    ///
    /// The input is named by its [`InputHash`] rather than by its value, because
    /// the hash is what an `ExactCall` grant is bound to and the caller has
    /// already had to derive it in order to look for an existing grant. Passing
    /// the value here would invite two derivations of one identity, which can
    /// disagree where one cannot.
    #[must_use]
    pub fn new(
        run_id: RunId,
        tool_call_id: ToolCallId,
        tool: ToolIdentity,
        input_hash: InputHash,
        workspace: WorkspaceBinding,
        risk: RiskLevel,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            run_id,
            tool_call_id,
            tool,
            capabilities: Vec::new(),
            integration_identity: IntegrationIdentity::none(),
            input_hash,
            input_summary: String::new(),
            workspace,
            risk,
            requested_scope: ApprovalScope::ExactCall,
            expires_at: None,
            created_at: created_at.to_offset(UtcOffset::UTC),
        }
    }

    /// Asks for a broader scope than one call.
    #[must_use]
    pub const fn requesting(mut self, scope: ApprovalScope) -> Self {
        self.requested_scope = scope;
        self
    }

    /// Records the capabilities the tool's descriptor declares.
    ///
    /// Sorted and deduplicated on the way in, exactly as
    /// [`ToolMetadata::with_capabilities`](crate::tool::ToolMetadata::with_capabilities)
    /// does, so a stored request and the descriptor it came from carry the same
    /// set in the same order and the subset test the matcher runs is stable.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities.extend(capabilities);
        self.capabilities.sort_unstable();
        self.capabilities.dedup();
        self
    }

    /// Binds the external subject identities observed for this operation.
    ///
    /// The matcher compares the complete value for every scope, including
    /// absence, so an executable, schema, or recipe change always defeats a
    /// previously granted approval.
    #[must_use]
    pub const fn with_integration_identity(mut self, identity: IntegrationIdentity) -> Self {
        self.integration_identity = identity;
        self
    }

    /// Attaches the human-readable digest of the input.
    ///
    /// A summary, never the input itself: this text reaches the timeline and
    /// every notification, and the raw input stays in `tool_calls.input_json`
    /// where a surface can expand it on demand.
    #[must_use]
    pub fn summarized_as(mut self, summary: impl Into<String>) -> Self {
        self.input_summary = summary.into();
        self
    }

    /// Sets a wall-clock deadline for a human to answer.
    ///
    /// Past it, [`ApprovalRequest::decide`] refuses, so the deadline is enforced
    /// by the record rather than by whoever holds the timer — a deadline nothing
    /// checks is advice. A lapsed request stays `pending` until something closes
    /// it with [`expire`](ApprovalRequest::expire); a caller that sets a deadline
    /// therefore owes it a sweeper, which is the coordinator's job.
    ///
    /// It bounds only the *answer*. A grant given in time outlives it, because a
    /// grant's lifetime is its run.
    ///
    /// v0.3 leaves this absent by default: runs are interactive, and a request
    /// that expires while a user is reading it is a worse failure than one that
    /// waits. A cancelled or failed run resolves its pending requests regardless
    /// of any deadline.
    #[must_use]
    pub fn expiring_at(mut self, at: OffsetDateTime) -> Self {
        self.expires_at = Some(at.to_offset(UtcOffset::UTC));
        self
    }
}

/// One durable question, persisted before any human is shown anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRequest {
    id: ApprovalId,
    pending: PendingApproval,
    effective_scope: ApprovalScope,
    state: ApprovalState,
    resolved_at: Option<OffsetDateTime>,
    decision: Option<ApprovalDecision>,
}

impl ApprovalRequest {
    /// Opens a pending request under a fresh identity.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::ScopeRequiresCapability`] when a
    /// [`CapabilityForRun`](ApprovalScope::CapabilityForRun) request names no
    /// capability — a scope with nothing to scope to would match every later
    /// call of the run or none of them, depending only on how the matcher read
    /// an empty set.
    pub fn open(pending: PendingApproval) -> Result<Self, ApprovalError> {
        Self::open_with_id(ApprovalId::new(), pending)
    }

    /// Opens a pending request under a caller-chosen identity.
    ///
    /// # Errors
    ///
    /// As [`ApprovalRequest::open`].
    pub fn open_with_id(id: ApprovalId, pending: PendingApproval) -> Result<Self, ApprovalError> {
        let effective_scope = pending.requested_scope.ceiling(pending.risk);
        if effective_scope == ApprovalScope::CapabilityForRun && pending.capabilities.is_empty() {
            return Err(ApprovalError::ScopeRequiresCapability {
                scope: ApprovalScope::CapabilityForRun,
            });
        }
        validate_integration_identity(&pending.capabilities, pending.integration_identity)
            .map_err(|reason| ApprovalError::InvalidIntegrationIdentity { reason })?;
        Ok(Self {
            id,
            pending,
            effective_scope,
            state: ApprovalState::Pending,
            resolved_at: None,
            decision: None,
        })
    }

    /// Replaces the input summary with what the store's redactor made of it.
    ///
    /// A record is redacted on its way into the store and handed back in the
    /// form that was stored, so a caller never holds a request whose summary
    /// differs from the row and the timeline entry describing it.
    pub(crate) fn with_redacted_summary(mut self, summary: String) -> Self {
        self.pending.input_summary = summary;
        self
    }

    /// Rebuilds a stored record, re-checking every rule a fresh one passes.
    ///
    /// Nothing downstream re-derives `effective_scope`: the matcher reads it and
    /// grants exactly that breadth. A row edited outside Harkness to widen it
    /// past the risk ceiling would therefore simply be honored, so the ceiling
    /// is re-applied here rather than trusted from the column. The same
    /// reasoning covers the other cross-column claims a row can make — a
    /// resolved state with no resolution time, a decision whose verdict
    /// disagrees with the state it produced, a granted row whose decision
    /// authorized some other breadth.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::InconsistentRecord`] naming the rule the row
    /// broke.
    pub(crate) fn from_stored(
        id: ApprovalId,
        pending: PendingApproval,
        effective_scope: ApprovalScope,
        state: ApprovalState,
        resolved_at: Option<OffsetDateTime>,
        decision: Option<ApprovalDecision>,
    ) -> Result<Self, ApprovalError> {
        let refuse = |reason| Err(ApprovalError::InconsistentRecord { id, reason });

        // A human may always narrow to the single call in front of them, so
        // `ExactCall` is admissible whatever was asked; anything else has to be
        // exactly what the ceiling would have produced.
        let ceiling = pending.requested_scope.ceiling(pending.risk);
        if effective_scope != ceiling && effective_scope != ApprovalScope::ExactCall {
            return refuse(
                "its effective scope is broader than its requested scope and risk allow",
            );
        }
        if effective_scope == ApprovalScope::CapabilityForRun && pending.capabilities.is_empty() {
            return refuse("it is scoped to a capability but names none");
        }
        if let Err(reason) =
            validate_integration_identity(&pending.capabilities, pending.integration_identity)
        {
            return refuse(reason);
        }
        if state.is_terminal() != resolved_at.is_some() {
            return refuse(
                "a resolved approval records when it was resolved, and a pending one does not",
            );
        }

        match (&decision, state) {
            (None, ApprovalState::Granted | ApprovalState::Denied) => {
                return refuse("a decided approval records the decision that resolved it");
            }
            (Some(_), state)
                if !matches!(state, ApprovalState::Granted | ApprovalState::Denied) =>
            {
                return refuse("an approval nobody answered must record no decision");
            }
            (Some(decision), _) => {
                if decision.approval_id() != id {
                    return refuse("its decision answers a different approval");
                }
                let expected = match decision.verdict() {
                    ApprovalVerdict::Granted => ApprovalState::Granted,
                    ApprovalVerdict::Denied => ApprovalState::Denied,
                };
                if expected != state {
                    return refuse("its state disagrees with the verdict that produced it");
                }
                // A row cannot claim an answer `decide` would have refused as
                // late, or an edited `expires_at` would resurrect the grant the
                // deadline exists to prevent.
                if pending
                    .expires_at
                    .is_some_and(|at| decision.decided_at() >= at)
                {
                    return refuse("it was decided after the deadline for answering it");
                }
                // A decision's timestamp is deliberately not checked against
                // `resolved_at`: they are one stored column read twice, because
                // the moment a request was decided *is* the moment it resolved.
                // Splitting them into two columns would need this rule back.
                //
                // The matcher grants `effective_scope`, so a decision that
                // authorized something else would hand out a breadth nobody
                // approved.
                if decision.verdict() == ApprovalVerdict::Granted
                    && decision.scope() != Some(effective_scope)
                {
                    return refuse("its effective scope is not the scope its decision authorized");
                }
            }
            (None, _) => {}
        }

        Ok(Self {
            id,
            pending,
            effective_scope,
            state,
            resolved_at,
            decision,
        })
    }

    /// Stable identity of this question.
    #[must_use]
    pub const fn id(&self) -> ApprovalId {
        self.id
    }

    /// Run the request belongs to.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.pending.run_id
    }

    /// Tool call the request is holding.
    #[must_use]
    pub const fn tool_call_id(&self) -> ToolCallId {
        self.pending.tool_call_id
    }

    /// Resolved tool identity and version the answer authorizes.
    #[must_use]
    pub const fn tool(&self) -> &ToolIdentity {
        &self.pending.tool
    }

    /// Capabilities the tool's descriptor declares, sorted and deduplicated.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.pending.capabilities
    }

    /// External identity hashes this request and any resulting grant bind to.
    #[must_use]
    pub const fn integration_identity(&self) -> IntegrationIdentity {
        self.pending.integration_identity
    }

    /// Canonical hash of the validated input.
    #[must_use]
    pub const fn input_hash(&self) -> InputHash {
        self.pending.input_hash
    }

    /// Human-readable digest of the input.
    #[must_use]
    pub fn input_summary(&self) -> &str {
        &self.pending.input_summary
    }

    /// Workspace identity the answer is bound to.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceBinding {
        &self.pending.workspace
    }

    /// Effective risk of the classified request.
    #[must_use]
    pub const fn risk(&self) -> RiskLevel {
        self.pending.risk
    }

    /// Scope the caller asked for, before the risk ceiling.
    #[must_use]
    pub const fn requested_scope(&self) -> ApprovalScope {
        self.pending.requested_scope
    }

    /// Scope the request may actually be granted at.
    ///
    /// Equal to [`requested_scope`](Self::requested_scope) unless the risk
    /// ceiling reduced it; the two are stored separately so the record shows the
    /// downgrade rather than hiding it.
    #[must_use]
    pub const fn effective_scope(&self) -> ApprovalScope {
        self.effective_scope
    }

    /// Whether the risk ceiling narrowed what was asked for.
    #[must_use]
    pub fn was_downgraded(&self) -> bool {
        self.effective_scope != self.pending.requested_scope
    }

    /// UTC time the request was recorded.
    #[must_use]
    pub const fn created_at(&self) -> OffsetDateTime {
        self.pending.created_at
    }

    /// Wall-clock expiry, when one was set.
    #[must_use]
    pub const fn expires_at(&self) -> Option<OffsetDateTime> {
        self.pending.expires_at
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ApprovalState {
        self.state
    }

    /// UTC time the request left `pending`, when it has.
    #[must_use]
    pub const fn resolved_at(&self) -> Option<OffsetDateTime> {
        self.resolved_at
    }

    /// The answer, present only once a human gave one.
    ///
    /// Absent for [`Expired`](ApprovalState::Expired),
    /// [`Cancelled`](ApprovalState::Cancelled), and
    /// [`Superseded`](ApprovalState::Superseded): nobody answered those, and
    /// synthesizing a denial here would make the audit claim a decision that was
    /// never made.
    #[must_use]
    pub const fn decision(&self) -> Option<&ApprovalDecision> {
        self.decision.as_ref()
    }

    /// Whether this request has passed a wall-clock expiry it was given.
    #[must_use]
    pub fn is_expired(&self, at: OffsetDateTime) -> bool {
        self.pending.expires_at.is_some_and(|expiry| at >= expiry)
    }

    /// The grant this request became, if it became one.
    ///
    /// `None` for every state but [`Granted`](ApprovalState::Granted), so the
    /// only way to hold an [`ApprovalGrant`] is to hold a request a human
    /// allowed. The grant carries the record's *effective* scope, which is what
    /// the decision authorized rather than what was asked for.
    #[must_use]
    pub fn grant(&self) -> Option<ApprovalGrant> {
        ApprovalGrant::of(self)
    }

    /// Records a human decision and the state change it produces.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::AlreadyResolved`] when the request is no longer
    /// pending — the refusal the loser of two concurrent decisions receives —
    /// [`ApprovalError::DecisionIdentityMismatch`] when the decision answers a
    /// different request, and [`ApprovalError::ScopeExceedsRequest`] when a
    /// grant asks for more breadth than the stored request allows.
    pub fn decide(&mut self, decision: ApprovalDecision) -> Result<(), ApprovalError> {
        if self.state.is_terminal() {
            return Err(ApprovalError::AlreadyResolved {
                id: self.id,
                state: self.state,
            });
        }
        if decision.approval_id != self.id {
            return Err(ApprovalError::DecisionIdentityMismatch {
                request: self.id,
                decision: decision.approval_id,
            });
        }
        // Enforced here rather than left to whoever owns the timer, because a
        // deadline nothing checks is advice. Refusing keeps the record pending;
        // closing it is [`expire`](Self::expire)'s job, and the waiter observes
        // the expiry rather than a late grant.
        if self.is_expired(decision.decided_at) {
            return Err(ApprovalError::Expired {
                id: self.id,
                expires_at: self.pending.expires_at.unwrap_or(decision.decided_at),
            });
        }
        if let Some(scope) = decision.scope
            && scope != self.effective_scope
            && scope != ApprovalScope::ExactCall
        {
            // Narrowing to the single call in front of a human is always
            // available; anything else is a different question. Re-checking here
            // rather than trusting the surface is what stops a front end from
            // turning a one-call ceiling into a run-wide grant by sending a
            // wider scope back.
            return Err(ApprovalError::ScopeExceedsRequest {
                id: self.id,
                effective: self.effective_scope,
                requested: scope,
            });
        }

        self.state = match decision.verdict {
            ApprovalVerdict::Granted => ApprovalState::Granted,
            ApprovalVerdict::Denied => ApprovalState::Denied,
        };
        self.resolved_at = Some(decision.decided_at);
        // The granted scope becomes the effective scope, so a human narrowing to
        // one call leaves a record whose breadth is what was actually allowed.
        if let Some(scope) = decision.scope {
            self.effective_scope = scope;
        }
        self.decision = Some(decision);
        Ok(())
    }

    /// Resolves a pending request without an answer.
    ///
    /// The only route to [`Expired`](ApprovalState::Expired),
    /// [`Cancelled`](ApprovalState::Cancelled), and
    /// [`Superseded`](ApprovalState::Superseded). Closing a window, dismissing a
    /// dialog, or losing the surface reaches none of them: absence of an answer
    /// is never consent and is never a resolution either.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::InvalidTransition`] when the edge is absent from
    /// [`APPROVAL_TRANSITIONS`] — which includes every attempt to resolve an
    /// already-resolved request, and every attempt to reach `granted` or
    /// `denied` without a decision.
    pub fn resolve(&mut self, to: ApprovalState, at: OffsetDateTime) -> Result<(), ApprovalError> {
        let decided = matches!(to, ApprovalState::Granted | ApprovalState::Denied);
        if decided || !APPROVAL_TRANSITIONS.contains(&(self.state, to)) {
            return Err(ApprovalError::InvalidTransition {
                id: self.id,
                from: self.state,
                to,
            });
        }
        self.state = to;
        self.resolved_at = Some(at.to_offset(UtcOffset::UTC));
        Ok(())
    }

    /// Resolves the request as [`Expired`](ApprovalState::Expired).
    ///
    /// # Errors
    ///
    /// As [`ApprovalRequest::resolve`].
    pub fn expire(&mut self, at: OffsetDateTime) -> Result<(), ApprovalError> {
        self.resolve(ApprovalState::Expired, at)
    }

    /// Resolves the request as [`Cancelled`](ApprovalState::Cancelled).
    ///
    /// # Errors
    ///
    /// As [`ApprovalRequest::resolve`].
    pub fn cancel(&mut self, at: OffsetDateTime) -> Result<(), ApprovalError> {
        self.resolve(ApprovalState::Cancelled, at)
    }

    /// Resolves the request as [`Superseded`](ApprovalState::Superseded).
    ///
    /// # Errors
    ///
    /// As [`ApprovalRequest::resolve`].
    pub fn supersede(&mut self, at: OffsetDateTime) -> Result<(), ApprovalError> {
        self.resolve(ApprovalState::Superseded, at)
    }
}

fn validate_integration_identity(
    capabilities: &[Capability],
    identity: IntegrationIdentity,
) -> Result<(), &'static str> {
    let external = capabilities
        .iter()
        .filter_map(ExternalCapability::from_capability)
        .collect::<Vec<_>>();
    match external.as_slice() {
        [] if identity.is_empty() => Ok(()),
        [] => Err("a local operation cannot carry external integration identity"),
        [capability] => capability.validate_identity_shape(identity),
        _ => Err("an approval must describe exactly one external operation"),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::json;
    use time::OffsetDateTime;

    use crate::approval::canonical_input_hash;
    use crate::domain::{ApprovalId, RunId, ToolCallId};
    use crate::tool::{Capability, RiskLevel, ToolIdentity};

    use super::{
        APPROVAL_TRANSITIONS, ApprovalDecision, ApprovalRequest, ApprovalScope, ApprovalState,
        ApprovalVerdict, DecidedVia, PendingApproval, WorkspaceBinding,
    };

    pub(in crate::approval) fn at(offset: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
    }

    pub(in crate::approval) fn workspace() -> WorkspaceBinding {
        WorkspaceBinding::new(
            Some(
                "55555555-5555-4555-8555-555555555555"
                    .parse::<harkness_core::ProjectId>()
                    .unwrap(),
            ),
            "/workspace/harkness",
        )
    }

    pub(in crate::approval) fn pending(risk: RiskLevel) -> PendingApproval {
        PendingApproval::new(
            RunId::new(),
            ToolCallId::new(),
            ToolIdentity::parse("fs.write", "1.2.0").unwrap(),
            canonical_input_hash(&json!({"path": "src/lib.rs"})).unwrap(),
            workspace(),
            risk,
            at(0),
        )
        .with_capabilities([Capability::new("fs.write").unwrap()])
        .summarized_as("write 12 lines to src/lib.rs")
    }

    fn request(risk: RiskLevel, scope: ApprovalScope) -> ApprovalRequest {
        ApprovalRequest::open(pending(risk).requesting(scope)).unwrap()
    }

    // -- lifecycle -----------------------------------------------------------

    #[test]
    fn every_state_and_scope_spelling_round_trips_through_its_stored_form() {
        for state in ApprovalState::ALL.iter().copied() {
            assert_eq!(ApprovalState::from_stored(state.as_str()), Some(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        for scope in ApprovalScope::ALL.iter().copied() {
            assert_eq!(ApprovalScope::from_stored(scope.as_str()), Some(scope));
        }
        for verdict in ApprovalVerdict::ALL.iter().copied() {
            assert_eq!(
                ApprovalVerdict::from_stored(verdict.as_str()),
                Some(verdict)
            );
        }
        for via in DecidedVia::ALL.iter().copied() {
            assert_eq!(DecidedVia::from_stored(via.as_str()), Some(via));
        }
        assert_eq!(ApprovalState::from_stored("Pending"), None);
        assert_eq!(ApprovalScope::from_stored("exactCall"), None);
    }

    #[test]
    fn only_pending_has_outgoing_edges_and_reaches_every_other_state() {
        assert!(
            APPROVAL_TRANSITIONS
                .iter()
                .all(|(from, _)| !from.is_terminal())
        );
        let reached = APPROVAL_TRANSITIONS
            .iter()
            .map(|(_, to)| *to)
            .collect::<Vec<_>>();
        assert_eq!(reached, &ApprovalState::ALL[1..]);
    }

    #[test]
    fn every_invalid_edge_is_refused_and_every_valid_one_is_reachable() {
        for from in ApprovalState::ALL.iter().copied() {
            for to in ApprovalState::ALL.iter().copied() {
                let mut record = request(RiskLevel::WorkspaceWrite, ApprovalScope::ExactCall);
                if from != ApprovalState::Pending {
                    force(&mut record, from);
                }
                let outcome = record.resolve(to, at(5));
                // `resolve` deliberately never reaches a decided state: those
                // are only reachable by recording the decision itself.
                let expected = APPROVAL_TRANSITIONS.contains(&(from, to))
                    && !matches!(to, ApprovalState::Granted | ApprovalState::Denied);
                assert_eq!(outcome.is_ok(), expected, "{from} -> {to}");
                if let Err(error) = outcome {
                    assert_eq!(error.kind(), "approval_invalid_transition");
                }
            }
        }
    }

    /// Drives a record into `state` through its own public API.
    fn force(record: &mut ApprovalRequest, state: ApprovalState) {
        match state {
            ApprovalState::Pending => {}
            ApprovalState::Granted => record
                .decide(ApprovalDecision::grant(
                    record.id(),
                    ApprovalScope::ExactCall,
                    DecidedVia::Cli,
                    at(1),
                ))
                .unwrap(),
            ApprovalState::Denied => record
                .decide(ApprovalDecision::deny(record.id(), DecidedVia::Cli, at(1)))
                .unwrap(),
            ApprovalState::Expired => record.expire(at(1)).unwrap(),
            ApprovalState::Cancelled => record.cancel(at(1)).unwrap(),
            ApprovalState::Superseded => record.supersede(at(1)).unwrap(),
        }
    }

    #[test]
    fn a_denied_request_can_never_become_granted() {
        let mut record = request(RiskLevel::WorkspaceWrite, ApprovalScope::ExactCall);
        record
            .decide(ApprovalDecision::deny(record.id(), DecidedVia::Gui, at(1)))
            .unwrap();

        let error = record
            .decide(ApprovalDecision::grant(
                record.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(2),
            ))
            .unwrap_err();

        assert_eq!(error.kind(), "approval_already_resolved");
        assert_eq!(record.state(), ApprovalState::Denied);
        assert!(record.resolve(ApprovalState::Granted, at(3)).is_err());
    }

    #[test]
    fn a_decision_for_another_request_is_refused() {
        let mut record = request(RiskLevel::Execute, ApprovalScope::ExactCall);
        let error = record
            .decide(ApprovalDecision::grant(
                ApprovalId::new(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(1),
            ))
            .unwrap_err();

        assert_eq!(error.kind(), "approval_decision_identity_mismatch");
        assert_eq!(record.state(), ApprovalState::Pending);
    }

    // -- scope ceilings ------------------------------------------------------

    #[test]
    fn remote_write_and_destructive_requests_are_stored_as_one_call_approvals() {
        for risk in [RiskLevel::RemoteWrite, RiskLevel::Destructive] {
            for asked in ApprovalScope::ALL.iter().copied() {
                let record = request(risk, asked);
                assert_eq!(record.requested_scope(), asked, "{risk:?}/{asked}");
                assert_eq!(record.effective_scope(), ApprovalScope::ExactCall);
                assert_eq!(record.was_downgraded(), asked != ApprovalScope::ExactCall);
            }
        }
    }

    #[test]
    fn every_milder_risk_keeps_the_scope_that_was_asked_for() {
        for risk in [
            RiskLevel::Observe,
            RiskLevel::WorkspaceWrite,
            RiskLevel::Execute,
            RiskLevel::Network,
        ] {
            for asked in ApprovalScope::ALL.iter().copied() {
                let record = request(risk, asked);
                assert_eq!(record.effective_scope(), asked, "{risk:?}/{asked}");
                assert!(!record.was_downgraded());
            }
        }
    }

    #[test]
    fn a_downgraded_request_cannot_be_granted_at_the_scope_it_asked_for() {
        let mut record = request(RiskLevel::RemoteWrite, ApprovalScope::ToolForRun);

        let error = record
            .decide(ApprovalDecision::grant(
                record.id(),
                ApprovalScope::ToolForRun,
                DecidedVia::Gui,
                at(1),
            ))
            .unwrap_err();

        assert_eq!(error.kind(), "approval_scope_exceeds_request");
        assert_eq!(record.state(), ApprovalState::Pending);

        record
            .decide(ApprovalDecision::grant(
                record.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(2),
            ))
            .unwrap();
        assert_eq!(record.state(), ApprovalState::Granted);
    }

    #[test]
    fn a_human_may_narrow_a_run_wide_request_to_one_call() {
        let mut record = request(RiskLevel::Execute, ApprovalScope::ToolForRun);

        record
            .decide(ApprovalDecision::grant(
                record.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                at(1),
            ))
            .unwrap();

        assert_eq!(record.requested_scope(), ApprovalScope::ToolForRun);
        assert_eq!(
            record.effective_scope(),
            ApprovalScope::ExactCall,
            "the record must show what was allowed, not what was asked"
        );
    }

    #[test]
    fn a_capability_request_with_no_capability_is_refused_at_creation() {
        let error = ApprovalRequest::open(
            PendingApproval::new(
                RunId::new(),
                ToolCallId::new(),
                ToolIdentity::parse("net.fetch", "1.0.0").unwrap(),
                canonical_input_hash(&json!({})).unwrap(),
                workspace(),
                RiskLevel::Network,
                at(0),
            )
            .requesting(ApprovalScope::CapabilityForRun),
        )
        .unwrap_err();

        assert_eq!(error.kind(), "approval_scope_requires_capability");
    }

    #[test]
    fn a_capability_request_downgraded_to_one_call_no_longer_needs_a_capability() {
        // The ceiling runs first, so a destructive request that asked for a
        // capability scope is stored as an exact call and never has to invent a
        // capability it does not declare.
        let record = ApprovalRequest::open(
            PendingApproval::new(
                RunId::new(),
                ToolCallId::new(),
                ToolIdentity::parse("git.push", "1.0.0").unwrap(),
                canonical_input_hash(&json!({})).unwrap(),
                workspace(),
                RiskLevel::Destructive,
                at(0),
            )
            .requesting(ApprovalScope::CapabilityForRun),
        )
        .unwrap();

        assert_eq!(record.effective_scope(), ApprovalScope::ExactCall);
        assert!(record.capabilities().is_empty());
    }

    // -- record shape --------------------------------------------------------

    #[test]
    fn a_fresh_request_is_pending_with_no_decision_and_no_resolution_time() {
        let record = request(RiskLevel::Execute, ApprovalScope::ExactCall);
        assert_eq!(record.state(), ApprovalState::Pending);
        assert!(record.decision().is_none());
        assert!(record.resolved_at().is_none());
        assert!(!record.is_expired(at(1_000_000)));
    }

    #[test]
    fn an_unanswered_resolution_records_no_decision() {
        for state in [
            ApprovalState::Expired,
            ApprovalState::Cancelled,
            ApprovalState::Superseded,
        ] {
            let mut record = request(RiskLevel::Execute, ApprovalScope::ExactCall);
            record.resolve(state, at(9)).unwrap();
            assert_eq!(record.state(), state);
            assert_eq!(record.resolved_at(), Some(at(9)));
            assert!(
                record.decision().is_none(),
                "nobody answered, so nothing may be recorded as an answer"
            );
        }
    }

    #[test]
    fn expiry_is_read_against_a_supplied_instant_rather_than_a_clock() {
        let record =
            ApprovalRequest::open(pending(RiskLevel::Execute).expiring_at(at(60))).unwrap();

        assert!(!record.is_expired(at(59)));
        assert!(record.is_expired(at(60)));
        assert!(record.is_expired(at(61)));
    }

    #[test]
    fn an_answer_after_the_deadline_cannot_grant_the_request_it_lapsed_on() {
        // The deadline's whole job. A stale dialog answered an hour late must
        // not produce a live grant, and enforcing it here rather than leaving it
        // to whoever owns the timer is what stops it being advice.
        for scope in ApprovalScope::ALL.iter().copied() {
            let mut record = ApprovalRequest::open(
                pending(RiskLevel::Execute)
                    .requesting(scope)
                    .expiring_at(at(60)),
            )
            .unwrap();

            let error = record
                .decide(ApprovalDecision::grant(
                    record.id(),
                    scope,
                    DecidedVia::Gui,
                    at(60),
                ))
                .unwrap_err();

            assert_eq!(error.kind(), "approval_expired", "{scope}");
            assert_eq!(record.state(), ApprovalState::Pending);
            assert!(record.grant().is_none());

            // A denial is refused too: the question is over either way, and the
            // record is closed by expiring it rather than by answering it late.
            assert_eq!(
                record
                    .decide(ApprovalDecision::deny(record.id(), DecidedVia::Cli, at(61)))
                    .unwrap_err()
                    .kind(),
                "approval_expired"
            );
            record.expire(at(61)).unwrap();
            assert_eq!(record.state(), ApprovalState::Expired);
        }
    }

    #[test]
    fn an_answer_inside_the_deadline_is_unaffected_by_it() {
        let mut record =
            ApprovalRequest::open(pending(RiskLevel::Execute).expiring_at(at(60))).unwrap();
        record
            .decide(ApprovalDecision::grant(
                record.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Gui,
                at(59),
            ))
            .unwrap();

        assert_eq!(record.state(), ApprovalState::Granted);
        assert!(
            record.grant().is_some(),
            "a grant given in time outlives the deadline for giving it"
        );
    }

    #[test]
    fn a_decision_carries_its_surface_reason_and_utc_timestamp() {
        let mut record = request(RiskLevel::Execute, ApprovalScope::ExactCall);
        let shifted = at(5).to_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap());
        record
            .decide(
                ApprovalDecision::grant(
                    record.id(),
                    ApprovalScope::ExactCall,
                    DecidedVia::Gui,
                    shifted,
                )
                .because("the diff is what I asked for"),
            )
            .unwrap();

        let decision = record.decision().unwrap();
        assert_eq!(decision.verdict(), ApprovalVerdict::Granted);
        assert_eq!(decision.decided_via(), DecidedVia::Gui);
        assert_eq!(decision.reason(), Some("the diff is what I asked for"));
        assert_eq!(decision.decided_at().offset(), time::UtcOffset::UTC);
        assert_eq!(decision.decided_at(), at(5));
    }

    #[test]
    fn a_denial_carries_no_scope() {
        let decision = ApprovalDecision::deny(ApprovalId::new(), DecidedVia::Cli, at(1));
        assert!(decision.scope().is_none());
    }

    #[test]
    fn capabilities_are_sorted_and_deduplicated_like_the_descriptor_declares_them() {
        let record = ApprovalRequest::open(pending(RiskLevel::Network).with_capabilities([
            Capability::new("network").unwrap(),
            Capability::new("fs.write").unwrap(),
            Capability::new("network").unwrap(),
        ]))
        .unwrap();

        assert_eq!(
            record
                .capabilities()
                .iter()
                .map(Capability::as_str)
                .collect::<Vec<_>>(),
            ["fs.write", "network"]
        );
    }

    #[test]
    fn identity_bearing_external_approvals_cannot_omit_their_identity() {
        let error = ApprovalRequest::open(
            pending(RiskLevel::Execute)
                .with_capabilities([Capability::new("invoke_mcp_tool").unwrap()]),
        )
        .unwrap_err();
        assert_eq!(error.kind(), "approval_invalid_integration_identity");
    }

    #[test]
    fn a_workspace_binding_needs_both_halves_to_compare_equal() {
        let other = "66666666-6666-4666-8666-666666666666"
            .parse::<harkness_core::ProjectId>()
            .unwrap();
        assert_ne!(
            workspace(),
            WorkspaceBinding::new(Some(other), "/workspace/harkness")
        );
        assert_ne!(
            workspace(),
            WorkspaceBinding::new(workspace().project_id(), "/workspace/other")
        );
        assert_ne!(
            workspace(),
            WorkspaceBinding::new(None, "/workspace/harkness")
        );
    }
}
