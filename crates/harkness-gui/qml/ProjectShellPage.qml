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
    }

    actions: [
        Kirigami.Action {
            icon.name: "go-previous-symbolic"
            shortcut: "Alt+Left"
            text: qsTr("Back")
            Controls.ToolTip.text: qsTr("Back to the launcher")
            onTriggered: shell.backend.closeProject()
        },
        Kirigami.Action {
            icon.name: "view-refresh"
            text: qsTr("Refresh")
            Controls.ToolTip.text: qsTr("Re-read availability and Git state")
            onTriggered: shell.backend.openProject(shell.project.id)
        },
        Kirigami.Action {
            icon.name: "delete"
            text: qsTr("Remove from Harkness…")
            Controls.ToolTip.text: shell.project.managed ? qsTr("Delete this managed checkout") : qsTr("Forget this project; files stay untouched")
            onTriggered: {
                if (shell.project.managed)
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

                    Kirigami.Chip {
                        checkable: false
                        closable: false
                        enabled: false
                        text: shell.project.managed ? qsTr("Managed clone") : qsTr("Local folder")
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

    Controls.TreeView {
        id: tree

        anchors.fill: parent
        clip: true
        model: fileModel
        selectionModel: ItemSelectionModel {}
        visible: shell.project.available

        delegate: Controls.TreeViewDelegate {
            id: treeDelegate

            required property string fileName
            required property string filePath
            required property bool isDirectory
            required property bool expandable

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

    Kirigami.PlaceholderMessage {
        anchors.centerIn: parent
        icon.name: "dialog-warning"
        text: qsTr("Project unavailable")
        explanation: qsTr("The directory %1 no longer exists on disk.").arg(shell.project.root)
        visible: !shell.project.available
        width: parent.width - Kirigami.Units.gridUnit * 4
    }
}
