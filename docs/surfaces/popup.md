# popup

An `xdg_popup` attached to a shown `panel`, `window` or another `popup`. The compositor positions it
relative to a rectangle in the parent, keeps it on screen and dismisses it on an outside click,
which is why a dropdown, context menu or tooltip is a popup and not a second panel. No Wayland
object exists while it is hidden. Rules every role shares are in [surfaces](index.md).

```lua
local menu_open = state("menu_open", false)
local menu_anchor = state("menu_anchor", { x = 0, y = 0, width = 1, height = 1 })

local bar = panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    width = "Fill",
    height = 32,
    exclusive = true,
    child = row {
        width = "Fill", height = "Fill", background = "#1e1e2e",
        children = {
            button {
                padding = 8,
                on_click = function(rect)
                    menu_anchor:set(rect)
                    menu_open:set(not menu_open:get())
                end,
                children = { text { content = "Menu", foreground = "#cdd6f4" } },
            },
        },
    },
}

local menu = popup {
    id = "menu",
    parent = "bar",
    anchor_rect = menu_anchor,
    anchor = "Bottom",
    gravity = "Bottom",
    offset = { y = 4 },
    visible = menu_open,
    on_dismiss = function() menu_open:set(false) end,
    child = column {
        padding = 8, spacing = 4, radius = 8, background = "#1e1e2e",
        children = {
            text { content = "Settings", foreground = "#cdd6f4" },
            text { content = "Log out", foreground = "#cdd6f4" },
        },
    },
}

return { bar, menu }
```

