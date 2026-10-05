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
        width = "fill",
        padding = { left = 10, right = 12, top = 8, bottom = 8 },
        spacing = 10,
        radius = 8,
        background = selected:map(function(on) return on and "#89b4fa26" or "#00000000" end),
        on_click = function() page:set(entry.name) end,
        children = {
            icon { name = entry.icon, size = 16, align_v = "center",
                   foreground = selected:map(function(on) return on and "#89b4fa" or "#a6adc8" end) },
            text { content = entry.name, foreground = "#cdd6f4", align_v = "center" },
        },
    }
end

local function switch(label, detail, on)
    return row {
        width = "fill",
        padding = 12,
        spacing = 12,
        children = {
            column { width = "fill", spacing = 2, align_v = "center", children = {
                text { content = label, foreground = "#cdd6f4" },
                text { content = detail, font_size = 12, foreground = "#a6adc8" },
            } },
            rect {
                width = 36, height = 20, radius = 10, align_v = "center",
                background = on and "#89b4fa" or "#45475a",
                children = { rect { width = 14, height = 14, radius = 7, margin = { left = 3, right = 3 },
                    align_h = on and "end" or "start", align_v = "center", background = on and "#ffffff" or "#bac2de" } },
            },
        },
    }
end

local function group(title, rows)
    local divided = {}
    for index, item in ipairs(rows) do
        if index > 1 then divided[#divided + 1] = rect { width = "fill", height = 1, margin = { left = 12, right = 12 }, background = "#313244" } end
        divided[#divided + 1] = item
    end
    return column { width = "fill", spacing = 10, children = {
        text { content = title, font_size = 12, font_weight = 700, foreground = "#89b4fa", margin = { left = 4 } },
        column { width = "fill", radius = 10, background = "#181825", children = divided },
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
        width = "fill",
        height = "fill",
        background = "#1e1e2e",
        children = {
            column { width = 180, height = "fill", padding = 10, spacing = 2, background = "#181825", children = tabs },
            column { width = "fill", padding = 24, spacing = 18, children = {
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
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id` |
| `title` | `string\|Bound` | `""` | The window title |
| `app_id` | `string\|Bound` | `"mantle-{id}"` | What compositor window rules match |
| `min_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size on an axis the compositor leaves to the client ([size](#size)) |
| `max_size` | `{ width: number, height: number }\|Bound`, `[0, 8192]` | None | Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size |
| `decorations` | `"server"\|"client"\|Bound` | `"server"` | Who draws the window's frame: `"server"` asks the compositor for its decorations, `"client"` leaves the frame to the app. A compositor without `zxdg_decoration_manager_v1` always leaves it to the app; `toplevel(id):state().decoration` says which was chosen |
| `geometry_inset` | `number\|Edges\|Bound`, `[0, 256]` | `0` | Room around the frame for a client-drawn shadow; the compositor sizes and tiles by the frame alone. A number sets all four edges ([client-side decoration](#client-side-decoration)) |
| `on_close` | `fun()` | None | The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing |
| `visible` | `boolean\|Bound` | `true` | Opens and closes the window; state and `id` survive |
| `width` | `Length\|Bound`, `[0, 8192]` | Fill the window | The root's size inside the window, not the window's ([size](#size)) |
| `height` | `Length\|Bound`, `[0, 8192]` | Fill the window | As `width` |
| `on_escape` | `fun()` | None | Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins, with no order promised among sibling popups. Never on a surface without `keyboard_interactivity` |
| `reset_on_close` | `(StateSignal<any>\|ScrollSignal)[]` | `{}` | `state` and `scroll` handles written back when the surface stops being shown: a state to its `initial`, a scroll to the top ([reset on close](index.md#reset-on-close)) |
| `child` | `Node\|Bound` | None | The one root node; a function `child` is refused |
<!-- End of the generated table. -->

By default the engine requests server-side decorations and draws none itself. `decorations = "client"`
asks for none, and a compositor without `zxdg_decoration_manager_v1` (or one that insists on its
choice) decides regardless: the mode it chose is `toplevel(id):state().decoration`. Changing the
property on a reload asks again.

## Size

The window's size is the compositor's configure. The root fills it on each axis where it has no
`width`/`height` of its own; a set one sizes the root inside the window. A `geometry_inset` grows
the surface past that size on each side ([client-side decoration](#client-side-decoration)).

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
| Scroll content taller than the window | A `column { height = "fill", scroll = scroll("name") }` ([scroll](../guide/input.md#scroll)) |
| Close it from a button inside it | Set its `visible` state to `false` from `on_click` |
| Draw my own title bar | [Custom title bar](#custom-title-bar) |
| Draw a shadow and rounded corners around my own frame | [Client-side decoration](#client-side-decoration) |
| Open a menu from it | A [popup](popup.md) with `parent` set to the window's `id` |

### Custom title bar

An app that draws its own frame asks the compositor to run the pointer: `toplevel(id)` takes a
`window`'s `id` and has three request methods, called from an [`on_press`](../guide/input.md) (or an
`on_drag` `"start"`), and `:state()` ([window state](#window-state)).

| Method | Does |
| :--- | :--- |
| `:move()` | Starts an interactive move, as dragging a title bar does |
| `:resize(edge)` | Starts a resize from `"top"`, `"bottom"`, `"left"`, `"right"`, `"top_left"`, `"top_right"`, `"bottom_left"` or `"bottom_right"`; any other value raises |
| `:show_menu()` | Opens the compositor's window menu at the press position |

Compositors honour these only for the serial of the press that started them, so a call from
`on_click`, a timer or any other callback, on a hidden window, or on a press that landed on a
different surface logs a warning and sends nothing. After a request the compositor owns the
pointer and may send no release: the engine drops the drag and the armed click, so `on_drag` gets no
`"end"` and `on_click` does not fire. There is no minimize, maximize or fullscreen request; the
compositor's own bindings still do those.

```lua
local frame = toplevel("main")
local GRIP = 6

local function grip(edge, props)
    props.on_press = function(_, button)
        if button == "left" then frame:resize(edge) end
    end
    return rect(props)
end

return {
    window {
        id = "main",
        title = "Notes",
        child = column {
            width = "fill", height = "fill", background = "#1e1e2e",
            children = {
                grip("top", { width = "fill", height = GRIP, cursor = "n-resize" }),
                row {
                    width = "fill", height = 32, padding = { left = 12, right = 12 }, background = "#181825",
                    on_press = function(_, button)
                        if button == "left" then frame:move() else frame:show_menu() end
                    end,
                    children = { text { content = "Notes", foreground = "#cdd6f4", align_v = "center" } },
                },
                grip("bottom_right", { width = GRIP, height = GRIP, align_h = "end", cursor = "se-resize" }),
            },
        },
    },
}
```

### Window state

`toplevel(id):state()` is a read-only signal of what the compositor last configured the window to
be, so the app's own frame can follow it. It is rewritten only when a configure changes a value.
Before the first configure and after the window closes it holds the default: every flag `false`,
no `bounds`, every capability `true`, `decoration = "client"`. The engine sends no request to change
these; maximizing and fullscreen stay with the compositor's bindings.

| Key | Type | Meaning |
| :--- | :--- | :--- |
| `activated`, `maximized`, `fullscreen`, `resizing` | `boolean` | The `xdg_toplevel` state flags |
| `tiled` | `{ left, right, top, bottom }` | The edges a tiling compositor tiled the window against |
| `bounds` | `{ width, height }` or `nil` | The most room the compositor suggests, in logical pixels |
| `capabilities` | `{ window_menu, maximize, fullscreen, minimize }` | What the compositor supports; all `true` when it never says |
| `decoration` | `"server"` or `"client"` | The mode the compositor chose |

```lua
local focused = toplevel("main"):state():map(function(s) return s.activated end)

return {
    window {
        id = "main",
        title = "Notes",
        decorations = "client",
        child = rect {
            width = "fill", height = "fill",
            background = focused:map(function(on) return on and "#1e1e2e" or "#313244" end),
        },
    },
}
```

### Client-side decoration

A window that draws its own frame can also draw its shadow and rounded corners outside it.
`geometry_inset` adds a band around the window's frame: the buffer is the frame plus the band, the
root fills the whole buffer, and the compositor sizes, tiles and snaps by the frame alone
(`xdg_surface.set_window_geometry`). Configure sizes, `min_size` and `max_size` are all the
frame's. Put the frame inside the band with `padding` on a transparent root, and its `shadows` and
`radius` draw into the band.

Compositors expect no shadow on a maximized, fullscreen or tiled edge. The engine does not guess:
bind `geometry_inset` to the [window state](#window-state) and zero those edges.

```lua
local frame = toplevel("main")
local SHADOW = 24

local function band(s)
    if s.maximized or s.fullscreen then return 0 end
    return {
        left = s.tiled.left and 0 or SHADOW, right = s.tiled.right and 0 or SHADOW,
        top = s.tiled.top and 0 or SHADOW, bottom = s.tiled.bottom and 0 or SHADOW,
    }
end
local inset = frame:state():map(band)
local floating = frame:state():map(function(s) return band(s) ~= 0 end)

return {
    window {
        id = "main",
        title = "Notes",
        decorations = "client",
        geometry_inset = inset,
        child = column {
            width = "fill", height = "fill", padding = inset,
            children = {
                column {
                    width = "fill", height = "fill", background = "#1e1e2e",
                    radius = floating:map(function(on) return on and 10 or 0 end),
                    shadows = { { color = "#00000080", blur = 18, offset = { x = 0, y = 4 } } },
                    children = {
                        row {
                            width = "fill", height = 32, padding = { left = 12 }, background = "#181825",
                            on_press = function(_, button)
                                if button == "left" then frame:move() else frame:show_menu() end
                            end,
                            children = { text { content = "Notes", foreground = "#cdd6f4", align_v = "center" } },
                        },
                    },
                },
            },
        },
    },
}
```

The band takes no pointer input until something in it does: the [input region](index.md#input-region)
follows content, and a shadow is not content. Resize handles go where the config puts them, inside
the frame's edge as in the [custom title bar](#custom-title-bar), or in the band as transparent
nodes with an `on_press` calling `:resize(edge)`.

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
        width = "fill", height = "fill", padding = 16, spacing = 8, background = "#1e1e2e",
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
| A click on the window's empty background reaches the window behind it | Put the background on a `"fill"` child, not the window ([input region](index.md#input-region)) |
| No title bar under a compositor without server-side decorations | The engine draws none; draw your own row ([custom title bar](#custom-title-bar)), or use compositor rules |
| `decorations = "server"` still leaves a bare window | The compositor has no `zxdg_decoration_manager_v1` or chose client-side; check `toplevel(id):state().decoration` |
| `toplevel("main"):move()` warns and does nothing | Call it from `on_press`, not `on_click`; the press serial is gone by release |

See also: [surfaces](index.md), [popup](popup.md), [nodes](../nodes/index.md),
[signals](../guide/signals.md).

Source: [window spec](../../renderer/src/layout/node/toplevel.rs),
[window](../../renderer/src/wayland/xdg_shell/window.rs),
[root size](../../renderer/src/layout/scene/pass.rs).
