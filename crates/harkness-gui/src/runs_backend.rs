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
//! `cancelRun`, `retryRun`, `approve`, `deny`, and the on-demand reads that
//! answer one question each: `loadApprovalInput`, `loadRun`, and
//! `loadArtifactExcerpt`.
//!
//! Those three are reads and could have been models. They are not, because each
//! answers a question a surface asks once about one identifier — the input this
//! approval is holding, the shape of this run, the text of this artifact — and a
//! `QAbstractListModel` per single-value answer would be three more paging,
//! staleness and reset mechanisms for rows that never number more than one. They
//! follow `loadApprovalInput`'s shape exactly: a worker reads, a property
//! carries the answer, and the operation that filled it is the only one that
//! writes it.
//!
//! # Reading and attaching are different things
//!
//! Everything this process shares about one data directory lives in
//! [`Attached`]: one `Store`, and at most one `RunCoordinator` above it.
//! A read takes the store ([`read_store`]) and a decision to drive work takes
//! the coordinator ([`coordinator_for`]), and the split is the point — building
//! a coordinator takes this process's lease and runs the recovery sweep, which
//! writes. Looking at run history must not do that; cancelling, retrying, or
//! answering an approval is exactly when it should.
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
//! longer the newest still clears its share of `busy` but never writes `status`
//! or `kind`. Two operations overlapping otherwise let the slower one's message
//! overwrite the faster one's, so the panel would report the outcome of the
//! thing the user did *first*.
//!
//! The *answer* properties are counted separately, one watermark each. `status`
//! is genuinely shared and a newer operation of any kind supersedes it; `run`,
//! `detail` and `excerpt` are three different questions, and a header re-read
//! landing while an artifact excerpt is still being read supersedes nothing
//! about that excerpt. Measuring all four against one counter would drop the
//! reply, leaving the row that asked expanded, empty, and reporting no failure.
//! [`settlement`] is the rule, and it is a pure function so it can be tested
//! without a Qt thread.
//!
//! # `busy` falls last
//!
//! Qt emits `busyChanged` from inside the setter, so a surface reacting to an
//! operation finishing runs *during* [`settle`]. Everything it might read is
//! therefore written first: the shared `status`/`kind` pair, then the answer
//! property, then `busy`. A page that hears the decision it issued settle and
//! reads `kind` to find out how it went would otherwise read the previous
//! operation's answer — and would report a refusal as a success.
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
        /// the newest operation's user-facing message, and `kind` its stable
        /// discriminant (empty on success).
        ///
        /// The other three properties are answers rather than status: `detail`
        /// is the last approval input `loadApprovalInput` fetched, `run` the
        /// last run `loadRun` projected, and `excerpt` the last artifact
        /// rendering `loadArtifactExcerpt` produced. Each is written only by
        /// the loader that answers it, so cancelling the run a page is showing
        /// does not blank the page.
        #[qobject]
        #[qml_element]
        #[qproperty(bool, busy)]
        #[qproperty(QString, status)]
        #[qproperty(QString, kind)]
        #[qproperty(QVariant, detail)]
        #[qproperty(QVariant, run)]
        #[qproperty(QVariant, excerpt)]
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

        /// Projects one run into the `run` property: header fields, whether a
        /// retry is available and why not, and its steps, calls, approvals and
        /// artifact metadata.
        ///
        /// A read, so it goes through the store rather than attaching a
        /// coordinator. Nothing here is decided by this bridge — retry
        /// eligibility is read back off the same durable state
        /// `RunCoordinator::retry_run` refuses on, so the button a page offers
        /// and the answer the runtime gives cannot disagree except by racing.
        #[qinvokable]
        #[cxx_name = "loadRun"]
        fn load_run(self: Pin<&mut RunsBackend>, run_id: &QString);

        /// Renders one small text artifact into the `excerpt` property.
        ///
        /// Bounded on the bytes actually read, not on the recorded size, and
        /// refused by name for anything that is not text this build can render.
        /// Artifact content is tool output: it is never executed, never opened
        /// through the desktop, and reaches QML as plain text.
        #[qinvokable]
        #[cxx_name = "loadArtifactExcerpt"]
        fn load_artifact_excerpt(self: Pin<&mut RunsBackend>, artifact_id: &QString);
    }

    impl cxx_qt::Threading for RunsBackend {}
}

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use harkness_runtime::agent::{
    AgentAction, MockAgent, ObservationPattern, Scenario, ScenarioId, ScenarioStep, WorkspaceRef,
};
use harkness_runtime::approval::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalScope, ApprovalState, DecidedVia,
};
use harkness_runtime::coordinator::{RunCoordinator, RuntimeError};
use harkness_runtime::domain::{ArtifactId, ExecutionState, Run, RunId, ToolCall, ToolCallState};
use harkness_runtime::store::{Artifact, Availability, EventPage, PassThrough, Store, StoreError};
use harkness_runtime::tool::WorkspaceMetadata;

use super::approval_model::ApprovalRow;
use super::run_list_model::{RunRow, run_row};
use super::run_timeline_model::{TOOL_PROGRESS_KIND, summarize};

/// Largest approval input this bridge hands to QML.
///
/// A stored input is already held to the run store's 64 KiB inline bound, so
/// this is not what keeps it finite; it keeps a single delegate from being
/// asked to lay out sixty kilobytes of JSON. What is dropped is named in the
/// projection rather than silently missing.
const MAX_APPROVAL_INPUT_BYTES: usize = 8 * 1024;

/// Events scanned backwards from the tip when looking for a running call's
/// newest progress line.
///
/// A run is bounded by nothing in particular, so "the last thing this call
/// said" is read from one bounded page at the tip of the log rather than from
/// the whole of it. A call that is genuinely executing reported its progress
/// recently by construction, and a call whose newest tick fell off this page
/// simply shows none — the timeline beside the row still holds every one of
/// them.
const MAX_PROGRESS_SCAN_EVENTS: usize = 200;

/// Rows of each kind a run's detail projection carries.
///
/// A run is bounded by nothing in particular — an agent may make thousands of
/// calls and store an artifact per call — and the header of a page is not a
/// place to page through them. What is dropped is named in `truncated` rather
/// than silently missing, and the timeline beside the header is the surface that
/// does page.
const MAX_RUN_DETAIL_ROWS: usize = 200;

/// Largest artifact rendering this bridge hands to QML.
///
/// Enforced on the bytes *read* rather than on the recorded size, because the
/// file is on disk where anything may have rewritten it since; the recorded size
/// only decides whether it is worth opening at all.
const MAX_ARTIFACT_EXCERPT_BYTES: usize = 8 * 1024;

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
/// An artifact identifier QML supplied is not one.
pub(crate) const INVALID_ARTIFACT_ID: &str = "invalid_artifact_id";
/// An artifact is not text this build is willing to render inline.
pub(crate) const ARTIFACT_NOT_TEXT: &str = "artifact_not_text";

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
pub(crate) const BRIDGE_KINDS: [&str; 9] = [
    DATA_DIRECTORY_UNAVAILABLE,
    COORDINATOR_UNAVAILABLE,
    NO_RUN_STORE,
    INVALID_RUN_ID,
    INVALID_APPROVAL_ID,
    UNKNOWN_APPROVAL_SCOPE,
    RUN_NOT_REPLAYABLE,
    INVALID_ARTIFACT_ID,
    ARTIFACT_NOT_TEXT,
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

/// What this process has attached to one data directory.
///
/// One `Store` per directory and at most one `RunCoordinator` above it, behind
/// one lock so there is no order to get wrong between them. Two stores would
/// mean two write mutexes inside one process, which is safe — SQLite serializes
/// them — but turns an in-process wait into a `busy_timeout` for no reason. Two
/// coordinators would be worse: two leases, child-process caps that do not see
/// each other, and — because `RunCoordinator::cancel_run` only knows the runs
/// its own coordinator started — every run one of them started uncancellable
/// from the other.
///
/// Keyed by data directory rather than held as single values: the directory is
/// chosen at startup, but a test process can drive more than one.
#[derive(Default)]
struct Attached {
    stores: HashMap<PathBuf, Arc<Store>>,
    coordinators: HashMap<PathBuf, RunCoordinator>,
}

static ATTACHED: OnceLock<Mutex<Attached>> = OnceLock::new();

fn attached() -> Result<std::sync::MutexGuard<'static, Attached>, RunsFailure> {
    ATTACHED
        .get_or_init(|| Mutex::new(Attached::default()))
        .lock()
        .map_err(|_| {
            RunsFailure::new(
                COORDINATOR_UNAVAILABLE,
                "the run store cache is poisoned; restart Harkness",
            )
        })
}

