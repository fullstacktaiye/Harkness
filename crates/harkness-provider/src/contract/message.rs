//! What is sent to a provider: messages, tool definitions, and one request.
//!
//! Everything here is inert data. A [`ContentPart::ToolOutput`] carries the
//! bytes a Harkness tool produced and a [`ContentPart::Text`] carries whatever
//! the prompt builder assembled, and *neither is an instruction to Harkness*:
//! repository content, tool output, and model output are untrusted alike
//! (ADR-0006). Delimiting them so a model cannot confuse one for the system's
//! own voice is [#127]'s job; this module only refuses to pretend the
//! distinction does not exist.
//!
//! Nothing here can carry credentials. There is no endpoint, no header, no key
//! and no profile in this crate at all — [#124] owns those — so a request
//! cannot leak one by construction rather than by review.
//!
//! [#124]: https://github.com/fullstacktaiye/harkness/issues/124
//! [#127]: https://github.com/fullstacktaiye/harkness/issues/127

use std::fmt;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::text::{Preview, PreviewList};

use super::{
    error::ContractError,
    ids::{ModelId, ProviderToolCallId},
};

/// Bytes of any one string a `Debug` rendering shows.
const DEBUG_TEXT_BYTES: usize = 48;

/// Entries of any one list a `Debug` rendering shows.
const DEBUG_LIST_ENTRIES: usize = 3;

/// Who produced one message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Harkness's own instructions.
    System,
    /// The user's request.
    User,
    /// What the model produced on an earlier turn.
    Assistant,
    /// What a Harkness tool returned for a call the model requested.
    ToolResult,
}

impl Role {
    /// Every stable spelling, in declaration order.
    pub const SPELLINGS: &'static [&'static str] = &["system", "user", "assistant", "tool_result"];

    /// Stable machine-readable spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolResult => "tool_result",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One piece of one message.
///
/// A tool call travels as `arguments_json` rather than a parsed value because
/// this is the *transcript* of an earlier turn: what the model actually emitted
/// is the string, and re-encoding a parsed value would send the provider
/// something it did not say.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    /// Free text.
    Text {
        /// The text.
        text: String,
    },
    /// A tool call the model requested on an earlier turn.
    ToolCall {
        /// Identity the call was recorded under.
        id: ProviderToolCallId,
        /// Tool the model named.
        name: String,
        /// Exactly the argument text the model emitted.
        arguments_json: String,
    },
    /// What Harkness returned for one call.
    ToolOutput {
        /// The call this answers.
        call_id: ProviderToolCallId,
        /// The result, already redacted by the runtime that produced it.
        content: String,
        /// Whether the call failed. Carried rather than encoded into `content`,
        /// so a failure is legible to the provider's own tool protocol.
        is_error: bool,
    },
}

impl fmt::Debug for ContentPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => formatter
                .debug_struct("Text")
                .field("text", &Preview::new(text, DEBUG_TEXT_BYTES))
                .finish(),
            Self::ToolCall {
                id,
                name,
                arguments_json,
            } => formatter
                .debug_struct("ToolCall")
                .field("id", id)
                .field("name", name)
                .field(
                    "arguments_json",
                    &Preview::new(arguments_json, DEBUG_TEXT_BYTES),
                )
                .finish(),
            Self::ToolOutput {
                call_id,
                content,
                is_error,
            } => formatter
                .debug_struct("ToolOutput")
                .field("call_id", call_id)
                .field("content", &Preview::new(content, DEBUG_TEXT_BYTES))
                .field("is_error", is_error)
                .finish(),
        }
    }
}

/// One message in a conversation.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMessage {
    /// Who produced it.
    pub role: Role,
    /// What it contains.
    pub parts: Vec<ContentPart>,
}

impl ModelMessage {
    /// Builds a message of one text part.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![ContentPart::Text { text: text.into() }],
        }
    }

    /// Builds a message from parts.
    #[must_use]
    pub const fn new(role: Role, parts: Vec<ContentPart>) -> Self {
        Self { role, parts }
    }
}

impl fmt::Debug for ModelMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelMessage")
            .field("role", &self.role)
            .field("parts", &PreviewList::new(&self.parts, DEBUG_LIST_ENTRIES))
            .finish()
    }
}

/// One tool offered to the model.
///
/// `input_schema` is a JSON Schema the runtime already publishes for the tool
/// ([#87]); this crate carries it verbatim and validates nothing, because the
/// registry is the only thing that can say what a tool accepts.
///
/// [#87]: https://github.com/fullstacktaiye/harkness/issues/87
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    /// Name the provider will echo back on a call.
    pub name: String,
    /// What the tool does, in the model's terms.
    pub description: String,
    /// JSON Schema of the tool's input.
    pub input_schema: Value,
}

