#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("cxx-qt-lib/core/qlist/qlist_QVariant.h");
        type QList_QVariant = cxx_qt_lib::QList<QVariant>;
    }

    extern "RustQt" {
        /// The small Rust-backed object exposed to the Harkness QML module.
        ///
        /// cxx-qt does not convert names to camel case, so a `snake_case`
        /// member reaches QML spelled exactly as written here and any
        /// camel-case call site silently resolves to `undefined`. Every
        /// multi-word member is therefore named for the Qt side explicitly,
        /// and property names are kept to a single word.
        ///
        /// `opened` is the project shown in the shell page as a QVariantMap
        /// with the same keys as a `projects` row, or an empty map when the
        /// launcher is showing.
        #[qobject]
        #[qml_element]
        #[qproperty(bool, busy)]
        #[qproperty(QString, status)]
        #[qproperty(QList_QVariant, projects)]
        #[qproperty(QList_QVariant, jobs)]
        #[qproperty(QList_QVariant, branches)]
        #[qproperty(QList_QVariant, worktrees)]
        #[qproperty(QVariant, opened)]
        #[qproperty(QVariant, git)]
        #[qproperty(QVariant, diff)]
        #[qproperty(QVariant, history)]
        #[qproperty(QVariant, review)]
        type HarknessBackend = super::HarknessBackendRust;

        #[qinvokable]
        fn refresh(self: Pin<&mut HarknessBackend>);

        /// Returns an actionable error for `remote`, or an empty string when
        /// the clone would accept it. Drives live form validation in QML.
        #[qinvokable]
        #[cxx_name = "validateRemote"]
        fn validate_remote(self: &HarknessBackend, remote: &QString) -> QString;

        #[qinvokable]
        #[cxx_name = "importLocal"]
        fn import_local(self: Pin<&mut HarknessBackend>, path: &QString);

        #[qinvokable]
        #[cxx_name = "importRepository"]
        fn import_repository(self: Pin<&mut HarknessBackend>, remote: &QString);

        #[qinvokable]
        #[cxx_name = "cancelImport"]
        fn cancel_import(self: Pin<&mut HarknessBackend>);

        /// Cancels exactly one operation without affecting any other job.
        #[qinvokable]
        #[cxx_name = "cancelJob"]
        fn cancel_job(self: Pin<&mut HarknessBackend>, job_id: &QString);

        #[qinvokable]
        #[cxx_name = "openProject"]
        fn open_project(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Leaves the project shell without touching the catalog.
        #[qinvokable]
        #[cxx_name = "closeProject"]
        fn close_project(self: Pin<&mut HarknessBackend>);

        /// Loads branch-picker data without blocking the GUI thread.
        #[qinvokable]
        #[cxx_name = "refreshBranches"]
        fn refresh_branches(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Loads the detailed Git state for the open project.
        #[qinvokable]
        #[cxx_name = "refreshGit"]
        fn refresh_git(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Loads staged and unstaged content only for the selected path.
        #[qinvokable]
        #[cxx_name = "refreshDiff"]
        fn refresh_diff(self: Pin<&mut HarknessBackend>, project_id: &QString, path_id: &QString);

        /// Invalidates an in-flight diff request and clears its hunk tokens.
        #[qinvokable]
        #[cxx_name = "clearDiff"]
        fn clear_diff(self: Pin<&mut HarknessBackend>);

        /// Starts a fresh bounded history walk at HEAD.
        #[qinvokable]
        #[cxx_name = "refreshHistory"]
        fn refresh_history(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Requests exactly the continuation returned by the current history page.
        #[qinvokable]
        #[cxx_name = "loadMoreHistory"]
        fn load_more_history(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Opens a commit against its first parent in the read-only review surface.
        #[qinvokable]
        #[cxx_name = "reviewCommit"]
        fn review_commit(self: Pin<&mut HarknessBackend>, project_id: &QString, revision: &QString);

        /// Pins a branch and its merge-base, then opens their read-only diff.
        #[qinvokable]
        #[cxx_name = "reviewBranch"]
        fn review_branch(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            branch: &QString,
            base_branch: &QString,
        );

        /// Opens either side of the index in the shared read-only renderer.
        #[qinvokable]
        #[cxx_name = "reviewWorkingChanges"]
        fn review_working_changes(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            staged: bool,
        );

        /// Fetches hunk content for one backend-owned review path.
        #[qinvokable]
        #[cxx_name = "loadReviewFile"]
        fn load_review_file(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            file_id: &QString,
        );

        /// Expands one collapsed region through the Git service's stable context API.
        #[qinvokable]
        #[cxx_name = "expandReviewContext"]
        fn expand_review_context(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            hunk_id: &QString,
            direction: &QString,
        );

        /// Invalidates in-flight history and review requests for the open shell.
        #[qinvokable]
        #[cxx_name = "clearReview"]
        fn clear_review(self: Pin<&mut HarknessBackend>);

        #[qinvokable]
        #[cxx_name = "stagePath"]
        fn stage_path(self: Pin<&mut HarknessBackend>, project_id: &QString, path_id: &QString);

        #[qinvokable]
        #[cxx_name = "unstagePath"]
        fn unstage_path(self: Pin<&mut HarknessBackend>, project_id: &QString, path_id: &QString);

        /// Stages one backend-owned selection from the current unstaged diff.
        #[qinvokable]
        #[cxx_name = "stageHunk"]
        fn stage_hunk(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            selection_id: &QString,
        );

        /// Unstages one backend-owned selection from the current staged diff.
        #[qinvokable]
        #[cxx_name = "unstageHunk"]
        fn unstage_hunk(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            selection_id: &QString,
        );

        #[qinvokable]
        fn commit(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            message: &QString,
            amend: bool,
        );

        #[qinvokable]
        fn fetch(self: Pin<&mut HarknessBackend>, project_id: &QString);

        #[qinvokable]
        fn pull(self: Pin<&mut HarknessBackend>, project_id: &QString);

        #[qinvokable]
        fn push(self: Pin<&mut HarknessBackend>, project_id: &QString, allow_default_branch: bool);

        /// Loads the parent's linked worktrees without blocking the GUI thread.
        #[qinvokable]
        #[cxx_name = "refreshWorktrees"]
        fn refresh_worktrees(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Checks out a local branch and refreshes both project and branch state.
        #[qinvokable]
        #[cxx_name = "checkoutBranch"]
        fn checkout_branch(self: Pin<&mut HarknessBackend>, project_id: &QString, branch: &QString);

        /// Creates and checks out a local branch.
        #[qinvokable]
        #[cxx_name = "createBranch"]
        fn create_branch(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            branch: &QString,
            start_point: &QString,
        );

        /// Creates a new-branch, existing-branch, or detached worktree.
        #[qinvokable]
        #[cxx_name = "createWorktree"]
        fn create_worktree(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            mode: &QString,
            branch: &QString,
            start_point: &QString,
        );

        /// Relocates a Harkness-owned worktree to an absolute destination.
        #[qinvokable]
        #[cxx_name = "moveWorktree"]
        fn move_worktree(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            destination: &QString,
        );

        /// Protects a Harkness-owned worktree with a mandatory Git-validated reason.
        #[qinvokable]
        #[cxx_name = "lockWorktree"]
        fn lock_worktree(self: Pin<&mut HarknessBackend>, project_id: &QString, reason: &QString);

        /// Removes Git's lifecycle lock from a Harkness-owned worktree.
        #[qinvokable]
        #[cxx_name = "unlockWorktree"]
        fn unlock_worktree(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Reconciles missing or externally moved Harkness worktrees.
        #[qinvokable]
        #[cxx_name = "reconcileWorktrees"]
        fn reconcile_worktrees(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Removes a local project from the catalog without touching its
        /// directory.
        #[qinvokable]
        #[cxx_name = "removeProject"]
        fn remove_project(self: Pin<&mut HarknessBackend>, project_id: &QString);

        #[qinvokable]
        #[cxx_name = "removeManaged"]
        fn remove_managed(self: Pin<&mut HarknessBackend>, project_id: &QString);

        #[qinvokable]
        #[cxx_name = "removeWorktree"]
        fn remove_worktree(self: Pin<&mut HarknessBackend>, project_id: &QString, force: bool);
    }

    impl cxx_qt::Threading for HarknessBackend {}
}

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    pin::Pin,
};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};

pub struct HarknessBackendRust {
    busy: bool,
    status: QString,
    projects: QList<QVariant>,
    jobs: QList<QVariant>,
    branches: QList<QVariant>,
    worktrees: QList<QVariant>,
    opened: QVariant,
    git: QVariant,
    diff: QVariant,
    history: QVariant,
    review: QVariant,
    job_records: Vec<JobRecord>,
    cancellations: HashMap<String, harkness_git::Cancellation>,
    path_selections: HashMap<String, PathSelectionKey>,
    path_selection_ids: HashMap<PathSelectionKey, String>,
    diff_selections: HashMap<String, DiffSelectionRecord>,
    legacy_job: Option<String>,
    next_job_id: u64,
    next_path_selection: u64,
    next_diff_request: u64,
    next_diff_generation: u64,
    history_state: Option<HistoryStateRow>,
    review_state: Option<ReviewStateRow>,
    next_history_request: u64,
    next_review_request: u64,
    next_review_file_request: u64,
}

impl Default for HarknessBackendRust {
    fn default() -> Self {
        Self {
            busy: false,
            status: "Ready".into(),
            projects: QList::default(),
            jobs: QList::default(),
            branches: QList::default(),
            worktrees: QList::default(),
            opened: empty_opened(),
            git: empty_git(),
            diff: empty_diff(),
            history: empty_history(),
            review: empty_review(),
            job_records: Vec::new(),
            cancellations: HashMap::new(),
            path_selections: HashMap::new(),
            path_selection_ids: HashMap::new(),
            diff_selections: HashMap::new(),
            legacy_job: None,
            next_job_id: 0,
            next_path_selection: 0,
            next_diff_request: 0,
            next_diff_generation: 0,
            history_state: None,
            review_state: None,
            next_history_request: 0,
            next_review_request: 0,
            next_review_file_request: 0,
        }
    }
}

/// One operation visible to QML. The authoritative copy lives in
/// `HarknessBackendRust` and is only mutated on the Qt thread.
#[derive(Clone, Debug, Eq, PartialEq)]
struct JobRecord {
    id: String,
    kind: String,
    project_id: String,
    label: String,
    progress: String,
    cancellable: bool,
}

fn begin_job(
    jobs: &mut Vec<JobRecord>,
    next_job_id: &mut u64,
    kind: &str,
    project_id: &str,
    label: &str,
    cancellable: bool,
) -> Option<JobRecord> {
    if jobs
        .iter()
        .any(|job| job.kind == kind && job.project_id == project_id)
    {
        return None;
    }
    *next_job_id += 1;
    let job = JobRecord {
        id: format!("job-{}", *next_job_id),
        kind: kind.to_owned(),
        project_id: project_id.to_owned(),
        label: label.to_owned(),
        progress: "Starting…".to_owned(),
        cancellable,
    };
    jobs.push(job.clone());
    Some(job)
}

fn update_job(jobs: &mut [JobRecord], job_id: &str, progress: String) -> bool {
    let Some(job) = jobs.iter_mut().find(|job| job.id == job_id) else {
        return false;
    };
    job.progress = progress;
    true
}

fn end_job(jobs: &mut Vec<JobRecord>, job_id: &str) -> Option<JobRecord> {
    let index = jobs.iter().position(|job| job.id == job_id)?;
    Some(jobs.remove(index))
}

fn to_jobs(rows: &[JobRecord]) -> QList<QVariant> {
    let mut jobs = QList::<QVariant>::default();
    for row in rows {
        let mut entry = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, value: QVariant| entry.insert(QString::from(key), value);
        insert("id", QVariant::from(&QString::from(row.id.as_str())));
        insert("kind", QVariant::from(&QString::from(row.kind.as_str())));
        insert(
            "projectId",
            QVariant::from(&QString::from(row.project_id.as_str())),
        );
        insert("label", QVariant::from(&QString::from(row.label.as_str())));
        insert(
            "progress",
            QVariant::from(&QString::from(row.progress.as_str())),
        );
        insert("cancellable", QVariant::from(&row.cancellable));
        jobs.append(QVariant::from(&entry));
    }
    jobs
}

fn sync_jobs(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let jobs = to_jobs(&backend.as_ref().rust().job_records);
    backend.as_mut().set_jobs(jobs);
}

fn start_job(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    kind: &str,
    project_id: &str,
    label: &str,
    cancellable: bool,
) -> Option<(String, harkness_git::Cancellation)> {
    let job = {
        let rust = backend.as_mut().rust_mut().get_mut();
        begin_job(
            &mut rust.job_records,
            &mut rust.next_job_id,
            kind,
            project_id,
            label,
            cancellable,
        )
    };
    let Some(job) = job else {
        backend
            .as_mut()
            .set_status(format!("{label} is already running for this project").into());
        return None;
    };
    let cancellation = harkness_git::Cancellation::default();
    if cancellable {
        backend
            .as_mut()
            .rust_mut()
            .get_mut()
            .cancellations
            .insert(job.id.clone(), cancellation.clone());
    }
    sync_jobs(backend.as_mut());
    Some((job.id, cancellation))
}

fn update_backend_job(mut backend: Pin<&mut ffi::HarknessBackend>, job_id: &str, progress: String) {
    if update_job(
        &mut backend.as_mut().rust_mut().get_mut().job_records,
        job_id,
        progress,
    ) {
        sync_jobs(backend.as_mut());
    }
}

fn finish_job(mut backend: Pin<&mut ffi::HarknessBackend>, job_id: &str) {
    let changed = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.cancellations.remove(job_id);
        if rust.legacy_job.as_deref() == Some(job_id) {
            rust.legacy_job = None;
        }
        end_job(&mut rust.job_records, job_id).is_some()
    };
    if changed {
        sync_jobs(backend.as_mut());
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PathSelectionKey {
    project_id: String,
    path: PathBuf,
}

#[derive(Debug)]
struct StatusEntryRow {
    path: PathBuf,
    display_path: String,
    staged: String,
    unstaged: String,
    rename_source: String,
    conflicted: bool,
}

#[derive(Debug)]
struct GitStateRow {
    project_id: String,
    branch: String,
    head: String,
    detached: bool,
    unborn: bool,
    upstream: String,
    ahead: usize,
    behind: usize,
    pending: String,
    entries: Vec<StatusEntryRow>,
    error: String,
    error_kind: String,
}

impl GitStateRow {
    fn from_status(project_id: String, status: harkness_git::DetailedStatus) -> Self {
        let (branch, head, detached, unborn) = match status.head {
            harkness_git::HeadState::Unborn { branch } => {
                let branch = branch.unwrap_or_default();
                let head = if branch.is_empty() {
                    "unborn branch".to_owned()
                } else {
                    format!("{branch} (unborn)")
                };
                (branch, head, false, true)
            }
            harkness_git::HeadState::Branch { name } => (name.clone(), name, false, false),
            harkness_git::HeadState::Detached { commit } => {
                let short = commit.chars().take(12).collect::<String>();
                (String::new(), format!("detached at {short}"), true, false)
            }
        };
        let (upstream, ahead, behind) = status
            .upstream
            .map(|upstream| (upstream.name, upstream.ahead, upstream.behind))
            .unwrap_or_default();
        let pending = status
            .pending
            .map(|pending| pending.to_string())
            .unwrap_or_default();
        let entries = status
            .entries
            .into_iter()
            .map(|entry| {
                let display_path = entry.path.display().to_string();
                StatusEntryRow {
                    path: entry.path,
                    display_path,
                    staged: entry.staged.map(change_name).unwrap_or_default().to_owned(),
                    unstaged: entry
                        .unstaged
                        .map(change_name)
                        .unwrap_or_default()
                        .to_owned(),
                    rename_source: entry
                        .rename_source
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                    conflicted: entry.conflicted,
                }
            })
            .collect();
        Self {
            project_id,
            branch,
            head,
            detached,
            unborn,
            upstream,
            ahead,
            behind,
            pending,
            entries,
            error: String::new(),
            error_kind: String::new(),
        }
    }

    fn with_failure(mut self, failure: &GitFailure) -> Self {
        self.error.clone_from(&failure.message);
        self.error_kind.clone_from(&failure.kind);
        self
    }
}

fn change_name(change: harkness_git::FileChange) -> &'static str {
    match change {
        harkness_git::FileChange::Added => "added",
        harkness_git::FileChange::Modified => "modified",
        harkness_git::FileChange::Deleted => "deleted",
        harkness_git::FileChange::Renamed => "renamed",
        harkness_git::FileChange::Copied => "copied",
        harkness_git::FileChange::TypeChanged => "type changed",
        harkness_git::FileChange::Untracked => "untracked",
        harkness_git::FileChange::Unmerged => "unmerged",
    }
}

fn register_path_selection(
    backend: &mut HarknessBackendRust,
    project_id: &str,
    path: &Path,
) -> String {
    let key = PathSelectionKey {
        project_id: project_id.to_owned(),
        path: path.to_path_buf(),
    };
    if let Some(selection_id) = backend.path_selection_ids.get(&key) {
        return selection_id.clone();
    }
    backend.next_path_selection += 1;
    let selection_id = format!("path-{}", backend.next_path_selection);
    backend
        .path_selection_ids
        .insert(key.clone(), selection_id.clone());
    backend.path_selections.insert(selection_id.clone(), key);
    selection_id
}

fn resolve_path_selection(
    backend: &HarknessBackendRust,
    project_id: &str,
    selection_id: &str,
) -> Result<PathBuf, String> {
    let Some(selection) = backend.path_selections.get(selection_id) else {
        return Err("The selected path is no longer available; refresh Git status".to_owned());
    };
    if selection.project_id != project_id {
        return Err("The selected path belongs to a different project".to_owned());
    }
    Ok(selection.path.clone())
}

fn to_git(row: &GitStateRow, path_selection_ids: &[String]) -> QVariant {
    debug_assert_eq!(row.entries.len(), path_selection_ids.len());
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| state.insert(QString::from(key), value);
    insert(
        "projectId",
        QVariant::from(&QString::from(row.project_id.as_str())),
    );
    insert(
        "branch",
        QVariant::from(&QString::from(row.branch.as_str())),
    );
    insert("head", QVariant::from(&QString::from(row.head.as_str())));
    insert("detached", QVariant::from(&row.detached));
    insert("unborn", QVariant::from(&row.unborn));
    insert(
        "upstream",
        QVariant::from(&QString::from(row.upstream.as_str())),
    );
    insert(
        "ahead",
        QVariant::from(&(i32::try_from(row.ahead).unwrap_or(i32::MAX))),
    );
    insert(
        "behind",
        QVariant::from(&(i32::try_from(row.behind).unwrap_or(i32::MAX))),
    );
    insert(
        "pending",
        QVariant::from(&QString::from(row.pending.as_str())),
    );
    insert("error", QVariant::from(&QString::from(row.error.as_str())));
    insert(
        "errorKind",
        QVariant::from(&QString::from(row.error_kind.as_str())),
    );

    let mut entries = QList::<QVariant>::default();
    for (row, path_selection_id) in row.entries.iter().zip(path_selection_ids) {
        let mut entry = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, value: QVariant| entry.insert(QString::from(key), value);
        insert(
            "pathId",
            QVariant::from(&QString::from(path_selection_id.as_str())),
        );
        insert(
            "path",
            QVariant::from(&QString::from(row.display_path.as_str())),
        );
        insert(
            "staged",
            QVariant::from(&QString::from(row.staged.as_str())),
        );
        insert(
            "unstaged",
            QVariant::from(&QString::from(row.unstaged.as_str())),
        );
        insert(
            "renameSource",
            QVariant::from(&QString::from(row.rename_source.as_str())),
        );
        insert("conflicted", QVariant::from(&row.conflicted));
        entries.append(QVariant::from(&entry));
    }
    insert("entries", QVariant::from(&entries));
    QVariant::from(&state)
}

fn empty_git() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn set_git_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: &GitStateRow) {
    let path_selection_ids = {
        let rust = backend.as_mut().rust_mut().get_mut();
        row.entries
            .iter()
            .map(|entry| register_path_selection(rust, &row.project_id, &entry.path))
            .collect::<Vec<_>>()
    };
    backend.as_mut().set_git(to_git(row, &path_selection_ids));
}

fn clear_git_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.path_selections.clear();
        rust.path_selection_ids.clear();
    }
    backend.as_mut().set_git(empty_git());
}

#[derive(Clone, Debug)]
struct DiffSelectionRecord {
    project_id: String,
    target: harkness_git::DiffTarget,
    selection: harkness_git::HunkSelection,
}

#[derive(Debug)]
struct DiffStateRow {
    project_id: String,
    path_id: String,
    path: String,
    files: Vec<harkness_git::FileDiff>,
    loading: bool,
    error: String,
    error_kind: String,
}

impl DiffStateRow {
    fn loading(project_id: String, path_id: String, path: String) -> Self {
        Self {
            project_id,
            path_id,
            path,
            files: Vec::new(),
            loading: true,
            error: String::new(),
            error_kind: String::new(),
        }
    }

    fn with_failure(
        project_id: String,
        path_id: String,
        path: String,
        failure: &GitFailure,
    ) -> Self {
        Self {
            project_id,
            path_id,
            path,
            files: Vec::new(),
            loading: false,
            error: failure.message.clone(),
            error_kind: failure.kind.clone(),
        }
    }
}

fn empty_diff() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn diff_target_name(target: &harkness_git::DiffTarget) -> &'static str {
    match target {
        harkness_git::DiffTarget::Staged => "staged",
        harkness_git::DiffTarget::Unstaged => "unstaged",
        _ => "unknown",
    }
}

fn diff_line_name(line: harkness_git::DiffLineKind) -> (&'static str, &'static str) {
    match line {
        harkness_git::DiffLineKind::Context => ("context", " "),
        harkness_git::DiffLineKind::Addition => ("addition", "+"),
        harkness_git::DiffLineKind::Deletion => ("deletion", "-"),
        harkness_git::DiffLineKind::BothEofNoNewline
        | harkness_git::DiffLineKind::OldEofNoNewline
        | harkness_git::DiffLineKind::NewEofNoNewline => ("eof", "\\"),
        _ => ("unknown", "?"),
    }
}

fn display_patch_bytes(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    text.strip_suffix('\r').unwrap_or(text).to_owned()
}

fn display_diff_path(file: &harkness_git::FileDiff) -> String {
    match (file.old_path.as_deref(), file.new_path.as_deref()) {
        (Some(old), Some(new)) if old != new => {
            format!("{} → {}", old.to_string_lossy(), new.to_string_lossy())
        }
        (_, Some(path)) | (Some(path), None) => path.to_string_lossy().into_owned(),
        (None, None) => "(unnamed path)".to_owned(),
    }
}

fn omission_summary(omission: &harkness_git::DiffOmission) -> String {
    match omission {
        harkness_git::DiffOmission::FileTooLarge { limit } => {
            format!("File too large — content exceeds the {limit}-byte display limit.")
        }
        harkness_git::DiffOmission::Unmerged => {
            "Unmerged file — resolve the conflict before viewing a two-sided diff.".to_owned()
        }
        harkness_git::DiffOmission::ContentBudgetExhausted { limit } => {
            format!("Content budget exhausted — the diff reached its {limit}-byte limit.")
        }
        harkness_git::DiffOmission::FileBudgetExhausted { limit } => {
            format!("File budget exhausted — the diff reached its {limit}-file limit.")
        }
        harkness_git::DiffOmission::Unrepresentable { detail } => {
            format!("Unrepresentable diff — {detail}")
        }
        _ => "Content omitted for an unknown reason.".to_owned(),
    }
}

/// `Repeater` creates every delegate eagerly. Keep the GUI model below a
/// predictable object count even when a byte-small file contains many lines.
const MAX_GUI_DIFF_LINES_PER_FILE: usize = 1_000;

fn diff_line_count(file: &harkness_git::FileDiff) -> usize {
    file.hunks.iter().map(|hunk| hunk.lines.len()).sum()
}

fn displays_diff_hunks(file: &harkness_git::FileDiff) -> bool {
    file.omission.is_none() && !file.binary && diff_line_count(file) <= MAX_GUI_DIFF_LINES_PER_FILE
}

fn file_content_summary(file: &harkness_git::FileDiff) -> String {
    if let Some(omission) = &file.omission {
        omission_summary(omission)
    } else if file.binary {
        "Binary file — content diff and hunk staging are unavailable.".to_owned()
    } else if diff_line_count(file) > MAX_GUI_DIFF_LINES_PER_FILE {
        format!(
            "Diff has {} lines — content exceeds the {}-line GUI display limit. Stage or unstage the whole path instead.",
            diff_line_count(file),
            MAX_GUI_DIFF_LINES_PER_FILE
        )
    } else if file.hunks.is_empty() {
        "No textual hunks — stage or unstage the whole path instead.".to_owned()
    } else {
        String::new()
    }
}

fn bounded_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn to_diff(
    row: &DiffStateRow,
    generation: u64,
) -> (QVariant, HashMap<String, DiffSelectionRecord>) {
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| state.insert(QString::from(key), value);
    insert(
        "projectId",
        QVariant::from(&QString::from(row.project_id.as_str())),
    );
    insert(
        "pathId",
        QVariant::from(&QString::from(row.path_id.as_str())),
    );
    insert("path", QVariant::from(&QString::from(row.path.as_str())));
    insert("loading", QVariant::from(&row.loading));
    insert("error", QVariant::from(&QString::from(row.error.as_str())));
    insert(
        "errorKind",
        QVariant::from(&QString::from(row.error_kind.as_str())),
    );

    let mut selections = HashMap::new();
    let mut files = QList::<QVariant>::default();
    for (file_index, file) in row.files.iter().enumerate() {
        let mut file_value = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert_file = |key: &str, value: QVariant| {
            file_value.insert(QString::from(key), value);
        };
        insert_file(
            "target",
            QVariant::from(&QString::from(diff_target_name(&file.target))),
        );
        insert_file(
            "change",
            QVariant::from(&QString::from(change_name(file.change))),
        );
        insert_file(
            "path",
            QVariant::from(&QString::from(display_diff_path(file).as_str())),
        );
        insert_file(
            "summary",
            QVariant::from(&QString::from(file_content_summary(file).as_str())),
        );
        insert_file("binary", QVariant::from(&file.binary));

        let mut hunks = QList::<QVariant>::default();
        let displayed_hunks = if displays_diff_hunks(file) {
            file.hunks.as_slice()
        } else {
            &[]
        };
        for (hunk_index, hunk) in displayed_hunks.iter().enumerate() {
            let selection_id = format!("diff-{generation}-{file_index}-{hunk_index}");
            selections.insert(
                selection_id.clone(),
                DiffSelectionRecord {
                    project_id: row.project_id.clone(),
                    target: file.target.clone(),
                    selection: harkness_git::HunkSelection::new(file, hunk),
                },
            );

            let mut hunk_value = QMap::<QMapPair_QString_QVariant>::default();
            let mut insert_hunk = |key: &str, value: QVariant| {
                hunk_value.insert(QString::from(key), value);
            };
            insert_hunk(
                "selectionId",
                QVariant::from(&QString::from(selection_id.as_str())),
            );
            insert_hunk(
                "header",
                QVariant::from(&QString::from(display_patch_bytes(&hunk.header).as_str())),
            );
            insert_hunk("oldStart", QVariant::from(&bounded_i32(hunk.old_start)));
            insert_hunk("oldLines", QVariant::from(&bounded_i32(hunk.old_lines)));
            insert_hunk("newStart", QVariant::from(&bounded_i32(hunk.new_start)));
            insert_hunk("newLines", QVariant::from(&bounded_i32(hunk.new_lines)));

            let mut lines = QList::<QVariant>::default();
            for line in &hunk.lines {
                let (kind, marker) = diff_line_name(line.kind);
                let mut line_value = QMap::<QMapPair_QString_QVariant>::default();
                let mut insert_line = |key: &str, value: QVariant| {
                    line_value.insert(QString::from(key), value);
                };
                insert_line("kind", QVariant::from(&QString::from(kind)));
                insert_line("marker", QVariant::from(&QString::from(marker)));
                insert_line(
                    "oldLine",
                    QVariant::from(&line.old_line_number.map_or(0, bounded_i32)),
                );
                insert_line(
                    "newLine",
                    QVariant::from(&line.new_line_number.map_or(0, bounded_i32)),
                );
                insert_line(
                    "content",
                    QVariant::from(&QString::from(display_patch_bytes(&line.content).as_str())),
                );
                lines.append(QVariant::from(&line_value));
            }
            insert_hunk("lines", QVariant::from(&lines));
            hunks.append(QVariant::from(&hunk_value));
        }
        insert_file("hunks", QVariant::from(&hunks));
        files.append(QVariant::from(&file_value));
    }
    insert("files", QVariant::from(&files));
    (QVariant::from(&state), selections)
}

fn set_diff_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: &DiffStateRow) {
    let generation = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.next_diff_generation += 1;
        rust.next_diff_generation
    };
    let (value, selections) = to_diff(row, generation);
    backend.as_mut().rust_mut().get_mut().diff_selections = selections;
    backend.as_mut().set_diff(value);
}

