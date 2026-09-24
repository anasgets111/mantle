# Processes

Run other programs from Lua: read a command's output, launch an app the user keeps, or hold a
long-running program across reloads. Call them from event handlers (`on_click`, `on_change`,
a `timer`) or a module's top level. Storage, timers, actions and the utilities are on
[scripting](scripting.md).

Terms used below (*Supervisor*, *Renderer*, *generation*, *push*) are in the
[glossary](../glossary.md).

A clickable temperature read from an HTTP API. It needs `process.run`, [`json`](scripting.md#jsondecode),
[`log`](scripting.md#log) and a [named state](signals.md#named-state):

```lua
local temperature = state("temperature", "--")

local function refresh()
    local body = {}
    process.run("curl", { "-fsS", "--max-time", "5",
        "https://api.open-meteo.com/v1/forecast?latitude=52.52&longitude=13.41&current_weather=true" },
        function(line, stream)
            if stream == "stdout" then body[#body + 1] = line end
        end,
        function(code)
            local data = code == 0 and json.decode(table.concat(body)) or nil
            if type(data) == "table" and data.current_weather then
                temperature:set(string.format("%.0f°", data.current_weather.temperature))
            else
                log.warn("weather: curl exited", code)
            end
        end)
end
refresh()

return panel {
    id = "weather", layer = "Top", anchor = { top = true },
    child = button {
        on_click = refresh,
        children = { text { content = temperature, font_size = 14 } },
    },
}
```


## Which one do I use

| To | Use |
| :--- | :--- |
| Run a command and read its output | [`process.run`](#processrun) |
| Launch an app the user keeps | [`process.detach`](#processdetach) |
| Keep a long-running program across reloads (a recorder) | [`session_process`](#session_process) |

What each one keeps across a reload, a crash and a restart: [runtime](runtime.md#what-survives-a-reload).

## process.run

Spawns a helper, streams its output line by line, and reports its exit.

| Part | Contract |
| :--- | :--- |
| Signature | `process.run(cmd, args, out_cb, exit_cb)` → handle |
| `cmd` | Program name, looked up on `PATH`. No shell: no globbing, pipes, `~`, `$VAR` or quoting |
| `args` | List of already-split strings; `"a b"` is one argument. Numbers coerce |
| `out_cb(line, stream)` | Once per line, newline stripped. `stream` is `"stdout"` or `"stderr"`. Invalid UTF-8 is replaced; a final line without a newline still arrives |
| `exit_cb(code)` | Once, after both streams close and the process exits. `code` is the exit status, or `nil` when a signal ended it or the spawn failed |
| Handle | `handle:kill()`: `SIGTERM` to the whole process group, `SIGKILL` 100 ms later. `exit_cb` still fires. A no-op after exit |
| stdio | stdin `/dev/null`, so a prompt fails instead of hanging; stdout and stderr piped |
| Environment | Inherited from the shell. The working directory is the shell's and unspecified: use absolute paths |
| Lifetime | Until the next reload, failed ones included, which kills its group as `kill()` does and calls `exit_cb(nil)` at once, before the new evaluation runs; no `out_cb` follows. A Renderer replacement or shell exit reaps its group without calling `exit_cb` |
| Limits | A line over 64 KiB is cut there and the rest of that line dropped, with one warning per stream. Callbacks run outside the [CPU budget](runtime.md#limits-and-budgets). A raise in either callback is logged as a warning |

The call returns immediately. A spawn failure (command not on `PATH`) reaches Lua only as
`exit_cb(nil)` with no output; the reason is logged as a warning.

## process.detach

Starts a program that stops being the shell's: its own session, reparented to init, stdio on
`/dev/null`. It outlives reloads, Renderer replacement and the shell itself.

```lua
return panel {
    id = "dock", layer = "Top", anchor = { bottom = true },
    child = button {
        padding = 8,
        on_click = function()
            process.detach("xdg-open", { os.getenv("HOME") or "/" })
        end,
        children = { text { content = "Home folder" } },
    },
}
```

| Part | Contract |
| :--- | :--- |
| Signature | `process.detach(cmd, args)` → nothing |
| `cmd`, `args` | As `process.run` |
| Output, exit code | None. A failed spawn is logged as a warning and otherwise silent |

## session_process

Declares one named, long-running program that the Supervisor holds. It survives reloads and
Renderer replacement, and its state comes back as signals, so there is no pid file or poll.
Use `process.run` when you need the output.

```lua
-- SIGINT lets the recorder finish the file; shutdown uses it too.
local recorder = session_process { name = "recorder", stop_signal = "INT" }
local recording = recorder.running:map(function(running) return running == true end)

local function toggle()
    if recording:get() then
        recorder:stop()
    else
        local file = (os.getenv("HOME") or "") .. "/Videos/" .. os.date("%Y%m%d_%H%M%S") .. ".mp4"
        recorder:start("gpu-screen-recorder", { "-w", "screen", "-o", file })
    end
end

return panel {
    id = "rec", layer = "Top", anchor = { top = true, right = true },
    child = button {
        on_click = toggle,
        children = {
            text {
                content = computed({ recording, recorder.start_error }, function(on, err)
                    if err and err ~= "" then return "Recorder failed: " .. err end
                    return on and "● REC" or "Record"
                end),
                foreground = recording:map(function(on) return on and "#F38BA8" or "#CDD6F4" end),
            },
        },
    },
}
```

`session_process { name, stop_signal? }` returns a handle. `name` must be non-empty; declaring
the same name again, on reload or from another module, returns the same handle and re-reads only
`stop_signal`. `stop_signal` defaults to `"TERM"`. Any other key raises.

Signal names drop the `SIG` prefix: `TERM`, `INT`, `HUP`, `QUIT`, `USR1`, `USR2`, `KILL`, `STOP`,
`CONT`.

Handle signals, each `nil` until `mantle.processes` first pushes (and in `mantle check`):

| Signal | Value |
| :--- | :--- |
| `running` | Whether it is up. When `false`, the rest describe the last run |
| `pid` | Process id, also its group id; kept after exit |
| `started_at` | Unix seconds the current or last run began |
| `exit_code` | Last run's exit status; `nil` while running, before any run, or after a signal ended it |
| `start_error` | Why the last `start` spawned nothing, e.g. `cmd` not on `PATH`; `""` when it spawned |

Handle methods (call with `:`):

| Method | Effect |
| :--- | :--- |
| `start(cmd, args?)` | Spawns in a new process group, no shell, stdio inherited so output lands in `mantle log`. A no-op while running. Clears the last run's fields |
| `signal(name)` | Sends one signal to the process only, not its group. A no-op when not running |
| `stop()` | Sends `stop_signal` to the group, then `SIGKILL` to the group after 5 s. A no-op when not running |

At shutdown the Supervisor runs `stop()` on every running program and waits for each.
The whole state is also readable as `mantle.processes` ([capabilities](../capabilities/processes.md)).

## How do I…

| Task | Answer |
| :--- | :--- |
| Fetch JSON over HTTP | The example at the top |
| Poll a command every N seconds | [Below](#poll-a-command-every-n-seconds) |
| Follow a long-running command's output | [Below](#follow-a-long-running-commands-output) |
| Run a recorder that survives reloads | [session_process](#session_process) |
| Stop a child | `handle:kill()`, or save: a reload kills them all |
| Open an app or URL | [process.detach](#processdetach) |
| Read a file | `process.run("cat", { path }, ...)`, collecting lines, or [persistent_table](scripting.md#persistent_table) for JSON settings |

### Poll a command every N seconds

A reload kills the `df` in flight and clears the timer, and the top-level call starts one fresh
chain.

```lua
local disk = state("disk_usage", "")

local function poll()
    local lines = {}
    process.run("df", { "--output=pcent", "/" }, function(line, stream)
        if stream == "stdout" then lines[#lines + 1] = line end
    end, function(code)
        if code == 0 and lines[2] then disk:set(lines[2]:match("%d+%%") or "") end
    end)
    timer(30000, poll)
end
poll()

return panel {
    id = "disk", layer = "Top", anchor = { top = true },
    child = text { content = disk:map(function(value) return "/ " .. value end) },
}
```

### Follow a long-running command's output

A `process.run` child that never exits keeps calling `out_cb`. Start it at the top level: each
save kills the old one and starts one fresh. The retry below runs after the child exits on its
own; the one a reload's `exit_cb(nil)` arms is cleared with the old timers.

```lua
local title = state("now_playing", "")

local function follow()
    process.run("playerctl", { "--follow", "metadata", "--format", "{{artist}} - {{title}}" },
        function(line, stream)
            if stream == "stdout" then title:set(line) end
        end,
        function() timer(5000, follow) end)
end
follow()

return panel {
    id = "media", layer = "Top", anchor = { top = true },
    child = text { content = title, elide = "End", max_width = 300 },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `process.run("ls ~/*.png", {})` or `process.run("ls", { "~/*.png" })` | No shell parses anything, so `~`, globs and pipes stay literal. Split the arguments yourself, or run `"sh", { "-c", "..." }` explicitly |
| Calling `json.decode(line)` in `out_cb` | Output arrives one line at a time. Collect lines and decode once in `exit_cb` |
| Treating `kill()` as cancel | `exit_cb` still fires, usually with `nil`. Tag requests with a counter and ignore stale ones |
| `exit_cb` never arrives | It waits for stdout and stderr to close. A backgrounded grandchild holding the pipes delays it; redirect its output |
| A `process.run` child that must outlive a save | A reload kills it. Use [`session_process`](#session_process) |
| A failure logged on every save | A reload's kill calls `exit_cb(nil)`. Report only a non-zero `code` |
| Invalid `stop_signal` | The Supervisor refuses the declaration with a warning, and `start` then does nothing. Use a name from the list above |

See also: [scripting](scripting.md) (`timer`, `json`, `log`, `persistent_table`), [runtime](runtime.md)
(reloads, logging), [signals](signals.md) (`state`), [processes capability](../capabilities/processes.md)
(`mantle.processes`), [cli](cli.md) (`mantle log`).

Source: [process](../../renderer/src/lua/process.rs), [spawn and reap](../../supervisor/src/process/mod.rs),
[process registry](../../supervisor/src/process/registry.rs), [session process](../../renderer/src/lua/session_process.rs),
[processes controller](../../supervisor/src/capabilities/processes/controller.rs).
