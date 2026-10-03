# window

An `xdg_toplevel`: an application window the compositor places, tiles, decorates and closes. Use it
for a settings window or a dialog; use a [panel](panel.md) for anything pinned to the desktop.
Rules every role shares are in [surfaces](index.md).

<!-- shot-alt: A settings window: a sidebar titled Settings with General selected among icon tabs, and General's Appearance and Clock groups of described switches. -->
```lua,shot
local open = state("settings_open", false)
local page = state("settings_page", "General")
local TABS = {
    { name = "General", icon = "dialog-information-symbolic" },
    { name = "Display", icon = "display-brightness-symbolic" },
    { name = "Sound", icon = "audio-volume-high-symbolic" },
    { name = "Power", icon = "system-shutdown-symbolic" },
}

local function tab(entry)
    local selected = page:map(function(current) return current == entry.name end)
    return row {
        width = "Fill",
        padding = { left = 10, right = 12, top = 8, bottom = 8 },
        spacing = 10,
        radius = 8,
        background = selected:map(function(on) return on and "#89b4fa26" or "#00000000" end),
        on_click = function() page:set(entry.name) end,
        children = {
            icon { name = entry.icon, size = 16, align_v = "Center",
                   foreground = selected:map(function(on) return on and "#89b4fa" or "#a6adc8" end) },
            text { content = entry.name, foreground = "#cdd6f4", align_v = "Center" },
        },
    }
end

local function switch(label, detail, on)
    return row {
        width = "Fill",
        padding = 12,
        spacing = 12,
        children = {
            column { width = "Fill", spacing = 2, align_v = "Center", children = {
                text { content = label, foreground = "#cdd6f4" },
                text { content = detail, font_size = 12, foreground = "#a6adc8" },
            } },
            rect {
                width = 36, height = 20, radius = 10, align_v = "Center",
                background = on and "#89b4fa" or "#45475a",
                children = { rect { width = 14, height = 14, radius = 7, margin = { left = 3, right = 3 },
                    align_h = on and "End" or "Start", align_v = "Center", background = on and "#ffffff" or "#bac2de" } },
            },
        },
    }
end

local function group(title, rows)
    local divided = {}
    for index, item in ipairs(rows) do
        if index > 1 then divided[#divided + 1] = rect { width = "Fill", height = 1, margin = { left = 12, right = 12 }, background = "#313244" } end
        divided[#divided + 1] = item
    end
    return column { width = "Fill", spacing = 10, children = {
        text { content = title, font_size = 12, font_weight = 700, foreground = "#89b4fa", margin = { left = 4 } },
        column { width = "Fill", radius = 10, background = "#181825", children = divided },
    } }
end

local tabs = { text { content = "Settings", font_size = 15, font_weight = 700, foreground = "#cdd6f4", margin = { left = 10, top = 6, bottom = 10 } } }
for _, entry in ipairs(TABS) do tabs[#tabs + 1] = tab(entry) end

local settings = window {
    id = "settings",
    title = page:map(function(name) return "Settings: " .. name end),
    app_id = "org.example.settings",
    min_size = { width = 480, height = 360 },
    visible = open,
    on_close = function() open:set(false) end,
    child = row {
        width = "Fill",
        height = "Fill",
        background = "#1e1e2e",
        children = {
            column { width = 180, height = "Fill", padding = 10, spacing = 2, background = "#181825", children = tabs },
            column { width = "Fill", padding = 24, spacing = 18, children = {
                text { content = page, font_size = 22, font_weight = 700, foreground = "#cdd6f4" },
                group("Appearance", {
                    switch("Dark style", "Use dark colours in every window", true),
                    switch("Animations", "Ease panels and popups in and out", true),
                }),
                group("Clock", {
                    switch("Seconds", "Show seconds in the bar clock", false),
                    switch("24-hour time", "Show 13:00 instead of 1:00 PM", true),
                }),
            } },
        },
    },
}

return { settings }
```

`mantle toggle settings_open` opens it; the compositor's close button or keybind closes it through
`on_close`. The title follows the selected tab.

## Properties

