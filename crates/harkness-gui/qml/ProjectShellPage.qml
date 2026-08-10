import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.Page {
    id: shell

    required property HarknessBackend backend
    required property var project

    // Lets Main.qml recognize this page when `opened` is re-set by a refresh.
    property bool isShell: true
    readonly property string shellName: project.worktree && project.parentName.length > 0
        ? qsTr("%1 — from %2").arg(project.displayName).arg(project.parentName)
        : project.displayName
    readonly property string repositoryLockScope: String(
        project.lockScope || project.parentId || project.id
    )

    title: shell.shellName

    onProjectChanged: {
        // A refresh can replace the project object; the root itself only
        // changes in theory, but re-applying it is a no-op then.
        if (project.available)
            fileModel.setRoot(project.root);
    }

    function repositoryOperationRunning() {
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if (String(candidate.projectId) === String(project.id)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope)
                return true;
        }
        return false;
    }

    actions: [
        Kirigami.Action {
            icon.name: "go-previous-symbolic"
            shortcut: "Alt+Left"
            text: qsTr("Back")
            tooltip: qsTr("Back to the launcher")
            onTriggered: shell.backend.closeProject()
        },
        Kirigami.Action {
            enabled: !shell.repositoryOperationRunning()
            icon.name: "view-refresh"
            text: qsTr("Refresh")
            tooltip: qsTr("Re-read availability and Git state")
            onTriggered: shell.backend.openProject(shell.project.id)
        },
        Kirigami.Action {
            enabled: !shell.repositoryOperationRunning()
            icon.name: "delete"
            text: qsTr("Remove from Harkness…")
            tooltip: shell.project.worktree
                ? qsTr("Remove this managed worktree through Git")
                : shell.project.managed
                    ? qsTr("Delete this managed checkout")
                    : qsTr("Forget this project; files stay untouched")
            onTriggered: {
                if (shell.project.worktree)
                    applicationWindow().confirmRemoveWorktree(shell.project.id, shell.project.displayName, shell.project.root, shell.project.branch, shell.project.dirty);
                else if (shell.project.managed)
                    applicationWindow().confirmRemoveManaged(shell.project.id, shell.project.displayName, shell.project.root);
                else
                    applicationWindow().confirmRemoveLocal(shell.project.id, shell.project.displayName);
            }
        }
    ]

    // Clipboard helper: Qt exposes no direct clipboard API to QML.
    TextEdit {
        id: clipboard
        visible: false
    }

    header: Controls.Pane {
        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing
            width: parent.width

            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Icon {
                    Layout.preferredHeight: Kirigami.Units.iconSizes.large
                    Layout.preferredWidth: Kirigami.Units.iconSizes.large
                    source: shell.project.isGit ? "folder-git" : "folder"
                }

                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 0

                    Kirigami.Heading {
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        level: 2
                        text: shell.shellName
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Controls.Label {
                            Layout.fillWidth: true
                            color: Kirigami.Theme.disabledTextColor
                            elide: Text.ElideMiddle
                            font.family: "monospace"
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: shell.project.root
                        }

                        Controls.ToolButton {
                            Controls.ToolTip.text: qsTr("Copy path")
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "edit-copy"
                            onClicked: {
                                clipboard.text = shell.project.root;
                                clipboard.selectAll();
                                clipboard.copy();
                                clipboard.deselect();
                            }
                        }
                    }
                }

                RowLayout {
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Chip {
                        Controls.ToolTip.text: qsTr("Parent project: %1").arg(shell.project.parentName)
                        Controls.ToolTip.visible: hovered && shell.project.worktree
                        checkable: false
                        closable: false
                        hoverEnabled: true
                        text: shell.project.worktree
                            ? shell.project.branch.length > 0
                                ? qsTr("Worktree: %1").arg(shell.project.branch)
                                : qsTr("Worktree: detached HEAD")
                            : shell.project.managed
                                ? qsTr("Managed clone")
                                : qsTr("Local folder")
                    }

                    Kirigami.Chip {
                        checkable: false
                        closable: false
                        enabled: false
                        text: shell.project.branch.length > 0 ? shell.project.branch : qsTr("detached HEAD")
                        visible: shell.project.isGit
                    }

                    Kirigami.Chip {
                        checkable: false
                        closable: false
                        text: qsTr("Uncommitted changes")
                        visible: shell.project.isGit && shell.project.dirty
                    }

                    Kirigami.Chip {
                        checkable: false
                        closable: false
                        text: qsTr("Missing from disk")
                        visible: !shell.project.available
                    }
                }
            }
        }
    }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        GitPanel {
            Layout.fillHeight: true
            Layout.minimumWidth: Kirigami.Units.gridUnit * 20
            Layout.preferredWidth: Kirigami.Units.gridUnit * 23
            Layout.maximumWidth: parent.width * 0.5
            backend: shell.backend
            project: shell.project
            visible: shell.project.available && shell.project.isGit
        }

        Kirigami.Separator {
            Layout.fillHeight: true
            visible: shell.project.available && shell.project.isGit
        }

        // I will address this some other time. I am working on adding the Diff View to the Project Shell Page, minimize Git's presence as well by hiding it in plain sight with actions.
        /*
        FileTreeModel {
            id: fileModel
        }

        TreeView {
            id: tree

            Layout.fillHeight: true
            Layout.fillWidth: true
            Layout.minimumWidth: Kirigami.Units.gridUnit * 12
            clip: true
            model: fileModel
            selectionModel: ItemSelectionModel {}
            visible: shell.project.available

            delegate: Controls.TreeViewDelegate {
                id: treeDelegate

                // The KDE TreeViewDelegate already requires a `model` object.
                // Consequently custom roles are exposed through that object,
                // not injected as standalone required properties.
                readonly property string fileName: model.fileName
                readonly property string filePath: model.filePath
                readonly property bool isDirectory: model.isDirectory

                Controls.ToolTip.text: treeDelegate.filePath
                Controls.ToolTip.visible: treeDelegate.hovered

                contentItem: RowLayout {
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Icon {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        source: treeDelegate.isDirectory ? "folder" : "text-plain"
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        text: treeDelegate.fileName
                    }
                }
            }

            Component.onCompleted: fileModel.setRoot(shell.project.root)
        }
        */
    }

    Kirigami.PlaceholderMessage {
        anchors.centerIn: parent
        icon.name: "dialog-warning"
        text: qsTr("Project unavailable")
        explanation: qsTr("The directory %1 no longer exists on disk.").arg(shell.project.root)
        visible: !shell.project.available
        width: parent.width - Kirigami.Units.gridUnit * 4
    }
}
