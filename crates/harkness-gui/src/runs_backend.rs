//! The run and approval mutation bridge, and the process's shared coordinator.
//!
//! `HarknessBackend` is the Git and catalog surface. This is a second, separate
//! `#[qml_element]` object for runs, and it is separate on purpose: run state
//! has nothing to refresh in step with a working tree, and adding it to an
//! eleven-thousand-line bridge would couple the two refresh cycles for no
//! reason. QML instantiates one of each.
//!
//! # What lives here and what does not
//!
//! Reads live on the three models beside this file. cxx-qt gives one bridge
//! object no handle to another — a `RunListModel` created in QML is not
//! reachable from `RunsBackend`'s Rust — so a `refresh_runs` invokable here
//! could not fill a model over there. Each model therefore drives its own
//! paging, and this object carries the operations that change something:
//! `cancelRun`, `retryRun`, `approve`, `deny`, and the on-demand
//! `loadApprovalInput`.
//!
//! # The Qt-thread mutation invariant
//!
//! Every `RunsBackendRust` field is read and written on the Qt thread and
//! nowhere else. Every store, coordinator, and catalog call is a blocking call,
//! so each one runs on a `std::thread::spawn` worker whose result re-enters
//! through `qt_thread().queue(...)`. Workers own plain data — identifiers,
//! request numbers, cloned handles — and never a `QString`, a `QVariant`, or a
//! pinned reference, none of which are `Send`.
//!
//! # The staleness counter
//!
//! `next_request` is the same mechanism `HarknessBackend::next_review_request`
//! is: every operation takes the next number, and a reply whose number is no
//! longer the newest still clears its share of `busy` but never writes
//! `status`, `kind`, or `detail`. Two operations overlapping otherwise let the
//! slower one's message overwrite the faster one's, so the panel would report
//! the outcome of the thing the user did *first*.
//!
//! # The error namespace
//!
//! A failure crossing to QML carries a machine-readable `kind` beside its
//! message. Runtime failures keep the discriminant `RuntimeError::kind` or
//! `StoreError::kind` gave them, exactly as `harkness contract` publishes them;
//! the handful this bridge raises itself are listed in [`BRIDGE_KINDS`] and are
//! front-end kinds, not additions to a runtime namespace.

#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;
    }

    extern "RustQt" {
        /// The run and approval mutation surface.
        ///
        /// cxx-qt does not convert names to camel case, so a `snake_case`
        /// member reaches QML spelled exactly as written and a camel-case call
        /// site silently resolves to `undefined`. Every multi-word invokable is
        /// therefore named for the Qt side explicitly, and property names are
        /// kept to a single word.
        ///
        /// `busy` is true while any operation is outstanding, `status` carries
        /// the newest operation's user-facing message, `kind` its stable
        /// discriminant (empty on success), and `detail` the last approval
        /// input `loadApprovalInput` fetched.
        #[qobject]
        #[qml_element]
        #[qproperty(bool, busy)]
        #[qproperty(QString, status)]
        #[qproperty(QString, kind)]
        #[qproperty(QVariant, detail)]
        type RunsBackend = super::RunsBackendRust;

        /// Stops a run: its queued calls, its executing tool, and any approval
        /// it is parked on.
        #[qinvokable]
        #[cxx_name = "cancelRun"]
        fn cancel_run(self: Pin<&mut RunsBackend>, run_id: &QString);

        /// Starts a *new* run re-attempting `run_id`; nothing is resumed.
        #[qinvokable]
        #[cxx_name = "retryRun"]
        fn retry_run(self: Pin<&mut RunsBackend>, run_id: &QString);

        /// Grants one pending approval.
        ///
        /// `scope` is a stored scope spelling — `exact_call`, `tool_for_run`,
        /// or `capability_for_run` — or empty for the breadth the stored
        /// request already allows. A decision may narrow and never widen, which
        /// the runtime re-checks against the record rather than trusting here.
        #[qinvokable]
        fn approve(
            self: Pin<&mut RunsBackend>,
            approval_id: &QString,
            scope: &QString,
            reason: &QString,
        );

        /// Refuses one pending approval.
        #[qinvokable]
        fn deny(self: Pin<&mut RunsBackend>, approval_id: &QString, reason: &QString);

        /// Loads the validated input a pending approval is holding.
        ///
        /// Not carried on the approval row: the row holds the request's own
        /// summary, and the input is the tool call's, fetched only when a
        /// surface is about to show it.
        #[qinvokable]
        #[cxx_name = "loadApprovalInput"]
        fn load_approval_input(self: Pin<&mut RunsBackend>, approval_id: &QString);
    }

    impl cxx_qt::Threading for RunsBackend {}
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QMap, QMapPair_QString_QVariant, QString, QVariant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use harkness_runtime::agent::{
    AgentAction, MockAgent, ObservationPattern, Scenario, ScenarioId, ScenarioStep, WorkspaceRef,
};
use harkness_runtime::approval::{ApprovalDecision, ApprovalId, ApprovalScope, DecidedVia};
use harkness_runtime::coordinator::{RunCoordinator, RuntimeError};
use harkness_runtime::domain::{RunId, ToolCall, ToolCallState};
use harkness_runtime::store::{PassThrough, Store, StoreError};

