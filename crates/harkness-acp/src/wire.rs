//! The one module in the workspace that names `agent-client-protocol-schema`.
//!
//! ADR-0010 adopts the official schema artifacts rather than hand-rolled serde,
//! and ADR-0009 keeps every type they define private to this crate. Both rules
//! are cheaper to hold if there is exactly one file to look at: nothing outside
//! this module imports `agent_client_protocol_schema`, so "does a wire type
//! escape" is answered by reading one `use` list rather than by trusting a
//! convention.
//!
//! The types here are also the reason the crate has no hand-written camel-case
//! spellings, no `#[serde(rename)]`, and no opinion about which fields are
//! optional. Upstream is the specification's own account of what the bytes mean,
//! and disagreeing with it silently is the failure ADR-0010 exists to prevent.
//!
//! # What upstream decides, and what that costs
//!
//! Every optional field on an `initialize` response is `#[serde(default)]` *and*
//! `DefaultOnError`, so a field that is absent and a field whose value has the
//! wrong type both decode to the unsupported value. That is stricter than it
//! sounds and exactly what ACP requires — an omitted capability MUST be treated
//! as unsupported — but it means a malformed capability object is not a decode
//! failure. `protocolVersion` is the one field with no default, which is why a
//! response missing it is the [`AcpError::MalformedResponse`] this crate reports
//! and everything else is a capability that reads as unsupported.
//!
//! [`AcpError::MalformedResponse`]: crate::AcpError::MalformedResponse

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub(crate) use agent_client_protocol_schema::{
    ProtocolVersion,
    v1::{
        AGENT_METHOD_NAMES, AuthenticateRequest, ClientCapabilities, FileSystemCapabilities,
        Implementation, InitializeRequest, InitializeResponse,
    },
};

use crate::{
    OFFERED_PROTOCOL_VERSION,
    capabilities::{AdvertisedClientCapabilities, ClientIdentity},
    error::AcpError,
};

/// The `initialize` method name, taken from upstream rather than typed here.
pub(crate) const INITIALIZE: &str = AGENT_METHOD_NAMES.initialize;

/// The `authenticate` method name, taken from upstream rather than typed here.
pub(crate) const AUTHENTICATE: &str = AGENT_METHOD_NAMES.authenticate;

/// ACP's reserved code for "authenticate first".
///
/// Written as a literal because it is compared against a `PeerError::code`,
/// which is an `i64` the transport read off the wire, while upstream models a
/// code as an `i32` enum with no `const` conversion. The test below is what
/// keeps the two in step.
pub(crate) const AUTH_REQUIRED_CODE: i64 = -32000;

/// JSON-RPC's code for a method the peer does not implement.
pub(crate) const METHOD_NOT_FOUND_CODE: i64 = -32601;

/// Builds the `initialize` request for one client identity and advertisement.
///
/// The capability advertisement is *input*: exactly the three flags the caller
/// passed are sent, and this crate never turns one on. #153 is the single
/// authority for what Harkness promises to mediate, and an adapter that
/// advertised `fs/write_text_file` on its own initiative would be promising
/// mediation nobody implemented.
pub(crate) fn initialize_request(
    client: &ClientIdentity,
    capabilities: &AdvertisedClientCapabilities,
) -> InitializeRequest {
    // Harkness's own constant rather than upstream's `LATEST`. The two agree
    // today and a test below says so, but a test is not what puts the number on
    // the wire: an upstream release moving `LATEST` would otherwise make a
    // release build offer a version `SUPPORTED_PROTOCOL_VERSIONS` refuses, so
    // every conformant agent that honoured the offer would be turned away for a
    // version Harkness itself proposed.
    InitializeRequest::new(ProtocolVersion::from(OFFERED_PROTOCOL_VERSION))
        .client_capabilities(
            ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(capabilities.fs_read_text_file)
                    .write_text_file(capabilities.fs_write_text_file))
                .terminal(capabilities.terminal),
        )
        .client_info(
            Implementation::new(client.name.clone(), client.version.clone())
                .title(client.title.clone()),
        )
}

/// Encodes an outbound request body.
///
/// `serde_json::to_value` fails on a non-finite float, a map key that is not a
/// string, and a `Serialize` implementation that raises — none of which the
/// request types above can hold, since every field of every one of them is a
/// `String`, a `bool`, an integer, or a `Vec` of those. The guard is kept
/// anyway, because the alternative is an `expect` in a production path and
/// because the types are governed upstream: a future release may add a field
/// this reasoning does not cover, and a typed refusal is a better way to find
/// that out than a panic in a user's session.
pub(crate) fn encode<T: Serialize>(method: &'static str, body: &T) -> Result<Value, AcpError> {
    serde_json::to_value(body).map_err(|error| AcpError::UnencodableRequest {
        method,
        detail: error.to_string(),
    })
}

/// Decodes one response body, naming the field that was wrong.
///
/// `serde_path_to_error` is what turns "invalid type: string" into a sentence
/// that names `protocolVersion`, and a decode failure is the caller's evidence
/// that the peer is not an ACP agent rather than a message to squint at.
pub(crate) fn decode<T: DeserializeOwned>(
    method: &'static str,
    body: Value,
) -> Result<T, AcpError> {
    serde_path_to_error::deserialize(body).map_err(|error| AcpError::MalformedResponse {
        method,
        detail: format!("{}: {}", error.path(), error.inner()),
    })
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::ErrorCode;

    use super::{
        AUTH_REQUIRED_CODE, AUTHENTICATE, INITIALIZE, METHOD_NOT_FOUND_CODE, ProtocolVersion,
    };
    use crate::{
        OFFERED_PROTOCOL_VERSION,
        capabilities::{AdvertisedClientCapabilities, ClientIdentity},
    };

    /// The two codes this crate branches on are upstream's, spelled locally so
    /// they can be compared against a wire `i64`. A release that renumbered
    /// either would otherwise change what Harkness reports without changing a
    /// line here.
    #[test]
    fn the_branched_error_codes_are_the_ones_upstream_defines() {
        assert_eq!(
            AUTH_REQUIRED_CODE,
            i64::from(i32::from(ErrorCode::AuthRequired))
        );
        assert_eq!(
            METHOD_NOT_FOUND_CODE,
            i64::from(i32::from(ErrorCode::MethodNotFound))
        );
    }

    /// ADR-0014 says the client offers the latest version it supports, and
    /// upstream's `LATEST` is what that phrase resolves to today. This is a
    /// notice rather than a guard — the wire carries `OFFERED_PROTOCOL_VERSION`
    /// either way, so a schema release moving `LATEST` fails here and changes
    /// nothing a user sees, which is the order those two things should happen
    /// in. Adopting the newer version is then a decision with an ADR behind it.
    #[test]
    fn the_offered_version_is_still_the_latest_upstream_calls_stable() {
        assert_eq!(ProtocolVersion::LATEST.as_u16(), OFFERED_PROTOCOL_VERSION);
    }

    /// And what actually goes on the wire is the crate's own constant.
    #[test]
    fn the_request_carries_the_version_this_crate_publishes() {
        let request = super::initialize_request(
            &ClientIdentity::new("harkness", "0.1.0"),
            &AdvertisedClientCapabilities::default(),
        );
        assert_eq!(request.protocol_version.as_u16(), OFFERED_PROTOCOL_VERSION);
    }

    /// Method names come from upstream so a rename is a compile error rather
    /// than a request no agent answers.
    #[test]
    fn the_handshake_method_names_are_the_specification_spellings() {
        assert_eq!(INITIALIZE, "initialize");
        assert_eq!(AUTHENTICATE, "authenticate");
    }
}
