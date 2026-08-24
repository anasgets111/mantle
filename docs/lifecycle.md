# Renderer and reload lifecycle

The supervisor's private reload transaction owns generation state and decides
when a renderer may become authoritative. This file defines the process-group
adapter it uses for cleanup.

- Start each renderer in its own process group.
- On candidate failure or rollback, the process-group adapter terminates and
  reaps the candidate. After a successful handoff, it terminates and reaps the
  old renderer.
- Termination sends `SIGTERM`, waits two seconds, then sends `SIGKILL` if needed.
- No successful handoff returns until the old renderer PID is reaped.
- On Linux, set `PR_SET_PDEATHSIG = SIGTERM` immediately before `exec`.
- After setting the signal, verify that the parent PID is still the supervisor.
- Do not claim the same crash cleanup behavior on another Unix platform until it
  has an equivalent parent-death mechanism.
- The phase-one headless renderer is a process fixture. It has no paint
  implementation. SHM and EGL are the only paint adapters.
- Test normal group cleanup, candidate rollback, successful handoff, and a real
  supervisor crash. Unit tests alone do not exercise the parent-death race.
