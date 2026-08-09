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

    use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

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

        // Instantiate the shell directly with a real directory so TreeView
        // creates delegates and validates the filesystem role bindings. The
        // normal application cannot reach this page until an async import or
        // open completes, which the no-event-loop smoke test cannot drive.
        LOADED.store(false, Ordering::SeqCst);
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    visible: false
    width: 640
    height: 480

    HarknessBackend { id: backend }
    pageStack.initialPage: ProjectShellPage {
        backend: backend
        project: ({
            "id": "00000000-0000-0000-0000-000000000000",
            "displayName": "QML fixture",
            "root": "/tmp",
            "remote": "",
            "branch": "",
            "managed": false,
            "worktree": false,
            "parentId": "",
            "parentName": "",
            "createdBranch": "",
            "available": true,
            "isGit": false,
            "dirty": false
        })
    }
}
"#,
                ),
                &QUrl::from("qrc:/ProjectShellSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "ProjectShellPage failed to load; see QML warnings above"
        );

        // Exercise every GitPanel delegate with hand-written state. Main.qml
        // cannot populate changed paths or running jobs without driving real
        // asynchronous Git operations, so this fixture is what catches typos
        // in those otherwise-lazy QML branches.
        LOADED.store(false, Ordering::SeqCst);
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    visible: false
    width: 640
    height: 720

    property var projectFixture: ({
        "id": "00000000-0000-0000-0000-000000000000",
        "displayName": "Git fixture",
        "root": "/tmp",
        "remote": "github.com/example/repository",
        "branch": "topic",
        "managed": false,
        "worktree": false,
        "parentId": "",
        "parentName": "",
        "createdBranch": "",
        "available": true,
        "isGit": true,
        "dirty": true
    })

    QtObject {
        id: fixtureBackend

        property var branches: [{
            "name": "topic",
            "current": true,
            "selectable": true,
            "detail": "Checked out here"
        }]
        property var worktrees: [{
            "id": "10000000-0000-0000-0000-000000000000",
            "root": "/tmp/worktree",
            "branch": "agent/topic",
            "owned": true,
            "locked": false,
            "lockReason": "",
            "prunable": false
        }, {
            "id": "20000000-0000-0000-0000-000000000000",
            "root": "/tmp/worktree-locked",
            "branch": "agent/locked",
            "owned": true,
            "locked": true,
            "lockReason": "agent is still working",
            "prunable": false
        }]
        property var jobs: [{
            "id": "job-1",
            "kind": "fetch",
            "projectId": "00000000-0000-0000-0000-000000000000",
            "label": "Fetch",
            "progress": "Receiving objects",
            "cancellable": true
        }]
        property var git: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "branch": "topic",
            "head": "topic",
            "detached": false,
            "unborn": false,
            "upstream": "origin/topic",
            "ahead": 1,
            "behind": 2,
            "pending": "",
            "error": "",
            "errorKind": "",
            "entries": [{
                "path": "src/main.rs",
                "staged": "added",
                "unstaged": "modified",
                "renameSource": "",
                "conflicted": false
            }]
        })
        property var diff: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "path": "src/main.rs",
            "loading": false,
            "error": "",
            "errorKind": "",
            "files": [{
                "target": "staged",
                "change": "modified",
                "path": "src/main.rs",
                "summary": "",
                "binary": false,
                "hunks": [{
                    "selectionId": "fixture-staged",
                    "header": "@@ -1,2 +1,2 @@",
                    "oldStart": 1,
                    "oldLines": 2,
                    "newStart": 1,
                    "newLines": 2,
                    "lines": [{
                        "kind": "deletion",
                        "marker": "-",
                        "oldLine": 1,
                        "newLine": 0,
                        "content": "old line"
                    }, {
                        "kind": "addition",
                        "marker": "+",
                        "oldLine": 0,
                        "newLine": 1,
                        "content": "new line"
                    }, {
                        "kind": "context",
                        "marker": " ",
                        "oldLine": 2,
                        "newLine": 2,
                        "content": "unchanged"
                    }]
                }]
            }, {
                "target": "unstaged",
                "change": "modified",
                "path": "src/main.rs",
                "summary": "Binary file - content diff and hunk staging are unavailable.",
                "binary": true,
                "hunks": []
            }]
        })

        function refreshGit(projectId) {}
        function refreshDiff(projectId, path) {}
        function clearDiff() {}
        function refreshBranches(projectId) {}
        function refreshWorktrees(projectId) {}
        function stagePath(projectId, path) {}
        function unstagePath(projectId, path) {}
        function stageHunk(projectId, selectionId) {}
        function unstageHunk(projectId, selectionId) {}
        function commit(projectId, message, amend) {}
        function fetch(projectId) {}
        function pull(projectId) {}
        function push(projectId, allowDefaultBranch) {}
        function cancelJob(jobId) {}
        function checkoutBranch(projectId, branch) {}
        function createBranch(projectId, branch, startPoint) {}
        function createWorktree(projectId, mode, branch, startPoint) {}
        function moveWorktree(projectId, destination) {}
        function lockWorktree(projectId, reason) {}
        function unlockWorktree(projectId) {}
        function reconcileWorktrees(projectId) {}
    }

    GitPanel {
        anchors.fill: parent
        backend: fixtureBackend
        diffProjectId: String(projectFixture.id)
        project: projectFixture
        selectedPath: "src/main.rs"
    }
}
"#,
                ),
                &QUrl::from("qrc:/GitPanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "GitPanel.qml failed to load; see QML warnings above"
        );
        // The engine must be released before the application; dropping locals
        // in declaration order would do the opposite.
        drop(engine);
        drop(app);
    }
}
