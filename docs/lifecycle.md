# Renderer and reload lifecycle

The supervisor's private reload transaction owns generation state and decides
when a renderer may become authoritative. This file defines the process-group
adapter it uses for cleanup.

- Start each renderer in its own process group.
- On candidate failure, rollback, or successful handoff, the reload transaction
  asks the process-group adapter to send `SIGTERM`, wait two seconds, send
  `SIGKILL` if needed, and reap the child.
- No successful handoff returns until the old renderer PID is reaped.
- On Linux, set `PR_SET_PDEATHSIG = SIGTERM` immediately before `exec`.
- After setting the signal, verify that the parent PID is still the supervisor.
- Other Unix platforms need an equivalent parent-death mechanism before they
  claim the same crash cleanup behavior.
- The phase-one headless renderer is a process fixture. It has no paint
  implementation. SHM and EGL are the only paint adapters.
- Test normal group cleanup, candidate rollback, successful handoff, and a real
  supervisor death. Unit tests alone do not exercise the parent-death race.
