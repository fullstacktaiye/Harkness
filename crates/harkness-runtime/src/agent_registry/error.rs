use std::path::PathBuf;
use std::time::Duration;

use harkness_acp::AcpError;
use thiserror::Error;

use crate::integration::{IntegrationDomainError, Sha256Hash, TrustState};
use crate::store::StoreError;

use super::AgentId;

/// Every way a registry operation can refuse or fail.
///
/// The namespace is a **union**, exactly as
/// [`InvocationError`](crate::tool::InvocationError) is: a failure that belongs
/// to the store, to the integration domain, or to the ACP conversation is
/// carried whole and keeps the discriminant its own namespace gave it, rather
/// than being re-spelled here. [`kinds`](Self::kinds) is the concatenation a
/// front end publishes, and a test holds the four tables disjoint.
///
/// Two failures are deliberately *not* passed through, and both are
/// classifications rather than re-spellings:
/// [`InitializeTimeout`](Self::InitializeTimeout) and
/// [`InvalidExecutable`](Self::InvalidExecutable) each answer a question the
/// transport cannot — *which* deadline expired, and whether the program is
/// runnable at all — and each keeps the underlying failure as its source.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentRegistryError {
    /// A registration field violated its grammar or its bound.
    #[error("agent registration {field} is invalid: {reason}")]
    InvalidRegistration {
        /// Field that violated the invariant.
        field: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// No registration in `agents.json` carries this identifier.
    #[error("no agent is registered as {id}")]
    UnknownAgent {
        /// Identifier that was asked for.
        id: AgentId,
    },

    /// A different registration already carries this identifier.
    #[error("agent {id} is already registered with a different configuration")]
    AgentAlreadyRegistered {
        /// Identifier that is already taken.
        id: AgentId,
    },

    /// The registry is full.
    #[error("agents.json already holds the maximum of {limit} registrations")]
    TooManyAgents {
        /// Most registrations the file may hold.
        limit: usize,
    },

    /// The agent is registered but switched off.
    #[error("agent {id} is disabled")]
    AgentDisabled {
        /// Identifier that was asked for.
        id: AgentId,
    },

    /// No trust grant covers the agent, or the one that did no longer applies.
    ///
    /// `reason` is present exactly when the grant was invalidated by drift, and
    /// is the sentence a re-prompt shows: a user asked to trust a program again
    /// is told what changed.
    #[error("agent {id} is {state}{}", reason.map(|reason| format!(": {reason}")).unwrap_or_default())]
    AgentNotTrusted {
        /// Identifier that was asked for.
        id: AgentId,
        /// State of the most recent record about it.
        state: TrustState,
        /// Why the grant stopped applying, when it was invalidated.
        reason: Option<&'static str>,
    },

    /// A grant covers the agent and does not reach where it is being used.
    ///
    /// Distinct from [`AgentNotTrusted`](Self::AgentNotTrusted), and it has to
    /// be: the record really is `Trusted`, so reporting this through that
    /// variant produces "agent X is trusted" followed by a refusal, which is a
    /// sentence that contradicts itself. Nothing is wrong with the grant — it
    /// simply says somewhere else — and a surface reading this can offer to
    /// widen it rather than sending a user to look for a fault.
    #[error(
        "agent {id} is trusted for {}, and is being launched in {}",
        granted_for.as_ref().map_or_else(|| "one workspace".to_owned(), |root| root.display().to_string()),
        observed_in.as_ref().map_or_else(|| "no workspace".to_owned(), |root| root.display().to_string()),
    )]
    GrantOutOfScope {
        /// Identifier that was asked for.
        id: AgentId,
        /// Workspace root the grant is confined to.
        granted_for: Option<PathBuf>,
        /// Workspace the launch named, when it named one.
        observed_in: Option<PathBuf>,
    },

    /// The executable on disk is not the one the grant was made about.
    #[error(
        "the executable for agent {id} has changed: it was trusted as {trusted} and is now {observed}"
    )]
    ExecutableHashMismatch {
        /// Identifier that was asked for.
        id: AgentId,
        /// Digest the grant was bound to.
        trusted: Sha256Hash,
        /// Digest observed now.
        observed: Sha256Hash,
    },

    /// Nothing is readable at the configured command path.
    #[error("the executable for agent {id} is not at {}: {reason}", path.display())]
    ExecutableNotFound {
        /// Identifier that was asked for.
        id: AgentId,
        /// Path the registration names.
        path: PathBuf,
        /// The operating system's reason, clamped.
        reason: String,
    },

    /// The configured command exists and cannot be run as a program.
    ///
    /// Raised for a path that is not a regular file and for a spawn that failed
    /// immediately. A transport failure is otherwise carried whole; this one is
    /// classified because "that file is not a program" and "the conversation
    /// broke" send a user to two different places.
    #[error("the executable for agent {id} at {} cannot be run: {reason}", path.display())]
    InvalidExecutable {
        /// Identifier that was asked for.
        id: AgentId,
        /// Path the registration names.
        path: PathBuf,
        /// What went wrong, clamped.
        reason: String,
        /// The transport failure underneath, when a spawn was attempted.
        #[source]
        source: Option<Box<AcpError>>,
    },

    /// The agent advertised authentication methods and nobody has completed one.
    #[error("agent {id} requires authentication before it can be used")]
    AuthenticationRequired {
        /// Identifier that was asked for.
        id: AgentId,
    },

    /// A recorded handshake selected a protocol version Harkness does not speak.
    ///
    /// Distinct from ACP's own `unsupported_protocol_version`, which is the
    /// agent saying so *now*. This is Harkness reading what it recorded the last
    /// time it asked, so a session refuses without launching the program again.
    #[error(
        "agent {id} last selected ACP protocol version {advertised}, which this build does not speak"
    )]
    IncompatibleAgent {
        /// Identifier that was asked for.
        id: AgentId,
        /// Version the agent selected.
        advertised: u16,
    },

    /// The agent did not finish `initialize` inside the health check's deadline.
    ///
    /// The transport reports the expiry of its startup window and of one
    /// request's deadline as two different kinds, and correctly so — it does not
    /// know what anybody asked for. During a health check both mean one thing:
    /// the program was launched, said nothing usable, and had to be terminated.
    #[error("agent {id} did not answer initialize within {}ms", timeout.as_millis())]
    InitializeTimeout {
        /// Identifier that was asked for.
        id: AgentId,
        /// Deadline that expired.
        timeout: Duration,
        /// The transport failure underneath.
        #[source]
        source: Box<AcpError>,
    },

    /// `agents.json` could not be read.
    #[error("could not read the agent registry at {}", path.display())]
    ConfigurationRead {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// `agents.json` could not be replaced.
    #[error("could not write the agent registry at {}", path.display())]
    ConfigurationWrite {
        /// File that could not be written.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// `agents.json` carries a supported version and a body this build cannot
    /// parse.
    #[error("the agent registry at {} is malformed", path.display())]
    MalformedConfiguration {
        /// File that could not be parsed.
        path: PathBuf,
        /// Underlying parse failure.
        #[source]
        source: serde_json::Error,
    },

    /// `agents.json` predates the oldest schema this build supports.
    #[error(
        "agents.json schema version {found} is older than the minimum supported version {minimum}"
    )]
    ConfigurationVersionTooOld {
        /// Version found in the file.
        found: u32,
        /// Oldest version this build understands.
        minimum: u32,
    },

    /// `agents.json` requires a newer build of Harkness.
    #[error(
        "agents.json schema version {found} is newer than the maximum supported version {maximum}; upgrade Harkness to read it"
    )]
    ConfigurationVersionTooNew {
        /// Version found in the file.
        found: u32,
        /// Newest version this build understands.
        maximum: u32,
    },

    /// Durable runtime state could not be read or changed.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A trust record could not be built, moved, or decoded.
    #[error(transparent)]
    Integration(#[from] IntegrationDomainError),

    /// The conversation with the agent failed, carried whole.
    #[error("agent {id}: {source}")]
    Acp {
        /// Identifier that was asked for.
        id: AgentId,
        /// The ACP or transport failure, keeping its own discriminant.
        #[source]
        source: Box<AcpError>,
    },
}

