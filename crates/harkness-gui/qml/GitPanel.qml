import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Dialogs
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
    readonly property bool diffReady: backend.diff !== undefined
        && backend.diff
        && backend.diff.projectId !== undefined
        && String(backend.diff.projectId) === String(project.id)
        && String(backend.diff.path) === selectedPath
    readonly property var diffState: diffReady ? backend.diff : ({})
    readonly property var diffFiles: diffReady && diffState.files !== undefined
        ? diffState.files
        : []
    property string diffProjectId: ""
    property string selectedPath: ""
    property string movingWorktreeId: ""
    property string movingWorktreeName: ""
    property string movingWorktreeRoot: ""
    property string lockingWorktreeId: ""
    property string lockingWorktreeName: ""

    implicitWidth: Kirigami.Units.gridUnit * 22

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.alternateBackgroundColor
    }

    function job(kind, targetProjectId) {
        const target = targetProjectId === undefined ? project.id : targetProjectId;
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if (String(candidate.projectId) === String(target) && candidate.kind === kind)
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

    function handleProjectChange() {
        const nextId = project && project.id !== undefined ? String(project.id) : "";
        if (diffProjectId !== nextId) {
            diffProjectId = nextId;
            selectedPath = "";
            backend.clearDiff();
        }
        refresh();
    }

    function selectPath(path) {
        selectedPath = String(path);
        backend.refreshDiff(project.id, selectedPath);
    }

    function diffTint(color) {
        return Qt.rgba(color.r, color.g, color.b, 0.14);
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

    onProjectChanged: handleProjectChange()
    Component.onCompleted: handleProjectChange()

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

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Label {
                                    Layout.fillWidth: true
                                    elide: Text.ElideMiddle
                                    font.family: "monospace"
                                    text: modelData.path
                                }

                                Controls.ToolButton {
                                    Controls.ToolTip.text: checked
                                        ? qsTr("Refresh selected diff")
                                        : qsTr("View staged and unstaged diff")
                                    Controls.ToolTip.visible: hovered
                                    checkable: true
                                    checked: panel.selectedPath === String(modelData.path)
                                    display: Controls.AbstractButton.TextOnly
                                    text: checked ? qsTr("Refresh diff") : qsTr("View diff")
                                    onClicked: panel.selectPath(modelData.path)
                                }
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

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing
                    visible: panel.selectedPath.length > 0

                    RowLayout {
                        Layout.fillWidth: true

                        Kirigami.Heading {
                            Layout.fillWidth: true
                            elide: Text.ElideMiddle
                            level: 5
                            text: qsTr("Diff · %1").arg(panel.selectedPath)
                        }

                        Controls.BusyIndicator {
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            running: panel.diffReady && panel.diffState.loading === true
                            visible: running
                        }

                        Controls.ToolButton {
                            Controls.ToolTip.text: qsTr("Refresh selected diff")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: !(panel.diffReady && panel.diffState.loading === true)
                            icon.name: "view-refresh"
                            text: qsTr("Refresh diff")
                            onClicked: panel.selectPath(panel.selectedPath)
                        }
                    }

                    Kirigami.InlineMessage {
                        Layout.fillWidth: true
                        text: panel.diffReady ? panel.diffState.error || "" : ""
                        type: Kirigami.MessageType.Error
                        visible: panel.diffReady
                            && panel.diffState.error
                            && panel.diffState.error.length > 0
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        text: qsTr("No staged or unstaged content remains for this path.")
                        visible: panel.diffReady
                            && panel.diffState.loading !== true
                            && (!panel.diffState.error || panel.diffState.error.length === 0)
                            && panel.diffFiles.length === 0
                        wrapMode: Text.Wrap
                    }

                    Repeater {
                        model: panel.diffFiles

                        delegate: Controls.Frame {
                            id: fileDiffFrame

                            required property var modelData
                            readonly property var file: modelData

                            Layout.fillWidth: true
                            padding: Kirigami.Units.smallSpacing

                            contentItem: ColumnLayout {
                                spacing: Kirigami.Units.smallSpacing

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: fileDiffFrame.file.target === "staged"
                                        ? Kirigami.Theme.positiveTextColor
                                        : Kirigami.Theme.textColor
                                    font.bold: true
                                    text: fileDiffFrame.file.target === "staged"
                                        ? qsTr("Staged · %1").arg(fileDiffFrame.file.change)
                                        : qsTr("Working tree · %1").arg(fileDiffFrame.file.change)
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: Kirigami.Theme.disabledTextColor
                                    elide: Text.ElideMiddle
                                    font.family: "monospace"
                                    text: fileDiffFrame.file.path
                                }

                                Kirigami.InlineMessage {
                                    Layout.fillWidth: true
                                    text: fileDiffFrame.file.summary
                                    type: Kirigami.MessageType.Information
                                    visible: fileDiffFrame.file.summary.length > 0
                                }

                                Repeater {
                                    model: fileDiffFrame.file.hunks

                                    delegate: Controls.Frame {
                                        id: hunkFrame

                                        required property var modelData
                                        readonly property var hunk: modelData

                                        Layout.fillWidth: true
                                        padding: Kirigami.Units.smallSpacing

                                        contentItem: ColumnLayout {
                                            spacing: 0

                                            RowLayout {
                                                Layout.fillWidth: true
                                                Layout.bottomMargin: Kirigami.Units.smallSpacing

                                                Controls.Label {
                                                    Layout.fillWidth: true
                                                    color: Kirigami.Theme.highlightColor
                                                    font.family: "monospace"
                                                    text: hunkFrame.hunk.header
                                                    wrapMode: Text.WrapAnywhere
                                                }

                                                Controls.Button {
                                                    enabled: panel.job(
                                                        fileDiffFrame.file.target === "staged"
                                                            ? "unstage_hunk"
                                                            : "stage_hunk"
                                                    ) === null
                                                    text: fileDiffFrame.file.target === "staged"
                                                        ? qsTr("Unstage hunk")
                                                        : qsTr("Stage hunk")
                                                    onClicked: {
                                                        if (fileDiffFrame.file.target === "staged") {
                                                            panel.backend.unstageHunk(
                                                                panel.project.id,
                                                                hunkFrame.hunk.selectionId
                                                            );
                                                        } else {
                                                            panel.backend.stageHunk(
                                                                panel.project.id,
                                                                hunkFrame.hunk.selectionId
                                                            );
                                                        }
                                                    }
                                                }
                                            }

                                            Repeater {
                                                model: hunkFrame.hunk.lines

                                                delegate: Rectangle {
                                                    id: diffLine

                                                    required property var modelData
                                                    readonly property var line: modelData

                                                    Layout.fillWidth: true
                                                    color: line.kind === "addition"
                                                        ? panel.diffTint(Kirigami.Theme.positiveTextColor)
                                                        : line.kind === "deletion"
                                                            ? panel.diffTint(Kirigami.Theme.negativeTextColor)
                                                            : "transparent"
                                                    implicitHeight: diffLineLayout.implicitHeight
                                                        + Kirigami.Units.smallSpacing

                                                    RowLayout {
                                                        id: diffLineLayout

                                                        anchors.fill: parent
                                                        anchors.leftMargin: Kirigami.Units.smallSpacing
                                                        anchors.rightMargin: Kirigami.Units.smallSpacing
                                                        spacing: Kirigami.Units.smallSpacing

                                                        Controls.Label {
                                                            Layout.preferredWidth: Kirigami.Units.gridUnit * 3
                                                            color: Kirigami.Theme.disabledTextColor
                                                            font.family: "monospace"
                                                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                                            horizontalAlignment: Text.AlignRight
                                                            text: "%1│%2"
                                                                .arg(diffLine.line.oldLine > 0
                                                                    ? diffLine.line.oldLine
                                                                    : "")
                                                                .arg(diffLine.line.newLine > 0
                                                                    ? diffLine.line.newLine
                                                                    : "")
                                                        }

                                                        Controls.Label {
                                                            Layout.preferredWidth: Kirigami.Units.gridUnit
                                                            color: diffLine.line.kind === "addition"
                                                                ? Kirigami.Theme.positiveTextColor
                                                                : diffLine.line.kind === "deletion"
                                                                    ? Kirigami.Theme.negativeTextColor
                                                                    : Kirigami.Theme.disabledTextColor
                                                            font.bold: true
                                                            font.family: "monospace"
                                                            horizontalAlignment: Text.AlignHCenter
                                                            text: diffLine.line.marker
                                                        }

                                                        Controls.Label {
                                                            Layout.fillWidth: true
                                                            font.family: "monospace"
                                                            text: diffLine.line.content
                                                            wrapMode: Text.WrapAnywhere
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
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

                    delegate: Controls.Frame {
                        id: worktreeRow

                        required property var modelData
                        readonly property var row: modelData

                        Layout.fillWidth: true
                        padding: Kirigami.Units.smallSpacing

                        contentItem: ColumnLayout {
                            spacing: Kirigami.Units.smallSpacing

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: Kirigami.Theme.disabledTextColor
                                    elide: Text.ElideMiddle
                                    font: Kirigami.Theme.smallFont
                                    text: qsTr("%1 — %2 (%3)")
                                        .arg(worktreeRow.row.branch.length > 0
                                            ? worktreeRow.row.branch
                                            : qsTr("detached HEAD"))
                                        .arg(worktreeRow.row.root)
                                        .arg(worktreeRow.row.owned ? qsTr("Harkness") : qsTr("external"))
                                }

                                Kirigami.Icon {
                                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                    source: "object-locked"
                                    visible: worktreeRow.row.locked
                                }
                            }

                            // A lock is lifecycle policy owned by core. Show
                            // Git's reason in the row so protection is visible
                            // before a move or removal is attempted.
                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.neutralTextColor
                                text: worktreeRow.row.lockReason.length > 0
                                    ? qsTr("Locked: %1").arg(worktreeRow.row.lockReason)
                                    : qsTr("Locked without a recorded reason")
                                visible: worktreeRow.row.locked
                                wrapMode: Text.Wrap
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                visible: worktreeRow.row.owned

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: panel.job(
                                        worktreeRow.row.locked
                                            ? "unlock_worktree"
                                            : "lock_worktree",
                                        worktreeRow.row.id
                                    ) === null
                                    icon.name: worktreeRow.row.locked
                                        ? "object-unlocked"
                                        : "object-locked"
                                    text: worktreeRow.row.locked ? qsTr("Unlock") : qsTr("Lock…")
                                    onClicked: {
                                        if (worktreeRow.row.locked) {
                                            panel.backend.unlockWorktree(worktreeRow.row.id);
                                        } else {
                                            panel.lockingWorktreeId = String(worktreeRow.row.id);
                                            panel.lockingWorktreeName = worktreeRow.row.branch.length > 0
                                                ? String(worktreeRow.row.branch)
                                                : qsTr("detached HEAD");
                                            lockReason.text = "";
                                            lockWorktreeForm.visible = true;
                                            moveWorktreeForm.visible = false;
                                            lockReason.forceActiveFocus();
                                        }
                                    }
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !worktreeRow.row.locked
                                        && panel.job("move_worktree", worktreeRow.row.id) === null
                                    icon.name: "folder-move"
                                    text: worktreeRow.row.locked
                                        ? qsTr("Unlock before moving")
                                        : qsTr("Move…")
                                    onClicked: {
                                        panel.movingWorktreeId = String(worktreeRow.row.id);
                                        panel.movingWorktreeName = worktreeRow.row.branch.length > 0
                                            ? String(worktreeRow.row.branch)
                                            : qsTr("detached HEAD");
                                        panel.movingWorktreeRoot = String(worktreeRow.row.root);
                                        moveDestination.text = "";
                                        moveWorktreeForm.visible = true;
                                        lockWorktreeForm.visible = false;
                                        moveDestination.forceActiveFocus();
                                    }
                                }
                            }
                        }
                    }
                }

                ColumnLayout {
                    id: lockWorktreeForm

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: false

                    Controls.Label {
                        Layout.fillWidth: true
                        text: qsTr("Lock %1").arg(panel.lockingWorktreeName)
                    }

                    Controls.TextField {
                        id: lockReason

                        Layout.fillWidth: true
                        placeholderText: qsTr("Required reason for protecting this worktree")
                    }

                    Controls.Button {
                        Layout.fillWidth: true
                        enabled: lockReason.text.trim().length > 0
                            && panel.job("lock_worktree", panel.lockingWorktreeId) === null
                        icon.name: "object-locked"
                        text: qsTr("Lock worktree")
                        onClicked: {
                            panel.backend.lockWorktree(panel.lockingWorktreeId, lockReason.text);
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
                        text: qsTr("Move %1").arg(panel.movingWorktreeName)
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
                        enabled: moveDestination.text.trim().length > 0
                            && panel.job("move_worktree", panel.movingWorktreeId) === null
                        icon.name: "folder-move"
                        text: qsTr("Review and move…")
                        onClicked: moveWorktreeDialog.open()
                    }
                }
            }
        }
    }

    FolderDialog {
        id: moveParentDialog

        title: qsTr("Choose the new parent folder")
        onAccepted: {
            const parent = panel.urlToPath(selectedFolder);
            const leaf = panel.pathBaseName(panel.movingWorktreeRoot);
            moveDestination.text = leaf.length > 0 ? panel.joinPath(parent, leaf) : parent;
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

    Kirigami.PromptDialog {
        id: moveWorktreeDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: qsTr("Git will relocate %1 to:\n%2\n\nThe destination must be an absolute path that does not already exist.")
            .arg(panel.movingWorktreeName)
            .arg(moveDestination.text.trim())
        title: qsTr("Move linked worktree?")
        onAccepted: {
            panel.backend.moveWorktree(panel.movingWorktreeId, moveDestination.text.trim());
            moveWorktreeForm.visible = false;
        }
    }
}
