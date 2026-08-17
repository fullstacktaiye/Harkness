//! What each side of the handshake says it can do, in Harkness's own words.
//!
//! Everything here is plain data with no protocol in it. That is the ADR-0009
//! boundary made concrete: an ACP revision may rename a field, move a capability
//! under a new object, or add a spelling, and the only code that has to notice
//! is the mapping at the bottom of this file. Nothing above this crate — no
//! run record, no policy context, no `runtime.db` column — is written against a
//! shape somebody else governs.
//!
//! # Omitted means unsupported
//!
//! Every capability is a `bool` that is `false` unless the agent said otherwise,
//! and there is deliberately no third state. An `Option<bool>` here would let a
//! caller ask whether the agent was *silent* about `loadSession`, and the only
//! honest answer to that question is the one ACP already fixes: silence is a
//! refusal. Making absence representable is how a client ends up calling
//! `session/load` against an agent that never claimed to implement it.

use std::fmt;

use crate::wire;

/// How Harkness names itself to an agent.
///
/// Sent verbatim as `clientInfo`. It carries a product name and a version and
/// nothing else — no username, no workspace path, no project identifier — because
/// `initialize` happens before any trust decision has been made about the program
/// on the other end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientIdentity {
    /// Programmatic name, used as the display fallback when `title` is absent.
    pub name: String,
    /// Human-readable name for a surface that has one.
    pub title: Option<String>,
    /// The Harkness version, for the agent's own diagnostics.
    pub version: String,
}

impl ClientIdentity {
    /// Names Harkness by product name and version.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            version: version.into(),
        }
    }

    /// Adds the human-readable name.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// What Harkness promises to serve *this* agent, decided by #153.
///
/// This type is an input and never a policy. The adapter advertises exactly the
/// flags it is handed and turns none of them on by itself, so the question "may
/// this agent ask Harkness to write a file" has one answer in one place. An
/// adapter that advertised `fs/write_text_file` unconditionally would be
/// promising mediation that does not exist yet, which is the security boundary
/// #153 owns rather than a convenience this crate may take.
///
/// [`Default`] advertises nothing, which is the safe advertisement: an agent
/// told Harkness serves no client method will not ask it to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdvertisedClientCapabilities {
    /// Harkness serves `fs/read_text_file`.
    pub fs_read_text_file: bool,
    /// Harkness serves `fs/write_text_file`.
    pub fs_write_text_file: bool,
    /// Harkness serves every `terminal/*` method.
    pub terminal: bool,
}

/// Everything one agent said it can do, as of one `initialize`.
///
/// The snapshot is returned to the caller rather than kept as adapter state
/// alone, because #150 persists it under the #146 identity model: an agent that
/// starts advertising a different capability set has changed in a way a trust
/// grant was not given for.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AcpAgentCapabilities {
    /// The agent serves `session/load`.
    ///
    /// Still a top-level flag rather than a member of [`SessionCapabilities`],
    /// because that is where ACP v1 puts it. Moving it here would be this crate
    /// disagreeing with the wire about what the agent said.
    pub load_session: bool,
    /// Prompt content the agent accepts beyond the text and resource-link
    /// baseline.
    pub prompt: PromptCapabilities,
    /// MCP server transports the agent can be pointed at.
    pub mcp: McpCapabilities,
    /// Session lifecycle methods beyond the mandatory ones.
    pub session: SessionCapabilities,
    /// Authentication methods the agent serves beyond `authenticate` itself.
    pub auth: AuthCapabilities,
    /// Authentication methods the agent will accept, empty when it advertised
    /// none.
    pub auth_methods: Vec<AuthMethod>,
}

impl AcpAgentCapabilities {
    /// Whether the agent advertised `method`.
    ///
    /// This is what gates [`AcpConnection::authenticate`], and it is asked
    /// before anything is written rather than after the agent refuses: an
    /// authentication attempt against a method nobody offered is a caller bug,
    /// and finding it out from the peer costs a round trip and an audit entry
    /// about a request Harkness should not have sent.
    ///
    /// [`AcpConnection::authenticate`]: crate::AcpConnection::authenticate
    #[must_use]
    pub fn advertises(&self, method: &AuthMethodId) -> bool {
        self.auth_methods
            .iter()
            .any(|offered| &offered.id == method)
    }

    /// Every advertised method's identifier, in the order the agent listed them.
    #[must_use]
    pub fn auth_method_ids(&self) -> Vec<AuthMethodId> {
        self.auth_methods
            .iter()
            .map(|method| method.id.clone())
            .collect()
    }
}

