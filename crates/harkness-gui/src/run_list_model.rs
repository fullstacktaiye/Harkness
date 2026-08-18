//! A keyset-paged list model for recent runs.
//!
//! The run list is the one surface that grows without bound: every check, every
//! task, every retry adds a row for ever. Binding it to a `QVariantList` the way
//! the older panels bind their snapshots would rebuild the whole list on every
//! refresh and materialize every run ever recorded to show the newest twenty, so
//! this is a real `QAbstractListModel` that loads
//! [`RUN_PAGE_SIZE`](self::RUN_PAGE_SIZE) rows at a time through `fetchMore`.
//!
//! Paging is by key rather than by offset, because run history grows at the tip:
//! `RunCursor` names a position in the data, so a run recorded between two pages
//! shifts nothing and no row is repeated or skipped.
//!
//! # The Qt-thread mutation invariant
//!
//! Every `RunListModelRust` field is read and written on the Qt thread. Reading
//! a page opens SQLite, so it happens on a `std::thread::spawn` worker and comes
//! back through `qt_thread().queue(...)`; the worker owns a `RunCursor` and
//! plain `String`s, never a `QString`, a `QVariant`, or a pinned reference.
//!
//! # The staleness counter
//!
//! `next_request` is `HarknessBackend::next_review_request`'s mechanism: a
//! refresh takes the next number, and a page whose number is stale is dropped
//! rather than appended. Without it a slow first page arriving after a refresh
//! would append rows the refresh had already replaced.
//!
//! # The role contract
//!
//! Roles are the QML contract and their names never change. A row carries the
//! run's identity, its task's title and workspace, its lifecycle state and
//! times, its failure discriminant, and its retry provenance — everything a list
//! delegate draws, and nothing that has to be read out of the event log.

#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;

        include!("cxx-qt-lib/qhash_i32_QByteArray.h");
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;

        include!("cxx-qt-lib/qmodelindex.h");
        type QModelIndex = cxx_qt_lib::QModelIndex;

        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("runlistmodelbase.h");
        type RunListModelBase;

        #[rust_name = "begin_insert"]
        fn beginInsert(self: Pin<&mut RunListModelBase>, first: i32, last: i32);

        #[rust_name = "end_insert"]
        fn endInsert(self: Pin<&mut RunListModelBase>);

        #[rust_name = "begin_reset"]
        fn beginReset(self: Pin<&mut RunListModelBase>);

        #[rust_name = "end_reset"]
        fn endReset(self: Pin<&mut RunListModelBase>);
    }

    extern "RustQt" {
        /// Newest-first run history.
        ///
        /// cxx-qt does not convert names to camel case, so property names are
        /// kept to a single word and every multi-word member names its Qt
        /// spelling explicitly. `loading` is true while a page is in flight,
        /// `more` says whether a further page exists, and `status` carries the
        /// last failure's message.
        #[qobject]
        #[qml_element]
        #[base = RunListModelBase]
        #[qproperty(bool, loading)]
        #[qproperty(bool, more)]
        #[qproperty(QString, status)]
        #[qproperty(QString, kind)]
        type RunListModel = super::RunListModelRust;

        #[cxx_override]
        fn data(self: &RunListModel, index: &QModelIndex, role: i32) -> QVariant;

        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &RunListModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &RunListModel) -> QHash_i32_QByteArray;

        #[cxx_override]
        #[cxx_name = "canFetchMore"]
        fn can_fetch_more(self: &RunListModel, parent: &QModelIndex) -> bool;

        #[cxx_override]
        #[cxx_name = "fetchMore"]
        fn fetch_more(self: Pin<&mut RunListModel>, parent: &QModelIndex);

        /// Discards every row and loads the newest page again.
        #[qinvokable]
        fn refresh(self: Pin<&mut RunListModel>);

        /// Appends the next page, if `more` says there is one.
        ///
        /// The same work `fetchMore` does, reachable by name. QML's `ListView`
        /// does not drive `canFetchMore`/`fetchMore` the way the widget views
        /// do — that is `QAbstractItemView`'s behavior — so a list bound to
        /// this model would otherwise stop at its first page with no way to ask
        /// for another. The override stays for anything that does drive it.
        #[qinvokable]
        #[cxx_name = "loadMore"]
        fn load_more(self: Pin<&mut RunListModel>);
    }

    impl cxx_qt::Threading for RunListModel {}
}

