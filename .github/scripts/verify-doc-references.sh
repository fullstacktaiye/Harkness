#!/usr/bin/env sh
# Checks the "What proves this" tables in docs/ against the tests that exist.
#
# The reference documents state invariants a contributor relies on — the transition
# tables, the tightening-only rule, what a grant binds, the storage guarantees —
# and each names the test that holds the code to it. Left as prose those names
# would be true only on the day they were written. Checked here, a renamed or
# deleted test fails a job instead of quietly leaving a documented guarantee
# unproven.
#
# This is the same bargain `verify-suite-mapping.sh` makes for the release
# scenarios, and it is deliberately a second script rather than a flag on that
# one: that document is checked against a *mandated* list as well, so a scenario
# cannot be covered by deleting its row. These documents have no such list —
# what they must not do is name a test that has gone.
#
# The complementary checks that need no Cargo — that every cited repository path
# exists, that every link resolves, and that the tool-authoring example is the
# file it says it mirrors — are in `harkness-runtime`'s `documentation` test, so
# they run in `cargo test --workspace` on every platform.
#
# Usage: sh .github/scripts/verify-doc-references.sh [cargo-flag...]
set -eu

documents='
docs/architecture-runtime.md
docs/tool-authoring.md
docs/policy.md
docs/approvals.md
docs/run-lifecycle-and-storage.md
docs/mock-agent-scenarios.md
docs/release-readiness-v0.3.md
docs/context-index.md
'

for document in $documents; do
    if [ ! -f "$document" ]; then
        echo "error: $document not found; run this from the repository root" >&2
        exit 2
    fi
done

# A mapping row ends `| `harkness-<crate>` | `test::path` |`, whatever its
# leading columns say. Taking the package and test from the *last* two cells
# keeps the tables free to differ in shape, and requiring the package cell to
# name a crate of this workspace keeps the prose tables in the same documents —
# risk levels, event kinds, migrations — out of the result.
rows=$(
    for document in $documents; do
        awk -F'|' -v document="$document" '
            /^\|/ {
                if (NF < 4) next
                package = $(NF - 2); test = $(NF - 1)
                gsub(/^[ \t]+|[ \t]+$/, "", package)
                gsub(/^[ \t]+|[ \t]+$/, "", test)
                if (package !~ /^`harkness-[a-z-]+`$/) next
                if (test !~ /^`[A-Za-z0-9_:]+`$/) next
                gsub(/`/, "", package); gsub(/`/, "", test)
                print document " " package " " test
            }
        ' "$document"
    done
)

if [ -z "$rows" ]; then
    echo "error: the v0.3 documents name no test at all; the parser or the tables changed" >&2
    exit 1
fi

status=0
packages=$(printf '%s\n' "$rows" | cut -d' ' -f2 | sort -u)
for package in $packages; do
    # The window's other three test binaries set `harness = false`, so libtest
    # never sees `--list` and they would simply run. Only the binary's own unit
    # tests are listable, and they are where its cited tests live. Same guard,
    # for the same reason, as `verify-suite-mapping.sh`.
    target=
    if [ "$package" = "harkness-gui" ]; then
        target="--bin harkness-gui"
    fi
    echo "listing $package…"
    # shellcheck disable=SC2086
    listing=$(cargo test --locked -p "$package" $target "$@" -- --list)
    for test in $(printf '%s\n' "$rows" | awk -v p="$package" '$2 == p { print $3 }' | sort -u); do
        if ! printf '%s\n' "$listing" | grep -Fqx "$test: test"; then
            citing=$(printf '%s\n' "$rows" | awk -v t="$test" '$3 == t { print $1 }' | sort -u | tr '\n' ' ')
            echo "error: package '$package' has no test named '$test' (cited by: $citing)" >&2
            status=1
        fi
    done
done

if [ "$status" -ne 0 ]; then
    echo "" >&2
    echo "The v0.3 documents and the test binaries disagree. A renamed test is a" >&2
    echo "change to the document that cites it, in the same commit." >&2
    exit "$status"
fi

echo "verified $(printf '%s\n' "$rows" | wc -l | tr -d ' ') citations over $(printf '%s\n' "$packages" | wc -l | tr -d ' ') packages"
