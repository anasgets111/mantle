# Recipes for building, running and gating Mantle. `just` alone runs `check`.
#
# `run` depends on `build` because the Supervisor finds the Renderer as a filesystem sibling
# (`supervisor/src/generation.rs`), not as a Cargo dependency: `cargo run -p supervisor` launches
# a stale Renderer and reports it as a config error in `shell.lua`, the last place the fault is.

default: check

# Both binaries.
build:
    cargo build --workspace

release:
    cargo build --workspace --release

# The just-built shell on `config`, which keeps a dev run off `~/.config/mantle`.
run config="share/starter": build
    target/debug/mantle -c {{config}}

# Everything a change has to pass before it is done, on what would be committed.
check:
    just staged-only just fmt-check test lint docs lua types

# Runs `command` on the staged tree. With both staged and unstaged edits, the unstaged ones are saved
# as a patch, reverted, and re-applied on exit. A patch left by a killed run blocks the next.
# Untracked files stay put.
[private]
staged-only +command:
    #!/usr/bin/env bash
    set -euo pipefail
    patch="$(git rev-parse --absolute-git-dir)/unstaged.patch"
    if [ -e "$patch" ]; then
        echo "$patch holds unstaged edits from an interrupted check. Restore them: git apply $patch && rm $patch" >&2
        exit 1
    fi
    if ! git diff --cached --quiet && ! git diff --quiet; then
        git diff --binary >"$patch"
        trap 'git apply "$patch" && rm "$patch" || echo "unstaged edits not re-applied; they are in $patch" >&2' EXIT
        git checkout -- .
        echo "checking the staged tree" >&2
    fi
    {{command}}

