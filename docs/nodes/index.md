# Nodes

Nodes are the UI tree inside a [surface](../surfaces/index.md). Each constructor (`row { ... }`,
`text { ... }`) takes a property table and returns it tagged with its kind. This page covers layout
and the properties every kind shares; each kind's page covers what it adds. How a box looks is on
[paint](../guide/paint.md), motion on [animation](../guide/animation.md), clicks and typing on
[input](../guide/input.md).

A bar with a left group, a centred clock and a right group:

```lua,shot
local clock = state("clock", "12:00")

local bar = panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    exclusive = 32,
    width = "Fill",
    height = 32,
    child = row {
        width = "Fill",
        height = "Fill",
        padding = { left = 8, right = 8 },
        background = "#1E1E2ECC",
        children = {
            row { width = "Fill", align_v = "Center", spacing = 6, children = {
                text { content = "left", foreground = "#CDD6F4" },
            } },
            text { content = clock, align_v = "Center", foreground = "#CDD6F4" },
            row { width = "Fill", align_h = "End", align_v = "Center", spacing = 6, children = {
                text { content = "right", foreground = "#CDD6F4" },
            } },
        },
    },
}

return { bar }
```

The two side rows are `"Fill"`, so they split what the clock leaves equally, and the clock sits at
the exact centre whatever its width. The right row packs its children at its end.

## Kinds

