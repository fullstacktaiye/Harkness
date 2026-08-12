import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

/// One rendering of the core-supplied discard description, shared by every
/// review and changed-file entry point.
Kirigami.PromptDialog {
    id: prompt

    property var description: ({})
    property string subject: ""

    readonly property string operation: String(description.operation || "")
    readonly property bool deletesUntracked: operation === "delete_untracked"
    readonly property bool restoresHead: operation === "restore_head"
    readonly property bool restoresHunks: operation === "restore_hunks"

    signal confirmed(string operation)

    background: FloatingSurface {}
    standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
    title: deletesUntracked
        ? qsTr("Delete untracked file?")
        : restoresHunks
            ? qsTr("Discard this hunk?")
            : qsTr("Discard tracked changes?")
    subtitle: {
        const paths = description.paths || [];
        const named = paths.length > 0 ? paths.join("\n") : subject;
        if (deletesUntracked) {
            return qsTr("Permanently delete %1 untracked file(s):\n%2\n\nGit has no copy of these bytes. This cannot be undone.")
                .arg(Number(description.untrackedFiles || 0))
                .arg(named);
        }
        if (restoresHead) {
            return qsTr("Restore %1 tracked file(s) from HEAD:\n%2\n\nBoth staged and unstaged edits will be lost. HEAD remains in Git, but these uncommitted edits cannot be recovered.")
                .arg(Number(description.trackedFiles || 0))
                .arg(named);
        }
        if (restoresHunks) {
            return qsTr("Restore %1 tracked hunk(s) from the index in:\n%2\n\nThe rest of the file and the index stay unchanged. These uncommitted edits cannot be recovered.")
                .arg(Number(description.hunks || 0))
                .arg(named);
        }
        return qsTr("Restore %1 tracked file(s) from the index:\n%2\n\nStaged content remains in Git. The discarded unstaged edits cannot be recovered.")
            .arg(Number(description.trackedFiles || 0))
            .arg(named);
    }
    onAccepted: confirmed(operation)
    onOpened: {
        // A destructive prompt begins on Cancel so Enter cannot confirm it by
        // accident. Pointer users still receive the ordinary button order.
        const cancel = standardButton(Kirigami.Dialog.Cancel);
        if (cancel)
            cancel.forceActiveFocus();
    }
}
