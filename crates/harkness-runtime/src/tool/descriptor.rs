use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Capability, RegistryError, SchemaDirection, ToolId, ToolIdentity, ToolVersion};

/// Longest accepted descriptor title.
///
/// A title is a one-line label in a tool picker and in `harkness contract`
/// output; anything longer is a description wearing the wrong field.
pub const MAX_TITLE_LENGTH: usize = 120;

/// Longest accepted descriptor description.
pub const MAX_DESCRIPTION_LENGTH: usize = 2048;

/// Reason reported for a blank title or description.
const BLANK: &str = "it must not be blank; it is what a person reads before approving";

/// Reason reported when a title exceeds [`MAX_TITLE_LENGTH`].
///
/// A `&'static str` cannot interpolate the constant, so
/// `the_length_reasons_state_the_actual_bounds` keeps the two in agreement.
const TITLE_TOO_LONG: &str = "it is longer than 120 characters";

/// Reason reported when a description exceeds [`MAX_DESCRIPTION_LENGTH`].
const DESCRIPTION_TOO_LONG: &str = "it is longer than 2048 characters";

/// What executing a tool can affect, ordered from least to most consequential.
///
/// This is the single definition the policy engine classifies against and the
/// trust model escalates on, which is why the ordering is part of the type
/// rather than a table beside it: a comparison like `risk >= RiskLevel::Execute`
/// has to mean the same thing everywhere or a policy is only advisory.
///
/// The levels are *categories of consequence*, not a severity score. Each one
/// admits something the levels below it cannot do:
///
/// - [`Observe`](Self::Observe) — reads state and changes nothing. `fs.read`,
///   `git.status`.
/// - [`WorkspaceWrite`](Self::WorkspaceWrite) — writes inside the workspace
///   under version control, so the change is visible and revertible.
///   `fs.write`, `git.stage`.
/// - [`Execute`](Self::Execute) — runs a program, whose effects this process
///   cannot enumerate in advance. `process.run`.
/// - [`Network`](Self::Network) — contacts a remote and may disclose local
///   content to it. `http.get`, `git.fetch`.
/// - [`RemoteWrite`](Self::RemoteWrite) — changes state other people can see,
///   which no local undo reaches. `git.push`, `github.comment`.
/// - [`Destructive`](Self::Destructive) — discards work that was not recorded
///   anywhere else. `git.reset_hard`, `fs.delete`.
///
/// A tool declares its level once, in its descriptor, and the registry never
/// rewrites it. A tool therefore cannot lower its declared risk for a
/// particular call; a call that turns out to be more dangerous than its level
/// suggests — a path outside the workspace, a remote that is not the project's
/// — is caught when the invocation is evaluated, not by re-labelling the tool.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Reads state without changing anything.
    Observe,
    /// Writes inside the workspace, where the change is visible and revertible.
    WorkspaceWrite,
    /// Runs a program whose effects cannot be enumerated in advance.
    Execute,
    /// Contacts a remote and may disclose local content to it.
    Network,
    /// Changes state outside this machine that others can observe.
    RemoteWrite,
    /// Discards work that is not recorded anywhere else.
    Destructive,
}

impl RiskLevel {
    /// Every level in ascending order of consequence.
    pub const ALL: &'static [Self] = &[
        Self::Observe,
        Self::WorkspaceWrite,
        Self::Execute,
        Self::Network,
        Self::RemoteWrite,
        Self::Destructive,
    ];

    /// Returns the stable spelling used in schemas, policy, and persisted rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::WorkspaceWrite => "workspace_write",
            Self::Execute => "execute",
            Self::Network => "network",
            Self::RemoteWrite => "remote_write",
            Self::Destructive => "destructive",
        }
    }

    /// The least consequential level, and the only one that changes nothing.
    #[must_use]
    pub const fn lowest() -> Self {
        Self::Observe
    }

    /// The most consequential level.
    #[must_use]
    pub const fn highest() -> Self {
        Self::Destructive
    }

    /// Whether executing at this level can change state.
    ///
    /// Exactly one level answers `false`, which is what makes "a read-only run"
    /// a checkable claim rather than a convention.
    #[must_use]
    pub const fn mutates_state(self) -> bool {
        !matches!(self, Self::Observe)
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RiskLevel {
    type Err = UnknownRiskLevel;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|level| level.as_str() == value)
            .ok_or_else(|| UnknownRiskLevel {
                value: value.to_owned(),
            })
    }
}