/// The store this process reads run history out of.
///
/// `None` means the data directory has recorded nothing at all. A read must
/// never be what brings a run store into existence — the same rule the checks
/// panel follows — so a directory with no `runtime.db` is left exactly as it
/// was.
///
/// # A read attaches to a store and nothing else
///
/// This deliberately does *not* build a `RunCoordinator`. Constructing one
/// takes this process's lease and runs the startup sweep, which marks every run
/// whose owning process is provably dead as `interrupted` and appends an event
/// saying so — writes, on a path a user reached by opening a panel to look at
/// something. The reads this bridge performs need none of it: paging runs,
/// paging a timeline, listing pending approvals and reading one call's input
/// are all `Store` queries, and the one read that genuinely needs a coordinator
/// — subscribing to a live run — settles for [`cached_coordinator`] instead.
///
/// The cost is that a run abandoned by a dead process keeps reading as
/// `running` until something in this process actually drives work. That is what
/// is recorded, and correcting it is a decision with side effects rather than a
/// side effect of looking.
pub(crate) fn read_store(data_dir: &Path) -> Result<Option<Arc<Store>>, RunsFailure> {
    assert_off_qt_thread();
    let mut attached = attached()?;
    open_store(&mut attached, data_dir, false)
}

/// The coordinator already built for a directory, never building one.
///
/// The subscription seam. A run this process is not driving cannot publish to
/// it anyway — `RunCoordinator::subscribe` hands back a receiver that replays
/// the durable history and then waits for a worker that does not exist — so
/// declining to build one here costs a timeline nothing it could have had.
pub(crate) fn cached_coordinator(data_dir: &Path) -> Result<Option<RunCoordinator>, RunsFailure> {
    assert_off_qt_thread();
    Ok(attached()?.coordinators.get(data_dir).cloned())
}

/// This process's run coordinator for one data directory, built if needed.
///
/// Reserved for the paths that drive work: starting a check, cancelling,
/// retrying, and answering an approval. Each of those is a decision the user
/// made, which is the right moment to take the lease and sweep.
pub(crate) fn coordinator_for(data_dir: &Path) -> Result<RunCoordinator, RunsFailure> {
    coordinator_in(data_dir, true).map(|coordinator| {
        coordinator.expect("a coordinator asked to be created is returned or refused")
    })
}

/// The thread every bridge method runs on.
///
/// Recorded from the Qt thread itself — see [`note_qt_thread`] — so the places
/// a store is reached from can assert they are somewhere else. There is no
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
/// with a large history watches the window freeze. Asserting where every read
/// and write passes through is what turns "we were careful" into something a
/// test run can fail on.
fn assert_off_qt_thread() {
    debug_assert!(
        QT_THREAD.get() != Some(&std::thread::current().id()),
        "the run store was reached from the Qt thread; every store and \
         coordinator call belongs on a std::thread::spawn worker"
    );
}

/// Opens and caches the one store for `data_dir`, creating it only if asked.
fn open_store(
    attached: &mut Attached,
    data_dir: &Path,
    create: bool,
) -> Result<Option<Arc<Store>>, RunsFailure> {
    if let Some(existing) = attached.stores.get(data_dir) {
        return Ok(Some(Arc::clone(existing)));
    }
    // Opened once and kept. `Store::open_existing` is not a cheap probe — it
    // opens the connection, enables WAL, and climbs the whole migration ladder
    // — so discarding it and opening again would pay for all of that twice.
    let store = if create {
        Store::open(data_dir)?
    } else {
        match Store::open_existing(data_dir)? {
            Some(store) => store,
            None => return Ok(None),
        }
    };
    let store = Arc::new(store);
    attached
        .stores
        .insert(data_dir.to_path_buf(), Arc::clone(&store));
    Ok(Some(store))
}

