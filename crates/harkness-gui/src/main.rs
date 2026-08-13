mod backend;
mod changes_model;
mod file_tree_model;
pub(crate) mod hotreload;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

/// Root of the statically compiled QML module.
const MAIN_QML_URL: &str = "qrc:/qt/qml/io/github/fullstacktaiye/harkness/qml/Main.qml";

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

    if let Some(mut engine) = engine.as_mut() {
        // Only applies to a binary still sitting beside the QML it was built
        // from; it has to be installed before the first load, because the
        // interceptor it adds is what redirects that load to disk.
        hotreload::install(engine.as_mut(), MAIN_QML_URL);
        engine.as_mut().load(&QUrl::from(MAIN_QML_URL));
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
        // The bridge check commits for real, so the identity has to come from
        // the repository rather than from whatever the developer running the
        // suite happens to have configured globally.
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Harkness Tests").unwrap();
        config.set_str("user.email", "tests@example.com").unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
        drop(config);
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
        // A second change the bridge leaves unchecked, so the commit it drives
        // has something to exclude and the exclusion can be proved.
        let excluded_path = Path::new("excluded.txt");
        fs::write(repository_root.join(excluded_path), "not this one\n").unwrap();

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

        // Drive the activity bar and the side panel it switches with stub
        // views. Both are backend-free, so the checks are plain synchronous
        // property reads; what they cover is the view contract itself, which
        // the shell fixture above only ever sees in its unavailable state.
        LOADED.store(false, Ordering::SeqCst);
        static SIDE_PANEL_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                SIDE_PANEL_ROOT.store(object, Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    visible: false
    width: 640
    height: 480

    component StubPanel: Item {
        property string viewId: ""
        property string viewTitle: ""
        property string viewIcon: "vcs-branch"
        property string viewShortcut: "Ctrl+Shift+G"
        property int viewBadge: 0
        property bool viewAvailable: true
    }

    property string currentViewId: "first"
    property bool secondAvailable: true

    SidePanel {
        id: sidePanel

        anchors.fill: parent
        currentViewId: window.currentViewId

        StubPanel {
            id: firstView

            viewId: "first"
            viewTitle: "First"
            viewBadge: 3
        }

        StubPanel {
            id: secondView

            viewId: "second"
            viewTitle: "Second"
            viewAvailable: window.secondAvailable
        }
    }

    ActivityBar {
        id: activityBar

        anchors.left: parent.left
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        currentViewId: window.currentViewId
        views: sidePanel.views
        visible: sidePanel.hasAvailableView
    }

    Component.onCompleted: {
        const failures = [];
        function check(name, passed) {
            if (!passed)
                failures.push(name);
        }

        check("collectsDeclaredViews", sidePanel.views.length === 2);
        check("keepsDeclarationOrder", sidePanel.firstAvailableViewId() === "first");
        check("looksUpByViewId", sidePanel.view("second") === secondView);
        check("ignoresUnknownViewId", sidePanel.view("third") === null);
        check("showsCurrentView", sidePanel.currentPanel === firstView);
        check("reportsCurrentViewReady", sidePanel.currentPanelReady);
        check("titlesFromCurrentView", sidePanel.currentPanel.viewTitle === "First");
        check("advertisesViewsToTheBar", activityBar.views.length === 2);
        check("showsTheBarWhileAViewApplies", activityBar.visible);

        window.currentViewId = "second";
        check("switchesView", sidePanel.currentPanel === secondView);

        // A view that stops applying must not leave the panel on it; the shell
        // watches `currentPanelReady` to move back to one that does.
        window.secondAvailable = false;
        check("dropsUnavailableView", !sidePanel.currentPanelReady);
        check("fallsBackToAnAvailableView", sidePanel.firstAvailableViewId() === "first");

        firstView.viewAvailable = false;
        check("hidesTheBarWithoutAnyView", !activityBar.visible);
        check("reportsNoAvailableView", sidePanel.firstAvailableViewId() === "");

        window.objectName = failures.length === 0
            ? "SidePanelSmokePassed"
            : "SidePanelSmokeFailed[" + failures.join(",") + "]";
    }
}
"#,
                ),
                &QUrl::from("qrc:/SidePanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "SidePanel.qml failed to load; see QML warnings above"
        );
        let side_panel_name = unsafe { SIDE_PANEL_ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(
            side_panel_name, "SidePanelSmokePassed",
            "activity bar and side panel view contract check failed"
        );

        // Exercise the Issues surface with representative backend projection
        // rows so labels, metadata, counts, scopes, filters, selection, and
        // sorting all instantiate without making a network request.
        LOADED.store(false, Ordering::SeqCst);
        static ISSUES_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                ISSUES_ROOT.store(object, Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br##"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    visible: true
    width: 1180
    height: 760

    property var projectFixture: ({
        "id": "00000000-0000-0000-0000-000000000000",
        "displayName": "Issues fixture",
        "root": "/tmp",
        "remote": "github.com/example/repository",
        "githubRemote": "github.com/example/repository",
        "available": true,
        "isGit": true
    })

    property var issueFixtureRows: [
            {
                "number": 186,
                "title": "ACP agents, MCP integrations, and GitHub issues",
                "state": "open",
                "author": "octocat",
                "updated": "updated 2d ago",
                "labels": [
                    { "name": "enhancement", "color": "#56d4dd" },
                    { "name": "v0.5", "color": "#3fb950" }
                ],
                "milestone": "v0.5",
                "assignees": ["@octocat", "@maintainer"],
                "commentCount": 41,
                "createdByMe": true,
                "assignedToMe": false
            },
            {
                "number": 185,
                "title": "Performance and output-bound stress benchmarks",
                "state": "open",
                "author": "maintainer",
                "updated": "updated 3d ago",
                "labels": [{ "name": "testing", "color": "#a5d6a7" }],
                "milestone": "v0.5",
                "assignees": ["@me"],
                "commentCount": 3,
                "createdByMe": false,
                "assignedToMe": true
            },
            {
                "number": 120,
                "title": "Persist workspace snapshot identity",
                "state": "closed",
                "author": "octocat",
                "updated": "closed last week",
                "labels": [{ "name": "architecture", "color": "#a371f7" }],
                "milestone": "",
                "assignees": [],
                "commentCount": 8,
                "createdByMe": true,
                "assignedToMe": false
            }
    ]

    QtObject {
        id: fixtureBackend

        property int refreshCallCount: 0

        property var issues: ({
            "projectId": window.projectFixture.id,
            "remote": window.projectFixture.githubRemote,
            "loading": false,
            "viewer": "octocat",
            "rows": window.issueFixtureRows,
            "hasMore": false,
            "limitReached": false,
            "error": ""
        })

        function refreshIssues(projectId, githubRemote) {
            refreshCallCount += 1;
        }
        function loadMoreIssues(projectId, githubRemote) {}
    }

    IssuesPanel {
        id: issues

        anchors.fill: parent
        backend: fixtureBackend
        project: window.projectFixture
    }

    Component.onCompleted: {
        const failures = [];
        function check(name, passed) {
            if (!passed)
                failures.push(name);
        }

        check("availableForGitProject", issues.viewAvailable);
        check("refreshesOnceOnInitialization", fixtureBackend.refreshCallCount === 1);
        // Every local mutation hands the panel a rewritten project map. An
        // issue fetch is a network round trip, so an equal map must not buy
        // one; only a different repository is worth reloading for.
        window.projectFixture = JSON.parse(JSON.stringify(window.projectFixture));
        check("ignoresEqualProjectReplacement", fixtureBackend.refreshCallCount === 1);
        const otherProject = JSON.parse(JSON.stringify(window.projectFixture));
        otherProject.id = "11111111-1111-1111-1111-111111111111";
        window.projectFixture = otherProject;
        check("refreshesForAnotherProject", fixtureBackend.refreshCallCount === 2);
        check("countsOpenRows", issues.openIssueCount === 2);
        check("countsClosedRows", issues.closedIssueCount === 1);
        check("defaultsToOpen", issues.visibleIssueCount === 2);
        check("defaultsToOldest", issues.sortOrder === "oldest"
            && Number(issues.filteredIssues()[0].number) === 185);

        issues.searchText = "performance";
        check("searchesIssueText", issues.filteredIssues().length === 1);
        issues.searchText = "";

        issues.selectScope("assigned");
        check("filtersAssignedToMe", issues.filteredIssues().length === 1);
        check("countsSelectedScope", issues.countByStateAndSelectedScope("open") === 1);
        issues.selectScope("issues");

        issues.setIssueSelected(issues.issueRows[0], true);
        issues.sortOrder = "newest";
        check("selectionUsesIdentity", issues.issueSelected(issues.issueRows[0]));
        check("selectionDoesNotMigrate", !issues.issueSelected(issues.issueRows[1]));
        issues.sortOrder = "oldest";

        issues.labelFilter = "v0.5";
        check("filtersLabels", issues.filteredIssues().length === 1);
        issues.labelFilter = "";

        issues.assigneeFilter = "@octocat";
        check("filtersOneMemberOfAssignmentSet", issues.filteredIssues().length === 1);
        issues.assigneeFilter = "";

        issues.stateFilter = "closed";
        check("switchesState", issues.filteredIssues().length === 1);
        issues.stateFilter = "open";
        check("sortsOldest", Number(issues.filteredIssues()[0].number) === 185);

        window.objectName = failures.length === 0
            ? "IssuesPanelSmokePassed"
            : "IssuesPanelSmokeFailed[" + failures.join(",") + "]";
    }

}
"##,
                ),
                &QUrl::from("qrc:/IssuesPanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "IssuesPanel.qml failed to load; see QML warnings above"
        );
        let issues_name = unsafe { ISSUES_ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(
            issues_name, "IssuesPanelSmokePassed",
            "IssuesPanel filter and projection check failed"
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
        && reviewFixture.splitLayout === true
        && selectionDetected === true
        && changesModelDetected === true
        && commitScopeDetected === true
        && amendGatingDetected === true
        && pairedRowsDetected === true
        && editorOpenDetected === true
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
        && realBridgePassed === true
        ? "GitPanelSmokePassed"
        : "GitPanelSmokeFailed-" + selectionDetected
            + "-" + changesModelDetected
            + "-" + commitScopeDetected
            + "-" + amendGatingDetected
            + "-" + pairedRowsDetected
            + "-" + editorOpenDetected
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
            + "-" + reviewFixture.reviewCurrentIndex
            + "-" + realBridgePhase
            + "-" + realBridgePassed
    visible: true
    width: screenshotPath.length > 0 ? 760 : 640
    height: screenshotPath.length > 0 ? 1140 : 720

    property bool reviewBusyDetected: false
    property bool selectionDetected: false
    property bool changesModelDetected: false
    property bool commitScopeDetected: false
    property bool amendGatingDetected: false
    property bool pairedRowsDetected: false
    property bool editorOpenDetected: false
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
    property bool screenshotSaved: false
    property bool fakePassed: false
    property bool realBridgePassed: false
    property int realBridgePhase: 0
    property string realProjectId: "__REAL_PROJECT_ID__"
    property string screenshotPath: "__SCREENSHOT_PATH__"
    property string realPathId: ""

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
            "openLine": line,
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
            "degradation": ""
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
        replaceReviewRows(reviewRowsWithHunkAt(11000));
        largeModelDetected = reviewFixture.reviewRows.length > 12000;
        Qt.callLater(function() {
            reviewFixture.reviewCurrentIndex = -1;
            reviewFixture.navigateHunk(1);
            Qt.callLater(function() {
                deepScrollDetected = reviewFixture.reviewContentY > 1000
                    && reviewFixture.reviewCurrentIndex === 11000;
                // A commit is the one mutation this surface still has to wait
                // out: navigation must be refused while the index is moving
                // under the rows it would jump between.
                fixtureBackend.jobs = [{
                    "id": "job-commit-navigation",
                    "kind": "commit",
                    "projectId": String(projectFixture.id),
                    "lockScope": String(projectFixture.lockScope),
                    "label": "Commit",
                    "progress": "Starting...",
                    "cancellable": true
                }];
                navigationLockDetected = !reviewFixture.hunkNavigationEnabled();
                fixtureBackend.jobs = [];
                fakePassed = true;
                finishIfReady();
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

    // The review has to have actually rendered a diff before the commit is
    // worth making: it is what proves the real backend produced rows for the
    // working-tree change the commit then records.
    function realReviewHasHunk() {
        const review = realBackend.review;
        if (!review || review.loading === true || review.fileLoading === true
                || !review.file || review.file.rows === undefined)
            return false;
        for (let index = 0; index < review.file.rows.length; ++index) {
            if (review.file.rows[index].type === "hunk")
                return true;
        }
        return false;
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
            if (String(realBackend.git.projectId || "") !== realProjectId)
                return;
            const entries = realBackend.git.entries === undefined
                ? []
                : realBackend.git.entries;
            if (realBridgePhase === 1) {
                if (entries.length < 2)
                    return;
                for (let index = 0; index < entries.length; ++index) {
                    if (String(entries[index].path) === "bridge.txt")
                        realPathId = String(entries[index].pathId || "");
                }
                if (realPathId.length === 0)
                    return;
                realBridgePhase = 2;
                realBackend.reviewWorkingChanges(realProjectId, false, realPathId);
            } else if (realBridgePhase === 3) {
                // The commit stages as it goes, so what survives is the whole
                // assertion: the checked path is gone from the list and the
                // unchecked one is still sitting there, untouched.
                if (String(realBackend.git.error || "").length > 0
                        || entries.length !== 1
                        || String(entries[0].path) !== "excluded.txt")
                    return;
                realBridgePhase = 4;
                realBridgePassed = true;
                finishIfReady();
            }
        }

        function onReviewChanged() {
            if (String(realBackend.review.projectId || "") !== realProjectId
                    || realBridgePhase !== 2
                    || !realReviewHasHunk())
                return;
            realBridgePhase = 3;
            realBackend.commit(realProjectId, "bridge commit", false, realPathId);
        }
    }

    QtObject {
        id: fixtureBackend

        property int workingReviewCalls: 0
        property bool lastReviewStaged: true
        property string lastReviewPath: ""
        property int commitCalls: 0
        property bool lastCommitAmend: false
        property string lastCommitPathIds: ""
        property int nextPageCalls: 0
        property int previousPageCalls: 0
        property int loadReviewFileCalls: 0
        property int nextFilePageCalls: 0
        property int previousFilePageCalls: 0
        property int openLineCalls: 0
        property int lastOpenLine: 0
        property string lastOpenFileId: ""
        property string lastReviewFileId: ""
        property bool crossPageNavigationActive: false
        property int crossPagePage: 0
        property int crossPagePhase: 0

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
            }, {
                "pathId": "fixture-path-2",
                "path": "src/lib.rs",
                "staged": "",
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
                    "degradation": ""
                }, {
                    "type": "line",
                    "hunkId": "review-hunk-fixture",
                    "openLine": 1,
                    "splitHidden": false,
                    "unified": {
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
                    "openLine": 1,
                    "splitHidden": true,
                    "unified": {
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
        function openReviewLine(projectId, fileId, line) {
            ++openLineCalls;
            lastOpenFileId = String(fileId);
            lastOpenLine = Number(line);
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
        function commit(projectId, message, amend, pathIds) {
            ++commitCalls;
            lastCommitAmend = amend;
            lastCommitPathIds = String(pathIds);
        }
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

    GitActivity {
        id: activityFixture

        backend: fixtureBackend
        project: projectFixture
    }

    // The tab and the header toolbar are what GitPanel hosts; instantiating
    // them here as well is what makes their functions callable by name below.
    CommitSelection {
        id: selectionFixture

        project: projectFixture
    }

    ChangesPanel {
        id: changesFixture

        activity: activityFixture
        backend: fixtureBackend
        project: projectFixture
        selection: selectionFixture
        visible: false
    }

    RepositoryToolbar {
        id: toolbarFixture

        activity: activityFixture
        backend: fixtureBackend
        project: projectFixture
        visible: false
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
            reviewFixture.grabToImage(function(result) {
                screenshotSaved = result.saveToFile(screenshotPath);
                Qt.quit();
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
        changesFixture.selectPath("fixture-path", "added", "modified");
        fixtureBackend.jobs = [];

        // The Changes list is a keyed model rather than the entry array, so
        // this is what catches the roles or the binding drifting apart from
        // the delegate. Dropping one entry must leave the list at the rows the
        // projection still holds.
        const wholeStatus = fixtureBackend.git;
        changesModelDetected = changesFixture.rowCount === 2;
        fixtureBackend.git = Object.assign({}, wholeStatus, {
            "entries": [wholeStatus.entries[1]]
        });
        changesModelDetected = changesModelDetected && changesFixture.rowCount === 1;
        fixtureBackend.git = wholeStatus;
        changesModelDetected = changesModelDetected && changesFixture.rowCount === 2;

        // Everything starts included, and a file is dropped from the commit by
        // unchecking it rather than by moving it across the index.
        const allEntries = fixtureBackend.git.entries;
        selectionDetected = selectionFixture.all(allEntries)
            && selectionFixture.countIncluded(allEntries) === 2
            && selectionFixture.includedPathIds(allEntries)
                === "fixture-path\nfixture-path-2";
        selectionFixture.setIncluded("src/main.rs", false);
        selectionDetected = selectionDetected
            && !selectionFixture.all(allEntries)
            && !selectionFixture.none(allEntries)
            && selectionFixture.countIncluded(allEntries) === 1
            && selectionFixture.includedPathIds(allEntries) === "fixture-path-2";
        // An exclusion for a path that is no longer changed must not survive to
        // silently hold that path back if it changes again.
        selectionFixture.setIncluded("src/gone.rs", false);
        selectionFixture.prune(allEntries);
        selectionDetected = selectionDetected
            && selectionFixture.included("src/gone.rs")
            && !selectionFixture.included("src/main.rs");
        selectionFixture.setAll(allEntries, false);
        selectionDetected = selectionDetected
            && selectionFixture.none(allEntries)
            && selectionFixture.includedPathIds(allEntries) === "";
        selectionFixture.setAll(allEntries, true);
        selectionDetected = selectionDetected && selectionFixture.all(allEntries);

        // The footer commits the panel's own selection, not the whole list.
        gitPanel.selection.setIncluded("src/main.rs", false);
        commitScopeDetected = gitPanel.includedCount === 1;
        const commitsBefore = fixtureBackend.commitCalls;
        gitPanel.backend.commit(
            gitPanel.project.id,
            "fixture commit",
            false,
            gitPanel.selection.includedPathIds(gitPanel.entries)
        );
        commitScopeDetected = commitScopeDetected
            && fixtureBackend.commitCalls === commitsBefore + 1
            && fixtureBackend.lastCommitAmend === false
            && fixtureBackend.lastCommitPathIds === "fixture-path-2";

        // Amending stays available with nothing checked, because rewriting the
        // previous commit's message is a real thing to want; committing does
        // not, because it would record nothing.
        gitPanel.draftSummary = "drafted subject";
        gitPanel.selection.setAll(gitPanel.entries, false);
        amendGatingDetected = gitPanel.includedCount === 0
            && !gitPanel.commitAllowed()
            && gitPanel.amendAllowed();
        gitPanel.selection.setAll(gitPanel.entries, true);
        amendGatingDetected = amendGatingDetected
            && gitPanel.commitAllowed()
            && gitPanel.amendAllowed();
        // There is no previous commit to rewrite on an unborn branch.
        fixtureBackend.git = Object.assign({}, fixtureBackend.git, { "unborn": true });
        amendGatingDetected = amendGatingDetected
            && !gitPanel.amendAllowed()
            && gitPanel.commitAllowed();
        fixtureBackend.git = Object.assign({}, fixtureBackend.git, { "unborn": false });
        gitPanel.draftSummary = "";

        // Side-by-side collapses a replacement's two unified rows into one, so
        // the split view must display exactly one row fewer than the unified
        // view of the same model.
        const unifiedFixtureRowCount = reviewFixture.displayedReviewRowCount();
        const lineOpensBeforeKeyboardActivation = fixtureBackend.openLineCalls;
        reviewFixture.reviewCurrentIndex = 2;
        reviewFixture.openCurrentReviewLine();
        reviewFixture.setSplitLayout(true);
        reviewFixture.reviewCurrentIndex = 2;
        reviewFixture.openCurrentReviewLine();
        editorOpenDetected = fixtureBackend.openLineCalls
                === lineOpensBeforeKeyboardActivation + 2
            && fixtureBackend.lastOpenFileId === "review-file-fixture"
            && fixtureBackend.lastOpenLine === 1;
        pairedRowsDetected = unifiedFixtureRowCount
                === fixtureBackend.review.file.rows.length
            && reviewFixture.displayedReviewRowCount()
                === unifiedFixtureRowCount - 1
            && fixtureBackend.review.file.rows[2].unified.kind === "deletion"
            && fixtureBackend.review.file.rows[3].unified.kind === "addition";
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
        reviewBusyDetected = activityFixture.reviewReadRunning()
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
        mutationBusyDetected = activityFixture.repositoryMutationRunning()
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
        operationBusyDetected = activityFixture.repositoryOperationRunning()
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
        if screenshot_path.is_none() {
            // The bridge checked one of two changed files and was handed no
            // staged content and no separate staging call. What reached HEAD,
            // and what did not, is the whole proof: the commit staged its own
            // selection and left the rest alone.
            let repository = Repository::open(&repository_root).unwrap();
            let tree = repository
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .tree()
                .unwrap();
            let committed = tree
                .get_path(path)
                .expect("the commit bridge must record the checked path")
                .to_object(&repository)
                .unwrap();
            assert_eq!(
                committed.as_blob().unwrap().content(),
                b"after\n",
                "the public commit bridge must record the working-tree bytes exactly"
            );
            assert!(
                tree.get_path(excluded_path).is_err(),
                "the unchecked path must stay out of the commit"
            );
            let statuses = repository.statuses(None).unwrap();
            let left = statuses
                .iter()
                .map(|entry| (entry.path().unwrap_or_default().to_owned(), entry.status()))
                .collect::<Vec<_>>();
            assert_eq!(
                left.len(),
                1,
                "only the unchecked path may survive the commit: {left:?}"
            );
            assert_eq!(left[0].0, "excluded.txt");
            assert!(
                left[0].1.contains(git2::Status::WT_NEW),
                "the unchecked path must be left in the working tree, unstaged: {left:?}"
            );
        }
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