Every kind accepts the [common properties](#common-properties). Box kinds also accept the
[box properties](#box-properties). Any other key raises an error: a typo such as `aling_v` asks
"did you mean `align_v`?", and a key close to nothing lists what the kind accepts.

| Kind | Page | Box | Children | Own properties |
| :--- | :--- | :---: | :--- | :--- |
| `rect` | [rect](rect.md) | ✓ | Stacked | `children` |
| `row`, `column` | [row and column](row-column.md) | ✓ | Flow | `children`, `spacing`, `scroll` |
| `button` | [button](button.md) | ✓ | Stacked | `children`, `on_click`, `on_drag`, `on_wheel`, `submit` |
| `list` | [list](list.md) | | Flow, from data | `source`, `itemfn`, `key`, `limit`, `direction`, `spacing`, `scroll` |
| `text` | [text](text.md) | | Leaf | `content`, `font`, `font_size`, `foreground`, `text_align`, `elide`, `wrap`, `max_lines`, `on_link` |
| `icon` | [icon](icon.md) | | Leaf | `name`, `size`, `foreground` |
| `image` | [image](image.md) | | Leaf | `source`, `fit`, `async`, `retain`, `transition`, `source_blur` |
| `capture` | [capture](capture.md) | | Leaf | `output`, `fit`, `live`, `region`, `paint_cursor` |
| `shader` | [shader](shader.md) | | Leaf | `source`, `progress`, `params` |
| `textfield` | [textfield](textfield.md) | | Leaf | `placeholder`, `font_size`, `foreground`, `text_align`, `autofocus`, `on_change`, `on_submit`, `on_cancel`, `on_navigate`, `secure_submit`, `mask_character` |

The four surface roles (`panel`, `window`, `popup`, `lock`) are node kinds too: they take the common
and box properties and stack their one `child` ([surfaces](../surfaces/index.md)).

### Values

| Rule | Detail |
| :--- | :--- |
| Types | A property table's Type column is the editor stubs' LuaCATS type. `Bound` means it also takes a signal; `Length` is a [size](#sizes); `Edges` is `{ top, right, bottom, left }` with missing edges 0; `Axes` is `{ x, y }` with a missing axis at the property's default; `Color` is a colour; `Animations` is per-property [tweens](../guide/animation.md), keyed by the node's own properties (`RectAnimations` on a `rect`). A range after the type is checked |
| Signals | A property whose Type includes `Bound` takes a [signal](../guide/signals.md); `id` and callbacks do not. `hover`, `scroll` and `geometry` take the signal handle itself. A signal inside a table property is refused: derive the whole table |
| `nil` | A signal reading `nil` leaves its property absent, at its default. Capabilities read `nil` until their first push, so binding one never fails layout |
| Numbers | Finite. A value outside a property's range is an error, not a clamp |
| Colours | `"#RRGGBB"` or `"#RRGGBBAA"` ([colours](../guide/paint.md#colours)) |
| Strings | Capped at 64 KB |
| Arrays | `children`, `list.source` and `text` runs take at most 10000 elements. A `nil` hole in `children` is an error; in `list.source` and runs it ends the array |
| Tables | A table property (`padding`, `anchor`, `shadow_offset`, `transition`, an `animate` entry, a run, ...) refuses a key it does not take, so `{ topp = 4 }` names `topp` |
| Callbacks and booleans | Every `on_*` takes only a function, and every `boolean` property (`visible`, `submit`, ...) only `true` or `false`. `on_click = "x"` or `submit = 1` is an error. For a conditional handler write `cond and fn or nil`: `false` is refused too |

## Layout model

A **pass** is one resolve of a surface: the engine re-runs the node tree's signals, re-lays it out
and repaints. It happens after a signal the surface reads changes ([signals](../guide/signals.md#how-re-resolution-works)).
Layout is flexbox, solved by [taffy](https://github.com/DioxusLabs/taffy) on every pass. Each
container either **flows** its children along one axis or **stacks** them on top of each other.

| Kind | Children | Main axis |
| :--- | :--- | :--- |
| `row` | Flow left to right | Horizontal |
| `column` | Flow top to bottom | Vertical |
| `list` | Flow, generated from `source` | `direction`: vertical by default |
| `rect`, `button`, every surface | Stack: each child gets the whole content box and aligns in it on its own. Later children paint over earlier ones | None |
| `text`, `icon`, `image`, `capture`, `shader`, `textfield` | None (leaves) | None |

A stacking parent's content size is the union of its children, so a `rect` is how you layer a badge
over an icon or a label over an image.

Of the leaves, only `text` and `icon` measure themselves. `image`, `capture`, `shader` and
`textfield` have no intrinsic size: without `width` and `height` they are 0 × 0 and draw nothing.

### Sizes

`width` and `height` take the same values.

| Value | Size |
| :--- | :--- |
| Omitted | Content: `text` and `icon` measure themselves, containers wrap their children, other leaves are 0 |
| Number | Pixels, `[0, 8192]` |
| `"Fill"` | Along the parent's main axis: an equal share of the space the fixed and content-sized siblings leave. Across it, or in a stacking parent: the whole slot, whatever `align_h`/`align_v` say |
| `"NN%"` (`"50%"`, `"12.5%"`) | A fraction of the parent's content box (inside its padding). It needs a parent with a definite size on that axis |

There is no `"Content"` literal; omit the property instead.

Children never shrink. Fixed and content-sized children that overflow a row keep their sizes and
spill out, cut by the parent's `clip`, and `"Fill"` siblings get 0. A `"Fill"` child along the main
axis of a content-sized parent also gets 0: there is no remainder to share. Across the axis,
`"Fill"` in a content-sized parent takes the largest sibling's size.

`min_width`, `min_height`, `max_width` and `max_height` are pixels `[0, 8192]` (not `"Fill"` or
percents). They clamp every size, content, fixed and `"Fill"` alike, as in CSS: a `"Fill"` capped
by `max_width` leaves the rest to its `"Fill"` siblings. A floor above a ceiling wins. Content past
a ceiling overflows; a `scroll` on the same node scrolls it ([scroll](../guide/input.md#scroll)).

### Spacing, padding and margin

| Property | Meaning |
| :--- | :--- |
| `padding` | Inside the node's box, around its children or text |
| `margin` | Outside the box; part of the room the node takes in its parent |
| `spacing` | Gap between visible children of a `row`, `column` or `list`. Negative values overlap them |

`padding` and `margin` take a number for all four edges or `{ top, right, bottom, left }` with
missing edges 0. Neither is range-checked, so negatives are accepted. A hidden child adds no gap.

### Alignment

`align_h` and `align_v` take `"Start"`, `"Center"`, `"End"` or `"Stretch"`, default `"Start"`, and
act by axis:

| Where | `align_h` / `align_v` does |
| :--- | :--- |
| A child in a stacking parent | Places the child in the parent's content box on that axis |
| A child in a flow, across its main axis | Places the child across the row's height or the column's width |
| A child in a flow, along the main axis | Ignored: the parent packs that axis |
| A `row`'s own `align_h`, a `column`'s own `align_v` (a `list`'s along its `direction`) | Packs its children along the main axis (`"Stretch"` packs like `"Start"`). The same value also places the container itself in its parent |

`"Stretch"` across an axis fills the slot and overrides a fixed size on that axis. To space items
out along a row, use `"Fill"` children as spacers.

`text_align` on [`text`](text.md) and [`textfield`](textfield.md) is separate: it places lines
inside the node's own box, and matters only when that box is wider than the text.

## Common properties

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `width` | `Length\|Bound`, `[0, 8192]` | Content | See [sizes](#sizes) |
| `height` | `Length\|Bound`, `[0, 8192]` | Content | See [sizes](#sizes) |
| `max_width` | `number\|Bound`, `[0, 8192]` | None | Pixel ceiling, CSS `max-width`. Content past it overflows; a `scroll` on the same node scrolls it ([sizes](#sizes)) |
| `max_height` | `number\|Bound`, `[0, 8192]` | None | Pixel ceiling, as `max_width` |
| `min_width` | `number\|Bound`, `[0, 8192]` | None | Pixel floor, CSS `min-width`; wins over a lower `max_width` |
| `min_height` | `number\|Bound`, `[0, 8192]` | None | Pixel floor, as `min_width` |
| `margin` | `number\|Edges\|Bound` | `0` | Outside the box; part of the room the node takes in its parent. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin)) |
| `padding` | `number\|Edges\|Bound` | `0` | Inside the box, around its children or text. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin)) |
| `align_h` | `"Start"\|"Center"\|"End"\|"Stretch"\|Bound` | `"Start"` | See [alignment](#alignment) |
| `align_v` | `"Start"\|"Center"\|"End"\|"Stretch"\|Bound` | `"Start"` | See [alignment](#alignment) |
| `visible` | `boolean\|Bound` | `true` | `false` removes the node from layout, paint and spacing and freezes its subtree ([showing and hiding](#showing-hiding-and-switching)) |
| `opacity` | `number\|Bound`, `[0, 1]` | `1` | Multiplied down the tree. At `0` the node still takes space and input |
| `z` | `number\|Bound` | `0` | Sibling paint and hit order. Higher paints later and hits first; ties keep declaration order. Layout and focus ignore it; `animate` refuses it |
| `scale` | `number\|Axes\|Bound`, `[0, 64]` | `1` | About `origin`; a missing axis is `1`. Paint only: layout and `geometry` see the unscaled box; hit-testing follows the painted one |
| `rotate` | `number\|Bound`, `[-8192, 8192]` | `0` | Degrees clockwise about `origin`. Paint only |
| `translate` | `Axes\|Bound`, `[-8192, 8192]` | `{ x = 0, y = 0 }` | Pixel offset per axis, a missing one `0`, applied after `scale` and `rotate`. Paint only |
| `origin` | `Axes\|Bound`, `[0, 1]` | `{ x = 0.5, y = 0.5 }` | Pivot for `scale` and `rotate` as box fractions; a missing axis is `0.5` |
| `shadow_color` | `Color\|Bound` | `"#000000"` | A drop shadow ([shadows](../guide/paint.md#shadows)). Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set |
| `shadow_blur` | `number\|Bound`, `[0, 8192]` | `0` | CSS `box-shadow` blur radius in px |
| `shadow_offset` | `Axes\|Bound`, `[-8192, 8192]` | `{ x = 0, y = 0 }` | Shadow offset in px per axis. Follows the node's transform |
| `shadow_spread` | `number\|Bound`, `[-8192, 8192]` | `0` | Px the shadow grows per side; negative shrinks it. On non-box content it scales the shadow about the box centre |
| `content_blur` | `number\|Bound`, `[0, 8192]` | `0` | Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()` ([blurs](../guide/paint.md#blurs)). Clipped like a shadow |
| `animate` | `Animations\|Bound` | None | Per-property tweens and an `exit` block ([animation](../guide/animation.md)). Only a node already on screen animates, unless the entry has `from` |
| `id` | `string` | None | Unique among siblings; matches this node across passes ([identity](#identity-and-reconciliation)). Never a signal |
| `hover` | `Bound` | None | A `hover(name)` signal the engine sets while the pointer is over this node or its children ([hover](../guide/input.md#hover)) |
| `geometry` | `Bound` | None | A `geometry(name)` signal the pass writes this node's surface-local rect into ([geometry](../guide/signals.md#geometry-read-a-nodes-laid-out-rect)) |
| `cursor` | `Cursor\|Bound` | `"pointer"` on a `button` with a handler or `submit` and on a link, `"text"` on a `textfield`, else the arrow | One of the [cursor names](#cursor-names). The innermost node under the pointer that sets one wins |
| `on_hover` | `fun(hovered: boolean)` | None | Called on each hover edge from pointer Enter, Motion or Leave; layout changes under a still pointer do not call it. Refused without `hover` on the same node |
<!-- End of the generated table. -->

`scale`, `rotate` and `translate` act like CSS `transform`: the subtree draws moved, while layout,
siblings and `geometry` see the untransformed box. A node scaled to 0 takes no input. Tween them for
motion that skips re-layout.

### Cursor names

`cursor` takes the CSS cursor names that the Wayland cursor-shape protocol (`wp_cursor_shape_v1`)
also uses. Any other string is refused.

| Group | Names |
| :--- | :--- |
| General | `default`, `context-menu`, `help`, `pointer`, `progress`, `wait` |
| Selection | `cell`, `crosshair`, `text`, `vertical-text` |
| Drag and drop | `alias`, `copy`, `move`, `no-drop`, `not-allowed`, `grab`, `grabbing` |
| Resize | `e-resize`, `n-resize`, `ne-resize`, `nw-resize`, `s-resize`, `se-resize`, `sw-resize`, `w-resize`, `ew-resize`, `ns-resize`, `nesw-resize`, `nwse-resize`, `col-resize`, `row-resize` |
| Other | `all-scroll`, `zoom-in`, `zoom-out` |

The compositor draws the shape from its cursor theme.

## Box properties

`rect`, `row`, `column`, `button` and the four surface roles also take `background`, `radius`,
`corner_shape`, `border_color`, `border_width`, `clip`, `mask`, `blur`, `backdrop_blur` and
`shadow_mode`. They are documented on [paint](../guide/paint.md#box-properties). Leaves and `list`
take none of them: wrap one in a `rect` for a background, border or rounded clip.

## Identity and reconciliation

Each pass walks the declared tree and matches it against the nodes on screen, one parent at a time.
A matched node keeps its state: running tweens, a held image, a capture stream, a text field's
draft, and its resolved properties until a signal they read is written. An unmatched old node is removed, after its `animate.exit` if it has one
([exit](../guide/animation.md#exit)).

| Child | Matches |
| :--- | :--- |
| With an `id` | The old sibling with the same `id`, wherever it moved. No such sibling: a new node |
| Without an `id` | The old id-less siblings, in order |
| Either, with a different kind | Nothing: the old one is removed and a new one built |

An `id` is a plain UTF-8 string, unique among its siblings (a duplicate is refused), never a
signal. In a `list`, `key` supplies it. Give a node a stable `id` when:

- Siblings before it come and go. A position shift pairs it with the wrong old node.
- It holds state across changes: an `image` with `retain` or `transition`, a `capture`, a `textfield`.
- It replaces another node of the same kind. Two switched views that are both id-less `column`s
  match each other: the new view is the old node with new properties, so no exit or entry plays.

## Showing, hiding and switching

`visible = false` takes a node out of layout, paint and input, with no gap. Its subtree stays in
memory, [frozen](../guide/signals.md#how-re-resolution-works) until it shows again. Use it for a
section toggled in place; for views that replace each other, bind the parent's `children`
([switching views](../guide/signals.md#switching-views)). `opacity = 0` still takes space and
input.

### Switching views with ids

Views swapped through a `children` signal, each with its own `id`, so the outgoing one fades out
while the incoming one fades in. The parent is a `rect`, so the two overlap during the swap instead
of stacking. The shot switches `tab` to `"bluetooth"`:

<!-- shot: frames=0..150/30 -->
```lua,shot
local tab = state("tab", "wifi")

local function page(name, label)
    return column {
        id = name, -- a new id per view: the old view leaves and fades instead of being reused
        padding = 12,
        opacity = 1, -- `from` needs the property set
        animate = { opacity = { duration = 150, from = 0 }, exit = { duration = 150, opacity = 0 } },
        children = { text { content = label } },
    }
end

local views = {
    wifi = function() return page("wifi", "Wi-Fi networks") end,
    bluetooth = function() return page("bluetooth", "Bluetooth devices") end,
}

local body = rect {
    width = 300,
    children = tab:map(function(current) return { views[current]() } end),
}

return body
```

## How do I…

| Task | Answer |
| :--- | :--- |
| Split a bar into left, centre and right | The [bar at the top](#nodes): two `"Fill"` rows around a content-sized middle |
| Centre something | [rect: centre something](rect.md#centre-something) |
| Put a badge over an icon | [rect](rect.md): a stacking parent with the badge aligned to a corner |
| Push items to the far end of a row | [row and column](row-column.md#push-items-apart): a `"Fill"` spacer |
| Show a progress bar | The [meter](row-column.md): a percentage-width `rect` in a `"Fill"` track |
| Truncate long text | `width` (or `"Fill"`) plus `elide = "End"`; see [text](text.md) |
| Make something clickable | Wrap it in a [`button`](button.md) with `on_click` |
| Build rows from data, or a grid | [list](list.md) |
| Scroll a long list | [list: scroll a long list](list.md#scroll-a-long-list) |
| Show an app's icon | [icon](icon.md) |
| Round an image's corners | [image: round an image's corners](image.md#round-an-images-corners) |
| Switch between tabs | [Switching views with ids](#switching-views-with-ids) |
| Toggle a section in place | `visible = signal`; see [showing and hiding](#showing-hiding-and-switching) |
| Read where a node ended up | `geometry = geometry("name")` ([geometry](../guide/signals.md#geometry-read-a-nodes-laid-out-rect)) |
| Press feedback that does not re-lay out | Tween `scale` or `translate` ([animation](../guide/animation.md)) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `width = "Content"` is refused | Omit the property; content size is the default |
| An `image`, `capture`, `shader` or `textfield` does not appear | They have no intrinsic size. Give `width` and `height`, or `"Fill"` in a sized parent |
| A `"Fill"` child is 0 wide | Its parent is content-sized along that axis, or fixed siblings already overflow. Size the parent |
| `"50%"` resolves to 0 | The parent has no definite size on that axis |
| Items in a `button` or `rect` overlap | They stack their children; put a `row` inside for side by side |
| A switched view snaps in without its entry or exit animation | Same kind at the same position is reused, not replaced. Give each view its own `id` |
| `duplicate id` error | Sibling ids, and `list` keys, must be unique |
| A signal inside a table property (`padding = { top = sig }`) raises an error | Map the whole table: `padding = sig:map(function(v) return { top = v } end)` |
| `on_click = cond and fn` raises `expected a function` | A false `cond` yields `false`: write `cond and fn or nil` |
| `children = { a, cond and b, c }` raises `children[1]: expected a node table`, counting from 0 like the rest of the path | A false or nil entry is a hole. Build the array with `table.insert`, or a signal of the whole array |
| A node table, or a `children` array, changed in place after the config ran does not update | A node reads each `children` or `child` table once and keeps it while it holds that table. Bind the property to a signal, or `:set` a new table |
| `opacity = 0` hides a node but it still takes clicks | Use `visible = false` |

See also: [surfaces](../surfaces/index.md) (where a tree lives), [signals](../guide/signals.md)
(live properties), [paint](../guide/paint.md), [animation](../guide/animation.md),
[input](../guide/input.md), [capabilities](../capabilities/index.md).

Source: [node vocabulary](../../renderer/src/lua/nodes.rs),
[layout solver](../../renderer/src/layout/scene/solver.rs),
[pass and reconciliation](../../renderer/src/layout/scene/pass.rs),
[property resolution](../../renderer/src/layout/node/mod.rs),
[geometry parsers](../../renderer/src/layout/node/style/mod.rs),
[transforms](../../renderer/src/layout/node/style/transform.rs),
[hit testing and cursors](../../renderer/src/layout/hit.rs) (names from the `cursor-icon` crate),
[check](../../renderer/src/check.rs).
