import QtQuick
import org.kde.kirigami as Kirigami

/// The ground a text box is drawn on: a field, a message area, the editable
/// part of a picker. The widget style fills those from the desktop colour
/// scheme, which on this application's black is the one place a grey box would
/// sit directly on the panel behind it. The focus ring is the whole reason
/// this carries the control rather than reading state off its own parent.
Rectangle {
    /// The control being drawn, which is what says whether it has focus.
    required property Item field

    border.color: field.activeFocus
        ? Kirigami.Theme.focusColor
        : Qt.rgba(1, 1, 1, 0.15)
    border.width: field.activeFocus ? 2 : 1
    // Stated rather than themed: a control carries the style's own view colour
    // set with inheritance off, so `Kirigami.Theme.backgroundColor` read from
    // inside one is the desktop scheme's grey, not the window's black.
    color: "#000000"
    radius: Kirigami.Units.smallSpacing

    Behavior on border.color {
        ColorAnimation {
            duration: Kirigami.Units.shortDuration
        }
    }
}
