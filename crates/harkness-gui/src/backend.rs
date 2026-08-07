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
        #[qproperty(QList_QVariant, branches)]
        #[qproperty(QVariant, opened)]
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

        /// Checks out a local branch and refreshes both project and branch state.
        #[qinvokable]
        #[cxx_name = "checkoutBranch"]
        fn checkout_branch(self: Pin<&mut HarknessBackend>, project_id: &QString, branch: &QString);

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
        fn remove_worktree(self: Pin<&mut HarknessBackend>, project_id: &QString);
    }

    impl cxx_qt::Threading for HarknessBackend {}
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};

pub struct HarknessBackendRust {
    busy: bool,
    status: QString,
    projects: QList<QVariant>,
    branches: QList<QVariant>,
    opened: QVariant,
    cancellation: Option<harkness_core::CloneCancellation>,
}

impl Default for HarknessBackendRust {
    fn default() -> Self {
        Self {
            busy: false,
            status: "Ready".into(),
            projects: QList::default(),
            branches: QList::default(),
            opened: empty_opened(),
            cancellation: None,
        }
    }
}

#[derive(Debug)]
struct BranchRow {
    name: String,
    current: bool,
    selectable: bool,
    detail: String,
}

impl From<harkness_core::Branch> for BranchRow {
    fn from(branch: harkness_core::Branch) -> Self {
        let (current, selectable, detail) = match branch.checkout {
            harkness_core::BranchCheckout::NotCheckedOut => (false, true, String::new()),
            harkness_core::BranchCheckout::CurrentWorktree => {
                (true, true, "Checked out here".to_owned())
            }
            harkness_core::BranchCheckout::OtherWorktree(path) => {
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
    parent: String,
    worktree_branch: String,
    available: bool,
    is_git: bool,
    dirty: bool,
}

impl From<harkness_core::Project> for ProjectRow {
    fn from(project: harkness_core::Project) -> Self {
        let (remote, managed, worktree, parent, worktree_branch) = match &project.source {
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
            parent,
            worktree_branch,
            available: project.available,
            is_git: project.git.is_some(),
            dirty: project.git.is_some_and(|git| git.dirty),
        }
    }
}

/// Reads the catalog. Availability and Git state are recomputed per entry, so
/// this touches the filesystem once per project and belongs off the GUI thread.
fn load_rows() -> Result<Vec<ProjectRow>, String> {
    let service = harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
    Ok(service.list().into_iter().map(ProjectRow::from).collect())
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
        "parent",
        QVariant::from(&QString::from(row.parent.as_str())),
    );
    insert(
        "worktreeBranch",
        QVariant::from(&QString::from(row.worktree_branch.as_str())),
    );
    insert("available", QVariant::from(&row.available));
    insert("isGit", QVariant::from(&row.is_git));
    insert("dirty", QVariant::from(&row.dirty));
    QVariant::from(&entry)
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
        &harkness_core::BranchListOptions {
            include_remote_tracking: false,
            calculate_divergence: false,
        },
        &harkness_core::Cancellation::default(),
    )
    .map(|branches| branches.into_iter().map(BranchRow::from).collect())
    .map_err(|error| error.to_string())
}

/// The `opened` value while no project is open: an empty map, so QML can
/// always treat `opened` as a map and test `opened.id` for emptiness.
fn empty_opened() -> QVariant {
    QVariant::from(&QMap::<QMapPair_QString_QVariant>::default())
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
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let rows = load_rows();
            let _ = qt_thread.queue(move |mut backend| match rows {
                Ok(rows) => backend.as_mut().set_projects(to_projects(&rows)),
                Err(error) => backend.as_mut().set_status(error.into()),
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

        let cancellation = harkness_core::CloneCancellation::default();
        self.as_mut().rust_mut().get_mut().cancellation = Some(cancellation.clone());
        self.as_mut().set_busy(true);
        self.as_mut().set_status("Starting Git clone…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let progress_thread = qt_thread.clone();
            let result = harkness_core::ProjectService::load()
                .and_then(|mut service| {
                    service.import_repository(&remote, &cancellation, move |message| {
                        let _ = progress_thread.queue(move |mut backend| {
                            backend.as_mut().set_status(message.into());
                        });
                    })
                })
                .map_err(|error| error.to_string());
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().rust_mut().get_mut().cancellation = None;
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Imported", true);
            });
        });
    }

    fn cancel_import(mut self: Pin<&mut Self>) {
        if let Some(cancellation) = &self.as_ref().rust().cancellation {
            cancellation.cancel();
            self.as_mut().set_status("Cancelling Git clone…".into());
        }
    }

    fn close_project(mut self: Pin<&mut Self>) {
        self.as_mut().set_opened(empty_opened());
        self.as_mut().set_branches(QList::default());
    }

    fn refresh_branches(self: Pin<&mut Self>, project_id: &QString) {
        let project_id = project_id.to_string();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = load_branches(&project_id);
            let _ = qt_thread.queue(move |mut backend| match result {
                Ok(rows) => backend.as_mut().set_branches(to_branches(&rows)),
                Err(error) => {
                    backend.as_mut().set_branches(QList::default());
                    backend.as_mut().set_status(error.into());
                }
            });
        });
    }

    fn checkout_branch(mut self: Pin<&mut Self>, project_id: &QString, branch: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let project_id = project_id.to_string();
        let branch = branch.to_string();
        self.as_mut().set_busy(true);
        self.as_mut()
            .set_status(format!("Checking out {branch}…").into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid project identifier".to_owned())?;
                let mut projects =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                let git = projects.git(id).map_err(|error| error.to_string())?;
                git.checkout_branch(&branch, &harkness_core::Cancellation::default())
                    .map_err(|error| error.to_string())?;
                let rows = git
                    .branches(
                        &harkness_core::BranchListOptions {
                            include_remote_tracking: false,
                            calculate_divergence: false,
                        },
                        &harkness_core::Cancellation::default(),
                    )
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(BranchRow::from)
                    .collect::<Vec<_>>();
                let project = projects.open(id).map_err(|error| error.to_string())?;
                Ok::<_, String>((ProjectRow::from(project), rows))
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                match result {
                    Ok((project, rows)) => {
                        backend.as_mut().set_opened(to_map(&project));
                        backend.as_mut().set_branches(to_branches(&rows));
                        backend
                            .as_mut()
                            .set_status(format!("Checked out {branch}").into());
                        backend.as_mut().refresh();
                    }
                    Err(error) => backend.as_mut().set_status(error.into()),
                }
            });
        });
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
    fn remove_worktree(mut self: Pin<&mut Self>, project_id: &QString) {
        if *self.as_ref().busy() {
            return;
        }
        let project_id = project_id.to_string();
        self.as_mut().set_busy(true);
        self.as_mut().set_status("Removing worktree…".into());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let result = (|| {
                let id = project_id
                    .parse()
                    .map_err(|_| "invalid worktree project identifier".to_owned())?;
                let mut service =
                    harkness_core::ProjectService::load().map_err(|error| error.to_string())?;
                service
                    .remove_worktree(id, false)
                    .map_err(|error| error.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                apply_result(backend.as_mut(), result, "Removed", false);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use cxx_qt_lib::{QMap, QMapPair_QString_QVariant, QString, QVariant};

    use super::{
        BranchRow, OpenedUpdate, ProjectRow, empty_opened, operation_outcome, to_branches, to_map,
        to_projects,
    };

    fn project(
        source: harkness_core::ProjectSource,
        git: Option<harkness_core::GitStatus>,
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
            Some(harkness_core::GitStatus {
                branch: Some("main".to_owned()),
                dirty: true,
                upstream: Some(harkness_core::UpstreamStatus {
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
            "parent",
            "worktreeBranch",
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
    fn local_and_detached_rows_distinguish_identity_states() {
        let detached = ProjectRow::from(project(
            harkness_core::ProjectSource::Local,
            Some(harkness_core::GitStatus {
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
        let row = BranchRow::from(harkness_core::Branch {
            name: "topic".to_owned(),
            kind: harkness_core::BranchKind::Local,
            tip: "0000000000000000000000000000000000000000".parse().unwrap(),
            upstream: None,
            checkout: harkness_core::BranchCheckout::OtherWorktree("/tmp/other".into()),
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
    fn worktree_row_exposes_parent_and_recorded_branch_without_looking_local() {
        let parent = harkness_core::ProjectId::new();
        let row = ProjectRow::from(project(
            harkness_core::ProjectSource::Worktree {
                parent,
                worktree_branch: Some("agent/catalog-v2".to_owned()),
            },
            None,
        ));

        assert!(row.worktree);
        assert!(!row.managed);
        assert_eq!(row.parent, parent.to_string());
        assert_eq!(row.worktree_branch, "agent/catalog-v2");
        let map = row_map(&row);
        assert_eq!(
            map.get(&QString::from("worktree"))
                .and_then(|value| value.value::<bool>()),
            Some(true)
        );
        assert_eq!(
            map.get(&QString::from("parent"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some(parent.to_string())
        );
        assert_eq!(
            map.get(&QString::from("worktreeBranch"))
                .and_then(|value| value.value::<QString>())
                .map(|value| value.to_string()),
            Some("agent/catalog-v2".to_owned())
        );
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
