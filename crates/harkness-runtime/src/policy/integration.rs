//! Policy vocabulary for external agents, MCP, forges, and workflow recipes.

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

use crate::integration::{IntegrationIdentity, Sha256Hash};
use crate::tool::{Capability, RegistryError, RiskLevel};
use crate::trust::RequestClassification;

/// Schema version of the external request context embedded in policy decisions.
pub const EXTERNAL_POLICY_CONTEXT_SCHEMA_VERSION: u32 = 1;

/// Stable denial kinds produced when an external operation cannot be authorized.
///
/// Every one belongs to the CLI exit-code-3 refusal family. Keeping the table
/// beside the enum makes `harkness contract` and the evaluator share one source
/// instead of maintaining matching strings in two crates.
pub const EXTERNAL_POLICY_DENIAL_KINDS: &[&str] = &[
    "noninteractive_external_agent_launch_denied",
    "noninteractive_mcp_server_connect_denied",
    "noninteractive_mcp_tool_invoke_denied",
    "noninteractive_forge_resource_read_denied",
    "noninteractive_remote_branch_push_denied",
    "noninteractive_pull_request_create_denied",
    "noninteractive_forge_resource_modify_denied",
    "noninteractive_workflow_recipe_execute_denied",
    "agent_executable_identity_required",
    "mcp_tool_schema_identity_required",
    "recipe_content_identity_required",
    "external_identity_context_invalid",
];

/// External-integration operations understood by the policy engine.
///
/// These are typed members layered over the open [`Capability`] vocabulary:
/// local and plugin tools may continue to declare their existing capabilities,
/// while integration adapters use these exact stable spellings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExternalCapability {
    /// Start an ACP agent, evaluated against the agent executable identity.
    LaunchExternalAgent,
    /// Start or connect to an MCP server, evaluated against its executable identity.
    ConnectMcpServer,
    /// Invoke one imported MCP tool, evaluated against its schema fingerprint.
    InvokeMcpTool,
    /// Read an issue, pull request, or other forge resource over the network.
    ReadForgeResource,
    /// Push one remote branch; always a one-call remote-write approval.
    PushRemoteBranch,
    /// Create one pull request; always a one-call remote-write approval.
    CreatePullRequest,
    /// Mutate a forge resource; always a one-call remote-write approval.
    ModifyForgeResource,
    /// Execute a compiled recipe, evaluated against its content hash and step risk.
    ExecuteWorkflowRecipe,
}

impl ExternalCapability {
    /// Every external capability in stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::LaunchExternalAgent,
        Self::ConnectMcpServer,
        Self::InvokeMcpTool,
        Self::ReadForgeResource,
        Self::PushRemoteBranch,
        Self::CreatePullRequest,
        Self::ModifyForgeResource,
        Self::ExecuteWorkflowRecipe,
    ];

    /// Stable spelling used in descriptors, policy files, approvals, and records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchExternalAgent => "launch_external_agent",
            Self::ConnectMcpServer => "connect_mcp_server",
            Self::InvokeMcpTool => "invoke_mcp_tool",
            Self::ReadForgeResource => "read_forge_resource",
            Self::PushRemoteBranch => "push_remote_branch",
            Self::CreatePullRequest => "create_pull_request",
            Self::ModifyForgeResource => "modify_forge_resource",
            Self::ExecuteWorkflowRecipe => "execute_workflow_recipe",
        }
    }

    /// Parses a canonical capability value without treating unknown plugin
    /// capabilities as errors.
    #[must_use]
    pub fn from_capability(capability: &Capability) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == capability.as_str())
    }

    /// Produces the open-vocabulary capability a tool descriptor declares.
    pub fn capability(self) -> Result<Capability, RegistryError> {
        Capability::new(self.as_str())
    }

    /// Lowest risk at which this operation may be evaluated.
    ///
    /// `classified_risk` is the MCP classifier's result or, for a recipe, the
    /// maximum risk of its compiled steps. `max` means neither a caller nor a
    /// policy rule can lower the fixed floors.
    #[must_use]
    pub fn risk_floor(self, classified_risk: RiskLevel) -> RiskLevel {
        match self {
            Self::LaunchExternalAgent | Self::ConnectMcpServer | Self::InvokeMcpTool => {
                classified_risk.max(RiskLevel::Execute)
            }
            Self::ReadForgeResource => classified_risk.max(RiskLevel::Network),
            Self::PushRemoteBranch | Self::CreatePullRequest | Self::ModifyForgeResource => {
                classified_risk.max(RiskLevel::RemoteWrite)
            }
            Self::ExecuteWorkflowRecipe => classified_risk,
        }
    }

    /// Capability-specific refusal emitted when an approval cannot be answered.
    #[must_use]
    pub const fn noninteractive_denial_kind(self) -> &'static str {
        match self {
            Self::LaunchExternalAgent => "noninteractive_external_agent_launch_denied",
            Self::ConnectMcpServer => "noninteractive_mcp_server_connect_denied",
            Self::InvokeMcpTool => "noninteractive_mcp_tool_invoke_denied",
            Self::ReadForgeResource => "noninteractive_forge_resource_read_denied",
            Self::PushRemoteBranch => "noninteractive_remote_branch_push_denied",
            Self::CreatePullRequest => "noninteractive_pull_request_create_denied",
            Self::ModifyForgeResource => "noninteractive_forge_resource_modify_denied",
            Self::ExecuteWorkflowRecipe => "noninteractive_workflow_recipe_execute_denied",
        }
    }

    pub(crate) fn validate_identity_shape(
        self,
        identity: IntegrationIdentity,
    ) -> Result<(), &'static str> {
        let executable = identity.agent_executable_sha256().is_some();
        let schema = identity.mcp_tool_schema_fingerprint().is_some();
        let recipe = identity.recipe_content_hash().is_some();
        let valid_shape = match self {
            Self::LaunchExternalAgent | Self::ConnectMcpServer => executable && !schema && !recipe,
            Self::InvokeMcpTool => !executable && schema && !recipe,
            Self::ExecuteWorkflowRecipe => !executable && !schema && recipe,
            Self::ReadForgeResource
            | Self::PushRemoteBranch
            | Self::CreatePullRequest
            | Self::ModifyForgeResource => !executable && !schema && !recipe,
        };
        valid_shape
            .then_some(())
            .ok_or("external operation carries missing or irrelevant identity evidence")
    }
}

