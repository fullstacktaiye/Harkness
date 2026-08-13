use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use time::{OffsetDateTime, UtcOffset};

use crate::domain::InvalidTransition;

use super::error::{IntegrationDomainError, invalid_record};
use super::state::{InvalidationReason, TrustCheck, TrustScope, TrustState};
use super::subject::{IdentityBasis, SubjectKind};

/// Record name used by this module's typed errors.
pub(super) const RECORD: &str = "trust_record";

/// One subject as it exists *now*, for checking a record against.
///
/// The workspace travels beside the identity rather than inside it because it
/// is a fact about the observation, not about the subject: the same MCP server
/// observed from two checkouts is one subject seen twice. Carrying it here is
/// what lets [`TrustRecord::check`] answer for a workspace-scoped grant without
/// a second entry point that a caller could forget to use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedIdentity {
    basis: IdentityBasis,
    workspace: Option<PathBuf>,
}

impl ObservedIdentity {
    /// Records an observation made outside any particular workspace.
    #[must_use]
    pub const fn new(basis: IdentityBasis) -> Self {
        Self {
            basis,
            workspace: None,
        }
    }

    /// Names the canonical workspace root the subject was observed in.
    #[must_use]
    pub fn in_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace = Some(root.into());
        self
    }

    /// The identity that was observed.
    #[must_use]
    pub const fn basis(&self) -> &IdentityBasis {
        &self.basis
    }

    /// Canonical workspace root the subject was observed in, when one applies.
    #[must_use]
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }
}

/// One durable trust grant, bound to the exact identity it was made about.
///
/// A record is created by [`grant`](Self::grant) and is therefore always a
/// grant: [`TrustState::Untrusted`] is what a *lookup* answers when no record
/// matches, and never the state of a record that exists.
///
/// # There is no structural key for "the same grant"
///
/// It is tempting to treat `(subject kind, identity basis, scope)` as a natural
/// key a store can deduplicate on. It cannot be one, and the reason is worth
/// stating because the alternative looks reasonable right up until it loses a
/// user's decision.
///
/// [`check`](Self::check) does not compare bases for equality. It ignores the
/// display name and the executable path, and it accepts a semver-compatible
/// upgrade — so an agent trusted at `1.4.2` and observed at `1.4.3` is the same
/// grant, while two records holding those two bases are not structurally equal.
/// A key over a *compatibility relation* is not a key. Worse, a revoked record
/// and a later grant about the same subject would collide on such a key, and a
/// store upserting on it would overwrite the revocation this state machine
/// exists to preserve.
///
/// The matching relation is [`check`](Self::check) itself: find the records for
/// a subject, then ask each one about the observation in front of you. A store
/// therefore addresses a row by its own row identity — never by a projection of
/// these fields, which [`regrant`](Self::regrant) deliberately moves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustRecord {
    subject_kind: SubjectKind,
    identity_basis: IdentityBasis,
    scope: TrustScope,
    state: TrustState,
    invalidation_reason: Option<InvalidationReason>,
    granted_at: OffsetDateTime,
}

impl TrustRecord {
    /// Records that a user trusted one subject at one exact identity.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidTimestamp`] when `granted_at`
    /// does not carry the UTC offset. The timestamp is refused rather than
    /// converted, because a grant time written in another offset means the
    /// caller is not producing the spelling every other durable record uses.
    ///
    /// Returns [`IntegrationDomainError::MissingIdentityEvidence`] when the
    /// basis carries none of the evidence its subject kind is recognized by,
    /// and [`IntegrationDomainError::InvalidRecord`] when a workspace scope
    /// names a root that is not absolute.
    pub fn grant(
        subject_kind: SubjectKind,
        identity_basis: IdentityBasis,
        scope: TrustScope,
        granted_at: OffsetDateTime,
    ) -> Result<Self, IntegrationDomainError> {
        validate_utc("granted_at", granted_at)?;
        validate_scope(&scope)?;
        require_evidence(subject_kind, &identity_basis, &scope)?;
        Ok(Self {
            subject_kind,
            identity_basis,
            scope,
            state: TrustState::Trusted,
            invalidation_reason: None,
            granted_at,
        })
    }

    pub(super) fn from_parts(
        subject_kind: SubjectKind,
        identity_basis: IdentityBasis,
        scope: TrustScope,
        state: TrustState,
        invalidation_reason: Option<InvalidationReason>,
        granted_at: OffsetDateTime,
    ) -> Result<Self, IntegrationDomainError> {
        validate_utc("granted_at", granted_at)?;
        validate_scope(&scope)?;
        require_evidence(subject_kind, &identity_basis, &scope)?;
        if state == TrustState::Untrusted {
            return Err(IntegrationDomainError::InvalidRecord {
                record: RECORD,
                reason: "an untrusted subject is the absence of a record, not a record",
            });
        }
        if state.requires_invalidation_reason() != invalidation_reason.is_some() {
            return Err(IntegrationDomainError::InvalidRecord {
                record: RECORD,
                reason: "an invalidation reason is required by invalidated and permitted nowhere else",
            });
        }
        Ok(Self {
            subject_kind,
            identity_basis,
            scope,
            state,
            invalidation_reason,
            granted_at,
        })
    }

