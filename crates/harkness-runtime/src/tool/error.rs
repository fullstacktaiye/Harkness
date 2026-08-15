use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::Failure;
use crate::trust::BoundaryError;

use super::ToolIdentity;

/// Violations reported for one rejected value before the list is truncated.
///
/// A schema violation report is a diagnostic for whoever produced the value —
/// often an agent retrying with a correction — and the first handful of
/// violations locate the mistake.
pub const MAX_REPORTED_VIOLATIONS: usize = 10;

/// Longest any single field of one violation retains.
///
/// Every field of a violation derives from caller-supplied data, not just the
/// explanation. A validator's explanation quotes the value it rejected, so a
/// 200 KB instance rejected at its root produces a 200 KB sentence. Less
/// obviously, the *pointer* is caller-controlled too: a JSON Pointer names the
/// keys it traverses, so an input type with a map-valued field lets the caller
/// pick the pointer's length by choosing a long key. Both are bounded, and for
/// the same reason.
///
/// That reason is not tidiness. [`ToolError::as_failure`] is how a refusal gets
/// recorded against the tool call, and the run store refuses an inline payload
/// over 64 KiB — an unbounded diagnostic would leave the call stuck in `running`
/// with nothing written about why it failed.
pub const MAX_VIOLATION_FIELD_BYTES: usize = 512;

/// Hard bound on the message [`ToolError::as_failure`] produces.
///
/// The per-field and per-list bounds keep a *schema* report small, but not every
/// failure is a schema report: a tool flattening a verbose `GitError` into
/// [`ToolError::ExecutionFailed`], or panicking with a payload that quotes its
/// own input, reaches the same durable path. Rather than bound each variant
/// separately and hope the next one remembers, the projection to a [`Failure`]
/// clamps whatever it is handed. This is the invariant the run store depends on,
/// so it is enforced where the record is actually made.
///
/// The value is chosen so the two bounds *compose*: a full report of
/// [`MAX_REPORTED_VIOLATIONS`] violations, each field at
/// [`MAX_VIOLATION_FIELD_BYTES`], comes to roughly 10.5 KB and so passes through
/// this clamp untouched. A backstop that routinely truncated legitimate reports
/// would be doing the per-field bound's job badly instead of its own.
/// `the_two_bounds_compose_so_a_full_report_survives_intact` holds that.
pub const MAX_FAILURE_MESSAGE_BYTES: usize = 16 * 1024;

/// Appended to text that was cut short.
const TRUNCATION_MARKER: &str = "… (truncated)";

/// Most further violations counted past the ones retained.
///
/// Counting the remainder exactly means walking the validator's whole lazy error
/// iterator, which constructs one error per violation. The input is
/// caller-supplied and unbounded, so an instance engineered to violate its schema
/// everywhere would do that work on the thread that is meant to be returning a
/// refusal. Past this many, the report says "at least" rather than paying to find
/// out — an exact count of ten thousand tells a caller nothing a bound does not.
pub const MAX_COUNTED_VIOLATIONS: usize = 1_000;

/// Which side of an invocation a schema describes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDirection {
    /// The value a caller supplies to a tool.
    Input,
    /// The value a tool returns to its caller.
    Output,
}

impl SchemaDirection {
    /// Every direction in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Input, Self::Output];

    /// Returns the stable spelling used in messages and published schemas.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

impl fmt::Display for SchemaDirection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One place a value failed to satisfy its declared schema.
///
/// Both pointers are RFC 6901 JSON Pointers: [`pointer`](Self::pointer) locates
/// the offending value inside the instance and
/// [`schema_pointer`](Self::schema_pointer) locates the rule it broke inside the
/// published schema. An empty pointer refers to the whole document, which is what
/// a wrong top-level type reports.
///
/// Every field is bounded to [`MAX_VIOLATION_FIELD_BYTES`]. The fields are
/// private and deserialization re-applies the bound, so there is no way — a
/// struct literal, a field assignment, or a value read back from JSON — to obtain
/// a violation whose text is unbounded. A truncated field ends in an ellipsis
/// marker, which also means a truncated pointer is visibly not a resolvable
/// pointer rather than one that silently addresses the wrong place.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "SchemaViolationWire")]
pub struct SchemaViolation {
    pointer: String,
    schema_pointer: String,
    message: String,
}

/// Deserialization target for [`SchemaViolation`], so a violation read from JSON
/// is bounded exactly as one built in process is.
///
/// `deny_unknown_fields` belongs here rather than on `SchemaViolation`: `from`
/// makes this type the whole of that one's `Deserialize`, so an attribute left on
/// the outer struct would look like a check while doing nothing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaViolationWire {
    pointer: String,
    schema_pointer: String,
    message: String,
}

impl From<SchemaViolationWire> for SchemaViolation {
    fn from(wire: SchemaViolationWire) -> Self {
        Self::new(wire.pointer, wire.schema_pointer, wire.message)
    }
}

impl SchemaViolation {
    /// Records a violation at the given instance and schema locations.
    ///
    /// Each field is truncated to [`MAX_VIOLATION_FIELD_BYTES`].
    #[must_use]
    pub fn new(
        pointer: impl Into<String>,
        schema_pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            pointer: truncate(pointer.into(), MAX_VIOLATION_FIELD_BYTES),
            schema_pointer: truncate(schema_pointer.into(), MAX_VIOLATION_FIELD_BYTES),
            message: truncate(message.into(), MAX_VIOLATION_FIELD_BYTES),
        }
    }

    /// JSON Pointer into the rejected value.
    #[must_use]
    pub fn pointer(&self) -> &str {
        &self.pointer
    }

    /// JSON Pointer into the schema rule that rejected it.
    #[must_use]
    pub fn schema_pointer(&self) -> &str {
        &self.schema_pointer
    }

    /// Human-readable explanation from the validator.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Bounds one string to `maximum` bytes, cutting on a character boundary.
