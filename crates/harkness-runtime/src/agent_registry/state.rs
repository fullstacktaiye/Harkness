//! What Harkness observed about one registered agent, and how it is persisted.
//!
//! Everything here is a record of a conversation that already happened: the
//! version the agent reported, the version both sides negotiated, what it said
//! it can do, whether it wants a person to authenticate it, and how the last
//! health check ended. None of it is configuration, and none of it is authority
//! — a capability an agent advertises says what it *offers*, never what Harkness
//! will let it do.
//!
//! The vocabulary is Harkness's own even where it mirrors `harkness-acp`'s. An
//! adapter type is not a persisted type: ADR-0009 puts the conversion at the
//! adapter's public surface precisely so an upstream revision lands in one
//! `From` implementation instead of in a `runtime.db` migration.

use std::time::Duration;

use harkness_acp::{AcpAgentCapabilities, AgentDescription, InitializeOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::integration::{InvalidationReason, Sha256Hash, TrustState};
use crate::tool::truncate_failure_text;

use super::error::AgentRegistryError;

/// The wire version of every JSON value this module persists.
///
/// Independent of [`RUNTIME_RECORD_SCHEMA_VERSION`](crate::domain::RUNTIME_RECORD_SCHEMA_VERSION),
/// which the row's own column carries, for the same reason
/// [`ExternalPolicyContext`](crate::policy::ExternalPolicyContext) is
/// independently versioned: an agent's capability vocabulary grows on somebody
/// else's schedule, and a new capability flag must not bump the version of every
/// stored run.
pub const AGENT_OBSERVATION_SCHEMA_VERSION: u32 = 1;

/// Most authentication methods one snapshot records.
pub const MAX_AGENT_AUTH_METHODS: usize = 16;
/// Longest an authentication method's identifier or name may be, in bytes.
pub const MAX_AUTH_METHOD_TEXT_LENGTH: usize = 256;
/// Longest an authentication method's description may be, in bytes.
///
/// This and the two constants above bound one value *together*, and it is their
/// product that matters: the encoded snapshot becomes one `runtime.db` column,
/// so a peer able to push the worst case past the store's inline threshold
/// could make its own health record unwritable — taking away the "a check is
/// always recorded" promise by doing nothing worse than advertising verbosely.
/// `the_largest_snapshot_a_peer_can_produce_fits_one_column` holds the
/// arithmetic; changing any of the three means re-running it.
pub const MAX_AUTH_METHOD_DESCRIPTION_LENGTH: usize = 1024;
/// Longest a recorded health-failure detail may be, in bytes.
pub const MAX_HEALTH_DETAIL_LENGTH: usize = 4096;
/// Longest a version an agent reports for itself may be, in bytes.
pub const MAX_AGENT_REPORTED_TEXT_LENGTH: usize = 512;

/// Whether a person still has to authenticate this agent.
///
/// A label and nothing else. No credential is stored by this module, by the
/// registry, or by the ACP adapter: ACP v1's one method shape has the agent
/// authenticate itself, and Harkness only names which of the offered ways to
/// use.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthStatus {
    /// Nothing is known yet; no handshake has succeeded.
    #[default]
    Unknown,
    /// The agent advertised no authentication method, so it wants none.
    NotRequired,
    /// The agent advertised methods and none has been completed.
    Required,
    /// A method was completed successfully.
    Authenticated,
    /// A method was attempted and the agent rejected it.
    Failed,
}

impl AuthStatus {
    /// Every status in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Unknown,
        Self::NotRequired,
        Self::Required,
        Self::Authenticated,
        Self::Failed,
    ];

    /// The stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotRequired => "not_required",
            Self::Required => "required",
            Self::Authenticated => "authenticated",
            Self::Failed => "failed",
        }
    }

    /// Parses the stable persisted spelling.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|it| it.as_str() == value)
    }
}

impl std::fmt::Display for AuthStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether this build can speak to the agent at all.
///
/// The unsupported case keeps the version the agent selected rather than folding
/// it into "incompatible", because a user reading "this agent speaks ACP 2 and
/// Harkness speaks 1" can act on it and a user reading "incompatible" cannot.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompatibilityStatus {
    /// No handshake has succeeded or failed on a version yet.
    #[default]
    Unknown,
    /// The agent selected a protocol version this build speaks.
    Compatible,
    /// The agent selected a version this build does not speak.
    UnsupportedProtocolVersion {
        /// The version the agent selected, preserved for display.
        advertised: u16,
    },
}