Beyond the [shared properties](index.md#properties-every-role-takes). Every field but `id` and
`on_close` takes a signal and updates the open window in place; a change while it is closed applies when it next
opens.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "Active"` keeps the bare `id` |
| `title` | `string\|Bound` | `""` | The window title |
| `app_id` | `string\|Bound` | `"mantle-{id}"` | What compositor window rules match |
| `min_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size on an axis the compositor leaves to the client ([size](#size)) |
| `max_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size |
| `on_close` | `fun()` | None | The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing |
| `visible` | `boolean\|Bound` | `true` | Opens and closes the window; state and `id` survive |
| `width` | `Length\|Bound`, `[0, 8192]` | Fill the window | The root's size inside the window, not the window's ([size](#size)) |
| `height` | `Length\|Bound`, `[0, 8192]` | Fill the window | As `width` |
| `reset_on_close` | `(StateSignal<any>\|ScrollSignal)[]` | `{}` | `state` and `scroll` handles written back when the surface stops being shown: a state to its `initial`, a scroll to the top ([reset on close](index.md#reset-on-close)) |
| `child` | `Node\|Bound` | None | The one root node; a function `child` is refused |
<!-- End of the generated table. -->

The engine requests server-side decorations and draws none itself. A compositor that insists on
client-side decorations gets an undecorated window, with a log line.

## Size

The window's size is the compositor's configure. The root fills it on each axis where it has no
`width`/`height` of its own; a set one sizes the root inside the window.

| Compositor | Opening size |
| :--- | :--- |
| Tiling (niri, a tiled Hyprland window) | The tile the compositor sends |
| Floating, leaving an axis to the client | A non-zero `min_size` on that axis, else 640×480, clamped by a non-zero `max_size` |

To set a floating window's size or position, use compositor rules on `app_id`, or `min_size` for
the opening size.

## How do I…

| Task | Answer |
| :--- | :--- |
| Open a settings window from a keybind | Bind `visible` to [named state](../guide/signals.md#named-state), as in the example; `mantle toggle settings_open` |
| Close it when the user clicks the close button | `on_close = function() open:set(false) end` |
| Ask before closing | [Confirm before closing](#confirm-before-closing) |
| Make it float, or place it | A Hyprland `windowrule` or niri `window-rule` matching `app_id` |
| Give it a starting size | `min_size`, or a compositor rule |
| Scroll content taller than the window | A `column { height = "Fill", scroll = scroll("name") }` ([scroll](../guide/input.md#scroll)) |
| Close it from a button inside it | Set its `visible` state to `false` from `on_click` |
| Open a menu from it | A [popup](popup.md) with `parent` set to the window's `id` |

### Confirm before closing

`on_close` is a request, so it can open a question instead of closing:

```lua
local open = state("editor_open", true)
local confirming = state("editor_confirm", false)

local editor = window {
    id = "editor",
    title = "Editor",
    visible = open,
    on_close = function() confirming:set(true) end,
    child = column {
        width = "Fill", height = "Fill", padding = 16, spacing = 8, background = "#1e1e2e",
        children = {
            text { content = "Unsaved changes", foreground = "#cdd6f4" },
            row {
                spacing = 8,
                visible = confirming,
                children = {
                    rect { padding = 8, background = "#f38ba8",
                        on_click = function() confirming:set(false); open:set(false) end,
                        children = { text { content = "Discard" } } },
                    rect { padding = 8, background = "#313244",
                        on_click = function() confirming:set(false) end,
                        children = { text { content = "Cancel", foreground = "#cdd6f4" } } },
                },
            },
        },
    },
}

return { editor }
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| The close button does nothing | Add `on_close` and set `visible` to `false` in it |
| `min_size` doesn't stop the root shrinking | It is advisory to the compositor; layout does not enforce it |
| `min_size = { width = 400 }` is refused | Name both axes; `0` leaves one unconstrained |
| `max_size` below `min_size` is refused | Keep every non-zero `max_size` axis at or above `min_size`'s, or `0` |
| `width = 600` on the window doesn't resize it | That sizes the root inside the window; the compositor owns the window's size |
| A click on the window's empty background reaches the window behind it | Put the background on a `"Fill"` child, not the window ([input region](index.md#input-region)) |
| No title bar under a compositor without server-side decorations | The engine draws none; draw your own row, or use compositor rules |

See also: [surfaces](index.md), [popup](popup.md), [nodes](../nodes/index.md),
[signals](../guide/signals.md).

Source: [window spec](../../renderer/src/layout/node/toplevel.rs),
[window](../../renderer/src/wayland/xdg_shell/window.rs),
[root size](../../renderer/src/layout/scene/pass.rs).
