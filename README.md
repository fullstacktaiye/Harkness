# Harkness

Harkness is an early native AI-harness scaffold. Its Rust core maintains a local
project catalog and can safely clone GitHub repositories through the system Git
executable, preserving the user's existing SSH and HTTPS credential setup. Its
core also provides cancellable branch listing and locked, typed create,
checkout, rename, delete, and upstream-management operations. The same locked
service stages explicit paths, stages a whole working tree, unstages safely
before or after the first commit, and creates guarded commits and amendments.
The KDE Kirigami application opens on a project launcher — Recents, local
folder import, and validated GitHub cloning with progress and cancellation —
and opens each project into a shell showing its Git identity next to a lazy,
read-only file tree that never descends into `.git` or through directory
symlinks. A worker-backed branch picker switches existing local branches
without blocking the UI and identifies branches held by another worktree before
selection.
Managed clones are deleted only after a confirmation naming the checkout;
local projects are simply forgotten, leaving their files untouched. Git
repositories can create, list, reconcile, and remove first-class linked
workspaces on new branches, existing branches, or detached commits.

Harkness requires Git 2.36 or newer. Worktree discovery uses the unambiguous
NUL-delimited `git worktree list --porcelain -z` format introduced in that
release.

## Fedora development setup

On Fedora 44, install Rust and the Qt 6 / KDE Frameworks 6 development tools:

```sh
sudo dnf install cargo rust gcc-c++ cmake extra-cmake-modules \
    qt6-qtbase-devel qt6-qtdeclarative-devel qt6-qttools-devel \
    kf6-kirigami-devel qqc2-desktop-style
```

The GUI build needs Qt's `qmake` on `PATH`. If more than one Qt installation is
present, set `QMAKE` to the Qt 6 executable before running Cargo.

## Develop with Cargo

```sh
cargo run -p harkness-cli
cargo run -p harkness-gui
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
```

The CLI exposes the project catalog with a versioned machine-readable contract
and stable exit codes suitable for agents. Commands that act on one project
accept `--project <SELECTOR>` after the command name. A selector may be a full
ID, an ID prefix of at least eight characters, an absolute or dot-relative
path, or a display name. Bare words are always names, so their meaning does not
change with the current directory. When `--project` is omitted, Harkness walks
upward from the current directory and selects the deepest catalogued root, so
commands run naturally from a repository or linked worktree.

```sh
harkness --json project list
harkness --json project list --no-status
harkness project show --project <selector>
harkness project resolve <selector>
harkness project import <path>
harkness project clone <github-remote>
harkness project reconcile
harkness project forget --project <selector>
harkness project delete --project <selector> --yes

harkness --json git status [--paths] [--project <selector>]
harkness --json git diff [--staged | --unstaged] [--context-lines <lines>] [--max-file-size <bytes>] [<path>...] [--project <selector>]
harkness --json git fetch [--remote <name>] [--prune] [--project <selector>]
harkness --json git pull [--ff-only | --rebase | --merge] [--project <selector>]
harkness --json git push [--set-upstream] [--allow-default-branch] [--force-with-lease] [--project <selector>]
harkness --json git branch list [--all] [--project <selector>]
harkness --json git branch create <name> [--from <ref>] [--checkout] [--project <selector>]
harkness --json git branch checkout <name> [--project <selector>]
harkness --json git branch delete <name> [--force] [--project <selector>]
harkness --json git stage (<path>... | --all | --hunk <selection-flags>) [--project <selector>]
harkness --json git unstage (<path>... | --hunk <selection-flags>) [--project <selector>]
harkness --json git commit --message <message> [--amend] [--allow-empty] [--project <selector>]

harkness --json worktree list [--project <parent-selector>]
harkness --json worktree add --branch <name> [--from <ref>] [--project <parent-selector>]
harkness --json worktree add --branch <name> --existing [--project <parent-selector>]
harkness --json worktree add --branch <revision> --detach [--project <parent-selector>]
harkness --json worktree remove [--force] [--project <worktree-selector>]
harkness --json worktree move <destination> [--project <worktree-selector>]
harkness --json worktree lock --reason <text> [--replace] [--project <worktree-selector>]
harkness --json worktree unlock [--project <worktree-selector>]
harkness --json worktree prune [--project <parent-selector>]

harkness --json contract
```

