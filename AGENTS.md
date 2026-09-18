# Obelisk

A Wayland shell engine, not a shell: the Rust workspace (`renderer`, `supervisor`, `shared`) runs shells
that Lua configs declare. It ships none of its own; `share/starter` is a minimal example config.

Rust code, tests and comments never depend on or cite a particular config. Engine tests use inline fixtures.
A feature one config lacks is not an engine gap.

## Ponytail: lazy senior dev mode

- **Removing lines is always welcome.** Redundant, dead or duplicated code goes.
- **New lines only when nothing else works.** The smallest diff in the right place wins; the smallest diff in
  the wrong place is a second bug.
- **No walls of text.** Replies, comments, commits, docs. Lead with the answer, tables over bullets, bullets
  over paragraphs, the number over the adjective. `unslop` applies to every one of them.
- **Comments** state the non-obvious decision, never the mechanism and never what the code used to do.
  Rationale longer than a line belongs in an ADR or nowhere.
- **Commit messages** say what changed and why it is not obvious. No before-state, no session transcript.

Before writing code, trace the real flow end to end, then stop at the first rung that holds:

1. Does it need to exist? (YAGNI)
2. Does the codebase already have it? Reuse it.
3. Does std, a platform feature or an installed crate cover it? Use it.
4. Can it be one line?
5. Only then, the minimum code that works.

- Fix the root cause, not the symptom. Grep every caller and fix the shared function once.
- No new abstractions, dependencies, boilerplate or files unless required.
- Boring over clever. Between similar-sized approaches, take the edge-case-correct one.
- Mark a deliberate simplification with a `ponytail:` comment naming its ceiling and upgrade path.
- Never lazy about understanding the problem, trust-boundary validation, data loss, security, accessibility or
  real-hardware calibration.
- Non-trivial logic gets one runnable check; trivial one-liners get none.

## Lua stubs

- **Capability payloads and actions** come from the Rust `*State`/`*Action` types; doc comments become the
  descriptions. Run `just stubs` and commit `lua-meta/obelisk.lua`. Never hand-edit it.
- **Node and surface properties** are hand-written (ADR-0081). Edit `lua-meta/nodes.lua`/`surfaces.lua` in
  the same commit as `accepted_properties`.
- **New Lua globals or signals** go in `lua-meta/globals.lua`/`signals.lua` in the same commit.
- **`just check` is the gate**, `just types` included. It needs `lua-language-server`.

## Logging

- **Every runtime diagnostic is `error!`/`warn!`/`info!`/`debug!`** from `shared`, imported by path
  (`use shared::warn;`). The clock, level and subsystem come from the macro; never write the
  subsystem into the message. `eprintln!` is for CLI output that runs before `shared::log::init`.
- **`OBELISK_LOG`** takes `debug`, `warn,tray=debug`, `network=off` (ADR-0229).

## Testing

- **Never hardcode `/sys` or `/proc`.** Readers take `sys_root`/`proc_root`; tests point them at a tempdir.
- **Tests live beside the code** in `#[cfg(test)] mod tests`; `shared/tests` is the one exception.
- **D-Bus tests use `p2p_pair()`** (`capabilities/test_support.rs`), never the session bus.
