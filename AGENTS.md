# Mantle

Rust engine that runs desktop shells declared in Lua. It ships no shell; `share/starter/shell.lua` is a
minimal config. Engine code, tests and comments never cite or depend on a user config, and a feature
missing from one is not an engine gap. Tests use self-contained fixtures.

Code is the source of truth. Docs follow it: where a doc disagrees, fix the doc. ADRs are history,
cited only as the why behind behavior the code confirms.

## Map

| Path | Holds |
| --- | --- |
| `supervisor/` | `mantle` binary: CLI, capabilities, generations, stub generator |
| `renderer/` | `mantle-renderer`: Wayland, Lua VM, layout, paint, text |
| `shared/` | Wire types, capability payloads and actions, paths, log macros |
| `lua-meta/` | LuaLS stubs for config authors (generated) |
| `demo/director/` | Demo shell: `shell.lua` the storyboard, `takes.lua` the edits, `stages/take.tpl` the marked shell it types, `stages/*.lua` its modules, `layout.lua` the geometry both shells share. Guide: `docs/development/demo.md` |
| `share/starter/` | Minimal `shell.lua`, embedded in the engine; used by `just run` and `just types` |
| `packaging/` | Package definitions, PAM stack, pacman polkit rule |
| `tools/` | Lua formatter, mdBook preprocessor, `heavy-shell/` benchmark config |
| `.githooks/` | `pre-commit`, installed by `just hooks` |
| `.github/workflows/` | `docs.yml` publishes the book; `release.yml` builds packages |
| `docs/` | mdBook site at anasgets111.github.io/mantle. Writing rules and fence tags: `docs/development/documenting.md` |
| `DECISIONS.md` | ADRs, numbered `## 0081.`, cited as ADR-0081. 500+ KB: read one by number (`grep -n '^## 0081\.'`), never the whole file |
| `CONTEXT.md` | Engine vocabulary, no implementation. User-facing terms: `docs/glossary.md` |

## Commands

| Command | Use |
| --- | --- |
| `just` / `just check` | Full suite: fmt-check, test, lint, rustdoc, lua, types. The commit hook runs the part the staged paths need |
| `just fmt` | Format Rust and Lua |
| `just book` | Build the docs and check local links and anchors. `just docs` serves them |
| `just stubs` | Regenerate the generated files below |
| `just shots` | Rewrite stale docs screenshots and delete orphaned images, unrelated ones included. Review every changed image |
| `just run [config]` | Build both binaries and run `config` (default `share/starter`). `cargo run -p supervisor` can launch a stale renderer |
| `just swap [args]` | Build the `swap` profile, replace the installed pair in `$CARGO_HOME/bin` (default `~/.cargo/bin`), restart detached |
| `just demo [out]` | Record the demo video; stops every running shell for the take, then restarts it |
| `just preview <edit>` | Play the demo from one edit (`09-media`) to the end without recording |
| `just preview-beat <from> <to>` | Play the demo from one edit to another, with screenshots in `target/demo-shots/<from>` |
| `just demo-check` | Headless demo validation: planner tests, every edit's checkpoint through `mantle check` at three screens, layout fit, typing budget, line length |
| `just heaptrack [renderer\|supervisor] [secs]` | Dev build under heaptrack for `secs` (default 900), then restore the installed shell |
| `just tag-release X.Y.Z` | Set the version, run `just stubs`, date the changelog's `Unreleased`, commit (the hook checks) and tag `vX.Y.Z`. Pushing the tag publishes the release |
| `just hooks` | Install pre-commit checks chosen by staged paths; stale generated files are rewritten and the commit refused |

## Checks by change

Run the rows that match the change; combine rows across categories. Always run `git diff --check`
(and `git diff --cached --check` for staged edits). Prose alone never needs the full suite.

| Changed | Checks |
| --- | --- |
| Prose outside `docs/` (README, AGENTS) | Review wording; verify changed links, paths and commands |
| Book prose, links, navigation, theme | `just book` |
| Lua examples, screenshot fixtures, images in `docs/` | `just book` and `cargo test -p renderer every_lua_block_in_the_docs` |
| `demo/director` edits | `just lua demo-check` |
| Lua configs | `just lua types` (needs `lua-language-server`). Types covers only the starter, `demo/director` and stubs; check other runnable configs with `mantle -c DIR check` |
| Rust, Cargo files, compiled fixtures, embedded files | The commit hook; `just check` only when not committing |
| Generated files or their sources | `just stubs`, then the checks for the changed sources and outputs |
| Build tools, hooks, packaging, CI | Validate the changed script, recipe or workflow; run the affected checks |