use std::collections::HashMap;
use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading, casting::Upcast};
use cxx_qt_lib::{QByteArray, QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};

use harkness_runtime::domain::{Run, Task, TaskId};
use harkness_runtime::store::{RunCursor, RunPage};

use super::runs_backend::{
    RunsFailure, data_dir, existing_coordinator, note_qt_thread, optional_rfc3339, rfc3339,
};

/// Rows loaded per `fetchMore`.
///
/// The same fifty `HarknessBackend::HISTORY_PAGE_SIZE` uses for commits and the
/// run store's own `DEFAULT_RUN_PAGE_LIMIT`: enough that a scroll rarely waits,
/// small enough that opening the panel is one bounded query.
pub const RUN_PAGE_SIZE: usize = 50;

/// `Qt::DisplayRole`, so a row reads as its task title in accessibility tooling.
const DISPLAY_ROLE: i32 = 0;
/// `Qt::UserRole + 1` and up: the roles QML delegates bind to.
const RUN_ID_ROLE: i32 = 257;
const TASK_ID_ROLE: i32 = 258;
const TITLE_ROLE: i32 = 259;
const STATE_ROLE: i32 = 260;
const TERMINAL_ROLE: i32 = 261;
const CREATED_ROLE: i32 = 262;
const STARTED_ROLE: i32 = 263;
const FINISHED_ROLE: i32 = 264;
const WORKSPACE_ROLE: i32 = 265;
const PROJECT_ROLE: i32 = 266;
const ERROR_KIND_ROLE: i32 = 267;
const ERROR_MESSAGE_ROLE: i32 = 268;
const RETRY_OF_ROLE: i32 = 269;
const MODIFIED_ROLE: i32 = 270;

fn model_roles() -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    roles.insert(DISPLAY_ROLE, QByteArray::from("display"));
    roles.insert(RUN_ID_ROLE, QByteArray::from("runId"));
    roles.insert(TASK_ID_ROLE, QByteArray::from("taskId"));
    roles.insert(TITLE_ROLE, QByteArray::from("title"));
    roles.insert(STATE_ROLE, QByteArray::from("state"));
    roles.insert(TERMINAL_ROLE, QByteArray::from("terminal"));
    roles.insert(CREATED_ROLE, QByteArray::from("created"));
    roles.insert(STARTED_ROLE, QByteArray::from("started"));
    roles.insert(FINISHED_ROLE, QByteArray::from("finished"));
    roles.insert(WORKSPACE_ROLE, QByteArray::from("workspace"));
    roles.insert(PROJECT_ROLE, QByteArray::from("projectId"));
    roles.insert(ERROR_KIND_ROLE, QByteArray::from("errorKind"));
    roles.insert(ERROR_MESSAGE_ROLE, QByteArray::from("errorMessage"));
    roles.insert(RETRY_OF_ROLE, QByteArray::from("retryOf"));
    roles.insert(MODIFIED_ROLE, QByteArray::from("workspaceModified"));
    roles
}

/// One run as a delegate draws it.
///
/// Owned `String`s rather than `QString`s so a worker can build a whole page
/// before anything crosses back to the Qt thread.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunRow {
    run_id: String,
    task_id: String,
    title: String,
    state: String,
    terminal: bool,
    created: String,
    started: String,
    finished: String,
    workspace: String,
    project_id: String,
    error_kind: String,
    error_message: String,
    retry_of: String,
    workspace_modified: bool,
}

/// Projects one run, with the task it attempts when that task could be read.
///
/// A task that fails to load leaves the title and workspace empty rather than
/// dropping the run: a run whose task row is unreadable is exactly the run
/// somebody needs to see.
pub(crate) fn run_row(run: &Run, task: Option<&Task>) -> RunRow {
    RunRow {
        run_id: run.id().to_string(),
        task_id: run.task_id().to_string(),
        title: task.map(|task| task.title().to_owned()).unwrap_or_default(),
        state: run.state().as_str().to_owned(),
        terminal: run.state().is_terminal(),
        created: rfc3339(run.created_at()),
        started: optional_rfc3339(run.started_at()),
        finished: optional_rfc3339(run.finished_at()),
        workspace: task
            .map(|task| task.workspace_root().display().to_string())
            .unwrap_or_default(),
        project_id: task
            .and_then(Task::project_id)
            .map(|id| id.to_string())
            .unwrap_or_default(),
        error_kind: run
            .failure()
            .map(|failure| failure.kind().to_owned())
            .unwrap_or_default(),
        error_message: run
            .failure()
            .map(|failure| failure.message().to_owned())
            .unwrap_or_default(),
        retry_of: run
            .retry_of()
            .map(|original| original.to_string())
            .unwrap_or_default(),
        workspace_modified: run.workspace_may_be_modified(),
    }
}

