import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Dialogs
import QtQuick.Window
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: root

    // The window draws its own title bar, so it asks for no decoration. The
    // title itself is still set: it is what the task switcher and the window
    // list show, and nothing inside the window displays it.
    flags: Qt.Window | Qt.FramelessWindowHint
    height: 900
    minimumHeight: 900
    minimumWidth: 1280
    title: qsTr("Harkness")
    visible: true
    width: 1280

    // The launcher and project shell are alternative application states, not
    // master/detail columns. Always show only the current page.
    pageStack.columnView.columnResizeMode: Kirigami.ColumnView.SingleColumn
    // Page-level actions live in the title bar's menus instead, so the strip
    // Kirigami would otherwise put above every page carries nothing.
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.None

    HarknessBackend {
        id: appBackend
        Component.onCompleted: refresh()
    }

    /// Project to reopen as the window comes up. Only the development QML hot
    /// reload sets it, so a reload returns to the project that was on screen
    /// instead of dropping back to the launcher; it is empty in every other run.
    property string restoreProjectId: ""

    // Navigation is driven entirely by `opened`: any operation that sets a
    // project pushes the shell, and clearing it (back, removal) returns to
    // the launcher.
    property var openedProject: appBackend.opened

    readonly property string openedProjectId: openedProject && openedProject.id !== undefined
        ? String(openedProject.id)
        : ""
    /// True while the shell page is on screen, which is what decides whether
    /// the project entries in the File menu apply to anything.
    readonly property bool projectOpen: openedProjectId.length > 0

    /// True while a job is running against the open project's repository. The
    /// menu has to answer this the same way the shell page's own controls do:
    /// a job on any linked worktree of the repository blocks this project too.
    function repositoryOperationRunning() {
        if (!root.projectOpen)
            return appBackend.busy;
        const scope = String(
            openedProject.lockScope || openedProject.parentId || openedProject.id
        );
        for (let index = 0; index < appBackend.jobs.length; ++index) {
            const candidate = appBackend.jobs[index];
            if (String(candidate.projectId) === root.openedProjectId
                    || String(candidate.lockScope || candidate.projectId) === scope)
                return true;
        }
        return false;
    }

    /// Re-reads whatever is on screen: the open project's availability and Git
    /// state, or the catalog behind the launcher.
    function refreshCurrent() {
        if (root.projectOpen)
            appBackend.openProject(root.openedProjectId);
        else
            appBackend.refresh();
    }

    /// Routes the open project to the removal confirmation its kind requires.
    function confirmRemoveOpened() {
        if (!root.projectOpen)
            return;
        const project = root.openedProject;
        if (project.worktree)
            root.confirmRemoveWorktree(project.id, project.displayName, project.root, project.branch, project.dirty);
        else if (project.managed)
            root.confirmRemoveManaged(project.id, project.displayName, project.root);
        else
            root.confirmRemoveLocal(project.id, project.displayName);
    }

    menuBar: AppTitleBar {
        Controls.Menu {
            // The style sizes a menu to its labels alone, so the widest entry
            // and its shortcut would be printed on top of each other. Reserve
            // the column the shortcuts need.
            implicitWidth: Kirigami.Units.gridUnit * 16
            title: qsTr("&File")

            Controls.Action {
                icon.name: "document-open-folder"
                shortcut: "Ctrl+O"
                text: qsTr("Open Local Folder…")
                onTriggered: root.chooseLocalFolder()
            }

            Controls.MenuSeparator {}

            Controls.Action {
                enabled: !root.repositoryOperationRunning()
                icon.name: "view-refresh"
                shortcut: "F5"
                text: qsTr("Refresh")
                onTriggered: root.refreshCurrent()
            }

            Controls.Action {
                enabled: root.projectOpen
                icon.name: "go-previous-symbolic"
                shortcut: "Alt+Left"
                text: qsTr("Back to Projects")
                onTriggered: appBackend.closeProject()
            }

            Controls.MenuSeparator {}

            Controls.Action {
                enabled: root.projectOpen && !root.repositoryOperationRunning()
                icon.name: "delete"
                text: qsTr("Remove from Harkness…")
                onTriggered: root.confirmRemoveOpened()
            }

            Controls.MenuSeparator {}

            Controls.Action {
                icon.name: "application-exit"
                shortcut: "Ctrl+Q"
                text: qsTr("Quit")
                onTriggered: root.close()
            }
        }

        Controls.Menu {
            title: qsTr("&Help")

            Controls.Action {
                icon.name: "help-about"
                text: qsTr("About Harkness")
                onTriggered: aboutDialog.open()
            }
        }
    }

    // Instantiated into the window overlay: it is the only item that spans the
    // title bar as well as the page stack, and a frameless window has to offer
    // resize grips along every edge of the whole window.
    Component {
        id: resizeBordersComponent

        WindowResizeBorders {}
    }

    Component.onCompleted: {
        resizeBordersComponent.createObject(root.overlay);
        if (root.restoreProjectId.length > 0)
            appBackend.openProject(root.restoreProjectId);
    }

    // The global toolbar's built-in Back button navigates PageRow history
    // directly, so it bypasses ProjectShellPage's explicit Back action. Keep
    // the backend in sync when either route returns to the launcher. Clearing
    // `opened` also pops the hidden shell, ensuring a later card click performs
    // a fresh push instead of depending on the Forward button.
    Connections {
        target: pageStack

        function onCurrentItemChanged() {
            const id = root.openedProject && root.openedProject.id !== undefined
                ? String(root.openedProject.id)
                : "";
            if (pageStack.currentItem && pageStack.currentItem.isLauncher === true && id.length > 0)
                appBackend.closeProject();
        }
    }

    onOpenedProjectChanged: {
        const id = openedProject && openedProject.id !== undefined ? String(openedProject.id) : "";
        if (id.length > 0) {
            // A refresh re-sets `opened` for the project already on screen;
            // update that page instead of stacking a duplicate.
            if (pageStack.depth > 1 && pageStack.currentItem.isShell === true) {
                pageStack.currentItem.project = openedProject;
            } else {
                pageStack.push(Qt.resolvedUrl("ProjectShellPage.qml"), {
                    "backend": appBackend,
                    "project": openedProject
                });
            }
        } else if (pageStack.depth > 1) {
            pageStack.pop();
        }
    }

    /// Opens the native folder dialog; the chosen path is imported on accept.
    function chooseLocalFolder() {
        folderDialog.open();
    }

    /// Asks before dropping a local project. The wording must make clear the
    /// directory itself is never touched.
    function confirmRemoveLocal(projectId, projectName) {
        removeLocalDialog.projectId = projectId;
        removeLocalDialog.subtitle = qsTr("“%1” is removed from Harkness only. Its files stay exactly where they are.").arg(projectName);
        removeLocalDialog.open();
    }

    /// Asks before deleting a managed clone, naming the checkout that dies.
    function confirmRemoveManaged(projectId, projectName, projectPath) {
        removeManagedDialog.projectId = projectId;
        removeManagedDialog.subtitle = qsTr("This permanently deletes the checkout of “%1” at:\n%2").arg(projectName).arg(projectPath);
        removeManagedDialog.open();
    }

    /// Asks before deleting a managed worktree. Dirty worktrees require a
    /// second, explicitly destructive confirmation before force is passed.
    function confirmRemoveWorktree(projectId, projectName, projectPath, branch, dirty) {
        const branchLabel = branch.length > 0 ? branch : qsTr("detached HEAD");
        removeWorktreeDialog.projectId = projectId;
        removeWorktreeDialog.projectName = projectName;
        removeWorktreeDialog.projectPath = projectPath;
        removeWorktreeDialog.branch = branchLabel;
        removeWorktreeDialog.dirty = dirty;
        removeWorktreeDialog.subtitle = dirty
            ? qsTr("“%1” for %2 has uncommitted changes at:\n%3\n\nContinue to review permanent removal.").arg(projectName).arg(branchLabel).arg(projectPath)
            : qsTr("This removes the worktree “%1” for %2 at:\n%3").arg(projectName).arg(branchLabel).arg(projectPath);
        removeWorktreeDialog.open();
    }

    FolderDialog {
        id: folderDialog
        title: qsTr("Choose a project folder")
        onAccepted: appBackend.importLocal(root.urlToPath(selectedFolder))
    }

    Kirigami.PromptDialog {
        id: removeLocalDialog

        property string projectId: ""

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Remove from Harkness?")
        onAccepted: appBackend.removeProject(projectId)
    }

    Kirigami.PromptDialog {
        id: removeManagedDialog

        property string projectId: ""

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Delete managed clone?")
        onAccepted: appBackend.removeManaged(projectId)
    }

    Kirigami.PromptDialog {
        id: removeWorktreeDialog

        property string projectId: ""
        property string projectName: ""
        property string projectPath: ""
        property string branch: ""
        property bool dirty: false

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Remove managed worktree?")
        onAccepted: {
            if (dirty) {
                forceWorktreeDialog.projectId = projectId;
                forceWorktreeDialog.subtitle = qsTr("Permanently discard every uncommitted file in “%1” at:\n%2\n\nThe branch itself is preserved. This cannot be undone.").arg(projectName).arg(projectPath);
                forceWorktreeDialog.open();
            } else {
                appBackend.removeWorktree(projectId, false);
            }
        }
    }

    Kirigami.PromptDialog {
        id: forceWorktreeDialog

        property string projectId: ""

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Discard changes and remove worktree?")
        onAccepted: appBackend.removeWorktree(projectId, true)
    }

    Kirigami.PromptDialog {
        id: aboutDialog

        standardButtons: Kirigami.Dialog.Ok
        subtitle: qsTr("Harkness manages Git projects, worktrees, and reviews for agent-driven work.")
        title: qsTr("About Harkness")
    }

    // QtQuick.Dialogs reports a url; the core expects a plain filesystem path.
    function urlToPath(url) {
        let text = url.toString();
        if (text.startsWith("file://"))
            text = text.substring(7);
        return decodeURIComponent(text);
    }

    // Give the stack the page directly. Wrapping the initial page in a
    // Component leaves an intermediate visual object unattached on some
    // Kirigami versions and produces a scene-placement warning at startup.
    pageStack.initialPage: LauncherPage {
        backend: appBackend
    }
}
