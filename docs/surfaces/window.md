# window

An `xdg_toplevel`: an application window the compositor places, tiles, decorates and closes. Use it
for a settings window or a dialog; use a [panel](panel.md) for anything pinned to the desktop.
Rules every role shares are in [surfaces](index.md).

```lua,shot
local open = state("settings_open", false)
local page = state("settings_page", "General")

local function tab(name)
    return button {
        width = "Fill",
        padding = { left = 12, right = 12, top = 8, bottom = 8 },
        radius = 8,
        background = page:map(function(current) return current == name and "#313244" or "#00000000" end),
        on_click = function() page:set(name) end,
        children = { text { content = name, foreground = "#cdd6f4" } },
    }
end

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
            column { width = 160, height = "Fill", padding = 8, spacing = 4, background = "#181825",
                children = { tab("General"), tab("Display"), tab("Sound"), tab("Power") } },
            column { width = "Fill", padding = 24,
                children = { text { content = page, font_size = 20, foreground = "#cdd6f4" } } },
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
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id` |
| `title` | `string\|Bound` | `""` | The window title |
| `app_id` | `string\|Bound` | `"mantle-{id}"` | What compositor window rules match |
| `min_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size on an axis the compositor leaves to the client ([size](#size)) |
| `max_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size |
| `on_close` | `fun()` | None | The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing |
| `visible` | `boolean\|Bound` | `true` | Opens and closes the window; state and `id` survive |
| `width` | `Length\|Bound`, `[0, 8192]` | Fill the window | The root's size inside the window, not the window's ([size](#size)) |
| `height` | `Length\|Bound`, `[0, 8192]` | Fill the window | As `width` |
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
                    button { padding = 8, background = "#f38ba8",
                        on_click = function() confirming:set(false); open:set(false) end,
                        children = { text { content = "Discard" } } },
                    button { padding = 8, background = "#313244",
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