    /// Kind of subject this grant is about.
    #[must_use]
    pub const fn subject_kind(&self) -> SubjectKind {
        self.subject_kind
    }

    /// The exact identity that was trusted.
    #[must_use]
    pub const fn identity_basis(&self) -> &IdentityBasis {
        &self.identity_basis
    }

    /// How far this grant reaches.
    #[must_use]
    pub const fn scope(&self) -> &TrustScope {
        &self.scope
    }

    /// Current state of the grant.
    #[must_use]
    pub const fn state(&self) -> TrustState {
        self.state
    }

    /// Why the grant was invalidated, when it was.
    #[must_use]
    pub const fn invalidation_reason(&self) -> Option<InvalidationReason> {
        self.invalidation_reason
    }

    /// When the grant now held was made.
    ///
    /// A re-grant after invalidation moves this forward, so the timestamp
    /// always names the decision that is in force rather than the first one
    /// ever made about the subject.
    #[must_use]
    pub const fn granted_at(&self) -> OffsetDateTime {
        self.granted_at
    }

    /// Decides whether this grant still describes the subject in front of you.
    ///
    /// Pure: it reads no clock, opens no file, and hashes nothing. Callers
    /// observe the current identity — an adapter reports the path, digest,
    /// version and fingerprint it saw — and this compares it against what was
    /// trusted.
    ///
    /// A record that is not [`Trusted`](TrustState::Trusted) yields
    /// [`NotTrusted`](TrustCheck::NotTrusted) without comparing anything: a
    /// revoked or already-invalidated grant authorizes nothing regardless of
    /// what is observed.
    ///
    /// Every comparison treats a *missing* observed field as a difference, so
    /// an executable that has been deleted, a server that stopped reporting a
    /// protocol version, and a recipe whose content could not be read all
    /// invalidate rather than passing by absence. When more than one trigger
    /// fires, the reported reason follows
    /// [`InvalidationReason::PRECEDENCE`].
    #[must_use]
    pub fn check(&self, observed: &ObservedIdentity) -> TrustCheck {
        if self.state != TrustState::Trusted {
            return TrustCheck::NotTrusted;
        }
        if let Some(root) = self.scope.root()
            && observed.workspace() != Some(root)
        {
            return TrustCheck::Invalidate(InvalidationReason::WorkspacePathChanged);
        }
        for &(reason, differs) in IDENTITY_CHECKS {
            if differs(&self.identity_basis, observed.basis()) {
                return TrustCheck::Invalidate(reason);
            }
        }
        TrustCheck::Valid
    }

    /// Withdraws the grant on an explicit user decision.
    ///
    /// Legal from [`Trusted`](TrustState::Trusted) and from
    /// [`Invalidated`](TrustState::Invalidated). The second is the re-prompt a
    /// user declined: without it, a refusal after drift would leave the record
    /// saying only what it already said before anybody was asked.
    ///
    /// Any invalidation reason is cleared, because the record now says the user
    /// refused rather than that a check found drift — the drift is history, and
    /// the reason field describes the state the record is *in*.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidTrustTransition`] when the
    /// grant is already [`Revoked`](TrustState::Revoked).
    pub fn revoke(&mut self) -> Result<(), IntegrationDomainError> {
        self.transition(TrustState::Revoked)?;
        self.invalidation_reason = None;
        Ok(())
    }

    /// Records that the subject drifted from the identity that was trusted.
    ///
    /// The reason is attached in the same step as the transition, so a record
    /// cannot exist in [`Invalidated`](TrustState::Invalidated) without saying
    /// which of the eight triggers fired.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidTrustTransition`] unless the
    /// grant is currently [`Trusted`](TrustState::Trusted).
    pub fn invalidate(&mut self, reason: InvalidationReason) -> Result<(), IntegrationDomainError> {
        self.transition(TrustState::Invalidated)?;
        self.invalidation_reason = Some(reason);
        Ok(())
    }

    /// Re-affirms the grant against the identity that is there now.
    ///
    /// This is the answer to a re-prompt, so it rebases the identity basis and
    /// moves `granted_at`: the record then describes what the user was actually
    /// shown, rather than the identity that had already stopped existing.
    ///
    /// Only an invalidated grant can be re-granted this way. A revoked one is
    /// terminal — see [`TrustState::is_terminal`] — and a fresh decision about
    /// it is a new record.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidTrustTransition`] unless the
    /// grant is currently [`Invalidated`](TrustState::Invalidated), and
    /// [`IntegrationDomainError::InvalidTimestamp`] when `granted_at` does not
    /// carry the UTC offset, and
    /// [`IntegrationDomainError::MissingIdentityEvidence`] when the replacement
    /// basis carries none of the evidence the subject kind is recognized by —
    /// a re-grant must not be the way a record loses the field that made it
    /// checkable.
    pub fn regrant(
        &mut self,
        identity_basis: IdentityBasis,
        granted_at: OffsetDateTime,
    ) -> Result<(), IntegrationDomainError> {
        validate_utc("granted_at", granted_at)?;
        require_evidence(self.subject_kind, &identity_basis, &self.scope)?;
        self.transition(TrustState::Trusted)?;
        self.identity_basis = identity_basis;
        self.invalidation_reason = None;
        self.granted_at = granted_at;
        Ok(())
    }

