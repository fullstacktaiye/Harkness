mod backend;
mod file_tree_model;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

fn main() {
    // Force-links the statically compiled QML module so its types register.
    cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    QGuiApplication::set_desktop_file_name(&QString::from("io.github.fullstacktaiye.harkness"));
    if let Some(mut app) = app.as_mut() {
        app.as_mut()
            .set_application_name(&QString::from("io.github.fullstacktaiye.harkness"));
        app.as_mut()
            .set_application_display_name(&QString::from("Harkness"));
    }

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from(
            "qrc:/qt/qml/io/github/fullstacktaiye/harkness/qml/Main.qml",
        ));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

    /// Loads Main.qml the same way `main` does and asserts the engine
    /// produced a root object, catching broken imports and malformed QML
    /// without a display.
    #[test]
    fn main_qml_loads() {
        // SAFETY: set before any Qt object is constructed, and tests in this
        // binary run single-threaded with respect to Qt usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
        }
        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let app = QGuiApplication::new();
        let mut engine = QQmlApplicationEngine::new();

        static LOADED: AtomicBool = AtomicBool::new(false);
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
            });
            engine.as_mut().load(&QUrl::from(
                "qrc:/qt/qml/io/github/fullstacktaiye/harkness/qml/Main.qml",
            ));
        }

        assert!(
            LOADED.load(Ordering::SeqCst),
            "Main.qml failed to load; see QML warnings above"
        );
        // The engine must be released before the application; dropping locals
        // in declaration order would do the opposite.
        drop(engine);
        drop(app);
    }
}
