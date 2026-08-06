import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// A large clickable card for the launcher's primary actions. Rises slightly
// on hover or keyboard focus.
Controls.AbstractButton {
    id: card

    required property string iconName
    required property string title
    required property string subtitle

    signal activated()

    hoverEnabled: true
    padding: Kirigami.Units.largeSpacing
    onClicked: card.activated()

    background: Rectangle {
        border.color: card.activeFocus ? Kirigami.Theme.focusColor : Kirigami.Theme.disabledTextColor
        border.width: card.activeFocus ? 2 : 1
        color: card.hovered ? Kirigami.Theme.hoverColor : Kirigami.Theme.backgroundColor
        opacity: card.activeFocus || card.hovered ? 1 : 0.94
        radius: Kirigami.Units.cornerRadius

        Behavior on color {
            ColorAnimation {
                duration: Kirigami.Units.shortDuration
            }
        }
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Icon {
            Layout.preferredHeight: Kirigami.Units.iconSizes.large
            Layout.preferredWidth: Kirigami.Units.iconSizes.large
            source: card.iconName
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            Kirigami.Heading {
                level: 4
                text: card.title
            }

            Controls.Label {
                Layout.fillWidth: true
                color: Kirigami.Theme.disabledTextColor
                font: Kirigami.Theme.smallFont
                text: card.subtitle
                wrapMode: Text.Wrap
            }
        }

        Kirigami.Icon {
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            source: "go-next-symbolic"
        }
    }
}
