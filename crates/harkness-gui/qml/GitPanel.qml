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
    property string selectedProjectId: ""
    property string movingWorktreeId: ""
    property string movingWorktreeName: ""
    property string movingWorktreeRoot: ""
    property string lockingWorktreeId: ""
    property string lockingWorktreeName: ""

    implicitWidth: Kirigami.Units.gridUnit * 56
    readonly property string repositoryLockScope: String(
        project.lockScope || project.parentId || project.id
    )

    /// Emitted when the user dismisses this view from its header. The whole
    /// surface goes, review side included: source control and the diff it
    /// drives are one view, not a panel with a detached companion.
    signal hideRequested()

    // Side-panel view contract; see SidePanel.qml.
    readonly property string viewId: "git"
    readonly property string viewTitle: qsTr("Source Control")
    readonly property string viewIcon: "vcs-branch"
    readonly property string viewShortcut: "Ctrl+Shift+G"
    readonly property int viewBadge: entries.length
    readonly property bool viewAvailable: project.available && project.isGit

    Kirigami.Action {
        id: refreshAction

        enabled: panel.job("status") === null && !panel.repositoryMutationRunning()
        icon.name: "view-refresh"
        text: qsTr("Refresh Git status")
        tooltip: qsTr("Refresh Git status")
        onTriggered: panel.backend.refreshGit(panel.project.id)
    }

    function job(kind, targetProjectId) {
        const target = targetProjectId === undefined ? project.id : targetProjectId;
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            const matchesTarget = targetProjectId === undefined
                ? String(candidate.projectId) === String(target)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope
                : String(candidate.projectId) === String(target);
            if (matchesTarget && candidate.kind === kind)
                return candidate;
        }
        return null;
    }

    function networkJobs() {
        const running = [];
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if ((String(candidate.projectId) === String(project.id)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope)
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
        backend.refreshHistory(project.id);
        if (!project.worktree)
            backend.refreshWorktrees(project.id);
    }

    // `ComboBox.valueAt()` does not reliably resolve roles from the QVariant
    // maps produced by the Rust backend while its model is being populated.
    // Read the map directly, as ReviewSurface does, so the picker always
    // reflects the branch Git reports as checked out rather than its first
    // alphabetical entry.
    function currentBranchIndex() {
        const currentBranch = String(gitState.branch || project.branch || "");
        for (let index = 0; index < backend.branches.length; ++index) {
            const branch = backend.branches[index];
            if (String(branch.name || "") === currentBranch)
                return index;
        }
        return -1;
    }

    function handleProjectChange() {
        const nextId = project && project.id !== undefined ? String(project.id) : "";
        if (selectedProjectId !== nextId) {
            selectedProjectId = nextId;
            backend.clearReview();
        }
        refresh();
    }

    function selectPath(pathId, staged, unstaged) {
        const selectedPathId = String(pathId);
        // Prefer the working-tree side when both exist. The staged side stays
        // one click away in the same review surface.
        backend.reviewWorkingChanges(
            project.id,
            String(unstaged || "").length === 0
                && String(staged || "").length > 0,
            selectedPathId
        );
    }

    function repositoryMutationRunning() {
        return job("stage") !== null
            || job("unstage") !== null
            || job("stage_hunk") !== null
            || job("unstage_hunk") !== null
            || job("commit") !== null
            || job("fetch") !== null
            || job("pull") !== null
            || job("push") !== null
            || job("checkout") !== null
            || job("create_branch") !== null
            || job("create_worktree") !== null
            || job("reconcile_worktrees") !== null
            || job("move_worktree") !== null
            || job("lock_worktree") !== null
            || job("unlock_worktree") !== null
            || job("remove_worktree") !== null
            || job("remove_managed") !== null;
    }

    // Mirrors the ahead/behind summary in the header: pulling takes
    // priority when behind (a push would be rejected anyway), otherwise
    // push if there's anything to push, otherwise just fetch to refresh
    // the remote-tracking state.
    function syncAction() {
        const behind = stateReady ? Number(gitState.behind || 0) : 0;
        const ahead = stateReady ? Number(gitState.ahead || 0) : 0;
        if (behind > 0)
            return "pull";
        if (ahead > 0)
            return "push";
        return "fetch";
    }

    // Clears the commit message once the in-flight commit finishes without
    // error, rather than leaving a stale message the user has to delete
    // themselves before writing the next one.
    property bool commitJobRunning: job("commit") !== null
    onCommitJobRunningChanged: {
        if (!commitJobRunning && stateReady && String(gitState.error || "").length === 0)
            commitMessage.text = "";
    }

    // Surfaces the default-branch override as a dialog the moment a push is
    // refused, rather than only as a button above the "Changes" section that
    // a user who clicked Push down in "Remote" would have to scroll up to
    // notice.
    property bool pushJobRunning: job("push") !== null
    onPushJobRunningChanged: {
        if (!pushJobRunning
                && stateReady
                && ["default_branch_push", "default_branch_unknown"].indexOf(gitState.errorKind) !== -1)
            pushOverrideDialog.open();
    }

    function reviewReadRunning() {
        return job("review") !== null
            || job("review_file") !== null
            || job("review_context") !== null;
    }

    function historyReadRunning() {
        return job("history") !== null;
    }

    function repositoryOperationRunning() {
        return repositoryMutationRunning()
            || reviewReadRunning()
            || historyReadRunning()
            || job("status") !== null
            || job("branches") !== null
            || job("worktrees") !== null;
    }

    // InlineMessage does not expose its internal label's textFormat. Wrap an
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

    onProjectChanged: handleProjectChange()
    Component.onCompleted: handleProjectChange()

    // The view is the source-control column plus the review surface it drives.
    // They are split here rather than in the shell so that collapsing the view
    // takes both away together.
    Controls.SplitView {
        anchors.fill: parent
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

        Item {
            objectName: "sourceControlColumn"

            Controls.SplitView.fillWidth: false
            Controls.SplitView.maximumWidth: Kirigami.Units.gridUnit * 40
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 18
            Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 23

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.alternateBackgroundColor
            }

            ColumnLayout {
                anchors.fill: parent
                spacing: 0

                PanelHeader {
                    Layout.fillWidth: true
                    actions: [refreshAction]
                    title: panel.viewTitle
                    onHideRequested: panel.hideRequested()
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                }

                Controls.ScrollView {
                    id: scroll

                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    clip: true
                    contentWidth: availableWidth

                    ColumnLayout {
                        spacing: 0
                        width: scroll.availableWidth

                        ColumnLayout {
                            Layout.fillWidth: true
                            Layout.bottomMargin: Kirigami.Units.largeSpacing
                            Layout.leftMargin: Kirigami.Units.largeSpacing
                            Layout.rightMargin: Kirigami.Units.largeSpacing
                            Layout.topMargin: Kirigami.Units.largeSpacing
                            spacing: Kirigami.Units.smallSpacing

                            Controls.Label {
                                Layout.fillWidth: true
                                elide: Text.ElideRight
                                font.bold: true
                                text: panel.stateReady ? panel.gitState.head : qsTr("Loading repository status…")
                                textFormat: Text.PlainText
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
                                textFormat: Text.PlainText
                            }

                            Kirigami.InlineMessage {
                                Layout.fillWidth: true
                                text: panel.escapedRichText(
                                    qsTr("A %1 is waiting to be resolved or aborted.")
                                        .arg(panel.gitState.pending || "")
                                )
                                type: Kirigami.MessageType.Warning
                                visible: panel.stateReady && panel.gitState.pending && panel.gitState.pending.length > 0
                            }

                            Kirigami.InlineMessage {
                                Layout.fillWidth: true
                                text: panel.escapedRichText(panel.gitState.error || "")
                                type: Kirigami.MessageType.Error
                                visible: panel.stateReady && panel.gitState.error && panel.gitState.error.length > 0
                            }

                            Controls.Button {
                                Layout.alignment: Qt.AlignRight
                                enabled: !panel.repositoryOperationRunning()
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
                                                textFormat: Text.PlainText
                                            }

                                            Controls.ToolButton {
                                                Controls.ToolTip.text: qsTr("View staged and unstaged diff")
                                                Controls.ToolTip.visible: hovered
                                                display: Controls.AbstractButton.TextOnly
                                                enabled: !panel.repositoryMutationRunning()
                                                    && !panel.reviewReadRunning()
                                                text: qsTr("View diff")
                                                onClicked: panel.selectPath(
                                                    modelData.pathId,
                                                    modelData.staged,
                                                    modelData.unstaged
                                                )
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
                                                textFormat: Text.PlainText
                                            }

                                            Controls.Button {
                                                enabled: !panel.repositoryOperationRunning()
                                                text: qsTr("Unstage")
                                                visible: modelData.staged.length > 0
                                                onClicked: panel.backend.unstagePath(panel.project.id, modelData.pathId)
                                            }

                                            Controls.Button {
                                                enabled: !panel.repositoryOperationRunning()
                                                text: qsTr("Stage")
                                                visible: modelData.unstaged.length > 0
                                                onClicked: panel.backend.stagePath(panel.project.id, modelData.pathId)
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
                                    && panel.job("push") === null
                                    && !panel.repositoryOperationRunning()
                                placeholderText: qsTr("Commit message")
                            }

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: panel.job("commit") === null
                                        && panel.job("push") === null
                                        && !panel.repositoryOperationRunning()
                                        && commitMessage.text.trim().length > 0
                                    text: qsTr("Amend")
                                    onClicked: panel.backend.commit(panel.project.id, commitMessage.text, true)
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: panel.job("commit") === null
                                        && panel.job("push") === null
                                        && !panel.repositoryOperationRunning()
                                        && commitMessage.text.trim().length > 0
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

                            Controls.Button {
                                Layout.fillWidth: true
                                enabled: !panel.repositoryOperationRunning()
                                icon.name: {
                                    switch (panel.syncAction()) {
                                    case "pull": return "go-down";
                                    case "push": return "go-up";
                                    default: return "download";
                                    }
                                }
                                text: {
                                    switch (panel.syncAction()) {
                                    case "pull": return qsTr("Pull (%1)").arg(panel.gitState.behind || 0);
                                    case "push": return qsTr("Push (%1)").arg(panel.gitState.ahead || 0);
                                    default: return qsTr("Fetch");
                                    }
                                }
                                onClicked: {
                                    switch (panel.syncAction()) {
                                    case "pull":
                                        panel.backend.pull(panel.project.id);
                                        break;
                                    case "push":
                                        panel.backend.push(panel.project.id, false);
                                        break;
                                    default:
                                        panel.backend.fetch(panel.project.id);
                                        break;
                                    }
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
                                        textFormat: Text.PlainText
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
                                    && panel.job("push") === null
                                    && !panel.repositoryOperationRunning()
                                model: panel.backend.branches
                                textRole: "name"
                                valueRole: "name"

                                currentIndex: panel.currentBranchIndex()

                                delegate: Controls.ItemDelegate {
                                    required property var modelData

                                    Controls.ToolTip.text: panel.escapedRichText(modelData.detail)
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
                                        && panel.job("push") === null
                                        && !panel.repositoryOperationRunning()
                                    placeholderText: qsTr("New branch name")
                                }

                                Controls.TextField {
                                    id: branchStart

                                    Layout.fillWidth: true
                                    enabled: panel.job("create_branch") === null
                                        && panel.job("push") === null
                                        && !panel.repositoryOperationRunning()
                                    placeholderText: qsTr("Start point")
                                    text: "HEAD"
                                }
                            }

                            Controls.Button {
                                Layout.fillWidth: true
                                enabled: panel.job("create_branch") === null
                                    && panel.job("push") === null
                                    && !panel.repositoryOperationRunning()
                                    && newBranch.text.trim().length > 0
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
                                textFormat: Text.PlainText
                                visible: panel.project.worktree
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                visible: !panel.project.worktree

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !panel.repositoryOperationRunning()
                                    icon.name: "vcs-branch"
                                    text: qsTr("New Worktree…")
                                    onClicked: worktreeForm.visible = !worktreeForm.visible
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !panel.repositoryOperationRunning()
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
                                    enabled: !panel.repositoryOperationRunning()
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
                                                textFormat: Text.PlainText
                                            }

                                            Kirigami.Icon {
                                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                                source: "object-locked"
                                                visible: worktreeRow.row.locked
                                            }
                                        }

                                        // A lock is lifecycle policy owned by the catalog. Show
                                        // Git's reason in the row so protection is visible
                                        // before a move or removal is attempted.
                                        Controls.Label {
                                            Layout.fillWidth: true
                                            color: Kirigami.Theme.neutralTextColor
                                            text: worktreeRow.row.lockReason.length > 0
                                                ? qsTr("Locked: %1").arg(worktreeRow.row.lockReason)
                                                : qsTr("Locked without a recorded reason")
                                            textFormat: Text.PlainText
                                            visible: worktreeRow.row.locked
                                            wrapMode: Text.Wrap
                                        }

                                        RowLayout {
                                            Layout.fillWidth: true
                                            visible: worktreeRow.row.owned

                                            Controls.Button {
                                                Layout.fillWidth: true
                                                enabled: !panel.repositoryOperationRunning()
                                                    && panel.job(
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
                                                enabled: !panel.repositoryOperationRunning()
                                                    && !worktreeRow.row.locked
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
                                    textFormat: Text.PlainText
                                }

                                Controls.TextField {
                                    id: lockReason

                                    Layout.fillWidth: true
                                    placeholderText: qsTr("Required reason for protecting this worktree")
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: !panel.repositoryOperationRunning()
                                        && lockReason.text.trim().length > 0
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
                                    enabled: !panel.repositoryOperationRunning()
                                        && moveDestination.text.trim().length > 0
                                        && panel.job("move_worktree", panel.movingWorktreeId) === null
                                    icon.name: "folder-move"
                                    text: qsTr("Review and move…")
                                    onClicked: moveWorktreeDialog.open()
                                }
                            }
                        }
                    }
                }
            }
        }

        Item {
            objectName: "reviewSidePanel"

            Controls.SplitView.fillWidth: true
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 26

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.backgroundColor
            }

            Controls.ScrollView {
                id: reviewScroll

                anchors.fill: parent
                clip: true
                contentWidth: availableWidth

                ReviewSurface {
                    width: reviewScroll.availableWidth
                        - Kirigami.Units.largeSpacing * 2
                    x: Kirigami.Units.largeSpacing
                    backend: panel.backend
                    gitState: panel.gitState
                    project: panel.project
                    stateReady: panel.stateReady
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
