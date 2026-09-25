# list

A `row` or `column` whose children come from data: one `itemfn(item)` call per element of `source`.
Reach for it for anything with a count you do not know up front: workspaces, notifications, search
results, a thumbnail grid. For a fixed set of children, a [`row` or `column`](row-column.md) is
simpler.

A scrolling thumbnail grid: a vertical list of two-image rows, decoded off-thread.

```lua,shot
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

return grid
```

Keyed workspace buttons from a capability: [workspaces cookbook](../cookbook/workspaces.md).

## Properties

`list` takes the [common properties](index.md#common-properties), plus the ones below. It takes no
[box properties](index.md#box-properties): wrap it in a `rect` or `column` for a background.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | `any[]\|Bound` | Empty | Array; bind a signal to rebuild on change. Missing or `nil` (a capability before its first push) is an empty list; a `nil` hole ends it. More than 10000 items without `limit` is an error |
| `itemfn` | `fun(item: any): Node` | Required | Builds a node for every built item, visible or not |
| `key` | `fun(item: any): string` | None | Unique UTF-8 key per item; replaces the node's `id`. Duplicates are refused. Without it items match by position |
| `limit` | `integer\|Bound` | None | Build at most this many items; above 10000 acts as 10000, `0` builds none |
| `direction` | `"Vertical"\|"Horizontal"\|Bound` | `"Vertical"` | Lays out as a `column` or a `row` |
| `spacing` | `number\|Bound` | `0` | Px between visible items along `direction`; negative values overlap them |
| `scroll` | `Bound` | None | A `scroll(name)` signal; makes the list a scrolling viewport along `direction` ([scroll](../guide/input.md#scroll)) |
<!-- End of the generated table. -->

A `list` packs and aligns exactly like the `row` or `column` its `direction` names: its own
`align_v` (vertical) or `align_h` (horizontal) packs the items.

## When items rebuild

A list keeps the items it built until something that build read changes. A pass that finds nothing
changed calls no `itemfn` and reads none of the items' signals; it lays the kept items out again,
about half the cost of building them. One change rebuilds every item, scrolled out of view or not.

| Change | Rebuilds the items |
| :--- | :--- |
| A write to `source`, or to a signal under a `map` or `computed` bound to it | ✓ |
| A write to a signal `itemfn` or `key` read with `:get()` | ✓ |
| A write to a signal bound to a property of a built item, at any depth | ✓ |
| A new `source`, `itemfn` or `key` value, a new `limit`, or a reload | ✓ |
| A write to anything else, even on the same surface | |

`key` carries each item's state (tweens, a held image, a text field's draft) onto its rebuilt node,
and across reorders. Cap a long list with `limit` (a launcher's top 50 matches), or hide it while
closed so it freezes.

The engine sees signal reads only. An `itemfn`, `key` or item `map` that reads the clock, a mutable
variable or a `source` table changed in place keeps what it read until a signal it read is written.
A `delay` or `pulse` rebuilds the list on every pass while one is pending or open. What to read
instead: [what a node reads again](../guide/signals.md#what-a-node-reads-again).

## How do I…

| Task | Answer |
| :--- | :--- |
| Lay out a grid | The example above: a `list` of `row`s, several items per row |
| Scroll a long list | [Below](#scroll-a-long-list) |
| Keep items' animations when the order changes | Give `key` a stable per-element string (an id from the data) |
| Show only the top N matches | `limit = 50` |
| Lay items out horizontally | `direction = "Horizontal"` |
| Filter as the user types | Bind `source` to a `map` of the query, as the [textfield](textfield.md) example does |
| Show an empty state | A sibling with `visible = items:map(function(all) return not all or #all == 0 end)` |

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
| A 2000-item `list` makes every update slow | Every item is laid out on every pass, and built whenever anything it read changes, visible or not. Cap it with `limit`, filter the `source`, or hide the list while it is closed |
| A `list` rebuilds though nothing it shows changed | Its `itemfn` or `key` is a new function each time the builder around it runs, as inside a function `child` that reads a signal. Define them once, outside the builder |
| A relative time ("3 min ago") in an item stops updating | `itemfn` read the clock with `os.time()`. Bind the text to a `map` of `mantle.system` instead ([what a node reads again](../guide/signals.md#what-a-node-reads-again)) |
| A `list` of more than 10000 elements is refused | Set `limit`, or page the `source` |
| `duplicate key` error | `key` must return a different string for every element |
| `key` returning a number is refused | Return a string: `tostring(item.id)` |
| Items lose their state when one is added at the top | Without `key` they match by position. Add `key` |
| `background` on a `list` is refused | A list is not a box. Wrap it |

See also: [row and column](row-column.md), [signals](../guide/signals.md), [input: scroll](../guide/input.md#scroll).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [list parser](../../renderer/src/layout/node/spec.rs),
[layout as row or column](../../renderer/src/layout/scene/solver.rs).