    fn transition(&mut self, to: TrustState) -> Result<(), IntegrationDomainError> {
        if !self.state.can_become(to) {
            return Err(InvalidTransition {
                from: self.state,
                to,
            }
            .into());
        }
        self.state = to;
        Ok(())
    }
}

/// Refuses a grant whose basis holds nothing its subject kind is known by.
///
/// Every basis field is optional, because no subject has all of them — an MCP
/// tool has a schema fingerprint and no executable, a recipe has a content hash
/// and no endpoint. That flexibility has one sharp edge: a basis carrying *no*
/// evidence at all reduces [`TrustRecord::check`] to comparing the fields both
/// sides leave empty, which is to say it answers `Valid` for every observation
/// ever made. A recipe registered without its content hash would then keep its
/// grant through any edit — the precise failure the module exists to prevent,
/// arriving silently.
///
/// So each kind names what it is recognized by, and a grant that cannot supply
/// it is refused at construction rather than mis-answering forever afterwards.
/// ADR-0016 puts it as: where an identity is genuinely unavailable, the record
/// says so rather than substituting a name.
fn require_evidence(
    subject_kind: SubjectKind,
    basis: &IdentityBasis,
    scope: &TrustScope,
) -> Result<(), IntegrationDomainError> {
    let (present, required) = match subject_kind {
        SubjectKind::AgentExecutable => (
            basis.executable().is_some(),
            "an executable path and content digest",
        ),
        // ADR-0012 fixes stdio transports, so a server is normally a local
        // executable; an endpoint is accepted beside it rather than instead of
        // it, so this rule does not have to be revisited to describe one.
        SubjectKind::McpServer => (
            basis.executable().is_some() || basis.endpoint().is_some(),
            "an executable or an endpoint",
        ),
        SubjectKind::McpToolSchema => (
            basis.schema_fingerprint().is_some(),
            "a tool schema fingerprint",
        ),
        SubjectKind::Recipe => (basis.content_hash().is_some(), "a recipe content hash"),
        SubjectKind::ForgeAccount => (basis.endpoint().is_some(), "an endpoint"),
        SubjectKind::ForgeRepository => (
            basis
                .endpoint()
                .is_some_and(|endpoint| endpoint.resource().is_some()),
            "an endpoint naming the repository on its host",
        ),
        // A workspace is identified by where it is, which is the scope rather
        // than a field of the basis.
        SubjectKind::Workspace => (scope.root().is_some(), "a workspace-scoped grant"),
    };
    if present {
        return Ok(());
    }
    Err(IntegrationDomainError::MissingIdentityEvidence {
        subject_kind,
        required,
    })
}

/// Refuses a workspace scope that names a root no observation can match.
///
/// `TrustScope` is a plain data enum whose variant field is public, so a
/// caller can build one without going through [`TrustScope::workspace`]. The
/// check therefore belongs here, where every record passes, rather than on the
/// constructor it could route around. A relative or empty root names a
/// different directory from every working directory, so a grant carrying one
/// fails closed *silently* — the user is re-prompted forever with
/// `WorkspacePathChanged` and never learns why.
fn validate_scope(scope: &TrustScope) -> Result<(), IntegrationDomainError> {
    let Some(root) = scope.root() else {
        return Ok(());
    };
    if root.as_os_str().is_empty() {
        return Err(invalid_record(
            RECORD,
            "a workspace scope cannot name an empty root",
        ));
    }
    if !root.is_absolute() {
        return Err(invalid_record(
            RECORD,
            "a workspace scope must name an absolute root",
        ));
    }
    Ok(())
}

/// One comparison between the identity that was trusted and the one observed.
type IdentityComparison = fn(&IdentityBasis, &IdentityBasis) -> bool;

/// The seven identity comparisons, in [`InvalidationReason::PRECEDENCE`] order.
///
/// The precedence is this table rather than a sequence of hand-written `if`s,
/// so the documented order and the order actually applied cannot drift — a test
/// asserts the two agree.
const IDENTITY_CHECKS: &[(InvalidationReason, IdentityComparison)] = &[
    (
        InvalidationReason::ExecutableHashChanged,
        executable_hash_changed,
    ),
    (
        InvalidationReason::EndpointHostChanged,
        endpoint_host_changed,
    ),
    (
        InvalidationReason::RepositoryRemoteChanged,
        endpoint_resource_changed,
    ),
    (
        InvalidationReason::ToolSchemaFingerprintChanged,
        schema_fingerprint_changed,
    ),
    (
        InvalidationReason::RecipeContentHashChanged,
        content_hash_changed,
    ),
    (
        InvalidationReason::CapabilityExpansion,
        authority_surface_widened,
    ),
    (
        InvalidationReason::IncompatibleVersionChange,
        version_is_incompatible,
    ),
];

/// The executable's path is deliberately not compared; see
/// [`ExecutableIdentity`](super::ExecutableIdentity).
fn executable_hash_changed(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted.executable().map(super::ExecutableIdentity::sha256)
        != observed.executable().map(super::ExecutableIdentity::sha256)
}

