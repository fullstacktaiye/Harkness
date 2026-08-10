import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// One cell of the repository toolbar: a caption above the value it currently
/// holds, the way GitHub Desktop draws its branch and sync controls.
///
/// The caption names the thing being shown ("Current branch") and the value is
/// what it is right now, so the toolbar reads as a statement of repository
/// state that happens to be clickable, rather than as a row of verbs.
Controls.ToolButton {
    id: segment

    /// Static name of what this cell holds.
    property string caption: ""
    /// The state itself, drawn prominently.
    property string value: ""
    /// Trailing count, e.g. the commits a pull would bring down. Empty hides it.
    property string badge: ""
    /// Draws the chevron that says this cell opens a list.
    property bool expandable: true
    /// Replaces the chevron with a spinner while this cell's work runs.
    property bool busy: false

    Layout.maximumWidth: Kirigami.Units.gridUnit * 18
    Layout.minimumWidth: Kirigami.Units.gridUnit * 7
    padding: Kirigami.Units.smallSpacing
    // Two stacked labels are taller than the default tool-button content, and
    // the toolbar cells must line up whatever text they hold.
    implicitHeight: Math.max(
        Kirigami.Units.gridUnit * 2.2,
        implicitContentHeight + topPadding + bottomPadding
    )

    contentItem: RowLayout {
        spacing: Kirigami.Units.smallSpacing

        Kirigami.Icon {
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            source: segment.icon.name
            visible: segment.icon.name.length > 0
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            Controls.Label {
                Layout.fillWidth: true
                color: Kirigami.Theme.disabledTextColor
                elide: Text.ElideRight
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: segment.caption
                textFormat: Text.PlainText
                visible: text.length > 0
            }

            Controls.Label {
                Layout.fillWidth: true
                elide: Text.ElideRight
                font.bold: true
                text: segment.value
                textFormat: Text.PlainText
            }
        }

        Controls.Label {
            Layout.alignment: Qt.AlignVCenter
            background: Rectangle {
                color: Kirigami.Theme.alternateBackgroundColor
                radius: height / 2
            }
            horizontalAlignment: Text.AlignHCenter
            leftPadding: Kirigami.Units.smallSpacing
            rightPadding: Kirigami.Units.smallSpacing
            text: segment.badge
            textFormat: Text.PlainText
            visible: segment.badge.length > 0
        }

        Controls.BusyIndicator {
            Layout.preferredHeight: Kirigami.Units.iconSizes.small
            Layout.preferredWidth: Kirigami.Units.iconSizes.small
            running: segment.busy
            visible: segment.busy
        }

        Kirigami.Icon {
            Layout.preferredHeight: Kirigami.Units.iconSizes.small
            Layout.preferredWidth: Kirigami.Units.iconSizes.small
            source: "pan-down-symbolic"
            visible: segment.expandable && !segment.busy
        }
    }
}
