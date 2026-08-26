# Process-group spawn/reap primitives land without `process.run`, a registry, or the PBA reload orchestrator

Phase 7's title ("Subprocess PGID Gating & Safe Reload Orchestration") and its cited prose
(`docs/oblisk-supervisor-services-dbus.md` § 12) describe a much larger eventual system than
its own spec body asks this phase to build: `process.run`'s Lua binding, a line-buffered
non-blocking stdout/stderr-to-Lua pipeline, a tracked table of active process handles, and the
Presentation-Before-Authority generation-swap orchestrator (§ 15, `build-steps.md`'s very next
phase heading) that would eventually call this phase's reaper on a crashed or superseded
Renderer generation. None of that exists yet, same situation ADR-0015 and ADR-0017 hit for
polkit's challenge metadata and `audio.apps`:

1. **`process.run` / the Lua IPC surface.** Nothing in this codebase calls subprocess spawning
   today -- there's no Lua VM, no scene node, no config table shape to bind against. Building
   the binding now would mean guessing at an interface nothing else defines.
2. **Line-buffered non-blocking stdout/stderr streaming to Lua callbacks.** `§ 12`'s other
   half ("No Lua Blockage"). Explicitly out of scope per this phase's own instructions -- a
   separate, later concern from the same section.
3. **A process registry/table keyed by generation id or similar.** `§ 12` says "the Supervisor
   tracks active process handles," but with no real caller yet, any registry shape invented
   here would be speculative -- the caller that eventually exists (`process.run`'s
   implementation, or the Phase 8 reload orchestrator) should dictate that data structure's
   real shape, not this phase guessing ahead of it.
4. **The Presentation-Before-Authority generation-swap orchestrator** (`§ 15`): candidate
   spawn, null-buffer staging, presentation-feedback verification, atomic promote. A
   completely separate, much larger phase (`build-steps.md`'s "Phase 8: Hot-Reload
   Presentation Before Authority (PBA) Flow") that consumes this phase's reaper as one
   low-level primitive among several, not something this phase builds toward on its own.

Decision: `supervisor/src/process/mod.rs` ships exactly the two primitives Phase 7's spec body
actually asks for, both `#[allow(dead_code)]` with no caller yet (module declared in `main.rs`
via `mod process;`, not wired into `main()`'s runtime):

- `spawn_group_leader(cmd, args) -> io::Result<tokio::process::Child>`: spawns as the leader
  of a new, independent process group. Deviates from the spec snippet's literal
  `unsafe { .pre_exec(|| nix::unistd::setpgid(...)) }` -- `tokio::process::Command` already
  has a safe `process_group(0)` builder method doing exactly that ("a process group ID of 0
  will use the process ID as the PGID," confirmed in the vendored tokio 1.53.1 source), so
  there's no `unsafe` block or hand-rolled `setpgid` call to write at all.
- `reap_process_group(child, grace) -> io::Result<ReapOutcome>`: `SIGTERM` to the whole group
  via `nix::sys::signal::killpg` (avoids the sign-error footgun of manually negating a pid),
  wait up to `grace` for the group leader to exit, escalate to `SIGKILL` on the whole group if
  it hasn't. `grace` is a caller-supplied `Duration`, not the spec's hardcoded 100ms --
  `DEFAULT_REAP_GRACE` names that value as a constant for whoever the real caller ends up
  being; tests pass their own short durations instead of eating a real 100ms wait each run.
  `ReapOutcome::ExitedCleanly`/`Escalated` makes the escalation decision observable in the
  return value rather than only inferable from process-exit side effects.

Tested against real OS process/process-group behavior (this crate's established discipline --
see `dbus::polkit`'s p2p D-Bus tests and `audio::mixer`'s PipeWire-property tests -- no mocks
needed here either): a spawned child's pgid differs from the test process's own
(`nix::unistd::getpgrp`); a `SIGTERM`-compliant child is reaped without escalation; a
`SIGTERM`-ignoring child (`trap '' TERM`) is escalated to `SIGKILL`; and a grandchild a child
backgrounds into the same group (without calling `setsid`/`setpgid` itself) is also gone after
one `reap_process_group` call, confirmed via `/proc/{grandchild_pid}` -- the actual point of
process-group gating over a plain per-pid kill, matching the spec's own "clean up active screen
recorders and input overlays" framing.

Upgrade path, in order: (a) `process.run`'s Lua binding gives `spawn_group_leader` a real
caller and defines what config shape a process registry actually needs to be, replacing the
`#[allow(dead_code)]` ceiling; (b) the line-buffered non-blocking stdout/stderr pipeline
(`§ 12`'s other half) rides alongside that binding, separately; (c) Phase 8's PBA reload
orchestrator (`§ 15`) calls `reap_process_group` on a superseded or crashed Renderer
generation's process handle as one step in its larger candidate-spawn/promote sequence -- that
orchestrator owns whatever registry or generation-keyed tracking it needs, built against its
own real requirements instead of guessed here.

This does not contradict `docs/oblisk-supervisor-services-dbus.md` § 12 or § 15, or
`build-steps.md`'s Phase 7/8 text: all three describe the target shape once `process.run`, the
Lua VM, and the PBA orchestrator exist, and none of that is built or wired against here.
