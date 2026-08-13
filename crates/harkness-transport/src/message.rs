//! The JSON-RPC 2.0 message vocabulary this engine frames and correlates.
//!
//! It is deliberately shallow. A [`Message`] knows whether it is a request, a
//! notification, or a response, and nothing about what any method means:
//! `initialize`, `session/prompt`, `tools/call`, and every protocol revision
//! that names them belong to the adapters above ([ADR-0009], [ADR-0012]).
//!
//! One type serves both directions. The ADR-0012 sketch names an
//! `OutboundMessage` and an `InboundMessage`, and the sketch fixes the shape
//! rather than the signatures: JSON-RPC is symmetric, both peers may open a
//! request, and two structurally identical types would mean every adapter wrote
//! a conversion between them for no property gained. Direction is carried by the
//! method that takes the message, which is where it is actually checked.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0012]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0012-stdio-only-protocol-transports.md

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::{DesyncDetail, TransportError};

/// The `jsonrpc` member every message on this transport carries.
const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request identifier.
///
/// The specification allows a string, a number, or null; null is excluded here
/// because it is the id a peer uses when it could not read the request at all,
/// which is a response to nothing and can never correlate. Fractional numbers
/// are excluded for a blunter reason: two ids that differ only below the
/// precision of a float are one key in a correlation table.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    /// An integer id. Every id this engine allocates is one of these.
    Number(i64),
    /// A string id, which a peer may open its own requests with.
    Text(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(formatter, "{number}"),
            Self::Text(text) => write!(formatter, "'{text}'"),
        }
    }
}

/// A call that expects exactly one response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// The identifier the response will name.
    pub id: RequestId,
    /// The method name, which this crate never interprets.
    pub method: String,
    /// The method's parameters, absent when the method takes none.
    pub params: Option<Value>,
}

/// A call that expects no response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    /// The method name, which this crate never interprets.
    pub method: String,
    /// The method's parameters, absent when the method takes none.
    pub params: Option<Value>,
}

/// The single answer to one [`Request`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// The request this answers.
    pub id: RequestId,
    /// The result, or the error the peer reported instead.
    pub outcome: Result<Value, PeerError>,
}

/// An error a peer reported for one request.
///
/// This is the peer's own vocabulary, carried through unread: the `code` is
/// whatever the protocol above assigns, and no meaning is attached to it here.
/// It is not a [`TransportError`] and must not be turned into one — a method
/// that failed is a working connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerError {
    /// The peer's numeric error code.
    pub code: i64,
    /// The peer's human-readable message.
    pub message: String,
    /// Any structured detail the peer attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

/// One JSON-RPC message, in either direction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Message {
    /// A call awaiting a response.
    Request(Request),
    /// A call awaiting nothing.
    Notification(Notification),
    /// An answer to a call.
    Response(Response),
}

impl Message {
    /// Builds a request.
    #[must_use]
    pub fn request(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Request(Request {
            id,
            method: method.into(),
            params,
        })
    }

    /// Builds a notification.
    #[must_use]
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification(Notification {
            method: method.into(),
            params,
        })
    }

    /// Builds a successful response.
    #[must_use]
    pub fn result(id: RequestId, result: Value) -> Self {
        Self::Response(Response {
            id,
            outcome: Ok(result),
        })
    }

    /// Builds a failed response.
    #[must_use]
    pub fn failure(id: RequestId, error: PeerError) -> Self {
        Self::Response(Response {
            id,
            outcome: Err(error),
        })
    }

    /// The message's JSON-RPC encoding, without a line terminator.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::UnencodableMessage`] when the message cannot be
    /// serialized — a `params` value holding a non-finite number is the reachable
    /// case.
    pub fn encode(&self) -> Result<String, TransportError> {
        let wire = match self {
            Self::Request(request) => WireMessage {
                jsonrpc: JSONRPC_VERSION,
                id: Some(&request.id),
                method: Some(&request.method),
                params: request.params.as_ref(),
                result: None,
                error: None,
            },
            Self::Notification(notification) => WireMessage {
                jsonrpc: JSONRPC_VERSION,
                id: None,
                method: Some(&notification.method),
                params: notification.params.as_ref(),
                result: None,
                error: None,
            },
            Self::Response(response) => WireMessage {
                jsonrpc: JSONRPC_VERSION,
                id: Some(&response.id),
                method: None,
                params: None,
                result: response.outcome.as_ref().ok(),
                error: response.outcome.as_ref().err(),
            },
        };
        serde_json::to_string(&wire).map_err(|source| TransportError::UnencodableMessage {
            detail: source.to_string(),
        })
    }

    /// Reads one message from one complete line.
    ///
    /// The whole line must be exactly one JSON value: trailing content is how a
    /// peer that wrote two messages without a separator presents itself, and a
    /// truncated value is how one that died mid-write does. Both are refused as
    /// desynchronization rather than parsed as far as they go.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Desynchronized`] when the line is not one JSON
    /// value, or is a JSON value that is not a JSON-RPC 2.0 message.
    pub fn decode(line: &str) -> Result<Self, TransportError> {
        let wire: InboundWire = serde_json::from_str(line).map_err(|source| {
            desynchronized(DesyncDetail::NonJsonLine {
                detail: source.to_string(),
            })
        })?;
        wire.into_message()
    }
}

