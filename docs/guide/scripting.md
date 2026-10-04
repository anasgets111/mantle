# Scripting

The imperative globals that are neither UI nor processes: keep settings on disk, schedule work,
answer keybinds, and a few utilities. Reach for them from event handlers (`on_click`,
`on_change`, callbacks) or a module's top level. Running other programs is on
[processes](processes.md); everything that renders goes through [signals](signals.md) and
[nodes](../nodes/index.md) instead.

Terms used below (*Supervisor*, *Renderer*, *generation*, *push*) are in the
[glossary](../glossary.md).

## Which one do I use

| To | Use |
| :--- | :--- |
| Run a command, launch an app, or keep a program running | [processes](processes.md) |
| Remember a setting across restarts | [`persistent_table`](#persistent_table) |
| Do something later, or retry | [`timer`](#timer) |
| Repeat every N ms | [`interval`](#interval) |
| Run code from a keybind or script | [`action`](#action) + `mantle call` |
| Parse or write JSON | [`json.decode`](#jsondecode), [`json.encode`](#jsonencode) |
| Write to the shell's log | [`log.*`](#log) |
| Rank search results | [`fuzzy`](#fuzzy) |
| Pull colours out of a wallpaper | [`palette.quantize`](#palettequantize) |
| Build Material 3 colours from a seed or a wallpaper | [`palette.scheme`](#palettescheme), [`palette.score`](#palettescore) |
| Set the font fallback chain | [`fonts`](#fonts) |

What each one keeps across a reload, a crash and a restart: [runtime](runtime.md#what-survives-a-reload).

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
    id = "clock", layer = "top", anchor = { top = true },
    child = rect {
        on_click = function() settings:set("clock_24h", not clock_24h:get()) end,
        children = {
            text { content = clock_24h:map(function(on) return on and "24h" or "12h" end) },
        },
    },
}
```

| Part | Contract |
| :--- | :--- |
| Signature | `persistent_table { path, name, defaults? }` → store. Another key raises |
| `path` | Absolute directory; relative raises. Created if missing. Build it from `os.getenv` or `mantle.config_dir` |
| `name` | One file name, no `/`; empty raises |
| `defaults` | Fills missing top-level keys; stored values win. Nested tables are one value and do not merge. Re-sent every evaluation, so a new default lands on reload |
| `store.<key>` | Signal for that key: `nil` before `mantle.storage` first pushes, then the stored value |
| `store:set(key, value)` | Writes one key; `nil` deletes it until the next reload re-fills its default. The signal updates on the next push, not inside `set` |
| Identity | One table per file: another call with the same `path`/`name` returns it, across reloads too |
| Raw state | `mantle.storage` ([capabilities](../capabilities/storage.md)) |

On disk:

| Behaviour | Detail |
| :--- | :--- |
| Save | 1 s after the last write to that file, as pretty JSON through a temporary file and rename. A write still waiting at shell exit is lost |
| First run | A missing file is created from `defaults` |
| Outside edits | Watched with inotify. Another writer's version replaces the in-memory one whole, dropping unsaved writes |
| Broken file | Not a JSON object, or unparsable: the last values stay, a warning is logged once, and nothing saves over it until it parses |

## timer

Runs a function once after a delay. To repeat at a fixed rate, use [`interval`](#interval).

```lua
local status = state("status", "checking")
local BACKOFF_MS = { 2000, 4000, 8000 }

local function check(attempt)
    attempt = attempt or 1
    process.run("ping", { "-c", "1", "-W", "2", "1.1.1.1" }, function() end, function(code)
        if code == 0 then
            status:set("online")
        elseif BACKOFF_MS[attempt] then
            status:set("retrying")
            timer(BACKOFF_MS[attempt], function() check(attempt + 1) end)
        else
            status:set("offline")
        end
    end)
end
check()

-- Re-arming from the callback repeats on a varying delay, here aligned to the minute.
local clock = state("clock", "")
local function tick()
    clock:set(os.date("%H:%M"))
    timer(60000 - (os.time() % 60) * 1000, tick)
end
tick()
```

| Part | Contract |
| :--- | :--- |
| Signature | `timer(ms, fn)` → handle |
| `ms` | `1` to `86400000` (one day), fractions allowed, monotonic clock; outside raises |
| `fn` | Called with no arguments under the [Lua CPU budget](runtime.md#limits-and-budgets). A raise or blown budget is logged as a warning |
| Handle | `handle:cancel()` disarms it. A no-op once fired or cancelled. Dropping the handle does not disarm |
| Order | Timers due at the same moment fire in the order they were armed; one may cancel another in the same batch |
| Lifetime | Every evaluation clears all timers, including chains armed from callbacks. Timers armed during an evaluation start only once its result is applied |

For a delayed or blinking *value*, `delay` and `pulse` ([signals](signals.md)) need no callback.

### When to use a timer

Each time a timer sets a signal, every `:map` reading it runs again. Once a minute is free; every
33 ms is 30 times a second, forever. Arm a timer as often as the data changes, never per frame.

| To | Use |
| :--- | :--- |
| Retry with backoff, or act once after a delay | `timer` |
| Update a clock | `timer`, re-armed for the next minute, as above |
| Poll something with no capability and no output to follow | `interval` at the slowest rate that stays useful ([poll a command](processes.md#poll-a-command-every-n-seconds)) |
| React to a program's output | Follow it with `process.run` instead of polling ([follow](processes.md#follow-a-long-running-commands-output)) |
| Hide something a few seconds after the last change | `timer`, cancelling the previous handle on each change; or `delay` |
| Show a value late, or blink it | `delay` or `pulse` |
| Move, spin, pulse or morph something | `animate` ([Lua cost](animation.md#lua-cost)) |

## interval

`interval(ms, fn)` → handle. Runs `fn` every `ms` until `handle:cancel()`, with `timer`'s range,
budget, order and lifetime. Turns stay on the original schedule; one that comes late runs once and
the next is `ms` after it, so missed runs never queue. A raise is logged and the next run still
happens. Cancelling from inside `fn` stops it.

```lua
local load = state("load", "")
interval(5000, function()
    process.run("cat", { "/proc/loadavg" }, function(line) load:set(line:match("^%S+")) end, function() end)
end)
```

## action

Names a function that `mantle call <name> [args...]` runs, usually from a compositor keybind.
Write a `state` when the shell should look different; call an action when it should do
something.

```lua
-- Keybind: mantle call volume.up 10
action("volume.up", function(step)
    local audio = mantle.audio:get()
    if not audio or not audio.volume then
        return "no output device"
    end
    local volume = math.min(150, audio.volume + (tonumber(step) or 5))
    mantle.audio:set_volume(volume)
    return string.format("%d%%", math.floor(volume + 0.5))
end)
```

| Part | Contract |
| :--- | :--- |
| Signature | `action(name, fn)` → nothing |
| `name` | Any non-empty string; nothing splits on `.`. Empty, or declared twice in one evaluation, raises |
| Arguments, return, failure | As [values and arguments](cli.md#values-and-arguments): each argument JSON-decoded when it parses, the return printed bare or as JSON. A return that is not convertible to JSON fails the call |
| Limits | [Lua CPU budget](runtime.md#limits-and-budgets) |
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

## json.encode

`json.encode(value)` → string: compact JSON, object keys sorted. It raises on a value JSON cannot
hold, since that is a config mistake rather than bad input.

| Lua | JSON |
| :--- | :--- |
| Table whose keys are all positive integers, `{}` included | Array; a hole inside it is `null`. More than half holes raises |
| Table with string keys | Object |
| Table mixing both, or with other keys | Raises |
| `nil` | `null` |
| Integer, float | `3`, `3.0`: `json.decode` reads each back as it was |
| Function, userdata, a table that contains itself, NaN, infinity, nesting past 127, over 2^20 values, a decoded array given a named key | Raises |

```lua
local body = json.encode({ title = "Backup", done = true }) -- {"done":true,"title":"Backup"}
process.detach("curl", { "-s", "-H", "Content-Type: application/json", "-d", body, "http://localhost:8080/events" })
```

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
    id = "launcher", layer = "overlay", keyboard_interactivity = "on_demand",
    child = column {
        width = 320, padding = 12, spacing = 6, background = "#1E1E2E",
        children = {
            textfield { autofocus = true, placeholder = "Search", on_change = function(text) query:set(text) end },
            list { source = results, itemfn = function(name) return text { content = name } end },
        },
    },
}
```

| Part | Contract |
| :--- | :--- |
| Signature | `fuzzy(haystack, needle)` → `score, start` |
| Match | Integer `score` (higher is better) and `start`, the 0-based byte offset of the needle's first character at its earliest in-order hit (the best-scoring one for a one-character needle). For tiebreaks, not highlighting |
| No match | `nil, nil`, also for non-UTF-8 input |
| Empty needle | `0, 0` |
| Case | Smart: an all-lowercase needle ignores case; one uppercase character makes the whole comparison exact. Don't lowercase the query |
| Non-ASCII | A greedy scorer whose scores do not compare with the ASCII path's |

Tie order is the caller's: `table.sort` is unstable, so end the comparator on a unique key.

## palette.quantize

Median-cut dominant colours of an image, computed on a background thread. Put a raster image at
`wallpaper.png` in your config directory, or change the path below to an existing image. The book
checks this file-dependent snippet's syntax; the callback runs only in a live Renderer.

```lua,fragment
local swatches = state("swatches", {})

palette.quantize(mantle.config_dir .. "/wallpaper.png", { depth = 3 }, function(found)
    swatches:set(found or {})
end)

return panel {
    id = "palette", layer = "top", anchor = { bottom = true },
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

| Part | Contract |
| :--- | :--- |
| Signature | `palette.quantize(path, opts?, on_done)` → handle |
| `path` | Local raster image; no SVG or URL |
| `opts.depth` | `0` to `8`, default `3`: up to `2^depth` colours, fewer when the image has fewer. Out of range raises |
| `opts` | Only `depth` and `rescale`; another key raises |
| `opts.rescale` | Longest edge in px before counting, default `128`; `0` is full size. Negative raises. A cached freedesktop thumbnail that covers it is used instead of decoding |
| `on_done(swatches)` | `{ color = "#RRGGBB", share = 0..1 }` entries, most common first. `share` counts only non-transparent pixels. `nil` on failure, with a logged warning. Runs outside the CPU budget; a raise is logged as a warning |
| Handle | `handle:cancel()` drops the callback; the work still finishes |

## palette.scheme

Material 3 colour roles from one seed colour, built by Material Color Utilities at spec 2021 on
the phone platform. Synchronous: one scheme takes about 0.5 ms.

```lua
local roles = palette.scheme("#6750A4", { dark = true, variant = "tonal_spot" })

return rect {
    background = roles.surface_container, padding = 12,
    children = { text { content = "Material", foreground = roles.on_surface } },
}
```

| Part | Contract |
| :--- | :--- |
| Signature | `palette.scheme(seed, opts?)` → roles |
| `seed` | `"#RRGGBB"`; anything else raises |
| `opts.dark` | Boolean, default `false` |
| `opts.variant` | `"tonal_spot"` (default), `"vibrant"`, `"expressive"`, `"fidelity"`, `"content"`, `"neutral"`, `"monochrome"`, `"rainbow"` or `"fruit_salad"`. Another raises |
| `opts.contrast` | `-1` to `1`, default `0`; `1` is high contrast. Out of range raises |
| `opts` | Only those three keys; another raises |
| Roles | A table of 49 `"#RRGGBB"` strings keyed by role in snake_case: `primary`, `on_primary_container`, `surface_container_highest`, … (`PaletteScheme` in the stubs) |

## palette.score

Ranks [`palette.quantize`](#palettequantize) swatches as Material 3 picks a wallpaper's seed:
chromatic colours that cover more of the image, spread across hues. Quantize at depth 6 or 7, so
Score has enough colours to choose from.

```lua,fragment
local roles = state("roles", palette.scheme("#6750A4"))

palette.quantize(mantle.config_dir .. "/wallpaper.png", { depth = 7 }, function(found)
    if found then roles:set(palette.scheme(palette.score(found)[1], { dark = true })) end
end)
```

| Part | Contract |
| :--- | :--- |
| Signature | `palette.score(swatches)` → `{ "#RRGGBB", ... }` |
| `swatches` | `{ color = "#RRGGBB", share }` entries; only the ratios of the `share`s count. A bad colour or a `share` not above `0` raises |
| Result | Up to 4 colours, best first, at least 15° of hue apart. Colours with chroma under 5 or hues under 1% of the image drop out; with none left it is `{ "#4285F4" }` |

## fonts

`fonts { family, ... }` sets the fallback chain every `text` node uses. Each glyph takes the
first family that covers it.

```lua
fonts { "Inter", "Symbols Nerd Font", "Noto Color Emoji" }
```

| Part | Contract |
| :--- | :--- |
| Argument | Dense array of family-name strings. A hole, a named key or a non-string raises |
| Resolution | Through `fc-match`. The first family that resolves is the primary and also loads its bold and italic faces; a family with no install is skipped (logged at `-vvv`) |
| Default | Without a call: `sans-serif`, `Noto Sans CJK JP`, `Noto Color Emoji` |
| Per node | A `text` node's `font` goes in front of the chain ([nodes](../nodes/text.md)) |
| Uncovered glyph | fontconfig is asked for any installed face that covers it |
| SVG `<text>` | Images, icons and file masks draw it with the loaded chain; declare a family it names ([image](../nodes/image.md)) |
| Lifetime | Read once at startup. Last call wins; an edit needs a shell restart |

## How do I…

| Task | Answer |
| :--- | :--- |
| Fetch JSON over HTTP | `curl` through [`process.run`](processes.md#processrun), then [`json.decode`](#jsondecode) the output |
| Poll a command every N seconds | [processes](processes.md#poll-a-command-every-n-seconds) |
| Search as you type | [fuzzy](#fuzzy); for a slow source, debounce the query with [`delay`](signals.md#delay-hold-a-value) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Callbacks from before a reload | `palette` callbacks run the old closures after a reload. Keep what they touch in named state |
| `timer` or `action` declared only inside a callback | Every evaluation clears both, so they vanish on the next save. Declare actions at the top level; start timer chains from the top level too |
| A `timer` re-armed every few milliseconds to move something | Each tick runs Lua and resolves again. Use `animate` ([Lua cost](animation.md#lua-cost)) |
| Two modules declare the same action | Raises. Pick unique names |
| `store.key:get()` right after `store:set` | Still the old value. The signal updates on the next push |
| `store.key` is `nil` at startup | Every key reads `nil` until `mantle.storage` pushes, even with defaults. Handle `nil` in every map |
| Storing a key named `set` | `store.set` is the method, so that key is unreadable |

See also: [processes](processes.md), [runtime](runtime.md) (reloads, budgets, logging),
[signals](signals.md) (`state`, `delay`), [storage capability](../capabilities/storage.md)
(`mantle.storage`), [cli](cli.md) (`mantle call`, `mantle log`).

Source: [store](../../renderer/src/lua/store.rs),
[storage controller](../../supervisor/src/capabilities/storage/controller.rs), [timer](../../renderer/src/lua/timer.rs),
[action](../../renderer/src/lua/action.rs), [json](../../renderer/src/lua/json.rs), [log](../../renderer/src/lua/log.rs),
[fuzzy](../../renderer/src/lua/fuzzy.rs), [palette](../../renderer/src/lua/palette.rs),
[scheme](../../renderer/src/lua/scheme.rs), [fonts](../../renderer/src/lua/fonts.rs).
