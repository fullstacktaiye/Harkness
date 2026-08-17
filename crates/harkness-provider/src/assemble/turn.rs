//! What assembly produces: one turn, and one record per tool call in it.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    contract::{ErrorDetail, ProviderToolCallId, StopReason, TokenUsage},
    text::{Preview, PreviewList},
};

/// Bytes of any one string a `Debug` rendering shows.
const DEBUG_TEXT_BYTES: usize = 48;

/// Entries of any one list a `Debug` rendering shows.
const DEBUG_LIST_ENTRIES: usize = 3;

/// Where a tool call's identity came from.
///
/// Recorded rather than inferred from the spelling, so a provider that happens
/// to issue an id shaped like a synthesized one is still recorded as having
/// issued it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdProvenance {
    /// The provider issued this id.
    Provider,
    /// The provider issued none and the assembler invented one.
    Synthesized,
}

/// Why an assembled call cannot be executed.
///
/// Every defect is *surfaced*, never dropped: a call the model asked for and
/// Harkness could not read is history, and a turn that silently omitted it
/// would describe a conversation that did not happen. [#126] refuses to execute
/// one and reports it back to the model, which is how a model gets the chance
/// to correct itself.
///
/// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "defect", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ToolCallDefect {
    /// The accumulated argument text is not one JSON value.
    UnparsableArguments {
        /// What the parser said.
        detail: ErrorDetail,
    },
    /// The provider never named a tool to call.
    MissingName,
    /// The stream ended before the call's arguments were complete.
    ///
    /// Reported in preference to [`UnparsableArguments`](Self::UnparsableArguments)
    /// when both are true: half a JSON object failing to parse is a consequence
    /// of the disconnect, not a second finding.
    Truncated,
}

/// One tool call the model asked for.
///
/// Two states rather than one struct with a validity flag, because the
/// difference is what a caller may do next: a [`Ready`](Self::Ready) call has
/// arguments the runtime can validate against a tool schema, and an
/// [`Invalid`](Self::Invalid) one has nothing that could be executed. Making
/// that a compile-time distinction is what stops "check the flag" from becoming
/// "forgot to check the flag".
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum AssembledToolCall {
    /// The provider named a tool and its arguments parsed.
    Ready {
        /// Position within the turn.
        index: u32,
        /// Identity, provider-issued or synthesized.
        id: ProviderToolCallId,
        /// Which of the two it is.
        id_provenance: IdProvenance,
        /// Set when an earlier call in this turn already presented this id.
        /// The two are never merged: a provider repeating an id is describing
        /// two calls, and folding them would execute one of them twice or not
        /// at all.
        duplicate_of: Option<ProviderToolCallId>,
        /// The tool the model named.
        name: String,
        /// The parsed arguments.
        arguments: Value,
    },
    /// The call was surfaced and cannot be executed.
    Invalid {
        /// Position within the turn.
        index: u32,
        /// Identity, provider-issued or synthesized.
        id: ProviderToolCallId,
        /// Which of the two it is.
        id_provenance: IdProvenance,
        /// Set when an earlier call in this turn already presented this id.
        duplicate_of: Option<ProviderToolCallId>,
        /// The tool the model named, when it named one.
        name: Option<String>,
        /// Exactly the argument text that accumulated, bounded by the
        /// assembler's per-call cap. A caller persisting this owes it whatever
        /// bound its own column has.
        raw_arguments: String,
        /// Why it cannot be executed.
        defect: ToolCallDefect,
    },
}

impl AssembledToolCall {
    /// Position of this call within its turn.
    #[must_use]
    pub const fn index(&self) -> u32 {
        match self {
            Self::Ready { index, .. } | Self::Invalid { index, .. } => *index,
        }
    }

    /// Identity of this call.
    #[must_use]
    pub const fn id(&self) -> &ProviderToolCallId {
        match self {
            Self::Ready { id, .. } | Self::Invalid { id, .. } => id,
        }
    }

