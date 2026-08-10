import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// One row in the sidebar's Projects list: flat, dense, no border or shadow.
// An unavailable root stays actionable so the list can offer a safe catalog
// recovery path, mirroring the launcher's previous grid delegate.
Controls.AbstractButton {
    id: row

    // Declaring modelData itself as required lets Qt's view populate it. A
    // differently named required property disables the legacy delegate
    // context, leaving `modelData` undefined at the call site.
    required property var modelData

    readonly property var project: modelData

    signal activated()

    function tinted(color, alpha) {
        return Qt.rgba(color.r, color.g, color.b, alpha);
    }

    // Describes the Git state inline; mirrors LauncherPage.describe.
    function describe() {
        if (!project.available)
            return qsTr("Missing from disk");
        if (!project.isGit)
            return qsTr("Not a Git repository");
        const branch = project.branch.length > 0 ? project.branch : qsTr("detached HEAD");
        return project.dirty ? qsTr("%1 — uncommitted changes").arg(branch) : branch;
    }

    enabled: true
    hoverEnabled: true
    implicitHeight: Kirigami.Units.gridUnit * 2.75
    padding: Kirigami.Units.smallSpacing

    Controls.ToolTip.text: project.available
        ? project.root
        : qsTr("This directory no longer exists. Click to remove its catalog entry.")
    Controls.ToolTip.visible: hovered
    onClicked: row.activated()

    background: Rectangle {
        color: row.pressed
            ? row.tinted(Kirigami.Theme.textColor, 0.09)
            : (row.hovered || row.activeFocus) && row.enabled
                ? row.tinted(Kirigami.Theme.textColor, 0.05)
                : "transparent"
        radius: Kirigami.Units.cornerRadius * 2

        Behavior on color {
            ColorAnimation {
                duration: Kirigami.Units.shortDuration
            }
        }
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.smallSpacing

        Kirigami.Icon {
            Layout.preferredHeight: Kirigami.Units.iconSizes.small
            Layout.preferredWidth: Kirigami.Units.iconSizes.small
            opacity: row.project.available ? 1 : 0.5
            source: row.project.isGit ? "folder-git" : "folder"
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            Controls.Label {
                Layout.fillWidth: true
                elide: Text.ElideRight
                font.bold: true
                opacity: row.project.available ? 1 : 0.6
                text: row.project.displayName
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing / 2

                Controls.Label {
                    Layout.fillWidth: true
                    color: {
                        if (!row.project.available)
                            return Kirigami.Theme.negativeTextColor;
                        if (row.project.dirty)
                            return Kirigami.Theme.neutralTextColor;
                        return Kirigami.Theme.disabledTextColor;
                    }
                    elide: Text.ElideRight
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: row.describe()
                }

                Kirigami.Icon {
                    Controls.ToolTip.text: row.project.worktree
                        ? qsTr("Worktree of %1").arg(row.project.parentName)
                        : qsTr("Managed clone")
                    Controls.ToolTip.visible: sourceHover.hovered
                    Layout.preferredHeight: Kirigami.Units.iconSizes.small * 0.7
                    Layout.preferredWidth: Kirigami.Units.iconSizes.small * 0.7
                    opacity: 0.7
                    source: row.project.worktree ? "vcs-branch" : "folder-download"
                    visible: row.project.managed || row.project.worktree

                    HoverHandler {
                        id: sourceHover
                    }
                }

                Kirigami.Icon {
                    Layout.preferredHeight: Kirigami.Units.iconSizes.small * 0.7
                    Layout.preferredWidth: Kirigami.Units.iconSizes.small * 0.7
                    source: "dialog-warning"
                    visible: !row.project.available
                }
            }
        }
    }
}
