# Mantle

The Rust workspace crates `renderer`, `supervisor`, and `shared` execute desktop shells
declared in Lua. Mantle ships no built-in shell; `share/starter` provides a minimal config.

Rust code, tests, and comments never cite specific user configs. Engine tests use inline fixtures.
A feature missing in a user config is not an engine gap.

## Ponytail mode

- **Code deletion.** Delete redundant, dead, or duplicated code. Removing lines is always preferred.
- **Minimal diffs.** Add new lines only when nothing else works. The smallest diff in the right place wins.
  The smallest diff in the wrong place is a second bug.
- **Concise communication.** In replies, comments, commits, and docs, lead with the answer. Prefer tables
  over bullets, bullets over paragraphs, and numbers over adjectives. Apply `unslop` everywhere.
- **Decision comments.** State the non-obvious decision, never the mechanism, and never what the code used
  to do. Rationale longer than one line belongs in an ADR.
- **Commit messages.** State what changed and why. Omit before-state and session transcripts.

Trace the execution flow end-to-end before writing code, then stop at the first rung that holds:

1. Does it need to exist? Apply YAGNI.
2. Does the codebase already have it? Reuse it.
3. Does std, a platform feature, or an installed crate cover it? Use it.
4. Can it be one line?
5. Write the minimum code that works.

- Fix the root cause, not the symptom. Grep every caller and fix the shared function once.
- No new abstractions, dependencies, boilerplate, or files unless strictly required.
- Boring over clever. Between similar approaches, choose the edge-case-correct one.
- Mark deliberate simplifications with a `ponytail:` comment stating the ceiling and upgrade path.
- Never skip problem analysis, trust-boundary validation, data integrity, security, accessibility, or
  hardware calibration.
- Non-trivial logic gets one runnable check. Trivial one-liners get none.

## Lua stubs

- **Capability payloads and actions.** Rust `*State` and `*Action` types generate them. Doc comments
  become descriptions. Run `just stubs` and commit `lua-meta/mantle.lua`. Never edit it by hand.
- **Node and surface properties.** Hand-written per ADR-0081. Edit `lua-meta/nodes.lua` and
  `lua-meta/surfaces.lua` in the same commit as `accepted_properties`.
- **Globals and signals.** Add new Lua globals or signals to `lua-meta/globals.lua` or
  `lua-meta/signals.lua` in the same commit.
- **Type checking.** `just check` is the gate, including `just types`. Requires `lua-language-server`.

## Logging

- **Runtime diagnostics.** Use `error!`, `warn!`, `notice!`, `info!`, or `debug!` from `shared`, imported by path
  like `use shared::warn;`. The macro provides timestamp, level, and subsystem. Never write the
  subsystem into the message. Use `eprintln!` only for CLI output preceding `shared::log::init`.
- **Log filtering.** `MANTLE_LOG` takes filters like `debug`, `warn,tray=debug`, `network=off` per ADR-0229.
- **Levels.** `notice!` is start, reload, respawn, and stop. `info!` is a state change, never setup (ADR-0251).

## Testing

- **Path injection.** Never hardcode `/sys` or `/proc`. Readers accept `sys_root` or `proc_root`. Tests supply a tempdir.
- **Test location.** Tests live beside the code in `#[cfg(test)] mod tests`. `shared/tests` is the sole exception.
- **D-Bus tests.** Use `p2p_pair()` in `capabilities/test_support.rs`, never the session bus.
