use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use super::error::{IntegrationDomainError, invalid_identity};

/// Longest an identity text field may be, in bytes.
///
/// Every field bounded by this becomes a `runtime.db` column under
/// [#86](https://github.com/fullstacktaiye/harkness/issues/86), and the store
/// refuses an oversized value on write *and* on load. A subject that could
/// register with a name too long to persist would be trustable and
/// unrecordable at the same time, so the bound belongs here rather than at the
/// column.
pub const MAX_IDENTITY_FIELD_LENGTH: usize = 512;

/// Longest an executable path may be, in bytes.
pub const MAX_EXECUTABLE_PATH_LENGTH: usize = 4096;

/// Most declared capabilities one identity may carry.
///
/// The capability set is the one collection in an identity basis, and
/// [`InvalidationReason::CapabilityExpansion`](super::InvalidationReason::CapabilityExpansion)
/// is decided by subset comparison over it, so it cannot be reduced to a single
/// fingerprint. Bounding the count keeps the record as bounded as every other
/// field claims to be.
pub const MAX_CAPABILITIES: usize = 64;

/// A kind of externally controlled thing a user can decide to trust.
///
/// A forge *host* is deliberately absent: it is part of the identity basis of
/// the account and repository that reach it, so a remote repointed at another
/// host invalidates those grants rather than needing a subject of its own.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubjectKind {
    /// The executable an ACP agent is launched from.
    AgentExecutable,
    /// One configured MCP server.
    McpServer,
    /// One tool schema published by an MCP server.
    McpToolSchema,
    /// One workflow recipe.
    Recipe,
    /// One forge account acting on the user's behalf.
    ForgeAccount,
    /// One forge repository reached through a canonical remote.
    ForgeRepository,
    /// One workspace whose code the user accepts running.
    Workspace,
}

impl SubjectKind {
    /// Every subject kind in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::AgentExecutable,
        Self::McpServer,
        Self::McpToolSchema,
        Self::Recipe,
        Self::ForgeAccount,
        Self::ForgeRepository,
        Self::Workspace,
    ];

    /// Returns the stable persisted spelling of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentExecutable => "agent_executable",
            Self::McpServer => "mcp_server",
            Self::McpToolSchema => "mcp_tool_schema",
            Self::Recipe => "recipe",
            Self::ForgeAccount => "forge_account",
            Self::ForgeRepository => "forge_repository",
            Self::Workspace => "workspace",
        }
    }

    /// Parses the stable persisted spelling of this kind.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

impl fmt::Display for SubjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where the configuration that introduced a subject came from.
///
/// The variant is part of the identity basis rather than metadata beside it:
/// who may edit a subject's configuration is part of what a user agreed to.
/// Under ADR-0006 repository content is untrusted, so
/// [`Repository`](Self::Repository) is structurally distinguishable and never
/// auto-enables execution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigurationSource {
    /// Shipped with Harkness.
    Builtin,
    /// Written by the user in their own configuration.
    User,
    /// Provided by the checked-out repository.
    Repository,
    /// Adopted from another tool's configuration.
    Imported,
}

impl ConfigurationSource {
    /// Every configuration source in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Builtin, Self::User, Self::Repository, Self::Imported];

    /// Returns the stable persisted spelling of this source.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Repository => "repository",
            Self::Imported => "imported",
        }
    }

    /// Parses the stable persisted spelling of this source.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == value)
    }
}

impl fmt::Display for ConfigurationSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A SHA-256 digest identifying content a trust grant is bound to.
///
/// The wire spelling is 64 lowercase hexadecimal characters. Uppercase is
/// refused rather than folded, because these values are compared as text once
/// they reach a database column and two spellings of one digest would mean a
/// grant could fail to match the subject it was granted for.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Hash([u8; 32]);

impl Sha256Hash {
    /// Length of the hexadecimal spelling.
    const HEX_LENGTH: usize = 64;

    /// Hashes `bytes` and returns the digest.
    ///
    /// Present so a caller observing a subject — reading an executable,
    /// rendering a tool schema, loading a recipe — never has to reach for a
    /// hasher of its own and pick a different one.
    #[must_use]
    pub fn of(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self(hasher.finalize().into())
    }

