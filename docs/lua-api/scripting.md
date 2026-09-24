# Scripting

The imperative globals that are not UI: run programs, keep settings on disk, schedule work,
answer keybinds, and a few utilities. Reach for them from event handlers (`on_click`,
`on_change`, callbacks) or a module's top level. Everything that renders goes through
[signals](signals.md) and [nodes](nodes.md) instead.

Terms used below. The *Supervisor* is the long-lived `mantle` process that owns child programs
and files. The *Renderer* is its child that runs your Lua and draws. A *generation* is one
Renderer and its VM; saving a file reloads in place and keeps it, a crash starts a new one. A
*push* is a capability sending new state; until the first one, a capability reads `nil`
([capabilities](capabilities.md#reading-and-acting), [CONTEXT](../../CONTEXT.md)).

A clickable temperature read from an HTTP API. It needs `process.run`, `json`, `log` and a
[named state](signals.md#named-state):

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
| Remember a setting across restarts | [`persistent_table`](#persistent_table) |
| Do something later, repeat, or retry | [`timer`](#timer) |
| Run code from a keybind or script | [`action`](#action) + `mantle call` |
| Parse JSON | [`json.decode`](#jsondecode) |
| Write to the shell's log | [`log.*`](#log) |
| Rank search results | [`fuzzy`](#fuzzy) |
| Pull colours out of a wallpaper | [`palette.quantize`](#palettequantize) |
| Set the font fallback chain | [`fonts`](#fonts) |

Lifetimes, compared. The full reload table is in [runtime](runtime.md#what-survives-a-reload).

| Global | Survives reload | Survives Renderer replacement | Survives shell exit |
| :--- | :--- | :--- | :--- |
| `process.run` child | Yes, callbacks included | No, group reaped | No |
| `process.detach` child | Yes | Yes | Yes |
| `session_process` program | Yes | Yes | No, stopped with `stop_signal` |
| `persistent_table` values | Yes | Yes | Yes, on disk |
| `timer`, `action` | No, cleared every evaluation | No | No |

## How do I

| Task | Answer |
| :--- | :--- |
| Fetch JSON over HTTP | The example at the top |
| Poll a command every N seconds | [Below](#poll-a-command-every-n-seconds) |
| Follow a long-running command's output | [Below](#follow-a-long-running-commands-output) |
| Run a recorder that survives reloads | [session_process](#session_process) |
| Persist a toggle or setting | [persistent_table](#persistent_table) |
| Bind a key to Lua code | [action](#action), then `mantle call <name>` from the compositor |
| Retry with backoff | [timer](#timer) |
| Search as you type | [fuzzy](#fuzzy); for a slow source, debounce the query with [`delay`](signals.md#delay-hold-a-value) |
| Open an app or URL | [process.detach](#processdetach) |
| Theme from the wallpaper | [palette.quantize](#palettequantize) |

### Poll a command every N seconds

Let the timer own the loop and keep `exit_cb` to updating state. A reload clears the timer and
the top-level call starts one fresh chain. Re-arming from `exit_cb` instead would let a child
still running across a reload arm a second chain.

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

A `process.run` child that never exits keeps calling `out_cb`. It survives reloads, so guard the
start with named state, which survives them too. When it exits, the next reload starts it again.

```lua
local title = state("now_playing", "")
local following = state("now_playing_following", false)

if not following:get() then
    following:set(true)
    process.run("playerctl", { "--follow", "metadata", "--format", "{{artist}} - {{title}}" },
        function(line, stream)
            if stream == "stdout" then title:set(line) end
        end,
        function() following:set(false) end)
end

return panel {
    id = "media", layer = "Top", anchor = { top = true },
    child = text { content = title, elide = "End", max_width = 300 },
}
```

## process.run

Spawns a helper, streams its output line by line, and reports its exit.

| | |
| :--- | :--- |
| Signature | `process.run(cmd, args, out_cb, exit_cb)` → handle |
| `cmd` | Program name, looked up on `PATH`. No shell: no globbing, pipes, `~`, `$VAR` or quoting |
| `args` | List of already-split strings; `"a b"` is one argument. Numbers coerce |
| `out_cb(line, stream)` | Once per line, newline stripped. `stream` is `"stdout"` or `"stderr"`. Invalid UTF-8 is replaced; a final line without a newline still arrives |
| `exit_cb(code)` | Once, after both streams close and the process exits. `code` is the exit status, or `nil` when a signal ended it or the spawn failed |
| Handle | `handle:kill()`: `SIGTERM` to the whole process group, `SIGKILL` 100 ms later. `exit_cb` still fires. A no-op after exit |
| stdio | stdin `/dev/null`, so a prompt fails instead of hanging; stdout and stderr piped |
| Environment | Inherited from the shell. The working directory is the shell's and unspecified: use absolute paths |
| Lifetime | The generation. A reload keeps the child and its callbacks; a Renderer replacement or shell exit reaps its group without calling `exit_cb` |
| Limits | A line over 64 KiB is cut there and the rest of that line dropped, with one warning per stream. Callbacks run outside the [CPU budget](runtime.md#limits-and-budgets). A raise in either callback is logged only at debug level: start with `mantle -vv` or `MANTLE_LOG=lua=debug` to see it |

The call returns immediately. A spawn failure (command not on `PATH`) reaches Lua only as
`exit_cb(nil)` with no output; the reason is logged at debug level (`-vv` or
`MANTLE_LOG=process=debug`).

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

| | |
| :--- | :--- |
| Signature | `process.detach(cmd, args)` → nothing |
| `cmd`, `args` | As `process.run` |
| Output, exit code | None. A failed spawn is logged at debug level (`-vv`) and otherwise silent |

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
`stop_signal`. `stop_signal` defaults to `"TERM"`.

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
The whole state is also readable as `mantle.processes` ([capabilities](capabilities.md#processes)).

## persistent_table

A JSON object on disk, read as one signal per key and written one key at a time.

```lua
local state_home = os.getenv("XDG_STATE_HOME")
if not state_home or state_home == "" then
    state_home = (os.getenv("HOME") or "") .. "/.local/state"
end

local settings = persistent_table {
    path = state_home .. "/myshell",
    name = "settings.json",
    defaults = { clock_24h = true, recorder = { fps = 60, audio = "desktop" } },
}

-- nil until the first push, so pick the default here too.
local clock_24h = settings.clock_24h:map(function(value) return value ~= false end)

return panel {
    id = "clock", layer = "Top", anchor = { top = true },
    child = button {
        on_click = function() settings:set("clock_24h", not clock_24h:get()) end,
        children = {
            text { content = clock_24h:map(function(on) return on and "24h" or "12h" end) },
        },
    },
}
```

| | |
| :--- | :--- |
| Signature | `persistent_table { path, name, defaults? }` → store |
| `path` | Absolute directory; relative raises. Created if missing. Build it from `os.getenv` or `mantle.config_dir` |
| `name` | One file name, no `/`; empty raises |
| `defaults` | Fills missing top-level keys; stored values win. Nested tables are one value and do not merge. Re-sent every evaluation, so a new default lands on reload |
| `store.<key>` | Signal for that key: `nil` before `mantle.storage` first pushes, then the stored value |
| `store:set(key, value)` | Writes one key; `nil` deletes it until the next reload re-fills its default. The signal updates on the next push, not inside `set` |
| Identity | One table per file: another call with the same `path`/`name` returns it, across reloads too |
| Raw state | `mantle.storage` ([capabilities](capabilities.md#storage)) |

On disk:

| Behavior | Detail |
| :--- | :--- |
| Save | 1 s after the last write to that file, as pretty JSON through a temporary file and rename. A write still waiting at shell exit is lost |
| First run | A missing file is created from `defaults` |
| Outside edits | Watched with inotify. Another writer's version replaces the in-memory one whole, dropping unsaved writes |
| Broken file | Not a JSON object, or unparsable: the last values stay, a warning is logged once, and nothing saves over it until it parses |

## timer

Runs a function once after a delay. Re-arm inside the callback to repeat.

```lua
local status = state("status", "checking")
local BACKOFF_MS = { 2000, 4000, 8000 }

local function check(attempt)
    attempt = attempt or 1
    process.run("ping", { "-c", "1", "-W", "2", "1.1.1.1" }, function() end, function(code)
        if code == 0 then
            status:set("online")
        elseif BACKOFF_MS[attempt] then
            -- Bounded, so a chain doubled by a reload mid-request ends on its own.
            status:set("retrying")
            timer(BACKOFF_MS[attempt], function() check(attempt + 1) end)
        else
            status:set("offline")
        end
    end)
end
check()

-- A repeating timer re-arms itself; calling it at the top level restarts the chain on reload.
local clock = state("clock", "")
local function tick()
    clock:set(os.date("%H:%M"))
    timer(60000 - (os.time() % 60) * 1000, tick)
end
tick()
```

| | |
| :--- | :--- |
| Signature | `timer(ms, fn)` → handle |
| `ms` | `1` to `86400000` (one day), monotonic clock; outside raises |
| `fn` | Called with no arguments under the 5 ms CPU budget. A raise or blown budget is logged only at debug level (`mantle -vv` or `MANTLE_LOG=lua=debug`) |
| Handle | `handle:cancel()` disarms it. A no-op once fired or cancelled. Dropping the handle does not disarm |
| Order | Timers due at the same moment fire in the order they were armed; one may cancel another in the same batch |
| Lifetime | Every evaluation clears all timers, including chains armed from callbacks. Timers armed during an evaluation start only once its result is applied |

For a delayed or blinking *value*, `delay` and `pulse` ([signals](signals.md)) need no callback.

## action

Names a function that `mantle call <name> [args...]` runs, usually from a compositor keybind.
Write a `state` when the shell should look different; call an action when it should do
something.

```lua
-- Keybind: mantle call volume.up 0.1
action("volume.up", function(step)
    local audio = mantle.audio:get()
    if not audio or not audio.volume then
        return "no output device"
    end
    local volume = math.min(1.5, audio.volume + (tonumber(step) or 0.05))
    mantle.audio:invoke("set_volume", volume)
    return string.format("%d%%", math.floor(volume * 100 + 0.5))
end)
```

| | |
| :--- | :--- |
| Signature | `action(name, fn)` → nothing |
| `name` | Any non-empty string; nothing splits on `.`. Empty, or declared twice in one evaluation, raises |
| Arguments | Each CLI argument is JSON-decoded when it parses (`0.1`, `true`, `{"a":1}`), else passed as a string |
| Return | `nil` prints nothing; a string prints bare; anything else prints as JSON. Over 1 MiB, or not convertible to JSON, fails the call |
| Failure | A raise, an unknown name, or a blown budget fails the call with the message. `mantle call` gives up after 5 s |
| Limits | 5 ms CPU budget |
| Lifetime | Cleared before every evaluation and again when one fails, so declare at the top level |

CLI flags such as `--pid`: [cli](cli.md).

## json.decode

`json.decode(text)` → value, or `nil, message`. It never raises on bad input.

| JSON | Lua |
| :--- | :--- |
| `null` field | Absent key |
| `null` array element | A hole: `ipairs` stops there, `#` may still count past it |
| Top-level `null` | `nil`, same as a failure without the message |
| Non-UTF-8 input | `nil, message` |

There is no `json.encode`; build JSON arguments with `string.format`.

## log

`log.error(...)`, `log.warn(...)`, `log.info(...)`, `log.debug(...)`. Arguments join with tabs
like `print`, and the line is stamped with time, level and the `config` subsystem. All levels
print by default. Filter with `MANTLE_LOG=config=warn` (or `config=off`); read with `mantle log`.

## fuzzy

fzf's scorer for one candidate. Iterating, sorting and tiebreaking stay in Lua.

```lua
local APPS = { "Firefox", "Files", "GIMP", "Terminal", "System Monitor" }
local query = state("query", "")

local results = query:map(function(needle)
    local scored = {}
    for _, name in ipairs(APPS) do
        local score, start = fuzzy(name, needle)
        if score then
            scored[#scored + 1] = { name = name, score = score, start = start }
        end
    end
    table.sort(scored, function(left, right)
        if left.score ~= right.score then return left.score > right.score end
        if left.start ~= right.start then return left.start < right.start end
        return left.name < right.name
    end)
    local names = {}
    for index, entry in ipairs(scored) do names[index] = entry.name end
    return names
end)

return panel {
    id = "launcher", layer = "Overlay", keyboard_interactivity = "OnDemand",
    child = column {
        width = 320, padding = 12, spacing = 6, background = "#1E1E2E",
        children = {
            textfield { autofocus = true, placeholder = "Search", on_change = function(text) query:set(text) end },
            list { source = results, itemfn = function(name) return text { content = name } end },
        },
    },
}
```

| | |
| :--- | :--- |
| Signature | `fuzzy(haystack, needle)` → `score, start` |
| Match | Integer `score` (higher is better) and `start`, the 0-based byte offset of the needle's first character at its earliest in-order hit (the best-scoring one for a one-character needle). For tiebreaks, not highlighting |
| No match | `nil, nil`, also for non-UTF-8 input |
| Empty needle | `0, 0` |
| Case | Smart: an all-lowercase needle ignores case; one uppercase character makes the whole comparison exact. Don't lowercase the query |
| Non-ASCII | A greedy scorer whose scores do not compare with the ASCII path's |

Tie order is the caller's: `table.sort` is unstable, so end the comparator on a unique key.

## palette.quantize

Median-cut dominant colours of an image, computed on a background thread.

```lua
local swatches = state("swatches", {})

palette.quantize("/usr/share/backgrounds/default.png", { depth = 3 }, function(found)
    swatches:set(found or {})
end)

return panel {
    id = "palette", layer = "Top", anchor = { bottom = true },
    child = row {
        children = swatches:map(function(list)
            local chips = {}
            for index, swatch in ipairs(list) do
                chips[index] = rect { width = math.max(4, 200 * swatch.share), height = 16, background = swatch.color }
            end
            return chips
        end),
    },
}
```

| | |
| :--- | :--- |
| Signature | `palette.quantize(path, opts?, cb)` → handle |
| `path` | Local raster image; no SVG or URL |
| `opts.depth` | `0` to `8`, default `3`: up to `2^depth` colours, fewer when the image has fewer. Out of range raises |
| `opts.rescale` | Longest edge in px before counting, default `128`; `0` is full size. Negative raises. A cached freedesktop thumbnail that covers it is used instead of decoding |
| `cb(swatches)` | `{ color = "#RRGGBB", share = 0..1 }` entries, most common first. `share` counts only non-transparent pixels. `nil` on failure, with a logged warning. Runs outside the CPU budget; a raise is logged only at debug level (`-vv`) |
| Handle | `handle:cancel()` drops the callback; the work still finishes |

## fonts

`fonts { family, ... }` sets the fallback chain every `text` node uses. Each glyph takes the
first family that covers it.

```lua
fonts { "Inter", "Symbols Nerd Font", "Noto Color Emoji" }
```

| | |
| :--- | :--- |
| Argument | Dense array of family-name strings. A hole, a named key or a non-string raises |
| Resolution | Through `fc-match`. The first family that resolves is the primary and also loads its bold and italic faces; a family with no install is skipped (logged at `-vvv`) |
| Default | Without a call: `sans-serif`, `Noto Sans CJK JP`, `Noto Color Emoji` |
| Per node | A `text` node's `font` goes in front of the chain ([nodes](nodes.md)) |
| Uncovered glyph | fontconfig is asked for any installed face that covers it |
| Lifetime | Read once at startup. Last call wins; an edit needs a shell restart |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `process.run("ls ~/*.png", {})` or `process.run("ls", { "~/*.png" })` | No shell parses anything, so `~`, globs and pipes stay literal. Split the arguments yourself, or run `"sh", { "-c", "..." }` explicitly |
| Calling `json.decode(line)` in `out_cb` | Output arrives one line at a time. Collect lines and decode once in `exit_cb` |
| `process.run` at a module's top level | It runs again on every reload, next to the child still running from the last one. Start it from a handler, or guard it with [named state](signals.md#named-state) |
| Treating `kill()` as cancel | `exit_cb` still fires, usually with `nil`. Tag requests with a counter and ignore stale ones |
| `exit_cb` never arrives | It waits for stdout and stderr to close. A backgrounded grandchild holding the pipes delays it; redirect its output |
| Callbacks from before a reload | `process.run` and `palette` callbacks run the old closures after a reload. Keep what they touch in named state |
| `timer` or `action` declared only inside a callback | Every evaluation clears both, so they vanish on the next save. Declare actions at the top level; start timer chains from the top level too |
| A failed reload | A failed evaluation leaves no timers or actions until the next good save, though the old scene stays. A failed apply keeps the new evaluation's actions but runs no timers |
| `timer` re-armed from `exit_cb` | A child in flight across a reload arms a second chain beside the one the top level restarts. Arm the timer outside the callback ([poll recipe](#poll-a-command-every-n-seconds)) |
| A callback "does nothing" | Raises in `timer`, `process.run` and `palette` callbacks log only at debug level. Run `mantle -vv`, or wrap the body in `pcall` and `log.warn` the error |
| Two modules declare the same action | Raises. Pick unique names |
| `store.key:get()` right after `store:set` | Still the old value. The signal updates on the next push |
| `store.key` is `nil` at startup | Every key reads `nil` until `mantle.storage` pushes, even with defaults. Handle `nil` in every map |
| Storing a key named `set` | `store.set` is the method, so that key is unreadable |
| Invalid `stop_signal` | The Supervisor refuses the declaration with a warning, and `start` then does nothing. Use a name from the list above |
| `dofile` / `loadfile` to read a data file | They block the render thread on file I/O. Use `persistent_table`, or `process.run("cat", { path }, ...)` |

Source: [process](../../renderer/src/lua/process.rs), [spawn and reap](../../supervisor/src/process/mod.rs),
[process registry](../../supervisor/src/process/registry.rs), [session process](../../renderer/src/lua/session_process.rs),
[processes controller](../../supervisor/src/capabilities/processes/controller.rs), [store](../../renderer/src/lua/store.rs),
[storage controller](../../supervisor/src/capabilities/storage/controller.rs), [timer](../../renderer/src/lua/timer.rs),
[action](../../renderer/src/lua/action.rs), [json](../../renderer/src/lua/json.rs), [log](../../renderer/src/lua/log.rs),
[fuzzy](../../renderer/src/lua/fuzzy.rs), [palette](../../renderer/src/lua/palette.rs), [fonts](../../renderer/src/lua/fonts.rs).

See also: [runtime](runtime.md) (reloads, budgets, logging), [signals](signals.md) (`state`,
`delay`), [capabilities](capabilities.md) (`mantle.processes`, `mantle.storage`),
[cli](cli.md) (`mantle call`, `mantle log`).