/// Largest approval input this bridge hands to QML.
///
/// A stored input is already held to the run store's 64 KiB inline bound, so
/// this is not what keeps it finite; it keeps a single delegate from being
/// asked to lay out sixty kilobytes of JSON. What is dropped is named in the
/// projection rather than silently missing.
const MAX_APPROVAL_INPUT_BYTES: usize = 8 * 1024;

/// The Harkness data directory could not be resolved at all.
pub(crate) const DATA_DIRECTORY_UNAVAILABLE: &str = "data_directory_unresolved";
/// The process's run coordinator could not be built.
pub(crate) const COORDINATOR_UNAVAILABLE: &str = "coordinator_unavailable";
/// The data directory holds no run store, so it holds no run or approval.
pub(crate) const NO_RUN_STORE: &str = "no_run_store";
/// A run identifier QML supplied is not one.
pub(crate) const INVALID_RUN_ID: &str = "invalid_run_id";
/// An approval identifier QML supplied is not one.
pub(crate) const INVALID_APPROVAL_ID: &str = "invalid_approval_id";
/// A scope spelling QML supplied is not one this build defines.
pub(crate) const UNKNOWN_APPROVAL_SCOPE: &str = "unknown_approval_scope";
/// A run's recorded calls cannot be rebuilt into a script that re-issues them.
pub(crate) const RUN_NOT_REPLAYABLE: &str = "run_not_replayable";

/// Kinds this bridge raises itself, in declaration order.
///
/// They are front-end discriminants and deliberately not additions to
/// `RuntimeError::KINDS` or `StoreError::KINDS`, which the CLI concatenates
/// into its published `exit_code_by_kind` table. Nothing here reaches that
/// table, and no spelling in it is reused.
///
/// The table is built from the named constants rather than being the source of
/// them, and every raise site names a constant. A positional table indexed at
/// the raise sites would remap every kind the moment one was inserted or
/// reordered, with nothing failing to compile and the set-based collision test
/// below still passing — which is why this exists only to be asserted over.
#[cfg(test)]
pub(crate) const BRIDGE_KINDS: [&str; 7] = [
    DATA_DIRECTORY_UNAVAILABLE,
    COORDINATOR_UNAVAILABLE,
    NO_RUN_STORE,
    INVALID_RUN_ID,
    INVALID_APPROVAL_ID,
    UNKNOWN_APPROVAL_SCOPE,
    RUN_NOT_REPLAYABLE,
];

/// One failure on its way to QML: a stable discriminant and a message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunsFailure {
    pub(crate) kind: String,
    pub(crate) message: String,
}

impl RunsFailure {
    pub(crate) fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            message: message.into(),
        }
    }
}

impl From<RuntimeError> for RunsFailure {
    fn from(error: RuntimeError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            message: error.to_string(),
        }
    }
}

impl From<StoreError> for RunsFailure {
    fn from(error: StoreError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            message: error.to_string(),
        }
    }
}