/// The serialization shape, borrowing from the message being written.
#[derive(Serialize)]
struct WireMessage<'a> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a PeerError>,
}

/// The deserialization shape.
///
/// Deliberately without `deny_unknown_fields`. An unknown member is a protocol
/// question, and this layer answers framing questions: quarantining a connection
/// because a peer attached an extension field would make the engine a
/// participant in a negotiation it cannot see. The members it *does* read are
/// checked strictly, because those are what decide where a message goes.
#[derive(Deserialize)]
struct InboundWire {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    /// Present-and-null is what a successful response with no value looks like,
    /// and it is a different message from one with no `result` member at all —
    /// which is not a response. Plain `Option` folds the two together, so
    /// presence is read explicitly here.
    #[serde(default, deserialize_with = "present_value")]
    result: Option<Value>,
    #[serde(default)]
    error: Option<PeerError>,
}

/// Reads a member that is present, whatever its value.
///
/// `serde` only calls this when the member exists, so `null` arrives as
/// `Some(Value::Null)` rather than collapsing to `None`.
fn present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

impl InboundWire {
    fn into_message(self) -> Result<Message, TransportError> {
        if self.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
            return Err(desynchronized(DesyncDetail::NotJsonRpc {
                detail: match self.jsonrpc {
                    Some(version) => format!("jsonrpc is '{version}', not '{JSONRPC_VERSION}'"),
                    None => "the message has no jsonrpc member".to_owned(),
                },
            }));
        }

        // A null id is the specification's "I could not tell which request this
        // was", which correlates with nothing. Treating it as absent is what
        // makes it a malformed response below rather than a table lookup for a
        // key that cannot exist.
        let id = match self.id {
            None | Some(Value::Null) => None,
            Some(value) => Some(request_id(value)?),
        };

        match (id, self.method) {
            (Some(id), Some(method)) => Ok(Message::Request(Request {
                id,
                method,
                params: self.params,
            })),
            (None, Some(method)) => Ok(Message::Notification(Notification {
                method,
                params: self.params,
            })),
            (Some(id), None) => match (self.result, self.error) {
                (Some(result), None) => Ok(Message::Response(Response {
                    id,
                    outcome: Ok(result),
                })),
                (None, Some(error)) => Ok(Message::Response(Response {
                    id,
                    outcome: Err(error),
                })),
                (Some(_), Some(_)) => Err(desynchronized(DesyncDetail::NotJsonRpc {
                    detail: "a response carries both result and error".to_owned(),
                })),
                (None, None) => Err(desynchronized(DesyncDetail::NotJsonRpc {
                    detail: "a response carries neither result nor error".to_owned(),
                })),
            },
            (None, None) => Err(desynchronized(DesyncDetail::NotJsonRpc {
                detail: "the message has neither a method nor an id".to_owned(),
            })),
        }
    }
}

fn request_id(value: Value) -> Result<RequestId, TransportError> {
    match value {
        Value::String(text) => Ok(RequestId::Text(text)),
        Value::Number(number) => number.as_i64().map(RequestId::Number).ok_or_else(|| {
            desynchronized(DesyncDetail::NotJsonRpc {
                detail: format!("id {number} is not an integer"),
            })
        }),
        other => Err(desynchronized(DesyncDetail::NotJsonRpc {
            detail: format!("id {other} is neither a string nor a number"),
        })),
    }
}