# Once per clone: git does not version `.git/hooks`.
hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks"

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Unresolved intra-doc links, which clippy does not check. Exact in both directions: over hides a
# link that moved, under leaves room for the next regression to sit in unreported.
[doc('Unresolved intra-doc links, against an exact per-crate baseline.')]
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo doc --workspace --no-deps 2>&1)
    for pair in renderer:3 supervisor:7; do
        crate=${pair%:*} baseline=${pair#*:}
        count=$(echo "$out" | grep -A1 "unresolved link" | grep -cE "^\s*--> $crate/" || true)
        if [ "$count" -ne "$baseline" ]; then
            echo "$crate: $count unresolved doc links, baseline $baseline. Over: demote the link to a plain backtick path with the module prefix, never widen visibility for rustdoc. Under: lower the baseline here." >&2
            echo "$out" | grep -B1 -A2 "unresolved link" >&2
            exit 1
        fi
        echo "$crate: $count unresolved doc links (baseline $baseline)"
    done

# `lua` proves a file parses; this proves `share/starter` agrees with `lua-meta`, through the
# engine the author's editor uses (ADR-0081). `lua-meta` is checked alone too, because a library's
# own diagnostics are suppressed -- that hid a `---@return` whose comma made prose a second return
# type. Missing server is a failure, not a skip: a skip once let `just check` go green having
# checked no stub.
[doc('The config and the stubs type-checked against each other.')]
types:
    #!/usr/bin/env bash
    set -euo pipefail
    luals=$(command -v lua-language-server 2>/dev/null || true)
    if [ -z "$luals" ]; then
        luals=$(ls -d ~/.local/share/zed/extensions/work/lua/lua-language-server-*/bin/lua-language-server 2>/dev/null | sort -V | tail -1 || true)
    fi
    if [ -z "$luals" ]; then
        echo "no lua-language-server on PATH or in Zed's extensions. Install it: pacman -S lua-language-server" >&2
        exit 1
    fi
    log=$(mktemp -d)
    trap 'rm -rf "$log"' EXIT
    # `share/starter` ships no `.luarc.json`: `mantle init` writes one pointing at the *installed*
    # stubs, which would overwrite a checked-in copy. Absolute path: a relative one resolves
    # against the workspace being checked.
    printf '{"runtime.version":"Lua 5.4","workspace.library":["%s/lua-meta"],"workspace.checkThirdParty":false}\n' "$PWD" >"$log/starter.luarc.json"
    # `--check` prints diagnostics to stdout mixed with a progress bar it redraws with carriage
    # returns, so capture and replay without the progress chunks on failure. Not
    # `--check_format=json`: only the human form carries the source line.
    check() {
        local out
        if out=$("$luals" --check "$PWD/$1" --checklevel=Warning --logpath="$log" "${@:2}" 2>&1); then
            return 0
        fi
        echo "$1 has type diagnostics:" >&2
        printf '%s' "$out" | tr '\r' '\n' |
            sed -E '/^[[:space:]]*$/d; /^[[:space:]]*Initializing/d; /^[[:space:]]*[>=]+[[:space:]]*[0-9]+\/[0-9]+/d; /^[[:space:]]*Diagnosis complet/d' >&2
        exit 1
    }
    check share/starter --configpath "$log/starter.luarc.json"
    # No library, which is the point: these files declare everything they reference.
    printf '{"runtime.version":"Lua 5.4","workspace.checkThirdParty":false}\n' >"$log/meta.luarc.json"
    check lua-meta --configpath "$log/meta.luarc.json"
    echo "lua-meta type-checks, and the starter type-checks against it"

lua_dirs := "lua-meta share"

# Here, not beside `cargo fmt`, so `lua_dirs` is written once and a Lua-only commit is gated by
# `just lua types` alone. `tools/luafmt.py` says why the formatter is a language server.
[doc('Every Lua file parses and is formatted.')]
lua:
    #!/usr/bin/env bash
    set -euo pipefail
    find {{lua_dirs}} -name '*.lua' -print0 | xargs -0 -n1 luac -p
    echo "all lua parses"
    python3 tools/luafmt.py --check {{lua_dirs}}

# Regenerate `lua-meta/mantle.lua` from the supervisor's payload types, then show what moved.
stubs:
    UPDATE_STUBS=1 cargo test -p supervisor stubs
    @git diff --stat -- lua-meta/mantle.lua

# Separate from `lint` because a diff and a warning fail differently, and folding them buries the
# diff. 71266cb and c83e79e landed four unformatted files with `just check` green on both.
[doc('rustfmt as a gate. `just fmt` fixes it.')]
fmt-check:
    cargo fmt --all -- --check

# Both languages, unlike the gates, because nobody wants two commands to fix a diff.
fmt:
    cargo fmt --all
    python3 tools/luafmt.py {{lua_dirs}}

# Where `cargo install` put the shell that is actually running.
cargo_bin := env("CARGO_HOME", home_directory() / ".cargo") / "bin"

# What `just swap` builds; `just profile=release swap` builds the shipped binary instead.
profile := "swap"

# The edit-build-swap-restart loop for the compositor-interaction bugs no unit test reaches, and the
# only path that exercises an optimised build. `args` passes through to the new shell, as in
# `just swap --profile=120`.
#
# Kills by the `exe` symlink, never by name: any `pkill -f` pattern holding "mantle" also matches
# the calling shell and takes the terminal down with it, and `pkill -x` cannot tell the `cargo_bin`
# copy from a `target/` one.
[doc('Rebuild, swap both binaries under `cargo_bin`, and restart the shell detached.')]
swap args="":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --workspace --profile {{profile}}
    for d in /proc/[0-9]*; do
        case "$(readlink "$d/exe" 2>/dev/null)" in
            {{cargo_bin}}/mantle|{{cargo_bin}}/mantle-renderer) kill "${d#/proc/}" || true;;
        esac
    done
    # Copying over a running binary is ETXTBSY, so let both of those actually go first.
    sleep 2
    install -Dm755 target/{{profile}}/mantle          "{{cargo_bin}}/mantle"
    install -Dm755 target/{{profile}}/mantle-renderer "{{cargo_bin}}/mantle-renderer"
    "{{cargo_bin}}/mantle" -d {{args}}