/// This process's run coordinator for one data directory.
///
/// A coordinator owns a `Scheduler`, and the scheduler is what serializes
/// mutating tool calls per workspace and caps child processes across all of
/// them; it also owns the *lease* that says which runs this process is driving.
/// Both make it a process-wide resource rather than a per-caller one, which is
/// why the checks panel and the runs bridge address it through this one cache
/// instead of building one each. Two coordinators in one process would take two
/// leases, cap child processes against each other, and — because
/// `RunCoordinator::cancel_run` only knows the runs its own coordinator started
/// — leave every run started by the other one uncancellable.
///
/// Keyed by data directory rather than held as a single value: the directory is
/// chosen at startup, but a test process can drive more than one, and a
/// coordinator belongs to exactly one store.
pub(crate) fn coordinator_for(data_dir: &Path) -> Result<RunCoordinator, RunsFailure> {
    coordinator_in(data_dir, true).map(|coordinator| {
        coordinator.expect("a coordinator asked to be created is returned or refused")
    })
}

/// The coordinator for a data directory that has already recorded something.
///
/// A read must not be what brings a run store into existence — the same rule
/// the checks panel follows. A directory with no `runtime.db` answers `None` and
/// is left exactly as it was; a coordinator already built for it is returned
/// without probing, because at that point the store demonstrably exists.
///
/// # Attaching to an existing store is not itself a read
///
/// Building the *first* coordinator for a directory takes this process's lease
/// and runs the startup sweep, which marks every run whose owning process is
/// provably dead as `interrupted` and appends an event saying so. That is a
/// write, and it happens on whichever call gets there first — the runs panel
/// opening, or a check being started. It is deliberate rather than incidental:
/// a front end attaching to run history is exactly the moment abandoned runs
/// should stop reading as live, and ADR-0020 makes the sweep a precondition of
/// accepting work rather than something a caller may skip. What it must never
/// do is disturb a *live* sibling process, and it cannot: the proof is an
/// advisory lock rather than a timestamp.
pub(crate) fn existing_coordinator(data_dir: &Path) -> Result<Option<RunCoordinator>, RunsFailure> {
    coordinator_in(data_dir, false)
}

/// The thread every bridge method runs on.
///
/// Recorded from the Qt thread itself — see [`note_qt_thread`] — so the one
/// place a store is reached from can assert it is somewhere else. There is no
/// portable way to ask Qt from Rust which thread a `QObject` lives in, and the
/// invariant is about this process's threads rather than about Qt anyway.
static QT_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// Records the calling thread as the Qt thread.
///
/// Called from the invokables that spawn workers, which by construction run on
/// the Qt thread; the first one to run arms [`assert_off_qt_thread`].
pub(crate) fn note_qt_thread() {
    let _ = QT_THREAD.set(std::thread::current().id());
}

/// Fails a debug build that reaches the run store from the Qt thread.
///
/// Blocking the Qt thread on SQLite is the failure this whole file's threading
/// is arranged to prevent, and it is invisible in a release build until a user
/// with a large history watches the window freeze. Asserting at the one place
/// every read and write passes through is what turns "we were careful" into
/// something a test run can fail on.
fn assert_off_qt_thread() {
    debug_assert!(
        QT_THREAD.get() != Some(&std::thread::current().id()),
        "the run store was reached from the Qt thread; every store and \
         coordinator call belongs on a std::thread::spawn worker"
    );
}

fn coordinator_in(data_dir: &Path, create: bool) -> Result<Option<RunCoordinator>, RunsFailure> {
    assert_off_qt_thread();
    static COORDINATORS: OnceLock<Mutex<HashMap<PathBuf, RunCoordinator>>> = OnceLock::new();
    let mut coordinators = COORDINATORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| {
            RunsFailure::new(
                COORDINATOR_UNAVAILABLE,
                "the run coordinator cache is poisoned; restart Harkness",
            )
        })?;
    if let Some(existing) = coordinators.get(data_dir) {
        return Ok(Some(existing.clone()));
    }
    // Opened once and handed on. `Store::open_existing` is not a cheap probe —
    // it opens the connection, enables WAL, and climbs the whole migration
    // ladder — so discarding it and calling `Store::open` would pay for all of
    // that twice on the first read.
    let store = if create {
        Store::open(data_dir)?
    } else {
        match Store::open_existing(data_dir)? {
            Some(store) => store,
            None => return Ok(None),
        }
    };
    let coordinator = harkness_runtime::check::check_coordinator(Arc::new(store))
        .map_err(|error| RunsFailure::new(COORDINATOR_UNAVAILABLE, error.to_string()))?;
    coordinators.insert(data_dir.to_path_buf(), coordinator.clone());
    Ok(Some(coordinator))
}