With staged and unstaged edits both present, `just check` sets tracked unstaged edits aside
(untracked files stay). To check the working tree, run `just fmt-check test lint rustdoc lua types`.
The commit is the gate: the hook (`just hooks`) runs, on the staged tree, the stub goldens plus
fmt-check, test, lint and rustdoc for Rust paths, the docs-example test for docs, and `lua types`
for Lua. Don't run those before committing; on failure, fix and recommit. The hook skips `just book`,
`demo-check` and `mantle -c DIR check`: run those by hand. Report failed or unavailable checks.

## Ponytail mode

- **Diffs.** Delete dead, redundant and duplicated code; add lines only when nothing else works. The
  smallest diff in the wrong place is a second bug.
- **Root cause.** Fix the cause, not the symptom. Grep every caller and fix the shared function once.
- **Boring.** No new abstractions, dependencies or boilerplate unless required. Between similar
  approaches, take the edge-case-correct one.
- **Ceilings.** Mark a deliberate limit with a `ponytail:` comment naming its ceiling and upgrade path.
- **Tests.** Cover changed behavior with a runnable check, extending an existing test where possible.
  Choose coverage by risk, not line count. Prose and mechanical edits need none.
- **Full effort.** Validate affected trust boundaries, data integrity, security and accessibility.
  Calibrate hardware measurements when the change depends on them.
- **Domains.** One domain per file; split out a clear second one at any size, like `check`,
  `install` and `aur` in `updates/pacman/`. Past ~700 production lines, review the boundaries.
- **Communication.** Concise, direct, organized: lead with the answer, bullets for parallel points,
  tables for comparisons. Detail only when asked or needed for a decision, risk or blocker.
- **Writing.** Apply `unslop` everywhere; prefer numbers to adjectives. Comments explain
  non-obvious decisions or constraints, never narrate code. Commits state what changed and why.
- **ADRs.** Append one to `DECISIONS.md` (next number) when a choice is hard to reverse, surprising
  without context, and a real trade-off. Routine choices need none. About 10 lines: the decision,
  the key measurement, the rejected alternative. History, not a tutorial.
- **Changelog.** Every user-facing Lua API or CLI change adds a line under `## Unreleased` in
  `docs/changelog.md`, in the same commit. One entry per change, typed `Added`, `Changed`, `Fixed`
  or `Breaking`. Lead with what the user sees or must edit; no engine internals. It is also the
  GitHub release notes.
- **Perf.** A performance change needs an A/B on real low-end hardware, not only headless.
  Headless timing can invert.
- **Deferred work.** Work the user defers to later becomes a GitHub issue on this repo (`gh issue
  create`) with the findings, measurements and planned fix, so it survives the session.

Trace the affected flow before writing code, then take the first option that applies:

1. Not needed for the task: omit it.
2. The codebase has it: reuse it.
3. std, the platform or an installed crate covers it: use it.
4. Otherwise write the minimum clear code that handles the required cases.

## Generated files

Never hand-edit generated output: change its source, run `just stubs`, commit both. The
`the_generated_*` golden tests use `shared::check_generated`, which rewrites with `UPDATE_STUBS` set
and otherwise fails on a stale file.

| Output | Source |
| --- | --- |
| `lua-meta/mantle.lua` | `shared/src/state/` payloads and `shared/src/action/` actions; doc comments become descriptions. Test: `supervisor/src/stubs.rs` |
| `lua-meta/nodes.lua`, `surfaces.lua` | Properties: `renderer/src/lua/nodes/properties.rs`. Nested fields and keys: `lua_shape!` beside each parser. Composite types: `LuaType`. Alias names and prose: `nodes/stubs.rs`, which holds the test; `every_type_the_stubs_declare_is_accepted_by_the_engine` probes each type |
| `lua-meta/globals.lua`, `signals.lua` | Signatures and docs registered through `lua::define` and `lua::luacats`; classes beside their Rust types; shared `Signal<T>` and `Bound` header and the test in `renderer/src/lua/mod.rs` |
| `docs/capabilities/<name>.md` | Rust capability types and `docs/capabilities/intro/<name>.md` |
| Property tables in `docs/{nodes,surfaces}/*.md`, `docs/guide/paint.md` | `renderer/src/lua/nodes/properties.rs`; prose outside the generated markers is hand-written |

`renderer/build.rs` derives config-check samples from `shared/src/schema/` into Cargo's `OUT_DIR`.
They are embedded in the renderer, never checked in or touched by `just stubs`.

## Logging

