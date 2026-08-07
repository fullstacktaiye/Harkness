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

    title: shell.project.displayName

    onProjectChanged: {
        // A refresh can replace the project object; the root itself only
        // changes in theory, but re-applying it is a no-op then.
        if (project.available)
            fileModel.setRoot(project.root);
        if (project.available && project.isGit)
            backend.refreshBranches(project.id);
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
            icon.name: "view-refresh"
            text: qsTr("Refresh")
            tooltip: qsTr("Re-read availability and Git state")
            onTriggered: shell.backend.openProject(shell.project.id)
        },
        Kirigami.Action {
            icon.name: "delete"
            text: qsTr("Remove from Harkness…")
            tooltip: shell.project.worktree
                ? qsTr("Remove this managed worktree through Git")
                : shell.project.managed
                    ? qsTr("Delete this managed checkout")
                    : qsTr("Forget this project; files stay untouched")
            onTriggered: {
                if (shell.project.worktree)
                    applicationWindow().confirmRemoveWorktree(shell.project.id, shell.project.displayName, shell.project.root, shell.project.worktreeBranch);
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
                        text: shell.project.displayName
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

                    Controls.ComboBox {
                        id: branchPicker

                        Accessible.name: qsTr("Current Git branch")
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 11
                        enabled: shell.project.isGit && shell.project.available && !shell.backend.busy
                        model: shell.backend.branches
                        textRole: "name"
                        valueRole: "name"
                        visible: shell.project.isGit

                        currentIndex: {
                            for (let index = 0; index < count; ++index) {
                                if (valueAt(index) === shell.project.branch)
                                    return index;
                            }
                            return -1;
                        }

                        delegate: Controls.ItemDelegate {
                            required property var modelData

                            Controls.ToolTip.text: modelData.detail
                            Controls.ToolTip.visible: hovered && modelData.detail.length > 0
                            enabled: modelData.selectable
                            text: modelData.name
                            width: branchPicker.width
                        }

                        onActivated: {
                            const selected = String(currentValue);
                            if (selected.length > 0 && selected !== shell.project.branch)
                                shell.backend.checkoutBranch(shell.project.id, selected);
                        }
                    }

                    Kirigami.Chip {
                        Controls.ToolTip.text: qsTr("Parent project: %1").arg(shell.project.parent)
                        Controls.ToolTip.visible: hovered && shell.project.worktree
                        checkable: false
                        closable: false
                        enabled: false
                        text: shell.project.worktree
                            ? qsTr("Worktree: %1").arg(shell.project.worktreeBranch)
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

    FileTreeModel {
        id: fileModel
    }

    TreeView {
        id: tree

        anchors.fill: parent
        clip: true
        model: fileModel
        selectionModel: ItemSelectionModel {}
        visible: shell.project.available

        delegate: Controls.TreeViewDelegate {
            id: treeDelegate

            // The KDE TreeViewDelegate already requires a `model` object.
            // Consequently custom roles are exposed through that object, not
            // injected as standalone required properties.
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

    Component.onCompleted: {
        if (shell.project.available && shell.project.isGit)
            shell.backend.refreshBranches(shell.project.id);
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
