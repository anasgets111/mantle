# row and column

Boxes that flow their children along one axis: a `row` left to right, a `column` top to bottom.
They are the main layout tools; everything else is placed inside one. The two accept the same
properties and differ only in their main axis. For children built from data, use a [`list`](list.md).

A meter: a `"Fill"`-wide track with a percentage-wide fill that follows a signal.

```lua,shot
local volume = state("volume", 0.45)

local meter = row {
    width = "Fill",
    height = 6,
    radius = 3,
    background = "#313244",
    children = { rect {
        width = volume:map(function(v) return string.format("%d%%", math.floor((v or 0) * 100 + 0.5)) end),
        height = "Fill",
        radius = 3,
        background = "#89B4FA",
        animate = { width = 150 },
    } },
}

return column { width = 240, children = { meter } }
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
<!-- End of the generated table. -->

How the container packs its children:

| Axis | Set by | Effect |
| :--- | :--- | :--- |
| Main (`row`: horizontal, `column`: vertical) | The container's own `align_h` (row) or `align_v` (column) | `"Start"`, `"Center"`, `"End"` pack the children; `"Stretch"` packs like `"Start"`. The children's own value on this axis is ignored |
| Cross | Each child's `align_v` (row) or `align_h` (column) | Places that child across the row's height or the column's width; `"Stretch"` fills it |

`"Fill"` children along the main axis share what the others leave, and no child shrinks; see
[sizes](index.md#sizes).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a progress bar | The meter above |
| Push items apart | [Below](#push-items-apart) |
| Split a bar into three groups | The [bar](index.md#nodes): two `"Fill"` rows around a content-sized middle |
| Centre items in a row | `align_h = "Center"` on the row itself |
| Make children equal width | Give each `width = "Fill"` |
| Scroll overflowing content | Bound the axis (`height` or `max_height` on a column), then `scroll = scroll("name")` ([scroll](../guide/input.md#scroll)) |
| Overlap items, like stacked avatars | Negative `spacing` |

### Push items apart

A `"Fill"` child takes the space its siblings leave, so a bare `rect` makes a spacer:

```lua
local header = row {
    width = 300,
    align_v = "Center",
    spacing = 8,
    children = {
        text { content = "Wi-Fi", font_size = 14 },
        rect { width = "Fill" }, -- takes the space left over, pushing what follows to the end
        text { content = "Connected", foreground = "#A6ADC8" },
    },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `align_h = "Center"` on a child of a `row` does nothing | The row packs its main axis: set `align_h` on the row, or use `"Fill"` spacers |
| A `"Fill"` child of a content-sized row is 0 wide | The row has no leftover space to share. Give the row a `width` or `"Fill"` |
| `direction = "Horizontal"` on a `column` is refused | `direction` is a [`list`](list.md) property. Use a `row` |
| A `scroll` row or column never scrolls | Its size on the main axis is content-sized, so nothing overflows. Set `width`/`height` or a `max_*` |

See also: [list](list.md), [rect](rect.md), [layout model](index.md#layout-model).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [layout solver](../../renderer/src/layout/scene/solver.rs),
[spacing and alignment parsers](../../renderer/src/layout/node/style/mod.rs),
[scroll](../../renderer/src/layout/scene/scroll.rs).
