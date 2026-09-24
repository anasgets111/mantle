# Lua API

Mantle runs a desktop shell written in Lua on Wayland. The engine evaluates your `shell.lua`,
which returns the *surfaces* to show (bars, windows, popups, a lock screen). Each surface holds a
tree of *nodes*, and any node property can be a live *signal* that updates itself when a
*capability* (audio, workspaces, the clock) pushes new state.

This page is the wiki's home: a first shell to build, the core concepts, then indexes by
[topic](#topic-index) and by [task](#how-do-i). Terms are defined in the
[glossary](../CONTEXT.md).

## Your first shell

### 1. Create the config

```sh
mantle init -c ~/.config/mantle
```

| Writes | Contents |
| :--- | :--- |
| `shell.lua` | A starter bar with a clock, only when absent |
| `.luarc.json` | Points lua-language-server at the API stubs, so your editor completes and type-checks |
| Stubs | `$XDG_DATA_HOME/mantle/lua-meta/`, refreshed when they differ from this binary. Skipped when a package installs them under `$PREFIX/share/mantle/lua-meta` |

`--force` overwrites `shell.lua` and `.luarc.json`. The config is a directory, not a file.

### 2. A first bar

`shell.lua` runs top to bottom and returns one surface or an array of them. This bar shows a
launcher button, the workspaces of the first output and a clock. The button and a keybind share
one piece of [named state](lua-api/signals.md#named-state), `launcher_open`, which shows a second
[panel](lua-api/surfaces.md#panel).

```lua
local launcher_open = state("launcher_open", false)

local clock = text {
    content = mantle.system:map(function(system)
        return os.date("%H:%M", system and system.time)
    end),
    foreground = "#cdd6f4",
}

local workspaces = list {
    direction = "Horizontal",
    spacing = 4,
    source = mantle.workspaces:map(function(ws)
        return ws and ws.outputs[1] and ws.outputs[1].workspaces or {}
    end),
    key = function(workspace) return tostring(workspace.id) end,
    itemfn = function(workspace)
        return button {
            padding = { left = 6, right = 6 },
            radius = 4,
            background = workspace.populated and "#45475a" or "#00000000",
            on_click = function()
                mantle.workspaces:invoke("focus", workspace.id)
            end,
            children = { text { content = tostring(workspace.idx), foreground = "#cdd6f4" } },
        }
    end,
}

local launcher_button = button {
    padding = { left = 8, right = 8 },
    background = launcher_open:map(function(open) return open and "#89b4fa" or "#313244" end),
    on_click = function() launcher_open:set(not launcher_open:get()) end,
    children = { text { content = "Apps", foreground = "#cdd6f4" } },
}

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        height = 32,
        background = "#1e1e2ee6",
        child = row {
            width = "Fill",
            align_v = "Center",
            spacing = 8,
            padding = { left = 8, right = 8 },
            children = { launcher_button, workspaces, rect { width = "Fill" }, clock },
        },
    },
    panel {
        id = "launcher",
        layer = "Overlay",
        visible = launcher_open,
        keyboard_interactivity = "OnDemand",
        width = 400,
        height = 300,
        background = "#1e1e2e",
        radius = 12,
        child = text { content = "Launcher", foreground = "#cdd6f4" },
    },
}
```

| Line | Why |
| :--- | :--- |
| `mantle.system:map(...)` | A [derived signal](lua-api/signals.md#derived-signals); `content` re-resolves on every push (once a second). `:get()` would freeze it |
| `system and system.time` | Capabilities read `nil` until their first push, so every map handles `nil` |
| `list { source, itemfn, key }` | Rebuilds one button per workspace when the list changes ([list](lua-api/nodes.md#list)) |
| `:invoke("focus", id)` | Fire and forget; the new active workspace arrives in the next push ([actions](lua-api/capabilities.md#reading-and-acting)) |
| `row { width = "Fill", align_v = "Center" }` | Spans the bar and centres itself in it; the `"Fill"` `rect` pushes the clock right ([alignment](lua-api/nodes.md#alignment)) |
| `visible = launcher_open` | The launcher panel maps and unmaps with the state |

### 3. Run it

| Command | Does |
| :--- | :--- |
| `mantle check` | Evaluates the config with no Wayland and every capability `nil`, then exits; 1 on error. Run after every edit |
| `mantle -d` | Starts the shell detached and prints its pid |
| `mantle log -f` | Follows the running shell's output, `print` included |
| `mantle` | Runs it in the foreground instead |

`mantle check` stops at evaluation: it does not resolve maps or lay out nodes
([what check covers](lua-api/cli.md#what-check-covers)).

### 4. Edit it live

Saving any `.lua` or `.frag` file under the config directory re-evaluates `shell.lua` in the same
process, and named state keeps its value. A reload whose evaluation raises keeps the previous scene on screen,
logs the error and sets `mantle.rescue`. Show it in the bar so you see it without a terminal:

```lua
local rescue_line = text {
    visible = mantle.rescue:map(function(rescue) return rescue.is_rescue end),
    content = mantle.rescue:map(function(rescue) return rescue.error_log:match("^[^\n]*") end),
    foreground = "#f38ba8",
}
```

The next successful reload clears it. Details:
[reload](lua-api/runtime.md#evaluation-reload-and-generations),
[what survives it](lua-api/runtime.md#what-survives-a-reload).

### 5. Split into modules

`require` resolves inside the config directory only (`?.lua`, `?/init.lua`). A module returns a
node like any other value:

```lua
-- widgets/clock.lua
return text {
    content = mantle.system:map(function(system)
        return os.date("%H:%M", system and system.time)
    end),
    foreground = "#cdd6f4",
}
```

```lua
-- shell.lua
local clock = require("widgets.clock")

return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        height = 32,
        child = row { width = "Fill", align_h = "End", align_v = "Center", children = { clock } },
    },
}
```

Bind every `require` to a local: it returns the module and its file path, and a call last in a
table constructor keeps both ([modules](lua-api/runtime.md#modules-and-require)).

### 6. Bind a key

`mantle toggle <name>` flips a declared boolean `state` in the running shell, so the launcher above
opens from the compositor:

```text
# Hyprland (hyprland.conf)
bind = SUPER, A, exec, mantle toggle launcher_open
# Hyprland (Lua config)
hl.bind("SUPER + A", hl.dsp.exec_cmd("mantle toggle launcher_open"))
# niri (config.kdl, inside binds {})
Mod+A { spawn "mantle" "toggle" "launcher_open"; }
```

`mantle set` writes any value, and `mantle call` runs an [`action`](lua-api/scripting.md#action)
([commands](lua-api/cli.md#commands)).

## Core concepts

| Concept | One line | Page |
| :--- | :--- | :--- |
| Surface | A top-level Wayland surface: `panel`, `window`, `popup` or `lock` | [surfaces](lua-api/surfaces.md) |
| Node | An element in a surface's tree: `row`, `text`, `button`, `list` and others | [nodes](lua-api/nodes.md) |
| Signal | A reactive value; pass it to a property to keep that property live | [signals](lua-api/signals.md) |
| Named state | `state(name, initial)`: a writable signal that survives reloads and answers `mantle set`/`toggle` | [signals](lua-api/signals.md#named-state) |
| Capability | `mantle.<name>`: a signal over one backend (audio, network, workspaces), `nil` until its first push | [capabilities](lua-api/capabilities.md) |
| Action | `mantle.<cap>:invoke(...)` asks a backend to act; `action(name, fn)` exposes Lua to `mantle call` | [capabilities](lua-api/capabilities.md#reading-and-acting), [scripting](lua-api/scripting.md#action) |
| Reload / generation | A save re-evaluates in place; a new generation starts only when the Renderer process is replaced, such as after a crash | [runtime](lua-api/runtime.md#evaluation-reload-and-generations) |

Every other term (Supervisor, Renderer, push, rescue, fingerprint): [glossary](../CONTEXT.md).

## Topic index

| Page | For | Sections |
| :--- | :--- | :--- |
| [runtime](lua-api/runtime.md) | The VM, `require`, reloads, limits | [The VM](lua-api/runtime.md#the-vm) · [Modules and require](lua-api/runtime.md#modules-and-require) · [Evaluation, reload and generations](lua-api/runtime.md#evaluation-reload-and-generations) · [What survives a reload](lua-api/runtime.md#what-survives-a-reload) · [Limits and budgets](lua-api/runtime.md#limits-and-budgets) · [Output and logging](lua-api/runtime.md#output-and-logging) |
| [cli](lua-api/cli.md) | The `mantle` binary | [Commands](lua-api/cli.md#commands) · [Flags](lua-api/cli.md#flags) · [Which config and which shell](lua-api/cli.md#which-config-and-which-shell) · [Values and arguments](lua-api/cli.md#values-and-arguments) · [What check covers](lua-api/cli.md#what-check-covers) · [Exit codes](lua-api/cli.md#exit-codes) |
| [signals](lua-api/signals.md) | Reactivity | [The one rule](lua-api/signals.md#the-one-rule) · [Reference](lua-api/signals.md#reference) · [Derived signals](lua-api/signals.md#derived-signals) · [Named state](lua-api/signals.md#named-state) · [How re-resolution works](lua-api/signals.md#how-re-resolution-works) · [Switching views](lua-api/signals.md#switching-views) |
| [capabilities](lua-api/capabilities.md) | `mantle.<cap>` state and actions | [Reading and acting](lua-api/capabilities.md#reading-and-acting) · [Capability list](lua-api/capabilities.md#capability-list) · [Renderer members](lua-api/capabilities.md#renderer-members) · [Capability reference](lua-api/capabilities.md#capability-reference) · [Idle](lua-api/capabilities.md#idle) · [Examples](lua-api/capabilities.md#examples) |
| [scripting](lua-api/scripting.md) | Processes, storage, timers, utilities | [Which one do I use](lua-api/scripting.md#which-one-do-i-use) · [process.run](lua-api/scripting.md#processrun) · [process.detach](lua-api/scripting.md#processdetach) · [session_process](lua-api/scripting.md#session_process) · [persistent_table](lua-api/scripting.md#persistent_table) · [timer](lua-api/scripting.md#timer) · [action](lua-api/scripting.md#action) · [json.decode](lua-api/scripting.md#jsondecode) · [log](lua-api/scripting.md#log) · [fuzzy](lua-api/scripting.md#fuzzy) · [palette.quantize](lua-api/scripting.md#palettequantize) · [fonts](lua-api/scripting.md#fonts) |
| [surfaces](lua-api/surfaces.md) | Top-level surfaces | [Shared rules](lua-api/surfaces.md#shared-rules) · [panel](lua-api/surfaces.md#panel) · [window](lua-api/surfaces.md#window) · [popup](lua-api/surfaces.md#popup) · [lock](lua-api/surfaces.md#lock) |
| [nodes](lua-api/nodes.md) | Layout and node kinds | [Layout model](lua-api/nodes.md#layout-model) · [Kinds](lua-api/nodes.md#kinds) · [rect](lua-api/nodes.md#rect) · [row and column](lua-api/nodes.md#row-and-column) · [button](lua-api/nodes.md#button) · [text](lua-api/nodes.md#text) · [icon](lua-api/nodes.md#icon) · [image](lua-api/nodes.md#image) · [capture](lua-api/nodes.md#capture) · [shader](lua-api/nodes.md#shader) · [list](lua-api/nodes.md#list) · [textfield](lua-api/nodes.md#textfield) · [Switching views with ids](lua-api/nodes.md#switching-views-with-ids) |
| [paint](lua-api/paint.md) | How a box is drawn | [Who takes what](lua-api/paint.md#who-takes-what) · [Colours](lua-api/paint.md#colours) · [Box properties](lua-api/paint.md#box-properties) · [Gradients](lua-api/paint.md#gradients) · [Clip](lua-api/paint.md#clip) · [Mask](lua-api/paint.md#mask) · [Shadows](lua-api/paint.md#shadows) · [Blurs](lua-api/paint.md#blurs) · [Combining effects](lua-api/paint.md#combining-effects) |
| [animation](lua-api/animation.md) | `animate` | [How a tween starts](lua-api/animation.md#how-a-tween-starts) · [Entry keys](lua-api/animation.md#entry-keys) · [Spring](lua-api/animation.md#spring) · [Keyframes](lua-api/animation.md#keyframes) · [Exit](lua-api/animation.md#exit) |
| [input](lua-api/input.md) | Pointer and keyboard | [Hit testing](lua-api/input.md#hit-testing) · [Pointer](lua-api/input.md#pointer) · [Hover](lua-api/input.md#hover) · [Scroll](lua-api/input.md#scroll) · [Text fields](lua-api/input.md#text-fields) · [Secure fields](lua-api/input.md#secure-fields) |

Every page has a Gotchas table (trap | fix).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a live value from the system | [First bar](#2-a-first-bar), [one rule](lua-api/signals.md#the-one-rule) |
| Handle a capability that has not pushed yet | [Capability examples](lua-api/capabilities.md#examples) |
| React to a capability change (OSD, sound) | [Capability examples](lua-api/capabilities.md#examples) (`on_change`) |
| Combine two sources into one value | [Derive from two capabilities](lua-api/signals.md#derive-from-two-capabilities) |
| Debounce a search or hold a value | [Debounce a search](lua-api/signals.md#debounce-a-search), [delay](lua-api/signals.md#delay-hold-a-value) |
| Flash a node when a value changes | [pulse](lua-api/signals.md#pulse-mark-a-change) |
| Open UI from a compositor keybind | [Bind a key](#6-bind-a-key), [drive UI from a keybind](lua-api/signals.md#drive-ui-from-a-keybind) |
| Make a keybind run Lua and print a result | [action](lua-api/scripting.md#action) |
| Switch between tabs or views | [Switching views](lua-api/signals.md#switching-views), [with ids](lua-api/nodes.md#switching-views-with-ids) |
| Show a dropdown under a button | [popup](lua-api/surfaces.md#popup), [derived signals](lua-api/signals.md#derived-signals) |
| Show a tooltip on hover | [Tooltip](lua-api/surfaces.md#tooltip) |
| Close a popup on outside click | [Dismissal](lua-api/surfaces.md#dismissal) |
| Type into a panel | [Keyboard focus](lua-api/surfaces.md#keyboard-focus), [text fields](lua-api/input.md#text-fields) |
| Draw different content per monitor | [Per-output content](lua-api/surfaces.md#per-output-content) |
| Build a lock screen | [lock](lua-api/surfaces.md#lock), [secure fields](lua-api/input.md#secure-fields) |
| Build a list from data | [list](lua-api/nodes.md#list) |
| Show an app's icon | [Show an app's icon](lua-api/nodes.md#show-an-apps-icon) |
| Centre or space out items | [Centre something](lua-api/nodes.md#centre-something), [alignment](lua-api/nodes.md#alignment) |
| Draw a progress meter | [row and column](lua-api/nodes.md#row-and-column) |
| Make a slider or wheel control | [Pointer](lua-api/input.md#pointer) |
| Scroll a long list | [Scroll a long list](lua-api/nodes.md#scroll-a-long-list), [scroll](lua-api/input.md#scroll) |
| Search a list as you type | [fuzzy](lua-api/scripting.md#fuzzy) |
| Crossfade a wallpaper | [image](lua-api/nodes.md#image) |
| Write a shader effect | [shader](lua-api/nodes.md#shader) |
| Blur the desktop behind a bar | [Blurs](lua-api/paint.md#blurs) |
| Round and clip content | [Round an image's corners](lua-api/nodes.md#round-an-images-corners), [clip](lua-api/paint.md#clip) |
| Fade or slide a node | [animation](lua-api/animation.md), [spring](lua-api/animation.md#spring) |
| Animate a node out before it goes | [Exit](lua-api/animation.md#exit) |
| Show a spinner | [Keyframes](lua-api/animation.md#keyframes) |
| Run a command and read its output | [process.run](lua-api/scripting.md#processrun) |
| Launch an app that outlives the shell | [process.detach](lua-api/scripting.md#processdetach) |
| Keep a daemon running for the session | [session_process](lua-api/scripting.md#session_process) |
| Repeat something every few seconds | [timer](lua-api/scripting.md#timer) |
| Remember a setting across restarts | [persistent_table](lua-api/scripting.md#persistent_table) |
| Theme from the wallpaper | [palette.quantize](lua-api/scripting.md#palettequantize) |
| Show a reload error in the bar | [Edit it live](#4-edit-it-live) |
| Find why a reload or budget failed | [Limits and budgets](lua-api/runtime.md#limits-and-budgets), [output and logging](lua-api/runtime.md#output-and-logging) |

More recipes, per page: [runtime](lua-api/runtime.md#how-do-i), [signals](lua-api/signals.md#how-do-i), [scripting](lua-api/scripting.md#how-do-i), [surfaces](lua-api/surfaces.md#how-do-i), [nodes](lua-api/nodes.md#how-do-i), [paint](lua-api/paint.md#how-do-i), [animation](lua-api/animation.md#how-do-i).

## Other references

| For | Read |
| :--- | :--- |
| Editor completion and type checks | [`lua-meta/`](../lua-meta/) stubs, wired up by the `.luarc.json` that `mantle init` writes. [`mantle.lua`](../lua-meta/mantle.lua) is generated from the Rust capability types |
| Backend behavior and wire format | [services](services.md) |
| Glossary | [CONTEXT](../CONTEXT.md) |
| Why a contract is what it is | [decisions](decisions.md), cited as ADR-NNNN |
| Gaps and proposed work | [roadmap](roadmap.md) |

Source: [init](../supervisor/src/setup.rs), [starter](../share/starter/shell.lua),
[CLI](../supervisor/src/cli.rs), [watcher](../supervisor/src/watcher.rs),
[reload and rescue](../renderer/src/socket/client/mod.rs), [require path](../renderer/src/lua/mod.rs).
