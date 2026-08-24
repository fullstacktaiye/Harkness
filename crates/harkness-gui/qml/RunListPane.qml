import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// Recorded runs, newest first, as a virtualized list.
///
/// The list body both the project shell's Runs view and the launcher's recent
/// runs show. It is its own component rather than duplicated in the two, so a
/// row reads the same way whichever surface opened it and both reach the same
/// detail page.
///
/// # Why every run is listed, whatever project is open
///
/// `RunListModel` pages by key through the whole store: a page is a window on
/// the recorded order rather than a query, and the run store has no per-project
/// listing. Hiding the rows of other projects would therefore hide runs rather
/// than exclude them — a page of fifty runs from another workspace would render
/// as an empty list with more history behind it and nothing saying so, and the
/// "load older" button would look broken. Each row names the workspace it ran
/// in instead, and the view says plainly that this is every recorded run.
Item {
    id: pane

    /// Whether the compact presentation is used, for the launcher's column:
    /// no toolbar of its own, because the section around it carries one.
    property bool compact: false

    /// Emitted when a row is opened.
    signal runActivated(string runId)

    readonly property alias count: runList.count
    readonly property bool loading: runs.loading
    readonly property string loadError: String(runs.status || "")
    readonly property string loadErrorKind: String(runs.kind || "")

    /// One reading of the clock every relative timestamp in the list shares, so
    /// fifty rows cannot disagree about what "now" is.
    property double now: Date.now()

    Timer {
        interval: 30000
        repeat: true
        running: pane.visible
        onTriggered: pane.now = Date.now()
    }

    function refresh() {
        runs.refresh();
        pane.now = Date.now();
    }

    RunState {
        id: runState
    }

    RunListModel {
        id: runs

        Component.onCompleted: refresh()
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        RowLayout {
            Layout.bottomMargin: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.smallSpacing
            spacing: Kirigami.Units.smallSpacing
            visible: !pane.compact

            Controls.Label {
                Layout.fillWidth: true
                color: runState.dimColor
                elide: Text.ElideRight
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: qsTr("Every run Harkness has recorded, newest first.")
                textFormat: Text.PlainText
            }

            Controls.BusyIndicator {
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                running: runs.loading
                visible: running
            }

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Re-read recorded runs")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: !runs.loading
                icon.name: "view-refresh-symbolic"
                text: qsTr("Refresh")
                onClicked: pane.refresh()
            }
        }

        Kirigami.InlineMessage {
            Layout.bottomMargin: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.smallSpacing
            Layout.rightMargin: Kirigami.Units.smallSpacing
            // A runtime failure carrying a workspace path or a store message;
            // InlineMessage renders rich text whatever it is told.
            text: runState.escapedRichText(pane.loadError)
            type: Kirigami.MessageType.Error
            visible: pane.loadErrorKind.length > 0
        }

        ListView {
            id: runList

            Layout.fillHeight: true
            Layout.fillWidth: true
            // Reachable by Tab rather than grabbing focus: the pane shares a
            // window with a source-control view and a search field, and a list
            // that took focus on load would take it from whatever the reader
            // was typing in.
            activeFocusOnTab: true
            // Delegates are reused rather than rebuilt, and a screenful either
            // side is kept ready, so scrolling a long history creates nothing.
            cacheBuffer: Math.max(height * 2, Kirigami.Units.gridUnit * 20)
            clip: true
            currentIndex: -1
            keyNavigationEnabled: true
            model: runs
            reuseItems: true

            Controls.ScrollBar.vertical: Controls.ScrollBar {}

            Keys.onEnterPressed: runList.openCurrent()
            Keys.onReturnPressed: runList.openCurrent()

            function openCurrent() {
                if (currentIndex >= 0 && currentItem)
                    pane.runActivated(currentItem.runId);
            }

            delegate: Controls.ItemDelegate {
                id: runRow

                required property int index
                required property string runId
                required property string title
                required property string state
                required property string created
                required property string finished
                required property string workspace
                required property string errorKind
                required property string retryOf
                required property bool workspaceModified

                readonly property color accent: runState.stateColor(state)
                /// A run whose task row could not be read has no title, which is
                /// exactly the run somebody needs to find; it is named by its
                /// identity rather than rendered as a blank line.
                readonly property string displayTitle: title.length > 0
                    ? title
                    : qsTr("Run %1").arg(runId)

                Accessible.name: qsTr("%1: %2")
                    .arg(runRow.displayTitle)
                    .arg(runState.stateLabel(runRow.state))
                hoverEnabled: true
                implicitHeight: rowBody.implicitHeight + Kirigami.Units.largeSpacing
                leftPadding: Kirigami.Units.largeSpacing
                rightPadding: Kirigami.Units.smallSpacing
                width: ListView.view.width
                onClicked: {
                    runList.currentIndex = runRow.index;
                    pane.runActivated(runRow.runId);
                }

                background: Rectangle {
                    color: runRow.ListView.isCurrentItem
                        ? Qt.rgba(1, 1, 1, 0.08)
                        : runRow.hovered
                            ? Qt.rgba(1, 1, 1, 0.045)
                            : "transparent"

                    Rectangle {
                        anchors.bottom: parent.bottom
                        anchors.left: parent.left
                        anchors.top: parent.top
                        color: runRow.accent
                        visible: runRow.ListView.isCurrentItem
                        width: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
                    }
                }

                contentItem: ColumnLayout {
                    id: rowBody

                    spacing: 0

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: Kirigami.Units.smallSpacing

                        Rectangle {
                            Layout.alignment: Qt.AlignVCenter
                            color: runRow.accent
                            implicitHeight: Kirigami.Units.smallSpacing
                            implicitWidth: Kirigami.Units.smallSpacing
                            radius: width / 2
                            visible: !runState.pending(runRow.state)
                        }

                        Controls.BusyIndicator {
                            Layout.alignment: Qt.AlignVCenter
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            running: runState.pending(runRow.state)
                            visible: running
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            color: runState.bodyColor
                            elide: Text.ElideRight
                            // A task title is written by whoever started the run.
                            text: runRow.displayTitle
                            textFormat: Text.PlainText
                        }

                        Controls.Label {
                            color: runState.dimColor
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: runState.since(
                                runRow.finished.length > 0 ? runRow.finished : runRow.created,
                                pane.now)
                            textFormat: Text.PlainText
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        Layout.topMargin: Math.round(Kirigami.Units.smallSpacing / 2)
                        spacing: Kirigami.Units.smallSpacing

                        Controls.Label {
                            color: runRow.accent
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: runState.stateLabel(runRow.state)
                            textFormat: Text.PlainText
                        }

                        Controls.Label {
                            color: runState.negativeColor
                            font.family: "monospace"
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            // A structured discriminant, not a message: the
                            // message is on the detail page where it has room.
                            text: runRow.errorKind
                            textFormat: Text.PlainText
                            visible: runRow.errorKind.length > 0
                        }

                        Controls.Label {
                            color: runState.neutralColor
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: qsTr("re-attempt")
                            textFormat: Text.PlainText
                            visible: runRow.retryOf.length > 0
                        }

                        Controls.Label {
                            color: runState.neutralColor
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            text: qsTr("workspace may be modified")
                            textFormat: Text.PlainText
                            visible: runRow.workspaceModified
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            color: runState.dimColor
                            elide: Text.ElideMiddle
                            font.family: "monospace"
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            horizontalAlignment: Text.AlignRight
                            // A filesystem path out of a task record.
                            text: runRow.workspace
                            textFormat: Text.PlainText
                        }
                    }
                }
            }

            // Paging is a button rather than a scroll trigger: a store read is
            // not free, and it is the `historyState.hasMore` pattern the history
            // panel already established.
            footer: Item {
                implicitHeight: loadMore.visible
                    ? loadMore.implicitHeight + Kirigami.Units.largeSpacing * 2
                    : 0
                width: runList.width

                Controls.Button {
                    id: loadMore

                    anchors.centerIn: parent
                    enabled: !runs.loading
                    text: qsTr("Load older runs")
                    visible: runs.more
                    onClicked: runs.loadMore()
                }
            }
        }

        Kirigami.PlaceholderMessage {
            Layout.alignment: Qt.AlignHCenter
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            explanation: qsTr("A run appears here as soon as one is started from this window or from the command line.")
            icon.name: "view-list-symbolic"
            text: qsTr("No runs recorded yet")
            visible: runList.count === 0 && !runs.loading && pane.loadErrorKind.length === 0
        }
    }
}
