import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    height: 420
    title: qsTr("Harkness")
    visible: true
    width: 560

    HarknessBackend {
        id: backend
    }

    Kirigami.PromptDialog {
        id: removeDialog
        title: qsTr("Delete managed repository?")
        subtitle: qsTr("This permanently deletes the checkout at:\n%1").arg(backend.managedPath)
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: backend.removeManaged(backend.managedProjectId)
    }

    Column {
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Heading {
            text: qsTr("Import a GitHub repository")
        }

        Controls.TextField {
            id: remoteField
            enabled: !backend.busy
            placeholderText: qsTr("https://github.com/owner/repository.git")
            width: parent.width
            onAccepted: backend.importRepository(text)
        }

        Row {
            spacing: Kirigami.Units.smallSpacing

            Controls.Button {
                enabled: !backend.busy && remoteField.text.length > 0
                text: qsTr("Clone")
                onClicked: backend.importRepository(remoteField.text)
            }

            Controls.Button {
                enabled: backend.busy
                text: qsTr("Cancel")
                onClicked: backend.cancelImport()
            }
        }

        Controls.BusyIndicator {
            running: backend.busy
            visible: running
        }

        Controls.Label {
            text: backend.status
            width: parent.width
            wrapMode: Text.Wrap
        }

        Controls.Label {
            text: backend.managedPath
            visible: text.length > 0
            width: parent.width
            wrapMode: Text.WrapAnywhere
        }

        Controls.Button {
            enabled: !backend.busy
            text: qsTr("Remove managed clone…")
            visible: backend.managedProjectId.length > 0
            onClicked: removeDialog.open()
        }
    }
}
