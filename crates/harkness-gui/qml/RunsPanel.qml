import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import QtQuick.Window
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// The project shell's Runs view: recorded runs, and the way into one's detail.
///
/// A side-panel view rather than a third pane beside the source-control column.
/// The panel column is already capped, and a runs list stacked under one of the
/// existing views would starve both; the activity bar is where a top-level
/// surface belongs, and switching to it keeps whatever the source-control view
/// had scrolled to.
///
/// The view holds no run logic of its own. Opening a row asks the window to push
/// `RunDetailPage`, which is the same page the launcher's recent-runs section
/// opens, so one run has one detail surface however it was reached.
///
/// # Why the pending queue sits above the history
///
/// A run parked on an approval is not history: it is stopped, and it stays
/// stopped until a person answers. That is the one thing in this view worth
/// interrupting a reader for, so it is the badge the activity bar carries and
/// the first thing the panel shows — above the list rather than inside it,
/// because the list is ordered by when a run started and the newest question is
/// not necessarily the newest run.
Item {
    id: panel

    signal hideRequested()

    // Side-panel view contract; see SidePanel.qml.
    readonly property string viewId: "runs"
    readonly property string viewTitle: qsTr("Runs")
    /// A list with a play mark: recorded work that was executed. It stays apart
    /// from `run-build`, which the checks view already uses for *running* one.
    readonly property string viewIcon: "view-list-symbolic"
    readonly property string viewShortcut: "Ctrl+Shift+R"
    /// Questions waiting for a person, which is the only count worth
    /// interrupting a reader for; a finished run is history and history does
    /// not need a badge, and a run still executing needs nothing from anybody.
    readonly property int viewBadge: approvals.count
    /// The badge counts things that are *waiting*, not things that went wrong,
    /// so it takes the neutral warning ground rather than the negative one the
    /// checks view uses.
    readonly property color viewBadgeColor: Kirigami.Theme.neutralTextColor
    /// Available whether or not the project is on disk: run history is evidence
    /// about work that has already happened, and a checkout that went missing
    /// is a reason to read it rather than a reason to hide it. This is also why
    /// the view takes no `project` — see `RunListPane` on why every run is
    /// listed — and no `backend`, since it reaches Git and the catalog for
    /// nothing.
    readonly property bool viewAvailable: true

    /// How many requests are waiting, for a host drawing its own indicator.
    readonly property alias pendingApprovals: approvals.count
    /// How many rows the queue actually built, which is not the same claim:
    /// the count is the model's and this is the view's, and the offscreen
    /// fixture checks that a queue of four is four rows rather than a number.
    readonly property alias pendingRows: queue.count

    /// One built queue row, for the fixture that drives its action.
    function pendingRow(index) {
        return queue.itemAt(index);
    }

    /// The reader's clock, refreshed with the queue so every row agrees about
    /// which deadlines have passed. Zero until the first read.
    property real now: 0

    implicitWidth: Kirigami.Units.gridUnit * 34

    // Shell chrome rather than a document, so the dark surface is restated the
    // way ProjectShellPage states it.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    /// Re-reads the list and the queue above it.
    ///
    /// The entry point for a host that has just started a run — the checks view
    /// is the one that reports such a moment — and what the pane's own Refresh
    /// button calls.
    function refresh() {
        runList.refresh();
        approvals.refresh();
    }

    /// Opens one run's detail.
    ///
    /// The single route out of this view: a row's activation and anything else
    /// that names a run both come through here, so the page the reader lands on
    /// is the same one the launcher's recent runs open.
    function openRun(runId) {
        applicationWindow().showRun(runId);
    }

    /// Opens one pending request's review surface.
    ///
    /// The row is handed on as the page's seed so its header draws immediately;
    /// the page re-reads the run regardless, and what it reads wins.
    function openApproval(row) {
        applicationWindow().showApproval(String(row.approvalId), String(row.runId), row);
    }

    RunState {
        id: vocabulary
    }

    /// The unanswered queue across every run. Unpaged, because a request exists
    /// only while a call is parked on it and the scheduler caps how many calls
    /// are in flight.
    ApprovalModel {
        id: approvals
    }

    Component.onCompleted: {
        panel.now = Date.now();
        approvals.refresh();
    }

    /// Nothing pushes a new question at this window: a request is persisted by
    /// whichever worker parked on it, and this view pages the store rather than
    /// subscribing to it. Polling is therefore what makes a queue a queue — the
    /// same reason the shell polls the working tree — and it is one bounded
    /// query on a worker thread.
    ///
    /// Deliberately *not* gated on this view being on screen. `SidePanel` hosts
    /// its views in a `StackLayout`, which leaves every view but the current one
    /// `visible: false` — and the two things this count feeds, the activity
    /// bar's badge and the shell's own banner, are precisely what a reader who
    /// is looking at another view sees. Gating on visibility froze both at
    /// whatever the first read found.
    ///
    /// Gated on the window instead, exactly as the shell gates its working-tree
    /// poll: an unfocused window is looking at nothing. The attached property is
    /// read through this item because a `Timer` is not an `Item` and cannot
    /// carry one.
    Timer {
        interval: 4000
        repeat: true
        running: !approvals.loading && panel.Window.window !== null
            && panel.Window.window.active
        onTriggered: {
            panel.now = Date.now();
            approvals.refresh();
        }
    }

    /// Arriving at this view re-reads rather than waiting out the poll: a queue
    /// a reader has just switched to must not be up to one interval stale.
    onVisibleChanged: {
        if (panel.visible) {
            panel.now = Date.now();
            approvals.refresh();
        }
    }

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            PanelHeader {
                Layout.fillWidth: true
                title: panel.viewTitle
                onHideRequested: panel.hideRequested()
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                Layout.margins: Kirigami.Units.smallSpacing
                // The model's own message, which may quote a data directory.
                text: vocabulary.escapedRichText(String(approvals.status || ""))
                type: Kirigami.MessageType.Error
                visible: String(approvals.kind || "").length > 0
            }

            // The queue, bounded by the scheduler and so never a list that
            // needs paging. It is a column rather than a `ListView` for that
            // reason: a view would virtualize rows there are at most a handful
            // of, and would fight the panel's own scrolling.
            ColumnLayout {
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
                Layout.rightMargin: Kirigami.Units.smallSpacing
                Layout.topMargin: Kirigami.Units.smallSpacing
                objectName: "pendingApprovalQueue"
                spacing: Kirigami.Units.smallSpacing
                visible: approvals.count > 0

                Controls.Label {
                    color: vocabulary.dimColor
                    font.capitalization: Font.AllUppercase
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: qsTr("Waiting for a decision")
                    textFormat: Text.PlainText
                }

                Repeater {
                    id: queue

                    model: approvals

                    delegate: ApprovalBanner {
                        required property string approvalId
                        required property string capabilities
                        required property bool downgraded
                        required property string expires
                        required property var grantableScopes
                        required property string requested
                        required property string requestedScope
                        required property string risk
                        required property string runId
                        required property string scope
                        required property string summary
                        required property string tool
                        required property string toolVersion
                        required property string workspace

                        Layout.fillWidth: true
                        now: panel.now
                        // Assembled from the delegate's own roles rather than
                        // handed the model row: a QML delegate has no handle on
                        // the record behind it. Every role the queue publishes
                        // is carried, so the page this seeds draws a complete
                        // header on the frame it opens rather than filling parts
                        // of one in when its own read lands.
                        request: ({
                            "approvalId": approvalId,
                            "capabilities": capabilities,
                            "downgraded": downgraded,
                            "expires": expires,
                            "grantableScopes": grantableScopes,
                            "requested": requested,
                            "requestedScope": requestedScope,
                            "risk": risk,
                            "runId": runId,
                            "scope": scope,
                            "summary": summary,
                            "tool": tool,
                            "toolVersion": toolVersion,
                            "workspace": workspace
                        })
                        onReviewRequested: panel.openApproval(request)
                    }
                }
            }

            RunListPane {
                id: runList

                Layout.fillHeight: true
                Layout.fillWidth: true
                onRunActivated: runId => panel.openRun(runId)
            }
        }
    }
}
