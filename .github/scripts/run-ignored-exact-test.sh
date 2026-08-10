#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <package> <exact-test-name>" >&2
    exit 2
fi

package=$1
test_name=$2
listing=$(cargo test --locked -p "$package" "$test_name" -- --ignored --exact --list)
if ! printf '%s\n' "$listing" | grep -Fqx "$test_name: test"; then
    echo "error: package '$package' has no ignored test named '$test_name'" >&2
    exit 1
fi

cargo test --locked -p "$package" "$test_name" -- --ignored --exact
