Declare programs with [`session_process`](../guide/processes.md#session_process), which sends these actions for you.

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

One task per session process owns its child and is the only place that signals it, so a recycled
pid is never hit. How `process.run`, `process.detach` (which application launches also use) and
`session_process` spawn and end: [processes](../guide/processes.md#which-one-do-i-use).
