use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Whether a subject is trusted, and if not, how that came to be.
///
/// This is not [`trust::TrustState`](crate::trust::TrustState). That one
/// answers whether the user accepts running one workspace's code and has two
/// values because that is all the question has. This one is about an external
/// subject whose identity can change underneath a grant, so it distinguishes a
/// user's refusal from detected drift — an audit trail that folded the two
/// together could not say whether a user said no or a binary was swapped.
///
/// [`Untrusted`](Self::Untrusted) is the initial state of the machine and the
/// answer a lookup gives when no record matches. It is never the state of a
/// stored record: a record exists because a grant was made, so a wire record
/// spelling `untrusted` is refused rather than loaded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// No grant covers this subject.
    Untrusted,
    /// The user granted trust to the exact identity the record names.
    Trusted,
    /// The user withdrew the grant.
    Revoked,
    /// The subject's identity drifted from the one that was trusted.
    Invalidated,
}

impl TrustState {
    /// Every trust state in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Untrusted,
        Self::Trusted,
        Self::Revoked,
        Self::Invalidated,
    ];

    /// Returns the stable persisted spelling of this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
            Self::Revoked => "revoked",
            Self::Invalidated => "invalidated",
        }
    }

    /// Parses the stable persisted spelling of this state.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == value)
    }

    /// Whether no later state may follow this state.
    ///
    /// Only [`Revoked`](Self::Revoked) is terminal. Re-granting after a
    /// revocation is a *new* record rather than a transition, because
    /// overwriting the state a user explicitly chose would erase the one
    /// decision the audit trail exists to preserve. Invalidation is not a
    /// decision anybody made, so an invalidated record can go either way: it
    /// re-affirms the same intent against the new identity, or — if the user
    /// declines the re-prompt — it becomes a revocation like any other.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Revoked)
    }

    /// Whether this state may become `to`.
    #[must_use]
    pub fn can_become(self, to: Self) -> bool {
        TRUST_TRANSITIONS.contains(&(self, to))
    }

    /// Whether a record in this state must carry an invalidation reason.
    #[must_use]
    pub const fn requires_invalidation_reason(self) -> bool {
        matches!(self, Self::Invalidated)
    }
}

impl fmt::Display for TrustState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every legal trust state edge. An absent edge is invalid.
///
/// `Invalidated -> Revoked` is the answer to a re-prompt the user declined.
/// Without it a refusal after drift would be unrecordable: the record would
/// stay `Invalidated`, which is what it already said before anybody was asked,
/// and the distinction this state machine exists to draw — a user said no
/// versus a binary was swapped — would be lost in exactly the case where both
/// happened.
pub const TRUST_TRANSITIONS: &[(TrustState, TrustState)] = &[
    (TrustState::Untrusted, TrustState::Trusted),
    (TrustState::Trusted, TrustState::Revoked),
    (TrustState::Trusted, TrustState::Invalidated),
    (TrustState::Invalidated, TrustState::Trusted),
    (TrustState::Invalidated, TrustState::Revoked),
];

/// How far a grant reaches, without the workspace it may name.
///
/// The wire form spells the scope and its workspace as two flat fields rather
/// than as a tagged enum, so the strict body can keep `deny_unknown_fields` —
/// serde's `flatten` silently disables it, and a same-version unknown field
/// slipping through unnoticed is exactly what that setting exists to stop.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustScopeKind {
    /// The grant applies wherever the subject is used.
    Global,
    /// The grant applies in one workspace only.
    Workspace,
}

impl TrustScopeKind {
    /// Every scope kind in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Global, Self::Workspace];

    /// Returns the stable persisted spelling of this scope kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }

    /// Parses the stable persisted spelling of this scope kind.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

impl fmt::Display for TrustScopeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How far a grant reaches.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TrustScope {
    /// The grant applies wherever the subject is used.
    Global,
    /// The grant applies in one workspace only.
    Workspace {
        /// Canonical root of the workspace the grant is confined to.
        workspace: PathBuf,
    },
}

impl TrustScope {
    /// Confines a grant to one workspace root.
    #[must_use]
    pub fn workspace(root: impl Into<PathBuf>) -> Self {
        Self::Workspace {
            workspace: root.into(),
        }
    }

    /// The scope's kind, without the workspace it may name.
    #[must_use]
    pub const fn kind(&self) -> TrustScopeKind {
        match self {
            Self::Global => TrustScopeKind::Global,
            Self::Workspace { .. } => TrustScopeKind::Workspace,
        }
    }

