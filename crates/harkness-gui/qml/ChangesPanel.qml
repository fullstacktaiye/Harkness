import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// The "Changes" tab of the source-control view: what the working tree holds
/// right now, and the diff each entry opens beside it.
///
/// A row's checkbox is the only control it carries, and it means one thing:
/// whether the next commit records this file. There is no staging step behind
/// it — the commit stages what is checked, as part of committing — so a checked
/// box is a statement about the commit rather than about the index. Anything
/// that reaches the remote or switches what is checked out lives in the header
/// toolbar.
ColumnLayout {
    id: changes

    required property var backend
    required property var project
    /// Shared job and Git-state projection; see GitActivity.qml.
    required property var activity
    /// Which files the commit footer will record; see CommitSelection.qml.
    required property var selection

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
        // The working tree is the side this view is about. The staged side is
        // only ever reached for a path some other tool — the CLI, or a terminal
        // — has already staged and not touched since, which is the one case
        // where the working-tree diff would come back empty.
        backend.reviewWorkingChanges(
            project.id,
            String(unstaged || "").length === 0
                && String(staged || "").length > 0,
            selectedPathId
        );
    }

    /// The single change description for a row.
    ///
    /// Which side of the index a change sits on is not a distinction this view
    /// asks the user to act on, so the row reports the change itself and
    /// prefers the working tree, which is what the next commit will record.
    function changeSummary(entry) {
        const states = [];
        const change = String(entry.unstaged || "") || String(entry.staged || "");
        if (change.length > 0)
            states.push(change);
        if (entry.conflicted)
            states.push(qsTr("conflict"));
        return states.join(" · ");
    }

    // A file that is gone from the working tree must not stay excluded in
    // secret if it ever comes back.
    onEntriesChanged: selection.prune(entries)

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

        // GitHub Desktop's header control: one box that reads the state of
        // every row and, when pressed, gives them all the same answer.
        Controls.CheckBox {
            id: selectAll

            readonly property int includedCount: changes.selection.countIncluded(changes.entries)

            Accessible.name: text
            Controls.ToolTip.text: qsTr("Include every changed file in the commit")
            Controls.ToolTip.visible: hovered
            checkState: includedCount === 0
                ? Qt.Unchecked
                : includedCount === changes.entries.length
                    ? Qt.Checked
                    : Qt.PartiallyChecked
            enabled: changes.entries.length > 0
                && !changes.activity.repositoryMutationRunning()
            text: changes.entries.length === 1
                ? qsTr("1 changed file")
                : qsTr("%1 of %2 changed files")
                    .arg(includedCount)
                    .arg(changes.entries.length)
            // A partial state resolves to "all", never back to "none": the
            // useful move from a half-made selection is to take everything.
            tristate: true
            nextCheckState: function() {
                return includedCount === changes.entries.length
                    ? Qt.Unchecked
                    : Qt.Checked;
            }
            onToggled: changes.selection.setAll(
                changes.entries,
                checkState === Qt.Checked
            )
        }

        // The whole-diff entry point: everything the next commit would record,
        // without having to pick a path first.
        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Review every change in the working tree")
            Controls.ToolTip.visible: hovered
            enabled: changes.entries.length > 0
                && !changes.activity.repositoryMutationRunning()
                && !changes.activity.reviewReadRunning()
            text: qsTr("Review all")
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

            contentItem: RowLayout {
                spacing: Kirigami.Units.smallSpacing

                Controls.CheckBox {
                    Accessible.name: qsTr("Include %1 in the commit")
                        .arg(entryDelegate.modelData.path)
                    checked: changes.selection.included(entryDelegate.modelData.path)
                    enabled: !changes.activity.repositoryMutationRunning()
                    onToggled: changes.selection.setIncluded(
                        entryDelegate.modelData.path,
                        checked
                    )
                }

                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 0

                    Controls.Label {
                        Layout.fillWidth: true
                        elide: Text.ElideMiddle
                        font.family: "monospace"
                        text: entryDelegate.modelData.path
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: entryDelegate.modelData.conflicted
                            ? Kirigami.Theme.negativeTextColor
                            : Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        text: changes.changeSummary(entryDelegate.modelData)
                        textFormat: Text.PlainText
                        visible: text.length > 0
                    }
                }
            }
        }

        Controls.ScrollBar.vertical: Controls.ScrollBar {}
    }
}
