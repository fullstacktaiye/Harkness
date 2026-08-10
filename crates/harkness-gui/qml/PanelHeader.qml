import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Title row for a side-panel view: the view's name, the actions it publishes,
/// and the control that collapses the whole view away again.
///
/// A view owns its header rather than the host drawing one, because a view is
/// free to span more than the narrow column the activity bar suggests — the
/// source-control view puts its header over the column and keeps the review
/// surface beside it uncovered.
RowLayout {
    id: header

    /// Name of the view, shown the way the activity bar tooltip spells it.
    property string title: ""
    /// `Kirigami.Action`s to draw as icon-only buttons beside the title.
    property var actions: []

    /// Emitted when the user dismisses the view from this header.
    signal hideRequested()

    Layout.bottomMargin: Kirigami.Units.smallSpacing
    Layout.leftMargin: Kirigami.Units.largeSpacing
    Layout.rightMargin: Kirigami.Units.smallSpacing
    Layout.topMargin: Kirigami.Units.smallSpacing
    spacing: Kirigami.Units.smallSpacing

    Controls.Label {
        Layout.fillWidth: true
        color: Kirigami.Theme.disabledTextColor
        elide: Text.ElideRight
        font.capitalization: Font.AllUppercase
        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
        text: header.title
        textFormat: Text.PlainText
    }

    Repeater {
        model: header.actions

        Controls.ToolButton {
            required property var modelData

            Controls.ToolTip.text: String(modelData.tooltip).length > 0
                ? modelData.tooltip
                : modelData.text
            Controls.ToolTip.visible: hovered
            action: modelData
            display: Controls.AbstractButton.IconOnly
        }
    }

    Controls.ToolButton {
        Controls.ToolTip.text: qsTr("Hide the side panel (Ctrl+B)")
        Controls.ToolTip.visible: hovered
        display: Controls.AbstractButton.IconOnly
        icon.name: "window-close-symbolic"
        text: qsTr("Hide")
        onClicked: header.hideRequested()
    }
}
