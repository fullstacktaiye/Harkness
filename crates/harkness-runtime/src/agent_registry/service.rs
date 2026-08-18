//! The facade every front end registers, trusts, checks and launches an agent
//! through.
//!
//! Two stores meet here and neither is the other's cache. `agents.json` holds
//! configuration a user wrote; `runtime.db` holds grants and observations
//! Harkness made. Writes that touch both are ordered so that a failure between
//! them fails *closed*: a removal drops the grant before the registration, and
//! an invalidation is recorded before the registration is switched off, so the
//! worst outcome of a half-finished mutation is an agent that refuses to launch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harkness_acp::harkness_transport::{Connection, SpawnSpec, TransportError};
use harkness_acp::{
    AcpConnection, AcpError, AcpTimeouts, AdvertisedClientCapabilities, ClientIdentity,
};
use harkness_git::Cancellation;
use serde_json::json;
use time::OffsetDateTime;

use crate::domain::RunId;
use crate::integration::{
    ConfigurationSource, ExecutableIdentity, IdentityBasis, IntegrationIdentity,
    InvalidationReason, ObservedIdentity, Sha256Hash, SubjectKind, TrustCheck, TrustRecord,
    TrustRecordId, TrustScope, TrustState,
};
use crate::store::{EventKind, RunEvent, Store, StoredTrustRecord};

use super::config::{
    AGENTS_FILE, AgentRegistration, AgentRegistryFile, lock_exclusive, persist_registry,
    read_registry, read_registry_shared,
};
use super::discovery::{Discovery, DiscoveryReport};
use super::error::AgentRegistryError;
use super::id::AgentId;
use super::state::{
    AgentObservations, AgentRuntimeState, AgentTeardown, AgentTrust, AuthStatus,
    CompatibilityStatus, HealthRecord, HealthStatus, InitializeRecord, initialize_record,
};
use super::suggestion::{AgentSuggestion, repository_suggestions};

/// How long a health check waits for `initialize` when a caller names nothing.
///
/// The same number is given to the transport's startup window, because the two
/// bound one wait from different sides and two different numbers would mean one
/// of them never fires.
pub const DEFAULT_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long teardown waits at each rung before escalating.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// One registration and everything known about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredAgent {
    registration: AgentRegistration,
    state: AgentRuntimeState,
}

impl RegisteredAgent {
    /// What the user configured.
    #[must_use]
    pub const fn registration(&self) -> &AgentRegistration {
        &self.registration
    }

    /// What Harkness observed and decided.
    #[must_use]
    pub const fn state(&self) -> &AgentRuntimeState {
        &self.state
    }

    /// The registration's identity.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        self.registration.id()
    }

    /// Whether the agent is enabled *and* currently trusted.
    ///
    /// Neither half alone is a launch decision, and this is deliberately not the
    /// check a launch performs — that one re-hashes the executable, because a
    /// grant is about bytes rather than about a row.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.registration.is_enabled() && self.state.trust().is_trusted()
    }
}

/// What one registration mutation did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationOutcome {
    registration: AgentRegistration,
    changed: bool,
}

impl RegistrationOutcome {
    /// The registration as `agents.json` now holds it.
    #[must_use]
    pub const fn registration(&self) -> &AgentRegistration {
        &self.registration
    }

    /// Whether the file was rewritten.
    ///
    /// `false` means the request was already true, which is what makes
    /// registering the same agent twice a no-op rather than a rewrite: an
    /// idempotent call must not churn a file the user keeps in version control.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }
}

/// What one removal did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemovalOutcome {
    removed: Option<AgentRegistration>,
}

impl RemovalOutcome {
    /// The registration that was removed, when there was one.
    #[must_use]
    pub const fn removed(&self) -> Option<&AgentRegistration> {
        self.removed.as_ref()
    }

    /// Whether anything was removed.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.removed.is_some()
    }
}

/// What one trust decision established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustOutcome {
    registration: AgentRegistration,
    trust: AgentTrust,
}

impl TrustOutcome {
    /// The registration the decision was about.
    #[must_use]
    pub const fn registration(&self) -> &AgentRegistration {
        &self.registration
    }

    /// The grant as it now stands.
    #[must_use]
    pub const fn trust(&self) -> &AgentTrust {
        &self.trust
    }
}

/// A trust grant about to be made.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustAgent {
    id: AgentId,
    scope: TrustScope,
    enable: bool,
    at: OffsetDateTime,
}

impl TrustAgent {
    /// Trusts one agent wherever it is used, as of `at`.
    #[must_use]
    pub const fn new(id: AgentId, at: OffsetDateTime) -> Self {
        Self {
            id,
            scope: TrustScope::Global,
            enable: false,
            at,
        }
    }

    /// Confines the grant to one workspace root.
    #[must_use]
    pub fn in_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.scope = TrustScope::workspace(root);
        self
    }

    /// Enables the registration in the same act.
    ///
    /// Enabling is otherwise refused for an untrusted agent, so this is not a
    /// short cut around that rule — it is the one call in which the grant that
    /// satisfies it is being made.
    #[must_use]
    pub const fn and_enable(mut self) -> Self {
        self.enable = true;
        self
    }
}

/// Where a launch is happening, for the checks that depend on it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchContext {
    workspace: Option<PathBuf>,
    run: Option<RunId>,
}

impl LaunchContext {
    /// Names the workspace the agent is being launched for.
    ///
    /// A workspace-scoped grant does not reach outside the root it names, and a
    /// caller that supplies nothing gets that refusal rather than a silent pass.
    /// The refusal costs the grant nothing: being used in the wrong place is not
    /// evidence that anything about the agent changed.
    #[must_use]
    pub fn in_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace = Some(root.into());
        self
    }

    /// Names the run this launch belongs to, so drift lands on its timeline.
    #[must_use]
    pub const fn during_run(mut self, run: RunId) -> Self {
        self.run = Some(run);
        self
    }
}

/// One health check about to run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthCheck {
    id: AgentId,
    context: LaunchContext,
    working_dir: Option<PathBuf>,
    initialize_timeout: Duration,
    shutdown_grace: Duration,
}

