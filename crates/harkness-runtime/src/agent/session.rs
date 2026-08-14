use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use uuid::Uuid;

/// Schema version of a persisted [`AgentSessionState`].
pub const AGENT_SESSION_STATE_SCHEMA_VERSION: u32 = 1;

/// Stable identity of one agent session within a run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentSessionId(Uuid);

impl AgentSessionId {
    /// Generates a fresh session identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for AgentSessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Serializable checkpoint for one agent session.
///
/// The scenario version and definition digest bind recovery to the exact
/// frozen script, while the cursor says which expected observation comes next.
/// The history digest commits to every observation already consumed without
/// retaining those observations (which may contain workspace data). It is a
/// chained SHA-256 digest, so a resumed mock can continue from this state
/// without recovering prior input.
///
/// Deserialization probes `schema_version` before parsing the strict body. A
/// newer checkpoint therefore requests an upgrade instead of being reported as
/// malformed current data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionState {
    schema_version: u32,
    session_id: AgentSessionId,
    scenario_version: u32,
    scenario_definition_digest: String,
    cursor: u32,
    observation_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionStateWire {
    schema_version: u32,
    session_id: AgentSessionId,
    scenario_version: u32,
    scenario_definition_digest: String,
    cursor: u32,
    observation_digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionEventPayload {
    schema_version: u32,
    session_id_bytes: Vec<u8>,
    scenario_version: u32,
    scenario_definition_digest_bytes: Vec<u8>,
    cursor: u32,
    observation_digest_bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for AgentSessionState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let version = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                D::Error::custom("agent session state is missing numeric schema_version")
            })?;
        if version > u64::from(AGENT_SESSION_STATE_SCHEMA_VERSION) {
            return Err(D::Error::custom(format!(
                "agent session state schema {version} is newer than supported schema {AGENT_SESSION_STATE_SCHEMA_VERSION}; upgrade Harkness"
            )));
        }
        if version != u64::from(AGENT_SESSION_STATE_SCHEMA_VERSION) {
            return Err(D::Error::custom(format!(
                "agent session state schema {version} is not supported"
            )));
        }

        let wire: AgentSessionStateWire =
            serde_json::from_value(value).map_err(D::Error::custom)?;
        Self::from_wire(wire).map_err(D::Error::custom)
    }
}

impl AgentSessionState {
    pub(super) fn new(
        session_id: AgentSessionId,
        scenario_version: u32,
        scenario_definition_digest: String,
        cursor: u32,
        observation_digest: String,
    ) -> Self {
        debug_assert!(is_sha256_hex(&scenario_definition_digest));
        debug_assert!(is_sha256_hex(&observation_digest));
        Self {
            schema_version: AGENT_SESSION_STATE_SCHEMA_VERSION,
            session_id,
            scenario_version,
            scenario_definition_digest,
            cursor,
            observation_digest,
        }
    }