    /// Where the identity came from.
    #[must_use]
    pub const fn id_provenance(&self) -> IdProvenance {
        match self {
            Self::Ready { id_provenance, .. } | Self::Invalid { id_provenance, .. } => {
                *id_provenance
            }
        }
    }

    /// The earlier call this one repeats the identity of, if any.
    #[must_use]
    pub const fn duplicate_of(&self) -> Option<&ProviderToolCallId> {
        match self {
            Self::Ready { duplicate_of, .. } | Self::Invalid { duplicate_of, .. } => {
                duplicate_of.as_ref()
            }
        }
    }

    /// The tool named, when one was.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Ready { name, .. } => Some(name),
            Self::Invalid { name, .. } => name.as_deref(),
        }
    }

    /// The parsed arguments, for a call that has some.
    #[must_use]
    pub const fn arguments(&self) -> Option<&Value> {
        match self {
            Self::Ready { arguments, .. } => Some(arguments),
            Self::Invalid { .. } => None,
        }
    }

    /// Whether this call could be executed at all.
    ///
    /// A duplicate is `Ready`: whether repeating an identity disqualifies a
    /// call is a decision for the loop that would run it, not for the reader
    /// that assembled it.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// Why the call cannot be executed, when it cannot.
    #[must_use]
    pub const fn defect(&self) -> Option<&ToolCallDefect> {
        match self {
            Self::Ready { .. } => None,
            Self::Invalid { defect, .. } => Some(defect),
        }
    }
}

/// Truncated on purpose, and for the same reason [`AssistantTurn`]'s is: a call
/// carries as much model-written text as the assembler's per-call cap allows,
/// so a derived `Debug` here would put a megabyte of arguments into whatever
/// logged the turn — or logged the [`Disconnected`](crate::contract::ProviderError::Disconnected)
/// error that carries one. Bounding the *list* is not enough when one entry can
/// be that large.
impl fmt::Debug for AssembledToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready {
                index,
                id,
                id_provenance,
                duplicate_of,
                name,
                arguments,
            } => formatter
                .debug_struct("Ready")
                .field("index", index)
                .field("id", id)
                .field("id_provenance", id_provenance)
                .field("duplicate_of", duplicate_of)
                .field("name", name)
                .field(
                    "arguments",
                    &Preview::new(&arguments.to_string(), DEBUG_TEXT_BYTES),
                )
                .finish(),
            Self::Invalid {
                index,
                id,
                id_provenance,
                duplicate_of,
                name,
                raw_arguments,
                defect,
            } => formatter
                .debug_struct("Invalid")
                .field("index", index)
                .field("id", id)
                .field("id_provenance", id_provenance)
                .field("duplicate_of", duplicate_of)
                .field("name", name)
                .field(
                    "raw_arguments",
                    &Preview::new(raw_arguments, DEBUG_TEXT_BYTES),
                )
                .field("defect", defect)
                .finish(),
        }
    }
}

/// Previewed too: a parser's account of what it rejected quotes the input, and
/// [`ErrorDetail`] bounds that at two kilobytes — enough for three entries to
/// overrun a turn's whole rendering budget.
impl fmt::Debug for ToolCallDefect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparsableArguments { detail } => formatter
                .debug_struct("UnparsableArguments")
                .field("detail", &Preview::new(detail.as_str(), DEBUG_TEXT_BYTES))
                .finish(),
            Self::MissingName => formatter.write_str("MissingName"),
            Self::Truncated => formatter.write_str("Truncated"),
        }
    }
}

/// What assembly had to work around while reading a turn.
///
/// Counters rather than a log: they are cheap enough to keep on every turn and
/// specific enough that a provider whose streams need working around shows up
/// in the numbers [#126] persists.
///
/// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssemblyDiagnostics {
    /// Events that arrived after the turn had already completed.
    pub ignored_after_completion: u32,
    /// Tool calls the provider left unnamed, which the assembler named.
    pub synthesized_ids: u32,
    /// Tool calls presenting an identity an earlier call in this turn used.
    pub duplicate_ids: u32,
}

