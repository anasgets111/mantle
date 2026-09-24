# Introduction

Mantle runs a desktop shell written in Lua on Wayland. The engine evaluates your `shell.lua`,
which returns the *surfaces* to show (bars, windows, popups, a lock screen). Each surface holds a
tree of *nodes*, and any node property can be a live *signal* that updates itself when a
*capability* (audio, workspaces, the clock) pushes new state.

This page is the book's home: a first shell to build, the core concepts, then indexes by
[topic](#topic-index) and by [task](#how-do-i). Terms are defined in the
[glossary](glossary.md).

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
| `mantle.system:map(...)` | A [derived signal](guide/signals.md#derived-signals); `content` re-resolves on every push (once a second). `:get()` would freeze it |
| `system and system.time` | Capabilities read `nil` until their first push, so every map handles `nil` |
| `list { source, itemfn, key }` | Rebuilds one button per workspace when the list changes ([list](nodes/list.md)) |
| `:invoke("focus", id)` | Fire and forget; the new active workspace arrives in the next push ([actions](capabilities/index.md#actions)) |
| `row { width = "Fill", align_v = "Center" }` | Spans the bar and centres itself in it; the `"Fill"` `rect` pushes the clock right ([alignment](nodes/index.md#alignment)) |
| `visible = launcher_open` | The launcher panel maps and unmaps with the state |

### 3. Run it

| Command | Does |
| :--- | :--- |
| `mantle check` | Evaluates the config with no Wayland and every capability `nil`, then exits; 1 on error. Run after every edit |
| `mantle -d` | Starts the shell detached and prints its pid |
| `mantle log -f` | Follows the running shell's output, `print` included |
| `mantle` | Runs it in the foreground instead |

`mantle check` stops at evaluation: it does not resolve maps or lay out nodes
([what check covers](guide/cli.md#what-check-covers)).

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

## Core concepts

| Concept | One line | Page |
| :--- | :--- | :--- |
| Surface | A top-level Wayland surface: `panel`, `window`, `popup` or `lock` | [surfaces](surfaces/index.md) |
| Node | An element in a surface's tree: `row`, `text`, `button`, `list` and others | [nodes](nodes/index.md) |
| Signal | A reactive value; pass it to a property to keep that property live | [signals](guide/signals.md) |
| Named state | `state(name, initial)`: a writable signal that survives reloads and answers `mantle set`/`toggle` | [signals](guide/signals.md#named-state) |
| Capability | `mantle.<name>`: a signal over one backend (audio, network, workspaces), `nil` until its first push | [capabilities](capabilities/index.md) |
| Action | `mantle.<cap>:invoke(...)` asks a backend to act; `action(name, fn)` exposes Lua to `mantle call` | [capabilities](capabilities/index.md#actions), [scripting](guide/scripting.md#action) |
| Reload / generation | A save re-evaluates in place; a new generation starts only when the Renderer process is replaced, such as after a crash | [runtime](guide/runtime.md#evaluation-reload-and-generations) |

Every other term (Supervisor, Renderer, push, rescue, fingerprint): [glossary](glossary.md).

## Topic index

| Page | For | Sections |
| :--- | :--- | :--- |
| [installation](guide/installation.md) | Requirements, install, first run | [Requirements](guide/installation.md#requirements) · [Install](guide/installation.md#install) · [Set up a config](guide/installation.md#set-up-a-config) · [Run the shell](guide/installation.md#run-the-shell) |
| [runtime](guide/runtime.md) | The VM, `require`, reloads, limits | [The VM](guide/runtime.md#the-vm) · [Modules and require](guide/runtime.md#modules-and-require) · [Evaluation, reload and generations](guide/runtime.md#evaluation-reload-and-generations) · [What survives a reload](guide/runtime.md#what-survives-a-reload) · [Limits and budgets](guide/runtime.md#limits-and-budgets) · [Output and logging](guide/runtime.md#output-and-logging) |
| [cli](guide/cli.md) | The `mantle` binary | [Commands](guide/cli.md#commands) · [Flags](guide/cli.md#flags) · [Environment variables](guide/cli.md#environment-variables) · [Binaries](guide/cli.md#binaries) · [Which config and which shell](guide/cli.md#which-config-and-which-shell) · [Values and arguments](guide/cli.md#values-and-arguments) · [What check covers](guide/cli.md#what-check-covers) · [Exit codes](guide/cli.md#exit-codes) |
| [signals](guide/signals.md) | Reactivity | [The one rule](guide/signals.md#the-one-rule) · [Reference](guide/signals.md#reference) · [Derived signals](guide/signals.md#derived-signals) · [Named state](guide/signals.md#named-state) · [How re-resolution works](guide/signals.md#how-re-resolution-works) · [Switching views](guide/signals.md#switching-views) |
| [capabilities](capabilities/index.md) | `mantle.<cap>` state and actions | [Reading and acting](capabilities/index.md#reading-and-acting) · [Capability list](capabilities/index.md#capability-list) · [Renderer members](capabilities/index.md#renderer-members) · [applications](capabilities/applications.md) · [audio](capabilities/audio.md) · [battery](capabilities/battery.md) · [bluetooth](capabilities/bluetooth.md) · [brightness](capabilities/brightness.md) · [files](capabilities/files.md) · [idle](capabilities/idle.md) · [keyboard](capabilities/keyboard.md) · [lock](capabilities/lock.md) · [mpris](capabilities/mpris.md) · [network](capabilities/network.md) · [notifications](capabilities/notifications.md) · [polkit](capabilities/polkit.md) · [power](capabilities/power.md) · [privacy](capabilities/privacy.md) · [processes](capabilities/processes.md) · [storage](capabilities/storage.md) · [sysinfo](capabilities/sysinfo.md) · [system](capabilities/system.md) · [tray](capabilities/tray.md) · [updates](capabilities/updates.md) · [windows](capabilities/windows.md) · [workspaces](capabilities/workspaces.md) |
| [processes](guide/processes.md) | Running other programs | [Which one do I use](guide/processes.md#which-one-do-i-use) · [process.run](guide/processes.md#processrun) · [process.detach](guide/processes.md#processdetach) · [session_process](guide/processes.md#session_process) |
| [scripting](guide/scripting.md) | Storage, timers, actions, utilities | [Which one do I use](guide/scripting.md#which-one-do-i-use) · [persistent_table](guide/scripting.md#persistent_table) · [timer](guide/scripting.md#timer) · [action](guide/scripting.md#action) · [json.decode](guide/scripting.md#jsondecode) · [log](guide/scripting.md#log) · [fuzzy](guide/scripting.md#fuzzy) · [palette.quantize](guide/scripting.md#palettequantize) · [fonts](guide/scripting.md#fonts) |
| [surfaces](surfaces/index.md) | Top-level surfaces | [Properties every role takes](surfaces/index.md#properties-every-role-takes) · [Per-output child](surfaces/index.md#per-output-child) · [Input region](surfaces/index.md#input-region) · [panel](surfaces/panel.md) · [window](surfaces/window.md) · [popup](surfaces/popup.md) · [lock](surfaces/lock.md) |
| [nodes](nodes/index.md) | Layout and node kinds | [Layout model](nodes/index.md#layout-model) · [Common properties](nodes/index.md#common-properties) · [Identity](nodes/index.md#identity-and-reconciliation) · [Switching](nodes/index.md#showing-hiding-and-switching) · [rect](nodes/rect.md) · [row and column](nodes/row-column.md) · [button](nodes/button.md) · [text](nodes/text.md) · [icon](nodes/icon.md) · [image](nodes/image.md) · [capture](nodes/capture.md) · [shader](nodes/shader.md) · [list](nodes/list.md) · [textfield](nodes/textfield.md) |
| [paint](guide/paint.md) | How a box is drawn | [Who takes what](guide/paint.md#who-takes-what) · [Colours](guide/paint.md#colours) · [Box properties](guide/paint.md#box-properties) · [Gradients](guide/paint.md#gradients) · [Clip](guide/paint.md#clip) · [Mask](guide/paint.md#mask) · [Shadows](guide/paint.md#shadows) · [Blurs](guide/paint.md#blurs) · [Combining effects](guide/paint.md#combining-effects) |
| [animation](guide/animation.md) | `animate` | [How a tween starts](guide/animation.md#how-a-tween-starts) · [Entry keys](guide/animation.md#entry-keys) · [Spring](guide/animation.md#spring) · [Keyframes](guide/animation.md#keyframes) · [Exit](guide/animation.md#exit) |
| [input](guide/input.md) | Pointer and keyboard | [Hit testing](guide/input.md#hit-testing) · [Pointer](guide/input.md#pointer) · [Hover](guide/input.md#hover) · [Scroll](guide/input.md#scroll) · [Text fields](guide/input.md#text-fields) · [Secure fields](guide/input.md#secure-fields) |
| [faq](guide/faq.md) | Troubleshooting | [First steps](guide/faq.md#first-steps-when-something-is-wrong) · [Nothing shows](guide/faq.md#nothing-shows) · [A save or a click does nothing](guide/faq.md#a-save-or-a-click-does-nothing) · [Values are wrong or stale](guide/faq.md#values-are-wrong-or-stale) · [Errors in the log](guide/faq.md#errors-in-the-log) · [Running processes](guide/faq.md#running-processes) · [Capabilities](guide/faq.md#capabilities) |
| [cookbook](cookbook/index.md) | Complete widgets to copy | [Clock bar](cookbook/clock-bar.md) · [Workspaces](cookbook/workspaces.md) · [Volume OSD](cookbook/volume-osd.md) · [Notification popups](cookbook/notifications.md) · [App launcher](cookbook/launcher.md) · [Battery](cookbook/battery.md) · [Tray](cookbook/tray.md) · [Lock screen](cookbook/lock-screen.md) · [Media player](cookbook/media-player.md) · [Power menu](cookbook/power-menu.md) |

Every page has a Gotchas table (trap | fix). Symptoms whose cause lives on another page are in the
[FAQ](guide/faq.md).

## How do I…

| Task | Answer |
| :--- | :--- |
| Install Mantle and start it with the session | [Install](guide/installation.md#install), [run the shell](guide/installation.md#run-the-shell) |
| Copy a complete bar, launcher or lock screen | [Cookbook](cookbook/index.md) |
| Show a live value from the system | [First bar](#2-a-first-bar), [one rule](guide/signals.md#the-one-rule) |
| Handle a capability that has not pushed yet | [Reading and acting](capabilities/index.md#reading-and-acting) |
| React to a capability change (OSD, sound) | [`on_change`](capabilities/index.md#reading-and-acting), [Volume OSD](cookbook/volume-osd.md) |
| Combine two sources into one value | [Derive from two capabilities](guide/signals.md#derive-from-two-capabilities) |
| Debounce a search or hold a value | [Debounce a search](guide/signals.md#debounce-a-search), [delay](guide/signals.md#delay-hold-a-value) |
| Flash a node when a value changes | [pulse](guide/signals.md#pulse-mark-a-change) |
| Open UI from a compositor keybind | [Bind a key](#6-bind-a-key), [drive UI from a keybind](guide/signals.md#drive-ui-from-a-keybind) |
| Make a keybind run Lua and print a result | [action](guide/scripting.md#action) |
| Switch between tabs or views | [Switching views](guide/signals.md#switching-views), [with ids](nodes/index.md#switching-views-with-ids) |
| Show a dropdown under a button | [Anchor to a node's geometry](surfaces/popup.md#anchor-to-a-nodes-geometry), [nested menus](surfaces/popup.md#nested-menus) |
| Show a tooltip on hover | [Tooltip](surfaces/popup.md#tooltip) |
| Close a popup on outside click | [Dismissal](surfaces/popup.md#dismissal), [close an overlay](surfaces/panel.md#close-an-overlay-on-an-outside-click) |
| Type into a panel | [Keyboard focus](surfaces/panel.md#keyboard-focus), [text fields](guide/input.md#text-fields) |
| Draw different content per monitor | [Per-output content](surfaces/panel.md#per-output-content), [per-output child](surfaces/index.md#per-output-child) |
| Build a lock screen | [lock](surfaces/lock.md), [secure fields](guide/input.md#secure-fields), [recipe](cookbook/lock-screen.md) |
| Build a list from data | [list](nodes/list.md) |
| Show an app's icon | [icon](nodes/icon.md#how-do-i) |
| Centre or space out items | [Centre something](nodes/rect.md#centre-something), [alignment](nodes/index.md#alignment), [push items apart](nodes/row-column.md#push-items-apart) |
| Draw a progress meter | [row and column](nodes/row-column.md#how-do-i) |
| Make a slider or wheel control | [Pointer](guide/input.md#pointer) |
| Scroll a long list | [Scroll a long list](nodes/list.md#scroll-a-long-list), [scroll](guide/input.md#scroll) |
| Search a list as you type | [fuzzy](guide/scripting.md#fuzzy) |
| Crossfade a wallpaper | [transition](nodes/image.md#transition) |
| Write a shader effect | [shader](nodes/shader.md) |
| Blur the desktop behind a bar | [Blurs](guide/paint.md#blurs) |
| Round and clip content | [Round an image's corners](nodes/image.md#round-an-images-corners), [clip](guide/paint.md#clip) |
| Fade or slide a node | [animation](guide/animation.md), [spring](guide/animation.md#spring) |
| Animate a node out before it goes | [Exit](guide/animation.md#exit) |
| Show a spinner | [Keyframes](guide/animation.md#keyframes) |
| Run a command and read its output | [process.run](guide/processes.md#processrun) |
| Launch an app that outlives the shell | [process.detach](guide/processes.md#processdetach) |
| Keep a daemon running for the session | [session_process](guide/processes.md#session_process) |
| Repeat something every few seconds | [timer](guide/scripting.md#timer) |
| Remember a setting across restarts | [persistent_table](guide/scripting.md#persistent_table) |
| Theme from the wallpaper | [palette.quantize](guide/scripting.md#palettequantize) |
| Show a reload error in the bar | [Error banner](guide/runtime.md#evaluation-reload-and-generations) |
| Find why a reload or budget failed | [Limits and budgets](guide/runtime.md#limits-and-budgets), [output and logging](guide/runtime.md#output-and-logging) |
| Find why something shows nothing or does nothing | [FAQ](guide/faq.md) |
| Look up a term | [Glossary](glossary.md) |

More recipes, per page: [runtime](guide/runtime.md#how-do-i), [cli](guide/cli.md#how-do-i),
[signals](guide/signals.md#how-do-i), [processes](guide/processes.md#how-do-i),
[scripting](guide/scripting.md#how-do-i), [capabilities](capabilities/index.md#how-do-i),
[surfaces](surfaces/index.md#how-do-i), [nodes](nodes/index.md#how-do-i),
[paint](guide/paint.md#how-do-i), [animation](guide/animation.md#how-do-i),
[input](guide/input.md#how-do-i), [cookbook](cookbook/index.md).

## Other references

| For | Read |
| :--- | :--- |
| Editor completion and type checks | [`lua-meta/`](../lua-meta/) stubs, wired up by the `.luarc.json` that `mantle init` writes. [`mantle.lua`](../lua-meta/mantle.lua) is generated from the Rust capability types |
| What each backend needs installed | [installation](guide/installation.md#requirements) |
| Glossary | [glossary](glossary.md); engine-internal terms in [`CONTEXT.md`](../CONTEXT.md) |
| Why a contract is what it is | [`DECISIONS.md`](../DECISIONS.md), cited as ADR-NNNN |
| What changed | [changelog](changelog.md) |
| Gaps and proposed work | [roadmap](roadmap.md) |
| Writing or testing a page of this book | [documenting](development/documenting.md) |

Source: [init](../supervisor/src/setup.rs), [starter](../share/starter/shell.lua),
[CLI](../supervisor/src/cli.rs), [watcher](../supervisor/src/watcher.rs),
[reload and rescue](../renderer/src/socket/client/mod.rs), [require path](../renderer/src/lua/mod.rs).
