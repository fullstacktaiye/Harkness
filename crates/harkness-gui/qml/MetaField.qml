import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// One `name value` pair of a run or approval header.
///
/// Hidden when it has no value, so a header states what a record holds rather
/// than a column of empty labels. Both halves are `Text.PlainText`: a value
/// here is a tool identifier, a workspace path, a scope word, or a timestamp,
/// none of which this window authored.
Row {
    id: field

    property string name: ""
    property string value: ""
    property bool monospace: false

    spacing: Kirigami.Units.smallSpacing
    visible: field.value.length > 0

    RunState {
        id: vocabulary
    }

    Controls.Label {
        color: vocabulary.dimColor
        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
        text: field.name
        textFormat: Text.PlainText
    }

    Controls.Label {
        color: vocabulary.bodyColor
        font.family: field.monospace ? "monospace" : Kirigami.Theme.defaultFont.family
        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
        text: field.value
        textFormat: Text.PlainText
    }
}