fn coordinator_in(data_dir: &Path, create: bool) -> Result<Option<RunCoordinator>, RunsFailure> {
    assert_off_qt_thread();
    let mut attached = attached()?;
    if let Some(existing) = attached.coordinators.get(data_dir) {
        return Ok(Some(existing.clone()));
    }
    let Some(store) = open_store(&mut attached, data_dir, create)? else {
        return Ok(None);
    };
    let coordinator = harkness_runtime::check::check_coordinator(store)
        .map_err(|error| RunsFailure::new(COORDINATOR_UNAVAILABLE, error.to_string()))?;
    attached
        .coordinators
        .insert(data_dir.to_path_buf(), coordinator.clone());
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

/// The coordinator a mutation goes through.
///
/// A mutation names a run or an approval, so a data directory with no run store
/// cannot be holding either; saying so is more useful than creating a store to
/// discover it is empty. Where one *does* exist, this is the call that attaches
/// to it — takes the lease, sweeps — because cancelling, retrying, or answering
/// an approval is this process deciding to drive work.
fn write_coordinator() -> Result<RunCoordinator, RunsFailure> {
    coordinator_in(&data_dir()?, false)?.ok_or_else(|| {
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

fn parse_artifact(value: &str) -> Result<ArtifactId, RunsFailure> {
    value.parse().map_err(|_| {
        RunsFailure::new(
            INVALID_ARTIFACT_ID,
            format!("{value:?} is not an artifact id"),
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

/// Which of the bridge's three answer properties an operation may write.
///
/// `detail`, `run` and `excerpt` answer three different questions about three
/// different identifiers, and an operation must never blank the answer to a
/// question it was not asked: cancelling the run a detail page is showing would
/// otherwise empty the page it was pressed from.
///
/// A decision is the one case that writes an answer it did not produce. It
/// clears `detail`, because approving or denying changes *which* approval is in
/// question and leaving the previous one's validated input bound is the worst
/// way for that property to be wrong. It does not clear `run` or `excerpt`,
/// which are about the run the decision was made on.
///
/// This is also why staleness is counted *per answer* and not once for the
/// bridge. Three independent questions share one counter only if every newer
/// operation genuinely supersedes every older one, and here they do not: a
/// header re-read arriving while an artifact excerpt is still being read says
/// nothing about the excerpt, and dropping its reply would leave the row
/// expanded, empty, and reporting no failure. Each slot therefore carries its
/// own watermark — see [`settlement`] — while `status` and `kind`, which really
/// are one shared pair, stay keyed on the newest operation of any kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Answer {
    /// A mutation: writes no answer of its own and clears `detail`.
    Decision,
    /// `loadApprovalInput`, which writes `detail`.
    ApprovalInput,
    /// `loadRun`, which writes `run`.
    RunDetail,
    /// `loadArtifactExcerpt`, which writes `excerpt`.
    ArtifactExcerpt,
}

/// How many answer properties an operation's staleness is tracked against.
///
/// Three, not four: `Decision` and `ApprovalInput` both write `detail`, so they
/// supersede one another and share a slot.
const ANSWER_SLOTS: usize = 3;

impl Answer {
    /// Which answer property this operation writes, as an index into
    /// [`RunsBackendRust::newest`].
    fn slot(self) -> usize {
        match self {
            Self::Decision | Self::ApprovalInput => 0,
            Self::RunDetail => 1,
            Self::ArtifactExcerpt => 2,
        }
    }
}

/// What one settling reply is still entitled to write.
///
/// Two questions rather than one, because this bridge answers three independent
/// questions about three different identifiers and reports *one* status. A
/// reply that is no longer the newest operation of any kind must not describe
/// itself in `status` — the panel would report the outcome of the thing the
/// user did first — but it is still the only answer its own question is ever
/// going to get, and dropping it leaves an empty panel with nothing said about
/// why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Settlement {
    /// Whether this reply is still the newest question of its own kind, and so
    /// may write the one answer property it owns.
    pub(crate) answer: bool,
    /// Whether it is the newest operation of any kind, and so may write the
    /// single `status`/`kind` pair the three of them share.
    pub(crate) status: bool,
}

/// Decides what a reply may write, from the two watermarks it is measured
/// against.
///
/// Split out and pure so the rule is testable: `settle` needs a pinned
/// `QObject` and a running Qt thread, and the rule is the part that is easy to
/// get wrong. `status` implies `answer` — the newest operation overall is by
/// construction the newest of its own kind — so the two are never both false
/// for a reply that still matters.
pub(crate) fn settlement(request: u64, newest_for_answer: u64, newest_overall: u64) -> Settlement {
    Settlement {
        answer: request == newest_for_answer,
        status: request == newest_overall,
    }
}

/// The answer one finished operation produced, if it produced one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum Answered {
    /// A mutation, or a load that failed.
    #[default]
    Nothing,
    ApprovalInput(ApprovalInput),
    /// Boxed: a run detail carries four bounded row lists and is much larger
    /// than every other variant, which would otherwise be the size of all of
    /// them.
    RunDetail(Box<RunDetail>),
    ArtifactExcerpt(ArtifactExcerpt),
}

/// What a finished operation has to say for itself, in `Send` data only.
///
/// `QString` and `QVariant` are not `Send`, so a worker returns plain Rust and
/// the Qt-thread closure is what turns it into something QML can bind to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Completion {
    message: String,
    answered: Answered,
}

impl Completion {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            answered: Answered::Nothing,
        }
    }

    fn answering(message: impl Into<String>, answered: Answered) -> Self {
        Self {
            message: message.into(),
            answered,
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

/// Reads the input one pending approval is holding.
///
/// A read, so it goes through the store rather than attaching a coordinator:
/// opening the dialog that asks a question must not be what takes this
/// process's lease on the runs somebody else is driving.
fn approval_input(approval: ApprovalId) -> Result<Completion, RunsFailure> {
    let Some(store) = read_store(&data_dir()?)? else {
        return Err(RunsFailure::new(
            NO_RUN_STORE,
            "this data directory has recorded no runs yet",
        ));
    };
    let request = store.approval(approval)?;
    let call = store.load_tool_call(request.tool_call_id())?;
    let rendered =
        serde_json::to_string_pretty(call.input()).unwrap_or_else(|_| call.input().to_string());
    let (input, truncated) = clamp(rendered, MAX_APPROVAL_INPUT_BYTES);
    Ok(Completion::answering(
        format!("Loaded the input {} is holding", request.tool()),
        Answered::ApprovalInput(ApprovalInput {
            approval_id: approval.to_string(),
            run_id: request.run_id().to_string(),
            tool: request.tool().to_string(),
            input,
            truncated,
        }),
    ))
}

// ---------------------------------------------------------------------------
// One run's detail
// ---------------------------------------------------------------------------

/// One step of a run as its detail page draws it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StepRow {
    step_id: String,
    ordinal: u32,
    title: String,
    state: String,
    terminal: bool,
    created: String,
    started: String,
    finished: String,
    error_kind: String,
    error_message: String,
}

/// One recorded tool call as its detail page draws it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CallRow {
    tool_call_id: String,
    step_id: String,
    tool_id: String,
    tool_version: String,
    state: String,
    terminal: bool,
    created: String,
    started: String,
    finished: String,
    error_kind: String,
    error_message: String,
    verdict: String,
    reason: String,
    source: String,
    /// Newest progress line this call reported, for a call that has not
    /// finished; empty for every other call.
    ///
    /// Only a call still in flight has a "latest" worth a row of its own. A
    /// finished call's progress is history, and history is the timeline's
    /// subject rather than a line that would sit under a terminal state pill
    /// claiming to be current.
    progress: String,
}

/// One approval of a run, with the answer it did or did not receive.
///
/// The queue model's row plus the fields only a *history* needs. The queue lists
/// unanswered requests, so it has no reason to carry a state or a verdict; a run
/// that ended because somebody said no has every reason to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunApprovalRow {
    row: ApprovalRow,
    state: String,
    pending: bool,
    verdict: String,
    decided_via: String,
    decided_at: String,
    reason: String,
}

/// One artifact's metadata as its detail page draws it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ArtifactRow {
    artifact_id: String,
    step_id: String,
    tool_call_id: String,
    name: String,
    media_type: String,
    byte_size: u64,
    availability: String,
    created: String,
    /// Where the bytes are, rebuilt from the two identifiers as the store does.
    path: String,
    /// Whether [`artifact_excerpt`] would render this one inline.
    excerptable: bool,
}

/// One run, everything under it, and what may still be done to it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunDetail {
    /// The header's fields, spelled exactly as the run list model's roles.
    run: RunRow,
    retryable: bool,
    /// Why a retry is unavailable, empty when it is available.
    ///
    /// The runtime's own discriminants, because this is a read of the state
    /// `RunCoordinator::retry_run` refuses on rather than a second opinion.
    retry_blocked: &'static str,
    /// Later attempts at the same task, oldest first — the order
    /// `Store::retries_of` lists them in, which is the order they were made.
    retries: Vec<String>,
    steps: Vec<StepRow>,
    calls: Vec<CallRow>,
    approvals: Vec<RunApprovalRow>,
    artifacts: Vec<ArtifactRow>,
    /// Which collections [`MAX_RUN_DETAIL_ROWS`] cut short, by name.
    truncated: Vec<&'static str>,
}

/// One artifact rendered as bounded text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactExcerpt {
    artifact_id: String,
    name: String,
    media_type: String,
    text: String,
    /// Whether [`MAX_ARTIFACT_EXCERPT_BYTES`] cut the rendering short.
    truncated: bool,
}

/// Whether this build is willing to render an artifact's bytes as text.
///
/// A deliberate allowlist rather than a sniff of the content. `media_type` is
/// what the producing tool declared, and the question being answered is "may
/// this be shown in a label", which nothing about the bytes themselves decides.
fn is_renderable_text(media_type: &str) -> bool {
    let media_type = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media_type.starts_with("text/")
        || media_type.ends_with("+json")
        || matches!(
            media_type.as_str(),
            "application/json" | "application/x-ndjson" | "application/xml"
        )
}

/// Projects one step into a row.
fn step_row(step: &harkness_runtime::domain::Step) -> StepRow {
    StepRow {
        step_id: step.id().to_string(),
        ordinal: step.ordinal(),
        title: step.title().to_owned(),
        state: step.state().as_str().to_owned(),
        terminal: step.state().is_terminal(),
        created: rfc3339(step.created_at()),
        started: optional_rfc3339(step.started_at()),
        finished: optional_rfc3339(step.finished_at()),
        error_kind: step
            .failure()
            .map(|failure| failure.kind().to_owned())
            .unwrap_or_default(),
        error_message: step
            .failure()
            .map(|failure| failure.message().to_owned())
            .unwrap_or_default(),
    }
}

/// Projects one recorded call into a row.
///
/// The call's input and output are deliberately absent. A call's input is what
/// `loadApprovalInput` fetches for the one call a question is being asked about,
/// and its output may be the whole of a tool's result; carrying either for every
/// call of a run would put megabytes behind a header.
fn call_row(call: &ToolCall, progress: &HashMap<String, String>) -> CallRow {
    CallRow {
        tool_call_id: call.id().to_string(),
        step_id: call.step_id().to_string(),
        tool_id: call.tool_id().to_owned(),
        tool_version: call.tool_version().to_owned(),
        state: call.state().as_str().to_owned(),
        terminal: call.state().is_terminal(),
        created: rfc3339(call.created_at()),
        started: optional_rfc3339(call.started_at()),
        finished: optional_rfc3339(call.finished_at()),
        error_kind: call
            .failure()
            .map(|failure| failure.kind().to_owned())
            .unwrap_or_default(),
        error_message: call
            .failure()
            .map(|failure| failure.message().to_owned())
            .unwrap_or_default(),
        verdict: call
            .policy_decision()
            .map(|decision| decision.verdict().as_str().to_owned())
            .unwrap_or_default(),
        reason: call
            .policy_decision()
            .map(|decision| decision.reason().to_owned())
            .unwrap_or_default(),
        source: call
            .policy_decision()
            .map(|decision| decision.source().as_str().to_owned())
            .unwrap_or_default(),
        progress: progress
            .get(&call.id().to_string())
            .cloned()
            .unwrap_or_default(),
    }
}

