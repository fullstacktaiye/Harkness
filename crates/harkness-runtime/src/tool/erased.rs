use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::value::RawValue;

use super::schema;
use super::{
    ExecutionContext, RegistryError, SchemaDirection, SchemaViolation, ToolDescriptor, ToolError,
    ToolMetadata,
};

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
/// differing associated types. Every implementation is produced by
/// [`erase`] — the trait is public so a registry can be enumerated and passed
/// around, not so it can be implemented by hand.
///
/// [`Debug`](std::fmt::Debug) is a supertrait so that an `Arc<dyn ErasedTool>`
/// can appear in an assertion or a log line at all; it renders the tool's
/// identity, never its state.
pub trait ErasedTool: std::fmt::Debug + Send + Sync {
    /// The frozen published contract of this tool.
    fn descriptor(&self) -> &ToolDescriptor;

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
            Ok(result) => result?,
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
        schema::validate(&self.output, identity, SchemaDirection::Output, &produced)?;

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

/// Recovers a panic payload's text when it is one of the two shapes the standard
/// library produces for `panic!`.
///
/// A payload of any other type is reported as `None` rather than guessed at: the
/// kind is already `tool_panicked`, and inventing a message for an opaque
/// payload would misattribute it.
fn panic_message(payload: &(dyn Any + Send)) -> Option<String> {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return Some((*message).to_owned());
    }
    payload.downcast_ref::<String>().cloned()
}
