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
    readonly property var reviewProvenance: reviewReady && reviewState.provenance !== undefined
        ? reviewState.provenance
        : ({})
    readonly property string repositoryLockScope: String(
        project.lockScope || project.parentId || project.id
    )
    property alias reviewContentY: reviewLineView.contentY
    property alias reviewCurrentIndex: reviewLineView.currentIndex
    // Whether every whitespace byte is drawn, rather than only the runs that
    // are wrong on their own. It belongs to the surface and not to a file:
    // a reader who turned it on is auditing indentation, not one file's.
    property bool revealWhitespace: false
    property bool splitLayout: false
    // Which half of a side-by-side row the open context menu was asked for,
    // empty for a unified row.
    property string menuCopySide: ""
    property int pendingHunkNavigation: 0
    property string pendingDiscardKind: ""
    property string pendingDiscardId: ""

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

    // The colour that says "these two files came from the same hands".
    //
    // The group is an index over the review's *distinct* producer sets, so the
    // only question the colour has to answer is same-or-different — which is
    // why the hues are spread by the golden ratio rather than ordered along a
    // scale a reader would try to rank. Colour is never the only signal: every
    // row carries the names beside it, and an unattributed row is drawn with
    // no bar at all rather than with a colour of its own.
    function provenanceTint(group) {
        const index = Number(group);
        if (!(index >= 0))
            return "transparent";
        const dark = Kirigami.Theme.backgroundColor.hslLightness < 0.5;
        return Qt.hsla((index * 0.6180339887498949) % 1, 0.55, dark ? 0.62 : 0.44, 1);
    }

    // What the whole review can say about where its files came from. Silence
    // is deliberate when nothing was resolved: the rows say "unknown" for
    // themselves, and a header repeating it for every file adds nothing.
    function provenanceHeadline() {
        const provenance = reviewProvenance;
        if (provenance.resolved !== true)
            return "";
        const commits = Number(provenance.commitCount || 0);
        if (commits === 0)
            return qsTr("No commit in this comparison produced these changes");
        const parts = [
            qsTr("%1 commits by %2 contributors")
                .arg(commits)
                .arg(Number(provenance.producerCount || 0))
        ];
        const slug = String(provenance.agentSlug || "");
        if (slug.length > 0)
            parts.push(qsTr("agent %1").arg(slug));
        const skipped = Number(provenance.skippedMerges || 0);
        if (skipped > 0)
            parts.push(qsTr("%1 merges left unattributed").arg(skipped));
        if (String(provenance.truncation || "") === "commit_budget_exhausted")
            parts.push(qsTr("older commits were not walked"));
        return parts.join(" · ");
    }

    // Absence is a first-class answer, so an unattributed file says so in as
    // many letters and is never left blank.
    function provenanceLabel(row) {
        const label = String((row || ({})).provenanceLabel || "");
        return label.length > 0 ? label : qsTr("Unknown");
    }

    // Names the producers, for a plain-text label and for a screen reader.
    function provenanceDetail(row) {
        const entry = row || ({});
        const gap = String(entry.provenanceGap || "");
        if (gap.length === 0 && String(entry.provenanceLabel || "").length > 0) {
            return qsTr("Produced by %1, across %2 commits in this comparison")
                .arg(entry.provenanceLabel)
                .arg(Number(entry.provenanceCommits || 0));
        }
        return qsTr("Unknown: %1").arg(provenanceGapText(gap));
    }

    // The same fact with no producer name in it, for a tool tip.
    //
    // A tool tip's text is rendered by the style's own label, which this file
    // cannot give `Text.PlainText` — and `Text.AutoText` treats anything that
    // looks like markup as markup. A producer name comes out of a commit
    // object, which is repository content a remote controls: a name shaped
    // like an image tag would fetch a URL on hover. The names are already on
    // the row beside the tool tip, as plain text, so counting them here costs
    // a reader nothing.
    function provenanceTooltip(row) {
        const entry = row || ({});
        const gap = String(entry.provenanceGap || "");
        if (gap.length > 0 || String(entry.provenanceLabel || "").length === 0)
            return qsTr("Unknown: %1").arg(provenanceGapText(gap));
        return qsTr("Produced by %1 contributors across %2 commits in this comparison")
            .arg(Number(entry.provenanceProducers || 0))
            .arg(Number(entry.provenanceCommits || 0));
    }

    function provenanceGapText(gap) {
        switch (String(gap)) {
        case "uncommitted":
            return qsTr("nothing has committed this content yet");
        case "empty_range":
            return qsTr("the two sides of this comparison name one commit");
        case "not_in_range":
            return qsTr("no commit in this comparison names this file");
        case "commit_budget_exhausted":
            return qsTr("it is beyond the commits that were walked");
        default:
            return qsTr("attribution is unavailable");
        }
    }

    // Blends `over` into `base` and returns an opaque colour. Every tint the
    // reveal treatment paints ends up inside a rich-text span, which cannot
    // see through itself to the row behind it, so the blend is resolved here
    // instead of being left to an alpha channel.
    function mixColor(base, over, amount) {
        return Qt.rgba(
            base.r + (over.r - base.r) * amount,
            base.g + (over.g - base.g) * amount,
            base.b + (over.b - base.b) * amount,
            1
        );
    }

    // What `lineColor` looks like once it has been painted: that function
    // returns a translucent tint, and a span drawn on top of it has to start
    // from the colour the row actually ended up.
    function lineBackground(kind) {
        if (kind === "addition") {
            return mixColor(
                Kirigami.Theme.backgroundColor,
                Kirigami.Theme.positiveTextColor,
                0.14
            );
        }
        if (kind === "deletion") {
            return mixColor(
                Kirigami.Theme.backgroundColor,
                Kirigami.Theme.negativeTextColor,
                0.14
            );
        }
        return Kirigami.Theme.backgroundColor;
    }

    // Trailing whitespace on a line this diff touched is tinted with the
    // reveal control off, because it is the change a reader is least likely to
    // think to look for and the one most likely to be an accident. On a
    // context line it is neither — it is whatever the file already had, and
    // marking it would light up untouched code in a legacy file — so that one
    // waits for reveal. A changed run is tinted whatever it is, so that
    // intra-line emphasis over a range that is entirely whitespace marks
    // something the eye can find rather than an apparently empty box.
    function whitespaceBackground(segment, kind) {
        if (segment.changed === true) {
            return mixColor(
                lineBackground(kind),
                Kirigami.Theme.highlightColor,
                0.34
            ).toString();
        }
        if (segment.zone === "trailing" && (revealWhitespace || kind !== "context")) {
            return mixColor(
                lineBackground(kind),
                Kirigami.Theme.neutralTextColor,
                0.32
            ).toString();
        }
        if (revealWhitespace) {
            return mixColor(
                lineBackground(kind),
                Kirigami.Theme.disabledTextColor,
                0.16
            ).toString();
        }
        return "";
    }

    // Spaces and tabs become entities because rich text collapses runs of
    // them. Revealed, each one becomes a glyph of the same advance width — a
    // tab stays four columns wide and a space stays one — so no column moves
    // between the two states and the side-by-side columns stay aligned.
    //
    // The reveal pass rewrites both byte kinds in a single scan on purpose:
    // the markup it inserts contains spaces of its own, which a second pass
    // over the result would go on to replace.
    function escapeCode(value) {
        const escaped = String(value)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;");
        if (!revealWhitespace) {
            return escaped
                .replace(/ /g, "&nbsp;")
                .replace(/\t/g, "&nbsp;&nbsp;&nbsp;&nbsp;");
        }
        // Both glyphs are Latin-1, which the monospace face the diff is set
        // in covers itself. An arrow reads better and is what an editor would
        // use, but it falls back to another font here, which draws it at that
        // font's size and off the baseline the rest of the line sits on.
        const opening = "<span style=\"color:"
            + Kirigami.Theme.disabledTextColor.toString() + "\">";
        return escaped.replace(/[ \t]/g, function(match) {
            return match === "\t"
                ? opening + "»</span>&nbsp;&nbsp;&nbsp;"
                : opening + "·</span>";
        });
    }

    // A run the backend cut out as whitespace: the glyphs come from
    // `escapeCode` like any other text, and only the tint is decided here.
    function whitespaceHtml(segment, kind) {
        const body = escapeCode(String(segment.text));
        const background = whitespaceBackground(segment, kind);
        if (background.length === 0)
            return body;
        return "<span style=\"background-color:" + background + "\">"
            + body + "</span>";
    }

    // The terminator is named rather than drawn: a CRLF and an LF differ by a
    // byte with no width. A pair whose endings disagree says which ending it
    // had in as many letters, whether or not the reveal control is on.
    //
    // Revealed, a plain LF is marked with a pilcrow and anything else is named
    // too — a file with mixed terminators is exactly what a reader turns this
    // on to find, and one mark for every kind would hide it again. The mark is
    // Latin-1 for the reason `escapeCode` gives.
    function lineEndHtml(lineEnd, changed, kind) {
        const ending = String(lineEnd || "");
        if (ending.length === 0)
            return "";
        if (changed !== true && !revealWhitespace)
            return "";
        // A dropped terminator is half of a pair whose other half is named, so
        // it is named too: one side saying LF and the other saying nothing
        // reads as an annotation on the old line rather than as the change.
        // Unchanged, a line with no terminator is simply the last one, and
        // Git's own marker row below it already says so.
        if (ending === "none") {
            return changed === true
                ? "&nbsp;<span style=\"background-color:"
                    + Kirigami.Theme.highlightColor.toString()
                    + ";color:" + Kirigami.Theme.highlightedTextColor.toString()
                    + "\">" + qsTr("NO EOL") + "</span>"
                : "";
        }
        // A changed ending is a label and has to be read, so it is a chip in
        // the selection colours; a revealed one is an annotation and must not
        // compete with the code beside it.
        if (changed === true) {
            return "&nbsp;<span style=\"background-color:"
                + Kirigami.Theme.highlightColor.toString()
                + ";color:" + Kirigami.Theme.highlightedTextColor.toString()
                + "\">" + ending.toUpperCase() + "</span>";
        }
        return "&nbsp;<span style=\"background-color:"
            + mixColor(
                lineBackground(kind),
                Kirigami.Theme.disabledTextColor,
                0.16
            ).toString()
            + ";color:" + Kirigami.Theme.disabledTextColor.toString()
            + "\">" + (ending === "lf" ? "¶" : ending.toUpperCase())
            + "</span>";
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

    // Emphasis wraps the whitespace treatment rather than replacing it, so a
    // changed run that is nothing but spaces carries both at once.
    function highlightedLine(segments, path, kind, lineEnd, lineEndChanged) {
        let result = "<span>";
        for (let index = 0; index < segments.length; ++index) {
            const segment = segments[index];
            let content = String(segment.whitespace || "").length > 0
                ? whitespaceHtml(segment, kind)
                : syntaxHtml(segment.text, path);
            if (segment.changed === true) {
                content = "<span style=\"font-weight:700;text-decoration:underline\">"
                    + content + "</span>";
            }
            result += content;
        }
        return result + lineEndHtml(lineEnd, lineEndChanged, kind) + "</span>";
    }

    // Revealing keeps every column where it was, but it does append a mark to
    // the end of a line that has a terminator — and these labels wrap, so a
    // row already at the wrap boundary can gain a wrapped line and move every
    // pixel below it. The row the reader was looking at is what survives that;
    // a pixel offset is only the fallback for when there is no row to name.
    function setRevealWhitespace(value) {
        if (revealWhitespace === value)
            return;
        const topIndex = reviewLineView.indexAt(1, reviewLineView.contentY + 1);
        const position = reviewLineView.contentY;
        const index = reviewLineView.currentIndex;
        revealWhitespace = value;
        Qt.callLater(function() {
            if (topIndex >= 0) {
                reviewLineView.positionViewAtIndex(topIndex, ListView.Beginning);
            } else {
                const maximum = Math.max(
                    0,
                    reviewLineView.contentHeight - reviewLineView.height
                );
                reviewLineView.contentY = Math.min(position, maximum);
            }
            reviewLineView.currentIndex = index;
        });
    }

    // What a copy takes is the content the backend read, never the glyphs
    // drawn over it: the reveal treatment lives entirely in the markup above
    // and none of it reaches this text.
    //
    // `side` is the half of a side-by-side row the reader pointed at. Without
    // one — a keyboard copy, or a unified row — the answer is the row's own
    // line, so the same row copies the same bytes in either layout.
    function copyTextForRow(row, side) {
        if (!row || row.type !== "line")
            return "";
        if (side === "old" || side === "new") {
            const half = row[side];
            if (half && half.present === true)
                return String(half.copyText || "");
        }
        return String((row.unified || ({})).copyText || "");
    }

    function copyReviewLine(row, side) {
        const value = copyTextForRow(row, side);
        if (value.length === 0)
            return;
        // Through the backend rather than through a TextEdit: QtQuick's own
        // clipboard writer carries the text through a text document, which
        // rewrites a CRLF into an LF — the very byte a diff line may have been
        // copied to inspect.
        backend.copyToClipboard(value);
    }

    function copyCurrentReviewLine() {
        const index = reviewLineView.currentIndex;
        if (index < 0 || index >= reviewRows.length)
            return;
        copyReviewLine(reviewRows[index], "");
    }

    // Right-clicking is the only way the surface learns which half of a
    // side-by-side row was meant, so the menu is opened with that answer
    // rather than guessing it back afterwards.
    function openReviewLineMenu(rowIndex, side) {
        reviewLineView.currentIndex = rowIndex;
        reviewLineView.forceActiveFocus();
        menuCopySide = side;
        reviewLineMenu.popup();
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

    function confirmFileDiscard() {
        pendingDiscardKind = "file";
        pendingDiscardId = String(reviewFile.fileId || "");
        discardPrompt.description = reviewFile.discard || ({});
        discardPrompt.subject = String(reviewFile.path || "");
        discardPrompt.open();
    }

    function confirmHunkDiscard(row) {
        pendingDiscardKind = "hunk";
        pendingDiscardId = String(row.hunkId || "");
        discardPrompt.description = row.discard || ({});
        discardPrompt.subject = String(reviewFile.path || "");
        discardPrompt.open();
    }

    function openCurrentReviewLine() {
        const index = reviewLineView.currentIndex;
        if (index < 0 || index >= reviewRows.length)
            return;
        const row = reviewRows[index];
        if (row.type === "line" && reviewRowDisplayed(row))
            openReviewLine(row.openLine);
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

    /// Which project is under review; see GitPanel.qml's own reload key. The
    /// project map is rewritten after every mutation, and a cross-page hunk
    /// navigation already in flight must survive a commit landing under it.
    readonly property string projectId: project && project.id !== undefined
        ? String(project.id)
        : ""

    onProjectIdChanged: pendingHunkNavigation = 0

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
        enabled: reviewSurface.reviewReady
        sequence: "Alt+W"
        onActivated: reviewSurface.setRevealWhitespace(!reviewSurface.revealWhitespace)
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

    Controls.Menu {
        id: reviewLineMenu

        Controls.MenuItem {
            text: qsTr("Copy line")
            onTriggered: {
                const index = reviewLineView.currentIndex;
                if (index >= 0 && index < reviewSurface.reviewRows.length) {
                    reviewSurface.copyReviewLine(
                        reviewSurface.reviewRows[index],
                        reviewSurface.menuCopySide
                    );
                }
            }
        }
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

                Controls.Label {
                    Layout.fillWidth: true
                    color: Kirigami.Theme.disabledTextColor
                    elide: Text.ElideRight
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                    text: reviewSurface.provenanceHeadline()
                    textFormat: Text.PlainText
                    visible: text.length > 0
                }
            }

            Controls.Label {
                color: Kirigami.Theme.disabledTextColor
                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                text: qsTr("Whitespace")
            }

            // The value is sent as the backend's own spelling rather than an
            // index, so adding a mode is a change in one place and a stale
            // index can never select the wrong comparison.
            Controls.ComboBox {
                id: whitespacePicker

                // The backend owns the value: it survives a refresh, a new
                // target and a failed request. Selecting an entry writes
                // `currentIndex` imperatively and would destroy a declarative
                // binding on it for good, so the state is tracked through a
                // property of our own and pushed into the control instead.
                property string activeMode: String(
                    reviewSurface.reviewState.whitespace || "exact"
                )

                Accessible.name: qsTr("Whitespace handling")
                Controls.ToolTip.text: qsTr("Anything but Exact hides differences that are only whitespace. The diff becomes read-only: discarding a hunk then recomputes the file exactly first, and refuses if that turns up changes this view is hiding.")
                Controls.ToolTip.visible: hovered
                Layout.preferredWidth: Kirigami.Units.gridUnit * 11
                enabled: reviewSurface.reviewState.loading !== true
                model: [
                    { label: qsTr("Exact"), value: "exact" },
                    { label: qsTr("Ignore line endings"), value: "ignore_eol" },
                    { label: qsTr("Ignore indentation"), value: "ignore_change" },
                    { label: qsTr("Ignore all whitespace"), value: "ignore_all" }
                ]
                textRole: "label"
                valueRole: "value"

                onActiveModeChanged: currentIndex = Math.max(0, indexOfValue(activeMode))
                Component.onCompleted: currentIndex = Math.max(0, indexOfValue(activeMode))

                // An accepted request is adopted into the review state before
                // this call returns, so re-reading it either re-selects what was
                // just chosen or snaps back to what the backend refused. Without
                // this the picker would keep displaying a mode the diff on
                // screen was never computed under.
                onActivated: {
                    backend.setReviewWhitespace(
                        project.id,
                        String(currentValue),
                        reviewSurface.reviewState.ignoreBlankLines === true
                    );
                    currentIndex = Math.max(0, indexOfValue(
                        String(reviewSurface.reviewState.whitespace || "exact")
                    ));
                }

                background: FieldSurface {
                    field: whitespacePicker
                }
            }

            // Deliberately not `checkable`: a checkable button toggles its own
            // `checked` on click, which would break the binding below the first
            // time it is pressed and leave the button showing a setting the
            // diff was never computed under. Clicking asks for the opposite of
            // what the backend currently reports and waits to be told.
            Controls.ToolButton {
                Accessible.name: text
                Controls.ToolTip.text: qsTr("Leave lines that are blank on both sides out of the comparison")
                Controls.ToolTip.visible: hovered
                checked: reviewSurface.reviewState.ignoreBlankLines === true
                enabled: reviewSurface.reviewState.loading !== true
                text: qsTr("Blank lines")
                onClicked: backend.setReviewWhitespace(
                    project.id,
                    String(reviewSurface.reviewState.whitespace || "exact"),
                    !checked
                )
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

        // Said once, at the top, rather than on every hunk: while this is
        // showing, what is on screen is not what is on disk.
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            text: qsTr("This diff hides whitespace-only differences, so it does not describe the file byte for byte. Discarding a hunk recomputes it exactly first.")
            type: Kirigami.MessageType.Information
            visible: reviewSurface.reviewReady
                && reviewSurface.reviewState.appliable === false
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

                            Accessible.name: qsTr("%1, %2 change, %3")
                                .arg(modelData.path)
                                .arg(modelData.change)
                                .arg(reviewSurface.provenanceDetail(modelData))
                            Controls.ToolTip.text: modelData.path
                                + "\n" + reviewSurface.provenanceTooltip(modelData)
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

                                // The one mark a reader can scan a long file
                                // list by without reading anything: files that
                                // came from the same hands carry the same bar,
                                // and an unattributed file carries none.
                                Rectangle {
                                    Layout.fillHeight: true
                                    Layout.preferredWidth: Math.round(
                                        Kirigami.Units.smallSpacing / 2
                                    ) + 2
                                    color: reviewSurface.provenanceTint(
                                        reviewFileDelegate.modelData.provenanceGroup
                                    )
                                    radius: width / 2
                                }

                                ColumnLayout {
                                    Layout.fillWidth: true
                                    spacing: 0

                                    Controls.Label {
                                        Layout.fillWidth: true
                                        elide: Text.ElideMiddle
                                        font.family: "monospace"
                                        text: reviewFileDelegate.modelData.path
                                        textFormat: Text.PlainText
                                    }

                                    // Producer names come out of commit
                                    // objects, which is repository content:
                                    // plain text, never interpreted as markup.
                                    Controls.Label {
                                        Layout.fillWidth: true
                                        color: Kirigami.Theme.disabledTextColor
                                        elide: Text.ElideRight
                                        font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                        text: reviewSurface.provenanceLabel(
                                            reviewFileDelegate.modelData
                                        )
                                        textFormat: Text.PlainText
                                        visible: reviewSurface.reviewProvenance.resolved === true
                                    }
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

                        // Deliberately not `checkable`, for the reason the
                        // blank-lines button above gives: a checkable button
                        // toggles its own `checked` on click and destroys the
                        // binding that reports the setting actually in force.
                        Controls.ToolButton {
                            Accessible.name: text
                            Controls.ToolTip.text: qsTr("Show spaces, tabs and line endings (Alt+W)")
                            Controls.ToolTip.visible: hovered
                            checked: reviewSurface.revealWhitespace
                            display: Controls.AbstractButton.IconOnly
                            icon.name: "view-visible"
                            text: qsTr("Reveal whitespace")
                            onClicked: reviewSurface.setRevealWhitespace(!checked)
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

                        // The same bar the row carried, so opening a file does
                        // not lose the grouping the list just showed.
                        Rectangle {
                            Layout.alignment: Qt.AlignVCenter
                            Layout.preferredHeight: Kirigami.Units.gridUnit
                            Layout.preferredWidth: Math.round(
                                Kirigami.Units.smallSpacing / 2
                            ) + 2
                            color: reviewSurface.provenanceTint(
                                reviewSurface.reviewFile.provenanceGroup
                            )
                            radius: width / 2
                            visible: reviewSurface.reviewProvenance.resolved === true
                        }

                        Controls.Label {
                            Layout.fillWidth: true
                            elide: Text.ElideMiddle
                            font.bold: true
                            font.family: "monospace"
                            text: reviewSurface.reviewFile.path || ""
                            textFormat: Text.PlainText
                        }

                        Controls.Label {
                            Accessible.name: text
                            Controls.ToolTip.text: reviewSurface.provenanceTooltip(
                                reviewSurface.reviewFile
                            )
                            Controls.ToolTip.visible: provenanceHover.hovered
                            color: Kirigami.Theme.disabledTextColor
                            elide: Text.ElideRight
                            font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            Layout.maximumWidth: Kirigami.Units.gridUnit * 12
                            text: reviewSurface.provenanceLabel(reviewSurface.reviewFile)
                            textFormat: Text.PlainText
                            visible: reviewSurface.reviewProvenance.resolved === true

                            HoverHandler { id: provenanceHover }
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

                        Controls.ToolButton {
                            Accessible.name: String((reviewSurface.reviewFile.discard || ({})).operation || "")
                                === "delete_untracked"
                                ? qsTr("Delete untracked file")
                                : qsTr("Discard file changes")
                            Controls.ToolTip.text: Accessible.name
                            Controls.ToolTip.visible: hovered
                            display: Controls.AbstractButton.IconOnly
                            enabled: !reviewSurface.repositoryMutationRunning()
                                && !reviewSurface.reviewReadRunning()
                            icon.name: "edit-delete"
                            text: Accessible.name
                            visible: String((reviewSurface.reviewFile.discard || ({})).operation || "").length > 0
                            onClicked: reviewSurface.confirmFileDiscard()
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
                        Accessible.description: qsTr("Use the arrow keys to select a row and Enter to open a changed line in the editor")
                        Accessible.role: Accessible.List
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

                        Keys.onEnterPressed: reviewSurface.openCurrentReviewLine()
                        Keys.onReturnPressed: reviewSurface.openCurrentReviewLine()
                        // Scoped to the list rather than declared as a
                        // Shortcut: copy belongs to whatever the reader is
                        // actually in, and the surface has no claim on it
                        // while the focus is somewhere else.
                        Keys.onPressed: function(event) {
                            if (event.matches(StandardKey.Copy)) {
                                reviewSurface.copyCurrentReviewLine();
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
                        enabled: !reviewSurface.repositoryMutationRunning()
                            && !reviewSurface.reviewReadRunning()
                        icon.name: "edit-delete"
                        text: qsTr("Discard hunk…")
                        visible: String((reviewHunk.row.discard || ({})).operation || "").length > 0
                        onClicked: reviewSurface.confirmHunkDiscard(reviewHunk.row)
                    }
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

    DiscardPrompt {
        id: discardPrompt

        onConfirmed: function(operation) {
            if (reviewSurface.pendingDiscardKind === "hunk") {
                reviewSurface.backend.discardReviewHunk(
                    reviewSurface.project.id,
                    reviewSurface.pendingDiscardId
                );
            } else if (reviewSurface.pendingDiscardKind === "file") {
                reviewSurface.backend.discardReviewFile(
                    reviewSurface.project.id,
                    reviewSurface.pendingDiscardId,
                    operation
                );
            }
            reviewSurface.pendingDiscardKind = "";
            reviewSurface.pendingDiscardId = "";
        }
        onRejected: {
            reviewSurface.pendingDiscardKind = "";
            reviewSurface.pendingDiscardId = "";
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
            readonly property bool current: reviewLineView.currentIndex === rowIndex
            Accessible.name: qsTr("Open diff line %1 in editor").arg(row.openLine || 1)
            Accessible.role: Accessible.Button
            Accessible.selectable: true
            Accessible.selected: current
            Controls.ToolTip.text: qsTr("Open line %1 in editor").arg(row.openLine || 1)
            Controls.ToolTip.visible: reviewLineHover.hovered
            implicitHeight: hidden
                ? 0
                : reviewSurface.splitLayout
                    ? splitLine.implicitHeight
                    : unifiedLine.implicitHeight
            visible: !hidden
            width: ListView.view ? ListView.view.width : implicitWidth

            HoverHandler {
                id: reviewLineHover
            }

            TapHandler {
                acceptedButtons: Qt.LeftButton
                onTapped: {
                    reviewLineView.currentIndex = reviewLineDelegate.rowIndex;
                    reviewLineView.forceActiveFocus();
                    reviewSurface.openReviewLine(reviewLineDelegate.row.openLine);
                }
            }

            // Off in side-by-side, where each half carries its own. A tap
            // handler takes a passive grab and does not consume the press, so
            // leaving this one on would let it run *after* the half's and
            // overwrite the side the reader pointed at with no side at all.
            TapHandler {
                acceptedButtons: Qt.RightButton
                enabled: !reviewSurface.splitLayout
                onTapped: reviewSurface.openReviewLineMenu(
                    reviewLineDelegate.rowIndex,
                    ""
                )
            }

            Rectangle {
                id: unifiedLine

                anchors.left: parent.left
                anchors.right: parent.right
                border.color: reviewLineDelegate.current
                    ? Kirigami.Theme.highlightColor
                    : "transparent"
                border.width: reviewLineDelegate.current ? 2 : 0
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
                            reviewSurface.reviewFile.path || "",
                            reviewLineDelegate.unified.kind,
                            reviewLineDelegate.unified.lineEnd,
                            reviewLineDelegate.row.lineEndChanged === true
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

                        required property int index
                        required property var modelData

                        readonly property string side: index === 0 ? "old" : "new"

                        Layout.fillWidth: true
                        border.color: reviewLineDelegate.current
                            ? Kirigami.Theme.highlightColor
                            : "transparent"
                        border.width: reviewLineDelegate.current ? 2 : 0
                        color: modelData.present === true
                            ? reviewSurface.lineColor(modelData.kind)
                            : reviewSurface.tint(Kirigami.Theme.disabledTextColor, 0.04)
                        implicitHeight: splitSideLayout.implicitHeight
                            + Kirigami.Units.smallSpacing

                        // A replacement shows its deletion and its addition on
                        // the same row, so which one a copy means is only ever
                        // answered by which one was pointed at. A half with
                        // nothing in it still answers, with the row's own
                        // line, rather than leaving a dead area.
                        TapHandler {
                            acceptedButtons: Qt.RightButton
                            onTapped: reviewSurface.openReviewLineMenu(
                                reviewLineDelegate.rowIndex,
                                splitSide.modelData.present === true
                                    ? splitSide.side
                                    : ""
                            )
                        }

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
                                        reviewSurface.reviewFile.path || "",
                                        splitSide.modelData.kind,
                                        splitSide.modelData.lineEnd,
                                        reviewLineDelegate.row.lineEndChanged === true
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