Operational `--json` commands write exactly one success or error envelope to
standard output. Every envelope starts with `"v": 1` and has a `type` of
`success`, `error`, or `progress`; success and error results also carry `ok`.
Clone, fetch, pull, and push progress remains on standard error as one versioned
JSON object per line, keeping standard output parseable. Help and version are
deliberately plain text even when `--json` is present. `harkness --json contract`
reports the current envelope version, exit codes, streams, and complete
error-kind namespaces.

`git diff` returns one structured `files` array, staged records first and
unstaged records second; `--staged` or `--unstaged` narrows it to one side of
the index. Each file carries its blob IDs, paths, modes, sizes, and hunks.
Every hunk line names its `content_encoding`: valid UTF-8 is emitted directly,
while arbitrary bytes use Base64, so consumers can reconstruct the exact
content. Binary and oversized files remain summary records rather than raw or
truncated patches.

To stage or unstage one hunk, pass `--hunk` with the selected file's
`--old-path` and/or `--new-path`, `--old-blob-id`, `--new-blob-id`, and
`--context-lines`, plus the hunk's `--old-start`, `--old-lines`, `--new-start`,
and `--new-lines`. Harkness recomputes the diff under the repository lock; if
any identity or coordinate is stale it exits 3 without changing the index.
Whole-path staging and unstaging keep their existing syntax.

Project JSON uses an explicit CLI projection rather than the catalog's storage
serializer. `last_opened` is RFC 3339, source-specific optional fields are
always present with `null` when inapplicable, and `git` has a fixed documented
shape. Paths use a lossy wire conversion when the platform path is not UTF-8
and mark the containing record with `path_is_lossy: true`. A `--no-status`
listing avoids filesystem and Git probes and reports
`status_checked: false`, `available: null`, and `git: null` rather than
pretending the project is missing. `HARKNESS_DATA_DIR` and `--data-dir <path>`
select an isolated catalog, with the explicit flag taking precedence. Run
`harkness --help` for the exit-code contract and complete command help.

Removing a worktree preserves its branch, allowing `--existing` to reuse it.
Ordinary removal refuses uncommitted files; discarding them requires the
explicit `--force` override. Worktree pruning removes only missing
Harkness-owned rows and their exact Git administrative records. It never
performs a repository-wide prune or adopts or removes external worktrees.

A worktree lock records a mandatory reason and protects the checkout from
removal, relocation, and pruning; `--force` does not override it, so clearing
protection always takes an explicit `worktree unlock`. Git trims the stored
reason and `worktree list` reports it as `lock_reason`, which is `null` when a
lock records no reason at all. Locking an already-locked worktree is refused
rather than silently replaced; `--replace` supersedes an existing reason
without leaving the checkout unprotected in between. Lock changes apply only to
Harkness-created checkouts, so a catalog row aimed at a foreign worktree is
refused instead of having its protection altered.

Ctrl-C cooperatively cancels a running clone or Git operation, cleans partial
storage, and exits 130. A non-cooperative kill cannot run cleanup, so `project
reconcile` safely removes UUID-named managed directories with no catalog row.
Per-import locks make it skip live clones, and unrelated files or directories
under managed storage are left untouched.

The GUI opens a Kirigami window on the project launcher backed by the Rust
`HarknessBackend` and `FileTreeModel` QML objects. Its project shell exposes the
same creation modes, live linked-worktree inventory, selective reconciliation,
and a second confirmation before dirty files can be discarded.

### Worktree UI

![Recents showing a dirty managed worktree](docs/screenshots/worktree-recents.png)

![Project shell showing the linked-workspace creation form](docs/screenshots/worktree-creation.png)

## Install locally for Plasma

The thin CMake wrapper builds the locked Cargo workspace and installs both
executables and the desktop file. A user-local installation can be made with:

```sh
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$HOME/.local"
cmake --build build
cmake --install build
```

After Plasma refreshes its application database, Harkness appears in the
Development category. Run `harkness-gui` directly to launch it without the menu.
