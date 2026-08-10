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
pub(crate) mod tests {
    use std::{
        fs,
        path::Path,
        ptr,
        sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    };

    use cxx_qt::QObject;
    use cxx_qt_lib::{QByteArray, QGuiApplication, QObjectExt, QQmlApplicationEngine, QUrl};
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    /// Loads Main.qml the same way `main` does and asserts the engine
    /// produced a root object, catching broken imports and malformed QML
    /// without a display.
    #[allow(dead_code)]
    pub(crate) fn main_qml_loads() {
        let screenshot_path = std::env::var("HARKNESS_QML_SCREENSHOT_PATH")
            .ok()
            .filter(|path| !path.is_empty());
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let repository_root = fixture.path().join("token-bridge-repository");
        fs::create_dir_all(&repository_root).unwrap();
        let repository = Repository::init(&repository_root).unwrap();
        let path = Path::new("bridge.txt");
        fs::write(repository_root.join(path), "before\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        drop(tree);
        drop(index);
        drop(repository);
        fs::write(repository_root.join(path), "after\n").unwrap();

        // SAFETY: set before any Qt object is constructed, and tests in this
        // binary run single-threaded with respect to Qt usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
            std::env::set_var("QT_FATAL_WARNINGS", "1");
            std::env::set_var("HARKNESS_DATA_DIR", &data_dir);
        }
        let real_project = harkness_core::ProjectService::load()
            .unwrap()
            .import_local(&repository_root)
            .unwrap();
        let real_project_id = real_project.id.to_string();
        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let mut app = QGuiApplication::new();
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
            "lockScope": "00000000-0000-0000-0000-000000000000",
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
        static INTERACTION_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                INTERACTION_ROOT.store(object, Ordering::SeqCst);
            });
            let interaction_qml = String::from_utf8(
                br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    objectName: screenshotSaved
        ? "GitPanelScreenshotSaved"
        : fixtureBackend.workingReviewCalls === 1
        && fixtureBackend.lastReviewStaged === false
        && fixtureBackend.lastReviewPath === "fixture-path"
        && fixtureBackend.stageHunkCalls === 3
        && fixtureBackend.unstageHunkCalls === 1
        && fixtureBackend.stageLineCalls === 2
        && fixtureBackend.lastLineIds === "review-line-old\nreview-line-new"
        && fixtureBackend.lastHunkProject === String(projectFixture.id)
        && fixtureBackend.lastHunkId === "review-hunk-fixture"
        && reviewFixture.splitLayout === true
        && pairedRowsDetected === true
        && lineSelectionDetected === true
        && rowToggleDetected === true
        && offWindowSelectionDetected === true
        && reviewFixture.restorePositionAfterMutation === false
        && reviewFixture.heldReviewPathId === ""
        && reviewFixture.heldReviewRowIndex === -1
        && reviewBusyDetected === true
        && mutationBusyDetected === true
        && operationBusyDetected === true
        && fileBoundaryDetected === true
        && filePagingDetected === true
        && navigationLockDetected === true
        && pagingControlsDetected === true
        && crossPageNavigationDetected === true
        && terminalHunkBoundaryDetected === true
        && largeModelDetected === true
        && deepScrollDetected === true
        && nonzeroViewportAnchorDetected === true
        && asyncRestored === true
        && focusStealPrevented === true
        && realBridgePassed === true
        ? "GitPanelSmokePassed"
        : "GitPanelSmokeFailed-" + fixtureBackend.stageHunkCalls
            + "-" + fixtureBackend.unstageHunkCalls
            + "-" + pairedRowsDetected
            + "-" + lineSelectionDetected
            + "-" + rowToggleDetected
            + "-" + offWindowSelectionDetected
            + "-" + fixtureBackend.stageLineCalls
            + "-" + reviewBusyDetected
            + "-" + mutationBusyDetected
            + "-" + operationBusyDetected
            + "-" + fileBoundaryDetected
            + "-" + filePagingDetected
            + "-" + navigationLockDetected
            + "-" + pagingControlsDetected
            + "-" + crossPageNavigationDetected
            + "-" + terminalHunkBoundaryDetected
            + "-" + fixtureBackend.nextPageCalls
            + "-" + fixtureBackend.previousPageCalls
            + "-" + reviewFixture.pendingHunkNavigation
            + "-" + largeModelDetected
            + "-" + deepScrollDetected
            + "-" + nonzeroViewportAnchorDetected
            + "-" + asyncRestored
            + "-" + focusStealPrevented
            + "-" + outsideFocus.activeFocus
            + "-" + reviewFixture.reviewCurrentIndex
            + "-" + reviewFixture.restorePositionAfterMutation
            + "-" + realBridgePassed
    visible: true
    width: screenshotPath.length > 0 ? 760 : 640
    height: screenshotPath.length > 0 ? 1140 : 720

    property bool reviewBusyDetected: false
    property bool pairedRowsDetected: false
    property bool lineSelectionDetected: false
    property bool rowToggleDetected: false
    property bool offWindowSelectionDetected: false
    property bool mutationBusyDetected: false
    property bool operationBusyDetected: false
    property bool fileBoundaryDetected: false
    property bool filePagingDetected: false
    property bool navigationLockDetected: false
    property bool pagingControlsDetected: false
    property bool crossPageNavigationDetected: false
    property bool terminalHunkBoundaryDetected: false
    property bool largeModelDetected: false
    property bool deepScrollDetected: false
    property bool nonzeroViewportAnchorDetected: false
    property bool asyncRestored: false
    property bool focusStealPrevented: false
    property bool screenshotSaved: false
    property int restorationVerificationAttempts: 0
    property int restorationVerificationStableTicks: 0
    property int focusVerificationAttempts: 0
    property int focusVerificationStableTicks: 0
    property bool fakePassed: false
    property bool realBridgePassed: false
    property int realBridgePhase: 0
    property string realProjectId: "__REAL_PROJECT_ID__"
    property string screenshotPath: "__SCREENSHOT_PATH__"
    property string realPathId: ""
    property real capturedReviewContentY: 0
    property real capturedReviewViewportOffset: 0
    property real observedReviewViewportOffset: Number.NaN

    function finishIfReady() {
        if (fakePassed && realBridgePassed) {
            smokeTimeout.stop();
            Qt.quit();
        }
    }

    property var projectFixture: ({
        "id": "00000000-0000-0000-0000-000000000000",
        "lockScope": "fixture-repository-scope",
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

    function contextSide(line) {
        return {
            "present": true,
            "line": line,
            "kind": "context",
            "marker": " ",
            "segments": [{ "text": "context " + line, "changed": false }]
        };
    }

    function contextRow(line) {
        return {
            "type": "line",
            "splitHidden": false,
            "unified": {
                "oldLine": line,
                "newLine": line,
                "kind": "context",
                "marker": " ",
                "segments": [{ "text": "context " + line, "changed": false }]
            },
            "old": contextSide(line),
            "new": contextSide(line)
        };
    }

    function hunkRow(hunkId, oldStart, oldLines, newStart, newLines) {
        return {
            "type": "hunk",
            "hunkId": hunkId,
            "header": "@@ -%1,%2 +%3,%4 @@"
                .arg(oldStart).arg(oldLines).arg(newStart).arg(newLines),
            "degradation": "",
            "action": "stage",
            "oldStart": oldStart,
            "oldLines": oldLines,
            "newStart": newStart,
            "newLines": newLines
        };
    }

    function reviewRowsWithHunkAt(hunkIndex) {
        const rows = [];
        for (let index = 0; index < 12005; ++index)
            rows.push(index === 0
                ? ({ "type": "page", "direction": "previous", "count": 12000 })
                : index === hunkIndex
                    ? hunkRow("review-hunk-fixture", 220, 2, 220, 2)
                    : contextRow(index + 1));
        return rows;
    }

    function reviewRowsAfterMutation(targetIndex, start, lines) {
        const rows = [];
        for (let index = 0; index < 12005; ++index) {
            if (index === 1) {
                rows.push(hunkRow("pre-existing-hunk", 2, 1, 2, 1));
            } else if (index === targetIndex) {
                rows.push(hunkRow(
                    "review-hunk-fixture",
                    start,
                    lines,
                    start,
                    lines
                ));
            } else {
                rows.push(contextRow(index + 1));
            }
        }
        return rows;
    }

    function crossPageRows(page) {
        if (page === 0) {
            return [
                hunkRow("cross-page-first", 1, 1, 1, 1),
                contextRow(2),
                ({
                    "type": "page",
                    "direction": "next",
                    "count": 24000,
                    "hunkAvailable": true
                })
            ];
        }
        if (page === 1) {
            return [
                ({
                    "type": "page",
                    "direction": "previous",
                    "count": 12000,
                    "hunkAvailable": true
                }),
                contextRow(12001),
                contextRow(12002),
                ({
                    "type": "page",
                    "direction": "next",
                    "count": 12000,
                    "hunkAvailable": true
                })
            ];
        }
        return [
            ({
                "type": "page",
                "direction": "previous",
                "count": 24000,
                "hunkAvailable": true
            }),
            contextRow(24001),
            hunkRow("cross-page-last", 24002, 1, 24002, 1)
        ];
    }

    function replaceReviewRows(rows) {
        const current = fixtureBackend.review;
        fixtureBackend.review = {
            "projectId": current.projectId,
            "title": current.title,
            "detail": current.detail,
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": current.selectedFileId,
            "commitId": current.commitId || "",
            "fileOffset": current.fileOffset || 0,
            "totalFiles": current.totalFiles || current.files.length,
            "files": current.files,
            "file": {
                "fileId": current.file.fileId,
                "path": current.file.path,
                "pathId": current.file.pathId,
                "summary": "",
                "binary": false,
                "lineAction": "stage",
                "hunkCount": 1,
                "totalRows": rows.length,
                "rowOffset": 0,
                "rows": rows
            }
        };
    }

    function replaceReviewFileWindow(files, offset, total, selectedFileId) {
        const current = fixtureBackend.review;
        fixtureBackend.review = {
            "projectId": current.projectId,
            "title": current.title,
            "detail": current.detail,
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": selectedFileId === undefined
                ? current.selectedFileId
                : selectedFileId,
            "commitId": current.commitId || "",
            "fileOffset": offset,
            "totalFiles": total,
            "files": files,
            "file": current.file
        };
    }

    function beginLargeModelSmoke() {
        const initialRows = reviewRowsWithHunkAt(11000);
        replaceReviewRows(initialRows);
        largeModelDetected = reviewFixture.reviewRows.length > 12000;
        Qt.callLater(function() {
            reviewFixture.reviewCurrentIndex = -1;
            reviewFixture.navigateHunk(1);
            Qt.callLater(function() {
                deepScrollDetected = reviewFixture.reviewContentY > 1000
                    && reviewFixture.reviewCurrentIndex === 11000;
                reviewFixture.reviewContentY = Math.max(
                    0,
                    reviewFixture.reviewContentY - 300
                );
                capturedReviewViewportOffset = reviewFixture.currentReviewViewportOffset();
                reviewFixture.activateCurrentHunkAction();
                nonzeroViewportAnchorDetected = capturedReviewViewportOffset > 100
                    && Math.abs(
                        reviewFixture.heldReviewViewportOffset
                            - capturedReviewViewportOffset
                    ) < 1;
                capturedReviewContentY = reviewFixture.heldReviewContentY;
                fixtureBackend.jobs = [{
                    "id": "job-stage-hunk",
                    "kind": "stage_hunk",
                    "projectId": String(projectFixture.id),
                    "lockScope": String(projectFixture.lockScope),
                    "label": "Stage hunk",
                    "progress": "Starting...",
                    "cancellable": true
                }];
                navigationLockDetected = !reviewFixture.hunkNavigationEnabled();
                replaceReviewRows(reviewRowsAfterMutation(11002, 218, 8));
                fixtureBackend.jobs = [{
                    "id": "job-review-refresh",
                    "kind": "review",
                    "projectId": String(projectFixture.id),
                    "lockScope": String(projectFixture.lockScope),
                    "label": "Load review",
                    "progress": "Starting...",
                    "cancellable": false
                }];
                Qt.callLater(function() {
                    fixtureBackend.jobs = [];
                    verifyRestoration.start();
                });
            });
        });
    }

    function verifyCrossPageNavigation(attempts) {
        const current = reviewFixture.reviewCurrentIndex >= 0
            ? reviewFixture.reviewRows[reviewFixture.reviewCurrentIndex]
            : null;
        if (fixtureBackend.crossPagePhase === 0
                && current && current.hunkId === "cross-page-last") {
            fixtureBackend.crossPagePhase = 1;
            reviewFixture.navigateHunk(-1);
        } else if (fixtureBackend.crossPagePhase === 1
                && current && current.hunkId === "cross-page-first") {
            Qt.callLater(function() {
                crossPageNavigationDetected = fixtureBackend.nextPageCalls === 3
                    && fixtureBackend.previousPageCalls === 3
                    && reviewFixture.pendingHunkNavigation === 0;
                fixtureBackend.crossPageNavigationActive = false;
                replaceReviewRows([
                    hunkRow("terminal-hunk", 1, 24000, 1, 24000),
                    ({
                        "type": "page",
                        "direction": "next",
                        "count": 12000,
                        "hunkAvailable": false
                    })
                ]);
                reviewFixture.reviewCurrentIndex = 0;
                const nextCalls = fixtureBackend.nextPageCalls;
                reviewFixture.navigateHunk(1);
                terminalHunkBoundaryDetected = !reviewFixture.hunkNavigationEnabled(1)
                    && fixtureBackend.nextPageCalls === nextCalls;
                beginLargeModelSmoke();
            });
            return;
        }
        if (attempts <= 0) {
            objectName = "GitPanelCrossPageNavigationTimedOut-"
                + fixtureBackend.crossPagePhase + "-"
                + fixtureBackend.crossPagePage + "-"
                + reviewFixture.reviewCurrentIndex + "-"
                + reviewFixture.pendingHunkNavigation + "-"
                + fixtureBackend.nextPageCalls + "-"
                + fixtureBackend.previousPageCalls;
            smokeTimeout.stop();
            Qt.quit();
            return;
        }
        Qt.callLater(function() {
            verifyCrossPageNavigation(attempts - 1);
        });
    }

    function actionableRealHunk(action) {
        const review = realBackend.review;
        if (!review || review.loading === true || review.fileLoading === true
                || !review.file || review.file.rows === undefined)
            return null;
        for (let index = 0; index < review.file.rows.length; ++index) {
            const row = review.file.rows[index];
            if (row.type === "hunk" && row.action === action)
                return row;
        }
        return null;
    }

    HarknessBackend {
        id: realBackend
    }

    Connections {
        target: realBackend

        function onOpenedChanged() {
            if (realBridgePhase !== 0
                    || String(realBackend.opened.id || "") !== realProjectId)
                return;
            realBridgePhase = 1;
            realBackend.refreshGit(realProjectId);
        }

        function onGitChanged() {
            if (realBridgePhase !== 1
                    || String(realBackend.git.projectId || "") !== realProjectId
                    || realBackend.git.entries === undefined
                    || realBackend.git.entries.length === 0)
                return;
            realPathId = String(realBackend.git.entries[0].pathId || "");
            if (realPathId.length === 0)
                return;
            realBridgePhase = 2;
            realBackend.reviewWorkingChanges(realProjectId, false, realPathId);
        }

        function onReviewChanged() {
            if (String(realBackend.review.projectId || "") !== realProjectId)
                return;
            if (realBridgePhase === 2) {
                const stage = actionableRealHunk("stage");
                if (!stage)
                    return;
                realBridgePhase = 3;
                realBackend.stageHunk(realProjectId, stage.hunkId);
            } else if (realBridgePhase === 3) {
                const unstage = actionableRealHunk("unstage");
                if (!unstage)
                    return;
                realBridgePhase = 4;
                realBackend.unstageHunk(realProjectId, unstage.hunkId);
            } else if (realBridgePhase === 4 && actionableRealHunk("stage")) {
                realBridgePhase = 5;
                realBridgePassed = true;
                finishIfReady();
            }
        }
    }

    QtObject {
        id: fixtureBackend

        property int workingReviewCalls: 0
        property bool lastReviewStaged: true
        property string lastReviewPath: ""
        property int stageHunkCalls: 0
        property int unstageHunkCalls: 0
        property int stageLineCalls: 0
        property int unstageLineCalls: 0
        property int nextPageCalls: 0
        property int previousPageCalls: 0
        property int loadReviewFileCalls: 0
        property int nextFilePageCalls: 0
        property int previousFilePageCalls: 0
        property string lastReviewFileId: ""
        property bool crossPageNavigationActive: false
        property int crossPagePage: 0
        property int crossPagePhase: 0
        property string lastHunkProject: ""
        property string lastHunkId: ""
        property string lastLineIds: ""

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
            "lockScope": "fixture-repository-scope",
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
                "pathId": "fixture-path",
                "path": "src/main.rs",
                "staged": "added",
                "unstaged": "modified",
                "renameSource": "",
                "conflicted": false
            }]
        })
        property var history: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "loading": false,
            "hasMore": true,
            "error": "",
            "errorKind": "",
            "commits": [{
                "id": "0123456789abcdef0123456789abcdef01234567",
                "shortId": "0123456789ab",
                "summary": "Exercise the review surface",
                "message": "Exercise the review surface\n\nFixture body.",
                "author": "QML Fixture",
                "authorEmail": "fixture@example.com",
                "authorTime": "1700000000",
                "parentCount": 1
            }]
        })
        property var review: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "title": "Working-tree changes",
            "detail": "Index against the working tree",
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": "review-file-fixture",
            "commitId": "",
            "fileOffset": 0,
            "totalFiles": 1,
            "files": [{
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }],
            "file": {
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "pathId": "fixture-review-path",
                "summary": "",
                "binary": false,
                "lineAction": "stage",
                "hunkCount": 1,
                "rows": [{
                    "type": "collapsed",
                    "hunkId": "review-hunk-fixture",
                    "direction": "before",
                    "count": 12
                }, {
                    "type": "hunk",
                    "hunkId": "review-hunk-fixture",
                    "header": "@@ -1 +1 @@",
                    "degradation": "",
                    "action": "stage",
                    "oldStart": 1,
                    "oldLines": 1,
                    "newStart": 1,
                    "newLines": 1
                }, {
                    "type": "line",
                    "hunkId": "review-hunk-fixture",
                    "lineAction": "stage",
                    "splitHidden": false,
                    "unified": {
                        "lineId": "review-line-old",
                        "oldLine": 1,
                        "newLine": 0,
                        "kind": "deletion",
                        "marker": "-",
                        "segments": [{
                            "text": "let old_word = true;",
                            "changed": true
                        }]
                    },
                    "old": {
                        "lineId": "review-line-old",
                        "present": true,
                        "line": 1,
                        "kind": "deletion",
                        "marker": "-",
                        "segments": [{
                            "text": "let old_word = true;",
                            "changed": true
                        }]
                    },
                    "new": {
                        "lineId": "review-line-new",
                        "present": true,
                        "line": 1,
                        "kind": "addition",
                        "marker": "+",
                        "segments": [{
                            "text": "let new_word = true;",
                            "changed": true
                        }]
                    }
                }, {
                    "type": "line",
                    "hunkId": "review-hunk-fixture",
                    "lineAction": "stage",
                    "splitHidden": true,
                    "unified": {
                        "lineId": "review-line-new",
                        "oldLine": 0,
                        "newLine": 1,
                        "kind": "addition",
                        "marker": "+",
                        "segments": [{
                            "text": "let new_word = true;",
                            "changed": true
                        }]
                    },
                    "old": { "present": false },
                    "new": { "present": false }
                }]
            }
        })

        function refreshGit(projectId) {}
        function refreshHistory(projectId) {}
        function loadMoreHistory(projectId) {}
        function reviewCommit(projectId, revision) {}
        function reviewBranch(projectId, branch, baseBranch) {}
        function reviewWorkingChanges(projectId, staged, pathId) {
            ++workingReviewCalls;
            lastReviewStaged = staged;
            lastReviewPath = String(pathId);
        }
        function loadReviewFile(projectId, fileId) {
            ++loadReviewFileCalls;
            lastReviewFileId = String(fileId);
            replaceReviewFileWindow(
                review.files,
                review.fileOffset,
                review.totalFiles,
                String(fileId)
            );
        }
        function expandReviewContext(projectId, hunkId, direction) {}
        function loadMoreReviewFiles(projectId) {
            ++nextFilePageCalls;
            replaceReviewFileWindow([{
                "fileId": "review-file-page-2-first",
                "path": "src/page-2-first.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }, {
                "fileId": "review-file-page-2-second",
                "path": "src/page-2-second.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }], 512, 514);
        }
        function loadPreviousReviewFiles(projectId) {
            ++previousFilePageCalls;
            replaceReviewFileWindow([{
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }], 0, 514);
        }
        function loadMoreReviewRows(projectId) {
            ++nextPageCalls;
            if (crossPageNavigationActive && crossPagePage < 2) {
                ++crossPagePage;
                replaceReviewRows(crossPageRows(crossPagePage));
            }
        }
        function loadPreviousReviewRows(projectId) {
            ++previousPageCalls;
            if (crossPageNavigationActive && crossPagePage > 0) {
                --crossPagePage;
                replaceReviewRows(crossPageRows(crossPagePage));
            }
        }
        function clearReview() {}
        function refreshBranches(projectId) {}
        function refreshWorktrees(projectId) {}
        function stagePath(projectId, pathId) {}
        function unstagePath(projectId, pathId) {}
        function stageHunk(projectId, hunkId) {
            ++stageHunkCalls;
            lastHunkProject = String(projectId);
            lastHunkId = String(hunkId);
        }
        function unstageHunk(projectId, hunkId) {
            ++unstageHunkCalls;
            lastHunkProject = String(projectId);
            lastHunkId = String(hunkId);
        }
        function stageLines(projectId, lineIds) {
            ++stageLineCalls;
            lastLineIds = String(lineIds);
        }
        function unstageLines(projectId, lineIds) {
            ++unstageLineCalls;
            lastLineIds = String(lineIds);
        }
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
        id: gitPanel

        anchors.fill: parent
        backend: fixtureBackend
        project: projectFixture
        selectedProjectId: String(projectFixture.id)
    }

    ReviewSurface {
        id: reviewFixture

        visible: true
        width: 640
        height: 720
        backend: fixtureBackend
        gitState: fixtureBackend.git
        project: projectFixture
        stateReady: true
    }

    FocusScope {
        id: outsideFocus

        height: 1
        width: 1
        z: 100
    }

    Timer {
        id: verifyRestoration

        interval: 25
        repeat: true
        onTriggered: {
            observedReviewViewportOffset = reviewFixture.currentReviewViewportOffset();
            const settled = capturedReviewContentY > 0
                && isFinite(observedReviewViewportOffset)
                && Math.abs(
                    observedReviewViewportOffset
                        - capturedReviewViewportOffset
                ) < 2
                && reviewFixture.reviewCurrentIndex === 11002
                && reviewFixture.reviewListHasActiveFocus
                && reviewFixture.restorePositionAfterMutation === false;
            ++restorationVerificationAttempts;
            restorationVerificationStableTicks = settled
                ? restorationVerificationStableTicks + 1
                : 0;
            if (restorationVerificationStableTicks < 3
                    && restorationVerificationAttempts < 80)
                return;
            stop();
            if (restorationVerificationStableTicks < 3) {
                objectName = "GitPanelSmokeRestorationFailed-"
                    + capturedReviewViewportOffset
                    + "-" + observedReviewViewportOffset
                    + "-" + reviewFixture.reviewListHasActiveFocus
                    + "-" + reviewFixture.reviewCurrentIndex
                    + "-" + reviewFixture.restorePositionAfterMutation;
                smokeTimeout.stop();
                Qt.quit();
                return;
            }
            asyncRestored = true;
            reviewFixture.reviewCurrentIndex = 1;
            reviewFixture.navigateHunk(1);
            reviewFixture.mutateHunk(fixtureBackend.review.file.rows[11002]);
            fixtureBackend.jobs = [{
                "id": "job-stage-hunk-focus",
                "kind": "stage_hunk",
                "projectId": String(projectFixture.id),
                "lockScope": String(projectFixture.lockScope),
                "label": "Stage hunk",
                "progress": "Starting...",
                "cancellable": true
            }];
            replaceReviewRows(reviewRowsAfterMutation(11004, 216, 12));
            outsideFocus.forceActiveFocus();
            fixtureBackend.jobs = [{
                "id": "job-review-focus",
                "kind": "review",
                "projectId": String(projectFixture.id),
                "lockScope": String(projectFixture.lockScope),
                "label": "Load review",
                "progress": "Starting...",
                "cancellable": false
            }];
            Qt.callLater(function() {
                fixtureBackend.jobs = [];
                verifyFocus.start();
            });
        }
    }

    Timer {
        id: verifyFocus

        interval: 25
        repeat: true
        onTriggered: {
            const settled = outsideFocus.activeFocus
                && reviewFixture.reviewCurrentIndex === 11004
                && reviewFixture.restorePositionAfterMutation === false;
            ++focusVerificationAttempts;
            focusVerificationStableTicks = settled
                ? focusVerificationStableTicks + 1
                : 0;
            if (focusVerificationStableTicks >= 3) {
                stop();
                focusStealPrevented = true;
                fakePassed = true;
                finishIfReady();
            } else if (focusVerificationAttempts >= 80) {
                stop();
                smokeTimeout.stop();
                Qt.quit();
            }
        }
    }

    Timer {
        id: smokeTimeout

        interval: 10000
        repeat: false
        running: true
        onTriggered: {
            objectName = "GitPanelSmokeTimedOut";
            Qt.quit();
        }
    }

    Timer {
        id: screenshotTimer

        interval: 250
        repeat: false
        onTriggered: {
            reviewFixture.toggleReviewLine("review-line-old", false);
            reviewFixture.toggleReviewLine("review-line-new", true);
            Qt.callLater(function() {
                reviewFixture.grabToImage(function(result) {
                    screenshotSaved = result.saveToFile(screenshotPath);
                    Qt.quit();
                });
            });
        }
    }

    Component.onCompleted: {
        if (screenshotPath.length > 0) {
            fixtureBackend.jobs = [];
            smokeTimeout.stop();
            screenshotTimer.start();
            return;
        }
        realBackend.openProject(realProjectId);
        gitPanel.selectPath("fixture-path", "added", "modified");
        fixtureBackend.jobs = [];
        reviewFixture.toggleReviewLine("review-line-old", false);
        reviewFixture.toggleReviewLine("review-line-new", true);
        lineSelectionDetected = reviewFixture.selectedReviewLineCount === 2
            && reviewFixture.isReviewLineSelected("review-line-old")
            && reviewFixture.isReviewLineSelected("review-line-new");

        // The row toggle behind both the delegate click and the Space key has
        // to treat a split row's two sides as one control. Half-selecting the
        // pair and pressing again must complete it, never invert it.
        reviewFixture.setSplitLayout(true);
        reviewFixture.clearReviewLineSelection();
        reviewFixture.toggleReviewLine("review-line-old", false);
        reviewFixture.setCurrentReviewRow(2);
        const halfSelectedRowToggled = reviewFixture.toggleCurrentReviewLine(false);
        rowToggleDetected = halfSelectedRowToggled
            && reviewFixture.selectedReviewLineCount === 2
            && reviewFixture.isReviewLineSelected("review-line-old")
            && reviewFixture.isReviewLineSelected("review-line-new")
            // A whole row toggles back off in one press.
            && reviewFixture.toggleCurrentReviewLine(false)
            && reviewFixture.selectedReviewLineCount === 0;
        reviewFixture.setSplitLayout(false);

        // The verb comes from the loaded file, so a selection whose rows have
        // paged out of the window still resolves one.
        reviewFixture.clearReviewLineSelection();
        reviewFixture.toggleReviewLine("review-line-old", false);
        const windowedRows = fixtureBackend.review.file.rows;
        fixtureBackend.review = Object.assign({}, fixtureBackend.review, {
            "file": Object.assign({}, fixtureBackend.review.file, { "rows": [] })
        });
        offWindowSelectionDetected = reviewFixture.selectedReviewLineCount === 1
            && reviewFixture.selectedReviewLineAction() === "stage"
            && reviewFixture.selectedReviewLineAnchorRow() === null;
        // Staging must still be attempted with nothing left to anchor on.
        const stagedBeforeOffWindow = fixtureBackend.stageLineCalls;
        reviewFixture.mutateSelectedLines();
        offWindowSelectionDetected = offWindowSelectionDetected
            && fixtureBackend.stageLineCalls === stagedBeforeOffWindow + 1;
        fixtureBackend.review = Object.assign({}, fixtureBackend.review, {
            "file": Object.assign({}, fixtureBackend.review.file, {
                "rows": windowedRows
            })
        });

        reviewFixture.clearReviewLineSelection();
        reviewFixture.toggleReviewLine("review-line-old", false);
        reviewFixture.toggleReviewLine("review-line-new", true);
        reviewFixture.mutateSelectedLines();
        reviewFixture.mutateHunk(fixtureBackend.review.file.rows[1]);
        const unifiedFixtureRowCount = reviewFixture.displayedReviewRowCount();
        reviewFixture.setSplitLayout(true);
        pairedRowsDetected = unifiedFixtureRowCount
                === fixtureBackend.review.file.rows.length
            && reviewFixture.displayedReviewRowCount()
                === unifiedFixtureRowCount - 1
            && fixtureBackend.review.file.rows[2].unified.kind === "deletion"
            && fixtureBackend.review.file.rows[3].unified.kind === "addition";
        reviewFixture.mutateHunk({
            "action": "unstage",
            "hunkId": "review-hunk-fixture"
        });
        const fileCallsBeforeBoundaryNavigation = fixtureBackend.loadReviewFileCalls;
        reviewFixture.navigateFile(-1);
        reviewFixture.navigateFile(1);
        fileBoundaryDetected = fixtureBackend.loadReviewFileCalls
            === fileCallsBeforeBoundaryNavigation;
        replaceReviewFileWindow(fixtureBackend.review.files, 0, 514);
        reviewFixture.loadReviewFilePage("next");
        Qt.callLater(function() {
            const nextBoundarySelected = fixtureBackend.lastReviewFileId
                === "review-file-page-2-first";
            reviewFixture.navigateFile(1);
            const adjacentFileSelected = fixtureBackend.lastReviewFileId
                === "review-file-page-2-second";
            reviewFixture.loadReviewFilePage("previous");
            Qt.callLater(function() {
                filePagingDetected = nextBoundarySelected
                    && adjacentFileSelected
                    && fixtureBackend.nextFilePageCalls === 1
                    && fixtureBackend.previousFilePageCalls === 1
                    && fixtureBackend.loadReviewFileCalls
                        === fileCallsBeforeBoundaryNavigation + 3
                    && fixtureBackend.lastReviewFileId
                        === "review-file-fixture";
            });
        });
        fixtureBackend.jobs = [{
            "id": "job-review",
            "kind": "review",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Load review",
            "progress": "Starting...",
            "cancellable": false
        }];
        reviewBusyDetected = gitPanel.reviewReadRunning()
            && reviewFixture.reviewReadRunning();
        fixtureBackend.jobs = [{
            "id": "job-commit",
            "kind": "commit",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Commit",
            "progress": "Starting...",
            "cancellable": true
        }];
        mutationBusyDetected = gitPanel.repositoryMutationRunning()
            && reviewFixture.repositoryMutationRunning();
        fixtureBackend.jobs = [{
            "id": "job-status",
            "kind": "status",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Refresh Git status",
            "progress": "Starting...",
            "cancellable": true
        }];
        operationBusyDetected = gitPanel.repositoryOperationRunning()
            && reviewFixture.repositoryOperationRunning();
        fixtureBackend.jobs = [];
        reviewFixture.loadReviewRowPage("next");
        reviewFixture.loadReviewRowPage("previous");
        pagingControlsDetected = fixtureBackend.nextPageCalls === 1
            && fixtureBackend.previousPageCalls === 1;
        fixtureBackend.crossPageNavigationActive = true;
        fixtureBackend.crossPagePage = 0;
        fixtureBackend.crossPagePhase = 0;
        replaceReviewRows(crossPageRows(0));
        Qt.callLater(function() {
            reviewFixture.reviewCurrentIndex = 0;
            reviewFixture.navigateHunk(1);
            Qt.callLater(function() {
                verifyCrossPageNavigation(40);
            });
        });
    }
}
"#
                .to_vec(),
            )
            .unwrap()
            .replace("__REAL_PROJECT_ID__", &real_project_id)
            .replace(
                "__SCREENSHOT_PATH__",
                &screenshot_path
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r"),
            );
            engine.as_mut().load_data(
                &QByteArray::from(interaction_qml.as_bytes()),
                &QUrl::from("qrc:/GitPanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "GitPanel.qml failed to load; see QML warnings above"
        );
        if let Some(app) = app.as_mut() {
            app.exec();
        }
        let interaction_root = INTERACTION_ROOT.load(Ordering::SeqCst);
        let interaction_name = unsafe { interaction_root.as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        let expected_interaction_name = if screenshot_path.is_some() {
            "GitPanelScreenshotSaved"
        } else {
            "GitPanelSmokePassed"
        };
        assert_eq!(
            interaction_name, expected_interaction_name,
            "GitPanel and ReviewSurface interaction check failed"
        );
        let repository = Repository::open(&repository_root).unwrap();
        let index = repository.index().unwrap();
        let entry = index
            .get_path(path, 0)
            .expect("the real token bridge must restore the HEAD index entry");
        assert_eq!(
            repository.find_blob(entry.id).unwrap().content(),
            b"before\n",
            "the public stage/unstage bridge must round-trip the exact index bytes"
        );
        // The engine must be released before the application; dropping locals
        // in declaration order would do the opposite.
        drop(engine);
        drop(app);
    }

    /// One-off diagnostic: drives a real `HarknessBackend` push against a
    /// clone whose remote default branch is protected, through the real
    /// `GitPanel.qml`, and reports whether `gitState.errorKind` comes back
    /// as `default_branch_push` the way the override button expects.
    #[allow(dead_code)]
    pub(crate) fn default_branch_push_repro() {
        use std::process::Command;

        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let remote_root = fixture.path().join("remote");
        let clone_root = fixture.path().join("clone");

        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {dir:?}");
        };

        fs::create_dir_all(&remote_root).unwrap();
        git(&remote_root, &["init", "--bare", "-q"]);
        git(
            fixture.path(),
            &[
                "clone",
                "-q",
                remote_root.to_str().unwrap(),
                clone_root.to_str().unwrap(),
            ],
        );
        git(&clone_root, &["config", "user.email", "tests@example.com"]);
        git(&clone_root, &["config", "user.name", "Harkness Tests"]);
        fs::write(clone_root.join("a.txt"), "one\n").unwrap();
        git(&clone_root, &["add", "a.txt"]);
        git(&clone_root, &["commit", "-q", "-m", "init"]);
        git(&clone_root, &["branch", "-M", "main"]);
        git(&clone_root, &["push", "-q", "-u", "origin", "main"]);
        git(&clone_root, &["remote", "set-head", "origin", "--auto"]);
        fs::write(clone_root.join("a.txt"), "two\n").unwrap();
        git(&clone_root, &["commit", "-q", "-am", "second"]);

        // SAFETY: set before any Qt object is constructed, and this binary
        // runs single-threaded with respect to Qt/env usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
            std::env::set_var("HARKNESS_DATA_DIR", &data_dir);
        }
        let real_project = harkness_core::ProjectService::load()
            .unwrap()
            .import_local(&clone_root)
            .unwrap();
        let real_project_id = real_project.id.to_string();

        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let mut app = QGuiApplication::new();
        let mut engine = QQmlApplicationEngine::new();

        static RESULT_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                RESULT_ROOT.store(object, Ordering::SeqCst);
            });
            let qml = String::from_utf8(
                br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window
    visible: false
    width: 640
    height: 480
    objectName: "PushReproPending"

    property int phase: 0

    HarknessBackend { id: backend }

    property var proj: ({
        "id": "__REAL_PROJECT_ID__",
        "lockScope": "__REAL_PROJECT_ID__",
        "displayName": "repro",
        "root": "__REPO_ROOT__",
        "remote": "",
        "branch": "",
        "managed": false,
        "worktree": false,
        "parentId": "",
        "parentName": "",
        "createdBranch": "",
        "available": true,
        "isGit": true,
        "dirty": false
    })

    GitPanel {
        id: gitPanel
        backend: backend
        project: window.proj
    }

    function overrideButtonWouldShow() {
        return gitPanel.stateReady
            && ["default_branch_push", "default_branch_unknown"]
                .indexOf(gitPanel.gitState.errorKind) !== -1;
    }

    Timer {
        interval: 20000
        running: true
        onTriggered: {
            window.objectName = "PushReproTimedOut-phase" + window.phase
                + "-jobs" + backend.jobs.length
                + "-gitProjectId" + String(backend.git.projectId || "<none>")
                + "-jobKinds[" + backend.jobs.map(function(j) { return j.kind; }).join(",") + "]";
            Qt.quit();
        }
    }

    Connections {
        target: backend

        function onOpenedChanged() {
            if (window.phase === 0
                    && String(backend.opened.id || "") === "__REAL_PROJECT_ID__") {
                window.phase = 1;
                backend.refreshGit("__REAL_PROJECT_ID__");
            }
        }

        function onGitChanged() {
            if (window.phase === 1 && backend.jobs.length === 0
                    && String(backend.git.projectId || "") === "__REAL_PROJECT_ID__") {
                window.phase = 2;
                backend.push("__REAL_PROJECT_ID__", false);
            } else if (window.phase === 2 && backend.jobs.length === 0
                    && String(backend.git.projectId || "") === "__REAL_PROJECT_ID__") {
                window.phase = 3;
                Qt.callLater(function() {
                    const passed = gitPanel.stateReady
                        && String(gitPanel.gitState.errorKind || "") === "default_branch_push"
                        && overrideButtonWouldShow();
                    window.objectName = passed
                        ? "PushOverrideConditionPassed"
                        : "PushOverrideConditionFailed:" + JSON.stringify({
                            error: String(gitPanel.gitState.error || ""),
                            errorKind: String(gitPanel.gitState.errorKind || ""),
                            stateReady: gitPanel.stateReady,
                            overrideButtonWouldShow: overrideButtonWouldShow()
                        });
                    Qt.quit();
                });
            }
        }
    }

    Component.onCompleted: backend.openProject("__REAL_PROJECT_ID__")
}
"#
                .to_vec(),
            )
            .unwrap()
            .replace("__REAL_PROJECT_ID__", &real_project_id)
            .replace("__REPO_ROOT__", &clone_root.display().to_string());
            engine.as_mut().load_data(
                &QByteArray::from(qml.as_bytes()),
                &QUrl::from("qrc:/PushOverrideRepro.qml"),
            );
        }

        if let Some(app) = app.as_mut() {
            app.exec();
        }

        let root = RESULT_ROOT.load(Ordering::SeqCst);
        let name = unsafe { root.as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        drop(engine);
        drop(app);
        assert_eq!(
            name, "PushOverrideConditionPassed",
            "GitPanel must surface a default_branch_push refusal with the \
             override button's visibility condition true; got: {name}"
        );
    }
}
