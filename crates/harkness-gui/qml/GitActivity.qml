import QtQuick

/// Read-only projection of the backend's Git state and job list for one
/// project.
///
/// The repository toolbar in the shell header and the source-control view both
/// have to answer the same questions — "is this state about the project on
/// screen?", "is a mutation running?", "what would the sync button do?" — and
/// they must answer them identically, or the toolbar would offer an action the
/// panel has already disabled. The answers live here once. Every member is
/// derived from `backend`, so an instance per consumer costs nothing and holds
/// no state of its own.
QtObject {
    id: activity

    required property var backend
    required property var project

    /// Scope shared by every linked worktree of one repository, which is what
    /// the backend keys repository jobs by.
    readonly property string repositoryLockScope: String(
        project.lockScope || project.parentId || project.id
    )

    /// True once `backend.git` describes *this* project rather than the one
    /// opened before it.
    readonly property bool stateReady: backend.git
        && backend.git.projectId !== undefined
        && String(backend.git.projectId) === String(project.id)
    readonly property var gitState: stateReady ? backend.git : ({})
    readonly property var entries: stateReady && gitState.entries !== undefined
        ? gitState.entries
        : []

    /// The branch Git reports as checked out, falling back to the catalog's
    /// record while the first status read is still in flight.
    readonly property string currentBranch: String(gitState.branch || project.branch || "")

    /// The running job of this kind, or null. Without `targetProjectId` a job
    /// anywhere in this repository's lock scope counts, because a sibling
    /// worktree's mutation blocks this one just as its own would.
    function job(kind, targetProjectId) {
        const target = targetProjectId === undefined ? project.id : targetProjectId;
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            const matchesTarget = targetProjectId === undefined
                ? String(candidate.projectId) === String(target)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope
                : String(candidate.projectId) === String(target);
            if (matchesTarget && candidate.kind === kind)
                return candidate;
        }
        return null;
    }

    function networkJobs() {
        const running = [];
        for (let index = 0; index < backend.jobs.length; ++index) {
            const candidate = backend.jobs[index];
            if ((String(candidate.projectId) === String(project.id)
                    || String(candidate.lockScope || candidate.projectId)
                        === repositoryLockScope)
                    && ["fetch", "pull", "push"].indexOf(candidate.kind) !== -1)
                running.push(candidate);
        }
        return running;
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

    /// The mutations that act on one path or hunk rather than on the repository
    /// as a whole.
    ///
    /// Kept apart from `repositoryMutationRunning` because the two answer
    /// different questions. A staging control asks "may I stage?", and disabling
    /// the whole file list because one discard is in flight would be wrong. But
    /// the backend's own `jobs_conflict` refuses these alongside anything else
    /// that mutates, so a caller asking "would a mutation be accepted right
    /// now?" has to count them, and gets both.
    function pathMutationRunning() {
        return job("stage") !== null
            || job("unstage") !== null
            || job("stage_hunk") !== null
            || job("unstage_hunk") !== null
            || job("discard_path") !== null
            || job("discard_hunk") !== null;
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

    /// What a single sync control should do: pulling takes priority when
    /// behind (a push would be rejected anyway), otherwise push if there is
    /// anything to push, otherwise just fetch to refresh remote-tracking state.
    function syncAction() {
        const behind = stateReady ? Number(gitState.behind || 0) : 0;
        const ahead = stateReady ? Number(gitState.ahead || 0) : 0;
        if (behind > 0)
            return "pull";
        if (ahead > 0)
            return "push";
        return "fetch";
    }
}