///
/// The single truncation used by every bounded field in this module, so the cut
/// and its marker cannot drift between them.
pub(super) fn truncate(mut text: String, maximum: usize) -> String {
    if text.len() <= maximum {
        return text;
    }

    // `floor_char_boundary` is unstable, so walk back to one by hand; a cut
    // inside a multi-byte character would panic on `truncate`.
    let mut boundary = maximum;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(TRUNCATION_MARKER);
    text
}

impl fmt::Display for SchemaViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pointer.is_empty() {
            write!(formatter, "the value itself: {}", self.message)
        } else {
            write!(formatter, "{}: {}", self.pointer, self.message)
        }
    }
}

/// Failures a tool invocation can report.
///
/// Every variant carries a stable [`kind`](ToolError::kind) discriminant, the
/// same convention `GitError`, `RunDomainError`, and
/// [`StoreError`](crate::store::StoreError) follow, so a front end, the policy
/// engine, and a persisted [`Failure`] all branch on one spelling instead of on
/// Rust types.
///
/// The namespace deliberately separates *why the value was wrong*
/// ([`Self::InvalidInput`], [`Self::InvalidOutput`]) from *why the work did not
/// happen* ([`Self::Denied`], [`Self::Cancelled`], [`Self::Interrupted`],
/// [`Self::TimedOut`]) from *the tool itself misbehaving*
/// ([`Self::ExecutionFailed`], [`Self::ToolPanicked`]). A caller deciding
/// whether to retry, to re-prompt, or to stop needs those three apart.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ToolError {
    /// The supplied input does not satisfy the tool's declared input schema.
    ///
    /// Raised before the tool body runs, so nothing was executed.
    #[error("{tool} input does not satisfy its declared schema: {}", render_violations(.violations, *.omitted))]
    InvalidInput {
        /// Tool whose schema refused the value.
        tool: ToolIdentity,
        /// Where the value broke the schema, at most
        /// [`MAX_REPORTED_VIOLATIONS`] of them.
        violations: Vec<SchemaViolation>,
        /// Further violations found but not retained.
        omitted: usize,
    },

    /// The tool returned a value that does not satisfy its declared output
    /// schema.
    ///
    /// The result is discarded rather than delivered: a consumer that trusted
    /// the published schema would otherwise receive a shape it cannot handle.
    #[error("{tool} output does not satisfy its declared schema: {}", render_violations(.violations, *.omitted))]
    InvalidOutput {
        /// Tool whose own output was refused.
        tool: ToolIdentity,
        /// Where the value broke the schema, at most
        /// [`MAX_REPORTED_VIOLATIONS`] of them.
        violations: Vec<SchemaViolation>,
        /// Further violations found but not retained.
        omitted: usize,
    },

    /// The tool ran and reported failure.
    #[error("the tool failed: {message}")]
    ExecutionFailed {
        /// Explanation from the tool.
        message: String,
    },

    /// A child process the tool supervised ended unsuccessfully.
    ///
    /// Distinguished from [`Self::ExecutionFailed`] because the two call for
    /// different handling: a tool's own failure is a statement about the work,
    /// while an exit status is a statement about a program whose whole
    /// diagnostic lives on a stream this process captured. Keeping the status
    /// typed means a caller can branch on it without parsing prose.
    #[error("{}", render_exit(*.code, .stderr_tail))]
    ProcessFailed {
        /// Status the child reported, or `None` when a signal ended it.
        code: Option<i32>,
        /// End of what the child wrote to standard error.
        ///
        /// The tail rather than the whole, because a program's diagnosis comes
        /// last and its progress reporting comes in unbounded quantity. The full
        /// stream is in the artifact the call recorded.
        stderr_tail: String,
    },

    /// The tool exceeded the time it was allowed.
    ///
    /// Rendered with `Duration`'s own formatting rather than in whole seconds,
    /// so a sub-second limit does not report itself as `0`.
    #[error("the tool exceeded its {limit:?} time limit")]
    TimedOut {
        /// Limit the invocation was given.
        limit: Duration,
    },

    /// The invocation was cancelled through its cancellation token.
    #[error("the tool invocation was cancelled")]
    Cancelled,

    /// Policy or a human decision refused the invocation.
    #[error("the tool invocation was denied: {reason}")]
    Denied {
        /// Stable explanation of the refusal.
        reason: String,
    },

    /// A path argument resolved outside the workspace the tool may touch.
    #[error("{} is outside the workspace: {reason}", .path.display())]
    ForbiddenPath {
        /// Path that was refused, as the caller supplied it.
        path: PathBuf,
        /// Stable explanation of the refusal.
        reason: String,
    },

    /// A requested workspace entry does not exist.
    #[error("{} was not found", .path.display())]
    NotFound {
        /// Path the caller requested.
        path: PathBuf,
    },

    /// A result could not be represented within its required inline budget.
    #[error("the tool output exceeded its {limit}-byte inline budget")]
    OutputBudgetExhausted {
        /// Inline byte limit that was enforced.
        limit: u64,
    },

    /// A canonical filesystem boundary refused a path or root.
    #[error(transparent)]
    Boundary(#[from] BoundaryError),

    /// A patch was approved against bytes that are no longer present.
    #[error(
        "patch base for {} is stale: expected {expected}, found {actual}",
        .path.display()
    )]
    StalePatch {
        /// Workspace-relative path whose precondition failed.
        path: PathBuf,
        /// Approved SHA-256, or `new file` for an expected absence.
        expected: String,
        /// SHA-256 observed at execution, or `missing` when absent.
        actual: String,
    },

    /// A unified diff is malformed or does not apply cleanly to its base.
    #[error("patch for {} conflicts with the workspace: {reason}", .path.display())]
    PatchConflict {
        /// Target path, or `<patch>` when the patch as a whole is malformed.
        path: PathBuf,
        /// Bounded explanation of the parse or hunk mismatch.
        reason: String,
    },

    /// The owning process stopped before the invocation completed.
    #[error("the tool invocation was interrupted before it completed")]
    Interrupted,

    /// The tool panicked and the panic was contained at the erasure boundary.
    #[error("{tool} panicked{}", render_payload(.payload))]
    ToolPanicked {
        /// Tool that panicked.
        tool: ToolIdentity,
        /// Panic payload, when it was a string this build could recover.
        payload: Option<String>,
    },
}

