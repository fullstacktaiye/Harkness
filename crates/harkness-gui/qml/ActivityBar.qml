import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// The vertical strip of top-level views along the leading edge of the shell.
/// It only reports what the user picked; the host decides whether that
/// switches the side panel, expands it or collapses it again.
Item {
    id: activityBar

    /// Panel items to advertise, in display order. Each one implements the
    /// view contract documented in SidePanel.qml.
    required property var views
    /// Identifier of the view the side panel is showing.
    property string currentViewId: ""
    /// False while the side panel is collapsed, which leaves every entry
    /// unaccented even though one of them is still the current view.
    property bool panelExpanded: true

    /// Emitted when an entry is activated, including the current one.
    signal viewTriggered(string viewId)

    implicitWidth: Kirigami.Units.gridUnit * 2.5

    // The bar is chrome rather than content, so it keeps the window colour
    // instead of inheriting the panel background beside it. Turning inheritance
    // off also cuts it off from the window's black ground, so it restates it.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
    }

    ColumnLayout {
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.topMargin: Kirigami.Units.smallSpacing
        spacing: 0

        Repeater {
            model: activityBar.views

            ActivityBarItem {
                required property var modelData

                Layout.alignment: Qt.AlignHCenter
                active: activityBar.panelExpanded
                    && activityBar.currentViewId === modelData.viewId
                badge: modelData.viewBadge
                // Optional in the view contract: a view that does not name a
                // colour is counting something ordinary, so the count keeps
                // the accent every other badge in the bar uses.
                badgeColor: modelData.viewBadgeColor !== undefined
                    ? modelData.viewBadgeColor
                    : Kirigami.Theme.highlightColor
                iconName: modelData.viewIcon
                shortcutText: modelData.viewShortcut
                text: modelData.viewTitle
                visible: modelData.viewAvailable
                onClicked: activityBar.viewTriggered(modelData.viewId)
            }
        }
    }
}
