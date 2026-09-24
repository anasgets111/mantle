# Runtime

What runs a config: the Lua VM and its libraries, how `require` finds modules, what a reload
re-runs and what it keeps, and the limits the engine enforces. Read it before splitting a config
into modules, when a reload does something unexpected, or when a log line names a budget.

Only `shell.lua` is required; how you split the rest is up to you. One possible layout puts the
bar in `bar.lua` and its clock in `widgets/clock.lua`:

```lua,fragment
-- shell.lua
local bar = require("bar")
return { bar }
```

```lua,fragment
-- bar.lua
local clock = require("widgets.clock") -- widgets/clock.lua

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    background = "#1e1e2eff",
    child = row { padding = 8, children = { clock } },
}
```

```lua
-- widgets/clock.lua
return text {
    content = mantle.system:map(function(system)
        return system and os.date("%H:%M", system.time) or "" -- nil before the first push
    end),
    foreground = "#cdd6f4ff",
}
```

Saving any of the three re-runs `shell.lua` in place, and the edited module is loaded fresh.

## The VM

Mantle runs as two processes ([glossary](../glossary.md)). The **Supervisor** is the `mantle`
process: it owns the backends, watches the config and restarts the other one. The **Renderer**
holds the Lua VM, the scene and the Wayland connection, all on one thread. There is one Lua 5.4 VM
per [generation](#evaluation-reload-and-generations). Anything that blocks it freezes every surface
on every monitor, so the blocking parts of the standard library are removed (ADR-0048).

| Library | Available | Missing |
| :--- | :--- | :--- |
| Base | `assert`, `collectgarbage`, `dofile`, `error`, `getmetatable`, `ipairs`, `load`, `loadfile`, `next`, `pairs`, `pcall`, `print`, `rawequal`, `rawget`, `rawlen`, `rawset`, `require`, `select`, `setmetatable`, `tonumber`, `tostring`, `type`, `warn`, `xpcall`, `_G`, `_VERSION` (`"Lua 5.4"`) | None. `warn` is accepted but prints nothing, even after `warn("@on")` |
| `coroutine` | `close`, `create`, `isyieldable`, `resume`, `running`, `status`, `wrap`, `yield` | None |
| `string` | `byte`, `char`, `dump`, `find`, `format`, `gmatch`, `gsub`, `len`, `lower`, `match`, `pack`, `packsize`, `rep`, `reverse`, `sub`, `unpack`, `upper`. Strings have the usual `("x"):upper()` metatable | None |
| `table` | `concat`, `insert`, `move`, `pack`, `remove`, `sort`, `unpack` | None |
| `math` | `abs`, `acos`, `asin`, `atan`, `ceil`, `cos`, `deg`, `exp`, `floor`, `fmod`, `huge`, `log`, `max`, `maxinteger`, `min`, `mininteger`, `modf`, `pi`, `rad`, `random`, `randomseed`, `sin`, `sqrt`, `tan`, `tointeger`, `type`, `ult`, plus the 5.3 compatibility functions `atan2`, `cosh`, `frexp`, `ldexp`, `log10`, `pow`, `sinh`, `tanh` | None |
| `utf8` | `char`, `charpattern`, `codepoint`, `codes`, `len`, `offset` | None |
| `package` | `config`, `cpath` (unused), `loaded`, `path` (the config directory only), `preload`, `searchers`, `searchpath` | `loadlib` exists but raises. C modules never load |
| `os` | `clock`, `date`, `getenv`, `time`. `require("os")` returns the same four | `difftime`, `execute`, `exit`, `remove`, `rename`, `setlocale`, `tmpname` |
| `io` | Nothing | The whole library. `require("io")` fails too |
| `debug` | Nothing | The whole library. `debug.traceback` included |
| FFI, native modules | Nothing | All |

`load` accepts text and precompiled chunks. `dofile` and `loadfile` remain and read any path, but
they are synchronous file I/O on the render thread. To read a file, use
[`process.run`](processes.md#processrun) or [`persistent_table`](scripting.md#persistent_table). To
run a program, use `process.run`. For `os.difftime(a, b)`, write `a - b`.

The engine adds these globals. Every module sees the same ones.

| Global | Kind | Owning page |
| :--- | :--- | :--- |
| `panel`, `window`, `popup`, `lock` | Surface constructors | [surfaces](../surfaces/index.md) |
| `rect`, `row`, `column`, `text`, `icon`, `image`, `capture`, `shader`, `button`, `list`, `textfield` | Node constructors | [nodes](../nodes/index.md) |
| `state`, `computed`, `delay`, `pulse`, `geometry` | Signal constructors | [signals](signals.md) |
| `hover`, `hover_rect`, `scroll` | Input signals | [input](input.md) |
| `mantle` | Capabilities and renderer members | [capabilities](../capabilities/index.md) |
| `process`, `session_process` | Processes | [processes](processes.md) |
| `persistent_table`, `timer`, `action`, `json`, `log`, `fuzzy`, `palette`, `fonts` | Scripting | [scripting](scripting.md) |

## Modules and require

The config is a directory, not a file. `shell.lua` is the entry, and `require` resolves module
names inside that directory only.

| Rule | Detail |
| :--- | :--- |
| Search path | `<config>/?.lua;<config>/?/init.lua`, nothing else. No system Lua paths, no `./` |
| Names | Dots are directories: `require("widgets.clock")` loads `widgets/clock.lua`. `require("widgets")` also finds `widgets/init.lua` |
| Cache | `package.loaded` behaves as usual within one evaluation. Every reload drops the config's own modules first, so an edited module is re-read. Standard modules stay |
| Second value | Lua 5.4's `require` returns the module and its file path. In the last position of a table constructor both land in the table |
| Symlinks | A symlinked subdirectory is followed, both by `require` and by the reload watcher |

Bind every module to a local before listing it, or the file path becomes a surface:

<!-- no-check: two alternative returns in one block do not parse -->
```lua,no-check
return { require("bar") }        -- { bar, "/home/me/.config/mantle/bar.lua" }: fails
local bar = require("bar")
return { bar }                   -- works
```

The failure reads `surface 2 is a string, not a node`, followed by this hint. Parentheses,
`(require("bar"))`, also truncate to one value.

## Evaluation, reload and generations

`shell.lua` returns a surface (`panel`, `window`, `popup` or `lock`), an array of them, `{}`, or
nothing. Running it top to bottom is one **evaluation**. The engine runs it at startup and again
on every reload.

| Term | Meaning |
| :--- | :--- |
| Evaluation | One run of `shell.lua` and whatever it `require`s. Its top level has no CPU budget |
| Reload | An evaluation in the same VM, triggered by a saved file or an output change, then one apply to the live scene |
| Apply | The new surface list is reconciled with the scene on screen. A surface whose [fingerprint](../surfaces/index.md) changed is rebuilt; everything else updates in place |
| Generation | One Renderer process and its VM. A reload never starts a new one. Only a crash does, when the Supervisor respawns the Renderer |

What triggers a reload:

| Event | Reloads? |
| :--- | :--- |
| A `.lua` or `.frag` file anywhere under the config directory is written, created, renamed in or deleted | Yes, 200 ms after the last event of a burst |
| A save with the same bytes as the last one the watcher saw for that file | No |
| Any other extension (`.json`, images, editor swap files) | No |
| A new subdirectory | Watched from then on, and walked if it already has files |
| An output is added, removed or reconfigured (`mantle.screens` changes) | Yes, at once, by the Renderer itself |
| The whole directory disappears (a `git checkout`) | Watched again once it is back, polled every second |

The watcher follows the config path resolved at startup. Retargeting a symlink later changes
nothing until a restart.

How each failure ends:

| Failure | Result |
| :--- | :--- |
| Startup evaluation raises | No scene. Surfaces paint nothing. `mantle.rescue` is set as below. The next successful reload brings the shell up |
| Startup evaluation succeeds but the scene rejects it | No scene. `mantle.rescue` is set |
| Reload evaluation raises (syntax error, runtime error, bad top-level return) | The previous scene stays on screen. [`mantle.rescue`](../capabilities/index.md) becomes `{ is_rescue = true, error_log = "<the error>" }`. The error is logged |
| Reload evaluates but the scene rejects it (bad property value, a map over budget) | The previous scene stays. `mantle.rescue` is set. The error is logged |
| A live update fails later (a pushed value breaks a map) | The previous scene stays. `mantle.rescue` is set until a pass applies. Warning logged |
| The session lock is refused, or the compositor ends it | `mantle.rescue` becomes `{ is_rescue = true, error_log = "<the reason>" }`. The error is logged ([lock](../surfaces/lock.md)) |
| A reload would recreate the lock surface while locked | Refused with a warning and `mantle.rescue`; save again after unlocking |
| The Renderer crashes | The Supervisor starts a new generation. After three crashes within 60 s, it waits 30 s before the next respawn |
| The compositor goes away | The Renderer exits with code 71 and the Supervisor shuts down instead of respawning |

A failed reload keeps only the scene. The actions, timers, `on_change` handlers and idle
thresholds registered by the evaluation that drew it are already gone (see the next table). A failed evaluation leaves none registered; a failed apply
keeps the new evaluation's actions, handlers and thresholds but no timers. Fix and save to get
them back.

`mantle.rescue` clears when a reload applies. A rescue from a failed live update or startup apply
also clears when a later pass over the same scene applies. A config can draw its own error banner:

```lua
local rescue = mantle.rescue

return panel {
    id = "rescue",
    layer = "Overlay",
    anchor = { bottom = true, left = true, right = true },
    visible = rescue:map(function(state) return state ~= nil and state.is_rescue end),
    background = "#f38ba8ff",
    padding = 8,
    child = text {
        content = rescue:map(function(state) return state and state.error_log or "" end),
        foreground = "#11111bff",
    },
}
```

## What survives a reload

| Thing | In-place reload | New generation (crash) | Shell stops |
| :--- | :--- | :--- | :--- |
| `state`, `hover`, `hover_rect`, `scroll`, `geometry` values (by name) | Kept. A changed scalar `state` seed re-seeds ([named state](signals.md#named-state)) | Lost | Lost |
| Plain Lua globals the config assigns | Kept: same VM | Lost | Lost |
| Config modules in `package.loaded` | Dropped, re-required | Lost | Lost |
| Derived signals (`:map`, `computed`, `delay`, `pulse`) | Rebuilt. A pending `delay` or open `pulse` resets | Rebuilt | Lost |
| `persistent_table` (by file) | Same table | Same file, new table | On disk |
| `session_process` (by name) | Keeps running, same table | Keeps running (the Supervisor holds it) | Stopped |
| `process.run` child | Keeps running. Its callbacks still fire, into the old evaluation's closures | Killed with its process group | Killed |
| `process.detach` program | Unaffected | Unaffected | Unaffected |
| `timer` | Cleared. The new evaluation's timers start when its result is applied | Cleared | Gone |
| `action`, `mantle.<cap>:on_change` | Cleared, re-registered by the new evaluation | Cleared | Gone |
| `mantle.idle` thresholds | Cleared, re-registered | Cleared | Gone |
| `fonts { ... }` chain | Not re-read | Re-read | Gone |
| Capability state (`mantle.<cap>`) | Unchanged | Replayed from the Supervisor's last snapshot | Gone |

Top-level side effects run again on every reload. A top-level `timer` chain is restarted, not
doubled, because the old timers are cleared. A long-running `process.run` started at the top level
is doubled: the old child keeps running beside the new one. Declare such a program with
`session_process` instead ([processes](processes.md#session_process)).

## Limits and budgets

Config Lua and the renderer share one thread, so the engine bounds how long config code can hold
it.

| Limit | Value | Applies to | When exceeded |
| :--- | :--- | :--- | :--- |
| CPU budget | 5 ms of thread CPU time | Each `:map` and `computed` recompute, each `delay`/`pulse` read, each `on_change` handler, `action` handler and `timer` callback. Nested reads share the outermost deadline | The call raises `exceeded the 5ms CPU budget for one evaluation`. `pcall` inside the callback does not hide it |
| Signal nesting | 32 levels | Signal reads nested inside other signal reads (a `map` of a `map` of ..., a computed reading itself) | Raises `signal nesting exceeded its maximum depth of 32 levels` |
| Layout pass | 2 s | One whole pass over the scene, including list `itemfn`s and function `child` builders | The pass fails and the previous scene stays |
| Tree depth | 64 levels | Nested nodes in one surface | The pass fails |
| Scalar values | Numbers finite, integers within ±(2^53 − 1), strings at most 64 KiB | `state` seeds, `:set()`, `mantle set`, and number or string node properties. Tables are not checked | `state` and `:set` raise. `mantle set` is refused: it exits 1 and logs a warning. A node property fails the pass |
| Numeric properties | `[0, 8192]` logical px for most sizes. `[-8192, 8192]` for `translate`, `rotate`, shader `progress`, shadow offset and spread. `opacity` and `origin` `[0, 1]`, `scale` `[0, 64]`, `font_size` `[1, 8192]`. `margin`, `padding`, `spacing` and icon `size` are unbounded (a tween still clamps them) | Node and surface properties ([nodes](../nodes/index.md)) | The pass fails, naming the property |
| Array length | 10,000 | `children` of one node, items of one `list` (`source`, and `limit` is clamped to it), runs in one `text` `content` | The pass fails |
| `delay`, `pulse` duration | `[1, 60000]` ms | `delay(signal, ms)`, `pulse(signal, ms)` ([signals](signals.md)) | Raises at the call |
| `timer` delay | `[1, 86400000]` ms (one day) | `timer(ms, fn)` ([scripting](scripting.md#timer)) | Raises at the call |
| Action answer | 1 MiB of JSON | What an `action` handler returns | The `mantle call` fails |
| `mantle call` wait | 5 s | The CLI waiting for an answer | The CLI gives up. The handler may still have run |
| Process output line | 64 KiB | One line a `process.run` child writes | The line arrives cut; its tail is dropped |
| Reload debounce | 200 ms after the last file event | The watcher | A burst of saves reloads once |
| Respawn brake | 3 Renderer deaths within 60 s | The Supervisor | The next respawn waits 30 s |

Not budgeted: an evaluation's top level, input handlers (`on_click`, `on_drag`, `on_wheel`,
`on_hover`, `on_link`, textfield callbacks, `on_close`, `on_dismiss`), `process.run` callbacks,
`palette` callbacks and idle callbacks. A slow one stalls every surface until it returns.

Keep maps cheap. Build lookup tables once at the top level, which has no budget, and do only an
index and a format inside the map:

```lua
-- Evaluation has no CPU budget: build lookup tables here, once.
local levels = { "empty", "low", "half", "high", "full" }

-- The map runs on every push, under 5 ms: one nil check, one index, one format.
local battery_label = mantle.battery:map(function(battery)
    if not battery or not battery.present then
        return "" -- nil until the first push; no battery on a desktop
    end
    local level = levels[math.min(#levels, battery.percent // 20 + 1)]
    return string.format("%s %d%%", level, battery.percent)
end)
```

Work that is slow by nature (parsing a large file, searching many entries) belongs in a program
started with `process.run`, whose output callback sets a `state`.

## Output and logging

| Call | Goes to |
| :--- | :--- |
| `print(...)` | The Renderer's stdout, unstamped |
| `log.error/warn/info/debug(...)` | Stamped lines under the `config` subsystem, printed at every verbosity ([log](scripting.md#log)) |
| A raise from any callback: input handlers, `on_close`, `on_dismiss`, `on_change`, `timer`, `process.run`, `palette` or idle | A warning |
| A `process.run` or `process.detach` that cannot spawn, a failing `mantle call` | A warning |
| An icon name no theme has, an image that does not decode | A warning, once per name |

Both streams land in the shell's log file. Read it with [`mantle log`](cli.md). A terminal that
started `mantle` in the foreground also gets a copy.

## How do I…

**…find out why a reload did nothing?** Work down this list:

| Step | Command | Tells you |
| :--- | :--- | :--- |
| 1 | `mantle check` | Syntax and top-level errors, with file and line. Node and layout errors as laid out with every capability `nil` ([what check covers](cli.md#what-check-covers)) |
| 2 | `mantle log` | `shell.lua re-evaluation failed` (evaluation error) or `the re-evaluated config failed to apply` (layout error, previous scene kept), and errors raised in callbacks |
| 3 | Draw `mantle.rescue` | The evaluation or apply error on screen, as in the [banner above](#evaluation-reload-and-generations) |

**…split a config into files?** Put modules beside `shell.lua` and bind each `require` to a local,
as in the [example at the top](#runtime).

**…share values between modules?** A module runs once per evaluation, and every later `require` of
it returns the same table. For a value that changes, use a named `state`: the same name gives the
same signal in any module.

```lua
-- lib/palette.lua: every module that requires it in one evaluation gets this same table.
return {
    accent = "#89b4faff",
    surface = "#1e1e2eff",
    dnd = state("dnd", false), -- named state: one signal per name, from any module
}
```

```lua,fragment
-- shell.lua
local palette = require("lib.palette")

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    background = palette.surface,
    child = text {
        content = palette.dnd:map(function(on) return on and "silent" or "" end),
        foreground = palette.accent,
    },
}
```

**…run something once, not on every reload?** Globals survive a reload, so a global guard runs
once per Renderer start:

```lua
-- A global lives in the VM: it survives reloads and resets when the Renderer restarts.
if not started_at then
    started_at = os.time()
    log.info("renderer started")
end

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    child = text { content = "up since " .. os.date("%H:%M", started_at) },
}
```

Timers and actions cannot be guarded this way, because every reload clears them. Declare them at
the top level every time.

**…keep a program running across reloads?** Declare it with
[`session_process`](processes.md#session_process). A top-level `process.run` starts a second copy
on every reload.

**…do heavy work without blowing the 5 ms budget?** Build tables at the top level and keep maps to
an index and a format ([example](#limits-and-budgets)). Move anything slower into a program run with
`process.run`.

**…name a file shipped beside `shell.lua`?** `mantle.config_dir .. "/shaders/wave.frag"`.
`os.getenv("HOME")` and the other variables of the shell's environment work too.

**…guard code that needs a newer engine?** Compare `mantle.version.major`, `.minor` and `.patch`
([renderer members](../capabilities/index.md)).

**…see what `print` wrote?** `mantle log`, or `mantle check`, which prints it above its report
when the config evaluates.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `return { require("a"), require("b") }` fails with `surface 3 is a string` | Bind each module to a local first |
| `require("lib.json")` from a luarocks install is not found | Only the config directory is searched. Copy the pure-Lua module into it |
| A map raises `exceeded the 5ms CPU budget` | Move the heavy work to the top level or to `process.run`. The map should only index and format |
| `dofile("/big/file")` stutters every frame it runs | Read files through `process.run` or `persistent_table` |
| A global counter keeps growing across reloads | Globals live in the VM, and a reload reuses the VM. Use `local`, or `state` when it should persist on purpose |
| After a broken save, `mantle call` says no action exists | A failed reload clears actions, timers and handlers. Fix the error and save again |
| A config edit to `fonts { ... }` does nothing | The font chain is read when the Renderer starts. Restart the shell |
| Saving a `.json` or an image beside `shell.lua` does not reload | Only `.lua` and `.frag` changes trigger a reload |

See also: [cli](cli.md) · [signals](signals.md) · [processes](processes.md) · [scripting](scripting.md) ·
[capabilities](../capabilities/index.md) · [surfaces](../surfaces/index.md) · [glossary](../glossary.md) for
generation, Supervisor and Renderer.

Source: [VM setup and `require`](../../renderer/src/lua/mod.rs),
[reload](../../renderer/src/socket/client/mod.rs), [apply](../../renderer/src/socket/client/resolve.rs),
[watcher](../../supervisor/src/watcher.rs), [budget](../../renderer/src/lua/signal/budget.rs),
[scalar checks](../../renderer/src/lua/marshal.rs), [property ranges](../../renderer/src/layout/node/style/mod.rs),
[respawn](../../supervisor/src/supervisor.rs).