impl AssemblyDiagnostics {
    /// Whether the turn arrived exactly as the contract describes.
    #[must_use]
    pub const fn is_clean(self) -> bool {
        self.ignored_after_completion == 0 && self.synthesized_ids == 0 && self.duplicate_ids == 0
    }
}

/// One assistant turn, assembled from the events that produced it.
///
/// `stop` is what the *provider* said, and is `None` when it never said —
/// a disconnected stream, or one Harkness stopped itself. The outcome's own
/// [`stop`](crate::contract::TurnOutcome::stop) is what the call concluded, and
/// the two are deliberately not the same field.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantTurn {
    /// Everything the model said, in order.
    pub text: String,
    /// Every call it asked for, in index order, valid and invalid alike.
    pub tool_calls: Vec<AssembledToolCall>,
    /// What the turn cost, when the provider reported it.
    pub usage: Option<TokenUsage>,
    /// Why the provider said the turn ended, when it said.
    pub stop: Option<StopReason>,
}

impl AssistantTurn {
    /// The calls a caller may consider executing.
    pub fn ready_calls(&self) -> impl Iterator<Item = &AssembledToolCall> {
        self.tool_calls.iter().filter(|call| call.is_ready())
    }

    /// The calls that were surfaced and cannot be executed.
    pub fn invalid_calls(&self) -> impl Iterator<Item = &AssembledToolCall> {
        self.tool_calls.iter().filter(|call| !call.is_ready())
    }
}

