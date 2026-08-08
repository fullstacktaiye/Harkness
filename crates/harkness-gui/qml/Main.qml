import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Dialogs
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: root

    height: 900
    minimumHeight: 900
    minimumWidth: 1280
    title: qsTr("Harkness")
    visible: true
    width: 1280

    // The launcher and project shell are alternative application states, not
    // master/detail columns. Always show only the current page.
    pageStack.columnView.columnResizeMode: Kirigami.ColumnView.SingleColumn

    HarknessBackend {
        id: appBackend
        Component.onCompleted: refresh()
    }

    // Navigation is driven entirely by `opened`: any operation that sets a
    // project pushes the shell, and clearing it (back, removal) returns to
    // the launcher.
    property var openedProject: appBackend.opened

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

    Shortcut {
        sequences: [StandardKey.Open]
        onActivated: root.chooseLocalFolder()
    }

    Shortcut {
        sequences: [StandardKey.Refresh]
        onActivated: appBackend.refresh()
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
