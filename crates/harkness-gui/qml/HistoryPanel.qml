import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// The "History" tab of the source-control view: the commits behind HEAD, and
/// the branch comparison that is not a commit but answers the same question —
/// what changed, and where do I read it.
///
/// Picking anything here loads the diff into the review surface beside the
/// column, so the list stays a list and never grows a diff of its own.
ColumnLayout {
    id: history

    required property var backend
    required property var project
    /// Shared job and Git-state projection; see GitActivity.qml.
    required property var activity

    readonly property bool historyReady: backend.history !== undefined
        && backend.history
        && backend.history.projectId !== undefined
        && String(backend.history.projectId) === String(project.id)
    readonly property var historyState: historyReady ? backend.history : ({})
    readonly property var commits: historyReady && historyState.commits !== undefined
        ? historyState.commits
        : []
    readonly property bool reviewReady: backend.review !== undefined
        && backend.review
        && backend.review.projectId !== undefined
        && String(backend.review.projectId) === String(project.id)
    readonly property string reviewCommitId: reviewReady
        ? String(backend.review.commitId || "")
        : ""

    /// Branch the current one is compared against. Kept here rather than in the
    /// review surface because the comparison is chosen next to the commits it
    /// sits among, and the surface only ever displays the result.
    property string baseBranch: ""

    spacing: Kirigami.Units.smallSpacing

    /// Picks the branch a comparison most likely means: the project's trunk if
    /// it has one, otherwise any branch that is not the one checked out.
    function chooseBaseBranch() {
        if (!backend || backend.branches === undefined)
            return;
        if (baseBranch.length > 0 && baseBranch !== activity.currentBranch)
            return;
        let fallback = "";
        for (let index = 0; index < backend.branches.length; ++index) {
            const branch = String(backend.branches[index].name || "");
            if (branch === activity.currentBranch)
                continue;
            if (branch === "main" || branch === "master") {
                baseBranch = branch;
                return;
            }
            if (fallback.length === 0)
                fallback = branch;
        }
        baseBranch = fallback;
    }

    function formatCommitTime(seconds) {
        const value = Number(seconds);
        if (!isFinite(value))
            return "";
        return new Date(value * 1000).toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    Component.onCompleted: chooseBaseBranch()
    onBaseBranchChanged: {
        basePicker.currentIndex = -1;
        if (basePicker.editText !== baseBranch)
            basePicker.editText = baseBranch;
    }

    Connections {
        target: history.backend

        function onBranchesChanged() {
            history.chooseBaseBranch();
        }
    }

    RowLayout {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing
        Layout.topMargin: Kirigami.Units.smallSpacing

        Controls.Label {
            Layout.fillWidth: true
            color: Kirigami.Theme.disabledTextColor
            elide: Text.ElideRight
            text: qsTr("Commits on %1").arg(
                history.activity.currentBranch.length > 0
                    ? history.activity.currentBranch
                    : qsTr("detached HEAD")
            )
            textFormat: Text.PlainText
        }

        Controls.BusyIndicator {
            Layout.preferredHeight: Kirigami.Units.iconSizes.small
            Layout.preferredWidth: Kirigami.Units.iconSizes.small
            running: history.activity.job("history") !== null
            visible: running
        }

        Controls.ToolButton {
            Accessible.name: text
            Controls.ToolTip.text: qsTr("Refresh commit history")
            Controls.ToolTip.visible: hovered
            display: Controls.AbstractButton.IconOnly
            enabled: history.activity.job("history") === null
                && !history.activity.repositoryMutationRunning()
            icon.name: "view-refresh"
            text: qsTr("Refresh commit history")
            onClicked: history.backend.refreshHistory(history.project.id)
        }
    }

    RowLayout {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing

        Controls.Label {
            color: Kirigami.Theme.disabledTextColor
            text: qsTr("Compare with")
        }

        Controls.ComboBox {
            id: basePicker

            Layout.fillWidth: true
            editable: true
            enabled: history.activity.currentBranch.length > 0
                && !history.activity.reviewReadRunning()
                && !history.activity.repositoryMutationRunning()
            model: history.backend.branches
            textRole: "name"

            Component.onCompleted: {
                currentIndex = -1;
                editText = history.baseBranch;
            }
            onActivated: history.baseBranch = String(currentText).trim()
            onAccepted: history.baseBranch = String(editText).trim()
            onEditTextChanged: history.baseBranch = String(editText).trim()
        }

        Controls.ToolButton {
            Accessible.name: text
            Controls.ToolTip.text: qsTr("Review branch against merge-base")
            Controls.ToolTip.visible: hovered
            display: Controls.AbstractButton.IconOnly
            enabled: history.activity.currentBranch.length > 0
                && history.baseBranch.length > 0
                && !history.activity.reviewReadRunning()
                && !history.activity.repositoryMutationRunning()
            icon.name: "vcs-diff"
            text: qsTr("Review branch against merge-base")
            onClicked: history.backend.reviewBranch(
                history.project.id,
                history.activity.currentBranch,
                history.baseBranch
            )
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        text: history.historyReady ? history.historyState.error || "" : ""
        type: Kirigami.MessageType.Error
        visible: text.length > 0
    }

    Controls.Label {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        color: Kirigami.Theme.disabledTextColor
        text: history.historyReady && history.historyState.loading !== true
            ? qsTr("No commits yet")
            : qsTr("Loading commit history…")
        visible: history.commits.length === 0
        wrapMode: Text.Wrap
    }

    ListView {
        id: historyList

        Layout.fillHeight: true
        Layout.fillWidth: true
        activeFocusOnTab: true
        boundsBehavior: Flickable.StopAtBounds
        clip: true
        currentIndex: -1
        keyNavigationEnabled: true
        model: history.commits
        reuseItems: true
        visible: history.commits.length > 0

        delegate: Controls.ItemDelegate {
            id: commitDelegate

            required property int index
            required property var modelData

            Accessible.name: qsTr("Commit %1: %2 by %3")
                .arg(modelData.shortId)
                .arg(modelData.summary.length > 0
                    ? modelData.summary
                    : qsTr("no commit message"))
                .arg(modelData.author)
            Controls.ToolTip.text: modelData.message
            Controls.ToolTip.visible: hovered && modelData.message.length > 0
            highlighted: history.reviewCommitId === String(modelData.id)
            enabled: !history.activity.repositoryMutationRunning()
                && !history.activity.reviewReadRunning()
            width: historyList.width
            onClicked: {
                historyList.currentIndex = index;
                history.backend.reviewCommit(history.project.id, modelData.id);
            }

            contentItem: ColumnLayout {
                spacing: 0

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    font.bold: true
                    text: commitDelegate.modelData.summary.length > 0
                        ? commitDelegate.modelData.summary
                        : qsTr("(no commit message)")
                    textFormat: Text.PlainText
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        color: Kirigami.Theme.linkColor
                        font.family: "monospace"
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: commitDelegate.modelData.shortId
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        text: qsTr("%1 · %2")
                            .arg(commitDelegate.modelData.author)
                            .arg(history.formatCommitTime(
                                commitDelegate.modelData.authorTime
                            ))
                        textFormat: Text.PlainText
                    }
                }
            }
        }

        Controls.ScrollBar.vertical: Controls.ScrollBar {}
    }

    Controls.Button {
        Layout.alignment: Qt.AlignHCenter
        Layout.bottomMargin: Kirigami.Units.smallSpacing
        enabled: history.activity.job("history") === null
            && !history.activity.repositoryMutationRunning()
        icon.name: "go-down"
        text: history.activity.job("history") === null
            ? qsTr("Load older commits")
            : qsTr("Loading…")
        visible: history.historyReady && history.historyState.hasMore === true
        onClicked: history.backend.loadMoreHistory(history.project.id)
    }
}
