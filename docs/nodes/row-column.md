# row and column

Boxes that flow their children along one axis: a `row` left to right, a `column` top to bottom.
They are the main layout tools; everything else is placed inside one. The two accept the same
properties and differ only in their main axis. For children built from data, use a [`list`](list.md).

A meter: a `"fill"`-wide track with a percentage-wide fill that follows a signal.

<!-- shot-alt: A volume card: a speaker icon, a blue meter filling 45% of its dark track, and the percentage. -->
```lua,shot
local volume = state("volume", 0.45)
local percent = volume:map(function(v) return string.format("%d%%", math.floor((v or 0) * 100 + 0.5)) end)

local meter = row {
    width = "fill",
    height = 6,
    radius = 3,
    align_v = "center",
    background = "#313244",
    children = { rect { width = percent, height = "fill", radius = 3, background = "#89B4FA", animate = { width = 150 } } },
}

return row {
    width = 280,
    padding = 14,
    spacing = 12,
    radius = 12,
    background = "#1E1E2E",
    children = {
        icon { name = "audio-volume-medium-symbolic", size = 18, foreground = "#89B4FA", align_v = "center" },
        meter,
        text { content = percent, foreground = "#CDD6F4", align_v = "center" },
    },
}
```

## Properties

`row` and `column` take the [common](index.md#common-properties) and
[box](index.md#box-properties) properties, plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `children` | `Node[]\|Bound` | None | Array of node tables, up to 10000; a `nil` or `false` entry is an error. Laid out in order along the main axis. Bind a signal of an array to [switch views](index.md#switching-views-with-ids) |
| `spacing` | `number\|Bound` | `0` | Px between visible children; negative values overlap them. Not range-checked |
| `scroll` | `Bound` | None | A `scroll(name)` signal; makes the node a scrolling viewport along its main axis ([scroll](../guide/input.md#scroll)) |
| `homogeneous` | `boolean\|Bound` | `false` | `true` gives every visible child one equal main-axis slot, as GTK's `homogeneous`; see [equal slots](row-column.md#equal-slots) |
| `wrap` | `boolean\|Bound` | `false` | `true` flows children onto new lines when the next does not fit the main axis, CSS `flex-wrap`; see [wrapping](row-column.md#wrapping). Refused with `scroll` |
| `line_spacing` | `number\|Bound` | `0` | Px between lines under `wrap`, `0` by default; negative values overlap them. Ignored without `wrap` |
<!-- End of the generated table. -->

How the container packs its children:

| Axis | Set by | Effect |
| :--- | :--- | :--- |
| Main (`row`: horizontal, `column`: vertical) | The container's own `align_h` (row) or `align_v` (column) | `"start"`, `"center"`, `"end"` pack the children; `"stretch"` packs like `"start"`. The children's own value on this axis is ignored |
| Cross | Each child's `align_v` (row) or `align_h` (column) | Places that child across the row's height or the column's width; `"stretch"` fills it |

`"fill"` children along the main axis share what the others leave, and no child shrinks; see
[sizes](index.md#sizes).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a progress bar | The meter above |
| Push items apart | [Below](#push-items-apart) |
| Split a bar into three groups | The [bar](index.md#nodes): two `"fill"` rows around a content-sized middle |
| Centre items in a row | `align_h = "center"` on the row itself |
| Make children equal width | `homogeneous = true` on the row, or [equal slots](#equal-slots) |
| Scroll overflowing content | Bound the axis (`height` or `max_height` on a column), then `scroll = scroll("name")` ([scroll](../guide/input.md#scroll)) |
| Overlap items, like stacked avatars | Negative `spacing` |
| Flow chips or tiles onto lines | `wrap = true`, [below](#wrapping) |

### Equal slots

`homogeneous = true` gives every visible child a slot of the same size along the main axis.

| Container | Slot |
| :--- | :--- |
| Content-sized on the main axis | The largest child's size plus its margins. A `max_*` cap below that overflows by whole slots |
| `width`/`height`, a percent or `"fill"` | An equal share of the axis after `spacing`, whatever the content: a child can end up narrower than its text |

A child fills its slot less its own margins, whatever its `width` (or `height` in a column), `"fill"` or content; a child with a pixel size on that axis keeps it at the slot's start. The container's main-axis `align_*` has no room left to pack, and the cross axis behaves as in any row.

A segmented control: labels of different widths in equal segments.

```lua
return row {
    homogeneous = true,
    spacing = 2,
    padding = 2,
    radius = 8,
    background = "#313244",
    children = {
        text { content = "Day", foreground = "#CDD6F4", text_align = "center", padding = 6 },
        text { content = "Week", foreground = "#CDD6F4", text_align = "center", padding = 6 },
        text { content = "Month", foreground = "#CDD6F4", text_align = "center", padding = 6 },
    },
}
```

### Wrapping

`wrap = true` starts a new line when the next child does not fit the main axis: a `row` breaks into
lines top to bottom, a `column` into columns left to right.

| Property | Effect |
| :--- | :--- |
| `spacing` | Between children within a line |
| `line_spacing` | Between lines; `0` by default |
| `align_h` / `align_v` | The container's main-axis value packs each line; its cross-axis value packs the lines. `"stretch"` packs as `"start"` |
| Child cross alignment | Places the child within its line, not within the container |
| `"fill"` child | Counts as zero when lines break, then takes what its line leaves |
| Content-sized cross axis | Grows with the lines |
| `homogeneous` | Every cell is the largest child's size across all lines, a grid |
| `scroll` | Refused: a wrapped container has no single main-axis extent to scroll |

A line breaks only against a bounded main axis: `width`, `"fill"`, a `max_width` (`max_height`) or a
stretching cross axis. A content-sized container with none holds one line. A `homogeneous` wrapping
container needs the bound itself, and without one has a single cell per line.

A chip field:

```lua
local chips = {}
for _, name in ipairs({ "Rust", "Lua", "Wayland", "Taffy", "GPU", "Fonts", "Icons" }) do
    chips[#chips + 1] = row {
        padding = { left = 10, right = 10, top = 4, bottom = 4 },
        radius = 12,
        background = "#313244",
        children = { text { content = name, foreground = "#CDD6F4" } },
    }
end

return row { wrap = true, width = 180, spacing = 6, line_spacing = 6, children = chips }
```

With `homogeneous = true` the same chips become a grid of equal tiles, each the widest chip.

### Push items apart

A `"fill"` child takes the space its siblings leave, so a bare `rect` makes a spacer:

<!-- shot-alt: A card row with a Wi-Fi icon and label on the left and a green Connected pushed to the right. -->
```lua,shot
local header = row {
    width = 300,
    padding = 14,
    spacing = 10,
    radius = 12,
    background = "#1E1E2E",
    children = {
        icon { name = "network-wireless-symbolic", size = 18, foreground = "#89B4FA", align_v = "center" },
        text { content = "Wi-Fi", font_size = 14, font_weight = 700, foreground = "#CDD6F4", align_v = "center" },
        rect { width = "fill" }, -- takes the space left over, pushing what follows to the end
        text { content = "Connected", foreground = "#A6E3A1", align_v = "center" },
    },
}

return header
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `align_h = "center"` on a child of a `row` does nothing | The row packs its main axis: set `align_h` on the row, or use `"fill"` spacers |
| `align_v = "center"` on a `row` leaves its children at the top | A row's own `align_v` places the row in its parent. Set `align_v` on each child, as in [push items apart](#push-items-apart) |
| A `"fill"` child of a content-sized row is 0 wide | The row has no leftover space to share. Give the row a `width` or `"fill"` |
| `direction = "horizontal"` on a `column` is refused | `direction` is a [`list`](list.md) property. Use a `row` |
| A `scroll` row or column never scrolls | Its size on the main axis is content-sized, so nothing overflows. Set `width`/`height` or a `max_*` |

See also: [list](list.md), [rect](rect.md), [layout model](index.md#layout-model).

Source: [vocabulary](../../renderer/src/lua/nodes/properties.rs), [layout solver](../../renderer/src/layout/scene/solver.rs),
[spacing and alignment parsers](../../renderer/src/layout/node/style/mod.rs),
[scroll](../../renderer/src/layout/scene/scroll.rs).