fn clear_diff_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.next_diff_request += 1;
        rust.diff_selections.clear();
    }
    backend.as_mut().set_diff(empty_diff());
}

fn diff_identity(diff: &QVariant) -> Option<(String, String)> {
    let map = diff.value::<QMap<QMapPair_QString_QVariant>>()?;
    let project_id = map
        .get(&QString::from("projectId"))?
        .value::<QString>()?
        .to_string();
    let path_id = map
        .get(&QString::from("pathId"))?
        .value::<QString>()?
        .to_string();
    (!project_id.is_empty() && !path_id.is_empty()).then_some((project_id, path_id))
}

fn load_diff_with_git(
    git: &harkness_git::GitService,
    project_id: String,
    path_id: String,
    path: PathBuf,
) -> Result<DiffStateRow, GitFailure> {
    if path.as_os_str().is_empty() {
        return Err(GitFailure {
            kind: "invalid_path".to_owned(),
            message: "select a changed path before loading its diff".to_owned(),
        });
    }
    let options = harkness_git::DiffOptions::default().with_paths([path.as_path()]);
    let files = git
        .diff_snapshot(
            &[
                harkness_git::DiffTarget::Staged,
                harkness_git::DiffTarget::Unstaged,
            ],
            &options,
        )
        .map_err(GitFailure::from)?;
    Ok(DiffStateRow {
        project_id,
        path_id,
        path: path.display().to_string(),
        files,
        loading: false,
        error: String::new(),
        error_kind: String::new(),
    })
}

#[derive(Debug)]
struct GitFailure {
    kind: String,
    message: String,
}

impl From<harkness_git::GitError> for GitFailure {
    fn from(error: harkness_git::GitError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            message: error.to_string(),
        }
    }
}

fn load_project_git(project_id: &str) -> Result<harkness_git::GitService, GitFailure> {
    let id = project_id.parse().map_err(|_| GitFailure {
        kind: "invalid_project".to_owned(),
        message: "invalid project identifier".to_owned(),
    })?;
    let service = harkness_core::ProjectService::load().map_err(|error| GitFailure {
        kind: "project".to_owned(),
        message: error.to_string(),
    })?;
    service.git(id).map_err(|error| GitFailure {
        kind: "project".to_owned(),
        message: error.to_string(),
    })
}

const HISTORY_PAGE_SIZE: usize = 50;
const REVIEW_CONTEXT_STEP: u32 = 20;

#[derive(Clone, Debug)]
struct HistoryCommitRow {
    id: String,
    short_id: String,
    summary: String,
    message: String,
    author: String,
    author_email: String,
    author_time: i64,
    parent_count: usize,
}

impl From<harkness_git::CommitInfo> for HistoryCommitRow {
    fn from(commit: harkness_git::CommitInfo) -> Self {
        let id = commit.id.to_string();
        Self {
            short_id: id.chars().take(12).collect(),
            id,
            summary: display_patch_bytes(&commit.summary),
            message: String::from_utf8_lossy(&commit.message).into_owned(),
            author: String::from_utf8_lossy(&commit.author.name).into_owned(),
            author_email: String::from_utf8_lossy(&commit.author.email).into_owned(),
            author_time: commit.author.time.seconds(),
            parent_count: commit.parent_ids.len(),
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryStateRow {
    project_id: String,
    commits: Vec<HistoryCommitRow>,
    next_cursor: Option<harkness_git::LogCursor>,
    loading: bool,
    error: String,
    error_kind: String,
}

impl HistoryStateRow {
    fn loading(project_id: String) -> Self {
        Self {
            project_id,
            commits: Vec::new(),
            next_cursor: None,
            loading: true,
            error: String::new(),
            error_kind: String::new(),
        }
    }

    fn with_failure(mut self, failure: &GitFailure) -> Self {
        self.loading = false;
        self.error.clone_from(&failure.message);
        self.error_kind.clone_from(&failure.kind);
        self
    }
}

fn load_history_page_with_git(
    git: &harkness_git::GitService,
    cursor: Option<harkness_git::LogCursor>,
    cancellation: &harkness_git::Cancellation,
) -> Result<(Vec<HistoryCommitRow>, Option<harkness_git::LogCursor>), GitFailure> {
    let mut options = harkness_git::LogOptions::new("HEAD", HISTORY_PAGE_SIZE);
    if let Some(cursor) = cursor {
        options = options.with_cursor(cursor);
    }
    let page = git.log(&options, cancellation).map_err(GitFailure::from)?;
    Ok((
        page.commits
            .into_iter()
            .map(HistoryCommitRow::from)
            .collect(),
        page.next_cursor,
    ))
}

fn empty_history() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn to_history(row: &HistoryStateRow) -> QVariant {
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| state.insert(QString::from(key), value);
    insert(
        "projectId",
        QVariant::from(&QString::from(row.project_id.as_str())),
    );
    insert("loading", QVariant::from(&row.loading));
    insert("hasMore", QVariant::from(&row.next_cursor.is_some()));
    insert("error", QVariant::from(&QString::from(row.error.as_str())));
    insert(
        "errorKind",
        QVariant::from(&QString::from(row.error_kind.as_str())),
    );

    let mut commits = QList::<QVariant>::default();
    for commit in &row.commits {
        let mut value = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, field: QVariant| value.insert(QString::from(key), field);
        insert("id", QVariant::from(&QString::from(commit.id.as_str())));
        insert(
            "shortId",
            QVariant::from(&QString::from(commit.short_id.as_str())),
        );
        insert(
            "summary",
            QVariant::from(&QString::from(commit.summary.as_str())),
        );
        insert(
            "message",
            QVariant::from(&QString::from(commit.message.as_str())),
        );
        insert(
            "author",
            QVariant::from(&QString::from(commit.author.as_str())),
        );
        insert(
            "authorEmail",
            QVariant::from(&QString::from(commit.author_email.as_str())),
        );
        insert(
            "authorTime",
            QVariant::from(&QString::from(commit.author_time.to_string().as_str())),
        );
        insert(
            "parentCount",
            QVariant::from(&i32::try_from(commit.parent_count).unwrap_or(i32::MAX)),
        );
        commits.append(QVariant::from(&value));
    }
    insert("commits", QVariant::from(&commits));
    QVariant::from(&state)
}

fn set_history_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: HistoryStateRow) {
    let value = to_history(&row);
    backend.as_mut().rust_mut().get_mut().history_state = Some(row);
    backend.as_mut().set_history(value);
}

fn clear_history_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let rust = backend.as_mut().rust_mut().get_mut();
    rust.next_history_request += 1;
    rust.history_state = None;
    backend.as_mut().set_history(empty_history());
}

#[derive(Clone, Debug)]
enum ReviewSelection {
    Staged,
    Unstaged,
    Commit { revision: String },
    Branch { branch: String, base_branch: String },
}

#[derive(Clone, Debug)]
struct ReviewTargetRecord {
    target: harkness_git::DiffTarget,
    title: String,
    detail: String,
}

#[derive(Clone, Debug)]
struct ReviewFileEntry {
    id: String,
    path: PathBuf,
    file: harkness_git::FileDiff,
}

#[derive(Clone, Debug)]
struct ReviewContextLine {
    old_line: u32,
    new_line: u32,
    content: Vec<u8>,
}

#[derive(Clone, Debug)]
struct ReviewHunkState {
    id: String,
    before: Vec<ReviewContextLine>,
    after: Vec<ReviewContextLine>,
}

#[derive(Clone, Debug)]
struct ReviewLoadedFile {
    id: String,
    file: harkness_git::FileDiff,
    hunks: Vec<ReviewHunkState>,
    total_lines: Option<u32>,
}

#[derive(Clone, Debug)]
struct ReviewStateRow {
    project_id: String,
    target: Option<ReviewTargetRecord>,
    title: String,
    detail: String,
    files: Vec<ReviewFileEntry>,
    selected_file_id: String,
    loaded_file: Option<ReviewLoadedFile>,
    loading: bool,
    file_loading: bool,
    error: String,
    error_kind: String,
}

impl ReviewStateRow {
    fn loading(project_id: String, title: String, detail: String) -> Self {
        Self {
            project_id,
            target: None,
            title,
            detail,
            files: Vec::new(),
            selected_file_id: String::new(),
            loaded_file: None,
            loading: true,
            file_loading: false,
            error: String::new(),
            error_kind: String::new(),
        }
    }

