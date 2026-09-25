# Signals

A config builds its node tree once; signals change it afterwards. Any node or surface property can
hold a signal in place of a plain value. The engine reads the signal while it lays out, and a write
to it re-resolves only the surfaces that read it.

## The one rule

A property that holds a signal stays live. A property that holds a plain value, including
whatever `:get()` returned, keeps that value until the next reload.

```lua
local clock = mantle.system:map(function(system)
  if not system then return "--:--" end -- nil until the first push
  return os.date("%H:%M", system.time)
end)

return panel {
  id = "bar", layer = "Top", anchor = { top = true, left = true, right = true }, height = 28,
  child = row { spacing = 12, children = {
    text { content = clock },       -- live: follows every push
    text { content = clock:get() }, -- snapshot: "--:--" until the next reload
  } },
}
```

| Property value | Behaviour |
| :--- | :--- |
| `sig` | Read again once it is written |
| `sig:map(fn)` | Live: `fn` of `sig`'s current value |
| `sig:get()` | A plain value, taken when the config was evaluated |
| A table with a signal inside, e.g. `{ left = sig }` | Refused at layout. Derive the whole table: `sig:map(function(v) return { left = v } end)` |

A capability (`mantle.<name>`, see [capabilities](../capabilities/index.md)) is a signal too. It reads `nil`
until its first snapshot arrives (hydration), and in `mantle check`'s first pass, before a sample push.
Every function that reads a capability must handle `nil`. When a signal resolves to `nil`, its
property counts as absent and takes the property's default.

## Reference

