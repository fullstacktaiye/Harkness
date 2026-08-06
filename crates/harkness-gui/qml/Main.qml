import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    height: 300
    title: qsTr("Harkness")
    visible: true
    width: 400

    HarknessBackend {
        id: backend
    }

    Controls.Label {
        anchors.centerIn: parent
        text: backend.greeting
    }
}