impl ToolDefinition {
    /// Builds a tool definition.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let schema = self.input_schema.to_string();
        formatter
            .debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field(
                "description",
                &Preview::new(&self.description, DEBUG_TEXT_BYTES),
            )
            .field("input_schema", &Preview::new(&schema, DEBUG_TEXT_BYTES))
            .finish()
    }
}

/// One request for one assistant turn.
///
/// Fields are private and the setters are fallible for one reason: a value that
/// cannot be encoded must be refused where it is written. A non-finite
/// temperature serializes as JSON `null`, which reaches the endpoint as "no
/// temperature at all" — the same folding of two different inputs onto one
/// encoding that `canonical_input_hash` refuses in `harkness-runtime`.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(try_from = "ModelRequestWire")]
pub struct ModelRequest {
    model: ModelId,
    messages: Vec<ModelMessage>,
    tools: Vec<ToolDefinition>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl ModelRequest {
    /// Builds a request for `model` over `messages`.
    #[must_use]
    pub fn new(model: ModelId, messages: Vec<ModelMessage>) -> Self {
        Self {
            model,
            messages,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
        }
    }

    /// Offers `tools` to the model.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    /// Asks the model to stop after `tokens` of output.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidRequest`] for zero, which asks for a
    /// turn that cannot say anything.
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Result<Self, ContractError> {
        if tokens == 0 {
            return Err(ContractError::InvalidRequest {
                field: "max_output_tokens",
                reason: "a turn bounded to zero tokens cannot produce an answer",
            });
        }
        self.max_output_tokens = Some(tokens);
        Ok(self)
    }

    /// Sets the sampling temperature.
    ///
    /// The accepted range belongs to the provider and is not narrowed here.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidRequest`] for a negative or non-finite
    /// value.
    pub fn with_temperature(mut self, temperature: f32) -> Result<Self, ContractError> {
        if !temperature.is_finite() {
            return Err(ContractError::InvalidRequest {
                field: "temperature",
                reason: "a non-finite temperature encodes as null and would read as unset",
            });
        }
        if temperature < 0.0 {
            return Err(ContractError::InvalidRequest {
                field: "temperature",
                reason: "a temperature cannot be negative",
            });
        }
        self.temperature = Some(temperature);
        Ok(self)
    }

    /// The model this request names.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// The conversation.
    #[must_use]
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// The tools offered.
    #[must_use]
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// The requested output bound.
    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    /// The requested sampling temperature.
    #[must_use]
    pub const fn temperature(&self) -> Option<f32> {
        self.temperature
    }
}

/// Truncated on purpose: `{:?}` on a request must not be able to dump a prompt.
impl fmt::Debug for ModelRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequest")
            .field("model", &self.model)
            .field(
                "messages",
                &PreviewList::new(&self.messages, DEBUG_LIST_ENTRIES),
            )
            .field("tools", &PreviewList::new(&self.tools, DEBUG_LIST_ENTRIES))
            .field("max_output_tokens", &self.max_output_tokens)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl Serialize for ModelRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ModelRequestWireRef {
            model: &self.model,
            messages: &self.messages,
            tools: &self.tools,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
        }
        .serialize(serializer)
    }
}

/// The borrowing serialization form, kept byte-compatible with [`ModelRequestWire`].
#[derive(Serialize)]
struct ModelRequestWireRef<'a> {
    model: &'a ModelId,
    messages: &'a [ModelMessage],
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

/// The owned deserialization form, revalidated through the fallible setters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRequestWire {
    model: ModelId,
    messages: Vec<ModelMessage>,
    #[serde(default)]
    tools: Vec<ToolDefinition>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
}

impl TryFrom<ModelRequestWire> for ModelRequest {
    type Error = ContractError;

