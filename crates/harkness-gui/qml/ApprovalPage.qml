import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// One approval request: what is about to happen, and the decision about it.
///
/// # A page, deliberately not a `Kirigami.PromptDialog`
///
/// Every other confirmation in this application is a prompt dialog, and this
/// one is not, because a prompt dialog has an implicit accept. Escape, the
/// window's close button, and clicking outside all resolve one, and the
/// convention across desktops is that the affirmative button is the default.
/// None of that may be true here: absence of an answer is never consent. A page
/// with Back has no affirmative default and no dismissal that decides anything,
/// so leaving is leaving — the request stays `Pending`, the run stays
/// `WaitingForApproval`, and the banner that led here is still there.
///
/// **Nothing on the way out calls `approve`.** There is no `Component.onDestruction`
/// handler, no `onVisibleChanged`, no `onClosed`, and no `StandardButton` on
/// this page; the only two call sites are the two buttons a person presses, and
/// the offscreen fixture destroys a page mid-review and re-reads the store to
/// prove the request is untouched.
///
/// # This window holds no authority
///
/// The scopes offered are the record's own `grantableScopes` — the runtime's
/// `ApprovalRequest::grantable_scopes`, carried through the bridge — so the
/// combo box cannot express a breadth `decide` would refuse. It refuses one
/// anyway if a modified client asks, and it refuses a lapsed or already
/// answered request too; when it does, the structured refusal is displayed
/// here rather than swallowed. A disabled Approve on this page is a courtesy,
/// not the enforcement.
///
/// # Everything shown here is untrusted
///
/// The tool identifier, the summary, the workspace path, the capability list
/// and the validated input are written by tools, by agents, or by the
/// repository. Every label rendering one is `Text.PlainText`, and the controls
/// that render rich text whatever they are told — `Kirigami.InlineMessage` —
/// are only ever handed text through `escapedRichText`.
Kirigami.Page {
    id: page

    /// The request to answer.
    ///
    /// Read once, at construction. This page is never re-targeted: `showApproval`
    /// pops the surface it is replacing and pushes a new one, because a review
    /// surface that changed subject under a reader would be the same mistake as
    /// a dialog that answers on close — the thing on screen would stop being the
    /// thing they were reading.
    required property string approvalId
    /// The run it belongs to, which is what this page re-reads.
    required property string runId
    /// The request as whoever opened this page already knew it — a pending
    /// queue row, or one of a run's recorded approvals. Used only until this
    /// page's own read lands, so the surface has fields to draw immediately
    /// rather than a blank frame; `loaded` supersedes it the moment it arrives.
    property var seed: null
    /// The Git and catalog bridge, used here for the clipboard alone.
    property var backend: null

    /// Lets a host recognize this page in a stack, as `isRunDetail` does.
    readonly property bool isApprovalReview: true

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

    RunState {
        id: runState
    }

    RunsBackend {
        id: runs
    }

    // --- What this page is looking at -------------------------------------

    readonly property var run: runs.run !== undefined && runs.run !== null ? runs.run : ({})
    /// True once the loaded projection is the run this page was opened for.
    readonly property bool runReady: String(run.runId || "") === page.runId

    /// This request as the run's own record has it, or null before the read.
    readonly property var loaded: {
        if (!runReady || run.approvals === undefined)
            return null;
        for (let index = 0; index < run.approvals.length; ++index) {
            if (String(run.approvals[index].approvalId) === page.approvalId)
                return run.approvals[index];
        }
        return null;
    }

    /// The record this page draws, newest reading first.
    ///
    /// One derived property rather than two sources a delegate picks between:
    /// a surface that read some fields off the seed and others off the load
    /// would show a decision that has landed beside a state that has not.
    readonly property var request: page.loaded !== null
        ? page.loaded
        : (page.seed !== undefined && page.seed !== null ? page.seed : null)

    readonly property bool ready: page.request !== null

    /// One field of the record, or empty when the reading this page has does
    /// not carry it.
    ///
    /// A seed is whatever the surface that opened this page already had, and
    /// the pending queue's rows carry a subset of what a run's own approval
    /// record does. `String(undefined)` is the five-character word "undefined",
    /// which a `MetaField` would show as happily as a real value, so every
    /// field on this page is read through here.
    function field(name) {
        if (page.request === null)
            return "";
        const value = page.request[name];
        return value === undefined || value === null ? "" : String(value);
    }
    readonly property string headerTitle: page.ready
        ? qsTr("Approve %1").arg(page.field("tool"))
        : qsTr("Approval")

    /// The request's lifecycle state.
    ///
    /// Defaulted to `pending` while only the seed is available: the pending
    /// queue's rows carry no state role, because a request leaves that queue
    /// the moment it is answered. The loaded record always carries one.
    readonly property string requestState: page.ready
        ? (page.field("state").length > 0 ? page.field("state") : "pending")
        : ""
    readonly property bool pending: page.requestState === "pending"

    /// The reader's clock, ticked so a deadline passing is visible without a
    /// reload. One reading shared by every binding below, the way `since` takes
    /// one for a whole list.
    property real now: 0

    readonly property string expires: page.field("expires")
    /// Whether the deadline for answering has passed.
    ///
    /// A lapsed request stays `pending` in the store until something expires
    /// it, so this is the clock disagreeing with the row — and the reason
    /// Approve is withdrawn while the row still says `pending`.
    readonly property bool lapsed: runState.hasLapsed(page.expires, page.now)

    /// Whether this request is one an answer can still be given for.
    readonly property bool decidable: page.pending && !page.lapsed

    /// Every breadth the runtime would accept an answer at, narrowest first.
    readonly property var grantableScopes: page.ready
            && page.request.grantableScopes !== undefined
        ? page.request.grantableScopes
        : []
    /// Whether there is a breadth to choose *between*. False for a request the
    /// risk ceiling already reduced to one call, which is every `RemoteWrite`
    /// and `Destructive` request — the control is not rendered at all rather
    /// than rendered with one entry.
    readonly property bool scopeChoiceAllowed: page.grantableScopes.length > 1

    /// The offered set as one comparable value.
    ///
    /// `grantableScopes` is a `var` derived from `request`, so it is a *new*
    /// array every time the seed is superseded by this page's own read — with
    /// identical contents. Watching the array would reset a reader's choice
    /// underneath them a frame after they made it; watching this does not.
    readonly property string scopeSignature: page.grantableScopes.join(",")
    onScopeSignatureChanged: scopeChoice.currentIndex = 0

    /// The breadth the decision row is on, read off the control rather than
    /// mirrored into a property of this page.
    ///
    /// A `currentIndex` bound to a page property and *also* written from
    /// `onActivated` is a two-way binding Qt Quick resolves by destroying the
    /// declarative half the first time a reader picks something — after which
    /// the control and the page can disagree about what Approve would send,
    /// which is the one thing this page may not get wrong.
    readonly property string chosenScope: {
        const index = page.scopeChoiceAllowed ? scopeChoice.currentIndex : 0;
        return index >= 0 && index < page.grantableScopes.length
            ? String(page.grantableScopes[index]) : "";
    }

    /// Whether the raw input is on screen.
    property bool rawExpanded: false

    /// The validated input this request is holding, empty until it is asked
    /// for — and only while it is *this* request's: the bridge is shared, and
    /// an input left standing under another question is the worst way for that
    /// property to be wrong.
    readonly property string input: {
        if (!page.rawExpanded || runs.detail === undefined || runs.detail === null)
            return "";
        return String(runs.detail.approvalId || "") === page.approvalId
            ? String(runs.detail.input) : "";
    }
    readonly property bool inputCut: page.input.length > 0
        && runs.detail !== undefined && runs.detail !== null
        && runs.detail.truncated === true

    /// How the last decision this page issued went, straight off the bridge.
    ///
    /// Read from `outcome` rather than captured from `status` when `busy`
    /// falls: `busy` is a count across every operation the bridge has
    /// outstanding, so a load overlapping a decision — pressing "Show input"
    /// while Approve is in flight is enough — would let the load's success be
    /// read as the decision's, and a refusal would reach nobody. `outcome` is
    /// written by mutations alone and superseded only by another one.
    readonly property string decisionKind: runs.outcome !== undefined
            && runs.outcome !== null
        ? String(runs.outcome.kind || "") : ""
    readonly property string decisionMessage: runs.outcome !== undefined
            && runs.outcome !== null
        ? String(runs.outcome.message || "") : ""

    /// The discriminant of the failure now on screen, empty when there is
    /// none. A refused decision is the whole reason this exists: the runtime is
    /// the authority and its refusal has to reach the reader rather than being
    /// swallowed by whatever the page did next.
    readonly property string failureKind: page.decisionKind.length > 0
        ? page.decisionKind : String(runs.kind || "")
    readonly property string failureMessage: page.decisionKind.length > 0
        ? page.decisionMessage : String(runs.status || "")

    /// Whether a decision this page issued is still outstanding.
    property bool deciding: false

    /// Exposed for the offscreen fixture, which asserts that the control state
    /// a press changes is a property of this page rather than of the bridge.
    readonly property alias approveEnabled: approveButton.enabled
    readonly property alias denyEnabled: denyButton.enabled

    // --- Reads and decisions ----------------------------------------------

    function reload() {
        runs.loadRun(page.runId);
    }

    /// Shows or hides the canonical input the approval hash binds, fetching it
    /// the first time somebody asks to see one. A request nobody expanded
    /// costs no read: the page opens on the row's summary alone.
    function toggleRawInput() {
        page.rawExpanded = !page.rawExpanded;
        if (page.rawExpanded)
            runs.loadApprovalInput(page.approvalId);
    }

    /// Chooses one of the offered breadths, by index.
    ///
    /// The offscreen fixture drives this rather than reaching into the control,
    /// because what it is checking is that what the page would send follows
    /// what the reader picked.
    function chooseScope(index) {
        scopeChoice.currentIndex = index;
    }

    /// Grants the request at the chosen breadth.
    ///
    /// One of exactly two call sites for `approve` on this page, and the other
    /// is the button below. Nothing about navigation, destruction, or window
    /// state reaches here.
    function approve() {
        if (!page.decidable)
            return;
        page.deciding = true;
        runs.approve(page.approvalId, page.chosenScope, reasonField.text);
    }

    function deny() {
        if (!page.decidable)
            return;
        page.deciding = true;
        runs.deny(page.approvalId, reasonField.text);
    }

    Component.onCompleted: {
        page.now = Date.now();
        page.reload();
    }

    /// A deadline is wall-clock, so it passes while nobody touches anything.
    /// One second is finer than any deadline a person is given and coarse
    /// enough to cost nothing.
    Timer {
        interval: 1000
        repeat: true
        running: page.decidable
        triggeredOnStart: true
        onTriggered: page.now = Date.now()
    }

    Connections {
        /// A decision is answered by the durable state it changed rather than
        /// by the message it returned, so the page re-reads when one settles —
        /// which is also how a refusal ends up displayed beside a request that
        /// is still pending.
        ///
        /// Keyed on `outcome` and not on `busy`. `busy` says only that *some*
        /// operation finished, so a load overlapping a decision would have this
        /// page read the load's answer as the decision's; `outcome` is written
        /// by a mutation and by nothing else.
        function onOutcomeChanged() {
            page.deciding = false;
            // The question this input belonged to has been answered, and the
            // bridge blanks `detail` with the decision — so the view closes
            // rather than sitting open over nothing it can refill.
            page.rawExpanded = false;
            page.reload();
        }

        target: runs
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

                    // Back, and nothing else. Leaving decides nothing, which is
                    // why this is the one navigation control on the page.
                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Back; the request stays unanswered")
                        Controls.ToolTip.visible: hovered
                        display: Controls.AbstractButton.IconOnly
                        icon.name: "go-previous-symbolic"
                        objectName: "approvalBack"
                        text: qsTr("Back")
                        onClicked: applicationWindow().pageStack.pop()
                    }

                    Kirigami.Heading {
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        level: 3
                        // A tool identifier the runtime recorded.
                        text: page.ready ? page.field("tool") : qsTr("Loading the request…")
                        textFormat: Text.PlainText
                    }

                    StatePill {
                        pillColor: runState.riskColor(page.field("risk"))
                        text: page.ready ? runState.riskLabel(page.field("risk")) : ""
                    }

                    StatePill {
                        pillColor: page.lapsed
                            ? runState.dimColor
                            : runState.approvalStateColor(page.requestState)
                        // A lapsed request is still stored as pending, and
                        // saying "waiting for an answer" over a question that
                        // can no longer be answered would be the wrong word.
                        text: {
                            if (!page.ready)
                                return "";
                            return page.lapsed && page.pending
                                ? qsTr("Too late to answer")
                                : runState.approvalStateLabel(page.requestState);
                        }
                    }
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.largeSpacing
                    visible: page.ready

                    MetaField {
                        name: qsTr("Version")
                        value: page.field("toolVersion")
                    }

                    MetaField {
                        name: qsTr("Asked")
                        value: runState.localTime(page.field("requested"))
                    }

                    MetaField {
                        name: qsTr("Answer by")
                        value: runState.localTime(page.expires)
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
                        // A filesystem path out of the approval's binding.
                        text: page.field("workspace")
                        textFormat: Text.PlainText
                    }

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Open the run this request belongs to")
                        Controls.ToolTip.visible: hovered
                        objectName: "approvalOpenRun"
                        text: page.runReady && String(page.run.title || "").length > 0
                            ? String(page.run.title)
                            : qsTr("Open the run")
                        onClicked: applicationWindow().showRun(page.runId)
                    }
                }
            }
        }

        Rectangle {
            Layout.fillWidth: true
            color: runState.frameColor
            implicitHeight: 1
        }

        // --- Body -----------------------------------------------------------

        Controls.ScrollView {
            Layout.fillHeight: true
            Layout.fillWidth: true
            clip: true

            Controls.ScrollBar.horizontal.policy: Controls.ScrollBar.AlwaysOff

            ColumnLayout {
                // Bound to the view rather than filling it: a ScrollView sizes
                // its content from the item inside, and an item that filled the
                // view would make its own height depend on a height derived
                // from it.
                spacing: Kirigami.Units.largeSpacing
                width: page.width

                // The bridge's own message, which may quote a workspace path or
                // carry a refusal out of the runtime's namespace.
                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    objectName: "approvalFailure"
                    text: runState.escapedRichText(page.failureMessage)
                    type: Kirigami.MessageType.Error
                    // Hidden while a fresh decision is in flight: the outcome
                    // still standing is the previous answer's, and leaving it
                    // up would read as this one's.
                    visible: page.failureKind.length > 0 && !page.deciding
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    text: runState.escapedRichText(qsTr("This request was not answered in time. Nothing here can grant it now; the run it belongs to can be cancelled or re-attempted."))
                    type: Kirigami.MessageType.Warning
                    visible: page.lapsed && page.pending
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    text: runState.escapedRichText(qsTr("The risk of this request reduced it to a single call, so approving it authorizes exactly this one and nothing that follows."))
                    type: Kirigami.MessageType.Information
                    visible: page.ready && page.request.downgraded === true
                }

                // --- What is about to happen -------------------------------

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        color: runState.dimColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("What this would do")
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.bodyColor
                        objectName: "approvalSummary"
                        // The request's own summary, written by the tool layer
                        // when the call was classified.
                        text: page.field("summary")
                        textFormat: Text.PlainText
                        visible: text.length > 0
                        wrapMode: Text.WordWrap
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("This tool published no summary of its own. Read the input below before answering.")
                        textFormat: Text.PlainText
                        visible: page.ready && page.field("summary").length === 0
                        wrapMode: Text.WordWrap
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.largeSpacing

                        MetaField {
                            name: qsTr("Capabilities")
                            value: page.field("capabilities")
                        }

                        MetaField {
                            name: qsTr("Asked for")
                            value: page.ready && page.request.downgraded === true
                                ? runState.scopeLabel(page.field("requestedScope"))
                                : ""
                        }

                        Item {
                            Layout.fillWidth: true
                        }
                    }
                }

                // --- The exact input the hash binds -------------------------

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    spacing: Kirigami.Units.smallSpacing

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Controls.Label {
                            Layout.fillWidth: true
                            color: runState.dimColor
                            font.capitalization: Font.AllUppercase
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: qsTr("The exact input this answer is bound to")
                            textFormat: Text.PlainText
                        }

                        Controls.Button {
                            enabled: !page.deciding
                            objectName: "approvalRawToggle"
                            text: page.rawExpanded ? qsTr("Hide input") : qsTr("Show input")
                            onClicked: page.toggleRawInput()
                        }
                    }

                    // Spinning while a read is actually outstanding, rather
                    // than while the answer happens to be empty: an empty
                    // answer is also what a blanked `detail` looks like.
                    Controls.BusyIndicator {
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        running: page.rawExpanded && page.input.length === 0
                            && page.failureKind.length === 0 && runs.busy
                        visible: running
                    }

                    BoundedText {
                        Layout.fillWidth: true
                        clipboard: page.backend
                        // The recorded call's own input, already redacted at
                        // the persistence boundary and clamped by the bridge.
                        content: page.input
                        cut: page.inputCut
                        visible: page.rawExpanded && page.input.length > 0
                    }
                }

                // --- The decision -------------------------------------------

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.bottomMargin: Kirigami.Units.largeSpacing
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    spacing: Kirigami.Units.smallSpacing
                    visible: page.decidable

                    Controls.Label {
                        color: runState.dimColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Your decision")
                        textFormat: Text.PlainText
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Controls.Label {
                            color: runState.dimColor
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: qsTr("Approving covers")
                            textFormat: Text.PlainText
                        }

                        // Rendered only when there is something to choose
                        // between. A request the risk ceiling reduced to one
                        // call has exactly one answer, and a combo box holding
                        // it would imply a breadth that was never on offer.
                        // `currentIndex` is deliberately left unbound; see
                        // `chosenScope`. The narrowest breadth is index zero,
                        // which is where a fresh model starts and where
                        // `onScopeSignatureChanged` puts it back.
                        Controls.ComboBox {
                            id: scopeChoice

                            Layout.preferredWidth: Kirigami.Units.gridUnit * 18
                            model: page.grantableScopes.map(
                                scope => runState.scopeLabel(String(scope)))
                            objectName: "approvalScopeChoice"
                            visible: page.scopeChoiceAllowed
                        }

                        Controls.Label {
                            color: runState.bodyColor
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: runState.scopeLabel(page.chosenScope)
                            textFormat: Text.PlainText
                            visible: !page.scopeChoiceAllowed
                        }

                        Item {
                            Layout.fillWidth: true
                        }
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        objectName: "approvalScopeExplanation"
                        text: runState.scopeExplanation(page.chosenScope)
                        textFormat: Text.PlainText
                        wrapMode: Text.WordWrap
                    }

                    Controls.TextField {
                        id: reasonField

                        Layout.fillWidth: true
                        objectName: "approvalReason"
                        placeholderText: qsTr("Why (recorded with the decision, optional)")
                        // Enter must not decide anything. The field accepts a
                        // reason and the buttons below decide; a text field
                        // that submitted on Return would make the fastest path
                        // through this page the one that reads nothing.
                        Keys.onReturnPressed: event => event.accepted = true
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Item {
                            Layout.fillWidth: true
                        }

                        // Neither button takes focus on arrival, and neither is
                        // a default: the page opens with nothing armed, so no
                        // key press that a reader did not aim decides anything.
                        Controls.Button {
                            id: denyButton

                            enabled: !runs.busy && page.decidable
                            icon.name: "dialog-cancel"
                            objectName: "approvalDeny"
                            text: qsTr("Deny")
                            onClicked: page.deny()
                        }

                        Controls.Button {
                            id: approveButton

                            enabled: !runs.busy && page.decidable
                            icon.name: "dialog-ok"
                            objectName: "approvalApprove"
                            text: page.deciding ? qsTr("Deciding…") : qsTr("Approve")
                            onClicked: page.approve()
                        }
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("Leaving this page answers nothing. The request stays open until it is approved, denied, cancelled with its run, or lapses.")
                        textFormat: Text.PlainText
                        wrapMode: Text.WordWrap
                    }
                }

                // --- The answer, once there is one --------------------------

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.bottomMargin: Kirigami.Units.largeSpacing
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.rightMargin: Kirigami.Units.largeSpacing
                    spacing: Kirigami.Units.smallSpacing
                    visible: page.ready && !page.pending

                    Controls.Label {
                        color: runState.dimColor
                        font.capitalization: Font.AllUppercase
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: qsTr("How it was answered")
                        textFormat: Text.PlainText
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.largeSpacing

                        MetaField {
                            name: qsTr("Answer")
                            value: runState.approvalStateLabel(page.requestState)
                        }

                        MetaField {
                            name: qsTr("Covering")
                            value: page.requestState === "granted"
                                ? runState.scopeLabel(page.field("scope")) : ""
                        }

                        MetaField {
                            name: qsTr("Given")
                            value: page.field("decidedVia").length > 0
                                ? runState.decidedViaLabel(page.field("decidedVia")) : ""
                        }

                        MetaField {
                            name: qsTr("At")
                            value: runState.localTime(page.field("decidedAt"))
                        }

                        Item {
                            Layout.fillWidth: true
                        }
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.bodyColor
                        objectName: "approvalRecordedReason"
                        // A reason a person typed when they answered.
                        text: page.field("reason")
                        textFormat: Text.PlainText
                        visible: text.length > 0
                        wrapMode: Text.WordWrap
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: runState.dimColor
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        // The three states nobody answered. Saying so is the
                        // point: the record deliberately carries no decision
                        // for them, and a surface that implied one would make
                        // the audit claim a refusal that was never made.
                        text: {
                            if (page.requestState === "cancelled")
                                return qsTr("Nobody answered this. The run it belonged to was cancelled, which closed the question.");
                            if (page.requestState === "expired")
                                return qsTr("Nobody answered this before its deadline, so it closed unanswered.");
                            if (page.requestState === "superseded")
                                return qsTr("Nobody answered this. The process driving the run stopped, and a request nobody can answer any more is closed rather than left open.");
                            return "";
                        }
                        textFormat: Text.PlainText
                        visible: text.length > 0
                        wrapMode: Text.WordWrap
                    }
                }
            }
        }
    }
}
