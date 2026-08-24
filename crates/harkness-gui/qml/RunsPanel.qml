import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

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
    /// Runs still in flight, which is the only count worth interrupting a
    /// reader for; a finished run is history and history does not need a badge.
    readonly property int viewBadge: 0
    /// Available whether or not the project is on disk: run history is evidence
    /// about work that has already happened, and a checkout that went missing
    /// is a reason to read it rather than a reason to hide it. This is also why
    /// the view takes no `project` — see `RunListPane` on why every run is
    /// listed — and no `backend`, since it reaches Git and the catalog for
    /// nothing.
    readonly property bool viewAvailable: true

    implicitWidth: Kirigami.Units.gridUnit * 34

    // Shell chrome rather than a document, so the dark surface is restated the
    // way ProjectShellPage states it.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    /// Re-reads the list.
    ///
    /// The entry point for a host that has just started a run. Nothing in the
    /// shell calls it yet — the checks view reports no such moment — so until
    /// one does, the pane's own Refresh button is what brings the list current.
    function refresh() {
        runList.refresh();
    }

    /// Opens one run's detail.
    ///
    /// The single route out of this view: a row's activation and anything else
    /// that names a run both come through here, so the page the reader lands on
    /// is the same one the launcher's recent runs open.
    function openRun(runId) {
        applicationWindow().showRun(runId);
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

            RunListPane {
                id: runList

                Layout.fillHeight: true
                Layout.fillWidth: true
                onRunActivated: runId => panel.openRun(runId)
            }
        }
    }
}
