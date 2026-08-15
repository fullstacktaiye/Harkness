use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::value::RawValue;

use super::error::truncate;
use super::schema;
use super::{
    ExecutionContext, MAX_FAILURE_MESSAGE_BYTES, RegistryError, SchemaDirection, SchemaViolation,
    ToolDescriptor, ToolError, ToolIdentity, ToolMetadata,
};
use crate::trust::{
    ContainedPath, PathAccess, PathBoundary, RequestClassification, RequestFlags, RequestPath,
    classify_request,
};

/// Invocation facts derived from validated typed input before policy runs.
#[derive(Clone, Debug, Default)]
pub struct RequestEffects {
    paths: Vec<(ContainedPath, PathAccess)>,
    flags: RequestFlags,
}

impl RequestEffects {
    /// Adds one already-contained path and its concrete access mode.
    #[must_use]
    pub fn with_path(mut self, path: ContainedPath, access: PathAccess) -> Self {
        self.paths.push((path, access));
        self
    }

    /// Replaces the non-filesystem effects of this invocation.
    #[must_use]
    pub const fn with_flags(mut self, flags: RequestFlags) -> Self {
        self.flags = flags;
        self
    }
}

/// A schema-valid request prepared for policy evaluation without execution.
#[derive(Clone, Debug)]
pub struct PreparedRequest {
    paths: Vec<ContainedPath>,
    classification: RequestClassification,
}

impl PreparedRequest {
    /// Contained paths policy may inspect.
    #[must_use]
    pub fn paths(&self) -> &[ContainedPath] {
        &self.paths
    }

    /// Effective risk and force variant derived from the descriptor and input.
    #[must_use]
    pub const fn classification(&self) -> RequestClassification {
        self.classification
    }
}

/// Name the artifact store holds a schema-refused result under.
///
/// Published because it is how a consumer finds the evidence: an
/// [`ToolError::InvalidOutput`] says *where* the value broke its schema, and
/// this names the artifact holding the value itself.
pub const REJECTED_OUTPUT_ARTIFACT: &str = "rejected-output.json";

/// One typed operation the runtime can execute.
///
/// A tool states its input and output as Rust types and its metadata as data;
/// everything published about it — the JSON Schemas, the validation gates, the
/// panic boundary — is derived from those. There is no way to implement this
/// trait and end up with a published contract that disagrees with the types the
/// body actually handles, which is the property that lets the GUI, the CLI, the
/// workflow engine, and an agent all drive the same operation.
///
/// # Unknown fields
///
/// `schemars` closes an object schema — emits `additionalProperties: false` —
/// only when the type carries `#[serde(deny_unknown_fields)]`. A tool that omits
/// it publishes an open schema, and an unexpected key in the input is silently
/// discarded by serde rather than refused. **Add
/// `#[serde(deny_unknown_fields)]` to every `Input` type.** An agent that
/// misspells a field name should be told, not quietly ignored, and the published
/// schema is what tells it.
///
/// # Example
///
/// ```
/// use harkness_runtime::domain::{RunId, StepId, ToolCallId};
/// use harkness_runtime::tool::{
///     ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, ToolRegistry,
/// };
/// use schemars::JsonSchema;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Deserialize, JsonSchema)]
/// #[serde(deny_unknown_fields)]
/// struct GreetInput {
///     name: String,
/// }
///
/// #[derive(Serialize, JsonSchema)]
/// struct GreetOutput {
///     greeting: String,
/// }
///
/// struct Greet;
///
/// impl Tool for Greet {
///     type Input = GreetInput;
///     type Output = GreetOutput;
///
///     fn metadata(&self) -> ToolMetadata {
///         ToolMetadata::new(
///             ToolIdentity::parse("example.greet", "1.0.0").expect("a valid identity"),
///             "Greet someone",
///             "Returns a greeting for the supplied name.",
///             RiskLevel::Observe,
///         )
///     }
///
///     fn execute(
///         &self,
///         input: GreetInput,
///         _context: &mut ExecutionContext,
///     ) -> Result<GreetOutput, ToolError> {
///         Ok(GreetOutput {
///             greeting: format!("hello, {}", input.name),
///         })
///     }
/// }
///
/// let mut registry = ToolRegistry::new();
/// registry.register(Greet)?;
///
/// let workspace = std::env::temp_dir();
/// let mut context =
///     ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), workspace)?;
/// let outcome = harkness_runtime::tool::invoke(
///     &registry,
///     &"example.greet".parse()?,
///     None,
///     &serde_json::from_str::<Box<serde_json::value::RawValue>>(r#"{"name":"world"}"#)?,
///     &mut context,
/// )?;
///
/// assert_eq!(outcome.tool().to_string(), "example.greet@1.0.0");
/// assert_eq!(outcome.output().get(), r#"{"greeting":"hello, world"}"#);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait Tool: Send + Sync {
    /// What the tool accepts. Its JSON Schema is generated from this type.
    type Input: DeserializeOwned + JsonSchema;

    /// What the tool returns. Its JSON Schema is generated from this type.
    type Output: Serialize + JsonSchema;

    /// Declares the tool's identity, wording, risk, and capabilities.
    ///
    /// Called once, at registration. Returning different metadata on a later
    /// call has no effect, because the descriptor the registry publishes is
    /// built here and then frozen.
    fn metadata(&self) -> ToolMetadata;

    /// Derives policy facts from schema-valid typed input without executing it.
    ///
    /// The default adds no invocation-specific facts; the descriptor's risk is
    /// still a floor. Implementations may resolve path arguments through
    /// `boundary` and raise effects, but must not perform the operation.
    fn request_effects(
        &self,
        _input: &Self::Input,
        _boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        Ok(RequestEffects::default())
    }

    /// Performs the operation.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] describing why the operation did not complete.
    /// Prefer [`ToolError::execution_failed`] for a tool-specific failure and
    /// [`ExecutionContext::check_cancelled`] for cooperative cancellation.
    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError>;
}

