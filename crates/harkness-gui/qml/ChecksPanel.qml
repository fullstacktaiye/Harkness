import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// Explicit, state-bound project checks: what is configured to run on the left,
/// what the last run of the selected one recorded on the right.
///
/// The split is the point. A check asks two different questions — "what exactly
/// would this run?" and "what did it find, and does that still describe the
/// workspace?" — and stacking the second under the first put every answer for
/// every check into one column, so the output being read was separated from its
/// own heading by the whole of the check above it. Naming one check on the left
/// gives its evidence the width to be read.
///
/// Opening, selecting and refreshing read the run store only. A command starts
/// from a Run action and the confirmation behind it, never from navigation.
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

    /// One pass over the configured checks, so the summary line, the badge and
    /// the automatic selection all describe the same tally.
    readonly property var tally: {
        let failing = 0;
        let passing = 0;
        let stale = 0;
        let unverifiable = 0;
        let neverRun = 0;
        for (let index = 0; index < configured.length; ++index) {
            const result = latest(configured[index].id);
            if (result === null) {
                ++neverRun;
                continue;
            }
            const outcome = String(result.outcome || "");
            if (outcomePending(outcome))
                continue;
            if (outcome === "passed")
                ++passing;
            else
                ++failing;
            const freshness = String(result.freshness || "");
            if (freshness === "stale")
                ++stale;
            else if (freshness !== "current")
                ++unverifiable;
        }
        return ({
            "failing": failing,
            "passing": passing,
            "stale": stale,
            "unverifiable": unverifiable,
            "neverRun": neverRun
        });
    }
    readonly property int failureCount: tally.failing

    /// The check whose evidence the right-hand pane is showing.
    property string selectedCheckId: ""
    /// Which evidence tab that pane is on: output, problems or invocation. It
    /// stays put across a change of check, because a reader comparing two
    /// checks is comparing the same kind of evidence.
    property int evidenceTab: 0
    /// The check this window asked to run, so the live band can name it. Only
    /// one check runs at a time per repository, which is what makes a single
    /// identifier enough.
    property string runRequestedCheckId: ""
    property double runStartedMs: 0
    property int runElapsedSeconds: 0

    signal hideRequested()

    // Side-panel view contract; see SidePanel.qml.
    readonly property string viewId: "checks"
    readonly property string viewTitle: qsTr("Checks")
    /// A gear turning on a play button: a configured command that is run to
    /// reach a verdict. It also has to stay apart from its neighbours in the
    /// bar at icon size, which a second tick-in-a-box beside `view-task` would
    /// not, and it says nothing about spelling, which the previous
    /// `tools-check-spelling` said loudly.
    readonly property string viewIcon: "run-build"
    readonly property string viewShortcut: "Ctrl+Shift+C"
    readonly property int viewBadge: failureCount
    /// The badge counts checks that did not pass, so it is not the neutral
    /// accent the issue count uses.
    readonly property color viewBadgeColor: negativeColor
    readonly property bool viewAvailable: project.available

    implicitWidth: Kirigami.Units.gridUnit * 56

    // This is shell chrome rather than a document following an editor palette,
    // so the dark surface is restated the way ProjectShellPage states it.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    /// The palette, read once here and referred to as `panel.*` everywhere
    /// below. A control carries the widget style's own colour set with theme
    /// inheritance off — the reason FieldSurface.qml states its colours — so
    /// `Kirigami.Theme.negativeTextColor` read from inside a button answers
    /// with the desktop scheme's red rather than this window's.
    readonly property color accentColor: Kirigami.Theme.highlightColor
    readonly property color positiveColor: Kirigami.Theme.positiveTextColor
    readonly property color negativeColor: Kirigami.Theme.negativeTextColor
    readonly property color neutralColor: Kirigami.Theme.neutralTextColor
    readonly property color dimColor: Kirigami.Theme.disabledTextColor
    readonly property color bodyColor: Kirigami.Theme.textColor
    readonly property color frameColor: Qt.alpha(Kirigami.Theme.textColor, 0.15)

    /// Shared job projection: the same one the shell header and the
    /// source-control view read, so a running check and a running commit
    /// disable each other's actions the way the backend already refuses them.
    GitActivity {
        id: checkActivity

        backend: panel.backend
        project: panel.project
    }

    readonly property var checkJob: checkActivity.job("check")
    readonly property bool checkRunning: checkJob !== null
    /// The set of jobs `jobs_conflict` refuses a check alongside: another
    /// check, any repository mutation — the path-scoped discards included, which
    /// is why `pathMutationRunning` is asked separately — and any review read.
    ///
    /// It has to be that set and not a smaller one. Anything left out leaves the
    /// Run action enabled, and pressing it reaches the backend only to be
    /// refused with a status line, which is the outcome this guard exists to
    /// spare the reader.
    readonly property bool runBlocked: checkRunning
        || checkActivity.repositoryMutationRunning()
        || checkActivity.pathMutationRunning()
        || checkActivity.reviewReadRunning()

    function refreshChecks() {
        if (viewAvailable && backend && typeof backend.refreshChecks === "function")
            backend.refreshChecks(project.id);
    }

    /// What this panel reloads for; see GitPanel.qml's own key. The catalog
    /// rewrites the project map's branch and dirty fields after every mutation,
    /// so reacting to the map itself spent a run-store read on every commit.
    readonly property string reloadKey: [
        project && project.id !== undefined ? String(project.id) : "",
        project.available ? "1" : "0"
    ].join("/")

    onReloadKeyChanged: refreshChecks()
    Component.onCompleted: {
        // The projection outlives this panel — a hot reload rebuilds the window
        // over state the backend already holds — so the selection is settled
        // here as well as on the change that normally brings it in.
        ensureSelection();
        refreshChecks();
    }

    function latest(checkId) {
        for (let index = 0; index < results.length; ++index) {
            if (String(results[index].checkId) === String(checkId))
                return results[index];
        }
        return null;
    }

    function configuredCheck(checkId) {
        for (let index = 0; index < configured.length; ++index) {
            if (String(configured[index].id) === String(checkId))
                return configured[index];
        }
        return null;
    }

    readonly property var selectedCheck: configuredCheck(selectedCheckId)
    readonly property var selectedResult: latest(selectedCheckId)

    /// Keeps a check named on the right whenever there is one to name. The
    /// first failing check leads, because a panel opened after a failure was
    /// opened to read that failure; nothing failing means the first check.
    function ensureSelection() {
        if (configuredCheck(selectedCheckId) !== null)
            return;
        for (let index = 0; index < configured.length; ++index) {
            const result = latest(configured[index].id);
            if (result !== null && outcomeFailed(String(result.outcome || ""))) {
                selectedCheckId = String(configured[index].id);
                return;
            }
        }
        selectedCheckId = configured.length > 0 ? String(configured[0].id) : "";
    }

    onConfiguredChanged: ensureSelection()

    // --- Outcome and freshness presentation -------------------------------

    function outcomePending(outcome) {
        return ["queued", "waiting_for_approval", "running"].indexOf(String(outcome)) !== -1;
    }

    /// A terminal outcome that is not a pass. Denied and cancelled belong here:
    /// the check was asked for and there is still no verdict.
    function outcomeFailed(outcome) {
        return !outcomePending(outcome) && String(outcome) !== "passed";
    }

    function outcomeLabel(outcome) {
        const value = String(outcome);
        if (value === "passed")
            return qsTr("Passed");
        if (value === "failed")
            return qsTr("Failed");
        if (value === "timed_out")
            return qsTr("Timed out");
        if (value === "denied")
            return qsTr("Denied");
        if (value === "cancelled")
            return qsTr("Cancelled");
        if (value === "interrupted")
            return qsTr("Interrupted");
        if (value === "queued")
            return qsTr("Queued");
        if (value === "waiting_for_approval")
            return qsTr("Waiting for approval");
        if (value === "running")
            return qsTr("Running");
        return value;
    }

    function freshnessLabel(freshness) {
        const value = String(freshness);
        if (value === "current")
            return qsTr("Current");
        if (value === "stale")
            return qsTr("Stale");
        return qsTr("Unverifiable");
    }

    /// The same word as it reads inside a sentence. Stated separately rather
    /// than lowercased from the label above: whether a word changes case when
    /// it stops leading a phrase is the translator's business, not
    /// `toLowerCase`'s — in German it would not.
    function freshnessWord(freshness) {
        const value = String(freshness);
        if (value === "current")
            return qsTr("current");
        if (value === "stale")
            return qsTr("stale");
        return qsTr("unverifiable");
    }

    function evidenceLabel(evidence) {
        const value = String(evidence);
        if (value === "harkness_observed")
            return qsTr("Harkness-observed");
        if (value === "harkness_mediated")
            return qsTr("Harkness-mediated");
        if (value === "acp_reported")
            return qsTr("ACP-reported");
        if (value === "snapshot_inferred")
            return qsTr("Snapshot-inferred");
        return qsTr("Unobserved");
    }

    /// A pass that no longer describes the workspace is not reported in the
    /// colour of a pass. That distinction is the whole subject of this view:
    /// the verdict is bound to the state it was taken against.
    function statusColor(result) {
        if (result === null)
            return dimColor;
        const outcome = String(result.outcome || "");
        if (outcomePending(outcome))
            return accentColor;
        if (outcome !== "passed")
            return negativeColor;
        return String(result.freshness || "") === "current" ? positiveColor : neutralColor;
    }

    function statusText(result) {
        if (result === null)
            return qsTr("Never run");
        return outcomeLabel(result.outcome);
    }

    function durationText(milliseconds) {
        const value = Number(milliseconds || 0);
        if (value <= 0)
            return "";
        if (value < 1000)
            return qsTr("%1 ms").arg(value);
        if (value < 60000)
            return qsTr("%1 s").arg((value / 1000).toFixed(1));
        const minutes = Math.floor(value / 60000);
        return qsTr("%1 min %2 s").arg(minutes).arg(Math.round((value % 60000) / 1000));
    }

    /// The one-line story of a check, for the list where only one line fits.
    function resultSummary(result) {
        if (result === null)
            return qsTr("Never run");
        const parts = [outcomeLabel(result.outcome)];
        if (String(result.freshness || "") !== "current")
            parts.push(freshnessWord(result.freshness));
        const duration = durationText(result.durationMs);
        if (duration.length > 0)
            parts.push(duration);
        return parts.join(" · ");
    }

    function lineCount(body) {
        const value = String(body || "");
        if (value.length === 0)
            return 0;
        const trailing = value.charAt(value.length - 1) === "\n";
        return value.split("\n").length - (trailing ? 1 : 0);
    }

    function streamNote(name, body, truncated) {
        const lines = lineCount(body);
        if (lines === 0 && !truncated)
            return "";
        const counted = lines === 1
            ? qsTr("%1: 1 line").arg(name)
            : qsTr("%1: %2 lines").arg(name).arg(lines);
        return truncated ? qsTr("%1, tail only").arg(counted) : counted;
    }

    function byteText(bytes) {
        const value = Number(bytes || 0);
        if (value <= 0)
            return "";
        if (value >= 1048576)
            return qsTr("%1 MiB").arg(Math.round(value / 1048576));
        if (value >= 1024)
            return qsTr("%1 KiB").arg(Math.round(value / 1024));
        return qsTr("%1 bytes").arg(value);
    }

    /// What is in the console, stated where scrolling cannot take it away. The
    /// console opens at the tail, so the stream headings inside it are the
    /// first thing to scroll out of sight.
    ///
    /// The stored-artifact limit belongs here rather than in a message above the
    /// pane: it says that output is missing, which only matters to somebody
    /// reading the output.
    function outputNote(result) {
        if (result === null)
            return "";
        const parts = [];
        const out = streamNote(qsTr("stdout"), result.stdoutTail,
            result.stdoutTruncated === true);
        const err = streamNote(qsTr("stderr"), result.stderrTail,
            result.stderrTruncated === true);
        if (out.length > 0)
            parts.push(out);
        if (err.length > 0)
            parts.push(err);
        if (result.stdoutArtifactTruncated === true
                || result.stderrArtifactTruncated === true) {
            parts.push(qsTr("stored output cut at the %1 per-stream limit")
                .arg(byteText(result.artifactByteLimit)));
        }
        return parts.join(" · ");
    }

    function problemsTabText() {
        const count = selectedResult !== null
            ? (selectedResult.diagnostics || []).length
            : 0;
        return count > 0 ? qsTr("Problems (%1)").arg(count) : qsTr("Problems");
    }

    function summaryText() {
        if (configured.length === 0)
            return "";
        const parts = [configured.length === 1
            ? qsTr("1 check")
            : qsTr("%1 checks").arg(configured.length)];
        if (tally.failing > 0)
            parts.push(qsTr("%1 not passing").arg(tally.failing));
        if (tally.stale > 0)
            parts.push(qsTr("%1 stale").arg(tally.stale));
        if (tally.unverifiable > 0)
            parts.push(qsTr("%1 unverifiable").arg(tally.unverifiable));
        if (tally.neverRun > 0)
            parts.push(qsTr("%1 never run").arg(tally.neverRun));
        return parts.join(" · ");
    }

    // --- Invocation and recorded state ------------------------------------

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

    /// Whether the recorded evidence was produced by something other than what
    /// the project now configures, which makes the verdict about a command that
    /// would no longer be run.
    function invocationDrifted(configuration, result) {
        return result !== null
            && (result.definitionCurrent !== true
                || !invocationMatches(configuration, result));
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
        return format(qsTr("HEAD %1 · digest %2 · %3 · %4"), [
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

    // --- Running a check ---------------------------------------------------

    function requestRun(checkId) {
        confirmPrompt.checkId = String(checkId);
        confirmPrompt.open();
    }

    function cancelRun() {
        if (checkJob !== null && checkJob.cancellable === true)
            backend.cancelJob(checkJob.id);
    }

    /// Names the check being run when this window is the one that asked. A
    /// sibling worktree's check occupies the same scheduling domain and is
    /// reported as running here without a name to attach it to.
    function runningCheckLabel() {
        const configuration = configuredCheck(runRequestedCheckId);
        return configuration === null
            ? ""
            : String(configuration.label || configuration.id);
    }

    onCheckRunningChanged: {
        if (checkRunning) {
            runStartedMs = Date.now();
            runElapsedSeconds = 0;
        } else {
            runRequestedCheckId = "";
        }
    }

    // Elapsed time counts from when this window saw the job appear, which is
    // the only start it can observe; the recorded duration on the result is
    // the measured one.
    Timer {
        interval: 1000
        repeat: true
        running: panel.checkRunning
        onTriggered: panel.runElapsedSeconds =
            Math.floor((Date.now() - panel.runStartedMs) / 1000)
    }

    Kirigami.Action {
        id: refreshAction

        enabled: !panel.loading
        icon.name: "view-refresh"
        text: qsTr("Refresh checks")
        tooltip: qsTr("Re-read recorded results")
        onTriggered: panel.refreshChecks()
    }

    // --- Reusable pieces ---------------------------------------------------

    /// The state indicator, drawn rather than themed.
    ///
    /// A themed icon under `isMask` is only as good as the active icon theme's
    /// alpha channel: several widely installed themes ship opaque status icons,
    /// and masking one of those paints a solid block where a glyph should be.
    /// A ring with a core is two rectangles, reads at list size, and looks the
    /// same on every desktop. Colour is never the only carrier — the state is
    /// spelled out in words on the line beside it.
    component StatusDot: Rectangle {
        required property color dotColor
        /// Whether a verdict was actually reached. A ring on its own is the
        /// absence of one.
        required property bool settled

        border.color: dotColor
        border.width: Math.max(1, Math.round(Kirigami.Units.smallSpacing / 3))
        color: settled ? Qt.alpha(dotColor, 0.25) : "transparent"
        implicitHeight: Kirigami.Units.iconSizes.small
        implicitWidth: implicitHeight
        radius: implicitHeight / 2

        Rectangle {
            anchors.centerIn: parent
            color: parent.dotColor
            height: Math.round(parent.height / 2)
            radius: height / 2
            visible: parent.settled
            width: height
        }
    }

    /// A short state word on its own ground: outcome, freshness, evidence.
    component StatusPill: Rectangle {
        required property color pillColor
        property alias text: pillLabel.text

        border.color: Qt.alpha(pillColor, 0.85)
        border.width: 1
        color: Qt.alpha(pillColor, 0.16)
        implicitHeight: pillLabel.implicitHeight + Kirigami.Units.smallSpacing
        implicitWidth: pillLabel.implicitWidth + Kirigami.Units.largeSpacing
        radius: implicitHeight / 2
        visible: pillLabel.text.length > 0

        Controls.Label {
            id: pillLabel

            anchors.centerIn: parent
            color: parent.pillColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            textFormat: Text.PlainText
        }
    }

    /// One `name: value` line of the recorded-run summary. The value is
    /// repository or command content, so it is inert plain text throughout.
    component MetaRow: RowLayout {
        required property string name
        required property string value
        property bool monospace: false

        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing
        visible: value.length > 0

        Controls.Label {
            Layout.alignment: Qt.AlignTop
            Layout.preferredWidth: Kirigami.Units.gridUnit * 5
            color: panel.dimColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: parent.name
            textFormat: Text.PlainText
        }

        Controls.Label {
            Layout.fillWidth: true
            color: panel.bodyColor
            font.family: parent.monospace ? "monospace" : Kirigami.Theme.defaultFont.family
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: parent.value
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
        }
    }

    /// One tab of the evidence pane, marked along its bottom edge the way the
    /// activity bar marks the current view.
    ///
    /// A `TabBar` is deliberately not used. It divides its width between its
    /// buttons, so across a pane this wide three short labels drift apart until
    /// they stop reading as a group — and constraining its width instead
    /// collapses the buttons to nothing, because they are sized *from* the bar.
    /// A row of buttons that each size to their own label has neither problem,
    /// and it keeps the ground and the mark out of the widget style's hands.
    component EvidenceTab: Controls.AbstractButton {
        id: tab

        required property bool selected

        implicitHeight: tabLabel.implicitHeight + Kirigami.Units.largeSpacing
        implicitWidth: tabLabel.implicitWidth + Kirigami.Units.gridUnit * 1.5
        hoverEnabled: true

        Accessible.name: tab.text

        background: Rectangle {
            color: tab.hovered && !tab.selected
                ? Qt.rgba(1, 1, 1, 0.06)
                : "transparent"

            Rectangle {
                anchors.bottom: parent.bottom
                anchors.left: parent.left
                anchors.right: parent.right
                color: panel.accentColor
                height: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
                visible: tab.selected
            }
        }

        contentItem: Controls.Label {
            id: tabLabel

            color: tab.selected ? panel.bodyColor : panel.dimColor
            font.weight: tab.selected ? Font.DemiBold : Font.Normal
            horizontalAlignment: Text.AlignHCenter
            text: tab.text
            textFormat: Text.PlainText
            verticalAlignment: Text.AlignVCenter
        }
    }

    /// A button on this panel's own ground.
    ///
    /// The widget style fills a button from the desktop colour scheme, which
    /// leaves a light grey box on this black — the same problem FieldSurface
    /// solves for text fields. `accent` marks the one action the pane is for.
    /// Every one of these is text-only on purpose: an icon here would be
    /// whatever the active icon theme happens to ship under that name, and the
    /// panel already declines to trust that for its status glyphs.
    component PanelButton: Controls.Button {
        id: button

        property bool accent: false

        hoverEnabled: true

        background: Rectangle {
            border.color: !button.enabled
                ? panel.frameColor
                : button.accent
                    ? Qt.alpha(panel.accentColor, 0.8)
                    : Qt.alpha(Kirigami.Theme.textColor, 0.3)
            border.width: 1
            color: !button.enabled
                ? "transparent"
                : button.accent
                    ? Qt.alpha(panel.accentColor, button.hovered ? 0.34 : 0.22)
                    : button.hovered
                        ? Qt.rgba(1, 1, 1, 0.1)
                        : Qt.rgba(1, 1, 1, 0.04)
            radius: Kirigami.Units.smallSpacing
        }

        contentItem: Controls.Label {
            color: button.enabled ? panel.bodyColor : panel.dimColor
            horizontalAlignment: Text.AlignHCenter
            text: button.text
            textFormat: Text.PlainText
            verticalAlignment: Text.AlignVCenter
        }
    }

    /// One recorded stream, headed by what it is and whether it is all of it.
    ///
    /// A tail is what the store keeps, so the newest bytes are at the bottom
    /// and the top is wherever the byte limit fell. The console scrolls to the
    /// end when the text changes for that reason.
    component OutputStream: ColumnLayout {
        id: stream

        required property string title
        required property string body
        required property bool truncated

        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing / 2
        visible: body.length > 0 || truncated

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            Rectangle {
                Layout.preferredHeight: 1
                Layout.preferredWidth: Kirigami.Units.gridUnit
                color: panel.frameColor
            }

            Controls.Label {
                color: panel.dimColor
                font.capitalization: Font.AllUppercase
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: stream.truncated
                    ? qsTr("%1 · tail only").arg(stream.title)
                    : stream.title
                textFormat: Text.PlainText
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: 1
                color: panel.frameColor
            }
        }

        Controls.TextArea {
            Layout.fillWidth: true
            // Command output is untrusted text that has to stay selectable so
            // a failure can be copied out; a read-only text area is the only
            // control that offers both.
            background: Rectangle {
                color: "transparent"
            }
            color: panel.bodyColor
            font.family: "monospace"
            padding: 0
            readOnly: true
            selectByMouse: true
            text: stream.body
            textFormat: TextEdit.PlainText
            // Word boundaries where there are any, mid-token only where a path
            // or a type name leaves no choice. Wrapping everything anywhere
            // broke every long path in the middle of a directory name.
            wrapMode: TextEdit.Wrap
        }
    }

    // --- The view ----------------------------------------------------------

    Controls.SplitView {
        anchors.fill: parent
        orientation: Qt.Horizontal

        handle: Rectangle {
            readonly property bool active: Controls.SplitHandle.hovered
                || Controls.SplitHandle.pressed

            color: active ? panel.accentColor : "transparent"
            implicitWidth: Kirigami.Units.smallSpacing

            Kirigami.Separator {
                anchors.horizontalCenter: parent.horizontalCenter
                height: parent.height
                visible: !parent.active
            }
        }

        Item {
            objectName: "checkListColumn"

            Controls.SplitView.fillWidth: false
            Controls.SplitView.maximumWidth: Kirigami.Units.gridUnit * 34
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 16
            Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 24

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.alternateBackgroundColor
            }

            ColumnLayout {
                anchors.fill: parent
                spacing: 0

                PanelHeader {
                    Layout.fillWidth: true
                    actions: [refreshAction]
                    title: panel.viewTitle
                    onHideRequested: panel.hideRequested()
                }

                RowLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing
                    visible: panel.summaryText().length > 0

                    Controls.Label {
                        Layout.fillWidth: true
                        color: panel.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        // Wrapped rather than elided: with four states to
                        // report this line outgrows a narrow column, and the
                        // count that gets cut off is the one nobody asked for.
                        text: panel.summaryText()
                        textFormat: Text.PlainText
                        wrapMode: Text.WordWrap
                    }

                    Controls.BusyIndicator {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: panel.loading
                        visible: running
                    }
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    text: panel.escapedRichText(panel.loadError)
                    type: Kirigami.MessageType.Error
                    visible: panel.loadError.length > 0
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                }

                ListView {
                    id: checkList

                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    clip: true
                    model: panel.configured
                    reuseItems: true

                    delegate: Controls.ItemDelegate {
                        id: checkRow

                        required property var modelData

                        readonly property string checkId: String(modelData.id)
                        readonly property var result: panel.latest(checkId)
                        readonly property bool selected: panel.selectedCheckId === checkId
                        readonly property bool running: panel.checkRunning
                            && panel.runRequestedCheckId === checkId

                        width: ListView.view.width
                        implicitHeight: rowContent.implicitHeight
                            + Kirigami.Units.smallSpacing * 2
                        hoverEnabled: true
                        leftPadding: Kirigami.Units.largeSpacing
                        rightPadding: Kirigami.Units.smallSpacing
                        Accessible.name: qsTr("%1: %2")
                            .arg(String(modelData.label || checkId))
                            .arg(panel.resultSummary(result))
                        onClicked: panel.selectedCheckId = checkId

                        background: Rectangle {
                            color: checkRow.selected
                                ? Qt.rgba(1, 1, 1, 0.08)
                                : checkRow.hovered
                                    ? Qt.rgba(1, 1, 1, 0.045)
                                    : "transparent"

                            Rectangle {
                                anchors.bottom: parent.bottom
                                anchors.left: parent.left
                                anchors.top: parent.top
                                color: panel.accentColor
                                visible: checkRow.selected
                                width: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
                            }
                        }

                        contentItem: RowLayout {
                            id: rowContent

                            spacing: Kirigami.Units.smallSpacing

                            StatusDot {
                                Layout.alignment: Qt.AlignVCenter
                                dotColor: panel.statusColor(checkRow.result)
                                settled: checkRow.result !== null
                                    && !panel.outcomePending(
                                        String(checkRow.result.outcome || ""))
                                visible: !checkRow.running
                            }

                            Controls.BusyIndicator {
                                Layout.alignment: Qt.AlignVCenter
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                running: checkRow.running
                                visible: checkRow.running
                            }

                            ColumnLayout {
                                Layout.fillWidth: true
                                spacing: 0

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: panel.bodyColor
                                    elide: Text.ElideRight
                                    font.weight: checkRow.selected ? Font.DemiBold : Font.Normal
                                    text: String(checkRow.modelData.label || checkRow.checkId)
                                    textFormat: Text.PlainText
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: checkRow.running
                                        ? panel.accentColor
                                        : panel.statusColor(checkRow.result)
                                    elide: Text.ElideRight
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    text: checkRow.running
                                        ? qsTr("Running…")
                                        : panel.resultSummary(checkRow.result)
                                    textFormat: Text.PlainText
                                }
                            }

                            PanelButton {
                                Layout.alignment: Qt.AlignVCenter
                                Controls.ToolTip.text: qsTr("Review and run this check")
                                Controls.ToolTip.visible: hovered
                                Accessible.name: qsTr("Run %1")
                                    .arg(String(checkRow.modelData.label || checkRow.checkId))
                                // Kept out of the way until the row is being
                                // read: a column of identical Run buttons is
                                // harder to scan than the states beside them.
                                opacity: checkRow.hovered || checkRow.selected ? 1 : 0
                                text: qsTr("Run")
                                visible: !panel.runBlocked
                                onClicked: {
                                    panel.selectedCheckId = checkRow.checkId;
                                    panel.requestRun(checkRow.checkId);
                                }
                            }
                        }
                    }
                }

                Controls.Label {
                    Layout.alignment: Qt.AlignHCenter
                    Layout.margins: Kirigami.Units.largeSpacing
                    color: panel.dimColor
                    horizontalAlignment: Text.AlignHCenter
                    text: qsTr("No checks are configured for this project.")
                    textFormat: Text.PlainText
                    visible: panel.stateReady && !panel.loading && panel.configured.length === 0
                    wrapMode: Text.WordWrap
                }
            }
        }

        Item {
            objectName: "checkResultSurface"

            Controls.SplitView.fillWidth: true
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 28

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.backgroundColor
            }

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                explanation: panel.configured.length === 0
                    ? qsTr("Configure project checks to record verdicts against the workspace state they were taken from.")
                    : qsTr("Choose a check to read what its last run recorded.")
                icon.name: "run-build"
                text: panel.configured.length === 0
                    ? qsTr("No checks configured")
                    : qsTr("No check selected")
                visible: panel.selectedCheck === null
                width: parent.width - Kirigami.Units.gridUnit * 4
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.bottomMargin: Kirigami.Units.largeSpacing
                anchors.leftMargin: Kirigami.Units.gridUnit
                anchors.rightMargin: Kirigami.Units.gridUnit
                anchors.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing
                visible: panel.selectedCheck !== null

                RowLayout {
                    id: resultHeaderRow

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Heading {
                        Layout.maximumWidth: resultHeaderRow.width / 2
                        elide: Text.ElideRight
                        level: 2
                        text: panel.selectedCheck !== null
                            ? String(panel.selectedCheck.label || panel.selectedCheck.id)
                            : ""
                        textFormat: Text.PlainText
                    }

                    StatusPill {
                        pillColor: panel.statusColor(panel.selectedResult)
                        text: panel.statusText(panel.selectedResult)
                    }

                    StatusPill {
                        pillColor: panel.selectedResult !== null
                            && String(panel.selectedResult.freshness || "") === "current"
                            ? panel.positiveColor
                            : panel.neutralColor
                        text: panel.selectedResult === null
                            ? ""
                            : panel.freshnessLabel(panel.selectedResult.freshness)
                    }

                    Item {
                        Layout.fillWidth: true
                    }

                    PanelButton {
                        Controls.ToolTip.text: qsTr("Stop the running check")
                        Controls.ToolTip.visible: hovered
                        text: qsTr("Cancel")
                        visible: panel.checkRunning
                            && panel.checkJob.cancellable === true
                        onClicked: panel.cancelRun()
                    }

                    PanelButton {
                        Controls.ToolTip.text: qsTr("Review the exact invocation, then run it")
                        Controls.ToolTip.visible: hovered
                        accent: true
                        enabled: !panel.runBlocked
                        text: panel.selectedResult === null
                            ? qsTr("Run check")
                            : qsTr("Run again")
                        visible: !panel.checkRunning
                        onClicked: panel.requestRun(panel.selectedCheckId)
                    }
                }

                // The live band. Only the job list can say a check is in flight
                // today: nothing is written to the run store until the command
                // finishes, so the output below is still the previous run's
                // until this band goes away.
                RowLayout {
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.smallSpacing / 2
                    spacing: Kirigami.Units.smallSpacing
                    visible: panel.checkRunning

                    Controls.BusyIndicator {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: panel.checkRunning
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: panel.accentColor
                        elide: Text.ElideRight
                        text: panel.runningCheckLabel().length > 0
                            ? panel.format(qsTr("Running %1 · %2 s elapsed · output is recorded when it finishes"), [
                                panel.runningCheckLabel(),
                                panel.runElapsedSeconds
                            ])
                            : qsTr("A check is running in this repository")
                        textFormat: Text.PlainText
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: panel.dimColor
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: qsTr("Waiting for the running repository operation to finish.")
                    textFormat: Text.PlainText
                    visible: panel.runBlocked && !panel.checkRunning
                    wrapMode: Text.WordWrap
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    text: panel.selectedResult === null
                        ? ""
                        : panel.escapedRichMultiline(panel.format(
                            qsTr("This verdict was recorded against a different workspace state (%1). %2"), [
                                panel.freshnessWord(panel.selectedResult.freshness),
                                String(panel.selectedResult.freshnessDetail || "")
                            ]))
                    type: Kirigami.MessageType.Warning
                    visible: panel.selectedResult !== null
                        && String(panel.selectedResult.freshness || "") !== "current"
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    text: panel.escapedRichText(qsTr("The recorded run used a different invocation from the one configured now. Compare them under Invocation."))
                    type: Kirigami.MessageType.Warning
                    visible: panel.invocationDrifted(panel.selectedCheck, panel.selectedResult)
                }

                RowLayout {
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    spacing: 0

                    EvidenceTab {
                        selected: panel.evidenceTab === 0
                        text: qsTr("Output")
                        onClicked: panel.evidenceTab = 0
                    }

                    EvidenceTab {
                        selected: panel.evidenceTab === 1
                        text: panel.problemsTabText()
                        onClicked: panel.evidenceTab = 1
                    }

                    EvidenceTab {
                        selected: panel.evidenceTab === 2
                        text: qsTr("Invocation")
                        onClicked: panel.evidenceTab = 2
                    }

                    Item {
                        Layout.fillWidth: true
                    }
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                }

                StackLayout {
                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    currentIndex: panel.evidenceTab

                    // Output. One console, both recorded streams, each headed
                    // by what it is. The frame is what makes it read as the
                    // record of a process rather than as more of the page.
                    Item {
                        Rectangle {
                            anchors.fill: parent
                            border.color: panel.frameColor
                            border.width: 1
                            color: "#000000"
                            radius: Kirigami.Units.smallSpacing
                        }

                        Controls.Label {
                            anchors.centerIn: parent
                            color: panel.dimColor
                            horizontalAlignment: Text.AlignHCenter
                            text: panel.selectedResult === null
                                ? qsTr("This check has never run.\nRunning it records a verdict against the workspace state it ran on.")
                                : qsTr("The run recorded no output.")
                            textFormat: Text.PlainText
                            visible: !outputColumn.hasOutput
                            width: Math.min(parent.width - Kirigami.Units.gridUnit * 2,
                                Kirigami.Units.gridUnit * 28)
                            wrapMode: Text.WordWrap
                        }

                        ColumnLayout {
                            anchors.fill: parent
                            anchors.margins: Kirigami.Units.smallSpacing
                            spacing: Kirigami.Units.smallSpacing / 2
                            visible: outputColumn.hasOutput

                            RowLayout {
                                Layout.fillWidth: true
                                spacing: Kirigami.Units.smallSpacing

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: panel.dimColor
                                    elide: Text.ElideRight
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    // Nothing of the run in flight is stored
                                    // until it ends, so while one is running
                                    // this console is the run before it, and
                                    // says so rather than looking current.
                                    text: panel.checkRunning
                                            && panel.runRequestedCheckId === panel.selectedCheckId
                                        ? qsTr("Previous run · %1")
                                            .arg(panel.outputNote(panel.selectedResult))
                                        : panel.outputNote(panel.selectedResult)
                                    textFormat: Text.PlainText
                                }

                                // The console opens at the tail, so getting back
                                // to the first kept byte needs an affordance
                                // rather than a long drag.
                                PanelButton {
                                    Controls.ToolTip.text: qsTr("Back to the start of the recorded output")
                                    Controls.ToolTip.visible: hovered
                                    text: qsTr("Top")
                                    onClicked: outputScroll.contentItem.contentY = 0
                                }
                            }

                            Controls.ScrollView {
                                id: outputScroll

                                Layout.fillHeight: true
                                Layout.fillWidth: true
                                clip: true

                                /// Scrolls to the newest bytes. Both recorded
                                /// streams are tails, so the end is the part
                                /// that was kept on purpose.
                                function showTail() {
                                    const flickable = outputScroll.contentItem;
                                    flickable.contentY = Math.max(
                                        0, flickable.contentHeight - flickable.height);
                                }

                                ColumnLayout {
                                    id: outputColumn

                                    readonly property var result: panel.selectedResult
                                    readonly property string stdoutBody: result !== null
                                        ? String(result.stdoutTail || "") : ""
                                    readonly property string stderrBody: result !== null
                                        ? String(result.stderrTail || "") : ""
                                    readonly property bool hasOutput: stdoutBody.length > 0
                                        || stderrBody.length > 0

                                    width: outputScroll.availableWidth
                                        - Kirigami.Units.smallSpacing * 2
                                    spacing: Kirigami.Units.largeSpacing

                                    // Deferred so the layout has resized to the
                                    // new text before the position is computed.
                                    onHasOutputChanged: Qt.callLater(outputScroll.showTail)
                                    onStdoutBodyChanged: Qt.callLater(outputScroll.showTail)
                                    onStderrBodyChanged: Qt.callLater(outputScroll.showTail)

                                    OutputStream {
                                        body: outputColumn.stdoutBody
                                        title: qsTr("Standard output")
                                        truncated: outputColumn.result !== null
                                            && outputColumn.result.stdoutTruncated === true
                                    }

                                    OutputStream {
                                        body: outputColumn.stderrBody
                                        title: qsTr("Standard error")
                                        truncated: outputColumn.result !== null
                                            && outputColumn.result.stderrTruncated === true
                                    }
                                }
                            }
                        }
                    }

                    // Problems. What the configured parser attributed to a
                    // place in the workspace, and what it had to leave out.
                    Item {
                        Controls.Label {
                            anchors.centerIn: parent
                            color: panel.dimColor
                            text: panel.selectedResult === null
                                ? qsTr("This check has never run.")
                                : qsTr("No diagnostics were extracted from this run.")
                            textFormat: Text.PlainText
                            visible: problemsColumn.diagnostics.length === 0
                        }

                        Controls.ScrollView {
                            id: problemsScroll

                            anchors.fill: parent
                            clip: true
                            visible: problemsColumn.diagnostics.length > 0
                                || problemsColumn.omissionNote.length > 0

                            ColumnLayout {
                                id: problemsColumn

                                readonly property var diagnostics: panel.selectedResult !== null
                                    ? (panel.selectedResult.diagnostics || []) : []
                                readonly property string omissionNote: {
                                    const result = panel.selectedResult;
                                    if (result === null)
                                        return "";
                                    const omitted = Number(result.diagnosticsOmitted || 0);
                                    if (omitted === 0 && result.diagnosticsScanTruncated !== true)
                                        return "";
                                    return panel.format(qsTr("%1 diagnostics omitted%2"), [
                                        omitted,
                                        result.diagnosticsScanTruncated === true
                                            ? qsTr("; the diagnostic source was itself truncated")
                                            : ""
                                    ]);
                                }

                                spacing: Kirigami.Units.smallSpacing
                                // Without a width the layout takes its
                                // children's natural width, and a diagnostic
                                // message is one long line that then runs off
                                // the pane instead of wrapping inside it.
                                width: problemsScroll.availableWidth
                                    - Kirigami.Units.smallSpacing

                                Repeater {
                                    model: problemsColumn.diagnostics

                                    delegate: RowLayout {
                                        required property var modelData

                                        Layout.fillWidth: true
                                        spacing: Kirigami.Units.smallSpacing

                                        StatusDot {
                                            Layout.alignment: Qt.AlignTop
                                            dotColor: String(parent.modelData.level) === "error"
                                                ? panel.negativeColor
                                                : panel.neutralColor
                                            settled: true
                                        }

                                        Controls.Label {
                                            Layout.alignment: Qt.AlignTop
                                            Layout.minimumWidth: Kirigami.Units.gridUnit * 4
                                            Layout.preferredWidth: Kirigami.Units.gridUnit * 4
                                            color: String(parent.modelData.level) === "error"
                                                ? panel.negativeColor
                                                : panel.neutralColor
                                            elide: Text.ElideRight
                                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                            // The level in words, so the line is
                                            // not read by colour alone.
                                            text: String(parent.modelData.level || "")
                                            textFormat: Text.PlainText
                                        }

                                        Controls.Label {
                                            Layout.fillWidth: true
                                            color: panel.bodyColor
                                            font.family: "monospace"
                                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                            text: {
                                                const row = parent.modelData;
                                                const path = String(row.path || "");
                                                const line = Number(row.line || 0);
                                                const column = Number(row.column || 0);
                                                let place = path;
                                                if (path.length > 0 && line > 0) {
                                                    place += ":" + line;
                                                    if (column > 0)
                                                        place += ":" + column;
                                                }
                                                return place.length > 0
                                                    ? place + ": " + String(row.message || "")
                                                    : String(row.message || "");
                                            }
                                            textFormat: Text.PlainText
                                            wrapMode: Text.Wrap
                                        }
                                    }
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    Layout.topMargin: Kirigami.Units.smallSpacing
                                    color: panel.dimColor
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    text: problemsColumn.omissionNote
                                    textFormat: Text.PlainText
                                    visible: text.length > 0
                                    wrapMode: Text.WordWrap
                                }
                            }
                        }
                    }

                    // Invocation. What would run now, what did run, and the
                    // workspace identity the verdict is bound to.
                    Controls.ScrollView {
                        id: invocationScroll

                        clip: true

                        ColumnLayout {
                            spacing: Kirigami.Units.smallSpacing
                            width: invocationScroll.availableWidth
                                - Kirigami.Units.smallSpacing

                            Controls.Label {
                                color: panel.dimColor
                                font.capitalization: Font.AllUppercase
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                text: qsTr("Configured now")
                                textFormat: Text.PlainText
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: panel.bodyColor
                                font.family: "monospace"
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                text: panel.invocationPreview(panel.selectedCheck)
                                textFormat: Text.PlainText
                                wrapMode: Text.WrapAnywhere
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                Layout.topMargin: Kirigami.Units.smallSpacing
                                color: panel.negativeColor
                                font.capitalization: Font.AllUppercase
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                text: qsTr("Recorded run")
                                textFormat: Text.PlainText
                                visible: panel.invocationDrifted(
                                    panel.selectedCheck, panel.selectedResult)
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                color: panel.bodyColor
                                font.family: "monospace"
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                text: panel.invocationDrifted(
                                        panel.selectedCheck, panel.selectedResult)
                                    ? panel.recordedInvocation(panel.selectedResult)
                                    : ""
                                textFormat: Text.PlainText
                                visible: text.length > 0
                                wrapMode: Text.WrapAnywhere
                            }

                            Kirigami.Separator {
                                Layout.fillWidth: true
                                Layout.topMargin: Kirigami.Units.smallSpacing
                                visible: panel.selectedResult !== null
                            }

                            MetaRow {
                                name: qsTr("Verdict")
                                value: panel.selectedResult === null
                                    ? qsTr("Never run")
                                    : panel.format(qsTr("%1 · %2"), [
                                        panel.outcomeLabel(panel.selectedResult.outcome),
                                        panel.freshnessLabel(panel.selectedResult.freshness)
                                    ])
                            }

                            MetaRow {
                                name: qsTr("Took")
                                value: panel.selectedResult === null
                                    ? ""
                                    : panel.durationText(panel.selectedResult.durationMs)
                            }

                            MetaRow {
                                name: qsTr("Recorded")
                                value: panel.selectedResult === null
                                    ? ""
                                    : String(panel.selectedResult.createdAt || "")
                                monospace: true
                            }

                            MetaRow {
                                name: qsTr("Evidence")
                                value: panel.selectedResult === null
                                    ? ""
                                    : panel.evidenceLabel(panel.selectedResult.evidenceClass)
                            }

                            MetaRow {
                                name: qsTr("Run")
                                value: panel.selectedResult === null
                                    ? ""
                                    : String(panel.selectedResult.runId || "")
                                monospace: true
                            }

                            MetaRow {
                                name: qsTr("State")
                                value: panel.selectedResult === null
                                    ? ""
                                    : panel.stateReference(panel.selectedResult)
                                monospace: true
                            }

                            Item {
                                Layout.fillHeight: true
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
            if (configuration !== null) {
                panel.runRequestedCheckId = checkId;
                panel.backend.runCheck(panel.project.id, checkId, true);
            }
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
