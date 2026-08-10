use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::value::RawValue;

use super::{
    ErasedTool, ExecutionContext, InvocationError, RegistryError, Tool, ToolDescriptor, ToolError,
    ToolId, ToolIdentity, ToolVersion, erase,
};

/// What one successful invocation produced.
///
/// The resolved identity travels with the output because a caller that asked for
/// "the latest `fs.read`" has to record *which* version actually ran: that pair
/// is what `tool_calls.tool_version` stores and what an approval is matched
/// against. Returning the output alone would make the pinned version something
/// the caller had to look up again, and a second lookup can disagree with the
/// first.
#[derive(Clone, Debug)]
pub struct ToolOutcome {
    tool: ToolIdentity,
    output: Box<RawValue>,
}

impl PartialEq for ToolOutcome {
    /// Compares the identity and the serialized result text.
    ///
    /// `RawValue` holds the bytes the tool's output serialized to, and those
    /// bytes are what a caller stores and what a downstream consumer parses.
    /// Comparing them directly is therefore the comparison that matters, and it
    /// is stable because the pipeline always re-serializes through one path.
    fn eq(&self, other: &Self) -> bool {
        self.tool == other.tool && self.output.get() == other.output.get()
    }
}

impl Eq for ToolOutcome {}

impl ToolOutcome {
    /// The `(id, version)` that actually executed.
    #[must_use]
    pub const fn tool(&self) -> &ToolIdentity {
        &self.tool
    }

    /// The schema-validated JSON result.
    #[must_use]
    pub const fn output(&self) -> &RawValue {
        &self.output
    }

    /// Takes the result, discarding the identity.
    #[must_use]
    pub fn into_output(self) -> Box<RawValue> {
        self.output
    }

    /// Splits the outcome into the identity that ran and the result it produced.
    #[must_use]
    pub fn into_parts(self) -> (ToolIdentity, Box<RawValue>) {
        (self.tool, self.output)
    }
}

/// The tools one process can execute, keyed by `(id, version)`.
///
/// A registered tool is immutable. There is no method to replace or remove one,
/// because a recorded call and an approval both name a version and expect it to
/// mean what it meant when they were written; a registry that could rebind
/// `fs.read@1.0.0` would make every such record unverifiable. Publishing a
/// change means registering a new version alongside the old one.
///
/// Two requirements pull in different directions here, so each gets its own
/// index over the same shared `Arc`s: resolution has to be a constant-time hash
/// hit, and enumeration has to be ordered by identifier and then by version
/// precedence so `harkness contract` output and anything generated from it are
/// byte-stable regardless of registration order. Neither structure alone gives
/// both, and both are written in the same `register_erased`, so an entry cannot
/// exist in one and not the other.
#[derive(Default)]
pub struct ToolRegistry {
    /// Constant-time resolution by exact identity.
    tools: HashMap<ToolIdentity, Arc<dyn ErasedTool>>,
    /// The same tools, ordered by identifier and then by version precedence.
    ordered: BTreeMap<ToolId, BTreeMap<ToolVersion, Arc<dyn ErasedTool>>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Generates schemas for a typed tool, then registers it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidMetadata`] or
    /// [`RegistryError::InvalidSchema`] when the tool cannot be declared, and
    /// [`RegistryError::DuplicateRegistration`] when its `(id, version)` is
    /// already taken. Registering the same identifier at a *different* version
    /// succeeds; that is how a tool evolves.
    pub fn register<T>(&mut self, tool: T) -> Result<(), RegistryError>
    where
        T: Tool + 'static,
    {
        self.register_erased(erase(tool)?)
    }

    /// Registers a tool that has already been erased.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateRegistration`] when the tool's
    /// `(id, version)` is already registered.
    pub fn register_erased(&mut self, tool: Arc<dyn ErasedTool>) -> Result<(), RegistryError> {
        let identity = tool.descriptor().identity().clone();
        if self.tools.contains_key(&identity) {
            return Err(RegistryError::DuplicateRegistration { tool: identity });
        }

        self.ordered
            .entry(identity.id.clone())
            .or_default()
            .insert(identity.version.clone(), Arc::clone(&tool));
        self.tools.insert(identity, tool);
        Ok(())
    }

    /// Looks a tool up, or returns `None`.
    ///
    /// `version` of `None` resolves exactly as [`Self::resolve`] does: the highest
    /// registered *stable* version by semantic-version precedence, falling back to
    /// a pre-release only when no stable version exists. The two methods differ
    /// only in whether a miss is an `Option` or a typed error — never in which tool
    /// they pick.
    #[must_use]
    pub fn get(&self, id: &ToolId, version: Option<&ToolVersion>) -> Option<&Arc<dyn ErasedTool>> {
        match version {
            Some(version) => self
                .tools
                .get(&ToolIdentity::new(id.clone(), version.clone())),
            None => self.latest(id).map(|(_, tool)| tool),
        }
    }

    /// Looks a tool up, reporting why it was not found.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::UnknownTool`] when the identifier is not
    /// registered at all, and [`RegistryError::UnknownToolVersion`] — listing the
    /// versions that do exist — when only the requested version is missing. The
    /// two are distinguished because they call for different fixes: one is a
    /// wrong name, the other a stale pin.
    pub fn resolve(
        &self,
        id: &ToolId,
        version: Option<&ToolVersion>,
    ) -> Result<&Arc<dyn ErasedTool>, RegistryError> {
        let Some(by_version) = self.ordered.get(id) else {
            return Err(RegistryError::UnknownTool { id: id.to_string() });
        };

        let Some(version) = version else {
            return self
                .latest(id)
                .map(|(_, tool)| tool)
                .ok_or_else(|| RegistryError::UnknownTool { id: id.to_string() });
        };

        // The hash index answers the exact case, so a pinned resolution stays a
        // constant-time hit no matter how many versions of the id exist.
        self.tools
            .get(&ToolIdentity::new(id.clone(), version.clone()))
            .ok_or_else(|| RegistryError::UnknownToolVersion {
                id: id.to_string(),
                version: version.to_string(),
                available: by_version.keys().map(ToString::to_string).collect(),
            })
    }

    /// The highest registered version of `id` by precedence.
    #[must_use]
    pub fn latest_version(&self, id: &ToolId) -> Option<&ToolVersion> {
        self.latest(id).map(|(version, _)| version)
    }

    /// Every registered version of `id`, in ascending precedence order.
    #[must_use]
    pub fn versions(&self, id: &ToolId) -> Vec<&ToolVersion> {
        self.ordered
            .get(id)
            .map(|by_version| by_version.keys().collect())
            .unwrap_or_default()
    }

    /// Every descriptor, ordered by identifier and then by version precedence.
    ///
    /// The order is a contract: it is what makes generated documentation and
    /// `harkness contract` output diff-stable rather than dependent on hash
    /// iteration order.
    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.ordered
            .values()
            .flat_map(|by_version| by_version.values().map(|tool| tool.descriptor()))
    }

    /// Every registered identifier, in ascending order.
    pub fn ids(&self) -> impl Iterator<Item = &ToolId> {
        self.ordered.keys()
    }

    /// The entry an unversioned resolution selects for `id`.
    ///
    /// The highest *stable* version wins, and a pre-release is chosen only when no
    /// stable version is registered. Taking the highest by raw precedence would
    /// mean that registering `2.0.0-rc.1` beside a production `1.10.0` instantly
    /// redirects every caller that passes no version — which is the documented
    /// default entry point — onto the release candidate. Publishing a pre-release
    /// must not be a way to change what production runs; a caller that wants one
    /// asks for it by version.
    fn latest(&self, id: &ToolId) -> Option<(&ToolVersion, &Arc<dyn ErasedTool>)> {
        let by_version = self.ordered.get(id)?;
        by_version
            .iter()
            .rev()
            .find(|(version, _)| !version.is_prerelease())
            .or_else(|| by_version.last_key_value())
    }

    /// How many `(id, version)` pairs are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolRegistry {
    /// Lists the registered identities rather than the opaque trait objects.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field(
                "tools",
                &self
                    .descriptors()
                    .map(|descriptor| descriptor.identity().to_string())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Resolves a tool and runs one invocation against it.