impl CompatibilityStatus {
    /// The stable persisted tag, without the version it may carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Compatible => "compatible",
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version",
        }
    }

    /// Rebuilds a status from its stored tag and payload.
    ///
    /// The pair is validated together: a tag that requires a version and does
    /// not have one, or one that has a version it cannot carry, is a row nobody
    /// wrote through this type.
    #[must_use]
    pub fn from_stored(tag: &str, advertised: Option<u16>) -> Option<Self> {
        match (tag, advertised) {
            ("unknown", None) => Some(Self::Unknown),
            ("compatible", None) => Some(Self::Compatible),
            ("unsupported_protocol_version", Some(advertised)) => {
                Some(Self::UnsupportedProtocolVersion { advertised })
            }
            _ => None,
        }
    }

    /// The version the agent selected, when it selected one Harkness refuses.
    #[must_use]
    pub const fn advertised(self) -> Option<u16> {
        match self {
            Self::UnsupportedProtocolVersion { advertised } => Some(advertised),
            Self::Unknown | Self::Compatible => None,
        }
    }
}

impl std::fmt::Display for CompatibilityStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedProtocolVersion { advertised } => {
                write!(formatter, "unsupported_protocol_version {advertised}")
            }
            other => formatter.write_str(other.as_str()),
        }
    }
}

/// One authentication method an agent advertised.
///
/// Text the agent chose, clamped: a peer must not decide how large a Harkness
/// row is. The clamp is visible in the value rather than silent, and the
/// authoritative list for an actual `authenticate` is the one a live
/// `initialize` returned — this is the record of what was said, for display and
/// for noticing that it changed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAuthMethod {
    /// What `authenticate` would name.
    pub id: String,
    /// A human-readable name for a surface offering the choice.
    pub name: String,
    /// Longer prose about what choosing this method does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Everything one agent said it can do, as of one successful `initialize`.
///
/// Flat rather than nested, because it is a row's worth of flags rather than a
/// protocol object, and every field defaults to `false`: an omitted capability
/// **is** an unsupported capability, which is ACP's rule and is held here
/// structurally rather than by a caller remembering it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCapabilitySnapshot {
    /// The agent serves `session/load`.
    #[serde(default)]
    pub load_session: bool,
    /// The agent accepts image content blocks.
    #[serde(default)]
    pub prompt_image: bool,
    /// The agent accepts audio content blocks.
    #[serde(default)]
    pub prompt_audio: bool,
    /// The agent accepts embedded resource content blocks.
    #[serde(default)]
    pub prompt_embedded_context: bool,
    /// The agent can reach a streamable-HTTP MCP server.
    #[serde(default)]
    pub mcp_http: bool,
    /// The agent can reach an SSE MCP server.
    #[serde(default)]
    pub mcp_sse: bool,
    /// The agent serves `session/list`.
    #[serde(default)]
    pub session_list: bool,
    /// The agent serves `session/delete`.
    #[serde(default)]
    pub session_delete: bool,
    /// The agent accepts `additionalDirectories` on session lifecycle requests.
    #[serde(default)]
    pub session_additional_directories: bool,
    /// The agent serves `session/resume`.
    #[serde(default)]
    pub session_resume: bool,
    /// The agent serves `session/close`.
    #[serde(default)]
    pub session_close: bool,
    /// The agent serves `logout`.
    #[serde(default)]
    pub auth_logout: bool,
    /// Authentication methods the agent advertised, at most
    /// [`MAX_AGENT_AUTH_METHODS`] of them.
    #[serde(default)]
    pub auth_methods: Vec<AgentAuthMethod>,
    /// Whether the agent advertised more methods than the snapshot records.
    ///
    /// A named omission rather than a silently short list: a snapshot that
    /// dropped three methods and said nothing would read as an agent that never
    /// offered them.
    #[serde(default)]
    pub auth_methods_truncated: bool,
}

impl AgentCapabilitySnapshot {
    /// Whether the agent wants a person to authenticate it.
    #[must_use]
    pub fn requires_authentication(&self) -> bool {
        !self.auth_methods.is_empty() || self.auth_methods_truncated
    }
}

