import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Explicit, state-bound project checks. Merely opening or refreshing this
/// panel only reads the run store; commands start from the Run button.
Item {
    id: panel

    required property var backend
    required property var project

    readonly property var backendChecks: backend && backend.checks !== undefined
        ? backend.checks : ({})
    readonly property bool stateReady: String(backendChecks.projectId || "") === String(project.id)
    readonly property var configured: stateReady && backendChecks.configured !== undefined
        ? backendChecks.configured : []
    readonly property var results: stateReady && backendChecks.results !== undefined
        ? backendChecks.results : []
    readonly property bool loading: stateReady && backendChecks.loading === true
    readonly property string loadError: stateReady ? String(backendChecks.error || "") : ""
    readonly property int failureCount: {
        let count = 0;
        for (let index = 0; index < results.length; ++index) {
            const outcome = String(results[index].outcome || "");
            if (outcome !== "passed" && outcome !== "running" && outcome !== "queued")
                ++count;
        }
        return count;
    }

    signal hideRequested()

    readonly property string viewId: "checks"
    readonly property string viewTitle: qsTr("Checks")
    readonly property string viewIcon: "tools-check-spelling"
    readonly property string viewShortcut: "Ctrl+Shift+C"
    readonly property int viewBadge: failureCount
    readonly property bool viewAvailable: project.available

    implicitWidth: Kirigami.Units.gridUnit * 32

    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    function refreshChecks() {
        if (viewAvailable && backend && typeof backend.refreshChecks === "function")
            backend.refreshChecks(project.id);
    }

    function latest(checkId) {
        for (let index = 0; index < results.length; ++index) {
            if (String(results[index].checkId) === String(checkId))
                return results[index];
        }
        return null;
    }

    // InlineMessage does not expose the text format of its internal label.
    // Escape backend failures so command output or paths render literally.
    function escapedRichText(value) {
        return "<span>" + String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;") + "</span>";
    }

    Component.onCompleted: refreshChecks()
    onProjectChanged: refreshChecks()

    Kirigami.Action {
        id: refreshAction
        enabled: !panel.loading
        icon.name: "view-refresh"
        text: qsTr("Refresh checks")
        onTriggered: panel.refreshChecks()
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        PanelHeader {
            title: panel.viewTitle
            actions: [refreshAction]
            onHideRequested: panel.hideRequested()
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            text: panel.escapedRichText(panel.loadError)
            type: Kirigami.MessageType.Error
            visible: panel.loadError.length > 0
        }

        Controls.BusyIndicator {
            Layout.alignment: Qt.AlignHCenter
            running: panel.loading
            visible: running
        }

        Controls.Label {
            Layout.alignment: Qt.AlignHCenter
            Layout.margins: Kirigami.Units.largeSpacing
            color: Kirigami.Theme.disabledTextColor
            text: qsTr("No checks are configured for this project.")
            textFormat: Text.PlainText
            visible: panel.stateReady && !panel.loading && panel.configured.length === 0
        }

        Controls.ScrollView {
            Layout.fillHeight: true
            Layout.fillWidth: true
            clip: true

            Column {
                width: parent.width

                Repeater {
                    model: panel.configured

                    delegate: Controls.Frame {
                        id: checkRow
                        required property var modelData

                        readonly property var result: panel.latest(modelData.id)
                        width: parent.width
                        background: Rectangle {
                            color: index % 2 === 0
                                ? Kirigami.Theme.backgroundColor
                                : Kirigami.Theme.alternateBackgroundColor
                        }

                        ColumnLayout {
                            anchors.fill: parent
                            spacing: Kirigami.Units.smallSpacing

                            RowLayout {
                                Layout.fillWidth: true

                                Kirigami.Icon {
                                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                    source: checkRow.result === null
                                        ? "question"
                                        : String(checkRow.result.outcome) === "passed"
                                            ? "emblem-success"
                                            : "data-error"
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    elide: Text.ElideRight
                                    font.bold: true
                                    text: String(checkRow.modelData.label || checkRow.modelData.id)
                                    textFormat: Text.PlainText
                                }

                                Controls.Button {
                                    text: qsTr("Run")
                                    onClicked: {
                                        confirmPrompt.checkId = String(checkRow.modelData.id);
                                        confirmPrompt.open();
                                    }
                                }
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                elide: Text.ElideMiddle
                                font.family: "monospace"
                                text: (checkRow.modelData.command || []).join(" ")
                                textFormat: Text.PlainText
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: checkRow.result !== null
                                    && String(checkRow.result.freshness) === "stale"
                                    ? Kirigami.Theme.negativeTextColor
                                    : Kirigami.Theme.disabledTextColor
                                text: checkRow.result === null
                                    ? qsTr("Never run")
                                    : qsTr("%1 · %2 · %3 ms")
                                        .arg(String(checkRow.result.outcome))
                                        .arg(String(checkRow.result.freshness))
                                        .arg(Number(checkRow.result.durationMs || 0))
                                textFormat: Text.PlainText
                            }

                            Repeater {
                                model: checkRow.result !== null
                                    ? checkRow.result.diagnostics || [] : []

                                delegate: Controls.Label {
                                    required property var modelData
                                    Layout.fillWidth: true
                                    color: String(modelData.level) === "error"
                                        ? Kirigami.Theme.negativeTextColor
                                        : Kirigami.Theme.textColor
                                    font.family: "monospace"
                                    text: (String(modelData.path || "").length > 0
                                            ? String(modelData.path) + ":" + Number(modelData.line || 0) + ": "
                                            : "") + String(modelData.message || "")
                                    textFormat: Text.PlainText
                                    wrapMode: Text.Wrap
                                }
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                font.family: "monospace"
                                text: checkRow.result !== null
                                    ? String(checkRow.result.stderrTail || checkRow.result.stdoutTail || "")
                                    : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }
                        }
                    }
                }
            }
        }
    }

    Kirigami.PromptDialog {
        id: confirmPrompt

        property string checkId: ""

        background: FloatingSurface {}
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Trust and run this project check?")
        subtitle: qsTr("Review the exact command shown in the Checks panel. Harkness will trust this project identity and path, then record the result and pre-execution workspace identity.")
        onAccepted: panel.backend.runCheck(panel.project.id, checkId, true)
        onOpened: {
            const cancel = standardButton(Kirigami.Dialog.Cancel);
            if (cancel)
                cancel.forceActiveFocus();
        }
    }
}
