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

    /// Identifier of the side-panel view on screen. An empty value means no
    /// view applies to this project, and the activity bar stays hidden.
    property string currentViewId: ""
    /// Whether the side panel is showing beside the review surface.
    property bool sidePanelExpanded: true

    title: shell.shellName
    // The activity bar and the panels beside it are chrome: they run to the
    // window edge instead of floating inside the page's content padding.
    padding: 0

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

        ActivityBar {
            Layout.fillHeight: true
            currentViewId: shell.currentViewId
            panelExpanded: shell.sidePanelExpanded
            views: sidePanel.views
            visible: sidePanel.hasAvailableView
            onViewTriggered: viewId => shell.activateView(viewId)
        }

        Kirigami.Separator {
            Layout.fillHeight: true
            visible: sidePanel.hasAvailableView
        }

        Controls.SplitView {
            Layout.fillHeight: true
            Layout.fillWidth: true
            orientation: Qt.Horizontal

            handle: Rectangle {
                readonly property bool active: Controls.SplitHandle.hovered
                    || Controls.SplitHandle.pressed

                color: active ? Kirigami.Theme.highlightColor : "transparent"
                implicitWidth: Kirigami.Units.smallSpacing

                Kirigami.Separator {
                    anchors.horizontalCenter: parent.horizontalCenter
                    height: parent.height
                    visible: !parent.active
                }
            }

            SidePanel {
                id: sidePanel

                Controls.SplitView.fillWidth: false
                Controls.SplitView.maximumWidth: Kirigami.Units.gridUnit * 40
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 18
                Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 23
                currentViewId: shell.currentViewId
                visible: shell.sidePanelExpanded && sidePanel.currentPanelReady
                onHideRequested: shell.sidePanelExpanded = false

                GitPanel {
                    id: gitPanel

                    backend: shell.backend
                    project: shell.project
                }
            }

            ReviewPanel {
                objectName: "reviewSidePanel"

                Controls.SplitView.fillWidth: true
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 26
                backend: shell.backend
                project: shell.project
                visible: shell.project.available && shell.project.isGit
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

    Kirigami.PlaceholderMessage {
        anchors.centerIn: parent
        icon.name: "dialog-warning"
        text: qsTr("Project unavailable")
        explanation: qsTr("The directory %1 no longer exists on disk.").arg(shell.project.root)
        visible: !shell.project.available
        width: parent.width - Kirigami.Units.gridUnit * 4
    }
}