impl From<&AcpAgentCapabilities> for AgentCapabilitySnapshot {
    /// The one place an ACP capability shape becomes a Harkness record.
    fn from(capabilities: &AcpAgentCapabilities) -> Self {
        let mut auth_methods = Vec::new();
        for method in capabilities
            .auth_methods
            .iter()
            .take(MAX_AGENT_AUTH_METHODS)
        {
            auth_methods.push(AgentAuthMethod {
                id: truncate_failure_text(
                    method.id.as_str().to_owned(),
                    MAX_AUTH_METHOD_TEXT_LENGTH,
                ),
                name: truncate_failure_text(method.name.clone(), MAX_AUTH_METHOD_TEXT_LENGTH),
                description: method.description.clone().map(|description| {
                    truncate_failure_text(description, MAX_AUTH_METHOD_DESCRIPTION_LENGTH)
                }),
            });
        }
        Self {
            load_session: capabilities.load_session,
            prompt_image: capabilities.prompt.image,
            prompt_audio: capabilities.prompt.audio,
            prompt_embedded_context: capabilities.prompt.embedded_context,
            mcp_http: capabilities.mcp.http,
            mcp_sse: capabilities.mcp.sse,
            session_list: capabilities.session.list,
            session_delete: capabilities.session.delete,
            session_additional_directories: capabilities.session.additional_directories,
            session_resume: capabilities.session.resume,
            session_close: capabilities.session.close,
            auth_logout: capabilities.auth.logout,
            auth_methods_truncated: capabilities.auth_methods.len() > MAX_AGENT_AUTH_METHODS,
            auth_methods,
        }
    }
}

/// What one successful `initialize` established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeRecord {
    agent_name: Option<String>,
    agent_version: Option<String>,
    protocol_version: u16,
    capabilities: AgentCapabilitySnapshot,
    recorded_at: OffsetDateTime,
}

impl InitializeRecord {
    /// Records what an `initialize` returned.
    ///
    /// The agent's self-description is optional because ACP v1 makes `agentInfo`
    /// optional; an agent that omits it is conformant and must not be refused.
    /// Both strings are the agent's own claims and are clamped as such.
    #[must_use]
    pub fn new(
        agent_info: Option<&AgentDescription>,
        protocol_version: u16,
        capabilities: AgentCapabilitySnapshot,
        recorded_at: OffsetDateTime,
    ) -> Self {
        Self {
            agent_name: agent_info.map(|info| {
                truncate_failure_text(info.name.clone(), MAX_AGENT_REPORTED_TEXT_LENGTH)
            }),
            agent_version: agent_info.map(|info| {
                truncate_failure_text(info.version.clone(), MAX_AGENT_REPORTED_TEXT_LENGTH)
            }),
            protocol_version,
            capabilities,
            recorded_at: recorded_at.to_offset(UtcOffset::UTC),
        }
    }

    /// What the agent calls itself, when it said.
    #[must_use]
    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    /// The version the agent reports for itself, when it said.
    #[must_use]
    pub fn agent_version(&self) -> Option<&str> {
        self.agent_version.as_deref()
    }

    /// The version both sides agreed on.
    #[must_use]
    pub const fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    /// What the agent said it can do.
    #[must_use]
    pub const fn capabilities(&self) -> &AgentCapabilitySnapshot {
        &self.capabilities
    }

    /// When the handshake happened.
    #[must_use]
    pub const fn recorded_at(&self) -> OffsetDateTime {
        self.recorded_at
    }
}

/// How one health check ended.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum HealthStatus {
    /// The agent answered `initialize` and wants no authentication.
    Healthy,
    /// The agent answered `initialize` and advertised authentication methods.
    ///
    /// Deliberately not a failure: nothing is wrong with the program, and a
    /// surface that showed it as broken would send a user looking for a fault
    /// instead of to the sign-in it is asking for.
    AuthenticationRequired,
    /// The agent answered, on a protocol version this build does not speak.
    Incompatible,
    /// The agent did not answer usefully.
    Failed,
}

