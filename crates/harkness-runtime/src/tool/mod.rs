//! The typed tool contract: descriptors, risk, errors, context, and registry.
//!
//! # Why a contract layer exists at all
//!
//! Every front end and every agent in Harkness performs the same underlying
//! operations, and the tempting shortcut is to let each of them pass a string to
//! something that interprets it. That shortcut is what makes a policy
//! unenforceable: a decision about "running a shell command" cannot be made
//! about a blob nobody has parsed yet, and an approval cannot be bound to work
//! whose shape is only known once it has started. So a tool declares its input
//! and output as Rust types, the schemas are generated from those types, and the
//! registry refuses to publish anything it cannot validate. What the GUI runs,
//! what the CLI runs, and what an agent runs is then the same typed operation
//! under the same gates.
//!
//! # The invocation pipeline
//!
//! [`invoke`] resolves a tool and runs six steps in a fixed order:
//!
//! 1. **Validate the input** against the published JSON Schema.
//! 2. **Deserialize** it into the tool's own `Input` type.
//! 3. **Execute** the tool body, inside a panic boundary.
//! 4. **Serialize** the typed `Output`.
//! 5. **Validate the output** against the published JSON Schema.
//! 6. **Return** it, together with the `(id, version)` that ran.
//!
//! The order carries the guarantees. Validation precedes execution, so a
//! rejected input means the body provably never ran and a caller may retry a
//! correction without wondering about side effects —
//! [`ToolError::happened_before_execution`] states that in the type. Validation
//! also precedes policy evaluation, because policy must classify what will
//! actually execute rather than an unparsed blob. And validation of the *output*
//! precedes delivery, so a consumer that trusted the published schema never
//! receives a shape it cannot handle; a tool that emits the wrong thing produces
//! a structured [`ToolError::InvalidOutput`] instead of a downstream crash.
//!
//! Both gates locate their findings. A [`SchemaViolation`] carries an RFC 6901
//! JSON Pointer into the offending value and another into the schema rule it
//! broke, which is what makes a refusal actionable for an agent retrying on its
//! own.
//!
//! # Risk and capabilities
//!
//! [`RiskLevel`] is the single definition of what executing a tool can affect,
//! ordered `Observe < WorkspaceWrite < Execute < Network < RemoteWrite <
//! Destructive`. The ordering lives in the type because policy compares against
//! it, and a comparison that means different things in different modules is not
//! a policy. [`Capability`] names what a tool needs granted. Both are declared
//! once and frozen in the descriptor: a tool cannot lower its declared risk for
//! a particular call. Whether a *specific* invocation is more dangerous than its
//! level suggests — a path leaving the workspace, a remote that is not the
//! project's — is decided when the invocation is evaluated, not by relabelling
//! the tool.
//!
//! # Panic containment
//!
//! The tool body is the only foreign code in the pipeline, and it runs under
//! [`catch_unwind`](std::panic::catch_unwind). A panic becomes
//! [`ToolError::ToolPanicked`], carrying the payload text when it was a string;
//! the registry and the calling thread stay usable, so one buggy tool cannot tear
//! down the coordinator and orphan a run record. This depends on the workspace
//! unwinding rather than aborting on panic, which is the default profile
//! behaviour and is not overridden.
//!
//! Two limits are worth stating. A panic leaves the [`ExecutionContext`] in
//! whatever state the body abandoned it in, so a contained panic ends that call
//! rather than resuming it. And an abort — `panic = "abort"`, a failed
//! allocation, a `std::process::exit` — is not a panic and is not containable
//! here.
//!
//! # Schemas are generated, never declared
//!
//! [`Tool`] has no method returning a schema. Schemas are produced from the
//! `Input` and `Output` associated types by `schemars` at registration, so a
//! descriptor cannot publish a contract that disagrees with the type the body
//! deserializes. They are compiled into validators once, at the same moment, and
//! a schema that cannot be compiled is a registration failure rather than a
//! surprise on the first call.
//!
//! Nothing here retrieves a schema from outside the process: `jsonschema` is
//! built without its `resolve-http` and `resolve-file` features, so a `$ref` to
//! a URL or a local file cannot be followed.
//!
//! One thing is the tool author's responsibility. `schemars` closes an object
//! schema only when the type carries `#[serde(deny_unknown_fields)]`, so **every
//! `Input` type should carry it** — see [`Tool`]. Without it an agent's
//! misspelled field is discarded silently instead of being reported.
//!
//! # Registration and versions
//!
//! [`ToolRegistry`] keys on `(id, version)` and refuses a duplicate. There is no
//! way to replace or remove a registration, because a recorded tool call and an
//! approval both name a version and expect it to keep meaning what it meant;
//! publishing a change means registering a new version beside the old one.
//! Enumeration is ordered by identifier and then by version precedence, so
//! generated documentation and the `harkness contract` projection are
//! diff-stable regardless of registration order.
//!
//! Resolving without a version selects the highest by semantic-version
//! precedence — which is why a version is parsed rather than compared as text:
//! `0.10.0` follows `0.9.0`, and `1.0.0-alpha.1` does not outrank `1.0.0`.
//!
//! # What this module does not do
//!
//! It supplies metadata; it does not decide anything with it. There is no policy
//! evaluation, no approval flow, no timeout enforcement, no progress transport,
//! and no artifact storage here — [`ProgressSink`] and [`ArtifactWriter`] are
//! contracts whose implementations live elsewhere, and
//! [`ToolError::TimedOut`] and [`ToolError::Interrupted`] are kinds this module
//! defines for the layers that raise them. No concrete production tool is
//! registered here either; this module only defines the shape they take.

mod context;
mod descriptor;
mod erased;
mod error;
mod identifier;
mod registry;
mod schema;

#[cfg(test)]
mod tests;

pub use context::{
    ArtifactRef, ArtifactWriter, DiscardedProgress, ExecutionContext, ProgressEvent, ProgressSink,
    ProgressUnit, RecordedProgress, UnsupportedArtifacts,
};
pub use descriptor::{
    MAX_DESCRIPTION_LENGTH, MAX_TITLE_LENGTH, RiskLevel, ToolDescriptor, ToolMetadata,
    UnknownRiskLevel,
};
pub use erased::{ErasedTool, Tool, erase};
pub use error::{
    InvocationError, MAX_REPORTED_VIOLATIONS, RegistryError, SchemaDirection, SchemaViolation,
    ToolError,
};
pub use identifier::{Capability, MAX_IDENTIFIER_LENGTH, ToolId, ToolIdentity, ToolVersion};
pub use registry::{ToolOutcome, ToolRegistry, invoke, invoke_resolved};
