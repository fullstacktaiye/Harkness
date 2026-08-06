import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    height: 560
    title: qsTr("Harkness")
    visible: true
    width: 720

    HarknessBackend {
        id: backend
        Component.onCompleted: refresh()
    }

    Kirigami.PromptDialog {
        id: removeDialog

        property string projectId: ""
        property string projectName: ""
        property string projectPath: ""

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: qsTr("This permanently deletes the checkout at:\n%1").arg(projectPath)
        title: qsTr("Delete “%1”?").arg(projectName)
        onAccepted: backend.removeManaged(projectId)
    }

    pageStack.initialPage: Kirigami.ScrollablePage {
        id: repositoryPage
        title: qsTr("Repositories")

        // Describes a row's Git state. Composed here rather than in Rust so
        // every user-visible string stays translatable.
        function describe(project) {
            if (!project.available)
                return qsTr("Missing from disk");
            if (!project.isGit)
                return qsTr("Not a Git repository");
            const branch = project.branch.length > 0 ? project.branch : qsTr("detached HEAD");
            return project.dirty ? qsTr("%1 — uncommitted changes").arg(branch) : branch;
        }

        header: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing

            RowLayout {
                Layout.fillWidth: true
                Layout.margins: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Controls.TextField {
                    id: remoteField
                    Layout.fillWidth: true
                    enabled: !backend.busy
                    placeholderText: qsTr("https://github.com/owner/repository.git")
                    onAccepted: backend.importRepository(text)
                }

                Controls.Button {
                    enabled: !backend.busy && remoteField.text.length > 0
                    text: qsTr("Clone")
                    onClicked: backend.importRepository(remoteField.text)
                }

                Controls.Button {
                    enabled: backend.busy
                    text: qsTr("Cancel")
                    onClicked: backend.cancelImport()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Controls.BusyIndicator {
                    Layout.preferredHeight: Kirigami.Units.gridUnit
                    Layout.preferredWidth: Kirigami.Units.gridUnit
                    running: backend.busy
                    visible: running
                }

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    text: backend.status
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.smallSpacing
            }
        }

        ListView {
            id: projectList
            model: backend.projects
            reuseItems: true

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                icon.name: "folder-git"
                text: qsTr("No repositories yet")
                explanation: qsTr("Clone a GitHub repository to add it here.")
                visible: projectList.count === 0
                width: parent.width - Kirigami.Units.gridUnit * 4
            }

            delegate: Controls.ItemDelegate {
                id: projectDelegate

                required property var modelData

                hoverEnabled: true
                width: ListView.view.width

                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing

                    ColumnLayout {
                        Layout.fillWidth: true
                        spacing: 0

                        RowLayout {
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.smallSpacing

                            Kirigami.Heading {
                                elide: Text.ElideRight
                                level: 4
                                opacity: projectDelegate.modelData.available ? 1 : 0.6
                                text: projectDelegate.modelData.displayName
                            }

                            Kirigami.Chip {
                                checkable: false
                                closable: false
                                enabled: false
                                text: qsTr("Managed")
                                visible: projectDelegate.modelData.managed
                            }

                            Item {
                                Layout.fillWidth: true
                            }
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            color: Kirigami.Theme.disabledTextColor
                            elide: Text.ElideMiddle
                            font: Kirigami.Theme.smallFont
                            text: projectDelegate.modelData.root
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            color: projectDelegate.modelData.available ? Kirigami.Theme.disabledTextColor : Kirigami.Theme.negativeTextColor
                            elide: Text.ElideRight
                            font: Kirigami.Theme.smallFont
                            text: repositoryPage.describe(projectDelegate.modelData)
                        }
                    }

                    // Only managed clones are Harkness's to delete. A local
                    // project's directory belongs to the user, so no
                    // destructive action is offered for it here.
                    Controls.Button {
                        enabled: !backend.busy
                        icon.name: "delete"
                        text: qsTr("Delete checkout…")
                        visible: projectDelegate.modelData.managed
                        onClicked: {
                            removeDialog.projectId = projectDelegate.modelData.id;
                            removeDialog.projectName = projectDelegate.modelData.displayName;
                            removeDialog.projectPath = projectDelegate.modelData.root;
                            removeDialog.open();
                        }
                    }
                }
            }
        }
    }
}