impl HealthStatus {
    /// Every status in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Healthy,
        Self::AuthenticationRequired,
        Self::Incompatible,
        Self::Failed,
    ];

    /// The stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::AuthenticationRequired => "authentication_required",
            Self::Incompatible => "incompatible",
            Self::Failed => "failed",
        }
    }

    /// Parses the stable persisted spelling.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|it| it.as_str() == value)
    }

    /// Whether an agent in this state can start a session.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How far teardown had to go to end the agent's process group.
///
/// Harkness's own spelling of the transport's rung, recorded because "this agent
/// had to be killed" is a bug report about somebody's program rather than an
/// implementation detail.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum AgentTeardown {
    /// The agent had already exited when teardown began.
    AlreadyExited,
    /// The agent exited after its standard input was closed.
    ClosedStdin,
    /// The agent exited after its process group was signalled.
    Signalled,
    /// The agent's process group had to be killed.
    Killed,
}

impl AgentTeardown {
    /// Every rung in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::AlreadyExited,
        Self::ClosedStdin,
        Self::Signalled,
        Self::Killed,
    ];

    /// The stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyExited => "already_exited",
            Self::ClosedStdin => "closed_stdin",
            Self::Signalled => "signalled",
            Self::Killed => "killed",
        }
    }

    /// Parses the stable persisted spelling.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|it| it.as_str() == value)
    }

    /// Whether the agent had to be forced to stop.
    #[must_use]
    pub const fn was_forced(self) -> bool {
        matches!(self, Self::Signalled | Self::Killed)
    }
}

impl std::fmt::Display for AgentTeardown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<harkness_acp::harkness_transport::ShutdownRung> for AgentTeardown {
    fn from(rung: harkness_acp::harkness_transport::ShutdownRung) -> Self {
        use harkness_acp::harkness_transport::ShutdownRung;
        match rung {
            ShutdownRung::AlreadyExited => Self::AlreadyExited,
            ShutdownRung::ClosedStdin => Self::ClosedStdin,
            ShutdownRung::Signalled => Self::Signalled,
            ShutdownRung::Killed => Self::Killed,
        }
    }
}

/// The record of one health check, written whether or not it succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthRecord {
    status: HealthStatus,
    failure_kind: Option<String>,
    detail: Option<String>,
    teardown: Option<AgentTeardown>,
    elapsed: Duration,
    checked_at: OffsetDateTime,
}

impl HealthRecord {
    /// Records a check that ended without a failure.
    #[must_use]
    pub fn succeeded(status: HealthStatus, elapsed: Duration, checked_at: OffsetDateTime) -> Self {
        Self {
            status,
            failure_kind: None,
            detail: None,
            teardown: None,
            elapsed,
            checked_at: checked_at.to_offset(UtcOffset::UTC),
        }
    }

    /// Records a check that ended in a typed failure.
    ///
    /// `kind` is the failure's stable discriminant, kept as text rather than as
    /// a `&'static str` because it is durable: a build that no longer defines a
    /// spelling must still be able to read a row that used it.
    #[must_use]
    pub fn failed(
        status: HealthStatus,
        kind: &str,
        detail: impl Into<String>,
        elapsed: Duration,
        checked_at: OffsetDateTime,
    ) -> Self {
        Self {
            status,
            failure_kind: Some(kind.to_owned()),
            detail: Some(truncate_failure_text(
                detail.into(),
                MAX_HEALTH_DETAIL_LENGTH,
            )),
            teardown: None,
            elapsed,
            checked_at: checked_at.to_offset(UtcOffset::UTC),
        }
    }

    /// Attaches how far teardown had to go.
    #[must_use]
    pub fn torn_down(mut self, teardown: AgentTeardown) -> Self {
        self.teardown = Some(teardown);
        self
    }

    /// How the check ended.
    #[must_use]
    pub const fn status(&self) -> HealthStatus {
        self.status
    }

    /// The failure's stable discriminant, when it failed.
    #[must_use]
    pub fn failure_kind(&self) -> Option<&str> {
        self.failure_kind.as_deref()
    }

    /// What went wrong, clamped, when it failed.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// How far teardown had to go, when the agent was launched at all.
    #[must_use]
    pub const fn teardown(&self) -> Option<AgentTeardown> {
        self.teardown
    }

