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
    readonly property bool reviewListHasActiveFocus: reviewLineView.activeFocus
    property bool splitLayout: false
    property real heldReviewContentY: 0
    property real heldReviewViewportOffset: 0
    property string heldReviewPathId: ""
    property string heldReviewProjectId: ""
    property int heldReviewRowIndex: -1
    property int heldReviewOldStart: 0
    property int heldReviewOldLines: 0
    property int heldReviewNewStart: 0
    property int heldReviewNewLines: 0
    property string heldReviewAction: ""
    property bool heldReviewHadFocus: false
    property bool heldRestorationScheduled: false
    property int heldRestorationAttempts: 0
    property int heldRestorationStableTicks: 0
    property real heldRestorationLastDelegateY: Number.NaN
    property real heldRestorationLastContentHeight: Number.NaN
    readonly property int heldRestorationMaxAttempts: 80
    property bool restorePositionAfterMutation: false
    property int pendingHunkNavigation: 0
    property var selectedReviewLineIds: []
    property string reviewLineSelectionAnchor: ""
    readonly property int selectedReviewLineCount: selectedReviewLineIds.length
    // Every changed-line delegate tests membership on every rebind, so the
    // selection is kept as a lookup as well as an ordered list.
    readonly property var selectedReviewLineIndex: {
        const index = {};
        for (let position = 0; position < selectedReviewLineIds.length; ++position)
            index[selectedReviewLineIds[position]] = true;
        return index;
    }
    readonly property string reviewLineSelectionScope: String(project.id)
        + "|" + String(reviewState.title || "")
        + "|" + String(reviewState.detail || "")
        + "|" + String(reviewFile.fileId || "")

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

    function selectedLineColor(kind, lineId) {
        return isReviewLineSelected(lineId)
            ? tint(Kirigami.Theme.highlightColor, 0.32)
            : lineColor(kind);
    }

    function isReviewLineSelected(lineId) {
        const id = String(lineId || "");
        return id.length > 0 && selectedReviewLineIndex[id] === true;
    }

    function reviewLineIds(row) {
        const ids = [];
        if (!row || row.type !== "line")
            return ids;
        if (!splitLayout) {
            const unifiedId = String((row.unified || {}).lineId || "");
            if (unifiedId.length > 0)
                ids.push(unifiedId);
            return ids;
        }
        const oldId = String((row.old || {}).lineId || "");
        const newId = String((row.new || {}).lineId || "");
        if (oldId.length > 0)
            ids.push(oldId);
        if (newId.length > 0 && ids.indexOf(newId) === -1)
            ids.push(newId);
        return ids;
    }

    // Range order is always the unified one: it lists every changed line of the
    // window exactly once and in file order, which is what a split row's two
    // sides collapse back to.
    function orderedReviewLineIds() {
        const ids = [];
        for (let index = 0; index < reviewRows.length; ++index) {
            const row = reviewRows[index];
            if (row.type !== "line")
                continue;
            const id = String((row.unified || {}).lineId || "");
            if (id.length > 0 && ids.indexOf(id) === -1)
                ids.push(id);
        }
        return ids;
    }

    function setReviewLinesSelected(ids, select) {
        if (repositoryOperationRunning())
            return false;
        let selected = selectedReviewLineIds.slice();
        let changed = false;
        let anchor = "";
        for (let index = 0; index < ids.length; ++index) {
            const id = String(ids[index] || "");
            if (id.length === 0)
                continue;
            if (anchor.length === 0)
                anchor = id;
            const existing = selected.indexOf(id);
            if (select && existing === -1) {
                selected.push(id);
                changed = true;
            } else if (!select && existing !== -1) {
                selected.splice(existing, 1);
                changed = true;
            }
        }
        if (!changed)
            return false;
        selectedReviewLineIds = selected;
        reviewLineSelectionAnchor = anchor;
        return true;
    }

    function toggleReviewLine(lineId, extendRange) {
        const id = String(lineId || "");
        if (id.length === 0 || repositoryOperationRunning())
            return false;
        if (extendRange === true && reviewLineSelectionAnchor.length > 0) {
            const ordered = orderedReviewLineIds();
            const anchorIndex = ordered.indexOf(reviewLineSelectionAnchor);
            const selectedIndex = ordered.indexOf(id);
            if (anchorIndex >= 0 && selectedIndex >= 0) {
                let selected = selectedReviewLineIds.slice();
                const first = Math.min(anchorIndex, selectedIndex);
                const last = Math.max(anchorIndex, selectedIndex);
                for (let index = first; index <= last; ++index) {
                    if (selected.indexOf(ordered[index]) === -1)
                        selected.push(ordered[index]);
                }
                selectedReviewLineIds = selected;
                return true;
            }
        }
        return setReviewLinesSelected([id], !isReviewLineSelected(id));
    }

    function toggleCurrentReviewLine(extendRange) {
        if (reviewLineView.currentIndex < 0
                || reviewLineView.currentIndex >= reviewRows.length)
            return false;
        const ids = reviewLineIds(reviewRows[reviewLineView.currentIndex]);
        if (ids.length === 0)
            return false;
        if (extendRange === true && reviewLineSelectionAnchor.length > 0) {
            let extended = false;
            for (let index = 0; index < ids.length; ++index)
                extended = toggleReviewLine(ids[index], true) || extended;
            return extended;
        }
        // A split row carries both sides of a replacement. Toggling each side in
        // turn would invert the pair whenever only one of them was selected, so
        // the row acts as a single control: it selects unless already whole.
        let whole = true;
        for (let index = 0; index < ids.length && whole; ++index)
            whole = isReviewLineSelected(ids[index]);
        return setReviewLinesSelected(ids, !whole);
    }

    function clearReviewLineSelection() {
        selectedReviewLineIds = [];
        reviewLineSelectionAnchor = "";
    }

    // The verb belongs to the loaded file, not to any row: a selection outlives
    // the row window, and scanning the visible rows for it used to hide the
    // action button as soon as the selected line paged out of view.
    function selectedReviewLineAction() {
        return String(reviewFile.lineAction || "");
    }

    function selectedReviewLineAnchorRow() {
        for (let index = 0; index < reviewRows.length; ++index) {
            const row = reviewRows[index];
            const ids = reviewLineIds(row);
            for (let idIndex = 0; idIndex < ids.length; ++idIndex) {
                if (isReviewLineSelected(ids[idIndex])) {
                    // The hunk header carries the coordinates the refresh
                    // anchors on, but a hunk taller than one row page can leave
                    // it outside the window. The selected row is then the
                    // closest anchor there is, and it is visible by definition.
                    return reviewHunkRow(row.hunkId) || row;
                }
            }
        }
        return null;
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

    // Moves the keyboard cursor, which is what the Space binding acts on.
    function setCurrentReviewRow(index) {
        if (index < 0 || index >= reviewRows.length)
            return false;
        reviewLineView.currentIndex = index;
        return true;
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
                if (repositoryMutationRunning()
                        || reviewReadRunning()
                        || restorePositionAfterMutation) {
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

    function reviewHunkRow(hunkId) {
        for (let index = 0; index < reviewRows.length; ++index) {
            const row = reviewRows[index];
            if (row.type === "hunk"
                    && String(row.hunkId || "") === String(hunkId || ""))
                return row;
        }
        return null;
    }

    function holdReviewMutationPosition(row) {
        heldReviewContentY = reviewLineView.contentY;
        heldReviewViewportOffset = 0;
        heldReviewPathId = String(reviewFile.pathId || "");
        heldReviewProjectId = String(project.id);
        heldReviewOldStart = Number(row.oldStart || 0);
        heldReviewOldLines = Number(row.oldLines || 0);
        heldReviewNewStart = Number(row.newStart || 0);
        heldReviewNewLines = Number(row.newLines || 0);
        heldReviewAction = String(row.action || "");
        heldReviewHadFocus = reviewOwnsActiveFocus();
        heldReviewRowIndex = -1;
        for (let index = 0; index < reviewRows.length; ++index) {
            if (reviewRows[index].type === "hunk"
                    && String(reviewRows[index].hunkId || "") === String(row.hunkId || "")) {
                heldReviewRowIndex = index;
                const delegate = reviewLineView.itemAtIndex(index);
                if (delegate)
                    heldReviewViewportOffset = delegate.y - reviewLineView.contentY;
                break;
            }
        }
        heldRestorationAttempts = 0;
        heldRestorationStableTicks = 0;
        heldRestorationLastDelegateY = Number.NaN;
        heldRestorationLastContentHeight = Number.NaN;
        restorePositionAfterMutation = true;
    }

    function mutateHunk(row) {
        if (!row || (row.action !== "stage" && row.action !== "unstage"))
            return;
        holdReviewMutationPosition(row);
        if (row.action === "stage") {
            backend.stageHunk(project.id, row.hunkId);
        } else {
            backend.unstageHunk(project.id, row.hunkId);
        }
        Qt.callLater(function() {
            if (restorePositionAfterMutation
                    && !repositoryMutationRunning()
                    && !reviewReadRunning())
                clearHeldPosition();
        });
    }

    function mutateSelectedLines() {
        const action = selectedReviewLineAction();
        if (selectedReviewLineIds.length === 0
                || (action !== "stage" && action !== "unstage"))
            return;
        // A selection can outlive the rows that produced it, leaving nothing on
        // screen to anchor on. Only the scroll restoration depends on that; the
        // mutation itself must still happen.
        const anchorRow = selectedReviewLineAnchorRow();
        if (anchorRow)
            holdReviewMutationPosition(anchorRow);
        const lineIds = selectedReviewLineIds.join("\n");
        clearReviewLineSelection();
        if (action === "stage")
            backend.stageLines(project.id, lineIds);
        else
            backend.unstageLines(project.id, lineIds);
        Qt.callLater(function() {
            if (restorePositionAfterMutation
                    && !repositoryMutationRunning()
                    && !reviewReadRunning())
                clearHeldPosition();
        });
    }

    function repositoryMutationRunning() {
        return job("stage") !== null
            || job("unstage") !== null
            || job("stage_hunk") !== null
            || job("unstage_hunk") !== null
            || job("commit") !== null
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
            && !reviewReadRunning()
            && !restorePositionAfterMutation;
    }

    function clearHeldPosition() {
        restorePositionAfterMutation = false;
        heldReviewPathId = "";
        heldReviewProjectId = "";
        heldReviewRowIndex = -1;
        heldReviewOldStart = 0;
        heldReviewOldLines = 0;
        heldReviewNewStart = 0;
        heldReviewNewLines = 0;
        heldReviewAction = "";
        heldReviewHadFocus = false;
        heldRestorationScheduled = false;
        heldRestorationAttempts = 0;
        heldRestorationStableTicks = 0;
        heldRestorationLastDelegateY = Number.NaN;
        heldRestorationLastContentHeight = Number.NaN;
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

    function coordinateDistance(start, lines, anchorStart, anchorLines) {
        const firstStart = Number(start || 0);
        const secondStart = Number(anchorStart || 0);
        const firstEnd = firstStart + Math.max(1, Number(lines || 0)) - 1;
        const secondEnd = secondStart + Math.max(1, Number(anchorLines || 0)) - 1;
        if (firstEnd >= secondStart && secondEnd >= firstStart)
            return 0;
        return firstEnd < secondStart
            ? secondStart - firstEnd
            : firstStart - secondEnd;
    }

    function semanticHunkIndex() {
        if (heldReviewNewStart <= 0 && heldReviewOldStart <= 0)
            return nearestHunkIndex(heldReviewRowIndex);
        let bestIndex = -1;
        let bestCoordinateDistance = Number.MAX_VALUE;
        let bestSpanDistance = Number.MAX_VALUE;
        let bestRowDistance = Number.MAX_VALUE;
        for (let index = 0; index < reviewRows.length; ++index) {
            const row = reviewRows[index];
            if (row.type !== "hunk")
                continue;
            const destinationStart = heldReviewAction === "stage"
                ? row.newStart
                : row.oldStart;
            const destinationLines = heldReviewAction === "stage"
                ? row.newLines
                : row.oldLines;
            const sourceStart = heldReviewAction === "stage"
                ? heldReviewOldStart
                : heldReviewNewStart;
            const sourceLines = heldReviewAction === "stage"
                ? heldReviewOldLines
                : heldReviewNewLines;
            const distance = coordinateDistance(
                destinationStart,
                destinationLines,
                sourceStart,
                sourceLines
            );
            const spanDistance = Math.abs(
                Number(destinationLines || 0) - Number(sourceLines || 0)
            );
            const rowDistance = Math.abs(index - heldReviewRowIndex);
            if (distance < bestCoordinateDistance
                    || (distance === bestCoordinateDistance
                        && spanDistance < bestSpanDistance)
                    || (distance === bestCoordinateDistance
                        && spanDistance === bestSpanDistance
                        && rowDistance < bestRowDistance)) {
                bestIndex = index;
                bestCoordinateDistance = distance;
                bestSpanDistance = spanDistance;
                bestRowDistance = rowDistance;
            }
        }
        return bestCoordinateDistance === 0 ? bestIndex : -1;
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

    function mayRestoreReviewFocus() {
        const window = reviewLineView.Window.window;
        return !window || !window.activeFocusItem
            || focusIsInside(window.activeFocusItem, reviewLineView);
    }

    function restoreHeldPosition() {
        if (!restorePositionAfterMutation
                || !reviewReady
                || reviewState.loading === true
                || reviewState.fileLoading === true
                || repositoryMutationRunning()
                || reviewReadRunning()
                || heldRestorationScheduled)
            return;
        if (String(project.id) !== heldReviewProjectId
                || String(reviewFile.pathId || "") !== heldReviewPathId) {
            clearHeldPosition();
            return;
        }
        heldRestorationScheduled = true;
        Qt.callLater(function() {
            reviewSurface.restoreHeldPositionStep();
        });
    }

    function restoreHeldPositionStep() {
        if (!restorePositionAfterMutation) {
            heldRestorationScheduled = false;
            return;
        }
        if (String(project.id) !== heldReviewProjectId
                || String(reviewFile.pathId || "") !== heldReviewPathId) {
            clearHeldPosition();
            return;
        }
        if (reviewState.loading === true
                || reviewState.fileLoading === true
                || repositoryMutationRunning()
                || reviewReadRunning()) {
            heldRestorationScheduled = false;
            heldRestorationStableTicks = 0;
            return;
        }

        ++heldRestorationAttempts;
        const focusIndex = semanticHunkIndex();
        let delegate = null;
        if (focusIndex >= 0) {
            const indexChanged = reviewLineView.currentIndex !== focusIndex;
            reviewLineView.currentIndex = focusIndex;
            delegate = reviewLineView.itemAtIndex(focusIndex);
            if (indexChanged || !delegate) {
                reviewLineView.positionViewAtIndex(focusIndex, ListView.Contain);
                reviewLineView.forceLayout();
                delegate = reviewLineView.itemAtIndex(focusIndex);
            }
            if (!delegate) {
                heldRestorationStableTicks = 0;
                heldRestorationLastDelegateY = Number.NaN;
                heldRestorationLastContentHeight = reviewLineView.contentHeight;
                if (heldRestorationAttempts >= heldRestorationMaxAttempts) {
                    clearHeldPosition();
                } else {
                    Qt.callLater(function() {
                        reviewSurface.restoreHeldPositionStep();
                    });
                }
                return;
            }
        }

        reviewLineView.forceLayout();
        if (focusIndex >= 0)
            delegate = reviewLineView.itemAtIndex(focusIndex);
        if (focusIndex >= 0 && !delegate) {
            heldRestorationStableTicks = 0;
            heldRestorationLastDelegateY = Number.NaN;
            heldRestorationLastContentHeight = reviewLineView.contentHeight;
            if (heldRestorationAttempts >= heldRestorationMaxAttempts) {
                clearHeldPosition();
            } else {
                Qt.callLater(function() {
                    reviewSurface.restoreHeldPositionStep();
                });
            }
            return;
        }
        const maximum = Math.max(
            0,
            reviewLineView.contentHeight - reviewLineView.height
        );
        const desiredPosition = Math.max(
            0,
            Math.min(
                delegate
                    ? delegate.y - heldReviewViewportOffset
                    : heldReviewContentY,
                maximum
            )
        );
        const positionStable = Math.abs(
            reviewLineView.contentY - desiredPosition
        ) < 1;
        const heightStable = isFinite(heldRestorationLastContentHeight)
            && Math.abs(
                reviewLineView.contentHeight
                    - heldRestorationLastContentHeight
            ) < 1;
        const delegateStable = !delegate
            || (isFinite(heldRestorationLastDelegateY)
                && Math.abs(delegate.y - heldRestorationLastDelegateY) < 1);
        if (!positionStable)
            reviewLineView.contentY = desiredPosition;

        const viewportStable = positionStable
            && heightStable
            && delegateStable
            && (focusIndex < 0
                || reviewLineView.currentIndex === focusIndex);
        const focusMayBeRestored = heldReviewHadFocus
            && mayRestoreReviewFocus();
        if (viewportStable && focusMayBeRestored)
            reviewLineView.forceActiveFocus();
        const focusStable = !focusMayBeRestored
            || reviewLineView.activeFocus;
        heldRestorationStableTicks = viewportStable && focusStable
            ? heldRestorationStableTicks + 1
            : 0;
        heldRestorationLastDelegateY = delegate ? delegate.y : Number.NaN;
        heldRestorationLastContentHeight = reviewLineView.contentHeight;

        if (heldRestorationStableTicks >= 3
                || heldRestorationAttempts >= heldRestorationMaxAttempts) {
            clearHeldPosition();
            return;
        }
        Qt.callLater(function() {
            reviewSurface.restoreHeldPositionStep();
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

    function activateCurrentHunkAction() {
        const loader = reviewLineView.itemAtIndex(reviewLineView.currentIndex);
        const button = loader && loader.item ? loader.item.actionButton : null;
        if (!button || !button.visible || !button.enabled)
            return false;
        button.forceActiveFocus();
        button.click();
        return true;
    }

    function currentReviewViewportOffset() {
        const delegate = reviewLineView.itemAtIndex(reviewLineView.currentIndex);
        return delegate ? delegate.y - reviewLineView.contentY : Number.NaN;
    }

    onProjectChanged: {
        pendingHunkNavigation = 0;
        clearReviewLineSelection();
        clearHeldPosition();
    }
    onReviewLineSelectionScopeChanged: clearReviewLineSelection()
    onReviewStateChanged: restoreHeldPosition()

    Connections {
        target: reviewSurface.backend

        function onJobsChanged() {
            // A mutation job is replaced by a review-refresh job in the same
            // backend callback. Re-check on the next turn so the transient gap
            // cannot consume the held scroll position.
            Qt.callLater(function() {
                reviewSurface.restoreHeldPosition();
            });
        }
    }

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
    Kirigami.PlaceholderMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.gridUnit * 2
        explanation: qsTr("Pick a changed file, a commit, or a branch comparison in the Changes and History tabs.")
        icon.name: "vcs-diff"
        text: qsTr("No diff selected")
        visible: !reviewSurface.reviewReady
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
            visible: reviewSurface.reviewFileTotal > reviewSurface.reviewFiles.length

            Controls.Button {
                enabled: reviewSurface.reviewFileOffset > 0
                    && !reviewSurface.repositoryMutationRunning()
                    && !reviewSurface.reviewReadRunning()
                icon.name: "go-previous"
                text: qsTr("Previous files")
                onClicked: reviewSurface.loadReviewFilePage("previous")
            }

            Controls.Label {
                Layout.fillWidth: true
                horizontalAlignment: Text.AlignHCenter
                text: qsTr("Files %1–%2 of %3")
                    .arg(reviewSurface.reviewFileOffset + 1)
                    .arg(reviewSurface.reviewFileOffset + reviewSurface.reviewFiles.length)
                    .arg(reviewSurface.reviewFileTotal)
            }

            Controls.Button {
                enabled: reviewSurface.reviewFileOffset + reviewSurface.reviewFiles.length
                    < reviewSurface.reviewFileTotal
                    && !reviewSurface.repositoryMutationRunning()
                    && !reviewSurface.reviewReadRunning()
                icon.name: "go-next"
                text: qsTr("Next files")
                onClicked: reviewSurface.loadReviewFilePage("next")
            }
        }

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

            Controls.Button {
                readonly property string lineAction: reviewSurface.selectedReviewLineAction()

                Accessible.name: text
                enabled: reviewSurface.selectedReviewLineCount > 0
                    && !reviewSurface.repositoryOperationRunning()
                icon.name: lineAction === "stage"
                    ? "list-add"
                    : "edit-undo"
                text: lineAction === "stage"
                    ? qsTr("Stage %1 selected line(s)").arg(
                        reviewSurface.selectedReviewLineCount
                    )
                    : qsTr("Unstage %1 selected line(s)").arg(
                        reviewSurface.selectedReviewLineCount
                    )
                visible: reviewSurface.selectedReviewLineCount > 0
                    && (lineAction === "stage" || lineAction === "unstage")
                onClicked: reviewSurface.mutateSelectedLines()
            }

            Controls.ToolButton {
                Accessible.name: text
                Controls.ToolTip.text: text
                Controls.ToolTip.visible: hovered
                display: Controls.AbstractButton.IconOnly
                enabled: reviewSurface.selectedReviewLineCount > 0
                icon.name: "edit-clear"
                text: qsTr("Clear selected lines")
                visible: reviewSurface.selectedReviewLineCount > 0
                onClicked: reviewSurface.clearReviewLineSelection()
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

            Accessible.name: qsTr("Changed lines for %1").arg(reviewSurface.reviewFile.path || "")
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

            Keys.onPressed: function(event) {
                if (event.key === Qt.Key_Space
                        && reviewSurface.toggleCurrentReviewLine(
                            (event.modifiers & Qt.ShiftModifier) !== 0
                        )) {
                    event.accepted = true;
                }
            }

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

    Component {
        id: reviewHunkComponent

        Controls.Frame {
            id: reviewHunk

            property var row: ({})
            property int rowIndex: -1
            property alias actionButton: reviewHunkAction

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

                RowLayout {
                    Layout.fillWidth: true

                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.highlightColor
                        font.family: "monospace"
                        text: reviewHunk.row.header || ""
                        textFormat: Text.PlainText
                        wrapMode: Text.WrapAnywhere
                    }

                    Controls.Button {
                        id: reviewHunkAction

                        enabled: !reviewSurface.repositoryOperationRunning()
                        text: reviewHunk.row.action === "stage"
                            ? qsTr("Stage hunk")
                            : qsTr("Unstage hunk")
                        visible: reviewHunk.row.action === "stage"
                            || reviewHunk.row.action === "unstage"
                        onClicked: reviewSurface.mutateHunk(reviewHunk.row)
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.neutralTextColor
                    font: Kirigami.Theme.smallFont
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
                color: reviewSurface.selectedLineColor(
                    reviewLineDelegate.unified.kind,
                    reviewLineDelegate.unified.lineId
                )
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

                MouseArea {
                    anchors.fill: parent
                    Accessible.checked: reviewSurface.isReviewLineSelected(
                        reviewLineDelegate.unified.lineId
                    )
                    Accessible.name: reviewLineDelegate.unified.kind === "deletion"
                        ? qsTr("Select removed line %1").arg(
                            reviewLineDelegate.unified.oldLine
                        )
                        : qsTr("Select added line %1").arg(
                            reviewLineDelegate.unified.newLine
                        )
                    Accessible.role: Accessible.CheckBox
                    cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
                    enabled: String(reviewLineDelegate.unified.lineId || "").length > 0
                        && !reviewSurface.repositoryOperationRunning()
                    onClicked: function(mouse) {
                        reviewLineView.currentIndex = reviewLineDelegate.rowIndex;
                        reviewLineView.forceActiveFocus();
                        reviewSurface.toggleReviewLine(
                            reviewLineDelegate.unified.lineId,
                            (mouse.modifiers & Qt.ShiftModifier) !== 0
                        );
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

                        readonly property string lineId: String(modelData.lineId || "")

                        Layout.fillWidth: true
                        color: modelData.present === true
                            ? reviewSurface.selectedLineColor(modelData.kind, lineId)
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

                        MouseArea {
                            anchors.fill: parent
                            Accessible.checked: reviewSurface.isReviewLineSelected(
                                splitSide.lineId
                            )
                            Accessible.name: splitSide.modelData.kind === "deletion"
                                ? qsTr("Select removed line %1").arg(
                                    splitSide.modelData.line || ""
                                )
                                : qsTr("Select added line %1").arg(
                                    splitSide.modelData.line || ""
                                )
                            Accessible.role: Accessible.CheckBox
                            cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
                            enabled: splitSide.lineId.length > 0
                                && !reviewSurface.repositoryOperationRunning()
                            onClicked: function(mouse) {
                                reviewLineView.currentIndex = reviewLineDelegate.rowIndex;
                                reviewLineView.forceActiveFocus();
                                reviewSurface.toggleReviewLine(
                                    splitSide.lineId,
                                    (mouse.modifiers & Qt.ShiftModifier) !== 0
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
