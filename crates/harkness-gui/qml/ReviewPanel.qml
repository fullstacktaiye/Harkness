import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Item {
    id: panel

    required property var backend
    required property var project

    readonly property bool stateReady: backend.git
        && backend.git.projectId !== undefined
        && String(backend.git.projectId) === String(project.id)
    readonly property var gitState: stateReady ? backend.git : ({})

    implicitWidth: Kirigami.Units.gridUnit * 34

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
    }

    Controls.ScrollView {
        id: scroll

        anchors.fill: parent
        clip: true
        contentWidth: availableWidth

        ReviewSurface {
            width: scroll.availableWidth
                - Kirigami.Units.largeSpacing * 2
            x: Kirigami.Units.largeSpacing
            backend: panel.backend
            gitState: panel.gitState
            project: panel.project
            stateReady: panel.stateReady
        }
    }
}