/// The newest progress line each unfinished call of `run` reported.
///
/// Empty — and, more to the point, free — when every call has finished, which
/// is what a page of run history is looking at. Only then is one bounded page
/// of the log read, newest first, so the first tick naming a call is that
/// call's latest.
fn latest_progress(
    store: &Store,
    run: RunId,
    calls: &[ToolCall],
) -> Result<HashMap<String, String>, RunsFailure> {
    let mut wanted: HashMap<String, ()> = calls
        .iter()
        .filter(|call| !call.state().is_terminal())
        .map(|call| (call.id().to_string(), ()))
        .collect();
    let mut newest = HashMap::new();
    if wanted.is_empty() {
        return Ok(newest);
    }
    let listing = store.event_page(run, EventPage::newest(MAX_PROGRESS_SCAN_EVENTS))?;
    for stored in &listing.events {
        if stored.event.kind().as_str() != TOOL_PROGRESS_KIND {
            continue;
        }
        let Some(call) = stored.event.tool_call_id() else {
            continue;
        };
        let call = call.to_string();
        // The page is newest first, so the first tick naming a call is the one
        // the row shows; the rest of that call's ticks are the timeline's.
        if wanted.remove(&call).is_some() {
            newest.insert(call, summarize(stored.event.payload()));
            if wanted.is_empty() {
                break;
            }
        }
    }
    Ok(newest)
}

/// Projects one durable approval into a row, answer and all.
fn run_approval_row(request: &ApprovalRequest) -> RunApprovalRow {
    let decision = request.decision();
    RunApprovalRow {
        row: super::approval_model::approval_row(request),
        state: request.state().as_str().to_owned(),
        pending: request.state() == ApprovalState::Pending,
        verdict: decision
            .map(|decision| decision.verdict().as_str().to_owned())
            .unwrap_or_default(),
        decided_via: decision
            .map(|decision| decision.decided_via().as_str().to_owned())
            .unwrap_or_default(),
        decided_at: decision
            .map(|decision| rfc3339(decision.decided_at()))
            .unwrap_or_default(),
        reason: decision
            .and_then(|decision| decision.reason())
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Projects one artifact's metadata into a row.
fn artifact_row(data_dir: &Path, artifact: &Artifact) -> ArtifactRow {
    ArtifactRow {
        artifact_id: artifact.id().to_string(),
        step_id: artifact
            .step_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        tool_call_id: artifact
            .tool_call_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        name: artifact.name().to_owned(),
        media_type: artifact.media_type().to_owned(),
        byte_size: artifact.byte_size(),
        availability: artifact.availability().as_str().to_owned(),
        created: rfc3339(artifact.created_at()),
        // Rebuilt from the two identifiers, exactly as the store rebuilds it
        // rather than trusting the path it stored. Shown and copied; never
        // opened, and never handed to a desktop launcher.
        path: data_dir
            .join("artifacts")
            .join(artifact.run_id().to_string())
            .join(artifact.id().to_string())
            .display()
            .to_string(),
        excerptable: artifact.availability() == Availability::Available
            && is_renderable_text(artifact.media_type())
            && artifact.byte_size() <= MAX_ARTIFACT_EXCERPT_BYTES as u64,
    }
}

/// Keeps a projected collection inside its bound, naming it when it cut one.
fn bounded<T>(mut rows: Vec<T>, name: &'static str, truncated: &mut Vec<&'static str>) -> Vec<T> {
    if rows.len() > MAX_RUN_DETAIL_ROWS {
        rows.truncate(MAX_RUN_DETAIL_ROWS);
        truncated.push(name);
    }
    rows
}

/// Reads one run and everything recorded under it.
fn run_detail(run: RunId) -> Result<Completion, RunsFailure> {
    run_detail_in(&data_dir()?, run)
}

/// Reads one run's detail from a named data directory.
///
/// Split from [`run_detail`] so a test can seed a temporary store and read it
/// back without touching `HARKNESS_DATA_DIR`, which is process-wide.
///
/// A read, so it goes through the store rather than attaching a coordinator:
/// opening a page to look at what a run did must not take this process's lease
/// and mark somebody else's runs interrupted.
fn run_detail_in(data_dir: &Path, run: RunId) -> Result<Completion, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Err(RunsFailure::new(
            NO_RUN_STORE,
            "this data directory has recorded no runs yet",
        ));
    };
    let record = store.load_run(run)?;
    let recorded_calls = store.load_run_tool_calls(run)?;
    // Read before the projection is built, and only when something is still in
    // flight: a page of finished history pays nothing for it.
    let progress = latest_progress(&store, run, &recorded_calls)?;
    // A run whose task row is unreadable is exactly the run somebody needs to
    // see, so the header loses its title and workspace rather than the page
    // failing to open — the same rule the list model's rows follow.
    let task = store.load_task(record.task_id()).ok();
    let mut truncated = Vec::new();
    // Retry eligibility is read off the same durable state `retry_run` refuses
    // on, in the same order, so the button and the runtime cannot disagree
    // about *why*. It is still a read of a moment: a run that finishes between
    // this and the press is refused by the coordinator, which is the authority.
    let retries: Vec<Run> = store
        .retries_of(run)?
        .into_iter()
        .filter_map(|attempt| store.load_run(attempt).ok())
        .collect();
    let retry_blocked = if !record.state().is_terminal() {
        "run_still_active"
    } else if record.state() == ExecutionState::Succeeded {
        "run_not_retryable"
    } else if retries.iter().any(|attempt| !attempt.state().is_terminal()) {
        "run_still_active"
    } else {
        ""
    };
    let detail = RunDetail {
        run: run_row(&record, task.as_ref()),
        retryable: retry_blocked.is_empty(),
        retry_blocked,
        retries: retries
            .iter()
            .map(|attempt| attempt.id().to_string())
            .collect(),
        steps: bounded(
            store.load_run_steps(run)?.iter().map(step_row).collect(),
            "steps",
            &mut truncated,
        ),
        calls: bounded(
            recorded_calls
                .iter()
                .map(|call| call_row(call, &progress))
                .collect(),
            "calls",
            &mut truncated,
        ),
        // Every approval the run recorded, not only the unanswered ones: a
        // denial is the reason a run ended, and a page that showed only what is
        // still pending would render a denied run with nothing to explain it.
        approvals: bounded(
            store
                .run_approvals(run)?
                .iter()
                .map(run_approval_row)
                .collect(),
            "approvals",
            &mut truncated,
        ),
        // Metadata only. `run_artifacts` probes each file's presence and size
        // and opens none of them, so a run with a gigabyte of output costs this
        // read one query.
        artifacts: bounded(
            store
                .run_artifacts(run)?
                .iter()
                .map(|artifact| artifact_row(store.data_dir(), artifact))
                .collect(),
            "artifacts",
            &mut truncated,
        ),
        truncated,
    };
    Ok(Completion::answering(
        format!("Loaded run {run}"),
        Answered::RunDetail(Box::new(detail)),
    ))
}

/// Renders one small text artifact.
fn artifact_excerpt(artifact: ArtifactId) -> Result<Completion, RunsFailure> {
    artifact_excerpt_in(&data_dir()?, artifact)
}

