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

    #[namespace = "harkness"]
    unsafe extern "C++" {
        include!("harknessclipboard.h");

        #[rust_name = "set_clipboard_text"]
        fn setClipboardText(text: &QString);
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
        #[qproperty(QVariant, history)]
        #[qproperty(QVariant, issues)]
        #[qproperty(QVariant, checks)]
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

        /// Starts a fresh bounded history walk at HEAD.
        #[qinvokable]
        #[cxx_name = "refreshHistory"]
        fn refresh_history(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Requests exactly the continuation returned by the current history page.
        #[qinvokable]
        #[cxx_name = "loadMoreHistory"]
        fn load_more_history(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Loads the open repository's issues through the authenticated GitHub CLI.
        #[qinvokable]
        #[cxx_name = "refreshIssues"]
        fn refresh_issues(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            github_remote: &QString,
        );

        /// Loads one additional bounded page for the open repository.
        #[qinvokable]
        #[cxx_name = "loadMoreIssues"]
        fn load_more_issues(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            github_remote: &QString,
        );

        /// Reads configured checks and recorded results without executing any command.
        #[qinvokable]
        #[cxx_name = "refreshChecks"]
        fn refresh_checks(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Trusts the reviewed workspace when requested, then runs one exact configured check.
        #[qinvokable]
        #[cxx_name = "runCheck"]
        fn run_check(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            check_id: &QString,
            trust_workspace: bool,
        );

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

        /// Opens either side of the index in the shared renderer, optionally
        /// landing on one backend-owned changed-path token.
        #[qinvokable]
        #[cxx_name = "reviewWorkingChanges"]
        fn review_working_changes(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            staged: bool,
            path_id: &QString,
        );

        /// Applies a confirmed discard to one backend-owned status path.
        #[qinvokable]
        #[cxx_name = "discardPath"]
        fn discard_path(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            path_id: &QString,
            operation: &QString,
        );

        /// Applies a confirmed whole-file discard to the loaded review file.
        #[qinvokable]
        #[cxx_name = "discardReviewFile"]
        fn discard_review_file(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            file_id: &QString,
            operation: &QString,
        );

        /// Applies a confirmed reverse patch for one loaded review hunk.
        #[qinvokable]
        #[cxx_name = "discardReviewHunk"]
        fn discard_review_hunk(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            hunk_id: &QString,
        );

        /// Puts one diff line on the clipboard byte for byte.
        ///
        /// The review surface owns what is copied — it is the only place that
        /// knows which side of a row the reader asked for — and this owns only
        /// the writing, because QtQuick's own writer rewrites terminators.
        #[qinvokable]
        #[cxx_name = "copyToClipboard"]
        fn copy_to_clipboard(self: &HarknessBackend, text: &QString);

        /// Opens the loaded working-tree file at a backend-derived diff line.
        #[qinvokable]
        #[cxx_name = "openReviewLine"]
        fn open_review_line(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            file_id: &QString,
            line: i32,
        );

        /// Recomputes the open review under a named whitespace handling.
        ///
        /// `mode` is one of `exact`, `ignore_eol`, `ignore_change` and
        /// `ignore_all`; anything else leaves the surface untouched and says
        /// so. The same target, the same open file and the same scroll offset
        /// are re-requested, so the control changes only how the diff was
        /// computed. Anything but `exact` makes the surface view-only: hunk
        /// discard then re-requests the file exactly before it applies
        /// anything, and refuses when that recomputation disagrees with what
        /// was on screen.
        #[qinvokable]
        #[cxx_name = "setReviewWhitespace"]
        fn set_review_whitespace(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            mode: &QString,
            ignore_blank_lines: bool,
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

        /// Moves to the next fixed-size row window without omitting content.
        #[qinvokable]
        #[cxx_name = "loadMoreReviewRows"]
        fn load_more_review_rows(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Moves to the previous fixed-size row window.
        #[qinvokable]
        #[cxx_name = "loadPreviousReviewRows"]
        fn load_previous_review_rows(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Moves to the next bounded changed-file identity window.
        #[qinvokable]
        #[cxx_name = "loadMoreReviewFiles"]
        fn load_more_review_files(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Moves to the previous bounded changed-file identity window.
        #[qinvokable]
        #[cxx_name = "loadPreviousReviewFiles"]
        fn load_previous_review_files(self: Pin<&mut HarknessBackend>, project_id: &QString);

        /// Invalidates in-flight history and review requests for the open shell.
        #[qinvokable]
        #[cxx_name = "clearReview"]
        fn clear_review(self: Pin<&mut HarknessBackend>);

        /// Records the changes named by `path_ids` as one commit.
        ///
        /// `path_ids` is newline-delimited, which is safe because the values
        /// are backend-minted `path-N` tokens rather than paths or any other
        /// user content. Unknown entries are refused rather than skipped, so a
        /// stale token cannot silently shrink the commit.
        ///
        /// There is no separate staging step to invoke first: the index is not
        /// a surface this front end asks the user to operate. Staging happens
        /// inside the commit, under the repository lock the commit already
        /// holds, so what the Changes list showed is what the commit records.
        /// An empty selection is only valid when amending, where it means
        /// rewriting the previous commit's message and nothing else.
        #[qinvokable]
        fn commit(
            self: Pin<&mut HarknessBackend>,
            project_id: &QString,
            message: &QString,
            amend: bool,
            path_ids: &QString,
        );

        /// Updates remote-tracking state without touching the working tree.
        ///
        /// `quiet` suppresses only the success line in the status bar, for the
        /// callers that fetch on their own schedule rather than because the
        /// user asked. A failure is always reported: a background fetch that
        /// cannot reach the remote is exactly what the user needs told, and
        /// silence would leave a stale ahead/behind count looking current.
        #[qinvokable]
        fn fetch(self: Pin<&mut HarknessBackend>, project_id: &QString, quiet: bool);

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
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};
use serde::Deserialize;

pub struct HarknessBackendRust {
    busy: bool,
    status: QString,
    projects: QList<QVariant>,
    jobs: QList<QVariant>,
    branches: QList<QVariant>,
    worktrees: QList<QVariant>,
    opened: QVariant,
    git: QVariant,
    history: QVariant,
    issues: QVariant,
    checks: QVariant,
    review: QVariant,
    job_records: Vec<JobRecord>,
    cancellations: HashMap<String, harkness_git::Cancellation>,
    path_selections: HashMap<String, PathSelectionKey>,
    path_selection_ids: HashMap<PathSelectionKey, String>,
    path_discard_operations: HashMap<String, String>,
    path_discard_snapshots: HashMap<String, harkness_git::DiscardSnapshot>,
    discard_snapshot_cache: DiscardSnapshotCache,
    review_path_ids: HashMap<PathSelectionKey, String>,
    /// Catalog project ID to the actual Git common-directory scheduling domain.
    repository_lock_scopes: HashMap<String, String>,
    /// Managed worktree ID to the catalog parent's Git mutation domain. Git
    /// lifecycle operations act through the parent even when the checkout
    /// path has been replaced by another repository.
    worktree_lifecycle_lock_scopes: HashMap<String, String>,
    legacy_job: Option<String>,
    next_job_id: u64,
    next_path_selection: u64,
    next_review_path_identity: u64,
    history_state: Option<HistoryStateRow>,
    issues_state: Option<IssuesStateRow>,
    review_state: Option<ReviewStateRow>,
    next_catalog_request: u64,
    next_history_request: u64,
    next_issues_request: u64,
    next_branch_request: u64,
    next_worktree_request: u64,
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
            history: empty_history(),
            issues: empty_issues(),
            checks: empty_checks(),
            review: empty_review(),
            job_records: Vec::new(),
            cancellations: HashMap::new(),
            path_selections: HashMap::new(),
            path_selection_ids: HashMap::new(),
            path_discard_operations: HashMap::new(),
            path_discard_snapshots: HashMap::new(),
            discard_snapshot_cache: Arc::default(),
            review_path_ids: HashMap::new(),
            repository_lock_scopes: HashMap::new(),
            worktree_lifecycle_lock_scopes: HashMap::new(),
            legacy_job: None,
            next_job_id: 0,
            next_path_selection: 0,
            next_review_path_identity: 0,
            history_state: None,
            issues_state: None,
            review_state: None,
            next_catalog_request: 0,
            next_history_request: 0,
            next_issues_request: 0,
            next_branch_request: 0,
            next_worktree_request: 0,
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
    lock_scope: String,
    label: String,
    progress: String,
    cancellable: bool,
}

#[cfg(test)]
fn begin_job(
    jobs: &mut Vec<JobRecord>,
    next_job_id: &mut u64,
    kind: &str,
    project_id: &str,
    label: &str,
    cancellable: bool,
) -> Option<JobRecord> {
    begin_job_in_scope(
        jobs,
        next_job_id,
        kind,
        project_id,
        project_id,
        label,
        cancellable,
    )
}

fn begin_job_in_scope(
    jobs: &mut Vec<JobRecord>,
    next_job_id: &mut u64,
    kind: &str,
    project_id: &str,
    lock_scope: &str,
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
        lock_scope: lock_scope.to_owned(),
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
        insert(
            "lockScope",
            QVariant::from(&QString::from(row.lock_scope.as_str())),
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

fn is_review_read_job(kind: &str) -> bool {
    matches!(kind, "review" | "review_file" | "review_context")
}

fn is_repository_snapshot_job(kind: &str) -> bool {
    matches!(kind, "status" | "history" | "branches" | "worktrees")
}

fn is_check_job(kind: &str) -> bool {
    kind == "check"
}

fn is_repository_mutation_job(kind: &str) -> bool {
    matches!(
        kind,
        "stage"
            | "unstage"
            | "stage_hunk"
            | "unstage_hunk"
            | "discard_path"
            | "discard_hunk"
            | "commit"
            | "fetch"
            | "pull"
            | "push"
            | "checkout"
            | "create_branch"
            | "create_worktree"
            | "reconcile_worktrees"
            | "move_worktree"
            | "lock_worktree"
            | "unlock_worktree"
            | "remove_worktree"
            | "remove_managed"
    )
}

fn jobs_conflict(existing: &str, requested: &str) -> bool {
    (is_review_read_job(existing) && is_review_read_job(requested))
        || (is_review_read_job(existing) && is_repository_mutation_job(requested))
        || (is_repository_mutation_job(existing) && is_review_read_job(requested))
        || (is_repository_mutation_job(existing) && is_repository_mutation_job(requested))
        || (is_repository_snapshot_job(existing) && is_repository_mutation_job(requested))
        || (is_repository_mutation_job(existing) && is_repository_snapshot_job(requested))
        || (is_check_job(existing)
            && (is_check_job(requested)
                || is_repository_mutation_job(requested)
                || is_review_read_job(requested)))
        || (is_check_job(requested)
            && (is_repository_mutation_job(existing) || is_review_read_job(existing)))
}

fn conflicting_repository_job<'a>(
    jobs: &'a [JobRecord],
    lock_scope: &str,
    requested_kind: &str,
) -> Option<&'a JobRecord> {
    jobs.iter()
        .find(|job| job.lock_scope == lock_scope && jobs_conflict(&job.kind, requested_kind))
}

fn start_job(
    backend: Pin<&mut ffi::HarknessBackend>,
    kind: &str,
    project_id: &str,
    label: &str,
    cancellable: bool,
) -> Option<(String, harkness_git::Cancellation)> {
    let lock_scope = backend
        .as_ref()
        .rust()
        .repository_lock_scopes
        .get(project_id)
        .cloned()
        .or_else(|| {
            (opened_project_id(backend.as_ref().opened()).as_deref() == Some(project_id))
                .then(|| opened_repository_lock_scope(backend.as_ref().opened()))
                .flatten()
        })
        .unwrap_or_else(|| project_id.to_owned());
    start_job_in_scope(backend, kind, project_id, &lock_scope, label, cancellable)
}

fn start_job_in_scope(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    kind: &str,
    project_id: &str,
    lock_scope: &str,
    label: &str,
    cancellable: bool,
) -> Option<(String, harkness_git::Cancellation)> {
    let conflicting_label =
        conflicting_repository_job(&backend.as_ref().rust().job_records, lock_scope, kind)
            .map(|job| job.label.clone());
    if let Some(conflicting_label) = conflicting_label {
        backend.as_mut().set_status(
            format!("Wait for {conflicting_label} to finish before starting {label}").into(),
        );
        return None;
    }
    let job = {
        let rust = backend.as_mut().rust_mut().get_mut();
        begin_job_in_scope(
            &mut rust.job_records,
            &mut rust.next_job_id,
            kind,
            project_id,
            lock_scope,
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

/// What a `pathId` token handed to QML stands for.
///
/// The rename sources are part of the identity rather than description: a path
/// whose rename state has changed is a different selection, so a token minted
/// against the old state stops resolving instead of silently addressing a row
/// the user never picked.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PathSelectionKey {
    project_id: String,
    path: PathBuf,
    staged_rename_source: Option<PathBuf>,
    unstaged_rename_source: Option<PathBuf>,
}

impl PathSelectionKey {
    /// The identity of one status row, with the rename sources that belong to
    /// it. A rename source is part of the key only on the side that reported
    /// the rename, so the two sides stay independently addressable.
    fn new(
        project_id: &str,
        path: &Path,
        rename_source: Option<&Path>,
        staged_rename: bool,
        unstaged_rename: bool,
    ) -> Self {
        Self {
            project_id: project_id.to_owned(),
            path: path.to_path_buf(),
            staged_rename_source: staged_rename
                .then(|| rename_source.map(Path::to_path_buf))
                .flatten(),
            unstaged_rename_source: unstaged_rename
                .then(|| rename_source.map(Path::to_path_buf))
                .flatten(),
        }
    }

    /// Every native path a commit of this one row has to name.
    ///
    /// A rename is one row but two paths. Naming only the destination would
    /// record it as a new file and leave the original standing, so the source
    /// travels with it and the commit records the rename it displayed.
    fn commit_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.path.clone()];
        for source in [&self.staged_rename_source, &self.unstaged_rename_source]
            .into_iter()
            .flatten()
        {
            if !paths.contains(source) {
                paths.push(source.clone());
            }
        }
        paths
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DiscardSnapshotCacheKey {
    root: PathBuf,
    selection: PathSelectionKey,
    operation: String,
    staged: String,
    unstaged: String,
}

#[derive(Clone, Debug)]
struct CachedDiscardSnapshot {
    metadata: Vec<PathMetadataFingerprint>,
    snapshot: harkness_git::DiscardSnapshot,
}

type DiscardSnapshotCache = Arc<Mutex<HashMap<DiscardSnapshotCacheKey, CachedDiscardSnapshot>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathMetadataFingerprint {
    path: PathBuf,
    state: MetadataState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MetadataState {
    Present(MetadataFingerprint),
    Unavailable(std::io::ErrorKind),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataFingerprint {
    len: u64,
    modified_seconds: u64,
    modified_nanoseconds: u32,
    is_file: bool,
    is_dir: bool,
    is_symlink: bool,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

fn path_metadata_fingerprints(root: &Path, paths: &[PathBuf]) -> Vec<PathMetadataFingerprint> {
    paths
        .iter()
        .map(|path| {
            let state = fs::symlink_metadata(root.join(path)).map_or_else(
                |error| MetadataState::Unavailable(error.kind()),
                |metadata| {
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .unwrap_or_default();
                    #[cfg(unix)]
                    use std::os::unix::fs::MetadataExt;
                    MetadataState::Present(MetadataFingerprint {
                        len: metadata.len(),
                        modified_seconds: modified.as_secs(),
                        modified_nanoseconds: modified.subsec_nanos(),
                        is_file: metadata.file_type().is_file(),
                        is_dir: metadata.file_type().is_dir(),
                        is_symlink: metadata.file_type().is_symlink(),
                        readonly: metadata.permissions().readonly(),
                        #[cfg(unix)]
                        device: metadata.dev(),
                        #[cfg(unix)]
                        inode: metadata.ino(),
                        #[cfg(unix)]
                        mode: metadata.mode(),
                        #[cfg(unix)]
                        changed_seconds: metadata.ctime(),
                        #[cfg(unix)]
                        changed_nanoseconds: metadata.ctime_nsec(),
                    })
                },
            );
            PathMetadataFingerprint {
                path: path.clone(),
                state,
            }
        })
        .collect()
}

#[derive(Debug)]
struct StatusEntryRow {
    path: PathBuf,
    display_path: String,
    staged: String,
    unstaged: String,
    rename_source: String,
    rename_source_path: Option<PathBuf>,
    staged_rename: bool,
    unstaged_rename: bool,
    conflicted: bool,
    discard_snapshot: Option<harkness_git::DiscardSnapshot>,
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
                let staged_rename = entry.staged == Some(harkness_git::FileChange::Renamed);
                let unstaged_rename = entry.unstaged == Some(harkness_git::FileChange::Renamed);
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
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                    rename_source_path: entry.rename_source,
                    staged_rename,
                    unstaged_rename,
                    conflicted: entry.conflicted,
                    discard_snapshot: None,
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

fn attach_discard_snapshots(
    git: &harkness_git::GitService,
    state: &mut GitStateRow,
    cache: &DiscardSnapshotCache,
) {
    attach_discard_snapshots_with(git.root(), state, cache, |paths| {
        git.discard_snapshot(paths).ok()
    });
}

fn attach_discard_snapshots_with(
    root: &Path,
    state: &mut GitStateRow,
    cache: &DiscardSnapshotCache,
    mut capture: impl FnMut(Vec<PathBuf>) -> Option<harkness_git::DiscardSnapshot>,
) {
    let mut current_keys = HashSet::new();
    for entry in &mut state.entries {
        let Some(description) = status_discard_description(entry) else {
            continue;
        };
        let selection = PathSelectionKey {
            project_id: state.project_id.clone(),
            path: entry.path.clone(),
            staged_rename_source: entry
                .staged_rename
                .then(|| entry.rename_source_path.clone())
                .flatten(),
            unstaged_rename_source: entry
                .unstaged_rename
                .then(|| entry.rename_source_path.clone())
                .flatten(),
        };
        let paths = selection.commit_paths();
        let key = DiscardSnapshotCacheKey {
            root: root.to_path_buf(),
            selection,
            operation: discard_operation_name(&description).to_owned(),
            staged: entry.staged.clone(),
            unstaged: entry.unstaged.clone(),
        };
        let metadata = path_metadata_fingerprints(root, &paths);
        current_keys.insert(key.clone());
        let cached = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .filter(|cached| cached.metadata == metadata)
            .map(|cached| cached.snapshot.clone());
        entry.discard_snapshot = if let Some(snapshot) = cached {
            Some(snapshot)
        } else {
            let snapshot = capture(paths);
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(snapshot) = &snapshot {
                cache.insert(
                    key,
                    CachedDiscardSnapshot {
                        metadata,
                        snapshot: snapshot.clone(),
                    },
                );
            } else {
                cache.remove(&key);
            }
            snapshot
        };
    }
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|key, _| {
            key.selection.project_id != state.project_id || current_keys.contains(key)
        });
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
    rename_source: Option<&Path>,
    staged_rename: bool,
    unstaged_rename: bool,
) -> String {
    let key = PathSelectionKey::new(
        project_id,
        path,
        rename_source,
        staged_rename,
        unstaged_rename,
    );
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

fn register_review_path_identity(
    backend: &mut HarknessBackendRust,
    project_id: &str,
    path: &Path,
) -> String {
    let key = PathSelectionKey {
        project_id: project_id.to_owned(),
        path: path.to_path_buf(),
        staged_rename_source: None,
        unstaged_rename_source: None,
    };
    if let Some(identity) = backend.review_path_ids.get(&key) {
        return identity.clone();
    }
    backend.next_review_path_identity += 1;
    let identity = format!("review-path-{}", backend.next_review_path_identity);
    backend.review_path_ids.insert(key, identity.clone());
    identity
}

fn resolve_path_selection(
    backend: &HarknessBackendRust,
    project_id: &str,
    selection_id: &str,
) -> Result<PathSelectionKey, String> {
    let Some(selection) = backend.path_selections.get(selection_id) else {
        return Err("The selected path is no longer available; refresh Git status".to_owned());
    };
    if selection.project_id != project_id {
        return Err("The selected path belongs to a different project".to_owned());
    }
    Ok(selection.clone())
}

/// Turns the tokens QML checked into the scope one commit should record.
///
/// A selection covering every currently registered path becomes
/// [`harkness_git::CommitScope::WorkingTree`] rather than a path list. The two
/// record the same tree, but the path list has to name every path on one
/// command line, and a working tree large enough would overrun it.
fn resolve_commit_scope(
    backend: &HarknessBackendRust,
    project_id: &str,
    path_ids: &str,
    amend: bool,
) -> Result<harkness_git::CommitScope, String> {
    let mut selected = Vec::new();
    let mut paths = Vec::new();
    for path_id in path_ids.lines().filter(|line| !line.is_empty()) {
        let selection = resolve_path_selection(backend, project_id, path_id)?;
        if !selected.contains(&path_id.to_owned()) {
            selected.push(path_id.to_owned());
            for path in selection.commit_paths() {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }

    if selected.is_empty() {
        // Amending with nothing selected rewrites the previous commit's
        // message against its own tree, so there is nothing to stage.
        return if amend {
            Ok(harkness_git::CommitScope::Index)
        } else {
            Err("Select at least one file to commit".to_owned())
        };
    }
    if selected.len() == backend.path_selections.len() {
        return Ok(harkness_git::CommitScope::WorkingTree);
    }
    Ok(harkness_git::CommitScope::Paths(paths))
}

fn status_discard_description(entry: &StatusEntryRow) -> Option<harkness_git::DiscardDescription> {
    if entry.conflicted {
        return None;
    }
    let mut paths = vec![entry.path.as_path()];
    if (entry.staged_rename || entry.unstaged_rename)
        && let Some(source) = entry.rename_source_path.as_deref()
        && !paths.contains(&source)
    {
        paths.push(source);
    }
    if entry.unstaged == "untracked" {
        return Some(harkness_git::DiscardDescription::delete_untracked(paths));
    }
    if !entry.unstaged.is_empty() {
        return Some(harkness_git::DiscardDescription::restore_tracked(
            paths,
            harkness_git::TrackedRestoreSource::Index,
        ));
    }
    (!entry.staged.is_empty()).then(|| {
        harkness_git::DiscardDescription::restore_tracked(
            paths,
            harkness_git::TrackedRestoreSource::Head,
        )
    })
}

fn review_file_discard_description(
    file: &harkness_git::FileDiff,
) -> Option<harkness_git::DiscardDescription> {
    let path = review_path(file);
    let mut paths = [file.old_path.as_deref(), file.new_path.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    match (&file.target, file.change) {
        (_, harkness_git::FileChange::Unmerged) => None,
        (harkness_git::DiffTarget::Unstaged, harkness_git::FileChange::Untracked) => {
            Some(harkness_git::DiscardDescription::delete_untracked([path]))
        }
        (harkness_git::DiffTarget::Unstaged, _) => {
            Some(harkness_git::DiscardDescription::restore_tracked(
                paths,
                harkness_git::TrackedRestoreSource::Index,
            ))
        }
        (harkness_git::DiffTarget::Staged, _) => {
            Some(harkness_git::DiscardDescription::restore_tracked(
                paths,
                harkness_git::TrackedRestoreSource::Head,
            ))
        }
        _ => None,
    }
}

fn discard_operation_name(description: &harkness_git::DiscardDescription) -> &'static str {
    match description.operation() {
        harkness_git::DiscardOperation::RestoreTracked {
            source: harkness_git::TrackedRestoreSource::Index,
        } => "restore_index",
        harkness_git::DiscardOperation::RestoreTracked {
            source: harkness_git::TrackedRestoreSource::Head,
        } => "restore_head",
        harkness_git::DiscardOperation::RestoreTrackedHunks { .. } => "restore_hunks",
        harkness_git::DiscardOperation::RestoreTrackedLines { .. } => "restore_lines",
        harkness_git::DiscardOperation::DeleteUntracked => "delete_untracked",
        _ => "",
    }
}

fn to_discard_description(description: Option<&harkness_git::DiscardDescription>) -> QVariant {
    let Some(description) = description else {
        return QVariant::from(&QMap::<QMapPair_QString_QVariant>::default());
    };
    let mut value = QMap::<QMapPair_QString_QVariant>::default();
    value.insert(
        QString::from("operation"),
        QVariant::from(&QString::from(discard_operation_name(description))),
    );
    value.insert(
        QString::from("trackedFiles"),
        QVariant::from(&i32::try_from(description.tracked_files()).unwrap_or(i32::MAX)),
    );
    value.insert(
        QString::from("untrackedFiles"),
        QVariant::from(&i32::try_from(description.untracked_files()).unwrap_or(i32::MAX)),
    );
    value.insert(
        QString::from("unrecoverable"),
        QVariant::from(&matches!(
            description.recoverability(),
            harkness_git::DiscardRecoverability::Unrecoverable
        )),
    );
    let (hunks, lines) = match description.operation() {
        harkness_git::DiscardOperation::RestoreTrackedHunks { hunks } => (hunks, 0),
        harkness_git::DiscardOperation::RestoreTrackedLines { lines, hunks } => (hunks, lines),
        _ => (0, 0),
    };
    value.insert(
        QString::from("hunks"),
        QVariant::from(&i32::try_from(hunks).unwrap_or(i32::MAX)),
    );
    value.insert(
        QString::from("lines"),
        QVariant::from(&i32::try_from(lines).unwrap_or(i32::MAX)),
    );
    let mut paths = QList::<QVariant>::default();
    for path in description.paths() {
        paths.append(QVariant::from(&QString::from(path.display().to_string())));
    }
    value.insert(QString::from("paths"), QVariant::from(&paths));
    QVariant::from(&value)
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
        let discard = status_discard_description(row);
        insert("discard", to_discard_description(discard.as_ref()));
        entries.append(QVariant::from(&entry));
    }
    insert("entries", QVariant::from(&entries));
    QVariant::from(&state)
}

fn empty_git() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn replace_status_path_selections(
    backend: &mut HarknessBackendRust,
    row: &GitStateRow,
) -> Vec<String> {
    // These opaque IDs are capabilities for native paths, including both
    // sides of a rename. Replace the capability set atomically with the status
    // projection so a cached token can never authorize paths from an older
    // repository state.
    //
    // A key whose row is unchanged keeps the token it already has. The
    // capability is the same one — same project, same path, same rename sides
    // — and the stability is what lets an unchanged status project
    // byte-identically, so QML is told the working tree changed only when it
    // did. Minting fresh tokens every read made every poll look like a change
    // and rebuilt the whole Changes list on a timer.
    //
    // "Unchanged" has to include the discard snapshot, not just the key. A
    // confirmation the user is looking at was shown against the snapshot
    // recorded under this token, and a poll recaptures that snapshot as soon
    // as the file's metadata moves. Re-minting when it does is what makes the
    // pending prompt refuse instead of silently applying an unrecoverable
    // delete to content the user never saw.
    let surviving = row
        .entries
        .iter()
        .filter_map(|entry| {
            let key = PathSelectionKey::new(
                &row.project_id,
                &entry.path,
                entry.rename_source_path.as_deref(),
                entry.staged_rename,
                entry.unstaged_rename,
            );
            let token = backend.path_selection_ids.get(&key)?;
            (backend.path_discard_snapshots.get(token) == entry.discard_snapshot.as_ref())
                .then(|| (key, token.clone()))
        })
        .collect::<HashMap<_, _>>();
    backend.path_selections = surviving
        .iter()
        .map(|(key, token)| (token.clone(), key.clone()))
        .collect();
    backend.path_selection_ids = surviving;
    backend.path_discard_operations.clear();
    backend.path_discard_snapshots.clear();
    row.entries
        .iter()
        .map(|entry| {
            let token = register_path_selection(
                backend,
                &row.project_id,
                &entry.path,
                entry.rename_source_path.as_deref(),
                entry.staged_rename,
                entry.unstaged_rename,
            );
            if let Some(description) = status_discard_description(entry) {
                backend.path_discard_operations.insert(
                    token.clone(),
                    discard_operation_name(&description).to_owned(),
                );
                if let Some(snapshot) = &entry.discard_snapshot {
                    backend
                        .path_discard_snapshots
                        .insert(token.clone(), snapshot.clone());
                }
            }
            token
        })
        .collect()
}

fn set_git_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: &GitStateRow) {
    let path_selection_ids = {
        let rust = backend.as_mut().rust_mut().get_mut();
        replace_status_path_selections(rust, row)
    };
    backend.as_mut().set_git(to_git(row, &path_selection_ids));
}

fn clear_git_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.path_selections.clear();
        rust.path_selection_ids.clear();
        rust.path_discard_operations.clear();
        rust.path_discard_snapshots.clear();
    }
    backend.as_mut().set_git(empty_git());
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

fn bounded_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
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

fn load_check_project(
    project_id: &str,
) -> Result<(harkness_core::ProjectService, harkness_core::Project), String> {
    let id: harkness_core::ProjectId = project_id
        .parse()
        .map_err(|_| "invalid project identifier".to_owned())?;
    let service = harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
    let project = service
        .resolve(&harkness_core::ProjectSelector::from(id.to_string()))
        .map_err(|error| error.to_string())?;
    Ok((service, project))
}

fn load_project_checks(
    project_id: &str,
) -> Result<
    (
        Vec<harkness_core::CheckConfiguration>,
        Vec<harkness_runtime::check::CheckSummary>,
    ),
    String,
> {
    let (service, project) = load_check_project(project_id)?;
    let configurations = project.effective_checks();
    let store = harkness_runtime::store::Store::open(service.data_dir())
        .map_err(|error| error.to_string())?;
    let results = harkness_runtime::check::project_checks(&store, &project)
        .map_err(|error| error.to_string())?;
    Ok((configurations, results))
}

fn run_project_check(
    project_id: &str,
    check_id: &str,
    trust_workspace: bool,
    cancellation: &harkness_git::Cancellation,
) -> Result<
    (
        Vec<harkness_core::CheckConfiguration>,
        Vec<harkness_runtime::check::CheckSummary>,
    ),
    String,
> {
    let (service, project) = load_check_project(project_id)?;
    let configurations = project.effective_checks();
    let check = configurations
        .iter()
        .find(|check| check.id == check_id)
        .ok_or_else(|| format!("project has no configured check {check_id:?}"))?;
    let store = Arc::new(
        harkness_runtime::store::Store::open(service.data_dir())
            .map_err(|error| error.to_string())?,
    );
    if trust_workspace {
        let trust = harkness_runtime::trust::WorkspaceTrust::decide(
            project.id,
            &project.root,
            harkness_runtime::trust::TrustState::Trusted,
            time::OffsetDateTime::now_utc(),
        )
        .map_err(|error| error.to_string())?;
        store
            .put_workspace_trust(&trust)
            .map_err(|error| error.to_string())?;
    }
    if store
        .resolve_workspace_trust(project.id, &project.root)
        .map_err(|error| error.to_string())?
        != harkness_runtime::trust::TrustState::Trusted
    {
        return Err(
            "workspace is untrusted; review and trust it before running a check".to_owned(),
        );
    }
    harkness_runtime::check::run_configured_check(
        Arc::clone(&store),
        &project,
        check,
        harkness_runtime::approval::DecidedVia::Gui,
        cancellation,
    )
    .map_err(|error| error.to_string())?;
    let results = harkness_runtime::check::project_checks(&store, &project)
        .map_err(|error| error.to_string())?;
    Ok((configurations, results))
}

const HISTORY_PAGE_SIZE: usize = 50;
const REVIEW_CONTEXT_STEP: u32 = 20;
// This is a transfer page, not an omission limit: QML can request every
// subsequent page, while each GUI-thread QVariant conversion remains bounded.
const REVIEW_ROW_PAGE_SIZE: usize = 12_000;
const REVIEW_FILE_PAGE_SIZE: usize = 512;
/// How far back attribution walks before a review gives up and says so.
///
/// This is well below `harkness_git::DEFAULT_MAX_PROVENANCE_COMMITS`, because
/// the two are paid for differently. A caller typing `harkness git diff
/// --provenance` asked for a walk and is waiting for its result; the panel
/// resolves attribution on the path that opens a review, so every commit walked
/// is a commit a reader waits through before they see a file list. A range
/// longer than this is not a review anybody reads file by file, and reaching
/// the bound is reported by name rather than guessed at — the header says older
/// commits were not walked, and the files beyond it read as unknown.
const REVIEW_PROVENANCE_MAX_COMMITS: usize = 250;

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
struct IssueLabelRow {
    name: String,
    color: String,
}

#[derive(Clone, Debug)]
struct IssueRow {
    id: String,
    number: u64,
    title: String,
    state: String,
    url: String,
    author: String,
    updated: String,
    labels: Vec<IssueLabelRow>,
    milestone: String,
    assignees: Vec<String>,
    comment_count: u64,
    created_by_me: bool,
    assigned_to_me: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubUserRow {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubLabelRow {
    name: String,
    color: String,
}

#[derive(Debug, Deserialize)]
struct GithubMilestoneRow {
    title: String,
}

#[derive(Debug, Deserialize)]
struct GithubNodeConnection<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubCount {
    #[serde(rename = "totalCount")]
    total_count: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubIssueRow {
    id: String,
    number: u64,
    title: String,
    state: String,
    url: String,
    author: Option<GithubUserRow>,
    updated_at: String,
    labels: GithubNodeConnection<GithubLabelRow>,
    milestone: Option<GithubMilestoneRow>,
    assignees: GithubNodeConnection<GithubUserRow>,
    comments: GithubCount,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubIssueConnection {
    total_count: u64,
    page_info: GithubPageInfo,
    nodes: Vec<GithubIssueRow>,
}

#[derive(Debug, Deserialize)]
struct GithubRepositoryRow {
    issues: GithubIssueConnection,
}

#[derive(Debug, Deserialize)]
struct GithubGraphqlData {
    viewer: GithubUserRow,
    repository: Option<GithubRepositoryRow>,
}

#[derive(Debug, Deserialize)]
struct GithubGraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GithubGraphqlResponse {
    data: Option<GithubGraphqlData>,
    #[serde(default)]
    errors: Vec<GithubGraphqlError>,
}

#[derive(Debug)]
struct GithubIssuePage {
    viewer: String,
    rows: Vec<IssueRow>,
    next_cursor: Option<String>,
    total_count: u64,
    limit_reached: bool,
}

#[derive(Clone, Debug)]
struct IssuesStateRow {
    project_id: String,
    remote: String,
    loading: bool,
    viewer: String,
    rows: Vec<IssueRow>,
    next_cursor: Option<String>,
    total_count: u64,
    limit_reached: bool,
    error: String,
    error_kind: String,
}

impl IssuesStateRow {
    fn loading(project_id: String, remote: String) -> Self {
        Self {
            project_id,
            remote,
            loading: true,
            viewer: String::new(),
            rows: Vec::new(),
            next_cursor: None,
            total_count: 0,
            limit_reached: false,
            error: String::new(),
            error_kind: String::new(),
        }
    }

    fn with_failure(mut self, failure: GitFailure) -> Self {
        self.loading = false;
        self.error = failure.message;
        self.error_kind = failure.kind;
        self
    }
}

const GITHUB_HOST: &str = "github.com";
const GITHUB_MAX_ISSUES: usize = 1_000;
const GITHUB_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const GITHUB_ERROR_LIMIT: usize = 64 * 1024;
const GITHUB_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_POLL_INTERVAL: Duration = Duration::from_millis(20);

const GITHUB_ISSUES_QUERY: &str = r#"
query($owner: String!, $name: String!, $endCursor: String) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    issues(
      first: 100
      after: $endCursor
      orderBy: {field: CREATED_AT, direction: ASC}
      states: [OPEN, CLOSED]
    ) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        id number title state url updatedAt
        author { login }
        labels(first: 100) { nodes { name color } }
        milestone { title }
        assignees(first: 100) { nodes { login } }
        comments { totalCount }
      }
    }
  }
}
"#;

fn empty_issues() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn empty_checks() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
}

fn check_outcome_name(outcome: harkness_runtime::check::CheckOutcome) -> &'static str {
    use harkness_runtime::check::CheckOutcome;
    match outcome {
        CheckOutcome::Queued => "queued",
        CheckOutcome::WaitingForApproval => "waiting_for_approval",
        CheckOutcome::Running => "running",
        CheckOutcome::Passed => "passed",
        CheckOutcome::Failed => "failed",
        CheckOutcome::TimedOut => "timed_out",
        CheckOutcome::Denied => "denied",
        CheckOutcome::Cancelled => "cancelled",
        CheckOutcome::Interrupted => "interrupted",
    }
}

fn check_evidence_class_name(evidence: harkness_runtime::check::ActivityClass) -> &'static str {
    use harkness_runtime::check::ActivityClass;
    match evidence {
        ActivityClass::HarknessObserved => "harkness_observed",
        ActivityClass::HarknessMediated => "harkness_mediated",
        ActivityClass::AcpReported => "acp_reported",
        ActivityClass::SnapshotInferred => "snapshot_inferred",
        ActivityClass::Unobserved => "unobserved",
    }
}

fn check_parser_name(parser: harkness_core::CheckParser) -> &'static str {
    match parser {
        harkness_core::CheckParser::Plain => "plain",
        harkness_core::CheckParser::CargoJson => "cargo_json",
    }
}

fn check_environment(environment: &std::collections::BTreeMap<String, String>) -> QList<QVariant> {
    let mut projected = QList::<QVariant>::default();
    for (name, value) in environment {
        let mut row = QMap::<QMapPair_QString_QVariant>::default();
        row.insert(
            QString::from("name"),
            QVariant::from(&QString::from(name.as_str())),
        );
        row.insert(
            QString::from("value"),
            QVariant::from(&QString::from(value.as_str())),
        );
        projected.append(QVariant::from(&row));
    }
    projected
}

fn to_checks(
    project_id: &str,
    configurations: &[harkness_core::CheckConfiguration],
    results: &[harkness_runtime::check::CheckSummary],
    loading: bool,
    error: &str,
) -> QVariant {
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    state.insert(
        QString::from("projectId"),
        QVariant::from(&QString::from(project_id)),
    );
    state.insert(QString::from("loading"), QVariant::from(&loading));
    state.insert(
        QString::from("error"),
        QVariant::from(&QString::from(error)),
    );

    let mut configured = QList::<QVariant>::default();
    for check in configurations {
        let mut row = QMap::<QMapPair_QString_QVariant>::default();
        row.insert(
            QString::from("id"),
            QVariant::from(&QString::from(check.id.as_str())),
        );
        row.insert(
            QString::from("label"),
            QVariant::from(&QString::from(check.label.as_str())),
        );
        let mut command = QList::<QVariant>::default();
        for part in &check.command {
            command.append(QVariant::from(&QString::from(part.as_str())));
        }
        row.insert(QString::from("command"), QVariant::from(&command));
        row.insert(
            QString::from("cwd"),
            QVariant::from(&QString::from(check.cwd.as_deref().unwrap_or(""))),
        );
        row.insert(
            QString::from("environment"),
            QVariant::from(&check_environment(&check.env)),
        );
        row.insert(
            QString::from("timeoutSeconds"),
            QVariant::from(&i64::try_from(check.timeout_seconds.unwrap_or(0)).unwrap_or(i64::MAX)),
        );
        row.insert(
            QString::from("parser"),
            QVariant::from(&QString::from(check_parser_name(check.parser))),
        );
        configured.append(QVariant::from(&row));
    }
    state.insert(QString::from("configured"), QVariant::from(&configured));

    let mut recorded = QList::<QVariant>::default();
    for result in results {
        let mut row = QMap::<QMapPair_QString_QVariant>::default();
        row.insert(
            QString::from("runId"),
            QVariant::from(&QString::from(result.run_id.as_str())),
        );
        row.insert(
            QString::from("checkId"),
            QVariant::from(&QString::from(result.check_id.as_str())),
        );
        row.insert(
            QString::from("label"),
            QVariant::from(&QString::from(result.label.as_str())),
        );
        row.insert(
            QString::from("outcome"),
            QVariant::from(&QString::from(check_outcome_name(result.outcome))),
        );
        row.insert(
            QString::from("evidenceClass"),
            QVariant::from(&QString::from(check_evidence_class_name(
                result.evidence_class,
            ))),
        );
        let (freshness, freshness_detail) = match &result.freshness {
            harkness_runtime::check::CheckFreshness::Current => ("current", String::new()),
            harkness_runtime::check::CheckFreshness::Stale { changed } => {
                ("stale", changed.join(", "))
            }
            harkness_runtime::check::CheckFreshness::Unverifiable { reason } => {
                ("unverifiable", reason.clone())
            }
        };
        row.insert(
            QString::from("freshness"),
            QVariant::from(&QString::from(freshness)),
        );
        row.insert(
            QString::from("freshnessDetail"),
            QVariant::from(&QString::from(freshness_detail.as_str())),
        );
        row.insert(
            QString::from("stateHead"),
            QVariant::from(&QString::from(result.state_head.as_deref().unwrap_or(""))),
        );
        row.insert(
            QString::from("stateDigest"),
            QVariant::from(&QString::from(result.state_digest.as_deref().unwrap_or(""))),
        );
        row.insert(
            QString::from("createdAt"),
            QVariant::from(&QString::from(result.created_at.as_str())),
        );
        row.insert(
            QString::from("durationMs"),
            QVariant::from(&i64::try_from(result.duration_ms.unwrap_or(0)).unwrap_or(i64::MAX)),
        );
        row.insert(
            QString::from("stdoutTail"),
            QVariant::from(&QString::from(result.stdout_tail.as_str())),
        );
        row.insert(
            QString::from("stderrTail"),
            QVariant::from(&QString::from(result.stderr_tail.as_str())),
        );
        row.insert(
            QString::from("stdoutTruncated"),
            QVariant::from(&result.stdout_truncated),
        );
        row.insert(
            QString::from("stderrTruncated"),
            QVariant::from(&result.stderr_truncated),
        );
        row.insert(
            QString::from("artifactByteLimit"),
            QVariant::from(&i64::try_from(result.artifact_byte_limit).unwrap_or(i64::MAX)),
        );
        row.insert(
            QString::from("stdoutArtifactTruncated"),
            QVariant::from(&result.stdout_artifact_truncated),
        );
        row.insert(
            QString::from("stderrArtifactTruncated"),
            QVariant::from(&result.stderr_artifact_truncated),
        );
        let mut recorded_command = QList::<QVariant>::default();
        for part in &result.command {
            recorded_command.append(QVariant::from(&QString::from(part.as_str())));
        }
        row.insert(
            QString::from("recordedCommand"),
            QVariant::from(&recorded_command),
        );
        row.insert(
            QString::from("recordedCwd"),
            QVariant::from(&QString::from(result.recorded_cwd.as_deref().unwrap_or(""))),
        );
        row.insert(
            QString::from("recordedEnvironment"),
            QVariant::from(&check_environment(&result.recorded_env)),
        );
        row.insert(
            QString::from("recordedTimeoutSeconds"),
            QVariant::from(
                &i64::try_from(result.recorded_timeout.unwrap_or(0)).unwrap_or(i64::MAX),
            ),
        );
        row.insert(
            QString::from("recordedParser"),
            QVariant::from(&QString::from(result.recorded_parser.as_str())),
        );
        row.insert(
            QString::from("definitionCurrent"),
            QVariant::from(&result.definition_current),
        );
        row.insert(
            QString::from("workspaceCleanKnown"),
            QVariant::from(&result.workspace_clean.is_some()),
        );
        row.insert(
            QString::from("workspaceClean"),
            QVariant::from(&result.workspace_clean.unwrap_or(false)),
        );
        row.insert(
            QString::from("workspaceMatchesIndexKnown"),
            QVariant::from(&result.workspace_matches_index.is_some()),
        );
        row.insert(
            QString::from("workspaceMatchesIndex"),
            QVariant::from(&result.workspace_matches_index.unwrap_or(false)),
        );
        let mut diagnostics = QList::<QVariant>::default();
        for diagnostic in &result.diagnostics {
            let mut value = QMap::<QMapPair_QString_QVariant>::default();
            value.insert(
                QString::from("path"),
                QVariant::from(&QString::from(diagnostic.path.as_deref().unwrap_or(""))),
            );
            value.insert(
                QString::from("line"),
                QVariant::from(&i32::try_from(diagnostic.line.unwrap_or(0)).unwrap_or(i32::MAX)),
            );
            value.insert(
                QString::from("column"),
                QVariant::from(&i32::try_from(diagnostic.column.unwrap_or(0)).unwrap_or(i32::MAX)),
            );
            value.insert(
                QString::from("level"),
                QVariant::from(&QString::from(diagnostic.level.as_str())),
            );
            value.insert(
                QString::from("message"),
                QVariant::from(&QString::from(diagnostic.message.as_str())),
            );
            diagnostics.append(QVariant::from(&value));
        }
        row.insert(QString::from("diagnostics"), QVariant::from(&diagnostics));
        row.insert(
            QString::from("diagnosticsOmitted"),
            QVariant::from(&i32::try_from(result.diagnostics_omitted).unwrap_or(i32::MAX)),
        );
        row.insert(
            QString::from("diagnosticsScanTruncated"),
            QVariant::from(&result.diagnostics_scan_truncated),
        );
        recorded.append(QVariant::from(&row));
    }
    state.insert(QString::from("results"), QVariant::from(&recorded));
    QVariant::from(&state)
}

fn to_issues(row: &IssuesStateRow) -> QVariant {
    let mut state = QMap::<QMapPair_QString_QVariant>::default();
    let mut insert = |key: &str, value: QVariant| state.insert(QString::from(key), value);
    insert(
        "projectId",
        QVariant::from(&QString::from(row.project_id.as_str())),
    );
    insert(
        "remote",
        QVariant::from(&QString::from(row.remote.as_str())),
    );
    insert("loading", QVariant::from(&row.loading));
    insert(
        "viewer",
        QVariant::from(&QString::from(row.viewer.as_str())),
    );
    insert("error", QVariant::from(&QString::from(row.error.as_str())));
    insert(
        "errorKind",
        QVariant::from(&QString::from(row.error_kind.as_str())),
    );
    insert("hasMore", QVariant::from(&row.next_cursor.is_some()));
    insert("limitReached", QVariant::from(&row.limit_reached));
    insert(
        "totalCount",
        QVariant::from(&i32::try_from(row.total_count).unwrap_or(i32::MAX)),
    );

    let mut issues = QList::<QVariant>::default();
    for row in &row.rows {
        let mut issue = QMap::<QMapPair_QString_QVariant>::default();
        let mut insert = |key: &str, value: QVariant| issue.insert(QString::from(key), value);
        insert("id", QVariant::from(&QString::from(row.id.as_str())));
        insert(
            "number",
            QVariant::from(&i32::try_from(row.number).unwrap_or(i32::MAX)),
        );
        insert("title", QVariant::from(&QString::from(row.title.as_str())));
        insert("state", QVariant::from(&QString::from(row.state.as_str())));
        insert("url", QVariant::from(&QString::from(row.url.as_str())));
        insert(
            "author",
            QVariant::from(&QString::from(row.author.as_str())),
        );
        insert(
            "updated",
            QVariant::from(&QString::from(row.updated.as_str())),
        );
        insert(
            "milestone",
            QVariant::from(&QString::from(row.milestone.as_str())),
        );
        let mut assignees = QList::<QVariant>::default();
        for assignee in &row.assignees {
            assignees.append(QVariant::from(&QString::from(assignee.as_str())));
        }
        insert("assignees", QVariant::from(&assignees));
        insert(
            "commentCount",
            QVariant::from(&i32::try_from(row.comment_count).unwrap_or(i32::MAX)),
        );
        insert("createdByMe", QVariant::from(&row.created_by_me));
        insert("assignedToMe", QVariant::from(&row.assigned_to_me));

        let mut labels = QList::<QVariant>::default();
        for row in &row.labels {
            let mut label = QMap::<QMapPair_QString_QVariant>::default();
            label.insert(
                QString::from("name"),
                QVariant::from(&QString::from(row.name.as_str())),
            );
            label.insert(
                QString::from("color"),
                QVariant::from(&QString::from(row.color.as_str())),
            );
            labels.append(QVariant::from(&label));
        }
        insert("labels", QVariant::from(&labels));
        issues.append(QVariant::from(&issue));
    }
    insert("rows", QVariant::from(&issues));
    QVariant::from(&state)
}

fn set_issues_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: IssuesStateRow) {
    let issues = to_issues(&row);
    backend.as_mut().rust_mut().get_mut().issues_state = Some(row);
    backend.as_mut().set_issues(issues);
}

#[derive(Debug)]
struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn read_bounded(mut stream: impl Read, limit: usize, exceeded: Arc<AtomicBool>) -> BoundedOutput {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            exceeded.store(true, Ordering::Release);
        }
    }
    BoundedOutput {
        bytes,
        exceeded: exceeded.load(Ordering::Acquire),
    }
}

fn terminate_github_process(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

const GITHUB_CLI_REMOVED_ENV: &[&str] = &[
    "GH_HOST",
    "GH_FORCE_TTY",
    "GH_DEBUG",
    "DEBUG",
    "GH_PAGER",
    "PAGER",
    "CLICOLOR_FORCE",
];

fn github_cli_command(executable: &Path, arguments: &[String]) -> Command {
    let mut command = Command::new(executable);
    for name in GITHUB_CLI_REMOVED_ENV {
        command.env_remove(name);
    }
    command
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

fn github_cli_output_with_executable(
    executable: &Path,
    arguments: &[String],
    cancellation: &harkness_git::Cancellation,
    deadline: Instant,
) -> Result<Vec<u8>, GitFailure> {
    if cancellation.is_cancelled() {
        return Err(GitFailure {
            kind: "cancelled".to_owned(),
            message: "GitHub issue loading was cancelled".to_owned(),
        });
    }
    let mut command = github_cli_command(executable, arguments);
    let mut child = command.spawn().map_err(|error| GitFailure {
        kind: "github_cli_missing".to_owned(),
        message: format!("Could not start GitHub CLI: {error}. Install gh and sign in."),
    })?;
    let stdout = child.stdout.take().expect("piped GitHub stdout");
    let stderr = child.stderr.take().expect("piped GitHub stderr");
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_flag = stdout_exceeded.clone();
    let stderr_flag = stderr_exceeded.clone();
    let stdout_reader =
        thread::spawn(move || read_bounded(stdout, GITHUB_OUTPUT_LIMIT, stdout_flag));
    let stderr_reader =
        thread::spawn(move || read_bounded(stderr, GITHUB_ERROR_LIMIT, stderr_flag));

    let status = loop {
        if cancellation.is_cancelled() {
            terminate_github_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(GitFailure {
                kind: "cancelled".to_owned(),
                message: "GitHub issue loading was cancelled".to_owned(),
            });
        }
        if Instant::now() >= deadline {
            terminate_github_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(GitFailure {
                kind: "github_timeout".to_owned(),
                message: format!(
                    "GitHub did not answer within {} seconds",
                    GITHUB_REQUEST_TIMEOUT.as_secs()
                ),
            });
        }
        if stdout_exceeded.load(Ordering::Acquire) || stderr_exceeded.load(Ordering::Acquire) {
            terminate_github_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(GitFailure {
                kind: "github_output_too_large".to_owned(),
                message: "GitHub returned more data than the issue loader accepts".to_owned(),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_github_process(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(GitFailure {
                    kind: "github_api".to_owned(),
                    message: format!("Could not wait for GitHub CLI: {error}"),
                });
            }
        }
        thread::sleep(GITHUB_POLL_INTERVAL);
    };
    // A descendant can outlive the direct child while retaining either pipe.
    // End the whole group before joining readers so output collection cannot
    // escape the cancellation and deadline loop above.
    terminate_github_process(&mut child);
    let stdout = stdout_reader.join().unwrap_or(BoundedOutput {
        bytes: Vec::new(),
        exceeded: true,
    });
    let stderr = stderr_reader.join().unwrap_or(BoundedOutput {
        bytes: Vec::new(),
        exceeded: true,
    });
    if stdout.exceeded || stderr.exceeded {
        return Err(GitFailure {
            kind: "github_output_too_large".to_owned(),
            message: "GitHub returned more data than the issue loader accepts".to_owned(),
        });
    }
    if status.success() {
        return Ok(stdout.bytes);
    }
    let detail = String::from_utf8_lossy(&stderr.bytes).trim().to_owned();
    let detail = if detail.is_empty() {
        format!("GitHub CLI exited with {status}")
    } else {
        detail
    };
    Err(GitFailure {
        kind: "github_api".to_owned(),
        message: format!("Could not load GitHub issues: {detail}"),
    })
}

fn github_graphql_arguments(owner: &str, name: &str, cursor: Option<&str>) -> Vec<String> {
    let mut arguments = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "--hostname".to_owned(),
        GITHUB_HOST.to_owned(),
        "-f".to_owned(),
        format!("query={GITHUB_ISSUES_QUERY}"),
        "-f".to_owned(),
        format!("owner={owner}"),
        "-f".to_owned(),
        format!("name={name}"),
    ];
    if let Some(cursor) = cursor {
        arguments.extend(["-f".to_owned(), format!("endCursor={cursor}")]);
    }
    arguments
}

fn load_github_issue_page(
    remote: &str,
    cursor: Option<&str>,
    loaded_count: usize,
    cancellation: &harkness_git::Cancellation,
) -> Result<GithubIssuePage, GitFailure> {
    let Some(slug) = remote.strip_prefix("github.com/") else {
        return Err(GitFailure {
            kind: "unsupported_remote".to_owned(),
            message: "Issues require a GitHub repository remote".to_owned(),
        });
    };
    let mut parts = slug.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(GitFailure {
            kind: "unsupported_remote".to_owned(),
            message: "The GitHub repository identity is invalid".to_owned(),
        });
    };
    if owner.is_empty() || name.is_empty() {
        return Err(GitFailure {
            kind: "unsupported_remote".to_owned(),
            message: "The GitHub repository identity is invalid".to_owned(),
        });
    }
    let arguments = github_graphql_arguments(owner, name, cursor);
    let bytes = github_cli_output_with_executable(
        Path::new("gh"),
        &arguments,
        cancellation,
        Instant::now() + GITHUB_REQUEST_TIMEOUT,
    )?;
    parse_github_issues(&bytes, loaded_count)
}

fn parse_github_issues(bytes: &[u8], loaded_count: usize) -> Result<GithubIssuePage, GitFailure> {
    let response =
        serde_json::from_slice::<GithubGraphqlResponse>(bytes).map_err(|error| GitFailure {
            kind: "github_api".to_owned(),
            message: format!("GitHub returned invalid issue data: {error}"),
        })?;
    if !response.errors.is_empty() {
        return Err(GitFailure {
            kind: "github_api".to_owned(),
            message: format!(
                "Could not load GitHub issues: {}",
                response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    let data = response.data.ok_or_else(|| GitFailure {
        kind: "github_api".to_owned(),
        message: "GitHub returned no issue data".to_owned(),
    })?;
    let repository = data.repository.ok_or_else(|| GitFailure {
        kind: "github_api".to_owned(),
        message: "GitHub could not find this repository".to_owned(),
    })?;
    let viewer = data.viewer.login;
    let connection = repository.issues;
    let remaining = GITHUB_MAX_ISSUES.saturating_sub(loaded_count);
    let mut rows = connection
        .nodes
        .into_iter()
        .map(|issue| {
            let author = issue.author.map(|author| author.login).unwrap_or_default();
            let created_by_me = author.eq_ignore_ascii_case(&viewer);
            let assigned_to_me = issue
                .assignees
                .nodes
                .iter()
                .any(|user| user.login.eq_ignore_ascii_case(&viewer));
            IssueRow {
                id: issue.id,
                number: issue.number,
                title: issue.title,
                state: issue.state.to_ascii_lowercase(),
                url: issue.url,
                author,
                updated: issue.updated_at,
                labels: issue
                    .labels
                    .nodes
                    .into_iter()
                    .map(|label| IssueLabelRow {
                        name: label.name,
                        color: format!("#{}", label.color),
                    })
                    .collect(),
                milestone: issue
                    .milestone
                    .map(|milestone| milestone.title)
                    .unwrap_or_default(),
                assignees: issue
                    .assignees
                    .nodes
                    .into_iter()
                    .map(|user| format!("@{}", user.login))
                    .collect(),
                comment_count: issue.comments.total_count,
                created_by_me,
                assigned_to_me,
            }
        })
        .take(remaining)
        .collect::<Vec<_>>();
    let loaded_after_page = loaded_count.saturating_add(rows.len());
    let limit_reached =
        connection.page_info.has_next_page && loaded_after_page >= GITHUB_MAX_ISSUES;
    let next_cursor = (connection.page_info.has_next_page && !limit_reached)
        .then_some(connection.page_info.end_cursor)
        .flatten();
    rows.shrink_to_fit();
    Ok(GithubIssuePage {
        viewer,
        rows,
        next_cursor,
        total_count: connection.total_count,
        limit_reached,
    })
}

fn clear_issues_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let jobs_changed = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.next_issues_request += 1;
        rust.issues_state = None;
        cancel_issue_jobs(&mut rust.job_records, &mut rust.cancellations)
    };
    if jobs_changed {
        sync_jobs(backend.as_mut());
    }
    backend.as_mut().set_issues(empty_issues());
}

fn cancel_issue_jobs(
    jobs: &mut Vec<JobRecord>,
    cancellations: &mut HashMap<String, harkness_git::Cancellation>,
) -> bool {
    let issue_jobs = jobs
        .iter()
        .filter(|job| job.kind == "issues")
        .map(|job| job.id.clone())
        .collect::<Vec<_>>();
    for job_id in &issue_jobs {
        if let Some(cancellation) = cancellations.remove(job_id) {
            cancellation.cancel();
        }
        end_job(jobs, job_id);
    }
    !issue_jobs.is_empty()
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
    discard_snapshot: Option<harkness_git::DiscardSnapshot>,
    hunks: Vec<ReviewHunkState>,
    total_lines: Option<u32>,
    row_offset: usize,
    // Mutation refreshes can anchor a hunk at an arbitrary global row. Keep
    // that page-grid origin so paging toward zero ends at the old boundary
    // instead of producing a large overlapping first page.
    row_page_origin: usize,
}

/// Where a reloaded review should resume, so a refresh does not throw the
/// reader back to the top of a long diff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ReviewLaunchPosition {
    row_offset: usize,
    row_page_origin: usize,
}

/// What produced one file, reduced to what a row has to draw.
///
/// `group` is the point of the record: it is an index over the review's
/// *distinct* producer sets, so two files that came from the same hands share
/// one number and a reader can tell them apart from the rest without comparing
/// names. `None` is the unknown case, which renders as unknown and never as
/// blank.
#[derive(Clone, Debug, Default)]
struct ReviewAttribution {
    group: Option<usize>,
    /// Producer names joined for display, empty when nothing was attributed.
    label: String,
    /// The named reason there is no attribution, empty when there is one.
    gap: String,
    commits: usize,
    /// How many distinct identities [`Self::label`] names. Carried as a number
    /// so a surface that must not render an untrusted name can still say how
    /// many there were.
    producers: usize,
}

/// What a review says about where its files came from.
///
/// Attribution is advisory, so this is filled in on a best-effort basis: a
/// failed or unavailable resolution leaves it unresolved and costs the reader
/// a label rather than the review.
#[derive(Clone, Debug, Default)]
struct ReviewProvenance {
    resolved: bool,
    agent_slug: String,
    head_revision: String,
    commits: usize,
    producers: usize,
    groups: usize,
    skipped_merges: usize,
    /// The named truncation, empty when the whole range was walked.
    truncation: String,
    /// One entry per file in [`ReviewStateRow::files`], in the same order.
    files: Vec<ReviewAttribution>,
}

impl ReviewProvenance {
    fn file(&self, index: usize) -> ReviewAttribution {
        self.files.get(index).cloned().unwrap_or_default()
    }
}

/// Folds every run of whitespace into one space and trims the ends.
///
/// Applied to producer names for the same reason `harkness-cli` applies it to
/// them: they come out of commit objects, which is repository content, and a
/// tab or a newline inside one decides how a row is laid out rather than what
/// it says.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What to call one producer on a row.
///
/// A `Co-Authored-By` trailer may carry an address and no name — `<model@host>`
/// is a spelling Git itself accepts — and a producer with an empty name would
/// otherwise join into a dangling separator, or, where it is a file's only
/// producer, into an empty label that the surface would then read back as
/// *unattributed*. The address is what the record actually has, so it is what
/// gets shown.
fn producer_display_name(producer: &harkness_git::Producer) -> String {
    let name = collapse_whitespace(&String::from_utf8_lossy(&producer.name));
    if !name.is_empty() {
        return name;
    }
    collapse_whitespace(&String::from_utf8_lossy(&producer.email))
}

#[derive(Clone, Debug)]
struct ReviewStateRow {
    project_id: String,
    /// What this surface was asked to show, kept so the whitespace control can
    /// re-request the same comparison without the QML side having to remember
    /// which of four review entry points opened it.
    selection: ReviewSelection,
    /// The whitespace handling every diff in this row was computed under.
    ///
    /// Anything but [`harkness_git::Whitespace::EXACT`] makes the surface
    /// view-only, which is why the one mutation it offers — hunk discard —
    /// routes through [`harkness_git::remap_to_exact`] rather than building a
    /// selection from what is on screen.
    whitespace: harkness_git::Whitespace,
    target: Option<ReviewTargetRecord>,
    title: String,
    detail: String,
    files: Vec<ReviewFileEntry>,
    /// What produced each of [`Self::files`]. Advisory throughout: nothing on
    /// this surface may act differently because of what it says.
    provenance: ReviewProvenance,
    file_offset: usize,
    selected_file_id: String,
    loaded_file: Option<ReviewLoadedFile>,
    loading: bool,
    file_loading: bool,
    error: String,
    error_kind: String,
}

impl ReviewStateRow {
    fn loading(
        project_id: String,
        selection: ReviewSelection,
        whitespace: harkness_git::Whitespace,
        title: String,
        detail: String,
    ) -> Self {
        Self {
            project_id,
            selection,
            whitespace,
            target: None,
            title,
            detail,
            files: Vec::new(),
            provenance: ReviewProvenance::default(),
            file_offset: 0,
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

/// Only an unstaged diff reads its new side from the same working-tree file
/// that an editor will open. Every other target is pinned to the index or a
/// commit and may therefore disagree with the checkout by the time it opens.
fn working_tree_may_differ(target: &harkness_git::DiffTarget) -> bool {
    !matches!(target, harkness_git::DiffTarget::Unstaged)
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
    whitespace: harkness_git::Whitespace,
    generation: u64,
) -> Result<ReviewStateRow, GitFailure> {
    let target = prepare_review_target(git, selection.clone())?;
    // A zero content budget asks the Git service for the complete identity list while
    // intentionally omitting every hunk. Opening a path makes the second,
    // path-restricted request below, so a thousand-file review never eagerly
    // builds a thousand line models.
    let options = harkness_git::DiffOptions::default()
        .with_max_total_bytes(0)
        .with_whitespace(whitespace);
    let files: Vec<ReviewFileEntry> = git
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
    let provenance = resolve_review_provenance(git, &target.target, &selection, &files);
    Ok(ReviewStateRow {
        project_id,
        selection,
        whitespace,
        title: target.title.clone(),
        detail: target.detail.clone(),
        target: Some(target),
        files,
        provenance,
        file_offset: 0,
        selected_file_id: String::new(),
        loaded_file: None,
        loading: false,
        file_loading: false,
        error: String::new(),
        error_kind: String::new(),
    })
}

/// Attributes the review's file list to the commits in its own range.
///
/// One walk of the range serves the whole list, which is what keeps opening a
/// thousand-file review the same cost as opening a one-file one. The result is
/// advisory in the strongest sense: a failure is swallowed into an unresolved
/// record rather than turned into a review that will not open, because a
/// missing label is a cosmetic loss and a missing diff is not.
fn resolve_review_provenance(
    git: &harkness_git::GitService,
    target: &harkness_git::DiffTarget,
    selection: &ReviewSelection,
    files: &[ReviewFileEntry],
) -> ReviewProvenance {
    // An empty file list is passed through rather than short-circuited: asking
    // about no paths is answered without a walk, and the result still reports
    // itself resolved. `resolved: false` is reserved for an attribution that
    // could not be made, which is a different thing from one with nothing in it.
    let mut options = harkness_git::ProvenanceOptions::default()
        .with_paths(files.iter().map(|entry| entry.path.clone()))
        .with_max_commits(REVIEW_PROVENANCE_MAX_COMMITS);
    // A branch review is pinned to object ids so the comparison cannot move
    // under the reader, which leaves the target with nothing a reference
    // convention could be read off. The name the reviewer actually asked for
    // travels beside it for exactly that.
    if let ReviewSelection::Branch { branch, .. } = selection {
        options = options.with_head_reference(branch.as_str());
    }
    let Ok(record) = git.provenance(target, &options, &harkness_git::Cancellation::default())
    else {
        return ReviewProvenance::default();
    };

    // Files are grouped by the *set* of identities behind them rather than by
    // one name, because "the same hands produced these two files" is the
    // question the tint answers and two files sharing only their newest
    // committer are not the same answer.
    //
    // The key is sorted before it is compared, and the label is built from the
    // sorted key. `FileProvenance::producers` is ordered by first contribution
    // from the newest commit down, so one file can list Ada then Grace while
    // its neighbour lists Grace then Ada for the same two people — and an
    // order-sensitive key would give them two colours and two spellings of one
    // answer, which is precisely what the mark exists to prevent.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let attributions = record
        .files
        .iter()
        .map(|file| {
            if !file.is_attributed() {
                return ReviewAttribution {
                    gap: file
                        .gap
                        .map_or_else(String::new, |gap| gap.name().to_owned()),
                    ..ReviewAttribution::default()
                };
            }
            let mut key = file.producers.clone();
            key.sort_unstable();
            let group = groups
                .iter()
                .position(|existing| *existing == key)
                .unwrap_or_else(|| {
                    groups.push(key.clone());
                    groups.len() - 1
                });
            let label = key
                .iter()
                .map(|producer| producer_display_name(&record.producers[*producer]))
                .collect::<Vec<_>>()
                .join(", ");
            ReviewAttribution {
                group: Some(group),
                label,
                gap: String::new(),
                commits: file.commits.len(),
                producers: key.len(),
            }
        })
        .collect();

    ReviewProvenance {
        resolved: true,
        agent_slug: record
            .range
            .as_ref()
            .and_then(|range| range.agent_slug.clone())
            .unwrap_or_default(),
        head_revision: record
            .range
            .as_ref()
            .map(|range| range.head_revision.clone())
            .unwrap_or_default(),
        commits: record.commits.len(),
        producers: record.producers.len(),
        groups: groups.len(),
        skipped_merges: record.skipped_merges,
        truncation: record
            .truncation
            .map_or_else(String::new, |truncation| truncation.name().to_owned()),
        files: attributions,
    }
}

/// Selects the requested changed path, or the first one when no preference is
/// available. The metadata pass remains bounded and only that one selection
/// receives a path-restricted content request.
fn load_review_with_initial_file_with_git(
    git: &harkness_git::GitService,
    project_id: String,
    selection: ReviewSelection,
    whitespace: harkness_git::Whitespace,
    review_generation: u64,
    file_generation: u64,
    preferred_path: Option<&Path>,
) -> Result<ReviewStateRow, GitFailure> {
    let mut review =
        load_review_with_git(git, project_id, selection, whitespace, review_generation)?;
    let entry = preferred_path
        .and_then(|path| review.files.iter().position(|entry| entry.path == path))
        .or_else(|| (!review.files.is_empty()).then_some(0))
        .map(|index| (index, review.files[index].clone()));
    let Some((entry_index, entry)) = entry else {
        return Ok(review);
    };
    let target = review.target.as_ref().ok_or_else(|| GitFailure {
        kind: "review_target_missing".to_owned(),
        message: "The selected review target is no longer available".to_owned(),
    })?;
    let loaded = load_review_file_with_git(git, target, whitespace, &entry, file_generation)?;
    review.file_offset = entry_index / REVIEW_FILE_PAGE_SIZE * REVIEW_FILE_PAGE_SIZE;
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
    whitespace: harkness_git::Whitespace,
    entry: &ReviewFileEntry,
    generation: u64,
) -> Result<ReviewLoadedFile, GitFailure> {
    let options = harkness_git::DiffOptions::default()
        .with_paths([entry.path.as_path()])
        .with_whitespace(whitespace)
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
    let discard_paths = [file.old_path.as_ref(), file.new_path.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let discard_snapshot = review_file_discard_description(&file)
        .map(|_| git.discard_snapshot(discard_paths))
        .transpose()
        .map_err(GitFailure::from)?;
    Ok(ReviewLoadedFile {
        id: entry.id.clone(),
        file,
        discard_snapshot,
        hunks,
        total_lines,
        row_offset: 0,
        row_page_origin: 0,
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

/// Says what this hunk cannot show, and says it in full.
///
/// A degraded hunk has no paired lines, and the pair is what a changed line
/// ending is read from — so the chip that would name a CRLF turning into an LF
/// is absent here too, not merely the word emphasis. Reveal still marks every
/// terminator it is asked to, which is what the reader is pointed at.
fn hunk_degradation_summary(hunk: &harkness_git::Hunk) -> String {
    let limit = match hunk.intra_line_degradation.as_ref() {
        Some(harkness_git::IntraLineDegradation::LineTooLong { limit }) => {
            format!("a line exceeds the {limit}-byte pairing limit")
        }
        Some(harkness_git::IntraLineDegradation::PairingTooLarge { limit }) => {
            format!("pairing exceeds the {limit}-comparison limit")
        }
        Some(_) => "a named Git limit was reached".to_owned(),
        None => return String::new(),
    };
    format!(
        "Word emphasis unavailable — {limit}. A line-ending change is not \
         flagged here either; reveal whitespace to compare terminators."
    )
}

fn display_line_end(bytes: &[u8]) -> usize {
    let without_newline = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    without_newline
        .strip_suffix(b"\r")
        .unwrap_or(without_newline)
        .len()
}

/// The terminator `display_line_end` cuts away, named for QML.
///
/// The segment text stays free of the bytes — a renderer that had to carry
/// them would have to decide what a carriage return looks like in the middle
/// of a run — so the row says which ending it had instead. Without this a
/// CRLF-to-LF change reaches the surface as two identical-looking lines.
fn line_ending_name(bytes: &[u8]) -> &'static str {
    if bytes.ends_with(b"\r\n") {
        "crlf"
    } else if bytes.ends_with(b"\n") {
        "lf"
    } else if bytes.ends_with(b"\r") {
        "cr"
    } else {
        "none"
    }
}

/// Which whitespace byte a run is made of, when it is one run of one byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WhitespaceRun {
    /// The run is file content, whatever bytes it happens to hold.
    None,
    Space,
    Tab,
}

impl WhitespaceRun {
    fn name(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Space => "space",
            Self::Tab => "tab",
        }
    }
}

/// Where in the line a run sits, for the runs whose position is the point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineZone {
    Content,
    Leading,
    Trailing,
}

impl LineZone {
    fn name(self) -> &'static str {
        match self {
            Self::Content => "",
            Self::Leading => "leading",
            Self::Trailing => "trailing",
        }
    }
}

/// One run of a diff line as QML paints it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextSegment<'a> {
    text: &'a [u8],
    changed: bool,
    whitespace: WhitespaceRun,
    zone: LineZone,
}

/// The first content byte and the byte after the last one.
///
/// A line that is nothing but whitespace has no content byte, so it reports an
/// empty trailing boundary at zero: `zone_at` tests trailing first, which
/// makes the whole of such a line trailing whitespace. That is the reading a
/// reviewer wants — an all-blank line is the accident, not an indent.
fn whitespace_zones(display: &[u8]) -> (usize, usize) {
    let leading_end = display
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(display.len());
    let trailing_start = display
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(0, |index| index + 1);
    (leading_end, trailing_start)
}

/// How one byte is painted: where it sits, and whether it is whitespace the
/// reader is being shown rather than content they are reading.
fn run_at(
    display: &[u8],
    index: usize,
    leading_end: usize,
    trailing_start: usize,
) -> (LineZone, WhitespaceRun) {
    let zone = if index >= trailing_start {
        LineZone::Trailing
    } else if index < leading_end {
        LineZone::Leading
    } else {
        LineZone::Content
    };
    let whitespace = match (zone, display[index]) {
        (LineZone::Content, _) => WhitespaceRun::None,
        (_, b'\t') => WhitespaceRun::Tab,
        (_, _) => WhitespaceRun::Space,
    };
    (zone, whitespace)
}

/// Splits `display[start..stop]` into runs QML can paint separately.
///
/// Only the leading and trailing whitespace runs are cut out, one segment per
/// whitespace byte kind, so a tab is never handed over as the same run as the
/// spaces beside it. Whitespace *inside* the line stays in its content run on
/// purpose: the QML lexer reads a segment as a whole, and splitting every
/// interior space would break a string literal into unrecognisable halves. The
/// renderer reveals those bytes within the run instead.
///
/// Every cut lands on an ASCII space or tab, which is never a UTF-8
/// continuation byte, so this never turns valid content into replacement
/// characters that lossy decoding would not have produced anyway.
fn push_segments<'a>(
    display: &'a [u8],
    start: usize,
    stop: usize,
    changed: bool,
    leading_end: usize,
    trailing_start: usize,
    out: &mut Vec<TextSegment<'a>>,
) {
    let mut cursor = start;
    while cursor < stop {
        let (zone, whitespace) = run_at(display, cursor, leading_end, trailing_start);
        let mut end = cursor + 1;
        while end < stop && run_at(display, end, leading_end, trailing_start) == (zone, whitespace)
        {
            end += 1;
        }
        out.push(TextSegment {
            text: &display[cursor..end],
            changed,
            whitespace,
            zone,
        });
        cursor = end;
    }
}

/// The rendered runs of one diff line, in order, terminator excluded.
fn text_segments<'a>(
    bytes: &'a [u8],
    ranges: Option<&[harkness_git::IntraLineRange]>,
) -> Vec<TextSegment<'a>> {
    let end = display_line_end(bytes);
    let display = &bytes[..end];
    let (leading_end, trailing_start) = whitespace_zones(display);
    let mut segments = Vec::new();
    let Some(ranges) = ranges else {
        push_segments(
            display,
            0,
            end,
            false,
            leading_end,
            trailing_start,
            &mut segments,
        );
        return segments;
    };
    let mut cursor = 0;
    for range in ranges {
        let start = range.start.min(end).max(cursor);
        let range_end = range.end.min(end).max(start);
        push_segments(
            display,
            cursor,
            start,
            false,
            leading_end,
            trailing_start,
            &mut segments,
        );
        push_segments(
            display,
            start,
            range_end,
            true,
            leading_end,
            trailing_start,
            &mut segments,
        );
        cursor = range_end;
    }
    push_segments(
        display,
        cursor,
        end,
        false,
        leading_end,
        trailing_start,
        &mut segments,
    );
    segments
}

fn to_text_segments(
    bytes: &[u8],
    ranges: Option<&[harkness_git::IntraLineRange]>,
) -> QList<QVariant> {
    let mut segments = QList::<QVariant>::default();
    for segment in text_segments(bytes, ranges) {
        let mut value = QMap::<QMapPair_QString_QVariant>::default();
        value.insert(
            QString::from("text"),
            QVariant::from(&QString::from(
                String::from_utf8_lossy(segment.text).as_ref(),
            )),
        );
        value.insert(QString::from("changed"), QVariant::from(&segment.changed));
        value.insert(
            QString::from("whitespace"),
            QVariant::from(&QString::from(segment.whitespace.name())),
        );
        value.insert(
            QString::from("zone"),
            QVariant::from(&QString::from(segment.zone.name())),
        );
        segments.append(QVariant::from(&value));
    }
    segments
}

fn empty_review_side() -> QVariant {
    let mut side = QMap::<QMapPair_QString_QVariant>::default();
    side.insert(QString::from("present"), QVariant::from(&false));
    QVariant::from(&side)
}

/// Whether this row is Git's own annotation rather than a line of the file.
///
/// `\ No newline at end of file` arrives as a `DiffLine` like any other, and
/// its `content` is the sentence libgit2 wrote plus newlines that exist
/// nowhere on disk. It is shown, because it is what tells the reader the file
/// is unterminated — but it has no terminator to name and nothing to copy, and
/// treating its bytes as content would put that sentence on the clipboard as
/// though the file contained it.
fn is_eof_marker(kind: harkness_git::DiffLineKind) -> bool {
    matches!(
        kind,
        harkness_git::DiffLineKind::BothEofNoNewline
            | harkness_git::DiffLineKind::OldEofNoNewline
            | harkness_git::DiffLineKind::NewEofNoNewline
    )
}

/// The line exactly as it exists, for the clipboard.
///
/// What the reader copies is the content bytes, never the glyphs a revealing
/// renderer drew over them, so the copy carries the terminator the row only
/// names.
fn to_copy_text(bytes: &[u8]) -> QVariant {
    QVariant::from(&QString::from(String::from_utf8_lossy(bytes).as_ref()))
}

/// As `to_copy_text`, except that an annotation row copies nothing.
fn diff_line_copy_text(line: &harkness_git::DiffLine) -> QVariant {
    if is_eof_marker(line.kind) {
        return to_copy_text(b"");
    }
    to_copy_text(&line.content)
}

/// The terminator to name on a row, which an annotation row does not have.
fn diff_line_ending(line: &harkness_git::DiffLine) -> &'static str {
    if is_eof_marker(line.kind) {
        "none"
    } else {
        line_ending_name(&line.content)
    }
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
    side.insert(
        QString::from("lineEnd"),
        QVariant::from(&QString::from(diff_line_ending(line))),
    );
    side.insert(QString::from("copyText"), diff_line_copy_text(line));
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
    value.insert(
        QString::from("lineEnd"),
        QVariant::from(&QString::from(diff_line_ending(line))),
    );
    value.insert(QString::from("copyText"), diff_line_copy_text(line));
    QVariant::from(&value)
}

fn to_review_line_row(hunk_id: &str, hunk: &harkness_git::Hunk, index: usize) -> QVariant {
    let line = &hunk.lines[index];
    let partner = line
        .paired_line_index
        .and_then(|partner| hunk.lines.get(partner));
    let split_hidden = matches!(line.kind, harkness_git::DiffLineKind::Addition)
        && partner
            .is_some_and(|partner| matches!(partner.kind, harkness_git::DiffLineKind::Deletion));
    let (old, new) = match line.kind {
        harkness_git::DiffLineKind::Deletion => (
            to_review_side(line, line.old_line_number),
            partner.map_or_else(empty_review_side, |partner| {
                to_review_side(partner, partner.new_line_number)
            }),
        ),
        harkness_git::DiffLineKind::Addition if split_hidden => {
            (empty_review_side(), empty_review_side())
        }
        harkness_git::DiffLineKind::Addition => (
            empty_review_side(),
            to_review_side(line, line.new_line_number),
        ),
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
    row.insert(
        QString::from("hunkId"),
        QVariant::from(&QString::from(hunk_id)),
    );
    row.insert(QString::from("unified"), to_unified_review_line(line));
    row.insert(QString::from("old"), old);
    row.insert(QString::from("new"), new);
    row.insert(
        QString::from("openLine"),
        QVariant::from(&bounded_i32(best_working_tree_line(hunk, index))),
    );
    row.insert(QString::from("splitHidden"), QVariant::from(&split_hidden));
    // A line whose only change is its terminator has no changed byte inside
    // the segments — the ranges clamp to nothing — so the pair reports the
    // difference here or it is never shown at all.
    row.insert(
        QString::from("lineEndChanged"),
        QVariant::from(&partner.is_some_and(|partner| {
            line_ending_name(&partner.content) != line_ending_name(&line.content)
        })),
    );
    QVariant::from(&row)
}

/// Maps a rendered diff row to the file that exists in the working tree.
/// Deletions have no new-side coordinate, so prefer their paired replacement,
/// then the following new-side line, then the preceding neighbourhood.
fn best_working_tree_line(hunk: &harkness_git::Hunk, index: usize) -> u32 {
    let line = &hunk.lines[index];
    if let Some(number) = line.new_line_number {
        return number.max(1);
    }
    if let Some(number) = line
        .paired_line_index
        .and_then(|partner| hunk.lines.get(partner))
        .and_then(|partner| partner.new_line_number)
    {
        return number.max(1);
    }
    hunk.lines[index + 1..]
        .iter()
        .find_map(|candidate| candidate.new_line_number)
        .or_else(|| {
            hunk.lines[..index]
                .iter()
                .rev()
                .find_map(|candidate| candidate.new_line_number)
        })
        .unwrap_or(hunk.new_start)
        .max(1)
}

fn first_working_tree_line(file: &harkness_git::FileDiff) -> u32 {
    file.hunks
        .iter()
        .find_map(|hunk| (!hunk.lines.is_empty()).then(|| best_working_tree_line(hunk, 0)))
        .unwrap_or(1)
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
    side.insert(
        QString::from("lineEnd"),
        QVariant::from(&QString::from(line_ending_name(content))),
    );
    side.insert(QString::from("copyText"), to_copy_text(content));
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
    unified.insert(
        QString::from("lineEnd"),
        QVariant::from(&QString::from(line_ending_name(&line.content))),
    );
    unified.insert(QString::from("copyText"), to_copy_text(&line.content));

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
    row.insert(
        QString::from("openLine"),
        QVariant::from(&bounded_i32(line.new_line.max(1))),
    );
    row.insert(QString::from("splitHidden"), QVariant::from(&false));
    // Expanded context is one line read from one side; there is no pair whose
    // terminators could disagree.
    row.insert(QString::from("lineEndChanged"), QVariant::from(&false));
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

fn review_hunk_row(
    hunk_id: &str,
    hunk: &harkness_git::Hunk,
    file: &harkness_git::FileDiff,
) -> QVariant {
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
    let discard = matches!(file.target, harkness_git::DiffTarget::Unstaged)
        .then(|| match file.change {
            harkness_git::FileChange::Untracked | harkness_git::FileChange::Unmerged => None,
            _ => Some(harkness_git::DiscardDescription::restore_hunks(
                [review_path(file)],
                1,
            )),
        })
        .flatten();
    row.insert(
        QString::from("discard"),
        to_discard_description(discard.as_ref()),
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

fn review_row_count(loaded: &ReviewLoadedFile) -> usize {
    loaded
        .file
        .hunks
        .iter()
        .zip(&loaded.hunks)
        .enumerate()
        .fold(0usize, |count, (index, (hunk, state))| {
            let remaining_before = hidden_before(loaded, index)
                .saturating_sub(u32::try_from(state.before.len()).unwrap_or(u32::MAX));
            let before = state.before.len() + usize::from(remaining_before > 0);
            let after = if index + 1 == loaded.file.hunks.len() {
                let remaining_after = hidden_after(loaded, index)
                    .saturating_sub(u32::try_from(state.after.len()).unwrap_or(u32::MAX));
                state.after.len() + usize::from(remaining_after > 0)
            } else {
                0
            };
            count
                .saturating_add(before)
                .saturating_add(1)
                .saturating_add(hunk.lines.len())
                .saturating_add(after)
        })
}

fn discard_review_file(file: Option<ReviewLoadedFile>) {
    if file
        .as_ref()
        .is_some_and(|loaded| review_row_count(loaded) > REVIEW_ROW_PAGE_SIZE)
    {
        std::thread::spawn(move || drop(file));
    }
}

fn discard_review_state(state: Option<ReviewStateRow>) {
    if state
        .as_ref()
        .and_then(|state| state.loaded_file.as_ref())
        .is_some_and(|loaded| review_row_count(loaded) > REVIEW_ROW_PAGE_SIZE)
    {
        std::thread::spawn(move || drop(state));
    }
}

fn normalized_review_row_offset(loaded: &ReviewLoadedFile) -> usize {
    let total = review_row_count(loaded);
    if total == 0 {
        return 0;
    }
    let origin = loaded.row_page_origin.min(total);
    let requested = loaded.row_offset.min(total - 1);
    if requested < origin {
        0
    } else {
        origin.saturating_add(
            requested
                .saturating_sub(origin)
                .div_euclid(REVIEW_ROW_PAGE_SIZE)
                .saturating_mul(REVIEW_ROW_PAGE_SIZE),
        )
    }
}

fn review_row_window(loaded: &ReviewLoadedFile) -> (usize, usize, usize) {
    let total = review_row_count(loaded);
    let start = normalized_review_row_offset(loaded);
    let origin = loaded.row_page_origin.min(total);
    let end = if start == 0 && origin > 0 {
        origin
    } else {
        start.saturating_add(REVIEW_ROW_PAGE_SIZE).min(total)
    };
    (start, end, total)
}

fn visit_review_hunk_rows(
    loaded: &ReviewLoadedFile,
    mut visit: impl FnMut(usize, usize, &ReviewHunkState, &harkness_git::Hunk) -> bool,
) {
    let mut row_index = 0usize;
    for (index, (hunk, state)) in loaded.file.hunks.iter().zip(&loaded.hunks).enumerate() {
        let remaining_before = hidden_before(loaded, index)
            .saturating_sub(u32::try_from(state.before.len()).unwrap_or(u32::MAX));
        row_index = row_index
            .saturating_add(usize::from(remaining_before > 0))
            .saturating_add(state.before.len());
        if !visit(index, row_index, state, hunk) {
            return;
        }
        row_index = row_index.saturating_add(1).saturating_add(hunk.lines.len());
        if index + 1 == loaded.file.hunks.len() {
            let remaining_after = hidden_after(loaded, index)
                .saturating_sub(u32::try_from(state.after.len()).unwrap_or(u32::MAX));
            row_index = row_index
                .saturating_add(state.after.len())
                .saturating_add(usize::from(remaining_after > 0));
        }
    }
}

fn review_hunk_exists_where(loaded: &ReviewLoadedFile, predicate: impl Fn(usize) -> bool) -> bool {
    let mut found = false;
    visit_review_hunk_rows(loaded, |_, row_index, _, _| {
        if predicate(row_index) {
            found = true;
            return false;
        }
        true
    });
    found
}

fn review_page_row(direction: &str, remaining: usize, hunk_available: bool) -> QVariant {
    let mut row = QMap::<QMapPair_QString_QVariant>::default();
    row.insert(
        QString::from("type"),
        QVariant::from(&QString::from("page")),
    );
    row.insert(
        QString::from("direction"),
        QVariant::from(&QString::from(direction)),
    );
    row.insert(
        QString::from("count"),
        QVariant::from(&i32::try_from(remaining).unwrap_or(i32::MAX)),
    );
    row.insert(
        QString::from("hunkAvailable"),
        QVariant::from(&hunk_available),
    );
    QVariant::from(&row)
}

fn advance_review_row_window(loaded: &mut ReviewLoadedFile) -> bool {
    let (_, end, total) = review_row_window(loaded);
    if end >= total {
        return false;
    }
    loaded.row_offset = end;
    true
}

fn retreat_review_row_window(loaded: &mut ReviewLoadedFile) -> bool {
    let start = normalized_review_row_offset(loaded);
    if start == 0 {
        return false;
    }
    let origin = loaded.row_page_origin.min(review_row_count(loaded));
    loaded.row_offset = if start <= origin {
        0
    } else {
        start.saturating_sub(REVIEW_ROW_PAGE_SIZE).max(origin)
    };
    true
}

fn append_review_row(
    rows: &mut QList<QVariant>,
    row_index: &mut usize,
    start: usize,
    end: usize,
    make_row: impl FnOnce() -> QVariant,
) -> bool {
    if *row_index >= end {
        return false;
    }
    if *row_index >= start {
        rows.append(make_row());
    }
    *row_index = (*row_index).saturating_add(1);
    *row_index < end
}

fn append_review_row_slice<T>(
    rows: &mut QList<QVariant>,
    row_index: &mut usize,
    start: usize,
    end: usize,
    values: &[T],
    mut make_row: impl FnMut(usize, &T) -> QVariant,
) -> bool {
    let segment_start = *row_index;
    let segment_end = segment_start.saturating_add(values.len());
    if segment_start >= end {
        return false;
    }
    if segment_end > start {
        let first = start.saturating_sub(segment_start).min(values.len());
        let last = end.saturating_sub(segment_start).min(values.len());
        for (relative_index, value) in values[first..last].iter().enumerate() {
            let value_index = first + relative_index;
            rows.append(make_row(value_index, value));
        }
    }
    *row_index = segment_end;
    *row_index < end
}

fn review_rows(loaded: &ReviewLoadedFile) -> QList<QVariant> {
    debug_assert_eq!(loaded.hunks.len(), loaded.file.hunks.len());
    let mut rows = QList::<QVariant>::default();
    let (start, end, total) = review_row_window(loaded);
    if start > 0 {
        rows.append(review_page_row(
            "previous",
            start,
            review_hunk_exists_where(loaded, |row_index| row_index < start),
        ));
    }
    let mut row_index = 0usize;
    'all_rows: for (index, (hunk, state)) in loaded.file.hunks.iter().zip(&loaded.hunks).enumerate()
    {
        let remaining_before = hidden_before(loaded, index)
            .saturating_sub(u32::try_from(state.before.len()).unwrap_or(u32::MAX));
        if remaining_before > 0
            && !append_review_row(&mut rows, &mut row_index, start, end, || {
                collapsed_review_row(&state.id, "before", remaining_before)
            })
        {
            break 'all_rows;
        }
        if !append_review_row_slice(
            &mut rows,
            &mut row_index,
            start,
            end,
            &state.before,
            |_, line| to_context_row(line),
        ) {
            break 'all_rows;
        }
        if !append_review_row(&mut rows, &mut row_index, start, end, || {
            review_hunk_row(&state.id, hunk, &loaded.file)
        }) {
            break 'all_rows;
        }
        if !append_review_row_slice(
            &mut rows,
            &mut row_index,
            start,
            end,
            &hunk.lines,
            |line_index, _| to_review_line_row(&state.id, hunk, line_index),
        ) {
            break 'all_rows;
        }
        if index + 1 == loaded.file.hunks.len() {
            if !append_review_row_slice(
                &mut rows,
                &mut row_index,
                start,
                end,
                &state.after,
                |_, line| to_context_row(line),
            ) {
                break 'all_rows;
            }
            let remaining_after = hidden_after(loaded, index)
                .saturating_sub(u32::try_from(state.after.len()).unwrap_or(u32::MAX));
            if remaining_after > 0
                && !append_review_row(&mut rows, &mut row_index, start, end, || {
                    collapsed_review_row(&state.id, "after", remaining_after)
                })
            {
                break 'all_rows;
            }
        }
    }
    if end < total {
        rows.append(review_page_row(
            "next",
            total - end,
            review_hunk_exists_where(loaded, |row_index| row_index >= end),
        ));
    }
    rows
}

fn review_file_window(row: &ReviewStateRow) -> (usize, usize, usize) {
    let total = row.files.len();
    if total == 0 {
        return (0, 0, 0);
    }
    let last_page = (total - 1) / REVIEW_FILE_PAGE_SIZE * REVIEW_FILE_PAGE_SIZE;
    let start = (row.file_offset / REVIEW_FILE_PAGE_SIZE * REVIEW_FILE_PAGE_SIZE).min(last_page);
    let end = start.saturating_add(REVIEW_FILE_PAGE_SIZE).min(total);
    (start, end, total)
}

fn advance_review_file_window(row: &mut ReviewStateRow) -> bool {
    let (start, end, total) = review_file_window(row);
    if end >= total {
        return false;
    }
    row.file_offset = start.saturating_add(REVIEW_FILE_PAGE_SIZE);
    true
}

fn retreat_review_file_window(row: &mut ReviewStateRow) -> bool {
    let (start, _, _) = review_file_window(row);
    if start == 0 {
        return false;
    }
    row.file_offset = start.saturating_sub(REVIEW_FILE_PAGE_SIZE);
    true
}

/// The review-wide attribution header.
///
/// `resolved` is separate from every count on purpose: a review with no
/// attribution and a review whose attribution could not be resolved are
/// different answers, and neither may render as the other.
fn to_review_provenance(provenance: &ReviewProvenance) -> QVariant {
    let mut value = QMap::<QMapPair_QString_QVariant>::default();
    value.insert(
        QString::from("resolved"),
        QVariant::from(&provenance.resolved),
    );
    value.insert(
        QString::from("agentSlug"),
        QVariant::from(&QString::from(provenance.agent_slug.as_str())),
    );
    value.insert(
        QString::from("headRevision"),
        QVariant::from(&QString::from(provenance.head_revision.as_str())),
    );
    value.insert(
        QString::from("commitCount"),
        QVariant::from(&bounded_usize(provenance.commits)),
    );
    value.insert(
        QString::from("producerCount"),
        QVariant::from(&bounded_usize(provenance.producers)),
    );
    value.insert(
        QString::from("groupCount"),
        QVariant::from(&bounded_usize(provenance.groups)),
    );
    value.insert(
        QString::from("skippedMerges"),
        QVariant::from(&bounded_usize(provenance.skipped_merges)),
    );
    value.insert(
        QString::from("truncation"),
        QVariant::from(&QString::from(provenance.truncation.as_str())),
    );
    QVariant::from(&value)
}

/// One file's attribution, on the row and on the open file's header alike.
///
/// `provenanceGroup` is `-1` rather than absent when nothing is known, so a
/// delegate reads one field and gets an answer instead of having to tell a
/// missing key from a real group.
fn insert_attribution(
    value: &mut QMap<QMapPair_QString_QVariant>,
    attribution: &ReviewAttribution,
) {
    value.insert(
        QString::from("provenanceGroup"),
        QVariant::from(&attribution.group.map_or(-1, bounded_usize)),
    );
    value.insert(
        QString::from("provenanceLabel"),
        QVariant::from(&QString::from(attribution.label.as_str())),
    );
    value.insert(
        QString::from("provenanceGap"),
        QVariant::from(&QString::from(attribution.gap.as_str())),
    );
    value.insert(
        QString::from("provenanceCommits"),
        QVariant::from(&bounded_usize(attribution.commits)),
    );
    value.insert(
        QString::from("provenanceProducers"),
        QVariant::from(&bounded_usize(attribution.producers)),
    );
}

fn bounded_usize(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn to_review(row: &ReviewStateRow, loaded_path_id: &str) -> QVariant {
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
    let commit_id = row
        .target
        .as_ref()
        .and_then(|target| match &target.target {
            harkness_git::DiffTarget::Commit { revision, .. } => Some(revision.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    insert("commitId", QVariant::from(&QString::from(commit_id)));
    let (check_target_kind, check_target_head) =
        row.target
            .as_ref()
            .map_or(("unavailable", ""), |target| match &target.target {
                harkness_git::DiffTarget::Staged => ("index", ""),
                harkness_git::DiffTarget::Unstaged => ("worktree", ""),
                harkness_git::DiffTarget::Commit { revision, .. } => ("commit", revision.as_str()),
                harkness_git::DiffTarget::Revisions { new_revision, .. } => {
                    ("commit", new_revision.as_str())
                }
                _ => ("unsupported", ""),
            });
    insert(
        "checkTargetKind",
        QVariant::from(&QString::from(check_target_kind)),
    );
    insert(
        "checkTargetHead",
        QVariant::from(&QString::from(check_target_head)),
    );

    let (file_start, file_end, file_total) = review_file_window(row);
    insert(
        "fileOffset",
        QVariant::from(&i32::try_from(file_start).unwrap_or(i32::MAX)),
    );
    insert(
        "totalFiles",
        QVariant::from(&i32::try_from(file_total).unwrap_or(i32::MAX)),
    );
    insert(
        "whitespace",
        QVariant::from(&QString::from(row.whitespace.mode.name())),
    );
    insert(
        "ignoreBlankLines",
        QVariant::from(&row.whitespace.ignore_blank_lines),
    );
    // The one thing the control has to tell the reader beyond its own value:
    // while this is false the surface is a view, and its hunk actions can only
    // proceed by recomputing the file exactly first.
    insert("appliable", QVariant::from(&row.whitespace.is_exact()));
    insert("provenance", to_review_provenance(&row.provenance));
    let mut files = QList::<QVariant>::default();
    for (offset, entry) in row.files[file_start..file_end].iter().enumerate() {
        let mut value = QMap::<QMapPair_QString_QVariant>::default();
        insert_attribution(&mut value, &row.provenance.file(file_start + offset));
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
            // The open file carries the same attribution its row does, so the
            // header answers "what produced this" without the reader having to
            // look back at the list they just left.
            let attribution = row
                .files
                .iter()
                .position(|entry| entry.id == loaded.id)
                .map_or_else(ReviewAttribution::default, |index| {
                    row.provenance.file(index)
                });
            insert_attribution(&mut value, &attribution);
            value.insert(
                QString::from("fileId"),
                QVariant::from(&QString::from(loaded.id.as_str())),
            );
            value.insert(
                QString::from("path"),
                QVariant::from(&QString::from(display_diff_path(&loaded.file).as_str())),
            );
            value.insert(
                QString::from("pathId"),
                QVariant::from(&QString::from(loaded_path_id)),
            );
            value.insert(
                QString::from("summary"),
                QVariant::from(&QString::from(
                    review_content_summary(&loaded.file).as_str(),
                )),
            );
            value.insert(
                QString::from("firstLine"),
                QVariant::from(&bounded_i32(first_working_tree_line(&loaded.file))),
            );
            value.insert(QString::from("binary"), QVariant::from(&loaded.file.binary));
            let discard = review_file_discard_description(&loaded.file);
            value.insert(
                QString::from("discard"),
                to_discard_description(discard.as_ref()),
            );
            value.insert(
                QString::from("hunkCount"),
                QVariant::from(&i32::try_from(loaded.file.hunks.len()).unwrap_or(i32::MAX)),
            );
            value.insert(
                QString::from("totalRows"),
                QVariant::from(&i32::try_from(review_row_count(loaded)).unwrap_or(i32::MAX)),
            );
            value.insert(
                QString::from("rowOffset"),
                QVariant::from(
                    &i32::try_from(normalized_review_row_offset(loaded)).unwrap_or(i32::MAX),
                ),
            );
            value.insert(QString::from("rows"), QVariant::from(&review_rows(loaded)));
            QVariant::from(&value)
        },
    );
    insert("file", file);
    QVariant::from(&state)
}

fn set_review_state(mut backend: Pin<&mut ffi::HarknessBackend>, row: ReviewStateRow) {
    let loaded_path_id = {
        let rust = backend.as_mut().rust_mut().get_mut();
        row.loaded_file.as_ref().map_or_else(String::new, |loaded| {
            register_review_path_identity(rust, &row.project_id, &review_path(&loaded.file))
        })
    };
    let value = to_review(&row, &loaded_path_id);
    let previous = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.review_state.replace(row)
    };
    backend.as_mut().set_review(value);
    discard_review_state(previous);
}

fn sync_review_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let identity = backend
        .as_ref()
        .rust()
        .review_state
        .as_ref()
        .and_then(|row| {
            row.loaded_file
                .as_ref()
                .map(|loaded| (row.project_id.clone(), review_path(&loaded.file)))
        });
    let loaded_path_id = identity.map_or_else(String::new, |(project_id, path)| {
        register_review_path_identity(backend.as_mut().rust_mut().get_mut(), &project_id, &path)
    });
    let value = {
        let backend_ref = backend.as_ref();
        let Some(row) = backend_ref.rust().review_state.as_ref() else {
            return;
        };
        to_review(row, &loaded_path_id)
    };
    backend.as_mut().set_review(value);
}

fn clear_review_state(mut backend: Pin<&mut ffi::HarknessBackend>) {
    let previous = {
        let rust = backend.as_mut().rust_mut().get_mut();
        rust.next_review_request += 1;
        rust.next_review_file_request += 1;
        rust.review_path_ids.clear();
        rust.review_state.take()
    };
    backend.as_mut().set_review(empty_review());
    discard_review_state(previous);
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

/// Reads a QML whitespace spelling, which is the wire spelling core publishes.
///
/// QML sends a string rather than an enumerator because cxx-qt would otherwise
/// need a registered Qt enum for a four-valued control, and because keeping the
/// spelling identical to [`harkness_git::WhitespaceMode::name`] means the panel,
/// the CLI envelope and a stored selection all say the same word.
fn parse_whitespace_mode(mode: &str) -> Option<harkness_git::WhitespaceMode> {
    match mode {
        "exact" => Some(harkness_git::WhitespaceMode::Exact),
        "ignore_eol" => Some(harkness_git::WhitespaceMode::IgnoreEol),
        "ignore_change" => Some(harkness_git::WhitespaceMode::IgnoreChange),
        "ignore_all" => Some(harkness_git::WhitespaceMode::IgnoreAll),
        _ => None,
    }
}

/// Puts a freshly loaded review back where the reader left it.
///
/// A refresh, a mutation and a change of whitespace handling all rebuild the
/// row from scratch, and all three must land on the same line of the same file
/// rather than at the top of a long diff. The offset is renormalized afterwards
/// because the new model can be shorter than the one it replaces — which is
/// exactly what relaxing whitespace does.
fn resume_at(row: &mut ReviewStateRow, position: ReviewLaunchPosition) {
    let Some(loaded) = row.loaded_file.as_mut() else {
        return;
    };
    loaded.row_offset = position.row_offset;
    loaded.row_page_origin = position.row_page_origin % REVIEW_ROW_PAGE_SIZE;
    loaded.row_offset = normalized_review_row_offset(loaded);
}

/// The whitespace handling the open review is already using.
///
/// A refresh after a mutation, and opening a different target from the same
/// shell, both keep what the reader chose. Snapping back to exact would undo
/// the control every time Git state moved underneath it, which is exactly when
/// a noisy diff is hardest to read. A different project starts exact.
fn current_review_whitespace(
    backend: Pin<&ffi::HarknessBackend>,
    project_id: &str,
) -> harkness_git::Whitespace {
    backend
        .rust()
        .review_state
        .as_ref()
        .filter(|state| state.project_id == project_id)
        .map_or(harkness_git::Whitespace::EXACT, |state| state.whitespace)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one request describes what to show, how to compute it and where to resume"
)]
fn launch_review_request(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: String,
    selection: ReviewSelection,
    whitespace: harkness_git::Whitespace,
    preferred_path: Option<PathBuf>,
    loading_title: String,
    loading_detail: String,
    position: ReviewLaunchPosition,
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
        ReviewStateRow::loading(
            project_id.clone(),
            selection.clone(),
            whitespace,
            loading_title,
            loading_detail,
        ),
    );
    let qt_thread = backend.qt_thread();
    // The failure path below rebuilds a row from scratch when there is nothing
    // to fall back on, and that row still has to say what was being requested.
    let requested = selection.clone();
    std::thread::spawn(move || {
        let result = load_project_git(&project_id).and_then(|git| {
            let mut row = load_review_with_initial_file_with_git(
                &git,
                project_id.clone(),
                selection,
                whitespace,
                request_id,
                file_request,
                preferred_path.as_deref(),
            )?;
            resume_at(&mut row, position);
            Ok(row)
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
                            ReviewStateRow::loading(
                                project_id,
                                requested,
                                whitespace,
                                "Review".to_owned(),
                                String::new(),
                            )
                        })
                        .with_failure(&failure);
                    set_review_state(backend.as_mut(), row);
                }
            }
        });
    });
}

fn launch_working_review(
    backend: Pin<&mut ffi::HarknessBackend>,
    project_id: String,
    staged: bool,
    preferred_path: Option<PathBuf>,
) {
    let position = backend
        .as_ref()
        .rust()
        .review_state
        .as_ref()
        .filter(|state| state.project_id == project_id)
        .and_then(|state| state.loaded_file.as_ref())
        .filter(|loaded| {
            preferred_path
                .as_deref()
                .is_some_and(|path| path == review_path(&loaded.file).as_path())
        })
        .map_or_else(ReviewLaunchPosition::default, |loaded| {
            ReviewLaunchPosition {
                row_offset: normalized_review_row_offset(loaded),
                row_page_origin: loaded.row_page_origin,
            }
        });
    let whitespace = current_review_whitespace(backend.as_ref(), &project_id);
    launch_review_request(
        backend,
        project_id,
        if staged {
            ReviewSelection::Staged
        } else {
            ReviewSelection::Unstaged
        },
        whitespace,
        preferred_path,
        if staged {
            "Staged changes".to_owned()
        } else {
            "Working-tree changes".to_owned()
        },
        "Loading changed paths…".to_owned(),
        position,
    );
}

#[derive(Debug)]
struct GitWorkerResult {
    project_id: String,
    message: Result<String, GitFailure>,
    state: Option<GitStateRow>,
}

fn run_git_status(
    project_id: String,
    cancellation: &harkness_git::Cancellation,
    discard_snapshot_cache: &DiscardSnapshotCache,
) -> GitWorkerResult {
    run_git_status_with_git_and_cache(
        project_id.clone(),
        load_project_git(&project_id),
        cancellation,
        discard_snapshot_cache,
    )
}

#[cfg(test)]
fn run_git_status_with_git(
    project_id: String,
    git: Result<harkness_git::GitService, GitFailure>,
    cancellation: &harkness_git::Cancellation,
) -> GitWorkerResult {
    run_git_status_with_git_and_cache(project_id, git, cancellation, &Arc::default())
}

fn run_git_status_with_git_and_cache(
    project_id: String,
    git: Result<harkness_git::GitService, GitFailure>,
    cancellation: &harkness_git::Cancellation,
    discard_snapshot_cache: &DiscardSnapshotCache,
) -> GitWorkerResult {
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
    match git.detailed_status(cancellation) {
        Ok(status) => {
            let mut state = GitStateRow::from_status(project_id.clone(), status);
            attach_discard_snapshots(&git, &mut state, discard_snapshot_cache);
            GitWorkerResult {
                state: Some(state),
                project_id,
                message: Ok("Git status refreshed".to_owned()),
            }
        }
        Err(error) => GitWorkerResult {
            project_id,
            message: Err(GitFailure::from(error)),
            state: None,
        },
    }
}

fn run_git_operation(
    project_id: String,
    cancellation: &harkness_git::Cancellation,
    discard_snapshot_cache: &DiscardSnapshotCache,
    operation: impl FnOnce(
        &harkness_git::GitService,
        &harkness_git::Cancellation,
    ) -> Result<String, GitFailure>,
) -> GitWorkerResult {
    run_git_operation_with_git_and_cache(
        project_id.clone(),
        load_project_git(&project_id),
        cancellation,
        discard_snapshot_cache,
        operation,
    )
}

#[cfg(test)]
fn run_git_operation_with_git(
    project_id: String,
    git: Result<harkness_git::GitService, GitFailure>,
    cancellation: &harkness_git::Cancellation,
    operation: impl FnOnce(
        &harkness_git::GitService,
        &harkness_git::Cancellation,
    ) -> Result<String, GitFailure>,
) -> GitWorkerResult {
    run_git_operation_with_git_and_cache(project_id, git, cancellation, &Arc::default(), operation)
}

fn run_git_operation_with_git_and_cache(
    project_id: String,
    git: Result<harkness_git::GitService, GitFailure>,
    cancellation: &harkness_git::Cancellation,
    discard_snapshot_cache: &DiscardSnapshotCache,
    operation: impl FnOnce(
        &harkness_git::GitService,
        &harkness_git::Cancellation,
    ) -> Result<String, GitFailure>,
) -> GitWorkerResult {
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
            let mut state = GitStateRow::from_status(project_id.clone(), status);
            attach_discard_snapshots(&git, &mut state, discard_snapshot_cache);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuiDiscardOperation {
    RestoreIndex,
    RestoreHead,
    DeleteUntracked,
}

impl GuiDiscardOperation {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "restore_index" => Some(Self::RestoreIndex),
            "restore_head" => Some(Self::RestoreHead),
            "delete_untracked" => Some(Self::DeleteUntracked),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RestoreIndex => "restore_index",
            Self::RestoreHead => "restore_head",
            Self::DeleteUntracked => "delete_untracked",
        }
    }
}

fn discard_success_message(description: &harkness_git::DiscardDescription) -> String {
    match description.operation() {
        harkness_git::DiscardOperation::RestoreTracked { source } => format!(
            "Restored {} tracked file(s) from {}",
            description.tracked_files(),
            match source {
                harkness_git::TrackedRestoreSource::Index => "the index",
                harkness_git::TrackedRestoreSource::Head => "HEAD",
                _ => "the selected boundary",
            }
        ),
        harkness_git::DiscardOperation::RestoreTrackedHunks { hunks } => {
            format!("Discarded {hunks} tracked hunk(s)")
        }
        harkness_git::DiscardOperation::DeleteUntracked => format!(
            "Permanently deleted {} untracked file(s)",
            description.untracked_files()
        ),
        _ => "Discarded selected changes".to_owned(),
    }
}

fn launch_path_discard(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: String,
    paths: Vec<PathBuf>,
    operation: GuiDiscardOperation,
    snapshot: harkness_git::DiscardSnapshot,
) {
    let Some((job_id, cancellation)) = start_job(
        backend.as_mut(),
        "discard_path",
        &project_id,
        "Discard file changes",
        true,
    ) else {
        return;
    };
    let discard_snapshot_cache = backend.as_ref().rust().discard_snapshot_cache.clone();
    let qt_thread = backend.qt_thread();
    std::thread::spawn(move || {
        let result = run_git_operation(
            project_id,
            &cancellation,
            &discard_snapshot_cache,
            |git, cancellation| {
                let outcome = match operation {
                    GuiDiscardOperation::RestoreIndex => git.restore_tracked_if_unchanged(
                        &paths,
                        harkness_git::TrackedRestoreSource::Index,
                        &snapshot,
                        cancellation,
                    ),
                    GuiDiscardOperation::RestoreHead => git.restore_tracked_if_unchanged(
                        &paths,
                        harkness_git::TrackedRestoreSource::Head,
                        &snapshot,
                        cancellation,
                    ),
                    GuiDiscardOperation::DeleteUntracked => {
                        git.delete_untracked_if_unchanged(&paths, &snapshot, cancellation)
                    }
                }
                .map_err(GitFailure::from)?;
                Ok(discard_success_message(&outcome.description))
            },
        );
        let _ = qt_thread.queue(move |mut backend| {
            apply_git_result(
                backend.as_mut(),
                &job_id,
                result,
                GitResultFollowUp::WORKING_TREE,
            );
        });
    });
}

/// One hunk the reader picked, named in the coordinates it was rendered in.
///
/// A whitespace-insensitive view cannot yield a [`harkness_git::HunkSelection`]
/// at all, so what travels to the worker is the view itself plus the hunk
/// chosen inside it. The worker re-requests the same path exactly and maps the
/// two through [`harkness_git::remap_to_exact`].
#[derive(Clone, Debug)]
struct ReviewHunkRequest {
    view: harkness_git::FileDiff,
    hunk: harkness_git::Hunk,
}

/// Turns a rendered hunk into selections that describe the bytes on disk.
///
/// For an already-exact view this is the identity, and the extra diff it costs
/// is the same path-restricted request the surface makes constantly. For a
/// relaxed one it is the seam issue #80 requires: the file is re-requested at
/// [`harkness_git::Whitespace::EXACT`] and the chosen region is re-expressed in
/// that model, or refused by name when the exact diff carries changed lines the
/// view was hiding.
fn exact_hunk_selections(
    git: &harkness_git::GitService,
    request: &ReviewHunkRequest,
) -> Result<Vec<harkness_git::HunkSelection>, harkness_git::GitError> {
    let path = review_path(&request.view);
    let options = harkness_git::DiffOptions::unbounded()
        .with_context_lines(request.view.context_lines)
        .with_paths([path.as_path()]);
    let files = git.diff(request.view.target.clone(), &options)?;
    let exact = files
        .iter()
        .find(|file| {
            file.old_path == request.view.old_path && file.new_path == request.view.new_path
        })
        .ok_or_else(|| harkness_git::GitError::StaleHunkSelection { path: path.clone() })?;
    // `unbounded()` is exact by construction, so this cannot fail; it is
    // checked rather than unwrapped because the guarantee lives in another
    // crate and a refusal is a better answer here than a panic.
    let exact = exact
        .exact()
        .ok_or(harkness_git::GitError::StaleHunkSelection { path })?;
    harkness_git::remap_to_exact(&request.view, &request.hunk, exact)
}

fn launch_hunk_discard(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: String,
    request: ReviewHunkRequest,
) {
    let Some((job_id, cancellation)) = start_job(
        backend.as_mut(),
        "discard_hunk",
        &project_id,
        "Discard hunk",
        true,
    ) else {
        return;
    };
    let discard_snapshot_cache = backend.as_ref().rust().discard_snapshot_cache.clone();
    let qt_thread = backend.qt_thread();
    std::thread::spawn(move || {
        let result = run_git_operation(
            project_id,
            &cancellation,
            &discard_snapshot_cache,
            |git, cancellation| {
                let selections = exact_hunk_selections(git, &request).map_err(GitFailure::from)?;
                let outcome = git
                    .discard_hunks(&selections, cancellation)
                    .map_err(GitFailure::from)?;
                Ok(discard_success_message(&outcome.description))
            },
        );
        let _ = qt_thread.queue(move |mut backend| {
            apply_git_result(
                backend.as_mut(),
                &job_id,
                result,
                GitResultFollowUp::WORKING_TREE,
            );
        });
    });
}

/// The catalog fields a mutation's own status read already answers.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CatalogGitProjection {
    branch: String,
    dirty: bool,
}

impl CatalogGitProjection {
    fn from_state(state: &GitStateRow) -> Self {
        Self {
            // Both projections spell a detached head as an empty branch. An
            // unborn one has to be spelled that way too: this row carries the
            // branch a first commit would create, but the catalog's inspection
            // reports no branch at all until that commit exists, and writing
            // the name here would be silently undone by the next full reload.
            branch: if state.unborn {
                String::new()
            } else {
                state.branch.clone()
            },
            // The catalog derives `dirty` from the same walk these entries
            // come from — any changed, untracked or conflicted path — so an
            // empty entry list is exactly a clean working tree.
            dirty: !state.entries.is_empty(),
        }
    }

    /// Returns `map` with this projection applied, or `None` when `map` is not
    /// a project map for `project_id`.
    fn applied_to(&self, map: &QVariant, project_id: &str) -> Option<QVariant> {
        let mut entry = map.value::<QMap<QMapPair_QString_QVariant>>()?;
        if entry
            .get(&QString::from("id"))?
            .value::<QString>()?
            .to_string()
            != project_id
        {
            return None;
        }
        entry.insert(
            QString::from("branch"),
            QVariant::from(&QString::from(self.branch.as_str())),
        );
        entry.insert(QString::from("dirty"), QVariant::from(&self.dirty));
        Some(QVariant::from(&entry))
    }
}

/// Brings the catalog projection in line with a mutation's own status read.
///
/// A commit, a checkout or a pull changes exactly two derived fields of the
/// acting project: the branch it is on, and whether its working tree is
/// dirty. The status the mutation already read answers both. Reloading the
/// whole catalog to learn them cost one Git inspection, one repository
/// identity read and one remote read *per catalogued project*, and handed
/// `opened` a freshly built map — which invalidated every binding in the
/// shell and made both side panels reload as though a different project had
/// been opened.
///
/// A mutation whose status could not be read falls back to the full reload:
/// nothing local is known to be true any more, including whether the project
/// is still on disk.
fn sync_catalog_projection(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    project_id: &str,
    state: Option<&GitStateRow>,
) {
    let Some(projection) = state.map(CatalogGitProjection::from_state) else {
        backend.as_mut().refresh();
        return;
    };
    // This projection is newer than any catalog reload already in flight, and
    // those reloads carry the pre-mutation branch and dirty state. Claim the
    // generation the reload replies are gated on so a listing that started
    // before the mutation cannot land after it and undo this.
    backend.as_mut().rust_mut().get_mut().next_catalog_request += 1;
    if let Some(opened) = projection.applied_to(backend.as_ref().opened(), project_id) {
        backend.as_mut().set_opened(opened);
    }
    let projects = backend
        .as_ref()
        .projects()
        .iter()
        .map(|entry| {
            projection
                .applied_to(entry, project_id)
                .unwrap_or_else(|| entry.clone())
        })
        .collect::<QList<QVariant>>();
    backend.as_mut().set_projects(projects);
}

/// What a finished Git operation brings up to date beyond its own status.
///
/// Named rather than positional: every one of these is a reload the user did
/// not ask for, and a call site has to be able to say why it wants each. Only
/// what the operation can actually have invalidated belongs here — a status
/// poll that restarts the commit walk resets whatever History had scrolled to.
#[derive(Clone, Copy, Debug)]
struct GitResultFollowUp {
    /// Re-project the acting project's catalog row from the fresh status.
    catalog: bool,
    /// Reload the branch picker, for an operation that changes what exists.
    branches: bool,
    /// Restart the commit walk, for an operation that can move `HEAD`.
    history: bool,
    /// Reload the working-tree diff the review surface is showing.
    review: bool,
    /// Suppress the success line, for work the user did not ask for.
    quiet: bool,
}

impl GitResultFollowUp {
    /// A working-tree mutation: the catalog row it changed and the diff on
    /// screen. `HEAD` and the set of branches are where they were.
    const WORKING_TREE: Self = Self {
        catalog: true,
        branches: false,
        history: false,
        review: true,
        quiet: false,
    };
}

fn apply_git_result(
    mut backend: Pin<&mut ffi::HarknessBackend>,
    job_id: &str,
    result: GitWorkerResult,
    follow_up: GitResultFollowUp,
) {
    finish_job(backend.as_mut(), job_id);
    let is_open =
        opened_project_id(backend.as_ref().opened()).as_deref() == Some(result.project_id.as_str());
    let should_refresh_review = follow_up.review && is_open && result.state.is_some();
    let project_id = result.project_id.clone();
    if is_open {
        if let Some(state) = &result.state {
            set_git_state(backend.as_mut(), state);
        } else {
            // A status that could not be refreshed must not leave mutation
            // capabilities from the last successful snapshot usable.
            clear_git_state(backend.as_mut());
        }
        match result.message {
            Ok(message) if !follow_up.quiet => backend.as_mut().set_status(message.into()),
            Ok(_) => {}
            Err(failure) => backend.as_mut().set_status(failure.message.into()),
        }
        if follow_up.branches {
            backend
                .as_mut()
                .refresh_branches(&QString::from(result.project_id.as_str()));
        }
        if follow_up.history {
            backend
                .as_mut()
                .refresh_history(&QString::from(result.project_id.as_str()));
        }
    }
    if follow_up.catalog {
        sync_catalog_projection(backend.as_mut(), &project_id, result.state.as_ref());
    }
    if should_refresh_review {
        refresh_current_working_review(backend.as_mut(), &project_id);
    }
}

fn selected_review_path(state: &ReviewStateRow) -> Option<PathBuf> {
    state
        .loaded_file
        .as_ref()
        .map(|loaded| review_path(&loaded.file))
        .or_else(|| {
            state
                .files
                .iter()
                .find(|entry| entry.id == state.selected_file_id)
                .map(|entry| entry.path.clone())
        })
}

fn refresh_current_working_review(mut backend: Pin<&mut ffi::HarknessBackend>, project_id: &str) {
    let Some((staged, preferred_path)) =
        backend
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .and_then(|state| {
                if state.project_id != project_id || state.loading || state.file_loading {
                    return None;
                }
                let staged = match state.target.as_ref().map(|target| &target.target) {
                    Some(harkness_git::DiffTarget::Staged) => true,
                    Some(harkness_git::DiffTarget::Unstaged) => false,
                    _ => return None,
                };
                Some((staged, selected_review_path(state)))
            })
    else {
        return;
    };
    launch_working_review(
        backend.as_mut(),
        project_id.to_owned(),
        staged,
        preferred_path,
    );
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
    lock_scope: String,
    lock_scope_resolved: bool,
    display_name: String,
    root: String,
    remote: String,
    github_remote: String,
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
        let github_remote = if remote.starts_with("github.com/") {
            remote.clone()
        } else if project.available && project.git.is_some() {
            harkness_git::repository_remote_url(&project.root)
                .ok()
                .flatten()
                .and_then(|url| harkness_core::normalize_remote(&url).ok())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let id = project.id.to_string();
        let (lock_scope, lock_scope_resolved) = if project.available && project.git.is_some() {
            match harkness_git::repository_identity(&project.root) {
                Ok(identity) => (identity, true),
                Err(_) if parent_id.is_empty() => (id.clone(), false),
                Err(_) => (parent_id.clone(), false),
            }
        } else if parent_id.is_empty() {
            (id.clone(), false)
        } else {
            (parent_id.clone(), false)
        };
        Self {
            id,
            lock_scope,
            lock_scope_resolved,
            display_name: project.display_name,
            root: project.root.display().to_string(),
            remote,
            github_remote,
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
    let mut rows = projects
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
        .collect::<Vec<_>>();
    // A missing managed worktree cannot open its own Git common directory.
    // It still belongs to its catalog parent's mutation domain, so inherit the
    // parent's already-derived identity rather than falling back to a catalog
    // UUID that available siblings and the parent do not use.
    for _ in 0..rows.len() {
        let scopes = rows
            .iter()
            .map(|row| (row.id.clone(), row.lock_scope.clone()))
            .collect::<HashMap<_, _>>();
        let mut changed = false;
        for row in &mut rows {
            if row.lock_scope_resolved {
                continue;
            }
            let Some(parent_scope) = scopes.get(&row.parent_id) else {
                continue;
            };
            if &row.lock_scope != parent_scope {
                row.lock_scope.clone_from(parent_scope);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    rows
}

fn project_repository_lock_scopes(rows: &[ProjectRow]) -> HashMap<String, String> {
    rows.iter()
        .map(|row| (row.id.clone(), row.lock_scope.clone()))
        .collect()
}

fn project_worktree_lifecycle_lock_scopes(rows: &[ProjectRow]) -> HashMap<String, String> {
    let scopes = project_repository_lock_scopes(rows);
    rows.iter()
        .filter(|row| !row.parent_id.is_empty())
        .map(|row| {
            let scope = scopes
                .get(&row.parent_id)
                .cloned()
                .unwrap_or_else(|| row.parent_id.clone());
            (row.id.clone(), scope)
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
        "lockScope",
        QVariant::from(&QString::from(row.lock_scope.as_str())),
    );
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
        "githubRemote",
        QVariant::from(&QString::from(row.github_remote.as_str())),
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

fn worktree_job_lock_scope(
    lifecycle_scopes: &HashMap<String, String>,
    worktree_id: &str,
    parent_id: &str,
    opened_scope: Option<String>,
) -> String {
    lifecycle_scopes
        .get(worktree_id)
        .cloned()
        .or(opened_scope)
        .unwrap_or_else(|| parent_id.to_owned())
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
    let lock_scope = worktree_job_lock_scope(
        &backend.as_ref().rust().worktree_lifecycle_lock_scopes,
        &project_id,
        &scope_project_id,
        opened_repository_lock_scope(backend.as_ref().opened()),
    );
    let Some((job_id, cancellation)) = start_job_in_scope(
        backend.as_mut(),
        kind,
        &project_id,
        &lock_scope,
        label,
        true,
    ) else {
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

fn opened_github_remote(opened: &QVariant) -> Option<String> {
    opened
        .value::<QMap<QMapPair_QString_QVariant>>()?
        .get(&QString::from("githubRemote"))?
        .value::<QString>()
        .map(|remote| remote.to_string())
        .filter(|remote| !remote.is_empty())
}

fn opened_repository_lock_scope(opened: &QVariant) -> Option<String> {
    let opened = opened.value::<QMap<QMapPair_QString_QVariant>>()?;
    if let Some(scope) = opened
        .get(&QString::from("lockScope"))
        .and_then(|value| value.value::<QString>())
        .map(|scope| scope.to_string())
        .filter(|scope| !scope.is_empty())
    {
        return Some(scope);
    }
    let project_id = opened
        .get(&QString::from("id"))?
        .value::<QString>()?
        .to_string();
    if project_id.is_empty() {
        return None;
    }
    let worktree = opened
        .get(&QString::from("worktree"))
        .and_then(|value| value.value::<bool>())
        .unwrap_or(false);
    if !worktree {
        return Some(project_id);
    }
    opened
        .get(&QString::from("parentId"))
        .and_then(|value| value.value::<QString>())
        .map(|parent| parent.to_string())
        .filter(|parent| !parent.is_empty())
        .or(Some(project_id))
}

#[derive(Debug)]
enum OpenedUpdate {
    Keep,
    Open(Box<ProjectRow>),
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
                OpenedUpdate::Open(Box::new(ProjectRow::from(project)))
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
        OpenedUpdate::Open(row) => {
            clear_issues_state(backend.as_mut());
            backend.as_mut().set_opened(to_map(&row));
        }
        OpenedUpdate::Clear => {
            clear_issues_state(backend.as_mut());
            backend.as_mut().set_opened(empty_opened());
        }
    }
    backend.as_mut().refresh();
}

fn accept_current_catalog_refresh<T>(latest_request: u64, request: u64, result: T) -> Option<T> {
    (latest_request == request).then_some(result)
}

impl ffi::HarknessBackend {
    /// Reloads the whole catalog into [`projects`](Self::projects).
    fn refresh(mut self: Pin<&mut Self>) {
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_catalog_request += 1;
            rust.next_catalog_request
        };
        let opened_id = opened_project_id(self.as_ref().opened());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let rows = load_rows();
            let _ = qt_thread.queue(move |mut backend| {
                let Some(rows) = accept_current_catalog_refresh(
                    backend.as_ref().rust().next_catalog_request,
                    request_id,
                    rows,
                ) else {
                    return;
                };
                match rows {
                    Ok(rows) => {
                        if opened_id == opened_project_id(backend.as_ref().opened())
                            && let Some(row) = opened_id
                                .as_ref()
                                .and_then(|id| rows.iter().find(|row| &row.id == id))
                        {
                            backend.as_mut().set_opened(to_map(row));
                        }
                        let repository_lock_scopes = project_repository_lock_scopes(&rows);
                        let worktree_lifecycle_lock_scopes =
                            project_worktree_lifecycle_lock_scopes(&rows);
                        let rust = backend.as_mut().rust_mut().get_mut();
                        rust.repository_lock_scopes = repository_lock_scopes;
                        rust.worktree_lifecycle_lock_scopes = worktree_lifecycle_lock_scopes;
                        backend.as_mut().set_projects(to_projects(&rows));
                    }
                    Err(error) => {
                        backend
                            .as_mut()
                            .rust_mut()
                            .get_mut()
                            .repository_lock_scopes
                            .clear();
                        backend
                            .as_mut()
                            .rust_mut()
                            .get_mut()
                            .worktree_lifecycle_lock_scopes
                            .clear();
                        backend.as_mut().set_projects(QList::default());
                        backend.as_mut().set_status(error.into());
                    }
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
        {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_branch_request += 1;
            rust.next_worktree_request += 1;
        }
        self.as_mut().set_opened(empty_opened());
        clear_git_state(self.as_mut());
        self.as_mut().set_branches(QList::default());
        self.as_mut().set_worktrees(QList::default());
        clear_history_state(self.as_mut());
        clear_issues_state(self.as_mut());
        self.as_mut().set_checks(empty_checks());
        clear_review_state(self.as_mut());
    }

    fn refresh_branches(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, _cancellation)) = start_job(
            self.as_mut(),
            "branches",
            &project_id,
            "Refresh branches",
            false,
        ) else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_branch_request += 1;
            rust.next_branch_request
        };
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_branches(&project_id);
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_branch_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
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
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_status(project_id, &cancellation, &discard_snapshot_cache);
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        catalog: false,
                        quiet: true,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
            });
        });
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

    fn refresh_issues(mut self: Pin<&mut Self>, project_id: &QString, github_remote: &QString) {
        let project_id = project_id.to_string();
        let github_remote = github_remote.to_string();
        if opened_project_id(self.as_ref().opened()).as_deref() != Some(project_id.as_str())
            || opened_github_remote(self.as_ref().opened()).as_deref()
                != Some(github_remote.as_str())
        {
            self.as_mut()
                .set_status("The requested issues do not belong to the open project".into());
            return;
        }
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "issues", &project_id, "Refresh issues", true)
        else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_issues_request += 1;
            rust.next_issues_request
        };
        let loading = IssuesStateRow::loading(project_id.clone(), github_remote.clone());
        set_issues_state(self.as_mut(), loading.clone());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_github_issue_page(&github_remote, None, 0, &cancellation);
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_issues_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                    || opened_github_remote(backend.as_ref().opened()).as_deref()
                        != Some(github_remote.as_str())
                {
                    return;
                }
                match result {
                    Ok(page) => set_issues_state(
                        backend.as_mut(),
                        IssuesStateRow {
                            project_id,
                            remote: github_remote,
                            loading: false,
                            viewer: page.viewer,
                            rows: page.rows,
                            next_cursor: page.next_cursor,
                            total_count: page.total_count,
                            limit_reached: page.limit_reached,
                            error: String::new(),
                            error_kind: String::new(),
                        },
                    ),
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_issues_state(backend.as_mut(), loading.with_failure(failure));
                    }
                }
            });
        });
    }

    fn load_more_issues(mut self: Pin<&mut Self>, project_id: &QString, github_remote: &QString) {
        let project_id = project_id.to_string();
        let github_remote = github_remote.to_string();
        let Some(mut current) = self.as_ref().rust().issues_state.clone() else {
            self.as_mut()
                .set_status("Refresh issues before loading more".into());
            return;
        };
        if current.project_id != project_id || current.remote != github_remote {
            self.as_mut()
                .set_status("The visible issues belong to a different project".into());
            return;
        }
        let Some(cursor) = current.next_cursor.clone() else {
            return;
        };
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "issues",
            &project_id,
            "Load more issues",
            true,
        ) else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_issues_request += 1;
            rust.next_issues_request
        };
        current.loading = true;
        current.error.clear();
        current.error_kind.clear();
        set_issues_state(self.as_mut(), current.clone());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_github_issue_page(
                &github_remote,
                Some(&cursor),
                current.rows.len(),
                &cancellation,
            );
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_issues_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                    || opened_github_remote(backend.as_ref().opened()).as_deref()
                        != Some(github_remote.as_str())
                {
                    return;
                }
                match result {
                    Ok(page) => {
                        current.rows.extend(page.rows);
                        current.viewer = page.viewer;
                        current.next_cursor = page.next_cursor;
                        current.total_count = page.total_count;
                        current.limit_reached = page.limit_reached;
                        current.loading = false;
                        set_issues_state(backend.as_mut(), current);
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        set_issues_state(backend.as_mut(), current.with_failure(failure));
                    }
                }
            });
        });
    }

    fn refresh_checks(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, _cancellation)) =
            start_job(self.as_mut(), "checks", &project_id, "Load checks", false)
        else {
            return;
        };
        self.as_mut()
            .set_checks(to_checks(&project_id, &[], &[], true, ""));
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_checks(&project_id);
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if opened_project_id(backend.as_ref().opened()).as_deref()
                    != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok((configured, results)) => backend.as_mut().set_checks(to_checks(
                        &project_id,
                        &configured,
                        &results,
                        false,
                        "",
                    )),
                    Err(error) => {
                        backend.as_mut().set_status(error.as_str().into());
                        backend.as_mut().set_checks(to_checks(
                            &project_id,
                            &[],
                            &[],
                            false,
                            &error,
                        ));
                    }
                }
            });
        });
    }

    fn run_check(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        check_id: &QString,
        trust_workspace: bool,
    ) {
        let project_id = project_id.to_string();
        let check_id = check_id.to_string();
        if check_id.trim().is_empty() {
            self.as_mut().set_status("Choose a configured check".into());
            return;
        }
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "check", &project_id, "Run check", true)
        else {
            return;
        };
        self.as_mut().set_status("Running check…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_project_check(&project_id, &check_id, trust_workspace, &cancellation);
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if opened_project_id(backend.as_ref().opened()).as_deref()
                    != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok((configured, results)) => {
                        backend.as_mut().set_checks(to_checks(
                            &project_id,
                            &configured,
                            &results,
                            false,
                            "",
                        ));
                        backend.as_mut().set_status("Check completed".into());
                    }
                    Err(error) => {
                        backend.as_mut().set_status(error.as_str().into());
                        backend.as_mut().set_checks(to_checks(
                            &project_id,
                            &[],
                            &[],
                            false,
                            &error,
                        ));
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
        let whitespace = current_review_whitespace(self.as_ref(), &project_id);
        launch_review_request(
            self,
            project_id,
            ReviewSelection::Commit {
                revision: revision.clone(),
            },
            whitespace,
            None,
            format!("Commit {short}"),
            revision,
            ReviewLaunchPosition::default(),
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
        let whitespace = current_review_whitespace(self.as_ref(), &project_id);
        launch_review_request(
            self,
            project_id,
            ReviewSelection::Branch {
                branch: branch.clone(),
                base_branch: base_branch.clone(),
            },
            whitespace,
            None,
            format!("{branch} against {base_branch}"),
            "Resolving the merge-base…".to_owned(),
            ReviewLaunchPosition::default(),
        );
    }

    fn review_working_changes(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        staged: bool,
        path_id: &QString,
    ) {
        let project_id = project_id.to_string();
        let path_id = path_id.to_string();
        let preferred_path = if path_id.is_empty() {
            self.as_ref()
                .rust()
                .review_state
                .as_ref()
                .filter(|state| state.project_id == project_id)
                .and_then(selected_review_path)
        } else {
            match resolve_path_selection(self.as_ref().rust(), &project_id, &path_id) {
                Ok(selection) => Some(selection.path),
                Err(error) => {
                    self.as_mut().set_status(error.into());
                    return;
                }
            }
        };
        launch_working_review(self, project_id, staged, preferred_path);
    }

    fn discard_path(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        path_id: &QString,
        operation: &QString,
    ) {
        let project_id = project_id.to_string();
        let path_id = path_id.to_string();
        let operation = operation.to_string();
        let Some(operation) = GuiDiscardOperation::parse(&operation) else {
            self.as_mut()
                .set_status("The discard operation is invalid; refresh Git status".into());
            return;
        };
        let selection = match resolve_path_selection(self.as_ref().rust(), &project_id, &path_id) {
            Ok(selection) => selection,
            Err(error) => {
                self.as_mut().set_status(error.into());
                return;
            }
        };
        let expected = self
            .as_ref()
            .rust()
            .path_discard_operations
            .get(&path_id)
            .cloned();
        if expected.as_deref() != Some(operation.name()) {
            self.as_mut().set_status(
                "The path changed after confirmation; refresh Git status and review it again"
                    .into(),
            );
            return;
        }
        let Some(snapshot) = self
            .as_ref()
            .rust()
            .path_discard_snapshots
            .get(&path_id)
            .cloned()
        else {
            self.as_mut().set_status(
                "The path could not be verified; refresh Git status and confirm again".into(),
            );
            return;
        };
        launch_path_discard(
            self,
            project_id,
            selection.commit_paths(),
            operation,
            snapshot,
        );
    }

    fn discard_review_file(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        file_id: &QString,
        operation: &QString,
    ) {
        let project_id = project_id.to_string();
        let file_id = file_id.to_string();
        let operation_text = operation.to_string();
        let Some(operation) = GuiDiscardOperation::parse(&operation_text) else {
            self.as_mut()
                .set_status("The discard operation is invalid; refresh the review".into());
            return;
        };
        let resolved = self
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .filter(|state| state.project_id == project_id)
            .and_then(|state| state.loaded_file.as_ref())
            .filter(|loaded| loaded.id == file_id)
            .and_then(|loaded| {
                review_file_discard_description(&loaded.file)
                    .zip(loaded.discard_snapshot.clone())
                    .map(|(description, snapshot)| {
                        let mut paths =
                            [loaded.file.old_path.clone(), loaded.file.new_path.clone()]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>();
                        paths.sort_unstable();
                        paths.dedup();
                        (paths, description, snapshot)
                    })
            });
        let Some((paths, description, snapshot)) = resolved else {
            self.as_mut()
                .set_status("The selected review file is no longer discardable".into());
            return;
        };
        if discard_operation_name(&description) != operation.name() {
            self.as_mut().set_status(
                "The review file changed after confirmation; refresh it and review again".into(),
            );
            return;
        }
        launch_path_discard(self, project_id, paths, operation, snapshot);
    }

    fn discard_review_hunk(mut self: Pin<&mut Self>, project_id: &QString, hunk_id: &QString) {
        let project_id = project_id.to_string();
        let hunk_id = hunk_id.to_string();
        let selection = self
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .filter(|state| state.project_id == project_id)
            .and_then(|state| state.loaded_file.as_ref())
            .filter(|loaded| {
                matches!(loaded.file.target, harkness_git::DiffTarget::Unstaged)
                    && !matches!(
                        loaded.file.change,
                        harkness_git::FileChange::Untracked | harkness_git::FileChange::Unmerged
                    )
            })
            .and_then(|loaded| {
                loaded
                    .hunks
                    .iter()
                    .position(|state| state.id == hunk_id)
                    .map(|index| ReviewHunkRequest {
                        // The view is carried whole rather than reduced to a
                        // selection here, because a whitespace-insensitive one
                        // has no selection to reduce to: the exact hunks it
                        // maps onto are only knowable once the file has been
                        // re-requested, which is worker-thread work.
                        view: loaded.file.clone(),
                        hunk: loaded.file.hunks[index].clone(),
                    })
            });
        let Some(request) = selection else {
            self.as_mut().set_status(
                "The selected hunk is no longer discardable; refresh the review".into(),
            );
            return;
        };
        launch_hunk_discard(self, project_id, request);
    }

    /// Writes exactly what it is handed, so a copied line keeps the carriage
    /// return the review surface only drew a label for.
    fn copy_to_clipboard(&self, text: &QString) {
        ffi::set_clipboard_text(text);
    }

    fn open_review_line(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        file_id: &QString,
        line: i32,
    ) {
        let project_id_text = project_id.to_string();
        let file_id = file_id.to_string();
        let selection = self
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .filter(|state| state.project_id == project_id_text)
            .and_then(|state| {
                state.loaded_file.as_ref().and_then(|loaded| {
                    (loaded.id == file_id).then(|| {
                        let may_differ = state
                            .target
                            .as_ref()
                            .is_some_and(|target| working_tree_may_differ(&target.target));
                        (review_path(&loaded.file), may_differ)
                    })
                })
            });
        let Some((path, may_differ)) = selection else {
            self.as_mut()
                .set_status("The selected review file is no longer available".into());
            return;
        };
        let Ok(project_id) = project_id_text.parse::<harkness_core::ProjectId>() else {
            self.as_mut()
                .set_status("The project identifier is invalid".into());
            return;
        };
        let line = u32::try_from(line)
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .unwrap_or(std::num::NonZeroU32::MIN);
        let result = harkness_core::ProjectService::load().and_then(|service| {
            service.open_in_editor(
                project_id,
                &path,
                harkness_core::EditorPosition::new(line, std::num::NonZeroU32::MIN),
                harkness_core::EditorLaunchContext::Graphical,
            )
        });
        match result {
            Ok(launch) => {
                let mut message = format!(
                    "Opened {} at line {} with {}",
                    path.display(),
                    launch.position.line(),
                    launch.command
                );
                if may_differ {
                    message.push_str(" (working-tree content may differ from this diff)");
                }
                self.as_mut().set_status(message.into());
            }
            Err(error) => self.as_mut().set_status(error.to_string().into()),
        }
    }

    fn set_review_whitespace(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        mode: &QString,
        ignore_blank_lines: bool,
    ) {
        let project_id = project_id.to_string();
        let mode = mode.to_string();
        let Some(mode) = parse_whitespace_mode(&mode) else {
            self.as_mut()
                .set_status("That whitespace setting is not one this build offers".into());
            return;
        };
        let whitespace = harkness_git::Whitespace {
            mode,
            ignore_blank_lines,
        };
        let Some((selection, preferred_path, position)) = self
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .filter(|state| state.project_id == project_id)
            .map(|state| {
                (
                    state.selection.clone(),
                    state
                        .loaded_file
                        .as_ref()
                        .map(|loaded| review_path(&loaded.file)),
                    state.loaded_file.as_ref().map_or_else(
                        ReviewLaunchPosition::default,
                        |loaded| ReviewLaunchPosition {
                            row_offset: normalized_review_row_offset(loaded),
                            row_page_origin: loaded.row_page_origin,
                        },
                    ),
                )
            })
        else {
            self.as_mut()
                .set_status("Open a review before changing its whitespace handling".into());
            return;
        };
        if current_review_whitespace(self.as_ref(), &project_id) == whitespace {
            return;
        }
        let title = self
            .as_ref()
            .rust()
            .review_state
            .as_ref()
            .map_or_else(|| "Review".to_owned(), |state| state.title.clone());
        launch_review_request(
            self,
            project_id,
            selection,
            whitespace,
            preferred_path,
            title,
            "Recomputing this diff…".to_owned(),
            position,
        );
    }

    fn load_review_file(mut self: Pin<&mut Self>, project_id: &QString, file_id: &QString) {
        let project_id = project_id.to_string();
        let file_id = file_id.to_string();
        let selection = match self.as_ref().rust().review_state.as_ref() {
            None => Err("Open a review before choosing a file"),
            Some(state) if state.project_id != project_id => {
                Err("The visible review belongs to a different project")
            }
            Some(state) => match state.target.clone() {
                None => Err("Wait for the review to finish loading"),
                Some(target) => state
                    .files
                    .iter()
                    .enumerate()
                    .find(|(_, entry)| entry.id == file_id)
                    .map(|(index, entry)| (target, state.whitespace, entry.clone(), index))
                    .ok_or("The selected review file is no longer available"),
            },
        };
        let (target, whitespace, entry, entry_index) = match selection {
            Ok(selection) => selection,
            Err(message) => {
                self.as_mut().set_status(message.into());
                return;
            }
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
        let (requests, discarded_file) = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_review_file_request += 1;
            let mut discarded_file = None;
            let requests = rust.review_state.as_mut().map(|state| {
                state.file_offset = entry_index / REVIEW_FILE_PAGE_SIZE * REVIEW_FILE_PAGE_SIZE;
                state.selected_file_id.clone_from(&file_id);
                discarded_file = state.loaded_file.take();
                state.file_loading = true;
                state.error.clear();
                state.error_kind.clear();
                (rust.next_review_request, rust.next_review_file_request)
            });
            (requests, discarded_file)
        };
        discard_review_file(discarded_file);
        let Some((review_request, file_request)) = requests else {
            finish_job(self.as_mut(), &job_id);
            self.as_mut()
                .set_status("The review closed before the file could load".into());
            return;
        };
        sync_review_state(self.as_mut());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_project_git(&project_id).and_then(|git| {
                load_review_file_with_git(&git, &target, whitespace, &entry, file_request)
            });
            let _ = qt_thread.queue(move |mut backend| {
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_review_request != review_request
                    || backend.as_ref().rust().next_review_file_request != file_request
                    || opened_project_id(backend.as_ref().opened()).as_deref()
                        != Some(project_id.as_str())
                {
                    return;
                }
                match result {
                    Ok(file) => {
                        let previous = {
                            let Some(state) =
                                backend.as_mut().rust_mut().get_mut().review_state.as_mut()
                            else {
                                return;
                            };
                            state.file_loading = false;
                            state.loaded_file.replace(file)
                        };
                        sync_review_state(backend.as_mut());
                        discard_review_file(previous);
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        let Some(state) =
                            backend.as_mut().rust_mut().get_mut().review_state.as_mut()
                        else {
                            return;
                        };
                        state.file_loading = false;
                        state.error.clone_from(&failure.message);
                        state.error_kind.clone_from(&failure.kind);
                        sync_review_state(backend.as_mut());
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
        let loaded = match self.as_ref().rust().review_state.as_ref() {
            None => Err("Open a review file first"),
            Some(state) if state.project_id != project_id => {
                Err("The visible review belongs to a different project")
            }
            Some(state) => state
                .loaded_file
                .as_ref()
                .cloned()
                .ok_or("Open a review file first"),
        };
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(message) => {
                self.as_mut().set_status(message.into());
                return;
            }
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
                match result {
                    Ok(ReviewContextOutcome::Loaded(file)) => {
                        let previous = {
                            let Some(state) =
                                backend.as_mut().rust_mut().get_mut().review_state.as_mut()
                            else {
                                return;
                            };
                            state.error.clear();
                            state.error_kind.clear();
                            state.loaded_file.replace(*file)
                        };
                        sync_review_state(backend.as_mut());
                        discard_review_file(previous);
                    }
                    Ok(ReviewContextOutcome::Stale) => {
                        let Some(file_id) = backend
                            .as_ref()
                            .rust()
                            .review_state
                            .as_ref()
                            .map(|state| state.selected_file_id.clone())
                        else {
                            return;
                        };
                        backend.as_mut().set_status(
                            "The file changed; refreshed the review before expanding context"
                                .into(),
                        );
                        let project = QString::from(project_id.as_str());
                        let file = QString::from(file_id.as_str());
                        backend.as_mut().load_review_file(&project, &file);
                    }
                    Err(failure) => {
                        backend.as_mut().set_status(failure.message.as_str().into());
                        let Some(state) =
                            backend.as_mut().rust_mut().get_mut().review_state.as_mut()
                        else {
                            return;
                        };
                        state.error.clone_from(&failure.message);
                        state.error_kind.clone_from(&failure.kind);
                        sync_review_state(backend.as_mut());
                    }
                }
            });
        });
    }

    fn load_more_review_rows(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let result = {
            let rust = self.as_mut().rust_mut().get_mut();
            match rust.review_state.as_mut() {
                None => Err("Open a review file first"),
                Some(state) if state.project_id != project_id => {
                    Err("The visible review belongs to a different project")
                }
                Some(state) => state
                    .loaded_file
                    .as_mut()
                    .map(advance_review_row_window)
                    .ok_or("Open a review file first"),
            }
        };
        match result {
            Ok(true) => sync_review_state(self),
            Ok(false) => {}
            Err(message) => self.as_mut().set_status(message.into()),
        }
    }

    fn load_previous_review_rows(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let result = {
            let rust = self.as_mut().rust_mut().get_mut();
            match rust.review_state.as_mut() {
                None => Err("Open a review file first"),
                Some(state) if state.project_id != project_id => {
                    Err("The visible review belongs to a different project")
                }
                Some(state) => state
                    .loaded_file
                    .as_mut()
                    .map(retreat_review_row_window)
                    .ok_or("Open a review file first"),
            }
        };
        match result {
            Ok(true) => sync_review_state(self),
            Ok(false) => {}
            Err(message) => self.as_mut().set_status(message.into()),
        }
    }

    fn load_more_review_files(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let result = match self.as_mut().rust_mut().get_mut().review_state.as_mut() {
            None => Err("Open a review first"),
            Some(state) if state.project_id != project_id => {
                Err("The visible review belongs to a different project")
            }
            Some(state) => Ok(advance_review_file_window(state)),
        };
        match result {
            Ok(true) => sync_review_state(self),
            Ok(false) => {}
            Err(message) => self.as_mut().set_status(message.into()),
        }
    }

    fn load_previous_review_files(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let result = match self.as_mut().rust_mut().get_mut().review_state.as_mut() {
            None => Err("Open a review first"),
            Some(state) if state.project_id != project_id => {
                Err("The visible review belongs to a different project")
            }
            Some(state) => Ok(retreat_review_file_window(state)),
        };
        match result {
            Ok(true) => sync_review_state(self),
            Ok(false) => {}
            Err(message) => self.as_mut().set_status(message.into()),
        }
    }

    fn clear_review(mut self: Pin<&mut Self>) {
        clear_history_state(self.as_mut());
        clear_review_state(self);
    }

    fn commit(
        mut self: Pin<&mut Self>,
        project_id: &QString,
        message: &QString,
        amend: bool,
        path_ids: &QString,
    ) {
        let project_id = project_id.to_string();
        let message = message.to_string();
        let scope = match resolve_commit_scope(
            self.as_ref().rust(),
            &project_id,
            &path_ids.to_string(),
            amend,
        ) {
            Ok(scope) => scope,
            Err(error) => {
                self.as_mut().set_status(error.into());
                return;
            }
        };
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "commit", &project_id, "Commit", true)
        else {
            return;
        };
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
                    let outcome = git
                        .commit(
                            &message,
                            &harkness_git::CommitOptions::default()
                                .with_amend(amend)
                                .with_scope(scope),
                            cancellation,
                        )
                        .map_err(GitFailure::from)?;
                    let short = outcome.commit_id.chars().take(12).collect::<String>();
                    Ok(if outcome.amended {
                        format!("Amended commit {short}")
                    } else {
                        format!("Created commit {short}")
                    })
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        history: true,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
            });
        });
    }

    fn fetch(mut self: Pin<&mut Self>, project_id: &QString, quiet: bool) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) =
            start_job(self.as_mut(), "fetch", &project_id, "Fetch", true)
        else {
            return;
        };
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
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
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        quiet,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
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
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
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
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        history: true,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
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
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let progress_job_id = job_id.clone();
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
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
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp::WORKING_TREE,
                );
            });
        });
    }

    fn refresh_worktrees(mut self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let Some((job_id, cancellation)) = start_job(
            self.as_mut(),
            "worktrees",
            &project_id,
            "Refresh worktrees",
            true,
        ) else {
            return;
        };
        let request_id = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_worktree_request += 1;
            rust.next_worktree_request
        };
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
                finish_job(backend.as_mut(), &job_id);
                if backend.as_ref().rust().next_worktree_request != request_id
                    || opened_project_id(backend.as_ref().opened()).as_deref()
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
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
                    git.checkout_branch(&branch, cancellation)
                        .map_err(GitFailure::from)?;
                    Ok(format!("Checked out {branch}"))
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        branches: true,
                        history: true,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
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
        let discard_snapshot_cache = self.as_ref().rust().discard_snapshot_cache.clone();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = run_git_operation(
                project_id,
                &cancellation,
                &discard_snapshot_cache,
                |git, cancellation| {
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
                },
            );
            let _ = qt_thread.queue(move |mut backend| {
                apply_git_result(
                    backend.as_mut(),
                    &job_id,
                    result,
                    GitResultFollowUp {
                        branches: true,
                        history: true,
                        ..GitResultFollowUp::WORKING_TREE
                    },
                );
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
        let lock_scope = self
            .as_ref()
            .rust()
            .worktree_lifecycle_lock_scopes
            .get(&project_id)
            .cloned()
            .or_else(|| opened_repository_lock_scope(self.as_ref().opened()))
            .unwrap_or_else(|| project_id.clone());
        let Some((job_id, cancellation)) = start_job_in_scope(
            self.as_mut(),
            "move_worktree",
            &project_id,
            &lock_scope,
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
        let Some((job_id, _cancellation)) = start_job(
            self.as_mut(),
            "remove_managed",
            &project_id,
            "Remove managed repository",
            false,
        ) else {
            return;
        };
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
                finish_job(backend.as_mut(), &job_id);
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
        let lock_scope = self
            .as_ref()
            .rust()
            .worktree_lifecycle_lock_scopes
            .get(&project_id)
            .cloned()
            .or_else(|| opened_repository_lock_scope(self.as_ref().opened()))
            .unwrap_or_else(|| project_id.clone());
        let Some((job_id, cancellation)) = start_job_in_scope(
            self.as_mut(),
            "remove_worktree",
            &project_id,
            &lock_scope,
            label,
            true,
        ) else {
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
    /// The exactness token a selection constructor takes.
    ///
    /// Production code reaches selections through `exact_hunk_selections`,
    /// which re-requests the file; a test that already holds an exact record
    /// spells the check here instead of repeating it.
    fn exact(file: &harkness_git::FileDiff) -> harkness_git::ExactFileDiff<'_> {
        file.exact()
            .expect("fixture diffs are computed at exact whitespace")
    }

    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
    };

    use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    use super::{
        BranchRow, CatalogGitProjection, GITHUB_CLI_REMOVED_ENV, GitFailure, GitStateRow,
        HarknessBackendRust, OpenedUpdate, ProjectRow, REVIEW_FILE_PAGE_SIZE, REVIEW_ROW_PAGE_SIZE,
        ReviewContextDirection, ReviewContextOutcome, ReviewHunkRequest, ReviewLaunchPosition,
        ReviewSelection, WorktreeLockAction, accept_current_catalog_refresh,
        advance_review_file_window, advance_review_row_window, attach_discard_snapshots,
        attach_discard_snapshots_with, begin_job, begin_job_in_scope, best_working_tree_line,
        cancel_issue_jobs, change_worktree_lock_with_service, conflicting_repository_job,
        display_diff_path, display_line_end, empty_opened, end_job, exact_hunk_selections,
        expand_review_context_with_git, github_cli_command, github_cli_output_with_executable,
        github_graphql_arguments, hidden_before, jobs_conflict, line_ending_name,
        load_history_page_with_git, load_review_file_with_git, load_review_with_git,
        load_review_with_initial_file_with_git, move_worktree_with_service, operation_outcome,
        parse_github_issues, parse_whitespace_mode, project_repository_lock_scopes, project_rows,
        project_worktree_lifecycle_lock_scopes, register_path_selection,
        register_review_path_identity, remove_worktree_with_service,
        replace_status_path_selections, resolve_commit_scope, resolve_path_selection, resume_at,
        retreat_review_file_window, retreat_review_row_window, review_content_summary,
        review_file_discard_description, review_file_window, review_hunk_exists_where, review_path,
        review_row_count, review_rows, run_git_operation_with_git, run_git_status_with_git,
        selected_review_path, status_discard_description, text_segments, to_branches, to_checks,
        to_git, to_jobs, to_map, to_projects, to_review, to_review_line_row, update_job,
        working_tree_may_differ, worktree_base, worktree_job_lock_scope,
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
            checks: None,
        }
    }

    #[test]
    fn checks_projection_carries_the_complete_invocation_and_recorded_evidence() {
        let configuration = harkness_core::CheckConfiguration {
            id: "custom.verify".to_owned(),
            label: "Custom verify".to_owned(),
            command: vec!["custom tool".to_owned(), "%2".to_owned()],
            cwd: Some("nested dir".to_owned()),
            env: std::collections::BTreeMap::from([
                ("A_FIRST".to_owned(), "first".to_owned()),
                ("Z_LAST".to_owned(), "last".to_owned()),
            ]),
            parser: harkness_core::CheckParser::CargoJson,
            timeout_seconds: Some(45),
        };
        let summary = harkness_runtime::check::CheckSummary {
            run_id: "run-1".to_owned(),
            check_id: configuration.id.clone(),
            label: configuration.label.clone(),
            command: vec!["old tool".to_owned(), "verify".to_owned()],
            recorded_cwd: Some("old dir".to_owned()),
            recorded_env: std::collections::BTreeMap::from([(
                "MODE".to_owned(),
                "strict".to_owned(),
            )]),
            recorded_timeout: Some(77),
            recorded_parser: "plain".to_owned(),
            definition_current: false,
            outcome: harkness_runtime::check::CheckOutcome::Failed,
            evidence_class: harkness_runtime::check::ActivityClass::HarknessObserved,
            created_at: "2026-08-17T00:00:00.000000000Z".to_owned(),
            finished_at: Some("2026-08-17T00:00:01.000000000Z".to_owned()),
            duration_ms: Some(1_000),
            state_digest: Some("digest-1".to_owned()),
            state_head: Some("head-1".to_owned()),
            workspace_clean: Some(false),
            workspace_matches_index: Some(true),
            freshness: harkness_runtime::check::CheckFreshness::Stale {
                changed: vec!["src/main.rs".to_owned()],
            },
            diagnostics: Vec::new(),
            diagnostics_omitted: 3,
            diagnostics_scan_truncated: true,
            stdout_tail: "stdout".to_owned(),
            stderr_tail: "stderr".to_owned(),
            stdout_truncated: true,
            stderr_truncated: false,
            artifact_byte_limit: 8 * 1024 * 1024,
            stdout_artifact_truncated: true,
            stderr_artifact_truncated: false,
        };

        let state = review_map(&to_checks(
            "project-1",
            &[configuration],
            &[summary],
            false,
            "",
        ));
        let configured = review_field(&state, "configured")
            .value::<QList<QVariant>>()
            .expect("configured checks should flatten to a QVariantList");
        let configured = review_map(configured.get(0).expect("one configured check"));
        assert_eq!(review_text(&configured, "cwd"), "nested dir");
        assert_eq!(review_text(&configured, "parser"), "cargo_json");
        assert_eq!(
            review_field(&configured, "timeoutSeconds").value::<i64>(),
            Some(45)
        );
        let environment = review_field(&configured, "environment")
            .value::<QList<QVariant>>()
            .expect("environment should flatten to a QVariantList");
        assert_eq!(
            review_text(&review_map(environment.get(0).unwrap()), "name"),
            "A_FIRST"
        );
        assert_eq!(
            review_text(&review_map(environment.get(1).unwrap()), "name"),
            "Z_LAST"
        );

        let results = review_field(&state, "results")
            .value::<QList<QVariant>>()
            .expect("recorded checks should flatten to a QVariantList");
        let result = review_map(results.get(0).expect("one recorded check"));
        let recorded_command = review_field(&result, "recordedCommand")
            .value::<QList<QVariant>>()
            .expect("recorded command should flatten to a QVariantList");
        assert_eq!(
            recorded_command
                .iter()
                .map(|part| part.value::<QString>().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["old tool", "verify"]
        );
        assert_eq!(review_text(&result, "recordedCwd"), "old dir");
        assert_eq!(review_text(&result, "recordedParser"), "plain");
        assert_eq!(
            review_field(&result, "recordedTimeoutSeconds").value::<i64>(),
            Some(77)
        );
        let recorded_environment = review_field(&result, "recordedEnvironment")
            .value::<QList<QVariant>>()
            .expect("recorded environment should flatten to a QVariantList");
        assert_eq!(
            review_text(&review_map(recorded_environment.get(0).unwrap()), "value"),
            "strict"
        );
        assert_eq!(review_text(&result, "stateHead"), "head-1");
        assert_eq!(review_text(&result, "stateDigest"), "digest-1");
        assert_eq!(review_text(&result, "evidenceClass"), "harkness_observed");
        assert!(!review_flag(&result, "definitionCurrent"));
        assert!(review_flag(&result, "workspaceCleanKnown"));
        assert!(!review_flag(&result, "workspaceClean"));
        assert!(review_flag(&result, "workspaceMatchesIndexKnown"));
        assert!(review_flag(&result, "workspaceMatchesIndex"));
        assert!(review_flag(&result, "stdoutTruncated"));
        assert!(!review_flag(&result, "stderrTruncated"));
        assert!(review_flag(&result, "stdoutArtifactTruncated"));
        assert!(!review_flag(&result, "stderrArtifactTruncated"));
        assert!(review_flag(&result, "diagnosticsScanTruncated"));
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

    /// Everything the whitespace control promises, in the order the panel does
    /// it: the recomputation keeps the open file and the row the reader was on,
    /// the surface reports itself as view-only, and the one mutation it offers
    /// still writes the bytes that were on screen by going back to an exact
    /// diff first.
    #[test]
    fn the_whitespace_control_recomputes_in_place_and_still_discards_correct_bytes() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("whitespace-review");
        initialize_repository(&root);
        let path = Path::new("tracked.txt");
        // Forty lines so the file is longer than one row page, a re-indent near
        // the top and a real edit far below it.
        let original = (1..=40)
            .map(|line| format!("    line {line}\n"))
            .collect::<String>();
        commit_file(&root, path, &original, "prepare a whitespace review");
        let edited = original
            .replace("    line 3\n", "\t\tline 3\n")
            .replace("    line 30\n", "    line thirty\n");
        fs::write(root.join(path), edited.as_bytes()).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));

        let exact = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            1,
            2,
            Some(path),
        )
        .unwrap();
        assert_eq!(exact.loaded_file.as_ref().unwrap().file.hunks.len(), 2);
        // A page is far larger than this diff, so what a resume can carry here
        // is the page grid the reader was on rather than a distant offset: the
        // point is that the recomputation resumes on that page instead of
        // snapping back to row zero.
        let position = ReviewLaunchPosition {
            row_offset: 5,
            row_page_origin: 3,
        };

        // What `setReviewWhitespace` does: same selection, same preferred path,
        // same position, one different setting.
        let relaxed_whitespace =
            harkness_git::Whitespace::new(harkness_git::WhitespaceMode::IgnoreChange);
        let mut relaxed = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            relaxed_whitespace,
            3,
            4,
            Some(path),
        )
        .unwrap();
        resume_at(&mut relaxed, position);

        assert_eq!(relaxed.whitespace, relaxed_whitespace);
        assert!(!relaxed.whitespace.is_exact());
        let loaded = relaxed.loaded_file.as_ref().unwrap();
        // The file identity a reload preserves is the path: the `file-N-M`
        // tokens are minted per request precisely so a stale one from the
        // previous model cannot address the new one.
        assert_eq!(relaxed.selected_file_id, loaded.id);
        assert_eq!(
            review_path(&loaded.file),
            path,
            "the recomputation lost the open file"
        );
        assert_eq!(loaded.row_page_origin, position.row_page_origin);
        assert_eq!(
            loaded.row_offset, position.row_page_origin,
            "the recomputation threw the reader back to the top of the diff"
        );
        assert_eq!(
            loaded.file.hunks.len(),
            1,
            "the re-indent must be hidden in the relaxed view"
        );

        // Discarding the one hunk this view shows goes back to an exact diff,
        // so the re-indent it was hiding survives untouched.
        let request = ReviewHunkRequest {
            view: loaded.file.clone(),
            hunk: loaded.file.hunks[0].clone(),
        };
        let selections = exact_hunk_selections(&git, &request).unwrap();
        assert_eq!(selections.len(), 1);
        assert!(selections[0].whitespace.is_exact());
        git.discard_hunks(&selections, &harkness_git::Cancellation::default())
            .unwrap();
        assert_eq!(
            fs::read(root.join(path)).unwrap(),
            original.replace("    line 3\n", "\t\tline 3\n").as_bytes(),
            "the discard reverted the edit the reader saw and nothing else"
        );
    }

    #[test]
    fn an_unknown_whitespace_spelling_is_refused_rather_than_read_as_exact() {
        assert_eq!(
            parse_whitespace_mode("ignore_change"),
            Some(harkness_git::WhitespaceMode::IgnoreChange)
        );
        assert_eq!(
            parse_whitespace_mode("exact"),
            Some(harkness_git::WhitespaceMode::Exact)
        );
        // Neither the CLI's kebab spelling nor a made-up one may be accepted:
        // one wire spelling is what keeps a stored selection legible.
        assert_eq!(parse_whitespace_mode("ignore-change"), None);
        assert_eq!(parse_whitespace_mode(""), None);
    }

    #[test]
    fn only_unstaged_reviews_share_the_editor_working_tree() {
        assert!(!working_tree_may_differ(
            &harkness_git::DiffTarget::Unstaged
        ));
        assert!(working_tree_may_differ(&harkness_git::DiffTarget::Staged));
        assert!(working_tree_may_differ(&harkness_git::DiffTarget::Commit {
            revision: "a".repeat(40),
            parent: None,
        }));
        assert!(working_tree_may_differ(
            &harkness_git::DiffTarget::Revisions {
                old_revision: "a".repeat(40),
                new_revision: "b".repeat(40),
            }
        ));
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

    /// The hunk identities the current row window actually hands to QML.
    fn visible_hunk_ids(loaded: &super::ReviewLoadedFile) -> Vec<String> {
        review_rows(loaded)
            .iter()
            .filter_map(|row| row.value::<QMap<QMapPair_QString_QVariant>>())
            .filter(|row| {
                row.get(&QString::from("type"))
                    .and_then(|value| value.value::<QString>())
                    .is_some_and(|kind| kind.to_string() == "hunk")
            })
            .filter_map(|row| {
                row.get(&QString::from("hunkId"))
                    .and_then(|value| value.value::<QString>())
            })
            .map(|id| id.to_string())
            .collect()
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
        assert_eq!(row.github_remote, "github.com/example/sample");
        assert!(row.is_git && row.dirty);

        let map = row_map(&row);
        for key in [
            "id",
            "lockScope",
            "displayName",
            "root",
            "remote",
            "githubRemote",
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
    fn github_issue_projection_uses_bounded_graphql_page_contract() {
        let page = parse_github_issues(
            br##"{
                "data": {
                    "viewer": {"login": "octocat"},
                    "repository": {
                        "issues": {
                            "totalCount": 101,
                            "pageInfo": {"hasNextPage": true, "endCursor": "cursor-1"},
                            "nodes": [{
                                "id": "I_101",
                                "number": 7,
                                "title": "Keep issue browsing live",
                                "state": "OPEN",
                                "url": "https://github.com/example/sample/issues/7",
                                "author": {"login": "OctoCat"},
                                "updatedAt": "2026-08-12T12:00:00Z",
                                "labels": {"nodes": [{"name": "enhancement", "color": "3fb950"}]},
                                "milestone": {"title": "v1"},
                                "assignees": {"nodes": [{"login": "octocat"}, {"login": "alice"}]},
                                "comments": {"totalCount": 3}
                            }]
                        }
                    }
                }
            }"##,
            0,
        )
        .unwrap();

        assert_eq!(page.viewer, "octocat");
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].id, "I_101");
        assert_eq!(
            page.rows[0].url,
            "https://github.com/example/sample/issues/7"
        );
        assert!(page.rows[0].created_by_me && page.rows[0].assigned_to_me);
        assert_eq!(page.rows[0].labels[0].color, "#3fb950");
        assert_eq!(page.rows[0].milestone, "v1");
        assert_eq!(page.rows[0].assignees, ["@octocat", "@alice"]);
        assert_eq!(page.next_cursor.as_deref(), Some("cursor-1"));
        assert!(!page.limit_reached);
    }

    #[test]
    fn github_graphql_calls_are_pinned_to_github_dot_com() {
        let arguments = github_graphql_arguments("example", "sample", Some("cursor"));
        let hostname = arguments
            .windows(2)
            .find(|pair| pair[0] == "--hostname")
            .map(|pair| pair[1].as_str());
        assert_eq!(hostname, Some("github.com"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "endCursor=cursor")
        );
    }

    #[test]
    fn github_transport_sanitizes_output_control_environment() {
        let command = github_cli_command(Path::new("gh"), &[]);
        for name in GITHUB_CLI_REMOVED_ENV {
            assert!(command.get_envs().any(|(configured, value)| {
                configured == std::ffi::OsStr::new(name) && value.is_none()
            }));
        }
        for (name, expected) in [
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("NO_COLOR", "1"),
            ("CLICOLOR", "0"),
        ] {
            assert!(command.get_envs().any(|(configured, value)| {
                configured == std::ffi::OsStr::new(name)
                    && value == Some(std::ffi::OsStr::new(expected))
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn github_transport_refuses_oversized_stdout() {
        let result = github_cli_output_with_executable(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "head -c 2097153 /dev/zero".to_owned()],
            &harkness_git::Cancellation::default(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        assert_eq!(result.unwrap_err().kind, "github_output_too_large");
    }

    #[cfg(unix)]
    #[test]
    fn github_transport_cancels_a_running_process_group() {
        let cancellation = harkness_git::Cancellation::default();
        let trigger = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let result = github_cli_output_with_executable(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 10".to_owned()],
            &cancellation,
            started + std::time::Duration::from_secs(5),
        );
        canceller.join().unwrap();

        assert_eq!(result.unwrap_err().kind, "cancelled");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn github_transport_times_out_a_running_process_group() {
        let started = std::time::Instant::now();
        let result = github_cli_output_with_executable(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 10".to_owned()],
            &harkness_git::Cancellation::default(),
            started + std::time::Duration::from_millis(50),
        );

        assert_eq!(result.unwrap_err().kind, "github_timeout");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn github_transport_ends_descendants_before_joining_output_readers() {
        let started = std::time::Instant::now();
        let output = github_cli_output_with_executable(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 10 & printf done".to_owned()],
            &harkness_git::Cancellation::default(),
            started + std::time::Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(output, b"done");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn clearing_issue_jobs_allows_an_immediate_reopen_refresh() {
        let mut jobs = Vec::new();
        let mut next_id = 0;
        let old_job = begin_job(
            &mut jobs,
            &mut next_id,
            "issues",
            "project-1",
            "Refresh issues",
            true,
        )
        .unwrap();
        let cancellation = harkness_git::Cancellation::default();
        let mut cancellations = HashMap::from([(old_job.id, cancellation.clone())]);

        assert!(cancel_issue_jobs(&mut jobs, &mut cancellations));
        assert!(cancellation.is_cancelled());
        assert!(cancellations.is_empty());
        assert!(
            begin_job(
                &mut jobs,
                &mut next_id,
                "issues",
                "project-1",
                "Refresh issues",
                true,
            )
            .is_some()
        );
    }

    #[test]
    fn catalog_refresh_generation_rejects_an_older_completion() {
        let mut latest_request = 0;
        latest_request += 1;
        let older_request = latest_request;
        latest_request += 1;
        let current_request = latest_request;

        assert_eq!(
            accept_current_catalog_refresh(latest_request, current_request, "current"),
            Some("current")
        );
        assert_eq!(
            accept_current_catalog_refresh(latest_request, older_request, "stale"),
            None
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
            "lockScope",
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
    fn review_reads_and_repository_mutations_are_serialized() {
        for review in ["review", "review_file", "review_context"] {
            for mutation in [
                "stage",
                "unstage",
                "stage_hunk",
                "unstage_hunk",
                "commit",
                "fetch",
                "pull",
                "push",
                "checkout",
                "create_branch",
                "create_worktree",
                "reconcile_worktrees",
                "move_worktree",
                "lock_worktree",
                "unlock_worktree",
                "remove_worktree",
                "remove_managed",
            ] {
                assert!(jobs_conflict(review, mutation));
                assert!(jobs_conflict(mutation, review));
            }
        }
        assert!(jobs_conflict("stage_hunk", "commit"));
        assert!(jobs_conflict("pull", "unstage"));
        assert!(jobs_conflict("review_file", "review_context"));
        assert!(jobs_conflict("commit", "push"));
        assert!(jobs_conflict("push", "checkout"));
        assert!(jobs_conflict("push", "review"));
        assert!(jobs_conflict("push", "stage_hunk"));
        assert!(jobs_conflict("fetch", "stage"));
        assert!(jobs_conflict("status", "stage_hunk"));
        assert!(jobs_conflict("fetch", "status"));
        for read in ["branches", "worktrees"] {
            assert!(jobs_conflict(read, "checkout"));
            assert!(jobs_conflict("remove_worktree", read));
            assert!(!jobs_conflict(read, "status"));
        }
        for mutation in [
            "commit",
            "pull",
            "checkout",
            "create_branch",
            "create_worktree",
            "remove_managed",
        ] {
            assert!(jobs_conflict("history", mutation));
            assert!(jobs_conflict(mutation, "history"));
        }
        assert!(!jobs_conflict("status", "review"));
    }

    #[test]
    fn linked_worktree_jobs_share_the_parent_repository_lock_scope() {
        let parent = project(harkness_core::ProjectSource::Local, None);
        let parent_id = parent.id;
        let child = project(
            harkness_core::ProjectSource::Worktree {
                parent: parent_id,
                worktree_branch: Some("topic".to_owned()),
            },
            None,
        );
        let child_id = child.id;
        let other = project(harkness_core::ProjectSource::Local, None);
        let other_id = other.id;
        let rows = project_rows(vec![child, other, parent]);
        let scopes = project_repository_lock_scopes(&rows);
        let parent_id = parent_id.to_string();
        let child_id = child_id.to_string();
        let other_id = other_id.to_string();
        assert_eq!(scopes.get(&parent_id), Some(&parent_id));
        assert_eq!(scopes.get(&child_id), Some(&parent_id));
        assert_eq!(scopes.get(&other_id), Some(&other_id));

        let mut records = Vec::new();
        let mut next_id = 0;
        begin_job_in_scope(
            &mut records,
            &mut next_id,
            "fetch",
            &parent_id,
            &parent_id,
            "Fetch",
            true,
        )
        .unwrap();
        assert_eq!(
            conflicting_repository_job(&records, &scopes[&child_id], "move_worktree")
                .map(|job| job.kind.as_str()),
            Some("fetch")
        );
        assert_eq!(
            conflicting_repository_job(&records, &scopes[&child_id], "lock_worktree")
                .map(|job| job.kind.as_str()),
            Some("fetch")
        );
        assert!(
            conflicting_repository_job(&records, &scopes[&other_id], "create_worktree").is_none()
        );
    }

    #[test]
    fn separately_imported_linked_worktrees_share_the_git_common_directory_scope() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("independent-parent");
        initialize_repository(&parent_root);
        let linked_root = fixture.path().join("independent-linked");
        Repository::open(&parent_root)
            .unwrap()
            .worktree("independent-linked", &linked_root, None)
            .unwrap();
        let other_root = fixture.path().join("independent-other");
        initialize_repository(&other_root);

        let mut parent = project(harkness_core::ProjectSource::Local, None);
        parent.root = parent_root.clone();
        parent.git = harkness_git::GitService::new(&parent_root, fixture.path().join("data"))
            .status()
            .unwrap();
        let parent_id = parent.id.to_string();
        let mut linked = project(harkness_core::ProjectSource::Local, None);
        linked.root = linked_root.clone();
        linked.git = harkness_git::GitService::new(&linked_root, fixture.path().join("data"))
            .status()
            .unwrap();
        let linked_id = linked.id.to_string();
        let mut other = project(harkness_core::ProjectSource::Local, None);
        other.root = other_root.clone();
        other.git = harkness_git::GitService::new(&other_root, fixture.path().join("data"))
            .status()
            .unwrap();
        let other_id = other.id.to_string();

        let rows = project_rows(vec![linked, other, parent]);
        let scopes = project_repository_lock_scopes(&rows);
        assert_eq!(scopes[&linked_id], scopes[&parent_id]);
        assert_ne!(scopes[&other_id], scopes[&parent_id]);
        assert_eq!(
            harkness_git::repository_identity(&linked_root).unwrap(),
            harkness_git::repository_identity(&parent_root).unwrap()
        );
    }

    #[test]
    fn unavailable_worktrees_inherit_the_available_parent_git_scope() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("available-parent");
        initialize_repository(&parent_root);
        let mut parent = project(harkness_core::ProjectSource::Local, None);
        parent.root = parent_root.clone();
        parent.git = harkness_git::GitService::new(&parent_root, fixture.path().join("data"))
            .status()
            .unwrap();
        let parent_id = parent.id;
        let mut child = project(
            harkness_core::ProjectSource::Worktree {
                parent: parent_id,
                worktree_branch: Some("agent/missing".to_owned()),
            },
            None,
        );
        child.root = fixture.path().join("missing-worktree");
        child.available = false;
        let child_id = child.id.to_string();
        let parent_id = parent_id.to_string();

        let rows = project_rows(vec![child, parent]);
        let scopes = project_repository_lock_scopes(&rows);
        let lifecycle_scopes = project_worktree_lifecycle_lock_scopes(&rows);
        let repository_scope = harkness_git::repository_identity(&parent_root).unwrap();
        assert_eq!(scopes[&parent_id], repository_scope);
        assert_eq!(scopes[&child_id], repository_scope);
        assert_ne!(scopes[&child_id], parent_id);
        assert_eq!(
            worktree_job_lock_scope(
                &lifecycle_scopes,
                &child_id,
                &parent_id,
                Some("stale-opened-scope".to_owned()),
            ),
            repository_scope
        );

        let jobs = vec![super::JobRecord {
            id: "job-parent-fetch".to_owned(),
            kind: "fetch".to_owned(),
            project_id: parent_id,
            lock_scope: repository_scope.clone(),
            label: "Fetch".to_owned(),
            progress: "Starting…".to_owned(),
            cancellable: true,
        }];
        assert!(conflicting_repository_job(&jobs, &repository_scope, "lock_worktree").is_some());
    }

    #[test]
    fn available_replaced_worktrees_use_their_actual_git_scope_for_repository_jobs() {
        let fixture = TempDir::new().unwrap();
        let parent_root = fixture.path().join("catalog-parent");
        let replacement_parent_root = fixture.path().join("replacement-parent");
        let replacement_worktree_root = fixture.path().join("replacement-worktree");
        initialize_repository(&parent_root);
        initialize_repository(&replacement_parent_root);
        Repository::open(&replacement_parent_root)
            .unwrap()
            .worktree("replacement-worktree", &replacement_worktree_root, None)
            .unwrap();

        let data_dir = fixture.path().join("data");
        let mut parent = project(harkness_core::ProjectSource::Local, None);
        parent.root = parent_root.clone();
        parent.git = harkness_git::GitService::new(&parent_root, &data_dir)
            .status()
            .unwrap();
        let parent_id = parent.id;
        let mut replacement_parent = project(harkness_core::ProjectSource::Local, None);
        replacement_parent.root = replacement_parent_root.clone();
        replacement_parent.git = harkness_git::GitService::new(&replacement_parent_root, &data_dir)
            .status()
            .unwrap();
        let replacement_parent_id = replacement_parent.id.to_string();
        let mut child = project(
            harkness_core::ProjectSource::Worktree {
                parent: parent_id,
                worktree_branch: Some("agent/original".to_owned()),
            },
            None,
        );
        child.root = replacement_worktree_root.clone();
        child.git = harkness_git::GitService::new(&replacement_worktree_root, &data_dir)
            .status()
            .unwrap();
        let child_id = child.id.to_string();
        let parent_id = parent_id.to_string();

        let rows = project_rows(vec![child, replacement_parent, parent]);
        let scopes = project_repository_lock_scopes(&rows);
        let lifecycle_scopes = project_worktree_lifecycle_lock_scopes(&rows);
        let parent_scope = harkness_git::repository_identity(&parent_root).unwrap();
        let replacement_scope =
            harkness_git::repository_identity(&replacement_parent_root).unwrap();
        assert_eq!(scopes[&replacement_parent_id], replacement_scope);
        assert_eq!(scopes[&child_id], replacement_scope);
        assert_ne!(scopes[&child_id], parent_scope);
        assert_eq!(lifecycle_scopes[&child_id], parent_scope);
        assert_eq!(
            worktree_job_lock_scope(
                &lifecycle_scopes,
                &child_id,
                &parent_id,
                Some(replacement_scope),
            ),
            parent_scope
        );
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
        let first_id =
            register_path_selection(&mut backend, "project-1", &first, None, false, false);
        let repeated_id =
            register_path_selection(&mut backend, "project-1", &first, None, false, false);
        let second_id =
            register_path_selection(&mut backend, "project-1", &second, None, false, false);

        assert_eq!(first_id, repeated_id);
        assert_ne!(first_id, second_id);
        let first_review_id = register_review_path_identity(&mut backend, "project-1", &first);
        let repeated_review_id = register_review_path_identity(&mut backend, "project-1", &first);
        let second_review_id = register_review_path_identity(&mut backend, "project-1", &second);
        assert_eq!(first_review_id, repeated_review_id);
        assert_ne!(first_review_id, second_review_id);
        assert!(
            resolve_path_selection(&backend, "project-1", &first_review_id).is_err(),
            "a display-only review identity must not authorize a path mutation"
        );
        assert_eq!(
            resolve_path_selection(&backend, "project-1", &first_id)
                .unwrap()
                .path,
            first
        );
        assert!(resolve_path_selection(&backend, "project-2", &first_id).is_err());

        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("byte-path-repository");
        initialize_repository(&root);
        fs::write(root.join(&first), b"byte-exact content\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            1,
            2,
            Some(&first),
        )
        .unwrap();
        assert_eq!(
            review.selected_file_id,
            review.loaded_file.as_ref().unwrap().id
        );
        assert_eq!(review.loaded_file.unwrap().file.new_path, Some(first));
    }

    /// A status projection with one entry per `(path, rename source)` pair.
    fn status_row(entries: &[(&str, Option<&str>, bool)]) -> GitStateRow {
        GitStateRow::from_status(
            "project-1".to_owned(),
            harkness_git::DetailedStatus {
                head: harkness_git::HeadState::Branch {
                    name: "topic".to_owned(),
                },
                upstream: None,
                pending: None,
                entries: entries
                    .iter()
                    .map(|(path, rename_source, staged)| harkness_git::StatusEntry {
                        path: PathBuf::from(path),
                        staged: rename_source
                            .filter(|_| *staged)
                            .map(|_| harkness_git::FileChange::Renamed),
                        unstaged: if *staged {
                            None
                        } else {
                            Some(
                                rename_source.map_or(harkness_git::FileChange::Modified, |_| {
                                    harkness_git::FileChange::Renamed
                                }),
                            )
                        },
                        rename_source: rename_source.map(PathBuf::from),
                        conflicted: false,
                    })
                    .collect(),
            },
        )
    }

    #[test]
    fn rename_discard_descriptions_name_source_and_destination() {
        let status = status_row(&[("new-name.txt", Some("old-name.txt"), false)]);
        let description = status_discard_description(&status.entries[0]).unwrap();
        assert_eq!(
            description.paths(),
            &[PathBuf::from("new-name.txt"), PathBuf::from("old-name.txt")]
        );

        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("rename-description-repository");
        initialize_repository(&root);
        let old_path = Path::new("old-name.txt");
        let new_path = Path::new("new-name.txt");
        commit_file(&root, old_path, "original\n", "add rename fixture");
        fs::rename(root.join(old_path), root.join(new_path)).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        git.stage([old_path, new_path], &harkness_git::Cancellation::default())
            .unwrap();
        let file = git
            .diff(
                harkness_git::DiffTarget::Staged,
                &harkness_git::DiffOptions::default(),
            )
            .unwrap()
            .into_iter()
            .find(|file| file.old_path.as_deref() == Some(old_path))
            .unwrap();
        let description = review_file_discard_description(&file).unwrap();
        assert_eq!(
            description.paths(),
            &[PathBuf::from("new-name.txt"), PathBuf::from("old-name.txt")]
        );
    }

    #[test]
    fn unchanged_status_refreshes_reuse_discard_snapshots_and_keep_stale_refusals() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("discard-snapshot-cache-repository");
        initialize_repository(&root);
        let path = Path::new("cached.txt");
        commit_file(&root, path, "original\n", "add cache fixture");
        fs::write(root.join(path), "changed once\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let cache = super::DiscardSnapshotCache::default();
        let captures = std::cell::Cell::new(0);

        let refresh = || {
            let status = git
                .detailed_status(&harkness_git::Cancellation::default())
                .unwrap();
            let mut state = GitStateRow::from_status("project-1".to_owned(), status);
            attach_discard_snapshots_with(&root, &mut state, &cache, |paths| {
                captures.set(captures.get() + 1);
                git.discard_snapshot(paths).ok()
            });
            state
        };

        let first = refresh();
        assert_eq!(captures.get(), 1);
        let second = refresh();
        assert_eq!(
            captures.get(),
            1,
            "an unchanged poll must not hash the file again"
        );
        let stale_snapshot = second.entries[0].discard_snapshot.clone().unwrap();
        drop(first);

        fs::write(root.join(path), "changed twice and longer\n").unwrap();
        let error = git
            .restore_tracked_if_unchanged(
                [path],
                harkness_git::TrackedRestoreSource::Index,
                &stale_snapshot,
                &harkness_git::Cancellation::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            harkness_git::GitError::StaleDiscardSelection
        ));

        let _third = refresh();
        assert_eq!(
            captures.get(),
            2,
            "a metadata change must refresh the snapshot"
        );
    }

    #[test]
    fn an_unchanged_status_read_projects_identically() {
        let mut backend = HarknessBackendRust::default();
        let row = status_row(&[("first.txt", None, false), ("second.txt", None, false)]);

        let first = replace_status_path_selections(&mut backend, &row);
        let second = replace_status_path_selections(&mut backend, &row);

        // Minting fresh tokens per read made every poll a change, which rebuilt
        // the whole Changes list on a timer.
        assert_eq!(first, second);
        assert_eq!(to_git(&row, &first), to_git(&row, &second));
    }

    #[test]
    fn a_path_that_leaves_the_status_takes_its_token_with_it() {
        let mut backend = HarknessBackendRust::default();
        let tokens = replace_status_path_selections(
            &mut backend,
            &status_row(&[("first.txt", None, false), ("second.txt", None, false)]),
        );

        let remaining = replace_status_path_selections(
            &mut backend,
            &status_row(&[("second.txt", None, false)]),
        );

        // The survivor keeps the identity the list already scrolled to, and
        // the committed path's token stops authorizing anything.
        assert_eq!(remaining, vec![tokens[1].clone()]);
        assert!(resolve_path_selection(&backend, "project-1", &tokens[0]).is_err());
    }

    #[test]
    fn an_edited_file_is_re_minted_so_a_pending_confirmation_refuses() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("re-mint-repository");
        initialize_repository(&root);
        let path = Path::new("confirmed.txt");
        commit_file(&root, path, "original\n", "add confirmation fixture");
        fs::write(root.join(path), "what the user confirmed\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let cache = super::DiscardSnapshotCache::default();
        let read = || {
            let status = git
                .detailed_status(&harkness_git::Cancellation::default())
                .unwrap();
            let mut state = GitStateRow::from_status("project-1".to_owned(), status);
            attach_discard_snapshots(&git, &mut state, &cache);
            state
        };
        let mut backend = HarknessBackendRust::default();

        let confirmed = replace_status_path_selections(&mut backend, &read());
        let unchanged = replace_status_path_selections(&mut backend, &read());
        fs::write(root.join(path), "content the user never saw\n").unwrap();
        let edited = replace_status_path_selections(&mut backend, &read());

        // An idle poll leaves the row alone, so the list does not churn.
        assert_eq!(confirmed, unchanged);
        // An edit under an open discard prompt does not: the token the prompt
        // was opened with must stop resolving, or confirming it would apply an
        // unrecoverable operation to content that arrived afterwards.
        assert_ne!(confirmed, edited);
        assert!(resolve_path_selection(&backend, "project-1", &confirmed[0]).is_err());
    }

    #[test]
    fn a_mutation_projects_its_own_status_onto_the_catalog_row() {
        let row = ProjectRow {
            id: "project-1".to_owned(),
            lock_scope: "scope".to_owned(),
            lock_scope_resolved: true,
            display_name: "sample".to_owned(),
            root: "/tmp/sample".to_owned(),
            remote: String::new(),
            github_remote: String::new(),
            branch: "main".to_owned(),
            managed: false,
            worktree: false,
            parent_id: String::new(),
            parent_name: String::new(),
            created_branch: String::new(),
            available: true,
            is_git: true,
            dirty: true,
        };
        let projection = CatalogGitProjection::from_state(&status_row(&[]));

        let updated = projection
            .applied_to(&to_map(&row), "project-1")
            .expect("the row is the acting project");

        assert_eq!(projection.branch, "topic");
        // An empty status entry list is exactly a clean working tree.
        assert!(!projection.dirty);
        let map = updated
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("a project map");
        assert_eq!(
            map.get(&QString::from("branch"))
                .and_then(|value| value.value::<QString>())
                .map(|branch| branch.to_string()),
            Some("topic".to_owned())
        );
        assert_eq!(
            map.get(&QString::from("dirty"))
                .and_then(|value| value.value::<bool>()),
            Some(false)
        );
        // Nothing a status read cannot answer is touched.
        assert_eq!(
            map.get(&QString::from("displayName")),
            to_map(&row)
                .value::<QMap<QMapPair_QString_QVariant>>()
                .and_then(|original| original.get(&QString::from("displayName")))
        );
        assert!(projection.applied_to(&to_map(&row), "project-2").is_none());
    }

    #[test]
    fn an_unborn_head_projects_no_branch_the_catalog_would_contradict() {
        let unborn = GitStateRow::from_status(
            "project-1".to_owned(),
            harkness_git::DetailedStatus {
                head: harkness_git::HeadState::Unborn {
                    branch: Some("main".to_owned()),
                },
                upstream: None,
                pending: None,
                entries: Vec::new(),
            },
        );

        let projection = CatalogGitProjection::from_state(&unborn);

        // The Git state names the branch a first commit would create, and the
        // panel header says so. The catalog reports no branch until that
        // commit exists, so writing the name would be undone by the next full
        // reload and read as the branch changing on its own.
        assert_eq!(unborn.branch, "main");
        assert!(projection.branch.is_empty());
    }

    #[test]
    fn a_complete_selection_commits_the_working_tree_instead_of_naming_every_path() {
        let mut backend = HarknessBackendRust::default();
        let tokens = replace_status_path_selections(
            &mut backend,
            &status_row(&[("first.txt", None, false), ("second.txt", None, false)]),
        );

        assert_eq!(
            resolve_commit_scope(&backend, "project-1", &tokens.join("\n"), false).unwrap(),
            harkness_git::CommitScope::WorkingTree
        );
        // Repeats must not be counted as coverage, or a selection of one path
        // named twice would commit the whole working tree.
        assert_eq!(
            resolve_commit_scope(
                &backend,
                "project-1",
                &format!("{}\n{}", tokens[0], tokens[0]),
                false,
            )
            .unwrap(),
            harkness_git::CommitScope::Paths(vec![PathBuf::from("first.txt")])
        );
    }

    #[test]
    fn a_partial_selection_commits_exactly_the_selected_paths() {
        let mut backend = HarknessBackendRust::default();
        let tokens = replace_status_path_selections(
            &mut backend,
            &status_row(&[
                ("first.txt", None, false),
                ("second.txt", None, false),
                ("third.txt", None, false),
            ]),
        );

        assert_eq!(
            resolve_commit_scope(
                &backend,
                "project-1",
                &format!("{}\n{}", tokens[0], tokens[2]),
                false,
            )
            .unwrap(),
            harkness_git::CommitScope::Paths(vec![
                PathBuf::from("first.txt"),
                PathBuf::from("third.txt"),
            ])
        );
    }

    #[test]
    fn committing_a_rename_names_both_of_its_native_paths() {
        let mut backend = HarknessBackendRust::default();
        let tokens = replace_status_path_selections(
            &mut backend,
            &status_row(&[
                ("new-name.txt", Some("old-name.txt"), false),
                ("other.txt", None, false),
            ]),
        );

        let scope = resolve_commit_scope(&backend, "project-1", &tokens[0], false).unwrap();

        assert_eq!(
            scope,
            harkness_git::CommitScope::Paths(vec![
                PathBuf::from("new-name.txt"),
                PathBuf::from("old-name.txt"),
            ]),
            "a rename committed by its destination alone would leave the source standing"
        );
    }

    #[test]
    fn an_empty_selection_is_a_message_amend_and_otherwise_a_refusal() {
        let mut backend = HarknessBackendRust::default();
        replace_status_path_selections(&mut backend, &status_row(&[("first.txt", None, false)]));

        assert_eq!(
            resolve_commit_scope(&backend, "project-1", "", true).unwrap(),
            harkness_git::CommitScope::Index,
            "amending nothing rewrites the previous message against its own tree"
        );
        assert!(resolve_commit_scope(&backend, "project-1", "", false).is_err());
        assert!(resolve_commit_scope(&backend, "project-1", "\n\n", false).is_err());
    }

    #[test]
    fn a_stale_or_foreign_token_refuses_the_whole_commit() {
        let mut backend = HarknessBackendRust::default();
        let tokens = replace_status_path_selections(
            &mut backend,
            &status_row(&[("first.txt", None, false), ("second.txt", None, false)]),
        );

        // Silently dropping the unknown entry would commit a strict subset of
        // what the user ticked, which is the one outcome worse than refusing.
        assert!(
            resolve_commit_scope(
                &backend,
                "project-1",
                &format!("{}\npath-does-not-exist", tokens[0]),
                false,
            )
            .is_err()
        );
        assert!(resolve_commit_scope(&backend, "project-2", &tokens[0], false).is_err());
    }

    #[test]
    fn a_path_capability_only_ever_names_the_path_it_was_minted_for() {
        let source = Path::new("source.txt");
        let destination = Path::new("copy.txt");
        let mut backend = HarknessBackendRust::default();

        let copy = register_path_selection(
            &mut backend,
            "project-1",
            destination,
            Some(source),
            false,
            false,
        );
        let copy = resolve_path_selection(&backend, "project-1", &copy).unwrap();
        assert_eq!(copy.path, destination.to_path_buf());

        let staged_rename = register_path_selection(
            &mut backend,
            "project-1",
            destination,
            Some(source),
            true,
            false,
        );
        let staged_rename = resolve_path_selection(&backend, "project-1", &staged_rename).unwrap();
        assert_eq!(staged_rename.path, destination.to_path_buf());
        // Same destination, different rename state: the two are distinct
        // selections, which is what stops a token minted before the rename was
        // recorded from resolving afterwards.
        assert_ne!(copy, staged_rename);
    }

    #[test]
    fn status_replacement_revokes_old_path_capabilities() {
        let mut backend = HarknessBackendRust::default();
        let rename = GitStateRow::from_status(
            "project-1".to_owned(),
            harkness_git::DetailedStatus {
                head: harkness_git::HeadState::Branch {
                    name: "topic".to_owned(),
                },
                upstream: None,
                pending: None,
                entries: vec![harkness_git::StatusEntry {
                    path: "new.txt".into(),
                    staged: None,
                    unstaged: Some(harkness_git::FileChange::Renamed),
                    rename_source: Some("old.txt".into()),
                    conflicted: false,
                }],
            },
        );
        let stale_token = replace_status_path_selections(&mut backend, &rename).remove(0);
        assert_eq!(
            resolve_path_selection(&backend, "project-1", &stale_token)
                .unwrap()
                .path,
            PathBuf::from("new.txt")
        );

        let replacement = GitStateRow::from_status(
            "project-1".to_owned(),
            harkness_git::DetailedStatus {
                head: harkness_git::HeadState::Branch {
                    name: "topic".to_owned(),
                },
                upstream: None,
                pending: None,
                entries: vec![harkness_git::StatusEntry {
                    path: "old.txt".into(),
                    staged: None,
                    unstaged: Some(harkness_git::FileChange::Modified),
                    rename_source: None,
                    conflicted: false,
                }],
            },
        );
        let current_token = replace_status_path_selections(&mut backend, &replacement).remove(0);
        assert!(resolve_path_selection(&backend, "project-1", &stale_token).is_err());
        assert_eq!(backend.path_selections.len(), 1);
        assert_eq!(backend.path_selection_ids.len(), 1);
        assert_eq!(
            resolve_path_selection(&backend, "project-1", &current_token)
                .unwrap()
                .path,
            PathBuf::from("old.txt")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_worker_cancellation_reaches_running_git() {
        use std::{
            os::unix::fs::PermissionsExt,
            thread,
            time::{Duration, Instant},
        };

        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("cancel-status-repository");
        initialize_repository(&root);
        let marker = fixture.path().join("status-running");
        let shim = fixture.path().join("hanging-status-git");
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\n: > {}\nwhile true; do sleep 0.05; done\n",
                quote(&marker),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&shim).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shim, permissions).unwrap();

        let cancellation = harkness_git::Cancellation::default();
        let worker_cancellation = cancellation.clone();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"))
            .with_git_executable(shim);
        let worker = thread::spawn(move || {
            run_git_status_with_git("project-1".to_owned(), Ok(git), &worker_cancellation)
        });
        for _ in 0..500 {
            if marker.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let status_started = marker.exists();
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let result = worker.join().unwrap();

        assert!(status_started, "the Git status shim never started");
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "status cancellation did not release the worker promptly"
        );
        assert_eq!(result.message.unwrap_err().kind, "cancelled");
        assert!(result.state.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_worker_refreshes_after_git_wrote_the_index() {
        use std::{os::unix::fs::PermissionsExt, thread, time::Duration};

        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("cancel-after-write-repository");
        initialize_repository(&root);
        let path = Path::new("cancelled.txt");
        commit_file(&root, path, "before\n", "add cancellation fixture");
        fs::write(root.join(path), "after\n").unwrap();
        let data_dir = fixture.path().join("data");

        let git_binary = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("Git must be available to the workspace test");
        let marker = fixture.path().join("index-written");
        let shim = fixture.path().join("git-after-write");
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\nwrite=0\nfor argument in \"$@\"; do\n  if [ \"$argument\" = \"add\" ]; then write=1; fi\ndone\n{} \"$@\"\nresult=$?\nif [ \"$write\" = \"1\" ]; then\n  : > {}\n  while true; do sleep 1; done\nfi\nexit \"$result\"\n",
                quote(&git_binary),
                quote(&marker),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&shim).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shim, permissions).unwrap();

        let cancellation = harkness_git::Cancellation::default();
        let worker_cancellation = cancellation.clone();
        let git = harkness_git::GitService::new(&root, data_dir).with_git_executable(shim);
        // A commit stages before it records, so the shim's hang lands between
        // the two. That is exactly the window this test is about: Git has
        // already written the index when the cancellation arrives.
        let worker = thread::spawn(move || {
            run_git_operation_with_git(
                "project-1".to_owned(),
                Ok(git),
                &worker_cancellation,
                |git, cancellation| {
                    git.commit(
                        "cancelled commit",
                        &harkness_git::CommitOptions::default()
                            .with_scope(harkness_git::CommitScope::WorkingTree),
                        cancellation,
                    )
                    .map_err(GitFailure::from)?;
                    Ok("Committed cancelled.txt".to_owned())
                },
            )
        });
        for _ in 0..500 {
            if marker.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker.exists(),
            "the Git shim never completed the index write"
        );
        cancellation.cancel();
        let result = worker.join().unwrap();
        assert_eq!(result.message.unwrap_err().kind, "cancelled");
        let state = result
            .state
            .as_ref()
            .expect("the worker must refresh status with a fresh token");
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.path == path && !entry.staged.is_empty()),
            "the refreshed status must show the index write the cancelled commit already made"
        );
    }

    #[test]
    fn review_path_tokens_keep_scroll_identity_across_a_partially_staged_rename() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("rename-scroll-repository");
        initialize_repository(&root);
        let old_path = Path::new("old-name.txt");
        let new_path = Path::new("new-name.txt");
        let original = (1..=20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        commit_file(&root, old_path, &original, "add rename fixture");
        fs::rename(root.join(old_path), root.join(new_path)).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        git.stage([old_path, new_path], &harkness_git::Cancellation::default())
            .unwrap();
        fs::write(
            root.join(new_path),
            original.replace("line 10\n", "line 10 changed\n"),
        )
        .unwrap();

        let unstaged = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            27,
            28,
            Some(new_path),
        )
        .unwrap();
        let unstaged_file = &unstaged.loaded_file.as_ref().unwrap().file;
        assert_eq!(display_diff_path(unstaged_file), "new-name.txt");
        // Partial staging is not a GUI action any more, but it remains a state
        // the working tree can be found in — the CLI stages hunks — and the
        // review path token has to keep its identity across it.
        git.stage_hunks(
            std::slice::from_ref(&harkness_git::HunkSelection::new(
                exact(unstaged_file),
                &unstaged_file.hunks[0],
            )),
            &harkness_git::Cancellation::default(),
        )
        .unwrap();

        let staged = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Staged,
            harkness_git::Whitespace::EXACT,
            29,
            30,
            Some(new_path),
        )
        .unwrap();
        let staged_file = &staged.loaded_file.as_ref().unwrap().file;
        assert_eq!(
            display_diff_path(staged_file),
            "old-name.txt → new-name.txt"
        );
        assert_eq!(review_path(unstaged_file), review_path(staged_file));

        let mut backend = HarknessBackendRust::default();
        let unstaged_token =
            register_review_path_identity(&mut backend, "project-1", &review_path(unstaged_file));
        let staged_token =
            register_review_path_identity(&mut backend, "project-1", &review_path(staged_file));
        assert_eq!(staged_token, unstaged_token);
        assert!(resolve_path_selection(&backend, "project-1", &staged_token).is_err());
    }

    #[test]
    fn review_surface_names_binary_and_oversize_omissions() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("summary-repository");
        initialize_repository(&root);
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));

        fs::write(root.join("binary.dat"), [0_u8, 1, 2, 0, 3]).unwrap();
        let binary = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            15,
            16,
            Some(Path::new("binary.dat")),
        )
        .unwrap();
        let binary = &binary.loaded_file.unwrap().file;
        assert!(binary.binary);
        assert!(review_content_summary(binary).starts_with("Binary file"));

        fs::write(
            root.join("large.txt"),
            vec![b'x'; usize::try_from(harkness_git::DEFAULT_MAX_DIFF_FILE_SIZE).unwrap() + 1],
        )
        .unwrap();
        let large = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            17,
            18,
            Some(Path::new("large.txt")),
        )
        .unwrap();
        let large = &large.loaded_file.unwrap().file;
        assert!(matches!(
            large.omission.as_ref(),
            Some(harkness_git::DiffOmission::FileTooLarge { .. })
        ));
        assert!(review_content_summary(large).starts_with("File too large"));
    }

    #[test]
    fn five_thousand_changed_lines_render_from_the_review_surface() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("virtualized-review-repository");
        initialize_repository(&root);
        let path = Path::new("many-lines.txt");
        let original = (0..5_000)
            .map(|line| format!("old {line}\n"))
            .collect::<String>();
        let modified = (0..5_000)
            .map(|line| format!("new {line}\n"))
            .collect::<String>();
        commit_file(&root, path, &original, "add many lines");
        fs::write(root.join(path), &modified).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            21,
            22,
            Some(path),
        )
        .unwrap();
        let loaded = review.loaded_file.as_ref().unwrap();
        assert!(review_content_summary(&loaded.file).is_empty());
        assert!(review_rows(loaded).len() > 5_000);
    }

    #[test]
    fn oversized_review_models_use_bounded_windows_and_visible_capabilities() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("paged-review-repository");
        initialize_repository(&root);
        let path = Path::new("paged-lines.txt");
        let original = (0..4)
            .flat_map(|chunk| {
                (0..5_000)
                    .map(move |line| format!("old {chunk}-{line}\n"))
                    .chain((0..10).map(move |line| format!("separator {chunk}-{line}\n")))
            })
            .collect::<String>();
        let modified = (0..4)
            .flat_map(|chunk| {
                (0..5_000)
                    .map(move |line| format!("new {chunk}-{line}\n"))
                    .chain((0..10).map(move |line| format!("separator {chunk}-{line}\n")))
            })
            .collect::<String>();
        commit_file(&root, path, &original, "add paged lines");
        fs::write(root.join(path), &modified).unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let mut review = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            23,
            24,
            Some(path),
        )
        .unwrap();
        let total = review_row_count(review.loaded_file.as_ref().unwrap());
        assert!(total > REVIEW_ROW_PAGE_SIZE * 3);
        let all_hunk_ids = review
            .loaded_file
            .as_ref()
            .unwrap()
            .hunks
            .iter()
            .map(|hunk| hunk.id.clone())
            .collect::<Vec<_>>();
        assert!(all_hunk_ids.len() >= 4);

        // A hunk beyond the first row window is not addressable from it: the
        // window is the whole of what QML can see at a time.
        let hidden_id = all_hunk_ids.last().unwrap();
        assert!(!visible_hunk_ids(review.loaded_file.as_ref().unwrap()).contains(hidden_id));

        let page_count = total.div_ceil(REVIEW_ROW_PAGE_SIZE);
        let mut seen_hunks = std::collections::HashSet::new();
        for page in 0..page_count {
            let loaded = review.loaded_file.as_ref().unwrap();
            let offset = page * REVIEW_ROW_PAGE_SIZE;
            assert_eq!(loaded.row_offset, offset);
            let rows = review_rows(loaded);
            let content_rows = (total - offset).min(REVIEW_ROW_PAGE_SIZE);
            let controls = usize::from(page > 0) + usize::from(page + 1 < page_count);
            assert_eq!(
                rows.len(),
                isize::try_from(content_rows + controls).unwrap()
            );
            assert!(rows.len() <= isize::try_from(REVIEW_ROW_PAGE_SIZE + 2).unwrap());

            let page_rows = rows
                .iter()
                .filter_map(|row| row.value::<QMap<QMapPair_QString_QVariant>>())
                .filter(|row| {
                    row.get(&QString::from("type"))
                        .and_then(|value| value.value::<QString>())
                        .is_some_and(|kind| kind.to_string() == "page")
                })
                .filter_map(|row| {
                    let direction = row
                        .get(&QString::from("direction"))
                        .and_then(|value| value.value::<QString>())
                        .map(|direction| direction.to_string())?;
                    let hunk_available = row
                        .get(&QString::from("hunkAvailable"))
                        .and_then(|value| value.value::<bool>())?;
                    Some((direction, hunk_available))
                })
                .collect::<Vec<_>>();
            let directions = page_rows
                .iter()
                .map(|(direction, _)| direction.clone())
                .collect::<Vec<_>>();
            assert_eq!(directions.contains(&"previous".to_owned()), page > 0);
            assert_eq!(
                directions.contains(&"next".to_owned()),
                page + 1 < page_count
            );
            for (direction, hunk_available) in page_rows {
                let expected = if direction == "previous" {
                    review_hunk_exists_where(loaded, |row_index| row_index < offset)
                } else {
                    review_hunk_exists_where(loaded, |row_index| {
                        row_index >= offset.saturating_add(REVIEW_ROW_PAGE_SIZE)
                    })
                };
                assert_eq!(hunk_available, expected);
            }

            seen_hunks.extend(visible_hunk_ids(loaded));
            if page + 1 < page_count {
                assert!(advance_review_row_window(
                    review.loaded_file.as_mut().unwrap()
                ));
            }
        }
        assert_eq!(seen_hunks.len(), all_hunk_ids.len());
        assert!(!advance_review_row_window(
            review.loaded_file.as_mut().unwrap()
        ));
        for page in (1..page_count).rev() {
            assert!(retreat_review_row_window(
                review.loaded_file.as_mut().unwrap()
            ));
            assert_eq!(
                review.loaded_file.as_ref().unwrap().row_offset,
                (page - 1) * REVIEW_ROW_PAGE_SIZE
            );
        }
        assert!(!retreat_review_row_window(
            review.loaded_file.as_mut().unwrap()
        ));
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
            harkness_git::Whitespace::EXACT,
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
        let loaded = load_review_file_with_git(
            &git,
            review.target.as_ref().unwrap(),
            harkness_git::Whitespace::EXACT,
            &review.files[0],
            5,
        )
        .unwrap();
        // The identity pass exhausts its budget, so the hunk only exists once
        // the file is loaded on demand with a real one.
        assert_eq!(visible_hunk_ids(&loaded).len(), 1);
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
            harkness_git::Whitespace::EXACT,
            5,
        )
        .unwrap();
        assert_eq!(review.files.len(), 1);
        assert!(review.loaded_file.is_none());
        assert!(review.files[0].file.hunks.is_empty());

        let target = review.target.as_ref().unwrap();
        let loaded = load_review_file_with_git(
            &git,
            target,
            harkness_git::Whitespace::EXACT,
            &review.files[0],
            6,
        )
        .unwrap();
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
        let hunk = &loaded.file.hunks[0];
        let deletion_index = hunk
            .lines
            .iter()
            .position(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
            .unwrap();
        let addition_index = hunk
            .lines
            .iter()
            .position(|line| matches!(line.kind, harkness_git::DiffLineKind::Addition))
            .unwrap();
        assert_eq!(
            best_working_tree_line(hunk, deletion_index),
            addition.new_line_number.unwrap()
        );
        assert_eq!(
            best_working_tree_line(hunk, addition_index),
            addition.new_line_number.unwrap()
        );
    }

    #[test]
    fn review_line_navigation_uses_existing_coordinates_around_unpaired_deletions() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("review-line-navigation-repository");
        initialize_repository(&root);
        let path = Path::new("lines.txt");
        commit_file(&root, path, "first\nremoved\nlast\n", "add lines");
        fs::write(root.join(path), "first\nlast\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let files = git
            .diff(
                harkness_git::DiffTarget::Unstaged,
                &harkness_git::DiffOptions::default(),
            )
            .unwrap();
        let hunk = &files[0].hunks[0];
        let deletion_index = hunk
            .lines
            .iter()
            .position(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
            .unwrap();
        let following_context = hunk.lines[deletion_index + 1..]
            .iter()
            .find_map(|line| line.new_line_number)
            .unwrap();
        assert_eq!(
            best_working_tree_line(hunk, deletion_index),
            following_context
        );

        fs::write(root.join(path), "first\n").unwrap();
        let files = git
            .diff(
                harkness_git::DiffTarget::Unstaged,
                &harkness_git::DiffOptions::default(),
            )
            .unwrap();
        let hunk = &files[0].hunks[0];
        let final_deletion_index = hunk
            .lines
            .iter()
            .rposition(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
            .unwrap();
        assert_eq!(best_working_tree_line(hunk, final_deletion_index), 1);
    }

    fn segment_shapes(
        bytes: &[u8],
        ranges: Option<&[harkness_git::IntraLineRange]>,
    ) -> Vec<(String, bool, &'static str, &'static str)> {
        text_segments(bytes, ranges)
            .into_iter()
            .map(|segment| {
                (
                    String::from_utf8_lossy(segment.text).into_owned(),
                    segment.changed,
                    segment.whitespace.name(),
                    segment.zone.name(),
                )
            })
            .collect()
    }

    #[test]
    fn diff_line_segments_cut_out_the_runs_the_reader_cannot_see() {
        // One run per whitespace byte kind, so a tab is never handed over as
        // the same run as the spaces beside it. Interior whitespace stays in
        // its content run: the QML lexer reads a segment whole, and a string
        // literal split at its space would stop being recognisable as one.
        assert_eq!(
            segment_shapes(b"\t  value = 1;  \t\r\n", None),
            vec![
                ("\t".to_owned(), false, "tab", "leading"),
                ("  ".to_owned(), false, "space", "leading"),
                ("value = 1;".to_owned(), false, "", ""),
                ("  ".to_owned(), false, "space", "trailing"),
                ("\t".to_owned(), false, "tab", "trailing"),
            ]
        );
    }

    #[test]
    fn a_line_of_nothing_but_whitespace_is_trailing_whitespace() {
        assert_eq!(
            segment_shapes(b"   \t\n", None),
            vec![
                ("   ".to_owned(), false, "space", "trailing"),
                ("\t".to_owned(), false, "tab", "trailing"),
            ]
        );
    }

    #[test]
    fn intra_line_emphasis_lands_on_the_leading_run_that_changed() {
        let ranges = [harkness_git::IntraLineRange { start: 0, end: 4 }];
        assert_eq!(
            segment_shapes(b"    value\n", Some(&ranges)),
            vec![
                ("    ".to_owned(), true, "space", "leading"),
                ("value".to_owned(), false, "", ""),
            ]
        );
    }

    #[test]
    fn whitespace_runs_never_split_a_multibyte_character() {
        // Latin-1 bytes that are not valid UTF-8, indented and with a trailing
        // space. Every cut lands on an ASCII space or tab, which is never a
        // continuation byte, so the invalid bytes reach QML in one run and are
        // decoded lossily exactly where they would have been anyway.
        assert_eq!(
            segment_shapes(b"  caf\xe9 \n", None),
            vec![
                ("  ".to_owned(), false, "space", "leading"),
                ("caf\u{fffd}".to_owned(), false, "", ""),
                (" ".to_owned(), false, "space", "trailing"),
            ]
        );
    }

    #[test]
    fn line_endings_are_named_rather_than_segmented() {
        assert_eq!(line_ending_name(b"value\r\n"), "crlf");
        assert_eq!(line_ending_name(b"value\n"), "lf");
        assert_eq!(line_ending_name(b"value\r"), "cr");
        assert_eq!(line_ending_name(b"value"), "none");
        assert_eq!(display_line_end(b"value\r\n"), "value".len());
    }

    /// The reason the panel carries attribution at all: two files from
    /// different hands are told apart by a mark rather than by reading a name,
    /// and a file nothing produced says so instead of going blank.
    #[test]
    fn review_rows_group_files_by_who_produced_them_and_name_the_unknown_ones() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("provenance-review");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let base_branch = repository.head().unwrap().shorthand().unwrap().to_owned();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("agent/demo", &head, true).unwrap();
        drop(head);
        repository.set_head("refs/heads/agent/demo").unwrap();
        drop(repository);

        let ada = ("Ada", "ada@example.invalid");
        let grace = ("Grace", "grace@example.invalid");
        commit_file_as(&root, Path::new("alpha.txt"), "one\n", "add alpha", ada);
        commit_file_as(&root, Path::new("beta.txt"), "one\n", "add beta", grace);
        commit_file_as(&root, Path::new("gamma.txt"), "one\n", "add gamma", ada);

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Branch {
                branch: "agent/demo".to_owned(),
                base_branch,
            },
            harkness_git::Whitespace::EXACT,
            21,
        )
        .unwrap();

        let state = review_map(&to_review(&review, ""));
        let provenance = review_map(&review_field(&state, "provenance"));
        assert!(review_flag(&provenance, "resolved"));
        // The panel pins a branch review to object ids, so the branch
        // convention can only be read because the name travelled beside it.
        assert_eq!(review_text(&provenance, "agentSlug"), "demo");

        let rows = review_field(&state, "files")
            .value::<QList<QVariant>>()
            .expect("the file list should flatten to a QVariantList")
            .iter()
            .map(|value| {
                let row = review_map(value);
                (
                    review_text(&row, "path"),
                    review_field(&row, "provenanceGroup")
                        .value::<i32>()
                        .unwrap_or(-2),
                    review_text(&row, "provenanceLabel"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        let group = |path: &str| {
            rows.iter()
                .find(|row| row.0 == path)
                .unwrap_or_else(|| panic!("{path} was not projected"))
                .1
        };
        assert!(group("alpha.txt") >= 0);
        // Same hands, same mark; different hands, a different one.
        assert_eq!(group("alpha.txt"), group("gamma.txt"));
        assert_ne!(group("alpha.txt"), group("beta.txt"));
        assert_eq!(
            rows.iter()
                .find(|row| row.0 == "beta.txt")
                .map(|row| row.2.clone()),
            Some("Grace".to_owned())
        );

        // The common case, and the one that must be calmest: content nothing
        // has committed carries no group and a named reason, never a blank.
        fs::write(root.join("scratch.txt"), "written by nobody\n").unwrap();
        let working = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            22,
        )
        .unwrap();
        let working_state = review_map(&to_review(&working, ""));
        let working_provenance = review_map(&review_field(&working_state, "provenance"));
        assert!(review_flag(&working_provenance, "resolved"));
        assert_eq!(review_text(&working_provenance, "agentSlug"), "");
        let scratch = review_field(&working_state, "files")
            .value::<QList<QVariant>>()
            .unwrap()
            .iter()
            .map(review_map)
            .find(|row| review_text(row, "path") == "scratch.txt")
            .expect("the untracked file should be projected");
        assert_eq!(
            review_field(&scratch, "provenanceGroup")
                .value::<i32>()
                .unwrap_or(-2),
            -1
        );
        assert_eq!(review_text(&scratch, "provenanceGap"), "uncommitted");
        assert_eq!(review_text(&scratch, "provenanceLabel"), "");
    }

    /// The mark answers "the same hands", which is a question about a set. Two
    /// files produced by one pair of people list them in whichever order their
    /// own newest commit fell, so an order-sensitive key would give one answer
    /// two colours and two spellings — the exact confusion the mark exists to
    /// remove.
    #[test]
    fn one_set_of_producers_is_one_group_whatever_order_a_file_lists_them_in() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("producer-order");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let base_branch = repository.head().unwrap().shorthand().unwrap().to_owned();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("feature", &head, true).unwrap();
        drop(head);
        repository.set_head("refs/heads/feature").unwrap();
        drop(repository);

        let ada = ("Ada", "ada@example.invalid");
        let grace = ("Grace", "grace@example.invalid");
        // alpha ends up [Grace, Ada] and beta [Ada, Grace]: one set, two orders.
        commit_file_as(&root, Path::new("alpha.txt"), "one\n", "ada on alpha", ada);
        commit_file_as(
            &root,
            Path::new("alpha.txt"),
            "two\n",
            "grace on alpha",
            grace,
        );
        commit_file_as(
            &root,
            Path::new("beta.txt"),
            "one\n",
            "grace on beta",
            grace,
        );
        commit_file_as(&root, Path::new("beta.txt"), "two\n", "ada on beta", ada);

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Branch {
                branch: "feature".to_owned(),
                base_branch,
            },
            harkness_git::Whitespace::EXACT,
            23,
        )
        .unwrap();

        let alpha = review.provenance.file(0);
        let beta = review.provenance.file(1);
        assert_eq!(review.files[0].path, PathBuf::from("alpha.txt"));
        assert_eq!(review.files[1].path, PathBuf::from("beta.txt"));
        assert_eq!(alpha.producers, 2);
        assert_eq!(beta.producers, 2);
        assert_eq!(alpha.group, beta.group);
        assert_eq!(alpha.label, beta.label);
        assert_eq!(review.provenance.groups, 1);
    }

    /// A producer name is repository content and a row is one line tall.
    ///
    /// Git refuses a control character in a signature, so the vector is the
    /// trailer: `Co-Authored-By` is free text inside the message body, and
    /// whatever it holds becomes a name on a row.
    #[test]
    fn a_producer_name_reaches_the_panel_on_one_line() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("noisy-name");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let base_branch = repository.head().unwrap().shorthand().unwrap().to_owned();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("feature", &head, true).unwrap();
        drop(head);
        repository.set_head("refs/heads/feature").unwrap();
        drop(repository);

        commit_file_as(
            &root,
            Path::new("alpha.txt"),
            "one\n",
            "add alpha\n\nCo-Authored-By: Some\t\tModel   Name <model@example.invalid>\n",
            ("Ada", "ada@example.invalid"),
        );

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Branch {
                branch: "feature".to_owned(),
                base_branch,
            },
            harkness_git::Whitespace::EXACT,
            24,
        )
        .unwrap();

        let label = review.provenance.file(0).label;
        assert_eq!(label, "Ada, Some Model Name");
        assert!(
            !label.contains('\t'),
            "a tab survived into a row: {label:?}"
        );
    }

    /// A trailer may carry an address and no name. That producer is still
    /// somebody, so the row names the address rather than joining an empty
    /// string into a dangling separator — or, where it is the only producer, an
    /// empty label a surface would read back as *unattributed*.
    #[test]
    fn a_producer_with_no_name_is_shown_by_address_rather_than_as_unknown() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("unnamed-producer");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();
        let base_branch = repository.head().unwrap().shorthand().unwrap().to_owned();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("feature", &head, true).unwrap();
        drop(head);
        repository.set_head("refs/heads/feature").unwrap();
        drop(repository);

        commit_file_as(
            &root,
            Path::new("alpha.txt"),
            "one\n",
            "add alpha\n\nCo-Authored-By: <model@example.invalid>\n",
            ("Ada", "ada@example.invalid"),
        );

        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Branch {
                branch: "feature".to_owned(),
                base_branch,
            },
            harkness_git::Whitespace::EXACT,
            25,
        )
        .unwrap();

        let alpha = review.provenance.file(0);
        assert_eq!(alpha.label, "Ada, model@example.invalid");
        assert_eq!(alpha.producers, 2);
        assert_eq!(alpha.gap, "");
    }

    /// A review with no changed files asks about no paths, which is answered
    /// without a walk — and *answered*, rather than reported as an attribution
    /// that could not be made.
    #[test]
    fn a_review_with_no_files_resolves_to_an_empty_attribution() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("empty-review");
        initialize_repository(&root);
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));

        let review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            26,
        )
        .unwrap();

        assert!(review.files.is_empty());
        assert!(review.provenance.resolved);
        assert_eq!(review.provenance.commits, 0);
        assert_eq!(review.provenance.producers, 0);
        assert_eq!(review.provenance.groups, 0);
        assert!(review.provenance.files.is_empty());
    }

    fn commit_file_as(
        root: &Path,
        path: &Path,
        contents: &str,
        message: &str,
        author: (&str, &str),
    ) {
        fs::write(root.join(path), contents).unwrap();
        let repository = Repository::open(root).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now(author.0, author.1).unwrap();
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

    fn review_map(value: &QVariant) -> QMap<QMapPair_QString_QVariant> {
        value
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("a review projection should flatten to a QVariantMap")
    }

    fn review_field(map: &QMap<QMapPair_QString_QVariant>, key: &str) -> QVariant {
        map.get(&QString::from(key))
            .unwrap_or_else(|| panic!("the review projection should carry {key}"))
    }

    fn review_text(map: &QMap<QMapPair_QString_QVariant>, key: &str) -> String {
        review_field(map, key)
            .value::<QString>()
            .unwrap_or_default()
            .to_string()
    }

    fn review_flag(map: &QMap<QMapPair_QString_QVariant>, key: &str) -> bool {
        review_field(map, key).value::<bool>().unwrap_or_default()
    }

    fn review_segment_shapes(
        map: &QMap<QMapPair_QString_QVariant>,
    ) -> Vec<(String, bool, String, String)> {
        review_field(map, "segments")
            .value::<QList<QVariant>>()
            .expect("segments should flatten to a QVariantList")
            .iter()
            .map(review_map)
            .map(|segment| {
                (
                    review_text(&segment, "text"),
                    review_flag(&segment, "changed"),
                    review_text(&segment, "whitespace"),
                    review_text(&segment, "zone"),
                )
            })
            .collect()
    }

    #[test]
    fn review_rows_carry_the_whitespace_change_the_text_alone_hides() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("whitespace-review-repository");
        initialize_repository(&root);
        // The terminator *is* the fixture here, and a developer whose global
        // configuration sets `core.autocrlf` would otherwise have it rewritten
        // on the way into the index, taking the case under test with it.
        let repository = Repository::open(&root).unwrap();
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        drop(repository);
        let path = Path::new("indent.txt");
        commit_file(&root, path, "value = 1;   \nwindows\r\n", "add whitespace");
        fs::write(root.join(path), "value = 1;\nwindows\n").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let files = git
            .diff(
                harkness_git::DiffTarget::Unstaged,
                &harkness_git::DiffOptions::default().with_intra_line_ranges(true),
            )
            .unwrap();
        let hunk = &files[0].hunks[0];
        let deletion_at = |prefix: &[u8]| {
            hunk.lines
                .iter()
                .position(|line| {
                    matches!(line.kind, harkness_git::DiffLineKind::Deletion)
                        && line.content.starts_with(prefix)
                })
                .expect("the fixture should delete this line")
        };

        // The stripped trailing run is the whole change, and it survives to
        // QML as a trailing run on the old side that the new side does not
        // have — which is the difference the two texts do not show.
        let stripped = review_map(&to_review_line_row("hunk-1", hunk, deletion_at(b"value")));
        let old_side = review_map(&review_field(&stripped, "old"));
        let new_side = review_map(&review_field(&stripped, "new"));
        assert_eq!(
            review_segment_shapes(&old_side).last(),
            Some(&(
                "   ".to_owned(),
                true,
                "space".to_owned(),
                "trailing".to_owned()
            ))
        );
        assert_eq!(
            review_segment_shapes(&new_side),
            vec![("value = 1;".to_owned(), false, String::new(), String::new())]
        );
        assert!(!review_flag(&stripped, "lineEndChanged"));
        assert_eq!(review_text(&old_side, "copyText"), "value = 1;   \n");

        // The terminator change has no changed byte inside the segments at
        // all: the ranges clamp to nothing, so the row is the only place it
        // can be reported.
        let terminated = review_map(&to_review_line_row("hunk-1", hunk, deletion_at(b"windows")));
        assert!(review_flag(&terminated, "lineEndChanged"));
        assert_eq!(
            review_text(&review_map(&review_field(&terminated, "old")), "lineEnd"),
            "crlf"
        );
        assert_eq!(
            review_text(&review_map(&review_field(&terminated, "new")), "lineEnd"),
            "lf"
        );
        assert_eq!(
            review_text(
                &review_map(&review_field(&terminated, "unified")),
                "copyText"
            ),
            "windows\r\n"
        );
    }

    #[test]
    fn the_no_newline_marker_is_an_annotation_and_not_a_line() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("unterminated-review-repository");
        initialize_repository(&root);
        let path = Path::new("unterminated.txt");
        commit_file(&root, path, "alpha\nbeta\n", "add terminated lines");
        fs::write(root.join(path), "alpha\nbeta").unwrap();
        let git = harkness_git::GitService::new(&root, fixture.path().join("data"));
        let files = git
            .diff(
                harkness_git::DiffTarget::Unstaged,
                &harkness_git::DiffOptions::default().with_intra_line_ranges(true),
            )
            .unwrap();
        let hunk = &files[0].hunks[0];
        let marker_index = hunk
            .lines
            .iter()
            .position(|line| super::is_eof_marker(line.kind))
            .expect("dropping the final newline should produce Git's marker");

        // Its content is the sentence libgit2 wrote plus newlines that are in
        // no file, so the row carries no terminator to mark and nothing to
        // copy — the reader would otherwise paste the annotation as content.
        let marker = review_map(&to_review_line_row("hunk-1", hunk, marker_index));
        let unified = review_map(&review_field(&marker, "unified"));
        assert_eq!(review_text(&unified, "kind"), "eof");
        assert_eq!(review_text(&unified, "lineEnd"), "none");
        assert_eq!(review_text(&unified, "copyText"), "");
        assert!(!review_flag(&marker, "lineEndChanged"));

        // The line that lost its terminator still reports the pair, so the
        // surface can name what each side ended with.
        let dropped = review_map(&to_review_line_row(
            "hunk-1",
            hunk,
            hunk.lines
                .iter()
                .position(|line| matches!(line.kind, harkness_git::DiffLineKind::Deletion))
                .expect("the final line is rewritten without its newline"),
        ));
        assert!(review_flag(&dropped, "lineEndChanged"));
        assert_eq!(
            review_text(&review_map(&review_field(&dropped, "old")), "lineEnd"),
            "lf"
        );
        assert_eq!(
            review_text(&review_map(&review_field(&dropped, "new")), "lineEnd"),
            "none"
        );
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
            harkness_git::Whitespace::EXACT,
            7,
            8,
            None,
        )
        .unwrap();

        assert_eq!(review.files.len(), 2);
        assert_eq!(review.selected_file_id, review.files[0].id);
        let loaded = review.loaded_file.as_ref().unwrap();
        assert_eq!(loaded.id, review.files[0].id);
        assert_eq!(loaded.file.new_path, review.files[0].file.new_path);
        assert!(!loaded.file.hunks.is_empty());
        assert!(review.files.iter().all(|entry| entry.file.hunks.is_empty()));

        let revision = match review.target.as_ref().unwrap().target.clone() {
            harkness_git::DiffTarget::Commit { revision, .. } => revision,
            _ => unreachable!(),
        };
        let preferred = load_review_with_initial_file_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            harkness_git::Whitespace::EXACT,
            9,
            10,
            Some(second_path),
        )
        .unwrap();
        let loaded = preferred.loaded_file.as_ref().unwrap();
        assert_eq!(loaded.file.new_path.as_deref(), Some(second_path));
        assert_eq!(preferred.selected_file_id, loaded.id);
        assert_eq!(
            selected_review_path(&preferred).as_deref(),
            Some(second_path)
        );
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
        let mut review = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Commit { revision },
            harkness_git::Whitespace::EXACT,
            7,
        )
        .unwrap();
        assert_eq!(review.files.len(), 1_000);
        assert!(review.loaded_file.is_none());
        assert!(review.files.iter().all(|entry| entry.file.hunks.is_empty()));
        assert_eq!(
            review_file_window(&review),
            (0, REVIEW_FILE_PAGE_SIZE, 1_000)
        );
        let first = to_review(&review, "")
            .value::<QMap<QMapPair_QString_QVariant>>()
            .unwrap();
        assert_eq!(
            first
                .get(&QString::from("files"))
                .and_then(|value| value.value::<QList<QVariant>>())
                .unwrap()
                .len(),
            isize::try_from(REVIEW_FILE_PAGE_SIZE).unwrap()
        );
        assert!(advance_review_file_window(&mut review));
        assert_eq!(
            review_file_window(&review),
            (REVIEW_FILE_PAGE_SIZE, 1_000, 1_000)
        );
        let second = to_review(&review, "")
            .value::<QMap<QMapPair_QString_QVariant>>()
            .unwrap();
        assert_eq!(
            second
                .get(&QString::from("files"))
                .and_then(|value| value.value::<QList<QVariant>>())
                .unwrap()
                .len(),
            isize::try_from(1_000 - REVIEW_FILE_PAGE_SIZE).unwrap()
        );
        assert!(retreat_review_file_window(&mut review));
        assert_eq!(review_file_window(&review).0, 0);
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
            harkness_git::Whitespace::EXACT,
            8,
        )
        .unwrap();
        let loaded = load_review_file_with_git(
            &git,
            review.target.as_ref().unwrap(),
            harkness_git::Whitespace::EXACT,
            &review.files[0],
            9,
        )
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
        let staged = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Staged,
            harkness_git::Whitespace::EXACT,
            10,
        )
        .unwrap();
        let staged_file = load_review_file_with_git(
            &git,
            staged.target.as_ref().unwrap(),
            harkness_git::Whitespace::EXACT,
            &staged.files[0],
            11,
        )
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

        let unstaged = load_review_with_git(
            &git,
            "project-1".to_owned(),
            ReviewSelection::Unstaged,
            harkness_git::Whitespace::EXACT,
            12,
        )
        .unwrap();
        let unstaged_file = load_review_file_with_git(
            &git,
            unstaged.target.as_ref().unwrap(),
            harkness_git::Whitespace::EXACT,
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