fn endpoint_host_changed(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted.endpoint().map(super::EndpointIdentity::host)
        != observed.endpoint().map(super::EndpointIdentity::host)
}

fn endpoint_resource_changed(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted
        .endpoint()
        .and_then(super::EndpointIdentity::resource)
        != observed
            .endpoint()
            .and_then(super::EndpointIdentity::resource)
}

fn schema_fingerprint_changed(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted.schema_fingerprint() != observed.schema_fingerprint()
}

fn content_hash_changed(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted.content_hash() != observed.content_hash()
}

/// Whether the subject may now do more, or is now controlled by someone else.
///
/// Narrowing is not expansion: a subject that dropped a capability is still
/// covered by the grant that allowed it. A changed configuration source is,
/// because who may edit a subject's configuration is part of what the user
/// agreed to — a server the repository now supplies is not the server the user
/// wrote into their own configuration, whichever direction the move went.
fn authority_surface_widened(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    trusted.configuration_source() != observed.configuration_source()
        || observed
            .capabilities()
            .any(|capability| !trusted.declares_capability(capability))
}

/// Whether either version the subject reports is incompatible with the grant.
///
/// The protocol revision is compared verbatim: ADR-0013 and ADR-0014 pin which
/// revisions Harkness speaks, and a subject that switched revision is a
/// user-visible event by design. The subject's own version is compared by
/// semantic-version compatibility where both spellings parse, so a patch
/// release keeps its grant while a major bump does not. A downgrade is
/// incompatible in both directions of that rule, because an older build can
/// reintroduce whatever the newer one removed.
fn version_is_incompatible(trusted: &IdentityBasis, observed: &IdentityBasis) -> bool {
    if trusted.protocol_version() != observed.protocol_version() {
        return true;
    }
    match (trusted.subject_version(), observed.subject_version()) {
        (None, None) => false,
        (Some(trusted), Some(observed)) => !versions_are_compatible(trusted, observed),
        (Some(_), None) | (None, Some(_)) => true,
    }
}

fn versions_are_compatible(trusted: &str, observed: &str) -> bool {
    if trusted == observed {
        return true;
    }
    let (Ok(trusted), Ok(observed)) = (Version::parse(trusted), Version::parse(observed)) else {
        // A version this build cannot order is compared as an opaque string, so
        // any difference is a change nobody can rule compatible.
        return false;
    };
    VersionReq::parse(&format!("^{trusted}")).is_ok_and(|compatible| compatible.matches(&observed))
}

fn validate_utc(
    field: &'static str,
    timestamp: OffsetDateTime,
) -> Result<(), IntegrationDomainError> {
    if timestamp.offset() == UtcOffset::UTC {
        return Ok(());
    }
    Err(IntegrationDomainError::InvalidTimestamp {
        record: RECORD,
        field,
        reason: "must use the UTC offset",
    })
}

#[cfg(test)]
pub(super) mod tests {
    use time::OffsetDateTime;

    use super::{IDENTITY_CHECKS, ObservedIdentity, TrustRecord};
    use crate::integration::{
        ConfigurationSource, EndpointIdentity, ExecutableIdentity, IdentityBasis,
        InvalidationReason, Sha256Hash, SubjectKind, TrustCheck, TrustScope, TrustState,
    };

    pub(in crate::integration) fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    pub(in crate::integration) fn hash(seed: &str) -> Sha256Hash {
        Sha256Hash::of(seed.as_bytes())
    }

    /// The identity of a trusted ACP agent, carrying every field an agent has.
    pub(in crate::integration) fn agent_basis() -> IdentityBasis {
        IdentityBasis::new("Example agent", ConfigurationSource::User)
            .unwrap()
            .versioned("1.4.2")
            .unwrap()
            .speaking("1")
            .unwrap()
            .launched_from(
                ExecutableIdentity::new("/usr/local/bin/example-agent", hash("agent-binary"))
                    .unwrap(),
            )
            .declaring(["fs.read", "terminal"])
            .unwrap()
    }

    pub(in crate::integration) fn agent_record() -> TrustRecord {
        TrustRecord::grant(
            SubjectKind::AgentExecutable,
            agent_basis(),
            TrustScope::workspace("/workspace"),
            at(0),
        )
        .unwrap()
    }

    fn observed(basis: IdentityBasis) -> ObservedIdentity {
        ObservedIdentity::new(basis).in_workspace("/workspace")
    }

    fn assert_invalidates(basis: IdentityBasis, expected: InvalidationReason) {
        assert_eq!(
            agent_record().check(&observed(basis)),
            TrustCheck::Invalidate(expected)
        );
    }

    #[test]
    fn a_grant_starts_trusted_with_no_invalidation_reason() {
        let record = agent_record();
        assert_eq!(record.state(), TrustState::Trusted);
        assert_eq!(record.invalidation_reason(), None);
        assert_eq!(record.granted_at(), at(0));
        assert_eq!(record.subject_kind(), SubjectKind::AgentExecutable);
        assert_eq!(record.scope(), &TrustScope::workspace("/workspace"));
    }

