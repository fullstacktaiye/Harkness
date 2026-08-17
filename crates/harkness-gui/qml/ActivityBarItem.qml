import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

/// One entry of the activity bar: an icon-only button carrying the accent bar
/// that marks the view on screen and an optional count badge.
Controls.AbstractButton {
    id: item

    /// Marks this entry as the view the side panel is currently showing.
    property bool active: false
    /// Count drawn in the corner badge; anything below one hides it.
    property int badge: 0
    /// Ground the badge is drawn on. A view whose count is a count of things
    /// that are wrong — failing checks, rather than open issues — says so
    /// here, so the bar distinguishes "there is work" from "something failed"
    /// without the panel having to be open to find out which.
    property color badgeColor: Kirigami.Theme.highlightColor
    /// Themed icon name for the view.
    property string iconName: ""
    /// Shortcut advertised in the tooltip, for example "Ctrl+Shift+G".
    property string shortcutText: ""

    implicitHeight: Kirigami.Units.gridUnit * 2.5
    implicitWidth: Kirigami.Units.gridUnit * 2.5
    hoverEnabled: true

    Accessible.name: item.text
    Controls.ToolTip.delay: Kirigami.Units.toolTipDelay
    Controls.ToolTip.text: item.shortcutText.length > 0
        ? qsTr("%1 (%2)").arg(item.text).arg(item.shortcutText)
        : item.text
    Controls.ToolTip.visible: item.hovered

    background: Rectangle {
        color: item.pressed
            ? Kirigami.Theme.alternateBackgroundColor
            : item.hovered
                ? Kirigami.Theme.hoverColor
                : "transparent"

        // The active view is marked on the leading edge rather than by a
        // filled background, so the mark stays readable while the pointer
        // hovers a different entry.
        Rectangle {
            anchors.bottom: parent.bottom
            anchors.left: parent.left
            anchors.top: parent.top
            color: Kirigami.Theme.highlightColor
            visible: item.active
            width: Math.round(Kirigami.Units.smallSpacing / 2)
        }
    }

    contentItem: Item {
        Kirigami.Icon {
            id: viewIcon

            anchors.centerIn: parent
            // Dimming inactive entries is what makes the active one legible at
            // a glance; masking keeps every view icon to a single weight.
            color: item.active || item.hovered
                ? Kirigami.Theme.textColor
                : Kirigami.Theme.disabledTextColor
            height: Kirigami.Units.iconSizes.smallMedium
            isMask: true
            source: item.iconName
            width: Kirigami.Units.iconSizes.smallMedium
        }

        Rectangle {
            anchors.horizontalCenter: viewIcon.right
            anchors.verticalCenter: viewIcon.bottom
            color: item.badgeColor
            height: badgeLabel.implicitHeight + Math.round(Kirigami.Units.smallSpacing / 2)
            radius: height / 2
            visible: item.badge > 0
            width: Math.max(height, badgeLabel.implicitWidth + Kirigami.Units.smallSpacing * 2)

            Controls.Label {
                id: badgeLabel

                anchors.centerIn: parent
                color: Kirigami.Theme.highlightedTextColor
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                // Three-digit counts would widen the badge past the icon; the
                // exact number stops being useful long before that.
                text: item.badge > 99 ? qsTr("99+") : String(item.badge)
            }
        }
    }
}
