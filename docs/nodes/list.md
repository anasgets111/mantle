# list

A `row` or `column` whose children come from data: one `itemfn(item)` call per element of `source`.
Reach for it for anything with a count you do not know up front: workspaces, notifications, search
results, a thumbnail grid. For a fixed set of children, a [`row` or `column`](row-column.md) is
simpler.

A scrolling thumbnail grid: a vertical list of two-image rows, decoded off-thread.

```lua
local paths = state("wallpapers", { "/usr/share/backgrounds/a.jpg", "/usr/share/backgrounds/b.jpg",
    "/usr/share/backgrounds/c.jpg", "/usr/share/backgrounds/d.jpg" })

-- Two per row: a vertical list of rows, keyed by the paths they hold.
local rows = paths:map(function(all)
    local out = {}
    for i = 1, #all, 2 do out[#out + 1] = { all[i], all[i + 1] } end
    return out
end)

local grid = list {
    width = 420,
    height = 300,
    spacing = 8,
    scroll = scroll("thumbs"),
    source = rows,
    key = function(pair) return table.concat(pair, "\n") end,
    itemfn = function(pair)
        local tiles = {}
        for i, path in ipairs(pair) do
            tiles[i] = image { source = path, async = true, fit = "cover", width = 206, height = 116 }
        end
        return row { spacing = 8, children = tiles }
    end,
}
```

Keyed workspace buttons from a capability: [workspaces cookbook](../cookbook/workspaces.md).

## Properties

`list` takes the [common properties](index.md#common-properties), plus the ones below. It takes no
[box properties](index.md#box-properties): wrap it in a `rect` or `column` for a background.

| Property | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | Array. Bind a signal to rebuild on change | Empty | One item per element, in order; a `nil` hole ends it. Missing or `nil` (a capability before its first push) builds no items. More than 10000 elements is refused unless `limit` caps it |
| `itemfn(item)` | Function, required | — | Returns one node table for an element |
| `key(item)` | Function returning a UTF-8 string | None | Called with the element; the result becomes that item's `id`, replacing any `id` `itemfn` set. Duplicates are refused. Without it, items match by position |
| `limit` | Non-negative integer | None | Build at most this many items. Values above 10000 act as 10000. `0` builds none |
| `direction` | `"Vertical"`, `"Horizontal"` | `"Vertical"` | Lays out as a `column` or a `row` |
| `spacing` | Pixels, any finite number | 0 | Gap between visible items; negative overlaps them |
| `scroll` | A `scroll(name)` signal | None | Makes the list a scrolling viewport along `direction` ([scroll](../guide/input.md#scroll)) |

A `list` packs and aligns exactly like the `row` or `column` its `direction` names: its own
`align_v` (vertical) or `align_h` (horizontal) packs the items.

A list builds every item it is given on every pass that re-resolves it, including items scrolled
out of view. `key` makes matching cheap and keeps each item's state (tweens, a held image, a text
field's draft) across reorders; it does not skip `itemfn`. Bound long lists with `limit` (a launcher
showing the top 50 matches), or hide them while closed so they freeze.

## How do I…

| Task | Answer |
| :--- | :--- |
| Lay out a grid | The example above: a `list` of `row`s, several items per row |
| Scroll a long list | [Below](#scroll-a-long-list) |
| Keep items' animations when the order changes | Give `key` a stable per-element string (an id from the data) |
| Show only the top N matches | `limit = 50` |
| Lay items out horizontally | `direction = "Horizontal"` |
| Filter as the user types | Bind `source` to a `computed` of the query ([textfield](textfield.md)) |
| Show an empty state | Bind a sibling's `visible` to `#items == 0` |

### Scroll a long list

Bound the size on the scrolling axis, then bind a [`scroll`](../guide/input.md#scroll) signal.
`max_height` lets the list shrink to fit a few items and scroll past 200 px.

```lua
local names = {}
for i = 1, 40 do names[i] = "Item " .. i end

local items = list {
    width = 240,
    max_height = 200, -- grows with its items up to 200 px, then scrolls
    spacing = 2,
    scroll = scroll("items"),
    source = names,
    itemfn = function(name)
        return text { content = name, width = "Fill", padding = 6 }
    end,
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A 2000-item `list` makes every update slow | Every item is built on every pass, visible or not. Cap it with `limit`, filter the `source`, or hide the list while it is closed |
| A `list` of more than 10000 elements is refused | Set `limit`, or page the `source` |
| `duplicate key` error | `key` must return a different string for every element |
| `key` returning a number is refused | Return a string: `tostring(item.id)` |
| Items lose their state when one is added at the top | Without `key` they match by position. Add `key` |
| `background` on a `list` is refused | A list is not a box. Wrap it |

See also: [row and column](row-column.md), [signals](../guide/signals.md), [input: scroll](../guide/input.md#scroll).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [list parser](../../renderer/src/layout/node/spec.rs),
[layout as row or column](../../renderer/src/layout/scene/solver.rs).