/// One page of rows and the continuation that follows it.
#[derive(Clone, Debug, Default)]
pub(crate) struct RunPageResult {
    rows: Vec<RunRow>,
    next: Option<RunCursor>,
}

/// Turns a listing's runs into rows, reading each run's task at most once.
///
/// A page of fifty runs is commonly a page of one or two tasks — a check run and
/// its retries all name the same one — so the cache is what keeps this one query
/// per distinct task rather than one per row.
pub(crate) fn page_rows(runs: &[Run], mut task: impl FnMut(TaskId) -> Option<Task>) -> Vec<RunRow> {
    let mut tasks: HashMap<TaskId, Option<Task>> = HashMap::new();
    runs.iter()
        .map(|run| {
            let cached = tasks
                .entry(run.task_id())
                .or_insert_with(|| task(run.task_id()));
            run_row(run, cached.as_ref())
        })
        .collect()
}

/// Reads one page off the Qt thread.
fn load_page(cursor: Option<RunCursor>) -> Result<RunPageResult, RunsFailure> {
    load_page_in(&data_dir()?, cursor)
}

/// Reads one page from a named data directory.
///
/// Split from [`load_page`] so a test can seed a temporary store and read it
/// back without touching `HARKNESS_DATA_DIR`, which is process-wide. A directory
/// that has recorded nothing answers with an empty page rather than creating a
/// run store: opening the runs panel is a read.
fn load_page_in(
    data_dir: &std::path::Path,
    cursor: Option<RunCursor>,
) -> Result<RunPageResult, RunsFailure> {
    let Some(coordinator) = existing_coordinator(data_dir)? else {
        return Ok(RunPageResult::default());
    };
    let page = match cursor {
        Some(cursor) => RunPage::after(cursor, RUN_PAGE_SIZE),
        None => RunPage::new(RUN_PAGE_SIZE),
    };
    let listing = coordinator.list_runs(page)?;
    let store = coordinator.store();
    Ok(RunPageResult {
        rows: page_rows(&listing.runs, |task_id| store.load_task(task_id).ok()),
        next: listing.next_cursor,
    })
}

#[derive(Default)]
pub struct RunListModelRust {
    rows: Vec<RunRow>,
    cursor: Option<RunCursor>,
    loading: bool,
    more: bool,
    status: QString,
    kind: QString,
    next_request: u64,
}