/// A spelling that names no [`RiskLevel`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{value:?} is not a known risk level")]
pub struct UnknownRiskLevel {
    /// Value that matched no level.
    pub value: String,
}

/// Everything a tool declares about itself except its schemas.
///
/// Schemas are deliberately absent: they are generated from the `Input` and
/// `Output` associated types when the tool is erased, so a descriptor cannot
/// publish a schema that disagrees with the type the tool actually
/// deserializes. A tool author states the things only they know — identity,
/// wording, consequence, required capabilities — and nothing that could be
/// derived and therefore contradicted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolMetadata {
    identity: ToolIdentity,
    title: String,
    description: String,
    risk: RiskLevel,
    capabilities: Vec<Capability>,
}

impl ToolMetadata {
    /// Declares a tool with no required capabilities.
    #[must_use]
    pub fn new(
        identity: ToolIdentity,
        title: impl Into<String>,
        description: impl Into<String>,
        risk: RiskLevel,
    ) -> Self {
        Self {
            identity,
            title: title.into(),
            description: description.into(),
            risk,
            capabilities: Vec::new(),
        }
    }

    /// Adds the capabilities this tool requires.
    ///
    /// The list is sorted and deduplicated, so a declaration order cannot leak
    /// into descriptor enumeration and a repeated capability cannot be mistaken
    /// for a stronger requirement.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities.extend(capabilities);
        self.capabilities.sort_unstable();
        self.capabilities.dedup();
        self
    }

    /// Identity this tool is registered and recorded under.
    #[must_use]
    pub const fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    /// One-line human-readable label.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Explanation of what the tool does, written for whoever decides to run it.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Declared consequence of executing this tool.
    #[must_use]
    pub const fn risk(&self) -> RiskLevel {
        self.risk
    }

    /// Capabilities this tool requires, sorted and deduplicated.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Checks the fields a front end has to render.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidMetadata`] when the title or description
    /// is blank or exceeds its bound. Blank is refused rather than defaulted
    /// because an approval prompt with nothing to read is worse than a refusal
    /// at registration.
    pub(super) fn validate(&self) -> Result<(), RegistryError> {
        // Bounds are counted in characters, not bytes, so a description written
        // in a non-Latin script is not silently held to a third of the documented
        // length.
        let fields = [
            (
                "title",
                self.title.as_str(),
                MAX_TITLE_LENGTH,
                TITLE_TOO_LONG,
            ),
            (
                "description",
                self.description.as_str(),
                MAX_DESCRIPTION_LENGTH,
                DESCRIPTION_TOO_LONG,
            ),
        ];

        for (field, value, maximum, too_long) in fields {
            let reason = if value.trim().is_empty() {
                Some(BLANK)
            } else if value.chars().count() > maximum {
                Some(too_long)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(RegistryError::InvalidMetadata {
                    tool: self.identity.clone(),
                    field,
                    reason,
                });
            }
        }
        Ok(())
    }
}

/// The complete published contract of one registered tool.
///
/// A descriptor is assembled once, when the tool is registered, and never
/// mutated afterwards. That immutability is what lets a recorded tool call and
/// an approval both refer to `(id, version)` and mean the same executable
/// contract: pinning a version would be meaningless if the thing behind it
/// could be edited in place.
///
/// The serialized form is the projection `harkness contract` publishes. Field
/// order is fixed and schema objects serialize with sorted keys, so the output
/// is diff-stable across runs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolDescriptor {
    #[serde(flatten)]
    identity: ToolIdentity,
    title: String,
    description: String,
    risk: RiskLevel,
    capabilities: Vec<Capability>,
    input_schema: Value,
    output_schema: Value,
}

