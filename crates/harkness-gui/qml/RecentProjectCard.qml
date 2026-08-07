import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// One Recents card: identity, path, and live Git state. An unavailable root
// is dimmed and disabled rather than hidden, so a missing checkout is
// explained instead of silently absent.
Controls.AbstractButton {
    id: card

    // Declaring modelData itself as required lets Qt's view populate it. A
    // differently named required property disables the legacy delegate
    // context, leaving `modelData` undefined at the call site.
    required property var modelData

    readonly property var project: modelData

    signal activated()

    enabled: card.project.available
    hoverEnabled: true
    padding: Kirigami.Units.largeSpacing
    onClicked: card.activated()

    // Describes the Git state inline; mirrors LauncherPage.describe.
    function describe() {
        if (!project.available)
            return qsTr("Missing from disk");
        if (!project.isGit)
            return qsTr("Not a Git repository");
        const branch = project.branch.length > 0 ? project.branch : qsTr("detached HEAD");
        return project.dirty ? qsTr("%1 — uncommitted changes").arg(branch) : branch;
    }

    Controls.ToolTip.text: project.available ? project.root : qsTr("This directory no longer exists on disk.")
    Controls.ToolTip.visible: hovered

    background: Rectangle {
        border.color: card.activeFocus ? Kirigami.Theme.focusColor : Kirigami.Theme.disabledTextColor
        border.width: card.activeFocus ? 2 : 1
        color: card.hovered && card.enabled ? Kirigami.Theme.hoverColor : Kirigami.Theme.backgroundColor
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
            opacity: card.project.available ? 1 : 0.5
            source: card.project.isGit ? "folder-git" : "folder"
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    level: 5
                    opacity: card.project.available ? 1 : 0.6
                    text: card.project.displayName
                }

                Kirigami.Chip {
                    checkable: false
                    closable: false
                    enabled: false
                    text: qsTr("Managed")
                    visible: card.project.managed
                }

                Kirigami.Icon {
                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                    source: "dialog-warning"
                    visible: !card.project.available
                }
            }

            Controls.Label {
                Layout.fillWidth: true
                color: Kirigami.Theme.disabledTextColor
                elide: Text.ElideMiddle
                font.family: "monospace"
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: card.project.root
            }

            Controls.Label {
                Layout.fillWidth: true
                color: {
                    if (!card.project.available)
                        return Kirigami.Theme.negativeTextColor;
                    if (card.project.dirty)
                        return Kirigami.Theme.neutralTextColor;
                    return Kirigami.Theme.disabledTextColor;
                }
                elide: Text.ElideRight
                font: Kirigami.Theme.smallFont
                text: card.describe()
            }
        }
    }
}