impl HealthCheck {
    /// Checks one agent with the default deadlines.
    #[must_use]
    pub fn new(id: AgentId) -> Self {
        Self {
            id,
            context: LaunchContext::default(),
            working_dir: None,
            initialize_timeout: DEFAULT_HEALTH_CHECK_TIMEOUT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Runs the agent in this directory instead of a fresh temporary one.
    ///
    /// The default is deliberately *not* a user workspace. A health check asks
    /// an agent one question and needs no project to ask it, so handing one a
    /// checkout would expose a workspace for no reason.
    #[must_use]
    pub fn in_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(directory.into());
        self
    }

    /// Names the launch context the check runs under.
    #[must_use]
    pub fn with_context(mut self, context: LaunchContext) -> Self {
        self.context = context;
        self
    }

    /// Replaces how long the agent has to answer `initialize`.
    #[must_use]
    pub const fn within(mut self, timeout: Duration) -> Self {
        self.initialize_timeout = timeout;
        self
    }

    /// Replaces how long teardown waits at each rung.
    #[must_use]
    pub const fn tearing_down_within(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }
}

/// What one health check established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthOutcome {
    id: AgentId,
    record: HealthRecord,
    initialize: Option<InitializeRecord>,
}

impl HealthOutcome {
    /// The agent that was checked.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        &self.id
    }

    /// How the check ended.
    #[must_use]
    pub const fn status(&self) -> HealthStatus {
        self.record.status()
    }

    /// The record that was persisted.
    #[must_use]
    pub const fn record(&self) -> &HealthRecord {
        &self.record
    }

    /// What the handshake established, when one succeeded.
    #[must_use]
    pub const fn initialize(&self) -> Option<&InitializeRecord> {
        self.initialize.as_ref()
    }
}

/// A validated, trusted, hash-verified description of how to launch one agent.
///
/// Holding one is the proof that every gate was passed *for the bytes currently
/// on disk*: the registration exists, it is enabled, a grant covers it, and the
/// executable still hashes to what the grant was made about. It carries the
/// digest it verified so a caller records exactly what it launched rather than
/// re-deriving it and possibly disagreeing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLaunch {
    id: AgentId,
    command: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    executable_sha256: Sha256Hash,
}

impl AgentLaunch {
    /// The agent being launched.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        &self.id
    }

    /// The program that will run.
    #[must_use]
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// The digest verified immediately before this value was built.
    #[must_use]
    pub const fn executable_sha256(&self) -> Sha256Hash {
        self.executable_sha256
    }

    /// The identity policy and approvals bind this launch to.
    ///
    /// Every protected call carries it, and a grant is matched against it as one
    /// whole value, so a swapped binary defeats an existing approval even where
    /// the scope would otherwise have covered the call.
    #[must_use]
    pub fn integration_identity(&self) -> IntegrationIdentity {
        IntegrationIdentity::none().with_agent_executable_sha256(self.executable_sha256)
    }

    /// Every environment variable the agent will see, and nothing else.
    pub fn environment(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Describes the subprocess, with the allowlisted environment applied.
    ///
    /// The spec starts from an empty environment and admits exactly the pairs
    /// named here, so what is absent from the registration's allowlist is absent
    /// from the child.
    #[must_use]
    pub fn spawn_spec(&self, working_dir: impl Into<PathBuf>) -> SpawnSpec {
        SpawnSpec::new(self.command.clone(), working_dir)
            .args(&self.args)
            .envs(self.env.iter().map(|(name, value)| (name, value)))
    }
}

/// Registration, discovery, trust, and health for external ACP agents.
///
/// Every boundary this type enforces is enforced *here* rather than in a front
/// end: no untrusted candidate is executed, no disabled agent is launched, no
/// grant survives its executable changing, and no repository suggestion enables
/// itself. A GUI or a command line that forgot one of those rules cannot get
/// past this type.
pub struct AgentRegistryService {
    data_dir: PathBuf,
    store: Arc<Store>,
}

