# Surfaces

A surface is a top-level Wayland object that holds one node tree. `shell.lua` returns one surface or
a list of them, and every other node lives under some surface's `child`. This page holds the rules
all four roles share; each role has its own page.

| You are building | Role | Protocol | Instances |
| :--- | :--- | :--- | :--- |
| Bar, dock, wallpaper, OSD, launcher overlay, notification stack | [`panel`](panel.md) | `zwlr_layer_surface_v1` | One per matched output, id `id@output` |
| Settings window, dialog the user can move, tile or close | [`window`](window.md) | `xdg_toplevel` | One, id `id` |
| Dropdown, context menu, tooltip hanging off a panel or window | [`popup`](popup.md) | `xdg_popup` | One, id `id` |
| Lock screen | [`lock`](lock.md) | `ext_session_lock_surface_v1` | One per output, id `id@output` |

An *instance* is one mapped copy of a declared surface; its id keys the retained scene and names
the surface in `mantle log` ([glossary](../glossary.md#surfaces)).

A 32 px bar on every output that pushes windows down by its height and prints the output's
connector name:

<!-- shot-alt: A top bar whose left end shows the DP-1 output name in a pill. -->
```lua,shot
local bar = panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 32,
    exclusive = true,
    width = "Fill",
    child = function(output)
        return row {
            width = "Fill",
            height = "Fill",
            padding = { left = 8, right = 8 },
            background = "#1e1e2e",
            children = {
                rect {
                    align_v = "Center",
                    padding = { left = 10, right = 10, top = 3, bottom = 3 },
                    radius = 10,
                    background = "#313244",
                    children = { text { content = output, font_weight = 700, foreground = "#89b4fa" } },
                },
            },
        }
    end,
}

return { bar }
```

## Properties every role takes

Every surface takes `id` (required), one `child` node and, except `lock`, `visible`; each role's
page has their table. The root is itself a box node, so it
also takes the [common and box node properties](../nodes/index.md): its own `background`, `radius`,
`padding`, `border_*`, [paint](../guide/paint.md) and [`animate`](../guide/animation.md). Any other
key is refused, and the error names the closest accepted one or, with none close, lists them all.

### The surface root

The root has no parent, so a few node properties mean something else on it:

| Property | On a surface root |
| :--- | :--- |
| `width`, `height` | Role-specific: the layer surface's size on a [`panel`](panel.md#size), the root's size inside the configured window on a [`window`](window.md#size), the popup's size on a [`popup`](popup.md#properties). Refused on a `lock` |
| `min_width`, `max_width`, `min_height`, `max_height` | Bound the root's measured size, so they cap a content-sized `panel` or `popup` |
| `margin` | A `panel`'s offset from its anchored edges. Ignored on the other roles |
| `align_h`, `align_v` | Ignored: the root sits at the surface's origin |
| Everything else | As on any box node |

## Reload and structural fields

The returned list is re-read on every [reload](../guide/runtime.md#evaluation-reload-and-generations).
Each surface is matched to the last evaluation by its *fingerprint*, the fields that the protocol
fixes when the object is created.

| Rule | Behaviour |
| :--- | :--- |
| Return value | One surface, a list of them, `{}` or nothing. Any other top-level node is refused |
| Matching | A surface whose fingerprint is unchanged keeps its Wayland objects; a missing one is destroyed; a new one is created |
| Fingerprint | `panel`: `id`, `layer`, `anchor`, `monitor`, `namespace`. `window`, `popup`, `lock`: `id` alone. A changed fingerprint destroys and recreates that surface's objects under the same instance ids |
| Structural fields | `id`, a panel's `layer`, `anchor`, `monitor`, `namespace` and a popup's `parent` refuse a signal: they are read once per evaluation. Change them by editing the file |
| Live fields | Everything else takes a signal and updates the existing object in place |
| Invalid live value | A signal that resolves to a bad value logs a warning and keeps the last applied spec |
| Hotplug | An output change re-evaluates the config and adds or removes `panel` and `lock` instances; instances on other outputs keep their objects |
| Count | Any number of panels, windows and popups; at most one `lock` |

## Per-output child

`child = function(output)` is for `panel` and `lock`, the roles with one instance per output. It
runs with that instance's connector name (`"DP-1"`) on the first pass, and again only when the
surface root resolves again: a signal the function or the root's own properties read is written, or
a reload ([what a node reads again](../guide/signals.md#what-a-node-reads-again)). Bind signals to
properties inside it rather than reading them with `:get()`, and key per-output state by name:
`state("wallpaper_" .. output, ...)`. Returning `nil` leaves that output's instance empty. A `window`, `popup` or `monitor = "Active"` panel has no output name and refuses a
function `child`. Example: [per-output wallpaper](panel.md#per-output-content).

## Reset on close

Named state keeps its value while a surface is hidden, so a reopened menu shows what it showed
last. List the state a `panel`, `window` or `popup` owns in `reset_on_close` to start it fresh
instead: when the surface stops being shown, each `state` goes back to its declared `initial` and
each `scroll` to the top.

```lua
local open = state("picker_open", true)
local query = state("picker_query", "")
local list_scroll = scroll("picker_list")

return panel {
  id = "picker",
  layer = "Overlay",
  visible = open,
  reset_on_close = { query, list_scroll },
  child = textfield { width = 300, height = 32, on_change = function(text) query:set(text) end },
}
```

| Rule | Detail |
| :--- | :--- |
| Takes | `state(...)` and `scroll(...)` handles. A map, a capability, another signal kind or a plain value fails the evaluation, naming the surface and the entry |
| Closes | `visible` turns false, a reload removes the surface, its last output goes away, or (a popup) its parent closes |
| Does not close | A surface never shown, a failed reload, a reload that rebuilds it under the same id, being covered, losing the keyboard |
| Per-output panel | Resets when its last instance closes |
| Resets to | The `initial` the evaluation on screen declares, the value `mantle toggle` returns to; a scroll to offset 0 |
| Timing | The turn the surface is unmapped, after it is gone, so the closed surface never paints the reset value |
| Write | An ordinary write: readers re-resolve, and a state that changed runs its [`on_change`](../guide/signals.md#on_change-react-to-a-write). A state listed by several surfaces resets when any of them closes |

A `textfield`'s draft is the engine's and is not state; it follows its own
[lifetime](../guide/input.md#text-fields).

## Input region

A surface takes pointer input only where its content is solid; everywhere else clicks, hover and
focus-follows-mouse pass through to what is below. The engine rebuilds this region on every pass.

| Node under the root | Claims input |
| :--- | :--- |
| A box (`rect`, `row`, `column`) with a `background` or a non-zero `border_width` | Its whole box, painted bounds under its own transform. `#00000000` counts |
| `text`, `icon`, `image`, `capture`, `textfield` | Its box |
| Any node with `on_click`, `on_drag`, `on_wheel` or `submit = true` | Its box, even with nothing painted |
| A `shader` | Nothing; its alpha is unknown to the engine. Give it a handler for a hit area |
| A `path` with a `fill`, or a `stroke` with `stroke_width` above 0, and at least one `L`, `Q` or `C` command | Its whole box, not the drawn shape. Otherwise nothing; give it a handler for a hit area |
| A box's [`mask` child](../guide/paint.md#mask) | Nothing, nor its subtree |
| A transparent container | Nothing; its children are asked instead |
| The surface root itself | Nothing, even with a `background`; with a handler, the whole surface |
| Anything on a `layer = "Background"` panel | Only a node with a handler |

A claiming box that clips its children ends the walk there; `visible = false` subtrees claim
nothing. To make an empty area catch clicks, put a `rect { width = "Fill", height = "Fill",
on_click = ... }` there ([click outside to close](panel.md#close-an-overlay-on-an-outside-click)).

## How do I…

| Task | Answer |
| :--- | :--- |
| Pick a role | The table at the top |
| Put a bar on every monitor | The example at the top |
| Show and hide a surface | Bind `visible` to [named state](../guide/signals.md#named-state), then `mantle toggle <name>` |
| Start a surface fresh each time it opens | [Reset on close](#reset-on-close) |
| Keep different state per monitor | [Per-output child](#per-output-child) |
| See which surfaces a config declares | `mantle check -c <dir>` prints each role and id ([CLI](../guide/cli.md)) |
| Let clicks through the empty part of a surface | Nothing to do; see [input region](#input-region) |
| Make a transparent area catch clicks | A full-size `rect` with `on_click` ([input region](#input-region)) |
| Change a panel's layer or anchors at run time | Declare two panels and toggle their `visible`, or edit the file |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `layer = state(...)` or a signal `anchor` is refused | Structural fields take literals; switch between two declared panels, or edit the file |
| A click on a panel's or window's background reaches the window behind it | The root's own `background` claims no input. Put the background on a `width = "Fill", height = "Fill"` child; on a panel, make the panel `"Fill"` on those axes too |
| `two surfaces declare` an id | Surface ids are unique across every role; rename one |
| `margin` or `align_h` on a `window` or `popup` root does nothing | Set it on the child |
| A function `child` on a `window` or `popup` is refused | Only `panel` (not `monitor = "Active"`) and `lock` have an output to pass |

See also: [nodes](../nodes/index.md), [paint](../guide/paint.md), [input](../guide/input.md),
[signals](../guide/signals.md), [runtime](../guide/runtime.md), [CLI](../guide/cli.md).

Source: [surface parsing](../../renderer/src/lua/surfaces.rs),
[accepted properties](../../renderer/src/lua/nodes/properties.rs),
[fingerprints](../../renderer/src/layout/node/spec.rs),
[instances](../../renderer/src/layout/instance.rs),
[function child and root size](../../renderer/src/layout/scene/pass.rs),
[input region](../../renderer/src/layout/region.rs),
[reload and hotplug](../../renderer/src/wayland/output.rs).
