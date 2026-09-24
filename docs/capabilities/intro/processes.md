Declare programs with [`session_process`](../guide/processes.md#session_process): it sends these
actions and exposes each field as a signal. Read `mantle.processes` directly to see every declared
program in one table.

```lua
text {
    content = mantle.processes:map(function(processes)
        local recorder = processes and processes.sessions.recorder
        return (recorder and recorder.running) and "● REC" or ""
    end),
}
```

<!-- reference -->

## Backend

The Supervisor spawns and owns each program, so it outlives reloads and Renderer replacement. One
task per program holds its child and sends every signal, so a signal never reaches a recycled pid.
When to use `process.run` or `process.detach` instead:
[processes](../guide/processes.md#which-one-do-i-use).
