<p class="wordmark"><img src="theme/m.png" alt="M">antle</p>

# Introduction

Mantle runs a desktop shell written in Lua on Wayland. The engine evaluates your `shell.lua`,
which returns the [surfaces](surfaces/index.md) to show (bars, windows, popups, a lock screen).
Each surface holds a tree of [nodes](nodes/index.md), and any node property can be a live
[signal](guide/signals.md) that updates itself when a [capability](capabilities/index.md) (audio,
workspaces, the clock) pushes new state.

<video src="https://github.com/user-attachments/assets/b4a56c2f-a946-44f9-9bfd-2c6046d7a72f" controls muted loop playsinline preload="metadata"></video>

<video src="https://github.com/user-attachments/assets/038ee763-d7b6-4df9-9f79-2f131d4f0dcd" controls muted loop playsinline preload="metadata"></video>

<video src="https://github.com/user-attachments/assets/5533b578-1d9e-484e-bb18-4b4fae1da50d" controls muted loop playsinline preload="metadata"></video>

Build the first shell below, then find the rest by [topic](#topic-index) or by
[task](#how-do-i). The [glossary](glossary.md) defines every term.

## Your first shell

### 1. Create the config

```sh
mantle init -c ~/.config/mantle
```

It writes a starter `shell.lua` and a `.luarc.json` that points lua-language-server at the API
stubs, so your editor completes and type-checks. The config is a directory, not a file
([`init`](guide/cli.md#commands)).

### 2. A first bar

`shell.lua` runs top to bottom and returns one surface or an array of them. This bar shows a
launcher button, the workspaces of the first output and a clock. The button and a keybind share
one piece of [named state](guide/signals.md#named-state), `launcher_open`, which shows a second
[panel](surfaces/panel.md).

```lua,shot
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
                mantle.workspaces:focus(workspace.id)
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
        width = "Fill",
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
| `mantle.system:map(...)` | A [derived signal](guide/signals.md#derived-signals); `content` re-resolves on every push (once a second unless [`configure`](capabilities/system.md) says otherwise). `:get()` would freeze it |
| `system and system.time` | Capabilities read `nil` until their first push, so every map handles `nil` |
| `list { source, itemfn, key }` | Rebuilds one button per workspace when the list changes ([list](nodes/list.md)) |
| `:focus(id)` | Fire and forget; the new active workspace arrives in the next push ([actions](capabilities/index.md#actions)) |
| `width = "Fill"` on the panel and the `row` | The panel's root spans the anchored edges only when asked ([size](surfaces/panel.md#size)); the `"Fill"` `rect` then pushes the clock right ([alignment](nodes/index.md#alignment)) |
| `visible = launcher_open` | The launcher panel maps and unmaps with the state |

### 3. Run it

| Command | Does |
| :--- | :--- |
| `mantle check` | Evaluates and lays out the config with no Wayland, with every capability `nil` and again with sample data, then exits; 1 on error. Run after every edit. [What it misses](guide/cli.md#what-check-covers) |
| `mantle -d` | Starts the shell detached and prints its pid |
| `mantle log -f` | Follows the running shell's output, `print` included |
| `mantle` | Runs it in the foreground instead |

### 4. Edit it live

Saving any `.lua` or `.frag` file under the config directory re-evaluates `shell.lua` in the same
process, and named state keeps its value. A reload whose evaluation raises keeps the previous
scene on screen, logs the error and sets `mantle.rescue`, which a config can draw as an
[error banner](guide/runtime.md#evaluation-reload-and-generations). The next successful reload
clears it. What a reload keeps: [what survives it](guide/runtime.md#what-survives-a-reload).

### 5. Split into modules

`require("widgets.clock")` loads `widgets/clock.lua` from the config directory, and nothing
outside it. Bind every `require` to a local before listing it in a table: it returns the module
and its file path. Example and rules: [modules](guide/runtime.md#modules-and-require).

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

`mantle set` writes any value, and `mantle call` runs an [`action`](guide/scripting.md#action)
([commands](guide/cli.md#commands)).

## Topic index

The sidebar's order. Every guide, surface and node page ends with How do I… and Gotchas tables.

| Page | For | Sections |
| :--- | :--- | :--- |
| [installation](guide/installation.md) | Requirements, install, autostart | [Requirements](guide/installation.md#requirements) · [Install](guide/installation.md#install) · [Set up a config](guide/installation.md#set-up-a-config) · [Run the shell](guide/installation.md#run-the-shell) |
| [cli](guide/cli.md) | The `mantle` binary and keybinds | [Commands](guide/cli.md#commands) · [Flags](guide/cli.md#flags) · [Environment variables](guide/cli.md#environment-variables) · [Binaries](guide/cli.md#binaries) · [Which config and which shell](guide/cli.md#which-config-and-which-shell) · [Values and arguments](guide/cli.md#values-and-arguments) · [What check covers](guide/cli.md#what-check-covers) · [Exit codes](guide/cli.md#exit-codes) |
| [runtime](guide/runtime.md) | The VM, `require`, reloads, limits | [The VM](guide/runtime.md#the-vm) · [Modules and require](guide/runtime.md#modules-and-require) · [Evaluation, reload and generations](guide/runtime.md#evaluation-reload-and-generations) · [What survives a reload](guide/runtime.md#what-survives-a-reload) · [Limits and budgets](guide/runtime.md#limits-and-budgets) · [Output and logging](guide/runtime.md#output-and-logging) |
| [signals](guide/signals.md) | Reactivity and named state | [The one rule](guide/signals.md#the-one-rule) · [Reference](guide/signals.md#reference) · [Derived signals](guide/signals.md#derived-signals) · [Named state](guide/signals.md#named-state) · [How re-resolution works](guide/signals.md#how-re-resolution-works) · [Switching views](guide/signals.md#switching-views) |
| [surfaces](surfaces/index.md) | `panel`, `window`, `popup`, `lock` | [Properties every role takes](surfaces/index.md#properties-every-role-takes) · [Per-output child](surfaces/index.md#per-output-child) · [Input region](surfaces/index.md#input-region) |
| [nodes](nodes/index.md) | Layout and the node kinds | [Layout model](nodes/index.md#layout-model) · [Common properties](nodes/index.md#common-properties) · [Identity](nodes/index.md#identity-and-reconciliation) · [Switching](nodes/index.md#showing-hiding-and-switching) |
| [paint](guide/paint.md) | How a box is drawn | [Colours](guide/paint.md#colours) · [Box properties](guide/paint.md#box-properties) · [Gradients](guide/paint.md#gradients) · [Clip](guide/paint.md#clip) · [Mask](guide/paint.md#mask) · [Shadows](guide/paint.md#shadows) · [Blurs](guide/paint.md#blurs) |
| [animation](guide/animation.md) | `animate` | [How a tween starts](guide/animation.md#how-a-tween-starts) · [Entry keys](guide/animation.md#entry-keys) · [Spring](guide/animation.md#spring) · [Keyframes](guide/animation.md#keyframes) · [Exit](guide/animation.md#exit) |
| [input](guide/input.md) | Pointer and keyboard | [Hit testing](guide/input.md#hit-testing) · [Pointer](guide/input.md#pointer) · [Hover](guide/input.md#hover) · [Scroll](guide/input.md#scroll) · [Text fields](guide/input.md#text-fields) · [Secure fields](guide/input.md#secure-fields) |
| [processes](guide/processes.md) | Running other programs | [Which one do I use](guide/processes.md#which-one-do-i-use) · [process.run](guide/processes.md#processrun) · [process.detach](guide/processes.md#processdetach) · [session_process](guide/processes.md#session_process) |
| [scripting](guide/scripting.md) | Storage, timers, actions, utilities | [persistent_table](guide/scripting.md#persistent_table) · [timer](guide/scripting.md#timer) · [action](guide/scripting.md#action) · [json.decode](guide/scripting.md#jsondecode) · [log](guide/scripting.md#log) · [fuzzy](guide/scripting.md#fuzzy) · [palette.quantize](guide/scripting.md#palettequantize) · [fonts](guide/scripting.md#fonts) |
| [capabilities](capabilities/index.md) | `mantle.<name>` state and actions | [Reading and acting](capabilities/index.md#reading-and-acting) · [Capability list](capabilities/index.md#capability-list) · [Renderer members](capabilities/index.md#renderer-members) |
| [cookbook](cookbook/index.md) | Complete widgets to copy | |
| [faq](guide/faq.md) | A symptom whose cause lives on another page | [First steps](guide/faq.md#first-steps-when-something-is-wrong) · [Nothing shows](guide/faq.md#nothing-shows) · [A save or a click does nothing](guide/faq.md#a-save-or-a-click-does-nothing) · [Values are wrong or stale](guide/faq.md#values-are-wrong-or-stale) · [Errors in the log](guide/faq.md#errors-in-the-log) · [Running processes](guide/faq.md#running-processes) · [Capabilities](guide/faq.md#capabilities) |
| [glossary](glossary.md) | Terms; engine-internal ones are in [`CONTEXT.md`](../CONTEXT.md) | |
| [changelog](changelog.md) | Lua API and CLI changes | |
| [roadmap](roadmap.md) | Gaps, proposed work, non-goals | |
| [documenting](development/documenting.md) | Writing and testing a page of this book | |

Editor completion comes from the [`lua-meta/`](../lua-meta/) stubs that `mantle init` wires up;
[`mantle.lua`](../lua-meta/mantle.lua) is generated from the Rust capability types.
[`DECISIONS.md`](../DECISIONS.md) records why each contract is what it is, cited as ADR-NNNN.

## How do I…

| Task | Answer |
| :--- | :--- |
| Install Mantle and start it with the session | [Install](guide/installation.md#install), [run the shell](guide/installation.md#run-the-shell) |
| Open UI from a compositor keybind | [Bind a key](#6-bind-a-key), [drive UI from a keybind](guide/signals.md#drive-ui-from-a-keybind) |
| Make a keybind run Lua and print a result | [action](guide/scripting.md#action) |
| Show a reload error in the bar | [Error banner](guide/runtime.md#evaluation-reload-and-generations) |
| Find why a reload or budget failed | [Limits and budgets](guide/runtime.md#limits-and-budgets), [output and logging](guide/runtime.md#output-and-logging) |
| Show a live value from the system | [First bar](#2-a-first-bar), [one rule](guide/signals.md#the-one-rule) |
| Combine two sources into one value | [Derive from two capabilities](guide/signals.md#derive-from-two-capabilities) |
| Debounce a search or hold a value | [Debounce a search](guide/signals.md#debounce-a-search), [delay](guide/signals.md#delay-hold-a-value) |
| Flash a node when a value changes | [pulse](guide/signals.md#pulse-mark-a-change) |
| Switch between tabs or views | [Switching views](guide/signals.md#switching-views), [with ids](nodes/index.md#switching-views-with-ids) |
| Draw different content per monitor | [Per-output content](surfaces/panel.md#per-output-content), [per-output child](surfaces/index.md#per-output-child) |
| Type into a panel | [Keyboard focus](surfaces/panel.md#keyboard-focus), [text fields](guide/input.md#text-fields) |
| Close an overlay or popup on an outside click | [Close an overlay](surfaces/panel.md#close-an-overlay-on-an-outside-click), [dismissal](surfaces/popup.md#dismissal) |
| Show a dropdown under a button | [Anchor to a node's geometry](surfaces/popup.md#anchor-to-a-nodes-geometry), [nested menus](surfaces/popup.md#nested-menus) |
| Show a tooltip on hover | [Tooltip](surfaces/popup.md#tooltip) |
| Build a lock screen | [lock](surfaces/lock.md), [secure fields](guide/input.md#secure-fields), [recipe](cookbook/lock-screen.md) |
| Centre or space out items | [Centre something](nodes/rect.md#centre-something), [alignment](nodes/index.md#alignment), [push items apart](nodes/row-column.md#push-items-apart) |
| Draw a progress meter | [row and column](nodes/row-column.md#how-do-i) |
| Show an app's icon | [icon](nodes/icon.md#how-do-i) |
| Crossfade a wallpaper | [transition](nodes/image.md#transition) |
| Build a list from data | [list](nodes/list.md) |
| Scroll a long list | [Scroll a long list](nodes/list.md#scroll-a-long-list), [scroll](guide/input.md#scroll) |
| Write a shader effect | [shader](nodes/shader.md) |
| Round and clip content | [Round an image's corners](nodes/image.md#round-an-images-corners), [clip](guide/paint.md#clip) |
| Blur the desktop behind a bar | [Blurs](guide/paint.md#blurs) |
| Fade or slide a node | [animation](guide/animation.md), [spring](guide/animation.md#spring) |
| Show a spinner | [Keyframes](guide/animation.md#keyframes) |
| Animate a node out before it goes | [Exit](guide/animation.md#exit) |
| Make a slider or wheel control | [Pointer](guide/input.md#pointer) |
| Run a command and read its output | [process.run](guide/processes.md#processrun) |
| Launch an app that outlives the shell | [process.detach](guide/processes.md#processdetach) |
| Keep a daemon running for the session | [session_process](guide/processes.md#session_process) |
| Remember a setting across restarts | [persistent_table](guide/scripting.md#persistent_table) |
| Repeat something every few seconds | [timer](guide/scripting.md#timer) |
| Search a list as you type | [fuzzy](guide/scripting.md#fuzzy) |
| Theme from the wallpaper | [palette.quantize](guide/scripting.md#palettequantize) |
| Handle a capability that has not pushed yet | [Reading and acting](capabilities/index.md#reading-and-acting) |
| React to a capability change (OSD, sound) | [`on_change`](capabilities/index.md#reading-and-acting), [Volume OSD](cookbook/volume-osd.md) |
| Copy a complete bar, launcher or lock screen | [Cookbook](cookbook/index.md) |
| Find why something shows nothing or does nothing | [FAQ](guide/faq.md) |

Source: [init](../supervisor/src/setup.rs), [starter](../share/starter/shell.lua),
[CLI](../supervisor/src/cli.rs), [watcher](../supervisor/src/watcher.rs),
[reload and rescue](../renderer/src/socket/client/mod.rs), [require path](../renderer/src/lua/mod.rs).