    fn with_failure(mut self, failure: &GitFailure) -> Self {
        self.loading = false;
        self.file_loading = false;
        self.error.clone_from(&failure.message);
        self.error_kind.clone_from(&failure.kind);
        self
    }
}

fn review_path(file: &harkness_git::FileDiff) -> PathBuf {
    file.new_path
        .as_ref()
        .or(file.old_path.as_ref())
        .cloned()
        .unwrap_or_default()
}

fn prepare_review_target(
    git: &harkness_git::GitService,
    selection: ReviewSelection,
) -> Result<ReviewTargetRecord, GitFailure> {
    match selection {
        ReviewSelection::Staged => Ok(ReviewTargetRecord {
            target: harkness_git::DiffTarget::Staged,
            title: "Staged changes".to_owned(),
            detail: "HEAD against the index; context is pinned to recorded blobs".to_owned(),
        }),
        ReviewSelection::Unstaged => Ok(ReviewTargetRecord {
            target: harkness_git::DiffTarget::Unstaged,
            title: "Working-tree changes".to_owned(),
            detail: "Index against the working tree; changed content refreshes if it becomes stale"
                .to_owned(),
        }),
        ReviewSelection::Commit { revision } => {
            let commit = git.resolve_revision(&revision).map_err(GitFailure::from)?;
            let id = commit.to_string();
            let short = id.chars().take(12).collect::<String>();
            Ok(ReviewTargetRecord {
                target: harkness_git::DiffTarget::Commit {
                    revision: id.clone(),
                    parent: None,
                },
                title: format!("Commit {short}"),
                detail: format!("{id} against its first parent"),
            })
        }
        ReviewSelection::Branch {
            branch,
            base_branch,
        } => {
            let branch_id = git.resolve_revision(&branch).map_err(GitFailure::from)?;
            let base_id = git
                .merge_base(&branch, &base_branch)
                .map_err(GitFailure::from)?;
            let branch_short = branch_id.to_string().chars().take(12).collect::<String>();
            let base_short = base_id.to_string().chars().take(12).collect::<String>();
            Ok(ReviewTargetRecord {
                target: harkness_git::DiffTarget::Revisions {
                    old_revision: base_id.to_string(),
                    new_revision: branch_id.to_string(),
                },
                title: format!("{branch} against {base_branch}"),
                detail: format!(
                    "Pinned {branch_short} against merge-base {base_short}; only branch changes are shown"
                ),
            })
        }
    }
}

fn load_review_with_git(
    git: &harkness_git::GitService,
    project_id: String,
    selection: ReviewSelection,
    generation: u64,
) -> Result<ReviewStateRow, GitFailure> {
    let target = prepare_review_target(git, selection)?;
    // A zero content budget asks the Git service for the complete identity list while
    // intentionally omitting every hunk. Opening a path makes the second,
    // path-restricted request below, so a thousand-file review never eagerly
    // builds a thousand line models.
    let options = harkness_git::DiffOptions::default().with_max_total_bytes(0);
    let files = git
        .diff(target.target.clone(), &options)
        .map_err(GitFailure::from)?
        .into_iter()
        .enumerate()
        .map(|(index, file)| ReviewFileEntry {
            id: format!("review-file-{generation}-{index}"),
            path: review_path(&file),
            file,
        })
        .collect();
    Ok(ReviewStateRow {
        project_id,
        title: target.title.clone(),
        detail: target.detail.clone(),
        target: Some(target),
        files,
        selected_file_id: String::new(),
        loaded_file: None,
        loading: false,
        file_loading: false,
        error: String::new(),
        error_kind: String::new(),
    })
}

/// Selects the first changed path as part of opening a review target. The
/// metadata pass remains bounded and only this one default selection receives
/// a path-restricted content request.
fn load_review_with_initial_file_with_git(
    git: &harkness_git::GitService,
    project_id: String,
    selection: ReviewSelection,
    review_generation: u64,
    file_generation: u64,
) -> Result<ReviewStateRow, GitFailure> {
    let mut review = load_review_with_git(git, project_id, selection, review_generation)?;
    let Some(entry) = review.files.first().cloned() else {
        return Ok(review);
    };
    let target = review.target.as_ref().ok_or_else(|| GitFailure {
        kind: "review_target_missing".to_owned(),
        message: "The selected review target is no longer available".to_owned(),
    })?;
    let loaded = load_review_file_with_git(git, target, &entry, file_generation)?;
    review.selected_file_id.clone_from(&entry.id);
    review.loaded_file = Some(loaded);
    Ok(review)
}

fn file_context_side(file: &harkness_git::FileDiff) -> harkness_git::FileSide {
    if file.new_path.is_some() {
        harkness_git::FileSide::New
    } else {
        harkness_git::FileSide::Old
    }
}

fn hunk_side_coordinates(file: &harkness_git::FileDiff, hunk: &harkness_git::Hunk) -> (u32, u32) {
    match file_context_side(file) {
        harkness_git::FileSide::Old => (hunk.old_start, hunk.old_lines),
        harkness_git::FileSide::New => (hunk.new_start, hunk.new_lines),
        _ => (hunk.new_start, hunk.new_lines),
    }
}

fn load_review_file_with_git(
    git: &harkness_git::GitService,
    target: &ReviewTargetRecord,
    entry: &ReviewFileEntry,
    generation: u64,
) -> Result<ReviewLoadedFile, GitFailure> {
    let options = harkness_git::DiffOptions::default()
        .with_paths([entry.path.as_path()])
        .with_intra_line_ranges(true);
    let mut files = git
        .diff(target.target.clone(), &options)
        .map_err(GitFailure::from)?;
    let file = files
        .drain(..)
        .find(|file| {
            file.old_path.as_deref() == Some(entry.path.as_path())
                || file.new_path.as_deref() == Some(entry.path.as_path())
        })
        .ok_or_else(|| GitFailure {
            kind: "review_path_not_found".to_owned(),
            message: format!(
                "{} is no longer present in this review target",
                entry.path.display()
            ),
        })?;

    let total_lines = file.hunks.last().and_then(|hunk| {
        git.file_context(&harkness_git::FileContextRequest::for_hunk(
            &file,
            hunk,
            file_context_side(&file),
            0,
            0,
        ))
        .ok()
        .and_then(|response| response.total_lines)
    });
    let hunks = file
        .hunks
        .iter()
        .enumerate()
        .map(|(index, _)| ReviewHunkState {
            id: format!("review-hunk-{generation}-{index}"),
            before: Vec::new(),
            after: Vec::new(),
        })
        .collect();
    Ok(ReviewLoadedFile {
        id: entry.id.clone(),
        file,
        hunks,
        total_lines,
    })
}

fn empty_review() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn review_content_summary(file: &harkness_git::FileDiff) -> String {
    if let Some(omission) = &file.omission {
        omission_summary(omission)
    } else if file.binary {
        "Binary file — textual review is unavailable.".to_owned()
    } else if file.hunks.is_empty() {
        "No textual hunks are available for this file.".to_owned()
    } else {
        String::new()
    }
}

fn hunk_degradation_summary(hunk: &harkness_git::Hunk) -> String {
    match hunk.intra_line_degradation.as_ref() {
        Some(harkness_git::IntraLineDegradation::LineTooLong { limit }) => {
            format!("Word emphasis unavailable — a line exceeds the {limit}-byte pairing limit.")
        }
        Some(harkness_git::IntraLineDegradation::PairingTooLarge { limit }) => {
            format!("Word emphasis unavailable — pairing exceeds the {limit}-comparison limit.")
        }
        Some(_) => "Word emphasis unavailable for a named Git limit.".to_owned(),
        None => String::new(),
    }
}

fn display_line_end(bytes: &[u8]) -> usize {
    let without_newline = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    without_newline
        .strip_suffix(b"\r")
        .unwrap_or(without_newline)
        .len()
}

fn to_text_segments(
    bytes: &[u8],
    ranges: Option<&[harkness_git::IntraLineRange]>,
) -> QList<QVariant> {
    let end = display_line_end(bytes);
    let mut segments = QList::<QVariant>::default();
    let mut push = |slice: &[u8], changed: bool| {
        if slice.is_empty() {
            return;
        }
        let mut value = QMap::<QMapPair_QString_QVariant>::default();
        value.insert(
            QString::from("text"),
            QVariant::from(&QString::from(String::from_utf8_lossy(slice).as_ref())),
        );
        value.insert(QString::from("changed"), QVariant::from(&changed));
        segments.append(QVariant::from(&value));
    };

    let Some(ranges) = ranges else {
        push(&bytes[..end], false);
        return segments;
    };
    let mut cursor = 0;
    for range in ranges {
        let start = range.start.min(end).max(cursor);
        let range_end = range.end.min(end).max(start);
        push(&bytes[cursor..start], false);
        push(&bytes[start..range_end], true);
        cursor = range_end;
    }
    push(&bytes[cursor..end], false);
    segments
}

fn empty_review_side() -> QVariant {
    let mut side = QMap::<QMapPair_QString_QVariant>::default();
    side.insert(QString::from("present"), QVariant::from(&false));
    QVariant::from(&side)
}

fn to_review_side(line: &harkness_git::DiffLine, number: Option<u32>) -> QVariant {
    let (kind, marker) = diff_line_name(line.kind);
    let mut side = QMap::<QMapPair_QString_QVariant>::default();
    side.insert(QString::from("present"), QVariant::from(&true));
    side.insert(
        QString::from("line"),
        QVariant::from(&number.map_or(0, bounded_i32)),
    );
    side.insert(QString::from("kind"), QVariant::from(&QString::from(kind)));
    side.insert(
        QString::from("marker"),
        QVariant::from(&QString::from(marker)),
    );
    side.insert(
        QString::from("segments"),
        QVariant::from(&to_text_segments(
            &line.content,
            line.intra_line_ranges.as_deref(),
        )),
    );
    QVariant::from(&side)
}

fn to_unified_review_line(line: &harkness_git::DiffLine) -> QVariant {
    let (kind, marker) = diff_line_name(line.kind);
    let mut value = QMap::<QMapPair_QString_QVariant>::default();
    value.insert(
        QString::from("oldLine"),
        QVariant::from(&line.old_line_number.map_or(0, bounded_i32)),
    );
    value.insert(
        QString::from("newLine"),
        QVariant::from(&line.new_line_number.map_or(0, bounded_i32)),
    );
    value.insert(QString::from("kind"), QVariant::from(&QString::from(kind)));
    value.insert(
        QString::from("marker"),
        QVariant::from(&QString::from(marker)),
    );
    value.insert(
        QString::from("segments"),
        QVariant::from(&to_text_segments(
            &line.content,
            line.intra_line_ranges.as_deref(),
        )),
    );
    QVariant::from(&value)
}

fn to_review_line_row(hunk: &harkness_git::Hunk, index: usize) -> QVariant {
    let line = &hunk.lines[index];
    let partner = line
        .paired_line_index
        .and_then(|partner| hunk.lines.get(partner));
    let split_hidden = matches!(line.kind, harkness_git::DiffLineKind::Addition)
        && partner
            .is_some_and(|partner| matches!(partner.kind, harkness_git::DiffLineKind::Deletion));
    let (old, new) = match line.kind {
        harkness_git::DiffLineKind::Context => (
            to_review_side(line, line.old_line_number),
            to_review_side(line, line.new_line_number),
        ),
        harkness_git::DiffLineKind::Deletion => (
            to_review_side(line, line.old_line_number),
            partner.map_or_else(empty_review_side, |partner| {
                to_review_side(partner, partner.new_line_number)
            }),
        ),
        harkness_git::DiffLineKind::Addition => {
            if split_hidden {
                (empty_review_side(), empty_review_side())
            } else {
                (
                    empty_review_side(),
                    to_review_side(line, line.new_line_number),
                )
            }
        }
        _ => (
            to_review_side(line, line.old_line_number),
            to_review_side(line, line.new_line_number),
        ),
    };
    let mut row = QMap::<QMapPair_QString_QVariant>::default();
    row.insert(
        QString::from("type"),
        QVariant::from(&QString::from("line")),
    );
    row.insert(QString::from("unified"), to_unified_review_line(line));
    row.insert(QString::from("old"), old);
    row.insert(QString::from("new"), new);
    row.insert(QString::from("splitHidden"), QVariant::from(&split_hidden));
    QVariant::from(&row)
}

fn to_context_side(content: &[u8], number: u32, present: bool) -> QVariant {
    if !present {
        return empty_review_side();
    }
    let mut side = QMap::<QMapPair_QString_QVariant>::default();
    side.insert(QString::from("present"), QVariant::from(&true));
    side.insert(QString::from("line"), QVariant::from(&bounded_i32(number)));
    side.insert(
        QString::from("kind"),
        QVariant::from(&QString::from("context")),
    );
    side.insert(QString::from("marker"), QVariant::from(&QString::from(" ")));
    side.insert(
        QString::from("segments"),
        QVariant::from(&to_text_segments(content, None)),
    );
    QVariant::from(&side)
}

fn to_context_row(line: &ReviewContextLine) -> QVariant {
    let mut unified = QMap::<QMapPair_QString_QVariant>::default();
    unified.insert(
        QString::from("oldLine"),
        QVariant::from(&bounded_i32(line.old_line)),
    );
    unified.insert(
        QString::from("newLine"),
        QVariant::from(&bounded_i32(line.new_line)),
    );
    unified.insert(
        QString::from("kind"),
        QVariant::from(&QString::from("context")),
    );
    unified.insert(QString::from("marker"), QVariant::from(&QString::from(" ")));
    unified.insert(
        QString::from("segments"),
        QVariant::from(&to_text_segments(&line.content, None)),
    );

    let mut row = QMap::<QMapPair_QString_QVariant>::default();
    row.insert(
        QString::from("type"),
        QVariant::from(&QString::from("line")),
    );
    row.insert(QString::from("unified"), QVariant::from(&unified));
    row.insert(
        QString::from("old"),
        to_context_side(&line.content, line.old_line, line.old_line > 0),
    );
    row.insert(
        QString::from("new"),
        to_context_side(&line.content, line.new_line, line.new_line > 0),
    );
    row.insert(QString::from("splitHidden"), QVariant::from(&false));
    QVariant::from(&row)
}

fn collapsed_review_row(hunk_id: &str, direction: &str, count: u32) -> QVariant {
    let mut row = QMap::<QMapPair_QString_QVariant>::default();
    row.insert(
        QString::from("type"),
        QVariant::from(&QString::from("collapsed")),
    );
    row.insert(
        QString::from("hunkId"),
        QVariant::from(&QString::from(hunk_id)),
    );
    row.insert(
        QString::from("direction"),
        QVariant::from(&QString::from(direction)),
    );
    row.insert(QString::from("count"), QVariant::from(&bounded_i32(count)));
    QVariant::from(&row)
}

fn review_hunk_row(hunk_id: &str, hunk: &harkness_git::Hunk) -> QVariant {
    let mut row = QMap::<QMapPair_QString_QVariant>::default();
    row.insert(
        QString::from("type"),
        QVariant::from(&QString::from("hunk")),
    );
    row.insert(
        QString::from("hunkId"),
        QVariant::from(&QString::from(hunk_id)),
    );
    row.insert(
        QString::from("header"),
        QVariant::from(&QString::from(display_patch_bytes(&hunk.header).as_str())),
    );
    row.insert(
        QString::from("degradation"),
        QVariant::from(&QString::from(hunk_degradation_summary(hunk).as_str())),
    );
    QVariant::from(&row)
}

fn hidden_before(loaded: &ReviewLoadedFile, index: usize) -> u32 {
    let (start, _) = hunk_side_coordinates(&loaded.file, &loaded.file.hunks[index]);
    let prior_end = index.checked_sub(1).map_or(1, |prior| {
        let (prior_start, prior_count) =
            hunk_side_coordinates(&loaded.file, &loaded.file.hunks[prior]);
        prior_start.saturating_add(prior_count)
    });
    start.saturating_sub(prior_end)
}

fn hidden_after(loaded: &ReviewLoadedFile, index: usize) -> u32 {
    if index + 1 < loaded.file.hunks.len() {
        return 0;
    }
    let Some(total_lines) = loaded.total_lines else {
        return 0;
    };
    let (start, count) = hunk_side_coordinates(&loaded.file, &loaded.file.hunks[index]);
    total_lines.saturating_sub(start.saturating_add(count).saturating_sub(1))
}

fn review_rows(loaded: &ReviewLoadedFile) -> QList<QVariant> {
    let mut rows = QList::<QVariant>::default();
    for (index, (hunk, state)) in loaded.file.hunks.iter().zip(&loaded.hunks).enumerate() {
        let remaining_before = hidden_before(loaded, index)
            .saturating_sub(u32::try_from(state.before.len()).unwrap_or(u32::MAX));
        if remaining_before > 0 {
            rows.append(collapsed_review_row(&state.id, "before", remaining_before));
        }
        for line in &state.before {
            rows.append(to_context_row(line));
        }
        rows.append(review_hunk_row(&state.id, hunk));
        for line_index in 0..hunk.lines.len() {
            rows.append(to_review_line_row(hunk, line_index));
        }
        if index + 1 == loaded.file.hunks.len() {
            for line in &state.after {
                rows.append(to_context_row(line));
            }
            let remaining_after = hidden_after(loaded, index)
                .saturating_sub(u32::try_from(state.after.len()).unwrap_or(u32::MAX));
            if remaining_after > 0 {
                rows.append(collapsed_review_row(&state.id, "after", remaining_after));
            }
        }
    }
    rows
}

fn to_review(row: &ReviewStateRow) -> QVariant {
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| state.insert(QString::from(key), value);
    insert(
        "projectId",
        QVariant::from(&QString::from(row.project_id.as_str())),
    );
    insert("title", QVariant::from(&QString::from(row.title.as_str())));
    insert(
        "detail",
        QVariant::from(&QString::from(row.detail.as_str())),
    );
    insert("loading", QVariant::from(&row.loading));
    insert("fileLoading", QVariant::from(&row.file_loading));
    insert("error", QVariant::from(&QString::from(row.error.as_str())));
    insert(
        "errorKind",
        QVariant::from(&QString::from(row.error_kind.as_str())),
    );
    insert(
        "selectedFileId",
        QVariant::from(&QString::from(row.selected_file_id.as_str())),
    );

    let mut files = QList::<QVariant>::default();
    for entry in &row.files {
        let mut value = QMap::<QMapPair_QString_QVariant>::default();
        value.insert(
            QString::from("fileId"),
            QVariant::from(&QString::from(entry.id.as_str())),
        );
        value.insert(
            QString::from("path"),
            QVariant::from(&QString::from(display_diff_path(&entry.file).as_str())),
        );
        value.insert(
            QString::from("change"),
            QVariant::from(&QString::from(change_name(entry.file.change))),
        );
        value.insert(
            QString::from("oldSize"),
            QVariant::from(&QString::from(entry.file.old_size.to_string().as_str())),
        );
        value.insert(
            QString::from("newSize"),
            QVariant::from(&QString::from(entry.file.new_size.to_string().as_str())),
        );
        files.append(QVariant::from(&value));
    }
    insert("files", QVariant::from(&files));

    let file = row.loaded_file.as_ref().map_or_else(
        || QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
        |loaded| {
            let mut value = QMap::<QMapPair_QString_QVariant>::default();
            value.insert(
                QString::from("fileId"),
                QVariant::from(&QString::from(loaded.id.as_str())),
            );
            value.insert(
                QString::from("path"),
                QVariant::from(&QString::from(display_diff_path(&loaded.file).as_str())),
            );
            value.insert(
                QString::from("summary"),
                QVariant::from(&QString::from(
                    review_content_summary(&loaded.file).as_str(),
                )),
            );
            value.insert(QString::from("binary"), QVariant::from(&loaded.file.binary));
            value.insert(
                QString::from("hunkCount"),
                QVariant::from(&i32::try_from(loaded.file.hunks.len()).unwrap_or(i32::MAX)),
            );
            value.insert(QString::from("rows"), QVariant::from(&review_rows(loaded)));
            QVariant::from(&value)
        },
    );
    insert("file", file);
    QVariant::from(&state)
}

fn set_review_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: ReviewStateRow) {
    let value = to_review(&row);
    backend.as_mut().rust_mut().get_mut().review_state = Some(row);
    backend.as_mut().set_review(value);
}