/// Truncated on purpose: a turn holds model output, and `{:?}` on one must not
/// be able to dump it into a log before redaction applies.
impl fmt::Debug for AssistantTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssistantTurn")
            .field("text", &Preview::new(&self.text, DEBUG_TEXT_BYTES))
            .field(
                "tool_calls",
                &PreviewList::new(&self.tool_calls, DEBUG_LIST_ENTRIES),
            )
            .field("usage", &self.usage)
            .field("stop", &self.stop)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        AssembledToolCall, AssemblyDiagnostics, AssistantTurn, IdProvenance, ToolCallDefect,
    };
    use crate::contract::{ErrorDetail, ProviderToolCallId, StopReason};

    fn ready(index: u32, id: &str) -> AssembledToolCall {
        AssembledToolCall::Ready {
            index,
            id: ProviderToolCallId::new(id).unwrap(),
            id_provenance: IdProvenance::Provider,
            duplicate_of: None,
            name: "fs.read".to_owned(),
            arguments: json!({"path": "src/lib.rs"}),
        }
    }

    fn invalid(index: u32, id: &str) -> AssembledToolCall {
        AssembledToolCall::Invalid {
            index,
            id: ProviderToolCallId::new(id).unwrap(),
            id_provenance: IdProvenance::Synthesized,
            duplicate_of: None,
            name: Some("fs.read".to_owned()),
            raw_arguments: "{\"path\":".to_owned(),
            defect: ToolCallDefect::UnparsableArguments {
                detail: ErrorDetail::new("EOF while parsing an object"),
            },
        }
    }

    #[test]
    fn accessors_answer_for_both_states() {
        let call = ready(0, "call_1");
        assert_eq!(call.index(), 0);
        assert_eq!(call.name(), Some("fs.read"));
        assert!(call.is_ready());
        assert!(call.defect().is_none());
        assert_eq!(call.arguments(), Some(&json!({"path": "src/lib.rs"})));

        let broken = invalid(1, "call_2");
        assert!(!broken.is_ready());
        assert!(broken.arguments().is_none());
        assert_eq!(broken.id_provenance(), IdProvenance::Synthesized);
        assert!(matches!(
            broken.defect(),
            Some(ToolCallDefect::UnparsableArguments { .. })
        ));
    }

    #[test]
    fn a_turn_round_trips_through_serde() {
        let turn = AssistantTurn {
            text: "Reading the file.".to_owned(),
            tool_calls: vec![ready(0, "call_1"), invalid(1, "call_2")],
            usage: None,
            stop: Some(StopReason::ToolUse),
        };
        let json = serde_json::to_string(&turn).unwrap();
        assert_eq!(serde_json::from_str::<AssistantTurn>(&json).unwrap(), turn);
        assert!(json.contains("\"state\":\"ready\""), "{json}");
        assert!(json.contains("\"state\":\"invalid\""), "{json}");
    }

    #[test]
    fn ready_and_invalid_calls_are_both_reachable_from_the_turn() {
        let turn = AssistantTurn {
            text: String::new(),
            tool_calls: vec![ready(0, "call_1"), invalid(1, "call_2"), ready(2, "call_3")],
            usage: None,
            stop: None,
        };
        assert_eq!(turn.ready_calls().count(), 2);
        assert_eq!(turn.invalid_calls().count(), 1);
        assert_eq!(turn.tool_calls.len(), 3, "nothing is dropped");
    }

    /// Bounding the list is not enough when one entry holds a megabyte: a call's
    /// arguments are model-written text of exactly the size the assembler's
    /// per-call cap allows, and a disconnect mid-arguments is the case most
    /// likely to be logged.
    #[test]
    fn debugging_a_turn_bounds_the_calls_inside_it_and_not_only_their_number() {
        let huge = "x".repeat(900 * 1024);
        let turn = AssistantTurn {
            text: String::new(),
            tool_calls: vec![
                AssembledToolCall::Invalid {
                    index: 0,
                    id: ProviderToolCallId::new("call_1").unwrap(),
                    id_provenance: IdProvenance::Provider,
                    duplicate_of: None,
                    name: Some("fs.read".to_owned()),
                    raw_arguments: huge.clone(),
                    defect: ToolCallDefect::UnparsableArguments {
                        detail: ErrorDetail::new(huge.clone()),
                    },
                },
                AssembledToolCall::Ready {
                    index: 1,
                    id: ProviderToolCallId::new("call_2").unwrap(),
                    id_provenance: IdProvenance::Provider,
                    duplicate_of: None,
                    name: "fs.read".to_owned(),
                    arguments: json!({ "blob": huge }),
                },
            ],
            usage: None,
            stop: None,
        };

        let rendered = format!("{turn:?}");
        assert!(rendered.len() < 4 * 1024, "{} bytes", rendered.len());

        // The same turn reached through the failure that carries it, which is
        // the path a disconnect mid-arguments takes into a log.
        let error = crate::contract::ProviderError::Disconnected {
            detail: ErrorDetail::new("the endpoint went away"),
            partial: Some(Box::new(turn)),
        };
        let rendered = format!("{error:?}");
        assert!(rendered.len() < 4 * 1024, "{} bytes", rendered.len());
    }

    #[test]
    fn debugging_a_turn_bounds_what_it_shows() {
        let turn = AssistantTurn {
            text: "z".repeat(1024 * 1024),
            tool_calls: (0..100).map(|index| ready(index, "call_1")).collect(),
            usage: None,
            stop: None,
        };
        let rendered = format!("{turn:?}");
        assert!(rendered.len() < 4 * 1024, "{} bytes", rendered.len());
        assert!(rendered.contains("(+1048528 bytes)"), "{rendered}");
    }

    #[test]
    fn clean_diagnostics_mean_the_turn_arrived_as_the_contract_describes() {
        assert!(AssemblyDiagnostics::default().is_clean());
        assert!(
            !AssemblyDiagnostics {
                duplicate_ids: 1,
                ..AssemblyDiagnostics::default()
            }
            .is_clean()
        );
    }
}