impl AgentRegistryService {
    /// Opens the registry stored under `data_dir`, recording state in `store`.
    ///
    /// Nothing is created. A registry that has never been written reads as
    /// empty, which is what makes constructing this service safe in a read-only
    /// front end.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>, store: Arc<Store>) -> Self {
        Self {
            data_dir: data_dir.into(),
            store,
        }
    }

    /// Where `agents.json` lives.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.data_dir.join(AGENTS_FILE)
    }

    // -- reads --------------------------------------------------------------

    /// Every registration, exactly as the file lists them.
    ///
    /// # Errors
    ///
    /// Returns the file's own read, version, and validation failures.
    pub fn registrations(&self) -> Result<AgentRegistryFile, AgentRegistryError> {
        read_registry_shared(&self.data_dir)
    }

    /// Every registration with everything known about it.
    ///
    /// # Errors
    ///
    /// Returns the file's own failures, and [`AgentRegistryError::Store`] when
    /// the observations cannot be read.
    pub fn list(&self) -> Result<Vec<RegisteredAgent>, AgentRegistryError> {
        let file = self.registrations()?;
        let mut agents = Vec::with_capacity(file.agents().len());
        for registration in file.agents() {
            let state = self.runtime_state(registration.id())?;
            agents.push(RegisteredAgent {
                registration: registration.clone(),
                state,
            });
        }
        Ok(agents)
    }

    /// One registration with everything known about it.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`] when nothing carries `id`.
    pub fn get(&self, id: &AgentId) -> Result<RegisteredAgent, AgentRegistryError> {
        let file = self.registrations()?;
        let registration = file
            .get(id)
            .ok_or_else(|| AgentRegistryError::UnknownAgent { id: id.clone() })?
            .clone();
        let state = self.runtime_state(id)?;
        Ok(RegisteredAgent {
            registration,
            state,
        })
    }

    /// Every grant ever recorded about one agent, oldest first.
    ///
    /// The whole history, because a revocation followed by a fresh grant is two
    /// records and an audit that showed only the current one would omit the
    /// decision the trust model exists to preserve.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::Store`] when the records cannot be read.
    pub fn trust_history(
        &self,
        id: &AgentId,
    ) -> Result<Vec<StoredTrustRecord>, AgentRegistryError> {
        Ok(self
            .store
            .trust_records(SubjectKind::AgentExecutable, id.as_str())?)
    }

    // -- registration -------------------------------------------------------

    /// Adds one registration, or confirms an identical one already exists.
    ///
    /// Idempotent by identifier: re-registering the same configuration rewrites
    /// nothing and reports [`changed`](RegistrationOutcome::changed) as `false`.
    /// A *different* configuration under an existing identifier is refused
    /// rather than silently replacing it — changing a registration is
    /// [`update`](Self::update), and the difference matters because a command
    /// path is what a grant was made about.
    ///
    /// The stored entry is always disabled, whatever else happens.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::AgentAlreadyRegistered`] for a conflicting
    /// identifier, [`AgentRegistryError::TooManyAgents`] when the file is full,
    /// and the file's own read and write failures.
    pub fn register(
        &self,
        registration: AgentRegistration,
    ) -> Result<RegistrationOutcome, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let mut file = read_registry(&self.path())?;
        if let Some(existing) = file.get(registration.id()) {
            if existing.describes_same_configuration(&registration) {
                return Ok(RegistrationOutcome {
                    registration: existing.clone(),
                    changed: false,
                });
            }
            return Err(AgentRegistryError::AgentAlreadyRegistered {
                id: registration.id().clone(),
            });
        }
        // Disabled here rather than trusting the value that arrived. A fresh
        // `AgentRegistration` is built disabled, but the type is `Clone` and
        // `AgentRegistryFile::get` hands one out, so a caller can round-trip an
        // *enabled* registration back in — and after a `remove` that dropped
        // its grant, storing it verbatim would put an enabled agent in the file
        // that no trust decision covers. The invariant is enforced where the
        // write happens, not where the value is usually built.
        let mut registration = registration;
        registration.set_enabled(false);
        file.insert(registration.clone())?;
        persist_registry(&self.data_dir, &self.path(), &file)?;
        Ok(RegistrationOutcome {
            registration,
            changed: true,
        })
    }

    /// Replaces one registration's configuration.
    ///
    /// The replacement is stored **disabled**, and any grant about the agent is
    /// left exactly as it was. That is not an oversight: a grant is bound to an
    /// executable digest, so an update that repoints the command at another
    /// program is caught by the hash check on the next launch and reported as
    /// the drift it is, with the reason a user can act on. Switching the agent
    /// off in the meantime means the window between the two is not a window in
    /// which the new program runs.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`] when nothing carries the
    /// identifier, and the file's own read and write failures.
    pub fn update(
        &self,
        registration: AgentRegistration,
    ) -> Result<RegistrationOutcome, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let mut file = read_registry(&self.path())?;
        let existing =
            file.get(registration.id())
                .ok_or_else(|| AgentRegistryError::UnknownAgent {
                    id: registration.id().clone(),
                })?;
        if existing.describes_same_configuration(&registration) {
            return Ok(RegistrationOutcome {
                registration: existing.clone(),
                changed: false,
            });
        }
        // Replaced in place rather than removed and appended. The file is one a
        // user keeps in version control, and reordering every later entry to
        // change one field is exactly the churn the idempotency rule above
        // exists to avoid.
        let mut registration = registration;
        registration.set_enabled(false);
        let slot =
            file.get_mut(registration.id())
                .ok_or_else(|| AgentRegistryError::UnknownAgent {
                    id: registration.id().clone(),
                })?;
        *slot = registration.clone();
        persist_registry(&self.data_dir, &self.path(), &file)?;
        Ok(RegistrationOutcome {
            registration,
            changed: true,
        })
    }

    /// Switches one registration on or off.
    ///
    /// Enabling requires a grant that currently stands. This is the structural
    /// half of "a suggestion never auto-enables": every registration is created
    /// disabled and the only way out of that state passes through a trust
    /// decision somebody made.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`],
    /// [`AgentRegistryError::AgentNotTrusted`] when enabling an agent no grant
    /// covers, and the file's own read and write failures.
    pub fn set_enabled(
        &self,
        id: &AgentId,
        enabled: bool,
    ) -> Result<RegistrationOutcome, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        if enabled {
            let trust = self.trust_state(id)?;
            if !trust.is_trusted() {
                return Err(not_trusted(id, &trust));
            }
        }
        self.set_enabled_locked(id, enabled)
    }

    /// Removes one registration, its grants, and everything observed about it.
    ///
    /// Under the registry lock, the durable state goes first and the file
    /// second. A failure between the two therefore leaves a registration nothing
    /// trusts, which refuses to launch; the other order would leave a grant
    /// behind for an identifier the user could re-register, and the new program
    /// would arrive trusted.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::Store`] and the file's own failures.
    pub fn remove(&self, id: &AgentId) -> Result<RemovalOutcome, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let mut file = read_registry(&self.path())?;
        if file.get(id).is_none() {
            return Ok(RemovalOutcome { removed: None });
        }
        self.store.forget_agent(id)?;
        let removed = file.remove(id);
        persist_registry(&self.data_dir, &self.path(), &file)?;
        Ok(RemovalOutcome { removed })
    }

    // -- trust --------------------------------------------------------------

    /// Grants trust to the executable that is at the configured path *now*.
    ///
    /// The digest is computed here rather than taken from a caller, because a
    /// grant a caller could describe is a grant a caller could describe
    /// incorrectly. Re-granting after drift continues the same record — the
    /// decision is the same one, re-affirmed against the identity that is
    /// actually there — while a grant after a revocation is a new record, so the
    /// refusal a user expressed is never overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`],
    /// [`AgentRegistryError::ExecutableNotFound`],
    /// [`AgentRegistryError::InvalidExecutable`],
    /// [`AgentRegistryError::Integration`] when the identity cannot be built,
    /// and [`AgentRegistryError::Store`].
    pub fn trust(&self, options: TrustAgent) -> Result<TrustOutcome, AgentRegistryError> {
        // The registry lock is taken first and held across both stores, which is
        // the order every mutation here uses: `agents.json` lock, then the run
        // store. Reading the registration through `registrations` instead would
        // ask for a *shared* lock while this exclusive one is held, and an
        // advisory lock does not care that the same process is on both ends of
        // that wait.
        let _lock = lock_exclusive(&self.data_dir)?;
        let registration = self.registration_locked(&options.id)?;
        let digest = self.verify_executable(&registration)?;
        let basis = identity_basis(&registration, digest)?;

        let latest = self
            .store
            .latest_trust_record(SubjectKind::AgentExecutable, options.id.as_str())?;
        let record = match latest {
            // An invalidated grant is re-affirmed on the same record: nobody
            // decided to withdraw it, so moving the basis and the grant time is
            // what "trust it again" means.
            Some(stored) if stored.record().state() == TrustState::Invalidated => {
                let mut record = stored.record().clone();
                record.regrant(basis, options.at)?;
                self.store.update_trust_record(stored.id(), &record)?;
                record
            }
            // A revoked grant is terminal and a still-standing one is already
            // the answer; either way a fresh decision is a fresh record.
            _ => {
                let record = TrustRecord::grant(
                    SubjectKind::AgentExecutable,
                    basis,
                    options.scope.clone(),
                    options.at,
                )?;
                self.store.insert_trust_record(
                    TrustRecordId::new(),
                    SubjectKind::AgentExecutable,
                    options.id.as_str(),
                    &record,
                    // `recorded_at` is when the *row* was written and is read
                    // by nothing but the ordering that decides which record is
                    // the latest one. It is this clock rather than the caller's
                    // `granted_at` for exactly that reason: a caller naming an
                    // older grant time would otherwise file a fresh decision
                    // behind the revocation it replaces, and the agent would be
                    // enabled while its most recent record said `revoked`.
                    OffsetDateTime::now_utc(),
                )?;
                record
            }
        };

        let trust = AgentTrust::from_record(&record);
        let registration = if options.enable {
            self.set_enabled_locked(&options.id, true)?.registration
        } else {
            registration
        };
        Ok(TrustOutcome {
            registration,
            trust,
        })
    }

    /// Withdraws the grant on an explicit decision, and disables the agent.
    ///
    /// Revocation is terminal for the record it ends: trusting the agent again
    /// is a new decision and a new record, so the refusal stays in the audit
    /// trail instead of being overwritten by the next answer.
    ///
    /// **Idempotent.** "Make sure this agent is not trusted" is a call a surface
    /// makes without first asking whether it already is, so an agent with no
    /// record and an agent whose latest record is already revoked both report
    /// the state rather than refusing. Only the transition is skipped; the
    /// registration is still switched off either way.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`],
    /// [`AgentRegistryError::Store`], and the file's own write failures.
    pub fn revoke_trust(&self, id: &AgentId) -> Result<TrustOutcome, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let _registration = self.registration_locked(id)?;
        let stored = self
            .store
            .latest_trust_record(SubjectKind::AgentExecutable, id.as_str())?;
        let trust = match stored {
            // Already the answer. `TrustRecord::revoke` would refuse the edge —
            // correctly, since `Revoked` is terminal — and turning "it is
            // already what you asked for" into an error would make a surface
            // check the state before every call it makes to set it.
            Some(stored) if stored.record().state() == TrustState::Revoked => {
                AgentTrust::from_record(stored.record())
            }
            Some(stored) => {
                let mut record = stored.record().clone();
                record.revoke()?;
                self.store.update_trust_record(stored.id(), &record)?;
                AgentTrust::from_record(&record)
            }
            // Nothing to withdraw, and nothing to complain about either.
            None => AgentTrust::untrusted(),
        };
        let registration = self.set_enabled_locked(id, false)?.registration;
        Ok(TrustOutcome {
            registration,
            trust,
        })
    }

    /// Records that a person has, or has not, authenticated this agent.
    ///
    /// ACP v1 has the agent handle authentication itself, so Harkness never
    /// learns the outcome from the wire: an agent that is signed in and an agent
    /// that is not both advertise the same `authMethods`, and a health check
    /// therefore records [`AuthStatus::Required`] for either. This is how a
    /// surface that walked a person through the agent's own sign-in tells the
    /// registry, and it is the only way [`AuthStatus::Authenticated`] is ever
    /// reached.
    ///
    /// Without it, running a health check on an agent that offers a sign-in
    /// would make that agent permanently unlaunchable — a check is a
    /// convenience, and one that could take an agent away would be a trap.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`] when nothing carries `id`,
    /// and [`AgentRegistryError::Store`].
    pub fn record_authentication(
        &self,
        id: &AgentId,
        status: AuthStatus,
    ) -> Result<AgentRuntimeState, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let _registration = self.registration_locked(id)?;
        let at = OffsetDateTime::now_utc();
        let mut observations = self
            .store
            .agent_observations(id)?
            .unwrap_or_else(|| AgentObservations::unobserved(at));
        observations.record_authentication(status, at);
        self.store.put_agent_observations(id, &observations)?;
        Ok(AgentRuntimeState::new(
            self.trust_state(id)?,
            Some(observations),
        ))
    }

    // -- discovery ----------------------------------------------------------

    /// Lists candidate executables on the search path, and runs none of them.
    ///
    /// The result is a suggestion and nothing more: no candidate is executed,
    /// opened, hashed, or recorded until a user registers and trusts it.
    #[must_use]
    pub fn discover(&self, discovery: &Discovery, cancel: &Cancellation) -> DiscoveryReport {
        discovery.run(cancel)
    }

    /// Reads the agent configuration a checked-out repository ships, if any.
    ///
    /// Every entry comes back disabled whatever the file says, and none of them
    /// is written anywhere. Adopting one is a call the user makes.
    ///
    /// # Errors
    ///
    /// Returns the same failures as reading the user's own registry.
    pub fn repository_suggestions(
        &self,
        workspace_root: &Path,
    ) -> Result<Vec<AgentSuggestion>, AgentRegistryError> {
        repository_suggestions(workspace_root)
    }

    // -- launching ----------------------------------------------------------

    /// Proves one agent may be launched, for the bytes that are on disk now.
    ///
    /// Every gate, in the order that makes each refusal the most useful one: the
    /// registration exists, it is enabled, a grant covers it, the executable is
    /// there and runnable, its digest still matches the grant, nobody owes it an
    /// authentication, and the version it last selected is one this build
    /// speaks. A mismatch invalidates the grant, disables the agent, and refuses
    /// — so a swapped binary stops every later launch too, rather than being
    /// re-detected each time.
    ///
    /// The last two gates are about what a *previous* handshake found, so an
    /// agent nobody has health-checked passes both. That is deliberate: a health
    /// check is a convenience, not a precondition, and refusing to launch an
    /// agent because nobody has asked it a question yet would make the check
    /// mandatory by the back door. The real handshake happens at session start
    /// and answers both questions again.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::UnknownAgent`],
    /// [`AgentRegistryError::AgentDisabled`],
    /// [`AgentRegistryError::AgentNotTrusted`],
    /// [`AgentRegistryError::ExecutableNotFound`],
    /// [`AgentRegistryError::InvalidExecutable`],
    /// [`AgentRegistryError::ExecutableHashMismatch`],
    /// [`AgentRegistryError::AuthenticationRequired`], and
    /// [`AgentRegistryError::IncompatibleAgent`].
    pub fn prepare_launch(
        &self,
        id: &AgentId,
        context: &LaunchContext,
    ) -> Result<AgentLaunch, AgentRegistryError> {
        let (registration, digest) = self.admit(id, context, DriftDetection::Launch)?;
        // The observations alone, not the composed runtime state: that one
        // re-reads the trust record `admit` has just read and validated, which
        // is both a wasted query and a second moment at which the answer could
        // differ from the one the trust gate was decided on.
        let observed = self.store.agent_observations(id)?;
        let auth = observed
            .as_ref()
            .map_or(AuthStatus::Unknown, AgentObservations::auth_status);
        if auth == AuthStatus::Required {
            return Err(AgentRegistryError::AuthenticationRequired { id: id.clone() });
        }
        if let Some(CompatibilityStatus::UnsupportedProtocolVersion { advertised }) =
            observed.as_ref().map(AgentObservations::compatibility)
        {
            return Err(AgentRegistryError::IncompatibleAgent {
                id: id.clone(),
                advertised,
            });
        }
        Ok(launch(&registration, digest))
    }

    /// Spawns the agent, negotiates once, tears it down, and records what it
    /// found.
    ///
    /// The record is persisted whether the check succeeded or not, which is the
    /// whole point of having one: an agent that failed yesterday and has not
    /// been checked since is a different thing from an agent nobody ever asked.
    /// The line is what the failure is *about*. Anything learned about the
    /// executable or the conversation is recorded — including a command that is
    /// missing or is not a program, which is a fact about the agent even though
    /// nothing ran. What is not recorded is the registry's own state: an
    /// unknown, disabled or untrusted agent, and a digest that no longer matches
    /// its grant, which has its own durable consequence in the trust record and
    /// would only be repeated here.
    ///
    /// The connection advertises no client capability at all. Each one is a
    /// promise to mediate a request the agent may then make, and a health check
    /// mediates nothing.
    ///
    /// # Errors
    ///
    /// Returns the registry-state refusals unrecorded, and every other failure
    /// after recording it: [`AgentRegistryError::ExecutableNotFound`] and
    /// [`AgentRegistryError::InvalidExecutable`] when the command will not run,
    /// [`AgentRegistryError::InitializeTimeout`] when the agent says nothing in
    /// time, and [`AgentRegistryError::Acp`] carrying whatever else went wrong.
    pub fn health_check(
        &self,
        options: &HealthCheck,
        cancel: &Cancellation,
    ) -> Result<HealthOutcome, AgentRegistryError> {
        let started = Instant::now();
        let (registration, digest) =
            match self.admit(&options.id, &options.context, DriftDetection::HealthCheck) {
                Ok(admitted) => admitted,
                // A command that is missing or unrunnable is something the check
                // *found out about the agent*, so it lands in the record like
                // any other outcome. Reaching this without one would leave a
                // user staring at a registry that says nothing happened.
                Err(
                    error @ (AgentRegistryError::ExecutableNotFound { .. }
                    | AgentRegistryError::InvalidExecutable { .. }),
                ) => {
                    return self.record_health(
                        options,
                        ProbeOutcome {
                            initialize: None,
                            teardown: None,
                            failure: Some(error),
                        },
                        started.elapsed(),
                    );
                }
                Err(error) => return Err(error),
            };
        let launch = launch(&registration, digest);

        // A fresh directory rather than a workspace: the agent is asked one
        // question and needs no project to answer it. It is created under the
        // data directory so an isolated front end or a test stays inside its own
        // `HARKNESS_DATA_DIR`, and it is removed when this call returns.
        //
        // The path and the guard that owns it are decided together, so there is
        // no arrangement in which they can disagree.
        let (working_dir, scratch) = match &options.working_dir {
            Some(directory) => (directory.clone(), None),
            None => {
                let scratch = self.scratch_directory()?;
                (scratch.path().to_path_buf(), Some(scratch))
            }
        };

        let outcome = self.run_health_check(options, &launch, &working_dir, cancel);
        let elapsed = started.elapsed();
        drop(scratch);

        self.record_health(options, outcome, elapsed)
    }

    // -- internals ----------------------------------------------------------

    /// Runs the check itself, with no persistence and no clock of its own.
    fn run_health_check(
        &self,
        options: &HealthCheck,
        launch: &AgentLaunch,
        working_dir: &Path,
        cancel: &Cancellation,
    ) -> ProbeOutcome {
        let spec = launch
            .spawn_spec(working_dir.to_path_buf())
            // The transport's startup window and the adapter's `initialize`
            // deadline bound one wait from two sides; giving them two different
            // numbers would mean one of them never fires.
            .startup_deadline(options.initialize_timeout);
        let connection = match Connection::spawn(spec, cancel.clone()) {
            Ok(connection) => connection,
            Err(error) => {
                return ProbeOutcome {
                    initialize: None,
                    teardown: None,
                    failure: Some(self.classify_spawn(&options.id, launch, error)),
                };
            }
        };

        let mut agent = AcpConnection::with_timeouts(
            connection,
            AcpTimeouts {
                initialize: options.initialize_timeout,
                // Nothing here authenticates, so this deadline is never reached;
                // it is set to the same window so a future call on this
                // connection cannot inherit a value nobody chose.
                authenticate: options.initialize_timeout,
                shutdown_grace: options.shutdown_grace,
            },
        );
        let negotiated =
            agent.initialize(&client_identity(), &AdvertisedClientCapabilities::default());
        // Teardown always runs, and always before the outcome is interpreted: an
        // agent that hung must not stay alive while Harkness decides what to
        // call the failure.
        let teardown = AgentTeardown::from(agent.shutdown().rung);

        match negotiated {
            Ok(outcome) => ProbeOutcome {
                initialize: Some(outcome),
                teardown: Some(teardown),
                failure: None,
            },
            Err(error) => ProbeOutcome {
                initialize: None,
                teardown: Some(teardown),
                failure: Some(self.classify_handshake(options, error)),
            },
        }
    }

    /// Turns a spawn failure into the refusal a user can act on.
    fn classify_spawn(
        &self,
        id: &AgentId,
        launch: &AgentLaunch,
        error: TransportError,
    ) -> AgentRegistryError {
        match &error {
            // The program is not runnable. The transport's own kind is right
            // about *what happened* and says nothing about *what to do*; this
            // distinction is the one a user acts on, and the transport failure
            // is kept as the source so its discriminant is not lost.
            //
            // `invalid_spawn_spec` is deliberately *not* folded in with it: that
            // one means Harkness described the launch badly — a working
            // directory that is not absolute, a command rooted under the other
            // platform's convention — and telling a user their agent binary is
            // broken would send them to look at the wrong thing.
            TransportError::SpawnFailed { .. } => AgentRegistryError::InvalidExecutable {
                id: id.clone(),
                path: launch.command.clone(),
                reason: error.to_string(),
                source: Some(Box::new(AcpError::from(error))),
            },
            _ => AgentRegistryError::Acp {
                id: id.clone(),
                source: Box::new(AcpError::from(error)),
            },
        }
    }

    /// Turns a handshake failure into the refusal a user can act on.
    fn classify_handshake(&self, options: &HealthCheck, error: AcpError) -> AgentRegistryError {
        let id = options.id.clone();
        if matches!(
            error.transport(),
            Some(
                TransportError::RequestTimedOut { .. }
                    | TransportError::StartupDeadlineExceeded { .. }
                    | TransportError::SendTimedOut
            )
        ) {
            return AgentRegistryError::InitializeTimeout {
                id,
                timeout: options.initialize_timeout,
                source: Box::new(error),
            };
        }
        AgentRegistryError::Acp {
            id,
            source: Box::new(error),
        }
    }

    /// Persists what the probe found, then reports it.
    fn record_health(
        &self,
        options: &HealthCheck,
        probe: ProbeOutcome,
        elapsed: Duration,
    ) -> Result<HealthOutcome, AgentRegistryError> {
        // Taken for the read-modify-write and *not* for the check itself: an
        // agent gets up to a full deadline to answer, and holding the registry
        // lock across that would stop every other registry operation for as long
        // as somebody else's program felt like taking. What it does close is the
        // window in which a concurrent `record_authentication` is read, kept,
        // and then written back stale — which would put an agent somebody just
        // signed in to back into `Required` and out of reach.
        let _lock = lock_exclusive(&self.data_dir)?;
        let at = OffsetDateTime::now_utc();
        let mut observations = self
            .store
            .agent_observations(&options.id)?
            .unwrap_or_else(|| AgentObservations::unobserved(at));

        let (status, record, initialize) = match (&probe.initialize, &probe.failure) {
            (Some(outcome), _) => {
                let record = initialize_record(outcome, at);
                let status = if record.capabilities().requires_authentication() {
                    HealthStatus::AuthenticationRequired
                } else {
                    HealthStatus::Healthy
                };
                observations.record_initialize(record.clone());
                (
                    status,
                    HealthRecord::succeeded(status, elapsed, at),
                    Some(record),
                )
            }
            (None, Some(failure)) => {
                let status = match failure {
                    AgentRegistryError::Acp { source, .. } => match **source {
                        AcpError::UnsupportedProtocolVersion { agent_selected } => {
                            observations.record_compatibility(
                                CompatibilityStatus::UnsupportedProtocolVersion {
                                    advertised: agent_selected,
                                },
                            );
                            HealthStatus::Incompatible
                        }
                        _ => HealthStatus::Failed,
                    },
                    _ => HealthStatus::Failed,
                };
                (
                    status,
                    HealthRecord::failed(status, failure.kind(), failure.to_string(), elapsed, at),
                    None,
                )
            }
            // Unreachable by construction: the probe reports an outcome or a
            // failure. Recording it as a failure rather than panicking keeps the
            // executor's promise that a check always leaves a record.
            (None, None) => (
                HealthStatus::Failed,
                HealthRecord::failed(
                    HealthStatus::Failed,
                    "invalid_agent_registration",
                    "the health check produced neither an outcome nor a failure",
                    elapsed,
                    at,
                ),
                None,
            ),
        };

        let record = match probe.teardown {
            Some(teardown) => record.torn_down(teardown),
            None => record,
        };
        observations.record_health(record.clone(), at);
        self.store
            .put_agent_observations(&options.id, &observations)?;

        if let Some(run) = options.context.run {
            let payload = json!({
                "agent_id": options.id.as_str(),
                "status": status.as_str(),
                "failure_kind": record.failure_kind(),
                "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "protocol_version": initialize.as_ref().map(InitializeRecord::protocol_version),
                "teardown": record.teardown().map(AgentTeardown::as_str),
            });
            self.store.append_event(
                run,
                RunEvent::new(EventKind::ExternalAgentHealthChecked, at).with_payload(payload),
            )?;
        }

        match probe.failure {
            Some(failure) => Err(failure),
            None => Ok(HealthOutcome {
                id: options.id.clone(),
                record,
                initialize,
            }),
        }
    }

    /// Every gate a launch and a health check share, in refusal order.
    fn admit(
        &self,
        id: &AgentId,
        context: &LaunchContext,
        detected_at: DriftDetection,
    ) -> Result<(AgentRegistration, Sha256Hash), AgentRegistryError> {
        let registration = self.registration(id)?;
        if !registration.is_enabled() {
            return Err(AgentRegistryError::AgentDisabled { id: id.clone() });
        }
        let stored = match self
            .store
            .latest_trust_record(SubjectKind::AgentExecutable, id.as_str())?
        {
            Some(stored) if stored.record().state() == TrustState::Trusted => stored,
            Some(stored) => {
                return Err(not_trusted(id, &AgentTrust::from_record(stored.record())));
            }
            None => return Err(not_trusted(id, &AgentTrust::untrusted())),
        };

        // Hashed *after* the cheap refusals and *before* anything is launched.
        let digest = self.verify_executable(&registration)?;
        let mut observed = ObservedIdentity::new(identity_basis(&registration, digest)?);
        if let Some(workspace) = context.workspace.as_ref() {
            observed = observed.in_workspace(workspace.clone());
        }

        match stored.record().check(&observed) {
            TrustCheck::Valid => Ok((registration, digest)),
            // Unreachable: the record was filtered to `Trusted` above. Reported
            // rather than asserted, because a state machine's guarantee is not
            // an excuse for a panic in a launch path.
            TrustCheck::NotTrusted => {
                Err(not_trusted(id, &AgentTrust::from_record(stored.record())))
            }
            // A workspace-scoped grant used somewhere else is the one answer
            // that is *not* drift, and it must not be treated as drift. It is
            // first in `InvalidationReason::PRECEDENCE` precisely because it
            // decides whether this record governs the situation at all — the
            // subject has not changed, the record simply does not reach here.
            // Invalidating on it would destroy a grant that is still perfectly
            // valid in the workspace it was given for, and would do it every
            // time a caller forgot to name the workspace.
            TrustCheck::Invalidate(InvalidationReason::WorkspacePathChanged) => {
                Err(AgentRegistryError::AgentNotTrusted {
                    id: id.clone(),
                    state: stored.record().state(),
                    reason: Some(InvalidationReason::WorkspacePathChanged.explanation()),
                })
            }
            TrustCheck::Invalidate(reason) => {
                Err(self.invalidate(id, &stored, reason, digest, context, detected_at)?)
            }
        }
    }

    /// Records drift, switches the agent off, and returns the refusal.
    ///
    /// Both orders here are deliberate. The registry lock is taken first and
    /// held across both stores, like every other mutation. Underneath it, the
    /// grant is invalidated in durable state *before* the registration is
    /// disabled, so a failure between the two leaves an agent every launch path
    /// already refuses.
    ///
    /// The record `admit` read is deliberately **not** the one that is written.
    /// That read happened outside the lock, so between it and here another
    /// caller may have re-granted trust against the very bytes this call is
    /// about to complain about; writing the stale clone would silently undo
    /// their decision. Re-reading under the lock makes the whole
    /// read-modify-write serialize against `trust` and `revoke_trust`, which
    /// hold the same lock across theirs — the discipline the run store states
    /// for its own transactions, applied to a pair of writes no single
    /// transaction spans.
    fn invalidate(
        &self,
        id: &AgentId,
        stored: &StoredTrustRecord,
        reason: InvalidationReason,
        observed: Sha256Hash,
        context: &LaunchContext,
        detected_at: DriftDetection,
    ) -> Result<AgentRegistryError, AgentRegistryError> {
        let _lock = lock_exclusive(&self.data_dir)?;
        let current = self
            .store
            .latest_trust_record(SubjectKind::AgentExecutable, id.as_str())?;
        let stored = match current {
            // Still the same row, still standing: the drift is real and is ours
            // to record.
            Some(current)
                if current.id() == stored.id()
                    && current.record().state() == TrustState::Trusted =>
            {
                current
            }
            // Somebody moved it while this call was hashing. Whatever they
            // decided is newer than what this call observed, so it is reported
            // rather than overwritten, and the caller re-checks against it.
            Some(current) => {
                return Ok(not_trusted(id, &AgentTrust::from_record(current.record())));
            }
            None => return Ok(not_trusted(id, &AgentTrust::untrusted())),
        };

        let trusted = stored
            .record()
            .identity_basis()
            .executable()
            .map(ExecutableIdentity::sha256);

        let mut record = stored.record().clone();
        record.invalidate(reason)?;
        self.store.update_trust_record(stored.id(), &record)?;

        if let Some(run) = context.run {
            let payload = json!({
                "agent_id": id.as_str(),
                "reason": reason.as_str(),
                "trusted_sha256": trusted.map(Sha256Hash::to_hex),
                "observed_sha256": observed.to_hex(),
                "detected_at": detected_at.as_str(),
            });
            self.store.append_event(
                run,
                RunEvent::new(
                    EventKind::ExternalAgentTrustInvalidated,
                    OffsetDateTime::now_utc(),
                )
                .with_payload(payload),
            )?;
        }

        self.set_enabled_locked(id, false)?;

        Ok(match (reason, trusted) {
            (InvalidationReason::ExecutableHashChanged, Some(trusted)) => {
                AgentRegistryError::ExecutableHashMismatch {
                    id: id.clone(),
                    trusted,
                    observed,
                }
            }
            _ => AgentRegistryError::AgentNotTrusted {
                id: id.clone(),
                state: TrustState::Invalidated,
                reason: Some(reason.explanation()),
            },
        })
    }

    /// Reads one registration under a shared lock, or reports that there is none.
    fn registration(&self, id: &AgentId) -> Result<AgentRegistration, AgentRegistryError> {
        self.registrations()?
            .get(id)
            .cloned()
            .ok_or_else(|| AgentRegistryError::UnknownAgent { id: id.clone() })
    }

    /// Reads one registration with the exclusive lock already held.
    ///
    /// Separate from [`registration`](Self::registration) because that one asks
    /// for a *shared* lock, and an advisory lock does not care that the same
    /// process already holds the exclusive one: a mutation that read through it
    /// would wait for itself.
    fn registration_locked(&self, id: &AgentId) -> Result<AgentRegistration, AgentRegistryError> {
        read_registry(&self.path())?
            .get(id)
            .cloned()
            .ok_or_else(|| AgentRegistryError::UnknownAgent { id: id.clone() })
    }

    /// Rewrites the enabled flag with the file lock already held.
    fn set_enabled_locked(
        &self,
        id: &AgentId,
        enabled: bool,
    ) -> Result<RegistrationOutcome, AgentRegistryError> {
        let mut file = read_registry(&self.path())?;
        let registration = file
            .get_mut(id)
            .ok_or_else(|| AgentRegistryError::UnknownAgent { id: id.clone() })?;
        if registration.is_enabled() == enabled {
            return Ok(RegistrationOutcome {
                registration: registration.clone(),
                changed: false,
            });
        }
        registration.set_enabled(enabled);
        let registration = registration.clone();
        persist_registry(&self.data_dir, &self.path(), &file)?;
        Ok(RegistrationOutcome {
            registration,
            changed: true,
        })
    }

    /// The grant that currently stands, or its absence.
    fn trust_state(&self, id: &AgentId) -> Result<AgentTrust, AgentRegistryError> {
        Ok(self
            .store
            .latest_trust_record(SubjectKind::AgentExecutable, id.as_str())?
            .map_or_else(AgentTrust::untrusted, |stored| {
                AgentTrust::from_record(stored.record())
            }))
    }

    fn runtime_state(&self, id: &AgentId) -> Result<AgentRuntimeState, AgentRegistryError> {
        Ok(AgentRuntimeState::new(
            self.trust_state(id)?,
            self.store.agent_observations(id)?,
        ))
    }

    /// Hashes the configured executable, refusing what cannot be run.
    ///
    /// The digest is streamed rather than buffered: an agent binary is routinely
    /// tens of megabytes, and its size must not decide this process's memory.
    fn verify_executable(
        &self,
        registration: &AgentRegistration,
    ) -> Result<Sha256Hash, AgentRegistryError> {
        let path = registration.command();
        let id = registration.id().clone();
        let metadata =
            std::fs::metadata(path).map_err(|error| AgentRegistryError::ExecutableNotFound {
                id: id.clone(),
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?;
        if !metadata.is_file() {
            return Err(AgentRegistryError::InvalidExecutable {
                id,
                path: path.to_path_buf(),
                reason: "it is not a regular file".to_owned(),
                source: None,
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(AgentRegistryError::InvalidExecutable {
                    id,
                    path: path.to_path_buf(),
                    reason: "it is not executable".to_owned(),
                    source: None,
                });
            }
        }
        let mut file =
            std::fs::File::open(path).map_err(|error| AgentRegistryError::InvalidExecutable {
                id: id.clone(),
                path: path.to_path_buf(),
                reason: error.to_string(),
                source: None,
            })?;
        Sha256Hash::of_reader(&mut file).map_err(|error| AgentRegistryError::InvalidExecutable {
            id,
            path: path.to_path_buf(),
            reason: error.to_string(),
            source: None,
        })
    }

    /// A private, empty directory for one health check to run in.
    fn scratch_directory(&self) -> Result<tempfile::TempDir, AgentRegistryError> {
        std::fs::create_dir_all(&self.data_dir).map_err(|source| {
            AgentRegistryError::ConfigurationWrite {
                path: self.data_dir.clone(),
                source,
            }
        })?;
        tempfile::Builder::new()
            .prefix("agent-health-")
            .tempdir_in(&self.data_dir)
            .map_err(|source| AgentRegistryError::ConfigurationWrite {
                path: self.data_dir.clone(),
                source,
            })
    }
}

/// Which side of the registry noticed that a grant stopped applying.
///
/// Recorded on the audit entry, because a check somebody asked for and a launch
/// that was about to happen are two very different moments to have found a
/// swapped binary in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriftDetection {
    HealthCheck,
    Launch,
}

impl DriftDetection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::HealthCheck => "health_check",
            Self::Launch => "launch",
        }
    }
}

