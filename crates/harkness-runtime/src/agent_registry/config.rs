//! `agents.json`: the durable, hand-editable list of registered ACP agents.
//!
//! The file follows the project catalog's discipline exactly, because it is the
//! same kind of thing: a small user-data document rewritten whole under an
//! exclusive lock. `schema_version` is probed before the strict body is parsed,
//! so a file written by a newer build asks for an upgrade rather than reading as
//! corruption; unknown fields at a known version are refused rather than
//! silently dropped on the next write; and a read never rewrites anything.
//!
//! Nothing observed lives here. The executable's digest, the capability
//! snapshot, the health record and the trust grant are all `runtime.db` rows, so
//! this file stays small enough to read in a diff and safe enough to edit by
//! hand.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::integration::{ConfigurationSource, IdentityBasis, is_rooted_anywhere};
use crate::tool::MAX_ENVIRONMENT_NAME_LENGTH;

use super::error::{AgentRegistryError, invalid_registration};
use super::{AgentId, SchemaVersionProbe};

/// Name of the agent registry inside the Harkness data directory.
pub const AGENTS_FILE: &str = "agents.json";
/// Name of the stable lock inode guarding [`AGENTS_FILE`].
pub const AGENTS_LOCK_FILE: &str = "agents.lock";

/// The newest `agents.json` schema this build understands.
pub const AGENTS_SCHEMA_VERSION: u32 = 1;
/// The oldest `agents.json` schema this build can load without losing data.
pub const MINIMUM_AGENTS_SCHEMA_VERSION: u32 = 1;

/// Most registrations one `agents.json` may hold.
pub const MAX_REGISTERED_AGENTS: usize = 256;
/// Largest `agents.json` this build will read *or write*, in bytes.
///
/// Small enough that the one file Harkness parses out of an untrusted repository
/// cannot decide this process's memory, and enforced on the **write** as well —
/// which it has to be, because it is far below what the per-field bounds alone
/// permit. [`MAX_REGISTERED_AGENTS`] entries each carrying
/// [`MAX_AGENT_ARGUMENTS`] arguments of [`MAX_AGENT_ARGUMENT_LENGTH`] is about
/// seventy megabytes, so a registry built entirely of legal entries could
/// otherwise be written and then refused for good by the reader that owns it.
/// A bound checked in one direction is not a bound.
///
/// Ordinary registries are nowhere near it: a hundred agents with real commands
/// and a handful of arguments each is a few tens of kilobytes.
pub const MAX_AGENTS_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Most arguments one registration may pass to its agent.
pub const MAX_AGENT_ARGUMENTS: usize = 64;
/// Longest one argument may be, in bytes.
pub const MAX_AGENT_ARGUMENT_LENGTH: usize = 4096;
/// Most environment variables one registration may admit.
pub const MAX_ENV_ALLOWLIST_ENTRIES: usize = 64;

/// Where a registration came from.
///
/// Presentation and provenance, not authority: every entry in `agents.json` was
/// written by the user's own configuration whatever this says, which is what
/// makes adopting a repository suggestion an explicit act rather than a source
/// spelling. See [`AgentSuggestion`](super::AgentSuggestion).
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentSource {
    /// Typed or chosen by the user.
    #[default]
    User,
    /// Registered from a discovery suggestion the user accepted.
    Discovered,
    /// A local build the user is working on.
    Development,
}

impl AgentSource {
    /// Every source in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::User, Self::Discovered, Self::Development];

    /// The stable persisted spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Discovered => "discovered",
            Self::Development => "development",
        }
    }
}

impl std::fmt::Display for AgentSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One registered agent, exactly as `agents.json` holds it.
///
/// Every value here is configuration. Nothing Harkness *observed* about the
/// program — its digest, its version, what it can do, whether it answered — is a
/// field of this type; those live in
/// [`AgentRuntimeState`](super::AgentRuntimeState).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentRegistration {
    id: AgentId,
    display_name: String,
    command: PathBuf,
    args: Vec<String>,
    env_allowlist: Vec<String>,
    enabled: bool,
    source: AgentSource,
}

