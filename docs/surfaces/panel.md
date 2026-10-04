# panel

A layer-shell surface (`zwlr_layer_surface_v1`) pinned to screen edges: bars, docks, wallpapers,
OSDs, launcher overlays, notification stacks. Rules every role shares are in [surfaces](index.md).

A 32 px bar across the top of every output, reserving its height so windows start below it:

<!-- shot-alt: A top bar: workspace pills with the active one wider and blue on the left, the time and date centred, and Wi-Fi, Bluetooth and volume icons with the battery level on the right. -->
```lua,shot
local function now(format)
    return mantle.system:map(function(system) return system and os.date(format, system.time) or "" end)
end

local function workspace(active)
    return rect { width = active and 20 or 8, height = 8, radius = 4, background = active and "#89b4fa" or "#45475a" }
end

local function status(name)
    return icon { name = name, size = 16, foreground = "#cdd6f4", align_v = "center" }
end

local bar = panel {
    id = "bar",
    layer = "top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    exclusive_zone = true,
    child = row {
        width = "fill",
        height = "fill",
        padding = { left = 14, right = 14 },
        background = "#1e1e2e",
        children = {
            row { spacing = 6, align_v = "center", children = { workspace(false), workspace(true), workspace(false), workspace(false) } },
            rect { width = "fill" },
            row {
                spacing = 8,
                align_v = "center",
                children = {
                    text { content = now("%H:%M"), font_weight = 700, foreground = "#cdd6f4" },
                    text { content = now("%a %d %b"), foreground = "#a6adc8" },
                },
            },
            rect { width = "fill" },
            row {
                spacing = 12,
                align_v = "center",
                children = {
                    status("network-wireless-symbolic"),
                    status("bluetooth-active-symbolic"),
                    status("audio-volume-high-symbolic"),
                    text { content = "82%", foreground = "#a6e3a1", align_v = "center" },
                },
            },
        },
    },
}

return { bar }
```

