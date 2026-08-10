use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::Failure;

use super::ToolIdentity;

/// Violations reported for one rejected value before the list is truncated.
///
/// A schema violation report is a diagnostic for whoever produced the value —
/// often an agent retrying with a correction — and the first handful of
/// violations locate the mistake. The bound also keeps the rendered message
/// small enough to persist as a [`Failure`] beside the tool call.
pub const MAX_REPORTED_VIOLATIONS: usize = 10;

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
/// Both pointers are RFC 6901 JSON Pointers: `pointer` locates the offending
/// value inside the instance and `schema_pointer` locates the rule it broke
/// inside the published schema. An empty pointer refers to the whole document,
/// which is what a wrong top-level type reports.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaViolation {
    /// JSON Pointer into the rejected value.
    pub pointer: String,
    /// JSON Pointer into the schema rule that rejected it.
    pub schema_pointer: String,
    /// Human-readable explanation from the validator.
    pub message: String,
}

impl SchemaViolation {
    /// Records a violation at the given instance and schema locations.
    #[must_use]
    pub fn new(
        pointer: impl Into<String>,
        schema_pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            pointer: pointer.into(),
            schema_pointer: schema_pointer.into(),
            message: message.into(),
        }
    }
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
    #[error("{tool} input does not satisfy its declared schema: {}", render_violations(.violations))]
    InvalidInput {
        /// Tool whose schema refused the value.
        tool: ToolIdentity,
        /// Where the value broke the schema.
        violations: Vec<SchemaViolation>,
    },

    /// The tool returned a value that does not satisfy its declared output
    /// schema.
    ///
    /// The result is discarded rather than delivered: a consumer that trusted
    /// the published schema would otherwise receive a shape it cannot handle.
    #[error("{tool} output does not satisfy its declared schema: {}", render_violations(.violations))]
    InvalidOutput {
        /// Tool whose own output was refused.
        tool: ToolIdentity,
        /// Where the value broke the schema.
        violations: Vec<SchemaViolation>,
    },

    /// The tool ran and reported failure.
    #[error("the tool failed: {message}")]
    ExecutionFailed {
        /// Explanation from the tool.
        message: String,
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
        reason: &'static str,
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
        "timed_out",
        "cancelled",
        "denied",
        "forbidden_path",
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
            Self::TimedOut { .. } => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Denied { .. } => "denied",
            Self::ForbiddenPath { .. } => "forbidden_path",
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
    #[must_use]
    pub fn execution_failed(cause: impl fmt::Display) -> Self {
        Self::ExecutionFailed {
            message: cause.to_string(),
        }
    }

    /// Reports a policy or human refusal.
    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    /// Whether the tool body ran before this failure was reported.
    ///
    /// The two validation failures and every pre-execution refusal answer
    /// `false`, which is what lets a caller retry a corrected input without
    /// wondering whether the first attempt had a side effect.
    #[must_use]
    pub const fn happened_before_execution(&self) -> bool {
        matches!(
            self,
            Self::InvalidInput { .. } | Self::Denied { .. } | Self::ForbiddenPath { .. }
        )
    }

    /// Projects this failure into the durable form a tool call records.
    ///
    /// The kind is the stable discriminant and the message is this error's
    /// [`Display`](fmt::Display), which is bounded because violation lists are
    /// truncated at [`MAX_REPORTED_VIOLATIONS`].
    #[must_use]
    pub fn as_failure(&self) -> Failure {
        Failure::new(self.kind(), self.to_string())
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
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InvocationError {
    /// The named tool or version does not exist in the registry.
    #[error(transparent)]
    Resolution(#[from] RegistryError),

    /// The invocation reached a real tool and failed.
    #[error(transparent)]
    Tool(#[from] ToolError),
}

impl InvocationError {
    /// Stable machine-readable discriminant, delegated to the namespace that
    /// owns the failure.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Resolution(error) => error.kind(),
            Self::Tool(error) => error.kind(),
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
    #[must_use]
    pub fn as_failure(&self) -> Failure {
        match self {
            Self::Resolution(error) => Failure::new(error.kind(), error.to_string()),
            Self::Tool(error) => error.as_failure(),
        }
    }
}

/// Renders a bounded violation list for an error message.
fn render_violations(violations: &[SchemaViolation]) -> String {
    if violations.is_empty() {
        return "no violation was reported".to_owned();
    }

    let shown = violations.len().min(MAX_REPORTED_VIOLATIONS);
    let mut rendered = violations
        .iter()
        .take(shown)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if violations.len() > shown {
        rendered.push_str(&format!(
            " (and {} more)",
            violations.len().saturating_sub(shown)
        ));
    }
    rendered
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
        InvocationError, MAX_REPORTED_VIOLATIONS, RegistryError, SchemaDirection, SchemaViolation,
        ToolError,
    };
    use crate::tool::ToolIdentity;

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
                },
                "invalid_input",
            ),
            (
                ToolError::InvalidOutput {
                    tool: identity(),
                    violations: vec![violation()],
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
                    reason: "fixture",
                },
                "forbidden_path",
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
        let execution = InvocationError::from(ToolError::Cancelled);
        assert_eq!(execution.kind(), "cancelled");
        assert_eq!(execution.as_failure().kind(), "cancelled");
    }

    #[test]
    fn a_violation_message_names_the_pointer_and_truncates_a_long_list() {
        let single = ToolError::InvalidInput {
            tool: identity(),
            violations: vec![violation()],
        };
        assert_eq!(
            single.to_string(),
            "fixture.tool@1.0.0 input does not satisfy its declared schema: \
             /depth: \"deep\" is not of type integer"
        );

        let many = (0..MAX_REPORTED_VIOLATIONS + 3)
            .map(|index| SchemaViolation::new(format!("/field{index}"), "/properties", "wrong"))
            .collect::<Vec<_>>();
        let truncated = ToolError::InvalidInput {
            tool: identity(),
            violations: many,
        }
        .to_string();
        assert!(truncated.contains("(and 3 more)"), "{truncated}");
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
        };
        assert!(
            error
                .to_string()
                .ends_with("the value itself: 42 is not of type object"),
            "{error}"
        );
    }

    #[test]
    fn only_pre_execution_failures_promise_the_tool_body_never_ran() {
        assert!(
            ToolError::InvalidInput {
                tool: identity(),
                violations: vec![violation()],
            }
            .happened_before_execution()
        );
        assert!(ToolError::denied("policy").happened_before_execution());
        assert!(
            ToolError::ForbiddenPath {
                path: PathBuf::from(".."),
                reason: "fixture",
            }
            .happened_before_execution()
        );

        for ran in [
            ToolError::Cancelled,
            ToolError::Interrupted,
            ToolError::execution_failed("boom"),
            ToolError::InvalidOutput {
                tool: identity(),
                violations: vec![violation()],
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