fn clear_review_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let rust = backend.as_mut().rust_mut().get_mut();
    rust.next_review_request += 1;
    rust.next_review_file_request += 1;
    rust.review_state = None;
    backend.as_mut().set_review(empty_review());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewContextDirection {
    Before,
    After,
}

impl ReviewContextDirection {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "before" => Some(Self::Before),
            "after" => Some(Self::After),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum ReviewContextOutcome {
    Loaded(Box<ReviewLoadedFile>),
    Stale,
}

fn context_omission_summary(omission: &harkness_git::FileContextOmission) -> String {
    match omission {
        harkness_git::FileContextOmission::FileTooLarge { limit } => {
            format!("File too large — context exceeds the {limit}-byte display limit.")
        }
        harkness_git::FileContextOmission::ContentBudgetExhausted { limit } => {
            format!("Context budget exhausted — the range exceeds {limit} bytes.")
        }
        _ => "Context omitted for a named Git limit.".to_owned(),
    }
}

fn translated_line(number: u32, from_start: u32, to_start: u32) -> u32 {
    let translated = i64::from(number) + i64::from(to_start) - i64::from(from_start);
    u32::try_from(translated).unwrap_or(0)
}

fn project_review_context(
    response: &harkness_git::FileContextResponse,
    hunk: &harkness_git::Hunk,
    direction: ReviewContextDirection,
) -> Vec<ReviewContextLine> {
    let (side_start, side_count) = match response.side {
        harkness_git::FileSide::Old => (hunk.old_start, hunk.old_lines),
        harkness_git::FileSide::New => (hunk.new_start, hunk.new_lines),
        _ => (hunk.new_start, hunk.new_lines),
    };
    let side_end = side_start.saturating_add(side_count);
    response
        .lines
        .iter()
        .filter_map(|line| {
            let number = match response.side {
                harkness_git::FileSide::Old => line.old_line_number,
                harkness_git::FileSide::New => line.new_line_number,
                _ => line.new_line_number,
            }?;
            let belongs = match direction {
                ReviewContextDirection::Before => number < side_start,
                ReviewContextDirection::After => number >= side_end,
            };
            if !belongs {
                return None;
            }
            let (old_line, new_line) = match (response.side, direction) {
                (harkness_git::FileSide::New, ReviewContextDirection::Before) => (
                    translated_line(number, hunk.new_start, hunk.old_start),
                    number,
                ),
                (harkness_git::FileSide::New, ReviewContextDirection::After) => (
                    translated_line(
                        number,
                        hunk.new_start.saturating_add(hunk.new_lines),
                        hunk.old_start.saturating_add(hunk.old_lines),
                    ),
                    number,
                ),
                (harkness_git::FileSide::Old, ReviewContextDirection::Before) => (
                    number,
                    translated_line(number, hunk.old_start, hunk.new_start),
                ),
                (harkness_git::FileSide::Old, ReviewContextDirection::After) => (
                    number,
                    translated_line(
                        number,
                        hunk.old_start.saturating_add(hunk.old_lines),
                        hunk.new_start.saturating_add(hunk.new_lines),
                    ),
                ),
                _ => (number, number),
            };
            Some(ReviewContextLine {
                old_line,
                new_line,
                content: line.content.clone(),
            })
        })
        .collect()
}

fn expand_review_context_with_git(
    git: &harkness_git::GitService,
    mut loaded: ReviewLoadedFile,
    hunk_id: &str,
    direction: ReviewContextDirection,
) -> Result<ReviewContextOutcome, GitFailure> {
    let Some(index) = loaded.hunks.iter().position(|hunk| hunk.id == hunk_id) else {
        return Err(GitFailure {
            kind: "review_hunk_not_found".to_owned(),
            message: "The selected hunk is no longer available; reopen the file".to_owned(),
        });
    };
    let available = match direction {
        ReviewContextDirection::Before => hidden_before(&loaded, index),
        ReviewContextDirection::After => hidden_after(&loaded, index),
    };
    let current = match direction {
        ReviewContextDirection::Before => loaded.hunks[index].before.len(),
        ReviewContextDirection::After => loaded.hunks[index].after.len(),
    };
    let requested = u32::try_from(current)
        .unwrap_or(u32::MAX)
        .saturating_add(REVIEW_CONTEXT_STEP)
        .min(available);
    if requested <= u32::try_from(current).unwrap_or(u32::MAX) {
        return Ok(ReviewContextOutcome::Loaded(Box::new(loaded)));
    }
    let hunk = &loaded.file.hunks[index];
    let (before, after) = match direction {
        ReviewContextDirection::Before => (requested, 0),
        ReviewContextDirection::After => (0, requested),
    };
    let request = harkness_git::FileContextRequest::for_hunk(
        &loaded.file,
        hunk,
        file_context_side(&loaded.file),
        before,
        after,
    );
    let response = match git.file_context(&request) {
        Ok(response) => response,
        Err(harkness_git::GitError::StaleHunkSelection { .. }) => {
            return Ok(ReviewContextOutcome::Stale);
        }
        Err(error) => return Err(GitFailure::from(error)),
    };
    if let Some(omission) = response.omission.as_ref() {
        return Err(GitFailure {
            kind: "review_context_omitted".to_owned(),
            message: context_omission_summary(omission),
        });
    }
    let lines = project_review_context(&response, hunk, direction);
    loaded.total_lines = response.total_lines.or(loaded.total_lines);
    match direction {
        ReviewContextDirection::Before => loaded.hunks[index].before = lines,
        ReviewContextDirection::After => loaded.hunks[index].after = lines,
    }
    Ok(ReviewContextOutcome::Loaded(Box::new(loaded)))
}

fn launch_review_request(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: String,
    selection: ReviewSelection,
    loading_title: String,
    loading_detail: String,
) {
    let Some((job_id, _cancellation)) = start_job(
        backend.as_mut(),
        "review",
        &project_id,
        "Load review",
        false,
    ) else {
        return;
    };
    let (request_id, file_request) = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.next_review_request += 1;
        rust.next_review_file_request += 1;
        (rust.next_review_request, rust.next_review_file_request)
    };
    set_review_state(
        backend.as_mut(),
        ReviewStateRow::loading(project_id.clone(), loading_title, loading_detail),
    );
    let qt_thread = backend.qt_thread();
    std::thread::spawn(move || {
        let result = load_project_git(&project_id).and_then(|git| {
            load_review_with_initial_file_with_git(
                &git,
                project_id.clone(),
                selection,
                request_id,
                file_request,
            )
        });
        let _ = qt_thread.queue(move |mut backend| {
            finish_job(backend.as_mut(), &job_id);
            if backend.as_ref().rust().next_review_request != request_id
                || backend.as_ref().rust().next_review_file_request != file_request
                || opened_project_id(backend.as_ref().opened()).as_deref()
                    != Some(project_id.as_str())
            {
                return;
            }
            match result {
                Ok(row) => set_review_state(backend.as_mut(), row),
                Err(failure) => {
                    backend.as_mut().set_status(failure.message.as_str().into());
                    let row = backend
                        .as_ref()
                        .rust()
                        .review_state
                        .clone()
                        .unwrap_or_else(|| {
                            ReviewStateRow::loading(project_id, "Review".to_owned(), String::new())
                        })
                        .with_failure(&failure);
                    set_review_state(backend.as_mut(), row);
                }
            }
        });
    });
}

#[derive(Debug)]
struct GitWorkerResult {
    project_id: String,
    message: Result<String, GitFailure>,
    state: Option<GitStateRow>,
}

fn run_git_operation(
    project_id: String,
    cancellation: &harkness_git::Cancellation,
    operation: impl FnOnce(
        &harkness_git::GitService,
        &harkness_git::Cancellation,
    ) -> Result<String, GitFailure>,
) -> GitWorkerResult {
    let git = load_project_git(&project_id);
    let git = match git {
        Ok(git) => git,
        Err(failure) => {
            return GitWorkerResult {
                project_id,
                message: Err(failure),
                state: None,
            };
        }
    };
    let mut message = operation(&git, cancellation);
    // A cancelled or failed mutation can still have changed the repository.
    // Use a fresh token so the mandatory post-operation refresh is not itself
    // suppressed by the user's cancellation request.
    let state = match git.detailed_status(&harkness_git::Cancellation::default()) {
        Ok(status) => {
            let state = GitStateRow::from_status(project_id.clone(), status);
            Some(match &message {
                Ok(_) => state,
                Err(failure) => state.with_failure(failure),
            })
        }
        Err(error) => {
            if message.is_ok() {
                message = Err(GitFailure::from(error));
            }
            None
        }
    };
    GitWorkerResult {
        project_id,
        message,
        state,
    }
}

fn apply_git_result(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    job_id: &str,
    result: GitWorkerResult,
    refresh_catalog: bool,
    refresh_branches: bool,
    quiet_success: bool,
) {
    finish_job(backend.as_mut(), job_id);
    let is_open =
        opened_project_id(backend.as_ref().opened()).as_deref() == Some(result.project_id.as_str());
    let refresh_diff = is_open && result.state.is_some();
    let project_id = result.project_id.clone();
    if is_open {
        if let Some(state) = &result.state {
            set_git_state(backend.as_mut(), state);
        }
        match result.message {
            Ok(message) if !quiet_success => backend.as_mut().set_status(message.into()),
            Ok(_) => {}
            Err(failure) => backend.as_mut().set_status(failure.message.into()),
        }
        if refresh_branches {
            backend
                .as_mut()
                .refresh_branches(&QString::from(result.project_id.as_str()));
        }
    }
    if refresh_catalog {
        backend.as_mut().refresh();
    }
    if refresh_diff {
        refresh_current_diff(backend.as_mut(), &project_id);
    }
}

