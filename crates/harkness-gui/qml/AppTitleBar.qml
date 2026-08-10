import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import QtQuick.Window
import org.kde.kirigami as Kirigami

/// The window's own title bar. The window is frameless, so this strip carries
/// everything a decoration would otherwise provide: the menus at the leading
/// edge, a region that drags the window, and the minimise, maximise and close
/// buttons. Menus declared inside an instance become the menu bar's menus.
///
/// Dragging and resizing are handed to the compositor rather than emulated by
/// tracking the pointer, which is what keeps tiling, snapping and edge
/// resistance working on Wayland where a client cannot place itself.
Item {
    id: titleBar

    /// Menus shown at the leading edge, in declaration order.
    default property alias menus: menuBar.contentData

    readonly property Window window: Window.window

    // The title bar is window chrome, not page content, so it takes the
    // header palette instead of inheriting the page background below it. That
    // also cuts it off from the window's black ground, so it restates it.
    Kirigami.Theme.colorSet: Kirigami.Theme.Header
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.textColor: "#ffffff"

    implicitHeight: Math.max(menuBar.implicitHeight, Kirigami.Units.gridUnit * 1.8)

    function tinted(color, alpha) {
        return Qt.rgba(color.r, color.g, color.b, alpha);
    }

    function toggleMaximized() {
        if (titleBar.window.visibility === Window.Maximized)
            titleBar.window.showNormal();
        else
            titleBar.window.showMaximized();
    }

    /// A title-bar button: square-ish, flat until hovered, and icon only.
    /// `hoverBackground` is a property because closing is the one destructive
    /// entry and reads as such only if its hover state says so.
    component WindowButton: Controls.AbstractButton {
        id: windowButton

        property color hoverBackground: titleBar.tinted(Kirigami.Theme.textColor, 0.12)
        property color hoverForeground: Kirigami.Theme.textColor

        Layout.fillHeight: true
        implicitWidth: Kirigami.Units.gridUnit * 2.4

        background: Rectangle {
            color: windowButton.pressed
                ? Qt.darker(windowButton.hoverBackground, 1.2)
                : windowButton.hovered
                    ? windowButton.hoverBackground
                    : "transparent"
        }

        contentItem: Item {
            Kirigami.Icon {
                anchors.centerIn: parent
                color: windowButton.hovered
                    ? windowButton.hoverForeground
                    : Kirigami.Theme.textColor
                height: Kirigami.Units.iconSizes.small
                source: windowButton.icon.name
                width: Kirigami.Units.iconSizes.small
            }
        }
    }

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
    }

    Kirigami.Separator {
        anchors.bottom: parent.bottom
        anchors.left: parent.left
        anchors.right: parent.right
    }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        Controls.MenuBar {
            id: menuBar

            Layout.fillHeight: true
            // The strip behind the whole bar already paints the background;
            // a second one here would draw a lighter block behind the menus.
            background: null
        }

        // Whatever the menus and buttons leave over drags the window. The move
        // is handed over only once the pointer has actually moved, because a
        // compositor-side move started on press swallows the second click and
        // double-click-to-maximise would never fire.
        MouseArea {
            id: dragArea

            property bool moving: false

            Layout.fillHeight: true
            Layout.fillWidth: true
            onDoubleClicked: titleBar.toggleMaximized()
            onPositionChanged: {
                if (!dragArea.pressed || dragArea.moving)
                    return;
                dragArea.moving = true;
                titleBar.window.startSystemMove();
            }
            onReleased: dragArea.moving = false
        }

        WindowButton {
            Controls.ToolTip.text: qsTr("Minimise")
            icon.name: "window-minimize"
            onClicked: titleBar.window.showMinimized()
        }

        WindowButton {
            Controls.ToolTip.text: titleBar.window.visibility === Window.Maximized
                ? qsTr("Restore")
                : qsTr("Maximise")
            icon.name: titleBar.window.visibility === Window.Maximized
                ? "window-restore"
                : "window-maximize"
            onClicked: titleBar.toggleMaximized()
        }

        WindowButton {
            Controls.ToolTip.text: qsTr("Close")
            hoverBackground: Kirigami.Theme.negativeTextColor
            hoverForeground: Kirigami.Theme.highlightedTextColor
            icon.name: "window-close"
            onClicked: titleBar.window.close()
        }
    }
}