/// The data directory every front-end read and write in this process uses.
///
/// Resolved from the environment and the platform directly rather than through
/// `ProjectService::load`, which reads and parses the whole project catalog on
/// the way to the same path. This is on the hot path of every page, every
/// timeline, and every mutation, and run history is independent of the catalog:
/// an unreadable `projects.json` must not be what stops a user seeing what
/// their runs did.
pub(crate) fn data_dir() -> Result<PathBuf, RunsFailure> {
    harkness_core::data_directory().ok_or_else(|| {
        RunsFailure::new(
            DATA_DIRECTORY_UNAVAILABLE,
            "this platform exposes no data directory and HARKNESS_DATA_DIR is not set",
        )
    })
}

/// The coordinator a read goes through, or `None` when nothing was ever run.
pub(crate) fn read_coordinator() -> Result<Option<RunCoordinator>, RunsFailure> {
    existing_coordinator(&data_dir()?)
}

/// The coordinator a mutation goes through.
///
/// A mutation names a run or an approval, so a data directory with no run store
/// cannot be holding either; saying so is more useful than creating a store to
/// discover it is empty.
fn write_coordinator() -> Result<RunCoordinator, RunsFailure> {
    read_coordinator()?.ok_or_else(|| {
        RunsFailure::new(NO_RUN_STORE, "this data directory has recorded no runs yet")
    })
}

/// Renders a UTC instant the way every other Harkness projection does.
pub(crate) fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// Renders an optional instant, with an absent one as the empty string.
pub(crate) fn optional_rfc3339(at: Option<OffsetDateTime>) -> String {
    at.map(rfc3339).unwrap_or_default()
}

pub(crate) fn parse_run(value: &str) -> Result<RunId, RunsFailure> {
    value
        .parse()
        .map_err(|_| RunsFailure::new(INVALID_RUN_ID, format!("{value:?} is not a run id")))
}

fn parse_approval(value: &str) -> Result<ApprovalId, RunsFailure> {
    value.parse().map_err(|_| {
        RunsFailure::new(
            INVALID_APPROVAL_ID,
            format!("{value:?} is not an approval id"),
        )
    })
}

/// Reads the scope a grant should carry from what QML asked for.
///
/// An empty request means "whatever the stored record already allows", which is
/// the effective scope the risk ceiling produced when the request was created.
/// Anything else has to be a spelling this build defines: a scope nobody
/// recognizes must not silently become the broadest one.
fn grant_scope(requested: &str, effective: ApprovalScope) -> Result<ApprovalScope, RunsFailure> {
    if requested.is_empty() {
        return Ok(effective);
    }
    ApprovalScope::from_stored(requested).ok_or_else(|| {
        RunsFailure::new(
            UNKNOWN_APPROVAL_SCOPE,
            format!("{requested:?} is not an approval scope"),
        )
    })
}

/// What a finished operation has to say for itself, in `Send` data only.
///
/// `QString` and `QVariant` are not `Send`, so a worker returns plain Rust and
/// the Qt-thread closure is what turns it into something QML can bind to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Completion {
    message: String,
    detail: Option<ApprovalInput>,
}

impl Completion {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
        }
    }
}

/// The validated input one pending approval is holding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApprovalInput {
    approval_id: String,
    run_id: String,
    tool: String,
    /// Pretty-printed JSON, already redacted by the store on the way in.
    input: String,
    /// Whether [`MAX_APPROVAL_INPUT_BYTES`] cut the rendering short.
    truncated: bool,
}