impl ToolError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_input",
        "invalid_output",
        "execution_failed",
        "process_failed",
        "timed_out",
        "cancelled",
        "denied",
        "forbidden_path",
        "not_found",
        "output_budget_exhausted",
        "outside_allowed_roots",
        "symlink_escapes",
        "root_unavailable",
        "candidate_unavailable",
        "stale_patch",
        "patch_conflict",
        "interrupted",
        "tool_panicked",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidInput { .. } => "invalid_input",
            Self::InvalidOutput { .. } => "invalid_output",
            Self::ExecutionFailed { .. } => "execution_failed",
            Self::ProcessFailed { .. } => "process_failed",
            Self::TimedOut { .. } => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Denied { .. } => "denied",
            Self::ForbiddenPath { .. } => "forbidden_path",
            Self::NotFound { .. } => "not_found",
            Self::OutputBudgetExhausted { .. } => "output_budget_exhausted",
            Self::Boundary(error) => error.kind(),
            Self::StalePatch { .. } => "stale_patch",
            Self::PatchConflict { .. } => "patch_conflict",
            Self::Interrupted => "interrupted",
            Self::ToolPanicked { .. } => "tool_panicked",
        }
    }

    /// Reports a tool-authored failure, borrowing any [`Display`](fmt::Display)
    /// cause for its message.
    ///
    /// The cause is flattened into text rather than retained as a source chain
    /// because this error is designed to become a durable [`Failure`], and a
    /// stored record cannot hold a live error object.
    ///
    /// The flattened text is clamped to [`MAX_FAILURE_MESSAGE_BYTES`]. A cause
    /// worth reporting is often verbose — `GitError::Failed` interpolates the
    /// whole captured stderr, and a rejected push or a chatty hook can make that
    /// arbitrarily long — and a failure nobody can record is worse than a failure
    /// described in less detail.
    #[must_use]
    pub fn execution_failed(cause: impl fmt::Display) -> Self {
        Self::ExecutionFailed {
            message: truncate(cause.to_string(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }

    /// Reports a policy or human refusal.
    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    /// Whether the invocation pipeline guarantees the tool body never started.
    ///
    /// Only [`Self::InvalidInput`] qualifies, and only because the erasure
    /// boundary raises it before it calls the body at all. That is what lets a
    /// caller retry a corrected input without wondering whether the first attempt
    /// had a side effect.
    ///
    /// Every other kind is deliberately excluded, because whether the body ran
    /// depends on who raised it and this type cannot tell. [`Self::Denied`] is
    /// pre-execution when the policy engine raises it and mid-body when a tool
    /// does. [`Self::ForbiddenPath`] is pre-execution from
    /// [`ExecutionContext::new`](super::ExecutionContext::new) but mid-body from
    /// [`ExecutionContext::resolve`](super::ExecutionContext::resolve), which
    /// tools are told to route every path argument through — a tool that wrote
    /// one file and then refused a second path has already had its side effect.
    /// Answering `true` there would licence exactly the double-apply this method
    /// exists to prevent.
    #[must_use]
    pub const fn happened_before_execution(&self) -> bool {
        matches!(self, Self::InvalidInput { .. })
    }

    /// Projects this failure into the durable form a tool call records.
    ///
    /// The kind is the stable discriminant and the message is this error's
    /// [`Display`](fmt::Display), clamped to [`MAX_FAILURE_MESSAGE_BYTES`].
    ///
    /// The clamp is here rather than on each variant because this is the single
    /// point at which a `ToolError` becomes something the run store has to accept.
    /// Bounding the variants alone would leave the invariant one new variant away
    /// from being broken again, and the consequence of breaking it is not a long
    /// message — it is a `payload_too_large` refusal that leaves the tool call in
    /// `running` with no record of why it failed.
    #[must_use]
    pub fn as_failure(&self) -> Failure {
        Failure::new(
            self.kind(),
            truncate(self.to_string(), MAX_FAILURE_MESSAGE_BYTES),
        )
    }
}

/// Failures raised while declaring a tool or resolving one from the registry.
///
/// Registration failures and resolution failures share one namespace because
/// they share one cause: a mismatch between the tool a caller names and the
/// tools that exist. Keeping them apart from [`ToolError`] preserves the
/// property that a `ToolError` always describes an invocation that was actually
/// attempted against a real tool.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RegistryError {
    /// A tool identifier is outside the accepted grammar.
    #[error("{value:?} is not a valid tool id: {reason}")]
    InvalidToolId {
        /// Value that was rejected.
        value: String,
        /// Stable explanation of the rejection.
        reason: &'static str,
    },

    /// A tool version is not a complete semantic version.
    #[error("{value:?} is not a valid tool version: {reason}")]
    InvalidToolVersion {
        /// Value that was rejected.
        value: String,
        /// Explanation from the semantic-version parser.
        reason: String,
    },

    /// A capability name is outside the accepted grammar.
    #[error("{value:?} is not a valid capability: {reason}")]
    InvalidCapability {
        /// Value that was rejected.
        value: String,
        /// Stable explanation of the rejection.
        reason: &'static str,
    },

    /// A descriptor field a front end has to render is unusable.
    #[error("{tool} cannot declare {field}: {reason}")]
    InvalidMetadata {
        /// Tool being declared.
        tool: ToolIdentity,
        /// Field that violated its bound.
        field: &'static str,
        /// Stable explanation of the rejection.
        reason: &'static str,
    },

    /// A generated schema could not be compiled into a validator.
    #[error("the {direction} schema of {tool} cannot be compiled: {reason}")]
    InvalidSchema {
        /// Tool being declared.
        tool: ToolIdentity,
        /// Side of the invocation the schema describes.
        direction: SchemaDirection,
        /// Explanation from the schema compiler.
        reason: String,
    },

    /// A second tool claimed an already-registered `(id, version)`.
    #[error("{tool} is already registered; a released version is immutable")]
    DuplicateRegistration {
        /// Identity that is already taken.
        tool: ToolIdentity,
    },

    /// No tool is registered under the requested identifier.
    #[error("no tool is registered as {id}")]
    UnknownTool {
        /// Identifier that matched nothing.
        id: String,
    },

    /// The identifier is registered, but not at the requested version.
    #[error("{id} is registered, but not at version {version}; available: {}", available.join(", "))]
    UnknownToolVersion {
        /// Identifier that exists.
        id: String,
        /// Version that does not.
        version: String,
        /// Versions that do exist, in precedence order.
        available: Vec<String>,
    },
}

impl RegistryError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_tool_id",
        "invalid_tool_version",
        "invalid_capability",
        "invalid_metadata",
        "invalid_schema",
        "duplicate_registration",
        "unknown_tool",
        "unknown_tool_version",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidToolId { .. } => "invalid_tool_id",
            Self::InvalidToolVersion { .. } => "invalid_tool_version",
            Self::InvalidCapability { .. } => "invalid_capability",
            Self::InvalidMetadata { .. } => "invalid_metadata",
            Self::InvalidSchema { .. } => "invalid_schema",
            Self::DuplicateRegistration { .. } => "duplicate_registration",
            Self::UnknownTool { .. } => "unknown_tool",
            Self::UnknownToolVersion { .. } => "unknown_tool_version",
        }
    }
}

