import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// The "Changes" tab of the source-control view: what the index and working
/// tree hold right now, and the diff each entry opens beside it.
///
/// Everything here is about the uncommitted state. Anything that reaches the
/// remote or switches what is checked out lives in the header toolbar, so this
/// column stays a list the commit footer below it acts on.
ColumnLayout {
    id: changes

    required property var backend
    required property var project
    /// Shared job and Git-state projection; see GitActivity.qml.
    required property var activity

    /// Asked for when a push has been refused because the branch is the
    /// remote's default; the view hosting this tab owns the confirmation.
    signal pushOverrideRequested()

    readonly property var gitState: activity.gitState
    readonly property var entries: activity.entries

    spacing: 0

    // InlineMessage does not expose its internal label's textFormat. Wrap an
    // escaped value so repository-controlled text is displayed literally.
    function escapedRichText(value) {
        return "<span>" + String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;") + "</span>";
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
            text: changes.activity.stateReady
                ? changes.gitState.head
                : qsTr("Loading repository status…")
            textFormat: Text.PlainText
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            text: changes.escapedRichText(
                qsTr("A %1 is waiting to be resolved or aborted.")
                    .arg(changes.gitState.pending || "")
            )
            type: Kirigami.MessageType.Warning
            visible: changes.activity.stateReady
                && changes.gitState.pending
                && changes.gitState.pending.length > 0
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            text: changes.escapedRichText(changes.gitState.error || "")
            type: Kirigami.MessageType.Error
            visible: changes.activity.stateReady
                && changes.gitState.error
                && changes.gitState.error.length > 0
        }

        Controls.Button {
            Layout.alignment: Qt.AlignRight
            enabled: !changes.activity.repositoryOperationRunning()
            icon.name: "dialog-warning"
            text: qsTr("Push to default branch anyway…")
            visible: changes.activity.stateReady
                && ["default_branch_push", "default_branch_unknown"]
                    .indexOf(changes.gitState.errorKind) !== -1
            onClicked: changes.pushOverrideRequested()
        }
    }

    Kirigami.Separator {
        Layout.fillWidth: true
    }

    RowLayout {
        Layout.fillWidth: true
        Layout.bottomMargin: Kirigami.Units.smallSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing
        Layout.topMargin: Kirigami.Units.smallSpacing

        Controls.Label {
            Layout.fillWidth: true
            elide: Text.ElideRight
            text: changes.entries.length === 1
                ? qsTr("1 changed file")
                : qsTr("%1 changed files").arg(changes.entries.length)
            textFormat: Text.PlainText
        }

        // The whole-diff entry points: a single review of everything staged, or
        // of everything still in the working tree, without picking a path.
        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Review every staged change")
            Controls.ToolTip.visible: hovered
            enabled: !changes.activity.repositoryMutationRunning()
                && !changes.activity.reviewReadRunning()
            text: qsTr("Staged")
            onClicked: changes.backend.reviewWorkingChanges(changes.project.id, true, "")
        }

        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Review every unstaged change")
            Controls.ToolTip.visible: hovered
            enabled: !changes.activity.repositoryMutationRunning()
                && !changes.activity.reviewReadRunning()
            text: qsTr("Unstaged")
            onClicked: changes.backend.reviewWorkingChanges(changes.project.id, false, "")
        }
    }

    Controls.Label {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        color: Kirigami.Theme.positiveTextColor
        text: qsTr("Working tree clean")
        visible: changes.activity.stateReady && changes.entries.length === 0
    }

    ListView {
        id: entryList

        Layout.fillHeight: true
        Layout.fillWidth: true
        boundsBehavior: Flickable.StopAtBounds
        clip: true
        model: changes.entries
        reuseItems: true

        delegate: Controls.ItemDelegate {
            id: entryDelegate

            required property var modelData

            Controls.ToolTip.text: modelData.path
            Controls.ToolTip.visible: hovered
            enabled: !changes.activity.repositoryMutationRunning()
                && !changes.activity.reviewReadRunning()
            width: entryList.width
            onClicked: changes.selectPath(
                modelData.pathId,
                modelData.staged,
                modelData.unstaged
            )

            contentItem: ColumnLayout {
                spacing: 0

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideMiddle
                    font.family: "monospace"
                    text: entryDelegate.modelData.path
                    textFormat: Text.PlainText
                }

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Label {
                        Layout.fillWidth: true
                        color: entryDelegate.modelData.conflicted
                            ? Kirigami.Theme.negativeTextColor
                            : Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        text: {
                            const states = [];
                            if (entryDelegate.modelData.staged)
                                states.push(qsTr("staged: %1").arg(entryDelegate.modelData.staged));
                            if (entryDelegate.modelData.unstaged)
                                states.push(qsTr("working tree: %1").arg(entryDelegate.modelData.unstaged));
                            if (entryDelegate.modelData.conflicted)
                                states.push(qsTr("conflict"));
                            return states.join(" · ");
                        }
                        textFormat: Text.PlainText
                    }

                    Controls.Button {
                        enabled: !changes.activity.repositoryOperationRunning()
                        text: qsTr("Unstage")
                        visible: entryDelegate.modelData.staged.length > 0
                        onClicked: changes.backend.unstagePath(
                            changes.project.id,
                            entryDelegate.modelData.pathId
                        )
                    }

                    Controls.Button {
                        enabled: !changes.activity.repositoryOperationRunning()
                        text: qsTr("Stage")
                        visible: entryDelegate.modelData.unstaged.length > 0
                        onClicked: changes.backend.stagePath(
                            changes.project.id,
                            entryDelegate.modelData.pathId
                        )
                    }
                }
            }
        }

        Controls.ScrollBar.vertical: Controls.ScrollBar {}
    }
}
