import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Item {
    id: panel

    required property var backend
    required property var project

    /// Shared job and Git-state projection; the header toolbar builds the same
    /// one, so both surfaces enable and disable the same actions together.
    readonly property bool stateReady: gitActivity.stateReady
    readonly property var gitState: gitActivity.gitState
    readonly property var entries: gitActivity.entries
    property string selectedProjectId: ""

    implicitWidth: Kirigami.Units.gridUnit * 56

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

    GitActivity {
        id: gitActivity

        backend: panel.backend
        project: panel.project
    }

    Kirigami.Action {
        id: refreshAction

        enabled: gitActivity.job("status") === null
            && !gitActivity.repositoryMutationRunning()
        icon.name: "view-refresh"
        text: qsTr("Refresh Git status")
        tooltip: qsTr("Refresh Git status")
        onTriggered: panel.backend.refreshGit(panel.project.id)
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

    function handleProjectChange() {
        const nextId = project && project.id !== undefined ? String(project.id) : "";
        if (selectedProjectId !== nextId) {
            selectedProjectId = nextId;
            backend.clearReview();
        }
        refresh();
    }

    /// The message the backend commits: GitHub Desktop's split summary and
    /// description, joined the way Git stores them.
    function composedCommitMessage() {
        const summary = commitSummary.text.trim();
        const description = commitDescription.text.trim();
        return description.length > 0 ? summary + "\n\n" + description : summary;
    }

    function commitAllowed() {
        return gitActivity.job("commit") === null
            && gitActivity.job("push") === null
            && !gitActivity.repositoryOperationRunning()
            && commitSummary.text.trim().length > 0;
    }

    // Clears the commit message once the in-flight commit finishes without
    // error, rather than leaving a stale message the user has to delete
    // themselves before writing the next one.
    property bool commitJobRunning: gitActivity.job("commit") !== null
    onCommitJobRunningChanged: {
        if (!commitJobRunning
                && gitActivity.stateReady
                && String(gitActivity.gitState.error || "").length === 0) {
            commitSummary.text = "";
            commitDescription.text = "";
        }
    }

    // Surfaces the default-branch override as a dialog the moment a push is
    // refused, rather than only as a button in the Changes tab that a user who
    // pushed from the header toolbar would have to go looking for.
    property bool pushJobRunning: gitActivity.job("push") !== null
    onPushJobRunningChanged: {
        if (!pushJobRunning
                && gitActivity.stateReady
                && ["default_branch_push", "default_branch_unknown"]
                    .indexOf(gitActivity.gitState.errorKind) !== -1)
            pushOverrideDialog.open();
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

                Controls.TabBar {
                    id: sectionTabs

                    Layout.fillWidth: true

                    Controls.TabButton {
                        text: panel.entries.length > 0
                            ? qsTr("Changes (%1)").arg(panel.entries.length)
                            : qsTr("Changes")
                    }

                    Controls.TabButton {
                        text: qsTr("History")
                    }
                }

                // The file list and the commit box divide the column's height
                // between them, so a long message and a long list of changes
                // can each be given the room the other does not need.
                Controls.SplitView {
                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    orientation: Qt.Vertical

                    handle: Rectangle {
                        readonly property bool active: Controls.SplitHandle.hovered
                            || Controls.SplitHandle.pressed

                        color: active ? Kirigami.Theme.highlightColor : "transparent"
                        implicitHeight: Kirigami.Units.smallSpacing

                        Kirigami.Separator {
                            anchors.verticalCenter: parent.verticalCenter
                            visible: !parent.active
                            width: parent.width
                        }
                    }

                    StackLayout {
                        Controls.SplitView.fillHeight: true
                        Controls.SplitView.minimumHeight: Kirigami.Units.gridUnit * 6
                        currentIndex: sectionTabs.currentIndex

                        ChangesPanel {
                            activity: gitActivity
                            backend: panel.backend
                            project: panel.project
                            onPushOverrideRequested: pushOverrideDialog.open()
                        }

                        HistoryPanel {
                            activity: gitActivity
                            backend: panel.backend
                            project: panel.project
                        }
                    }

                    // The commit box stays pinned under the file list, the way
                    // GitHub Desktop anchors it: what is being committed is the
                    // list directly above, so the box must not scroll away from it.
                    Item {
                        id: commitFooter

                        Controls.SplitView.fillHeight: false
                        Controls.SplitView.minimumHeight: Kirigami.Units.gridUnit * 8
                        Controls.SplitView.preferredHeight: Kirigami.Units.gridUnit * 11
                        visible: sectionTabs.currentIndex === 0

                        ColumnLayout {
                            anchors.fill: parent
                            anchors.bottomMargin: Kirigami.Units.largeSpacing
                            anchors.leftMargin: Kirigami.Units.largeSpacing
                            anchors.rightMargin: Kirigami.Units.largeSpacing
                            anchors.topMargin: Kirigami.Units.smallSpacing
                            spacing: Kirigami.Units.smallSpacing

                            Controls.TextField {
                                id: commitSummary

                                Layout.fillWidth: true
                                enabled: gitActivity.job("commit") === null
                                    && gitActivity.job("push") === null
                                    && !gitActivity.repositoryOperationRunning()
                                placeholderText: qsTr("Summary (required)")
                            }

                            Controls.TextArea {
                                id: commitDescription

                                Layout.fillHeight: true
                                Layout.fillWidth: true
                                Layout.minimumHeight: Kirigami.Units.gridUnit * 3
                                enabled: commitSummary.enabled
                                placeholderText: qsTr("Description")
                                wrapMode: TextEdit.Wrap
                            }

                            RowLayout {
                                Layout.fillWidth: true

                                Controls.Button {
                                    enabled: panel.commitAllowed()
                                    text: qsTr("Amend")
                                    onClicked: panel.backend.commit(
                                        panel.project.id,
                                        panel.composedCommitMessage(),
                                        true
                                    )
                                }

                                Controls.Button {
                                    Layout.fillWidth: true
                                    enabled: panel.commitAllowed()
                                    highlighted: true
                                    text: gitActivity.currentBranch.length > 0
                                        ? qsTr("Commit to %1").arg(gitActivity.currentBranch)
                                        : qsTr("Commit to detached HEAD")
                                    onClicked: panel.backend.commit(
                                        panel.project.id,
                                        panel.composedCommitMessage(),
                                        false
                                    )
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

            // No scroll view around the surface: the diff inside it is the only
            // part worth scrolling, and wrapping the whole thing is what forced
            // it into a fixed-height box.
            ReviewSurface {
                anchors.fill: parent
                anchors.margins: Kirigami.Units.largeSpacing
                backend: panel.backend
                gitState: panel.gitState
                project: panel.project
                stateReady: panel.stateReady
            }
        }
    }

    Kirigami.PromptDialog {
        id: pushOverrideDialog

        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        subtitle: qsTr("This publishes %1 directly to the remote's default branch. Confirm only if that protected action is intended.")
            .arg(gitActivity.currentBranch)
        title: qsTr("Push to the default branch?")
        onAccepted: panel.backend.push(panel.project.id, true)
    }
}