/// Everything one call to [`invoke`](super::invoke) can report.
///
/// Resolution and execution are separate namespaces because they have separate
/// audiences: a resolution failure is a bug in the caller or a stale client,
/// while a tool failure is an outcome worth recording against the call. This
/// type is the union a single entry point has to return, and
/// [`kinds`](Self::kinds) is the flattened list `harkness contract` publishes.
///
/// A tool failure carries the resolved [`ToolIdentity`] beside the error. That
/// matters for the same reason [`ToolOutcome`](super::ToolOutcome) carries it on
/// success: a caller that asked for a tool without naming a version still has to
/// write `tool_calls.tool_version` for the row it is failing, and re-resolving to
/// find out is a second lookup that can disagree with the first. Only some
/// [`ToolError`] variants name the tool themselves — a tool reporting
/// `execution_failed` does not know its own identity — so attaching it here is
/// what makes the version available on *every* failure path rather than most of
/// them.
///
/// There is deliberately no `From<ToolError>` conversion. Building this variant
/// requires naming the tool, so a `?` cannot produce a tool failure that forgot
/// to say which tool failed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InvocationError {
    /// The named tool or version does not exist in the registry.
    ///
    /// No identity accompanies this variant because resolving one is precisely
    /// what failed.
    #[error(transparent)]
    Resolution(#[from] RegistryError),

    /// The invocation reached a real tool and failed.
    #[error("{error}")]
    Tool {
        /// The `(id, version)` that was resolved and attempted.
        tool: ToolIdentity,
        /// What the invocation reported.
        ///
        /// Boxed because this variant carries both an identity and a whole
        /// [`ToolError`] — itself the widest type in this module — and every
        /// `Result<_, InvocationError>` in the runtime would otherwise be as wide
        /// as the rarest failure in it.
        #[source]
        error: Box<ToolError>,
    },
}

impl InvocationError {
    /// Reports a failure against the tool that was resolved and attempted.
    #[must_use]
    pub fn from_tool(tool: ToolIdentity, error: ToolError) -> Self {
        Self::Tool {
            tool,
            error: Box::new(error),
        }
    }

