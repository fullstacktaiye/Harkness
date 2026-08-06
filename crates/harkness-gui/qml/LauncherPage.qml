import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ScrollablePage {
    id: launcher

    required property HarknessBackend backend

    // Allows Main.qml to distinguish navigation back to this page from a
    // project-page transition and clear the backend's open-project state.
    readonly property bool isLauncher: true

    title: qsTr("Harkness")

    // Describes a row's Git state. Composed here rather than in Rust so every
    // user-visible string stays translatable.
    function describe(project) {
        if (!project.available)
            return qsTr("Missing from disk");
        if (!project.isGit)
            return qsTr("Not a Git repository");
        const branch = project.branch.length > 0 ? project.branch : qsTr("detached HEAD");
        return project.dirty ? qsTr("%1 — uncommitted changes").arg(branch) : branch;
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

    Connections {
        function onProjectsChanged() {
            launcher.refilter();
        }

        target: launcher.backend
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing * 2
        width: launcher.width - Kirigami.Units.largeSpacing * 2

        // The two ways in. Cards adapt: side by side when wide, stacked when narrow.
        GridLayout {
            id: actionGrid

            Layout.fillWidth: true
            columnSpacing: Kirigami.Units.largeSpacing
            columns: launcher.width > Kirigami.Units.gridUnit * 34 ? 2 : 1
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
                        enabled: !launcher.backend.busy
                        placeholderText: qsTr("https://github.com/owner/repository.git")
                        onAccepted: cloneButton.clicked()
                    }

                    Controls.Button {
                        id: cloneButton

                        Controls.ToolTip.text: qsTr("Clone the repository")
                        enabled: !launcher.backend.busy && remoteField.text.trim().length > 0 && remoteField.error.length === 0
                        icon.name: "download"
                        text: qsTr("Clone")
                        onClicked: launcher.backend.importRepository(remoteField.text)
                    }

                    Controls.Button {
                        Controls.ToolTip.text: qsTr("Stop the running clone")
                        enabled: launcher.backend.busy
                        text: qsTr("Cancel")
                        onClicked: launcher.backend.cancelImport()
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

        Kirigami.Separator {
            Layout.fillWidth: true
        }

        // Recents.
        RowLayout {
            Layout.fillWidth: true

            Kirigami.Heading {
                Layout.fillWidth: true
                level: 3
                text: qsTr("Recents")
            }

            Kirigami.SearchField {
                id: searchField

                Layout.preferredWidth: Kirigami.Units.gridUnit * 14
                placeholderText: qsTr("Filter projects")
                onTextChanged: launcher.refilter()
            }
        }

        GridView {
            id: recentGrid

            Layout.fillWidth: true
            Layout.preferredHeight: contentHeight
            cellHeight: Kirigami.Units.gridUnit * 5.5
            cellWidth: width > Kirigami.Units.gridUnit * 34 ? width / 2 : width
            interactive: false
            model: launcher.filteredProjects

            add: Transition {
                NumberAnimation {
                    duration: Kirigami.Units.shortDuration
                    properties: "opacity,scale"
                    from: 0
                    to: 1
                }
            }

            displaced: Transition {
                NumberAnimation {
                    duration: Kirigami.Units.shortDuration
                    properties: "x,y"
                }
            }

            delegate: RecentProjectCard {
                height: GridView.view.cellHeight - Kirigami.Units.smallSpacing
                width: GridView.view.cellWidth - Kirigami.Units.smallSpacing
                onActivated: {
                    if (project.available)
                        launcher.backend.openProject(project.id);
                }
            }
        }

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            icon.name: "folder-open"
            text: qsTr("No projects yet")
            explanation: qsTr("Open a local folder or clone a GitHub repository above.")
            visible: launcher.backend.projects.length === 0
        }

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            icon.name: "edit-none"
            text: qsTr("No matching projects")
            explanation: qsTr("Try a different search.")
            visible: launcher.backend.projects.length > 0 && launcher.filteredProjects.length === 0
        }

        // Status line: clone errors, removal results, cancellations.
        Controls.Label {
            Layout.fillWidth: true
            Layout.bottomMargin: Kirigami.Units.largeSpacing
            color: Kirigami.Theme.disabledTextColor
            elide: Text.ElideRight
            font: Kirigami.Theme.smallFont
            text: launcher.backend.status
            visible: !launcher.backend.busy && text.length > 0
        }
    }

    Component.onCompleted: refilter()
}