/// A registered tool addressed through JSON rather than through its own types.
///
/// This is what the registry stores, because a `HashMap` cannot hold values of
/// differing associated types. It is public so a registry can be enumerated and
/// passed around, and **sealed** so it cannot be implemented outside this module:
/// [`erase`] is the only way to produce one.
///
/// The seal is what makes this module's guarantees guarantees. A hand-written
/// implementation could publish a descriptor whose `input_schema` bears no
/// relation to what its `execute_json` deserializes, skip the cancellation gate,
/// skip the [`catch_unwind`] boundary, and skip both validation gates — while
/// `harkness contract` still advertised it as a validated contract and
/// [`ToolError::happened_before_execution`] still promised callers a safe retry.
/// Since [`ToolRegistry::register_erased`](super::ToolRegistry::register_erased)
/// accepts any `Arc<dyn ErasedTool>`, documenting "please do not implement this"
/// would have left every one of those properties on the honour system.
///
/// [`Debug`](std::fmt::Debug) is a supertrait so that an `Arc<dyn ErasedTool>`
/// can appear in an assertion or a log line at all; it renders the tool's
/// identity, never its state.
pub trait ErasedTool: sealed::Sealed + std::fmt::Debug + Send + Sync {
    /// The frozen published contract of this tool.
    fn descriptor(&self) -> &ToolDescriptor;

    /// Validates and deserializes input, then derives policy facts without
    /// invoking the tool body.
    fn prepare_json(
        &self,
        input: &RawValue,
        boundary: &PathBoundary,
    ) -> Result<PreparedRequest, ToolError>;

    /// Runs the full invocation pipeline for one JSON input.
    ///
    /// The order is fixed and total: validate the input against the published
    /// schema, deserialize it into the tool's own type, run the body under a
    /// panic boundary, serialize the result, validate the result against the
    /// published schema, and only then return it.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] before the body runs,
    /// [`ToolError::ToolPanicked`] if the body panicked,
    /// [`ToolError::InvalidOutput`] if the result does not match its schema, or
    /// whatever the tool itself reported.
    fn execute_json(
        &self,
        input: &RawValue,
        context: &mut ExecutionContext,
    ) -> Result<Box<RawValue>, ToolError>;
}