    /// Workspace root this grant is confined to, when it is confined to one.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        match self {
            Self::Global => None,
            Self::Workspace { workspace } => Some(workspace),
        }
    }
}

impl fmt::Display for TrustScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => formatter.write_str("global"),
            Self::Workspace { workspace } => {
                write!(formatter, "workspace {}", workspace.display())
            }
        }
    }
}

/// The exact change that made a grant stop describing its subject.
///
/// These spellings are user-facing vocabulary. The trust hub
/// ([#176](https://github.com/fullstacktaiye/harkness/issues/176)) and the CLI
/// ([#180](https://github.com/fullstacktaiye/harkness/issues/180)) present the
/// reason a user is being asked again, never a generic "trust lost".
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InvalidationReason {
    /// The grant names a workspace, and the subject was observed in another —
    /// or in none.
    WorkspacePathChanged,
    /// The executable's content digest differs from the one trusted, or the
    /// executable is no longer observable at all.
    ExecutableHashChanged,
    /// The subject is now reached at a different host.
    EndpointHostChanged,
    /// The subject is now reached at a different resource on the same host —
    /// for a forge repository, a remote repointed at another repository.
    RepositoryRemoteChanged,
    /// A published tool schema's fingerprint differs from the one trusted.
    ToolSchemaFingerprintChanged,
    /// A recipe's content digest differs from the one trusted.
    RecipeContentHashChanged,
    /// The subject declares a capability the grant did not cover, or its
    /// configuration is now controlled by a different party.
    CapabilityExpansion,
    /// The subject reports a version that is not compatible with the one
    /// trusted, or a different protocol revision.
    IncompatibleVersionChange,
}

impl InvalidationReason {
    /// Every invalidation reason in the fixed order
    /// [`TrustRecord::check`](super::TrustRecord::check) applies them.
    ///
    /// Two triggers can fire at once — a subject that was replaced usually
    /// renames itself too — so the reported reason has to be decided by a rule
    /// rather than by whichever comparison a reader's eye reaches first. The
    /// order runs from the question that decides whether the grant is about
    /// this situation at all, through the evidence of what the subject *is*,
    /// to what it merely *says* about itself:
    ///
    /// 1. [`WorkspacePathChanged`](Self::WorkspacePathChanged) — the grant's
    ///    reach. Comparing hashes against a record that was never about this
    ///    workspace would report a change in a subject the record does not
    ///    govern.
    /// 2. [`ExecutableHashChanged`](Self::ExecutableHashChanged),
    ///    [`EndpointHostChanged`](Self::EndpointHostChanged),
    ///    [`RepositoryRemoteChanged`](Self::RepositoryRemoteChanged),
    ///    [`ToolSchemaFingerprintChanged`](Self::ToolSchemaFingerprintChanged),
    ///    [`RecipeContentHashChanged`](Self::RecipeContentHashChanged) — bytes
    ///    and canonical locations, which the subject cannot misreport.
    /// 3. [`CapabilityExpansion`](Self::CapabilityExpansion) — what it may now
    ///    do, and who may now change it.
    /// 4. [`IncompatibleVersionChange`](Self::IncompatibleVersionChange) — the
    ///    number it calls itself, which is self-reported and therefore the
    ///    weakest evidence of the four.
    pub const PRECEDENCE: &'static [Self] = &[
        Self::WorkspacePathChanged,
        Self::ExecutableHashChanged,
        Self::EndpointHostChanged,
        Self::RepositoryRemoteChanged,
        Self::ToolSchemaFingerprintChanged,
        Self::RecipeContentHashChanged,
        Self::CapabilityExpansion,
        Self::IncompatibleVersionChange,
    ];

    /// Returns the stable persisted spelling of this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspacePathChanged => "workspace_path_changed",
            Self::ExecutableHashChanged => "executable_hash_changed",
            Self::EndpointHostChanged => "endpoint_host_changed",
            Self::RepositoryRemoteChanged => "repository_remote_changed",
            Self::ToolSchemaFingerprintChanged => "tool_schema_fingerprint_changed",
            Self::RecipeContentHashChanged => "recipe_content_hash_changed",
            Self::CapabilityExpansion => "capability_expansion",
            Self::IncompatibleVersionChange => "incompatible_version_change",
        }
    }

    /// Parses the stable persisted spelling of this reason.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::PRECEDENCE
            .iter()
            .copied()
            .find(|reason| reason.as_str() == value)
    }

    /// One sentence a user can act on, stating what changed.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::WorkspacePathChanged => {
                "this trust was granted for a different workspace than the one it is being used in"
            }
            Self::ExecutableHashChanged => "the program on disk is not the one that was trusted",
            Self::EndpointHostChanged => "this is now reached at a different host",
            Self::RepositoryRemoteChanged => "the remote now points at a different repository",
            Self::ToolSchemaFingerprintChanged => {
                "this tool's schema changed shape after it was trusted"
            }
            Self::RecipeContentHashChanged => "this recipe was edited after it was trusted",
            Self::CapabilityExpansion => {
                "this can now do more than it could when it was trusted, or is now configured by someone else"
            }
            Self::IncompatibleVersionChange => {
                "this reports a version that is not compatible with the one that was trusted"
            }
        }
    }
}

