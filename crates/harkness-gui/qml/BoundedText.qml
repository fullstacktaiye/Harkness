import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// A height-capped monospace rendering of something a tool produced.
///
/// Capped by line count rather than put in a scroll area on purpose: a
/// `Flickable` inside a `ListView` delegate steals the wheel from the list it
/// sits in, and the bridge has already bounded what arrives here — an event
/// payload at 8 KiB, an artifact excerpt at 8 KiB, an approval input at 8 KiB.
/// A rendering that is still cut says which of the two cut it, and Copy puts
/// the whole of what was loaded on the clipboard.
///
/// One file rather than an inline component per page, because the run detail
/// page and the approval surface render the same three things — a payload, an
/// artifact, a validated tool input — and a second copy of the rule that keeps
/// them inert is a second place for it to stop being true. Everything here is
/// `Text.PlainText`: the content is tool output, an agent's text, or a
/// recorded input, and none of it is ever executed or opened.
ColumnLayout {
    id: bounded

    /// The text to render; empty renders nothing at all.
    property string content: ""
    /// Whether the bridge cut the content before it arrived.
    property bool cut: false
    /// Lines shown before the rendering is elided.
    property int lines: 24
    /// The Git and catalog bridge, used here for the clipboard alone. Copy is
    /// hidden without one rather than offering a button that does nothing.
    property var clipboard: null

    spacing: Kirigami.Units.smallSpacing
    visible: bounded.content.length > 0

    RunState {
        id: vocabulary
    }

    // The ground is the label's own `background` rather than a `Rectangle`
    // around it. A rectangle sized from a wrapping label is an item whose
    // implicit height depends on the width the layout above it is still
    // deciding, which Qt Quick Layouts detects as a recursive rearrange and
    // abandons; a layout handles a `Text` directly.
    Controls.Label {
        id: boundedLabel

        Layout.fillWidth: true
        bottomPadding: Kirigami.Units.smallSpacing
        color: vocabulary.bodyColor
        elide: Text.ElideRight
        font.family: "monospace"
        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
        leftPadding: Kirigami.Units.smallSpacing
        // Twenty-four lines is a paragraph of evidence rather than a pane of
        // it; what is cut here is still on the clipboard and in
        // `harkness run show`.
        maximumLineCount: bounded.lines
        rightPadding: Kirigami.Units.smallSpacing
        // Tool output, an event payload, or a validated tool input.
        text: bounded.content
        textFormat: Text.PlainText
        topPadding: Kirigami.Units.smallSpacing
        wrapMode: Text.WrapAnywhere

        background: Rectangle {
            border.color: vocabulary.frameColor
            border.width: 1
            color: Kirigami.Theme.alternateBackgroundColor
            radius: Kirigami.Units.smallSpacing
        }
    }

    RowLayout {
        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing

        Controls.Label {
            Layout.fillWidth: true
            color: vocabulary.neutralColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: bounded.cut
                ? qsTr("Harkness cut this short; the command line prints the whole of it.")
                : qsTr("Cut short to fit; Copy takes all of what was loaded.")
            textFormat: Text.PlainText
            visible: bounded.cut || boundedLabel.truncated
            wrapMode: Text.WordWrap
        }

        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Copy what was loaded")
            Controls.ToolTip.visible: hovered
            display: Controls.AbstractButton.IconOnly
            icon.name: "edit-copy-symbolic"
            text: qsTr("Copy")
            visible: bounded.clipboard !== null
            onClicked: {
                if (bounded.clipboard !== null)
                    bounded.clipboard.copyToClipboard(bounded.content);
            }
        }
    }
}