impl AgentRegistration {
    /// Validates one registration.
    ///
    /// A fresh registration is always **disabled**. Enabling one is a separate,
    /// explicitly trusted act
    /// ([`AgentRegistryService::set_enabled`](super::AgentRegistryService::set_enabled)),
    /// which is what makes "a suggestion cannot auto-enable" a property of every
    /// registration path rather than a rule the repository path remembers.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::InvalidRegistration`] for a display name,
    /// command, argument list or environment allowlist outside its grammar or
    /// bound.
    pub fn new(
        id: AgentId,
        display_name: impl Into<String>,
        command: impl Into<PathBuf>,
        source: AgentSource,
    ) -> Result<Self, AgentRegistryError> {
        let display_name = display_name.into();
        // Validated by building the identity basis it will become. The grammar
        // is the integration module's and re-implementing it here is how the
        // two drift: a name that registers and then cannot be trusted is a dead
        // end a user cannot get out of.
        IdentityBasis::new(display_name.clone(), ConfigurationSource::User)
            .map_err(|_| invalid_registration("display_name", DISPLAY_NAME_GRAMMAR))?;

        let command = command.into();
        if command.as_os_str().is_empty() {
            return Err(invalid_registration("command", "it cannot be empty"));
        }
        // `is_rooted_anywhere` rather than `Path::is_absolute`, for the reason
        // that function documents: `agents.json` outlives the machine that
        // wrote it, and either built-in predicate alone refuses a valid entry
        // written on the other platform. Whether *this* host can launch the
        // path is a different question, answered by `SpawnSpec` at spawn time.
        if !is_rooted_anywhere(&command) {
            return Err(invalid_registration(
                "command",
                "it must be an absolute path, so no PATH search can decide which program runs",
            ));
        }
        if command.as_os_str().len() > crate::integration::MAX_EXECUTABLE_PATH_LENGTH {
            return Err(invalid_registration(
                "command",
                "it is longer than the maximum executable path length",
            ));
        }

        Ok(Self {
            id,
            display_name,
            command,
            args: Vec::new(),
            env_allowlist: Vec::new(),
            enabled: false,
            source,
        })
    }

