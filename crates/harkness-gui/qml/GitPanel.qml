import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Item {
    id: panel

    required property var backend
    required property var project

    readonly property bool stateReady: backend.git
        && backend.git.projectId !== undefined
        && String(backend.git.projectId) === String(project.id)
    readonly property var gitState: stateReady ? backend.git : ({})
    readonly property var entries: stateReady && gitState.entries !== undefined
        ? gitState.entries
        : []

    implicitWidth: Kirigami.Units.gridUnit * 22

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.alternateBackgroundColor
    }

    function job(kind) {
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if (String(candidate.projectId) === String(project.id) && candidate.kind === kind)
                return candidate;
        }
        return null;
    }

    function networkJobs() {
        const running = [];
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if (String(candidate.projectId) === String(project.id)
                    && ["fetch", "pull", "push"].indexOf(candidate.kind) !== -1)
                running.push(candidate);
        }
        return running;
    }

    function refresh() {
        if (!project.available || !project.isGit)
            return;
        backend.refreshGit(project.id);
        backend.refreshBranches(project.id);
        if (!project.worktree)
            backend.refreshWorktrees(project.id);
    }

    onProjectChanged: refresh()
    Component.onCompleted: refresh()

    Controls.ScrollView {
        id: scroll

        anchors.fill: parent
        clip: true
        contentWidth: availableWidth

        ColumnLayout {
            spacing: 0
            width: scroll.availableWidth

            RowLayout {
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                Layout.bottomMargin: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    Layout.fillWidth: true
                    level: 3
                    text: qsTr("Git")
                }

                Controls.ToolButton {
                    Controls.ToolTip.text: qsTr("Refresh Git status")
                    display: Controls.AbstractButton.IconOnly
                    enabled: panel.job("status") === null
                    icon.name: "view-refresh"
                    onClicked: panel.backend.refreshGit(panel.project.id)
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    font.bold: true
                    text: panel.stateReady ? panel.gitState.head : qsTr("Loading repository status…")
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    text: {
                        if (!panel.stateReady || !panel.gitState.upstream)
                            return qsTr("No upstream configured");
                        return qsTr("%1 · %2 ahead · %3 behind")
                            .arg(panel.gitState.upstream)
                            .arg(panel.gitState.ahead)
                            .arg(panel.gitState.behind);
                    }
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    text: qsTr("A %1 is waiting to be resolved or aborted.").arg(panel.gitState.pending || "")
                    type: Kirigami.MessageType.Warning
                    visible: panel.stateReady && panel.gitState.pending && panel.gitState.pending.length > 0
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    text: panel.gitState.error || ""
                    type: Kirigami.MessageType.Error
                    visible: panel.stateReady && panel.gitState.error && panel.gitState.error.length > 0
                }

                Controls.Button {
                    Layout.alignment: Qt.AlignRight
                    enabled: panel.job("push") === null
                    icon.name: "dialog-warning"
                    text: qsTr("Push to default branch anyway…")
                    visible: panel.stateReady
                        && ["default_branch_push", "default_branch_unknown"].indexOf(panel.gitState.errorKind) !== -1
                    onClicked: pushOverrideDialog.open()
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    Kirigami.Heading {
                        Layout.fillWidth: true
                        level: 4
                        text: qsTr("Changes")
                    }

                    Controls.Label {
                        color: Kirigami.Theme.disabledTextColor
                        text: panel.entries.length
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.positiveTextColor
                    text: qsTr("Working tree clean")
                    visible: panel.stateReady && panel.entries.length === 0
                }

                Repeater {
                    model: panel.entries

                    delegate: Controls.Frame {
                        required property var modelData

                        Layout.fillWidth: true
                        padding: Kirigami.Units.smallSpacing

                        contentItem: ColumnLayout {
                            spacing: Kirigami.Units.smallSpacing

                            Controls.Label {
                                Layout.fillWidth: true
                                elide: Text.ElideMiddle
                                font.family: "monospace"
                                text: modelData.path
                            }

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: modelData.conflicted
                                        ? Kirigami.Theme.negativeTextColor
                                        : Kirigami.Theme.disabledTextColor
                                    font: Kirigami.Theme.smallFont
                                    text: {
                                        const states = [];
                                        if (modelData.staged)
                                            states.push(qsTr("staged: %1").arg(modelData.staged));
                                        if (modelData.unstaged)
                                            states.push(qsTr("working tree: %1").arg(modelData.unstaged));
                                        if (modelData.conflicted)
                                            states.push(qsTr("conflict"));
                                        return states.join(" · ");
                                    }
                                }

                                Controls.Button {
                                    enabled: panel.job("unstage") === null
                                    text: qsTr("Unstage")
                                    visible: modelData.staged.length > 0
                                    onClicked: panel.backend.unstagePath(panel.project.id, modelData.path)
                                }

                                Controls.Button {
                                    enabled: panel.job("stage") === null
                                    text: qsTr("Stage")
                                    visible: modelData.unstaged.length > 0
                                    onClicked: panel.backend.stagePath(panel.project.id, modelData.path)
                                }
                            }
                        }
                    }
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    level: 4
                    text: qsTr("Commit")
                }

                Controls.TextField {
                    id: commitMessage

                    Layout.fillWidth: true
                    enabled: panel.job("commit") === null
                    placeholderText: qsTr("Commit message")
                }

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("commit") === null && commitMessage.text.trim().length > 0
                        text: qsTr("Amend")
                        onClicked: panel.backend.commit(panel.project.id, commitMessage.text, true)
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("commit") === null && commitMessage.text.trim().length > 0
                        highlighted: true
                        text: qsTr("Commit")
                        onClicked: panel.backend.commit(panel.project.id, commitMessage.text, false)
                    }
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    level: 4
                    text: qsTr("Remote")
                }

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("fetch") === null
                        icon.name: "download"
                        text: qsTr("Fetch")
                        onClicked: panel.backend.fetch(panel.project.id)
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("pull") === null
                        icon.name: "go-down"
                        text: qsTr("Pull")
                        onClicked: panel.backend.pull(panel.project.id)
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("push") === null
                        icon.name: "go-up"
                        text: qsTr("Push")
                        onClicked: panel.backend.push(panel.project.id, false)
                    }
                }

                Repeater {
                    model: panel.networkJobs()

                    delegate: RowLayout {
                        required property var modelData

                        Layout.fillWidth: true

                        Controls.BusyIndicator {
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            running: true
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            elide: Text.ElideRight
                            text: qsTr("%1: %2").arg(modelData.label).arg(modelData.progress)
                        }

                        Controls.Button {
                            enabled: modelData.cancellable
                            text: qsTr("Cancel")
                            onClicked: panel.backend.cancelJob(modelData.id)
                        }
                    }
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    level: 4
                    text: qsTr("Branches")
                }

                Controls.ComboBox {
                    id: branchPicker

                    Layout.fillWidth: true
                    enabled: panel.job("checkout") === null
                    model: panel.backend.branches
                    textRole: "name"
                    valueRole: "name"

                    currentIndex: {
                        for (let index = 0; index < count; ++index) {
                            if (valueAt(index) === panel.project.branch)
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
                        if (selected.length > 0 && selected !== panel.project.branch)
                            panel.backend.checkoutBranch(panel.project.id, selected);
                    }
                }

                RowLayout {
                    Layout.fillWidth: true

                    Controls.TextField {
                        id: newBranch

                        Layout.fillWidth: true
                        enabled: panel.job("create_branch") === null
                        placeholderText: qsTr("New branch name")
                    }

                    Controls.TextField {
                        id: branchStart

                        Layout.fillWidth: true
                        enabled: panel.job("create_branch") === null
                        placeholderText: qsTr("Start point")
                        text: "HEAD"
                    }
                }

                Controls.Button {
                    Layout.fillWidth: true
                    enabled: panel.job("create_branch") === null && newBranch.text.trim().length > 0
                    icon.name: "list-add"
                    text: qsTr("Create and switch")
                    onClicked: panel.backend.createBranch(
                        panel.project.id,
                        newBranch.text,
                        branchStart.text
                    )
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                Layout.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    level: 4
                    text: qsTr("Worktrees")
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    text: qsTr("This workspace comes from %1.").arg(panel.project.parentName)
                    visible: panel.project.worktree
                }

                RowLayout {
                    Layout.fillWidth: true
                    visible: !panel.project.worktree

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("create_worktree") === null
                        icon.name: "vcs-branch"
                        text: qsTr("New Worktree…")
                        onClicked: worktreeForm.visible = !worktreeForm.visible
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: panel.job("reconcile_worktrees") === null
                        icon.name: "view-refresh"
                        text: qsTr("Reconcile")
                        onClicked: panel.backend.reconcileWorktrees(panel.project.id)
                    }
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
                        enabled: panel.job("create_worktree") === null
                            && (worktreeMode.mode === "detached"
                                ? worktreeStart.text.trim().length > 0
                                : worktreeBranch.text.trim().length > 0)
                        icon.name: "list-add"
                        text: qsTr("Review and create…")
                        onClicked: createWorktreeDialog.open()
                    }
                }

                Repeater {
                    model: panel.project.worktree ? [] : panel.backend.worktrees

                    delegate: Controls.Label {
                        required property var modelData

                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        elide: Text.ElideMiddle
                        font: Kirigami.Theme.smallFont
                        text: qsTr("%1 — %2 (%3)")
                            .arg(modelData.branch.length > 0 ? modelData.branch : qsTr("detached HEAD"))
                            .arg(modelData.root)
                            .arg(modelData.owned ? qsTr("Harkness") : qsTr("external"))
                    }
                }
            }
        }
    }

    Kirigami.PromptDialog {
        id: pushOverrideDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: qsTr("This publishes %1 directly to the remote's default branch. Confirm only if that protected action is intended.")
            .arg(panel.gitState.branch || panel.project.branch)
        title: qsTr("Push to the default branch?")
        onAccepted: panel.backend.push(panel.project.id, true)
    }

    Kirigami.PromptDialog {
        id: createWorktreeDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: panel.project.managed
            ? qsTr("Harkness will add a linked worktree from the repository at:\n%1\n\nThe current checkout is untouched.").arg(panel.project.root)
            : qsTr("Harkness will add a linked worktree for the local project at:\n%1\n\nYour existing working tree and files are untouched.").arg(panel.project.root)
        title: qsTr("Create linked worktree?")
        onAccepted: panel.backend.createWorktree(
            panel.project.id,
            worktreeMode.mode,
            worktreeBranch.text,
            worktreeStart.text
        )
    }
}
