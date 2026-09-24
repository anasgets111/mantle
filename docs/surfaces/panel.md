# panel

A layer-shell surface (`zwlr_layer_surface_v1`) pinned to screen edges. Use it for anything that is
part of the desktop rather than an application window: bars, docks, wallpapers, OSDs, launcher
overlays and notification stacks. Rules every role shares are in [surfaces](index.md).

```lua
local clock = mantle.system:map(function(system)
    return system and os.date("%H:%M", system.time) or ""
end)

local bar = panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    width = "Fill",
    height = 32,
    exclusive = true,
    child = row {
        width = "Fill",
        height = "Fill",
        padding = { left = 12, right = 12 },
        background = "#1e1e2e",
        children = {
            text { content = "Workspaces", foreground = "#cdd6f4", align_v = "Center" },
            rect { width = "Fill" },
            text { content = clock, foreground = "#cdd6f4", align_v = "Center" },
            rect { width = "Fill" },
            text { content = "Tray", foreground = "#cdd6f4", align_v = "Center" },
        },
    },
}

return { bar }
```

A 32px bar across the top of every output, reserving its height so windows start below it. The
panel's `width = "Fill"` makes the root node as wide as the surface ([size](#size)), and the two
`"Fill"` spacers centre the clock. The background sits on the `row`, not the panel, so the whole
bar takes clicks ([input region](index.md#input-region)).

## Properties

Beyond the [shared properties](index.md#properties-every-role-takes). *Structural* fields, the types
without `Bound`, refuse a signal and rebuild the surface when edited; *live* ones take a signal and
update it in place ([reload](index.md#reload-and-structural-fields)).

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id` |
| `layer` | `"Background"\|"Bottom"\|"Top"\|"Overlay"` | Required | Stacking level, bottom to top. `"Overlay"` draws over fullscreen windows |
| `anchor` | `{ top?: boolean, bottom?: boolean, left?: boolean, right?: boolean }` | All `false` | Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge |
| `monitor` | `string` | `"All"` | A connector name, `"All"` or `"Active"`: which outputs get an instance ([monitor](#monitor)) |
| `namespace` | `string` | `"mantle-{id}"` | The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`) |
| `width` | `Length\|Bound` | Content | The surface's size ([size](#size)) |
| `height` | `Length\|Bound` | Content | The surface's size ([size](#size)) |
| `exclusive` | `boolean\|integer\|"Ignore"\|Bound` | `false` | The space reserved from other windows ([exclusive zones](#exclusive-zones)) |
| `keyboard_interactivity` | `"None"\|"OnDemand"\|"Exclusive"\|Bound` | `"None"` | Whether it takes the keyboard ([keyboard focus](#keyboard-focus)) |
| `margin` | `number\|Edges\|Bound` | `0` | Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing |
| `visible` | `boolean\|Bound` | `true` | Hiding destroys the layer surface; showing recreates it |
| `child` | `Node\|fun(output: string): Node?` | None | The root's content. A function runs per output instance with its connector name; `nil` leaves that instance empty ([per-output child](index.md#per-output-child)) |
<!-- End of the generated table. -->

## monitor

| Value | Instances | Notes |
| :--- | :--- | :--- |
| `"All"` | One per output: `bar@DP-1`, `bar@HDMI-A-1` | Follows hotplug |
| `"DP-1"` | One, `bar@DP-1`, while that output is connected | An unknown connector logs a warning and creates nothing |
| `"Active"` | One, bare id `bar`, on the output the compositor picks | Picked again at each show, because hiding destroys the object. Refuses `"NN%"` sizes and a function `child`. No instance while no output exists |

Connector names come from [`mantle.screens`](../capabilities/index.md#renderer-members) or `niri msg outputs` /
`hyprctl monitors`.

## Size

| `width`/`height` on an axis | Both edges of that axis anchored | One or no edge anchored |
| :--- | :--- | :--- |
| Omitted | The compositor's span, same as `"Fill"` | Measured from the content |
| `"Fill"` | The compositor's span | Protocol error. The panel stays hidden with a warning, or keeps its previous size on a live change |
| px or `"NN%"` | That size | That size |

The table sizes the Wayland surface. The root node inside it is laid out like any
[node](../nodes/index.md): omitted means content-sized even when the surface spans the output. On a
spanned axis write `"Fill"` on the panel so the root, its `background` and its `"Fill"` children
cover the surface.

Measured content is capped at the output minus the margins on the anchored edges, and by the root's
`max_width`/`max_height`. Other clients' exclusive zones are not subtracted, so content wider than
the space they leave is clipped by the compositor.

## Exclusive zones

| `exclusive` | Reserves | Covers others' zones |
| :--- | :--- | :--- |
| `false` | Nothing | No, stays inside them |
| `true` | The configured size along the one anchored edge: height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), width for the transposed case. Any other anchor shape (a corner, all four edges) reserves 0 | No |
| Positive integer | That many px whatever the surface's size; for a tall surface whose top strip is the bar | No |
| `"Ignore"` | Nothing | Yes |

`0`, `-1` and fractional numbers are refused; spell them `false` and `"Ignore"`.

## Keyboard focus

`keyboard_interactivity` is live, so bind it to the same signal that shows the panel:

```lua
local open = state("launcher_open", false)

local launcher = panel {
    id = "launcher",
    layer = "Overlay",
    monitor = "Active",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    visible = open,
    keyboard_interactivity = "Exclusive",
    background = "#11111b99",
    child = column {
        width = 480, align_h = "Center", align_v = "Center",
        padding = 16, radius = 12, background = "#1e1e2e",
        children = {
            textfield {
                width = "Fill",
                height = 32,
                placeholder = "Search",
                autofocus = true,
                on_change = function(query) end,
                on_cancel = function() open:set(false) end,
            },
        },
    },
}

return { launcher }
```

`mantle toggle launcher_open` from a compositor keybind opens it on the output the compositor
picks; Escape closes it. See [named state](../guide/signals.md#named-state) and
[text fields](../guide/input.md#text-fields).

| Mode | Behaviour |
| :--- | :--- |
| `"None"` | Never takes the keyboard |
| `"OnDemand"` | Takes it when the user clicks it; other windows can take it back |
| `"Exclusive"` | Takes it while mapped on `Top` or `Overlay`; nothing else gets keys |

| Compositor behaviour | What the engine does or you do |
| :--- | :--- |
| Hyprland refocuses the last window, onto its workspace, when a still-mapped panel drops to `"None"` | The engine skips layer requests for a panel being hidden in the same pass. Change `visible` and `keyboard_interactivity` together |
| niri hands an `xdg_popup` the keyboard only if its parent held it when the popup mapped | The engine routes keys that arrive on the parent to the text field in a popup shown under it |
| niri dismissed a grabbing popup when its parent's `keyboard_interactivity` changed | Raise the parent's mode before opening the popup, not from inside it |

## Per-output content

```lua
local wallpaper = panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = "Ignore",
    width = "Fill",
    height = "Fill",
    child = function(output)
        return image {
            source = state("wallpaper_" .. output, "/usr/share/backgrounds/default.jpg"),
            width = "Fill",
            height = "Fill",
        }
    end,
}

return { wallpaper }
```

`mantle set wallpaper_DP-1 /path/to/picture.jpg` changes one output's picture
([CLI](../guide/cli.md), [image](../nodes/image.md)). On the `Background` layer only a `button`
with a handler takes input ([input region](index.md#input-region)), so the desktop stays click-through.

## OSD

A card bottom-centre on the output the compositor picks that shows for 1.5 s after the level
changes. No `left`/`right` anchor, so the width is measured and the protocol centres it:

```lua
local level = state("osd_level", 0.5)

local osd = panel {
    id = "osd",
    layer = "Overlay",
    monitor = "Active",
    anchor = { bottom = true },
    margin = { bottom = 96 },
    visible = pulse(level, 1500),
    background = "#1e1e2ee6", radius = 12, padding = 12,
    child = row {
        spacing = 12,
        children = {
            text { content = "Volume" },
            rect {
                width = 200, height = 8, align_v = "Center", radius = 4, background = "#45475a",
                children = {
                    rect {
                        width = level:map(function(value) return string.format("%d%%", math.floor(value * 100)) end),
                        height = "Fill", radius = 4, background = "#89b4fa",
                    },
                },
            },
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
| Draw over fullscreen windows | `layer = "Overlay"` |
| Hide the bar from a keybind | `visible = state("bar_visible", true)`, then `mantle toggle bar_visible` |
| Reserve only the bar's strip of a taller surface | `exclusive = 32` ([exclusive zones](#exclusive-zones)) |
| Match the panel in compositor rules | `mantle-{id}` or `namespace` in a Hyprland `layerrule` or niri `layer-rule` |

### Dock on one output

Floating 8px above the bottom edge, with a fixed 56px zone because its own height is measured:

```lua
local dock = panel {
    id = "dock",
    layer = "Bottom",
    monitor = "DP-1",
    anchor = { bottom = true },
    margin = { bottom = 8 },
    exclusive = 56,
    background = "#1e1e2e", radius = 12, padding = 8,
    child = row { spacing = 8, children = { text { content = "Files" }, text { content = "Terminal" } } },
}

return { dock }
```

### Close an overlay on an outside click

A full-screen panel with a transparent `button` as its first child catches clicks everywhere; the
card, declared after it, is on top and takes its own clicks:

```lua
local open = state("overlay_open", false)

local overlay = panel {
    id = "overlay",
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    visible = open,
    keyboard_interactivity = "OnDemand",
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            button { width = "Fill", height = "Fill", on_click = function() open:set(false) end },
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

```lua
local items = state("toasts", { "Build finished", "Battery at 20%" })

local stack = panel {
    id = "toasts",
    layer = "Overlay",
    anchor = { top = true, right = true },
    margin = { top = 8, right = 8 },
    child = list {
        source = items,
        spacing = 8,
        itemfn = function(message)
            return rect {
                width = 320, padding = 12, radius = 12, background = "#1e1e2e",
                children = { text { content = message, foreground = "#cdd6f4" } },
            }
        end,
    },
}

return { stack }
```

An empty list measures 0×0; hide the panel with `visible` when there is nothing to show.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A panel anchored to both sides shows its background only behind its content | Omitted size spans the surface but not the root node; set `width = "Fill"` (or `height`) on the panel |
| `"Fill"` on an axis with only one edge anchored leaves the panel hidden with a warning in `mantle log` | Anchor both edges of that axis, or give a size |
| `height = "50%"` or a function `child` on a `monitor = "Active"` panel is refused | Use `"Fill"` with anchors and margins, or px |
| `exclusive = true` on a corner-anchored panel reserves nothing | Anchor one edge, alone or with both perpendicular edges, or give a px count |
| `exclusive = 0` is refused | `false` |
| `margin = { top = 8 }` on a bottom-anchored panel does nothing | The offset applies only to anchored edges |
| Clicks on the bar's empty background reach the window below | The panel's own `background` claims no input; put it on a `"Fill"` child of a `"Fill"` panel ([input region](index.md#input-region)) |
| On Hyprland, other surfaces stop taking clicks while an `"Exclusive"` panel is mapped | Use `"OnDemand"` unless the panel must hold every key; it still takes focus when it maps |
| A panel on `monitor = "HDMI-A-1"` never appears | The name must match a connected output exactly; `mantle log` warns with the connected list |

See also: [surfaces](index.md), [popup](popup.md), [input](../guide/input.md),
[signals](../guide/signals.md), [paint](../guide/paint.md).

Source: [panel spec](../../renderer/src/layout/node/surface.rs),
[instances](../../renderer/src/layout/instance.rs),
[layer shell](../../renderer/src/wayland/layer.rs),
[input region](../../renderer/src/layout/region.rs).