fn refresh_current_diff(mut backend: Pin<&mut ffi::HarknessBackend>, project_id: &str) {
    let Some((diff_project_id, path_id)) = diff_identity(backend.as_ref().diff()) else {
        return;
    };
    if diff_project_id != project_id {
        return;
    }
    let project_id = QString::from(project_id);
    let path_id = QString::from(path_id.as_str());
    backend.as_mut().refresh_diff(&project_id, &path_id);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HunkAction {
    Stage,
    Unstage,
}

impl HunkAction {
    fn kind(self) -> &'static str {
        match self {
            Self::Stage => "stage_hunk",
            Self::Unstage => "unstage_hunk",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Stage => "Stage hunk",
            Self::Unstage => "Unstage hunk",
        }
    }

    fn matches(self, target: &harkness_git::DiffTarget) -> bool {
        matches!(
            (self, target),
            (Self::Stage, harkness_git::DiffTarget::Unstaged)
                | (Self::Unstage, harkness_git::DiffTarget::Staged)
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
enum HunkMutationOutcome {
    Applied(usize),
    Stale,
}

fn mutate_hunk_with_git(
    git: &harkness_git::GitService,
    action: HunkAction,
    selection: &harkness_git::HunkSelection,
    cancellation: &harkness_git::Cancellation,
) -> Result<HunkMutationOutcome, GitFailure> {
    let result = match action {
        HunkAction::Stage => git.stage_hunks(std::slice::from_ref(selection), cancellation),
        HunkAction::Unstage => git.unstage_hunks(std::slice::from_ref(selection), cancellation),
    };
    match result {
        Ok(outcome) => Ok(HunkMutationOutcome::Applied(outcome.hunks)),
        Err(harkness_git::GitError::StaleHunkSelection { .. }) => Ok(HunkMutationOutcome::Stale),
        Err(error) => Err(GitFailure::from(error)),
    }
}

fn launch_hunk_operation(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: &QString,
    selection_id: &QString,
    action: HunkAction,
) {
    let project_id = project_id.to_string();
    let selection_id = selection_id.to_string();
    let selection = backend
        .as_ref()
        .rust()
        .diff_selections
        .get(&selection_id)
        .cloned();
    let Some(selection) = selection else {
        backend
            .as_mut()
            .set_status("The selected hunk is no longer available; refresh the diff".into());
        return;
    };
    if selection.project_id != project_id {
        backend
            .as_mut()
            .set_status("The selected hunk belongs to a different project".into());
        return;
    }
    if !action.matches(&selection.target) {
        backend
            .as_mut()
            .set_status("The selected hunk belongs to the other side of the index".into());
        return;
    }
    let Some((job_id, cancellation)) = start_job(
        backend.as_mut(),
        action.kind(),
        &project_id,
        action.label(),
        true,
    ) else {
        return;
    };
    let qt_thread = backend.qt_thread();
    std::thread::spawn(move || {
        let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
            match mutate_hunk_with_git(git, action, &selection.selection, cancellation)? {
                HunkMutationOutcome::Applied(count) => Ok(format!(
                    "{} {count} hunk{}",
                    if action == HunkAction::Stage {
                        "Staged"
                    } else {
                        "Unstaged"
                    },
                    if count == 1 { "" } else { "s" }
                )),
                HunkMutationOutcome::Stale => Ok(
                    "The file changed; refreshed the diff without changing the index".to_owned(),
                ),
            }
        });
        let _ = qt_thread.queue(move |mut backend| {
            apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
        });
    });
}

#[derive(Debug)]
struct BranchRow {
    name: String,
    current: bool,
    selectable: bool,
    detail: String,
}

impl From<harkness_git::Branch> for BranchRow {
    fn from(branch: harkness_git::Branch) -> Self {
        let (current, selectable, detail) = match branch.checkout {
            harkness_git::BranchCheckout::NotCheckedOut => (false, true, String::new()),
            harkness_git::BranchCheckout::CurrentWorktree => {
                (true, true, "Checked out here".to_owned())
            }
            harkness_git::BranchCheckout::OtherWorktree(path) => {
                (false, false, format!("Checked out at {}", path.display()))
            }
        };
        Self {
            name: branch.name,
            current,
            selectable,
            detail,
        }
    }
}

/// One catalog entry flattened into the plain data a QML delegate binds to.
///
/// Qt value types are not `Send`, so the catalog crosses the thread boundary
/// as these rows and only becomes a `QVariantList` on the GUI thread.
#[derive(Debug)]
struct ProjectRow {
    id: String,
    display_name: String,
    root: String,
    remote: String,
    branch: String,
    managed: bool,
    worktree: bool,
    parent_id: String,
    parent_name: String,
    created_branch: String,
    available: bool,
    is_git: bool,
    dirty: bool,
}

impl ProjectRow {
    fn from_project(project: harkness_core::Project, parent_name: String) -> Self {
        let (remote, managed, worktree, parent_id, created_branch) = match &project.source {
            harkness_core::ProjectSource::Local => {
                (String::new(), false, false, String::new(), String::new())
            }
            harkness_core::ProjectSource::ManagedRepository { remote } => {
                (remote.clone(), true, false, String::new(), String::new())
            }
            harkness_core::ProjectSource::Worktree {
                parent,
                worktree_branch,
            } => (
                String::new(),
                false,
                true,
                parent.to_string(),
                worktree_branch.clone().unwrap_or_default(),
            ),
        };
        Self {
            id: project.id.to_string(),
            display_name: project.display_name,
            root: project.root.display().to_string(),
            remote,
            // Left empty for a detached head, which `is_git` distinguishes
            // from a directory that is not a repository at all.
            branch: project
                .git
                .as_ref()
                .and_then(|git| git.branch.clone())
                .unwrap_or_default(),
            managed,
            worktree,
            parent_id,
            parent_name,
            created_branch,
            available: project.available,
            is_git: project.git.is_some(),
            dirty: project.git.is_some_and(|git| git.dirty),
        }
    }
}

impl From<harkness_core::Project> for ProjectRow {
    fn from(project: harkness_core::Project) -> Self {
        Self::from_project(project, String::new())
    }
}

fn project_rows(projects: Vec<harkness_core::Project>) -> Vec<ProjectRow> {
    let names = projects
        .iter()
        .map(|project| (project.id, project.display_name.clone()))
        .collect::<HashMap<_, _>>();
    projects
        .into_iter()
        .map(|project| {
            let parent_name = match &project.source {
                harkness_core::ProjectSource::Worktree { parent, .. } => {
                    names.get(parent).cloned().unwrap_or_default()
                }
                _ => String::new(),
            };
            ProjectRow::from_project(project, parent_name)
        })
        .collect()
}

/// Reads the catalog. Availability and Git state are recomputed per entry, so
/// this touches the filesystem once per project and belongs off the GUI thread.
fn load_rows() -> Result<Vec<ProjectRow>, String> {
    let service = harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
    service
        .list()
        .map(project_rows)
        .map_err(|error| error.to_string())
}

/// Flattens a row into the QVariantMap every QML binding reads.
fn to_map(row: &ProjectRow) -> QVariant {
    let mut entry = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| entry.insert(QString::from(key), value);
    insert("id", QVariant::from(&QString::from(row.id.as_str())));
    insert(
        "displayName",
        QVariant::from(&QString::from(row.display_name.as_str())),
    );
    insert("root", QVariant::from(&QString::from(row.root.as_str())));
    insert(
        "remote",
        QVariant::from(&QString::from(row.remote.as_str())),
    );
    insert(
        "branch",
        QVariant::from(&QString::from(row.branch.as_str())),
    );
    insert("managed", QVariant::from(&row.managed));
    insert("worktree", QVariant::from(&row.worktree));
    insert(
        "parentId",
        QVariant::from(&QString::from(row.parent_id.as_str())),
    );
    insert(
        "parentName",
        QVariant::from(&QString::from(row.parent_name.as_str())),
    );
    insert(
        "createdBranch",
        QVariant::from(&QString::from(row.created_branch.as_str())),
    );
    insert("available", QVariant::from(&row.available));
    insert("isGit", QVariant::from(&row.is_git));
    insert("dirty", QVariant::from(&row.dirty));
    QVariant::from(&entry)
}

#[derive(Debug)]
struct WorktreeRow {
    id: String,
    root: String,
    branch: String,
    owned: bool,
    locked: bool,
    /// Empty when unlocked and when Git recorded a lock without a reason;
    /// `locked` stays the authoritative state.
    lock_reason: String,
    prunable: bool,
}

impl From<harkness_core::Worktree> for WorktreeRow {
    fn from(worktree: harkness_core::Worktree) -> Self {
        let id = worktree
            .project
            .as_ref()
            .map(|project| project.id.to_string())
            .unwrap_or_default();
        Self {
            id,
            root: worktree.root.display().to_string(),
            branch: worktree.branch.unwrap_or_default(),
            owned: worktree.project.is_some(),
            locked: worktree.locked,
            lock_reason: worktree.lock_reason.unwrap_or_default(),
            prunable: worktree.prunable,
        }
    }
}

fn to_worktrees(rows: &[WorktreeRow]) -> QList<QVariant> {
    let mut worktrees = QList::<QVariant>::default();
    for row in rows {
        let mut entry = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, value: QVariant| entry.insert(QString::from(key), value);
        insert("id", QVariant::from(&QString::from(row.id.as_str())));
        insert("root", QVariant::from(&QString::from(row.root.as_str())));
        insert(
            "branch",
            QVariant::from(&QString::from(row.branch.as_str())),
        );
        insert("owned", QVariant::from(&row.owned));
        insert("locked", QVariant::from(&row.locked));
        insert(
            "lockReason",
            QVariant::from(&QString::from(row.lock_reason.as_str())),
        );
        insert("prunable", QVariant::from(&row.prunable));
        worktrees.append(QVariant::from(&entry));
    }
    worktrees
}

fn worktree_base(
    mode: &str,
    branch: &str,
    start_point: &str,
) -> Result<harkness_git::WorktreeBase, String> {
    let branch = branch.trim();
    let start_point = start_point.trim();
    match mode {
        "new" if branch.is_empty() => Err("Enter a name for the new branch".to_owned()),
        "new" => Ok(harkness_git::WorktreeBase::NewBranch {
            name: branch.to_owned(),
            start_point: (!start_point.is_empty()).then(|| start_point.to_owned()),
        }),
        "existing" if branch.is_empty() => Err("Enter an existing branch name".to_owned()),
        "existing" => Ok(harkness_git::WorktreeBase::ExistingBranch {
            name: branch.to_owned(),
        }),
        "detached" if start_point.is_empty() => {
            Err("Enter a commit or revision for detached HEAD".to_owned())
        }
        "detached" => Ok(harkness_git::WorktreeBase::Detached {
            commit: start_point.to_owned(),
        }),
        _ => Err("invalid worktree creation mode".to_owned()),
    }
}

fn remove_worktree_with_service(
    service: &mut harkness_core::ProjectService,
    project_id: &str,
    force: bool,
    cancellation: &harkness_git::Cancellation,
) -> Result<harkness_core::Project, String> {
    let id = project_id
        .parse()
        .map_err(|_| "invalid worktree project identifier".to_owned())?;
    service
        .remove_worktree(id, force, cancellation)
        .map_err(|error| error.to_string())
}

fn move_worktree_with_service(
    service: &mut harkness_core::ProjectService,
    project_id: &str,
    destination: &str,
    cancellation: &harkness_git::Cancellation,
) -> Result<harkness_core::Project, String> {
    let id = project_id
        .parse()
        .map_err(|_| "invalid worktree project identifier".to_owned())?;
    let destination = destination.trim();
    if destination.is_empty() {
        return Err("Enter an absolute destination path".to_owned());
    }
    service
        .move_worktree(id, std::path::Path::new(destination), cancellation)
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorktreeLockAction {
    Lock(String),
    Unlock,
}

#[derive(Debug)]
struct WorktreeLockOutcome {
    message: Result<String, String>,
    rows: Result<Vec<WorktreeRow>, String>,
}

fn change_worktree_lock_with_service(
    service: &mut harkness_core::ProjectService,
    project_id: &str,
    expected_parent_id: &str,
    action: &WorktreeLockAction,
    cancellation: &harkness_git::Cancellation,
) -> Result<WorktreeLockOutcome, String> {
    let id = project_id
        .parse()
        .map_err(|_| "invalid worktree project identifier".to_owned())?;
    let worktree = service
        .resolve(&harkness_core::ProjectSelector::Value(
            project_id.to_owned(),
        ))
        .map_err(|error| error.to_string())?;
    let harkness_core::ProjectSource::Worktree { parent, .. } = &worktree.source else {
        return Err(format!(
            "{} is not a Harkness-managed worktree",
            worktree.display_name
        ));
    };
    let parent = *parent;
    if parent.to_string() != expected_parent_id {
        return Err(format!(
            "{} does not belong to the open parent project",
            worktree.display_name
        ));
    }
    let message = match action {
        WorktreeLockAction::Lock(reason) => service
            .lock_worktree(id, reason, cancellation)
            .map_err(|error| error.to_string())
            .map(|()| format!("Locked {}: {}", worktree.display_name, reason.trim())),
        WorktreeLockAction::Unlock => service
            .unlock_worktree(id, cancellation)
            .map_err(|error| error.to_string())
            .map(|()| format!("Unlocked {}", worktree.display_name)),
    };
    // Git may have committed the mutation just before cancellation is
    // observed. Refresh even after an operation error, using a fresh token so
    // the row always reflects the repository's actual lock state.
    let rows = service
        .worktrees(parent, &harkness_git::Cancellation::default())
        .map(|rows| rows.into_iter().map(WorktreeRow::from).collect())
        .map_err(|error| error.to_string());
    Ok(WorktreeLockOutcome { message, rows })
}

fn launch_worktree_lock_operation(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: &QString,
    action: WorktreeLockAction,
) {
    let project_id = project_id.to_string();
    let Some(scope_project_id) = opened_project_id(backend.as_ref().opened()) else {
        backend
            .as_mut()
            .set_status("Open the worktree's parent project before changing its lock".into());
        return;
    };
    let (kind, label) = match &action {
        WorktreeLockAction::Lock(_) => ("lock_worktree", "Lock worktree"),
        WorktreeLockAction::Unlock => ("unlock_worktree", "Unlock worktree"),
    };
    let Some((job_id, cancellation)) = start_job(backend.as_mut(), kind, &project_id, label, true)
    else {
        return;
    };
    let qt_thread = backend.qt_thread();
    std::thread::spawn(move || {
        let result = (|| {
            let mut service =
                harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
            change_worktree_lock_with_service(
                &mut service,
                &project_id,
                &scope_project_id,
                &action,
                &cancellation,
            )
        })();
        let _ = qt_thread.queue(move |mut backend| {
            finish_job(backend.as_mut(), &job_id);
            if opened_project_id(backend.as_ref().opened()).as_deref()
                != Some(scope_project_id.as_str())
            {
                return;
            }
            match result {
                Ok(WorktreeLockOutcome { message, rows }) => {
                    if let Ok(rows) = &rows {
                        backend.as_mut().set_worktrees(to_worktrees(rows));
                    }
                    let status = match (message, rows) {
                        (Ok(message), Ok(_)) => message,
                        (Ok(message), Err(error)) => {
                            format!("{message}, but refreshing worktrees failed: {error}")
                        }
                        (Err(error), Ok(_)) => error,
                        (Err(error), Err(refresh_error)) => {
                            format!("{error}; refreshing worktrees also failed: {refresh_error}")
                        }
                    };
                    backend.as_mut().set_status(status.into());
                }
                Err(error) => backend.as_mut().set_status(error.into()),
            }
        });
    });
}

fn to_projects(rows: &[ProjectRow]) -> QList<QVariant> {
    let mut projects = QList::<QVariant>::default();
    for row in rows {
        projects.append(to_map(row));
    }
    projects
}

fn to_branches(rows: &[BranchRow]) -> QList<QVariant> {
    let mut branches = QList::<QVariant>::default();
    for row in rows {
        let mut entry = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, value: QVariant| entry.insert(QString::from(key), value);
        insert("name", QVariant::from(&QString::from(row.name.as_str())));
        insert("current", QVariant::from(&row.current));
        insert("selectable", QVariant::from(&row.selectable));
        insert(
            "detail",
            QVariant::from(&QString::from(row.detail.as_str())),
        );
        branches.append(QVariant::from(&entry));
    }
    branches
}

fn load_branches(project_id: &str) -> Result<Vec<BranchRow>, String> {
    let id = project_id
        .parse()
        .map_err(|_| "invalid project identifier".to_owned())?;
    let service = harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
    let git = service.git(id).map_err(|error| error.to_string())?;
    git.branches(
        &harkness_git::BranchListOptions {
            include_remote_tracking: false,
            calculate_divergence: false,
        },
        &harkness_git::Cancellation::default(),
    )
    .map(|branches| branches.into_iter().map(BranchRow::from).collect())
    .map_err(|error| error.to_string())
}

/// The `opened` value while no project is open: an empty map, so QML can
/// always treat `opened` as a map and test `opened.id` for emptiness.
fn empty_opened() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn opened_project_id(opened: &QVariant) -> Option<String> {
    opened
        .value::<QMap<QMapPair_QString_QVariant>>()?
        .get(&QString::from("id"))?
        .value::<QString>()
        .map(|id| id.to_string())
        .filter(|id| !id.is_empty())
}

#[derive(Debug)]
enum OpenedUpdate {
    Keep,
    Open(ProjectRow),
    Clear,
}

#[derive(Debug)]
struct OperationOutcome {
    status: String,
    opened: OpenedUpdate,
}

/// Converts a service result into the complete user-visible state transition.
/// Keeping this independent of Qt makes success, failure, open, and removal
/// behavior deterministic and directly testable.
fn operation_outcome(
    result: Result<harkness_core::Project, String>,
    verb: &str,
    opens: bool,
) -> OperationOutcome {
    match result {
        Ok(project) => OperationOutcome {
            status: format!("{verb} {}", project.display_name),
            opened: if opens {
                OpenedUpdate::Open(ProjectRow::from(project))
            } else {
                OpenedUpdate::Clear
            },
        },
        Err(error) => OperationOutcome {
            status: error,
            opened: OpenedUpdate::Keep,
        },
    }
}

/// Applies the outcome of an operation that opens a project (import, reopen)
/// or removes the open one. Every mutation also reloads the catalog rather
/// than patching a row: the catalog is the single source of truth, and a
/// clone or removal can reorder Recents.
fn apply_result(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    result: Result<harkness_core::Project, String>,
    verb: &str,
    opens: bool,
) {
    let outcome = operation_outcome(result, verb, opens);
    backend.as_mut().set_status(outcome.status.into());
    match outcome.opened {
        OpenedUpdate::Keep => {}
        OpenedUpdate::Open(row) => backend.as_mut().set_opened(to_map(&row)),
        OpenedUpdate::Clear => backend.as_mut().set_opened(empty_opened()),
    }
    backend.as_mut().refresh();
}

impl ffi::HarknessBackend {
    /// Reloads the whole catalog into [`projects`](Self::projects).
    fn refresh(self: Pin<&mut Self>) {
        let opened_id = opened_project_id(self.as_ref().opened());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let rows = load_rows();
            let _ = qt_thread.queue(move |mut backend| match rows {
                Ok(rows) => {
                    if opened_id == opened_project_id(backend.as_ref().opened())
                        && let Some(row) = opened_id
                            .as_ref()
                            .and_then(|id| rows.iter().find(|row| &row.id == id))
                    {
                        backend.as_mut().set_opened(to_map(row));
                    }
                    backend.as_mut().set_projects(to_projects(&rows));
                }
                Err(error) => {
                    backend.as_mut().set_projects(QList::default());
                    backend.as_mut().set_status(error.into());
                }
            });
        });
    }

    fn validate_remote(&self, remote: &QString) -> QString {
        match harkness_core::normalize_remote(&remote.to_string()) {
            Ok(_) => QString::from(""),
            Err(error) => error.to_string().into(),
        }
    }

    fn import_local(mut self: Pin<&mut Self>, path: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let path = path.to_string().trim().to_owned();
        if path.is_empty() {
            self.as_mut().set_status("Choose a project folder".into());
            return;
        }

        self.as_mut().set_busy(true);
        self.as_mut().set_status("Opening project folder…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = harkness_core::ProjectService::load()
                .and_then(|mut service| service.import_local(&path))
                .map_err(|error| error.to_string());
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Opened", true);
            });
        });
    }

    fn import_repository(mut self: Pin<&mut Self>, remote: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let remote = remote.to_string().trim().to_owned();
        if remote.is_empty() {
            self.as_mut()
                .set_status("Enter a GitHub repository URL".into());
            return;
        }

        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "import", "", "Import repository", true)
        else {
            return;
        };
        self.as_mut().rust_mut().get_mut().legacy_job = Some(job_id.clone());
        self.as_mut().set_busy(true);
        self.as_mut().set_status("Starting Git clone…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = harkness_core::ProjectService::load()
                .and_then(|mut service| {
                    service.import_repository(&remote, &cancellation, move |message| {
                        let update_job_id = progress_job_id.clone();
                        let _ = progress_thread.queue(move |mut backend| {
                            update_backend_job(backend.as_mut(), &update_job_id, message.clone());
                            backend.as_mut().set_status(message.into());
                        });
                    })
                })
                .map_err(|error| error.to_string());
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Imported", true);
            });
        });
    }

    fn cancel_import(mut self: Pin<&mut Self>) {
        let job_id = self.as_ref().rust().legacy_job.clone();
        if let Some(job_id) = job_id {
            if let Some(cancellation) = self.as_ref().rust().cancellations.get(&job_id) {
                cancellation.cancel();
            }
            update_backend_job(self.as_mut(), &job_id, "Cancelling…".to_owned());
            self.as_mut().set_status("Cancelling Git operation…".into());
        }
    }

    fn cancel_job(mut self: Pin<&mut Self>, job_id: &QString) {
        let job_id = job_id.to_string();
        if let Some(cancellation) = self.as_ref().rust().cancellations.get(&job_id) {
            cancellation.cancel();
            update_backend_job(self.as_mut(), &job_id, "Cancelling…".to_owned());
        }
    }

    fn close_project(mut self: Pin<&mut Self>) {
        self.as_mut().set_opened(empty_opened());
        clear_git_state(self.as_mut());
        self.as_mut().set_branches(QList::default());
        self.as_mut().set_worktrees(QList::default());
        clear_diff_state(self.as_mut());
        clear_history_state(self.as_mut());
        clear_review_state(self.as_mut());
    }

    fn refresh_branches(self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_branches(&project_id);
            let _ = qt_thread.queue(move |mut backend| {
                if opened_project_id(backend.as_ref().opened()).as_deref()
                    != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok(rows) => backend.as_mut().set_branches(to_branches(&rows)),
                    Err(error) => {
                        backend.as_mut().set_branches(QList::default());
                        backend.as_mut().set_status(error.into());
                    }
                }
            });
        });
    }

    fn refresh_git(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "status",
            &project_id,
            "Refresh Git status",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(project_id, &cancellation, |_git, _cancellation| {
                Ok("Git status refreshed".to_owned())
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, false, false, true);
            });
        });
    }

    fn refresh_diff(mut self: Pin<&mut Self>, project_id: &QString, path_id: &QString) {
        let project_id = project_id.to_string();
        let path_id = path_id.to_string();
        if path_id.is_empty() {
            clear_diff_state(self.as_mut());
            self.as_mut()
                .set_status("Select a changed path before loading its diff".into());
            return;
        }
        let selected_path =
            match resolve_path_selection(self.as_ref().rust(), &project_id, &path_id) {
                Ok(path) => path,
                Err(error) => {
                    clear_diff_state(self.as_mut());
                    self.as_mut().set_status(error.into());
                    return;
                }
            };
        let display_path = selected_path.display().to_string();
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_diff_request += 1;
            rust.next_diff_request
        };
        set_diff_state(
            self.as_mut(),
            &DiffStateRow::loading(project_id.clone(), path_id.clone(), display_path.clone()),
        );
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id).and_then(|git| {
                load_diff_with_git(&git, project_id.clone(), path_id.clone(), selected_path)
            });
            let _ = qt_thread.queue(move |mut backend| {
                if backend.as_ref().rust().next_diff_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok(row) => set_diff_state(backend.as_mut(), &row),
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_diff_state(
                            backend.as_mut(),
                            &DiffStateRow::with_failure(
                                project_id,
                                path_id,
                                display_path,
                                &failure,
                            ),
                        );
                    }
                }
            });
        });
    }

    fn clear_diff(self: Pin<&mut Self>) {
        clear_diff_state(self);
    }

    fn refresh_history(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "history",
            &project_id,
            "Load commit history",
            true,
        ) else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_history_request += 1;
            rust.next_history_request
        };
        let loading = HistoryStateRow::loading(project_id.clone());
        set_history_state(self.as_mut(), loading.clone());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id)
                .and_then(|git| load_history_page_with_git(&git, None, &cancellation));
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_history_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok((commits, next_cursor)) => {
                        set_history_state(
                            backend.as_mut(),
                            HistoryStateRow {
                                project_id,
                                commits,
                                next_cursor,
                                loading: false,
                                error: String::new(),
                                error_kind: String::new(),
                            },
                        );
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_history_state(backend.as_mut(), loading.with_failure(&failure));
                    }
                }
            });
        });
    }

    fn load_more_history(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some(mut current) = self.as_ref().rust().history_state.clone() else {
            self.as_mut()
                .set_status("Load commit history before requesting another page".into());
            return;
        };
        if current.project_id != project_id {
            self.as_mut()
                .set_status("The visible history belongs to a different project".into());
            return;
        }
        let Some(cursor) = current.next_cursor.clone() else {
            return;
        };
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "history",
            &project_id,
            "Load more history",
            true,
        ) else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_history_request += 1;
            rust.next_history_request
        };
        current.loading = true;
        current.error.clear();
        current.error_kind.clear();
        set_history_state(self.as_mut(), current.clone());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id)
                .and_then(|git| load_history_page_with_git(&git, Some(cursor), &cancellation));
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_history_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok((commits, next_cursor)) => {
                        let known = current
                            .commits
                            .iter()
                            .map(|commit| commit.id.clone())
                            .collect::<std::collections::HashSet<_>>();
                        current.commits.extend(
                            commits
                                .into_iter()
                                .filter(|commit| !known.contains(&commit.id)),
                        );
                        current.next_cursor = next_cursor;
                        current.loading = false;
                        set_history_state(backend.as_mut(), current);
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_history_state(backend.as_mut(), current.with_failure(&failure));
                    }
                }
            });
        });
    }

    fn review_commit(mut self: Pin<&mut Self>, project_id: &QString, revision: &QString) {
        let project_id = project_id.to_string();
        let revision = revision.to_string().trim().to_owned();
        if revision.is_empty() {
            self.as_mut().set_status("Choose a commit to review".into());
            return;
        }
        let short = revision.chars().take(12).collect::<String>();
        launch_review_request(
            self,
            project_id,
            ReviewSelection::Commit {
                revision: revision.clone(),
            },
            format!("Commit {short}"),
            revision,
        );
    }

    fn review_branch(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        branch: &QString,
        base_branch: &QString,
    ) {
        let project_id = project_id.to_string();
        let branch = branch.to_string().trim().to_owned();
        let base_branch = base_branch.to_string().trim().to_owned();
        if branch.is_empty() || base_branch.is_empty() {
            self.as_mut()
                .set_status("Choose both a branch and a base branch to review".into());
            return;
        }
        launch_review_request(
            self,
            project_id,
            ReviewSelection::Branch {
                branch: branch.clone(),
                base_branch: base_branch.clone(),
            },
            format!("{branch} against {base_branch}"),
            "Resolving the merge-base…".to_owned(),
        );
    }

    fn review_working_changes(self: Pin<&mut Self>, project_id: &QString, staged: bool) {
        launch_review_request(
            self,
            project_id.to_string(),
            if staged {
                ReviewSelection::Staged
            } else {
                ReviewSelection::Unstaged
            },
            if staged {
                "Staged changes".to_owned()
            } else {
                "Working-tree changes".to_owned()
            },
            "Loading changed paths…".to_owned(),
        );
    }

    fn load_review_file(mut self: Pin<&mut Self>, project_id: &QString, file_id: &QString) {
        let project_id = project_id.to_string();
        let file_id = file_id.to_string();
        let Some(mut state) = self.as_ref().rust().review_state.clone() else {
            self.as_mut()
                .set_status("Open a review before choosing a file".into());
            return;
        };
        if state.project_id != project_id {
            self.as_mut()
                .set_status("The visible review belongs to a different project".into());
            return;
        }
        let Some(target) = state.target.clone() else {
            self.as_mut()
                .set_status("Wait for the review to finish loading".into());
            return;
        };
        let Some(entry) = state
            .files
            .iter()
            .find(|entry| entry.id == file_id)
            .cloned()
        else {
            self.as_mut()
                .set_status("The selected review file is no longer available".into());
            return;
        };
        let Some((job_id, _cancellation)) = start_job(
            self.as_mut(),
            "review_file",
            &project_id,
            "Load review file",
            false,
        ) else {
            return;
        };
        let (review_request, file_request) = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_review_file_request += 1;
            (rust.next_review_request, rust.next_review_file_request)
        };
        state.selected_file_id.clone_from(&file_id);
        state.loaded_file = None;
        state.file_loading = true;
        state.error.clear();
        state.error_kind.clear();
        set_review_state(self.as_mut(), state);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id)
                .and_then(|git| load_review_file_with_git(&git, &target, &entry, file_request));
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_review_request != review_request
                    || backend.as_ref().rust().next_review_file_request != file_request
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                let Some(mut state) = backend.as_ref().rust().review_state.clone() else {
                    return;
                };
                match result {
                    Ok(file) => {
                        state.loaded_file = Some(file);
                        state.file_loading = false;
                        set_review_state(backend.as_mut(), state);
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_review_state(backend.as_mut(), state.with_failure(&failure));
                    }
                }
            });
        });
    }

    fn expand_review_context(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        hunk_id: &QString,
        direction: &QString,
    ) {
        let project_id = project_id.to_string();
        let hunk_id = hunk_id.to_string();
        let Some(direction) = ReviewContextDirection::parse(direction.to_string().as_str()) else {
            self.as_mut()
                .set_status("Choose context before or after the hunk".into());
            return;
        };
        let Some(state) = self.as_ref().rust().review_state.clone() else {
            self.as_mut().set_status("Open a review file first".into());
            return;
        };
        if state.project_id != project_id {
            self.as_mut()
                .set_status("The visible review belongs to a different project".into());
            return;
        }
        let Some(loaded) = state.loaded_file.clone() else {
            self.as_mut().set_status("Open a review file first".into());
            return;
        };
        if !loaded.hunks.iter().any(|hunk| hunk.id == hunk_id) {
            self.as_mut()
                .set_status("The selected hunk is no longer available".into());
            return;
        }
        let Some((job_id, _cancellation)) = start_job(
            self.as_mut(),
            "review_context",
            &project_id,
            "Expand review context",
            false,
        ) else {
            return;
        };
        let review_request = self.as_ref().rust().next_review_request;
        let file_request = self.as_ref().rust().next_review_file_request;
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id)
                .and_then(|git| expand_review_context_with_git(&git, loaded, &hunk_id, direction));
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_review_request != review_request
                    || backend.as_ref().rust().next_review_file_request != file_request
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                let Some(mut state) = backend.as_ref().rust().review_state.clone() else {
                    return;
                };
                match result {
                    Ok(ReviewContextOutcome::Loaded(file)) => {
                        state.loaded_file = Some(*file);
                        state.error.clear();
                        state.error_kind.clear();
                        set_review_state(backend.as_mut(), state);
                    }
                    Ok(ReviewContextOutcome::Stale) => {
                        let file_id = state.selected_file_id.clone();
                        backend.as_mut().set_status(
                            "The file changed; refreshed the review before expanding context"
                                .into(),
                        );
                        let project = QString::from(project_id.as_str());
                        let file = QString::from(file_id.as_str());
                        backend.as_mut().load_review_file(&project, &file);
                    }
                    Err(failure) => {
                        state.error.clone_from(&failure.message);
                        state.error_kind.clone_from(&failure.kind);
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_review_state(backend.as_mut(), state);
                    }
                }
            });
        });
    }

    fn clear_review(mut self: Pin<&mut Self>) {
        clear_history_state(self.as_mut());
        clear_review_state(self);
    }

    fn stage_path(mut self: Pin<&mut Self>, project_id: &QString, path_id: &QString) {
        let project_id = project_id.to_string();
        let path_id = path_id.to_string();
        let path = match resolve_path_selection(self.as_ref().rust(), &project_id, &path_id) {
            Ok(path) => path,
            Err(error) => {
                self.as_mut().set_status(error.into());
                return;
            }
        };
        let display_path = path.display().to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "stage", &project_id, "Stage path", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                let outcome = git.stage([path], cancellation).map_err(GitFailure::from)?;
                if let Some(failure) =
                    outcome
                        .paths
                        .into_iter()
                        .find_map(|outcome| match outcome.result {
                            harkness_git::StagePathResult::Succeeded => None,
                            harkness_git::StagePathResult::Failed(error) => {
                                Some(GitFailure::from(error))
                            }
                            harkness_git::StagePathResult::NotAttempted => Some(GitFailure {
                                kind: "not_attempted".to_owned(),
                                message: format!("Git did not attempt to stage {display_path}"),
                            }),
                            _ => Some(GitFailure {
                                kind: "unknown_stage_result".to_owned(),
                                message: format!(
                                    "Git returned an unknown staging result for {display_path}"
                                ),
                            }),
                        })
                {
                    return Err(failure);
                }
                Ok(format!("Staged {display_path}"))
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn unstage_path(mut self: Pin<&mut Self>, project_id: &QString, path_id: &QString) {
        let project_id = project_id.to_string();
        let path_id = path_id.to_string();
        let path = match resolve_path_selection(self.as_ref().rust(), &project_id, &path_id) {
            Ok(path) => path,
            Err(error) => {
                self.as_mut().set_status(error.into());
                return;
            }
        };
        let display_path = path.display().to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "unstage", &project_id, "Unstage path", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result =
                run_git_operation(project_id, &cancellation, |git, cancellation| {
                    let outcome = git
                        .unstage([path], cancellation)
                        .map_err(GitFailure::from)?;
                    if let Some(failure) = outcome.paths.into_iter().find_map(|outcome| {
                        match outcome.result {
                            harkness_git::StagePathResult::Succeeded => None,
                            harkness_git::StagePathResult::Failed(error) => {
                                Some(GitFailure::from(error))
                            }
                            harkness_git::StagePathResult::NotAttempted => Some(GitFailure {
                                kind: "not_attempted".to_owned(),
                                message: format!("Git did not attempt to unstage {display_path}"),
                            }),
                            _ => Some(GitFailure {
                                kind: "unknown_stage_result".to_owned(),
                                message: format!(
                                    "Git returned an unknown unstaging result for {display_path}"
                                ),
                            }),
                        }
                    }) {
                        return Err(failure);
                    }
                    Ok(format!("Unstaged {display_path}"))
                });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn stage_hunk(self: Pin<&mut Self>, project_id: &QString, selection_id: &QString) {
        launch_hunk_operation(self, project_id, selection_id, HunkAction::Stage);
    }

    fn unstage_hunk(self: Pin<&mut Self>, project_id: &QString, selection_id: &QString) {
        launch_hunk_operation(self, project_id, selection_id, HunkAction::Unstage);
    }

    fn commit(mut self: Pin<&mut Self>, project_id: &QString, message: &QString, amend: bool) {
        let project_id = project_id.to_string();
        let message = message.to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "commit", &project_id, "Commit", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                let outcome = git
                    .commit(
                        &message,
                        &harkness_git::CommitOptions::default().with_amend(amend),
                        cancellation,
                    )
                    .map_err(GitFailure::from)?;
                let short = outcome.commit_id.chars().take(12).collect::<String>();
                Ok(if outcome.amended {
                    format!("Amended commit {short}")
                } else {
                    format!("Created commit {short}")
                })
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn fetch(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "fetch", &project_id, "Fetch", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                let outcome = git
                    .fetch(
                        &harkness_git::FetchOptions::default(),
                        cancellation,
                        move |message| {
                            let update_job_id = progress_job_id.clone();
                            let _ = progress_thread.queue(move |mut backend| {
                                update_backend_job(backend.as_mut(), &update_job_id, message);
                            });
                        },
                    )
                    .map_err(GitFailure::from)?;
                Ok(if outcome.updated {
                    format!("Fetched updates from {}", outcome.remote)
                } else {
                    format!("{} is already up to date", outcome.remote)
                })
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn pull(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "pull", &project_id, "Pull", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                let outcome = git
                    .pull(
                        &harkness_git::PullOptions::default(),
                        cancellation,
                        move |message| {
                            let update_job_id = progress_job_id.clone();
                            let _ = progress_thread.queue(move |mut backend| {
                                update_backend_job(backend.as_mut(), &update_job_id, message);
                            });
                        },
                    )
                    .map_err(GitFailure::from)?;
                Ok(if outcome.updated {
                    format!("Pulled {} from {}", outcome.branch, outcome.remote)
                } else {
                    format!("{} is already up to date", outcome.branch)
                })
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn push(mut self: Pin<&mut Self>, project_id: &QString, allow_default_branch: bool) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "push", &project_id, "Push", true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                let outcome = git
                    .push(
                        &harkness_git::PushOptions {
                            set_upstream: true,
                            allow_default_branch,
                            ..harkness_git::PushOptions::default()
                        },
                        cancellation,
                        move |message| {
                            let update_job_id = progress_job_id.clone();
                            let _ = progress_thread.queue(move |mut backend| {
                                update_backend_job(backend.as_mut(), &update_job_id, message);
                            });
                        },
                    )
                    .map_err(GitFailure::from)?;
                Ok(if outcome.updated() {
                    format!("Pushed {} to {}", outcome.branch, outcome.remote)
                } else {
                    format!("{} is already published", outcome.branch)
                })
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, false, false);
            });
        });
    }

    fn refresh_worktrees(self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let cancellation = harkness_git::Cancellation::default();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid project identifier".to_owned())?;
                let service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service
                    .worktrees(id, &cancellation)
                    .map(|rows| rows.into_iter().map(WorktreeRow::from).collect::<Vec<_>>())
                    .map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                if opened_project_id(backend.as_ref().opened()).as_deref()
                    != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok(rows) => backend.as_mut().set_worktrees(to_worktrees(&rows)),
                    Err(error) => {
                        backend.as_mut().set_worktrees(QList::default());
                        backend.as_mut().set_status(error.into());
                    }
                }
            });
        });
    }

    fn checkout_branch(mut self: Pin<&mut Self>, project_id: &QString, branch: &QString) {
        let project_id = project_id.to_string();
        let branch = branch.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "checkout",
            &project_id,
            "Switch branch",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                git.checkout_branch(&branch, cancellation)
                    .map_err(GitFailure::from)?;
                Ok(format!("Checked out {branch}"))
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, true, false);
            });
        });
    }

    fn create_branch(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        branch: &QString,
        start_point: &QString,
    ) {
        let project_id = project_id.to_string();
        let branch = branch.to_string().trim().to_owned();
        let start_point = start_point.to_string().trim().to_owned();
        if branch.is_empty() {
            self.as_mut().set_status("Enter a branch name".into());
            return;
        }
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "create_branch",
            &project_id,
            "Create branch",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(project_id, &cancellation, |git, cancellation| {
                git.create_branch(
                    &branch,
                    &harkness_git::CreateBranchOptions {
                        start_point: (!start_point.is_empty()).then_some(start_point),
                        checkout: true,
                    },
                    cancellation,
                )
                .map_err(GitFailure::from)?;
                Ok(format!("Created and checked out {branch}"))
            });
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(backend.as_mut(), &job_id, result, true, true, false);
            });
        });
    }

    fn create_worktree(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        mode: &QString,
        branch: &QString,
        start_point: &QString,
    ) {
        let project_id = project_id.to_string();
        let base = match worktree_base(
            &mode.to_string(),
            &branch.to_string(),
            &start_point.to_string(),
        ) {
            Ok(base) => base,
            Err(error) => {
                self.as_mut().set_status(error.into());
                return;
            }
        };
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "create_worktree",
            &project_id,
            "Create worktree",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid parent project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service
                    .create_worktree(id, &base, &cancellation)
                    .map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                apply_result(backend.as_mut(), result, "Created", true);
            });
        });
    }

    fn reconcile_worktrees(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "reconcile_worktrees",
            &project_id,
            "Reconcile worktrees",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid parent project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                let outcome = service
                    .reconcile_worktrees(id, &cancellation)
                    .map_err(|error| error.to_string())?;
                let rows = service
                    .worktrees(id, &cancellation)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(WorktreeRow::from)
                    .collect::<Vec<_>>();
                Ok::<_, String>((outcome, rows))
            })();
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                match result {
                    Ok((outcome, rows)) => {
                        backend.as_mut().set_worktrees(to_worktrees(&rows));
                        backend.as_mut().set_status(
                            if outcome.removed.is_empty()
                                && outcome.repaired.is_empty()
                                && outcome.skipped.is_empty()
                            {
                                "Worktrees are already reconciled".into()
                            } else {
                                format!(
                                    "Reconciled worktrees: removed {}, repaired {}, skipped {}",
                                    outcome.removed.len(),
                                    outcome.repaired.len(),
                                    outcome.skipped.len()
                                )
                                .into()
                            },
                        );
                        backend.as_mut().refresh();
                    }
                    Err(error) => backend.as_mut().set_status(error.into()),
                }
            });
        });
    }

    fn move_worktree(mut self: Pin<&mut Self>, project_id: &QString, destination: &QString) {
        let project_id = project_id.to_string();
        let destination = destination.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "move_worktree",
            &project_id,
            "Move worktree",
            true,
        ) else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                let moved = move_worktree_with_service(
                    &mut service,
                    &project_id,
                    &destination,
                    &cancellation,
                )?;
                let harkness_core::ProjectSource::Worktree { parent, .. } = &moved.source else {
                    return Err("moved project lost its worktree relationship".to_owned());
                };
                let rows = service
                    .worktrees(*parent, &cancellation)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(WorktreeRow::from)
                    .collect::<Vec<_>>();
                Ok::<_, String>((moved, rows))
            })();
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                match result {
                    Ok((project, rows)) => {
                        backend.as_mut().set_worktrees(to_worktrees(&rows));
                        backend
                            .as_mut()
                            .set_status(format!("Moved {}", project.display_name).into());
                        backend.as_mut().refresh();
                    }
                    Err(error) => backend.as_mut().set_status(error.into()),
                }
            });
        });
    }

    fn lock_worktree(self: Pin<&mut Self>, project_id: &QString, reason: &QString) {
        launch_worktree_lock_operation(
            self,
            project_id,
            WorktreeLockAction::Lock(reason.to_string()),
        );
    }

    fn unlock_worktree(self: Pin<&mut Self>, project_id: &QString) {
        launch_worktree_lock_operation(self, project_id, WorktreeLockAction::Unlock);
    }

    fn open_project(mut self: Pin<&mut Self>, project_id: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let project_id = project_id.to_string();
        self.as_mut().set_busy(true);
        self.as_mut().set_status("Opening project…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service.open(id).map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Opened", true);
            });
        });
    }

    fn remove_project(mut self: Pin<&mut Self>, project_id: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let project_id = project_id.to_string();
        self.as_mut().set_busy(true);
        self.as_mut().set_status("Removing project…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service.remove(id).map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Removed", false);
            });
        });
    }

    /// Deleting a checkout walks the whole working tree, so it runs off the
    /// GUI thread for the same reason the clone does.
    fn remove_managed(mut self: Pin<&mut Self>, project_id: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let project_id = project_id.to_string();
        self.as_mut().set_busy(true);
        self.as_mut()
            .set_status("Removing managed repository…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid managed project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service
                    .remove_managed(id)
                    .map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Removed", false);
            });
        });
    }

    /// Git checks worktree cleanliness and removes its administrative record,
    /// so this runs off the GUI thread just like managed-repository removal.
    fn remove_worktree(mut self: Pin<&mut Self>, project_id: &QString, force: bool) {
        let project_id = project_id.to_string();
        let label = if force {
            "Remove worktree and discard changes"
        } else {
            "Remove worktree"
        };
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "remove_worktree", &project_id, label, true)
        else {
            return;
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                remove_worktree_with_service(&mut service, &project_id, force, &cancellation)
            })();
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                apply_result(backend.as_mut(), result, "Removed", false);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
    };

    use cxx_qt_lib::{QMap, QMapPair_QString_QVariant, QString, QVariant};
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    use super::{
        BranchRow, GitStateRow, HarknessBackendRust, HunkAction, HunkMutationOutcome,
        MAX_GUI_DIFF_LINES_PER_FILE, OpenedUpdate, ProjectRow, ReviewContextDirection,
        ReviewContextOutcome, ReviewSelection, WorktreeLockAction, begin_job,
        change_worktree_lock_with_service, empty_opened, end_job, expand_review_context_with_git,
        file_content_summary, hidden_before, load_diff_with_git, load_history_page_with_git,
        load_review_file_with_git, load_review_with_git, load_review_with_initial_file_with_git,
        move_worktree_with_service, mutate_hunk_with_git, operation_outcome, project_rows,
        register_path_selection, remove_worktree_with_service, resolve_path_selection, review_rows,
        to_branches, to_diff, to_git, to_jobs, to_map, to_projects, update_job, worktree_base,
    };

    fn project(
        source: harkness_core::ProjectSource,
        git: Option<harkness_git::GitStatus>,
    ) -> harkness_core::Project {
        harkness_core::Project {
            id: harkness_core::ProjectId::new(),
            display_name: "sample".to_owned(),
            root: "/tmp/sample".into(),
            source,
            last_opened: time::OffsetDateTime::now_utc(),
            available: true,
            git,
        }
    }

    fn initialize_repository(root: &Path) {
        fs::create_dir_all(root).unwrap();
        let repository = Repository::init(root).unwrap();
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
    }

    fn commit_file(root: &Path, path: &Path, contents: &str, message: &str) {
        let full_path = root.join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full_path, contents).unwrap();
        let repository = Repository::open(root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )
            .unwrap();
    }

    fn commit_index(root: &Path, message: &str) -> git2::Oid {
        let repository = Repository::open(root).unwrap();
        let mut index = repository.index().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )
            .unwrap()
    }

    fn committed_file(root: &Path, commit_id: git2::Oid, path: &Path) -> String {
        let repository = Repository::open(root).unwrap();
        let commit = repository.find_commit(commit_id).unwrap();
        let tree = commit.tree().unwrap();
        let entry = tree.get_path(path).unwrap();
        let blob = repository.find_blob(entry.id()).unwrap();
        String::from_utf8(blob.content().to_vec()).unwrap()
    }

    fn row_map(row: &ProjectRow) -> QMap<QMapPair_QString_QVariant> {
        to_map(row)
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("row should flatten to a QVariantMap")
    }

    #[test]
    fn managed_repository_row_carries_git_identity() {
        let row = ProjectRow::from(project(
            harkness_core::ProjectSource::ManagedRepository {
                remote: "github.com/example/sample".to_owned(),
            },
            Some(harkness_git::GitStatus {
                branch: Some("main".to_owned()),
                dirty: true,
                upstream: Some(harkness_git::UpstreamStatus {
                    name: "origin/main".to_owned(),
                    ahead: 1,
                    behind: 2,
                }),
                staged: 1,
                unstaged: 0,
            }),
        ));

        assert!(row.managed);
        assert_eq!(row.branch, "main");
        assert!(row.is_git && row.dirty);

        let map = row_map(&row);
        for key in [
            "id",
            "displayName",
            "root",
            "remote",
            "branch",
            "managed",
            "worktree",
            "parentId",
            "parentName",
            "createdBranch",
            "available",
            "isGit",
            "dirty",
        ] {
            assert!(map.contains(&QString::from(key)), "missing key '{key}'");
        }
        assert_eq!(
            map.get(&QString::from("branch"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("main".to_owned())
        );
        assert_eq!(
            map.get(&QString::from("managed"))
                .and_then(|value| value.value::<bool>()),
            Some(true)
        );
    }

    #[test]
    fn job_records_flatten_with_the_qml_contract() {
        let mut records = Vec::new();
        let mut next_id = 0;
        let job = begin_job(
            &mut records,
            &mut next_id,
            "fetch",
            "project-1",
            "Fetch",
            true,
        )
        .unwrap();
        let jobs = to_jobs(&records);
        let map = jobs
            .get(0)
            .unwrap()
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("job should flatten to a QVariantMap");

        for key in [
            "id",
            "kind",
            "projectId",
            "label",
            "progress",
            "cancellable",
        ] {
            assert!(map.contains(&QString::from(key)), "missing key '{key}'");
        }
        assert_eq!(
            map.get(&QString::from("id"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some(job.id)
        );
    }

    #[test]
    fn jobs_begin_update_and_end_by_kind_and_project() {
        let mut records = Vec::new();
        let mut next_id = 0;
        let mut fetch = begin_job(
            &mut records,
            &mut next_id,
            "fetch",
            "project-1",
            "Fetch",
            true,
        )
        .unwrap();
        assert!(
            begin_job(
                &mut records,
                &mut next_id,
                "fetch",
                "project-1",
                "Fetch",
                true,
            )
            .is_none()
        );
        assert!(
            begin_job(
                &mut records,
                &mut next_id,
                "pull",
                "project-1",
                "Pull",
                true,
            )
            .is_some()
        );
        assert!(
            begin_job(
                &mut records,
                &mut next_id,
                "fetch",
                "project-2",
                "Fetch",
                true,
            )
            .is_some()
        );

        assert!(update_job(
            &mut records,
            &fetch.id,
            "Receiving objects".to_owned()
        ));
        assert_eq!(
            records
                .iter()
                .find(|job| job.id == fetch.id)
                .map(|job| job.progress.as_str()),
            Some("Receiving objects")
        );
        fetch.progress = "Receiving objects".to_owned();
        assert_eq!(end_job(&mut records, &fetch.id), Some(fetch));
        assert_eq!(records.len(), 2);

        let first = harkness_git::Cancellation::default();
        let second = harkness_git::Cancellation::default();
        let mut cancellations = HashMap::from([
            ("job-1".to_owned(), first.clone()),
            ("job-2".to_owned(), second.clone()),
        ]);
        cancellations.get("job-1").unwrap().cancel();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        cancellations.remove("job-1");
        assert!(cancellations.contains_key("job-2"));
    }

    #[test]
    fn detailed_status_entries_flatten_for_the_git_panel() {
        let state = GitStateRow::from_status(
            "project-1".to_owned(),
            harkness_git::DetailedStatus {
                head: harkness_git::HeadState::Branch {
                    name: "topic".to_owned(),
                },
                upstream: Some(harkness_git::UpstreamStatus {
                    name: "origin/topic".to_owned(),
                    ahead: 2,
                    behind: 1,
                }),
                pending: Some(harkness_git::PendingOperation::Merge),
                entries: vec![harkness_git::StatusEntry {
                    path: "src/new.rs".into(),
                    staged: Some(harkness_git::FileChange::Added),
                    unstaged: Some(harkness_git::FileChange::Modified),
                    rename_source: Some("src/old.rs".into()),
                    conflicted: true,
                }],
            },
        );
        let map = to_git(&state, &["path-test".to_owned()])
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("Git state should flatten to a QVariantMap");
        assert_eq!(
            map.get(&QString::from("upstream"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("origin/topic".to_owned())
        );
        assert_eq!(
            map.get(&QString::from("pending"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("merge".to_owned())
        );
        let entries = map
            .get(&QString::from("entries"))
            .and_then(|value| value.value::<cxx_qt_lib::QList<QVariant>>())
            .expect("entries should be a QVariantList");
        let entry = entries
            .get(0)
            .unwrap()
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("status entry should be a QVariantMap");
        for key in [
            "pathId",
            "path",
            "staged",
            "unstaged",
            "renameSource",
            "conflicted",
        ] {
            assert!(entry.contains(&QString::from(key)), "missing key '{key}'");
        }
        assert_eq!(
            entry
                .get(&QString::from("path"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("src/new.rs".to_owned())
        );
        assert_eq!(
            entry
                .get(&QString::from("pathId"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("path-test".to_owned())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn backend_path_tokens_preserve_non_utf8_paths_exactly() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let first = PathBuf::from(OsStr::from_bytes(b"non-utf8-\xff.txt"));
        let second = PathBuf::from(OsStr::from_bytes(b"non-utf8-\xfe.txt"));
        let mut backend = HarknessBackendRust::default();
        let first_id = register_path_selection(&mut backend, "project-1", &first);
        let repeated_id = register_path_selection(&mut backend, "project-1", &first);
        let second_id = register_path_selection(&mut backend, "project-1", &second);

        assert_eq!(first_id, repeated_id);
        assert_ne!(first_id, second_id);
        assert_eq!(
            resolve_path_selection(&backend, "project-1", &first_id).unwrap(),
            first
        );
        assert!(resolve_path_selection(&backend, "project-2", &first_id).is_err());

        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("byte-path-repository");
        initialize_repository(&root);
        fs::write(root.join(&first), b"byte-exact content\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let state =
            load_diff_with_git(&git, "project-1".to_owned(), first_id, first.clone()).unwrap();
        assert_eq!(state.path_id, "path-1");
        assert!(state.files.iter().any(|file| {
            file.old_path.as_deref() == Some(first.as_path())
                || file.new_path.as_deref() == Some(first.as_path())
        }));
    }

    #[test]
    fn backend_diff_model_stages_only_the_selected_hunk() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("diff-repository");
        initialize_repository(&root);
        let path = Path::new("story.txt");
        let original = (1..=24)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        commit_file(&root, path, &original, "add story");
        let modified = original
            .replace("line 2\n", "line 2 changed\n")
            .replace("line 22\n", "line 22 changed\n");
        fs::write(root.join(path), &modified).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));

        let state = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-story".to_owned(),
            PathBuf::from("story.txt"),
        )
        .unwrap();
        let unstaged = state
            .files
            .iter()
            .find(|file| matches!(file.target, harkness_git::DiffTarget::Unstaged))
            .unwrap();
        assert_eq!(unstaged.hunks.len(), 2);
        assert!(
            unstaged
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| matches!(line.kind, harkness_git::DiffLineKind::Addition))
        );
        assert!(
            unstaged
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
        );

        let (model, selections) = to_diff(&state, 7);
        assert_eq!(selections.len(), 2);
        let map = model
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("diff state should flatten to a QVariantMap");
        let files = map
            .get(&QString::from("files"))
            .and_then(|value| value.value::<cxx_qt_lib::QList<QVariant>>())
            .expect("diff files should flatten to a QVariantList");
        assert_eq!(files.len(), 1);

        let selection = harkness_git::HunkSelection::new(unstaged, &unstaged.hunks[0]);
        assert_eq!(
            mutate_hunk_with_git(
                &git,
                HunkAction::Stage,
                &selection,
                &harkness_git::Cancellation::default(),
            )
            .unwrap(),
            HunkMutationOutcome::Applied(1)
        );

        let refreshed = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-story".to_owned(),
            PathBuf::from("story.txt"),
        )
        .unwrap();
        let staged = refreshed
            .files
            .iter()
            .find(|file| matches!(file.target, harkness_git::DiffTarget::Staged))
            .unwrap();
        let unstaged = refreshed
            .files
            .iter()
            .find(|file| matches!(file.target, harkness_git::DiffTarget::Unstaged))
            .unwrap();
        assert_eq!(staged.hunks.len(), 1);
        assert_eq!(unstaged.hunks.len(), 1);

        let staged_selection = harkness_git::HunkSelection::new(staged, &staged.hunks[0]);
        assert_eq!(
            mutate_hunk_with_git(
                &git,
                HunkAction::Unstage,
                &staged_selection,
                &harkness_git::Cancellation::default(),
            )
            .unwrap(),
            HunkMutationOutcome::Applied(1)
        );
        let unstaged_again = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-story".to_owned(),
            PathBuf::from("story.txt"),
        )
        .unwrap();
        assert!(
            unstaged_again
                .files
                .iter()
                .all(|file| !matches!(file.target, harkness_git::DiffTarget::Staged))
        );
        let file = unstaged_again
            .files
            .iter()
            .find(|file| matches!(file.target, harkness_git::DiffTarget::Unstaged))
            .unwrap();
        let selection = harkness_git::HunkSelection::new(file, &file.hunks[0]);
        assert_eq!(
            mutate_hunk_with_git(
                &git,
                HunkAction::Stage,
                &selection,
                &harkness_git::Cancellation::default(),
            )
            .unwrap(),
            HunkMutationOutcome::Applied(1)
        );

        let commit_id = commit_index(&root, "stage one hunk");
        let committed = committed_file(&root, commit_id, path);
        assert!(committed.contains("line 2 changed\n"));
        assert!(committed.contains("line 22\n"));
        assert!(!committed.contains("line 22 changed\n"));
    }

    #[test]
    fn backend_stale_hunk_refusal_leaves_the_index_unchanged() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("stale-repository");
        initialize_repository(&root);
        let path = Path::new("stale.txt");
        commit_file(&root, path, "one\ntwo\nthree\n", "add stale fixture");
        fs::write(root.join(path), "one\ntwo changed\nthree\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let state = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-stale".to_owned(),
            PathBuf::from("stale.txt"),
        )
        .unwrap();
        let file = state
            .files
            .iter()
            .find(|file| matches!(file.target, harkness_git::DiffTarget::Unstaged))
            .unwrap();
        let selection = harkness_git::HunkSelection::new(file, &file.hunks[0]);

        fs::write(root.join(path), "one\ntwo changed again\nthree\n").unwrap();
        assert_eq!(
            mutate_hunk_with_git(
                &git,
                HunkAction::Stage,
                &selection,
                &harkness_git::Cancellation::default(),
            )
            .unwrap(),
            HunkMutationOutcome::Stale
        );
        assert!(
            git.diff(
                harkness_git::DiffTarget::Staged,
                &harkness_git::DiffOptions::default(),
            )
            .unwrap()
            .is_empty()
        );
        let refreshed = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-stale".to_owned(),
            PathBuf::from("stale.txt"),
        )
        .unwrap();
        assert_eq!(refreshed.files.len(), 1);
        assert_eq!(refreshed.files[0].hunks.len(), 1);
    }

    #[test]
    fn backend_diff_names_binary_and_oversize_omissions() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("summary-repository");
        initialize_repository(&root);
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));

        fs::write(root.join("binary.dat"), [0_u8, 1, 2, 0, 3]).unwrap();
        let binary = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-binary".to_owned(),
            PathBuf::from("binary.dat"),
        )
        .unwrap();
        assert_eq!(binary.files.len(), 1);
        assert!(binary.files[0].binary);
        assert!(file_content_summary(&binary.files[0]).starts_with("Binary file"));

        fs::write(
            root.join("large.txt"),
            vec![b'x'; usize::try_from(harkness_git::DEFAULT_MAX_DIFF_FILE_SIZE).unwrap() + 1],
        )
        .unwrap();
        let large = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-large".to_owned(),
            PathBuf::from("large.txt"),
        )
        .unwrap();
        assert_eq!(large.files.len(), 1);
        assert!(matches!(
            large.files[0].omission.as_ref(),
            Some(harkness_git::DiffOmission::FileTooLarge { .. })
        ));
        assert!(file_content_summary(&large.files[0]).starts_with("File too large"));
    }

    #[test]
    fn backend_diff_caps_eager_qml_line_delegates_with_a_named_summary() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("line-limit-repository");
        initialize_repository(&root);
        let content = (0..=MAX_GUI_DIFF_LINES_PER_FILE)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("many-lines.txt"), content).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let state = load_diff_with_git(
            &git,
            "project-1".to_owned(),
            "path-many-lines".to_owned(),
            PathBuf::from("many-lines.txt"),
        )
        .unwrap();
        assert_eq!(state.files.len(), 1);
        assert!(file_content_summary(&state.files[0]).contains("GUI display limit"));

        let (model, selections) = to_diff(&state, 9);
        assert!(selections.is_empty());
        let map = model
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("diff state should flatten to a QVariantMap");
        let files = map
            .get(&QString::from("files"))
            .and_then(|value| value.value::<cxx_qt_lib::QList<QVariant>>())
            .expect("diff files should flatten to a QVariantList");
        let file = files
            .get(0)
            .unwrap()
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("diff file should flatten to a QVariantMap");
        let hunks = file
            .get(&QString::from("hunks"))
            .and_then(|value| value.value::<cxx_qt_lib::QList<QVariant>>())
            .expect("diff hunks should flatten to a QVariantList");
        assert!(hunks.is_empty());
    }

    #[test]
    fn review_history_uses_exactly_one_git_cursor_page_at_a_time() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("history-repository");
        initialize_repository(&root);
        for revision in 1..=52 {
            commit_file(
                &root,
                Path::new("history.txt"),
                format!("revision {revision}\n").as_str(),
                format!("revision {revision}").as_str(),
            );
        }
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let cancellation = harkness_git::Cancellation::default();

        let (first, cursor) = load_history_page_with_git(&git, None, &cancellation).unwrap();
        assert_eq!(first.len(), 50);
        assert_eq!(first[0].summary, "revision 52");
        let cursor = cursor.expect("53 commits require a continuation");

        // Advancing HEAD cannot move the cursor-anchored continuation.
        commit_file(
            &root,
            Path::new("history.txt"),
            "revision after cursor\n",
            "revision after cursor",
        );
        let (second, cursor) =
            load_history_page_with_git(&git, Some(cursor), &cancellation).unwrap();
        assert_eq!(second.len(), 3);
        assert!(cursor.is_none());
        assert!(
            second
                .iter()
                .all(|commit| commit.summary != "revision after cursor")
        );
        let mut ids = first
            .iter()
            .map(|commit| commit.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(second.iter().all(|commit| ids.insert(&commit.id)));
    }

    #[test]
    fn branch_review_pins_the_merge_base_and_excludes_base_only_changes() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("branch-review-repository");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let common = repository.head().unwrap().peel_to_commit().unwrap();
        let base_branch = repository.head().unwrap().shorthand().unwrap().to_owned();
        repository.branch("topic", &common, false).unwrap();
        drop(common);
        drop(repository);

        commit_file(
            &root,
            Path::new("base-only.txt"),
            "base moved\n",
            "advance base",
        );
        let repository = Repository::open(&root).unwrap();
        repository.set_head("refs/heads/topic").unwrap();
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        drop(repository);
        commit_file(
            &root,
            Path::new("topic-only.txt"),
            "topic change\n",
            "change topic",
        );

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Branch {
                branch: "topic".to_owned(),
                base_branch,
            },
            4,
        )
        .unwrap();

        assert!(review.detail.contains("merge-base"));
        assert_eq!(review.files.len(), 1);
        assert_eq!(review.files[0].path, Path::new("topic-only.txt"));
        assert!(review.files[0].file.hunks.is_empty());
        assert!(matches!(
            review.files[0].file.omission.as_ref(),
            Some(harkness_git::DiffOmission::ContentBudgetExhausted { limit: 0 })
        ));
    }

    #[test]
    fn review_file_loads_git_intra_line_ranges_only_on_demand() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("word-review-repository");
        initialize_repository(&root);
        let path = Path::new("src/word.rs");
        commit_file(
            &root,
            path,
            "fn example() { let value = old; }\n",
            "add old word",
        );
        commit_file(
            &root,
            path,
            "fn example() { let value = new; }\n",
            "replace one word",
        );
        let repository = Repository::open(&root).unwrap();
        let revision = repository.head().unwrap().target().unwrap().to_string();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            5,
        )
        .unwrap();
        assert_eq!(review.files.len(), 1);
        assert!(review.loaded_file.is_none());
        assert!(review.files[0].file.hunks.is_empty());

        let target = review.target.as_ref().unwrap();
        let loaded = load_review_file_with_git(&git, target, &review.files[0], 6).unwrap();
        let deletion = loaded.file.hunks[0]
            .lines
            .iter()
            .find(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
            .unwrap();
        let addition = loaded.file.hunks[0]
            .lines
            .iter()
            .find(|line| matches!(line.kind, harkness_git::DiffLineKind::Addition))
            .unwrap();
        let changed = |line: &harkness_git::DiffLine| {
            line.intra_line_ranges
                .as_ref()
                .unwrap()
                .iter()
                .map(|range| {
                    String::from_utf8_lossy(&line.content[range.start..range.end]).into_owned()
                })
                .collect::<String>()
        };
        assert_eq!(changed(deletion), "old");
        assert_eq!(changed(addition), "new");
    }

    #[test]
    fn opening_a_review_selects_and_loads_only_the_first_changed_file() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("default-review-file-repository");
        initialize_repository(&root);
        let first_path = Path::new("first.txt");
        let second_path = Path::new("second.txt");
        fs::write(root.join(first_path), "first old\n").unwrap();
        fs::write(root.join(second_path), "second old\n").unwrap();
        let repository = Repository::open(&root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(first_path).unwrap();
        index.add_path(second_path).unwrap();
        index.write().unwrap();
        drop(index);
        drop(repository);
        commit_index(&root, "add review files");

        fs::write(root.join(first_path), "first new\n").unwrap();
        fs::write(root.join(second_path), "second new\n").unwrap();
        let repository = Repository::open(&root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(first_path).unwrap();
        index.add_path(second_path).unwrap();
        index.write().unwrap();
        drop(index);
        drop(repository);
        let revision = commit_index(&root, "change both review files").to_string();

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            7,
            8,
        )
        .unwrap();

        assert_eq!(review.files.len(), 2);
        assert_eq!(review.selected_file_id, review.files[0].id);
        let loaded = review.loaded_file.as_ref().unwrap();
        assert_eq!(loaded.id, review.files[0].id);
        assert_eq!(loaded.file.new_path, review.files[0].file.new_path);
        assert!(!loaded.file.hunks.is_empty());
        assert!(review.files.iter().all(|entry| entry.file.hunks.is_empty()));
    }

    #[test]
    fn thousand_file_review_keeps_only_identity_rows_until_a_file_opens() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("large-review-repository");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let mut index = repository.index().unwrap();
        for number in 0..1_000 {
            let path = PathBuf::from(format!("files/file-{number:04}.txt"));
            let full_path = root.join(&path);
            fs::create_dir_all(full_path.parent().unwrap()).unwrap();
            fs::write(&full_path, format!("file {number}\n")).unwrap();
            index.add_path(&path).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        let revision = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "add one thousand files",
                &tree,
                &[&parent],
            )
            .unwrap()
            .to_string();
        drop(tree);
        drop(parent);
        drop(repository);

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            7,
        )
        .unwrap();
        assert_eq!(review.files.len(), 1_000);
        assert!(review.loaded_file.is_none());
        assert!(review.files.iter().all(|entry| entry.file.hunks.is_empty()));
    }

    #[test]
    fn context_expansion_keeps_the_original_review_hunk_identity() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("context-review-repository");
        initialize_repository(&root);
        let path = Path::new("context.txt");
        let original = (1..=100)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        commit_file(&root, path, &original, "add context fixture");
        let changed = original.replace("line 50\n", "line fifty\n");
        commit_file(&root, path, &changed, "change middle line");
        let repository = Repository::open(&root).unwrap();
        let revision = repository.head().unwrap().target().unwrap().to_string();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            8,
        )
        .unwrap();
        let loaded =
            load_review_file_with_git(&git, review.target.as_ref().unwrap(), &review.files[0], 9)
                .unwrap();
        assert_eq!(loaded.hunks.len(), 1);
        assert!(hidden_before(&loaded, 0) > 20);
        let hunk_id = loaded.hunks[0].id.clone();

        let ReviewContextOutcome::Loaded(expanded) =
            expand_review_context_with_git(&git, loaded, &hunk_id, ReviewContextDirection::Before)
                .unwrap()
        else {
            panic!("immutable commit context cannot become stale");
        };
        assert_eq!(expanded.hunks[0].id, hunk_id);
        assert_eq!(expanded.hunks[0].before.len(), 20);
        assert!(
            review_rows(&expanded).len()
                > isize::try_from(expanded.file.hunks[0].lines.len()).unwrap()
        );
    }

    #[test]
    fn working_change_context_is_blob_stable_or_refreshably_stale() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("working-context-repository");
        initialize_repository(&root);
        let path = Path::new("working.txt");
        let original = (1..=80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        commit_file(&root, path, &original, "add working fixture");
        let staged_content = original.replace("line 40\n", "line forty staged\n");
        fs::write(root.join(path), &staged_content).unwrap();
        let repository = Repository::open(&root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        drop(index);
        drop(repository);

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let staged =
            load_review_with_git(&git, "project-1".to_owned(), ReviewSelection::Staged, 10)
                .unwrap();
        let staged_file =
            load_review_file_with_git(&git, staged.target.as_ref().unwrap(), &staged.files[0], 11)
                .unwrap();
        let staged_hunk = staged_file.hunks[0].id.clone();

        let unstaged_content = staged_content.replace("line 60\n", "line sixty unstaged\n");
        fs::write(root.join(path), &unstaged_content).unwrap();
        assert!(matches!(
            expand_review_context_with_git(
                &git,
                staged_file,
                &staged_hunk,
                ReviewContextDirection::Before,
            )
            .unwrap(),
            ReviewContextOutcome::Loaded(_)
        ));

        let unstaged =
            load_review_with_git(&git, "project-1".to_owned(), ReviewSelection::Unstaged, 12)
                .unwrap();
        let unstaged_file = load_review_file_with_git(
            &git,
            unstaged.target.as_ref().unwrap(),
            &unstaged.files[0],
            13,
        )
        .unwrap();
        let unstaged_hunk = unstaged_file.hunks[0].id.clone();
        fs::write(
            root.join(path),
            unstaged_content.replace("line 70\n", "line seventy newer\n"),
        )
        .unwrap();
        assert!(matches!(
            expand_review_context_with_git(
                &git,
                unstaged_file,
                &unstaged_hunk,
                ReviewContextDirection::Before,
            )
            .unwrap(),
            ReviewContextOutcome::Stale
        ));
    }

    #[test]
    fn local_and_detached_rows_distinguish_identity_states() {
        let detached = ProjectRow::from(project(
            harkness_core::ProjectSource::Local,
            Some(harkness_git::GitStatus {
                branch: None,
                dirty: false,
                upstream: None,
                staged: 0,
                unstaged: 0,
            }),
        ));
        let plain = ProjectRow::from(project(harkness_core::ProjectSource::Local, None));

        assert!(detached.is_git && detached.branch.is_empty() && !detached.dirty);
        assert!(!plain.is_git && !plain.managed && plain.remote.is_empty());
    }

    #[test]
    fn projects_list_and_empty_opened_are_maps() {
        let rows = [ProjectRow::from(project(
            harkness_core::ProjectSource::Local,
            None,
        ))];
        let projects = to_projects(&rows);

        assert_eq!(projects.len(), 1);
        let empty = empty_opened()
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("empty opened should still be a QVariantMap");
        assert!(empty.is_empty());

        let filled = to_map(&rows[0])
            .value::<QMap<QMapPair_QString_QVariant>>()
            .unwrap();
        assert!(!filled.is_empty());
        assert_eq!(QVariant::default().value::<bool>(), None);
    }

    #[test]
    fn branch_rows_disable_a_branch_checked_out_elsewhere() {
        let row = BranchRow::from(harkness_git::Branch {
            name: "topic".to_owned(),
            kind: harkness_git::BranchKind::Local,
            tip: "0000000000000000000000000000000000000000".parse().unwrap(),
            upstream: None,
            checkout: harkness_git::BranchCheckout::OtherWorktree("/tmp/other".into()),
        });
        assert!(!row.current && !row.selectable);
        assert!(row.detail.contains("/tmp/other"));

        let rows = to_branches(&[row]);
        let map = rows
            .get(0)
            .unwrap()
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("branch row should flatten to a QVariantMap");
        assert_eq!(
            map.get(&QString::from("selectable"))
                .and_then(|value| value.value::<bool>()),
            Some(false)
        );
    }

    #[test]
    fn worktree_row_resolves_parent_name_and_keeps_creation_branch_separate() {
        let parent = harkness_core::ProjectId::new();
        let mut parent_project = project(harkness_core::ProjectSource::Local, None);
        parent_project.id = parent;
        parent_project.display_name = "parent project".to_owned();
        let worktree = project(
            harkness_core::ProjectSource::Worktree {
                parent,
                worktree_branch: Some("agent/catalog-v2".to_owned()),
            },
            Some(harkness_git::GitStatus {
                branch: Some("agent/live".to_owned()),
                dirty: false,
                upstream: None,
                staged: 0,
                unstaged: 0,
            }),
        );
        let row = project_rows(vec![parent_project, worktree])
            .into_iter()
            .find(|row| row.worktree)
            .unwrap();

        assert!(row.worktree);
        assert!(!row.managed);
        assert_eq!(row.parent_id, parent.to_string());
        assert_eq!(row.parent_name, "parent project");
        assert_eq!(row.created_branch, "agent/catalog-v2");
        assert_eq!(row.branch, "agent/live");
        let map = row_map(&row);
        assert_eq!(
            map.get(&QString::from("worktree"))
                .and_then(|value| value.value::<bool>()),
            Some(true)
        );
        assert_eq!(
            map.get(&QString::from("parentName"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("parent project".to_owned())
        );
        assert_eq!(
            map.get(&QString::from("createdBranch"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("agent/catalog-v2".to_owned())
        );
    }

    #[test]
    fn worktree_creation_modes_are_validated_before_spawning_git() {
        assert_eq!(
            worktree_base("new", "agent/topic", "HEAD").unwrap(),
            harkness_git::WorktreeBase::NewBranch {
                name: "agent/topic".to_owned(),
                start_point: Some("HEAD".to_owned()),
            }
        );
        assert_eq!(
            worktree_base("existing", "agent/topic", "ignored").unwrap(),
            harkness_git::WorktreeBase::ExistingBranch {
                name: "agent/topic".to_owned(),
            }
        );
        assert_eq!(
            worktree_base("detached", "ignored", "HEAD~1").unwrap(),
            harkness_git::WorktreeBase::Detached {
                commit: "HEAD~1".to_owned(),
            }
        );
        assert!(worktree_base("new", "", "HEAD").is_err());
        assert!(worktree_base("detached", "", "").is_err());
    }

    #[test]
    fn backend_worktree_removal_passes_the_explicit_force_choice() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("parent");
        initialize_repository(&parent_root);
        let mut service =
            harkness_core::ProjectService::load_from_data_dir(fixture.path().join("data")).unwrap();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = service
            .create_worktree(
                parent.id,
                &harkness_git::WorktreeBase::NewBranch {
                    name: "agent/gui-force".to_owned(),
                    start_point: None,
                },
                &harkness_git::Cancellation::default(),
            )
            .unwrap();
        fs::write(worktree.root.join("dirty.txt"), "discard me\n").unwrap();

        let refused = remove_worktree_with_service(
            &mut service,
            &worktree.id.to_string(),
            false,
            &harkness_git::Cancellation::default(),
        )
        .unwrap_err();
        assert!(refused.contains("uncommitted changes"));
        remove_worktree_with_service(
            &mut service,
            &worktree.id.to_string(),
            true,
            &harkness_git::Cancellation::default(),
        )
        .unwrap();
        assert!(!worktree.root.exists());
    }

    #[test]
    fn backend_worktree_lock_requires_and_surfaces_the_git_reason() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("lock-parent");
        initialize_repository(&parent_root);
        let mut service =
            harkness_core::ProjectService::load_from_data_dir(fixture.path().join("lock-data"))
                .unwrap();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = service
            .create_worktree(
                parent.id,
                &harkness_git::WorktreeBase::NewBranch {
                    name: "agent/gui-lock".to_owned(),
                    start_point: None,
                },
                &harkness_git::Cancellation::default(),
            )
            .unwrap();
        let worktree_id = worktree.id.to_string();
        let parent_id = parent.id.to_string();

        let wrong_parent = change_worktree_lock_with_service(
            &mut service,
            &worktree_id,
            &harkness_core::ProjectId::new().to_string(),
            &WorktreeLockAction::Lock("must not be applied".to_owned()),
            &harkness_git::Cancellation::default(),
        )
        .unwrap_err();
        assert!(wrong_parent.contains("does not belong to the open parent project"));

        let blank = change_worktree_lock_with_service(
            &mut service,
            &worktree_id,
            &parent_id,
            &WorktreeLockAction::Lock("   ".to_owned()),
            &harkness_git::Cancellation::default(),
        )
        .unwrap();
        assert!(
            blank
                .message
                .unwrap_err()
                .contains("reason cannot be empty")
        );
        assert_eq!(blank.rows.unwrap().len(), 1);

        let outcome = change_worktree_lock_with_service(
            &mut service,
            &worktree_id,
            &parent_id,
            &WorktreeLockAction::Lock("  agent is still working  ".to_owned()),
            &harkness_git::Cancellation::default(),
        )
        .unwrap();
        let message = outcome.message.unwrap();
        let rows = outcome.rows.unwrap();
        assert_eq!(message, "Locked agent/gui-lock: agent is still working");
        let row = rows.iter().find(|row| row.id == worktree_id).unwrap();
        assert!(row.locked);
        assert_eq!(row.lock_reason, "agent is still working");

        let removal = remove_worktree_with_service(
            &mut service,
            &worktree_id,
            true,
            &harkness_git::Cancellation::default(),
        )
        .unwrap_err();
        assert!(removal.contains("agent is still working"));

        let outcome = change_worktree_lock_with_service(
            &mut service,
            &worktree_id,
            &parent_id,
            &WorktreeLockAction::Unlock,
            &harkness_git::Cancellation::default(),
        )
        .unwrap();
        let message = outcome.message.unwrap();
        let rows = outcome.rows.unwrap();
        assert_eq!(message, "Unlocked agent/gui-lock");
        let row = rows.iter().find(|row| row.id == worktree_id).unwrap();
        assert!(!row.locked);
        assert!(row.lock_reason.is_empty());
    }

    #[test]
    fn backend_worktree_move_validates_and_relocates_the_checkout() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("move-parent");
        initialize_repository(&parent_root);
        let destination_parent = fixture.path().join("move-destination-parent");
        fs::create_dir(&destination_parent).unwrap();
        let destination = destination_parent.join("checkout");
        let mut service =
            harkness_core::ProjectService::load_from_data_dir(fixture.path().join("move-data"))
                .unwrap();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = service
            .create_worktree(
                parent.id,
                &harkness_git::WorktreeBase::NewBranch {
                    name: "agent/gui-move".to_owned(),
                    start_point: None,
                },
                &harkness_git::Cancellation::default(),
            )
            .unwrap();

        let relative = move_worktree_with_service(
            &mut service,
            &worktree.id.to_string(),
            "relative-checkout",
            &harkness_git::Cancellation::default(),
        )
        .unwrap_err();
        assert!(relative.contains("must be absolute"));

        let moved = move_worktree_with_service(
            &mut service,
            &worktree.id.to_string(),
            destination.to_str().unwrap(),
            &harkness_git::Cancellation::default(),
        )
        .unwrap();
        assert_eq!(moved.root, destination.canonicalize().unwrap());
        assert!(!worktree.root.exists());
        let rows = service
            .worktrees(parent.id, &harkness_git::Cancellation::default())
            .unwrap();
        assert!(rows.iter().any(|row| row.root == moved.root));
        assert!(rows.iter().all(|row| row.root != worktree.root));
    }

    #[test]
    fn successful_open_and_removal_have_opposite_navigation_transitions() {
        let opened = operation_outcome(
            Ok(project(harkness_core::ProjectSource::Local, None)),
            "Opened",
            true,
        );
        assert_eq!(opened.status, "Opened sample");
        assert!(matches!(opened.opened, OpenedUpdate::Open(_)));

        let removed = operation_outcome(
            Ok(project(harkness_core::ProjectSource::Local, None)),
            "Removed",
            false,
        );
        assert_eq!(removed.status, "Removed sample");
        assert!(matches!(removed.opened, OpenedUpdate::Clear));
    }

    #[test]
    fn errors_are_actionable_and_do_not_navigate() {
        let outcome = operation_outcome(
            Err("project is unavailable at /missing".to_owned()),
            "Opened",
            true,
        );

        assert_eq!(outcome.status, "project is unavailable at /missing");
        assert!(matches!(outcome.opened, OpenedUpdate::Keep));
    }
}