    /// Replaces the argument vector.
    ///
    /// There is no shell form and never will be: `argv` is a list, and a value
    /// carrying a space or a metacharacter is one argument.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::InvalidRegistration`] when there are more
    /// than [`MAX_AGENT_ARGUMENTS`] arguments, or one is empty, longer than
    /// [`MAX_AGENT_ARGUMENT_LENGTH`], or carries a NUL byte.
    pub fn with_args<I, S>(mut self, args: I) -> Result<Self, AgentRegistryError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut collected = Vec::new();
        for argument in args {
            let argument = argument.into();
            if collected.len() == MAX_AGENT_ARGUMENTS {
                return Err(invalid_registration(
                    "args",
                    "more arguments are declared than a registration may carry",
                ));
            }
            if argument.is_empty() {
                return Err(invalid_registration("args", "an argument cannot be empty"));
            }
            if argument.len() > MAX_AGENT_ARGUMENT_LENGTH {
                return Err(invalid_registration(
                    "args",
                    "an argument is longer than the maximum argument length",
                ));
            }
            if argument.contains('\0') {
                return Err(invalid_registration(
                    "args",
                    "an argument contains a NUL byte, which no process can carry",
                ));
            }
            collected.push(argument);
        }
        self.args = collected;
        Ok(self)
    }

    /// Replaces the environment allowlist.
    ///
    /// The list is **exhaustive**: the agent's process starts from an empty
    /// environment and sees exactly the named variables that are present in this
    /// process's own, and nothing else. There is deliberately no implicit
    /// baseline the way [`BASELINE_ENVIRONMENT`](crate::trust::BASELINE_ENVIRONMENT)
    /// is one for a Harkness tool: a tool is code Harkness ships and an agent is
    /// a program someone else wrote, so "which of my variables can it read" has
    /// one safe default, and it is none of them. An agent that needs `PATH` says
    /// so, in a file the user can read.
    ///
    /// Names are kept **verbatim** rather than folded to upper case the way
    /// [`EnvironmentName`](crate::tool::EnvironmentName) folds a tool's
    /// declaration. On Unix `path` and `PATH` are two variables, and folding one
    /// onto the other would pass a variable the user did not name.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::InvalidRegistration`] when there are more
    /// than [`MAX_ENV_ALLOWLIST_ENTRIES`] entries, one is not an environment
    /// identifier, or one appears twice.
    pub fn with_env_allowlist<I, S>(mut self, names: I) -> Result<Self, AgentRegistryError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut collected = Vec::new();
        let mut seen = BTreeSet::new();
        for name in names {
            let name = name.into();
            if collected.len() == MAX_ENV_ALLOWLIST_ENTRIES {
                return Err(invalid_registration(
                    "env_allowlist",
                    "more variables are admitted than a registration may carry",
                ));
            }
            validate_environment_name(&name)?;
            if !seen.insert(name.clone()) {
                return Err(invalid_registration(
                    "env_allowlist",
                    "a variable is admitted twice",
                ));
            }
            collected.push(name);
        }
        self.env_allowlist = collected;
        Ok(self)
    }

    /// The registration's identity.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        &self.id
    }

    /// Name shown to a user being asked about this agent.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Absolute path of the program that is launched.
    #[must_use]
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// Arguments passed after the program name.
    pub fn args(&self) -> impl ExactSizeIterator<Item = &str> {
        self.args.iter().map(String::as_str)
    }

    /// Environment variables this agent may see, in the order they were written.
    pub fn env_allowlist(&self) -> impl ExactSizeIterator<Item = &str> {
        self.env_allowlist.iter().map(String::as_str)
    }

    /// Whether the agent may be launched at all.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Where the registration came from.
    #[must_use]
    pub const fn source(&self) -> AgentSource {
        self.source
    }

    /// Whether two registrations describe the same configuration.
    ///
    /// `enabled` is deliberately excluded. Re-registering an agent that is
    /// already registered and already on must be the no-op the idempotency rule
    /// promises, rather than a silent switch-off — and a fresh registration is
    /// always built disabled, so comparing the flag would make every repeat
    /// registration look like a change.
    #[must_use]
    pub fn describes_same_configuration(&self, other: &Self) -> bool {
        self.id == other.id
            && self.display_name == other.display_name
            && self.command == other.command
            && self.args == other.args
            && self.env_allowlist == other.env_allowlist
            && self.source == other.source
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Restores a registration whose fields have already been validated.
    pub(super) fn from_parts(
        id: AgentId,
        display_name: String,
        command: PathBuf,
        args: Vec<String>,
        env_allowlist: Vec<String>,
        enabled: bool,
        source: AgentSource,
    ) -> Result<Self, AgentRegistryError> {
        Self::new(id, display_name, command, source)?
            .with_args(args)?
            .with_env_allowlist(env_allowlist)
            .map(|mut registration| {
                registration.enabled = enabled;
                registration
            })
    }
}

/// The stable explanation a refused display name carries.
///
/// One sentence rather than the integration module's field-specific text,
/// because a registration's `display_name` and an identity basis's are the same
/// value seen from two sides and only one of them is what the user typed.
const DISPLAY_NAME_GRAMMAR: &str =
    "it must be 1 to 512 bytes with no surrounding whitespace and no control characters";

