#!/usr/bin/env sh
# Checks docs/verification-suite.md against the tests that actually exist.
#
# The document names one test per mandated scenario. Left as prose that would be
# true only on the day it was written; checked here, a renamed or deleted test
# fails a job instead of quietly leaving a release-blocking scenario uncovered.
# This is the same bargain `run-ignored-exact-test.sh` makes for the latency
# targets it names, for the same reason.
#
# Two things are verified, and the second is the one that is easy to forget:
#
#   1. every test the document names still exists in the package it names, and
#   2. every scenario the milestone mandates still appears in the document.
#
# Without (2) a scenario could be covered by deleting its row. The mandated list
# lives here rather than in the document so the document cannot license its own
# omissions.
#
# Usage: sh .github/scripts/verify-suite-mapping.sh [cargo-flag...]
set -eu

document=docs/verification-suite.md
if [ ! -f "$document" ]; then
    echo "error: $document not found; run this from the repository root" >&2
    exit 2
fi

# Every scenario the milestone requires the suite to hold. Adding one here
# before its row exists is the intended order: the check names what is missing.
mandated='
flagship-command-line
flagship-engine
front-end-equivalence
clean-and-dirty-repository
valid-patch
invalid-patch
process-success
process-failure
process-timeout
user-cancellation
approval-granted
approval-denied
sqlite-lock-contention
missing-artifact-file
interrupted-run-recovery
concurrent-read-only-calls
conflicting-mutating-calls
paths-with-spaces-and-unicode
symlink-outside-workspace
invalid-run-state-transition
invalid-tool-call-state-transition
invalid-tool-input
invalid-tool-output
migrate-from-frozen-v1
schema-newer-than-supported
path-escape-dot-dot
path-escape-symlink
environment-leakage
shell-metacharacters-inert
approval-rebinding
repository-policy-cannot-weaken
latency-policy-evaluation
latency-registry-lookup
latency-per-call-overhead
latency-event-batch-persist
latency-run-list-100
latency-event-load-1000
latency-cancellation-visible
latency-approval-dispatch
latency-streaming-assembly
latency-inventory-walk
latency-chunking-1mib
latency-incremental-update
latency-lexical-search
latency-filename-search
'

# Rows are `| `scenario` | … | `package` | `test` |`, with an optional budget
# column in between, so the package and test are taken from the last two cells
# rather than from fixed positions. A row is a mapping only when its package
# cell names a crate of this workspace, which is what keeps the prose tables in
# the same document — job names, runners — out of the result.
rows=$(
    awk -F'|' '
        /^\|/ {
            if (NF < 5) next
            scenario = $2; package = $(NF - 2); test = $(NF - 1)
            gsub(/^[ \t]+|[ \t]+$/, "", scenario)
            gsub(/^[ \t]+|[ \t]+$/, "", package)
            gsub(/^[ \t]+|[ \t]+$/, "", test)
            if (scenario !~ /^`[a-z0-9-]+`$/) next
            if (package !~ /^`harkness-[a-z-]+`$/) next
            if (test !~ /^`[A-Za-z0-9_:]+`$/) next
            gsub(/`/, "", scenario); gsub(/`/, "", package); gsub(/`/, "", test)
            print scenario " " package " " test
        }
    ' "$document"
)

if [ -z "$rows" ]; then
    echo "error: $document names no scenario at all; the parser or the tables changed" >&2
    exit 1
fi

status=0

# --- (2) every mandated scenario is claimed by at least one row -------------
for scenario in $mandated; do
    if ! printf '%s\n' "$rows" | cut -d' ' -f1 | grep -Fqx "$scenario"; then
        echo "error: $document has no row for the mandated scenario '$scenario'" >&2
        status=1
    fi
done

# ... and no row claims a scenario the milestone does not mandate, which would
# otherwise be a typo silently covering nothing.
for scenario in $(printf '%s\n' "$rows" | cut -d' ' -f1 | sort -u); do
    if ! printf '%s\n' "$mandated" | grep -Fqx "$scenario"; then
        echo "error: $document names '$scenario', which is not a mandated scenario" >&2
        status=1
    fi
done

# --- (1) every named test still exists --------------------------------------
packages=$(printf '%s\n' "$rows" | cut -d' ' -f2 | sort -u)
for package in $packages; do
    # The window's other three test binaries set `harness = false`, so libtest
    # never sees `--list` and they would simply run. Only the binary's own unit
    # tests are listable, and they are where its mapped tests live.
    target=
    if [ "$package" = "harkness-gui" ]; then
        target="--bin harkness-gui"
    fi
    echo "listing $package…"
    # shellcheck disable=SC2086
    listing=$(cargo test --locked -p "$package" $target "$@" -- --list)
    for test in $(printf '%s\n' "$rows" | awk -v p="$package" '$2 == p { print $3 }'); do
        if ! printf '%s\n' "$listing" | grep -Fqx "$test: test"; then
            echo "error: package '$package' has no test named '$test'" >&2
            status=1
        fi
    done
done

if [ "$status" -ne 0 ]; then
    echo "" >&2
    echo "docs/verification-suite.md and the test binaries disagree. A renamed test" >&2
    echo "is a change to that document in the same commit." >&2
    exit "$status"
fi

echo "verified $(printf '%s\n' "$rows" | wc -l | tr -d ' ') mappings over $(printf '%s\n' "$packages" | wc -l | tr -d ' ') packages"
