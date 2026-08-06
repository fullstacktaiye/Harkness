mod backend;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

fn main() {
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
