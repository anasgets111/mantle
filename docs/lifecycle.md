# Renderer lifecycle

This is the lifecycle contract for the renderer process.

- Start each renderer in its own process group.
- On normal cleanup, send `SIGTERM`, wait two seconds, send `SIGKILL` if needed,
  then reap the child.
- On Linux, set `PR_SET_PDEATHSIG = SIGTERM` immediately before `exec`.
- After setting the signal, verify that the parent PID is still the supervisor.
- Other Unix platforms need an equivalent parent-death mechanism before they
  claim the same crash cleanup behavior.
- Test both normal group cleanup and a real supervisor death. Unit tests alone
  do not exercise the parent-death race.