impl fmt::Display for InvalidationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a record says about one observation of its subject.
///
/// [`Invalidate`](Self::Invalidate) is a finding, not a mutation: the check is
/// pure, and applying the finding is
/// [`TrustRecord::invalidate`](super::TrustRecord::invalidate).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TrustCheck {
    /// The record still describes the subject that was observed.
    Valid,
    /// The record is not a live grant, whatever the observation says.
    NotTrusted,
    /// The subject drifted from the identity that was trusted.
    Invalidate(InvalidationReason),
}

impl TrustCheck {
    /// Whether the observed subject may be used under this record.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        matches!(self, Self::Valid)
    }

    /// The reason a grant stopped applying, when drift is what stopped it.
    #[must_use]
    pub const fn reason(self) -> Option<InvalidationReason> {
        match self {
            Self::Invalidate(reason) => Some(reason),
            Self::Valid | Self::NotTrusted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InvalidationReason, TRUST_TRANSITIONS, TrustCheck, TrustScope, TrustScopeKind, TrustState,
    };

    #[test]
    fn trust_states_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (TrustState::Untrusted, "untrusted"),
            (TrustState::Trusted, "trusted"),
            (TrustState::Revoked, "revoked"),
            (TrustState::Invalidated, "invalidated"),
        ];

        assert_eq!(
            fixtures.iter().map(|(state, _)| *state).collect::<Vec<_>>(),
            TrustState::ALL
        );
        for (state, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(state.as_str(), spelling);
            assert_eq!(state.to_string(), spelling);
            assert_eq!(TrustState::from_stored(spelling), Some(state));
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<TrustState>(&json).unwrap(), state);
        }
    }

    #[test]
    fn state_deserialization_rejects_noncanonical_and_unknown_spellings() {
        for value in ["\"Trusted\"", "\"notTrusted\"", "\"unknown\""] {
            assert!(serde_json::from_str::<TrustState>(value).is_err());
        }
    }

    #[test]
    fn no_transition_leaves_a_terminal_state() {
        assert!(
            TRUST_TRANSITIONS
                .iter()
                .all(|(from, _)| !from.is_terminal())
        );
        assert!(TrustState::Revoked.is_terminal());
    }

    #[test]
    fn the_transition_table_admits_exactly_the_documented_edges() {
        let legal = [
            (TrustState::Untrusted, TrustState::Trusted),
            (TrustState::Trusted, TrustState::Revoked),
            (TrustState::Trusted, TrustState::Invalidated),
            (TrustState::Invalidated, TrustState::Trusted),
            (TrustState::Invalidated, TrustState::Revoked),
        ];
        assert_eq!(legal.as_slice(), TRUST_TRANSITIONS);

        for &from in TrustState::ALL {
            for &to in TrustState::ALL {
                assert_eq!(
                    from.can_become(to),
                    legal.contains(&(from, to)),
                    "{from} -> {to} disagrees with the transition table"
                );
            }
        }
    }

    #[test]
    fn no_state_transitions_to_itself_or_back_to_untrusted() {
        for &(from, to) in TRUST_TRANSITIONS {
            assert_ne!(from, to);
            assert_ne!(to, TrustState::Untrusted);
        }
    }

    #[test]
    fn invalidation_reasons_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (
                InvalidationReason::WorkspacePathChanged,
                "workspace_path_changed",
            ),
            (
                InvalidationReason::ExecutableHashChanged,
                "executable_hash_changed",
            ),
            (
                InvalidationReason::EndpointHostChanged,
                "endpoint_host_changed",
            ),
            (
                InvalidationReason::RepositoryRemoteChanged,
                "repository_remote_changed",
            ),
            (
                InvalidationReason::ToolSchemaFingerprintChanged,
                "tool_schema_fingerprint_changed",
            ),
            (
                InvalidationReason::RecipeContentHashChanged,
                "recipe_content_hash_changed",
            ),
            (
                InvalidationReason::CapabilityExpansion,
                "capability_expansion",
            ),
            (
                InvalidationReason::IncompatibleVersionChange,
                "incompatible_version_change",
            ),
        ];

        assert_eq!(
            fixtures
                .iter()
                .map(|(reason, _)| *reason)
                .collect::<Vec<_>>(),
            InvalidationReason::PRECEDENCE
        );
        for (reason, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(reason.as_str(), spelling);
            assert_eq!(reason.to_string(), spelling);
            assert_eq!(InvalidationReason::from_stored(spelling), Some(reason));
            assert_eq!(serde_json::to_string(&reason).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<InvalidationReason>(&json).unwrap(),
                reason
            );
            assert!(!reason.explanation().is_empty());
        }
    }

    #[test]
    fn scope_kinds_and_scopes_agree() {
        assert_eq!(TrustScope::Global.kind(), TrustScopeKind::Global);
        assert_eq!(TrustScope::Global.root(), None);

        let scoped = TrustScope::workspace("/workspace");
        assert_eq!(scoped.kind(), TrustScopeKind::Workspace);
        assert_eq!(scoped.root().unwrap().to_str(), Some("/workspace"));
        assert_eq!(scoped.to_string(), "workspace /workspace");
        assert_eq!(TrustScope::Global.to_string(), "global");

        for kind in TrustScopeKind::ALL {
            assert_eq!(TrustScopeKind::from_stored(kind.as_str()), Some(*kind));
        }
    }

    /// Guards the enumeration tables against a variant that never joins them.
    ///
    /// The tests above compare a hand-written fixture list against `ALL` and
    /// `PRECEDENCE`, which is circular: a variant added to none of the three
    /// keeps every one of those assertions green while `from_stored` — which
    /// scans the table — starts answering `None` for a spelling this build
    /// itself writes, and the store reads a valid row as corrupt.
    ///
    /// The exhaustive `match` below is not circular. It is over the *type*, so
    /// a new variant fails to compile here until it is given a position, and
    /// the length literal then fails until the table is extended too.
    #[test]
    fn every_variant_holds_a_position_in_its_enumeration_table() {
        assert_eq!(TrustState::ALL.len(), 4);
        for &state in TrustState::ALL {
            let position = match state {
                TrustState::Untrusted => 0,
                TrustState::Trusted => 1,
                TrustState::Revoked => 2,
                TrustState::Invalidated => 3,
            };
            assert_eq!(TrustState::ALL[position], state);
            assert_eq!(TrustState::from_stored(state.as_str()), Some(state));
        }

        assert_eq!(TrustScopeKind::ALL.len(), 2);
        for &kind in TrustScopeKind::ALL {
            let position = match kind {
                TrustScopeKind::Global => 0,
                TrustScopeKind::Workspace => 1,
            };
            assert_eq!(TrustScopeKind::ALL[position], kind);
        }

        assert_eq!(InvalidationReason::PRECEDENCE.len(), 8);
        for &reason in InvalidationReason::PRECEDENCE {
            let position = match reason {
                InvalidationReason::WorkspacePathChanged => 0,
                InvalidationReason::ExecutableHashChanged => 1,
                InvalidationReason::EndpointHostChanged => 2,
                InvalidationReason::RepositoryRemoteChanged => 3,
                InvalidationReason::ToolSchemaFingerprintChanged => 4,
                InvalidationReason::RecipeContentHashChanged => 5,
                InvalidationReason::CapabilityExpansion => 6,
                InvalidationReason::IncompatibleVersionChange => 7,
            };
            assert_eq!(InvalidationReason::PRECEDENCE[position], reason);
            assert_eq!(
                InvalidationReason::from_stored(reason.as_str()),
                Some(reason)
            );
        }
    }

    #[test]
    fn a_check_reports_its_reason_only_when_it_invalidates() {
        assert!(TrustCheck::Valid.is_valid());
        assert_eq!(TrustCheck::Valid.reason(), None);
        assert!(!TrustCheck::NotTrusted.is_valid());
        assert_eq!(TrustCheck::NotTrusted.reason(), None);

        let check = TrustCheck::Invalidate(InvalidationReason::CapabilityExpansion);
        assert!(!check.is_valid());
        assert_eq!(
            check.reason(),
            Some(InvalidationReason::CapabilityExpansion)
        );
    }
}
