use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("io.github.fullstacktaiye.harkness").qml_files([
            "qml/Main.qml",
            "qml/LauncherPage.qml",
            "qml/LauncherActionCard.qml",
            "qml/RecentProjectCard.qml",
            "qml/ProjectShellPage.qml",
        ]),
    )
    .qt_module("Network")
    .include_dir("cxx")
    .files(["src/backend.rs", "src/file_tree_model.rs"])
    .build();
}