/// One permission option an ACP agent supplied with a permission request.
///
/// This is recorded for audit and presentation only. The evaluator never reads
/// it when choosing a verdict.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPermissionOption {
    /// Agent offers permission for this request only.
    AllowOnce,
    /// Agent offers standing permission; Harkness still applies its own ceiling.
    AllowAlways,
    /// Agent offers rejecting this request only.
    RejectOnce,
    /// Agent offers a standing rejection.
    RejectAlways,
}

/// Advisory MCP tool annotations captured with a policy request.
///
/// A server controls these values, so none can lower risk or answer policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolAnnotations {
    /// Server claims the tool is read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Server claims the tool may be destructive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Server claims repeated calls are idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Server claims the tool can reach an open world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// External permission information retained as advisory audit context.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalPermissionContext {
    /// ACP option associated with this request, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_option: Option<AcpPermissionOption>,
    /// MCP annotations published for the imported tool, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_annotations: Option<McpToolAnnotations>,
}

/// External-integration facts attached to one policy evaluation.
///
/// This strict, explicitly versioned value is copied into the durable policy
/// decision. Hash fields omitted by older producers remain absent, while an
/// unknown same-version field is rejected rather than discarded on rewrite.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ExternalPolicyContext {
    schema_version: u32,
    capability: ExternalCapability,
    classified_risk: RiskLevel,
    #[serde(default, skip_serializing_if = "IntegrationIdentity::is_empty")]
    identity: IntegrationIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external_permission_context: Option<ExternalPermissionContext>,
}

impl ExternalPolicyContext {
    /// Starts an ACP-agent launch context from the executable observation.
    #[must_use]
    pub const fn launch_external_agent(executable: Option<Sha256Hash>) -> Self {
        Self::fixed(
            ExternalCapability::LaunchExternalAgent,
            IntegrationIdentity::none().with_agent_executable_sha256_optional(executable),
        )
    }

    /// Starts an MCP-server connection context from the executable observation.
    #[must_use]
    pub const fn connect_mcp_server(executable: Option<Sha256Hash>) -> Self {
        Self::fixed(
            ExternalCapability::ConnectMcpServer,
            IntegrationIdentity::none().with_agent_executable_sha256_optional(executable),
        )
    }

    /// Starts an imported MCP-tool invocation context.
    ///
    /// The classification is opaque and can only be produced by Harkness's
    /// request classifier, so a caller cannot replace a destructive result with
    /// an asserted lower risk.
    #[must_use]
    pub const fn invoke_mcp_tool(
        classification: RequestClassification,
        schema: Option<Sha256Hash>,
    ) -> Self {
        Self::from_parts(
            ExternalCapability::InvokeMcpTool,
            classification.risk(),
            IntegrationIdentity::none().with_mcp_tool_schema_fingerprint_optional(schema),
        )
    }

