import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Host for the collapsible views the activity bar switches between. Every view
/// stays instantiated while another is on screen, so switching back keeps its
/// scroll position and in-progress input.
///
/// The host only decides *which* view is on screen; a view draws its own title
/// row with `PanelHeader`, because how far a view spans is the view's business.
///
/// Every child panel declares the view contract:
///   viewId         unique identifier, the handle the host switches on
///   viewTitle      title for the header and the activity-bar tooltip
///   viewIcon       themed icon name for the activity bar
///   viewShortcut   shortcut advertised in the tooltip; may be empty
///   viewBadge      count drawn on the activity-bar icon; 0 hides it
///   viewBadgeColor ground for that count; optional, accent when not declared
///   viewAvailable  whether the view applies to the current project
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
            // The backdrop a view that does not paint its whole surface falls
            // back to, so the sidebar never shows through to the window.
            color: Kirigami.Theme.alternateBackgroundColor
        },

        StackLayout {
            id: stack

            anchors.fill: parent
            currentIndex: sidePanel.currentIndex
        }
    ]
}