    #[test]
    fn a_grant_refuses_a_timestamp_that_is_not_utc() {
        let offset = at(0).to_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
        let error = TrustRecord::grant(
            SubjectKind::Recipe,
            agent_basis(),
            TrustScope::Global,
            offset,
        )
        .unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_timestamp");
    }

    #[test]
    fn an_unchanged_identity_stays_valid() {
        assert_eq!(
            agent_record().check(&observed(agent_basis())),
            TrustCheck::Valid
        );
    }

    #[test]
    fn a_record_that_is_not_trusted_authorizes_nothing() {
        let mut revoked = agent_record();
        revoked.revoke().unwrap();
        assert_eq!(
            revoked.check(&observed(agent_basis())),
            TrustCheck::NotTrusted
        );

        let mut invalidated = agent_record();
        invalidated
            .invalidate(InvalidationReason::ExecutableHashChanged)
            .unwrap();
        assert_eq!(
            invalidated.check(&observed(agent_basis())),
            TrustCheck::NotTrusted
        );
    }

    #[test]
    fn each_trigger_reports_its_own_reason() {
        // Workspace path: observed somewhere else, and observed nowhere.
        assert_eq!(
            agent_record().check(&ObservedIdentity::new(agent_basis()).in_workspace("/elsewhere")),
            TrustCheck::Invalidate(InvalidationReason::WorkspacePathChanged)
        );
        assert_eq!(
            agent_record().check(&ObservedIdentity::new(agent_basis())),
            TrustCheck::Invalidate(InvalidationReason::WorkspacePathChanged)
        );

        assert_invalidates(
            agent_basis().launched_from(
                ExecutableIdentity::new("/usr/local/bin/example-agent", hash("other-binary"))
                    .unwrap(),
            ),
            InvalidationReason::ExecutableHashChanged,
        );

        let endpoint_record = TrustRecord::grant(
            SubjectKind::ForgeRepository,
            IdentityBasis::new("octocat/hello", ConfigurationSource::User)
                .unwrap()
                .reached_at(
                    EndpointIdentity::new("github.com", Some("octocat/hello".to_owned())).unwrap(),
                ),
            TrustScope::Global,
            at(0),
        )
        .unwrap();
        let repointed = |host: &str, resource: &str| {
            ObservedIdentity::new(
                IdentityBasis::new("octocat/hello", ConfigurationSource::User)
                    .unwrap()
                    .reached_at(EndpointIdentity::new(host, Some(resource.to_owned())).unwrap()),
            )
        };
        assert_eq!(
            endpoint_record.check(&repointed("git.example.com", "octocat/hello")),
            TrustCheck::Invalidate(InvalidationReason::EndpointHostChanged)
        );
        assert_eq!(
            endpoint_record.check(&repointed("github.com", "attacker/hello")),
            TrustCheck::Invalidate(InvalidationReason::RepositoryRemoteChanged)
        );

        assert_invalidates(
            agent_basis().fingerprinted(hash("new-schema")),
            InvalidationReason::ToolSchemaFingerprintChanged,
        );
        assert_invalidates(
            agent_basis().hashing(hash("edited-recipe")),
            InvalidationReason::RecipeContentHashChanged,
        );
        assert_invalidates(
            agent_basis()
                .declaring(["fs.read", "terminal", "network"])
                .unwrap(),
            InvalidationReason::CapabilityExpansion,
        );
        assert_invalidates(
            IdentityBasis::new("Example agent", ConfigurationSource::Repository)
                .unwrap()
                .versioned("1.4.2")
                .unwrap()
                .speaking("1")
                .unwrap()
                .launched_from(
                    ExecutableIdentity::new("/usr/local/bin/example-agent", hash("agent-binary"))
                        .unwrap(),
                )
                .declaring(["fs.read", "terminal"])
                .unwrap(),
            InvalidationReason::CapabilityExpansion,
        );
        assert_invalidates(
            agent_basis().versioned("2.0.0").unwrap(),
            InvalidationReason::IncompatibleVersionChange,
        );
        assert_invalidates(
            agent_basis().speaking("2").unwrap(),
            InvalidationReason::IncompatibleVersionChange,
        );
    }

    /// A basis with every field populated, so a perturbation of one field is
    /// the *only* difference between it and the observation.
    fn maximal_basis() -> IdentityBasis {
        IdentityBasis::new("Everything", ConfigurationSource::User)
            .unwrap()
            .versioned("2.3.4")
            .unwrap()
            .speaking("2026-07-28")
            .unwrap()
            .launched_from(ExecutableIdentity::new("/opt/everything", hash("binary")).unwrap())
            .reached_at(
                EndpointIdentity::new("example.com", Some("owner/repo".to_owned())).unwrap(),
            )
            .fingerprinted(hash("schema"))
            .hashing(hash("content"))
            .declaring(["one", "two"])
            .unwrap()
    }