    /// How long the check took.
    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// When the check ran.
    #[must_use]
    pub const fn checked_at(&self) -> OffsetDateTime {
        self.checked_at
    }
}

/// Everything Harkness observed about one agent, as one `runtime.db` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentObservations {
    auth_status: AuthStatus,
    compatibility: CompatibilityStatus,
    last_initialize: Option<InitializeRecord>,
    last_health: Option<HealthRecord>,
    updated_at: OffsetDateTime,
}

impl AgentObservations {
    /// The state of an agent nothing has ever been asked of.
    #[must_use]
    pub fn unobserved(updated_at: OffsetDateTime) -> Self {
        Self {
            auth_status: AuthStatus::Unknown,
            compatibility: CompatibilityStatus::Unknown,
            last_initialize: None,
            last_health: None,
            updated_at: updated_at.to_offset(UtcOffset::UTC),
        }
    }

    /// Restores a row that has already been decoded.
    #[must_use]
    pub fn from_parts(
        auth_status: AuthStatus,
        compatibility: CompatibilityStatus,
        last_initialize: Option<InitializeRecord>,
        last_health: Option<HealthRecord>,
        updated_at: OffsetDateTime,
    ) -> Self {
        Self {
            auth_status,
            compatibility,
            last_initialize,
            last_health,
            updated_at: updated_at.to_offset(UtcOffset::UTC),
        }
    }

    /// Whether a person still has to authenticate the agent.
    #[must_use]
    pub const fn auth_status(&self) -> AuthStatus {
        self.auth_status
    }

    /// Whether this build can speak to the agent at all.
    #[must_use]
    pub const fn compatibility(&self) -> CompatibilityStatus {
        self.compatibility
    }

    /// What the last successful handshake established, if one ever did.
    #[must_use]
    pub const fn last_initialize(&self) -> Option<&InitializeRecord> {
        self.last_initialize.as_ref()
    }

    /// How the last health check ended, if one ever ran.
    #[must_use]
    pub const fn last_health(&self) -> Option<&HealthRecord> {
        self.last_health.as_ref()
    }

    /// When this row was last written.
    #[must_use]
    pub const fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }

    /// Records what one successful handshake established.
    ///
    /// The authentication status and the compatibility status are *derived*
    /// here rather than set separately, because both are answers the handshake
    /// just gave: an agent that advertised no method wants none, and an agent
    /// that answered on a version this build speaks is compatible. A method
    /// already completed stays completed — an agent still advertising the way it
    /// was authenticated is not asking again.
    pub fn record_initialize(&mut self, record: InitializeRecord) {
        self.auth_status = if record.capabilities().requires_authentication() {
            // A method already completed stays completed: an agent still
            // advertising the way it was authenticated is not asking again.
            match self.auth_status {
                AuthStatus::Authenticated => AuthStatus::Authenticated,
                _ => AuthStatus::Required,
            }
        } else {
            AuthStatus::NotRequired
        };
        self.compatibility = CompatibilityStatus::Compatible;
        self.last_initialize = Some(record);
    }

    /// Records that a handshake failed on the protocol version itself.
    ///
    /// Separate from [`record_initialize`](Self::record_initialize) because
    /// there was no handshake to derive it from: the agent answered and named a
    /// version, and that is the whole of what was learned.
    pub fn record_compatibility(&mut self, compatibility: CompatibilityStatus) {
        self.compatibility = compatibility;
    }

    /// Records the outcome of a sign-in a person completed outside Harkness.
    ///
    /// The only way [`AuthStatus::Authenticated`] is reached. ACP v1 has the
    /// agent authenticate itself, so nothing on the wire distinguishes a signed
    /// in agent from one that is not — both advertise the same methods — and a
    /// handshake alone can therefore only ever answer "it offers a way in".
    pub fn record_authentication(&mut self, status: AuthStatus, at: OffsetDateTime) {
        self.auth_status = status;
        self.updated_at = at.to_offset(UtcOffset::UTC);
    }

    /// Records how one health check ended, successful or not.
    pub fn record_health(&mut self, health: HealthRecord, at: OffsetDateTime) {
        self.last_health = Some(health);
        self.updated_at = at.to_offset(UtcOffset::UTC);
    }
}

