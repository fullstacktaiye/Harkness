import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// One run: what it is, what it did, and what may still be done to it.
///
/// Pushed on the window's `pageStack` from either entry point — the shell's Runs
/// view or the launcher's recent runs — so Back works natively and one run has
/// one detail surface however it was reached.
///
/// # Two reads, deliberately separate
///
/// The header, the calls, the artifacts and the approvals come from one
/// `loadRun` projection; the timeline comes from `RunTimelineModel`, which pages
/// and subscribes on its own. They are not one read because they age
/// differently: a timeline appends events for as long as the run is executing,
/// and re-reading the whole run for every event would be a store query per
/// progress line. The timeline arriving is instead what *schedules* a header
/// re-read, coalesced by `headerReload` so a burst of events costs one.
///
/// # Everything shown here is untrusted
///
/// Task titles, tool identifiers, error messages, progress lines, artifact names
/// and event payloads are written by tools, by agents, or by the repository.
/// Every label rendering one is `Text.PlainText`, and the one control that
/// renders rich text whatever it is told — `Kirigami.InlineMessage` — is only
/// ever handed text through `escapedRichText`. Nothing on this page executes an
/// artifact, opens one through the desktop, or asks a tool to do anything: the
/// two mutations it offers are Cancel and Retry, both of which merely request.
Kirigami.Page {
    id: page

    /// The run to show.
    required property string runId
    /// The Git and catalog bridge, used here for the clipboard alone.
    property var backend: null

    /// Lets Main.qml recognize this page in the stack; see its navigation notes.
    readonly property bool isRunDetail: true

    /// Which of the three record sections is on screen. It stays put across a
    /// reload, because a reader comparing two runs compares the same evidence.
    property int section: 0

    /// The one artifact whose bytes are on screen, or empty.
    ///
    /// Held here rather than per row because `excerpt` is one property
    /// answering for every artifact: two rows open at once would leave the
    /// older one reading "Hide" over nothing, with nothing on either row saying
    /// which of them the property is about. Opening one closes the other, which
    /// a single identifier makes unrepresentable rather than merely enforced.
    property string openArtifact: ""

    /// The bytes on screen, empty until the read for `openArtifact` answers.
    ///
    /// Gated on the identifier here rather than in the delegate, because the
    /// bridge answers for whichever artifact was asked for last and a row must
    /// never render another row's content under its own name.
    readonly property string openArtifactText: {
        if (page.openArtifact.length === 0
                || runs.excerpt === undefined || runs.excerpt === null)
            return "";
        return String(runs.excerpt.artifactId || "") === page.openArtifact
            ? String(runs.excerpt.text) : "";
    }

    /// Whether the bridge cut the rendering now on screen.
    readonly property bool openArtifactCut: page.openArtifactText.length > 0
        && runs.excerpt !== undefined && runs.excerpt !== null
        && runs.excerpt.truncated === true

    /// Puts one artifact's bytes on screen, closing whatever was open.
    ///
    /// Naming the artifact already open closes it, so the row's one control
    /// both opens and hides. The rule lives here rather than in the delegate
    /// because a pooled delegate is not where state about the page belongs.
    function showArtifact(artifactId) {
        const id = String(artifactId || "");
        if (id.length === 0 || page.openArtifact === id) {
            page.openArtifact = "";
            return;
        }
        page.openArtifact = id;
        runs.loadArtifactExcerpt(id);
    }

    title: page.headerTitle
    padding: 0

    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    background: Rectangle {
        color: Kirigami.Theme.backgroundColor
    }

    // --- Shared row parts, declared before anything uses them --------------

    /// A short state word on its own ground, as the checks view draws one.
    component StatePill: Rectangle {
        id: pill

        required property color pillColor
        property alias text: pillLabel.text

        border.color: Qt.alpha(pill.pillColor, 0.85)
        border.width: 1
        color: Qt.alpha(pill.pillColor, 0.16)
        implicitHeight: pillLabel.implicitHeight + Kirigami.Units.smallSpacing
        implicitWidth: pillLabel.implicitWidth + Kirigami.Units.largeSpacing
        radius: implicitHeight / 2
        visible: pillLabel.text.length > 0

        Controls.Label {
            id: pillLabel

            anchors.centerIn: parent
            color: pill.pillColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            textFormat: Text.PlainText
        }
    }

    /// One `name value` pair of the header.
    component MetaField: Row {
        id: field

        property string name: ""
        property string value: ""
        property bool monospace: false

        spacing: Kirigami.Units.smallSpacing
        visible: field.value.length > 0

        Controls.Label {
            color: runState.dimColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: field.name
            textFormat: Text.PlainText
        }

        Controls.Label {
            color: runState.bodyColor
            font.family: field.monospace ? "monospace" : Kirigami.Theme.defaultFont.family
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: field.value
            textFormat: Text.PlainText
        }
    }

    /// A height-capped monospace rendering of something a tool produced.
    ///
    /// Capped by line count rather than put in a scroll area on purpose: a
    /// `Flickable` inside a `ListView` delegate steals the wheel from the list
    /// it sits in, and the bridge has already bounded what arrives here — an
    /// event payload at 8 KiB, an artifact excerpt at 8 KiB, an approval input
    /// at 8 KiB. A rendering that is still cut says which of the two cut it, and
    /// Copy puts the whole of what was loaded on the clipboard.
    component BoundedText: ColumnLayout {
        id: bounded

        /// The text to render; empty renders nothing at all.
        property string content: ""
        /// Whether the bridge cut the content before it arrived.
        property bool cut: false

        spacing: Kirigami.Units.smallSpacing
        visible: bounded.content.length > 0

        // The ground is the label's own `background` rather than a `Rectangle`
        // around it. A rectangle sized from a wrapping label is an item whose
        // implicit height depends on the width the layout above it is still
        // deciding, which Qt Quick Layouts detects as a recursive rearrange and
        // abandons; a layout handles a `Text` directly.
        Controls.Label {
            id: boundedLabel

            Layout.fillWidth: true
            bottomPadding: Kirigami.Units.smallSpacing
            color: runState.bodyColor
            elide: Text.ElideRight
            font.family: "monospace"
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            leftPadding: Kirigami.Units.smallSpacing
            // Twenty-four lines is a paragraph of evidence rather than a pane of
            // it; what is cut here is still on the clipboard and in
            // `harkness run show`.
            maximumLineCount: 24
            rightPadding: Kirigami.Units.smallSpacing
            // Tool output, an event payload, or a validated tool input.
            text: bounded.content
            textFormat: Text.PlainText
            topPadding: Kirigami.Units.smallSpacing
            wrapMode: Text.WrapAnywhere

            background: Rectangle {
                border.color: runState.frameColor
                border.width: 1
                color: Kirigami.Theme.alternateBackgroundColor
                radius: Kirigami.Units.smallSpacing
            }
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            Controls.Label {
                Layout.fillWidth: true
                color: runState.neutralColor
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: bounded.cut
                    ? qsTr("Harkness cut this short; the command line prints the whole of it.")
                    : qsTr("Cut short to fit; Copy takes all of what was loaded.")
                textFormat: Text.PlainText
                visible: bounded.cut || boundedLabel.truncated
                wrapMode: Text.WordWrap
            }

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Copy what was loaded")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: page.backend !== null
                icon.name: "edit-copy-symbolic"
                text: qsTr("Copy")
                onClicked: {
                    if (page.backend !== null)
                        page.backend.copyToClipboard(bounded.content);
                }
            }
        }
    }

    /// Whether both halves of the body fit side by side.
    ///
    /// The timeline and the record lists ask for 22 and 18 grid units, so below
    /// their sum plus a handle there is no horizontal arrangement that is not
    /// clipping one of them. Stacking is the answer rather than hiding: both
    /// halves stay reachable and the reader keeps the divider, which is the one
    /// control that decides how the space is shared.
    readonly property bool sideBySide: width >= Kirigami.Units.gridUnit * 44

    /// One section tab, marked along its bottom edge the way the checks view
    /// marks its evidence tabs — and for the reason a `TabBar` is not used
    /// there: a bar divides its width between its buttons and drifts them apart.
    component SectionTab: Controls.AbstractButton {
        id: tab

        required property bool selected

        Accessible.name: tab.text
        hoverEnabled: true
        implicitHeight: tabLabel.implicitHeight + Kirigami.Units.largeSpacing
        implicitWidth: tabLabel.implicitWidth + Kirigami.Units.gridUnit

        background: Rectangle {
            color: tab.hovered && !tab.selected ? Qt.rgba(1, 1, 1, 0.06) : "transparent"

            Rectangle {
                anchors.bottom: parent.bottom
                anchors.left: parent.left
                anchors.right: parent.right
                color: runState.accentColor
                height: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
                visible: tab.selected
            }
        }

        contentItem: Controls.Label {
            id: tabLabel

            color: tab.selected ? runState.bodyColor : runState.dimColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            font.weight: tab.selected ? Font.DemiBold : Font.Normal
            horizontalAlignment: Text.AlignHCenter
            text: tab.text
            textFormat: Text.PlainText
            verticalAlignment: Text.AlignVCenter
        }
    }

    // --- The bridges this page reads --------------------------------------

    RunState {
        id: runState
    }

    RunsBackend {
        id: runs
    }

    RunTimelineModel {
        id: timeline
    }

    readonly property var run: runs.run !== undefined && runs.run !== null ? runs.run : ({})
    /// True once the loaded projection is the run this page was pushed for.
    readonly property bool ready: String(run.runId || "") === page.runId
    readonly property string runStateValue: ready ? String(run.state || "") : ""
    readonly property string headerTitle: {
        if (!ready)
            return qsTr("Run");
        const title = String(run.title || "");
        return title.length > 0 ? title : qsTr("Run %1").arg(page.runId);
    }
    readonly property var calls: ready && run.calls !== undefined ? run.calls : []
    readonly property var artifacts: ready && run.artifacts !== undefined ? run.artifacts : []
    readonly property var approvals: ready && run.approvals !== undefined ? run.approvals : []
    readonly property var truncated: ready && run.truncated !== undefined ? run.truncated : []
    /// Later attempts at the same task, oldest first, as the bridge lists them.
    readonly property var retries: ready && run.retries !== undefined ? run.retries : []

    /// The unanswered request a parked run is waiting on, or null.
    ///
    /// Read from the approvals this run recorded rather than from the pending
    /// queue: a run parked on a question has that question in its own history,
    /// and the queue across every run is a different surface's subject.
    readonly property var pendingApproval: {
        for (let index = 0; index < approvals.length; ++index) {
            if (approvals[index].pending === true)
                return approvals[index];
        }
        return null;
    }

    /// The call that was executing when the owning process stopped.
    ///
    /// `interrupted` is written by the recovery sweep and by nothing else, so
    /// this is what was in flight rather than a guess from timestamps.
    readonly property var inFlightCall: {
        for (let index = 0; index < calls.length; ++index) {
            if (runState.callWasInFlight(String(calls[index].state)))
                return calls[index];
        }
        return null;
    }

    /// Whether the banner is showing the request rather than only naming it.
    ///
    /// Page state rather than banner state, so the one control that changes it
    /// and the offscreen tests that drive it reach the same property.
    property bool approvalExpanded: false
    /// Shows or hides the request the run is parked on, loading its input the
    /// first time somebody asks to see it.
    function reviewApproval() {
        page.approvalExpanded = !page.approvalExpanded;
        if (page.approvalExpanded && page.pendingApproval !== null)
            runs.loadApprovalInput(String(page.pendingApproval.approvalId));
    }

    /// Whether the waiting-for-approval banner is on screen.
    ///
    /// Exposed because the criterion it answers — exactly one run state renders
    /// this banner — is about the banner rather than about the two properties
    /// its condition is built from, and the offscreen tests assert it directly.
    readonly property alias approvalBannerVisible: approvalBanner.visible
    /// The validated input the banner is showing, empty until it is asked for.
    readonly property alias approvalInput: approvalReview.input
    /// The discriminant of the last failure the bridge reported, empty on
    /// success. Exposed for the same reason the two views below are: whether a
    /// refused mutation reaches the reader is a property of this page.
    readonly property string failureKind: String(runs.kind || "")
    /// The timeline's view, exposed for the same reason: whether a
    /// thousand-event run creates delegates only for its visible region is a
    /// property of this view and of nothing the model can be asked.
    readonly property alias timelineView: events
    /// The artifacts list, exposed for the same reason: whether its rows exist
    /// and render is a property of the view rather than of the projection.
    readonly property alias artifactView: artifactList

    readonly property bool cancellable: ready && runState.pending(runStateValue)
    readonly property bool retryable: ready && run.retryable === true
    readonly property string retryBlocked: ready ? String(run.retryBlocked || "") : ""

    /// Why the Retry action is absent, in the reader's words.
    ///
    /// The absence of a control is the worst kind of missing explanation, so the
    /// discriminant the runtime would refuse with is spelled out beside it.
    function retryBlockedReason(kind) {
        if (kind === "run_still_active")
            return qsTr("A retry is a fresh attempt at the same task, so it waits until this run — and every re-attempt of it — has finished.");
        if (kind === "run_not_retryable")
            return qsTr("This run succeeded. Start a new run rather than re-attempting one that worked.");
        return "";
    }

    function reload() {
        runs.loadRun(page.runId);
    }

    /// Whether a mutation this page issued is still outstanding.
    ///
    /// Cancel and Retry change exactly the durable state the header is a read
    /// of — which state the run is in, and whether a retry is still available —
    /// and nothing else re-reads it. The timeline is what schedules a header
    /// reload, and a run this process is not driving publishes nothing to it,
    /// so without this the page would go on offering a Retry the coordinator
    /// has already been asked for and would now refuse.
    property bool mutating: false

    /// Stops the run, and re-reads it once the request has been answered.
    function cancel() {
        page.mutating = true;
        runs.cancelRun(page.runId);
    }

    /// Starts a fresh attempt, and re-reads this run once it has been made.
    function retry() {
        page.mutating = true;
        runs.retryRun(page.runId);
    }

    /// Reads the run this page is pointed at, from both of its sources.
    function select() {
        // A new run is a new set of artifacts, so the identifier held here
        // names none of them. Cleared on a re-target and not on a reload: a
        // reload is the header catching up, and collapsing what the reader has
        // open every time a live run appends an event would be unusable.
        page.openArtifact = "";
        reload();
        timeline.select(page.runId);
    }

    /// Whether construction has already selected once.
    ///
    /// The window pushes a fresh page per run, so re-targeting is not how a
    /// user reaches another run; a host that *reuses* one page — the offscreen
    /// tests do — would otherwise leave it showing the run it was built with.
    property bool selected: false

    onRunIdChanged: {
        if (page.selected)
            page.select();
    }

    Component.onCompleted: {
        page.selected = true;
        page.select();
    }

    /// Coalesces the header re-reads a live run's events would otherwise ask
    /// for. A tool reporting a hundred progress lines must cost one store read,
    /// not a hundred; the delay is long enough to absorb a burst and short
    /// enough that a state change lands while the reader is still looking.
    Timer {
        id: headerReload

        interval: 750
        repeat: false
        onTriggered: page.reload()
    }

    Connections {
        /// A mutation is answered by the durable state it changed rather than
        /// by the message it returned, so the page re-reads once every
        /// operation it has outstanding has settled. `mutating` is cleared
        /// first, so the re-read this schedules cannot schedule another.
        function onBusyChanged() {
            if (runs.busy || !page.mutating)
                return;
            page.mutating = false;
            page.reload();
            timeline.refresh();
        }

        target: runs
    }

    Connections {
        /// The log grew at the tip, so the run's own state may have moved with
        /// it. `appended` rather than `rowsInserted` or `dataChanged`: both of
        /// those also fire for a backwards page, which is history that has not
        /// changed since it was written, and `dataChanged` fires again when a
        /// payload arrives on a row a reader opened — so listening to them made
        /// opening a row, or asking for older events, cost a full re-read of
        /// the run. It still covers the folded progress row, which absorbs
        /// ticks in place rather than inserting.
        function onAppended() {
            headerReload.restart();
        }

        function onLiveChanged() {
            page.reload();
        }

        target: timeline
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        // --- Header --------------------------------------------------------

        Item {
            Layout.fillWidth: true
            implicitHeight: headerBody.implicitHeight + Kirigami.Units.largeSpacing * 2

            Kirigami.Theme.colorSet: Kirigami.Theme.Header
            Kirigami.Theme.inherit: false
            Kirigami.Theme.backgroundColor: "#000000"
            Kirigami.Theme.textColor: "#ffffff"

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.backgroundColor
            }

            ColumnLayout {
                id: headerBody

                anchors.left: parent.left
                anchors.leftMargin: Kirigami.Units.largeSpacing
                anchors.right: parent.right
                anchors.rightMargin: Kirigami.Units.largeSpacing
                anchors.verticalCenter: parent.verticalCenter
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Back")
                        Controls.ToolTip.visible: hovered
                        display: Controls.AbstractButton.IconOnly
                        icon.name: "go-previous-symbolic"
                        text: qsTr("Back")
                        onClicked: applicationWindow().pageStack.pop()
                    }

                    Rectangle {
                        Layout.alignment: Qt.AlignVCenter
                        color: runState.stateColor(page.runStateValue)
                        implicitHeight: Kirigami.Units.smallSpacing
                        implicitWidth: Kirigami.Units.smallSpacing
                        radius: width / 2
                        visible: page.ready && !runState.pending(page.runStateValue)
                    }

                    Controls.BusyIndicator {
                        Layout.alignment: Qt.AlignVCenter
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: page.ready && runState.pending(page.runStateValue)
                        visible: running
                    }

                    Kirigami.Heading {
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        level: 3
                        // Written by whoever started the run.
                        text: page.headerTitle
                        textFormat: Text.PlainText
                    }

                    StatePill {
                        pillColor: runState.stateColor(page.runStateValue)
                        text: page.ready ? runState.stateLabel(page.runStateValue) : ""
                    }

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Re-read this run")
                        Controls.ToolTip.visible: hovered
                        display: Controls.AbstractButton.IconOnly
                        enabled: !runs.busy
                        icon.name: "view-refresh-symbolic"
                        text: qsTr("Refresh")
                        onClicked: {
                            page.reload();
                            timeline.refresh();
                        }
                    }

                    Controls.Button {
                        Controls.ToolTip.text: qsTr("Stop this run's queued calls, its executing tool, and any approval it is parked on")
                        Controls.ToolTip.visible: hovered
                        enabled: !runs.busy
                        icon.name: "process-stop-symbolic"
                        text: qsTr("Cancel")
                        visible: page.cancellable
                        onClicked: page.cancel()
                    }

                    Controls.Button {
                        // Shown only when the runtime's own durable state says a
                        // retry is available. Eligibility is the coordinator's
                        // decision and this is a read of it, never a second
                        // opinion: a press it would refuse is never offered.
                        Controls.ToolTip.text: qsTr("Start a fresh attempt at the same task; nothing is resumed and no approval carries over")
                        Controls.ToolTip.visible: hovered
                        enabled: !runs.busy
                        icon.name: "view-refresh-symbolic"
                        text: qsTr("Retry")
                        visible: page.retryable
                        onClicked: page.retry()
                    }
                }

                // A row rather than a `Flow`: a flow's height depends on the
                // width a layout is still deciding, which Qt Quick Layouts
                // detects as a recursive rearrange and gives up on.
                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.largeSpacing

                    MetaField {
                        name: qsTr("Started")
                        value: runState.localTime(page.ready ? String(page.run.started || "") : "")
                    }

                    MetaField {
                        name: qsTr("Finished")
                        value: runState.localTime(page.ready ? String(page.run.finished || "") : "")
                    }

                    MetaField {
                        name: qsTr("Took")
                        value: page.ready
                            ? runState.duration(String(page.run.started || ""),
                                                String(page.run.finished || ""))
                            : ""
                    }

                    Item {
                        Layout.fillWidth: true
                    }
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: page.ready

                    Controls.Label {
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Workspace")
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.bodyColor
                        elide: Text.ElideMiddle
                        font.family: "monospace"
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        // A filesystem path out of a task record.
                        text: page.ready ? String(page.run.workspace || "") : ""
                        textFormat: Text.PlainText
                    }

                    // The run identifier sits on this row rather than with the
                    // times above it: it is the widest thing in the header and
                    // the only one of them that can be elided without losing
                    // what it says, because the row above holds no label that
                    // shrinks and would simply overflow a narrow window.
                    Controls.Label {
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Run")
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        // No `fillWidth`, so it takes its natural width while
                        // there is room and gives way only when the row is over
                        // budget - at which point eliding an identifier beats
                        // pushing the workspace path off the window.
                        Layout.maximumWidth: implicitWidth
                        color: runState.bodyColor
                        elide: Text.ElideMiddle
                        font.family: "monospace"
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: page.runId
                        textFormat: Text.PlainText
                    }

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Open the run this one re-attempts")
                        Controls.ToolTip.visible: hovered
                        text: qsTr("Re-attempt of an earlier run")
                        visible: page.ready && String(page.run.retryOf || "").length > 0
                        onClicked: applicationWindow().showRun(String(page.run.retryOf))
                    }

                    // The other direction. "Was this already re-attempted, and
                    // how did that go" is the question a reader arrives at a
                    // failed run with, and the answer is a run of its own; the
                    // newest is the one that has the most to say.
                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Open the newest re-attempt of this run")
                        Controls.ToolTip.visible: hovered
                        text: qsTr("Re-attempted since")
                        visible: page.retries.length > 0
                        onClicked: applicationWindow().showRun(
                            String(page.retries[page.retries.length - 1]))
                    }
                }
            }
        }

        Rectangle {
            Layout.fillWidth: true
            color: runState.frameColor
            implicitHeight: 1
        }

        // --- Banners -------------------------------------------------------

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            // The bridge's own message, which may quote a workspace path.
            text: runState.escapedRichText(String(runs.status || ""))
            type: Kirigami.MessageType.Error
            visible: String(runs.kind || "").length > 0
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            // A tool's failure text: markup in it renders as the characters the
            // tool wrote, and control characters as themselves.
            text: runState.escapedRichMultiline(page.ready
                ? String(page.run.errorKind || "") + " — " + String(page.run.errorMessage || "")
                : "")
            type: Kirigami.MessageType.Error
            visible: page.ready && String(page.run.errorKind || "").length > 0
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            text: runState.escapedRichText(page.inFlightCall !== null
                ? qsTr("The process driving this run stopped while %1 was executing. Nothing was resumed; a retry is a fresh attempt.")
                    .arg(String(page.inFlightCall.toolId))
                : qsTr("The process driving this run stopped before it finished. Nothing was resumed; a retry is a fresh attempt."))
            type: Kirigami.MessageType.Warning
            visible: page.runStateValue === "interrupted"
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            text: runState.escapedRichText(qsTr("An earlier attempt may have changed this workspace before it stopped. Review the working tree before re-running."))
            type: Kirigami.MessageType.Warning
            visible: page.ready && page.run.workspaceModified === true
        }

        Controls.Label {
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
            color: runState.dimColor
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            text: page.retryBlockedReason(page.retryBlocked)
            textFormat: Text.PlainText
            visible: page.ready && !page.retryable && text.length > 0
            wrapMode: Text.WordWrap
        }

        // The waiting-for-approval banner, which no other state renders. It is
        // deliberately not an approve/deny control: the decision flow is the
        // approval surface's, and this page links to it rather than duplicating
        // it. What it does today is the review half — name the request, its risk
        // and its scope, and, on demand, the validated input the call holds.
        Rectangle {
            id: approvalBanner

            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            border.color: Qt.alpha(runState.neutralColor, 0.7)
            border.width: 1
            color: Qt.alpha(runState.neutralColor, 0.12)
            implicitHeight: approvalBody.implicitHeight + Kirigami.Units.largeSpacing
            radius: Kirigami.Units.smallSpacing
            visible: page.runStateValue === "waiting_for_approval" && page.pendingApproval !== null

            onVisibleChanged: {
                if (!visible)
                    page.approvalExpanded = false;
            }

            ColumnLayout {
                id: approvalBody

                anchors.left: parent.left
                anchors.leftMargin: Kirigami.Units.largeSpacing
                anchors.right: parent.right
                anchors.rightMargin: Kirigami.Units.largeSpacing
                anchors.verticalCenter: parent.verticalCenter
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Icon {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        source: "dialog-question-symbolic"
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.bodyColor
                        elide: Text.ElideRight
                        text: page.pendingApproval !== null
                            ? qsTr("%1 is waiting for a decision")
                                .arg(String(page.pendingApproval.tool))
                            : ""
                        textFormat: Text.PlainText
                    }

                    StatePill {
                        pillColor: runState.neutralColor
                        text: page.pendingApproval !== null
                            ? runState.riskLabel(String(page.pendingApproval.risk))
                            : ""
                    }

                    Controls.Button {
                        text: page.approvalExpanded
                            ? qsTr("Hide request")
                            : qsTr("Review request")
                        onClicked: page.reviewApproval()
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: runState.bodyColor
                    // The request's own summary, written by the tool layer.
                    text: page.pendingApproval !== null ? String(page.pendingApproval.summary) : ""
                    textFormat: Text.PlainText
                    visible: text.length > 0
                    wrapMode: Text.WordWrap
                }

                ColumnLayout {
                    id: approvalReview

                    /// The loaded input, but only while it is *this* request's:
                    /// the bridge is shared, and an input left standing under
                    /// another question is the worst way for it to be wrong.
                    readonly property string input: {
                        if (!page.approvalExpanded || page.pendingApproval === null)
                            return "";
                        if (runs.detail === undefined || runs.detail === null)
                            return "";
                        return String(runs.detail.approvalId || "")
                            === String(page.pendingApproval.approvalId)
                            ? String(runs.detail.input) : "";
                    }

                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    visible: page.approvalExpanded

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.largeSpacing

                        MetaField {
                            name: qsTr("Answering it would cover")
                            value: page.pendingApproval !== null
                                ? runState.scopeLabel(String(page.pendingApproval.scope)) : ""
                        }

                        MetaField {
                            name: qsTr("Asked for")
                            value: page.pendingApproval !== null
                                ? runState.scopeLabel(
                                    String(page.pendingApproval.requestedScope))
                                : ""
                        }

                        MetaField {
                            name: qsTr("Capabilities")
                            value: page.pendingApproval !== null
                                ? String(page.pendingApproval.capabilities) : ""
                        }

                        Item {
                            Layout.fillWidth: true
                        }
                    }

                    BoundedText {
                        Layout.fillWidth: true
                        // The recorded call's own input, already redacted at the
                        // persistence boundary and clamped by the bridge.
                        content: approvalReview.input
                        cut: approvalReview.input.length > 0
                            && runs.detail !== undefined && runs.detail !== null
                            && runs.detail.truncated === true
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Answering is done from the approval surface or from `harkness approval decide`; this page shows what was asked.")
                        textFormat: Text.PlainText
                        wrapMode: Text.WordWrap
                    }
                }
            }
        }

        // --- Timeline beside the run's own records -------------------------

        Controls.SplitView {
            Layout.fillHeight: true
            Layout.fillWidth: true
            orientation: page.sideBySide ? Qt.Horizontal : Qt.Vertical

            // One hairline whichever way the split runs; a vertical SplitView
            // sizes its handle by height and a horizontal one by width, and an
            // unstated implicit size collapses the handle to nothing.
            handle: Rectangle {
                color: runState.frameColor
                implicitHeight: 1
                implicitWidth: 1
            }

            ColumnLayout {
                Controls.SplitView.fillHeight: true
                Controls.SplitView.fillWidth: true
                Controls.SplitView.minimumHeight: Kirigami.Units.gridUnit * 10
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 22
                spacing: 0

                RowLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        color: runState.dimColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Timeline")
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        color: runState.accentColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("live")
                        textFormat: Text.PlainText
                        visible: timeline.live
                    }

                    Item {
                        Layout.fillWidth: true
                    }

                    Controls.BusyIndicator {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: timeline.loading
                        visible: running
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    color: runState.neutralColor
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    // The model's own message: which bound it reached, or the
                    // failure that stopped a page.
                    text: String(timeline.status || "")
                    textFormat: Text.PlainText
                    visible: text.length > 0
                    wrapMode: Text.WordWrap
                }

                ListView {
                    id: events

                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    activeFocusOnTab: true
                    cacheBuffer: Math.max(height * 2, Kirigami.Units.gridUnit * 20)
                    clip: true
                    currentIndex: -1
                    keyNavigationEnabled: true
                    model: timeline
                    reuseItems: true

                    Controls.ScrollBar.vertical: Controls.ScrollBar {}

                    Keys.onEnterPressed: events.toggleCurrent()
                    Keys.onReturnPressed: events.toggleCurrent()

                    /// Opens the row the keyboard is on, which is what a click
                    /// does to the row the pointer is over. Reading a payload is
                    /// the same act either way, so both routes go through the
                    /// delegate's own `toggle` rather than repeating its rule.
                    function toggleCurrent() {
                        if (currentIndex >= 0 && currentItem)
                            currentItem.toggle();
                    }

                    // Paging backwards is a header button, so a reader who has
                    // scrolled to the top of the loaded window asks for more
                    // rather than triggering a store read by arriving there.
                    header: Item {
                        implicitHeight: loadOlder.visible
                            ? loadOlder.implicitHeight + Kirigami.Units.largeSpacing
                            : 0
                        width: events.width

                        Controls.Button {
                            id: loadOlder

                            anchors.centerIn: parent
                            enabled: !timeline.loading
                            text: qsTr("Load older events")
                            visible: timeline.more
                            onClicked: timeline.loadOlder()
                        }
                    }

                    delegate: Item {
                        id: eventRow

                        required property string at
                        required property string artifactId
                        required property string detail
                        required property bool hasDetail
                        required property int index
                        required property string kind
                        required property string outcome
                        required property int progressCount
                        required property bool recognized
                        required property int seq
                        required property string summary
                        required property string toolCallId

                        property bool expanded: false

                        readonly property color accent: runState.eventColor(kind, outcome)

                        /// Opens or closes this row, reading its payload the
                        /// first time somebody asks to see one. A payload is
                        /// never fetched for a row nobody opened.
                        function toggle() {
                            expanded = !expanded;
                            if (expanded && hasDetail && detail.length === 0)
                                timeline.loadDetail(seq);
                        }

                        // The row is a control rather than a label: it opens,
                        // and a reader on the keyboard has to be told what they
                        // are on and that Enter does something to it.
                        Accessible.name: qsTr("%1 at %2")
                            .arg(runState.eventLabel(eventRow.kind))
                            .arg(runState.clockTime(eventRow.at))
                        Accessible.role: Accessible.Button
                        implicitHeight: eventBody.implicitHeight + Kirigami.Units.smallSpacing * 2
                        width: ListView.view.width

                        // A reused delegate is handed another event, so whatever
                        // the reader opened belongs to a row that is gone.
                        ListView.onPooled: expanded = false
                        ListView.onReused: expanded = false

                        MouseArea {
                            anchors.fill: parent
                            hoverEnabled: true
                            onClicked: {
                                events.currentIndex = eventRow.index;
                                eventRow.toggle();
                            }

                            Rectangle {
                                anchors.fill: parent
                                // The keyboard's position is marked as well as
                                // the pointer's: a list that answers Enter and
                                // shows nothing selected answers it invisibly.
                                color: eventRow.ListView.isCurrentItem
                                    ? Qt.rgba(1, 1, 1, 0.08)
                                    : parent.containsMouse
                                        ? Qt.rgba(1, 1, 1, 0.04)
                                        : "transparent"
                            }
                        }

                        RowLayout {
                            id: eventBody

                            anchors.left: parent.left
                            anchors.leftMargin: Kirigami.Units.largeSpacing
                            anchors.right: parent.right
                            anchors.rightMargin: Kirigami.Units.largeSpacing
                            anchors.verticalCenter: parent.verticalCenter
                            spacing: Kirigami.Units.smallSpacing

                            Kirigami.Icon {
                                Layout.alignment: Qt.AlignTop
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                color: eventRow.accent
                                source: runState.eventIcon(eventRow.kind)
                            }

                            Controls.Label {
                                Layout.alignment: Qt.AlignTop
                                color: runState.dimColor
                                font.family: "monospace"
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                text: runState.clockTime(eventRow.at)
                                textFormat: Text.PlainText
                            }

                            ColumnLayout {
                                Layout.fillWidth: true
                                spacing: 0

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Label {
                                        color: eventRow.accent
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        font.weight: Font.DemiBold
                                        // An unrecognized kind is a newer
                                        // build's spelling, shown verbatim.
                                        text: runState.eventLabel(eventRow.kind)
                                        textFormat: Text.PlainText
                                    }

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: qsTr("unknown to this build")
                                        textFormat: Text.PlainText
                                        visible: !eventRow.recognized
                                    }

                                    // Folded progress: the row says how many
                                    // ticks it stands for rather than the
                                    // timeline growing a row per tick.
                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: qsTr("%1 updates").arg(eventRow.progressCount)
                                        textFormat: Text.PlainText
                                        visible: eventRow.progressCount > 1
                                    }

                                    Item {
                                        Layout.fillWidth: true
                                    }

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: qsTr("payload")
                                        textFormat: Text.PlainText
                                        visible: eventRow.hasDetail && !eventRow.expanded
                                    }
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: runState.bodyColor
                                    elide: eventRow.expanded ? Text.ElideNone : Text.ElideRight
                                    // The event's bounded one-line summary,
                                    // built from a payload a tool wrote.
                                    maximumLineCount: eventRow.expanded ? 6 : 1
                                    text: eventRow.summary
                                    textFormat: Text.PlainText
                                    visible: text.length > 0
                                    wrapMode: eventRow.expanded ? Text.WrapAnywhere : Text.NoWrap
                                }

                                // Nothing is instantiated for the rows nobody
                                // opened, so a rare event kind costs nothing
                                // when it is absent and a thousand-event
                                // timeline holds no payload views at all.
                                Loader {
                                    Layout.fillWidth: true
                                    active: eventRow.expanded
                                    visible: active

                                    sourceComponent: Component {
                                        ColumnLayout {
                                            spacing: Kirigami.Units.smallSpacing

                                            BoundedText {
                                                Layout.fillWidth: true
                                                content: eventRow.detail
                                            }

                                            Controls.Label {
                                                Layout.fillWidth: true
                                                color: runState.dimColor
                                                font.family: "monospace"
                                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                                text: qsTr("artifact %1").arg(eventRow.artifactId)
                                                textFormat: Text.PlainText
                                                visible: eventRow.artifactId.length > 0
                                            }

                                            Controls.Label {
                                                Layout.fillWidth: true
                                                color: runState.dimColor
                                                font.family: "monospace"
                                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                                text: qsTr("call %1").arg(eventRow.toolCallId)
                                                textFormat: Text.PlainText
                                                visible: eventRow.toolCallId.length > 0
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // --- Calls, artifacts, approvals ------------------------------

            ColumnLayout {
                // Both axes are stated because the split turns: a vertical
                // SplitView reads the height constraints and ignores the width
                // ones, and an item with neither takes whatever is left.
                Controls.SplitView.minimumHeight: Kirigami.Units.gridUnit * 8
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 18
                Controls.SplitView.preferredHeight: Kirigami.Units.gridUnit * 16
                Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 26
                spacing: 0

                RowLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    spacing: 0

                    SectionTab {
                        selected: page.section === 0
                        text: qsTr("Calls (%1)").arg(page.calls.length)
                        onClicked: page.section = 0
                    }

                    SectionTab {
                        selected: page.section === 1
                        text: qsTr("Artifacts (%1)").arg(page.artifacts.length)
                        onClicked: page.section = 1
                    }

                    SectionTab {
                        selected: page.section === 2
                        text: qsTr("Approvals (%1)").arg(page.approvals.length)
                        onClicked: page.section = 2
                    }

                    Item {
                        Layout.fillWidth: true
                    }
                }

                Rectangle {
                    Layout.fillWidth: true
                    color: runState.frameColor
                    implicitHeight: 1
                }

                Controls.Label {
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    color: runState.neutralColor
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    // Named rather than silently missing: a page that cut a
                    // collection short has to say which one it cut.
                    text: qsTr("This run recorded more %1 than the page shows; `harkness run show` has all of them.")
                        .arg(String(page.truncated.join(", ")))
                    textFormat: Text.PlainText
                    visible: page.truncated.length > 0
                    wrapMode: Text.WordWrap
                }

                StackLayout {
                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    currentIndex: page.section

                    ListView {
                        id: callList

                        cacheBuffer: Math.max(height * 2, Kirigami.Units.gridUnit * 20)
                        clip: true
                        model: page.calls
                        reuseItems: true

                        Controls.ScrollBar.vertical: Controls.ScrollBar {}

                        delegate: Item {
                            id: callRow

                            required property var modelData

                            implicitHeight: callBody.implicitHeight + Kirigami.Units.smallSpacing * 2
                            width: ListView.view.width

                            ColumnLayout {
                                id: callBody

                                anchors.left: parent.left
                                anchors.leftMargin: Kirigami.Units.largeSpacing
                                anchors.right: parent.right
                                anchors.rightMargin: Kirigami.Units.smallSpacing
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 0

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Label {
                                        Layout.fillWidth: true
                                        color: runState.bodyColor
                                        elide: Text.ElideRight
                                        font.family: "monospace"
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        // A tool identifier the tool declared.
                                        text: String(callRow.modelData.toolId)
                                        textFormat: Text.PlainText
                                    }

                                    StatePill {
                                        pillColor: runState.callStateColor(
                                            String(callRow.modelData.state))
                                        text: runState.callStateLabel(String(callRow.modelData.state))
                                    }
                                }

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: qsTr("v%1").arg(String(callRow.modelData.toolVersion))
                                        textFormat: Text.PlainText
                                    }

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: runState.duration(String(callRow.modelData.started),
                                                                String(callRow.modelData.finished))
                                        textFormat: Text.PlainText
                                        visible: text.length > 0
                                    }

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: qsTr("policy: %1").arg(String(callRow.modelData.verdict))
                                        textFormat: Text.PlainText
                                        visible: String(callRow.modelData.verdict).length > 0
                                    }

                                    Item {
                                        Layout.fillWidth: true
                                    }
                                }

                                Kirigami.InlineMessage {
                                    Layout.fillWidth: true
                                    Layout.topMargin: Kirigami.Units.smallSpacing
                                    // A tool's own failure text; markup in it is
                                    // rendered as the characters it wrote.
                                    text: runState.escapedRichMultiline(
                                        String(callRow.modelData.errorKind) + " — "
                                            + String(callRow.modelData.errorMessage))
                                    type: Kirigami.MessageType.Error
                                    visible: String(callRow.modelData.errorKind).length > 0
                                }
                            }
                        }
                    }

                    ListView {
                        id: artifactList

                        cacheBuffer: Math.max(height * 2, Kirigami.Units.gridUnit * 20)
                        clip: true
                        model: page.artifacts
                        reuseItems: true

                        Controls.ScrollBar.vertical: Controls.ScrollBar {}

                        delegate: Item {
                            id: artifactRow

                            required property var modelData

                            readonly property bool expanded:
                                page.openArtifact === String(modelData.artifactId)
                            readonly property bool available:
                                String(modelData.availability) === "available"
                            /// The loaded excerpt, which is this row's only
                            /// while this row is the one that is open.
                            readonly property string excerpt: expanded
                                ? page.openArtifactText
                                : ""

                            implicitHeight: artifactBody.implicitHeight
                                + Kirigami.Units.smallSpacing * 2
                            width: ListView.view.width

                            ColumnLayout {
                                id: artifactBody

                                anchors.left: parent.left
                                anchors.leftMargin: Kirigami.Units.largeSpacing
                                anchors.right: parent.right
                                anchors.rightMargin: Kirigami.Units.smallSpacing
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 0

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Kirigami.Icon {
                                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                        source: artifactRow.available
                                            ? "document-save-symbolic"
                                            : "dialog-warning-symbolic"
                                    }

                                    Controls.Label {
                                        Layout.fillWidth: true
                                        color: runState.bodyColor
                                        elide: Text.ElideMiddle
                                        // A label the producing tool chose; it is
                                        // never a path component.
                                        text: String(artifactRow.modelData.name)
                                        textFormat: Text.PlainText
                                    }

                                    Controls.Label {
                                        color: runState.dimColor
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: runState.bytes(artifactRow.modelData.byteSize)
                                        textFormat: Text.PlainText
                                    }
                                }

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Label {
                                        color: runState.dimColor
                                        elide: Text.ElideRight
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        // A media type the producing tool declared.
                                        text: String(artifactRow.modelData.mediaType)
                                        textFormat: Text.PlainText
                                    }

                                    Controls.Label {
                                        color: runState.availabilityColor(
                                            String(artifactRow.modelData.availability))
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        // A file that went missing is a row that
                                        // says so, not a failed page.
                                        text: runState.availabilityLabel(
                                            String(artifactRow.modelData.availability))
                                        textFormat: Text.PlainText
                                    }

                                    Item {
                                        Layout.fillWidth: true
                                    }

                                    Controls.ToolButton {
                                        Controls.ToolTip.text: qsTr("Copy the path these bytes are stored at")
                                        Controls.ToolTip.visible: hovered
                                        display: Controls.AbstractButton.IconOnly
                                        enabled: page.backend !== null
                                        icon.name: "edit-copy-symbolic"
                                        text: qsTr("Copy path")
                                        onClicked: {
                                            if (page.backend !== null)
                                                page.backend.copyToClipboard(
                                                    String(artifactRow.modelData.path));
                                        }
                                    }

                                    Controls.ToolButton {
                                        text: artifactRow.expanded ? qsTr("Hide") : qsTr("Show")
                                        // Only what the runtime marked small and
                                        // textual is ever rendered inline, and
                                        // nothing here is ever executed.
                                        visible: artifactRow.modelData.excerptable === true
                                        onClicked: page.showArtifact(
                                            String(artifactRow.modelData.artifactId))
                                    }
                                }

                                BoundedText {
                                    Layout.fillWidth: true
                                    content: artifactRow.excerpt
                                    cut: artifactRow.expanded && page.openArtifactCut
                                }
                            }
                        }
                    }

                    ListView {
                        id: approvalList

                        cacheBuffer: Math.max(height * 2, Kirigami.Units.gridUnit * 20)
                        clip: true
                        model: page.approvals
                        reuseItems: true

                        Controls.ScrollBar.vertical: Controls.ScrollBar {}

                        delegate: Item {
                            id: approvalRow

                            required property var modelData

                            implicitHeight: approvalRowBody.implicitHeight
                                + Kirigami.Units.smallSpacing * 2
                            width: ListView.view.width

                            ColumnLayout {
                                id: approvalRowBody

                                anchors.left: parent.left
                                anchors.leftMargin: Kirigami.Units.largeSpacing
                                anchors.right: parent.right
                                anchors.rightMargin: Kirigami.Units.smallSpacing
                                anchors.verticalCenter: parent.verticalCenter
                                spacing: 0

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Label {
                                        Layout.fillWidth: true
                                        color: runState.bodyColor
                                        elide: Text.ElideRight
                                        font.family: "monospace"
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: String(approvalRow.modelData.tool)
                                        textFormat: Text.PlainText
                                    }

                                    StatePill {
                                        pillColor: runState.approvalStateColor(
                                            String(approvalRow.modelData.state))
                                        text: runState.approvalStateLabel(
                                            String(approvalRow.modelData.state))
                                    }
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: runState.dimColor
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    // The request's own summary, from the tool layer.
                                    text: String(approvalRow.modelData.summary)
                                    textFormat: Text.PlainText
                                    visible: text.length > 0
                                    wrapMode: Text.WordWrap
                                }

                                Controls.Label {
                                    Layout.fillWidth: true
                                    color: runState.dimColor
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    // A reason a person typed when they answered.
                                    text: String(approvalRow.modelData.decidedVia) + " · "
                                        + String(approvalRow.modelData.reason)
                                    textFormat: Text.PlainText
                                    visible: String(approvalRow.modelData.reason).length > 0
                                    wrapMode: Text.WordWrap
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
