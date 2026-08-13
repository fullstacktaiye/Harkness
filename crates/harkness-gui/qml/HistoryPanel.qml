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

    // Commit subjects are usually short, but history also contains commits
    // written with a full sentence or a ticket description in the subject.
    // Keep the row stable even when the delegate is narrower than the text;
    // the original message remains available from the row tooltip.
    readonly property int commitSummaryLimit: 50
    readonly property int commitMessageLimit: 240

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

    function truncateCommitText(value, limit) {
        const text = String(value || "").replace(/\s+/g, " ").trim();
        if (text.length <= limit)
            return text;

        // JavaScript strings are UTF-16. Advance over a surrogate pair as one
        // character so a supplementary-plane character cannot be split by the
        // preview boundary.
        let end = 0;
        let count = 0;
        while (end < text.length && count < limit - 1) {
            const first = text.charCodeAt(end);
            const second = end + 1 < text.length ? text.charCodeAt(end + 1) : 0;
            const surrogatePair = first >= 0xD800 && first <= 0xDBFF
                && second >= 0xDC00 && second <= 0xDFFF;
            end += surrogatePair ? 2 : 1;
            ++count;
        }
        return text.slice(0, end).replace(/\s+$/, "") + "…";
    }

    function commitSummaryPreview(summary, message) {
        const source = summary === undefined || summary === null ? message : summary;
        return truncateCommitText(source, commitSummaryLimit);
    }

    function commitMessagePreview(message) {
        return truncateCommitText(message, commitMessageLimit);
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

            background: FieldSurface {
                field: basePicker
            }
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
            readonly property string summaryPreview: history.commitSummaryPreview(
                modelData.summary,
                modelData.message
            )

            Accessible.name: qsTr("Commit %1: %2 by %3")
                .arg(modelData.shortId)
                .arg(modelData.summary.length > 0
                    ? modelData.summary
                    : qsTr("no commit message"))
                .arg(modelData.author)
            Accessible.description: modelData.message
            Controls.ToolTip.text: history.commitMessagePreview(modelData.message)
            Controls.ToolTip.visible: hovered && modelData.message.length > 0
            highlighted: history.reviewCommitId === String(modelData.id)
            enabled: !history.activity.repositoryMutationRunning()
                && !history.activity.reviewReadRunning()
            clip: true
            width: historyList.width
            onClicked: {
                historyList.currentIndex = index;
                history.backend.reviewCommit(history.project.id, modelData.id);
            }

            contentItem: ColumnLayout {
                clip: true
                spacing: 0

                Controls.Label {
                    Layout.fillWidth: true
                    Layout.maximumWidth: parent.width
                    Layout.minimumWidth: 0
                    elide: Text.ElideRight
                    font.bold: true
                    maximumLineCount: 1
                    text: commitDelegate.summaryPreview.length > 0
                        ? commitDelegate.summaryPreview
                        : qsTr("(no commit message)")
                    textFormat: Text.PlainText
                    wrapMode: Text.NoWrap
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
                        Layout.minimumWidth: 0
                        color: Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
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
