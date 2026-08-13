import QtQuick

/// Which changed files the next commit will record.
///
/// The Changes list draws the checkboxes and the commit footer acts on them, so
/// the answer has to outlive both and belong to neither. It lives here once,
/// the way the shared Git projection does; see GitActivity.qml.
///
/// Exclusions are held rather than inclusions, so a change that appears while a
/// commit message is being written is included by default. That is the same
/// answer a fresh status gives, and it means a file saved a moment before
/// pressing Commit is not silently left out.
QtObject {
    id: selection

    required property var project

    /// Paths the user has unchecked, as a set keyed by path.
    ///
    /// Path, never `pathId`: a token survives a status refresh only while the
    /// row behind it is unchanged, and is re-minted the moment the file is
    /// edited. Keying on one would silently re-include a file the user had
    /// unchecked and then saved again.
    property var excluded: ({})

    /// True when every changed file is included, which is the initial state.
    function all(entries) {
        return countIncluded(entries) === entries.length;
    }

    function none(entries) {
        return countIncluded(entries) === 0;
    }

    function included(path) {
        return excluded[String(path)] !== true;
    }

    function countIncluded(entries) {
        let count = 0;
        for (let index = 0; index < entries.length; ++index) {
            if (included(entries[index].path))
                ++count;
        }
        return count;
    }

    /// The backend path tokens for everything currently included, newline
    /// delimited the way `HarknessBackend.commit` expects them.
    function includedPathIds(entries) {
        const ids = [];
        for (let index = 0; index < entries.length; ++index) {
            const entry = entries[index];
            if (included(entry.path))
                ids.push(String(entry.pathId));
        }
        return ids.join("\n");
    }

    // `excluded` is replaced rather than mutated: QML tracks the property, not
    // the object behind it, so an in-place edit would leave every checkbox and
    // every count bound to it showing the previous answer.
    function setIncluded(path, include) {
        const key = String(path);
        if (included(key) === include)
            return;
        const next = {};
        for (const existing in excluded) {
            if (existing !== key)
                next[existing] = true;
        }
        if (!include)
            next[key] = true;
        excluded = next;
    }

    function toggle(path) {
        setIncluded(path, !included(path));
    }

    function setAll(entries, include) {
        const next = {};
        if (!include) {
            for (let index = 0; index < entries.length; ++index)
                next[String(entries[index].path)] = true;
        }
        excluded = next;
    }

    function clear() {
        excluded = ({});
    }

    /// Drops exclusions for paths that are no longer changed.
    ///
    /// Without this a file that was unchecked, committed by other means, and
    /// then changed again would come back still unchecked, with nothing on
    /// screen explaining why it was being left out.
    function prune(entries) {
        const live = {};
        for (let index = 0; index < entries.length; ++index)
            live[String(entries[index].path)] = true;
        const next = {};
        let changed = false;
        for (const path in excluded) {
            if (live[path] === true)
                next[path] = true;
            else
                changed = true;
        }
        if (changed)
            excluded = next;
    }
}