    /// Starts a forge-resource read context.
    #[must_use]
    pub const fn read_forge_resource() -> Self {
        Self::fixed(
            ExternalCapability::ReadForgeResource,
            IntegrationIdentity::none(),
        )
    }

    /// Starts a remote-branch push context.
    #[must_use]
    pub const fn push_remote_branch() -> Self {
        Self::fixed(
            ExternalCapability::PushRemoteBranch,
            IntegrationIdentity::none(),
        )
    }

    /// Starts a pull-request creation context.
    #[must_use]
    pub const fn create_pull_request() -> Self {
        Self::fixed(
            ExternalCapability::CreatePullRequest,
            IntegrationIdentity::none(),
        )
    }

    /// Starts a forge-resource mutation context.
    #[must_use]
    pub const fn modify_forge_resource() -> Self {
        Self::fixed(
            ExternalCapability::ModifyForgeResource,
            IntegrationIdentity::none(),
        )
    }

    /// Starts a compiled workflow-recipe context.
    ///
    /// The effective classification is derived as the maximum risk of the
    /// compiled steps. An empty recipe is observational; fixed policy floors
    /// are applied separately by [`Self::risk_floor`].
    #[must_use]
    pub fn execute_workflow_recipe(
        step_risks: impl IntoIterator<Item = RiskLevel>,
        content: Option<Sha256Hash>,
    ) -> Self {
        let classified_risk = step_risks.into_iter().max().unwrap_or(RiskLevel::Observe);
        Self::from_parts(
            ExternalCapability::ExecuteWorkflowRecipe,
            classified_risk,
            IntegrationIdentity::none().with_recipe_content_hash_optional(content),
        )
    }

    const fn fixed(capability: ExternalCapability, identity: IntegrationIdentity) -> Self {
        let risk = match capability {
            ExternalCapability::LaunchExternalAgent
            | ExternalCapability::ConnectMcpServer
            | ExternalCapability::InvokeMcpTool => RiskLevel::Execute,
            ExternalCapability::ReadForgeResource => RiskLevel::Network,
            ExternalCapability::PushRemoteBranch
            | ExternalCapability::CreatePullRequest
            | ExternalCapability::ModifyForgeResource => RiskLevel::RemoteWrite,
            ExternalCapability::ExecuteWorkflowRecipe => RiskLevel::Observe,
        };
        Self::from_parts(capability, risk, identity)
    }

    const fn from_parts(
        capability: ExternalCapability,
        classified_risk: RiskLevel,
        identity: IntegrationIdentity,
    ) -> Self {
        Self {
            schema_version: EXTERNAL_POLICY_CONTEXT_SCHEMA_VERSION,
            capability,
            classified_risk,
            identity,
            external_permission_context: None,
        }
    }

    /// Attaches untrusted external permission hints for audit and presentation.
    #[must_use]
    pub const fn with_permission_context(mut self, context: ExternalPermissionContext) -> Self {
        self.external_permission_context = Some(context);
        self
    }

    /// External capability being evaluated.
    #[must_use]
    pub const fn capability(self) -> ExternalCapability {
        self.capability
    }

    /// Risk assigned by classification or by the compiled recipe plan.
    #[must_use]
    pub const fn classified_risk(self) -> RiskLevel {
        self.classified_risk
    }

    /// Effective external risk after applying the fixed capability floor.
    #[must_use]
    pub fn risk_floor(self) -> RiskLevel {
        self.capability.risk_floor(self.classified_risk)
    }

    pub(super) const fn schema_version(self) -> u32 {
        self.schema_version
    }

    /// Identity evidence that approvals must bind exactly.
    #[must_use]
    pub const fn identity(self) -> IntegrationIdentity {
        self.identity
    }

    /// Advisory context retained in the audit record.
    #[must_use]
    pub const fn permission_context(self) -> Option<ExternalPermissionContext> {
        self.external_permission_context
    }

