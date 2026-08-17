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

    function quote(value) {
        return JSON.stringify(String(value));
    }

    function argvPreview(command) {
        const parts = command || [];
        const quoted = [];
        for (let index = 0; index < parts.length; ++index)
            quoted.push(quote(parts[index]));
        return "[" + quoted.join(", ") + "]";
    }

    function environmentPreview(environment) {
        const rows = environment || [];
        const projected = [];
        for (let index = 0; index < rows.length; ++index) {
            projected.push({
                "name": String(rows[index].name || ""),
                "value": String(rows[index].value || "")
            });
        }
        projected.sort(function(left, right) {
            return left.name < right.name ? -1 : left.name > right.name ? 1 : 0;
        });
        const entries = [];
        for (let index = 0; index < projected.length; ++index) {
            entries.push(quote(projected[index].name) + ": "
                + quote(projected[index].value));
        }
        return "{" + entries.join(", ") + "}";
    }

    function format(template, values) {
        return String(template).replace(/%(\d)/g, function(marker, index) {
            const value = values[Number(index) - 1];
            return value === undefined ? marker : String(value);
        });
    }

    function invocationPreview(configuration) {
        if (!configuration)
            return qsTr("No configured invocation");
        const timeout = Number(configuration.timeoutSeconds || 0);
        return format(qsTr("argv: %1\ncwd: %2\nenv: %3\ntimeout: %4\nparser: %5"), [
            argvPreview(configuration.command || []),
            String(configuration.cwd || "."),
            environmentPreview(configuration.environment || []),
            timeout > 0 ? qsTr("%1 seconds").arg(timeout) : qsTr("default"),
            String(configuration.parser || "plain")
        ]);
    }

    function recordedInvocation(result) {
        if (!result)
            return "";
        return invocationPreview({
            "command": result.recordedCommand || [],
            "cwd": result.recordedCwd || "",
            "environment": result.recordedEnvironment || [],
            "timeoutSeconds": result.recordedTimeoutSeconds || 0,
            "parser": result.recordedParser || "plain"
        });
    }

    function invocationMatches(configuration, result) {
        return result !== null
            && invocationPreview(configuration) === recordedInvocation(result);
    }

    function configuredCheck(checkId) {
        for (let index = 0; index < configured.length; ++index) {
            if (String(configured[index].id) === String(checkId))
                return configured[index];
        }
        return null;
    }

    function stateReference(result) {
        if (!result)
            return qsTr("No recorded workspace state");
        const head = String(result.stateHead || "");
        const digest = String(result.stateDigest || "");
        const clean = result.workspaceCleanKnown === true
            ? (result.workspaceClean === true ? qsTr("clean") : qsTr("dirty"))
            : qsTr("cleanliness unknown");
        const index = result.workspaceMatchesIndexKnown === true
            ? (result.workspaceMatchesIndex === true
                ? qsTr("matches index") : qsTr("differs from index"))
            : qsTr("index relation unknown");
        return format(qsTr("state: HEAD %1 · digest %2 · %3 · %4"), [
            head.length > 0 ? head : qsTr("unborn"),
            digest.length > 0 ? digest : qsTr("unavailable"),
            clean,
            index
        ]);
    }

    // InlineMessage does not expose the text format of its internal label.
    // Escape backend failures so command output or paths render literally.
    function escapedRichText(value) {
        return "<span>" + String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;") + "</span>";
    }

    function escapedRichMultiline(value) {
        return escapedRichText(value).replace(/\n/g, "<br>");
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
                        required property int index
                        required property var modelData

                        readonly property var result: panel.latest(modelData.id)
                        readonly property bool invocationChanged: result !== null
                            && (result.definitionCurrent !== true
                                || !panel.invocationMatches(modelData, result))
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
                                font.family: "monospace"
                                text: panel.invocationPreview(checkRow.modelData)
                                textFormat: Text.PlainText
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.negativeTextColor
                                text: checkRow.result !== null
                                    && (checkRow.result.stdoutArtifactTruncated === true
                                        || checkRow.result.stderrArtifactTruncated === true)
                                    ? panel.format(qsTr("Stored output truncated at the %1-byte per-stream limit: stdout %2, stderr %3"), [
                                        Number(checkRow.result.artifactByteLimit || 0),
                                        checkRow.result.stdoutArtifactTruncated === true
                                            ? qsTr("truncated") : qsTr("complete"),
                                        checkRow.result.stderrArtifactTruncated === true
                                            ? qsTr("truncated") : qsTr("complete")
                                    ]) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: checkRow.result !== null
                                    && (String(checkRow.result.freshness) !== "current"
                                        || checkRow.invocationChanged)
                                    ? Kirigami.Theme.negativeTextColor
                                    : Kirigami.Theme.disabledTextColor
                                text: checkRow.result === null
                                    ? qsTr("Never run")
                                    : panel.format(qsTr("%1 · %2 · %3 ms"), [
                                        String(checkRow.result.outcome),
                                        String(checkRow.result.freshness),
                                        Number(checkRow.result.durationMs || 0)
                                    ])
                                textFormat: Text.PlainText
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                font.family: "monospace"
                                text: checkRow.result !== null
                                    ? panel.stateReference(checkRow.result) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                text: checkRow.result !== null
                                    ? panel.format(qsTr("recorded %1 · evidence %2"), [
                                        String(checkRow.result.createdAt || qsTr("unknown time")),
                                        String(checkRow.result.evidenceClass || "unobserved")
                                    ]) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.negativeTextColor
                                text: checkRow.result !== null
                                    ? String(checkRow.result.freshnessDetail || "") : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.negativeTextColor
                                font.bold: true
                                text: checkRow.invocationChanged
                                    ? qsTr("Recorded invocation differs from the current configuration:")
                                        + "\n" + panel.recordedInvocation(checkRow.result)
                                    : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
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
                                color: Kirigami.Theme.disabledTextColor
                                text: checkRow.result !== null
                                    && (Number(checkRow.result.diagnosticsOmitted || 0) > 0
                                        || checkRow.result.diagnosticsScanTruncated === true)
                                    ? panel.format(qsTr("Diagnostics truncated: %1 omitted%2"), [
                                        Number(checkRow.result.diagnosticsOmitted || 0),
                                        checkRow.result.diagnosticsScanTruncated === true
                                            ? qsTr("; diagnostic source was truncated") : ""
                                    ]) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                text: checkRow.result !== null
                                    && (String(checkRow.result.stdoutTail || "").length > 0
                                        || checkRow.result.stdoutTruncated === true)
                                    ? (checkRow.result.stdoutTruncated === true
                                        ? qsTr("Standard output tail (truncated)")
                                        : qsTr("Standard output")) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                font.family: "monospace"
                                text: checkRow.result !== null
                                    ? String(checkRow.result.stdoutTail || "")
                                    : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: Kirigami.Theme.disabledTextColor
                                text: checkRow.result !== null
                                    && (String(checkRow.result.stderrTail || "").length > 0
                                        || checkRow.result.stderrTruncated === true)
                                    ? (checkRow.result.stderrTruncated === true
                                        ? qsTr("Standard error tail (truncated)")
                                        : qsTr("Standard error")) : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                font.family: "monospace"
                                text: checkRow.result !== null
                                    ? String(checkRow.result.stderrTail || "") : ""
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
        readonly property var configuration: panel.configuredCheck(checkId)

        background: FloatingSurface {}
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        title: qsTr("Trust and run this project check?")
        subtitle: panel.escapedRichMultiline(
            qsTr("Review the complete argv-only invocation below. Harkness will trust this project identity and path, then record the result and pre-execution workspace identity.")
                + "\n\n" + panel.invocationPreview(configuration)
        )
        onAccepted: {
            if (configuration !== null)
                panel.backend.runCheck(panel.project.id, checkId, true);
        }
        onOpened: {
            const cancel = standardButton(Kirigami.Dialog.Cancel);
            if (cancel)
                cancel.forceActiveFocus();
            const accept = standardButton(Kirigami.Dialog.Ok);
            if (accept)
                accept.enabled = configuration !== null;
        }
    }
}
