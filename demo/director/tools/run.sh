#!/usr/bin/env bash
# Runs the demo director from the swap build, one at a time. `-o` keeps the lock fd out of the
# shells the director restarts, which would otherwise hold it after the take.
# Reads MANTLE_DEMO_* from the environment; see the `demo` recipe in the justfile.
cd "$(dirname "$0")/../../.."
target=${CARGO_TARGET_DIR:-target}
[[ $target = /* ]] || target=$PWD/$target
PATH="$target/swap:$PATH" flock -n -o -E 99 "${XDG_RUNTIME_DIR:-/tmp}/mantle-demo.lock" \
    "$target/swap/mantle" -c demo/director
code=$?
[ $code -ne 99 ] || echo "a demo is already running (lock ${XDG_RUNTIME_DIR:-/tmp}/mantle-demo.lock)" >&2
exit $code