/// Erases a typed tool, generating and compiling its schemas.
///
/// # Errors
///
/// Returns [`RegistryError::InvalidMetadata`] when the declared wording is
/// unusable, or [`RegistryError::InvalidSchema`] when a generated schema cannot
/// be compiled. Both are registration-time failures by design: a tool that
/// cannot be validated is a tool that must not be reachable.
pub fn erase<T>(tool: T) -> Result<Arc<dyn ErasedTool>, RegistryError>
where
    T: Tool + 'static,
{
    let metadata = tool.metadata();
    metadata.validate()?;
    let identity = metadata.identity().clone();

    let input_schema = schema::generate::<T::Input>();
    let output_schema = schema::generate::<T::Output>();
    let input = schema::compile(&identity, SchemaDirection::Input, &input_schema)?;
    let output = schema::compile(&identity, SchemaDirection::Output, &output_schema)?;

    Ok(Arc::new(TypedTool {
        tool,
        descriptor: ToolDescriptor::new(metadata, input_schema, output_schema),
        input,
        output,
    }))
}

/// Seals [`ErasedTool`]. Private, so no downstream crate can name — and therefore
/// cannot satisfy — the supertrait it requires.
mod sealed {
    pub trait Sealed {}
}

impl<T> sealed::Sealed for TypedTool<T> {}

/// The erasure boundary for one typed tool.
struct TypedTool<T> {
    tool: T,
    descriptor: ToolDescriptor,
    input: jsonschema::Validator,
    output: jsonschema::Validator,
}

impl<T> std::fmt::Debug for TypedTool<T> {
    /// Names the tool. The body, the compiled validators, and the tool's own
    /// state are all opaque and none of them belongs in a log line.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tool")
            .field("identity", &self.descriptor.identity().to_string())
            .finish_non_exhaustive()
    }
}

impl<T> ErasedTool for TypedTool<T>
where
    T: Tool,
{
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    fn prepare_json(
        &self,
        input: &RawValue,
        boundary: &PathBoundary,
    ) -> Result<PreparedRequest, ToolError> {
        let identity = self.descriptor.identity();
        let instance = serde_json::from_str::<Value>(input.get()).map_err(|error| {
            schema::refusal(
                identity,
                SchemaDirection::Input,
                vec![SchemaViolation::new("", "", error.to_string())],
                0,
            )
        })?;
        schema::validate(&self.input, identity, SchemaDirection::Input, &instance)?;
        let typed = schema::deserialize_input::<T::Input>(identity, instance)?;
        let effects = self.tool.request_effects(&typed, boundary)?;
        let request_paths = effects
            .paths
            .iter()
            .map(|(path, access)| RequestPath::new(path, *access))
            .collect::<Vec<_>>();
        let classification = classify_request(&self.descriptor, &request_paths, effects.flags);
        Ok(PreparedRequest {
            paths: effects.paths.into_iter().map(|(path, _)| path).collect(),
            classification,
        })
    }

    fn execute_json(
        &self,
        input: &RawValue,
        context: &mut ExecutionContext,
    ) -> Result<Box<RawValue>, ToolError> {
        let identity = self.descriptor.identity();

        // Cancellation is checked before anything else, and again immediately
        // before the body. Honouring it only inside the body would make
        // "cancelled work does not happen" a property of well-written tools
        // rather than of the pipeline: a tool that does its work in one
        // non-polling call — a push, a request, a delete — would run to
        // completion after the user had already cancelled the run. These two
        // gates cannot close the window entirely, because a token cancelled
        // during a long call still relies on the tool polling, but they do
        // guarantee that a tool dispatched after cancellation never starts.
        context.check_cancelled()?;

        let instance = serde_json::from_str::<Value>(input.get()).map_err(|error| {
            schema::refusal(
                identity,
                SchemaDirection::Input,
                vec![SchemaViolation::new("", "", error.to_string())],
                0,
            )
        })?;
        schema::validate(&self.input, identity, SchemaDirection::Input, &instance)?;
        let typed = schema::deserialize_input::<T::Input>(identity, instance)?;

        context.check_cancelled()?;

        // The body is the only foreign code in this pipeline, so it is the only
        // part that runs inside a panic boundary. Everything before it has
        // already refused a malformed input, so a panic here is a bug in the
        // tool rather than a reaction to untrusted data — and a bug in one tool
        // must not take down the coordinator thread and orphan the run record.
        let executed = catch_unwind(AssertUnwindSafe(|| self.tool.execute(typed, context)));
        let produced = match executed {
            Ok(result) => result.map_err(|error| reattribute(identity, error))?,
            Err(payload) => {
                return Err(ToolError::ToolPanicked {
                    tool: identity.clone(),
                    payload: panic_message(&*payload),
                });
            }
        };

        let produced = serde_json::to_value(&produced).map_err(|error| {
            schema::refusal(
                identity,
                SchemaDirection::Output,
                vec![SchemaViolation::new("", "", error.to_string())],
                0,
            )
        })?;
        if let Err(rejection) =
            schema::validate(&self.output, identity, SchemaDirection::Output, &produced)
        {
            preserve_rejected_output(context, &produced);
            return Err(rejection);
        }

        serde_json::value::to_raw_value(&produced).map_err(|error| {
            schema::refusal(
                identity,
                SchemaDirection::Output,
                vec![SchemaViolation::new("", "", error.to_string())],
                0,
            )
        })
    }
}

