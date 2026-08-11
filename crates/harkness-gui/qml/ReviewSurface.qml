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

    readonly property bool reviewReady: backend.review !== undefined
        && backend.review
        && backend.review.projectId !== undefined
        && String(backend.review.projectId) === String(project.id)
    readonly property var reviewState: reviewReady ? backend.review : ({})
    readonly property var reviewFiles: reviewReady && reviewState.files !== undefined
        ? reviewState.files
        : []
    readonly property int reviewFileOffset: reviewReady
        ? Number(reviewState.fileOffset || 0)
        : 0
    readonly property int reviewFileTotal: reviewReady
        ? Number(reviewState.totalFiles || 0)
        : 0
    readonly property var reviewFile: reviewReady
        && reviewState.file !== undefined
        && reviewState.file.fileId !== undefined
        ? reviewState.file
        : ({})
    readonly property var reviewRows: reviewFile.rows !== undefined ? reviewFile.rows : []
    readonly property string repositoryLockScope: String(
        project.lockScope || project.parentId || project.id
    )
    property alias reviewContentY: reviewLineView.contentY
    property alias reviewCurrentIndex: reviewLineView.currentIndex
    property bool splitLayout: false
    property int pendingHunkNavigation: 0

    spacing: Kirigami.Units.smallSpacing

    function job(kind) {
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if ((String(candidate.projectId) === String(project.id)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope)
                    && candidate.kind === kind)
                return candidate;
        }
        return null;
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

    function openReviewLine(line) {
        if (reviewFile.fileId === undefined)
            return;
        backend.openReviewLine(project.id, reviewFile.fileId, Math.max(1, Number(line || 1)));
    }

    function reviewRowDisplayed(row) {
        return !(splitLayout && row.type === "line" && row.splitHidden === true);
    }

    function displayedReviewRowCount() {
        let count = 0;
        for (let index = 0; index < reviewRows.length; ++index) {
            if (reviewRowDisplayed(reviewRows[index]))
                ++count;
        }
        return count;
    }

    function loadReviewRowPage(direction, continueHunkNavigation) {
        if (continueHunkNavigation !== true)
            pendingHunkNavigation = 0;
        const hadFocus = reviewOwnsActiveFocus();
        if (direction === "previous")
            backend.loadPreviousReviewRows(project.id);
        else
            backend.loadMoreReviewRows(project.id);
        Qt.callLater(function() {
            if (continueHunkNavigation === true && pendingHunkNavigation !== 0) {
                const offset = pendingHunkNavigation;
                if (repositoryMutationRunning() || reviewReadRunning()) {
                    pendingHunkNavigation = 0;
                    return;
                }
                continueNavigateHunk(
                    offset,
                    offset > 0 ? -1 : reviewRows.length
                );
                return;
            }
            const origin = direction === "previous" ? reviewRows.length - 1 : 0;
            const focusIndex = nearestHunkIndex(origin);
            reviewLineView.currentIndex = focusIndex;
            if (focusIndex >= 0) {
                reviewLineView.positionViewAtIndex(
                    focusIndex,
                    direction === "previous" ? ListView.End : ListView.Beginning
                );
            } else {
                const maximum = Math.max(
                    0,
                    reviewLineView.contentHeight - reviewLineView.height
                );
                reviewLineView.contentY = direction === "previous" ? maximum : 0;
            }
            if (hadFocus)
                reviewLineView.forceActiveFocus();
        });
    }

    function repositoryMutationRunning() {
        return job("commit") !== null
            || job("fetch") !== null
            || job("pull") !== null
            || job("push") !== null
            || job("checkout") !== null
            || job("create_branch") !== null
            || job("create_worktree") !== null
            || job("reconcile_worktrees") !== null
            || job("move_worktree") !== null
            || job("lock_worktree") !== null
            || job("unlock_worktree") !== null
            || job("remove_worktree") !== null
            || job("remove_managed") !== null;
    }

    function reviewReadRunning() {
        return job("review") !== null
            || job("review_file") !== null
            || job("review_context") !== null;
    }

    function historyReadRunning() {
        return job("history") !== null;
    }

    function repositoryOperationRunning() {
        return repositoryMutationRunning()
            || reviewReadRunning()
            || historyReadRunning()
            || job("status") !== null
            || job("branches") !== null
            || job("worktrees") !== null;
    }

    function hunkNavigationAvailable(offset) {
        const direction = offset < 0 ? -1 : 1;
        let index = reviewLineView.currentIndex;
        if (index < 0)
            index = direction > 0 ? -1 : reviewRows.length;
        for (let candidate = index + direction;
             candidate >= 0 && candidate < reviewRows.length;
             candidate += direction) {
            const row = reviewRows[candidate];
            if (row.type === "hunk"
                    || (row.type === "page"
                        && row.hunkAvailable === true
                        && row.direction === (direction > 0 ? "next" : "previous")))
                return true;
        }
        return false;
    }

    function hunkNavigationEnabled(offset) {
        return hunkNavigationAvailable(offset === undefined ? 1 : offset)
            && !repositoryMutationRunning()
            && !reviewReadRunning();
    }

    function nearestHunkIndex(origin) {
        if (reviewRows.length === 0)
            return -1;
        const start = Math.max(0, Math.min(reviewRows.length - 1, origin));
        for (let distance = 0; distance < reviewRows.length; ++distance) {
            const after = start + distance;
            if (after < reviewRows.length && reviewRows[after].type === "hunk")
                return after;
            const before = start - distance;
            if (before >= 0 && reviewRows[before].type === "hunk")
                return before;
        }
        return -1;
    }

    function focusIsInside(item, ancestor) {
        for (let candidate = item; candidate; candidate = candidate.parent) {
            if (candidate === ancestor)
                return true;
        }
        return false;
    }

    function reviewOwnsActiveFocus() {
        const window = reviewLineView.Window.window;
        return window && focusIsInside(window.activeFocusItem, reviewLineView);
    }

    function selectedFileIndex() {
        const selected = reviewReady ? String(reviewState.selectedFileId || "") : "";
        for (let index = 0; index < reviewFiles.length; ++index) {
            if (String(reviewFiles[index].fileId) === selected)
                return index;
        }
        return -1;
    }

    function fileNavigationAvailable(offset) {
        if (reviewFiles.length === 0)
            return false;
        const index = selectedFileIndex();
        if (index < 0)
            return false;
        if (offset < 0)
            return index > 0 || reviewFileOffset > 0;
        return index + 1 < reviewFiles.length
            || reviewFileOffset + reviewFiles.length < reviewFileTotal;
    }

    function openReviewFileAt(index) {
        if (index < 0 || index >= reviewFiles.length)
            return;
        reviewFileList.currentIndex = index;
        reviewFileList.positionViewAtIndex(index, ListView.Contain);
        backend.loadReviewFile(project.id, reviewFiles[index].fileId);
    }

    function navigateFile(offset) {
        if (!fileNavigationAvailable(offset))
            return;
        pendingHunkNavigation = 0;
        let index = selectedFileIndex();
        if (index < 0)
            return;
        const destination = index + offset;
        if (destination >= 0 && destination < reviewFiles.length) {
            openReviewFileAt(destination);
            return;
        }
        if (offset < 0)
            backend.loadPreviousReviewFiles(project.id);
        else
            backend.loadMoreReviewFiles(project.id);
        Qt.callLater(function() {
            openReviewFileAt(offset < 0 ? reviewFiles.length - 1 : 0);
        });
    }

    function loadReviewFilePage(direction) {
        pendingHunkNavigation = 0;
        if (direction === "previous")
            backend.loadPreviousReviewFiles(project.id);
        else
            backend.loadMoreReviewFiles(project.id);
        Qt.callLater(function() {
            reviewFileList.currentIndex = selectedFileIndex();
            if (reviewFileList.currentIndex < 0 && reviewFiles.length > 0) {
                // A manually browsed file window must not leave the diff on a
                // hidden selection: define the new page boundary as the
                // selection so subsequent Previous/Next remains adjacent.
                openReviewFileAt(
                    direction === "previous" ? reviewFiles.length - 1 : 0
                );
                return;
            }
            if (reviewFileList.currentIndex >= 0)
                reviewFileList.positionViewAtIndex(
                    reviewFileList.currentIndex,
                    ListView.Contain
                );
        });
    }

    function continueNavigateHunk(offset, index) {
        for (let candidate = index + offset;
             candidate >= 0 && candidate < reviewRows.length;
             candidate += offset) {
            if (reviewRows[candidate].type === "hunk") {
                reviewLineView.currentIndex = candidate;
                reviewLineView.positionViewAtIndex(candidate, ListView.Beginning);
                reviewLineView.forceActiveFocus();
                pendingHunkNavigation = 0;
                return;
            }
        }
        const pageDirection = offset > 0 ? "next" : "previous";
        for (let candidate = 0; candidate < reviewRows.length; ++candidate) {
            const row = reviewRows[candidate];
            if (row.type === "page"
                    && row.hunkAvailable === true
                    && row.direction === pageDirection) {
                pendingHunkNavigation = offset;
                loadReviewRowPage(pageDirection, true);
                return;
            }
        }
        pendingHunkNavigation = 0;
    }

    function navigateHunk(offset) {
        if (!hunkNavigationEnabled(offset))
            return;
        pendingHunkNavigation = 0;
        let index = reviewLineView.currentIndex;
        if (index < 0)
            index = offset > 0 ? -1 : reviewRows.length;
        continueNavigateHunk(offset, index);
    }

    onProjectChanged: pendingHunkNavigation = 0

    Shortcut {
        enabled: reviewSurface.reviewReady
            && reviewSurface.fileNavigationAvailable(1)
            && !reviewSurface.repositoryMutationRunning()
            && !reviewSurface.reviewReadRunning()
        sequence: "Alt+J"
        onActivated: reviewSurface.navigateFile(1)
    }

    Shortcut {
        enabled: reviewSurface.reviewReady
            && reviewSurface.fileNavigationAvailable(-1)
            && !reviewSurface.repositoryMutationRunning()
            && !reviewSurface.reviewReadRunning()
        sequence: "Alt+K"
        onActivated: reviewSurface.navigateFile(-1)
    }

    Shortcut {
        enabled: reviewSurface.hunkNavigationEnabled(1)
        sequence: "Alt+Shift+J"
        onActivated: reviewSurface.navigateHunk(1)
    }

    Shortcut {
        enabled: reviewSurface.hunkNavigationEnabled(-1)
        sequence: "Alt+Shift+K"
        onActivated: reviewSurface.navigateHunk(-1)
    }

    // The surface is driven entirely from the column beside it: a changed file,
    // a commit, or a branch comparison picked there is what loads here.
    Item {
        Layout.fillHeight: true
        Layout.fillWidth: true
        visible: !reviewSurface.reviewReady

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            explanation: qsTr("Pick a changed file, a commit, or a branch comparison in the Changes and History tabs.")
            icon.name: "vcs-diff"
            text: qsTr("No diff selected")
            width: Math.min(
                parent.width - Kirigami.Units.gridUnit * 2,
                Kirigami.Units.gridUnit * 24
            )
        }
    }

    ColumnLayout {
        Layout.fillHeight: true
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
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
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

        // An empty comparison is the surface's resting state, not an error:
        // it takes the room the diff would and says so in the middle of it,
        // instead of leaving a stray line of text under the header.
        Item {
            Layout.fillHeight: true
            Layout.fillWidth: true
            visible: reviewSurface.reviewState.loading !== true
                && reviewSurface.reviewFiles.length === 0
                && (!reviewSurface.reviewState.error
                    || reviewSurface.reviewState.error.length === 0)

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                explanation: qsTr("No files changed in this comparison. New edits show up here as soon as they land in the working tree.")
                icon.name: "checkmark"
                text: qsTr("Nothing to review")
                width: Math.min(
                    parent.width - Kirigami.Units.gridUnit * 2,
                    Kirigami.Units.gridUnit * 24
                )
            }
        }

        Kirigami.Separator {
            Layout.fillWidth: true
            visible: reviewSurface.reviewFiles.length > 0
        }

        // GitHub Desktop's arrangement: the summary and description of what is
        // being reviewed sit above, and the changed files run down the left of
        // the diff they open. The handle between them is what lets a deep path
        // and a wide line of code each be read without the other giving way.
        Controls.SplitView {
            Layout.fillHeight: true
            Layout.fillWidth: true
            orientation: Qt.Horizontal
            visible: reviewSurface.reviewFiles.length > 0

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
                objectName: "reviewFileColumn"

                Controls.SplitView.fillWidth: false
                Controls.SplitView.maximumWidth: Kirigami.Units.gridUnit * 30
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 8
                Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 16

                ColumnLayout {
                    anchors.fill: parent
                    anchors.rightMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing

                    ListView {
                        id: reviewFileList

                        Layout.fillHeight: true
                        Layout.fillWidth: true
                        activeFocusOnTab: true
                        boundsBehavior: Flickable.StopAtBounds
                        clip: true
                        keyNavigationEnabled: true
                        model: reviewSurface.reviewFiles
                        reuseItems: true
                        spacing: Kirigami.Units.smallSpacing

                        delegate: Controls.ItemDelegate {
                            id: reviewFileDelegate

                            required property int index
                            required property var modelData

                            Accessible.name: qsTr("%1, %2 change")
                                .arg(modelData.path)
                                .arg(modelData.change)
                            Controls.ToolTip.text: modelData.path
                            Controls.ToolTip.visible: hovered
                            highlighted: String(reviewSurface.reviewState.selectedFileId || "")
                                === String(modelData.fileId)
                            enabled: !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            width: reviewFileList.width
                            onClicked: {
                                reviewSurface.pendingHunkNavigation = 0;
                                reviewFileList.currentIndex = index;
                                reviewSurface.backend.loadReviewFile(
                                    reviewSurface.project.id,
                                    modelData.fileId
                                );
                            }

                            contentItem: RowLayout {
                                spacing: Kirigami.Units.smallSpacing

                                Controls.Label {
                                    Layout.fillWidth: true
                                    elide: Text.ElideMiddle
                                    font.family: "monospace"
                                    text: reviewFileDelegate.modelData.path
                                    textFormat: Text.PlainText
                                }

                                Controls.Label {
                                    color: Kirigami.Theme.disabledTextColor
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                    text: reviewFileDelegate.modelData.change
                                    textFormat: Text.PlainText
                                }
                            }
                        }

                        Controls.ScrollBar.vertical: Controls.ScrollBar {}
                    }

                    // The file window is paged, and the column is too narrow for
                    // worded buttons: the arrows carry the verb in their tooltip
                    // and the count between them says where the window sits.
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 0
                        visible: reviewSurface.reviewFileTotal > reviewSurface.reviewFiles.length

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: text
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.reviewFileOffset > 0
                                && !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            icon.name: "go-previous"
                            text: qsTr("Previous files")
                            onClicked: reviewSurface.loadReviewFilePage("previous")
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            elide: Text.ElideRight
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            horizontalAlignment: Text.AlignHCenter
                            text: qsTr("Files %1–%2 of %3")
                                .arg(reviewSurface.reviewFileOffset + 1)
                                .arg(reviewSurface.reviewFileOffset + reviewSurface.reviewFiles.length)
                                .arg(reviewSurface.reviewFileTotal)
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: text
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.reviewFileOffset + reviewSurface.reviewFiles.length
                                < reviewSurface.reviewFileTotal
                                && !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            icon.name: "go-next"
                            text: qsTr("Next files")
                            onClicked: reviewSurface.loadReviewFilePage("next")
                        }
                    }
                }
            }

            Item {
                objectName: "reviewDiffColumn"

                Controls.SplitView.fillWidth: true
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 18

                ColumnLayout {
                    anchors.fill: parent
                    anchors.leftMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing

                    RowLayout {
                        Layout.fillWidth: true
                        visible: reviewSurface.reviewFile.fileId !== undefined

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Previous file (Alt+K)")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.fileNavigationAvailable(-1)
                                && !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            icon.name: "go-up"
                            text: qsTr("Previous file")
                            onClicked: reviewSurface.navigateFile(-1)
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Next file (Alt+J)")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.fileNavigationAvailable(1)
                                && !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            icon.name: "go-down"
                            text: qsTr("Next file")
                            onClicked: reviewSurface.navigateFile(1)
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Previous hunk (Alt+Shift+K)")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.hunkNavigationEnabled(-1)
                            icon.name: "go-up"
                            text: qsTr("Previous hunk")
                            onClicked: reviewSurface.navigateHunk(-1)
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Next hunk (Alt+Shift+J)")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: reviewSurface.hunkNavigationEnabled(1)
                            icon.name: "go-down"
                            text: qsTr("Next hunk")
                            onClicked: reviewSurface.navigateHunk(1)
                        }

                        Item {
                            Layout.fillWidth: true
                        }

                        Controls.ButtonGroup {
                            id: reviewLayoutGroup
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ButtonGroup.group: reviewLayoutGroup
                            Controls.ToolTip.text: qsTr("Unified layout")
                            Controls.ToolTip.visible: hovered
                            checkable: true
                            checked: !reviewSurface.splitLayout
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "view-list-text"
                            text: qsTr("Unified layout")
                            onClicked: reviewSurface.setSplitLayout(false)
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ButtonGroup.group: reviewLayoutGroup
                            Controls.ToolTip.text: qsTr("Side-by-side layout")
                            Controls.ToolTip.visible: hovered
                            checkable: true
                            checked: reviewSurface.splitLayout
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "view-split-left-right"
                            text: qsTr("Side-by-side layout")
                            onClicked: reviewSurface.setSplitLayout(true)
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        visible: reviewSurface.reviewFile.path !== undefined
                            && String(reviewSurface.reviewFile.path).length > 0

                        Controls.Label {
                            Layout.fillWidth: true
                            elide: Text.ElideMiddle
                            font.bold: true
                            font.family: "monospace"
                            text: reviewSurface.reviewFile.path || ""
                            textFormat: Text.PlainText
                        }

                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Open the first changed line in the configured editor")
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "document-edit"
                            text: qsTr("Open in editor")
                            onClicked: reviewSurface.openReviewLine(
                                reviewSurface.reviewFile.firstLine || 1
                            )
                        }
                    }

                    Kirigami.InlineMessage {
                        Layout.fillWidth: true
                        text: reviewSurface.reviewFile.summary || ""
                        type: Kirigami.MessageType.Information
                        visible: text.length > 0
                    }

                    ListView {
                        id: reviewLineView

                        Accessible.name: qsTr("Changed lines for %1").arg(reviewSurface.reviewFile.path || "")
                        // The diff takes every row the chrome above it does not need, and
                        // scrolls itself rather than riding a scroll view around the whole
                        // surface — which is what used to cap it at a fixed box.
                        Layout.fillHeight: true
                        Layout.fillWidth: true
                        Layout.minimumHeight: Kirigami.Units.gridUnit * 6
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

                            required property int index
                            required property var modelData

                            readonly property var row: modelData
                            sourceComponent: row.type === "hunk"
                                ? reviewHunkComponent
                                : row.type === "collapsed"
                                    ? reviewCollapsedComponent
                                    : row.type === "page"
                                        ? reviewPageComponent
                                        : reviewLineComponent
                            width: reviewLineView.width
                            onLoaded: {
                                item.row = row;
                                item.rowIndex = index;
                            }
                            onIndexChanged: {
                                if (item)
                                    item.rowIndex = index;
                            }
                            onRowChanged: {
                                // A reused Loader may switch component types in the same
                                // turn. Assign after sourceComponent has settled so a
                                // line delegate never receives a hunk row (or vice versa).
                                Qt.callLater(function() {
                                    if (item) {
                                        item.row = row;
                                        item.rowIndex = index;
                                    }
                                });
                            }
                        }

                        Controls.ScrollBar.horizontal: Controls.ScrollBar {}
                        Controls.ScrollBar.vertical: Controls.ScrollBar {}
                    }
                }
            }
        }
    }

    Component {
        id: reviewHunkComponent

        Controls.Frame {
            id: reviewHunk

            property var row: ({})
            property int rowIndex: -1

            Accessible.name: qsTr("Diff hunk %1").arg(row.header || "")
            Accessible.role: Accessible.ListItem
            Accessible.selectable: true
            Accessible.selected: reviewLineView.currentIndex === reviewHunk.rowIndex
            padding: Kirigami.Units.smallSpacing
            width: ListView.view ? ListView.view.width : implicitWidth

            background: Rectangle {
                border.color: reviewLineView.currentIndex === reviewHunk.rowIndex
                    ? Kirigami.Theme.highlightColor
                    : "transparent"
                border.width: reviewLineView.currentIndex === reviewHunk.rowIndex ? 2 : 0
                color: Kirigami.Theme.backgroundColor
                radius: Kirigami.Units.smallSpacing
            }

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.highlightColor
                    font.family: "monospace"
                    text: reviewHunk.row.header || ""
                    textFormat: Text.PlainText
                    wrapMode: Text.WrapAnywhere
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.neutralTextColor
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                        text: reviewHunk.row.degradation || ""
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
            property int rowIndex: -1

            enabled: !reviewSurface.reviewReadRunning()
                && !reviewSurface.repositoryMutationRunning()
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
        id: reviewPageComponent

        Controls.Button {
            property var row: ({})
            property int rowIndex: -1

            enabled: !reviewSurface.reviewReadRunning()
                && !reviewSurface.repositoryMutationRunning()
            flat: true
            icon.name: row.direction === "previous"
                ? "go-up-symbolic"
                : "go-down-symbolic"
            text: row.direction === "previous"
                ? qsTr("Show previous changed lines (%1 before)").arg(row.count)
                : qsTr("Show next changed lines (%1 remaining)").arg(row.count)
            width: ListView.view ? ListView.view.width : implicitWidth
            onClicked: reviewSurface.loadReviewRowPage(row.direction)
        }
    }

    Component {
        id: reviewLineComponent

        Item {
            id: reviewLineDelegate

            property var row: ({})
            property int rowIndex: -1

            readonly property var unified: row.unified || ({
                "oldLine": 0,
                "newLine": 0,
                "kind": "context",
                "marker": "",
                "segments": []
            })
            readonly property bool hidden: !reviewSurface.reviewRowDisplayed(row)
            activeFocusOnTab: !hidden
            Accessible.name: qsTr("Open diff line %1 in editor").arg(row.openLine || 1)
            Accessible.role: Accessible.Button
            Controls.ToolTip.text: qsTr("Open line %1 in editor").arg(row.openLine || 1)
            Controls.ToolTip.visible: reviewLineHover.hovered
            implicitHeight: hidden
                ? 0
                : reviewSurface.splitLayout
                    ? splitLine.implicitHeight
                    : unifiedLine.implicitHeight
            visible: !hidden
            width: ListView.view ? ListView.view.width : implicitWidth

            Keys.onEnterPressed: reviewSurface.openReviewLine(row.openLine)
            Keys.onReturnPressed: reviewSurface.openReviewLine(row.openLine)

            HoverHandler {
                id: reviewLineHover
            }

            TapHandler {
                acceptedButtons: Qt.LeftButton
                onTapped: {
                    reviewLineView.currentIndex = reviewLineDelegate.rowIndex;
                    reviewSurface.openReviewLine(reviewLineDelegate.row.openLine);
                }
            }

            Rectangle {
                id: unifiedLine

                anchors.left: parent.left
                anchors.right: parent.right
                color: reviewSurface.lineColor(reviewLineDelegate.unified.kind)
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
                            .arg(reviewLineDelegate.unified.oldLine > 0
                                ? reviewLineDelegate.unified.oldLine
                                : "")
                            .arg(reviewLineDelegate.unified.newLine > 0
                                ? reviewLineDelegate.unified.newLine
                                : "")
                    }

                    Controls.Label {
                        Layout.preferredWidth: Kirigami.Units.gridUnit
                        color: reviewSurface.markerColor(
                            reviewLineDelegate.unified.kind
                        )
                        font.bold: true
                        font.family: "monospace"
                        horizontalAlignment: Text.AlignHCenter
                        text: reviewLineDelegate.unified.marker
                    }

                    Controls.Label {
                        Layout.fillWidth: true
                        font.family: "monospace"
                        text: reviewSurface.highlightedLine(
                            reviewLineDelegate.unified.segments,
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
                    model: [
                        reviewLineDelegate.row.old || ({}),
                        reviewLineDelegate.row.new || ({})
                    ]

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
