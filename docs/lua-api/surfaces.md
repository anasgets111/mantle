# Surfaces

A surface is a top-level Wayland object that holds one node tree. `shell.lua` returns one surface or
a list of them, and every other node lives under some surface's `child`. Pick the role by what the
compositor must do with it:

| You are building | Role | Protocol | Instances |
| :--- | :--- | :--- | :--- |
| Bar, dock, wallpaper, OSD, launcher overlay, notification stack | [`panel`](#panel) | `zwlr_layer_surface_v1` | One per matched output, id `id@output` |
| Settings window, dialog the user can move, tile or close | [`window`](#window) | `xdg_toplevel` | One, id `id` |
| Dropdown, context menu, tooltip hanging off a panel or window | [`popup`](#popup) | `xdg_popup` | One, id `id` |
| Lock screen | [`lock`](#lock) | `ext_session_lock_surface_v1` | One per output, id `id@output` |

An *instance* is one mapped copy of a declared surface; its id keys the retained scene and names
the surface in `mantle log` ([CONTEXT](../../CONTEXT.md#surfaces)).

```lua
local bar = panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    exclusive = true,
    width = "Fill",
    background = "#1e1e2e",
    child = function(output)
        return row {
            width = "Fill",
            height = "Fill",
            padding = { left = 12, right = 12 },
            children = { text { content = output, foreground = "#cdd6f4", align_v = "Center" } },
        }
    end,
}

return { bar }
```

A 32px bar on every output that pushes windows down by its height and prints the output's
connector name.

## How do I…

| Task | Recipe |
| :--- | :--- |
| Put a bar on every monitor | The example above |
| Put a bar or dock on one monitor | [monitor](#monitor) |
| Open a launcher overlay that takes the keyboard | [Keyboard focus](#keyboard-focus) |
| Show a volume or brightness OSD | [OSD](#osd) |
| Draw a wallpaper per output | [Per-output content](#per-output-content) |
| Open a settings window | [window](#window) |
| Show a dropdown under a bar button | [Dismissal](#dismissal) |
| Open a submenu from a menu | [Nested menus](#nested-menus) |
| Show a tooltip on hover | [Tooltip](#tooltip) |
| Build a lock screen | [lock](#lock) |

## Shared rules

Every surface takes `id` (required, a string) and one `child` node. It also takes the
[common](nodes.md#common-properties) and [box](nodes.md#box-properties) node properties, so a
surface root has its own `background`, `radius`, `padding` and [paint](paint.md). A key no list
names is refused.

| Rule | Behavior |
| :--- | :--- |
| Return value | One surface, a list of them, `{}` or nothing. Any other top-level node is refused |
| Reload | The returned list is re-read on every [reload](runtime.md#evaluation-reload-and-generations). A surface whose fingerprint is unchanged keeps its Wayland objects; a missing one is destroyed; a new one is created |
| Fingerprint | `panel`: `id`, `layer`, `anchor`, `monitor`, `namespace`. `window`, `popup`, `lock`: `id` alone. A changed fingerprint destroys and recreates that surface's objects under the same instance ids |
| Structural fields | `id`, `layer`, `anchor`, `monitor`, `namespace` and a popup's `parent` refuse a signal: they are read once per evaluation. Change them by editing the file |
| Live fields | Everything else takes a signal and updates the existing object in place |
| Invalid live value | A signal that resolves to a bad value logs a warning and keeps the last applied spec |
| `visible` | Creates and destroys the protocol object, not a hidden map. Retained state and `id` survive. Default `true`; `lock` refuses it |
| `id` | Keep it unique across surfaces. The engine does not check duplicates |
| Hotplug | An output change re-evaluates the config and adds or removes `panel` and `lock` instances; instances on other outputs keep their objects |

`child = function(output)` is for `panel` and `lock`, the roles with one instance per output. It
runs on every layout pass with that instance's connector name (`"DP-1"`), so keep it cheap and key
per-output state by name: `state("wallpaper_" .. output, ...)`. Returning `nil` leaves that output's
instance empty. A `window`, `popup` or `monitor = "Active"` panel has no output name and refuses a
function `child`.

## panel

A layer-shell surface pinned to screen edges. Use it for anything that is part of the desktop
rather than an application window.

| Property | Values | Default | Kind |
| :--- | :--- | :--- | :--- |
| `layer` | `"Background"`, `"Bottom"`, `"Top"`, `"Overlay"` | Required | Structural |
| `anchor` | `{ top?, bottom?, left?, right? }` booleans; an absent edge is `false`. None anchored centres the surface | All `false` | Structural |
| `monitor` | A connector name, `"All"`, or `"Active"` | `"All"` | Structural |
| `namespace` | String; compositor layer rules (Hyprland `layerrule`) match it | `"mantle-<id>"` | Structural |
| `width`, `height` | px, `"NN%"` of the output, `"Fill"`, or omitted to measure the content (capped by the output less the anchored edges' margins, and by `max_width`/`max_height`) | Content | Live |
| `exclusive` | See [exclusive zones](#exclusive-zones) | `false` | Live |
| `keyboard_interactivity` | `"None"`, `"OnDemand"`, `"Exclusive"` | `"None"` | Live |
| `margin` | Number or edges table: the offset from the anchored edges, not layout margin | `0` | Live |
| `visible` | Hiding destroys the layer surface; showing builds a new one | `true` | Live |
| `child` | A node, or `function(output)` | None | Live |

### monitor

| Value | Instances | Notes |
| :--- | :--- | :--- |
| `"All"` | One per output: `bar@DP-1`, `bar@HDMI-A-1` | Follows hotplug |
| `"DP-1"` | One, `bar@DP-1`, while that output is connected | An unknown connector logs a warning and creates nothing |
| `"Active"` | One, bare id `bar`, on the output the compositor picks | Picked again at each show, because hiding destroys the object. Refuses `"NN%"` sizes and a function `child`. No instance while no output exists |

A dock on one output, floating 8px above the bottom edge, with a fixed 56px zone because its own
height is measured:

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
```

Connector names come from [`mantle.screens`](capabilities.md#renderer-members) or `niri msg outputs`
/ `hyprctl monitors`.

### Size

| `width`/`height` on an axis | Both edges of that axis anchored | One or no edge anchored |
| :--- | :--- | :--- |
| Omitted | The compositor's span, same as `"Fill"` | Measured from the content |
| `"Fill"` | The compositor's span | Protocol error. The panel stays hidden with a warning, or keeps its previous size on a live change |
| px or `"NN%"` | That size | That size |

The table sizes the Wayland surface. The root node inside it is laid out like any
[node](nodes.md#sizes): omitted means content-sized even when the surface spans the output. On a
spanned axis write `"Fill"` so the root, its `background` and its children cover the surface.

Measured content is capped at the output minus the margins on the anchored edges. Other clients'
exclusive zones are not subtracted, so content wider than the space they leave is clipped by the
compositor.

### Exclusive zones

| `exclusive` | Reserves | Covers others' zones |
| :--- | :--- | :--- |
| `false` | Nothing | No, stays inside them |
| `true` | The configured size along the one anchored edge: height when exactly one of `top`/`bottom` is anchored and `left`/`right` match, width for the transposed case. Any other anchor shape (a corner, all four edges) reserves 0 | No |
| Positive integer | That many px whatever the surface's size; for a tall surface whose top strip is the bar | No |
| `"Ignore"` | Nothing | Yes |

`0`, `-1` and fractional numbers are refused; spell them `false` and `"Ignore"`.

### Keyboard focus

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
```

`mantle toggle launcher_open` from a compositor keybind opens it on the output the compositor picks; Escape
closes it. See [named state](signals.md#named-state) and [text fields](input.md#text-fields).

| Compositor behavior | What the engine does or you do |
| :--- | :--- |
| Hyprland refocuses the last window, onto its workspace, when a still-mapped panel drops to `"None"` | The engine skips layer requests for a panel being hidden in the same pass. Change `visible` and `keyboard_interactivity` together |
| niri hands an `xdg_popup` the keyboard only if its parent held it when the popup mapped | The engine routes keys that arrive on the parent to the text field in a popup shown under it |
| niri dismissed a grabbing popup when its parent's `keyboard_interactivity` changed | Raise the parent's mode before opening the popup, not from inside it |

### Per-output content

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
```

`mantle set wallpaper_DP-1 /path/to/picture.jpg` changes one output's picture
([CLI](cli.md), [image](nodes.md#image)).

### OSD

A card bottom-centre on the output the compositor picks that shows for 1.5 s after the level changes. No
`left`/`right` anchor, so the width is measured and the protocol centres it:

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
```

`mantle set osd_level 0.7` shows it. Drive it from a capability by mapping, for example,
`mantle.audio` to the level; see [`pulse`](signals.md#pulse-mark-a-change). For a slide-in, animate
the root's `translate`, not the surface ([animation](animation.md)).

## window

An `xdg_toplevel` the compositor places, tiles and decorates. The engine requests server-side
decorations and draws none itself.

| Property | Values | Default | Kind |
| :--- | :--- | :--- | :--- |
| `title` | String | `""` | Live |
| `app_id` | String; compositor window rules match it | `"mantle-<id>"` | Live |
| `min_size`, `max_size` | `{ width, height }`, both keys required, each `[0, 8192]`, `0` = unconstrained. Advisory hints to the compositor; layout does not enforce them. A non-zero `max_size` axis below `min_size`'s is refused | None | Live |
| `on_close` | `function()`, called when the user asks to close. The window stays open unless the config sets `visible = false`. Without it, a close request does nothing | None | |
| `visible` | Creates or destroys the toplevel | `true` | Live |
| `child` | One node; a root without `width`/`height` fills the window | None | Live |

The window's size is the compositor's configure. When the compositor leaves an axis to the client
(the usual first configure on a floating compositor), it opens at `min_size`, else 640×480,
clamped by `max_size`. Tiling compositors such as niri always send the size.

```lua
local settings_open = state("settings_open", false)

local settings = window {
    id = "settings",
    title = "Settings",
    min_size = { width = 480, height = 360 },
    visible = settings_open,
    on_close = function() settings_open:set(false) end,
    background = "#1e1e2e",
    child = column { padding = 16, children = { text { content = "Settings", font_size = 20 } } },
}
```

## popup

An `xdg_popup` attached to a shown `panel`, `window` or another `popup`. The compositor positions
it relative to a rectangle in the parent and dismisses it on an outside click, which is why a
dropdown is a popup and not a second panel. No Wayland object exists while it is hidden.

| Property | Values | Default | Kind |
| :--- | :--- | :--- | :--- |
| `parent` | The `id` of a `panel`, `window` or `popup` | Required | Structural; a change applies at the next open |
| `anchor_rect` | `{ x?, y?, width, height }` in the parent's surface coordinates. `width`, `height` in `(0, 8192]`; `x`, `y` default `0` | Required | Live |
| `anchor` | Point on `anchor_rect` the popup hangs from: `"Top"`, `"Bottom"`, `"Left"`, `"Right"`, `"TopLeft"`, `"TopRight"`, `"BottomLeft"`, `"BottomRight"`, `"Center"` | `"Center"` | Live |
| `gravity` | Direction it extends from that point; same values | `"Center"` | Live |
| `constraint_adjustment` | Array of `"SlideX"`, `"SlideY"`, `"FlipX"`, `"FlipY"`, `"ResizeX"`, `"ResizeY"`; order is ignored, `{}` for none | `{ "FlipY", "SlideX" }` | Live |
| `offset` | `{ x?, y? }` px nudge after anchor and gravity; negative moves up or left | `{ x = 0, y = 0 }` | Live |
| `width`, `height` | px in `(0, 8192]`, or omitted to measure the content (capped at the first output's size). No `"Fill"`, no `"NN%"` | Content | Live |
| `grab` | `true` takes an input grab, so an outside click dismisses it. Needs a pointer press or release in the same turn; without one the popup is not opened and a warning is logged. `false` for a tooltip | `true` | Live |
| `on_dismiss` | `function()`, called after the compositor closes it (outside click, denied grab, parent gone). Not called when the config hides it | None | |
| `visible` | Opens or closes it | `true` | Live |
| `child` | One node | None | Live |

While a popup is open, a change to its measured size or any positioner field moves it through
`xdg_popup.reposition`. On a compositor with `xdg_popup` below version 3 it keeps the size and
place it opened at until it closes, with a warning.

Hiding or dismissing a popup also destroys the popups open under it and latches them as if
dismissed, so each reopens on the next pointer press while its own `visible` stays true.

When `parent` names a per-output panel, the popup opens on the instance the arming click landed
on, else the first instance. A `lock` cannot be a parent.

### Dismissal

A compositor dismissal destroys the popup but leaves your `visible` signal `true`. The engine
latches it shut until the next pointer press or release, then opens it again. Clear your state in
`on_dismiss`:

```lua
local menu_open = state("menu_open", false)
local menu_anchor = state("menu_anchor", { x = 0, y = 0, width = 1, height = 1 })

local menu_button = button {
    padding = 8,
    on_click = function(rect)
        menu_anchor:set(rect)
        menu_open:set(not menu_open:get())
    end,
    children = { text { content = "Menu" } },
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
    background = "#1e1e2e", radius = 8, padding = 8,
    child = column { spacing = 4, children = { text { content = "Settings" }, text { content = "Log out" } } },
}
```

`on_click`'s `rect` is the button in its surface's coordinates, exactly what `anchor_rect` wants.
Put `menu_button` in a panel with `id = "bar"`. See [pointer input](input.md#pointer).

### Nested menus

A popup can parent another popup. Here the submenu hangs off the right edge of the clicked row
and flips left near the screen edge:

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

A hover opens no grab, so `grab = false`. [`hover_rect`](input.md#hover) tracks the hovered node
and is 1×1 before the first hover, which keeps `anchor_rect` valid:

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

To anchor to a node without hovering it, bind [`geometry`](signals.md#geometry-read-a-nodes-laid-out-rect). It reads zero
before the first layout, so map it to a 1×1 fallback:

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

## lock

The session lock screen: one surface per connected output, covering it for as long as the
compositor holds the session locked. A config declares at most one. Declaring it does not lock;
`mantle.lock:invoke("lock")` does, and only a correct password unlocks. The lock's state
(`active`, `authenticating`, `error`, `attempts`) and its actions are under
[capabilities](capabilities.md).

| Property | Values |
| :--- | :--- |
| `id` | Required; a rename is refused while the session is locked (save again after unlocking) |
| `child` | A node or `function(output)`; the root fills the output |
| `visible`, `width`, `height`, `monitor`, `anchor` | Refused: the protocol owns coverage and lifetime |

```lua
local lock_screen = lock {
    id = "lock",
    background = "#11111b",
    child = column {
        width = "Fill", height = "Fill", align_v = "Center", spacing = 8,
        children = {
            textfield {
                width = 320,
                height = 32,
                align_h = "Center",
                placeholder = "Password",
                secure_submit = { capability = "lock", action = "authenticate" },
            },
            text {
                content = mantle.lock:map(function(lock) return lock and lock.error or "" end),
                foreground = "#f38ba8",
                align_h = "Center",
            },
        },
    },
}

action("lock", function() mantle.lock:invoke("lock") end)
```

The password never reaches Lua; see [secure fields](input.md#secure-fields). Bind
`mantle call lock` to a key ([action](scripting.md#action)). `mantle.lock:invoke("set_unlock_animation", ms)`
keeps the lock up to 600 ms after a correct password so the `child` can animate out; `unlocking` is
`true` during that window.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A panel anchored to both sides shows its background only behind its content | Omitted size spans the surface but not the root node; set `width = "Fill"` (or `height`) |
| `"Fill"` on an axis with only one edge anchored leaves the panel hidden with a warning in `mantle log` | Anchor both edges of that axis, or give a size |
| `mantle check` passes but a surface never appears | `check` validates surface fields only. It builds no node tree (unknown child properties pass), runs no function `child` and has no outputs for the Fill/anchor check. Read `mantle log` after a reload |
| `height = "50%"` or a function `child` on a `monitor = "Active"` panel is refused | Use `"Fill"` with anchors and margins, or px |
| `anchor_rect` with a zero `width` or `height` is refused (a `geometry` before first layout, a hand-built rect) | Fall back to `{ x = 0, y = 0, width = 1, height = 1 }` |
| A dropdown reopens on the next click after an outside click closed it | Set its `visible` state to `false` in `on_dismiss` |
| A popup with `visible` bound to startup-true state never opens | `grab = true` needs a click; open it from `on_click`, or set `grab = false` |
| A popup whose parent is hidden does not open | Show the parent first; the popup opens on the next pass |
| `layer = state(...)` or a signal `anchor` is refused | Structural fields take literals; switch between two declared panels, or edit the file |
| `exclusive = true` on a corner-anchored panel reserves nothing | Anchor one edge, alone or with both perpendicular edges, or give a px count |
| `exclusive = 0` is refused | `false` |

Source: [surface parsing](../../renderer/src/lua/surfaces.rs),
[panel spec](../../renderer/src/layout/node/surface.rs),
[window and popup specs](../../renderer/src/layout/node/toplevel.rs),
[lock spec and fingerprints](../../renderer/src/layout/node/spec.rs),
[instances](../../renderer/src/layout/instance.rs),
[function child](../../renderer/src/layout/scene/pass.rs),
[layer shell](../../renderer/src/wayland/layer.rs),
[window](../../renderer/src/wayland/xdg_shell/window.rs),
[popup](../../renderer/src/wayland/xdg_shell/popup.rs),
[reload and hotplug](../../renderer/src/wayland/output.rs).

See also: [nodes](nodes.md), [paint](paint.md), [input](input.md), [signals](signals.md),
[capabilities](capabilities.md#lock), [runtime](runtime.md), [CLI](cli.md).