    #[test]
    fn perturbing_any_single_identity_field_yields_that_field_s_reason() {
        let record = TrustRecord::grant(
            SubjectKind::McpServer,
            maximal_basis(),
            TrustScope::Global,
            at(0),
        )
        .unwrap();
        assert_eq!(
            record.check(&ObservedIdentity::new(maximal_basis())),
            TrustCheck::Valid,
            "an unperturbed observation must verify"
        );

        let perturbations: [(InvalidationReason, IdentityBasis); 7] = [
            (
                InvalidationReason::ExecutableHashChanged,
                maximal_basis().launched_from(
                    ExecutableIdentity::new("/opt/everything", hash("other-binary")).unwrap(),
                ),
            ),
            (
                InvalidationReason::EndpointHostChanged,
                maximal_basis().reached_at(
                    EndpointIdentity::new("other.example", Some("owner/repo".to_owned())).unwrap(),
                ),
            ),
            (
                InvalidationReason::RepositoryRemoteChanged,
                maximal_basis().reached_at(
                    EndpointIdentity::new("example.com", Some("other/repo".to_owned())).unwrap(),
                ),
            ),
            (
                InvalidationReason::ToolSchemaFingerprintChanged,
                maximal_basis().fingerprinted(hash("other-schema")),
            ),
            (
                InvalidationReason::RecipeContentHashChanged,
                maximal_basis().hashing(hash("other-content")),
            ),
            (
                InvalidationReason::CapabilityExpansion,
                maximal_basis().declaring(["one", "two", "three"]).unwrap(),
            ),
            (
                InvalidationReason::IncompatibleVersionChange,
                maximal_basis().versioned("3.0.0").unwrap(),
            ),
        ];

        assert_eq!(
            perturbations
                .iter()
                .map(|(reason, _)| *reason)
                .collect::<Vec<_>>(),
            &InvalidationReason::PRECEDENCE[1..],
            "every identity trigger needs a single-field perturbation here"
        );
        for (expected, perturbed) in perturbations {
            assert_ne!(perturbed, maximal_basis());
            assert_eq!(
                record.check(&ObservedIdentity::new(perturbed)),
                TrustCheck::Invalidate(expected)
            );
        }
    }

    #[test]
    fn every_documented_reason_is_reachable_from_the_check() {
        let table = std::iter::once(InvalidationReason::WorkspacePathChanged)
            .chain(IDENTITY_CHECKS.iter().map(|(reason, _)| *reason))
            .collect::<Vec<_>>();
        assert_eq!(table, InvalidationReason::PRECEDENCE);
    }

    #[test]
    fn a_missing_observed_field_invalidates_rather_than_passing_by_absence() {
        // The executable is gone: no hash can be observed at all.
        assert_invalidates(
            IdentityBasis::new("Example agent", ConfigurationSource::User)
                .unwrap()
                .versioned("1.4.2")
                .unwrap()
                .speaking("1")
                .unwrap()
                .declaring(["fs.read", "terminal"])
                .unwrap(),
            InvalidationReason::ExecutableHashChanged,
        );

        // The subject stopped reporting its protocol revision.
        assert_invalidates(
            IdentityBasis::new("Example agent", ConfigurationSource::User)
                .unwrap()
                .versioned("1.4.2")
                .unwrap()
                .launched_from(
                    ExecutableIdentity::new("/usr/local/bin/example-agent", hash("agent-binary"))
                        .unwrap(),
                )
                .declaring(["fs.read", "terminal"])
                .unwrap(),
            InvalidationReason::IncompatibleVersionChange,
        );

        // A recipe whose content could not be read this time.
        let recipe = TrustRecord::grant(
            SubjectKind::Recipe,
            IdentityBasis::new("release", ConfigurationSource::Repository)
                .unwrap()
                .hashing(hash("recipe")),
            TrustScope::Global,
            at(0),
        )
        .unwrap();
        assert_eq!(
            recipe.check(&ObservedIdentity::new(
                IdentityBasis::new("release", ConfigurationSource::Repository).unwrap()
            )),
            TrustCheck::Invalidate(InvalidationReason::RecipeContentHashChanged)
        );
    }

    #[test]
    fn several_triggers_at_once_report_the_documented_precedence() {
        // Everything below the executable hash also changed; the strongest
        // evidence wins.
        assert_invalidates(
            IdentityBasis::new("Renamed agent", ConfigurationSource::Repository)
                .unwrap()
                .versioned("9.9.9")
                .unwrap()
                .speaking("2")
                .unwrap()
                .launched_from(ExecutableIdentity::new("/opt/agent", hash("other-binary")).unwrap())
                .fingerprinted(hash("new-schema"))
                .declaring(["fs.read", "terminal", "network"])
                .unwrap(),
            InvalidationReason::ExecutableHashChanged,
        );

        // A workspace-scoped grant checked in another workspace reports the
        // scope before any identity comparison runs.
        assert_eq!(
            agent_record().check(
                &ObservedIdentity::new(agent_basis().launched_from(
                    ExecutableIdentity::new("/opt/agent", hash("other-binary")).unwrap(),
                ),)
                .in_workspace("/elsewhere")
            ),
            TrustCheck::Invalidate(InvalidationReason::WorkspacePathChanged)
        );

        // Capabilities outrank the version the subject reports for itself.
        assert_invalidates(
            agent_basis()
                .versioned("3.0.0")
                .unwrap()
                .declaring(["fs.read", "terminal", "network"])
                .unwrap(),
            InvalidationReason::CapabilityExpansion,
        );
    }