    /// The raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The lowercase hexadecimal spelling.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut hex = String::with_capacity(Self::HEX_LENGTH);
        for byte in self.0 {
            fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"))
                .expect("writing to a String cannot fail");
        }
        hex
    }

    /// Parses the lowercase hexadecimal spelling.
    ///
    /// The value is read as bytes and the `&str` is never sliced: a length
    /// check counts bytes while `str` indexing counts characters, so a 64-byte
    /// value carrying a multi-byte character would put a slice boundary inside
    /// one and panic. Every caller here is handling something from outside the
    /// process, and a panic is not a refusal.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::MalformedDigest`] when the value is
    /// not exactly 64 lowercase hexadecimal characters.
    pub fn parse(value: &str) -> Result<Self, IntegrationDomainError> {
        let malformed = |reason| IntegrationDomainError::MalformedDigest {
            value: value.to_owned(),
            reason,
        };
        let spelling = value.as_bytes();
        if spelling.len() != Self::HEX_LENGTH {
            return Err(malformed("it is not 64 hexadecimal characters long"));
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(spelling.chunks_exact(2)) {
            for digit in pair {
                let nibble = match digit {
                    b'0'..=b'9' => digit - b'0',
                    b'a'..=b'f' => digit - b'a' + 10,
                    _ => return Err(malformed("it is not spelled in lowercase hexadecimal")),
                };
                *byte = (*byte << 4) | nibble;
            }
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Sha256Hash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Sha256Hash {
    type Err = IntegrationDomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for Sha256Hash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256Hash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

/// The identity of a local subprocess subject: where it lives, and what it is.
///
/// Only [`sha256`](Self::sha256) takes part in the invalidation check. The path
/// is recorded because a user asked to re-grant trust needs to be told *which*
/// installation changed, but ADR-0016 fixes that trust never binds to a mutable
/// path: an identical binary reached through another path is the same program,
/// and a different binary at the same path is not.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ExecutableIdentity {
    path: PathBuf,
    sha256: Sha256Hash,
}

impl ExecutableIdentity {
    /// Records an executable's path and content digest.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when the path is
    /// empty, relative, or longer than [`MAX_EXECUTABLE_PATH_LENGTH`]. A
    /// relative path is refused because it names a different file from every
    /// working directory, which is not an identity.
    pub fn new(
        path: impl Into<PathBuf>,
        sha256: Sha256Hash,
    ) -> Result<Self, IntegrationDomainError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(invalid_identity("executable.path", "it cannot be empty"));
        }
        if !path.is_absolute() {
            return Err(invalid_identity(
                "executable.path",
                "it must be an absolute path",
            ));
        }
        if path.as_os_str().len() > MAX_EXECUTABLE_PATH_LENGTH {
            return Err(invalid_identity(
                "executable.path",
                "it is longer than the maximum executable path length",
            ));
        }
        Ok(Self { path, sha256 })
    }

    /// Path the executable was observed at.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Digest of the executable's content.
    #[must_use]
    pub const fn sha256(&self) -> Sha256Hash {
        self.sha256
    }
}

impl<'de> Deserialize<'de> for ExecutableIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Strict {
            path: PathBuf,
            sha256: Sha256Hash,
        }

        let strict = Strict::deserialize(deserializer)?;
        Self::new(strict.path, strict.sha256).map_err(de::Error::custom)
    }
}

/// The identity of a remote subject: the host it is reached at, and what on it.
///
/// The split is what lets one field answer two invalidation questions. A
/// changed [`host`](Self::host) is
/// [`EndpointHostChanged`](super::InvalidationReason::EndpointHostChanged) —
/// the strongest form of "this is somewhere else" — while a changed
/// [`resource`](Self::resource) at the same host is
/// [`RepositoryRemoteChanged`](super::InvalidationReason::RepositoryRemoteChanged).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EndpointIdentity {
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource: Option<String>,
}

impl EndpointIdentity {
    /// Records a host and, where one applies, the resource on it.
    ///
    /// The host is lowercased because DNS names are case-insensitive: two
    /// spellings of one host must not compare as two hosts. The resource is
    /// kept verbatim, so a caller identifying a forge repository passes the
    /// canonical `github.com/{owner}/{repo}` form `normalize_remote` produces
    /// rather than whatever the remote happened to be typed as.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when either value is
    /// empty, surrounded by whitespace, carries a control character, or exceeds
    /// [`MAX_IDENTITY_FIELD_LENGTH`].
    pub fn new(
        host: impl Into<String>,
        resource: Option<String>,
    ) -> Result<Self, IntegrationDomainError> {
        let host = host.into();
        validate_text("endpoint.host", &host)?;
        if let Some(resource) = resource.as_deref() {
            validate_text("endpoint.resource", resource)?;
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            resource,
        })
    }

    /// Lowercased host the subject is reached at, including any port.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Canonical resource on that host, where the subject names one.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }
}

