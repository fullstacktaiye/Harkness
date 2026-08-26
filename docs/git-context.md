# Git-aware context retrieval

Git context is the change-dynamics half of the context engine: diffs, recent
history, literal file history, changed paths, conflicts, worktree state, and
explicit line blame. The implementation lives in
`crates/harkness-context/src/gitctx/`; it adapts `harkness-git` and contains no
Git implementation of its own.

## Building a retrieval session

A caller captures one `WorkspaceSnapshot` and passes it to
`ContextEngine::git_context_under`. The engine builds the eligible-file
inventory under its effective policy and returns a `GitContextService` that owns
that capture and inventory. This pairing has two consequences:

- every returned provenance record carries the same `SnapshotId`; and
- a path excluded by the inventory never contributes diff bytes or a recorded
  name. Responses retain an aggregate `withheld_files` count so exclusion does
  not turn a partial inspection into a claim that nothing else changed.

Every retrieval verifies the snapshot after reading Git. A moved HEAD, index,
tracked file, or untracked set returns `stale_snapshot`; an unverifiable
workspace is the same refusal, not a soft success. This post-read guard is what
prevents a response assembled across two workspace moments.

## Diff comparisons and budgets

The default context budget is 1 MiB of hunk bytes, 200 content-bearing files,
and 50 commits. These are tighter than the review UI defaults because context is
destined for a bounded model request.

`workspace_diff` asks `GitService::diff_snapshot` for staged and unstaged
targets together. Both sides therefore use one open repository and index.
`staged_diff` and `working_diff` select from that coherent pair.

`branch_diff` resolves the merge-base between the caller's base expression and
the captured HEAD once, then compares the two resolved object IDs. A branch
that moves during retrieval cannot change the comparison silently: the pinned
IDs remain in `DiffComparison`, and the snapshot guard refuses the moved
workspace.

Every eligible file keeps its old and new blob IDs even when its content is
binary, unmerged, too large, or beyond a response budget. `GitDiffOmission`
names the reason hunk bytes are absent. The content SHA-256 in `Provenance`
describes the exact hunk bytes returned, while `DiffAnchor` retains Git's
immutable identity for later `FileContextRequest::blob` reads.

## History and status

Recent history is cursor-paged through `GitService::log`. File history is a
bounded literal-path scan: it does not follow renames and reports
`CommitBudgetExhausted` when the scan cannot prove it reached the beginning of
history. Commit messages and identities remain byte-preserving and are marked
as untrusted repository content before they become context.

Changed-file and conflict responses are projected from one detailed status.
Deleted names and rename sources are evaluated through the same inventory
policy because they have no present filesystem row. A rename retains both names
only when both pass; one that crosses the exclusion boundary contributes only
to the aggregate withheld count. Conflict state succeeds with unresolved paths
and the pending Git operation; an unmerged path does not fail the surrounding
diff.

Worktree state combines the same detailed status with `GitService::worktrees`.
All reads are local. Nothing in this module can fetch, pull, push, or mutate a
repository.

## Explicit blame

Blame is the only operation added to `harkness-git` for this adapter.
`GitService::blame_file` runs `git blame --porcelain --root -L` through the
hermetic local-read runner, so the scrubbed environment, 30-second timeout,
cancellation polling, and process-group teardown are unchanged. Requests are
limited to 10,000 lines and 16 MiB of retained porcelain output.

The context adapter adds a second gate: `BlameRequest::explicit` must be true
and a line range must be present. No diff, history, status, ranking, or pack
operation invokes blame as a side effect. Dirty lines carry the typed
`Uncommitted` marker rather than a fabricated commit ID.

## What proves this

| Contract | Package | Test |
| --- | --- | --- |
| staged and unstaged diffs share one index snapshot; staged blobs remain readable | `harkness-context` | `gitctx::tests::staged_and_working_diffs_share_one_index_snapshot_and_blob_ids_remain_readable` |
| branch comparison pins its merge-base | `harkness-context` | `gitctx::tests::branch_diff_pins_the_merge_base_and_excludes_base_only_commits` |
| a moved capture is refused | `harkness-context` | `gitctx::tests::moving_head_after_capture_is_a_typed_stale_snapshot` |
| denials exclude secret paths while renames keep sources | `harkness-context` | `gitctx::tests::renames_keep_their_source_and_secret_paths_never_enter_diff_items` |
| a rename crossing the inventory boundary leaks neither name | `harkness-context` | `gitctx::tests::a_rename_crossing_the_inventory_boundary_withholds_both_names` |
| explicit blame marks dirty lines and handles the maximum profile | `harkness-git` | `blame::tests::service_marks_worktree_only_lines_uncommitted`, `blame::tests::a_five_thousand_line_range_stays_inside_the_published_bound` |