/// Renders one artifact from a named data directory.
///
/// Refused by name rather than rendered as replacement characters when the
/// content is not text this build declares renderable: an excerpt is shown in a
/// label, and lossy bytes in a label are a worse answer than "this is not
/// something to read here".
fn artifact_excerpt_in(data_dir: &Path, artifact: ArtifactId) -> Result<Completion, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Err(RunsFailure::new(
            NO_RUN_STORE,
            "this data directory has recorded no runs yet",
        ));
    };
    let record = store.artifact(artifact)?;
    if !is_renderable_text(record.media_type()) {
        return Err(RunsFailure::new(
            ARTIFACT_NOT_TEXT,
            format!(
                "{} is {}, which Harkness does not render inline",
                record.name(),
                record.media_type()
            ),
        ));
    }
    // Bounded on what is read rather than on the recorded size. The recorded
    // size is what the file was at finalization, and the file is on disk where
    // anything may have grown it since; reading one byte past the budget is
    // what tells the reader it was cut.
    let mut bytes = Vec::new();
    let mut reader = store
        .open_artifact(artifact)?
        .take(MAX_ARTIFACT_EXCERPT_BYTES as u64 + 1);
    reader.read_to_end(&mut bytes).map_err(|error| {
        RunsFailure::new(
            ARTIFACT_NOT_TEXT,
            format!("{} could not be read: {error}", record.name()),
        )
    })?;
    let truncated = bytes.len() > MAX_ARTIFACT_EXCERPT_BYTES;
    bytes.truncate(MAX_ARTIFACT_EXCERPT_BYTES);
    // A truncation lands wherever the budget ran out, which is commonly inside
    // a character, so the tail is dropped rather than replaced: one lost glyph
    // beats a replacement character that reads as corruption in the file.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).expect("truncated at a boundary the error reported")
        }
        Err(_) => {
            return Err(RunsFailure::new(
                ARTIFACT_NOT_TEXT,
                format!(
                    "{} declares {} but is not valid UTF-8",
                    record.name(),
                    record.media_type()
                ),
            ));
        }
    };
    Ok(Completion::answering(
        format!("Loaded {}", record.name()),
        Answered::ArtifactExcerpt(ArtifactExcerpt {
            artifact_id: artifact.to_string(),
            name: record.name().to_owned(),
            media_type: record.media_type().to_owned(),
            text,
            truncated,
        }),
    ))
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

/// The catalog's own view of the project a task named, when it still has one.
///
/// Best effort by design: a retry must not be refused because the catalog has
/// moved on, and every failure here — no catalog, no such project, an
/// unreadable file — means the same thing, that there is no authoritative
/// metadata to pass.
fn catalog_metadata(project: harkness_core::ProjectId) -> Option<WorkspaceMetadata> {
    let service = harkness_core::ProjectService::load().ok()?;
    let project = service
        .resolve(&harkness_core::ProjectSelector::from(project.to_string()))
        .ok()?;
    Some(WorkspaceMetadata::from_project(&project))
}

/// Names the one failure a reader would otherwise have to guess at.
///
/// `WorkspaceMismatch` means the reference this bridge built is not the one the
/// coordinator rebuilt from the same task, and there is exactly one way that
/// happens: the store acquired a redactor, so the recorded workspace text no
/// longer round-trips through `PassThrough`. `Store::redactor` is not public,
/// so a front end cannot ask what the store would produce — the mismatch is the
/// only signal there is, and it is worth spending it on saying so.
fn retry_failure(error: RuntimeError) -> RunsFailure {
    match error {
        RuntimeError::WorkspaceMismatch { .. } => RunsFailure::new(
            error.kind(),
            format!(
                "{error}; the run store redacts workspace text this build cannot \
                 reproduce, so retrying from the application is not available"
            ),
        ),
        other => RunsFailure::from(other),
    }
}