/// Prompt content types an agent accepts beyond the v1 baseline.
///
/// The baseline — `ContentBlock::Text` and `ContentBlock::ResourceLink` — is
/// mandatory for every v1 agent and so is not represented: a flag that is always
/// true is not a capability.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptCapabilities {
    /// The agent accepts image content blocks.
    pub image: bool,
    /// The agent accepts audio content blocks.
    pub audio: bool,
    /// The agent accepts embedded resource content blocks.
    pub embedded_context: bool,
}

/// MCP server transports an agent can connect to on Harkness's behalf.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct McpCapabilities {
    /// The agent can reach a streamable-HTTP MCP server.
    pub http: bool,
    /// The agent can reach an SSE MCP server.
    pub sse: bool,
}

/// Session methods an agent serves beyond the mandatory `session/new`,
/// `session/prompt`, `session/cancel`, and `session/update`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionCapabilities {
    /// The agent serves `session/list`.
    pub list: bool,
    /// The agent serves `session/delete`.
    pub delete: bool,
    /// The agent accepts `additionalDirectories` on session lifecycle requests.
    pub additional_directories: bool,
    /// The agent serves `session/resume`.
    pub resume: bool,
    /// The agent serves `session/close`.
    pub close: bool,
}

/// Authentication methods an agent serves beyond `authenticate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuthCapabilities {
    /// The agent serves `logout`.
    pub logout: bool,
}

/// The identifier of one authentication method an agent advertised.
///
/// Opaque on purpose. The agent chooses the spelling and Harkness compares it
/// byte for byte; there is no vocabulary of known method ids to fall behind.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AuthMethodId(String);

impl AuthMethodId {
    /// Names the method the agent called this.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as the agent spelled it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AuthMethodId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One way an agent is willing to be authenticated.
///
/// v1 has exactly one method shape — the agent handles authentication itself and
/// Harkness only names which of the offered ways to use. The typed variants
/// upstream carries for an environment-variable secret and for an interactive
/// terminal flow sit behind `unstable_auth_methods`, which ADR-0014 forbids, so
/// they cannot appear here and no credential material passes through this crate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthMethod {
    /// What to name in `authenticate`.
    pub id: AuthMethodId,
    /// A human-readable name for a surface offering the choice.
    pub name: String,
    /// Longer prose about what choosing this method will do.
    pub description: Option<String>,
}

/// What an agent calls itself.
///
/// Optional because ACP v1 makes `agentInfo` optional; a v1 agent that omits it
/// is conformant and must not be refused for it. The version string is the
/// agent's own claim about itself and is treated as one — ADR-0016 binds trust
/// to an executable digest, never to a name a program reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDescription {
    /// Programmatic name, used as the display fallback when `title` is absent.
    pub name: String,
    /// Human-readable name for a surface that has one.
    pub title: Option<String>,
    /// The version the agent reports for itself.
    pub version: String,
}

/// Maps one decoded `initialize` response into the snapshot above.
///
/// Every field the response omitted has already become its unsupported value by
/// the time this runs, because upstream declares each one `#[serde(default)]`.
/// The mapping's job is the second half of the same rule: an optional *object*
/// on the wire — `sessionCapabilities.resume` and its siblings, whose presence
/// is the whole signal — becomes `true` when present and `false` when absent or
/// null, and nothing inside it is read.
pub(crate) fn agent_capabilities(response: &wire::InitializeResponse) -> AcpAgentCapabilities {
    let capabilities = &response.agent_capabilities;
    let session = &capabilities.session_capabilities;

    AcpAgentCapabilities {
        load_session: capabilities.load_session,
        prompt: PromptCapabilities {
            image: capabilities.prompt_capabilities.image,
            audio: capabilities.prompt_capabilities.audio,
            embedded_context: capabilities.prompt_capabilities.embedded_context,
        },
        mcp: McpCapabilities {
            http: capabilities.mcp_capabilities.http,
            sse: capabilities.mcp_capabilities.sse,
        },
        session: SessionCapabilities {
            list: session.list.is_some(),
            delete: session.delete.is_some(),
            additional_directories: session.additional_directories.is_some(),
            resume: session.resume.is_some(),
            close: session.close.is_some(),
        },
        auth: AuthCapabilities {
            logout: capabilities.auth.logout.is_some(),
        },
        auth_methods: response
            .auth_methods
            .iter()
            .map(|method| AuthMethod {
                id: AuthMethodId::new(method.id().to_string()),
                name: method.name().to_owned(),
                description: method.description().map(str::to_owned),
            })
            .collect(),
    }
}

/// Maps the agent's optional self-description.
pub(crate) fn agent_description(response: &wire::InitializeResponse) -> Option<AgentDescription> {
    response.agent_info.as_ref().map(|info| AgentDescription {
        name: info.name.clone(),
        title: info.title.clone(),
        version: info.version.clone(),
    })
}