impl ToolDescriptor {
    /// Assembles a descriptor from validated metadata and generated schemas.
    pub(super) fn new(metadata: ToolMetadata, input_schema: Value, output_schema: Value) -> Self {
        let ToolMetadata {
            identity,
            title,
            description,
            risk,
            capabilities,
        } = metadata;
        Self {
            identity,
            title,
            description,
            risk,
            capabilities,
            input_schema,
            output_schema,
        }
    }

    /// Identity this tool is registered and recorded under.
    #[must_use]
    pub const fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    /// Stable dotted identifier.
    #[must_use]
    pub const fn id(&self) -> &ToolId {
        &self.identity.id
    }

    /// Immutable version of this tool.
    #[must_use]
    pub const fn version(&self) -> &ToolVersion {
        &self.identity.version
    }

    /// One-line human-readable label.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Explanation of what the tool does.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Declared consequence of executing this tool.
    #[must_use]
    pub const fn risk(&self) -> RiskLevel {
        self.risk
    }

    /// Capabilities this tool requires, sorted and deduplicated.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// JSON Schema generated from the tool's `Input` associated type.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// JSON Schema generated from the tool's `Output` associated type.
    #[must_use]
    pub const fn output_schema(&self) -> &Value {
        &self.output_schema
    }

    /// Returns the schema for one side of an invocation.
    #[must_use]
    pub const fn schema(&self, direction: SchemaDirection) -> &Value {
        match direction {
            SchemaDirection::Input => &self.input_schema,
            SchemaDirection::Output => &self.output_schema,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::json;

    use super::{
        MAX_DESCRIPTION_LENGTH, MAX_TITLE_LENGTH, RiskLevel, ToolDescriptor, ToolMetadata,
    };
    use crate::tool::{Capability, SchemaDirection, ToolIdentity};

    fn metadata() -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.tool", "1.0.0").unwrap(),
            "Fixture tool",
            "Echoes its input back for tests.",
            RiskLevel::Observe,
        )
    }

