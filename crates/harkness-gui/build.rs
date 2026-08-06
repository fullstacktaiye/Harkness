use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("io.github.fullstacktaiye.harkness").qml_file("qml/Main.qml"),
    )
    .qt_module("Network")
    .files(["src/backend.rs"])
    .build();
}