/// One agent's trust grant, as the registry reports it.
///
/// [`TrustState::Untrusted`] with no digest is what a lookup answers when no
/// record covers the agent; every other state comes from the most recent record
/// stored about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTrust {
    state: TrustState,
    reason: Option<InvalidationReason>,
    executable_sha256: Option<Sha256Hash>,
    granted_at: Option<OffsetDateTime>,
}

impl AgentTrust {
    /// The answer when no record covers the agent.
    #[must_use]
    pub const fn untrusted() -> Self {
        Self {
            state: TrustState::Untrusted,
            reason: None,
            executable_sha256: None,
            granted_at: None,
        }
    }

    /// Projects the most recent record about one agent.
    #[must_use]
    pub fn from_record(record: &crate::integration::TrustRecord) -> Self {
        Self {
            state: record.state(),
            reason: record.invalidation_reason(),
            executable_sha256: record
                .identity_basis()
                .executable()
                .map(crate::integration::ExecutableIdentity::sha256),
            granted_at: Some(record.granted_at()),
        }
    }

    /// Whether the grant currently stands.
    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        matches!(self.state, TrustState::Trusted)
    }

    /// The state of the most recent record.
    #[must_use]
    pub const fn state(&self) -> TrustState {
        self.state
    }

    /// Why the grant stopped applying, when it was invalidated.
    #[must_use]
    pub const fn invalidation_reason(&self) -> Option<InvalidationReason> {
        self.reason
    }

    /// The executable digest the grant is bound to.
    #[must_use]
    pub const fn executable_sha256(&self) -> Option<Sha256Hash> {
        self.executable_sha256
    }

    /// When the grant now held was made.
    #[must_use]
    pub const fn granted_at(&self) -> Option<OffsetDateTime> {
        self.granted_at
    }
}

/// Everything the registry knows about one agent that is not configuration.
///
/// The observations are an [`Option`] rather than a defaulted row, and that is
/// the honest shape: an agent nobody has ever asked anything has *no*
/// observations, and manufacturing an empty row for it at registration time
/// would put a timestamp on a conversation that never happened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRuntimeState {
    trust: AgentTrust,
    observations: Option<AgentObservations>,
}

impl AgentRuntimeState {
    /// Composes the trust grant and the observations into one view.
    #[must_use]
    pub const fn new(trust: AgentTrust, observations: Option<AgentObservations>) -> Self {
        Self {
            trust,
            observations,
        }
    }

    /// The trust grant, or its absence.
    #[must_use]
    pub const fn trust(&self) -> &AgentTrust {
        &self.trust
    }

    /// What was observed, when anything was.
    #[must_use]
    pub const fn observations(&self) -> Option<&AgentObservations> {
        self.observations.as_ref()
    }

    /// The digest the grant is bound to, set when trust was granted.
    #[must_use]
    pub const fn executable_sha256(&self) -> Option<Sha256Hash> {
        self.trust.executable_sha256()
    }

    /// Whether a person still has to authenticate the agent.
    #[must_use]
    pub fn auth_status(&self) -> AuthStatus {
        self.observations
            .as_ref()
            .map_or(AuthStatus::Unknown, AgentObservations::auth_status)
    }

    /// Whether this build can speak to the agent at all.
    #[must_use]
    pub fn compatibility(&self) -> CompatibilityStatus {
        self.observations.as_ref().map_or(
            CompatibilityStatus::Unknown,
            AgentObservations::compatibility,
        )
    }

    /// What the last successful handshake established, if one ever did.
    #[must_use]
    pub fn last_initialize(&self) -> Option<&InitializeRecord> {
        self.observations
            .as_ref()
            .and_then(AgentObservations::last_initialize)
    }

    /// How the last health check ended, if one ever ran.
    #[must_use]
    pub fn last_health(&self) -> Option<&HealthRecord> {
        self.observations
            .as_ref()
            .and_then(AgentObservations::last_health)
    }
}

// -- wire forms --------------------------------------------------------------

#[derive(Deserialize)]
struct SchemaVersionProbe {
    schema_version: u32,
}

