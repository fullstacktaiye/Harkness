//! Identity evidence carried from external-subject observation into policy and approvals.

use serde::{Deserialize, Serialize};

use super::Sha256Hash;

/// The external identities an authorization is bound to.
///
/// Every field is optional because most operations involve only one kind of
/// subject. Absence is still part of the identity: approval matching compares
/// this value as a whole, so adding or removing a hash defeats an existing
/// grant just as changing one does.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationIdentity {
    /// Content digest of the agent or MCP-server executable being launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_executable_sha256: Option<Sha256Hash>,
    /// Fingerprint of the imported MCP tool schema being invoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_tool_schema_fingerprint: Option<Sha256Hash>,
    /// Content digest of the compiled workflow recipe being executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipe_content_hash: Option<Sha256Hash>,
}

impl IntegrationIdentity {
    /// An identity with no external-subject evidence.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            agent_executable_sha256: None,
            mcp_tool_schema_fingerprint: None,
            recipe_content_hash: None,
        }
    }

    /// Binds the executable content observed for an agent or MCP server.
    #[must_use]
    pub const fn with_agent_executable_sha256(mut self, hash: Sha256Hash) -> Self {
        self.agent_executable_sha256 = Some(hash);
        self
    }

    /// Binds the schema fingerprint observed for one MCP tool.
    #[must_use]
    pub const fn with_mcp_tool_schema_fingerprint(mut self, hash: Sha256Hash) -> Self {
        self.mcp_tool_schema_fingerprint = Some(hash);
        self
    }

    /// Binds the content digest of a compiled workflow recipe.
    #[must_use]
    pub const fn with_recipe_content_hash(mut self, hash: Sha256Hash) -> Self {
        self.recipe_content_hash = Some(hash);
        self
    }

    /// Executable digest, when this operation launches an external program.
    #[must_use]
    pub const fn agent_executable_sha256(self) -> Option<Sha256Hash> {
        self.agent_executable_sha256
    }

    /// Imported MCP tool schema fingerprint, when this operation invokes one.
    #[must_use]
    pub const fn mcp_tool_schema_fingerprint(self) -> Option<Sha256Hash> {
        self.mcp_tool_schema_fingerprint
    }

    /// Compiled workflow recipe content digest, when this operation executes one.
    #[must_use]
    pub const fn recipe_content_hash(self) -> Option<Sha256Hash> {
        self.recipe_content_hash
    }

    /// Whether no external identity participates in this request.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.agent_executable_sha256.is_none()
            && self.mcp_tool_schema_fingerprint.is_none()
            && self.recipe_content_hash.is_none()
    }
}