    /// Stable machine-readable discriminant, delegated to the namespace that
    /// owns the failure.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Resolution(error) => error.kind(),
            Self::Tool { error, .. } => error.kind(),
        }
    }

    /// The tool that was attempted, when one was resolved.
    ///
    /// `None` only for [`Self::Resolution`], where no tool was found to attempt.
    /// A caller recording a failed call reads the version to persist from here.
    #[must_use]
    pub const fn tool(&self) -> Option<&ToolIdentity> {
        match self {
            Self::Resolution(_) => None,
            Self::Tool { tool, .. } => Some(tool),
        }
    }

    /// The invocation failure, when the call reached a real tool.
    #[must_use]
    pub fn tool_error(&self) -> Option<&ToolError> {
        match self {
            Self::Resolution(_) => None,
            Self::Tool { error, .. } => Some(error),
        }
    }

    /// Every kind an invocation can report: the registry namespace followed by
    /// the tool namespace.
    ///
    /// Returned as an owned list rather than a const because it is the
    /// concatenation of two independently maintained tables, and duplicating
    /// their entries here is exactly the drift the tables exist to prevent.
    #[must_use]
    pub fn kinds() -> Vec<&'static str> {
        RegistryError::KINDS
            .iter()
            .chain(ToolError::KINDS)
            .copied()
            .collect()
    }

    /// Projects this failure into the durable form a tool call records.
    ///
    /// Clamped to [`MAX_FAILURE_MESSAGE_BYTES`] on both paths. A resolution
    /// message is built from an identifier a caller supplied, so it is bounded for
    /// the same reason everything else on this path is.
    #[must_use]
    pub fn as_failure(&self) -> Failure {
        match self {
            Self::Resolution(error) => Failure::new(
                error.kind(),
                truncate(error.to_string(), MAX_FAILURE_MESSAGE_BYTES),
            ),
            Self::Tool { error, .. } => error.as_failure(),
        }
    }
}

/// Renders a violation list, naming how many further violations were omitted.
///
/// `omitted` is supplied by the gate that found them rather than derived from
/// the list's own length, because the list has already been truncated by the time
/// it gets here — deriving it could only ever report the difference between the
/// list and the bound, understating a large report.
fn render_violations(violations: &[SchemaViolation], omitted: usize) -> String {
    if violations.is_empty() {
        return "no violation was reported".to_owned();
    }

    let mut rendered = violations
        .iter()
        .take(MAX_REPORTED_VIOLATIONS)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    match omitted {
        0 => {}
        // The count stops at the cap, so at the cap it is a lower bound and must
        // not be reported as if it were the total.
        counted if counted >= MAX_COUNTED_VIOLATIONS => {
            rendered.push_str(&format!(" (and at least {counted} more)"));
        }
        counted => rendered.push_str(&format!(" (and {counted} more)")),
    }
    rendered
}

/// Renders a child's exit status and whatever it said on the way out.
///
/// A signalled child reports no code at all, and saying "exited with status
/// None" would read as a bug in Harkness rather than as what it is — which
/// matters here, because the executor kills a timed-out child itself.
fn render_exit(code: Option<i32>, stderr_tail: &str) -> String {
    let status = match code {
        Some(code) => format!("a child process exited with status {code}"),
        None => "a child process was ended by a signal".to_owned(),
    };
    match stderr_tail.trim() {
        "" => status,
        reported => format!("{status}: {reported}"),
    }
}

