# The eligible-file inventory

Before Harkness indexes, searches, or shows a model anything, it decides which
files in a worktree are usable as context at all. That decision is one walk with
one set of rules, and its product is a `FileInventory`: a bounded, sorted list of
paths, each carrying exactly one `FileClass` and one answer to "may this file's
content be indexed".

Everything downstream reads that inventory rather than the filesystem —
chunking, the index cache, lexical search, the repository map, and instruction
discovery — which is what stops two retrieval features disagreeing about whether
a file exists. It also means a path excluded here is excluded everywhere,
without every later feature having to remember why.

The code is `crates/harkness-context/src/inventory.rs` (the walk and the
exclusion hierarchy) and `crates/harkness-context/src/classify.rs` (the class
each recorded path gets).

## The exclusion hierarchy

Four layers are consulted for every path, strictly in this order, and the first
layer with an opinion decides.

| # | Layer | Where it lives | May |
| --- | --- | --- | --- |
| 1 | Built-in denials | compiled in | exclude only, and no later layer may undo it |
| 2 | Global user ignore | `<data_dir>/context-ignore` | exclude, or explicitly re-include |
| 3 | Repository ignore | `<worktree>/.harkness/context-ignore` | exclude only |
| 4 | The `.gitignore` chain | every `.gitignore` in the worktree, deepest first | exclude, or explicitly re-include |

Every layer speaks gitignore syntax. A layer answers one of three things —
exclude, explicitly re-include, or nothing — and an explicit re-inclusion stops
the descent, which is what makes the order meaningful in both directions: a
user's own `!keep.log` outranks a repository's `.gitignore`, and *nothing*
outranks layer 1.

Layer 4 is the `.gitignore` files inside the worktree and nothing else. Git's
`.git/info/exclude` and its machine-level `core.excludesFile` are deliberately
not read: the first lives in the repository's common directory, which this crate
cannot resolve because it is addressed purely by path, and the second would make
one repository's context differ between two machines for reasons no one wrote
down. Layer 2 is where a person's own preferences belong.

Repository content can therefore narrow what Harkness reads and can never widen
it. That is ADR-0006's tightening-only rule applied to the walk, and it is the
same rule `.harkness/policy.json` follows for policy.

Two consequences are worth stating plainly:

- **A repository's negations are discarded, not merely outranked.** A `!` line in
  `.harkness/context-ignore` never applies, even against a rule from the same
  file, and each discarded line is reported as a diagnostic naming the file, the
  line number, and the pattern. A repository learns that its re-inclusion had no
  effect instead of assuming it worked.
- **A pruned directory is never reconsidered.** A directory an earlier layer
  excluded is not descended into, so a re-inclusion naming a path *inside* it has
  nothing to act on. This is Git's own behavior for `.gitignore`, and it is the
  one place the table above does not read literally.

### Built-in denials

Credential-bearing names, matched against every path *and every parent
directory* of the worktree:

```
.env                    .git-credentials        *.keystore
.env.*                  .netrc                  *.jks
*.pem                   **/.aws/credentials     .npmrc
*.key                   **/.config/gcloud/      .pypirc
*.p12                   **/.config/gcloud/**
*.pfx                   **/.kube/config
id_rsa*
id_ed25519*
id_ecdsa*
```

A path one of these matches is counted in `denied_count` and recorded
**nowhere**: not as an entry, not in a diagnostic, not in a count keyed by path,
and not in anything derived from the inventory. Its content is never opened. A
denied *directory* counts once and its contents are never visited, so
`denied_count` is a count of rules applied rather than of files that exist.

The list is deliberately blunt and occasionally over-broad — `*.key` catches a
game asset, `id_rsa*` catches a public key — because the cost of a false positive
is one unindexed file and the cost of a false negative is a credential in a
prompt. Changing it is a `CLASSIFY_VERSION` bump.

### Tightening a repository

Add `.harkness/context-ignore` to the worktree and write gitignore patterns in
it:

```gitignore
# Generated protobuf bindings: large, and rebuilt from the .proto files.
generated/**
# Vendored SDK we never edit.
sdk/thirdparty/**
```

Files matched there are excluded from every context feature for everybody who
opens the repository. Negations are ignored, and a malformed pattern is reported
and skipped while the rest of the file still applies — a rule file is never
silently widened by one bad line.