/// Stores a result the output schema refused, so the evidence is not discarded.
///
/// The refusal itself locates the violations, but only the value says what the
/// tool actually produced — and that is what an author debugging a contract
/// mismatch, or a reviewer auditing what a run tried to return, needs to read.
/// It cannot be the call's *result*: a consumer that trusted the published
/// schema must never receive a shape it cannot handle, which is the whole point
/// of the gate. So it goes where every other oversized, untrusted, non-result
/// byte goes.
///
/// Best effort by design. A context with no artifact store attached — a test, a
/// one-shot invocation — refuses the write, and losing the evidence must not
/// change the failure a caller is told about.
fn preserve_rejected_output(context: &mut ExecutionContext, produced: &Value) {
    let Ok(encoded) = serde_json::to_vec(produced) else {
        return;
    };
    let _ = context.write_artifact(REJECTED_OUTPUT_ARTIFACT, "application/json", &encoded);
}

/// Re-labels a failure a tool body raised but is not entitled to claim.
///
/// [`ToolError::happened_before_execution`] answers `true` for
/// [`ToolError::InvalidInput`] because *this* pipeline raises it before calling
/// the body. `ToolError` is `#[non_exhaustive]` at the enum level, which does not
/// seal its variants, so a tool that validates a sub-field itself can construct
/// one and return it — after having already written a file. Left alone, that error
/// would tell a coordinator the body never started and licence a retry that
/// applies the earlier write twice.
///
/// The detail is kept, as an `execution_failed` message: the tool's complaint is
/// still worth reading, it just cannot masquerade as a pre-execution refusal.
/// `InvalidOutput` is re-labelled too, for the same reason — it is this module's
/// verdict on the tool's result, not the tool's own.
fn reattribute(identity: &ToolIdentity, error: ToolError) -> ToolError {
    match error {
        ToolError::InvalidInput { .. } | ToolError::InvalidOutput { .. } => {
            ToolError::execution_failed(format!(
                "{identity} reported a schema violation from inside its own body, \
                 which the invocation pipeline had already validated: {error}"
            ))
        }
        other => other,
    }
}

/// Recovers a panic payload's text when it is one of the two shapes the standard
/// library produces for `panic!`.
///
/// A payload of any other type is reported as `None` rather than guessed at: the
/// kind is already `tool_panicked`, and inventing a message for an opaque
/// payload would misattribute it.
///
/// A recovered payload is a formatted string a tool chose, which can quote its own
/// input, so it is clamped like every other text that reaches
/// [`ToolError::as_failure`].
fn panic_message(payload: &(dyn Any + Send)) -> Option<String> {
    let recovered = payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())?;
    Some(truncate(recovered, MAX_FAILURE_MESSAGE_BYTES))
}