fn validate_environment_name(name: &str) -> Result<(), AgentRegistryError> {
    let mut bytes = name.bytes();
    let first = bytes.next();
    let valid = first.is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    if name.is_empty() {
        return Err(invalid_registration(
            "env_allowlist",
            "a variable name cannot be empty",
        ));
    }
    if name.len() > MAX_ENVIRONMENT_NAME_LENGTH {
        return Err(invalid_registration(
            "env_allowlist",
            "a variable name is longer than the maximum environment name length",
        ));
    }
    if !valid {
        return Err(invalid_registration(
            "env_allowlist",
            "a variable name must match [A-Za-z_][A-Za-z0-9_]*",
        ));
    }
    Ok(())
}

/// Everything `agents.json` holds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentRegistryFile {
    agents: Vec<AgentRegistration>,
}

impl AgentRegistryFile {
    /// Every registration, in the order the file lists them.
    pub fn agents(&self) -> impl ExactSizeIterator<Item = &AgentRegistration> {
        self.agents.iter()
    }

    /// The registration carrying `id`, if one does.
    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&AgentRegistration> {
        self.agents.iter().find(|agent| agent.id() == id)
    }

    pub(super) fn get_mut(&mut self, id: &AgentId) -> Option<&mut AgentRegistration> {
        self.agents.iter_mut().find(|agent| agent.id() == id)
    }

    pub(super) fn insert(
        &mut self,
        registration: AgentRegistration,
    ) -> Result<(), AgentRegistryError> {
        if self.agents.len() >= MAX_REGISTERED_AGENTS {
            return Err(AgentRegistryError::TooManyAgents {
                limit: MAX_REGISTERED_AGENTS,
            });
        }
        self.agents.push(registration);
        Ok(())
    }