impl ffi::RunListModel {
    fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        if !index.is_valid() {
            return QVariant::default();
        }
        let Ok(row) = usize::try_from(index.row()) else {
            return QVariant::default();
        };
        let Some(entry) = self.rust().rows.get(row) else {
            return QVariant::default();
        };
        let text = |value: &str| QVariant::from(&QString::from(value));
        match role {
            DISPLAY_ROLE | TITLE_ROLE => text(&entry.title),
            RUN_ID_ROLE => text(&entry.run_id),
            TASK_ID_ROLE => text(&entry.task_id),
            STATE_ROLE => text(&entry.state),
            TERMINAL_ROLE => QVariant::from(&entry.terminal),
            CREATED_ROLE => text(&entry.created),
            STARTED_ROLE => text(&entry.started),
            FINISHED_ROLE => text(&entry.finished),
            WORKSPACE_ROLE => text(&entry.workspace),
            PROJECT_ROLE => text(&entry.project_id),
            ERROR_KIND_ROLE => text(&entry.error_kind),
            ERROR_MESSAGE_ROLE => text(&entry.error_message),
            RETRY_OF_ROLE => text(&entry.retry_of),
            MODIFIED_ROLE => QVariant::from(&entry.workspace_modified),
            _ => QVariant::default(),
        }
    }

    fn row_count(&self, parent: &QModelIndex) -> i32 {
        // A list model has rows only below its invisible root.
        if parent.is_valid() {
            return 0;
        }
        i32::try_from(self.rust().rows.len()).unwrap_or(i32::MAX)
    }

    fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        model_roles()
    }

    fn can_fetch_more(&self, parent: &QModelIndex) -> bool {
        if parent.is_valid() {
            return false;
        }
        let rust = self.rust();
        rust.cursor.is_some() && !rust.loading
    }

    fn fetch_more(self: Pin<&mut Self>, parent: &QModelIndex) {
        if parent.is_valid() {
            return;
        }
        self.load_more();
    }

    fn load_more(mut self: Pin<&mut Self>) {
        let Some(cursor) = self.as_ref().rust().cursor else {
            return;
        };
        if self.as_ref().rust().loading {
            return;
        }
        let request = self.as_mut().begin_request();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = load_page(Some(cursor));
            let _ = qt_thread.queue(move |model| append_page(model, request, outcome));
        });
    }

    fn refresh(mut self: Pin<&mut Self>) {
        let request = self.as_mut().begin_request();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = load_page(None);
            let _ = qt_thread.queue(move |model| replace_page(model, request, outcome));
        });
    }

    /// Claims the next request number and marks the model busy.
    fn begin_request(mut self: Pin<&mut Self>) -> u64 {
        note_qt_thread();
        let request = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_request += 1;
            rust.next_request
        };
        self.as_mut().set_loading(true);
        self.as_mut().set_status(QString::default());
        self.as_mut().set_kind(QString::default());
        request
    }
}

/// Records a failed page without leaving `canFetchMore` true for ever.
///
/// Clearing the cursor is what stops a view asking again immediately and getting
/// the same failure in a loop; the row is recovered by an explicit refresh.
fn fail(mut model: Pin<&mut ffi::RunListModel>, failure: &RunsFailure) {
    model.as_mut().rust_mut().get_mut().cursor = None;
    model.as_mut().set_more(false);
    model
        .as_mut()
        .set_status(QString::from(failure.message.as_str()));
    // The discriminant travels beside the message, so a surface can tell a
    // directory that has recorded nothing from a store it could not read.
    model
        .as_mut()
        .set_kind(QString::from(failure.kind.as_str()));
}

fn replace_page(
    mut model: Pin<&mut ffi::RunListModel>,
    request: u64,
    outcome: Result<RunPageResult, RunsFailure>,
) {
    // A page for a load a later refresh superseded describes rows the model no
    // longer has; appending it would interleave two histories.
    if model.as_ref().rust().next_request != request {
        return;
    }
    model.as_mut().set_loading(false);
    match outcome {
        Ok(page) => {
            {
                let base: Pin<&mut ffi::RunListModelBase> = model.as_mut().upcast_pin();
                base.begin_reset();
            }
            {
                let rust = model.as_mut().rust_mut().get_mut();
                rust.rows = page.rows;
                rust.cursor = page.next;
            }
            {
                let base: Pin<&mut ffi::RunListModelBase> = model.as_mut().upcast_pin();
                base.end_reset();
            }
            let more = model.as_ref().rust().cursor.is_some();
            model.as_mut().set_more(more);
        }
        Err(failure) => fail(model, &failure),
    }
}

fn append_page(
    mut model: Pin<&mut ffi::RunListModel>,
    request: u64,
    outcome: Result<RunPageResult, RunsFailure>,
) {
    // A page for a load a later refresh superseded describes rows the model no
    // longer has; appending it would interleave two histories.
    if model.as_ref().rust().next_request != request {
        return;
    }
    model.as_mut().set_loading(false);
    match outcome {
        Ok(page) => {
            let first = model.as_ref().rust().rows.len();
            if !page.rows.is_empty() {
                let last = first + page.rows.len() - 1;
                {
                    let base: Pin<&mut ffi::RunListModelBase> = model.as_mut().upcast_pin();
                    base.begin_insert(first as i32, last as i32);
                }
                model.as_mut().rust_mut().get_mut().rows.extend(page.rows);
                let base: Pin<&mut ffi::RunListModelBase> = model.as_mut().upcast_pin();
                base.end_insert();
            }
            model.as_mut().rust_mut().get_mut().cursor = page.next;
            let more = model.as_ref().rust().cursor.is_some();
            model.as_mut().set_more(more);
        }
        Err(failure) => fail(model, &failure),
    }
}

