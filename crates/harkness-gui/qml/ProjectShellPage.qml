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
    property bool worktreeFormVisible: false

    title: shell.project.displayName

    onProjectChanged: {
        // A refresh can replace the project object; the root itself only
        // changes in theory, but re-applying it is a no-op then.
        if (project.available)
            fileModel.setRoot(project.root);
        if (project.available && project.isGit)
            backend.refreshBranches(project.id);
        if (project.available && project.isGit && !project.worktree)
            backend.refreshWorktrees(project.id);
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
            icon.name: "vcs-branch"
            text: qsTr("Create worktree…")
            tooltip: qsTr("Create a linked workspace on a new branch, an existing branch, or detached HEAD")
            visible: shell.project.available && shell.project.isGit && !shell.project.worktree
            onTriggered: {
                shell.worktreeFormVisible = !shell.worktreeFormVisible;
                if (shell.worktreeFormVisible)
                    worktreeBranch.forceActiveFocus();
            }
        },
        Kirigami.Action {
            icon.name: "view-refresh"
            text: qsTr("Reconcile worktrees")
            tooltip: qsTr("Remove stale Harkness entries without pruning external worktrees")
            visible: shell.project.available && shell.project.isGit && !shell.project.worktree
            onTriggered: shell.backend.reconcileWorktrees(shell.project.id)
        },
        Kirigami.Action {
            icon.name: "process-stop"
            text: qsTr("Cancel Git operation")
            tooltip: qsTr("Stop the running worktree operation safely")
            visible: shell.backend.busy
            onTriggered: shell.backend.cancelImport()
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

            Controls.Frame {
                Layout.fillWidth: true
                visible: shell.worktreeFormVisible

                ColumnLayout {
                    anchors.fill: parent
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Heading {
                        level: 4
                        text: qsTr("Create linked workspace")
                    }

                    RowLayout {
                        Layout.fillWidth: true

                        Controls.ComboBox {
                            id: worktreeMode

                            readonly property string mode: ["new", "existing", "detached"][currentIndex]

                            Layout.preferredWidth: Kirigami.Units.gridUnit * 10
                            model: [qsTr("New branch"), qsTr("Existing branch"), qsTr("Detached HEAD")]
                        }

                        Controls.TextField {
                            id: worktreeBranch

                            Layout.fillWidth: true
                            enabled: !shell.backend.busy
                            placeholderText: worktreeMode.mode === "existing"
                                ? qsTr("Existing branch to reuse")
                                : qsTr("New branch name")
                            visible: worktreeMode.mode !== "detached"
                        }

                        Controls.TextField {
                            id: worktreeStart

                            Layout.fillWidth: true
                            enabled: !shell.backend.busy
                            placeholderText: worktreeMode.mode === "detached"
                                ? qsTr("Commit or revision")
                                : qsTr("Start point (defaults to HEAD)")
                            text: worktreeMode.mode === "new" ? "HEAD" : ""
                            visible: worktreeMode.mode !== "existing"
                        }

                        Controls.Button {
                            enabled: !shell.backend.busy
                                && (worktreeMode.mode === "detached"
                                    ? worktreeStart.text.trim().length > 0
                                    : worktreeBranch.text.trim().length > 0)
                            icon.name: "list-add"
                            text: qsTr("Create")
                            onClicked: shell.backend.createWorktree(
                                shell.project.id,
                                worktreeMode.mode,
                                worktreeBranch.text,
                                worktreeStart.text
                            )
                        }

                        Controls.Button {
                            text: shell.backend.busy ? qsTr("Stop") : qsTr("Cancel")
                            onClicked: {
                                if (shell.backend.busy)
                                    shell.backend.cancelImport();
                                else
                                    shell.worktreeFormVisible = false;
                            }
                        }
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        font: Kirigami.Theme.smallFont
                        text: shell.backend.worktrees.length === 0
                            ? qsTr("No linked worktrees")
                            : qsTr("%n linked worktree(s), including external checkouts", "", shell.backend.worktrees.length)
                    }

                    Repeater {
                        model: shell.backend.worktrees

                        delegate: Controls.Label {
                            required property var modelData

                            Layout.fillWidth: true
                            elide: Text.ElideMiddle
                            font: Kirigami.Theme.smallFont
                            text: {
                                const branch = modelData.branch.length > 0 ? modelData.branch : qsTr("detached HEAD");
                                const ownership = modelData.owned ? qsTr("Harkness") : qsTr("external");
                                const lock = modelData.locked ? qsTr(", locked") : "";
                                return qsTr("%1 — %2 (%3%4)").arg(branch).arg(modelData.root).arg(ownership).arg(lock);
                            }
                        }
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
        if (shell.project.available && shell.project.isGit && !shell.project.worktree)
            shell.backend.refreshWorktrees(shell.project.id);
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