impl AgentRegistryError {
    /// Every kind this namespace declares, in variant declaration order.
    ///
    /// The three delegating variants are absent because they report their own
    /// namespace's spelling; [`kinds`](Self::kinds) is the concatenation.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_agent_registration",
        "unknown_agent",
        "agent_already_registered",
        "too_many_registered_agents",
        "agent_disabled",
        "agent_not_trusted",
        "agent_grant_out_of_scope",
        "executable_hash_mismatch",
        "executable_not_found",
        "invalid_executable",
        "agent_authentication_required",
        "agent_incompatible",
        "initialize_timeout",
        "agents_file_read_failed",
        "agents_file_write_failed",
        "agents_file_malformed",
        "agents_file_version_too_old",
        "agents_file_version_too_new",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidRegistration { .. } => "invalid_agent_registration",
            Self::UnknownAgent { .. } => "unknown_agent",
            Self::AgentAlreadyRegistered { .. } => "agent_already_registered",
            Self::TooManyAgents { .. } => "too_many_registered_agents",
            Self::AgentDisabled { .. } => "agent_disabled",
            Self::AgentNotTrusted { .. } => "agent_not_trusted",
            Self::GrantOutOfScope { .. } => "agent_grant_out_of_scope",
            Self::ExecutableHashMismatch { .. } => "executable_hash_mismatch",
            Self::ExecutableNotFound { .. } => "executable_not_found",
            Self::InvalidExecutable { .. } => "invalid_executable",
            Self::AuthenticationRequired { .. } => "agent_authentication_required",
            Self::IncompatibleAgent { .. } => "agent_incompatible",
            Self::InitializeTimeout { .. } => "initialize_timeout",
            Self::ConfigurationRead { .. } => "agents_file_read_failed",
            Self::ConfigurationWrite { .. } => "agents_file_write_failed",
            Self::MalformedConfiguration { .. } => "agents_file_malformed",
            Self::ConfigurationVersionTooOld { .. } => "agents_file_version_too_old",
            Self::ConfigurationVersionTooNew { .. } => "agents_file_version_too_new",
            Self::Store(error) => error.kind(),
            Self::Integration(error) => error.kind(),
            Self::Acp { source, .. } => source.kind(),
        }
    }

    /// Every kind a registry call can report, across the four tables it unions.
    ///
    /// Returned owned rather than as a `const` because it concatenates tables
    /// three other modules maintain, and copying their entries into this file is
    /// exactly the drift the tables exist to prevent.
    #[must_use]
    pub fn kinds() -> Vec<&'static str> {
        Self::KINDS
            .iter()
            .copied()
            .chain(StoreError::KINDS.iter().copied())
            .chain(IntegrationDomainError::KINDS.iter().copied())
            .chain(AcpError::kinds())
            .collect()
    }
}

