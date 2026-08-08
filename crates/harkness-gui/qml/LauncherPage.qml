import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.Page {
    id: launcher

    required property HarknessBackend backend

    // Allows Main.qml to distinguish navigation back to this page from a
    // project-page transition and clear the backend's open-project state.
    readonly property bool isLauncher: true


    function tinted(color, alpha) {
        return Qt.rgba(color.r, color.g, color.b, alpha);
    }

    // Recents filtered by the search field. Recomputed whenever the catalog
    // or the query changes; the QVariantList model cannot filter itself.
    property var filteredProjects: []

    function refilter() {
        const query = searchField.text.trim().toLowerCase();
        const all = backend.projects;
        if (query.length === 0) {
            filteredProjects = all;
            return;
        }
        const matched = [];
        for (let i = 0; i < all.length; ++i) {
            const project = all[i];
            if (project.displayName.toLowerCase().includes(query) || project.root.toLowerCase().includes(query))
                matched.push(project);
        }
        filteredProjects = matched;
    }

    // Opens an available project, or routes an unavailable one to the right
    // removal confirmation. Shared by every entry point into the list.
    function openOrRemove(project) {
        if (project.available) {
            launcher.backend.openProject(project.id);
        } else if (project.worktree) {
            applicationWindow().confirmRemoveWorktree(project.id, project.displayName, project.root, project.branch, project.dirty);
        } else if (project.managed) {
            applicationWindow().confirmRemoveManaged(project.id, project.displayName, project.root);
        } else {
            applicationWindow().confirmRemoveLocal(project.id, project.displayName);
        }
    }

    Connections {
        function onProjectsChanged() {
            launcher.refilter();
        }

        target: launcher.backend
    }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        // Sidebar: identity plus the Projects list, persistent the way a
        // desktop chat app keeps its conversation list always in view.
        Rectangle {
            Layout.fillHeight: true
            Layout.preferredWidth: Kirigami.Units.gridUnit * 15.5
            color: launcher.tinted(Kirigami.Theme.textColor, 0.025)

            Rectangle {
                anchors.bottom: parent.bottom
                anchors.right: parent.right
                anchors.top: parent.top
                color: launcher.tinted(Kirigami.Theme.textColor, 0.08)
                width: 1
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.largeSpacing


                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        font.bold: true
                        font.letterSpacing: 0.6
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("PROJECTS")
                    }

                    Kirigami.SearchField {
                        id: searchField

                        Layout.fillWidth: true
                        Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                        placeholderText: qsTr("Filter projects")
                        onTextChanged: launcher.refilter()

                        background: Rectangle {
                            border.color: searchField.activeFocus ? Kirigami.Theme.focusColor : launcher.tinted(Kirigami.Theme.textColor, 0.15)
                            border.width: searchField.activeFocus ? 2 : 1
                            color: Kirigami.Theme.backgroundColor
                            radius: searchField.height / 2

                            Behavior on border.color {
                                ColorAnimation {
                                    duration: Kirigami.Units.shortDuration
                                }
                            }
                        }
                    }
                }

                Controls.ScrollView {
                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    visible: launcher.filteredProjects.length > 0

                    ListView {
                        id: projectList

                        boundsBehavior: Flickable.StopAtBounds
                        model: launcher.filteredProjects
                        spacing: Kirigami.Units.smallSpacing / 2

                        add: Transition {
                            NumberAnimation {
                                duration: Kirigami.Units.shortDuration
                                properties: "opacity"
                                from: 0
                                to: 1
                            }
                        }

                        delegate: SidebarProjectRow {
                            width: projectList.width
                            onActivated: launcher.openOrRemove(project)
                        }
                    }
                }

                Kirigami.PlaceholderMessage {
                    Layout.fillWidth: true
                    explanation: qsTr("Add one below to get started.")
                    icon.name: "folder-open"
                    text: qsTr("No projects yet")
                    visible: launcher.backend.projects.length === 0
                }

                Kirigami.PlaceholderMessage {
                    Layout.fillWidth: true
                    icon.name: "edit-none"
                    text: qsTr("No matches")
                    visible: launcher.backend.projects.length > 0 && launcher.filteredProjects.length === 0
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Controls.BusyIndicator {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: launcher.backend.busy
                        visible: launcher.backend.busy
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        text: launcher.backend.status
                        visible: text.length > 0
                    }
                }
            }
        }

        // Main content: the two ways to add a project, presented as a
        // welcoming canvas instead of squeezed above the list.
        Flickable {
            id: mainFlick

            Layout.fillHeight: true
            Layout.fillWidth: true
            boundsBehavior: Flickable.StopAtBounds
            clip: true
            contentHeight: mainColumn.implicitHeight
            contentWidth: width

            ColumnLayout {
                id: mainColumn

                width: mainFlick.width
                spacing: Kirigami.Units.largeSpacing * 2

                ColumnLayout {
                    Layout.alignment: Qt.AlignHCenter
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 34
                    Layout.topMargin: Kirigami.Units.largeSpacing * 3
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        Layout.alignment: Qt.AlignHCenter
                        color: Kirigami.Theme.disabledTextColor
                        font.bold: true
                        font.letterSpacing: 0.6
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        horizontalAlignment: Text.AlignHCenter
                        text: qsTr("GET STARTED")
                    }

                    Kirigami.Heading {
                        Layout.alignment: Qt.AlignHCenter
                        horizontalAlignment: Text.AlignHCenter
                        level: 1
                        text: qsTr("Add a project")
                    }

                    Controls.Label {
                        Layout.alignment: Qt.AlignHCenter
                        color: Kirigami.Theme.disabledTextColor
                        horizontalAlignment: Text.AlignHCenter
                        text: qsTr("Open a folder already on this machine, or clone a repository into Harkness-managed storage.")
                        wrapMode: Text.Wrap
                    }
                }

                GridLayout {
                    id: actionGrid

                    Layout.alignment: Qt.AlignHCenter
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 34
                    columnSpacing: Kirigami.Units.largeSpacing
                    columns: mainFlick.width > Kirigami.Units.gridUnit * 34 ? 2 : 1
                    rowSpacing: Kirigami.Units.largeSpacing

                    LauncherActionCard {
                        Layout.fillWidth: true
                        iconName: "document-open-folder"
                        subtitle: qsTr("Import a directory that is already on this machine.")
                        title: qsTr("Open Local Folder")
                        onActivated: applicationWindow().chooseLocalFolder()
                    }

                    LauncherActionCard {
                        id: cloneCard

                        Layout.fillWidth: true
                        iconName: "vcs-git"
                        subtitle: qsTr("Clone a repository into Harkness-managed storage.")
                        title: qsTr("Clone from GitHub")
                        onActivated: {
                            cloneForm.expanded = true;
                            remoteField.forceActiveFocus();
                        }
                    }

                    // Inline clone form, revealed by the clone card.
                    ColumnLayout {
                        id: cloneForm

                        property bool expanded: false

                        Layout.columnSpan: actionGrid.columns
                        Layout.fillWidth: true
                        clip: true
                        visible: expanded

                        Behavior on Layout.preferredHeight {
                            NumberAnimation {
                                duration: Kirigami.Units.shortDuration
                                easing.type: Easing.InOutQuad
                            }
                        }

                        Layout.preferredHeight: expanded ? implicitHeight : 0

                        RowLayout {
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.smallSpacing

                            Controls.TextField {
                                id: remoteField

                                property string error: text.trim().length === 0 ? "" : launcher.backend.validateRemote(text)

                                Layout.fillWidth: true
                                Layout.preferredHeight: Kirigami.Units.gridUnit * 2.2
                                enabled: !launcher.backend.busy
                                leftPadding: Kirigami.Units.largeSpacing
                                placeholderText: qsTr("https://github.com/owner/repository.git")
                                rightPadding: Kirigami.Units.largeSpacing
                                selectByMouse: true
                                onAccepted: cloneButton.clicked()

                                background: Rectangle {
                                    border.color: remoteField.activeFocus
                                        ? Kirigami.Theme.focusColor
                                        : remoteField.error.length > 0
                                            ? Kirigami.Theme.negativeTextColor
                                            : launcher.tinted(Kirigami.Theme.textColor, 0.15)
                                    border.width: remoteField.activeFocus ? 2 : 1
                                    color: Kirigami.Theme.backgroundColor
                                    radius: remoteField.height / 2

                                    Behavior on border.color {
                                        ColorAnimation {
                                            duration: Kirigami.Units.shortDuration
                                        }
                                    }
                                }
                            }

                            Controls.Button {
                                id: cloneButton

                                readonly property bool primaryEnabled: !launcher.backend.busy && remoteField.text.trim().length > 0 && remoteField.error.length === 0

                                Controls.ToolTip.text: qsTr("Clone the repository")
                                Layout.preferredHeight: Kirigami.Units.gridUnit * 2.2
                                enabled: primaryEnabled
                                icon.name: "download"
                                text: qsTr("Clone")
                                onClicked: launcher.backend.importRepository(remoteField.text)

                                background: Rectangle {
                                    color: !cloneButton.enabled
                                        ? launcher.tinted(Kirigami.Theme.highlightColor, 0.35)
                                        : cloneButton.pressed
                                            ? Qt.darker(Kirigami.Theme.highlightColor, 1.15)
                                            : cloneButton.hovered
                                                ? Qt.lighter(Kirigami.Theme.highlightColor, 1.1)
                                                : Kirigami.Theme.highlightColor
                                    radius: height / 2

                                    Behavior on color {
                                        ColorAnimation {
                                            duration: Kirigami.Units.shortDuration
                                        }
                                    }
                                }

                                contentItem: RowLayout {
                                    spacing: Kirigami.Units.smallSpacing

                                    Kirigami.Icon {
                                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                        color: Kirigami.Theme.highlightedTextColor
                                        source: cloneButton.icon.name
                                    }

                                    Controls.Label {
                                        Layout.rightMargin: Kirigami.Units.smallSpacing
                                        color: Kirigami.Theme.highlightedTextColor
                                        font.bold: true
                                        text: cloneButton.text
                                    }
                                }
                            }

                            Controls.Button {
                                id: cancelCloneButton

                                Controls.ToolTip.text: qsTr("Stop the running clone")
                                Layout.preferredHeight: Kirigami.Units.gridUnit * 2.2
                                enabled: launcher.backend.busy
                                text: qsTr("Cancel")

                                background: Rectangle {
                                    border.color: launcher.tinted(Kirigami.Theme.textColor, 0.2)
                                    border.width: 1
                                    color: cancelCloneButton.hovered ? launcher.tinted(Kirigami.Theme.textColor, 0.06) : "transparent"
                                    radius: height / 2

                                    Behavior on color {
                                        ColorAnimation {
                                            duration: Kirigami.Units.shortDuration
                                        }
                                    }
                                }

                                contentItem: Controls.Label {
                                    color: Kirigami.Theme.textColor
                                    horizontalAlignment: Text.AlignHCenter
                                    text: cancelCloneButton.text
                                }
                            }
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            color: Kirigami.Theme.negativeTextColor
                            font: Kirigami.Theme.smallFont
                            text: remoteField.error
                            visible: text.length > 0
                            wrapMode: Text.Wrap
                        }

                        RowLayout {
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.smallSpacing
                            visible: launcher.backend.busy

                            Controls.BusyIndicator {
                                Layout.preferredHeight: Kirigami.Units.gridUnit
                                Layout.preferredWidth: Kirigami.Units.gridUnit
                                running: launcher.backend.busy
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                elide: Text.ElideRight
                                font: Kirigami.Theme.smallFont
                                text: launcher.backend.status
                            }
                        }
                    }
                }

                Item {
                    Layout.fillWidth: true
                    Layout.minimumHeight: Kirigami.Units.largeSpacing * 2
                }
            }
        }
    }

    Component.onCompleted: refilter()
}