/// What one probe of a live agent found.
struct ProbeOutcome {
    initialize: Option<harkness_acp::InitializeOutcome>,
    teardown: Option<AgentTeardown>,
    failure: Option<AgentRegistryError>,
}

/// How Harkness names itself to an agent.
///
/// A product name and a version and nothing else — no username, no workspace
/// path, no project identifier. `initialize` happens before Harkness has decided
/// anything about the program on the other end.
fn client_identity() -> ClientIdentity {
    ClientIdentity::new("harkness", env!("CARGO_PKG_VERSION")).title("Harkness")
}

/// The identity a grant about this registration is bound to.
///
/// Deliberately narrow. The display name is recorded and never compared, the
/// executable path is recorded and never compared, and the digest is the whole
/// of the check — so an agent that renames itself keeps its grant and an agent
/// whose bytes change does not. Nothing the *agent* reports takes part: a
/// version it prints about itself and a capability set it advertises are claims,
/// and binding a grant to a claim would let the subject decide whether its own
/// grant still applies.
fn identity_basis(
    registration: &AgentRegistration,
    digest: Sha256Hash,
) -> Result<IdentityBasis, AgentRegistryError> {
    let executable = ExecutableIdentity::new(registration.command().to_path_buf(), digest)?;
    Ok(
        IdentityBasis::new(registration.display_name(), ConfigurationSource::User)?
            .launched_from(executable),
    )
}

fn launch(registration: &AgentRegistration, digest: Sha256Hash) -> AgentLaunch {
    AgentLaunch {
        id: registration.id().clone(),
        command: registration.command().to_path_buf(),
        args: registration.args().map(str::to_owned).collect(),
        // Resolved here rather than at spawn time so what the agent will see is
        // a value a caller can inspect and record. A name the allowlist admits
        // and this process does not hold is simply absent, exactly as it would
        // be for any other program: an allowlist grants visibility, it does not
        // invent a value.
        env: registration
            .env_allowlist()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| (name.to_owned(), value))
            })
            .collect(),
        executable_sha256: digest,
    }
}

fn not_trusted(id: &AgentId, trust: &AgentTrust) -> AgentRegistryError {
    AgentRegistryError::AgentNotTrusted {
        id: id.clone(),
        state: trust.state(),
        reason: trust
            .invalidation_reason()
            .map(InvalidationReason::explanation),
    }
}
