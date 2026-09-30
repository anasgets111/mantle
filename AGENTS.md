# Mantle

Rust engine that runs desktop shells declared in Lua. It ships no shell; `share/starter/shell.lua` is a
minimal config. Engine code, tests and comments never cite or depend on a user config; tests use
self-contained fixtures. A feature missing from a user config is not an engine gap.

## Map

| Path | Holds |
| --- | --- |
| `supervisor/` | `mantle` binary: CLI, capabilities, generations, stub generator |
| `renderer/` | `mantle-renderer`: Wayland, Lua VM, layout, paint, text |
| `shared/` | Wire types, capability actions, paths, log macros |
| `lua-meta/` | LuaLS stubs for config authors |
| `demo/director/` | Demo shell; `stages/` holds the files it types, `mockups.lua` the prop windows |
| `share/starter/` | Minimal `shell.lua`, embedded in the engine and used by `just run` and `just types` |
| `packaging/` | Package definitions, PAM stack, pacman polkit rule |
| `tools/` | Lua formatter, mdBook preprocessor, `heavy-shell/` benchmark config |
| `.githooks/`, `.github/workflows/` | `pre-commit` (installed by `just hooks`); CI: `docs.yml` publishes the book, `release.yml` builds packages |
| `docs/` | mdBook site at anasgets111.github.io/mantle. Writing rules and fence tags: `docs/development/documenting.md` |
| `DECISIONS.md` | ADRs, numbered `## 0081.` and cited as ADR-0081 |
| `CONTEXT.md` | Engine vocabulary only, no implementation; user-facing terms live in `docs/glossary.md` |

Code is the source of truth. Docs follow it; ADRs are history and may be stale, cited only as the
why behind behavior the code confirms. Where a doc disagrees with the code, fix the doc.

## Commands

| Command | Use |
| --- | --- |
| `just` / `just check` | Full suite: fmt-check, test, lint, rustdoc, lua, types. Select checks by change below |
| `just book` | Build the docs site and check local links and anchors. `just docs` serves it |
| `just fmt` | Format Rust and Lua |
| `just stubs` | Regenerate the files listed under Generated files |
| `just shots` | Rewrite all stale docs screenshots and delete orphaned images, including unrelated ones. Review every changed image before committing |
| `just run [config]` | Build both binaries and run `config`, default `share/starter`. `cargo run -p supervisor` can launch a stale renderer |
| `just swap [args]` | Build the `swap` profile, replace the installed pair under `$CARGO_HOME/bin`, default `~/.cargo/bin`, and restart detached |
| `just demo [out]` | Record the demo video with `demo/director`; stops every running shell for the take, then restarts it |
| `just heaptrack [renderer\|supervisor] [secs]` | Dev build under heaptrack for `secs` (default 900), then restores the installed shell; prints the `heaptrack_print` command |
| `just hooks` | Install pre-commit checks selected by staged paths; stale stubs or generated pages are rewritten and the commit refused |

## Checks by change

Run checks for the behavior or files changed. Combine rows when a change crosses categories;
do not run the full suite for prose alone. Run `git diff --check`, plus `git diff --cached --check`
when reviewing staged edits.

| Changed | Checks |
| --- | --- |
| README, AGENTS or other prose outside `docs/` | Review wording and verify changed links, paths and commands |
| Book prose, links, navigation or theme | `just book` |
| Lua examples, screenshot fixtures or images in `docs/` | `just book` and `cargo test -p renderer every_lua_block_in_the_docs` |
| Lua configs | `just lua types`, which requires `lua-language-server`. Types covers only the starter and stubs; check other changed runnable configs with `mantle -c DIR check` |
| Rust, Cargo files, compiled fixtures or embedded files | `just check` |
| Generated files or their sources | `just stubs`, then the checks for the changed source and output files |
| Build tools, hooks, packaging or CI | Validate the changed script, recipe or workflow; run the affected checks |

With both staged and unstaged edits, `just check` temporarily sets tracked unstaged edits aside;
untracked files remain present. To check the working tree in that case, run
`just fmt-check test lint rustdoc lua types` directly. Report failed or unavailable checks;
a pre-commit hook does not replace validation of the working tree.

## Ponytail mode

- **Diffs.** Delete dead, redundant and duplicated code; add lines only when nothing else works. The
  smallest diff in the wrong place is a second bug.
- **Root cause.** Fix the cause, not the symptom. Grep every caller and fix the shared function once.
- **Boring.** No new abstractions, dependencies or boilerplate unless required. Between similar
  approaches, take the edge-case-correct one.
- **Ceilings.** Mark a deliberate limit with a `ponytail:` comment naming its ceiling and upgrade path.
- **Tests.** Cover changed behavior with a runnable check; extend an existing test where possible.
  Choose coverage by risk, not line count. Prose and mechanical edits need no new tests.