fn desynchronized(detail: DesyncDetail) -> TransportError {
    TransportError::Desynchronized { detail }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Message, PeerError, RequestId};
    use crate::error::{DesyncDetail, TransportError};

    fn desync(line: &str) -> DesyncDetail {
        match Message::decode(line) {
            Err(TransportError::Desynchronized { detail }) => detail,
            other => panic!("expected a desynchronization, got {other:?}"),
        }
    }

    #[test]
    fn each_message_shape_survives_a_round_trip() {
        let cases = [
            Message::request(RequestId::Number(1), "initialize", Some(json!({"v": 1}))),
            Message::request(RequestId::from("abc"), "session/new", None),
            Message::notification("notifications/cancelled", Some(json!({"id": 4}))),
            Message::notification("ping", None),
            Message::result(RequestId::Number(2), json!({"ok": true})),
            Message::result(RequestId::Number(3), Value::Null),
            Message::failure(
                RequestId::Number(4),
                PeerError {
                    code: -32601,
                    message: "method not found".to_owned(),
                    data: Some(json!({"method": "nope"})),
                },
            ),
        ];

        for message in cases {
            let encoded = message.encode().unwrap();
            assert_eq!(Message::decode(&encoded).unwrap(), message);
        }
    }

    /// A response's `result` is present and null on success, which is a
    /// different message from a response with no `result` member at all — the
    /// second is not a response.
    #[test]
    fn a_null_result_stays_a_successful_response() {
        let encoded = Message::result(RequestId::Number(1), Value::Null)
            .encode()
            .unwrap();
        assert!(encoded.contains("\"result\":null"));
        assert_eq!(
            desync(r#"{"jsonrpc":"2.0","id":1}"#).detail(),
            "not_json_rpc"
        );
    }

    /// Newline-delimited framing means an encoding carrying a newline would
    /// reach the peer as two messages. `serde_json` escapes them, and this is
    /// the assertion that keeps that true of the strings this crate frames.
    #[test]
    fn an_encoding_never_contains_a_line_terminator() {
        let encoded = Message::notification(
            "log",
            Some(json!({"text": "first\nsecond\rthird\u{2028}fourth"})),
        )
        .encode()
        .unwrap();

        assert!(!encoded.contains('\n'));
        assert!(!encoded.contains('\r'));
        assert_eq!(
            Message::decode(&encoded).unwrap(),
            Message::notification(
                "log",
                Some(json!({"text": "first\nsecond\rthird\u{2028}fourth"}))
            )
        );
    }

    #[test]
    fn a_line_holding_two_messages_is_refused() {
        let two = format!(
            "{} {}",
            Message::notification("a", None).encode().unwrap(),
            Message::notification("b", None).encode().unwrap()
        );
        assert_eq!(desync(&two).detail(), "non_json_line");
    }

    #[test]
    fn a_truncated_message_is_refused() {
        assert_eq!(
            desync(r#"{"jsonrpc":"2.0","method":"a","params":{"x":"#).detail(),
            "non_json_line"
        );
    }

    #[test]
    fn non_protocol_output_is_refused() {
        for line in [
            "Server listening on stdio",
            "",
            "  ",
            "null",
            "[1,2,3]",
            "\u{feff}{\"jsonrpc\":\"2.0\",\"method\":\"a\"}",
        ] {
            assert_eq!(desync(line).detail(), "non_json_line", "for {line:?}");
        }
    }

    #[test]
    fn json_that_is_not_json_rpc_is_refused_distinctly() {
        for line in [
            r#"{"method":"a"}"#,
            r#"{"jsonrpc":"1.0","method":"a"}"#,
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":null}"#,
            r#"{"jsonrpc":"2.0","id":1.5}"#,
            r#"{"jsonrpc":"2.0","id":true}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"m"}}"#,
        ] {
            assert_eq!(desync(line).detail(), "not_json_rpc", "for {line:?}");
        }
    }

    /// An extension member is a protocol question, and this layer answers
    /// framing questions. Quarantining a connection over one would make the
    /// engine a participant in a negotiation it cannot see.
    #[test]
    fn an_unknown_member_does_not_desynchronize_a_connection() {
        assert_eq!(
            Message::decode(r#"{"jsonrpc":"2.0","method":"a","_meta":{"x":1}}"#).unwrap(),
            Message::notification("a", None)
        );
    }

    /// Arbitrary bytes are the normal case for a peer that crashed part-way
    /// through a write, and every one of them has to be a typed error rather
    /// than a panic on the reader thread.
    #[test]
    fn arbitrary_input_is_always_a_typed_error_or_a_message() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..4096 {
            let mut line = String::new();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = (state % 40) as usize;
            for index in 0..length {
                let byte = ((state >> (index % 56)) & 0x7f) as u8;
                line.push(if byte < 0x20 { '{' } else { byte as char });
            }
            match Message::decode(&line) {
                Ok(_) | Err(TransportError::Desynchronized { .. }) => {}
                other => panic!("unexpected outcome for {line:?}: {other:?}"),
            }
        }
    }
}
