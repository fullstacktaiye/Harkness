import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

/// The compact "something is waiting for you" strip.
///
/// Two hosts show one of these and they ask slightly different questions. A run
/// detail page has *the* request its run is parked on and names it; the project
/// shell has a queue across every run and names how many. Both reduce to the
/// same strip, so one file draws it and neither host grows a second opinion
/// about what a waiting approval looks like.
///
/// # It names a question and never answers one
///
/// There is no Approve here, and adding one would be a mistake rather than a
/// convenience: a decision is given against the request a person has actually
/// read, and this strip deliberately shows a summary the tool wrote rather than
/// the canonical input the approval hash binds. `Review` is the only action,
/// and it opens `ApprovalPage`, which is where every field that matters is.
///
/// It also holds no model. Whoever shows one owns the read behind it — the run
/// projection for a page about one run, the pending queue for the shell — so
/// this cannot be the thing that decides how often SQLite is opened.
Rectangle {
    id: banner

    /// The one unanswered request to name, or null to name only a count.
    property var request: null
    /// How many requests are waiting, when the host is showing a queue.
    property int count: 0
    /// The reader's clock, for saying that a deadline has passed. Supplied by
    /// the host so a surface with several of these agrees with itself about
    /// what "now" is; zero means nothing is ticking and nothing has lapsed.
    property real now: 0

    /// Somebody asked to see the whole request. The host decides where that
    /// goes, because a page is pushed onto a stack this component cannot see.
    signal reviewRequested

    readonly property string requestExpiry: banner.request !== null
        ? String(banner.request.expires || "") : ""
    /// Whether the named request's deadline for an answer has passed.
    readonly property bool lapsed: banner.now > 0
        && vocabulary.hasLapsed(banner.requestExpiry, banner.now)

    border.color: Qt.alpha(banner.lapsed ? vocabulary.dimColor : vocabulary.neutralColor, 0.7)
    border.width: 1
    color: Qt.alpha(banner.lapsed ? vocabulary.dimColor : vocabulary.neutralColor, 0.12)
    implicitHeight: bannerBody.implicitHeight + Kirigami.Units.largeSpacing
    radius: Kirigami.Units.smallSpacing
    visible: banner.request !== null || banner.count > 0

    RunState {
        id: vocabulary
    }

    ColumnLayout {
        id: bannerBody

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
                color: vocabulary.bodyColor
                elide: Text.ElideRight
                // A tool identifier the runtime recorded, or a plain count.
                text: {
                    if (banner.request !== null) {
                        return banner.lapsed
                            ? qsTr("%1 waited too long for a decision")
                                .arg(String(banner.request.tool))
                            : qsTr("%1 is waiting for a decision")
                                .arg(String(banner.request.tool));
                    }
                    return banner.count === 1
                        ? qsTr("One request is waiting for a decision")
                        : qsTr("%1 requests are waiting for a decision").arg(banner.count);
                }
                textFormat: Text.PlainText
            }

            StatePill {
                pillColor: vocabulary.riskColor(banner.request !== null
                    ? String(banner.request.risk) : "")
                text: banner.request !== null
                    ? vocabulary.riskLabel(String(banner.request.risk)) : ""
            }

            Controls.Button {
                objectName: "approvalBannerReview"
                text: banner.request !== null ? qsTr("Review request") : qsTr("Review requests")
                onClicked: banner.reviewRequested()
            }
        }

        Controls.Label {
            Layout.fillWidth: true
            color: vocabulary.bodyColor
            // The request's own bounded summary, written by the tool layer.
            text: banner.request !== null ? String(banner.request.summary) : ""
            textFormat: Text.PlainText
            visible: text.length > 0
            wrapMode: Text.WordWrap
        }
    }
}