    pub(super) fn remove(&mut self, id: &AgentId) -> Option<AgentRegistration> {
        let position = self.agents.iter().position(|agent| agent.id() == id)?;
        Some(self.agents.remove(position))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRegistryFileWire {
    schema_version: u32,
    agents: Vec<AgentRegistrationWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRegistrationWire {
    id: AgentId,
    display_name: String,
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_allowlist: Vec<String>,
    /// Absent means off. A hand-written entry that says nothing about being
    /// enabled has said the safe thing, and the only way to turn one on is the
    /// explicit action that checks trust first.
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    source: AgentSource,
}

/// The borrowing form written back, byte-compatible with the strict form above.
#[derive(Serialize)]
struct PersistedRegistry<'a> {
    schema_version: u32,
    agents: &'a [AgentRegistration],
}

/// Reads `agents.json`, treating a missing file as an empty registry.
///
/// The read is **bounded**, which matters most for the one caller whose file is
/// untrusted: the same parser reads `.harkness/agents.json` out of a checked-out
/// repository, and ADR-0006 says repository content decides nothing — including
/// how much memory this process spends looking at it, nor how long it spends
/// there. Anything that is not a regular file is refused from its metadata
/// before it is opened, because `open(2)` on a FIFO with no writer never
/// returns; the size limit is then enforced on the *read* rather than on that
/// metadata, so a file that grows between the two cannot get past it.
///
/// # Errors
///
/// Returns [`AgentRegistryError::ConfigurationRead`] when the file cannot be
/// read or is larger than the bound, the two version errors when it names a
/// schema outside this build's range,
/// [`AgentRegistryError::MalformedConfiguration`] when the body does not parse,
/// and [`AgentRegistryError::InvalidRegistration`] when a parsed entry violates
/// an invariant or two entries share an identifier.
pub(super) fn read_registry(path: &Path) -> Result<AgentRegistryFile, AgentRegistryError> {
    // Decided from the metadata *before* the file is opened, which is the same
    // rule a workspace probe follows and for the same reason: `open(2)` on a
    // FIFO with no writer never returns, and a repository can ship
    // `.harkness/agents.json` as a symlink to one. A read with a size bound and
    // no deadline would let untrusted content decide this process's liveness
    // instead of its memory, which is not an improvement. `metadata` follows
    // symlinks, so what is checked is what would be opened.
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(AgentRegistryError::ConfigurationRead {
                path: path.to_path_buf(),
                source: std::io::Error::other("the agent registry is not a regular file"),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentRegistryFile::default());
        }
        Err(source) => {
            return Err(AgentRegistryError::ConfigurationRead {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentRegistryFile::default());
        }
        Err(source) => {
            return Err(AgentRegistryError::ConfigurationRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    // One byte past the limit, so a file *at* the limit reads whole and the
    // first byte over it is what proves the refusal rather than a separate stat
    // whose answer could already be stale.
    Read::take(file, MAX_AGENTS_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AgentRegistryError::ConfigurationRead {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_AGENTS_FILE_BYTES {
        return Err(AgentRegistryError::ConfigurationRead {
            path: path.to_path_buf(),
            source: std::io::Error::other(format!(
                "the agent registry is larger than the {MAX_AGENTS_FILE_BYTES} byte maximum"
            )),
        });
    }

    let malformed = |source| AgentRegistryError::MalformedConfiguration {
        path: path.to_path_buf(),
        source,
    };

    // Probe before the body. A future schema would fail to deserialize as a v1
    // registry, and reporting that as "malformed" would hide the one cause the
    // user can act on.
    let probe: SchemaVersionProbe = serde_json::from_slice(&bytes).map_err(malformed)?;
    if probe.schema_version < MINIMUM_AGENTS_SCHEMA_VERSION {
        return Err(AgentRegistryError::ConfigurationVersionTooOld {
            found: probe.schema_version,
            minimum: MINIMUM_AGENTS_SCHEMA_VERSION,
        });
    }
    if probe.schema_version > AGENTS_SCHEMA_VERSION {
        return Err(AgentRegistryError::ConfigurationVersionTooNew {
            found: probe.schema_version,
            maximum: AGENTS_SCHEMA_VERSION,
        });
    }

    let wire: AgentRegistryFileWire = serde_json::from_slice(&bytes).map_err(malformed)?;
    debug_assert_eq!(wire.schema_version, probe.schema_version);

    if wire.agents.len() > MAX_REGISTERED_AGENTS {
        return Err(AgentRegistryError::TooManyAgents {
            limit: MAX_REGISTERED_AGENTS,
        });
    }

    let mut agents = Vec::with_capacity(wire.agents.len());
    let mut seen = BTreeSet::new();
    for entry in wire.agents {
        if !seen.insert(entry.id.clone()) {
            return Err(invalid_registration(
                "id",
                "two registrations share one identifier",
            ));
        }
        agents.push(AgentRegistration::from_parts(
            entry.id,
            entry.display_name,
            entry.command,
            entry.args,
            entry.env_allowlist,
            entry.enabled,
            entry.source,
        )?);
    }
    Ok(AgentRegistryFile { agents })
}

/// The exact bytes `agents.json` holds for one registry.
///
/// Split out of [`persist_registry`] so the frozen fixture is compared against
/// the encoder that actually writes the file rather than against a second
/// spelling of it; a fixture that pins something nothing produces pins nothing.
///
/// # Errors
///
/// Returns the serializer's own failure, which for this shape means a platform
/// path that is not valid UTF-8 — the known Unix limitation every other durable
/// JSON format in the workspace shares.
pub(super) fn encode_registry(registry: &AgentRegistryFile) -> Result<String, serde_json::Error> {
    let persisted = PersistedRegistry {
        // v1 is the only schema, so there is no oldest-representable choice to
        // make yet. When a v2 field arrives, this becomes the same conditional
        // the project catalog carries: persist the oldest version that can
        // represent every entry, so an older build keeps reading a file that
        // uses nothing it does not understand.
        schema_version: AGENTS_SCHEMA_VERSION,
        agents: &registry.agents,
    };
    // A trailing newline, so the file ends the way every other text file in a
    // repository does and a diff of it has no "\ No newline" line in it.
    serde_json::to_string_pretty(&persisted).map(|encoded| format!("{encoded}\n"))
}

/// Replaces `agents.json` atomically: write a temporary file beside it, sync,
/// rename, then sync the directory holding the new entry.
///
/// # Errors
///
/// Returns [`AgentRegistryError::ConfigurationWrite`] for any filesystem
/// failure, and never leaves a partially written registry in place.
pub(super) fn persist_registry(
    data_dir: &Path,
    path: &Path,
    registry: &AgentRegistryFile,
) -> Result<(), AgentRegistryError> {
    let failed = |source| AgentRegistryError::ConfigurationWrite {
        path: path.to_path_buf(),
        source,
    };

    let encoded =
        encode_registry(registry).map_err(|error| failed(std::io::Error::other(error)))?;
    // Refused before anything is written, because the reader enforces the same
    // number: a registry of entirely legal entries can still exceed it, and
    // writing one would produce a file this build could never read again. The
    // caller keeps the registry it had and is told what happened.
    if encoded.len() > MAX_AGENTS_FILE_BYTES {
        return Err(invalid_registration(
            "agents",
            "the registry is larger than the maximum agents.json size",
        ));
    }

    fs::create_dir_all(data_dir).map_err(failed)?;
    let mut temporary = NamedTempFile::new_in(data_dir).map_err(failed)?;
    temporary.write_all(encoded.as_bytes()).map_err(failed)?;
    temporary.as_file_mut().sync_all().map_err(failed)?;
    temporary
        .persist(path)
        .map_err(|error| failed(error.error))?;

    // The file's contents are already durable; what the rename still needs is a
    // sync of the directory holding the new entry. Windows has no equivalent
    // handle to sync, so this is a Unix-only step.
    #[cfg(unix)]
    File::open(data_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(failed)?;

    Ok(())
}

/// Takes the exclusive registry lock for one read-modify-write.
///
/// The lock file is created once and never replaced, because atomic persistence
/// swaps `agents.json` for a new inode and a lock held against the old one would
/// exclude nobody — the same reason `projects.lock` is a separate, stable inode.
///
/// # Errors
///
/// Returns [`AgentRegistryError::ConfigurationWrite`] when the lock file cannot
/// be created or locked.
pub(super) fn lock_exclusive(data_dir: &Path) -> Result<File, AgentRegistryError> {
    let lock_path = data_dir.join(AGENTS_LOCK_FILE);
    let failed = |source| AgentRegistryError::ConfigurationWrite {
        path: lock_path.clone(),
        source,
    };
    fs::create_dir_all(data_dir).map_err(failed)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(failed)?;
    // Blocking rather than `try_lock`: the critical section is one small read
    // plus one write, and a caller has nothing useful to do with a "busy" error
    // except retry.
    lock.lock().map_err(failed)?;
    Ok(lock)
}

/// Reads the registry under a shared lock, creating nothing.
///
/// A registry that has never been written has no lock file and nothing to race
/// with, and a read must never be the thing that creates either — that is what
/// makes "read-only operations never rewrite `agents.json`" true of the
/// directory as well as of the file.
pub(super) fn read_registry_shared(
    data_dir: &Path,
) -> Result<AgentRegistryFile, AgentRegistryError> {
    let lock_path = data_dir.join(AGENTS_LOCK_FILE);
    let lock = match File::options().read(true).open(&lock_path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Atomic replacement makes an unlocked read safe when no writer has
            // created the stable lock inode yet.
            return read_registry(&data_dir.join(AGENTS_FILE));
        }
        Err(source) => {
            return Err(AgentRegistryError::ConfigurationRead {
                path: lock_path,
                source,
            });
        }
    };
    lock.lock_shared()
        .map_err(|source| AgentRegistryError::ConfigurationRead {
            path: lock_path,
            source,
        })?;
    read_registry(&data_dir.join(AGENTS_FILE))
}