impl<'de> Deserialize<'de> for EndpointIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Strict {
            host: String,
            #[serde(default)]
            resource: Option<String>,
        }

        let strict = Strict::deserialize(deserializer)?;
        Self::new(strict.host, strict.resource).map_err(de::Error::custom)
    }
}

/// The exact facts a trust grant is bound to.
///
/// A basis is built by whoever observed the subject — an adapter reports a
/// path, a hash, a version, a fingerprint as plain data — and the runtime
/// records it. Two fields are deliberately *not* compared by
/// [`TrustRecord::check`](super::TrustRecord::check):
///
/// - [`display_name`](Self::display_name), because a name is presentation and
///   ADR-0016 fixes that trust never binds to one.
/// - [`ExecutableIdentity::path`], for the reason given on that type.
///
/// Everything else takes part, and an expected field the observation lacks
/// invalidates rather than passing by absence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IdentityBasis {
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executable: Option<ExecutableIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<EndpointIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_fingerprint: Option<Sha256Hash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<Sha256Hash>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    capabilities: BTreeSet<String>,
    configuration_source: ConfigurationSource,
}

impl IdentityBasis {
    /// Begins a basis from the two facts every subject has.
    ///
    /// The remaining fields are attached by the builder methods below. A setter
    /// taking raw text is fallible because it has a grammar to enforce; a
    /// setter taking an already-validated type is not.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when the display
    /// name is empty, surrounded by whitespace, carries a control character, or
    /// exceeds [`MAX_IDENTITY_FIELD_LENGTH`].
    pub fn new(
        display_name: impl Into<String>,
        configuration_source: ConfigurationSource,
    ) -> Result<Self, IntegrationDomainError> {
        let display_name = display_name.into();
        validate_text("display_name", &display_name)?;
        Ok(Self {
            display_name,
            subject_version: None,
            protocol_version: None,
            executable: None,
            endpoint: None,
            schema_fingerprint: None,
            content_hash: None,
            capabilities: BTreeSet::new(),
            configuration_source,
        })
    }

    /// Records the version the subject reports for itself.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when the value
    /// violates the shared identity text grammar.
    pub fn versioned(mut self, version: impl Into<String>) -> Result<Self, IntegrationDomainError> {
        let version = version.into();
        validate_text("subject_version", &version)?;
        self.subject_version = Some(version);
        Ok(self)
    }

    /// Records the protocol revision the subject speaks.
    ///
    /// The value stays an opaque string: ADR-0013 and ADR-0014 pin which
    /// revisions Harkness targets, and interpreting one is an adapter's job.
    /// What matters here is that changing it is an identity change.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when the value
    /// violates the shared identity text grammar.
    pub fn speaking(mut self, protocol: impl Into<String>) -> Result<Self, IntegrationDomainError> {
        let protocol = protocol.into();
        validate_text("protocol_version", &protocol)?;
        self.protocol_version = Some(protocol);
        Ok(self)
    }

    /// Records the executable a local subprocess subject is launched from.
    #[must_use]
    pub fn launched_from(mut self, executable: ExecutableIdentity) -> Self {
        self.executable = Some(executable);
        self
    }

    /// Records the endpoint a remote subject is reached at.
    #[must_use]
    pub fn reached_at(mut self, endpoint: EndpointIdentity) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// Records the fingerprint of a published tool schema.
    #[must_use]
    pub const fn fingerprinted(mut self, schema_fingerprint: Sha256Hash) -> Self {
        self.schema_fingerprint = Some(schema_fingerprint);
        self
    }

    /// Records the content digest of a recipe.
    #[must_use]
    pub const fn hashing(mut self, content_hash: Sha256Hash) -> Self {
        self.content_hash = Some(content_hash);
        self
    }