/// Renders a recovered panic payload, or nothing when the payload was not a
/// string this build could read.
fn render_payload(payload: &Option<String>) -> String {
    match payload {
        Some(payload) => format!(": {payload}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        InvocationError, MAX_FAILURE_MESSAGE_BYTES, MAX_REPORTED_VIOLATIONS,
        MAX_VIOLATION_FIELD_BYTES, RegistryError, SchemaDirection, SchemaViolation,
        TRUNCATION_MARKER, ToolError,
    };
    use crate::tool::ToolIdentity;
    use crate::trust::BoundaryError;

    fn identity() -> ToolIdentity {
        ToolIdentity::parse("fixture.tool", "1.0.0").unwrap()
    }

    fn violation() -> SchemaViolation {
        SchemaViolation::new(
            "/depth",
            "/properties/depth/type",
            "\"deep\" is not of type integer",
        )
    }

    #[test]
    fn tool_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                ToolError::InvalidInput {
                    tool: identity(),
                    violations: vec![violation()],
                    omitted: 0,
                },
                "invalid_input",
            ),
            (
                ToolError::InvalidOutput {
                    tool: identity(),
                    violations: vec![violation()],
                    omitted: 0,
                },
                "invalid_output",
            ),
            (
                ToolError::ExecutionFailed {
                    message: "fixture".to_owned(),
                },
                "execution_failed",
            ),
            (
                ToolError::ProcessFailed {
                    code: Some(1),
                    stderr_tail: "fixture".to_owned(),
                },
                "process_failed",
            ),
            (
                ToolError::TimedOut {
                    limit: Duration::from_secs(30),
                },
                "timed_out",
            ),
            (ToolError::Cancelled, "cancelled"),
            (
                ToolError::Denied {
                    reason: "fixture".to_owned(),
                },
                "denied",
            ),
            (
                ToolError::ForbiddenPath {
                    path: PathBuf::from("../etc/passwd"),
                    reason: "fixture".to_owned(),
                },
                "forbidden_path",
            ),
            (
                ToolError::NotFound {
                    path: PathBuf::from("missing"),
                },
                "not_found",
            ),
            (
                ToolError::OutputBudgetExhausted { limit: 1024 },
                "output_budget_exhausted",
            ),
            (
                ToolError::Boundary(BoundaryError::OutsideAllowedRoots {
                    candidate: PathBuf::from("outside"),
                    roots: vec![PathBuf::from("/workspace")],
                }),
                "outside_allowed_roots",
            ),
            (
                ToolError::Boundary(BoundaryError::SymlinkEscapes {
                    link: PathBuf::from("link"),
                    target: PathBuf::from("target"),
                }),
                "symlink_escapes",
            ),
            (
                ToolError::Boundary(BoundaryError::RootUnavailable {
                    root: PathBuf::from("root"),
                    reason: "fixture".to_owned(),
                }),
                "root_unavailable",
            ),
            (
                ToolError::Boundary(BoundaryError::CandidateUnavailable {
                    candidate: PathBuf::from("candidate"),
                    reason: "fixture".to_owned(),
                }),
                "candidate_unavailable",
            ),
            (
                ToolError::StalePatch {
                    path: PathBuf::from("src/lib.rs"),
                    expected: "expected".to_owned(),
                    actual: "actual".to_owned(),
                },
                "stale_patch",
            ),
            (
                ToolError::PatchConflict {
                    path: PathBuf::from("src/lib.rs"),
                    reason: "fixture".to_owned(),
                },
                "patch_conflict",
            ),
            (ToolError::Interrupted, "interrupted"),
            (
                ToolError::ToolPanicked {
                    tool: identity(),
                    payload: Some("fixture".to_owned()),
                },
                "tool_panicked",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ToolError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn registry_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                RegistryError::InvalidToolId {
                    value: "Fixture".to_owned(),
                    reason: "fixture",
                },
                "invalid_tool_id",
            ),
            (
                RegistryError::InvalidToolVersion {
                    value: "1.0".to_owned(),
                    reason: "fixture".to_owned(),
                },
                "invalid_tool_version",
            ),
            (
                RegistryError::InvalidCapability {
                    value: "Fixture".to_owned(),
                    reason: "fixture",
                },
                "invalid_capability",
            ),
            (
                RegistryError::InvalidMetadata {
                    tool: identity(),
                    field: "title",
                    reason: "fixture",
                },
                "invalid_metadata",
            ),
            (
                RegistryError::InvalidSchema {
                    tool: identity(),
                    direction: SchemaDirection::Input,
                    reason: "fixture".to_owned(),
                },
                "invalid_schema",
            ),
            (
                RegistryError::DuplicateRegistration { tool: identity() },
                "duplicate_registration",
            ),
            (
                RegistryError::UnknownTool {
                    id: "fixture.tool".to_owned(),
                },
                "unknown_tool",
            ),
            (
                RegistryError::UnknownToolVersion {
                    id: "fixture.tool".to_owned(),
                    version: "9.0.0".to_owned(),
                    available: vec!["1.0.0".to_owned()],
                },
                "unknown_tool_version",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, RegistryError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn invocation_error_kinds_are_the_two_namespaces_without_collision() {
        let kinds = InvocationError::kinds();
        assert_eq!(
            kinds.len(),
            RegistryError::KINDS.len() + ToolError::KINDS.len()
        );
        assert_eq!(&kinds[..RegistryError::KINDS.len()], RegistryError::KINDS);
        assert_eq!(&kinds[RegistryError::KINDS.len()..], ToolError::KINDS);

        let mut sorted = kinds.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), kinds.len(), "two namespaces share a kind");
    }

    #[test]
    fn invocation_error_delegates_its_kind_to_the_owning_namespace() {
        let resolution = InvocationError::from(RegistryError::UnknownTool {
            id: "fixture.tool".to_owned(),
        });
        assert_eq!(resolution.kind(), "unknown_tool");
        let execution = InvocationError::from_tool(identity(), ToolError::Cancelled);
        assert_eq!(execution.kind(), "cancelled");
        assert_eq!(execution.as_failure().kind(), "cancelled");
    }

    #[test]
    fn a_tool_failure_names_the_tool_and_a_resolution_failure_cannot() {
        // Every failure that reached a tool carries the resolved identity, so a
        // caller recording the failed row reads the version from the error itself
        // instead of resolving a second time — a second lookup can disagree with
        // the first.
        let attempted = InvocationError::from_tool(
            identity(),
            ToolError::execution_failed("the remote refused"),
        );
        assert_eq!(attempted.tool(), Some(&identity()));
        assert_eq!(
            attempted.tool().map(|tool| tool.version.to_string()),
            Some("1.0.0".to_owned())
        );
        assert_eq!(
            attempted.tool_error(),
            Some(&ToolError::execution_failed("the remote refused"))
        );
        // The identity does not leak into the message; the wrapper is transparent.
        assert_eq!(attempted.to_string(), "the tool failed: the remote refused");
        assert!(
            std::error::Error::source(&attempted).is_some(),
            "the tool error must stay reachable as a source"
        );

        // Resolution is the one case with no identity, because finding one is
        // exactly what failed.
        let unresolved = InvocationError::from(RegistryError::UnknownTool {
            id: "fixture.absent".to_owned(),
        });
        assert_eq!(unresolved.tool(), None);
        assert_eq!(unresolved.tool_error(), None);
    }

    #[test]
    fn a_violation_message_names_the_pointer_and_reports_omitted_violations() {
        let single = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![violation()],
            omitted: 0,
        };
        assert_eq!(
            single.to_string(),
            "fixture.tool@1.0.0 input does not satisfy its declared schema: \
             /depth: \"deep\" is not of type integer"
        );

        // The omitted count comes from the gate that found them, so a large
        // report states its real remainder rather than the difference between the
        // truncated list and the bound.
        let reported = (0..MAX_REPORTED_VIOLATIONS)
            .map(|index| SchemaViolation::new(format!("/field{index}"), "/properties", "wrong"))
            .collect::<Vec<_>>();
        let truncated = ToolError::InvalidInput {
            tool: identity(),
            violations: reported,
            omitted: 40,
        }
        .to_string();
        assert!(truncated.contains("(and 40 more)"), "{truncated}");
        assert!(truncated.contains("/field9"), "{truncated}");
        assert!(!truncated.contains("/field10"), "{truncated}");
    }

    #[test]
    fn a_root_violation_reads_as_the_value_itself() {
        let error = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![SchemaViolation::new(
                "",
                "/type",
                "42 is not of type object",
            )],
            omitted: 0,
        };
        assert!(
            error
                .to_string()
                .ends_with("the value itself: 42 is not of type object"),
            "{error}"
        );
    }

    #[test]
    fn a_violation_explanation_is_bounded_so_the_failure_can_be_recorded() {
        // A validator quotes the value it rejected, and the value is
        // caller-supplied. Left whole, one violation over a 200 KB instance would
        // exceed the run store's 64 KiB inline bound and the refusal could not be
        // written against the call at all.
        let enormous = format!("{:?} is not of type object", "x".repeat(200 * 1024));
        let violation = SchemaViolation::new("", "/type", enormous);
        assert!(
            violation.message().len() <= MAX_VIOLATION_FIELD_BYTES + TRUNCATION_MARKER.len(),
            "one explanation is {} bytes",
            violation.message().len()
        );
        assert!(violation.message().ends_with(TRUNCATION_MARKER));

        let full = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![violation; MAX_REPORTED_VIOLATIONS],
            omitted: 3,
        };
        assert!(
            full.as_failure().message().len() < crate::store::MAX_INLINE_PAYLOAD_BYTES,
            "a full report is {} bytes, which the run store would refuse",
            full.as_failure().message().len()
        );
    }

    #[test]
    fn the_two_bounds_compose_so_a_full_report_survives_intact() {
        // The outer clamp is a backstop for unbounded sources, not a second
        // trimmer of schema reports. A worst-case full report — every violation at
        // the field bound in both pointer and message — has to pass through it
        // untouched, or the per-field bound is not the thing deciding how much
        // detail an agent gets back.
        let long_pointer = format!("/{}", "p".repeat(MAX_VIOLATION_FIELD_BYTES));
        let long_message = "m".repeat(MAX_VIOLATION_FIELD_BYTES);
        let violation = SchemaViolation::new(long_pointer, "s".repeat(64), long_message);
        let report = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![violation; MAX_REPORTED_VIOLATIONS],
            omitted: 40,
        };

        let rendered = report.to_string();
        let recorded = report.as_failure();
        assert_eq!(
            recorded.message().len(),
            rendered.len(),
            "the clamp trimmed a full report of {} bytes",
            rendered.len()
        );
        assert!(!recorded.message().ends_with(TRUNCATION_MARKER));
        assert!(
            recorded.message().contains("(and 40 more)"),
            "the omission count must survive to the end of the message"
        );
        assert!(recorded.message().len() < crate::store::MAX_INLINE_PAYLOAD_BYTES);
    }

    #[test]
    fn a_pointer_is_bounded_because_a_map_key_is_caller_chosen() {
        // The explanation is the obvious unbounded field; the pointer is the one
        // that is easy to miss. A JSON Pointer names the keys it traverses, so an
        // input type with a map-valued field lets the caller decide the pointer's
        // length by choosing a long key.
        let key = "k".repeat(100 * 1024);
        let violation = SchemaViolation::new(format!("/labels/{key}"), "/properties", "wrong");
        for field in [
            violation.pointer(),
            violation.schema_pointer(),
            violation.message(),
        ] {
            assert!(
                field.len() <= MAX_VIOLATION_FIELD_BYTES + TRUNCATION_MARKER.len(),
                "a field is {} bytes",
                field.len()
            );
        }
        assert!(violation.pointer().ends_with(TRUNCATION_MARKER));

        let report = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![violation; MAX_REPORTED_VIOLATIONS],
            omitted: 0,
        };
        assert!(
            report.as_failure().message().len() < crate::store::MAX_INLINE_PAYLOAD_BYTES,
            "a report of long pointers is {} bytes",
            report.as_failure().message().len()
        );
    }

    #[test]
    fn the_bound_cannot_be_bypassed_by_deserializing_a_violation() {
        // The fields are private, so a struct literal cannot smuggle a long value
        // in. Deserialization is the other door, and it routes through the same
        // constructor.
        let json = serde_json::json!({
            "pointer": "/".to_owned() + &"p".repeat(100 * 1024),
            "schema_pointer": "s".repeat(100 * 1024),
            "message": "m".repeat(100 * 1024),
        });
        let violation = serde_json::from_value::<SchemaViolation>(json).unwrap();
        for field in [
            violation.pointer(),
            violation.schema_pointer(),
            violation.message(),
        ] {
            assert!(
                field.len() <= MAX_VIOLATION_FIELD_BYTES + TRUNCATION_MARKER.len(),
                "a deserialized field is {} bytes",
                field.len()
            );
            assert!(field.ends_with(TRUNCATION_MARKER));
        }

        // Routing through a wire type must not have cost the strictness the outer
        // struct used to declare.
        let extra = serde_json::json!({
            "pointer": "/a",
            "schema_pointer": "/type",
            "message": "wrong",
            "surprise": true,
        });
        let error = serde_json::from_value::<SchemaViolation>(extra).unwrap_err();
        assert!(
            error.to_string().contains("unknown field"),
            "an unknown field should still be refused: {error}"
        );

        // And the round trip is stable, so the serialized form still matches what
        // deserialization accepts.
        let original = SchemaViolation::new("/a", "/type", "wrong");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(
            serde_json::from_str::<SchemaViolation>(&json).unwrap(),
            original
        );
    }

    #[test]
    fn every_failure_projection_is_bounded_not_only_the_schema_reports() {
        // A tool flattening a verbose cause, or panicking with a payload that
        // quotes its input, reaches the same durable path as a schema report. The
        // clamp lives on the projection so a variant added later cannot escape it.
        let verbose = "stderr line\n".repeat(20 * 1024);
        let cases = [
            ToolError::execution_failed(&verbose),
            ToolError::ExecutionFailed {
                message: verbose.clone(),
            },
            ToolError::ToolPanicked {
                tool: identity(),
                payload: Some(verbose.clone()),
            },
            ToolError::Denied {
                reason: verbose.clone(),
            },
            ToolError::ProcessFailed {
                code: Some(1),
                stderr_tail: verbose.clone(),
            },
        ];

        for error in cases {
            let failure = error.as_failure();
            assert!(
                failure.message().len() <= MAX_FAILURE_MESSAGE_BYTES + TRUNCATION_MARKER.len(),
                "{} projected {} bytes",
                error.kind(),
                failure.message().len()
            );
            assert!(
                failure.message().len() < crate::store::MAX_INLINE_PAYLOAD_BYTES,
                "{} would be refused by the run store",
                error.kind()
            );
        }
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary() {
        // A cut inside a multi-byte character would panic, so the boundary walk
        // matters for any value containing non-ASCII text.
        let multibyte = "é".repeat(MAX_VIOLATION_FIELD_BYTES);
        let violation = SchemaViolation::new("/name", "/type", multibyte);
        assert!(violation.message.ends_with(TRUNCATION_MARKER));
        assert!(
            violation.message.len() <= MAX_VIOLATION_FIELD_BYTES + TRUNCATION_MARKER.len(),
            "{}",
            violation.message.len()
        );
    }

    #[test]
    fn only_a_refused_input_promises_the_tool_body_never_ran() {
        assert!(
            ToolError::InvalidInput {
                tool: identity(),
                violations: vec![violation()],
                omitted: 0,
            }
            .happened_before_execution()
        );

        // Everything else is excluded because whether the body ran depends on who
        // raised it. `ForbiddenPath` in particular comes from
        // `ExecutionContext::resolve`, which tools call mid-body, so claiming it
        // is pre-execution would licence a retry that double-applies an earlier
        // write.
        for ran in [
            ToolError::denied("policy"),
            ToolError::ProcessFailed {
                code: Some(2),
                stderr_tail: "boom".to_owned(),
            },
            ToolError::ForbiddenPath {
                path: PathBuf::from(".."),
                reason: "fixture".to_owned(),
            },
            ToolError::Boundary(BoundaryError::CandidateUnavailable {
                candidate: PathBuf::from("loop"),
                reason: "fixture".to_owned(),
            }),
            ToolError::Cancelled,
            ToolError::Interrupted,
            ToolError::execution_failed("boom"),
            ToolError::TimedOut {
                limit: Duration::from_secs(1),
            },
            ToolError::InvalidOutput {
                tool: identity(),
                violations: vec![violation()],
                omitted: 0,
            },
            ToolError::ToolPanicked {
                tool: identity(),
                payload: None,
            },
        ] {
            assert!(
                !ran.happened_before_execution(),
                "{ran:?} claims the body never ran"
            );
        }
    }

    #[test]
    fn a_contained_panic_without_a_string_payload_still_names_the_tool() {
        let error = ToolError::ToolPanicked {
            tool: identity(),
            payload: None,
        };
        assert_eq!(error.to_string(), "fixture.tool@1.0.0 panicked");
        assert_eq!(error.as_failure().kind(), "tool_panicked");
    }

    #[test]
    fn a_signalled_child_does_not_report_itself_as_status_none() {
        // The executor kills a timed-out child itself, so this rendering is the
        // common case rather than an exotic one; "status None" would read as a
        // bug in Harkness.
        assert_eq!(
            ToolError::ProcessFailed {
                code: None,
                stderr_tail: String::new(),
            }
            .to_string(),
            "a child process was ended by a signal"
        );
        assert_eq!(
            ToolError::ProcessFailed {
                code: Some(128),
                stderr_tail: "  fatal: not a repository\n".to_owned(),
            }
            .to_string(),
            "a child process exited with status 128: fatal: not a repository"
        );
    }

    #[test]
    fn a_sub_second_timeout_does_not_report_itself_as_zero() {
        assert_eq!(
            ToolError::TimedOut {
                limit: Duration::from_millis(250),
            }
            .to_string(),
            "the tool exceeded its 250ms time limit"
        );
        assert_eq!(
            ToolError::TimedOut {
                limit: Duration::from_secs(30),
            }
            .to_string(),
            "the tool exceeded its 30s time limit"
        );
    }

    #[test]
    fn schema_directions_use_stable_spellings() {
        let spellings = SchemaDirection::ALL
            .iter()
            .map(|direction| direction.to_string())
            .collect::<Vec<_>>();
        assert_eq!(spellings, ["input", "output"]);
    }
}