A dropdown under a bar button. `on_click`'s `rect` is the button in its surface's coordinates,
exactly what `anchor_rect` wants ([pointer input](../guide/input.md#pointer)). An outside click
dismisses it and `on_dismiss` clears the state.

## Properties

Beyond the [shared properties](index.md#properties-every-role-takes). All fields but `parent` are
live.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `id` | `string` | Required | The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id` |
| `parent` | `string` | Required | The `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first. A change applies at the next open; a `lock` cannot be a parent |
| `anchor_rect` | `Rect\|Bound` | Required | In the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes |
| `anchor` | `"Top"\|"Bottom"\|"Left"\|"Right"\|"TopLeft"\|"TopRight"\|"BottomLeft"\|"BottomRight"\|"Center"\|Bound` | `"Center"` | The point on `anchor_rect` the popup hangs from |
| `gravity` | `"Top"\|"Bottom"\|"Left"\|"Right"\|"TopLeft"\|"TopRight"\|"BottomLeft"\|"BottomRight"\|"Center"\|Bound` | `"Center"` | The direction it extends from that point: `"Bottom"` hangs it below, `"BottomRight"` below and to the right |
| `constraint_adjustment` | `("SlideX"\|"SlideY"\|"FlipX"\|"FlipY"\|"ResizeX"\|"ResizeY")[]\|Bound` | `{ "FlipY", "SlideX" }` | How the compositor may keep it on screen; `{}` for none, order is ignored |
| `offset` | `{ x?: number, y?: number }\|Bound` | `{ x = 0, y = 0 }` | Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left |
| `width` | `number\|Bound` | Content | Pixels in `(0, 8192]`; no `"Fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+) |
| `height` | `number\|Bound` | Content | As `width`; each axis is independent |
| `grab` | `boolean\|Bound` | `true` | Takes an input grab so an outside click dismisses it ([grab](#grab)). `false` for a tooltip |
| `on_dismiss` | `fun()` | None | The compositor closed it (click outside, denied grab, parent gone); not called when the config hides it. Set `visible = false` here, or it reopens on the next click |
| `visible` | `boolean\|Bound` | `true` | Opens and closes the popup; state and `id` survive |
| `child` | `Node` | None | The one root node; a function `child` is refused |
<!-- End of the generated table. -->

## Placement

While a popup is open, a change to its measured size or any positioner field moves it through
`xdg_popup.reposition`. On a compositor with `xdg_popup` below version 3 it keeps the size and
place it opened at until it closes, with a warning.

When `parent` names a per-output panel, the popup opens on the instance the arming click landed
on, else the first instance. A popup whose parent is hidden does not open; hiding the parent closes
it.

## Grab

A grab needs a pointer press or release in the same turn: open a grabbing popup from `on_click`.
Without one the popup is not opened and a warning is logged. A compositor that denies the grab
dismisses the popup.

## Dismissal

A compositor dismissal destroys the popup but leaves your `visible` signal `true`. The engine
latches it shut until the next pointer press or release, then opens it again. Clear your state in
`on_dismiss`, as the example at the top does.

Hiding or dismissing a popup also destroys the popups open under it and latches them as if
dismissed, so each reopens on the next pointer press while its own `visible` stays true.

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a dropdown under a bar button | The example at the top |
| Open a submenu from a menu | [Nested menus](#nested-menus) |
| Show a tooltip on hover | [Tooltip](#tooltip) |
| Anchor to a node without clicking or hovering it | [Anchor to a node's geometry](#anchor-to-a-nodes-geometry) |
| Open a context menu on right click | `on_click = function(rect, which) if which == "right" then ... end end` ([pointer](../guide/input.md#pointer)) |
| Fade it out before it closes | Keep `visible` true with `delay` while the child's `opacity` animates ([delay](../guide/signals.md#delay-hold-a-value)) |
| Open it from a keybind | `grab = false`, since a keybind is no pointer press; it then stays until the config hides it |
| Keep it on screen near an edge | `constraint_adjustment = { "FlipX", "FlipY", "SlideX", "SlideY" }` |
| Give it a fixed size | `width` and `height` in px |
| Open it from a window | `parent = "<window id>"` |

### Nested menus

A popup can parent another popup. Here the submenu hangs off the right edge of the clicked row and
flips left near the screen edge:

```lua
local sub_open = state("sub_open", false)
local sub_anchor = state("sub_anchor", { x = 0, y = 0, width = 1, height = 1 })

local power_row = button {
    padding = 4,
    on_click = function(rect)
        sub_anchor:set(rect)
        sub_open:set(true)
    end,
    children = { text { content = "Power ›" } },
}

local submenu = popup {
    id = "power_menu",
    parent = "menu",
    anchor_rect = sub_anchor,
    anchor = "TopRight",
    gravity = "BottomRight",
    constraint_adjustment = { "FlipX", "SlideY" },
    visible = sub_open,
    on_dismiss = function() sub_open:set(false) end,
    background = "#1e1e2e", padding = 8,
    child = column { spacing = 4, children = { text { content = "Suspend" }, text { content = "Reboot" } } },
}
```

`power_row` goes inside the `menu` popup. `on_click`'s rect is in the menu's own coordinates, which
is what a child popup's `anchor_rect` expects. Set `sub_open` to `false` wherever the menu closes
(its `on_dismiss` included), or the submenu reopens on the next click.

### Tooltip

A hover opens no grab, so `grab = false`. [`hover_rect`](../guide/input.md#hover) tracks the
hovered node and is 1×1 before the first hover, which keeps `anchor_rect` valid:

```lua
local clock = text { content = "12:30", padding = 8, hover = hover("clock") }

local tooltip = popup {
    id = "clock_tooltip",
    parent = "bar",
    anchor_rect = hover_rect("clock"),
    anchor = "Bottom",
    gravity = "Bottom",
    offset = { y = 4 },
    grab = false,
    visible = hover("clock"),
    background = "#1e1e2e", radius = 6, padding = 6,
    child = text { content = "Thursday, 24 September" },
}
```

`clock` goes inside the `bar` panel.

### Anchor to a node's geometry

Bind [`geometry`](../guide/signals.md#geometry-read-a-nodes-laid-out-rect). It reads zero before
the first layout, so map it to a 1×1 fallback:

```lua
local battery = text { content = "87%", padding = 8, geometry = geometry("battery") }

local details = popup {
    id = "battery_details",
    parent = "bar",
    anchor_rect = geometry("battery"):map(function(rect)
        return rect.width > 0 and rect or { x = 0, y = 0, width = 1, height = 1 }
    end),
    anchor = "Bottom",
    gravity = "Bottom",
    visible = state("battery_open", false),
    child = text { content = "2 h 10 min left" },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `anchor_rect` with a zero `width` or `height` is refused (a `geometry` before first layout, a hand-built rect) | Fall back to `{ x = 0, y = 0, width = 1, height = 1 }` |
| A dropdown reopens on the next click after an outside click closed it | Set its `visible` state to `false` in `on_dismiss` |
| A popup with `visible` bound to startup-true state never opens | `grab = true` needs a click; open it from `on_click`, or set `grab = false` |
| A popup whose parent is hidden does not open | Show the parent first; the popup opens on the next pass |
| A submenu reopens after its menu closed | Clear the submenu's state wherever the menu closes, `on_dismiss` included |
| `width = "Fill"` or `"50%"` is refused | px, or omit it to size to the content |
| `anchor_rect` from a click in a popup is placed wrong on the bar | A rect is in its own surface's coordinates; anchor a popup only to rects from its `parent` |
| An open tooltip does not grow with its text on an old compositor | `xdg_popup` below version 3 cannot reposition; it keeps its opening size until it closes |

See also: [surfaces](index.md), [panel](panel.md), [input](../guide/input.md),
[signals](../guide/signals.md).

Source: [popup spec](../../renderer/src/layout/node/toplevel.rs),
[popup](../../renderer/src/wayland/xdg_shell/popup.rs),
[instances](../../renderer/src/layout/instance.rs).