    /// Records the capabilities the subject declares.
    ///
    /// These are the *subject's* declared capabilities — an MCP server's
    /// advertised feature set, an agent's announced abilities — and not
    /// [`tool::Capability`](crate::tool::Capability), which names what a
    /// Harkness tool requires. The two vocabularies are owned by different
    /// parties and must not be conflated.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationDomainError::InvalidIdentity`] when a capability
    /// violates the shared identity text grammar, or when more than
    /// [`MAX_CAPABILITIES`] distinct capabilities are declared.
    pub fn declaring<I, S>(mut self, capabilities: I) -> Result<Self, IntegrationDomainError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut declared = BTreeSet::new();
        for capability in capabilities {
            let capability = capability.into();
            validate_text("capabilities", &capability)?;
            declared.insert(capability);
        }
        if declared.len() > MAX_CAPABILITIES {
            return Err(invalid_identity(
                "capabilities",
                "more capabilities are declared than an identity may carry",
            ));
        }
        self.capabilities = declared;
        Ok(self)
    }

    /// Name shown to a user being asked about this subject.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Version the subject reports for itself, when it reports one.
    #[must_use]
    pub fn subject_version(&self) -> Option<&str> {
        self.subject_version.as_deref()
    }

    /// Protocol revision the subject speaks, when one applies.
    #[must_use]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// Executable identity, for a local subprocess subject.
    #[must_use]
    pub const fn executable(&self) -> Option<&ExecutableIdentity> {
        self.executable.as_ref()
    }

    /// Endpoint identity, for a remote subject.
    #[must_use]
    pub const fn endpoint(&self) -> Option<&EndpointIdentity> {
        self.endpoint.as_ref()
    }

    /// Fingerprint of a published tool schema.
    #[must_use]
    pub const fn schema_fingerprint(&self) -> Option<Sha256Hash> {
        self.schema_fingerprint
    }

    /// Content digest of a recipe.
    #[must_use]
    pub const fn content_hash(&self) -> Option<Sha256Hash> {
        self.content_hash
    }

    /// Capabilities the subject declares, in stable sorted order.
    pub fn capabilities(&self) -> impl ExactSizeIterator<Item = &str> {
        self.capabilities.iter().map(String::as_str)
    }

    /// Where the configuration that introduced this subject came from.
    #[must_use]
    pub const fn configuration_source(&self) -> ConfigurationSource {
        self.configuration_source
    }

    pub(super) fn declares_capability(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }
}

impl<'de> Deserialize<'de> for IdentityBasis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Strict {
            display_name: String,
            #[serde(default)]
            subject_version: Option<String>,
            #[serde(default)]
            protocol_version: Option<String>,
            #[serde(default)]
            executable: Option<ExecutableIdentity>,
            #[serde(default)]
            endpoint: Option<EndpointIdentity>,
            #[serde(default)]
            schema_fingerprint: Option<Sha256Hash>,
            #[serde(default)]
            content_hash: Option<Sha256Hash>,
            #[serde(default)]
            capabilities: BTreeSet<String>,
            configuration_source: ConfigurationSource,
        }

        let strict = Strict::deserialize(deserializer)?;
        let mut basis = Self::new(strict.display_name, strict.configuration_source)
            .map_err(de::Error::custom)?;
        if let Some(version) = strict.subject_version {
            basis = basis.versioned(version).map_err(de::Error::custom)?;
        }
        if let Some(protocol) = strict.protocol_version {
            basis = basis.speaking(protocol).map_err(de::Error::custom)?;
        }
        if let Some(executable) = strict.executable {
            basis = basis.launched_from(executable);
        }
        if let Some(endpoint) = strict.endpoint {
            basis = basis.reached_at(endpoint);
        }
        if let Some(fingerprint) = strict.schema_fingerprint {
            basis = basis.fingerprinted(fingerprint);
        }
        if let Some(content_hash) = strict.content_hash {
            basis = basis.hashing(content_hash);
        }
        basis
            .declaring(strict.capabilities)
            .map_err(de::Error::custom)
    }
}

