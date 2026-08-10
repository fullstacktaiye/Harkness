use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(
        QmlModule::new("io.github.fullstacktaiye.harkness").qml_files([
            "qml/Main.qml",
            "qml/LauncherPage.qml",
            "qml/LauncherActionCard.qml",
            "qml/SidebarProjectRow.qml",
            "qml/ProjectShellPage.qml",
            "qml/ActivityBar.qml",
            "qml/ActivityBarItem.qml",
            "qml/SidePanel.qml",
            "qml/GitPanel.qml",
            "qml/ReviewPanel.qml",
            "qml/ReviewSurface.qml",
        ]),
    )
    .qt_module("Network")
    .include_dir("cxx")
    .files(["src/backend.rs", "src/file_tree_model.rs"])
    .build();
}
