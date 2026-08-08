import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Effects
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// A large clickable card for the launcher's primary actions. Web-style: soft
// shadow, generous rounding, and a hover lift instead of native chrome.
Controls.AbstractButton {
    id: card

    required property string iconName
    required property string title
    required property string subtitle

    readonly property real cardRadius: Kirigami.Units.cornerRadius * 3
    readonly property bool raised: card.hovered || card.activeFocus

    signal activated()

    function tinted(color, alpha) {
        return Qt.rgba(color.r, color.g, color.b, alpha);
    }

    hoverEnabled: true
    padding: Kirigami.Units.largeSpacing * 1.25
    onClicked: card.activated()

    transform: Translate {
        y: card.raised ? -3 : 0

        Behavior on y {
            NumberAnimation {
                duration: Kirigami.Units.shortDuration
                easing.type: Easing.OutCubic
            }
        }
    }

    background: Item {
        Rectangle {
            id: surface

            anchors.fill: parent
            border.color: card.activeFocus ? Kirigami.Theme.focusColor : card.tinted(Kirigami.Theme.textColor, 0.08)
            border.width: card.activeFocus ? 2 : 1
            color: card.hovered ? Qt.tint(Kirigami.Theme.backgroundColor, card.tinted(Kirigami.Theme.highlightColor, 0.06)) : Kirigami.Theme.backgroundColor
            radius: card.cardRadius
            visible: false

            Behavior on color {
                ColorAnimation {
                    duration: Kirigami.Units.shortDuration
                }
            }
        }

        MultiEffect {
            anchors.fill: surface
            blurMax: 32
            shadowBlur: card.raised ? 0.7 : 0.4
            shadowColor: Qt.rgba(0, 0, 0, 0.5)
            shadowEnabled: true
            shadowHorizontalOffset: 0
            shadowOpacity: card.raised ? 0.28 : 0.14
            shadowVerticalOffset: card.raised ? 10 : 4
            source: surface

            Behavior on shadowVerticalOffset {
                NumberAnimation {
                    duration: Kirigami.Units.shortDuration
                    easing.type: Easing.OutCubic
                }
            }

            Behavior on shadowOpacity {
                NumberAnimation {
                    duration: Kirigami.Units.shortDuration
                }
            }
        }
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Rectangle {
            Layout.preferredHeight: Kirigami.Units.iconSizes.large + Kirigami.Units.largeSpacing
            Layout.preferredWidth: Kirigami.Units.iconSizes.large + Kirigami.Units.largeSpacing
            color: card.tinted(Kirigami.Theme.highlightColor, card.hovered ? 0.22 : 0.14)
            radius: card.cardRadius * 0.7

            Behavior on color {
                ColorAnimation {
                    duration: Kirigami.Units.shortDuration
                }
            }

            Kirigami.Icon {
                anchors.centerIn: parent
                color: Kirigami.Theme.highlightColor
                height: Kirigami.Units.iconSizes.large
                width: Kirigami.Units.iconSizes.large
                source: card.iconName
            }
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing / 2

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
            opacity: card.hovered ? 1 : 0.5
            source: "go-next-symbolic"

            Behavior on opacity {
                NumberAnimation {
                    duration: Kirigami.Units.shortDuration
                }
            }
        }
    }
}