The global `<data_dir>/context-ignore` is the same syntax, anchored at the
worktree root, and it is the one Harkness-owned layer whose negations are
honored, because it is the user's own file. (The `.gitignore` chain keeps Git's
semantics within itself, negations included; what it cannot do is re-include
something an earlier layer excluded.)

A configured ignore file that does not exist contributes no rules; one that
exists and cannot be read, is larger than 1 MiB, or is not valid UTF-8 fails the
walk with `ignore_rule_invalid`, because a rule meant to exclude something must
not be skipped quietly. An oversized `.gitignore` is the one exception: it is
skipped and reported rather than fatal, because layer 4 can only exclude and by
the time it is read every layer that can deny has already spoken.

## What a walk records

An entry per recorded path, sorted by exact path bytes:

| Field | Meaning |
| --- | --- |
| `path` | repository-relative, byte-exact, `/`-separated |
| `byte_size` | size as the filesystem reported it, without following a link |
| `mtime_ns` | modification time in nanoseconds since the Unix epoch, when the platform reports one |
| `class` | exactly one `FileClass` |
| `symlink` | the path is a symbolic link, recorded and never followed |
| `boundary` | the path is a directory the walk refused to descend into |
| `unreadable` | metadata or opening bytes could not be read |

`eligible()` is derived rather than stored: a class that permits indexing, and
not a symlink, a boundary, or an unreadable path.

A file excluded by a *rule* is counted rather than listed — the rules already
name it. A file excluded by what it *is* — binary, oversized, secret-sensitive,
undecodable — **is** listed, with that class and `eligible() == false`, because
"why is this file not in my context" is a question users ask about exactly those.

### Traversal rules

- **Symlinks are recorded and never followed.** A link to a directory produces
  one entry and no entries for the target's contents, whether the target is
  inside the worktree or outside it. The walk never leaves the worktree root, and
  the root itself comes from a captured snapshot rather than from a caller's
  string.
- **A nested repository is a boundary.** A directory holding its own `.git` is
  recorded with `boundary` set and never descended into, so no cross-repository
  content enters. It is labelled `submodule` when the worktree's `.gitmodules`
  declares its path and `nested_repository` otherwise; that read is advisory and
  decides only the spelling, never whether the walk descends.
- **The repository's own `.git` is not content** and is skipped rather than
  counted — nothing excluded it, it is simply not part of a worktree.
- **Hidden files are visited**, classified, and recorded like any other.
- **Sparse checkouts** inventory what is materialized. The walk consults no
  index, so a path Git knows about and the working tree does not hold is simply
  absent.
- **Monorepos get no special handling**: no package detection, no per-package
  rules, just a bounded walk.
- **A path that is not a regular file, a directory, or a symlink** — a FIFO, a
  socket, a device — is skipped and never opened, because `open(2)` on one can
  block forever.
- **Non-UTF-8 paths** are stored byte-exactly and rendered lossily for display,
  the same way `harkness-git` handles path bytes.
- **Two paths differing only by case** are both kept — indexing is keyed by exact
  bytes — and a `case_collision` diagnostic names the pair.

## Classification

Exactly one class per recorded path, by the first rule that matches:

| # | Class | Decided by | Eligible |
| --- | --- | --- | --- |
| 1 | `secret_sensitive` | a name starting `secret`/`credential`, containing `token`, `password`, `passwd`, `apikey`, `api_key`, `api-key`, or ending `.dump` — none of which apply to a file with a language extension | no |
| 2 | `binary` | a NUL byte in the first 8 KiB | no |
| 3 | `oversized` | larger than 1 MiB | no |
| 4 | `unsupported_encoding` | the sniff window decodes as neither UTF-8 nor UTF-16 | no |
| 5 | `lockfile` | `Cargo.lock`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lockb`, `poetry.lock`, `Pipfile.lock`, `uv.lock`, `go.sum`, `Gemfile.lock`, `composer.lock`, `flake.lock`, `gradle.lockfile` | yes |
| 6 | `vendor` | a `vendor`, `node_modules`, `third_party`, `thirdparty` or `.venv` directory segment | yes |
| 7 | `generated` | a `target`, `build`, `dist` or `out` segment; a `.min.js`/`.min.css`/`.min.mjs` name; an `@generated` marker in the first 1 KiB; or `.js`/`.css` whose sniffed average line runs past 512 bytes | yes |
| 8 | `instruction` | `AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`, or `.harkness/**.md` | yes |
| 9 | `build_manifest` | `Cargo.toml`, `package.json`, `CMakeLists.txt`, `pyproject.toml`, `setup.py`, `go.mod`, `Makefile`, `GNUmakefile`, `Gemfile`, `pom.xml`, `build.gradle`, `build.gradle.kts`, `meson.build` | yes |
| 10 | `test_code` | a `tests`, `test` or `__tests__` segment, or a `*_test.*` / `*.test.*` name | yes |
| 11 | `configuration` | `.toml`, `.yaml`, `.yml`, `.json`, `.ini`, `.cfg`, `.conf`, `.properties`, or a dotfile | yes |
| 12 | `documentation` | `.md`, `.markdown`, `.rst`, `.txt`, `.adoc`, `.org`, or a `docs`/`doc` segment | yes |
| 13 | `source` | a known language extension | yes |
| 14 | `unknown_text` | nothing above matched | yes |

Three positions earn their places. `binary` precedes `oversized` so a large image
is reported by what it *is*. `oversized` precedes `unsupported_encoding` because
a file too large to index is never decoded past the window, so calling it
undecodable would be a claim about eight kilobytes rather than about the file.
And `vendor` precedes `generated` so `node_modules/x/dist/y.js` reads as somebody
else's code rather than as this repository's output.

Classification reads at most 8 KiB of any file, and nothing at all from a file
whose *name* already makes it secret-sensitive. A UTF-16 file announced by a
byte-order mark is text rather than binary, despite being half NUL bytes; a file
with NUL bytes and no mark is binary. A character the sniff window cuts in half
is a window artifact, not an encoding failure.

`secret_sensitive` is the weaker, recorded cousin of a denial: the path and its
metadata are visible so a user can see *that* something was held back, and its
content is never retrievable. A name heuristic is not applied to a file carrying
a language extension, because source code about credentials is source code —
`token.rs` is the name of a parser far more often than of a secret. The denial
list has no such exemption.

## Bounds and truncation

| Bound | Value | On reaching it |
| --- | --- | --- |
| `MAX_INVENTORY_FILES` | 200,000 entries | stop, report `file_budget_exhausted` |
| `MAX_WALK_DURATION` | 60 seconds | stop, report `walk_time_exhausted` |
| `OVERSIZED_FILE_THRESHOLD` | 1 MiB | class `oversized` |
| `BINARY_SNIFF_BYTES` | 8 KiB | the most any file is read |
| `MAX_INVENTORY_DIAGNOSTICS` | 1,000 | count the overflow in `dropped_diagnostics` |
| `MAX_IGNORE_FILE_BYTES` | 1 MiB | refuse the rule file — fatal on layers 2 and 3, reported and skipped on a `.gitignore` |

A truncated inventory is a **partial answer** and must never be read as "the
repository has this many files" or "nothing matched". Truncation is reported, not
raised: the partial inventory is still returned, because a partial answer with a
reason attached is more useful than nothing.

Cancellation is the other way round. A cancelled walk returns
`InventoryError::Cancelled` and no inventory at all, because a caller who stopped
the walk did not ask for a subset. The token is polled before every directory
entry, so a cancellation is noticed well inside the workspace's 250 ms
visibility target whatever the size of the tree.

## Diagnostics

Carried on the inventory rather than logged, so a surface can show them beside
the entries they explain: a discarded re-inclusion, an invalid pattern, a case
collision, an unreadable path, and a path that vanished between being listed and
being read. Every quoted string is clamped, because a pattern and a path are both
repository content and neither may decide how long a Harkness message is.

An unreadable directory or file is a diagnostic and never a failure: one
permission bit must not cost a whole inventory, exactly as one unreadable file
does not cost a whole snapshot.

## Errors

`InventoryError` publishes five kinds, following the `GitError::KINDS`
convention: `root_unavailable`, `not_a_directory`, `walk_failed`,
`ignore_rule_invalid`, and `cancelled`. None of them collides with
`ContextDomainError`'s namespace.

## Versioning

`CLASSIFY_VERSION` covers the denial list and every classification rule together.
Bumping it invalidates whatever was derived from a classification — the index
cache above all — rather than silently reclassifying evidence recorded under the
old rules.

## What the inventory is not

It hashes no content, persists nothing, watches nothing, and emits no events. It
is an in-memory product: chunking, storage, reconciliation, and the events that
report a walk all read it and none of them lives here.
