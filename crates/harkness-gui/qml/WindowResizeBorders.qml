import QtQuick
import QtQuick.Window
import org.kde.kirigami as Kirigami

/// The eight invisible grips that let a frameless window be resized.
///
/// A decorated window gets these from the compositor; once the decoration is
/// gone they have to be drawn by the client. Each grip only asks the
/// compositor to start the resize, so the window never positions itself and
/// snapping, tiling and multi-screen behaviour stay with the compositor. Live
/// in the window overlay rather than in the page stack, because the overlay is
/// the one item that spans the title bar as well as the content.
Item {
    id: borders

    readonly property Window window: Window.window
    /// Reach of each grip. Matches the invisible border a decorated window
    /// offers outside its frame.
    property int thickness: Kirigami.Units.smallSpacing + 2

    anchors.fill: parent
    // A maximised or full-screen window has no border to drag, and leaving the
    // grips live there would steal clicks from the menus behind them.
    visible: borders.window && borders.window.visibility === Window.Windowed

    component Grip: MouseArea {
        required property int resizeEdges

        acceptedButtons: Qt.LeftButton
        onPressed: borders.window.startSystemResize(resizeEdges)
    }

    Grip {
        anchors.left: parent.left
        anchors.top: parent.top
        cursorShape: Qt.SizeFDiagCursor
        height: borders.thickness
        resizeEdges: Qt.LeftEdge | Qt.TopEdge
        width: borders.thickness
    }

    Grip {
        anchors.left: parent.left
        anchors.leftMargin: borders.thickness
        anchors.right: parent.right
        anchors.rightMargin: borders.thickness
        anchors.top: parent.top
        cursorShape: Qt.SizeVerCursor
        height: borders.thickness
        resizeEdges: Qt.TopEdge
    }

    Grip {
        anchors.right: parent.right
        anchors.top: parent.top
        cursorShape: Qt.SizeBDiagCursor
        height: borders.thickness
        resizeEdges: Qt.RightEdge | Qt.TopEdge
        width: borders.thickness
    }

    Grip {
        anchors.bottom: parent.bottom
        anchors.bottomMargin: borders.thickness
        anchors.left: parent.left
        anchors.top: parent.top
        anchors.topMargin: borders.thickness
        cursorShape: Qt.SizeHorCursor
        resizeEdges: Qt.LeftEdge
        width: borders.thickness
    }

    Grip {
        anchors.bottom: parent.bottom
        anchors.bottomMargin: borders.thickness
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.topMargin: borders.thickness
        cursorShape: Qt.SizeHorCursor
        resizeEdges: Qt.RightEdge
        width: borders.thickness
    }

    Grip {
        anchors.bottom: parent.bottom
        anchors.left: parent.left
        cursorShape: Qt.SizeBDiagCursor
        height: borders.thickness
        resizeEdges: Qt.LeftEdge | Qt.BottomEdge
        width: borders.thickness
    }

    Grip {
        anchors.bottom: parent.bottom
        anchors.left: parent.left
        anchors.leftMargin: borders.thickness
        anchors.right: parent.right
        anchors.rightMargin: borders.thickness
        cursorShape: Qt.SizeVerCursor
        height: borders.thickness
        resizeEdges: Qt.BottomEdge
    }

    Grip {
        anchors.bottom: parent.bottom
        anchors.right: parent.right
        cursorShape: Qt.SizeFDiagCursor
        height: borders.thickness
        resizeEdges: Qt.RightEdge | Qt.BottomEdge
        width: borders.thickness
    }
}
