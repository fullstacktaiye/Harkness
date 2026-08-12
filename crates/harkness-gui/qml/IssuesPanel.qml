import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// GitHub-style issue browsing surface. The view owns both its navigation
/// rail and its issue table, just as GitPanel owns source control and review.
///
/// `issueRows` is a plain projection supplied by HarknessBackend, keeping this
/// view independent from the authenticated GitHub CLI transport. Expected row
/// fields are documented by the smoke fixture in main.rs.
Item {
    id: panel

    required property var backend
    required property var project

    readonly property string githubRemote: String(project.githubRemote || "")
    readonly property var backendIssues: backend && backend.issues !== undefined
        ? backend.issues
        : ({})
    readonly property bool issueStateReady: String(backendIssues.projectId || "") === String(project.id)
        && String(backendIssues.remote || "") === githubRemote
    property var issueRows: issueStateReady && backendIssues.rows !== undefined
        ? backendIssues.rows
        : []
    readonly property bool loading: issueStateReady && backendIssues.loading === true
    readonly property string loadError: issueStateReady ? String(backendIssues.error || "") : ""
    readonly property bool hasMoreIssues: issueStateReady && backendIssues.hasMore === true
    readonly property bool issueLimitReached: issueStateReady && backendIssues.limitReached === true
    property string selectedScope: "issues"
    property string stateFilter: "open"
    property string searchText: ""
    property string authorFilter: ""
    property string labelFilter: ""
    property string milestoneFilter: ""
    property string assigneeFilter: ""
    property string sortOrder: "oldest"
    property var selectedIssueIds: []
    property bool initialRefreshCompleted: false

    readonly property var visibleIssues: filteredIssues()
    readonly property int visibleIssueCount: visibleIssues.length
    readonly property int openIssueCount: countByState("open")
    readonly property int closedIssueCount: countByState("closed")
    readonly property bool milestoneFilterControlVisible: milestoneFilterButton.visible
    readonly property bool assigneeFilterControlVisible: assigneeFilterButton.visible

    signal hideRequested()
    signal createIssueRequested()
    signal issueActivated(string url)

    // Side-panel view contract; see SidePanel.qml.
    readonly property string viewId: "issues"
    readonly property string viewTitle: qsTr("Issues")
    readonly property string viewIcon: "view-task"
    readonly property string viewShortcut: "Ctrl+Shift+I"
    readonly property int viewBadge: openIssueCount
    readonly property bool viewAvailable: project.available && project.isGit
        && githubRemote.length > 0

    implicitWidth: Kirigami.Units.gridUnit * 56

    // This is shell chrome, not a document that follows an editor palette.
    // Keep the same deliberate dark surface as ProjectShellPage so native
    // desktop themes cannot turn one of the two panes light.
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    Kirigami.Theme.backgroundColor: "#000000"
    Kirigami.Theme.alternateBackgroundColor: "#0d0d0d"
    Kirigami.Theme.textColor: "#ffffff"

    function refreshIssues() {
        if (!viewAvailable || !backend || typeof backend.refreshIssues !== "function")
            return;
        backend.refreshIssues(project.id, githubRemote);
    }

    function loadMoreIssues() {
        if (!hasMoreIssues || loading || !backend || typeof backend.loadMoreIssues !== "function")
            return;
        backend.loadMoreIssues(project.id, githubRemote);
    }

    onProjectChanged: {
        if (initialRefreshCompleted)
            refreshIssues();
    }
    Component.onCompleted: {
        initialRefreshCompleted = true;
        refreshIssues();
    }

    component NavigationRow: Controls.AbstractButton {
        id: navigationRow

        required property string iconName
        required property string label
        required property string scope
        property int count: 0
        property bool selected: false

        signal scopeRequested(string scope)

        Layout.fillWidth: true
        implicitHeight: Kirigami.Units.gridUnit * 2
        hoverEnabled: true
        leftPadding: Kirigami.Units.largeSpacing
        rightPadding: Kirigami.Units.largeSpacing

        Accessible.name: label

        background: Rectangle {
            color: navigationRow.selected
                ? Qt.rgba(1, 1, 1, 0.08)
                : navigationRow.hovered
                    ? Qt.rgba(1, 1, 1, 0.05)
                    : "transparent"
            radius: Kirigami.Units.smallSpacing

            Rectangle {
                anchors.bottom: parent.bottom
                anchors.left: parent.left
                anchors.top: parent.top
                color: Kirigami.Theme.highlightColor
                radius: width / 2
                visible: navigationRow.selected
                width: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
            }
        }

        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing

            Kirigami.Icon {
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                color: navigationRow.selected
                    ? Kirigami.Theme.textColor
                    : Kirigami.Theme.disabledTextColor
                isMask: true
                source: navigationRow.iconName
            }

            Controls.Label {
                Layout.fillWidth: true
                color: navigationRow.selected
                    ? Kirigami.Theme.textColor
                    : Kirigami.Theme.disabledTextColor
                elide: Text.ElideRight
                font.weight: navigationRow.selected ? Font.DemiBold : Font.Normal
                text: navigationRow.label
                textFormat: Text.PlainText
            }

            Rectangle {
                color: Qt.rgba(1, 1, 1, 0.08)
                implicitHeight: countLabel.implicitHeight + Kirigami.Units.smallSpacing / 2
                implicitWidth: Math.max(implicitHeight,
                    countLabel.implicitWidth + Kirigami.Units.smallSpacing * 1.5)
                radius: implicitHeight / 2
                visible: navigationRow.count > 0

                Controls.Label {
                    id: countLabel

                    anchors.centerIn: parent
                    color: Kirigami.Theme.disabledTextColor
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: navigationRow.count > 99 ? qsTr("99+") : String(navigationRow.count)
                }
            }
        }

        onClicked: scopeRequested(scope)
    }

    component IssueFilterButton: Controls.Button {
        id: filterButton

        required property string filterLabel
        property string selectedValue: ""
        property var choices: []

        signal valueRequested(string value)

        flat: true
        text: selectedValue.length > 0
            ? qsTr("%1: %2").arg(filterLabel).arg(selectedValue)
            : filterLabel

        background: Rectangle {
            color: filterButton.pressed
                ? Qt.rgba(1, 1, 1, 0.10)
                : filterButton.hovered
                    ? Qt.rgba(1, 1, 1, 0.06)
                    : "transparent"
            radius: Kirigami.Units.smallSpacing
        }

        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing

            Controls.Label {
                color: Kirigami.Theme.disabledTextColor
                text: filterButton.text
                textFormat: Text.PlainText
            }

            Kirigami.Icon {
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                color: Kirigami.Theme.disabledTextColor
                isMask: true
                source: "arrow-down"
            }
        }

        onClicked: filterMenu.open()

        Controls.Menu {
            id: filterMenu

            Controls.MenuItem {
                checkable: true
                checked: filterButton.selectedValue.length === 0
                text: qsTr("Any %1").arg(filterButton.filterLabel.toLowerCase())
                onTriggered: filterButton.valueRequested("")
            }

            Controls.MenuSeparator {}

            Repeater {
                model: filterButton.choices

                Controls.MenuItem {
                    required property var modelData

                    checkable: true
                    checked: filterButton.selectedValue === String(modelData)
                    text: String(modelData)
                    onTriggered: filterButton.valueRequested(String(modelData))
                }
            }
        }
    }

    component StateTab: Controls.TabButton {
        id: stateTab

        required property color stateColor

        background: Rectangle {
            color: stateTab.hovered ? Qt.rgba(1, 1, 1, 0.06) : "transparent"

            Rectangle {
                anchors.bottom: parent.bottom
                anchors.left: parent.left
                anchors.right: parent.right
                color: stateTab.stateColor
                height: Math.max(2, Math.round(Kirigami.Units.smallSpacing / 2))
                visible: stateTab.checked
            }
        }

        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing

            Rectangle {
                border.color: stateTab.stateColor
                border.width: 2
                color: "transparent"
                height: Kirigami.Units.iconSizes.small
                radius: height / 2
                width: height
            }

            Controls.Label {
                color: stateTab.checked
                    ? Kirigami.Theme.textColor
                    : Kirigami.Theme.disabledTextColor
                font.weight: stateTab.checked ? Font.DemiBold : Font.Normal
                text: stateTab.text
                textFormat: Text.PlainText
            }
        }
    }

    component LabelPill: Rectangle {
        required property var labelData

        readonly property string labelName: typeof labelData === "string"
            ? labelData
            : String(labelData.name || "")
        readonly property color labelColor: typeof labelData === "string"
            ? Kirigami.Theme.highlightColor
            : String(labelData.color || "").length > 0
                ? String(labelData.color)
                : Kirigami.Theme.highlightColor

        border.color: Qt.alpha(labelColor, 0.9)
        border.width: 1
        color: Qt.alpha(labelColor, 0.18)
        implicitHeight: pillLabel.implicitHeight + Kirigami.Units.smallSpacing
        implicitWidth: Math.min(pillLabel.implicitWidth + Kirigami.Units.largeSpacing,
            Kirigami.Units.gridUnit * 9)
        radius: implicitHeight / 2

        Controls.ToolTip.text: labelName
        Controls.ToolTip.visible: pillHover.hovered && pillLabel.truncated

        HoverHandler {
            id: pillHover
        }

        Controls.Label {
            id: pillLabel

            anchors.fill: parent
            anchors.leftMargin: Kirigami.Units.smallSpacing
            anchors.rightMargin: Kirigami.Units.smallSpacing
            color: parent.labelColor
            elide: Text.ElideRight
            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
            horizontalAlignment: Text.AlignHCenter
            text: parent.labelName
            textFormat: Text.PlainText
            verticalAlignment: Text.AlignVCenter
        }
    }

    function textValue(value) {
        return value === undefined || value === null ? "" : String(value);
    }

    function rowLabels(row) {
        const labels = [];
        const source = row && row.labels && row.labels.length !== undefined ? row.labels : [];
        for (let index = 0; index < source.length; ++index)
            labels.push(source[index]);
        return labels;
    }

    function visibleRowLabels(row) {
        return rowLabels(row).slice(0, 2);
    }

    function hiddenLabelCount(row) {
        return Math.max(0, rowLabels(row).length - visibleRowLabels(row).length);
    }

    function labelName(label) {
        return typeof label === "string" ? label : textValue(label.name);
    }

    function rowHasLabel(row, wanted) {
        const lowered = wanted.toLowerCase();
        return rowLabels(row).some(label => labelName(label).toLowerCase() === lowered);
    }

    function rowAssignees(row) {
        const source = row && row.assignees;
        if (source && source.length !== undefined && typeof source !== "string") {
            const assignees = [];
            for (let index = 0; index < source.length; ++index)
                assignees.push(textValue(source[index]));
            return assignees;
        }
        const legacy = textValue(source);
        return legacy.length > 0 ? legacy.split(",").map(value => value.trim()) : [];
    }

    function matchesNamedScope(row, scope) {
        switch (scope) {
        case "assigned":
            return row.assignedToMe === true;
        case "created":
            return row.createdByMe === true;
        case "milestones":
            return textValue(row.milestone).length > 0;
        case "labels":
            return rowLabels(row).length > 0;
        default:
            return true;
        }
    }

    function matchesScope(row) {
        return matchesNamedScope(row, selectedScope);
    }

    function matchesSearch(row) {
        const needle = searchText.trim().toLowerCase();
        if (needle.length === 0)
            return true;
        const haystack = [
            textValue(row.number),
            textValue(row.title),
            textValue(row.author),
            textValue(row.milestone),
            rowAssignees(row).join(" "),
            rowLabels(row).map(label => labelName(label)).join(" ")
        ].join(" ").toLowerCase();
        return haystack.indexOf(needle) !== -1;
    }

    function filteredIssues() {
        const rows = [];
        const source = issueRows && issueRows.length !== undefined ? issueRows : [];
        for (let index = 0; index < source.length; ++index)
            rows.push(source[index]);
        const filtered = rows.filter(row => {
            if (textValue(row.state).toLowerCase() !== stateFilter)
                return false;
            if (!matchesScope(row) || !matchesSearch(row))
                return false;
            if (authorFilter.length > 0 && textValue(row.author) !== authorFilter)
                return false;
            if (labelFilter.length > 0 && !rowHasLabel(row, labelFilter))
                return false;
            if (milestoneFilter.length > 0 && textValue(row.milestone) !== milestoneFilter)
                return false;
            if (assigneeFilter.length > 0 && rowAssignees(row).indexOf(assigneeFilter) === -1)
                return false;
            return true;
        });
        return filtered.sort((left, right) => {
            const leftNumber = Number(left.number || 0);
            const rightNumber = Number(right.number || 0);
            return sortOrder === "oldest"
                ? leftNumber - rightNumber
                : rightNumber - leftNumber;
        });
    }

    function countByState(state) {
        const rows = filteredRowsForScope("issues");
        return rows.filter(row => textValue(row.state).toLowerCase() === state).length;
    }

    function countByStateAndSelectedScope(state) {
        return filteredRowsForScope(selectedScope)
            .filter(row => textValue(row.state).toLowerCase() === state).length;
    }

    function filteredRowsForScope(scope) {
        const rows = [];
        const source = issueRows && issueRows.length !== undefined ? issueRows : [];
        for (let index = 0; index < source.length; ++index) {
            if (matchesNamedScope(source[index], scope))
                rows.push(source[index]);
        }
        return rows;
    }

    function countForScope(scope) {
        return filteredRowsForScope(scope).length;
    }

    function uniqueTextValues(field) {
        const seen = {};
        const values = [];
        const rows = filteredRowsForScope("issues");
        rows.forEach(row => {
            const value = textValue(row[field]);
            if (value.length > 0 && seen[value] !== true) {
                seen[value] = true;
                values.push(value);
            }
        });
        return values.sort();
    }

    function uniqueLabels() {
        const seen = {};
        const values = [];
        const rows = filteredRowsForScope("issues");
        rows.forEach(row => rowLabels(row).forEach(label => {
            const value = labelName(label);
            if (value.length > 0 && seen[value] !== true) {
                seen[value] = true;
                values.push(value);
            }
        }));
        return values.sort();
    }

    function uniqueAssignees() {
        const seen = {};
        const values = [];
        const rows = filteredRowsForScope("issues");
        rows.forEach(row => rowAssignees(row).forEach(value => {
            if (value.length > 0 && seen[value] !== true) {
                seen[value] = true;
                values.push(value);
            }
        }));
        return values.sort();
    }

    function scopeTitle() {
        switch (selectedScope) {
        case "assigned": return qsTr("Assigned to me");
        case "created": return qsTr("Created by me");
        case "milestones": return qsTr("Milestones");
        case "labels": return qsTr("Labels");
        default: return qsTr("All issues");
        }
    }

    function selectScope(scope) {
        selectedScope = scope;
        searchText = "";
    }

    function issueIdentity(row) {
        const id = textValue(row.id);
        return id.length > 0 ? id : textValue(row.number);
    }

    function issueSelected(row) {
        return selectedIssueIds.indexOf(issueIdentity(row)) !== -1;
    }

    function setIssueSelected(row, selected) {
        const identity = issueIdentity(row);
        const next = selectedIssueIds.filter(candidate => candidate !== identity);
        if (selected)
            next.push(identity);
        selectedIssueIds = next;
    }

    function responsiveFilterVisible(selectedValue, minimumWidth) {
        return textValue(selectedValue).length > 0 || width >= minimumWidth;
    }

    function activateIssue(row) {
        const url = textValue(row.url);
        if (url.length === 0)
            return;
        issueActivated(url);
        Qt.openUrlExternally(url);
    }

    Controls.SplitView {
        anchors.fill: parent
        orientation: Qt.Horizontal

        handle: Rectangle {
            readonly property bool active: Controls.SplitHandle.hovered
                || Controls.SplitHandle.pressed

            color: active ? Kirigami.Theme.highlightColor : "transparent"
            implicitWidth: Kirigami.Units.smallSpacing

            Kirigami.Separator {
                anchors.horizontalCenter: parent.horizontalCenter
                height: parent.height
                visible: !parent.active
            }
        }

        Item {
            objectName: "issuesNavigation"

            Controls.SplitView.fillWidth: false
            Controls.SplitView.maximumWidth: Kirigami.Units.gridUnit * 22
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 14
            Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 17

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.alternateBackgroundColor
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.bottomMargin: Kirigami.Units.largeSpacing
                anchors.leftMargin: Kirigami.Units.smallSpacing
                anchors.rightMargin: Kirigami.Units.smallSpacing
                anchors.topMargin: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing / 2

                Controls.Label {
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    color: Kirigami.Theme.disabledTextColor
                    font.capitalization: Font.AllUppercase
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: qsTr("Issues")
                }

                NavigationRow {
                    count: panel.openIssueCount
                    iconName: "view-task"
                    label: qsTr("Issues")
                    scope: "issues"
                    selected: panel.selectedScope === scope
                    onScopeRequested: scope => panel.selectScope(scope)
                }

                NavigationRow {
                    count: panel.countForScope("assigned")
                    iconName: "user-identity"
                    label: qsTr("Assigned to me")
                    scope: "assigned"
                    selected: panel.selectedScope === scope
                    onScopeRequested: scope => panel.selectScope(scope)
                }

                NavigationRow {
                    count: panel.countForScope("created")
                    iconName: "document-new"
                    label: qsTr("Created by me")
                    scope: "created"
                    selected: panel.selectedScope === scope
                    onScopeRequested: scope => panel.selectScope(scope)
                }

                Kirigami.Separator {
                    Layout.fillWidth: true
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    Layout.topMargin: Kirigami.Units.largeSpacing
                }

                NavigationRow {
                    iconName: "milestone"
                    label: qsTr("Milestones")
                    scope: "milestones"
                    selected: panel.selectedScope === scope
                    onScopeRequested: scope => panel.selectScope(scope)
                }

                NavigationRow {
                    iconName: "tag"
                    label: qsTr("Labels")
                    scope: "labels"
                    selected: panel.selectedScope === scope
                    onScopeRequested: scope => panel.selectScope(scope)
                }

                Item {
                    Layout.fillHeight: true
                }

                Controls.ToolButton {
                    Layout.fillWidth: true
                    Controls.ToolTip.text: qsTr("Hide the side panel (Ctrl+B)")
                    Controls.ToolTip.visible: hovered
                    display: Controls.AbstractButton.TextBesideIcon
                    icon.name: "window-close-symbolic"
                    text: qsTr("Collapse Issues")

                    contentItem: RowLayout {
                        spacing: Kirigami.Units.smallSpacing

                        Kirigami.Icon {
                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                            color: Kirigami.Theme.disabledTextColor
                            isMask: true
                            source: "window-close-symbolic"
                        }

                        Controls.Label {
                            color: Kirigami.Theme.disabledTextColor
                            text: qsTr("Collapse Issues")
                        }
                    }
                    onClicked: panel.hideRequested()
                }
            }
        }

        Item {
            objectName: "issuesTableSurface"

            Controls.SplitView.fillWidth: true
            Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 30

            Rectangle {
                anchors.fill: parent
                color: Kirigami.Theme.backgroundColor
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.bottomMargin: Kirigami.Units.largeSpacing
                anchors.leftMargin: Kirigami.Units.gridUnit * 2
                anchors.rightMargin: Kirigami.Units.gridUnit * 2
                anchors.topMargin: Kirigami.Units.gridUnit * 1.5
                spacing: Kirigami.Units.largeSpacing

                RowLayout {
                    Layout.fillWidth: true

                    Kirigami.Heading {
                        Layout.fillWidth: true
                        level: 2
                        text: panel.scopeTitle()
                    }

                    Controls.ToolButton {
                        Controls.ToolTip.text: qsTr("Refresh issues")
                        Controls.ToolTip.visible: hovered
                        Accessible.name: qsTr("Refresh issues")
                        enabled: panel.viewAvailable && !panel.loading
                        icon.name: "view-refresh"
                        onClicked: panel.refreshIssues()
                    }

                    Controls.Button {
                        Controls.ToolTip.text: qsTr("Create an issue on GitHub")
                        Controls.ToolTip.visible: hovered
                        enabled: panel.viewAvailable
                        highlighted: true
                        icon.name: "task-new"
                        text: qsTr("New issue")

                        background: Rectangle {
                            border.color: Qt.alpha("#3fb950", 0.55)
                            border.width: 1
                            color: Qt.alpha("#238636", 0.32)
                            radius: Kirigami.Units.smallSpacing
                        }

                        contentItem: RowLayout {
                            spacing: Kirigami.Units.smallSpacing

                            Kirigami.Icon {
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                color: Kirigami.Theme.disabledTextColor
                                isMask: true
                                source: "task-new"
                            }

                            Controls.Label {
                                color: Kirigami.Theme.disabledTextColor
                                text: qsTr("New issue")
                            }
                        }
                        onClicked: {
                            panel.createIssueRequested();
                            const slug = panel.githubRemote.substring("github.com/".length);
                            Qt.openUrlExternally("https://github.com/" + slug + "/issues/new");
                        }
                    }
                }

                Controls.TextField {
                    id: issueSearch

                    Layout.fillWidth: true
                    objectName: "issueSearch"
                    placeholderText: qsTr("Search issues by title, number, author, assignee, label, or milestone")
                    color: Kirigami.Theme.textColor
                    placeholderTextColor: Kirigami.Theme.disabledTextColor
                    text: panel.searchText
                    onTextEdited: panel.searchText = text

                    background: FieldSurface {
                        field: issueSearch
                    }
                }

                Rectangle {
                    Layout.fillHeight: true
                    Layout.fillWidth: true
                    border.color: Qt.rgba(1, 1, 1, 0.16)
                    border.width: 1
                    color: "transparent"
                    radius: Kirigami.Units.smallSpacing

                    ColumnLayout {
                        anchors.fill: parent
                        spacing: 0

                        Rectangle {
                            Layout.fillWidth: true
                            color: Qt.rgba(1, 1, 1, 0.035)
                            implicitHeight: filterRow.implicitHeight + Kirigami.Units.smallSpacing * 2
                            radius: Kirigami.Units.smallSpacing

                            RowLayout {
                                id: filterRow

                                anchors.fill: parent
                                anchors.leftMargin: Kirigami.Units.largeSpacing
                                anchors.rightMargin: Kirigami.Units.smallSpacing
                                spacing: Kirigami.Units.smallSpacing

                                StateTab {
                                    checked: panel.stateFilter === "open"
                                    stateColor: "#3fb950"
                                    text: qsTr("Open %1").arg(panel.countByStateAndSelectedScope("open"))
                                    onClicked: panel.stateFilter = "open"
                                }

                                StateTab {
                                    checked: panel.stateFilter === "closed"
                                    stateColor: "#a371f7"
                                    text: qsTr("Closed %1").arg(panel.countByStateAndSelectedScope("closed"))
                                    onClicked: panel.stateFilter = "closed"
                                }

                                Item {
                                    Layout.fillWidth: true
                                }

                                IssueFilterButton {
                                    choices: panel.uniqueTextValues("author")
                                    filterLabel: qsTr("Author")
                                    selectedValue: panel.authorFilter
                                    onValueRequested: value => panel.authorFilter = value
                                }

                                IssueFilterButton {
                                    choices: panel.uniqueLabels()
                                    filterLabel: qsTr("Labels")
                                    selectedValue: panel.labelFilter
                                    onValueRequested: value => panel.labelFilter = value
                                }

                                IssueFilterButton {
                                    id: milestoneFilterButton

                                    choices: panel.uniqueTextValues("milestone")
                                    filterLabel: qsTr("Milestones")
                                    selectedValue: panel.milestoneFilter
                                    visible: panel.responsiveFilterVisible(selectedValue,
                                        Kirigami.Units.gridUnit * 65)
                                    onValueRequested: value => panel.milestoneFilter = value
                                }

                                IssueFilterButton {
                                    id: assigneeFilterButton

                                    choices: panel.uniqueAssignees()
                                    filterLabel: qsTr("Assignees")
                                    selectedValue: panel.assigneeFilter
                                    visible: panel.responsiveFilterVisible(selectedValue,
                                        Kirigami.Units.gridUnit * 75)
                                    onValueRequested: value => panel.assigneeFilter = value
                                }

                                Controls.Button {
                                    id: sortButton

                                    flat: true
                                    text: panel.sortOrder === "newest" ? qsTr("Newest") : qsTr("Oldest")

                                    background: Rectangle {
                                        color: sortButton.hovered ? Qt.rgba(1, 1, 1, 0.06) : "transparent"
                                        radius: Kirigami.Units.smallSpacing
                                    }

                                    contentItem: RowLayout {
                                        spacing: Kirigami.Units.smallSpacing

                                        Kirigami.Icon {
                                            Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                            Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                            color: Kirigami.Theme.disabledTextColor
                                            isMask: true
                                            source: panel.sortOrder === "newest"
                                                ? "view-sort-descending"
                                                : "view-sort-ascending"
                                        }

                                        Controls.Label {
                                            color: Kirigami.Theme.disabledTextColor
                                            text: sortButton.text
                                        }
                                    }
                                    onClicked: panel.sortOrder = panel.sortOrder === "newest"
                                        ? "oldest"
                                        : "newest"
                                }
                            }
                        }

                        Kirigami.Separator {
                            Layout.fillWidth: true
                        }

                        Item {
                            Layout.fillHeight: true
                            Layout.fillWidth: true

                            ListView {
                                id: issueList

                                anchors.fill: parent
                                clip: true
                                model: panel.visibleIssues
                                reuseItems: true

                                delegate: Controls.ItemDelegate {
                                    id: issueDelegate

                                    required property var modelData

                                    width: ListView.view.width
                                    implicitHeight: issueContent.implicitHeight + Kirigami.Units.largeSpacing * 2
                                    hoverEnabled: true
                                    leftPadding: Kirigami.Units.largeSpacing
                                    rightPadding: Kirigami.Units.largeSpacing
                                    Accessible.name: qsTr("Issue %1: %2")
                                        .arg(modelData.number)
                                        .arg(panel.textValue(modelData.title))
                                    onClicked: panel.activateIssue(modelData)

                                    background: Rectangle {
                                        color: issueDelegate.hovered
                                            ? Qt.rgba(1, 1, 1, 0.045)
                                            : "transparent"

                                        Kirigami.Separator {
                                            anchors.bottom: parent.bottom
                                            width: parent.width
                                        }
                                    }

                                    contentItem: RowLayout {
                                        id: issueContent

                                        spacing: Kirigami.Units.largeSpacing

                                        Controls.CheckBox {
                                            Layout.alignment: Qt.AlignTop
                                            Accessible.name: qsTr("Select issue %1").arg(issueDelegate.modelData.number)
                                            checked: panel.issueSelected(issueDelegate.modelData)
                                            onToggled: panel.setIssueSelected(issueDelegate.modelData, checked)
                                        }

                                        Rectangle {
                                            Layout.alignment: Qt.AlignTop
                                            Layout.topMargin: Kirigami.Units.smallSpacing
                                            border.color: panel.stateFilter === "open" ? "#3fb950" : "#a371f7"
                                            border.width: 2
                                            color: "transparent"
                                            height: Kirigami.Units.iconSizes.small
                                            radius: height / 2
                                            width: height
                                        }

                                        ColumnLayout {
                                            Layout.fillWidth: true
                                            spacing: Kirigami.Units.smallSpacing / 2

                                            RowLayout {
                                                Layout.fillWidth: true
                                                spacing: Kirigami.Units.smallSpacing

                                                Controls.Label {
                                                    Layout.fillWidth: true
                                                    Layout.minimumWidth: Kirigami.Units.gridUnit * 8
                                                    elide: Text.ElideRight
                                                    color: Kirigami.Theme.textColor
                                                    font.pixelSize: Kirigami.Theme.defaultFont.pixelSize + 1
                                                    font.weight: Font.DemiBold
                                                    text: panel.textValue(issueDelegate.modelData.title)
                                                    textFormat: Text.PlainText
                                                }

                                                Repeater {
                                                    model: panel.visibleRowLabels(issueDelegate.modelData)

                                                    LabelPill {
                                                        required property var modelData

                                                        labelData: modelData
                                                    }
                                                }

                                                LabelPill {
                                                    labelData: qsTr("+%1").arg(panel.hiddenLabelCount(issueDelegate.modelData))
                                                    visible: panel.hiddenLabelCount(issueDelegate.modelData) > 0
                                                }

                                                Item {
                                                    Layout.fillWidth: true
                                                }

                                                Controls.Label {
                                                    color: Kirigami.Theme.disabledTextColor
                                                    text: panel.rowAssignees(issueDelegate.modelData).join(", ")
                                                    textFormat: Text.PlainText
                                                    visible: text.length > 0
                                                }
                                            }

                                            Controls.Label {
                                                Layout.fillWidth: true
                                                color: Kirigami.Theme.disabledTextColor
                                                elide: Text.ElideRight
                                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                                text: {
                                                    const number = panel.textValue(issueDelegate.modelData.number);
                                                    const author = panel.textValue(issueDelegate.modelData.author);
                                                    const updated = panel.textValue(issueDelegate.modelData.updated);
                                                    const milestone = panel.textValue(issueDelegate.modelData.milestone);
                                                    let metadata = qsTr("#%1 opened by %2 · %3").arg(number).arg(author).arg(updated);
                                                    if (milestone.length > 0)
                                                        metadata += qsTr(" · %1").arg(milestone);
                                                    return metadata;
                                                }
                                                textFormat: Text.PlainText
                                            }
                                        }

                                        Controls.Label {
                                            Layout.alignment: Qt.AlignTop
                                            color: Kirigami.Theme.disabledTextColor
                                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                            text: Number(issueDelegate.modelData.commentCount || 0) > 0
                                                ? qsTr("%1 comments").arg(issueDelegate.modelData.commentCount)
                                                : ""
                                            visible: text.length > 0 && panel.width >= Kirigami.Units.gridUnit * 58
                                        }
                                    }
                                }

                                Controls.ScrollBar.vertical: Controls.ScrollBar {}

                                footer: ColumnLayout {
                                    width: issueList.width
                                    spacing: Kirigami.Units.smallSpacing

                                    Controls.Button {
                                        Layout.alignment: Qt.AlignHCenter
                                        Layout.margins: Kirigami.Units.largeSpacing
                                        enabled: !panel.loading
                                        text: qsTr("Load more issues")
                                        visible: panel.hasMoreIssues
                                        onClicked: panel.loadMoreIssues()
                                    }

                                    Controls.Label {
                                        Layout.alignment: Qt.AlignHCenter
                                        Layout.bottomMargin: Kirigami.Units.largeSpacing
                                        color: Kirigami.Theme.disabledTextColor
                                        text: qsTr("Showing the first 1,000 issues. Use GitHub to browse the remaining results.")
                                        visible: panel.issueLimitReached
                                        wrapMode: Text.Wrap
                                    }
                                }
                            }

                            Kirigami.PlaceholderMessage {
                                anchors.centerIn: parent
                                icon.name: panel.loadError.length > 0
                                    ? "dialog-error"
                                    : panel.issueRows.length === 0
                                        ? "view-task"
                                        : "view-filter"
                                text: panel.loadError.length > 0
                                    ? qsTr("Could not load issues")
                                    : panel.issueRows.length === 0
                                        ? qsTr("No issues found")
                                        : qsTr("No issues match these filters")
                                explanation: panel.loadError.length > 0
                                    ? qsTr("%1 Use Refresh issues to try again.").arg(panel.loadError)
                                    : panel.issueRows.length === 0
                                        ? qsTr("GitHub returned no issues for this repository.")
                                        : qsTr("Try another scope, state, or search term.")
                                visible: !panel.loading && panel.visibleIssueCount === 0
                                width: Math.min(parent.width - Kirigami.Units.gridUnit * 4,
                                    Kirigami.Units.gridUnit * 28)
                            }

                            Controls.BusyIndicator {
                                anchors.centerIn: parent
                                running: panel.loading
                                visible: running
                            }
                        }
                    }
                }
            }
        }
    }
}