    #[test]
    fn a_global_grant_ignores_the_workspace_it_is_observed_in() {
        let record = TrustRecord::grant(
            SubjectKind::McpServer,
            agent_basis(),
            TrustScope::Global,
            at(0),
        )
        .unwrap();
        assert_eq!(
            record.check(&ObservedIdentity::new(agent_basis())),
            TrustCheck::Valid
        );
        assert_eq!(
            record.check(&ObservedIdentity::new(agent_basis()).in_workspace("/anywhere")),
            TrustCheck::Valid
        );
    }

    #[test]
    fn a_display_name_and_an_executable_path_are_not_identity() {
        assert_eq!(
            agent_record().check(&observed(
                IdentityBasis::new("Renamed by the vendor", ConfigurationSource::User)
                    .unwrap()
                    .versioned("1.4.2")
                    .unwrap()
                    .speaking("1")
                    .unwrap()
                    .launched_from(
                        ExecutableIdentity::new(
                            "/opt/other/place/example-agent",
                            hash("agent-binary"),
                        )
                        .unwrap(),
                    )
                    .declaring(["fs.read", "terminal"])
                    .unwrap()
            )),
            TrustCheck::Valid
        );
    }

    #[test]
    fn a_compatible_upgrade_keeps_its_grant_and_a_downgrade_does_not() {
        for compatible in ["1.4.2", "1.4.3", "1.9.0"] {
            assert_eq!(
                agent_record().check(&observed(agent_basis().versioned(compatible).unwrap())),
                TrustCheck::Valid,
                "{compatible} should have stayed compatible"
            );
        }
        for incompatible in ["1.4.1", "0.9.0", "2.0.0", "1.4.2-rc.1", "not-a-version"] {
            assert_eq!(
                agent_record().check(&observed(agent_basis().versioned(incompatible).unwrap())),
                TrustCheck::Invalidate(InvalidationReason::IncompatibleVersionChange),
                "{incompatible} should have invalidated"
            );
        }
    }

    #[test]
    fn a_narrowed_capability_set_keeps_its_grant() {
        assert_eq!(
            agent_record().check(&observed(agent_basis().declaring(["fs.read"]).unwrap())),
            TrustCheck::Valid
        );
    }

    #[test]
    fn every_illegal_transition_is_refused_by_name() {
        for &from in TrustState::ALL {
            for &to in TrustState::ALL {
                if from == TrustState::Untrusted || from.can_become(to) {
                    continue;
                }
                let mut record = agent_record();
                match from {
                    TrustState::Revoked => record.revoke().unwrap(),
                    TrustState::Invalidated => record
                        .invalidate(InvalidationReason::ExecutableHashChanged)
                        .unwrap(),
                    TrustState::Trusted | TrustState::Untrusted => {}
                }

                let error = match to {
                    TrustState::Revoked => record.revoke().unwrap_err(),
                    TrustState::Invalidated => record
                        .invalidate(InvalidationReason::ExecutableHashChanged)
                        .unwrap_err(),
                    TrustState::Trusted => record.regrant(agent_basis(), at(1)).unwrap_err(),
                    TrustState::Untrusted => continue,
                };
                assert_eq!(error.kind(), "invalid_trust_transition");
                assert_eq!(
                    error.to_string(),
                    format!("state {from} cannot become {to}")
                );
            }
        }
    }

    #[test]
    fn a_regrant_rebases_the_identity_and_moves_the_grant_time() {
        let mut record = agent_record();
        record
            .invalidate(InvalidationReason::ExecutableHashChanged)
            .unwrap();
        assert_eq!(
            record.invalidation_reason(),
            Some(InvalidationReason::ExecutableHashChanged)
        );

        let replaced = agent_basis().launched_from(
            ExecutableIdentity::new("/usr/local/bin/example-agent", hash("other-binary")).unwrap(),
        );
        record.regrant(replaced.clone(), at(60)).unwrap();

        assert_eq!(record.state(), TrustState::Trusted);
        assert_eq!(record.invalidation_reason(), None);
        assert_eq!(record.granted_at(), at(60));
        assert_eq!(record.identity_basis(), &replaced);
        assert_eq!(record.check(&observed(replaced)), TrustCheck::Valid);
    }