    fn from_wire(wire: AgentSessionStateWire) -> Result<Self, &'static str> {
        if !is_sha256_hex(&wire.scenario_definition_digest) {
            return Err("scenario_definition_digest must be 64 lowercase hexadecimal characters");
        }
        if !is_sha256_hex(&wire.observation_digest) {
            return Err("observation_digest must be 64 lowercase hexadecimal characters");
        }
        Ok(Self {
            schema_version: wire.schema_version,
            session_id: wire.session_id,
            scenario_version: wire.scenario_version,
            scenario_definition_digest: wire.scenario_definition_digest,
            cursor: wire.cursor,
            observation_digest: wire.observation_digest,
        })
    }

    /// Encodes this checkpoint for a generic redacting run-event payload.
    ///
    /// Event redaction deliberately rewrites every JSON string value. These are
    /// machine-control fields rather than content, so this form stores UUID and
    /// digest bytes as numeric arrays. The caller-controlled scenario id is not
    /// persisted: the exact definition digest already commits to it and is the
    /// identity recovery resolves. The payload therefore survives any
    /// conforming text redactor without hiding caller text from that redactor.
    #[must_use]
    pub fn to_event_payload(&self) -> Value {
        let payload = AgentSessionEventPayload {
            schema_version: self.schema_version,
            session_id_bytes: self.session_id.0.as_bytes().to_vec(),
            scenario_version: self.scenario_version,
            scenario_definition_digest_bytes: decode_sha256_hex(&self.scenario_definition_digest)
                .expect("AgentSessionState guarantees a canonical definition digest")
                .to_vec(),
            cursor: self.cursor,
            observation_digest_bytes: decode_sha256_hex(&self.observation_digest)
                .expect("AgentSessionState guarantees a canonical observation digest")
                .to_vec(),
        };
        serde_json::to_value(payload)
            .expect("agent session event payload contains only infallibly serializable values")
    }

    /// Decodes a checkpoint previously produced by [`Self::to_event_payload`].
    ///
    /// # Errors
    ///
    /// Returns a JSON decoding error for a missing, future, or malformed schema
    /// version, an unknown field, or noncanonical control bytes.
    pub fn from_event_payload(value: Value) -> Result<Self, serde_json::Error> {
        let version = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                serde_json::Error::custom(
                    "agent session event payload is missing numeric schema_version",
                )
            })?;
        if version > u64::from(AGENT_SESSION_STATE_SCHEMA_VERSION) {
            return Err(serde_json::Error::custom(format!(
                "agent session event payload schema {version} is newer than supported schema {AGENT_SESSION_STATE_SCHEMA_VERSION}; upgrade Harkness"
            )));
        }
        if version != u64::from(AGENT_SESSION_STATE_SCHEMA_VERSION) {
            return Err(serde_json::Error::custom(format!(
                "agent session event payload schema {version} is not supported"
            )));
        }

        let wire: AgentSessionEventPayload = serde_json::from_value(value)?;
        let session_id = <[u8; 16]>::try_from(wire.session_id_bytes)
            .map(Uuid::from_bytes)
            .map(AgentSessionId)
            .map_err(|_| {
                serde_json::Error::custom("session_id_bytes must contain exactly 16 bytes")
            })?;
        let scenario_definition_digest =
            <[u8; 32]>::try_from(wire.scenario_definition_digest_bytes)
                .map(encode_sha256_hex)
                .map_err(|_| {
                    serde_json::Error::custom(
                        "scenario_definition_digest_bytes must contain exactly 32 bytes",
                    )
                })?;
        let observation_digest = <[u8; 32]>::try_from(wire.observation_digest_bytes)
            .map(encode_sha256_hex)
            .map_err(|_| {
                serde_json::Error::custom("observation_digest_bytes must contain exactly 32 bytes")
            })?;
        Self::from_wire(AgentSessionStateWire {
            schema_version: wire.schema_version,
            session_id,
            scenario_version: wire.scenario_version,
            scenario_definition_digest,
            cursor: wire.cursor,
            observation_digest,
        })
        .map_err(serde_json::Error::custom)
    }

    /// Version of this checkpoint wire record.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Session this checkpoint describes.
    #[must_use]
    pub const fn session_id(&self) -> AgentSessionId {
        self.session_id
    }

    /// Exact frozen fixture version the checkpoint was executing.
    #[must_use]
    pub const fn scenario_version(&self) -> u32 {
        self.scenario_version
    }

    /// SHA-256 of the exact versioned scenario definition.
    #[must_use]
    pub fn scenario_definition_digest(&self) -> &str {
        &self.scenario_definition_digest
    }

    /// Number of expected observations already consumed.
    #[must_use]
    pub const fn cursor(&self) -> u32 {
        self.cursor
    }

    /// Chained SHA-256 digest of the consumed observation history.
    #[must_use]
    pub fn observation_digest(&self) -> &str {
        &self.observation_digest
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_sha256_hex(value: &str) -> Option<[u8; 32]> {
    if !is_sha256_hex(value) {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        digest[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(digest)
}

fn encode_sha256_hex(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{AGENT_SESSION_STATE_SCHEMA_VERSION, AgentSessionId, AgentSessionState};
    #[test]
    fn session_ids_follow_the_shared_uuid_contract() {
        let spelling = "123e4567-e89b-42d3-a456-426614174000";
        let id: AgentSessionId = spelling.parse().unwrap();
        assert_eq!(id.to_string(), spelling);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            format!("\"{spelling}\"")
        );
        assert_ne!(AgentSessionId::new(), AgentSessionId::new());
    }

    #[test]
    fn state_round_trips_and_probes_its_version_first() {
        let state = AgentSessionState::new(
            AgentSessionId::new(),
            1,
            "cd".repeat(32),
            2,
            "ab".repeat(32),
        );
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["schema_version"], AGENT_SESSION_STATE_SCHEMA_VERSION);
        assert_eq!(
            serde_json::from_value::<AgentSessionState>(value.clone()).unwrap(),
            state
        );

        let mut future = value;
        future["schema_version"] = serde_json::json!(99);
        future["future_field"] = serde_json::json!({"shape": "unknown"});
        let error = serde_json::from_value::<AgentSessionState>(future).unwrap_err();
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn state_rejects_noncanonical_digests_and_current_unknown_fields() {
        let state = AgentSessionState::new(
            AgentSessionId::new(),
            1,
            "cd".repeat(32),
            0,
            "00".repeat(32),
        );
        let mut invalid = serde_json::to_value(&state).unwrap();
        invalid["observation_digest"] = serde_json::json!("AA");
        assert!(
            serde_json::from_value::<AgentSessionState>(invalid)
                .unwrap_err()
                .to_string()
                .contains("64 lowercase hexadecimal")
        );

        let mut unknown = serde_json::to_value(&state).unwrap();
        unknown["extra"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<AgentSessionState>(unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn event_payloads_hold_no_redactor_visible_strings() {
        let state = AgentSessionState::new(
            AgentSessionId::new(),
            1,
            "cd".repeat(32),
            2,
            "ab".repeat(32),
        );
        let payload = state.to_event_payload();
        assert!(!contains_string(&payload));
        assert_eq!(
            AgentSessionState::from_event_payload(payload).unwrap(),
            state
        );
    }

    #[test]
    fn session_event_payload_matches_the_frozen_wire_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/wire-contract-v1.json")).unwrap();
        let state: AgentSessionState = serde_json::from_value(fixture["state"].clone()).unwrap();
        assert_eq!(
            state.to_event_payload(),
            fixture["event_payloads"]["session"]
        );
    }

    fn contains_string(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(_) => true,
            serde_json::Value::Array(values) => values.iter().any(contains_string),
            serde_json::Value::Object(fields) => fields.values().any(contains_string),
            _ => false,
        }
    }
}