- Use `error!`, `warn!`, `notice!`, `info!`, `debug!` from `shared`, imported by path
  (`use shared::warn;`). The macro adds timestamp, level and subsystem; never repeat the subsystem.
- `notice!`: start, reload, respawn, stop. `info!`: state changes, never setup (ADR-0251).
- `eprintln!` (shared's non-panicking override) only for CLI output before `shared::log::init`.
- `MANTLE_LOG` takes filters like `debug`, `warn,tray=debug`, `network=off` (ADR-0229).

## Testing

- Tests live beside the code in `#[cfg(test)] mod tests`; `shared/tests` is the only exception.
- Never hardcode `/sys` or `/proc`: readers take `sys_root`/`proc_root`; tests pass a tempdir.
- D-Bus tests use `p2p_pair()` from `supervisor/src/capabilities/test_support.rs`, or `private_bus()`
  there when they need `NameOwnerChanged`; never the session or system bus.
- `unwrap()` is a clippy error outside tests (`unwrap_used`); production code uses `let .. else`, `?`,
  or `expect` naming the invariant. Tests wait by polling inside a bound (`within` in `test_support`),
  never by a fixed sleep.
- Docs Lua blocks run per their fence tags; `lua,shot` blocks must match their images in
  `docs/images/`. Rules: `docs/development/documenting.md`.

## Delegating to agents

Any agent may coordinate others. The coordinator owns the result: it reviews, lands commits on
`main`, and writes `DECISIONS.md` and `docs/roadmap.md`.

- **Brief.** A delegate starts cold and cannot ask. Give it the files, the decisions already made,
  what must not change, the exact checks, an insertion ceiling, and permission to push back with
  evidence.
- **Isolation.** One task per git worktree, on its own branch `wt/<name>` cut from `main`.
  A delegate commits only there, one commit per change (`--amend` for follow-ups), and never
  stashes: the stash is shared across worktrees. Research runs read-only.
- **Shared outputs.** Delegates run `just stubs` and commit the regenerated files with their change.
  On a rebase conflict in a generated file, take either side, then `just stubs`; resolve
  changelog conflicts by hand.
- **Build.** Cargo names workspace artifacts by path relative to the workspace root, so worktrees
  sharing one `CARGO_TARGET_DIR` overwrite each other's crates and test stale code. Give each its
  own target dir. A target that was shared keeps foreign artifacts cargo thinks are fresh:
  `cargo clean -p shared -p supervisor -p renderer` before trusting it again.
- **Wait.** Record each delegate's process id and arm a notification that fires on exit, so nothing
  finished sits unread.
- **Distrust.** A delegate's report is a claim. Check its decisions item by item against the diff,
  then have three fresh read-only agents with no stake audit it, one lens each: decisions and
  bugs; size and duplication; tests, lifecycle, security and docs. Slop shows as a diff over its
  ceiling, helpers that duplicate existing code, a feature quietly narrowed, or tests that assert
  nothing new. Grep for an existing equivalent of every new function, type and fixture. Audit the
  commit (`git show wt/<name>`), so new files are included.
- **Sandbox.** A delegate's sandbox may block sockets, D-Bus and Wayland. Rerun failures it reports
  outside the sandbox before believing them.
- **Checks.** A delegate iterates on the tests it touched (`cargo test -p <crate> <filter>`) and
  runs the rows the hook skips; its commit runs the hook, and it fixes what fails. Never the full
  `just check`. After committing, it runs `cargo mutants --in-place --in-diff <(git diff main...)
  -p <crate>` under the lock, and kills each missed mutant with a test or names why it can't. A
  commit is a full build: commits and builds take one machine-wide lock, so only one runs at a
  time. The hook and the justfile's build recipes take it when `git config mantle.buildWrapper`
  names the lock command (shared by every worktree).
- **Land.** Send gaps back to the same delegate, which keeps its context and amends. Land one
  branch at a time: if `main` has not moved since its base, `git merge --ff-only wt/<name>`, with
  no second check. If it has, the delegate rebases onto `main`, then runs `git reset --soft main
  && git commit -C ORIG_HEAD`: the hook picks its checks from staged paths, so an amend that stages
  nothing checks nothing. Then fast-forward, and remove the worktree, branch and target dir.

## Skills

This file owns repository rules and check selection; skills own task workflows. They live in
`.agents/skills/` (symlinked as `.claude/skills`, `.gemini/skills`): `implement`, `tdd`,
`diagnosing-bugs`, `mantle-review`, `domain-modeling` (ADRs, `CONTEXT.md`), `grill-with-docs`,
`grill-me`, `project-design`, `unslop`. `architecture-review` runs only when invoked by name.