    #[test]
    fn a_regrant_refuses_a_timestamp_that_is_not_utc() {
        let mut record = agent_record();
        record
            .invalidate(InvalidationReason::ExecutableHashChanged)
            .unwrap();
        let offset = at(60).to_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap());
        let error = record.regrant(agent_basis(), offset).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_timestamp");
        // The refusal left the record where it was.
        assert_eq!(record.state(), TrustState::Invalidated);
    }

    #[test]
    fn a_user_can_decline_the_re_prompt_that_followed_a_drift() {
        let mut record = agent_record();
        record
            .invalidate(InvalidationReason::ExecutableHashChanged)
            .unwrap();
        record.revoke().unwrap();

        assert_eq!(record.state(), TrustState::Revoked);
        assert_eq!(
            record.invalidation_reason(),
            None,
            "the record now says the user refused, not that a check found drift"
        );
        assert_eq!(
            record.check(&observed(agent_basis())),
            TrustCheck::NotTrusted
        );
        assert!(record.revoke().is_err(), "revoked is terminal");
    }

    /// A basis with none of the evidence its kind is known by would make
    /// `check` answer `Valid` for any observation at all.
    #[test]
    fn a_grant_requires_the_evidence_its_subject_kind_is_recognized_by() {
        let bare = || IdentityBasis::new("bare", ConfigurationSource::User).unwrap();
        let missing = [
            (SubjectKind::AgentExecutable, TrustScope::Global),
            (SubjectKind::McpServer, TrustScope::Global),
            (SubjectKind::McpToolSchema, TrustScope::Global),
            (SubjectKind::Recipe, TrustScope::Global),
            (SubjectKind::ForgeAccount, TrustScope::Global),
            (SubjectKind::ForgeRepository, TrustScope::Global),
            (SubjectKind::Workspace, TrustScope::Global),
        ];
        assert_eq!(
            missing.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            SubjectKind::ALL
        );
        for (kind, scope) in missing {
            let error = TrustRecord::grant(kind, bare(), scope, at(0)).unwrap_err();
            assert_eq!(
                error.kind(),
                "missing_identity_evidence",
                "{kind} accepted a basis with nothing to compare"
            );
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("a {kind} grant requires"))
            );
        }

        // A forge repository needs the resource, not merely the host: a grant
        // naming only `github.com` would survive being repointed at any other
        // repository on it.
        let host_only = IdentityBasis::new("octocat/hello", ConfigurationSource::User)
            .unwrap()
            .reached_at(EndpointIdentity::new("github.com", None).unwrap());
        assert_eq!(
            TrustRecord::grant(
                SubjectKind::ForgeRepository,
                host_only,
                TrustScope::Global,
                at(0)
            )
            .unwrap_err()
            .kind(),
            "missing_identity_evidence"
        );
    }

    #[test]
    fn a_regrant_cannot_drop_the_evidence_that_made_a_record_checkable() {
        let mut record = agent_record();
        record
            .invalidate(InvalidationReason::ExecutableHashChanged)
            .unwrap();

        let without_executable = IdentityBasis::new("Example agent", ConfigurationSource::User)
            .unwrap()
            .versioned("1.4.2")
            .unwrap();
        let error = record.regrant(without_executable, at(60)).unwrap_err();
        assert_eq!(error.kind(), "missing_identity_evidence");
        assert_eq!(record.state(), TrustState::Invalidated);
    }

    #[test]
    fn a_workspace_scope_must_name_an_absolute_root() {
        for rejected in ["", "workspace", "../workspace"] {
            let error = TrustRecord::grant(
                SubjectKind::AgentExecutable,
                agent_basis(),
                TrustScope::workspace(rejected),
                at(0),
            )
            .unwrap_err();
            assert_eq!(
                error.kind(),
                "invalid_integration_record",
                "accepted {rejected:?} as a workspace root"
            );
        }
    }

    #[test]
    fn checking_a_basis_against_itself_is_valid_for_every_subject_shape() {
        let shapes = [
            (SubjectKind::AgentExecutable, agent_basis()),
            (
                SubjectKind::McpServer,
                IdentityBasis::new("files", ConfigurationSource::User)
                    .unwrap()
                    .speaking("2026-07-28")
                    .unwrap()
                    .launched_from(
                        ExecutableIdentity::new("/usr/bin/mcp-files", hash("server")).unwrap(),
                    )
                    .declaring(["resources", "tools"])
                    .unwrap(),
            ),
            (
                SubjectKind::McpToolSchema,
                IdentityBasis::new("files.read", ConfigurationSource::User)
                    .unwrap()
                    .fingerprinted(hash("schema")),
            ),
            (
                SubjectKind::Recipe,
                IdentityBasis::new("release", ConfigurationSource::Repository)
                    .unwrap()
                    .hashing(hash("recipe")),
            ),
            (
                SubjectKind::ForgeAccount,
                IdentityBasis::new("octocat", ConfigurationSource::User)
                    .unwrap()
                    .reached_at(EndpointIdentity::new("github.com", None).unwrap()),
            ),
            (
                SubjectKind::ForgeRepository,
                IdentityBasis::new("octocat/hello", ConfigurationSource::User)
                    .unwrap()
                    .reached_at(
                        EndpointIdentity::new("github.com", Some("octocat/hello".to_owned()))
                            .unwrap(),
                    ),
            ),
            (
                SubjectKind::Workspace,
                IdentityBasis::new("harkness", ConfigurationSource::User).unwrap(),
            ),
        ];

        assert_eq!(
            shapes.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            SubjectKind::ALL
        );
        for (kind, basis) in shapes {
            // A workspace is identified by where it is, so its grant is the one
            // shape that has to be workspace-scoped.
            let (scope, workspace) = if kind == SubjectKind::Workspace {
                (TrustScope::workspace("/workspace"), Some("/workspace"))
            } else {
                (TrustScope::Global, None)
            };
            let record = TrustRecord::grant(kind, basis.clone(), scope, at(0)).unwrap();

            let mut observation = ObservedIdentity::new(basis);
            if let Some(workspace) = workspace {
                observation = observation.in_workspace(workspace);
            }
            assert_eq!(
                record.check(&observation),
                TrustCheck::Valid,
                "{kind} did not verify against its own identity"
            );
        }
    }
}
