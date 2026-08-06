#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        /// The small Rust-backed object exposed to the Harkness QML module.
        #[qobject]
        #[qml_element]
        #[qproperty(QString, greeting, READ, CONSTANT)]
        #[qproperty(bool, busy)]
        #[qproperty(QString, status)]
        #[qproperty(QString, managed_path)]
        #[qproperty(QString, managed_project_id)]
        type HarknessBackend = super::HarknessBackendRust;

        #[qinvokable]
        fn import_repository(self: Pin<&mut HarknessBackend>, remote: &QString);

        #[qinvokable]
        fn cancel_import(self: Pin<&mut HarknessBackend>);

        #[qinvokable]
        fn remove_managed(self: Pin<&mut HarknessBackend>, project_id: &QString);
    }

    impl cxx_qt::Threading for HarknessBackend {}
}

pub struct HarknessBackendRust {
    greeting: cxx_qt_lib::QString,
    busy: bool,
    status: cxx_qt_lib::QString,
    managed_path: cxx_qt_lib::QString,
    managed_project_id: cxx_qt_lib::QString,
    cancellation: Option<harkness_core::CloneCancellation>,
}

impl Default for HarknessBackendRust {
    fn default() -> Self {
        Self {
            greeting: harkness_core::greeting().into(),
            busy: false,
            status: "Ready".into(),
            managed_path: "".into(),
            managed_project_id: "".into(),
            cancellation: None,
        }
    }
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading};

impl ffi::HarknessBackend {
    fn import_repository(mut self: Pin<&mut Self>, remote: &cxx_qt_lib::QString) {
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
                    Ok(project) => {
                        backend.as_mut().set_status("Repository imported".into());
                        backend
                            .as_mut()
                            .set_managed_path(project.root.display().to_string().into());
                        backend
                            .as_mut()
                            .set_managed_project_id(project.id.to_string().into());
                    }
                    Err(error) => backend.as_mut().set_status(error.to_string().into()),
                }
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
    fn remove_managed(mut self: Pin<&mut Self>, project_id: &cxx_qt_lib::QString) {
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
                    harkness_core::ProjectService::load().map_err(|e| e.to_string())?;
                service.remove_managed(id).map_err(|e| e.to_string())
            })();
            let _ = qt_thread.queue(move |mut backend| {
                backend.as_mut().set_busy(false);
                match result {
                    Ok(_) => {
                        backend
                            .as_mut()
                            .set_status("Managed repository removed".into());
                        backend.as_mut().set_managed_path("".into());
                        backend.as_mut().set_managed_project_id("".into());
                    }
                    Err(error) => backend.as_mut().set_status(error.into()),
                }
            });
        });
    }
}