# Dev binaries: `[profile.release] strip = true` leaves a capture of bare addresses. The prefix
# holds a *copy* of the Supervisor, since `current_exe` resolves symlinks, beside a wrapper
# standing in for the Renderer; what that wrapper does is the only difference between the modes.
#
# Two heaptrack patches, neither visible in its own output. `exec`, because the Supervisor refuses
# a control-socket claim from any pid but the child it spawned and heaptrack runs its target as a
# child. `setsid`, because `exec` then leaves heaptrack's reader in the group `reap_process_group`
# ends with `killpg`, which truncated the first capture mid-flush.
[doc('Record a heaptrack capture of `renderer` or `supervisor` for `secs`, then restore the installed shell.')]
heaptrack which="renderer" secs="900": build
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{which}}" in renderer|supervisor) ;; *) echo "which: renderer or supervisor" >&2; exit 2;; esac
    work="$PWD/target/heaptrack"
    mkdir -p "$work/bin" "$work/prefix"
    # heaptrack resolves helpers as `$EXE_PATH/../lib`, so the copy needs that layout.
    ln -sfn /usr/lib "$work/lib"
    # Anchored on `DUMP_HEAPTRACK_OUTPUT=` and on leading whitespace: a bare `"$client" "$@"` also
    # matches heaptrack's gdb branch, and patching that one corrupts it.
    sed -e 's|DUMP_HEAPTRACK_OUTPUT="$pipe" "$client" "$@"|DUMP_HEAPTRACK_OUTPUT="$pipe" exec "$client" "$@"|' \
        -e 's|^    "$INTERPRETER" < $pipe \| $COMPRESSOR > "$output" &|    setsid sh -c "$INTERPRETER < $pipe \| $COMPRESSOR > $output" \&|' \
        -e 's|^    $COMPRESSOR < $pipe > "$output" &|    setsid sh -c "$COMPRESSOR < $pipe > $output" \&|' \
        "$(command -v heaptrack)" > "$work/bin/heaptrack"
    chmod +x "$work/bin/heaptrack"
    cp target/debug/mantle "$work/prefix/mantle"
    if [ "{{which}}" = renderer ]; then
        printf '#!/bin/sh\nexec "%s/bin/heaptrack" -o "%s/renderer" "%s/target/debug/mantle-renderer" "$@"\n' \
            "$work" "$work" "$PWD" > "$work/prefix/mantle-renderer"
    else
        # `spawn_group_leader` adds to the inherited environment, so heaptrack's LD_PRELOAD would
        # trace the Renderer too: two writers on one fifo, its "duplicate exe event".
        printf '#!/bin/sh\nexec env -u LD_PRELOAD -u DUMP_HEAPTRACK_OUTPUT "%s/target/debug/mantle-renderer" "$@"\n' \
            "$PWD" > "$work/prefix/mantle-renderer"
    fi
    chmod +x "$work/prefix/mantle-renderer"
    rm -f "$work/{{which}}.zst"
    stop() {
        for d in /proc/[0-9]*; do
            case "$(readlink "$d/exe" 2>/dev/null)" in
                {{cargo_bin}}/mantle|"$work"/prefix/mantle|*/mantle-renderer) kill "${d#/proc/}" || true;;
            esac
        done
    }
    stop
    sleep 2
    if [ "{{which}}" = renderer ]; then
        "$work/prefix/mantle" -d --profile=120
    else
        # Not `-d`: it re-execs detached, and heaptrack would follow the process that leaves.
        setsid "$work/bin/heaptrack" -o "$work/supervisor" "$work/prefix/mantle" --profile=120 >/dev/null 2>&1 &
    fi
    echo "recording {{which}} for {{secs}}s; this is a dev build under heaptrack, so it will be slow"
    sleep {{secs}}
    stop
    # The reader outlived the killpg; give it time to drain the fifo and close the stream.
    sleep 8
    "{{cargo_bin}}/mantle" -d --profile=120
    zstd -t "$work/{{which}}.zst"
    echo "heaptrack_print -f $work/{{which}}.zst --print-leaks -n 15"
