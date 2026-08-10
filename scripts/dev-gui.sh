#!/bin/sh
# Runs harkness-gui for development.
#
# QML edits apply to the running window on their own; see the QML hot reload in
# crates/harkness-gui/cxx/qmlhotreload.h. Rust edits cannot, so this script adds
# the other half: a file watcher that rebuilds and restarts the binary whenever
# a Rust source in the workspace changes.
#
# Without a watcher installed it just runs the GUI once, which is still enough
# to iterate on QML.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

if command -v cargo-watch >/dev/null 2>&1; then
    exec cargo watch \
        --watch crates \
        --ignore '**/*.qml' \
        --shell 'cargo run -p harkness-gui'
fi

if command -v watchexec >/dev/null 2>&1; then
    exec watchexec \
        --watch crates \
        --exts rs \
        --restart \
        -- cargo run -p harkness-gui
fi

echo "dev-gui: no file watcher found; QML reloads live, Rust changes need a restart." >&2
echo "dev-gui: install one with 'cargo install cargo-watch' to restart on Rust edits." >&2
exec cargo run -p harkness-gui