pub(super) const fn invalid_registration(
    field: &'static str,
    reason: &'static str,
) -> AgentRegistryError {
    AgentRegistryError::InvalidRegistration { field, reason }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::AgentRegistryError;
    use harkness_acp::AcpError;

    use crate::integration::IntegrationDomainError;
    use crate::store::StoreError;

    #[test]
    fn the_declared_kinds_are_the_kinds_the_variants_report() {
        let id = crate::agent_registry::AgentId::new("gemini-cli").unwrap();
        let digest = crate::integration::Sha256Hash::of("bytes");
        let declared = [
            AgentRegistryError::InvalidRegistration {
                field: "command",
                reason: "it must start from a filesystem root",
            },
            AgentRegistryError::UnknownAgent { id: id.clone() },
            AgentRegistryError::AgentAlreadyRegistered { id: id.clone() },
            AgentRegistryError::TooManyAgents { limit: 256 },
            AgentRegistryError::AgentDisabled { id: id.clone() },
            AgentRegistryError::AgentNotTrusted {
                id: id.clone(),
                state: crate::integration::TrustState::Untrusted,
                reason: None,
            },
            AgentRegistryError::GrantOutOfScope {
                id: id.clone(),
                granted_for: Some("/workspace/project".into()),
                observed_in: None,
            },
            AgentRegistryError::ExecutableHashMismatch {
                id: id.clone(),
                trusted: digest,
                observed: digest,
            },
            AgentRegistryError::ExecutableNotFound {
                id: id.clone(),
                path: "/usr/bin/gemini".into(),
                reason: "no such file".to_owned(),
            },
            AgentRegistryError::InvalidExecutable {
                id: id.clone(),
                path: "/usr/bin/gemini".into(),
                reason: "it is not a regular file".to_owned(),
                source: None,
            },
            AgentRegistryError::AuthenticationRequired { id: id.clone() },
            AgentRegistryError::IncompatibleAgent {
                id: id.clone(),
                advertised: 2,
            },
            AgentRegistryError::InitializeTimeout {
                id: id.clone(),
                timeout: std::time::Duration::from_secs(1),
                source: Box::new(AcpError::AlreadyInitialized),
            },
            AgentRegistryError::ConfigurationRead {
                path: "/data/agents.json".into(),
                source: std::io::Error::other("read"),
            },
            AgentRegistryError::ConfigurationWrite {
                path: "/data/agents.json".into(),
                source: std::io::Error::other("write"),
            },
            AgentRegistryError::MalformedConfiguration {
                path: "/data/agents.json".into(),
                source: serde_json::from_str::<u32>("nope").unwrap_err(),
            },
            AgentRegistryError::ConfigurationVersionTooOld {
                found: 0,
                minimum: 1,
            },
            AgentRegistryError::ConfigurationVersionTooNew {
                found: 2,
                maximum: 1,
            },
        ];

        let kinds = declared
            .iter()
            .map(AgentRegistryError::kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, AgentRegistryError::KINDS);
    }

    #[test]
    fn a_delegating_variant_keeps_the_discriminant_its_own_namespace_gave_it() {
        let id = crate::agent_registry::AgentId::new("gemini-cli").unwrap();
        let acp = AgentRegistryError::Acp {
            id,
            source: Box::new(AcpError::UnsupportedProtocolVersion { agent_selected: 2 }),
        };
        assert_eq!(acp.kind(), "unsupported_protocol_version");

        let integration = AgentRegistryError::from(IntegrationDomainError::InvalidIdentity {
            field: "display_name",
            reason: "it cannot be empty",
        });
        assert_eq!(integration.kind(), "invalid_identity");
    }

    /// The four tables are concatenated by whatever publishes
    /// `exit_code_by_kind`, so one spelling appearing in two of them would make
    /// the map ambiguous.
    #[test]
    fn the_four_unioned_namespaces_do_not_collide() {
        let mut seen = HashSet::new();
        for kind in AgentRegistryError::kinds() {
            assert!(seen.insert(kind), "{kind} is declared by two namespaces");
        }
        assert_eq!(
            seen.len(),
            AgentRegistryError::KINDS.len()
                + StoreError::KINDS.len()
                + IntegrationDomainError::KINDS.len()
                + AcpError::kinds().len()
        );
    }
}