/// Keeps a rendering inside its byte budget on a character boundary.
fn clamp(text: String, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text, false);
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn approval_input(
    coordinator: &RunCoordinator,
    approval: ApprovalId,
) -> Result<Completion, RunsFailure> {
    let request = coordinator.store().approval(approval)?;
    let call = coordinator.store().load_tool_call(request.tool_call_id())?;
    let rendered =
        serde_json::to_string_pretty(call.input()).unwrap_or_else(|_| call.input().to_string());
    let (input, truncated) = clamp(rendered, MAX_APPROVAL_INPUT_BYTES);
    Ok(Completion {
        message: format!("Loaded the input {} is holding", request.tool()),
        detail: Some(ApprovalInput {
            approval_id: approval.to_string(),
            run_id: request.run_id().to_string(),
            tool: request.tool().to_string(),
            input,
            truncated,
        }),
    })
}

/// Rebuilds a script that re-issues what the original run recorded.
///
/// `RunCoordinator::retry_run` takes the agent that will drive the new run,
/// because a retry is a new run rather than a rewind and nothing about the
/// original is resumed. A front end cannot hand back the agent the original was
/// driven by — a `MockAgent`'s retained definition is the coordinator's, and a
/// persisted checkpoint decodes only inside the runtime — so what this
/// reconstructs is the *request sequence* the original recorded: one
/// `CallTool` per recorded call, in creation order, each expecting the shape of
/// observation that call actually produced, and a terminal `CompleteRun`.
///
/// It reconstructs a request and authorizes nothing. Every rebuilt call goes
/// through validation, policy, and approval again exactly as the first attempt
/// did, and no grant carries over, so the worst a wrong reconstruction can do
/// is record a diverged run. It does diverge when the re-attempt fails
/// differently from the attempt it copies — a call that failed and now succeeds
/// yields a `ToolResult` where the script expects a `ToolFailed` — and that
/// lands as one recorded failure of the new run rather than as anything the
/// original loses.
fn retry_scenario(run: RunId, calls: &[ToolCall]) -> Result<Scenario, RunsFailure> {
    let mut steps = Vec::with_capacity(calls.len() + 1);
    let mut expectation = ObservationPattern::RunStarted { task_title: None };
    for call in calls {
        let identity =
            harkness_runtime::tool::ToolIdentity::parse(call.tool_id(), call.tool_version())
                .map_err(|error| RunsFailure::new(RUN_NOT_REPLAYABLE, error.to_string()))?;
        steps.push(ScenarioStep::new(
            expectation,
            AgentAction::CallTool {
                tool_id: identity.id,
                tool_version: identity.version,
                input: call.input().clone(),
            },
        ));
        expectation = match call.state() {
            ToolCallState::Succeeded => ObservationPattern::ToolResult {
                artifact_media_type: None,
                output_contains: None,
            },
            _ => ObservationPattern::ToolFailed { error_kind: None },
        };
    }
    steps.push(ScenarioStep::new(
        expectation,
        AgentAction::CompleteRun {
            summary: format!("Re-attempted run {run}"),
        },
    ));
    let id = ScenarioId::new("retried_run")
        .map_err(|error| RunsFailure::new(RUN_NOT_REPLAYABLE, error.to_string()))?;
    Scenario::new(id, steps).map_err(|error| {
        RunsFailure::new(
            RUN_NOT_REPLAYABLE,
            format!("run {run} cannot be re-attempted from its recorded calls: {error}"),
        )
    })
}

pub struct RunsBackendRust {
    busy: bool,
    status: QString,
    kind: QString,
    detail: QVariant,
    /// Monotonic operation number; only the newest reply writes a message.
    next_request: u64,
    /// Operations still outstanding, which is what `busy` reports.
    pending: u32,
}

impl Default for RunsBackendRust {
    fn default() -> Self {
        Self {
            busy: false,
            status: QString::from("Ready"),
            kind: QString::default(),
            detail: QVariant::default(),
            next_request: 0,
            pending: 0,
        }
    }
}

fn approval_detail(input: &ApprovalInput) -> QVariant {
    let mut map = QMap::<QMapPair_QString_QVariant>::default();
    map.insert(
        QString::from("approvalId"),
        QVariant::from(&QString::from(input.approval_id.as_str())),
    );
    map.insert(
        QString::from("runId"),
        QVariant::from(&QString::from(input.run_id.as_str())),
    );
    map.insert(
        QString::from("tool"),
        QVariant::from(&QString::from(input.tool.as_str())),
    );
    map.insert(
        QString::from("input"),
        QVariant::from(&QString::from(input.input.as_str())),
    );
    map.insert(QString::from("truncated"), QVariant::from(&input.truncated));
    QVariant::from(&map)
}

