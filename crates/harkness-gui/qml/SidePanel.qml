import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Host for the collapsible views the activity bar switches between: a title
/// row carrying the current view's own actions, above the view itself. Every
/// view stays instantiated while another is on screen, so switching back keeps
/// its scroll position and in-progress input.
///
/// Every child panel declares the view contract:
///   viewId         unique identifier, the handle the host switches on
///   viewTitle      title for the header and the activity-bar tooltip
///   viewIcon       themed icon name for the activity bar
///   viewShortcut   shortcut advertised in the tooltip; may be empty
///   viewBadge      count drawn on the activity-bar icon; 0 hides it
///   viewAvailable  whether the view applies to the current project
///   viewActions    array of Kirigami.Action shown in the header
Item {
    id: sidePanel

    /// Panels to host, in activity-bar order.
    default property alias panels: stack.data
    /// Identifier of the panel to show.
    property string currentViewId: ""
    /// The hosted panels, as the model ActivityBar consumes.
    property var views: []

    readonly property Item currentPanel: currentIndex >= 0
        ? stack.children[currentIndex]
        : null
    /// True once the current view both exists and applies to this project,
    /// which is what makes the panel worth showing at all.
    readonly property bool currentPanelReady: currentPanel !== null
        && currentPanel.viewAvailable
    readonly property bool hasAvailableView: {
        for (let index = 0; index < views.length; ++index) {
            if (views[index].viewAvailable)
                return true;
        }
        return false;
    }
    readonly property int currentIndex: {
        for (let index = 0; index < stack.children.length; ++index) {
            if (String(stack.children[index].viewId || "") === currentViewId)
                return index;
        }
        return -1;
    }

    /// Emitted when the user dismisses the panel from its header.
    signal hideRequested()

    implicitWidth: Kirigami.Units.gridUnit * 23

    /// Returns the hosted panel with this identifier, or null when no such
    /// view is declared.
    function view(viewId) {
        for (let index = 0; index < views.length; ++index) {
            if (String(views[index].viewId) === String(viewId))
                return views[index];
        }
        return null;
    }

    /// Identifier of the first view that applies to this project, so the host
    /// never has to name a fallback itself.
    function firstAvailableViewId() {
        for (let index = 0; index < views.length; ++index) {
            if (views[index].viewAvailable)
                return String(views[index].viewId);
        }
        return "";
    }

    // The panels are declared statically, so the model is collected once
    // rather than rebuilt by a binding every time a badge changes.
    function collectViews() {
        const collected = [];
        for (let index = 0; index < stack.children.length; ++index) {
            const panel = stack.children[index];
            if (panel.viewId !== undefined)
                collected.push(panel);
        }
        views = collected;
    }

    Component.onCompleted: collectViews()

    // Redefining the default property sends children declared here into the
    // view stack as well, so this file's own chrome is assigned to `data`
    // explicitly.
    data: [
        Rectangle {
            anchors.fill: parent
            // Matches the panels themselves, so the header reads as part of
            // the sidebar rather than as a strip of window chrome.
            color: Kirigami.Theme.alternateBackgroundColor
        },

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            RowLayout {
                Layout.bottomMargin: Kirigami.Units.smallSpacing
                Layout.fillWidth: true
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
                    text: sidePanel.currentPanel ? sidePanel.currentPanel.viewTitle : ""
                }

                Repeater {
                    model: sidePanel.currentPanel ? sidePanel.currentPanel.viewActions : []

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
                    onClicked: sidePanel.hideRequested()
                }
            }

            Kirigami.Separator {
                Layout.fillWidth: true
            }

            StackLayout {
                id: stack

                Layout.fillHeight: true
                Layout.fillWidth: true
                currentIndex: sidePanel.currentIndex
            }
        }
    ]
}
