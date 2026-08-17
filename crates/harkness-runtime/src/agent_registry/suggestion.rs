//! Agent configuration a checked-out repository ships, read as a suggestion.
//!
//! ADR-0006 fixes that repository content is untrusted, and #148 states the rule
//! this module implements for agents: repository configuration may tighten
//! Harkness's posture and may never widen it. A repository can therefore say
//! "this project uses an agent called X, launched like this" and can never say
//! "and it is enabled".
//!
//! The enforcement is structural rather than a check somebody remembers to make.
//! A suggestion is a *different type* from a registration, nothing in this module
//! writes `agents.json`, and the only way one becomes a registration is a call
//! the user makes — which produces a disabled entry, exactly as every other
//! registration path does, and which still leaves trusting and enabling it as
//! two further explicit acts.

use std::path::{Path, PathBuf};

use super::config::{AgentRegistration, AgentSource, read_registry};
use super::error::AgentRegistryError;

/// Where a repository declares the agents it expects, relative to its root.
pub const REPOSITORY_AGENTS_PATH: &str = ".harkness/agents.json";

/// One agent a repository suggests, and where the suggestion came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSuggestion {
    registration: AgentRegistration,
    origin: PathBuf,
    requested_enable: bool,
}

impl AgentSuggestion {
    /// The registration this suggestion would become, always disabled.
    ///
    /// Handing it to
    /// [`register`](super::AgentRegistryService::register) is the user's
    /// explicit act of adoption. It carries no trust and no enablement, so
    /// adopting a suggestion for a program that turns out to be hostile has
    /// launched nothing.
    #[must_use]
    pub const fn registration(&self) -> &AgentRegistration {
        &self.registration
    }

    /// The repository file this suggestion was read from.
    #[must_use]
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    /// Whether the repository asked for the agent to be enabled.
    ///
    /// Recorded rather than obeyed, and rather than silently discarded. A
    /// surface can say "this repository wants this agent switched on" — which is
    /// information — without that ever being what happens.
    #[must_use]
    pub const fn requested_enable(&self) -> bool {
        self.requested_enable
    }

    /// Whether the suggestion names an agent that is not registered yet.
    #[must_use]
    pub fn is_new_to(&self, registry: &super::AgentRegistryFile) -> bool {
        registry.get(self.registration.id()).is_none()
    }
}

/// Reads the agent configuration a repository ships, if it ships any.
///
/// The file is the same `agents.json` schema, parsed with the same strictness —
/// probe first, `deny_unknown_fields`, every field validated — because
/// repository content getting a laxer parser is how a laxer parser ends up being
/// the one that matters. Every entry comes back disabled whatever the file says.
///
/// A missing file is an empty list rather than an error: most repositories ship
/// none, and a project that does not use Harkness agents has not failed at
/// anything.
///
/// The `source` a repository writes is **replaced** rather than carried through.
/// It is presentation — what a surface calls the provenance of a registration —
/// and a repository writing `user` would have a list of the user's own agents
/// showing an entry the user did not type. `discovered` is what is true of every
/// entry here: it was found rather than written down.
///
/// # Errors
///
/// Returns the same failures as reading the user's own registry: a version
/// outside this build's range, a body that does not parse, or an entry that
/// violates an invariant.
pub fn repository_suggestions(
    workspace_root: &Path,
) -> Result<Vec<AgentSuggestion>, AgentRegistryError> {
    let origin = workspace_root.join(REPOSITORY_AGENTS_PATH);
    let file = read_registry(&origin)?;
    let mut suggestions = Vec::with_capacity(file.agents().len());
    for registration in file.agents() {
        let requested_enable = registration.is_enabled();
        let normalized = AgentRegistration::new(
            registration.id().clone(),
            registration.display_name(),
            registration.command().to_path_buf(),
            AgentSource::Discovered,
        )?
        .with_args(registration.args().map(str::to_owned))?
        .with_env_allowlist(registration.env_allowlist().map(str::to_owned))?;
        suggestions.push(AgentSuggestion {
            registration: normalized,
            origin: origin.clone(),
            requested_enable,
        });
    }
    Ok(suggestions)
}