///
/// This is the whole entry point. It needs a registry, an identity, a JSON
/// input, and a context — no agent, no policy engine, no database — so a test,
/// the CLI, and a coordinator all drive tools through exactly the interface an
/// agent will use.
///
/// `version` of `None` resolves the highest registered *stable* version. The
/// version that actually ran is reported on both paths — in the [`ToolOutcome`]
/// on success and on [`InvocationError::Tool`] on failure — so a caller records
/// what executed rather than what it asked for, and never has to re-resolve to
/// find out. Two lookups can disagree; one cannot.
///
/// # Errors
///
/// Returns [`InvocationError::Resolution`] when the tool or version does not
/// exist, and [`InvocationError::Tool`] — carrying the resolved identity — for
/// everything the invocation itself can report: schema violations in either
/// direction, a contained panic, or the tool's own failure.
pub fn invoke(
    registry: &ToolRegistry,
    id: &ToolId,
    version: Option<&ToolVersion>,
    input: &RawValue,
    context: &mut ExecutionContext,
) -> Result<ToolOutcome, InvocationError> {
    let tool = registry.resolve(id, version)?;
    // Resolved once, then used for both the outcome and any failure. A `?` on
    // `execute_json` would drop it, which is why `InvocationError` has no
    // `From<ToolError>` to make that easy.
    let identity = tool.descriptor().identity().clone();
    match tool.execute_json(input, context) {
        Ok(output) => Ok(ToolOutcome {
            tool: identity,
            output,
        }),
        Err(error) => Err(InvocationError::from_tool(identity, error)),
    }
}

/// Runs one invocation against an already-resolved tool.
///
/// The half of [`invoke`] a caller wants when it resolved the tool earlier — to
/// evaluate policy against the descriptor, or to write the pending record —
/// before anything executes.
///
/// # Errors
///
/// Returns whatever the invocation reports; see [`ErasedTool::execute_json`]. The
/// identity is not attached here because the caller already holds the tool it
/// resolved and can read the descriptor directly.
pub fn invoke_resolved(
    tool: &Arc<dyn ErasedTool>,
    input: &RawValue,
    context: &mut ExecutionContext,
) -> Result<ToolOutcome, ToolError> {
    let identity = tool.descriptor().identity().clone();
    let output = tool.execute_json(input, context)?;
    Ok(ToolOutcome {
        tool: identity,
        output,
    })
}
