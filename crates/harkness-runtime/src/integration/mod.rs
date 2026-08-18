//! Identity and trust for the external things Harkness talks to.
//!
//! v0.5 introduces subjects nobody at Harkness controls: an ACP agent's
//! executable, an MCP server, each tool schema that server publishes, a
//! workflow recipe, a forge account, and a forge repository. Every one of them
//! can change out from under a decision the user already made, and the ways
//! they change are ordinary — a binary is replaced at the same path, a tool
//! gains a parameter, a repository recipe is edited, a remote is repointed.
//!
//! This module is the vocabulary that makes those changes visible. It defines
//! what a subject *is*, what a grant is a grant *about*, and the pure function
//! that decides whether the two still agree. It enforces nothing:
//! [#148](https://github.com/fullstacktaiye/harkness/issues/148) is where policy
//! and approvals consume these types, and persistence lands with the
//! [#86](https://github.com/fullstacktaiye/harkness/issues/86) framework.
//! ADR-0016 records why the model has this shape and which alternatives were
//! refused.
//!
//! # Trust is never one boolean
//!
//! A `trusted: true` keyed to a name or a path is inherited by whatever
//! occupies that name or path next, and nothing about a boolean can notice. A
//! [`TrustRecord`] instead names four things: the [`SubjectKind`] it is about,
//! the [`IdentityBasis`] — the exact hashes, endpoints and versions that were
//! trusted — the [`TrustScope`] it reaches, and the moment it was granted.
//!
//! The basis is the security boundary. [`TrustRecord::check`] compares it
//! against an [`ObservedIdentity`] a caller has just gathered and answers
//! [`Valid`](TrustCheck::Valid), [`NotTrusted`](TrustCheck::NotTrusted), or the
//! [`InvalidationReason`] that applies. It reads no clock, opens no file and
//! hashes nothing, which is what makes the whole model testable with none of
//! the subjects present.
//!
//! Two fields of a basis are deliberately not compared:
//! [`IdentityBasis::display_name`], because a name is presentation, and
//! [`ExecutableIdentity::path`], because an identical binary reached through
//! another path is the same program while a different binary at the same path
//! is not. Everything else takes part, and an expected field the observation
//! *lacks* invalidates rather than passing by absence.
//!
//! Every basis field is optional, since no subject has all of them — and a
//! basis carrying *none* of the ones its kind is known by would leave `check`
//! comparing the fields both sides left empty, which answers `Valid` for
//! anything at all. [`TrustRecord::grant`] therefore refuses such a record
//! outright: a recipe needs its content hash, an agent its executable, a forge
//! repository an endpoint naming the repository and not merely the host.
//!
//! # The state machine
//!
//! ```text
//! untrusted --grant--> trusted --user says no--> revoked
//!                         |                        ^
//!                    drift detected                |
//!                         v              re-prompt declined
//!                    invalidated --re-grant--> trusted
//! ```
//!
//! [`TRUST_TRANSITIONS`] is the table; an absent edge is a typed error rather
//! than a silent write. `Untrusted` is the initial state and the answer a
//! lookup gives when no record matches — never the state of a stored record, so
//! a wire record spelling it is refused. `Revoked` is terminal: re-granting
//! after a user said no is a new record, because overwriting that state would
//! erase the one decision the audit trail exists to keep. Invalidation is not a
//! decision anybody made, so [`TrustRecord::regrant`] continues the same record
//! against the identity that is there now — and a user who is re-prompted and
//! *declines* revokes from there, which is why that edge exists.
//!
//! There is no structural key for "the same grant", and adding one would be a
//! mistake: [`TrustRecord::check`] accepts a compatible upgrade and ignores two
//! fields, so equality over these fields is not the relation it implements. See
//! [`TrustRecord`] for what a store should key on instead.
//!
//! # Which reason is reported when several apply
//!
//! A replaced subject usually renames and re-versions itself at the same time,
//! so more than one trigger fires and the answer has to be decided by a rule.
//! [`InvalidationReason::PRECEDENCE`] fixes it: the grant's reach first, then
//! the evidence a subject cannot misreport — bytes and canonical locations —
//! then what it may now do, and last the version number it reports for itself.
//!
//! # Two things called trust
//!
//! [`trust::WorkspaceTrust`](crate::trust::WorkspaceTrust) answers whether the
//! user accepts running one workspace's code, and is keyed to a `ProjectId` and
//! a canonical root. This module answers whether Harkness may talk to an
//! external subject at all. Both are preconditions and neither is an
//! authorization: a trusted agent still passes [`policy`](crate::policy) on
//! every call and still needs an [`approval`](crate::approval) for anything the
//! policy lattice says needs one. An external permission system — an agent's
//! own allowlist, a server's own consent prompt — supplements Harkness policy
//! and never replaces it.
//!
//! # Frozen wire formats
//!
//! [`INTEGRATION_RECORD_SCHEMA_VERSION`] is independent of
//! [`RUNTIME_RECORD_SCHEMA_VERSION`](crate::domain::RUNTIME_RECORD_SCHEMA_VERSION),
//! so trust records and run records evolve without dragging each other along.
//! Deserialization probes the version before parsing the strict body, and the
//! committed fixtures under `src/integration/fixtures/` are the frozen baseline
//! the [#86](https://github.com/fullstacktaiye/harkness/issues/86) migrations
//! will store: adding a field, a subject kind, or a state spelling means a
//! version bump and a *new* fixture, never an edit to an existing one.

mod binding;
mod error;
mod id;
mod record;
mod state;
mod subject;
mod wire;

pub use binding::IntegrationIdentity;
pub use error::IntegrationDomainError;
pub use id::{
    ExternalAgentId, ForgeAccountId, ForgeRepoRef, McpServerId, McpToolRef, RecipeId, TrustRecordId,
};
pub use record::{ObservedIdentity, TrustRecord};
pub use state::{
    InvalidationReason, TRUST_TRANSITIONS, TrustCheck, TrustScope, TrustScopeKind, TrustState,
};
pub use subject::{
    ConfigurationSource, EndpointIdentity, ExecutableIdentity, IdentityBasis, MAX_CAPABILITIES,
    MAX_EXECUTABLE_PATH_LENGTH, MAX_IDENTITY_FIELD_LENGTH, Sha256Hash, SubjectKind,
    is_rooted_anywhere,
};
pub use wire::{
    INTEGRATION_RECORD_SCHEMA_VERSION, MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION, TrustRecordWire,
    TrustRecordWireRef, validate_integration_schema_version,
};
