use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use uuid::Uuid;

use super::ScenarioId;

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
/// The cursor says which scenario observation comes next. The digest commits
/// to every observation already consumed without retaining those observations
/// (which may contain workspace data). It is a chained SHA-256 digest, so a
/// resumed mock can continue from this state without recovering prior input.
///
/// Deserialization probes `schema_version` before parsing the strict body. A
/// newer checkpoint therefore requests an upgrade instead of being reported as
/// malformed current data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionState {
    schema_version: u32,
    session_id: AgentSessionId,
    scenario_id: ScenarioId,
    cursor: u32,
    observation_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionStateWire {
    schema_version: u32,
    session_id: AgentSessionId,
    scenario_id: ScenarioId,
    cursor: u32,
    observation_digest: String,
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
        scenario_id: ScenarioId,
        cursor: u32,
        observation_digest: String,
    ) -> Self {
        debug_assert!(is_sha256_hex(&observation_digest));
        Self {
            schema_version: AGENT_SESSION_STATE_SCHEMA_VERSION,
            session_id,
            scenario_id,
            cursor,
            observation_digest,
        }
    }

    fn from_wire(wire: AgentSessionStateWire) -> Result<Self, &'static str> {
        if !is_sha256_hex(&wire.observation_digest) {
            return Err("observation_digest must be 64 lowercase hexadecimal characters");
        }
        Ok(Self {
            schema_version: wire.schema_version,
            session_id: wire.session_id,
            scenario_id: wire.scenario_id,
            cursor: wire.cursor,
            observation_digest: wire.observation_digest,
        })
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

    /// Scenario the mock session is replaying.
    #[must_use]
    pub const fn scenario_id(&self) -> &ScenarioId {
        &self.scenario_id
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

#[cfg(test)]
mod tests {
    use super::{AGENT_SESSION_STATE_SCHEMA_VERSION, AgentSessionId, AgentSessionState};
    use crate::agent::ScenarioId;

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
            ScenarioId::new("read_only_success").unwrap(),
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
            ScenarioId::new("read_only_success").unwrap(),
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
}