#[derive(Serialize)]
struct InitializeRecordWireRef<'a> {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_version: Option<&'a str>,
    protocol_version: u16,
    capabilities: &'a AgentCapabilitySnapshot,
    #[serde(with = "time::serde::rfc3339")]
    recorded_at: OffsetDateTime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeRecordWire {
    schema_version: u32,
    #[serde(default)]
    agent_name: Option<String>,
    #[serde(default)]
    agent_version: Option<String>,
    protocol_version: u16,
    capabilities: AgentCapabilitySnapshot,
    #[serde(with = "time::serde::rfc3339")]
    recorded_at: OffsetDateTime,
}

#[derive(Serialize)]
struct HealthRecordWireRef<'a> {
    schema_version: u32,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    teardown: Option<&'a str>,
    elapsed_ms: u64,
    #[serde(with = "time::serde::rfc3339")]
    checked_at: OffsetDateTime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthRecordWire {
    schema_version: u32,
    status: String,
    #[serde(default)]
    failure_kind: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    teardown: Option<String>,
    elapsed_ms: u64,
    #[serde(with = "time::serde::rfc3339")]
    checked_at: OffsetDateTime,
}

fn validate_observation_version(found: u32) -> Result<(), AgentRegistryError> {
    if found == 0 || found > AGENT_OBSERVATION_SCHEMA_VERSION {
        return Err(AgentRegistryError::InvalidRegistration {
            field: "agent_runtime_state",
            reason: "the stored observation schema version is not one this build can read",
        });
    }
    Ok(())
}

/// Re-applies on load every bound the encoder applied on write.
///
/// A row this build wrote satisfies all of them, so a row that does not was
/// hand-edited or written by something else — and the store's rule is that a
/// record is rebuilt through its own validation on the way in rather than
/// trusted because it parsed. Refusing rather than clamping is the choice the
/// integration module beside this one makes for the same reason: a value nobody
/// here produced should not be silently rewritten into one that looks like it.
fn validate_capabilities(capabilities: &AgentCapabilitySnapshot) -> Result<(), AgentRegistryError> {
    let refuse = |reason| {
        Err(AgentRegistryError::InvalidRegistration {
            field: "last_initialize_json",
            reason,
        })
    };
    if capabilities.auth_methods.len() > MAX_AGENT_AUTH_METHODS {
        return refuse("more authentication methods are stored than a snapshot may carry");
    }
    for method in &capabilities.auth_methods {
        if method.id.len() > MAX_AUTH_METHOD_TEXT_LENGTH
            || method.name.len() > MAX_AUTH_METHOD_TEXT_LENGTH
        {
            return refuse("a stored authentication method identifier or name is too long");
        }
        if method
            .description
            .as_ref()
            .is_some_and(|description| description.len() > MAX_AUTH_METHOD_DESCRIPTION_LENGTH)
        {
            return refuse("a stored authentication method description is too long");
        }
    }
    Ok(())
}

fn validate_length(
    field: &'static str,
    value: Option<&str>,
    maximum: usize,
    reason: &'static str,
) -> Result<(), AgentRegistryError> {
    if value.is_some_and(|value| value.len() > maximum) {
        return Err(AgentRegistryError::InvalidRegistration { field, reason });
    }
    Ok(())
}

/// Encodes the last successful handshake for its `runtime.db` column.
pub(crate) fn encode_initialize(record: &InitializeRecord) -> Value {
    serde_json::to_value(InitializeRecordWireRef {
        schema_version: AGENT_OBSERVATION_SCHEMA_VERSION,
        agent_name: record.agent_name(),
        agent_version: record.agent_version(),
        protocol_version: record.protocol_version(),
        capabilities: record.capabilities(),
        recorded_at: record.recorded_at(),
    })
    .expect("an initialize record encodes without a custom serializer")
}

