import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Dialogs
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: root

    height: 640
    minimumHeight: 420
    minimumWidth: 520
    title: qsTr("Harkness")
    visible: true
    width: 960

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