- **Full effort.** Validate affected trust boundaries, data integrity, security and accessibility.
  Calibrate hardware measurements when the change depends on them.
- **Domains.** A file holds one domain; split out a clear second one at any size, like a backend's
  `check`, `install` and `aur` in `updates/pacman/`. Past ~700 production lines, review domain
  boundaries and split where they exist. Every resulting file must have a domain of its own.
- **Communication.** All agents keep replies, progress updates, reviews and messages to other agents
  concise, direct and organized. Lead with the answer. Use short paragraphs, bullets for parallel
  points and tables for comparisons. No walls of text, repeated summaries or narration of routine
  steps. Add detail only when requested or needed to explain a decision, risk or blocker.
- **Writing.** Apply `unslop` to all communication and writing; prefer numbers over adjectives.
  Comments explain non-obvious decisions or constraints, without narrating the code.
  Commits state what changed and why.
- **ADRs.** Append one to `DECISIONS.md` (next number) when a choice is hard to reverse,
  surprising without context, and a real trade-off. Routine implementation choices need no ADR.
- **Changelog.** Every user-facing Lua API or CLI change adds a line under `## Unreleased` in
  `docs/changelog.md`, in the same commit.

Trace the affected flow before writing code, then choose the first applicable option:

1. If it is not needed for the task, omit it.
2. Does the codebase already have it? Reuse it.
3. Does std, the platform or an installed crate cover it? Use it.
4. Otherwise, write the minimum clear code that handles the required cases.

## Generated files

Never hand-edit generated output. Change its source, run `just stubs`, and include both in the
change. The `the_generated_*` golden tests use `shared::check_generated`, which rewrites when
`UPDATE_STUBS` is set and otherwise fails on a stale file.

| Output | Edit here |
| --- | --- |
| `lua-meta/*.lua` | Rust sources listed below |
| `docs/capabilities/<name>.md` | Rust capability types and `docs/capabilities/intro/<name>.md` |
| Property tables in `docs/{nodes,surfaces}/*.md` and `docs/guide/paint.md` | `renderer/src/lua/nodes/properties.rs`; prose outside the generated markers is hand-written |
| `renderer/src/check_samples.json` | Rust capability types; sample generator in `supervisor/src/stubs/samples.rs` |

| Lua stub | Source | Golden test |
| --- | --- | --- |
| `mantle.lua` | Rust `*State`/`*Action` types; doc comments become descriptions | `supervisor/src/stubs.rs` |
| `nodes.lua`, `surfaces.lua` | Properties: `renderer/src/lua/nodes/properties.rs`. Nested fields and accepted keys: `lua_shape!` beside each parser. Composite types: `LuaType`. Alias names and prose: `nodes/stubs.rs` | `renderer/src/lua/nodes/stubs.rs`; `every_type_the_stubs_declare_is_accepted_by_the_engine` probes each type |
| `globals.lua`, `signals.lua` | Global signatures and docs registered through `lua::define` and `lua::luacats`; class declarations beside their Rust types, with the shared `Signal<T>` and `Bound` header in `renderer/src/lua/mod.rs` | `renderer/src/lua/mod.rs` |

## Logging

- Use `error!`, `warn!`, `notice!`, `info!`, `debug!` from `shared`, imported by path (`use shared::warn;`).
  The macro adds timestamp, level and subsystem; never write the subsystem into the message.
- `eprintln!` (shared's non-panicking override, not std's) only for CLI output before `shared::log::init`.
- `notice!` is start, reload, respawn, stop. `info!` is a state change, never setup (ADR-0251).
- `MANTLE_LOG` takes filters like `debug`, `warn,tray=debug`, `network=off` (ADR-0229).

## Testing

- Tests live beside the code in `#[cfg(test)] mod tests`; `shared/tests` is the only exception.
- Never hardcode `/sys` or `/proc`: readers take `sys_root`/`proc_root`, tests pass a tempdir.
- Docs Lua blocks are checked according to their fence tags; `lua,shot` blocks must match their
  images in `docs/images/`. See `docs/development/documenting.md` for execution and skip rules.
- D-Bus tests use `p2p_pair()` from `supervisor/src/capabilities/test_support.rs`, or `private_bus()` there when they need `NameOwnerChanged`; never the session or system bus.

## Skills

This file owns repository rules and check selection; use skills for the task workflow.

`.agents/skills/` (also `.claude/skills`, `.gemini/skills`): `implement`, `tdd`, `diagnosing-bugs`,
`mantle-review`, `domain-modeling` (ADRs, `CONTEXT.md`), `grill-with-docs`, `grill-me`, `project-design`,
`unslop`. `architecture-review` runs only when invoked by name.
