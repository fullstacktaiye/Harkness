import QtQuick
import org.kde.kirigami as Kirigami

/// The ground every floating surface — dialog, menu, dropdown — is drawn on.
///
/// Qt Quick Controls and Kirigami both paint those from the desktop colour
/// scheme, with theme inheritance turned off inside the component, so no
/// colour the window states can reach them: replacing the background outright
/// is the only way onto the application's black. The border stands in for the
/// shadow that background carried, because against a black window and a
/// translucent scrim an unbordered surface has no edge at all.
Rectangle {
    border.color: Qt.rgba(1, 1, 1, 0.18)
    border.width: 1
    color: "#000000"
    radius: Kirigami.Units.cornerRadius
}