/// Decodes the last successful handshake, probing its version first.
///
/// # Errors
///
/// Returns [`AgentRegistryError::InvalidRegistration`] when the version is
/// outside this build's range, the strict body does not parse, or a stored
/// value exceeds a bound the encoder applies.
pub(crate) fn decode_initialize(value: &Value) -> Result<InitializeRecord, AgentRegistryError> {
    let probe: SchemaVersionProbe =
        serde_json::from_value(value.clone()).map_err(|_| malformed("last_initialize_json"))?;
    validate_observation_version(probe.schema_version)?;
    let wire: InitializeRecordWire =
        serde_json::from_value(value.clone()).map_err(|_| malformed("last_initialize_json"))?;
    debug_assert_eq!(wire.schema_version, probe.schema_version);
    validate_capabilities(&wire.capabilities)?;
    validate_length(
        "last_initialize_json",
        wire.agent_name.as_deref(),
        MAX_AGENT_REPORTED_TEXT_LENGTH,
        "the stored agent name is longer than one this build writes",
    )?;
    validate_length(
        "last_initialize_json",
        wire.agent_version.as_deref(),
        MAX_AGENT_REPORTED_TEXT_LENGTH,
        "the stored agent version is longer than one this build writes",
    )?;
    Ok(InitializeRecord {
        agent_name: wire.agent_name,
        agent_version: wire.agent_version,
        protocol_version: wire.protocol_version,
        capabilities: wire.capabilities,
        recorded_at: wire.recorded_at.to_offset(UtcOffset::UTC),
    })
}

/// Encodes the last health check for its `runtime.db` column.
pub(crate) fn encode_health(record: &HealthRecord) -> Value {
    serde_json::to_value(HealthRecordWireRef {
        schema_version: AGENT_OBSERVATION_SCHEMA_VERSION,
        status: record.status().as_str(),
        failure_kind: record.failure_kind(),
        detail: record.detail(),
        teardown: record.teardown().map(AgentTeardown::as_str),
        // Milliseconds rather than a duration object: the value is a display
        // number and a rough one, and a whole-number column sorts and compares
        // where a `{secs, nanos}` pair does neither.
        elapsed_ms: u64::try_from(record.elapsed().as_millis()).unwrap_or(u64::MAX),
        checked_at: record.checked_at(),
    })
    .expect("a health record encodes without a custom serializer")
}

/// Decodes the last health check, probing its version first.
///
/// # Errors
///
/// Returns [`AgentRegistryError::InvalidRegistration`] when the version is
/// outside this build's range, the strict body does not parse, a status or
/// teardown spelling is one this build does not define, or a stored value
/// exceeds a bound the encoder applies.
pub(crate) fn decode_health(value: &Value) -> Result<HealthRecord, AgentRegistryError> {
    let probe: SchemaVersionProbe =
        serde_json::from_value(value.clone()).map_err(|_| malformed("last_health_json"))?;
    validate_observation_version(probe.schema_version)?;
    let wire: HealthRecordWire =
        serde_json::from_value(value.clone()).map_err(|_| malformed("last_health_json"))?;
    debug_assert_eq!(wire.schema_version, probe.schema_version);
    validate_length(
        "last_health_json",
        wire.detail.as_deref(),
        MAX_HEALTH_DETAIL_LENGTH,
        "the stored health detail is longer than one this build writes",
    )?;
    validate_length(
        "last_health_json",
        wire.failure_kind.as_deref(),
        MAX_AGENT_REPORTED_TEXT_LENGTH,
        "the stored failure kind is longer than any this build declares",
    )?;
    let status = HealthStatus::from_stored(&wire.status).ok_or_else(|| {
        AgentRegistryError::InvalidRegistration {
            field: "last_health_json",
            reason: "the stored health status is not one this build defines",
        }
    })?;
    let teardown = match wire.teardown.as_deref() {
        None => None,
        Some(spelling) => Some(AgentTeardown::from_stored(spelling).ok_or({
            AgentRegistryError::InvalidRegistration {
                field: "last_health_json",
                reason: "the stored teardown rung is not one this build defines",
            }
        })?),
    };
    Ok(HealthRecord {
        status,
        failure_kind: wire.failure_kind,
        detail: wire.detail,
        teardown,
        elapsed: Duration::from_millis(wire.elapsed_ms),
        checked_at: wire.checked_at.to_offset(UtcOffset::UTC),
    })
}

const fn malformed(field: &'static str) -> AgentRegistryError {
    AgentRegistryError::InvalidRegistration {
        field,
        reason: "the stored value is not one this build wrote",
    }
}

/// Builds the initialize record one handshake outcome established.
pub(super) fn initialize_record(
    outcome: &InitializeOutcome,
    at: OffsetDateTime,
) -> InitializeRecord {
    InitializeRecord::new(
        outcome.agent_info.as_ref(),
        outcome.protocol_version,
        AgentCapabilitySnapshot::from(&outcome.capabilities),
        at,
    )
}