#[cfg(test)]
mod tests {
    use cxx_qt_lib::QByteArray;
    use time::OffsetDateTime;

    use harkness_core::ProjectId;
    use harkness_runtime::domain::{ExecutionState, Failure, Run, RunId, Task, TaskId};
    use harkness_runtime::store::Store;
    use tempfile::TempDir;

    use super::{
        CREATED_ROLE, DISPLAY_ROLE, ERROR_KIND_ROLE, ERROR_MESSAGE_ROLE, FINISHED_ROLE,
        MODIFIED_ROLE, PROJECT_ROLE, RETRY_OF_ROLE, RUN_ID_ROLE, RUN_PAGE_SIZE, STARTED_ROLE,
        STATE_ROLE, TASK_ID_ROLE, TERMINAL_ROLE, TITLE_ROLE, WORKSPACE_ROLE, load_page_in,
        model_roles, page_rows, run_row,
    };

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_755_000_000 + seconds).unwrap()
    }

    fn task() -> Task {
        Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace/harkness",
            Some(ProjectId::new()),
            at(0),
        )
    }

    #[test]
    fn qml_roles_have_stable_names() {
        let roles = model_roles();

        for (role, name) in [
            (DISPLAY_ROLE, "display"),
            (RUN_ID_ROLE, "runId"),
            (TASK_ID_ROLE, "taskId"),
            (TITLE_ROLE, "title"),
            (STATE_ROLE, "state"),
            (TERMINAL_ROLE, "terminal"),
            (CREATED_ROLE, "created"),
            (STARTED_ROLE, "started"),
            (FINISHED_ROLE, "finished"),
            (WORKSPACE_ROLE, "workspace"),
            (PROJECT_ROLE, "projectId"),
            (ERROR_KIND_ROLE, "errorKind"),
            (ERROR_MESSAGE_ROLE, "errorMessage"),
            (RETRY_OF_ROLE, "retryOf"),
            (MODIFIED_ROLE, "workspaceModified"),
        ] {
            assert_eq!(roles.get(&role), Some(QByteArray::from(name)));
        }
    }

    #[test]
    fn a_queued_run_carries_its_task_and_no_outcome_yet() {
        let task = task();
        let run = Run::with_id(RunId::new(), task.id(), at(1));

        let row = run_row(&run, Some(&task));

        assert_eq!(row.title, "Check: cargo test");
        assert_eq!(row.state, "queued");
        assert!(!row.terminal);
        assert_eq!(row.workspace, "/workspace/harkness");
        assert_eq!(row.created, "2025-08-12T12:00:01Z");
        assert_eq!(row.started, "");
        assert_eq!(row.finished, "");
        assert_eq!(row.error_kind, "");
        assert_eq!(row.retry_of, "");
        assert!(!row.workspace_modified);
    }

    #[test]
    fn a_failed_run_carries_the_discriminant_and_the_message_apart() {
        let task = task();
        let mut run = Run::with_id(RunId::new(), task.id(), at(1));
        run.transition(ExecutionState::Running, at(2)).unwrap();
        run.fail(Failure::new("tool_panicked", "the tool panicked"), at(3))
            .unwrap();

        let row = run_row(&run, Some(&task));

        assert_eq!(row.state, "failed");
        assert!(row.terminal);
        assert_eq!(row.started, "2025-08-12T12:00:02Z");
        assert_eq!(row.finished, "2025-08-12T12:00:03Z");
        assert_eq!(row.error_kind, "tool_panicked");
        assert_eq!(row.error_message, "the tool panicked");
    }

    #[test]
    fn a_retry_names_the_attempt_it_follows_and_warns_about_the_workspace() {
        let task = task();
        let original = RunId::new();
        let run = Run::retrying_with_id(RunId::new(), task.id(), original, true, at(4));

        let row = run_row(&run, Some(&task));

        assert_eq!(row.retry_of, original.to_string());
        assert!(row.workspace_modified);
    }

    #[test]
    fn a_run_whose_task_cannot_be_read_still_becomes_a_row() {
        let run = Run::with_id(RunId::new(), TaskId::new(), at(1));

        let row = run_row(&run, None);

        assert_eq!(row.run_id, run.id().to_string());
        assert_eq!(row.title, "");
        assert_eq!(row.workspace, "");
        assert_eq!(row.project_id, "");
    }

    #[test]
    fn a_page_reads_each_task_once_however_many_runs_name_it() {
        let task = task();
        let runs: Vec<Run> = (0..RUN_PAGE_SIZE as i64)
            .map(|index| Run::with_id(RunId::new(), task.id(), at(index)))
            .collect();
        let mut reads = 0;

        let rows = page_rows(&runs, |id| {
            reads += 1;
            (id == task.id()).then(|| task.clone())
        });

        assert_eq!(rows.len(), RUN_PAGE_SIZE);
        assert_eq!(reads, 1, "one task was read once per run");
        assert!(rows.iter().all(|row| row.title == task.title()));
    }

    #[test]
    fn a_page_of_runs_from_two_tasks_reads_each_of_them() {
        let first = task();
        let second = task();
        let runs = vec![
            Run::with_id(RunId::new(), first.id(), at(1)),
            Run::with_id(RunId::new(), second.id(), at(2)),
            Run::with_id(RunId::new(), first.id(), at(3)),
        ];
        let mut reads = 0;

        let rows = page_rows(&runs, |id| {
            reads += 1;
            [&first, &second]
                .into_iter()
                .find(|task| task.id() == id)
                .cloned()
        });

        assert_eq!(reads, 2);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn an_empty_page_produces_no_rows() {
        assert!(page_rows(&[], |_| None).is_empty());
    }

    #[test]
    fn a_data_directory_that_recorded_nothing_reads_as_an_empty_page() {
        let fixture = TempDir::new().unwrap();

        let page = load_page_in(&fixture.path().join("never-used"), None).unwrap();

        assert!(page.rows.is_empty());
        assert!(page.next.is_none());
        assert!(
            !fixture.path().join("never-used").exists(),
            "a read must not be what creates the run store"
        );
    }

    #[test]
    fn attaching_to_a_store_ends_the_runs_no_live_process_is_driving() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let store = Store::open(&data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace",
            None,
            at(0),
        );
        store.insert_task(&task).unwrap();
        let run = Run::with_id(RunId::new(), task.id(), at(1));
        store.insert_run(&run).unwrap();
        // Left `running` with no claim behind it, exactly as a process killed
        // mid-run leaves it.
        store
            .transition_run(run.id(), ExecutionState::Running, at(2))
            .unwrap();
        drop(store);

        let page = load_page_in(&data_dir, None).unwrap();

        assert_eq!(
            page.rows[0].state, "interrupted",
            "building the coordinator a read goes through sweeps first, and a run \
             whose owning process is provably gone is what that sweep claims"
        );
        assert!(page.rows[0].terminal);
    }

    #[test]
    fn a_seeded_store_reads_back_newest_first_with_each_run_s_task_title() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let store = Store::open(&data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace/harkness",
            Some(ProjectId::new()),
            at(0),
        );
        store.insert_task(&task).unwrap();
        let recorded: Vec<RunId> = (0..3)
            .map(|index| {
                let run = Run::with_id(RunId::new(), task.id(), at(index * 10 + 1));
                store.insert_run(&run).unwrap();
                // Driven to a terminal state, because the coordinator this read
                // goes through sweeps at construction and an unfinished run
                // with no live claim is exactly what recovery ends.
                store
                    .transition_run(run.id(), ExecutionState::Running, at(index * 10 + 2))
                    .unwrap();
                store
                    .transition_run(run.id(), ExecutionState::Succeeded, at(index * 10 + 3))
                    .unwrap();
                run.id()
            })
            .collect();
        drop(store);

        let page = load_page_in(&data_dir, None).unwrap();

        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.run_id.as_str())
                .collect::<Vec<_>>(),
            recorded
                .iter()
                .rev()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "run history is newest first"
        );
        assert!(page.rows.iter().all(|row| row.title == task.title()));
        assert!(page.rows.iter().all(|row| row.state == "succeeded"));
        assert!(
            page.next.is_none(),
            "three runs are fewer than one page holds"
        );
    }
}
