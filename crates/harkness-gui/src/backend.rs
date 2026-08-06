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
        #[qobject]
        #[qml_element]
        #[qproperty(QString, greeting, READ, CONSTANT)]
        #[qproperty(bool, busy)]
        #[qproperty(QString, status)]
        #[qproperty(QList_QVariant, projects)]
        type HarknessBackend = super::HarknessBackendRust;

        #[qinvokable]
        fn refresh(self: Pin<&mut HarknessBackend>);

        #[qinvokable]
        #[cxx_name = "importRepository"]
        fn import_repository(self: Pin<&mut HarknessBackend>, remote: &QString);

        #[qinvokable]
        #[cxx_name = "cancelImport"]
        fn cancel_import(self: Pin<&mut HarknessBackend>);

        #[qinvokable]
        #[cxx_name = "removeManaged"]
        fn remove_managed(self: Pin<&mut HarknessBackend>, project_id: &QString);
    }

    impl cxx_qt::Threading for HarknessBackend {}
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QVariant};

pub struct HarknessBackendRust {
    greeting: QString,
    busy: bool,
    status: QString,
    projects: QList<QVariant>,
    cancellation: Option<harkness_core::CloneCancellation>,
}

impl Default for HarknessBackendRust {
    fn default() -> Self {
        Self {
            greeting: harkness_core::greeting().into(),
            busy: false,
            status: "Ready".into(),
            projects: QList::default(),
            cancellation: None,
        }
    }
}

/// One catalog entry flattened into the plain data a QML delegate binds to.
///
/// Qt value types are not `Send`, so the catalog crosses the thread boundary
/// as these rows and only becomes a `QVariantList` on the GUI thread.
struct ProjectRow {
    id: String,
    display_name: String,
    root: String,
    remote: String,
    branch: String,
    managed: bool,
    available: bool,
    is_git: bool,
    dirty: bool,
}

impl From<harkness_core::Project> for ProjectRow {
    fn from(project: harkness_core::Project) -> Self {
        Self {
            id: project.id.to_string(),
            display_name: project.display_name,
            root: project.root.display().to_string(),
            remote: project.remote.unwrap_or_default(),
            // Left empty for a detached head, which `is_git` distinguishes
            // from a directory that is not a repository at all.
            branch: project
                .git
                .as_ref()
                .and_then(|git| git.branch.clone())
                .unwrap_or_default(),
            managed: project.source == harkness_core::ProjectSource::ManagedRepository,
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

fn to_projects(rows: &[ProjectRow]) -> QList<QVariant> {
    let mut projects = QList::<QVariant>::default();
    for row in rows {
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
        insert("available", QVariant::from(&row.available));
        insert("isGit", QVariant::from(&row.is_git));
        insert("dirty", QVariant::from(&row.dirty));
        projects.append(QVariant::from(&entry));
    }
    projects
}

impl ffi::HarknessBackend {
    /// Reloads the whole catalog into [`projects`](Self::projects).
    ///
    /// Every mutation reloads rather than patching a row: the catalog is the
    /// single source of truth, and a clone or removal can reorder Recents.
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
            let result = harkness_core::ProjectService::load().and_then(|mut service| {
                service.import_repository(&remote, &cancellation, move |message| {
                    let _ = progress_thread.queue(move |mut backend| {
                        backend.as_mut().set_status(message.into());
                    });
                })
            });
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().rust_mut().get_mut().cancellation = None;
                backend.as_mut().set_busy(false);
                match result {
                    Ok(project) => backend
                        .as_mut()
                        .set_status(format!("Imported {}", project.display_name).into()),
                    Err(error) => backend.as_mut().set_status(error.to_string().into()),
                }
                backend.as_mut().refresh();
            });
        });
    }

    fn cancel_import(mut self: Pin<&mut Self>) {
        if let Some(cancellation) = &self.as_ref().rust().cancellation {
            cancellation.cancel();
            self.as_mut().set_status("Cancelling Git clone…".into());
        }
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
                match result {
                    Ok(project) => backend
                        .as_mut()
                        .set_status(format!("Removed {}", project.display_name).into()),
                    Err(error) => backend.as_mut().set_status(error.into()),
                }
                backend.as_mut().refresh();
            });
        });
    }
}