/// Applies the shared grammar every identity text field is held to.
///
/// Surrounding whitespace is refused rather than trimmed, because trimming
/// would make two spellings of one field compare equal in the record and
/// unequal everywhere the value came from.
fn validate_text(field: &'static str, value: &str) -> Result<(), IntegrationDomainError> {
    if value.is_empty() {
        return Err(invalid_identity(field, "it cannot be empty"));
    }
    if value.len() > MAX_IDENTITY_FIELD_LENGTH {
        return Err(invalid_identity(
            field,
            "it is longer than the maximum identity field length",
        ));
    }
    if value.trim() != value {
        return Err(invalid_identity(
            field,
            "it cannot begin or end with whitespace",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid_identity(
            field,
            "it cannot carry a control character",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigurationSource, EndpointIdentity, ExecutableIdentity, IdentityBasis,
        MAX_IDENTITY_FIELD_LENGTH, Sha256Hash, SubjectKind,
    };

    fn hash(seed: &str) -> Sha256Hash {
        Sha256Hash::of(seed.as_bytes())
    }

    #[test]
    fn subject_kinds_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (SubjectKind::AgentExecutable, "agent_executable"),
            (SubjectKind::McpServer, "mcp_server"),
            (SubjectKind::McpToolSchema, "mcp_tool_schema"),
            (SubjectKind::Recipe, "recipe"),
            (SubjectKind::ForgeAccount, "forge_account"),
            (SubjectKind::ForgeRepository, "forge_repository"),
            (SubjectKind::Workspace, "workspace"),
        ];

        assert_eq!(
            fixtures.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            SubjectKind::ALL
        );
        for (kind, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(kind.to_string(), spelling);
            assert_eq!(SubjectKind::from_stored(spelling), Some(kind));
            assert_eq!(serde_json::to_string(&kind).unwrap(), json);
            assert_eq!(serde_json::from_str::<SubjectKind>(&json).unwrap(), kind);
        }
        assert_eq!(SubjectKind::from_stored("AgentExecutable"), None);
    }

    #[test]
    fn configuration_sources_serialize_as_stable_snake_case_strings() {
        let fixtures = [
            (ConfigurationSource::Builtin, "builtin"),
            (ConfigurationSource::User, "user"),
            (ConfigurationSource::Repository, "repository"),
            (ConfigurationSource::Imported, "imported"),
        ];

        assert_eq!(
            fixtures
                .iter()
                .map(|(source, _)| *source)
                .collect::<Vec<_>>(),
            ConfigurationSource::ALL
        );
        for (source, spelling) in fixtures {
            let json = format!("\"{spelling}\"");
            assert_eq!(source.as_str(), spelling);
            assert_eq!(source.to_string(), spelling);
            assert_eq!(ConfigurationSource::from_stored(spelling), Some(source));
            assert_eq!(serde_json::to_string(&source).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<ConfigurationSource>(&json).unwrap(),
                source
            );
        }
    }

    #[test]
    fn digests_round_trip_through_their_lowercase_hexadecimal_spelling() {
        let digest = hash("harkness");
        let hex = digest.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(digest.to_string(), hex);
        assert_eq!(Sha256Hash::parse(&hex).unwrap(), digest);
        assert_eq!(
            serde_json::from_str::<Sha256Hash>(&format!("\"{hex}\"")).unwrap(),
            digest
        );
        assert_eq!(
            serde_json::to_string(&digest).unwrap(),
            format!("\"{hex}\"")
        );
    }

    #[test]
    fn digest_parsing_refuses_uppercase_short_and_non_hexadecimal_spellings() {
        let hex = hash("harkness").to_hex();
        for rejected in [
            hex.to_uppercase(),
            hex[..63].to_owned(),
            format!("{}g", &hex[..63]),
            // 64 bytes, but not 64 ASCII characters: parsing must count bytes
            // and must never slice the string.
            format!("{}é", &hex[..62]),
        ] {
            assert_eq!(
                Sha256Hash::parse(&rejected).unwrap_err().kind(),
                "malformed_digest",
                "accepted {rejected}"
            );
        }
    }

    #[test]
    fn an_executable_identity_requires_an_absolute_path() {
        assert!(ExecutableIdentity::new("/usr/bin/agent", hash("bin")).is_ok());
        for rejected in ["", "bin/agent", "./agent"] {
            let error = ExecutableIdentity::new(rejected, hash("bin")).unwrap_err();
            assert_eq!(error.kind(), "invalid_identity");
        }
    }

    #[test]
    fn an_executable_path_is_bounded_in_bytes() {
        let root = "/";
        let within = format!(
            "{root}{}",
            "a".repeat(super::MAX_EXECUTABLE_PATH_LENGTH - 1)
        );
        assert!(ExecutableIdentity::new(within, hash("bin")).is_ok());

        let beyond = format!("{root}{}", "a".repeat(super::MAX_EXECUTABLE_PATH_LENGTH));
        let error = ExecutableIdentity::new(beyond, hash("bin")).unwrap_err();
        assert_eq!(error.kind(), "invalid_identity");
    }

    #[test]
    fn an_endpoint_lowercases_its_host_and_keeps_its_resource_verbatim() {
        let endpoint =
            EndpointIdentity::new("GitHub.com", Some("octocat/Hello-World".to_owned())).unwrap();
        assert_eq!(endpoint.host(), "github.com");
        assert_eq!(endpoint.resource(), Some("octocat/Hello-World"));
    }

    #[test]
    fn identity_text_fields_refuse_empty_padded_control_and_oversized_values() {
        for rejected in [
            String::new(),
            " agent".to_owned(),
            "agent ".to_owned(),
            "agent\n".to_owned(),
            "a".repeat(MAX_IDENTITY_FIELD_LENGTH + 1),
        ] {
            let error =
                IdentityBasis::new(rejected.clone(), ConfigurationSource::User).unwrap_err();
            assert_eq!(error.kind(), "invalid_identity", "accepted {rejected:?}");
        }
        assert!(
            IdentityBasis::new(
                "a".repeat(MAX_IDENTITY_FIELD_LENGTH),
                ConfigurationSource::User
            )
            .is_ok()
        );
    }

    #[test]
    fn declaring_more_capabilities_than_the_bound_is_refused() {
        let within = (0..super::MAX_CAPABILITIES).map(|index| format!("capability.{index}"));
        assert!(
            IdentityBasis::new("server", ConfigurationSource::User)
                .unwrap()
                .declaring(within)
                .is_ok()
        );

        let beyond = (0..=super::MAX_CAPABILITIES).map(|index| format!("capability.{index}"));
        let error = IdentityBasis::new("server", ConfigurationSource::User)
            .unwrap()
            .declaring(beyond)
            .unwrap_err();
        assert_eq!(error.kind(), "invalid_identity");
    }

    #[test]
    fn an_identity_basis_round_trips_with_every_optional_field_present() {
        let basis = IdentityBasis::new("Example agent", ConfigurationSource::Repository)
            .unwrap()
            .versioned("1.4.2")
            .unwrap()
            .speaking("1")
            .unwrap()
            .launched_from(ExecutableIdentity::new("/usr/bin/agent", hash("bin")).unwrap())
            .reached_at(
                EndpointIdentity::new("github.com", Some("octocat/hello".to_owned())).unwrap(),
            )
            .fingerprinted(hash("schema"))
            .hashing(hash("recipe"))
            .declaring(["fs.read", "network"])
            .unwrap();

        let json = serde_json::to_string(&basis).unwrap();
        assert_eq!(serde_json::from_str::<IdentityBasis>(&json).unwrap(), basis);
    }

    #[test]
    fn absent_optional_fields_are_omitted_and_deserialize_to_absent() {
        let basis = IdentityBasis::new("Example recipe", ConfigurationSource::Builtin).unwrap();
        let json = serde_json::to_string(&basis).unwrap();
        assert_eq!(
            json,
            r#"{"display_name":"Example recipe","configuration_source":"builtin"}"#
        );
        assert_eq!(serde_json::from_str::<IdentityBasis>(&json).unwrap(), basis);
    }

    #[test]
    fn an_identity_basis_rejects_unknown_fields_and_invalid_nested_values() {
        let error = serde_json::from_str::<IdentityBasis>(
            r#"{"display_name":"a","configuration_source":"user","surprise":1}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));

        let error = serde_json::from_str::<IdentityBasis>(
            r#"{"display_name":"a","configuration_source":"user","executable":{"path":"relative","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be an absolute path"));
    }

    #[test]
    fn no_identity_field_is_named_like_credential_material() {
        let basis = IdentityBasis::new("Example agent", ConfigurationSource::User)
            .unwrap()
            .versioned("1.0.0")
            .unwrap()
            .speaking("1")
            .unwrap()
            .launched_from(ExecutableIdentity::new("/usr/bin/agent", hash("bin")).unwrap())
            .reached_at(
                EndpointIdentity::new("github.com", Some("octocat/hello".to_owned())).unwrap(),
            )
            .fingerprinted(hash("schema"))
            .hashing(hash("recipe"))
            .declaring(["fs.read"])
            .unwrap();

        let json = serde_json::to_string(&basis).unwrap();
        for forbidden in [
            "token",
            "secret",
            "password",
            "credential",
            "auth",
            "key",
            "bearer",
            "cookie",
        ] {
            assert!(
                !json.contains(forbidden),
                "identity wire form mentions {forbidden}: {json}"
            );
        }
    }
}