    #[test]
    fn risk_levels_order_from_observe_to_destructive() {
        assert_eq!(
            RiskLevel::ALL,
            &[
                RiskLevel::Observe,
                RiskLevel::WorkspaceWrite,
                RiskLevel::Execute,
                RiskLevel::Network,
                RiskLevel::RemoteWrite,
                RiskLevel::Destructive,
            ]
        );

        // Ascending and total: every level is strictly below the next, and the
        // ordering the policy engine will compare against is the declaration
        // order documented on the type.
        for pair in RiskLevel::ALL.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} is not below {:?}",
                pair[0],
                pair[1]
            );
        }
        let mut shuffled = [
            RiskLevel::Destructive,
            RiskLevel::Observe,
            RiskLevel::Network,
            RiskLevel::Execute,
            RiskLevel::RemoteWrite,
            RiskLevel::WorkspaceWrite,
        ];
        shuffled.sort_unstable();
        assert_eq!(shuffled.as_slice(), RiskLevel::ALL);

        assert_eq!(RiskLevel::lowest(), RiskLevel::Observe);
        assert_eq!(RiskLevel::highest(), RiskLevel::Destructive);
        assert_eq!(
            RiskLevel::ALL.iter().max().copied(),
            Some(RiskLevel::Destructive)
        );
    }

    #[test]
    fn only_observe_leaves_state_unchanged() {
        assert!(!RiskLevel::Observe.mutates_state());
        for level in &RiskLevel::ALL[1..] {
            assert!(level.mutates_state(), "{level} claims to change nothing");
        }
    }

    #[test]
    fn risk_levels_round_trip_through_their_stable_spellings() {
        for level in RiskLevel::ALL.iter().copied() {
            let spelling = level.as_str();
            assert_eq!(RiskLevel::from_str(spelling).unwrap(), level);
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{spelling}\"")
            );
            assert_eq!(
                serde_json::from_str::<RiskLevel>(&format!("\"{spelling}\"")).unwrap(),
                level
            );
        }
        assert!(RiskLevel::from_str("catastrophic").is_err());
    }

    #[test]
    fn capabilities_are_sorted_and_deduplicated_at_declaration() {
        let declared = metadata().with_capabilities([
            Capability::new("fs.write").unwrap(),
            Capability::new("network").unwrap(),
            Capability::new("fs.write").unwrap(),
            Capability::new("fs.read").unwrap(),
        ]);
        let spellings = declared
            .capabilities()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>();
        assert_eq!(spellings, ["fs.read", "fs.write", "network"]);
    }

    #[test]
    fn blank_or_overlong_wording_is_refused_at_declaration() {
        let identity = ToolIdentity::parse("fixture.tool", "1.0.0").unwrap();
        for (title, description, field) in
            [("   ", "fine", "title"), ("fine", "\n\t ", "description")]
        {
            let error = ToolMetadata::new(identity.clone(), title, description, RiskLevel::Observe)
                .validate()
                .unwrap_err();
            assert_eq!(error.kind(), "invalid_metadata");
            assert!(error.to_string().contains(field), "{error}");
            assert!(error.to_string().contains("must not be blank"), "{error}");
        }

        let long_title = "t".repeat(MAX_TITLE_LENGTH + 1);
        assert!(
            ToolMetadata::new(identity.clone(), long_title, "fine", RiskLevel::Observe)
                .validate()
                .unwrap_err()
                .to_string()
                .contains(super::TITLE_TOO_LONG)
        );

        let long_description = "d".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert!(
            ToolMetadata::new(identity, "fine", long_description, RiskLevel::Observe)
                .validate()
                .unwrap_err()
                .to_string()
                .contains(super::DESCRIPTION_TOO_LONG)
        );

        assert!(metadata().validate().is_ok());
    }

    #[test]
    fn the_length_reasons_state_the_actual_bounds() {
        for (reason, bound) in [
            (super::TITLE_TOO_LONG, MAX_TITLE_LENGTH),
            (super::DESCRIPTION_TOO_LONG, MAX_DESCRIPTION_LENGTH),
        ] {
            assert!(
                reason.contains(&bound.to_string()),
                "{reason} does not name {bound}"
            );
        }
    }

    #[test]
    fn wording_is_measured_in_characters_so_non_ascii_is_not_penalized() {
        let identity = ToolIdentity::parse("fixture.tool", "1.0.0").unwrap();
        // Each of these is three bytes but one character; a byte-length bound
        // would refuse a title a third the documented length.
        let title = "é".repeat(MAX_TITLE_LENGTH);
        assert!(
            ToolMetadata::new(identity, title, "fine", RiskLevel::Observe)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn the_serialized_descriptor_publishes_identity_inline_in_a_fixed_order() {
        let descriptor = ToolDescriptor::new(
            metadata().with_capabilities([Capability::new("fs.read").unwrap()]),
            json!({"type": "object"}),
            json!({"type": "string"}),
        );

        assert_eq!(
            serde_json::to_string(&descriptor).unwrap(),
            concat!(
                r#"{"id":"fixture.tool","version":"1.0.0","title":"Fixture tool","#,
                r#""description":"Echoes its input back for tests.","risk":"observe","#,
                r#""capabilities":["fs.read"],"input_schema":{"type":"object"},"#,
                r#""output_schema":{"type":"string"}}"#,
            )
        );
        assert_eq!(descriptor.id().as_str(), "fixture.tool");
        assert_eq!(descriptor.version().to_string(), "1.0.0");
        assert_eq!(
            descriptor.schema(SchemaDirection::Input),
            descriptor.input_schema()
        );
        assert_eq!(
            descriptor.schema(SchemaDirection::Output),
            descriptor.output_schema()
        );
    }
}
