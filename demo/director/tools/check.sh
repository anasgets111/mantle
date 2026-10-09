#!/usr/bin/env bash
# Headless demo validation, no shell started or stopped: the planner tests, every edit's checkpoint
# through `mantle check` at three screens with and without the mock samples (`typo` must fail,
# naming `aling_v`), layout fit, typing budget and line length. Everything runs; the exit code is
# the failure count. Mocks run at every size: a popup's data can overflow only on some screens, and
# the 6 runs per checkpoint cost about 1 s each.
set -uo pipefail
cd "$(dirname "$0")/../../.."
MAX_COLS=108

columns() {
    awk -v max="$MAX_COLS" 'length($0) > max { print FILENAME ":" FNR ": " length($0) " columns" }' \
        demo/director/stages/*
}

lua=$(command -v lua5.4 || command -v lua) || { echo "demo-check needs lua or lua5.4" >&2; exit 2; }
command -v rsvg-convert >/dev/null || { echo "demo-check needs rsvg-convert (librsvg)" >&2; exit 2; }
mantle=${CARGO_TARGET_DIR:-target}/swap/mantle
[ -x "$mantle" ] || mantle=$(command -v mantle) || { echo "demo-check needs a built or installed mantle" >&2; exit 2; }
echo "mantle: $mantle"

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
fail=0

"$lua" demo/director/tools/test.lua || fail=$((fail + 1))
"$lua" demo/director/tools/checkpoints.lua "$out" || exit 2
# The template is not Lua, so each pruned snapshot must parse and be formatted. `typo` is meant to fail
# the engine, not the parser.
find "$out" -name '*.lua' -print0 | xargs -0 -n1 luac -p || fail=$((fail + 1))
python3 tools/luafmt.py --check "$out" || fail=$((fail + 1))
for dir in "$out"/*/; do
    name=$(basename "$dir")
    for screen in 1920x1080 1920x1200 3440x1440; do
        for mocks in 0 1; do
            if [ $mocks = 1 ]; then export MANTLE_DEMO_MOCKS=1; else unset MANTLE_DEMO_MOCKS; fi
            report=$(MANTLE_DEMO_SCREEN=$screen timeout 60 "$mantle" -c "$dir" check 2>&1)
            code=$?
            if [ "$name" = typo ]; then
                { [ $code -ne 0 ] && grep -q aling_v <<<"$report"; } && continue
                echo "FAIL typo $screen mocks=$mocks: expected a failure naming aling_v"
            else
                [ $code -eq 0 ] && continue
                echo "FAIL $name $screen mocks=$mocks: $(head -n1 <<<"$report")"
            fi
            fail=$((fail + 1))
        done
    done
done
unset MANTLE_DEMO_MOCKS
echo "check: $fail failure(s) so far"

"$lua" demo/director/tools/layout_fit.lua || fail=$((fail + 1))
"$lua" demo/director/tools/timing.lua || fail=$((fail + 1))
long_lines=$(columns)
[ -n "$long_lines" ] && echo "$long_lines" >&2
long=$(printf "%s" "$long_lines" | grep -c .)
echo "lines over $MAX_COLS columns: $long"
[ "$long" -eq 0 ] || fail=$((fail + 1))
exit $((fail > 0))
