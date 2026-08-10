import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Repository controls in the shell header: which branch is checked out, which
/// worktree is open, and what the remote owes this checkout.
///
/// These three answers change what every other surface shows, so they sit in
/// the header where they stay visible whatever the side panel is doing —
/// switching a branch from inside a scrolled "Branches" section left the state
/// it changed off screen. Each cell states the current value and opens the list
/// it can be changed to, creation included.
RowLayout {
    id: toolbar

    required property var backend
    required property var project
    /// Shared job and Git-state projection; see GitActivity.qml.
    required property var activity

    readonly property var gitState: activity.gitState
    readonly property int ahead: activity.stateReady ? Number(gitState.ahead || 0) : 0
    readonly property int behind: activity.stateReady ? Number(gitState.behind || 0) : 0

    property string movingWorktreeId: ""
    property string movingWorktreeName: ""
    property string movingWorktreeRoot: ""
    property string lockingWorktreeId: ""
    property string lockingWorktreeName: ""

    spacing: Kirigami.Units.smallSpacing

    // InlineMessage and tooltips do not expose their label's textFormat. Wrap an
    // escaped value so repository-controlled text is displayed literally.
    function escapedRichText(value) {
        return "<span>" + String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;") + "</span>";
    }

    function pathBaseName(path) {
        let value = String(path).replace(/[\\/]+$/, "");
        const separator = Math.max(value.lastIndexOf("/"), value.lastIndexOf("\\"));
        return separator >= 0 ? value.substring(separator + 1) : value;
    }

    function joinPath(parent, child) {
        const value = String(parent).replace(/[\\/]+$/, "");
        return value + "/" + child;
    }

    function urlToPath(url) {
        let value = url.toString();
        if (value.startsWith("file://"))
            value = value.substring(7);
        return decodeURIComponent(value);
    }

    ToolbarSegment {
        id: branchSegment

        caption: qsTr("Current branch")
        enabled: !toolbar.activity.repositoryOperationRunning()
        icon.name: "vcs-branch"
        value: toolbar.activity.currentBranch.length > 0
            ? toolbar.activity.currentBranch
            : qsTr("detached HEAD")
        onClicked: branchPopup.visible ? branchPopup.close() : branchPopup.open()

        Controls.Popup {
            id: branchPopup

            // `ComboBox`-style role lookup is unreliable while the backend is
            // repopulating the model, so the rows are read as plain maps.
            readonly property var matches: {
                const needle = branchFilter.text.trim().toLowerCase();
                const rows = [];
                for (let index = 0; index < toolbar.backend.branches.length; ++index) {
                    const row = toolbar.backend.branches[index];
                    if (needle.length === 0
                            || String(row.name || "").toLowerCase().indexOf(needle) !== -1)
                        rows.push(row);
                }
                return rows;
            }

            closePolicy: Controls.Popup.CloseOnEscape
                | Controls.Popup.CloseOnPressOutsideParent
            padding: Kirigami.Units.smallSpacing
            width: Kirigami.Units.gridUnit * 20
            y: branchSegment.height

            onOpened: branchFilter.forceActiveFocus()
            onClosed: newBranchForm.visible = false

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Branches")
                    }

                    Controls.ToolButton {
                        checkable: true
                        checked: newBranchForm.visible
                        icon.name: "list-add"
                        text: qsTr("New branch…")
                        onClicked: {
                            newBranchForm.visible = !newBranchForm.visible;
                            if (newBranchForm.visible)
                                newBranch.forceActiveFocus();
                        }
                    }
                }

                Controls.TextField {
                    id: branchFilter

                    Layout.fillWidth: true
                    placeholderText: qsTr("Filter branches")
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    text: qsTr("No branch matches this filter")
                    visible: branchPopup.matches.length === 0
                    wrapMode: Text.Wrap
                }

                ListView {
                    id: branchList

                    Layout.fillWidth: true
                    Layout.preferredHeight: Math.min(
                        Kirigami.Units.gridUnit * 12,
                        contentHeight
                    )
                    boundsBehavior: Flickable.StopAtBounds
                    clip: true
                    model: branchPopup.matches
                    reuseItems: true

                    delegate: Controls.ItemDelegate {
                        id: branchDelegate

                        required property var modelData

                        Controls.ToolTip.text: toolbar.escapedRichText(modelData.detail)
                        Controls.ToolTip.visible: hovered && modelData.detail.length > 0
                        enabled: modelData.selectable
                            && !toolbar.activity.repositoryOperationRunning()
                        highlighted: modelData.current
                        width: branchList.width
                        onClicked: {
                            branchPopup.close();
                            const selected = String(modelData.name);
                            if (selected !== toolbar.activity.currentBranch)
                                toolbar.backend.checkoutBranch(toolbar.project.id, selected);
                        }

                        contentItem: RowLayout {
                            spacing: Kirigami.Units.smallSpacing

                            Kirigami.Icon {
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                source: "checkmark"
                                visible: branchDelegate.modelData.current
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                elide: Text.ElideMiddle
                                text: branchDelegate.modelData.name
                                textFormat: Text.PlainText
                            }
                        }
                    }

                    Controls.ScrollBar.vertical: Controls.ScrollBar {}
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: newBranchForm.visible
                }

                ColumnLayout {
                    id: newBranchForm

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: false

                    Controls.TextField {
                        id: newBranch

                        Layout.fillWidth: true
                        enabled: toolbar.activity.job("create_branch") === null
                            && !toolbar.activity.repositoryOperationRunning()
                        placeholderText: qsTr("New branch name")
                    }

                    Controls.TextField {
                        id: branchStart

                        Layout.fillWidth: true
                        enabled: toolbar.activity.job("create_branch") === null
                            && !toolbar.activity.repositoryOperationRunning()
                        placeholderText: qsTr("Start point")
                        text: "HEAD"
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: toolbar.activity.job("create_branch") === null
                            && !toolbar.activity.repositoryOperationRunning()
                            && newBranch.text.trim().length > 0
                        icon.name: "list-add"
                        text: qsTr("Create and switch")
                        onClicked: {
                            toolbar.backend.createBranch(
                                toolbar.project.id,
                                newBranch.text,
                                branchStart.text
                            );
                            newBranch.text = "";
                            branchPopup.close();
                        }
                    }
                }
            }
        }
    }

    ToolbarSegment {
        id: worktreeSegment

        caption: qsTr("Current worktree")
        enabled: !toolbar.activity.repositoryOperationRunning()
        icon.name: toolbar.project.worktree ? "folder-open" : "folder-git"
        value: toolbar.project.worktree
            ? (toolbar.project.branch.length > 0
                ? toolbar.project.branch
                : qsTr("detached HEAD"))
            : qsTr("Primary checkout")
        onClicked: worktreePopup.visible ? worktreePopup.close() : worktreePopup.open()

        Controls.Popup {
            id: worktreePopup

            closePolicy: Controls.Popup.CloseOnEscape
                | Controls.Popup.CloseOnPressOutsideParent
            padding: Kirigami.Units.smallSpacing
            width: Kirigami.Units.gridUnit * 24
            y: worktreeSegment.height

            onClosed: {
                worktreeForm.visible = false;
                lockWorktreeForm.visible = false;
                moveWorktreeForm.visible = false;
            }

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Worktrees")
                    }

                    Controls.ToolButton {
                        checkable: true
                        checked: worktreeForm.visible
                        enabled: !toolbar.activity.repositoryOperationRunning()
                        icon.name: "list-add"
                        text: qsTr("New worktree…")
                        visible: !toolbar.project.worktree
                        onClicked: worktreeForm.visible = !worktreeForm.visible
                    }

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Reconcile worktrees with Git")
                        Controls.ToolTip.visible: hovered
                        display: Controls.AbstractButton.IconOnly
                        enabled: !toolbar.activity.repositoryOperationRunning()
                        icon.name: "view-refresh"
                        text: qsTr("Reconcile")
                        visible: !toolbar.project.worktree
                        onClicked: toolbar.backend.reconcileWorktrees(toolbar.project.id)
                    }
                }

                // A worktree cannot list its siblings: the catalog answers that
                // question for the parent project only.
                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    text: qsTr("This workspace comes from %1.").arg(toolbar.project.parentName)
                    textFormat: Text.PlainText
                    visible: toolbar.project.worktree
                    wrapMode: Text.Wrap
                }

                Controls.Button {
                    Layout.fillWidth: true
                    enabled: !toolbar.activity.repositoryOperationRunning()
                    icon.name: "folder-git"
                    text: qsTr("Open %1").arg(toolbar.project.parentName)
                    visible: toolbar.project.worktree
                        && String(toolbar.project.parentId || "").length > 0
                    onClicked: {
                        worktreePopup.close();
                        toolbar.backend.openProject(toolbar.project.parentId);
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    text: qsTr("No linked worktrees yet")
                    visible: !toolbar.project.worktree
                        && toolbar.backend.worktrees.length === 0
                    wrapMode: Text.Wrap
                }

                ListView {
                    id: worktreeList

                    Layout.fillWidth: true
                    Layout.preferredHeight: Math.min(
                        Kirigami.Units.gridUnit * 14,
                        contentHeight
                    )
                    boundsBehavior: Flickable.StopAtBounds
                    clip: true
                    model: toolbar.project.worktree ? [] : toolbar.backend.worktrees
                    reuseItems: true
                    spacing: Kirigami.Units.smallSpacing

                    delegate: Controls.ItemDelegate {
                        id: worktreeDelegate

                        required property var modelData
                        readonly property var row: modelData

                        // Only a Harkness-owned worktree is a project the shell
                        // can open; an external one is listed for awareness.
                        enabled: row.owned && !toolbar.activity.repositoryOperationRunning()
                        width: worktreeList.width
                        onClicked: {
                            worktreePopup.close();
                            toolbar.backend.openProject(worktreeDelegate.row.id);
                        }

                        contentItem: ColumnLayout {
                            spacing: 0

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Label {
                                    Layout.fillWidth: true
                                    elide: Text.ElideRight
                                    font.bold: true
                                    text: worktreeDelegate.row.branch.length > 0
                                        ? worktreeDelegate.row.branch
                                        : qsTr("detached HEAD")
                                    textFormat: Text.PlainText
                                }

                                Kirigami.Icon {
                                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                    source: "object-locked"
                                    visible: worktreeDelegate.row.locked
                                }
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                elide: Text.ElideMiddle
                                font: Kirigami.Theme.smallFont
                                text: qsTr("%1 (%2)")
                                    .arg(worktreeDelegate.row.root)
                                    .arg(worktreeDelegate.row.owned
                                        ? qsTr("Harkness")
                                        : qsTr("external"))
                                textFormat: Text.PlainText
                            }

                            // A lock is lifecycle policy owned by the catalog.
                            // Show Git's reason in the row so protection is
                            // visible before a move or removal is attempted.
                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.neutralTextColor
                                font: Kirigami.Theme.smallFont
                                text: worktreeDelegate.row.lockReason.length > 0
                                    ? qsTr("Locked: %1").arg(worktreeDelegate.row.lockReason)
                                    : qsTr("Locked without a recorded reason")
                                textFormat: Text.PlainText
                                visible: worktreeDelegate.row.locked
                                wrapMode: Text.Wrap
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                visible: worktreeDelegate.row.owned

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !toolbar.activity.repositoryOperationRunning()
                                        && toolbar.activity.job(
                                            worktreeDelegate.row.locked
                                                ? "unlock_worktree"
                                                : "lock_worktree",
                                            worktreeDelegate.row.id
                                        ) === null
                                    icon.name: worktreeDelegate.row.locked
                                        ? "object-unlocked"
                                        : "object-locked"
                                    text: worktreeDelegate.row.locked
                                        ? qsTr("Unlock")
                                        : qsTr("Lock…")
                                    onClicked: {
                                        if (worktreeDelegate.row.locked) {
                                            toolbar.backend.unlockWorktree(worktreeDelegate.row.id);
                                            return;
                                        }
                                        toolbar.lockingWorktreeId = String(worktreeDelegate.row.id);
                                        toolbar.lockingWorktreeName = worktreeDelegate.row.branch.length > 0
                                            ? String(worktreeDelegate.row.branch)
                                            : qsTr("detached HEAD");
                                        lockReason.text = "";
                                        lockWorktreeForm.visible = true;
                                        moveWorktreeForm.visible = false;
                                        lockReason.forceActiveFocus();
                                    }
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !toolbar.activity.repositoryOperationRunning()
                                        && !worktreeDelegate.row.locked
                                        && toolbar.activity.job(
                                            "move_worktree",
                                            worktreeDelegate.row.id
                                        ) === null
                                    icon.name: "folder-move"
                                    text: worktreeDelegate.row.locked
                                        ? qsTr("Unlock before moving")
                                        : qsTr("Move…")
                                    onClicked: {
                                        toolbar.movingWorktreeId = String(worktreeDelegate.row.id);
                                        toolbar.movingWorktreeName = worktreeDelegate.row.branch.length > 0
                                            ? String(worktreeDelegate.row.branch)
                                            : qsTr("detached HEAD");
                                        toolbar.movingWorktreeRoot = String(worktreeDelegate.row.root);
                                        moveDestination.text = "";
                                        moveWorktreeForm.visible = true;
                                        lockWorktreeForm.visible = false;
                                        moveDestination.forceActiveFocus();
                                    }
                                }
                            }
                        }
                    }

                    Controls.ScrollBar.vertical: Controls.ScrollBar {}
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                    visible: worktreeForm.visible
                        || lockWorktreeForm.visible
                        || moveWorktreeForm.visible
                }

                ColumnLayout {
                    id: worktreeForm

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: false

                    Controls.ComboBox {
                        id: worktreeMode

                        readonly property string mode: ["new", "existing", "detached"][currentIndex]

                        Layout.fillWidth: true
                        model: [qsTr("New branch"), qsTr("Existing branch"), qsTr("Detached HEAD")]
                    }

                    Controls.TextField {
                        id: worktreeBranch

                        Layout.fillWidth: true
                        placeholderText: worktreeMode.mode === "existing"
                            ? qsTr("Existing branch")
                            : qsTr("New branch name")
                        visible: worktreeMode.mode !== "detached"
                    }

                    Controls.TextField {
                        id: worktreeStart

                        Layout.fillWidth: true
                        placeholderText: worktreeMode.mode === "detached"
                            ? qsTr("Commit or revision")
                            : qsTr("Start point")
                        text: worktreeMode.mode === "new" ? "HEAD" : ""
                        visible: worktreeMode.mode !== "existing"
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: !toolbar.activity.repositoryOperationRunning()
                            && (worktreeMode.mode === "detached"
                                ? worktreeStart.text.trim().length > 0
                                : worktreeBranch.text.trim().length > 0)
                        icon.name: "list-add"
                        text: qsTr("Review and create…")
                        onClicked: createWorktreeDialog.open()
                    }
                }

                ColumnLayout {
                    id: lockWorktreeForm

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: false

                    Controls.Label {
                        Layout.fillWidth: true
                        text: qsTr("Lock %1").arg(toolbar.lockingWorktreeName)
                        textFormat: Text.PlainText
                    }

                    Controls.TextField {
                        id: lockReason

                        Layout.fillWidth: true
                        placeholderText: qsTr("Required reason for protecting this worktree")
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: !toolbar.activity.repositoryOperationRunning()
                            && lockReason.text.trim().length > 0
                            && toolbar.activity.job(
                                "lock_worktree",
                                toolbar.lockingWorktreeId
                            ) === null
                        icon.name: "object-locked"
                        text: qsTr("Lock worktree")
                        onClicked: {
                            toolbar.backend.lockWorktree(toolbar.lockingWorktreeId, lockReason.text);
                            lockWorktreeForm.visible = false;
                        }
                    }
                }

                ColumnLayout {
                    id: moveWorktreeForm

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: false

                    Controls.Label {
                        Layout.fillWidth: true
                        text: qsTr("Move %1").arg(toolbar.movingWorktreeName)
                        textFormat: Text.PlainText
                    }

                    RowLayout {
                        Layout.fillWidth: true

                        Controls.TextField {
                            id: moveDestination

                            Layout.fillWidth: true
                            placeholderText: qsTr("Absolute destination path")
                        }

                        Controls.ToolButton {
                            Controls.ToolTip.text: qsTr("Choose destination parent folder")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "document-open-folder"
                            text: qsTr("Choose destination")
                            onClicked: moveParentDialog.open()
                        }
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: !toolbar.activity.repositoryOperationRunning()
                            && moveDestination.text.trim().length > 0
                            && toolbar.activity.job(
                                "move_worktree",
                                toolbar.movingWorktreeId
                            ) === null
                        icon.name: "folder-move"
                        text: qsTr("Review and move…")
                        onClicked: moveWorktreeDialog.open()
                    }
                }
            }
        }
    }

    ToolbarSegment {
        id: syncSegment

        readonly property var runningJob: {
            const running = toolbar.activity.networkJobs();
            return running.length > 0 ? running[0] : null;
        }

        badge: {
            switch (toolbar.activity.syncAction()) {
            case "pull": return String(toolbar.behind);
            case "push": return String(toolbar.ahead);
            default: return "";
            }
        }
        busy: runningJob !== null
        caption: runningJob !== null
            ? qsTr("%1: %2").arg(runningJob.label).arg(runningJob.progress)
            : toolbar.activity.stateReady && toolbar.gitState.upstream
                ? qsTr("%1 · %2 ahead · %3 behind")
                    .arg(toolbar.gitState.upstream)
                    .arg(toolbar.ahead)
                    .arg(toolbar.behind)
                : qsTr("No upstream configured")
        enabled: !toolbar.activity.repositoryOperationRunning()
        expandable: false
        icon.name: {
            switch (toolbar.activity.syncAction()) {
            case "pull": return "go-down";
            case "push": return "go-up";
            default: return "download";
            }
        }
        value: {
            switch (toolbar.activity.syncAction()) {
            case "pull": return qsTr("Pull");
            case "push": return qsTr("Push");
            default: return qsTr("Fetch");
            }
        }
        onClicked: {
            switch (toolbar.activity.syncAction()) {
            case "pull":
                toolbar.backend.pull(toolbar.project.id);
                break;
            case "push":
                toolbar.backend.push(toolbar.project.id, false);
                break;
            default:
                toolbar.backend.fetch(toolbar.project.id);
                break;
            }
        }
    }

    Controls.ToolButton {
        Controls.ToolTip.text: qsTr("Cancel %1").arg(
            syncSegment.runningJob ? syncSegment.runningJob.label : ""
        )
        Controls.ToolTip.visible: hovered
        display: Controls.AbstractButton.IconOnly
        enabled: syncSegment.runningJob !== null
            && syncSegment.runningJob.cancellable
        icon.name: "dialog-cancel"
        text: qsTr("Cancel")
        visible: syncSegment.runningJob !== null
        onClicked: toolbar.backend.cancelJob(syncSegment.runningJob.id)
    }

    FolderDialog {
        id: moveParentDialog

        title: qsTr("Choose the new parent folder")
        onAccepted: {
            const destination = toolbar.urlToPath(selectedFolder);
            const leaf = toolbar.pathBaseName(toolbar.movingWorktreeRoot);
            moveDestination.text = leaf.length > 0
                ? toolbar.joinPath(destination, leaf)
                : destination;
        }
    }

    Kirigami.PromptDialog {
        id: createWorktreeDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: toolbar.project.managed
            ? qsTr("Harkness will add a linked worktree from the repository at:\n%1\n\nThe current checkout is untouched.").arg(toolbar.project.root)
            : qsTr("Harkness will add a linked worktree for the local project at:\n%1\n\nYour existing working tree and files are untouched.").arg(toolbar.project.root)
        title: qsTr("Create linked worktree?")
        onAccepted: {
            toolbar.backend.createWorktree(
                toolbar.project.id,
                worktreeMode.mode,
                worktreeBranch.text,
                worktreeStart.text
            );
            worktreePopup.close();
        }
    }

    Kirigami.PromptDialog {
        id: moveWorktreeDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: qsTr("Git will relocate %1 to:\n%2\n\nThe destination must be an absolute path that does not already exist.")
            .arg(toolbar.movingWorktreeName)
            .arg(moveDestination.text.trim())
        title: qsTr("Move linked worktree?")
        onAccepted: {
            toolbar.backend.moveWorktree(toolbar.movingWorktreeId, moveDestination.text.trim());
            moveWorktreeForm.visible = false;
        }
    }
}