    fn try_from(wire: ModelRequestWire) -> Result<Self, Self::Error> {
        let mut request = Self::new(wire.model, wire.messages).with_tools(wire.tools);
        if let Some(tokens) = wire.max_output_tokens {
            request = request.with_max_output_tokens(tokens)?;
        }
        if let Some(temperature) = wire.temperature {
            request = request.with_temperature(temperature)?;
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ContentPart, ModelMessage, ModelRequest, Role, ToolDefinition};
    use crate::contract::ids::{ModelId, ProviderToolCallId};

    fn model() -> ModelId {
        ModelId::new("gpt-4o-mini").unwrap()
    }

    #[test]
    fn every_role_spelling_is_declared_in_order() {
        let cases = [
            (Role::System, "system"),
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
            (Role::ToolResult, "tool_result"),
        ];
        let spellings = cases.iter().map(|(_, s)| *s).collect::<Vec<_>>();
        assert_eq!(spellings, Role::SPELLINGS);
        for (role, expected) in cases {
            assert_eq!(role.as_str(), expected);
            assert_eq!(
                serde_json::to_string(&role).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn a_request_round_trips_through_serde() {
        let request = ModelRequest::new(
            model(),
            vec![
                ModelMessage::text(Role::System, "You are Harkness."),
                ModelMessage::new(
                    Role::Assistant,
                    vec![ContentPart::ToolCall {
                        id: ProviderToolCallId::new("call_1").unwrap(),
                        name: "fs.read".to_owned(),
                        arguments_json: "{\"path\":\"src/lib.rs\"}".to_owned(),
                    }],
                ),
                ModelMessage::new(
                    Role::ToolResult,
                    vec![ContentPart::ToolOutput {
                        call_id: ProviderToolCallId::new("call_1").unwrap(),
                        content: "pub fn main() {}".to_owned(),
                        is_error: false,
                    }],
                ),
            ],
        )
        .with_tools(vec![ToolDefinition::new(
            "fs.read",
            "Reads a file",
            json!({"type": "object"}),
        )])
        .with_max_output_tokens(1_024)
        .unwrap()
        .with_temperature(0.2)
        .unwrap();

        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<ModelRequest>(&json).unwrap(),
            request,
            "the borrowing and owned wire forms must stay byte-compatible"
        );
    }

    #[test]
    fn a_request_refuses_values_that_would_encode_as_silence() {
        assert!(
            ModelRequest::new(model(), Vec::new())
                .with_temperature(f32::NAN)
                .is_err()
        );
        assert!(
            ModelRequest::new(model(), Vec::new())
                .with_temperature(f32::INFINITY)
                .is_err()
        );
        assert!(
            ModelRequest::new(model(), Vec::new())
                .with_temperature(-0.5)
                .is_err()
        );
        assert!(
            ModelRequest::new(model(), Vec::new())
                .with_max_output_tokens(0)
                .is_err()
        );
    }

    #[test]
    fn a_deserialized_request_is_revalidated_rather_than_trusted() {
        let refused = serde_json::from_str::<ModelRequest>(
            "{\"model\":\"gpt-4o-mini\",\"messages\":[],\"max_output_tokens\":0}",
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("max_output_tokens"),
            "{refused}"
        );

        let unknown = serde_json::from_str::<ModelRequest>(
            "{\"model\":\"gpt-4o-mini\",\"messages\":[],\"top_p\":0.9}",
        )
        .unwrap_err();
        assert!(unknown.to_string().contains("top_p"), "{unknown}");
    }

    /// The acceptance criterion from [#111]: a megabyte of prompt must not be
    /// able to reach a log through `{:?}`.
    #[test]
    fn debugging_a_megabyte_request_stays_under_four_kibibytes() {
        let request = ModelRequest::new(
            model(),
            vec![ModelMessage::text(Role::User, "x".repeat(1024 * 1024))],
        );
        let rendered = format!("{request:?}");
        assert!(
            rendered.len() < 4 * 1024,
            "a debug rendering of {} bytes is not bounded",
            rendered.len()
        );
        assert!(rendered.contains("(+1048528 bytes)"), "{rendered}");
    }

    /// Many small messages are the other shape of the same problem: bounding
    /// each string alone still lets a thousand of them through.
    #[test]
    fn debugging_a_conversation_bounds_the_number_of_entries_too() {
        let request = ModelRequest::new(
            model(),
            (0..1_000)
                .map(|index| ModelMessage::text(Role::User, format!("message {index}")))
                .collect(),
        )
        .with_tools(
            (0..1_000)
                .map(|index| {
                    ToolDefinition::new(
                        format!("tool.{index}"),
                        "x".repeat(4_096),
                        serde_json::json!({"type": "object", "properties": {}}),
                    )
                })
                .collect(),
        );
        let rendered = format!("{request:?}");
        assert!(rendered.len() < 4 * 1024, "{} bytes", rendered.len());
        assert!(rendered.contains("… (+997 more)"), "{rendered}");
    }

    #[test]
    fn debugging_a_part_previews_every_string_it_carries() {
        let part = ContentPart::ToolOutput {
            call_id: ProviderToolCallId::new("call_1").unwrap(),
            content: "y".repeat(10_000),
            is_error: true,
        };
        let rendered = format!("{part:?}");
        assert!(rendered.len() < 256, "{rendered}");
        assert!(rendered.contains("is_error: true"), "{rendered}");
    }
}
