import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import QtQuick.Window
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

    /// Identifier of the side-panel view on screen. An empty value means no
    /// view applies to this project, and the activity bar stays hidden.
    property string currentViewId: ""
    /// Whether the side panel is showing beside the review surface.
    property bool sidePanelExpanded: true

    /// The hairline drawn around the shell surface and between its regions.
    /// One colour for the frame and every internal divider is what makes the
    /// whole shell read as a single bordered panel instead of stacked strips.
    readonly property color frameColor: Qt.alpha(Kirigami.Theme.textColor, 0.18)
    readonly property real frameRadius: Kirigami.Units.smallSpacing

    title: shell.shellName
    // The activity bar and the panels beside it are chrome: they run to the
    // window edge instead of floating inside the page's content padding.
    padding: 0

    // The page itself is the window-coloured ground the framed surface sits
    // on, so what shows through the frame's rounded corners matches the title
    // bar above it. Inheritance is off here, so the black ground is restated.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    background: Rectangle {
        color: Kirigami.Theme.backgroundColor
    }

    /// Applies an activity-bar pick the way Visual Studio Code does: another
    /// view switches the panel, the current one toggles it away and back.
    function activateView(viewId) {
        const view = sidePanel.view(viewId);
        if (!view || !view.viewAvailable)
            return;
        if (shell.currentViewId === viewId) {
            shell.sidePanelExpanded = !shell.sidePanelExpanded;
            return;
        }
        shell.currentViewId = viewId;
        shell.sidePanelExpanded = true;
    }

    function toggleSidePanel() {
        if (!sidePanel.hasAvailableView)
            return;
        shell.sidePanelExpanded = !shell.sidePanelExpanded;
    }

    /// Keeps the panel off views that do not apply to the project on screen.
    /// The shell is reused across projects, so what applies changes under it.
    function selectAvailableView() {
        shell.currentViewId = sidePanel.firstAvailableViewId();
    }

    Component.onCompleted: selectAvailableView()

    Connections {
        target: sidePanel

        function onCurrentPanelReadyChanged() {
            if (!sidePanel.currentPanelReady)
                shell.selectAvailableView();
        }

        function onHasAvailableViewChanged() {
            if (sidePanel.hasAvailableView && shell.currentViewId.length === 0)
                shell.selectAvailableView();
        }
    }

    // Visual Studio Code's sidebar bindings, which is what users reach for
    // first. Each view owns the shortcut it advertises in its tooltip.
    Shortcut {
        sequences: ["Ctrl+B"]
        onActivated: shell.toggleSidePanel()
    }

    Shortcut {
        enabled: gitPanel.viewAvailable
        sequences: [gitPanel.viewShortcut]
        onActivated: shell.activateView(gitPanel.viewId)
    }

    Shortcut {
        enabled: issuesPanel.viewAvailable
        sequences: [issuesPanel.viewShortcut]
        onActivated: shell.activateView(issuesPanel.viewId)
    }

    Shortcut {
        enabled: checksPanel.viewAvailable
        sequences: [checksPanel.viewShortcut]
        onActivated: shell.activateView(checksPanel.viewId)
    }

    // The header toolbar answers the same job and Git-state questions the
    // source-control view does; both derive them from this projection so a
    // running operation disables the toolbar and the panel together.
    GitActivity {
        id: shellActivity

        backend: shell.backend
        project: shell.project
    }

    /// Keeps the working-tree status current on its own. Nothing else re-reads
    /// the repository unprompted, so an edit made outside Harkness stayed
    /// invisible until the user asked for a refresh or ran another operation.
    ///
    /// Only status is polled. It is a local read, whereas rerunning history or
    /// branches on a timer would repeat the log walk and reset what those
    /// panes have scrolled to.
    Timer {
        interval: 15000
        repeat: true
        // An unfocused window is looking at nothing, and the focus fetch in
        // Main.qml already brings state current on the way back in. The
        // attached property is read through the page rather than here: it is
        // offered to Items and Windows only, and a Timer is neither.
        running: shell.project.available && shell.project.isGit
            && shell.Window.window !== null && shell.Window.window.active
        onTriggered: {
            // A tick during another operation would be refused by the backend
            // and reported as "wait for … to finish", overwriting the running
            // job's own status line every interval.
            if (shellActivity.repositoryOperationRunning())
                return;
            shell.backend.refreshGit(shell.project.id);
        }
    }

    // The whole shell — header band, activity bar and the current view — sits
    // on this one framed surface, run to the page edge so the border wraps the
    // entire look rather than each strip drawing its own edge.
    Rectangle {
        id: frame

        anchors.fill: parent
        border.color: shell.frameColor
        border.width: 1
        color: Kirigami.Theme.backgroundColor
        radius: shell.frameRadius

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: frame.border.width
            spacing: 0

            // The shell draws its own header instead of handing it to
            // Kirigami's `header` slot: with the global toolbar switched off,
            // that slot is laid out against a toolbar row that is no longer
            // there and squeezes its content into a fraction of the height it
            // asked for. It takes the Header palette so it reads as chrome,
            // matching the title bar above the frame.
            Item {
                id: headerBand

                Layout.fillWidth: true
                implicitHeight: headerRow.implicitHeight + Kirigami.Units.largeSpacing

                Kirigami.Theme.colorSet: Kirigami.Theme.Header
                Kirigami.Theme.inherit: false
                Kirigami.Theme.backgroundColor: "#000000"
                Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
                Kirigami.Theme.textColor: "#ffffff"

                Rectangle {
                    anchors.fill: parent
                    color: Kirigami.Theme.backgroundColor
                }

                RowLayout {
                    id: headerRow

                    anchors.left: parent.left
                    anchors.leftMargin: Kirigami.Units.largeSpacing
                    anchors.right: parent.right
                    anchors.rightMargin: Kirigami.Units.largeSpacing
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Kirigami.Units.largeSpacing

                    // Leaving the project is the one navigation step the shell
                    // owns, so it leads the header the way a browser's back button
                    // leads its toolbar. Alt+Left comes from the File menu.
                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Back to the launcher")
                        display: Controls.AbstractButton.IconOnly
                        icon.name: "go-previous-symbolic"
                        onClicked: shell.backend.closeProject()
                    }

                    // The project icon sits on its own tile so the identity
                    // block has an anchor point instead of an icon floating
                    // in the row.
                    Rectangle {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium + Kirigami.Units.smallSpacing * 2
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium + Kirigami.Units.smallSpacing * 2
                        border.color: shell.frameColor
                        border.width: 1
                        color: Kirigami.Theme.alternateBackgroundColor
                        radius: shell.frameRadius

                        Kirigami.Icon {
                            anchors.centerIn: parent
                            height: Kirigami.Units.iconSizes.smallMedium
                            source: shell.project.isGit ? "folder-git" : "folder"
                            width: Kirigami.Units.iconSizes.smallMedium
                        }
                    }

                    // Name and path share one baseline-aligned row: the header
                    // stays a single line tall however long either grows.
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Kirigami.Heading {
                            Layout.alignment: Qt.AlignBaseline
                            Layout.maximumWidth: headerRow.width / 2
                            elide: Text.ElideRight
                            level: 3
                            text: shell.shellName
                        }

                        Controls.Label {
                            Layout.alignment: Qt.AlignBaseline
                            Layout.fillWidth: true
                            color: Kirigami.Theme.disabledTextColor
                            elide: Text.ElideMiddle
                            font.family: "monospace"
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: shell.project.root
                        }

                        // Refreshes every repository projection the header and
                        // the source-control view read, mirroring GitPanel's
                        // own refresh so both surfaces stay in step.
                        Controls.ToolButton {
                            Controls.ToolTip.text: qsTr("Refresh repository")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: !shellActivity.repositoryOperationRunning()
                            icon.name: "view-refresh"
                            text: qsTr("Refresh repository")
                            visible: shell.project.isGit && shell.project.available
                            onClicked: {
                                shell.backend.refreshGit(shell.project.id);
                                shell.backend.refreshBranches(shell.project.id);
                                shell.backend.refreshHistory(shell.project.id);
                                if (!shell.project.worktree)
                                    shell.backend.refreshWorktrees(shell.project.id);
                            }
                        }
                    }

                    RowLayout {
                        spacing: Kirigami.Units.smallSpacing

                        Kirigami.Chip {
                            checkable: false
                            closable: false
                            text: shell.project.managed
                                ? qsTr("Managed clone")
                                : qsTr("Local folder")
                            visible: !shell.project.isGit || !shell.project.available
                        }

                        Kirigami.Chip {
                            checkable: false
                            closable: false
                            text: qsTr("Missing from disk")
                            visible: !shell.project.available
                        }

                        // Branch, worktree, and remote state are the repository
                        // controls proper: they name what is checked out and let it
                        // be changed from where it is displayed.
                        RepositoryToolbar {
                            activity: shellActivity
                            backend: shell.backend
                            project: shell.project
                            visible: shell.project.isGit && shell.project.available
                        }
                    }
                }
            }

            Rectangle {
                Layout.fillWidth: true
                color: shell.frameColor
                implicitHeight: 1
            }

            RowLayout {
                Layout.fillHeight: true
                Layout.fillWidth: true
                spacing: 0

                ActivityBar {
                    Layout.fillHeight: true
                    currentViewId: shell.currentViewId
                    panelExpanded: shell.sidePanelExpanded
                    views: sidePanel.views
                    visible: sidePanel.hasAvailableView
                    onViewTriggered: viewId => shell.activateView(viewId)
                }

                Rectangle {
                    Layout.fillHeight: true
                    color: shell.frameColor
                    implicitWidth: 1
                    visible: sidePanel.hasAvailableView
                }

                // The current view owns everything right of the activity bar, so
                // collapsing it takes the whole surface rather than baring a companion
                // pane the activity bar never advertised.
                SidePanel {
                    id: sidePanel

                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    currentViewId: shell.currentViewId
                    visible: shell.sidePanelExpanded && sidePanel.currentPanelReady

                    GitPanel {
                        id: gitPanel

                        backend: shell.backend
                        project: shell.project
                        onHideRequested: shell.sidePanelExpanded = false
                    }

                    IssuesPanel {
                        id: issuesPanel

                        backend: shell.backend
                        project: shell.project
                        onHideRequested: shell.sidePanelExpanded = false
                    }

                    ChecksPanel {
                        id: checksPanel

                        backend: shell.backend
                        project: shell.project
                        onHideRequested: shell.sidePanelExpanded = false
                    }
                }

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
        }

        // The panels inside paint square corners, so the frame's stroke is
        // drawn once more on top of them to keep the rounded outline crisp.
        Rectangle {
            anchors.fill: parent
            border.color: shell.frameColor
            border.width: 1
            color: "transparent"
            radius: frame.radius
        }
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
