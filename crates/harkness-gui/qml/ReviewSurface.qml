import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

ColumnLayout {
    id: reviewSurface

    required property var backend
    required property var project
    required property var gitState
    required property bool stateReady

    readonly property bool historyReady: backend.history !== undefined
        && backend.history
        && backend.history.projectId !== undefined
        && String(backend.history.projectId) === String(project.id)
    readonly property var historyState: historyReady ? backend.history : ({})
    readonly property var commits: historyReady && historyState.commits !== undefined
        ? historyState.commits
        : []
    readonly property bool reviewReady: backend.review !== undefined
        && backend.review
        && backend.review.projectId !== undefined
        && String(backend.review.projectId) === String(project.id)
    readonly property var reviewState: reviewReady ? backend.review : ({})
    readonly property var reviewFiles: reviewReady && reviewState.files !== undefined
        ? reviewState.files
        : []
    readonly property var reviewFile: reviewReady
        && reviewState.file !== undefined
        && reviewState.file.fileId !== undefined
        ? reviewState.file
        : ({})
    readonly property var reviewRows: reviewFile.rows !== undefined ? reviewFile.rows : []
    property string baseBranch: ""
    property bool splitLayout: false

    spacing: Kirigami.Units.smallSpacing

    function job(kind) {
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if (String(candidate.projectId) === String(project.id) && candidate.kind === kind)
                return candidate;
        }
        return null;
    }

    function chooseBaseBranch() {
        if (baseBranch.length > 0 && baseBranch !== String(gitState.branch || ""))
            return;
        let fallback = "";
        for (let index = 0; index < backend.branches.length; ++index) {
            const branch = String(backend.branches[index].name || "");
            if (branch === String(gitState.branch || ""))
                continue;
            if (branch === "main" || branch === "master") {
                baseBranch = branch;
                return;
            }
            if (fallback.length === 0)
                fallback = branch;
        }
        baseBranch = fallback;
    }

    function formatCommitTime(seconds) {
        const value = Number(seconds);
        if (!isFinite(value))
            return "";
        return new Date(value * 1000).toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    function tint(color, opacity) {
        return Qt.rgba(color.r, color.g, color.b, opacity);
    }

    function lineColor(kind) {
        if (kind === "addition")
            return tint(Kirigami.Theme.positiveTextColor, 0.14);
        if (kind === "deletion")
            return tint(Kirigami.Theme.negativeTextColor, 0.14);
        return "transparent";
    }

    function markerColor(kind) {
        if (kind === "addition")
            return Kirigami.Theme.positiveTextColor;
        if (kind === "deletion")
            return Kirigami.Theme.negativeTextColor;
        return Kirigami.Theme.disabledTextColor;
    }

    function escapeCode(value) {
        return String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;")
            .replace(/ /g, "&nbsp;")
            .replace(/\t/g, "&nbsp;&nbsp;&nbsp;&nbsp;");
    }

    function syntaxKeywords(path) {
        const extension = String(path).toLowerCase().split(".").pop();
        const common = {
            "as": true, "async": true, "await": true, "break": true,
            "case": true, "class": true, "const": true, "continue": true,
            "default": true, "do": true, "else": true, "enum": true,
            "false": true, "fn": true, "for": true, "function": true,
            "if": true, "impl": true, "import": true, "in": true,
            "let": true, "loop": true, "match": true, "mod": true,
            "move": true, "mut": true, "new": true, "null": true,
            "pub": true, "return": true, "self": true, "static": true,
            "struct": true, "super": true, "switch": true, "this": true,
            "trait": true, "true": true, "type": true, "typeof": true,
            "unsafe": true, "use": true, "var": true, "where": true,
            "while": true, "yield": true
        };
        if (["py", "pyi"].indexOf(extension) !== -1) {
            common["and"] = true;
            common["def"] = true;
            common["elif"] = true;
            common["except"] = true;
            common["from"] = true;
            common["is"] = true;
            common["lambda"] = true;
            common["none"] = true;
            common["not"] = true;
            common["or"] = true;
            common["pass"] = true;
            common["raise"] = true;
            common["try"] = true;
            common["with"] = true;
        }
        return common;
    }

    // This lexer is deliberately presentation-only. Pairing and byte ranges
    // arrive from the Git service; this function only colors familiar source tokens.
    function syntaxHtml(value, path) {
        const text = String(value);
        const extension = String(path).toLowerCase().split(".").pop();
        const supported = [
            "c", "cc", "cpp", "css", "go", "h", "hpp", "java", "js",
            "json", "kt", "kts", "py", "pyi", "qml", "rs", "swift",
            "ts", "tsx"
        ].indexOf(extension) !== -1;
        if (!supported)
            return escapeCode(text);

        const keywords = syntaxKeywords(path);
        const keywordColor = Kirigami.Theme.linkColor.toString();
        const stringColor = Kirigami.Theme.positiveTextColor.toString();
        const commentColor = Kirigami.Theme.disabledTextColor.toString();
        const numberColor = Kirigami.Theme.neutralTextColor.toString();
        const hashComments = extension === "py" || extension === "pyi";
        let result = "";
        let index = 0;
        while (index < text.length) {
            const character = text[index];
            const next = index + 1 < text.length ? text[index + 1] : "";
            if ((character === "/" && next === "/")
                    || (hashComments && character === "#")) {
                result += "<span style=\"color:" + commentColor + "\">"
                    + escapeCode(text.substring(index)) + "</span>";
                break;
            }
            if (character === "\"" || character === "'") {
                const quote = character;
                let end = index + 1;
                while (end < text.length) {
                    if (text[end] === "\\") {
                        end += 2;
                        continue;
                    }
                    if (text[end] === quote) {
                        ++end;
                        break;
                    }
                    ++end;
                }
                result += "<span style=\"color:" + stringColor + "\">"
                    + escapeCode(text.substring(index, end)) + "</span>";
                index = end;
                continue;
            }
            if (/[A-Za-z_]/.test(character)) {
                let end = index + 1;
                while (end < text.length && /[A-Za-z0-9_]/.test(text[end]))
                    ++end;
                const word = text.substring(index, end);
                if (keywords[word.toLowerCase()] === true) {
                    result += "<span style=\"color:" + keywordColor
                        + ";font-weight:600\">" + escapeCode(word) + "</span>";
                } else {
                    result += escapeCode(word);
                }
                index = end;
                continue;
            }
            if (/[0-9]/.test(character)) {
                let end = index + 1;
                while (end < text.length && /[0-9A-Fa-f_xX.]/.test(text[end]))
                    ++end;
                result += "<span style=\"color:" + numberColor + "\">"
                    + escapeCode(text.substring(index, end)) + "</span>";
                index = end;
                continue;
            }
            result += escapeCode(character);
            ++index;
        }
        return result;
    }

    function highlightedLine(segments, path) {
        let result = "<span>";
        for (let index = 0; index < segments.length; ++index) {
            const segment = segments[index];
            let content = syntaxHtml(segment.text, path);
            if (segment.changed === true) {
                content = "<span style=\"font-weight:700;text-decoration:underline\">"
                    + content + "</span>";
            }
            result += content;
        }
        return result + "</span>";
    }

    function setSplitLayout(value) {
        if (splitLayout === value)
            return;
        const position = reviewLineView.contentY;
        splitLayout = value;
        Qt.callLater(function() {
            const maximum = Math.max(0, reviewLineView.contentHeight - reviewLineView.height);
            reviewLineView.contentY = Math.min(position, maximum);
        });
    }

    function selectedFileIndex() {
        const selected = reviewReady ? String(reviewState.selectedFileId || "") : "";
        for (let index = 0; index < reviewFiles.length; ++index) {
            if (String(reviewFiles[index].fileId) === selected)
                return index;
        }
        return -1;
    }

    function navigateFile(offset) {
        if (reviewFiles.length === 0)
            return;
        let index = selectedFileIndex();
        if (index < 0)
            index = offset > 0 ? 0 : reviewFiles.length - 1;
        else
            index = Math.max(0, Math.min(reviewFiles.length - 1, index + offset));
        reviewFileList.currentIndex = index;
        reviewFileList.positionViewAtIndex(index, ListView.Contain);
        backend.loadReviewFile(project.id, reviewFiles[index].fileId);
    }

    function navigateHunk(offset) {
        if (reviewRows.length === 0)
            return;
        let index = reviewLineView.currentIndex;
        if (index < 0)
            index = offset > 0 ? -1 : reviewRows.length;
        for (let candidate = index + offset;
             candidate >= 0 && candidate < reviewRows.length;
             candidate += offset) {
            if (reviewRows[candidate].type === "hunk") {
                reviewLineView.currentIndex = candidate;
                reviewLineView.positionViewAtIndex(candidate, ListView.Beginning);
                reviewLineView.forceActiveFocus();
                return;
            }
        }
    }

    onStateReadyChanged: chooseBaseBranch()
    Component.onCompleted: chooseBaseBranch()
    onBaseBranchChanged: {
        reviewBasePicker.currentIndex = -1;
        if (reviewBasePicker.editText !== baseBranch)
            reviewBasePicker.editText = baseBranch;
    }

    Connections {
        target: reviewSurface.backend

        function onBranchesChanged() {
            reviewSurface.chooseBaseBranch();
        }
    }

    Shortcut {
        enabled: reviewSurface.reviewReady && reviewSurface.reviewFiles.length > 0
        sequence: "Alt+J"
        onActivated: reviewSurface.navigateFile(1)
    }

    Shortcut {
        enabled: reviewSurface.reviewReady && reviewSurface.reviewFiles.length > 0
        sequence: "Alt+K"
        onActivated: reviewSurface.navigateFile(-1)
    }

    Shortcut {
        enabled: reviewSurface.reviewRows.length > 0
        sequence: "Alt+Shift+J"
        onActivated: reviewSurface.navigateHunk(1)
    }

    Shortcut {
        enabled: reviewSurface.reviewRows.length > 0
        sequence: "Alt+Shift+K"
        onActivated: reviewSurface.navigateHunk(-1)
    }

    RowLayout {
        Layout.fillWidth: true

        Kirigami.Heading {
            Layout.fillWidth: true
            level: 4
            text: qsTr("Review")
        }

        Controls.BusyIndicator {
            Layout.preferredHeight: Kirigami.Units.iconSizes.small
            Layout.preferredWidth: Kirigami.Units.iconSizes.small
            running: reviewSurface.job("history") !== null
                || reviewSurface.job("review") !== null
            visible: running
        }

        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Refresh commit history")
            Controls.ToolTip.visible: hovered
            display: Controls.AbstractButton.IconOnly
            enabled: reviewSurface.job("history") === null
            icon.name: "view-refresh"
            onClicked: reviewSurface.backend.refreshHistory(reviewSurface.project.id)
        }
    }

    Controls.Label {
        Layout.fillWidth: true
        color: Kirigami.Theme.disabledTextColor
        text: qsTr("Review a commit, or compare the current branch with its merge-base.")
        wrapMode: Text.Wrap
    }

    RowLayout {
        Layout.fillWidth: true

        Controls.Label {
            Layout.fillWidth: true
            elide: Text.ElideRight
            font.bold: true
            text: reviewSurface.stateReady && reviewSurface.gitState.branch
                ? reviewSurface.gitState.branch
                : qsTr("Detached HEAD")
            textFormat: Text.PlainText
        }

        Controls.Label {
            color: Kirigami.Theme.disabledTextColor
            text: qsTr("against")
        }

        Controls.ComboBox {
            id: reviewBasePicker

            Layout.preferredWidth: Kirigami.Units.gridUnit * 7
            editable: true
            enabled: reviewSurface.stateReady && reviewSurface.gitState.branch
                && reviewSurface.job("review") === null
            model: reviewSurface.backend.branches
            textRole: "name"

            Component.onCompleted: {
                currentIndex = -1;
                editText = reviewSurface.baseBranch;
            }
            onActivated: reviewSurface.baseBranch = String(currentText).trim()
            onAccepted: reviewSurface.baseBranch = String(editText).trim()
            onEditTextChanged: reviewSurface.baseBranch = String(editText).trim()
        }

        Controls.ToolButton {
            Controls.ToolTip.text: qsTr("Review branch against merge-base")
            Controls.ToolTip.visible: hovered
            display: Controls.AbstractButton.IconOnly
            enabled: reviewSurface.stateReady
                && reviewSurface.gitState.branch
                && reviewSurface.baseBranch.length > 0
                && reviewSurface.job("review") === null
            icon.name: "vcs-diff"
            onClicked: reviewSurface.backend.reviewBranch(
                reviewSurface.project.id,
                reviewSurface.gitState.branch,
                reviewSurface.baseBranch
            )
        }
    }

    RowLayout {
        Layout.fillWidth: true

        Controls.Label {
            Layout.fillWidth: true
            color: Kirigami.Theme.disabledTextColor
            text: qsTr("Working changes")
        }

        Controls.Button {
            enabled: reviewSurface.job("review") === null
            text: qsTr("Staged")
            onClicked: reviewSurface.backend.reviewWorkingChanges(
                reviewSurface.project.id,
                true
            )
        }

        Controls.Button {
            enabled: reviewSurface.job("review") === null
            text: qsTr("Unstaged")
            onClicked: reviewSurface.backend.reviewWorkingChanges(
                reviewSurface.project.id,
                false
            )
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        text: reviewSurface.historyReady ? reviewSurface.historyState.error || "" : ""
        type: Kirigami.MessageType.Error
        visible: text.length > 0
    }

    Controls.Label {
        Layout.fillWidth: true
        color: Kirigami.Theme.disabledTextColor
        text: reviewSurface.historyReady && reviewSurface.historyState.loading !== true
            ? qsTr("No commits yet")
            : qsTr("Loading commit history…")
        visible: reviewSurface.commits.length === 0
        wrapMode: Text.Wrap
    }

    ListView {
        id: historyList

        Layout.fillWidth: true
        Layout.preferredHeight: Math.min(
            Kirigami.Units.gridUnit * 12,
            Math.max(Kirigami.Units.gridUnit * 3, contentHeight)
        )
        activeFocusOnTab: true
        boundsBehavior: Flickable.StopAtBounds
        clip: true
        keyNavigationEnabled: true
        model: reviewSurface.commits
        reuseItems: true
        spacing: Kirigami.Units.smallSpacing
        visible: reviewSurface.commits.length > 0

        delegate: Controls.ItemDelegate {
            id: commitDelegate

            required property var modelData

            Controls.ToolTip.text: modelData.message
            Controls.ToolTip.visible: hovered && modelData.message.length > 0
            highlighted: historyList.currentIndex === index
            width: historyList.width
            onClicked: {
                historyList.currentIndex = index;
                reviewSurface.backend.reviewCommit(
                    reviewSurface.project.id,
                    modelData.id
                );
            }

            contentItem: ColumnLayout {
                spacing: 0

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    font.bold: true
                    text: commitDelegate.modelData.summary.length > 0
                        ? commitDelegate.modelData.summary
                        : qsTr("(no commit message)")
                    textFormat: Text.PlainText
                }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        color: Kirigami.Theme.linkColor
                        font.family: "monospace"
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: commitDelegate.modelData.shortId
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        text: qsTr("%1 · %2")
                            .arg(commitDelegate.modelData.author)
                            .arg(reviewSurface.formatCommitTime(
                                commitDelegate.modelData.authorTime
                            ))
                        textFormat: Text.PlainText
                    }
                }
            }
        }

        Controls.ScrollBar.vertical: Controls.ScrollBar {}
    }

    Controls.Button {
        Layout.alignment: Qt.AlignHCenter
        enabled: reviewSurface.job("history") === null
        icon.name: "go-down"
        text: reviewSurface.job("history") === null
            ? qsTr("Load older commits")
            : qsTr("Loading…")
        visible: reviewSurface.historyReady && reviewSurface.historyState.hasMore === true
        onClicked: reviewSurface.backend.loadMoreHistory(reviewSurface.project.id)
    }

    Kirigami.Separator {
        Layout.fillWidth: true
        visible: reviewSurface.reviewReady
    }

    ColumnLayout {
        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing
        visible: reviewSurface.reviewReady

        RowLayout {
            Layout.fillWidth: true

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                Kirigami.Heading {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    level: 5
                    text: reviewSurface.reviewState.title || qsTr("Review")
                    textFormat: Text.PlainText
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    elide: Text.ElideMiddle
                    font: Kirigami.Theme.smallFont
                    text: reviewSurface.reviewState.detail || ""
                    textFormat: Text.PlainText
                }
            }

            Controls.BusyIndicator {
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                running: reviewSurface.reviewState.loading === true
                    || reviewSurface.reviewState.fileLoading === true
                    || reviewSurface.job("review_context") !== null
                visible: running
            }
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            text: reviewSurface.reviewState.error || ""
            type: Kirigami.MessageType.Error
            visible: text.length > 0
        }

        Controls.Label {
            Layout.fillWidth: true
            color: Kirigami.Theme.disabledTextColor
            text: qsTr("No files changed in this comparison.")
            visible: reviewSurface.reviewState.loading !== true
                && reviewSurface.reviewFiles.length === 0
                && (!reviewSurface.reviewState.error
                    || reviewSurface.reviewState.error.length === 0)
        }

        ListView {
            id: reviewFileList

            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(
                Kirigami.Units.gridUnit * 9,
                Math.max(Kirigami.Units.gridUnit * 2, contentHeight)
            )
            activeFocusOnTab: true
            boundsBehavior: Flickable.StopAtBounds
            clip: true
            keyNavigationEnabled: true
            model: reviewSurface.reviewFiles
            reuseItems: true
            spacing: Kirigami.Units.smallSpacing
            visible: reviewSurface.reviewFiles.length > 0

            delegate: Controls.ItemDelegate {
                id: reviewFileDelegate

                required property var modelData

                Controls.ToolTip.text: modelData.path
                Controls.ToolTip.visible: hovered
                highlighted: String(reviewSurface.reviewState.selectedFileId || "")
                    === String(modelData.fileId)
                width: reviewFileList.width
                onClicked: {
                    reviewFileList.currentIndex = index;
                    reviewSurface.backend.loadReviewFile(
                        reviewSurface.project.id,
                        modelData.fileId
                    );
                }

                contentItem: RowLayout {
                    Controls.Label {
                        Layout.fillWidth: true
                        elide: Text.ElideMiddle
                        font.family: "monospace"
                        text: reviewFileDelegate.modelData.path
                        textFormat: Text.PlainText
                    }

                    Controls.Label {
                        color: Kirigami.Theme.disabledTextColor
                        font: Kirigami.Theme.smallFont
                        text: reviewFileDelegate.modelData.change
                        textFormat: Text.PlainText
                    }
                }
            }

            Controls.ScrollBar.vertical: Controls.ScrollBar {}
        }

        RowLayout {
            Layout.fillWidth: true
            visible: reviewSurface.reviewFile.fileId !== undefined

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Previous file (Alt+K)")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: reviewSurface.selectedFileIndex() > 0
                icon.name: "go-up"
                onClicked: reviewSurface.navigateFile(-1)
            }

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Next file (Alt+J)")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: reviewSurface.selectedFileIndex() >= 0
                    && reviewSurface.selectedFileIndex() + 1 < reviewSurface.reviewFiles.length
                icon.name: "go-down"
                onClicked: reviewSurface.navigateFile(1)
            }

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Previous hunk (Alt+Shift+K)")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: reviewSurface.reviewRows.length > 0
                icon.name: "arrow-up-double"
                onClicked: reviewSurface.navigateHunk(-1)
            }

            Controls.ToolButton {
                Controls.ToolTip.text: qsTr("Next hunk (Alt+Shift+J)")
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: reviewSurface.reviewRows.length > 0
                icon.name: "arrow-down-double"
                onClicked: reviewSurface.navigateHunk(1)
            }

            Item {
                Layout.fillWidth: true
            }

            Controls.ButtonGroup {
                id: reviewLayoutGroup
            }

            Controls.ToolButton {
                Controls.ButtonGroup.group: reviewLayoutGroup
                Controls.ToolTip.text: qsTr("Unified layout")
                Controls.ToolTip.visible: hovered
                checkable: true
                checked: !reviewSurface.splitLayout
                display: Controls.AbstractButton.IconOnly
                icon.name: "view-list-text"
                onClicked: reviewSurface.setSplitLayout(false)
            }

            Controls.ToolButton {
                Controls.ButtonGroup.group: reviewLayoutGroup
                Controls.ToolTip.text: qsTr("Side-by-side layout")
                Controls.ToolTip.visible: hovered
                checkable: true
                checked: reviewSurface.splitLayout
                display: Controls.AbstractButton.IconOnly
                icon.name: "view-split-left-right"
                onClicked: reviewSurface.setSplitLayout(true)
            }
        }

        Controls.Label {
            Layout.fillWidth: true
            elide: Text.ElideMiddle
            font.bold: true
            font.family: "monospace"
            text: reviewSurface.reviewFile.path || ""
            textFormat: Text.PlainText
            visible: text.length > 0
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            text: reviewSurface.reviewFile.summary || ""
            type: Kirigami.MessageType.Information
            visible: text.length > 0
        }

        ListView {
            id: reviewLineView

            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 22
            activeFocusOnTab: true
            boundsBehavior: Flickable.StopAtBounds
            cacheBuffer: height * 2
            clip: true
            keyNavigationEnabled: true
            model: reviewSurface.reviewRows
            reuseItems: true
            visible: reviewSurface.reviewRows.length > 0

            delegate: Loader {
                id: reviewRowLoader

                required property var modelData

                readonly property var row: modelData
                sourceComponent: row.type === "hunk"
                    ? reviewHunkComponent
                    : row.type === "collapsed"
                        ? reviewCollapsedComponent
                        : reviewLineComponent
                width: reviewLineView.width
                onLoaded: item.row = row
                onRowChanged: {
                    if (item)
                        item.row = row;
                }
            }

            Controls.ScrollBar.horizontal: Controls.ScrollBar {}
            Controls.ScrollBar.vertical: Controls.ScrollBar {}
        }
    }

    Component {
        id: reviewHunkComponent

        Controls.Frame {
            id: reviewHunk

            property var row: ({})

            padding: Kirigami.Units.smallSpacing
            width: ListView.view ? ListView.view.width : implicitWidth

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.highlightColor
                    font.family: "monospace"
                    text: reviewHunk.row.header
                    textFormat: Text.PlainText
                    wrapMode: Text.WrapAnywhere
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.neutralTextColor
                    font: Kirigami.Theme.smallFont
                    text: reviewHunk.row.degradation
                    textFormat: Text.PlainText
                    visible: text.length > 0
                    wrapMode: Text.Wrap
                }
            }
        }
    }

    Component {
        id: reviewCollapsedComponent

        Controls.Button {
            property var row: ({})

            enabled: reviewSurface.job("review_context") === null
            flat: true
            icon.name: "view-more-symbolic"
            text: qsTr("Show %1 more unchanged line(s)").arg(Math.min(20, row.count))
            width: ListView.view ? ListView.view.width : implicitWidth
            onClicked: reviewSurface.backend.expandReviewContext(
                reviewSurface.project.id,
                row.hunkId,
                row.direction
            )
        }
    }

    Component {
        id: reviewLineComponent

        Item {
            id: reviewLineDelegate

            property var row: ({})

            readonly property bool hidden: reviewSurface.splitLayout && row.splitHidden === true
            implicitHeight: hidden
                ? 0
                : reviewSurface.splitLayout
                    ? splitLine.implicitHeight
                    : unifiedLine.implicitHeight
            visible: !hidden
            width: ListView.view ? ListView.view.width : implicitWidth

            Rectangle {
                id: unifiedLine

                anchors.left: parent.left
                anchors.right: parent.right
                color: reviewSurface.lineColor(reviewLineDelegate.row.unified.kind)
                implicitHeight: unifiedLayout.implicitHeight + Kirigami.Units.smallSpacing
                visible: !reviewSurface.splitLayout

                RowLayout {
                    id: unifiedLayout

                    anchors.fill: parent
                    anchors.leftMargin: Kirigami.Units.smallSpacing
                    anchors.rightMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 3
                        color: Kirigami.Theme.disabledTextColor
                        font.family: "monospace"
                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        horizontalAlignment: Text.AlignRight
                        text: "%1│%2"
                            .arg(reviewLineDelegate.row.unified.oldLine > 0
                                ? reviewLineDelegate.row.unified.oldLine
                                : "")
                            .arg(reviewLineDelegate.row.unified.newLine > 0
                                ? reviewLineDelegate.row.unified.newLine
                                : "")
                    }

                    Controls.Label {
                        Layout.preferredWidth: Kirigami.Units.gridUnit
                        color: reviewSurface.markerColor(
                            reviewLineDelegate.row.unified.kind
                        )
                        font.bold: true
                        font.family: "monospace"
                        horizontalAlignment: Text.AlignHCenter
                        text: reviewLineDelegate.row.unified.marker
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        font.family: "monospace"
                        text: reviewSurface.highlightedLine(
                            reviewLineDelegate.row.unified.segments,
                            reviewSurface.reviewFile.path || ""
                        )
                        textFormat: Text.RichText
                        wrapMode: Text.WrapAnywhere
                    }
                }
            }

            RowLayout {
                id: splitLine

                anchors.left: parent.left
                anchors.right: parent.right
                spacing: Kirigami.Units.smallSpacing
                visible: reviewSurface.splitLayout

                Repeater {
                    model: [reviewLineDelegate.row.old, reviewLineDelegate.row.new]

                    delegate: Rectangle {
                        id: splitSide

                        required property var modelData

                        Layout.fillWidth: true
                        color: modelData.present === true
                            ? reviewSurface.lineColor(modelData.kind)
                            : reviewSurface.tint(Kirigami.Theme.disabledTextColor, 0.04)
                        implicitHeight: splitSideLayout.implicitHeight
                            + Kirigami.Units.smallSpacing

                        RowLayout {
                            id: splitSideLayout

                            anchors.fill: parent
                            anchors.leftMargin: Kirigami.Units.smallSpacing
                            anchors.rightMargin: Kirigami.Units.smallSpacing
                            spacing: Kirigami.Units.smallSpacing

                            Controls.Label {
                                Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                                color: Kirigami.Theme.disabledTextColor
                                font.family: "monospace"
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                horizontalAlignment: Text.AlignRight
                                text: splitSide.modelData.present === true
                                    && splitSide.modelData.line > 0
                                    ? splitSide.modelData.line
                                    : ""
                            }

                            Controls.Label {
                                Layout.preferredWidth: Kirigami.Units.gridUnit
                                color: splitSide.modelData.present === true
                                    ? reviewSurface.markerColor(splitSide.modelData.kind)
                                    : Kirigami.Theme.disabledTextColor
                                font.bold: true
                                font.family: "monospace"
                                horizontalAlignment: Text.AlignHCenter
                                text: splitSide.modelData.present === true
                                    ? splitSide.modelData.marker
                                    : ""
                            }

                            Controls.Label {
                                Layout.fillWidth: true
                                font.family: "monospace"
                                text: splitSide.modelData.present === true
                                    ? reviewSurface.highlightedLine(
                                        splitSide.modelData.segments,
                                        reviewSurface.reviewFile.path || ""
                                    )
                                    : ""
                                textFormat: Text.RichText
                                wrapMode: Text.WrapAnywhere
                            }
                        }
                    }
                }
            }
        }
    }
}
