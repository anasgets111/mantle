# rect

A plain box that stacks its children: each one gets the whole content box and places itself with
`align_h`/`align_v`. Reach for it for a filled shape, a background behind something, or layering
one node over another. Side by side needs a [`row` or `column`](row-column.md) instead.

A bell icon with an unread badge in its top-right corner:

```lua,shot
local unread = state("unread", 3)

local bell = rect {
    width = 28,
    height = 28,
    children = {
        icon { name = "notification-symbolic", size = 20, foreground = "#CDD6F4",
               align_h = "Center", align_v = "Center" },
        rect {
            visible = unread:map(function(n) return (n or 0) > 0 end),
            align_h = "End",
            align_v = "Start",
            padding = { left = 4, right = 4 },
            radius = 7,
            background = "#F38BA8",
            children = { text { content = unread:map(tostring), font_size = 10, foreground = "#11111B" } },
        },
    },
}

return bell
```

The badge comes second, so it paints over the icon.

## Properties

`rect` takes the [common](index.md#common-properties) and [box](index.md#box-properties)
properties, plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `children` | `Node[]\|Bound` | None | Array of node tables, up to 10000; a `nil` or `false` entry is an error. Stacked in order: later children paint over earlier ones. Bind a signal of an array to [switch views](index.md#switching-views-with-ids) |
<!-- End of the generated table. -->

Without `width` and `height`, a `rect` is the union of its children, and 0 × 0 with none.

## How do I…

| Task | Answer |
| :--- | :--- |
| Layer a badge over an icon | The example above |
| Centre something | [Below](#centre-something) |
| Draw a divider line | `rect { width = "Fill", height = 1, background = "#45475A" }` |
| Round an image's corners | [image](image.md#round-an-images-corners): a `rect` with `radius` and `clip = "Rounded"` |
| Dim everything behind a dialog | A full-size `rect` with a translucent `background`, the dialog as its child ([paint](../guide/paint.md#dim-the-background-behind-a-modal)) |
| Overlap two views while they swap | Make the parent a `rect` ([switching views](index.md#switching-views-with-ids)) |

### Centre something

A stacking parent places each child on its own, so `align_h` and `align_v` of `"Center"` centre it.
Here a card sits in the middle of a full-screen dimmed layer.

```lua
local dialog = rect {
    width = "Fill",
    height = "Fill",
    background = "#00000080",
    children = {
        column {
            align_h = "Center",
            align_v = "Center",
            padding = 24,
            radius = 12,
            background = "#1E1E2E",
            children = { text { content = "Centred", font_size = 16 } },
        },
    },
}
```

Inside a `row`, set `align_h = "Center"` on the row instead: it packs its children, and they ignore
their own `align_h`.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Children of a `rect` sit on top of each other | That is stacking. Use a `row` or `column` to lay them out side by side |
| An empty `rect` draws nothing | With no children and no size it is 0 × 0. Give it `width` and `height` |
| A `rect` takes no clicks | Only a [`button`](button.md) does |
| A `background` tween snaps in instead of fading | An absent `background` has no colour to tween from. Start from a transparent one, such as `"#89B4FA00"` ([animation](../guide/animation.md#what-can-animate)) |

See also: [row and column](row-column.md), [button](button.md), [paint](../guide/paint.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [children](../../renderer/src/layout/node/spec.rs),
[box paint](../../renderer/src/layout/node/paint_style.rs).