| Expression | Returns | Contract |
| :--- | :--- | :--- |
| `sig:get()` | value | The current value, read once. `nil` before a capability's first push |
| `sig:map(fn)` | signal | `fn(value)`, run again on every read. Works on capabilities |
| `sig:set(value)` | nothing | State signals only; see [who writes each kind](#who-writes-each-kind) |
| `sig:reveal(index)` | nothing | `scroll` signals only; scrolls the `index`-th child into view ([input](input.md)) |
| `cap:on_change(fn)`, `cap:<action>(...)` | nothing | Capabilities only ([capabilities](../capabilities/index.md)) |
| `computed({ a, b, ... }, fn)` | signal | `fn(a_value, b_value, ...)`: the values in list order, not the signals. Each entry must be a signal or capability: a `nil`, another value or a named key raises, naming the entry |
| `state(name, initial)` | state signal | Writable [named state](#named-state), written with `:set(value)` |
| `delay(sig, ms)` | signal | `sig`'s value once a new value has held for `ms`, and the old value until then. A change that reverts sooner is dropped |
| `pulse(sig, ms)` | boolean signal | `true` for `ms` after `sig` changes, `false` otherwise. A change inside the window restarts it. Starts `false` |
| `geometry(name)` | rect signal | Bind it as a node's `geometry`. Layout writes that node's `{ x, y, width, height }` in surface coordinates. Zero until the first layout |

`delay` and `pulse` take `ms` in `[1, 60000]` rounded to whole milliseconds, and raise outside that
range. Both compare values with `==`, so a table value (every capability payload, for example)
counts as a new value on every push.

### Who writes each kind

Only `state` can be written from Lua. `:set` on any other kind raises an error that names the kind.
`:reveal` works only on a `scroll` signal.

| Kind | Made by | `:set` | `:reveal` | Written by |
| :--- | :--- | :---: | :---: | :--- |
| State | `state(name, initial)` | ✓ | | The config, `mantle set`, `mantle toggle` |
| Capability | `mantle.<name>` | | | The capability's snapshot pushes |
| Derived | `:map`, `computed`, `delay`, `pulse` | | | Nobody: recomputed on read |
| Stored | A `persistent_table` key ([scripting](scripting.md)) | | | The table's own `:set(key, value)` |
| Geometry | `geometry(name)` | | | Layout |
| Hover | `hover(name)`, `hover_rect(name)` ([input](input.md)) | | | The pointer |
| Scroll | `scroll(name)` ([input](input.md)) | | ✓ | The wheel and the layout clamp |

`:set` refuses the scalars outside the engine's [value limits](runtime.md#limits-and-budgets) and
leaves tables unchecked. It checks no types: `state("x", 1):set({})` succeeds, and only
LuaLS flags it. A `:set` of what the state already holds changes nothing: `1` over `1` is
skipped, `1.0` over `1` is a write. A fresh plain-data table (no metatable, only scalars and such
tables inside, 256 entries in all) equal entry for entry is skipped too; the same table written
again after changing it in place is a write.

### Errors

| Message starts with | Cause |
| :--- | :--- |
| `computed() dependency 2 is nil` / `is a table` | That `computed` list entry is not a signal or capability; `nil` is often a misspelled variable |
| `computed() dependencies: key` | The `computed` list has a named key; list the signals in `fn`'s order |
| `delay() takes a Signal` / `pulse() takes a Signal` | The first argument is not a signal or capability |
| `delay() hold must be within [1, 60000] ms` / `pulse() window must be within` | `ms` out of range, or rounds to 0 |
| `signal:set() is only valid on a state(name, initial) signal` | `:set` on a derived, capability, hover, scroll or geometry signal |
| `signal:set() refused its value at the marshalling boundary` | NaN, infinity, an integer past ±(2^53−1) or a string over 64 KiB |
| `state("name", ...) refused its initial value` | The same checks on `initial` |
| `signal:reveal() is only valid on a scroll(name) signal` / `takes a 1-based child index` | `:reveal` on another kind, or an index below 1 |
| `signal nesting exceeded its maximum depth of 32 levels` | A derived chain deeper than 32, or one that reads itself |
| `exceeded the 5ms CPU budget for one evaluation` | A map or computed body ran too long ([runtime](runtime.md)) |
| `a Signal resolved to another Signal` | A map returned a signal; return a plain value |
| `` `x` is a Signal handle, not a plain value `` | A signal in a structural property or inside a property table (see [gotchas](#gotchas)) |

## Derived signals

`:map` and `computed` run again once a signal they read is written, and their readers re-resolve
only when the result changed ([what a node reads again](#what-a-node-reads-again)): a scalar or a
plain-data table by value, anything holding a function, signal or metatable on every run. An
`HH:MM` label or a `{ { text = hour, bold = true } }` run list mapped from a per-second snapshot
re-resolves once a minute. Within one pass, a derived signal read by
several properties runs once. Keep their functions cheap and side-effect free: no `:set`, no
process, no action. They run under the CPU budget and nesting limit described in
[runtime](runtime.md). Side effects belong in `on_click`, a capability's
`on_change` ([capabilities](../capabilities/index.md)) or a `timer` ([scripting](scripting.md)).

A derived colour:

```lua
local battery_color = mantle.battery:map(function(battery)
  if not battery or not battery.present then return "#6c7086" end
  return battery.percent <= 20 and "#f38ba8" or "#a6e3a1"
end)

text { content = "●", foreground = battery_color }
```

Use `computed` to combine two sources, here a capability and a state that a click toggles:

```lua
local show_seconds = state("show_seconds", false)

local clock = computed({ mantle.system, show_seconds }, function(system, seconds)
  if not system then return "" end
  return os.date(seconds and "%H:%M:%S" or "%H:%M", system.time)
end)

button {
  on_click = function() show_seconds:set(not show_seconds:get()) end,
  children = { text { content = clock } },
}
```

A dropdown under a button is a `popup` bound to a state the click toggles: [dismissal](../surfaces/popup.md).

### delay: hold a value

`delay` answers the old value until the new one has held for `ms`. That makes it a trailing
debounce, and also a close-hold. OR-ing a signal with its delayed copy keeps a surface mapped for
`ms` after it closes, long enough for an exit fade to play. Hiding a surface or node plays no exit
animation of its own ([animation](animation.md)).

```lua
local open = state("menu_open", false)
local mapped = computed({ open, delay(open, 150) }, function(now, was) return now or was end)

local menu = popup {
  id = "menu", parent = "bar", anchor_rect = { x = 0, y = 0, width = 60, height = 28 },
  anchor = "Bottom", gravity = "Bottom",
  visible = mapped, -- stays mapped 150 ms after `open` goes false
  on_dismiss = function() open:set(false) end,
  child = column {
    padding = 8, background = "#1e1e2e",
    opacity = open:map(function(is_open) return is_open and 1 or 0 end), -- fades out while held
    animate = { opacity = { duration = 150, from = 0 } },
    children = { text { content = "Settings" } },
  },
}
```

### pulse: mark a change

`pulse` reports that a change just happened. Use it to fire a one-shot flash or a keyframe
animation, which a config cannot restart any other way ([animation](animation.md)):

```lua
local count = state("count", 0)
local flash = pulse(count, 300) -- true for 300 ms after each change

button {
  padding = 6,
  background = flash:map(function(on) return on and "#f9e2af" or "#313244" end),
  animate = { background = 300 },
  on_click = function() count:set(count:get() + 1) end,
  children = { text { content = count:map(tostring) } },
}
```

To fire on only one edge, combine the pulse with its source:
`computed({ pulse(plugged, 400), plugged }, function(fired, on) return fired and on end)`.

### geometry: read a node's laid-out rect

Layout writes the rect after it solves. A change triggers one follow-up pass over the surfaces
that read the rect, and never two passes in a row, so a binding that feeds its own measurement
cannot loop.

```lua
local track = geometry("track")

column { width = 200, children = {
  rect { geometry = track, width = "Fill", height = 4, background = "#45475a" },
  text { content = track:map(function(rect) return string.format("%d px wide", math.floor(rect.width)) end) },
} }
```

## Named state

`state(name, initial)` is the config's own writable value. Its identity is its name. Every call
with the same name returns the same signal, from any module and across reloads.

| Rule | Detail |
| :--- | :--- |
| Identity | One name, one signal. `hover`, `scroll` and `geometry` names are separate namespaces |
| Reload | Keeps its value across in-place reloads. Lost when the [Renderer](../glossary.md#processes) process is replaced (a crash respawn or a shell restart) |
| Changed seed | A scalar `initial` (nil, boolean, number, string) that differs from the last evaluation's re-seeds the value. `0` and `0.0` are equal. Two different scalar seeds for one name in one evaluation raise |
| Table seed | Never re-seeds: tables compare by identity, so a fresh table cannot count as a change |
| Types | Not checked at runtime; `initial` is the type LuaLS infers |
| CLI | `mantle set <name> <value>` and `mantle toggle <name> [value]` write it ([cli](cli.md)). A bare toggle needs a boolean. Toggling to the value it already holds restores `initial` |

Derived signals (`:map`, `computed`, `delay`, `pulse`) have no name. Each evaluation builds them
fresh, so a reload drops a pending `delay` and closes an open `pulse` window. See
[runtime](runtime.md) for everything else a reload keeps.

## How re-resolution works

While a surface instance (one surface on one output) resolves, the engine records every signal it
reads, including reads inside `:map` and `computed` bodies. A write marks the written signal
dirty, and the next pass re-resolves only the instances that read it.

| Event | Re-resolves |
| :--- | :--- |
| `:set`, a capability push, a hover or scroll change | Instances that read that signal in their last resolve |
| The same, under a `map` or `computed` | Instances that read it, once its result changed |
| A write to a signal no instance reads | Nothing |
| A `delay` coming due or a `pulse` window closing | Every instance |
| A `geometry` rect moving | One follow-up pass over the instances that read it |
| Any write while the session is locked | Every instance |
| A reload, or a re-resolve that failed | Every instance |

A node reads its `children` or `child` table once and keeps what it read while it holds that same
table. A node table or `children` array changed in place is not seen; a signal answering a new table
is.

### What a node reads again

Within a re-resolved surface, each node keeps the properties it resolved last time until a signal
that resolve read is written. A clock written every second resolves its one `text` again, not the
whole bar. A `list` keeps its items the same way ([when items rebuild](../nodes/list.md#when-items-rebuild)).

| Change | The node reads its properties again |
| :--- | :--- |
| A write to a signal bound to one of its properties, or one changing the result of a `map` or `computed` bound to one | ✓ |
| A write to its own `hover` or `scroll` slot | ✓ |
| A different table, function, signal or value in its declaration, as in a rebuilt `list` item | ✓ |
| A reload | ✓ |
| For a `panel` or `lock` root, a write to a signal its function `child` read. Every ✓ here runs that function again | ✓ |
| A write to anything else, even on the same surface | |

The engine sees signal reads only. A `map`, `computed`, `itemfn`, `key` or function `child` must
answer from its arguments and the signals it reads; anything else it reads is taken as it was at the
node's last resolve:

| Read inside the function | Kept until a signal it read changes | Instead |
| :--- | :--- | :--- |
| `os.time()`, `os.date()` with no time, `os.clock()`, `math.random()` | ✓ | `mantle.system:map(function(s) return s and os.date("%H:%M", s.time) or "" end)`, or a `state` a `timer` writes |
| A local or global changed without `:set` | ✓ | Keep it in a `state` |
| A file | ✓ | Read it in a `timer` and `:set` a `state` |
| A table changed in place, such as a `list`'s `source` | ✓ | `:set` the table again, or build a new one |
| A `delay` or `pulse` | | Nothing: its readers resolve on every pass while one is pending or open |

A `visible = false` node's subtree is frozen. Its children keep their nodes, ids, properties and
last geometry. None of their signals is read, no `list` item function runs and nothing re-lays
out until the node is shown again. Signals that only a hidden subtree reads therefore trigger
nothing.

## Switching views

`visible = false` keeps a subtree in the tree, frozen: right for a section shown and hidden in
place. For views that replace each other, bind the parent's `children` to a signal that returns
only the current view. The old view leaves the tree, playing its `animate.exit`, and the new one
builds fresh. Give each view its own `id`: [switching views with ids](../nodes/index.md#switching-views-with-ids).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a live clock | [The one rule](#the-one-rule) |
| Colour a node from a capability | [Derived signals](#derived-signals) |
| Derive one value from two capabilities | [Below](#derive-from-two-capabilities) |
| Debounce a search field | [Below](#debounce-a-search) |
| Open a dropdown under a button | [Dismissal](../surfaces/popup.md) |
| Keep a popup mapped while its exit plays | [delay](#delay-hold-a-value) |
| Flash a node when a value changes | [pulse](#pulse-mark-a-change) |
| Open or close UI from a compositor keybind | [Below](#drive-ui-from-a-keybind) |
| Switch tabs | [Switching views with ids](../nodes/index.md#switching-views-with-ids) |
| Size one node from another's layout | [geometry](#geometry-read-a-nodes-laid-out-rect) |
| Keep a toggle across shell restarts | Named state is lost with the Renderer; use `persistent_table` ([scripting](scripting.md)) |
| Run a side effect when a capability changes | `on_change` ([capabilities](../capabilities/index.md)), never a map |

### Derive from two capabilities

List every source in `computed`. Each one reads `nil` until its first push.

```lua
local status = computed({ mantle.network, mantle.audio }, function(network, audio)
  if not network or not audio then return "..." end
  local net = network.connected and "online" or "offline"
  local sound = audio.muted and "muted" or string.format("%d%%", math.floor((audio.volume or 0) * 100))
  return net .. " / " .. sound
end)

text { content = status }
```

### Debounce a search

The field writes every keystroke to a state. The filter reads a `delay` of that state, so the
filter runs once typing pauses for 250 ms.

```lua
local query = state("search_query", "")
local settled = delay(query, 250)

local results = settled:map(function(needle)
  local found = {}
  for _, name in ipairs({ "Firefox", "Files", "Terminal", "Settings" }) do
    if needle ~= "" and name:lower():find(needle:lower(), 1, true) then
      found[#found + 1] = text { id = name, content = name }
    end
  end
  return found
end)

column { width = 240, spacing = 4, children = {
  textfield { width = "Fill", height = 28, placeholder = "Search", on_change = function(text) query:set(text) end },
  column { spacing = 2, children = results },
} }
```

### Drive UI from a keybind

Bind the surface's `visible` to a named state. A compositor keybind that runs
`mantle toggle launcher_open` flips it, and `mantle set launcher_open false` closes it. Example and
compositor syntax: [cli](cli.md#cli). For a keybind that runs Lua code, use
[`action`](scripting.md#action).

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `content = sig:get()` never updates | Pass `sig` or `sig:map(...)`; `:get()` is a snapshot |
| A map errors with `attempt to index a nil value` at startup | Capabilities read `nil` before hydration and in `mantle check`'s first pass; return a fallback for `nil` |
| `visible = cap:map(function(c) return c and c.on end)` shows the node before hydration | `nil` means absent, and `visible` defaults to `true`; return `false` explicitly |
| `margin = { left = sig }` fails at layout: `` `margin.left` is a Signal handle `` | Signals inside a property table do not resolve. Derive the whole table with `:map` or `computed`; the error's `:get()` advice gives a snapshot |
| A map that returns a signal fails with `a Signal resolved to another Signal` | Resolution happens once; return a plain value, or combine the sources with `computed` |
| `layer`, `anchor`, `monitor`, `namespace`, `parent` or an `id` bound to a signal is refused | These are structural and take plain values only ([surfaces](../surfaces/index.md)) |
| A named state resets on every reload | Its scalar seed changed between evaluations. Keep it stable |
| `state("x", ...) is declared twice in this evaluation` | Two `state` calls give one name different seeds. Declare it in one module and require that |
| `delay(mantle.system, 2000)` never updates | Each push is a fresh table, so the hold restarts every second. Delay a scalar derived with `:map` |
| `pulse(cap, ms)` fires on every push | Table payloads are never `==`; pulse a mapped scalar |
| Hiding a view with `visible = false` keeps its whole subtree | Switch views through `children = sig:map(...)` |
| A `:set` inside a map or computed | Maps must be side-effect free; write state from `on_click`, `on_change` or a `timer` |
| A clock from `os.date()` alone stops updating | Nothing it read is a signal, so its node keeps the first answer. Derive it from `mantle.system`'s `time` ([what a node reads again](#what-a-node-reads-again)) |

See also: [runtime](runtime.md) (budgets, reload), [capabilities](../capabilities/index.md),
[input](input.md) (`hover`, `scroll`), [animation](animation.md), [cli](cli.md),
[glossary](../glossary.md) (generation, hydration).

Source: [signal core](../../renderer/src/lua/signal/mod.rs),
[globals](../../renderer/src/lua/signal/globals.rs),
[read tracking](../../renderer/src/lua/signal/tracking.rs),
[re-resolve](../../renderer/src/socket/client/resolve.rs),
[property resolution](../../renderer/src/layout/node/mod.rs),
[kept nodes](../../renderer/src/layout/scene/resolve.rs),
[frozen subtrees](../../renderer/src/layout/scene/pass.rs).