/// Applies one finished operation on the Qt thread.
fn settle(
    mut backend: Pin<&mut ffi::RunsBackend>,
    request: u64,
    outcome: Result<Completion, RunsFailure>,
) {
    let newest = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.pending = rust.pending.saturating_sub(1);
        rust.next_request == request
    };
    let busy = backend.as_ref().rust().pending > 0;
    backend.as_mut().set_busy(busy);
    // A superseded reply still gave up its share of `busy`; what it must not do
    // is describe the operation the user is currently watching.
    if !newest {
        return;
    }
    // `detail` describes the operation being reported and nothing else. Left
    // standing, a failed `loadApprovalInput` — or any later cancel, approve, or
    // deny — would leave the *previous* approval's validated input bound while
    // a dialog asks about a different one, which is the worst way for this
    // field to be wrong.
    let detail = match &outcome {
        Ok(completion) => completion.detail.as_ref().map(approval_detail),
        Err(_) => None,
    };
    backend.as_mut().set_detail(detail.unwrap_or_default());
    match outcome {
        Ok(completion) => {
            backend
                .as_mut()
                .set_status(QString::from(completion.message.as_str()));
            backend.as_mut().set_kind(QString::default());
        }
        Err(failure) => {
            backend
                .as_mut()
                .set_status(QString::from(failure.message.as_str()));
            backend
                .as_mut()
                .set_kind(QString::from(failure.kind.as_str()));
        }
    }
}