    pub(super) fn validate_declaration(self, declared: &[Capability]) -> Result<(), &'static str> {
        if !declared
            .iter()
            .any(|capability| capability.as_str() == self.capability.as_str())
        {
            return Err("external policy context names a capability the tool did not declare");
        }
        Ok(())
    }

    pub(super) fn validate_identity_shape(self) -> Result<(), &'static str> {
        self.capability.validate_identity_shape(self.identity)
    }

    /// Stable denial kind for the context's exact invalid identity shape.
    #[must_use]
    pub(super) const fn invalid_identity_denial_kind(self) -> Option<&'static str> {
        let executable = self.identity.agent_executable_sha256().is_some();
        let schema = self.identity.mcp_tool_schema_fingerprint().is_some();
        let recipe = self.identity.recipe_content_hash().is_some();
        let has_irrelevant_identity = match self.capability {
            ExternalCapability::LaunchExternalAgent | ExternalCapability::ConnectMcpServer => {
                schema || recipe
            }
            ExternalCapability::InvokeMcpTool => executable || recipe,
            ExternalCapability::ExecuteWorkflowRecipe => executable || schema,
            ExternalCapability::ReadForgeResource
            | ExternalCapability::PushRemoteBranch
            | ExternalCapability::CreatePullRequest
            | ExternalCapability::ModifyForgeResource => executable || schema || recipe,
        };
        if has_irrelevant_identity {
            Some("external_identity_context_invalid")
        } else {
            self.identity_denial_kind()
        }
    }

    /// Stable refusal for a required identity that is absent.
    #[must_use]
    pub(super) const fn identity_denial_kind(self) -> Option<&'static str> {
        match self.capability {
            ExternalCapability::LaunchExternalAgent | ExternalCapability::ConnectMcpServer
                if self.identity.agent_executable_sha256().is_none() =>
            {
                Some("agent_executable_identity_required")
            }
            ExternalCapability::InvokeMcpTool
                if self.identity.mcp_tool_schema_fingerprint().is_none() =>
            {
                Some("mcp_tool_schema_identity_required")
            }
            ExternalCapability::ExecuteWorkflowRecipe
                if self.identity.recipe_content_hash().is_none() =>
            {
                Some("recipe_content_identity_required")
            }
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct ExternalPolicyContextVersionProbe {
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalPolicyContextStrict {
    schema_version: u32,
    capability: ExternalCapability,
    classified_risk: RiskLevel,
    #[serde(default)]
    identity: IntegrationIdentity,
    #[serde(default)]
    external_permission_context: Option<ExternalPermissionContext>,
}

impl From<ExternalPolicyContextStrict> for ExternalPolicyContext {
    fn from(context: ExternalPolicyContextStrict) -> Self {
        Self {
            schema_version: context.schema_version,
            capability: context.capability,
            classified_risk: context.classified_risk,
            identity: context.identity,
            external_permission_context: context.external_permission_context,
        }
    }
}

impl<'de> Deserialize<'de> for ExternalPolicyContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let probe =
            ExternalPolicyContextVersionProbe::deserialize(&value).map_err(de::Error::custom)?;
        if probe.schema_version != EXTERNAL_POLICY_CONTEXT_SCHEMA_VERSION {
            return Err(de::Error::custom(
                "external policy context was written by a newer Harkness build",
            ));
        }
        ExternalPolicyContextStrict::deserialize(value)
            .map(Into::into)
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{EXTERNAL_POLICY_DENIAL_KINDS, ExternalCapability};
    use crate::tool::RiskLevel;

    #[test]
    fn every_external_capability_has_one_stable_contract_entry() {
        fn force_exhaustive(capability: ExternalCapability) {
            match capability {
                ExternalCapability::LaunchExternalAgent
                | ExternalCapability::ConnectMcpServer
                | ExternalCapability::InvokeMcpTool
                | ExternalCapability::ReadForgeResource
                | ExternalCapability::PushRemoteBranch
                | ExternalCapability::CreatePullRequest
                | ExternalCapability::ModifyForgeResource
                | ExternalCapability::ExecuteWorkflowRecipe => {}
            }
        }

        let spellings = ExternalCapability::ALL
            .iter()
            .copied()
            .map(|capability| {
                force_exhaustive(capability);
                assert_eq!(
                    ExternalCapability::from_capability(&capability.capability().unwrap()),
                    Some(capability)
                );
                capability.as_str()
            })
            .collect::<HashSet<_>>();
        assert_eq!(spellings.len(), 8);
    }

    #[test]
    fn denial_kind_table_has_the_eight_noninteractive_kinds_first_and_no_duplicates() {
        let expected = ExternalCapability::ALL
            .iter()
            .copied()
            .map(ExternalCapability::noninteractive_denial_kind)
            .collect::<Vec<_>>();
        assert_eq!(&EXTERNAL_POLICY_DENIAL_KINDS[..8], expected);
        assert_eq!(
            EXTERNAL_POLICY_DENIAL_KINDS
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len(),
            EXTERNAL_POLICY_DENIAL_KINDS.len()
        );
    }

    #[test]
    fn fixed_floors_can_only_raise_a_classification() {
        for capability in ExternalCapability::ALL.iter().copied() {
            for classified in RiskLevel::ALL.iter().copied() {
                assert!(capability.risk_floor(classified) >= classified);
            }
        }
    }
}
