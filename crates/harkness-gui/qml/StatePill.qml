import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

/// A short state word on its own tinted ground.
///
/// The run surfaces mark a lifecycle state, a risk level and an approval's
/// answer the same way, so the shape lives in one file rather than once per
/// page. It carries no palette of its own: the colour is the caller's
/// decision — `RunState.stateColor`, `approvalStateColor`, or a risk accent —
/// and this only draws it.
///
/// The label is `Text.PlainText` because a state this build does not define
/// falls through to its stored spelling, which came out of a database row.
Rectangle {
    id: pill

    required property color pillColor
    property alias text: pillLabel.text

    border.color: Qt.alpha(pill.pillColor, 0.85)
    border.width: 1
    color: Qt.alpha(pill.pillColor, 0.16)
    implicitHeight: pillLabel.implicitHeight + Kirigami.Units.smallSpacing
    implicitWidth: pillLabel.implicitWidth + Kirigami.Units.largeSpacing
    radius: implicitHeight / 2
    visible: pillLabel.text.length > 0

    Controls.Label {
        id: pillLabel

        anchors.centerIn: parent
        color: pill.pillColor
        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
        textFormat: Text.PlainText
    }
}