impl ffi::RunsBackend {
    /// Runs `work` off the Qt thread and applies its outcome back on it.
    fn dispatch(
        mut self: Pin<&mut Self>,
        work: impl FnOnce() -> Result<Completion, RunsFailure> + Send + 'static,
    ) {
        note_qt_thread();
        let request = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_request += 1;
            rust.pending += 1;
            rust.next_request
        };
        self.as_mut().set_busy(true);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = work();
            let _ = qt_thread.queue(move |backend| settle(backend, request, outcome));
        });
    }

    fn cancel_run(self: Pin<&mut Self>, run_id: &QString) {
        let run_id = run_id.to_string();
        self.dispatch(move || {
            let run = parse_run(&run_id)?;
            // The coordinator flips the run's cancellation token first and
            // resolves its pending approvals afterwards. Both happen here, on
            // the worker, rather than on the Qt thread: reaching the
            // coordinator at all opens the store, and a store open is a
            // blocking call the Qt thread may not make.
            write_coordinator()?.cancel_run(run)?;
            Ok(Completion::message(format!("Cancelled run {run}")))
        });
    }

    fn retry_run(self: Pin<&mut Self>, run_id: &QString) {
        let run_id = run_id.to_string();
        self.dispatch(move || {
            let run = parse_run(&run_id)?;
            let coordinator = write_coordinator()?;
            let record = coordinator.store().load_run(run)?;
            let task = coordinator.store().load_task(record.task_id())?;
            let scenario = retry_scenario(run, &coordinator.store().load_run_tool_calls(run)?)?;
            // `PassThrough` is what `Store::open` installs and this process
            // never replaces it, so this reference is byte-identical to the one
            // the coordinator rebuilds from the same task. A build that starts
            // redacting would fail this comparison by name rather than quietly
            // retrying against a different workspace.
            let workspace = WorkspaceRef::from_task(&task, &PassThrough);
            let retry = coordinator.retry_run(
                run,
                Box::new(MockAgent::from_scenario(scenario)),
                workspace,
            )?;
            Ok(Completion::message(format!(
                "Started run {retry}, re-attempting {run}"
            )))
        });
    }

    fn approve(self: Pin<&mut Self>, approval_id: &QString, scope: &QString, reason: &QString) {
        let approval_id = approval_id.to_string();
        let scope = scope.to_string();
        let reason = reason.to_string();
        self.dispatch(move || {
            let approval = parse_approval(&approval_id)?;
            let coordinator = write_coordinator()?;
            let request = coordinator.store().approval(approval)?;
            let scope = grant_scope(&scope, request.effective_scope())?;
            let mut decision = ApprovalDecision::grant(
                approval,
                scope,
                DecidedVia::Gui,
                OffsetDateTime::now_utc(),
            );
            if !reason.is_empty() {
                decision = decision.because(reason);
            }
            coordinator.decide_approval(decision)?;
            Ok(Completion::message(format!(
                "Approved {} for {scope}",
                request.tool()
            )))
        });
    }

    fn deny(self: Pin<&mut Self>, approval_id: &QString, reason: &QString) {
        let approval_id = approval_id.to_string();
        let reason = reason.to_string();
        self.dispatch(move || {
            let approval = parse_approval(&approval_id)?;
            let coordinator = write_coordinator()?;
            let request = coordinator.store().approval(approval)?;
            let mut decision =
                ApprovalDecision::deny(approval, DecidedVia::Gui, OffsetDateTime::now_utc());
            if !reason.is_empty() {
                decision = decision.because(reason);
            }
            coordinator.decide_approval(decision)?;
            Ok(Completion::message(format!("Denied {}", request.tool())))
        });
    }

    fn load_approval_input(self: Pin<&mut Self>, approval_id: &QString) {
        let approval_id = approval_id.to_string();
        self.dispatch(move || {
            let approval = parse_approval(&approval_id)?;
            approval_input(&write_coordinator()?, approval)
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::OffsetDateTime;

    use harkness_runtime::agent::{AgentAction, ObservationPattern};
    use harkness_runtime::approval::ApprovalScope;
    use harkness_runtime::coordinator::RuntimeError;
    use harkness_runtime::domain::{RunId, Step, ToolCall, ToolCallState};
    use harkness_runtime::store::StoreError;

    use super::{
        BRIDGE_KINDS, MAX_APPROVAL_INPUT_BYTES, RunsFailure, clamp, grant_scope, optional_rfc3339,
        parse_approval, parse_run, retry_scenario, rfc3339,
    };

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_755_000_000 + seconds).unwrap()
    }

    /// One recorded call in `state`, on a step of `run`.
    fn recorded(run: RunId, state: ToolCallState) -> ToolCall {
        let step = Step::new(run, 0, "run the check", at(0));
        let mut call = ToolCall::new(
            &step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test"]}),
            at(1),
        );
        if state != ToolCallState::Pending {
            call.dispatch("1.0.0", at(2)).unwrap();
        }
        match state {
            ToolCallState::Succeeded => call.succeed(json!({"passed": true}), at(3)).unwrap(),
            ToolCallState::Failed => call
                .fail(
                    harkness_runtime::domain::Failure::new("timed_out", "the tool timed out"),
                    at(3),
                )
                .unwrap(),
            _ => {}
        }
        call
    }

    #[test]
    fn a_runtime_failure_keeps_the_kind_the_runtime_published() {
        let failure = RunsFailure::from(RuntimeError::RunNotActive { run: RunId::new() });

        assert_eq!(failure.kind, "run_not_active");
        assert!(
            failure.message.contains("not active"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn a_store_failure_keeps_the_kind_the_store_published() {
        let failure = RunsFailure::from(StoreError::InvalidPageLimit {
            limit: 0,
            maximum: 500,
        });

        assert!(
            StoreError::KINDS.contains(&failure.kind.as_str()),
            "{} is not a store kind",
            failure.kind
        );
    }

    #[test]
    fn bridge_kinds_do_not_collide_with_the_runtime_namespaces() {
        for kind in BRIDGE_KINDS {
            assert!(
                !RuntimeError::KINDS.contains(&kind) && !StoreError::KINDS.contains(&kind),
                "{kind} is published by a runtime namespace as well"
            );
        }
    }

    #[test]
    fn every_bridge_kind_has_one_spelling() {
        let mut seen = std::collections::HashSet::new();

        for kind in BRIDGE_KINDS {
            assert!(seen.insert(kind), "{kind} is declared twice");
        }
    }

    #[test]
    fn an_unparseable_identifier_is_refused_by_name() {
        assert_eq!(parse_run("not-a-uuid").unwrap_err().kind, "invalid_run_id");
        assert_eq!(
            parse_approval("not-a-uuid").unwrap_err().kind,
            "invalid_approval_id"
        );
    }

    #[test]
    fn an_unrequested_scope_is_the_one_the_record_already_allows() {
        assert_eq!(
            grant_scope("", ApprovalScope::ToolForRun).unwrap(),
            ApprovalScope::ToolForRun
        );
    }

    #[test]
    fn a_narrower_scope_is_taken_as_written() {
        assert_eq!(
            grant_scope("exact_call", ApprovalScope::ToolForRun).unwrap(),
            ApprovalScope::ExactCall
        );
    }

    #[test]
    fn a_scope_this_build_does_not_define_is_refused_rather_than_widened() {
        let failure = grant_scope("everything", ApprovalScope::ExactCall).unwrap_err();

        assert_eq!(failure.kind, "unknown_approval_scope");
    }

    #[test]
    fn an_oversized_input_is_clamped_on_a_character_boundary_and_says_so() {
        let text = "é".repeat(MAX_APPROVAL_INPUT_BYTES);

        let (clamped, truncated) = clamp(text, 5);

        assert!(truncated);
        assert_eq!(clamped, "éé");
    }

    #[test]
    fn an_input_inside_the_budget_is_delivered_whole() {
        let (clamped, truncated) = clamp("{}".to_owned(), MAX_APPROVAL_INPUT_BYTES);

        assert!(!truncated);
        assert_eq!(clamped, "{}");
    }

    #[test]
    fn a_retry_re_issues_the_call_the_original_run_recorded() {
        let run = RunId::new();
        let calls = vec![recorded(run, ToolCallState::Succeeded)];

        let scenario = retry_scenario(run, &calls).unwrap();

        let steps = scenario.steps();
        assert_eq!(steps.len(), 2, "one call and the completion that ends it");
        assert!(matches!(
            steps[0].expectation(),
            ObservationPattern::RunStarted { task_title: None }
        ));
        assert!(matches!(
            steps[0].action(),
            AgentAction::CallTool { tool_id, tool_version, input }
                if tool_id.as_str() == "process.exec"
                    && tool_version.to_string() == "1.0.0"
                    && input == &json!({"argv": ["cargo", "test"]})
        ));
        assert!(
            matches!(
                steps[1].expectation(),
                ObservationPattern::ToolResult { .. }
            ),
            "the recorded call succeeded, so the re-attempt expects a result"
        );
        assert!(matches!(steps[1].action(), AgentAction::CompleteRun { .. }));
    }

    #[test]
    fn a_retry_expects_the_shape_of_outcome_the_recorded_call_produced() {
        let run = RunId::new();
        let calls = vec![recorded(run, ToolCallState::Failed)];

        let scenario = retry_scenario(run, &calls).unwrap();

        assert!(matches!(
            scenario.steps()[1].expectation(),
            ObservationPattern::ToolFailed { error_kind: None }
        ));
    }

    #[test]
    fn a_run_that_recorded_no_call_re_attempts_nothing_and_completes() {
        let run = RunId::new();

        let scenario = retry_scenario(run, &[]).unwrap();

        assert_eq!(scenario.steps().len(), 1);
        assert!(matches!(
            scenario.steps()[0].action(),
            AgentAction::CompleteRun { .. }
        ));
    }

    #[test]
    fn a_run_with_more_calls_than_a_script_may_hold_is_refused_by_name() {
        let run = RunId::new();
        let calls: Vec<ToolCall> = (0..harkness_runtime::agent::MAX_SCENARIO_STEPS)
            .map(|_| recorded(run, ToolCallState::Succeeded))
            .collect();

        let failure = retry_scenario(run, &calls).unwrap_err();

        assert_eq!(failure.kind, "run_not_replayable");
        assert!(
            failure.message.contains("cannot be re-attempted"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn timestamps_render_as_rfc_3339_and_absence_renders_as_nothing() {
        let at = time::OffsetDateTime::from_unix_timestamp(1_755_000_000).unwrap();

        assert_eq!(rfc3339(at), "2025-08-12T12:00:00Z");
        assert_eq!(optional_rfc3339(Some(at)), "2025-08-12T12:00:00Z");
        assert_eq!(optional_rfc3339(None), "");
    }
}
