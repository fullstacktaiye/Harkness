//! Registering, discovering, trusting, and health-checking external ACP agents.
//!
//! An ACP agent is a program somebody else wrote, launched as a child process on
//! a user's machine. This module is where Harkness decides *which* such program
//! it is willing to launch, and it is deliberately the only place that decision
//! is made: a front end cannot get past [`AgentRegistryService`] without every
//! gate below having been passed.
//!
//! # Two stores, and why neither is the other's cache
//!
//! `agents.json` is configuration a user wrote: an identifier, a display name,
//! an absolute command, its arguments, the environment variables it may see, and
//! whether it is on. It follows the project catalog's discipline exactly —
//! `schema_version` probed before the strict body, unknown fields refused within
//! a version, atomic replacement under a stable lock inode, and never rewritten
//! by a read — so it stays small, diffable, and safe to edit by hand.
//!
//! `runtime.db` holds what Harkness *observed*: the grant a user made against an
//! exact executable, the version the agent reported, the version both sides
//! negotiated, the capability snapshot, and how the last health check ended.
//! None of it belongs in a file a user edits, and losing it costs a re-trust and
//! a re-check rather than a registration.
//!
//! # Four gates, and the order they are asked in
//!
//! Every launch and every health check passes the same sequence, and the order
//! is what makes each refusal the most useful one available:
//!
//! 1. **Registered.** No entry, no launch.
//! 2. **Enabled.** A registration is created disabled and the only way out of
//!    that state passes through a trust decision somebody made. That is the
//!    structural half of "a repository suggestion never auto-enables": the rule
//!    holds for *every* registration path rather than for the one that
//!    remembered it.
//! 3. **Trusted.** A grant exists, currently stands, and reaches here — a
//!    workspace-scoped grant does not reach outside the root it names, and being
//!    used somewhere else refuses without costing the grant anything.
//! 4. **Unchanged.** The executable is hashed *now* and compared with the digest
//!    the grant was bound to. A mismatch invalidates the grant, disables the
//!    agent, and refuses; it does not merely refuse this once.
//!
//! Only then is anything launched. Trust is a precondition and never an
//! authorization: a trusted agent still passes [`policy`](crate::policy) and
//! still needs an [`approval`](crate::approval) for anything the policy lattice
//! says needs one.
//!
//! # Discovery runs nothing
//!
//! [`Discovery`] answers "is a program with one of these names on the search
//! path" by looking at directory entries. It executes nothing, opens nothing,
//! and hashes nothing, because a probe that ran candidates "to check them" would
//! turn enumeration into arbitrary code execution. Its output is a list of
//! paths, and turning one into a registration, a grant, and an enabled agent is
//! three separate things a user does.
//!
//! Repository-provided configuration enters the same channel through
//! [`repository_suggestions`], and is a different *type* from a registration so
//! that "a repository can suggest and can never enable" is a property of the
//! code rather than a rule to remember.
//!
//! # What is not here
//!
//! Sessions and prompt turns are #151's; permission mapping for agent tool use
//! is #152's; the trust hub is #176's and the command group is #180's. Harkness
//! never downloads or installs an agent, which is an epic-level non-goal rather
//! than a gap.

mod config;
mod discovery;
mod error;
mod id;
mod service;
mod state;
mod suggestion;
#[cfg(test)]
mod tests;

/// The forward-compatible prefix of every versioned value this module reads.
///
/// One definition rather than one per parser: `agents.json` and each of the two
/// observation columns are probed the same way, and three identical derives
/// would be three places for the idiom to drift. The *range* check stays with
/// each caller, because that is the part that genuinely differs — a file and a
/// column carry independent version numbers.
#[derive(serde::Deserialize)]
struct SchemaVersionProbe {
    schema_version: u32,
}

pub use config::{
    AGENTS_FILE, AGENTS_LOCK_FILE, AGENTS_SCHEMA_VERSION, AgentRegistration, AgentRegistryFile,
    AgentSource, MAX_AGENT_ARGUMENT_LENGTH, MAX_AGENT_ARGUMENTS, MAX_AGENTS_FILE_BYTES,
    MAX_ENV_ALLOWLIST_ENTRIES, MAX_REGISTERED_AGENTS, MINIMUM_AGENTS_SCHEMA_VERSION,
};
pub use discovery::{
    DEFAULT_AGENT_CANDIDATES, DEFAULT_DISCOVERY_BUDGET, DiscoveredCandidate, Discovery,
    DiscoveryReport, DiscoveryTruncation, MAX_DISCOVERY_CANDIDATES, MAX_DISCOVERY_DIRECTORIES,
};
pub use error::AgentRegistryError;
pub use id::{AgentId, MAX_AGENT_ID_LENGTH};
pub use service::{
    AGENT_SCRATCH_DIRECTORY, AgentLaunch, AgentRegistryService, DEFAULT_HEALTH_CHECK_TIMEOUT,
    DEFAULT_SHUTDOWN_GRACE, HealthCheck, HealthOutcome, LaunchContext, RegisteredAgent,
    RegistrationOutcome, RemovalOutcome, TrustAgent, TrustOutcome,
};
pub use state::{
    AGENT_OBSERVATION_SCHEMA_VERSION, AgentAuthMethod, AgentCapabilitySnapshot, AgentObservations,
    AgentRuntimeState, AgentTeardown, AgentTrust, AuthStatus, CompatibilityStatus, HealthRecord,
    HealthStatus, InitializeRecord, MAX_AGENT_AUTH_METHODS, MAX_AGENT_REPORTED_TEXT_LENGTH,
    MAX_AUTH_METHOD_DESCRIPTION_LENGTH, MAX_AUTH_METHOD_TEXT_LENGTH, MAX_HEALTH_DETAIL_LENGTH,
};
pub use suggestion::{AgentSuggestion, REPOSITORY_AGENTS_PATH, repository_suggestions};

/// The ACP adapter this module speaks through, re-exported.
///
/// [`AgentRegistryError::Acp`] carries an
/// [`AcpError`](harkness_acp::AcpError) whole, and
/// [`AgentLaunch::spawn_spec`] hands back a
/// [`SpawnSpec`](harkness_acp::harkness_transport::SpawnSpec), so a consumer
/// needs those types whatever happens. Re-exporting makes the seam one
/// dependency rather than two that have to resolve to the same version — which
/// is the same reason `harkness-acp` re-exports the transport.
pub use harkness_acp;

pub(crate) use state::{decode_health, decode_initialize, encode_health, encode_initialize};