Both horizontal edges are anchored, so the omitted width fills the root ([size](#size)), and the two
`"fill"` spacers centre the clock. The background sits on the `row`, not the panel, so the whole
bar takes clicks ([input region](index.md#input-region)).

## Properties

Beyond the [shared properties](index.md#properties-every-role-takes). *Structural* fields, the types
without `Bound`, refuse a signal and rebuild the surface when edited; *live* ones take a signal and
update it in place ([reload](index.md#reload-and-structural-fields)).

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id` |
| `layer` | `"background"\|"bottom"\|"top"\|"overlay"` | Required | Stacking level, bottom to top. `"overlay"` draws over fullscreen windows |
| `anchor` | `{ top?: boolean, bottom?: boolean, left?: boolean, right?: boolean }` | All `false` | Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge |
| `output` | `string` | `"all"` | A connector name, `"all"` or `"active"`: which outputs get an instance ([output](#output)) |
| `namespace` | `string` | `"mantle-{id}"` | The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`) |
| `width` | `Length\|Bound`, `[0, 8192]` | Content | The surface's size ([size](#size)) |
| `height` | `Length\|Bound`, `[0, 8192]` | Content | The surface's size ([size](#size)) |
| `exclusive_zone` | `boolean\|integer\|"ignore"\|Bound` | `false` | The space reserved from other windows ([exclusive zones](#exclusive-zones)) |
| `keyboard_interactivity` | `"none"\|"on_demand"\|"exclusive"\|Bound` | `"none"` | Whether it takes the keyboard ([keyboard focus](#keyboard-focus)) |
| `margin` | `number\|Edges\|Bound` | `0` | Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing |
| `visible` | `boolean\|Bound` | `true` | Hiding destroys the layer surface; showing recreates it |
| `child` | `Node\|fun(output: string): Node?\|Bound` | None | The root's content. A function runs per output instance with its connector name; `nil` leaves that instance empty ([per-output child](index.md#per-output-child)) |
| `on_escape` | `fun()` | None | Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins. Never on a surface without `keyboard_interactivity` |
| `reset_on_close` | `(StateSignal<any>\|ScrollSignal)[]` | `{}` | `state` and `scroll` handles written back when the surface stops being shown: a state to its `initial`, a scroll to the top ([reset on close](index.md#reset-on-close)) |
<!-- End of the generated table. -->

## output

| Value | Instances | Notes |
| :--- | :--- | :--- |
| `"all"` | One per output: `bar@DP-1`, `bar@HDMI-A-1` | Follows hotplug |
| `"DP-1"` | One, `bar@DP-1`, while that output is connected | An unknown connector logs a warning and creates nothing |
| `"active"` | One, bare id `bar`, on the output the compositor picks | Picked again at each show, because hiding destroys the object. Refuses `"NN%"` sizes and a function `child`. No instance while no output exists |

Connector names come from [`mantle.screens`](../capabilities/index.md#renderer-members) or `niri msg outputs` /
`hyprctl monitors`.

## Size

| `width`/`height` on an axis | Both edges of that axis anchored | One or no edge anchored |
| :--- | :--- | :--- |
| Omitted | The compositor's span; the root fills it too | Measured from the content |
| `"fill"` | The compositor's span | Protocol error. The panel stays hidden with a warning, or keeps its previous size on a live change |
| px or `"NN%"` | That size | That size |

The table sizes the Wayland surface. On a spanned axis, omitted size also fills its root node.
On an unspanned axis, omission measures the content. To keep a narrower box inside a spanned
surface, put its size and `background` on a child. `max_width`/`max_height` cap the root.

Measured content is capped at the output minus the margins on the anchored edges, and by the root's
`max_width`/`max_height`. Other clients' exclusive zones are not subtracted, so content wider than
the space they leave is clipped by the compositor.

## Exclusive zones

| `exclusive_zone` | Reserves | Covers others' zones |
| :--- | :--- | :--- |
| `false` | Nothing | No, stays inside them |
| `true` | The configured size along the one anchored edge: height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), width for the transposed case. Any other anchor shape (a corner, all four edges) reserves 0 | No |
| Positive integer | That many px whatever the surface's size; for a tall surface whose top strip is the bar | No |
| `"ignore"` | Nothing | Yes |

An integer zone counts from the output edge, so it must include the panel's `margin` on that edge;
`true` reserves the size alone.

## Keyboard focus

Hiding a panel destroys its layer surface and showing creates a fresh one, so a constant
`keyboard_interactivity` applies at every show. Bind it to a signal only to change the mode while
the panel stays shown. This launcher opens on `mantle toggle launcher_open` from a compositor
keybind, on the output the compositor picks, and closes on Escape:

<!-- shot-alt: A launcher overlay dimming the screen: a card with a search field above three apps, the first highlighted. -->
```lua,shot
local open = state("launcher_open", false)
local APPS = {
    { icon = "web-browser", name = "Web Browser" },
    { icon = "folder", name = "Files" },
    { icon = "utilities-terminal", name = "Terminal" },
}

local function result(index, app)
    return row {
        width = "fill",
        padding = 8,
        spacing = 12,
        radius = 8,
        background = index == 1 and "#89b4fa26" or "#00000000",
        children = {
            icon { name = app.icon, size = 24, align_v = "center" },
            text { content = app.name, foreground = "#cdd6f4", align_v = "center" },
        },
    }
end

local results = {}
for index, app in ipairs(APPS) do
    results[index] = result(index, app)
end

local launcher = panel {
    id = "launcher",
    layer = "overlay",
    output = "active",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    visible = open,
    keyboard_interactivity = "exclusive",
    background = "#11111b99",
    child = column {
        width = 420, align_h = "center", align_v = "center",
        padding = 12, spacing = 8, radius = 14, background = "#1e1e2e",
        children = {
            rect {
                width = "fill",
                height = 36,
                padding = { left = 12, right = 12 },
                radius = 8,
                background = "#313244",
                children = {
                    textfield {
                        width = "fill",
                        height = "fill",
                        placeholder = "Search apps",
                        autofocus = true,
                        on_change = function(query) end,
                        on_cancel = function() open:set(false) end,
                    },
                },
            },
            column { width = "fill", spacing = 2, children = results },
        },
    },
}

return { launcher }
```

See [named state](../guide/signals.md#named-state) and [text fields](../guide/input.md#text-fields).

| Mode | Behaviour |
| :--- | :--- |
| `"none"` | Never takes the keyboard |
| `"on_demand"` | Takes it when the user clicks it; other windows can take it back |
| `"exclusive"` | Takes it while mapped on `"top"` or `"overlay"`; nothing else gets keys |

| Compositor behaviour | What the engine does or you do |
| :--- | :--- |
| Hyprland refocuses the last window, onto its workspace, when a still-mapped panel drops to `"none"` | The engine sends no layer requests for a panel hidden in the same pass, so hiding one never drops it to `"none"` first. Hide it with `visible`, not by lowering the mode |
| niri hands an `xdg_popup` the keyboard only if its parent held it when the popup mapped | The engine routes keys that arrive on the parent to the text field in a popup shown under it |
| niri dismisses a grabbing popup when its parent's `keyboard_interactivity` changes | Raise the parent's mode before opening the popup, not from inside it |

## Per-output content

```lua
local wallpaper = panel {
    id = "wallpaper",
    layer = "background",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive_zone = "ignore",
    width = "fill",
    height = "fill",
    child = function(output)
        return image {
            source = state("wallpaper_" .. output, "/usr/share/backgrounds/default.jpg"),
            width = "fill",
            height = "fill",
        }
    end,
}

return { wallpaper }
```

`mantle set wallpaper_DP-1 /path/to/picture.jpg` changes one output's picture
([CLI](../guide/cli.md), [image](../nodes/image.md)). On the `"background"` layer only a node
with a pointer handler takes input ([input region](index.md#input-region)), so the desktop stays click-through.

## OSD

A card bottom-centre on the output the compositor picks, shown for 1.5 s after the level
changes. No `left`/`right` anchor, so the width is measured and the protocol centres it:

<!-- shot-alt: A volume card: a speaker icon, a blue level meter at 70% and the percentage. -->
```lua,shot
local level = state("osd_level", 0.5)
local percent = level:map(function(value) return string.format("%d%%", math.floor(value * 100)) end)

local osd = panel {
    id = "osd",
    layer = "overlay",
    output = "active",
    anchor = { bottom = true },
    margin = { bottom = 96 },
    visible = pulse(level, 1500),
    background = "#1e1e2ee6", radius = 14, padding = 14,
    child = row {
        spacing = 12,
        children = {
            icon { name = "audio-volume-high-symbolic", size = 20, foreground = "#89b4fa", align_v = "center" },
            rect {
                width = 200, height = 8, align_v = "center", radius = 4, background = "#45475a",
                children = { rect { width = percent, height = "fill", radius = 4, background = "#89b4fa" } },
            },
            text { content = percent, width = 36, foreground = "#cdd6f4", align_v = "center" },
        },
    },
}

return { osd }
```

`mantle set osd_level 0.7` shows it. Drive it from a capability by mapping, for example,
`mantle.audio` to the level; see [`pulse`](../guide/signals.md#pulse-mark-a-change). For a slide-in,
animate the root's `translate`, not the surface ([animation](../guide/animation.md)).

## How do I…

| Task | Answer |
| :--- | :--- |
| Put a bar on every monitor | The example at the top |
| Put a bar or dock on one monitor | [Dock on one output](#dock-on-one-output) |
| Open a launcher overlay that takes the keyboard | [Keyboard focus](#keyboard-focus) |
| Close an overlay when the user clicks outside its card | [Close an overlay on an outside click](#close-an-overlay-on-an-outside-click) |
| Show a volume or brightness OSD | [OSD](#osd) |
| Draw a wallpaper per output | [Per-output content](#per-output-content) |
| Stack cards in a screen corner | [Corner stack](#corner-stack) |
| Draw over fullscreen windows | `layer = "overlay"` |
| Hide the bar from a keybind | `visible = state("bar_visible", true)`, then `mantle toggle bar_visible` |
| Reserve only the bar's strip of a taller surface | `exclusive_zone = 32` ([exclusive zones](#exclusive-zones)) |
| Match the panel in compositor rules | `mantle-{id}` or `namespace` in a Hyprland `layerrule` or niri `layer-rule` |

### Dock on one output

Floating 8 px above the bottom edge. The zone is the dock's 54 px plus its 8 px margin;
`exclusive_zone = true` would reserve only the 54 px ([exclusive zones](#exclusive-zones)):

<!-- shot-alt: A dock with a browser, files and terminal icon; a dot under the browser marks it running. -->
```lua,shot
local function app(name, running)
    return column {
        spacing = 2,
        children = {
            icon { name = name, size = 32 },
            rect { width = 4, height = 4, radius = 2, align_h = "center", background = running and "#89b4fa" or "#00000000" },
        },
    }
end

local dock = panel {
    id = "dock",
    layer = "bottom",
    output = "DP-1",
    anchor = { bottom = true },
    margin = { bottom = 8 },
    exclusive_zone = 62,
    background = "#1e1e2e", radius = 14, padding = 8,
    border_width = 1, border_color = "#ffffff14",
    child = row { spacing = 12, children = {
        app("web-browser", true),
        app("folder", false),
        app("utilities-terminal", false),
    } },
}

return { dock }
```

### Close an overlay on an outside click

A full-screen panel with a transparent `rect` as its first child catches clicks everywhere; the
card, declared after it, is on top and takes its own clicks:

```lua
local open = state("overlay_open", false)

local overlay = panel {
    id = "overlay",
    layer = "top",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    visible = open,
    keyboard_interactivity = "on_demand",
    child = rect {
        width = "fill",
        height = "fill",
        children = {
            rect { width = "fill", height = "fill", on_click = function() open:set(false) end },
            column {
                width = 320, padding = 16, radius = 12, background = "#1e1e2e",
                margin = { top = 40, left = 40 },
                children = { text { content = "Quick settings", foreground = "#cdd6f4" } },
            },
        },
    },
}

return { overlay }
```

A `rect` stacks its children, so the card lies over the catcher. A click on the card lands on the
card's own buttons or on nothing; it never reaches the catcher.

### Corner stack

Anchored to two edges, so both axes are measured and the panel grows with its cards:

<!-- shot-alt: Two notification cards stacked in a screen corner, each with a coloured icon, a bold title and a detail line. -->
```lua,shot
local items = state("toasts", {
    { icon = "dialog-information-symbolic", color = "#89b4fa", title = "Build finished", body = "mantle built in 42 s" },
    { icon = "battery-caution-symbolic", color = "#fab387", title = "Battery at 20%", body = "About 1 h left" },
})

local stack = panel {
    id = "toasts",
    layer = "overlay",
    anchor = { top = true, right = true },
    margin = { top = 8, right = 8 },
    child = list {
        source = items,
        spacing = 8,
        itemfn = function(toast)
            return row {
                width = 320, padding = 12, spacing = 12, radius = 12, background = "#1e1e2e",
                children = {
                    icon { name = toast.icon, size = 20, foreground = toast.color, align_v = "center" },
                    column {
                        spacing = 2,
                        children = {
                            text { content = toast.title, font_weight = 700, foreground = "#cdd6f4" },
                            text { content = toast.body, font_size = 12, foreground = "#a6adc8" },
                        },
                    },
                },
            }
        end,
    },
}

return { stack }
```

An empty list still maps a 1×1 px surface; bind `visible` to whether the list has items.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A panel anchored to both sides needs a content-sized background | Put the size and `background` on a child; the omitted panel extent fills its root |
| `"fill"` on an axis with only one edge anchored leaves the panel hidden with a warning in `mantle log` | Anchor both edges of that axis, or give a size |
| `height = "50%"` or a function `child` on an `output = "active"` panel is refused | Use `"fill"` with anchors and margins, or px |
| `exclusive_zone = true` on a corner-anchored panel reserves nothing | Anchor one edge, alone or with both perpendicular edges, or give a px count |
| `exclusive_zone = 0`, `-1` or `32.5` is refused | `false`, `"ignore"`, or a whole px count |
| `margin = { top = 8 }` on a bottom-anchored panel does nothing | The offset applies only to anchored edges |
| Clicks on the bar's empty background reach the window below | The panel's own `background` claims no input; put it on a `"fill"` child covering the root ([input region](index.md#input-region)) |
| On Hyprland, other surfaces stop taking clicks while an `"exclusive"` panel is mapped | Use `"on_demand"` unless the panel must hold every key; it still takes focus when it maps |
| A panel on `output = "HDMI-A-1"` never appears | The name must match a connected output exactly; `mantle log` warns with the connected list |

See also: [surfaces](index.md), [popup](popup.md), [input](../guide/input.md),
[signals](../guide/signals.md), [paint](../guide/paint.md).

Source: [panel spec](../../renderer/src/layout/node/surface.rs),
[instances](../../renderer/src/layout/instance.rs),
[layer shell](../../renderer/src/wayland/layer.rs),
[input region](../../renderer/src/layout/region.rs).