pub struct RunsBackendRust {
    busy: bool,
    status: QString,
    kind: QString,
    detail: QVariant,
    run: QVariant,
    excerpt: QVariant,
    /// Monotonic operation number; only the newest reply writes a message.
    next_request: u64,
    /// The newest operation number issued against each answer property, indexed
    /// by [`Answer::slot`]. A reply writes its answer only while it is still
    /// the one this names.
    newest: [u64; ANSWER_SLOTS],
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
            run: QVariant::default(),
            excerpt: QVariant::default(),
            next_request: 0,
            newest: [0; ANSWER_SLOTS],
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

/// One run's projection as QML binds to it.
fn run_variant(detail: &RunDetail) -> QVariant {
    let mut map = super::run_list_model::row_map(&detail.run);
    map.insert(
        QString::from("retryable"),
        QVariant::from(&detail.retryable),
    );
    map.insert(
        QString::from("retryBlocked"),
        QVariant::from(&QString::from(detail.retry_blocked)),
    );
    map.insert(QString::from("retries"), strings(&detail.retries));
    map.insert(
        QString::from("steps"),
        list(&detail.steps, |row| {
            let mut map = QMap::<QMapPair_QString_QVariant>::default();
            text(&mut map, "stepId", &row.step_id);
            text(&mut map, "title", &row.title);
            text(&mut map, "state", &row.state);
            text(&mut map, "created", &row.created);
            text(&mut map, "started", &row.started);
            text(&mut map, "finished", &row.finished);
            text(&mut map, "errorKind", &row.error_kind);
            text(&mut map, "errorMessage", &row.error_message);
            map.insert(QString::from("terminal"), QVariant::from(&row.terminal));
            map.insert(
                QString::from("ordinal"),
                QVariant::from(&i32::try_from(row.ordinal).unwrap_or(i32::MAX)),
            );
            map
        }),
    );
    map.insert(
        QString::from("calls"),
        list(&detail.calls, |row| {
            let mut map = QMap::<QMapPair_QString_QVariant>::default();
            text(&mut map, "toolCallId", &row.tool_call_id);
            text(&mut map, "stepId", &row.step_id);
            text(&mut map, "toolId", &row.tool_id);
            text(&mut map, "toolVersion", &row.tool_version);
            text(&mut map, "state", &row.state);
            text(&mut map, "created", &row.created);
            text(&mut map, "started", &row.started);
            text(&mut map, "finished", &row.finished);
            text(&mut map, "errorKind", &row.error_kind);
            text(&mut map, "errorMessage", &row.error_message);
            text(&mut map, "verdict", &row.verdict);
            text(&mut map, "reason", &row.reason);
            text(&mut map, "source", &row.source);
            text(&mut map, "progress", &row.progress);
            map.insert(QString::from("terminal"), QVariant::from(&row.terminal));
            map
        }),
    );
    map.insert(
        QString::from("approvals"),
        list(&detail.approvals, |row| {
            let mut map = super::approval_model::row_map(&row.row);
            text(&mut map, "state", &row.state);
            text(&mut map, "verdict", &row.verdict);
            text(&mut map, "decidedVia", &row.decided_via);
            text(&mut map, "decidedAt", &row.decided_at);
            text(&mut map, "reason", &row.reason);
            map.insert(QString::from("pending"), QVariant::from(&row.pending));
            map
        }),
    );
    map.insert(
        QString::from("artifacts"),
        list(&detail.artifacts, |row| {
            let mut map = QMap::<QMapPair_QString_QVariant>::default();
            text(&mut map, "artifactId", &row.artifact_id);
            text(&mut map, "stepId", &row.step_id);
            text(&mut map, "toolCallId", &row.tool_call_id);
            text(&mut map, "name", &row.name);
            text(&mut map, "mediaType", &row.media_type);
            text(&mut map, "availability", &row.availability);
            text(&mut map, "created", &row.created);
            text(&mut map, "path", &row.path);
            map.insert(
                QString::from("byteSize"),
                QVariant::from(&i64::try_from(row.byte_size).unwrap_or(i64::MAX)),
            );
            map.insert(
                QString::from("excerptable"),
                QVariant::from(&row.excerptable),
            );
            map
        }),
    );
    map.insert(QString::from("truncated"), strings(&detail.truncated));
    QVariant::from(&map)
}

/// One artifact rendering as QML binds to it.
fn excerpt_variant(excerpt: &ArtifactExcerpt) -> QVariant {
    let mut map = QMap::<QMapPair_QString_QVariant>::default();
    text(&mut map, "artifactId", &excerpt.artifact_id);
    text(&mut map, "name", &excerpt.name);
    text(&mut map, "mediaType", &excerpt.media_type);
    text(&mut map, "text", &excerpt.text);
    map.insert(
        QString::from("truncated"),
        QVariant::from(&excerpt.truncated),
    );
    QVariant::from(&map)
}

/// Inserts one string field, which is most of what these projections are.
fn text(map: &mut QMap<QMapPair_QString_QVariant>, key: &str, value: &str) {
    map.insert(QString::from(key), QVariant::from(&QString::from(value)));
}

/// Projects a slice into the `QVariantList` a QML repeater binds to.
fn list<T>(rows: &[T], project: impl Fn(&T) -> QMap<QMapPair_QString_QVariant>) -> QVariant {
    let mut items = QList::<QVariant>::default();
    for row in rows {
        items.append(QVariant::from(&project(row)));
    }
    QVariant::from(&items)
}

/// Projects a slice of names into a `QVariantList` of plain strings.
pub(crate) fn strings(values: &[impl AsRef<str>]) -> QVariant {
    let mut items = QList::<QVariant>::default();
    for value in values {
        items.append(QVariant::from(&QString::from(value.as_ref())));
    }
    QVariant::from(&items)
}

/// Applies one finished operation on the Qt thread.
fn settle(
    mut backend: Pin<&mut ffi::RunsBackend>,
    request: u64,
    answer: Answer,
    outcome: Result<Completion, RunsFailure>,
) {
    let settled = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.pending = rust.pending.saturating_sub(1);
        settlement(request, rust.newest[answer.slot()], rust.next_request)
    };
    // `status` and `kind` are one pair shared by every operation, so only the
    // newest of any kind may write them: two operations overlapping otherwise
    // let the slower one's message overwrite the faster one's, and the panel
    // would report the outcome of the thing the user did *first*.
    if settled.status {
        match &outcome {
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
    // Exactly one answer property is written, and only by the operation that
    // answers it. A failed load blanks the property it was loading rather than
    // leaving the previous subject's answer standing under a new question —
    // see `Answer` for why a decision blanks `detail` as well.
    //
    // A reply superseded on its own question has nothing left to say: another
    // load of the same kind is already on its way to the same property, and
    // writing this one would show the older subject and then replace it.
    if settled.answer {
        let answered = match &outcome {
            Ok(completion) => &completion.answered,
            Err(_) => &Answered::Nothing,
        };
        match answer {
            Answer::Decision | Answer::ApprovalInput => {
                let detail = match answered {
                    Answered::ApprovalInput(input) => approval_detail(input),
                    _ => QVariant::default(),
                };
                backend.as_mut().set_detail(detail);
            }
            Answer::RunDetail => {
                let run = match answered {
                    Answered::RunDetail(detail) => run_variant(detail),
                    _ => QVariant::default(),
                };
                backend.as_mut().set_run(run);
            }
            Answer::ArtifactExcerpt => {
                let excerpt = match answered {
                    Answered::ArtifactExcerpt(excerpt) => excerpt_variant(excerpt),
                    _ => QVariant::default(),
                };
                backend.as_mut().set_excerpt(excerpt);
            }
        }
    }
    // Last, and deliberately so. Qt emits `busyChanged` from inside the setter,
    // so a surface that reacts to an operation finishing runs *during* this
    // call — and everything it would read has to be in place before it does. A
    // page that hears the decision it issued settle and then reads `kind` to
    // find out how it went would otherwise read the previous operation's
    // answer, and would report a refusal as a success.
    let busy = backend.as_ref().rust().pending > 0;
    backend.as_mut().set_busy(busy);
}

impl ffi::RunsBackend {
    /// Runs a mutation off the Qt thread and applies its outcome back on it.
    fn dispatch(
        self: Pin<&mut Self>,
        work: impl FnOnce() -> Result<Completion, RunsFailure> + Send + 'static,
    ) {
        self.dispatch_answering(Answer::Decision, work);
    }

    /// Runs `work` off the Qt thread, writing the answer property it names.
    fn dispatch_answering(
        mut self: Pin<&mut Self>,
        answer: Answer,
        work: impl FnOnce() -> Result<Completion, RunsFailure> + Send + 'static,
    ) {
        note_qt_thread();
        let request = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_request += 1;
            rust.pending += 1;
            // Claims the answer property this operation owns as well as the
            // shared status. Both watermarks are taken here, on the Qt thread,
            // so the reply that eventually lands can be measured against the
            // state that held when it was issued.
            rust.newest[answer.slot()] = rust.next_request;
            rust.next_request
        };
        self.as_mut().set_busy(true);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = work();
            let _ = qt_thread.queue(move |backend| settle(backend, request, answer, outcome));
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
            // the coordinator rebuilds from the same task.
            let workspace = WorkspaceRef::from_task(&task, &PassThrough);
            let agent = Box::new(MockAgent::from_scenario(scenario));
            // Catalog metadata when the task names a project the catalog still
            // knows, which is what makes the coordinator canonicalize and check
            // the workspace root rather than take the recorded path on trust.
            // A task with no project identity, or one the catalog has since
            // forgotten, retries without it exactly as it ran without it.
            let retry = match task.project_id().and_then(catalog_metadata) {
                Some(metadata) => {
                    coordinator.retry_run_with_workspace_metadata(run, agent, workspace, metadata)
                }
                None => coordinator.retry_run(run, agent, workspace),
            }
            .map_err(retry_failure)?;
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
        self.dispatch_answering(Answer::ApprovalInput, move || {
            approval_input(parse_approval(&approval_id)?)
        });
    }

    fn load_run(self: Pin<&mut Self>, run_id: &QString) {
        let run_id = run_id.to_string();
        self.dispatch_answering(Answer::RunDetail, move || run_detail(parse_run(&run_id)?));
    }

    fn load_artifact_excerpt(self: Pin<&mut Self>, artifact_id: &QString) {
        let artifact_id = artifact_id.to_string();
        self.dispatch_answering(Answer::ArtifactExcerpt, move || {
            artifact_excerpt(parse_artifact(&artifact_id)?)
        });
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    use harkness_runtime::agent::{AgentAction, ObservationPattern};
    use harkness_runtime::approval::ApprovalScope;
    use harkness_runtime::coordinator::RuntimeError;
    use harkness_runtime::domain::{
        ExecutionState, Failure, Run, RunId, Step, Task, TaskId, ToolCall, ToolCallState,
    };
    use harkness_runtime::store::{EventKind, Store, StoreError};

    use super::{
        Answer, Answered, BRIDGE_KINDS, MAX_APPROVAL_INPUT_BYTES, MAX_ARTIFACT_EXCERPT_BYTES,
        MAX_PROGRESS_SCAN_EVENTS, MAX_RUN_DETAIL_ROWS, RunDetail, RunsFailure, artifact_excerpt_in,
        bounded, clamp, grant_scope, is_renderable_text, optional_rfc3339, parse_approval,
        parse_artifact, parse_run, retry_scenario, rfc3339, run_detail_in, settlement,
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

    // -- what a settling reply may write -----------------------------------

    #[test]
    fn a_decision_and_an_approval_input_share_the_property_they_both_write() {
        assert_eq!(Answer::Decision.slot(), Answer::ApprovalInput.slot());
    }

    #[test]
    fn the_three_answer_properties_are_counted_apart() {
        let slots = [
            Answer::ApprovalInput.slot(),
            Answer::RunDetail.slot(),
            Answer::ArtifactExcerpt.slot(),
        ];

        let distinct: std::collections::HashSet<usize> = slots.into_iter().collect();

        assert_eq!(
            distinct.len(),
            3,
            "{slots:?} does not name three properties"
        );
        assert!(
            slots.iter().all(|slot| *slot < super::ANSWER_SLOTS),
            "{slots:?} indexes past the watermarks"
        );
    }

    #[test]
    fn the_newest_operation_of_all_writes_both_its_answer_and_the_status() {
        let settled = settlement(7, 7, 7);

        assert!(settled.answer);
        assert!(settled.status);
    }

    #[test]
    fn a_load_still_answers_its_own_question_when_another_kind_overtook_it() {
        // The header re-read a live run schedules is issued after the excerpt
        // the reader asked for and settles first. It says nothing about the
        // excerpt, so the excerpt still lands on the row that asked for it.
        let settled = settlement(7, 7, 9);

        assert!(
            settled.answer,
            "the excerpt is still the newest of its kind"
        );
        assert!(
            !settled.status,
            "but the message belongs to the operation the reader did last"
        );
    }

    #[test]
    fn a_load_superseded_by_another_of_its_own_kind_writes_nothing() {
        let settled = settlement(7, 9, 9);

        assert!(!settled.answer);
        assert!(!settled.status);
    }

    #[test]
    fn the_status_is_never_written_by_a_reply_that_may_not_write_its_answer() {
        // A slot's watermark is a value `next_request` held at some point, so
        // it can never run ahead of it; the combinations where it does are not
        // states `dispatch_answering` can produce.
        for request in 0..4u64 {
            for newest_overall in request..4u64 {
                for newest_for_answer in request..=newest_overall {
                    let settled = settlement(request, newest_for_answer, newest_overall);

                    assert!(
                        !settled.status || settled.answer,
                        "{request}/{newest_for_answer}/{newest_overall} would describe an \
                         operation whose answer was already superseded"
                    );
                }
            }
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

    // -- one run's detail ---------------------------------------------------

    /// A store with one task, in its own temporary data directory.
    fn seeded() -> (TempDir, std::path::PathBuf, Store, Task) {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let store = Store::open(&data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace/harkness",
            None,
            at(0),
        );
        store.insert_task(&task).unwrap();
        (fixture, data_dir, store, task)
    }

    /// Records one run of `task` and leaves it in `state`.
    fn recorded_run(store: &Store, task: &Task, state: ExecutionState, seconds: i64) -> RunId {
        let run = Run::with_id(RunId::new(), task.id(), at(seconds));
        store.insert_run(&run).unwrap();
        if state == ExecutionState::Queued {
            return run.id();
        }
        store
            .transition_run(run.id(), ExecutionState::Running, at(seconds + 1))
            .unwrap();
        match state {
            ExecutionState::Running => {}
            ExecutionState::Failed => {
                store
                    .fail_run(
                        run.id(),
                        Failure::new("tool_failed", "the tool exited 1"),
                        at(seconds + 2),
                    )
                    .unwrap();
            }
            other => {
                store
                    .transition_run(run.id(), other, at(seconds + 2))
                    .unwrap();
            }
        }
        run.id()
    }

    fn detail(data_dir: &std::path::Path, run: RunId) -> RunDetail {
        match run_detail_in(data_dir, run).unwrap().answered {
            Answered::RunDetail(detail) => *detail,
            other => panic!("a run detail load answers with a run detail, not {other:?}"),
        }
    }

    /// One step with one call in `state`, on `run`.
    fn stepped_call(store: &Store, run: RunId, state: ToolCallState) -> (Step, ToolCall) {
        let step = Step::new(run, 0, "run the check", at(1));
        store.insert_step(&step).unwrap();
        let mut call = ToolCall::new(
            &step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test"]}),
            at(2),
        );
        store.insert_tool_call(&call).unwrap();
        if state != ToolCallState::Pending {
            store
                .transition_tool_call(call.id(), ToolCallState::Running, at(3))
                .unwrap();
            call.dispatch("1.0.0", at(3)).unwrap();
        }
        if state == ToolCallState::Succeeded {
            store
                .succeed_tool_call(call.id(), json!({"passed": true}), at(4))
                .unwrap();
            call.succeed(json!({"passed": true}), at(4)).unwrap();
        }
        (step, call)
    }

    fn progress_events(store: &Store, run: RunId, step: &Step, call: &ToolCall, lines: &[&str]) {
        store
            .append_events(
                run,
                lines.iter().map(|line| {
                    harkness_runtime::store::RunEvent::new(EventKind::ToolProgress, at(5))
                        .for_step(step.id())
                        .for_tool_call(call.id())
                        .with_payload(json!({"line": line}))
                }),
            )
            .unwrap();
    }

    #[test]
    fn a_call_still_in_flight_carries_the_newest_line_it_reported() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Running, 10);
        let (step, call) = stepped_call(&store, run, ToolCallState::Running);
        progress_events(
            &store,
            run,
            &step,
            &call,
            &["compiling harkness-git", "compiling harkness-gui"],
        );
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.calls.len(), 1);
        assert_eq!(detail.calls[0].progress, "line=compiling harkness-gui");
    }

    #[test]
    fn a_finished_call_carries_no_progress_line_of_its_own() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Running, 10);
        let (step, call) = stepped_call(&store, run, ToolCallState::Succeeded);
        progress_events(&store, run, &step, &call, &["compiling harkness-git"]);
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.calls.len(), 1);
        assert_eq!(
            detail.calls[0].progress, "",
            "a finished call's progress is history, which the timeline is the surface for"
        );
    }

    #[test]
    fn a_call_whose_ticks_fell_off_the_scanned_page_shows_none_rather_than_an_old_one() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Running, 10);
        let (step, call) = stepped_call(&store, run, ToolCallState::Running);
        progress_events(&store, run, &step, &call, &["the only tick there is"]);
        store
            .append_events(
                run,
                (0..MAX_PROGRESS_SCAN_EVENTS).map(|index| {
                    harkness_runtime::store::RunEvent::new(EventKind::Diagnostic, at(6))
                        .with_payload(json!({"index": index}))
                }),
            )
            .unwrap();
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(
            detail.calls[0].progress, "",
            "the bound is on what is read, and the timeline still holds every tick"
        );
    }

    #[test]
    fn a_run_that_was_never_recorded_is_refused_rather_than_read_as_empty() {
        let (_fixture, data_dir, store, _task) = seeded();
        drop(store);

        let failure = run_detail_in(&data_dir, RunId::new()).unwrap_err();

        assert_eq!(failure.kind, "not_found");
    }

    #[test]
    fn a_recorded_run_is_projected_with_its_steps_calls_and_artifacts() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Failed, 1);
        let step = Step::new(run, 0, "run the check", at(2));
        store.insert_step(&step).unwrap();
        let call = ToolCall::new(
            &step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["ls"]}),
            at(3),
        );
        store.insert_tool_call(&call).unwrap();
        let mut sink = store
            .create_artifact(run, "stdout.log", "text/plain", at(4))
            .unwrap()
            .for_step(step.id())
            .for_tool_call(call.id());
        sink.write_all(b"hello\n").unwrap();
        sink.finish().unwrap();
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.run.state, "failed");
        assert_eq!(detail.run.error_kind, "tool_failed");
        assert_eq!(detail.steps.len(), 1);
        assert_eq!(detail.calls.len(), 1);
        assert_eq!(detail.calls[0].tool_id, "process.exec");
        assert_eq!(detail.artifacts.len(), 1);
        assert_eq!(detail.artifacts[0].media_type, "text/plain");
        assert_eq!(detail.artifacts[0].byte_size, 6);
        assert_eq!(detail.artifacts[0].availability, "available");
        assert!(detail.artifacts[0].excerptable);
        assert!(
            detail.artifacts[0].path.ends_with(&format!(
                "artifacts/{run}/{}",
                detail.artifacts[0].artifact_id
            )),
            "{}",
            detail.artifacts[0].path
        );
        assert!(detail.truncated.is_empty());
    }

    #[test]
    fn an_interrupted_run_still_names_the_call_that_was_in_flight() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Running, 1);
        let step = Step::new(run, 0, "run the check", at(2));
        store.insert_step(&step).unwrap();
        let call = ToolCall::new(
            &step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["ls"]}),
            at(3),
        );
        store.insert_tool_call(&call).unwrap();
        store
            .transition_tool_call(call.id(), ToolCallState::Running, at(5))
            .unwrap();
        store
            .transition_tool_call(call.id(), ToolCallState::Interrupted, at(6))
            .unwrap();
        store
            .transition_run(run, ExecutionState::Interrupted, at(7))
            .unwrap();
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.run.state, "interrupted");
        assert_eq!(detail.calls[0].state, "interrupted");
        assert_eq!(detail.calls[0].started, "2025-08-12T12:00:05Z");
        assert!(detail.retryable, "an interrupted run is re-attemptable");
    }

    #[test]
    fn a_run_that_has_not_finished_offers_no_retry_and_says_why() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Running, 1);
        drop(store);

        let detail = detail(&data_dir, run);

        assert!(!detail.retryable);
        assert_eq!(detail.retry_blocked, "run_still_active");
    }

    #[test]
    fn a_run_that_succeeded_offers_no_retry_and_says_why() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Succeeded, 1);
        drop(store);

        let detail = detail(&data_dir, run);

        assert!(!detail.retryable);
        assert_eq!(detail.retry_blocked, "run_not_retryable");
    }

    #[test]
    fn a_failed_run_whose_re_attempt_is_still_running_offers_no_second_one() {
        let (_fixture, data_dir, store, task) = seeded();
        let original = recorded_run(&store, &task, ExecutionState::Failed, 1);
        let retry = Run::retrying_with_id(RunId::new(), task.id(), original, false, at(10));
        store.insert_run(&retry).unwrap();
        store
            .transition_run(retry.id(), ExecutionState::Running, at(11))
            .unwrap();
        drop(store);

        let detail = detail(&data_dir, original);

        assert!(
            !detail.retryable,
            "two live attempts would share a worktree"
        );
        assert_eq!(detail.retry_blocked, "run_still_active");
        assert_eq!(detail.retries, vec![retry.id().to_string()]);
    }

    #[test]
    fn a_failed_run_whose_re_attempts_have_all_ended_is_retryable_again() {
        let (_fixture, data_dir, store, task) = seeded();
        let original = recorded_run(&store, &task, ExecutionState::Failed, 1);
        let retry = Run::retrying_with_id(RunId::new(), task.id(), original, true, at(10));
        store.insert_run(&retry).unwrap();
        store
            .transition_run(retry.id(), ExecutionState::Running, at(11))
            .unwrap();
        store
            .transition_run(retry.id(), ExecutionState::Cancelled, at(12))
            .unwrap();
        drop(store);

        let detail = detail(&data_dir, original);

        assert!(detail.retryable);
        assert_eq!(detail.retry_blocked, "");
    }

    #[test]
    fn an_oversized_collection_is_cut_and_names_itself() {
        let mut truncated = Vec::new();

        let rows = bounded(
            (0..MAX_RUN_DETAIL_ROWS + 1).collect::<Vec<_>>(),
            "calls",
            &mut truncated,
        );

        assert_eq!(rows.len(), MAX_RUN_DETAIL_ROWS);
        assert_eq!(truncated, vec!["calls"]);
    }

    #[test]
    fn a_collection_inside_the_bound_says_nothing_about_truncation() {
        let mut truncated = Vec::new();

        let rows = bounded(vec![1, 2, 3], "steps", &mut truncated);

        assert_eq!(rows.len(), 3);
        assert!(truncated.is_empty());
    }

    // -- one artifact's excerpt ---------------------------------------------

    /// Records one artifact of `content` under a fresh run.
    fn with_artifact(
        store: &Store,
        task: &Task,
        name: &str,
        media_type: &str,
        content: &[u8],
    ) -> harkness_runtime::domain::ArtifactId {
        let run = recorded_run(store, task, ExecutionState::Succeeded, 1);
        let mut sink = store.create_artifact(run, name, media_type, at(4)).unwrap();
        sink.write_all(content).unwrap();
        sink.finish().unwrap().id()
    }

    #[test]
    fn text_media_types_are_an_allowlist_rather_than_a_guess() {
        assert!(is_renderable_text("text/plain"));
        assert!(is_renderable_text("text/plain; charset=utf-8"));
        assert!(is_renderable_text("APPLICATION/JSON"));
        assert!(is_renderable_text("application/vnd.harkness.diff+json"));
        assert!(!is_renderable_text("application/octet-stream"));
        assert!(!is_renderable_text("image/png"));
        assert!(!is_renderable_text(""));
    }

    #[test]
    fn a_text_artifact_inside_the_budget_is_delivered_whole() {
        let (_fixture, data_dir, store, task) = seeded();
        let artifact = with_artifact(
            &store,
            &task,
            "stdout.log",
            "text/plain",
            b"two lines\nhere\n",
        );
        drop(store);

        let excerpt = match artifact_excerpt_in(&data_dir, artifact).unwrap().answered {
            Answered::ArtifactExcerpt(excerpt) => excerpt,
            other => panic!("an excerpt load answers with an excerpt, not {other:?}"),
        };

        assert_eq!(excerpt.text, "two lines\nhere\n");
        assert!(!excerpt.truncated);
        assert_eq!(excerpt.name, "stdout.log");
    }

    #[test]
    fn an_oversized_text_artifact_is_cut_on_a_character_boundary_and_says_so() {
        let (_fixture, data_dir, store, task) = seeded();
        // Three bytes per character, so the budget cannot land on a boundary.
        let content = "€".repeat(MAX_ARTIFACT_EXCERPT_BYTES);
        let artifact = with_artifact(&store, &task, "wide.log", "text/plain", content.as_bytes());
        drop(store);

        let excerpt = match artifact_excerpt_in(&data_dir, artifact).unwrap().answered {
            Answered::ArtifactExcerpt(excerpt) => excerpt,
            other => panic!("an excerpt load answers with an excerpt, not {other:?}"),
        };

        assert!(excerpt.truncated);
        assert!(excerpt.text.len() <= MAX_ARTIFACT_EXCERPT_BYTES);
        assert!(
            excerpt.text.chars().all(|character| character == '€'),
            "a cut inside a character drops the tail rather than replacing it"
        );
    }

    #[test]
    fn an_artifact_that_is_not_text_is_refused_by_name_rather_than_rendered() {
        let (_fixture, data_dir, store, task) = seeded();
        let artifact = with_artifact(&store, &task, "core", "application/octet-stream", &[0, 159]);
        drop(store);

        let failure = artifact_excerpt_in(&data_dir, artifact).unwrap_err();

        assert_eq!(failure.kind, "artifact_not_text");
        assert!(
            failure.message.contains("does not render inline"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn an_artifact_declaring_text_that_is_not_utf_8_is_refused_rather_than_mangled() {
        let (_fixture, data_dir, store, task) = seeded();
        let artifact = with_artifact(&store, &task, "latin.log", "text/plain", &[0xff, 0xfe]);
        drop(store);

        let failure = artifact_excerpt_in(&data_dir, artifact).unwrap_err();

        assert_eq!(failure.kind, "artifact_not_text");
        assert!(
            failure.message.contains("not valid UTF-8"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn a_deleted_artifact_file_reads_as_unavailable_and_the_rest_of_the_page_still_loads() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Succeeded, 1);
        let mut sink = store
            .create_artifact(run, "stdout.log", "text/plain", at(4))
            .unwrap();
        sink.write_all(b"gone soon\n").unwrap();
        let artifact = sink.finish().unwrap();
        std::fs::remove_file(
            data_dir
                .join("artifacts")
                .join(run.to_string())
                .join(artifact.id().to_string()),
        )
        .unwrap();
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.artifacts.len(), 1, "the row survives its content");
        assert_eq!(detail.artifacts[0].availability, "missing");
        assert!(
            !detail.artifacts[0].excerptable,
            "there is nothing left to excerpt"
        );
    }

    #[test]
    fn an_artifact_whose_bytes_changed_is_not_offered_inline_either() {
        let (_fixture, data_dir, store, task) = seeded();
        let run = recorded_run(&store, &task, ExecutionState::Succeeded, 1);
        let mut sink = store
            .create_artifact(run, "notes.txt", "text/plain", at(4))
            .unwrap();
        sink.write_all(b"original").unwrap();
        let artifact = sink.finish().unwrap();
        // The row records what was true at finalization and is never updated,
        // so the size the reader would be shown is not the size on disk. What
        // `excerptable` promises is that the bytes are still the recorded ones.
        std::fs::write(
            data_dir
                .join("artifacts")
                .join(run.to_string())
                .join(artifact.id().to_string()),
            b"rewritten from outside",
        )
        .unwrap();
        drop(store);

        let detail = detail(&data_dir, run);

        assert_eq!(detail.artifacts[0].availability, "size_mismatch");
        assert!(
            !detail.artifacts[0].excerptable,
            "these are not the bytes the row describes"
        );
    }

    #[test]
    fn an_unparseable_artifact_identifier_is_refused_before_any_store_opens() {
        assert_eq!(
            parse_artifact("not-a-uuid").unwrap_err().kind,
            "invalid_artifact_id"
        );
    }
}
