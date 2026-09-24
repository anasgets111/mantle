# Nodes

Nodes are the UI tree inside a [surface](surfaces.md): boxes, rows, text, icons, images, lists and
shaders. Each constructor (`row { ... }`, `text { ... }`) takes a property table and returns it
tagged with its kind. Read this page to place things and to look up what each kind accepts. How a
box looks (fills, borders, shadows, blurs) is on [paint](paint.md). Motion is on
[animation](animation.md). Clicks and typing are on [input](input.md).

A bar with a left group, a centred clock and a right group:

```lua
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

The two side rows are `"Fill"`, so they split what the clock leaves over equally, which puts the
clock at the exact centre whatever its width. The right row packs its children at its end.

## Layout model

A **pass** is one resolve of a surface: the engine re-runs the node tree's signals, re-lays it out
and repaints. It happens after a signal the surface reads changes ([signals](signals.md#how-re-resolution-works)).
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

### Sizes

`width` and `height` take the same values.

| Value | Size |
| :--- | :--- |
| Omitted | Content: text and icons measure themselves, containers wrap their children, other leaves are 0 |
| Number | Pixels, `[0, 8192]` |
| `"Fill"` | Along the parent's main axis: an equal share of the space the fixed and content-sized siblings leave. Across it, or in a stacking parent: the whole slot, whatever `align_h`/`align_v` say |
| `"NN%"` (`"50%"`, `"12.5%"`) | A fraction of the parent's content box (inside its padding). It needs a parent with a definite size on that axis |

There is no `"Content"` literal; omit the property instead.

Children never shrink. When fixed and content-sized children overflow a row, they keep their sizes
and spill out (clipped by the parent's `clip`), and `"Fill"` siblings get 0. A `"Fill"` child
along the main axis of a content-sized parent also gets 0: there is no remainder to share. Across
the axis, `"Fill"` in a content-sized parent takes the widest sibling's size.

`min_width`, `min_height`, `max_width` and `max_height` are pixels `[0, 8192]` (not `"Fill"` or
percents). They clamp every size, content, fixed and `"Fill"` alike, as CSS does: a `"Fill"` capped
by `max_width` gives the rest to its `"Fill"` siblings. A floor above a ceiling wins. Content past a
ceiling overflows, and a `scroll` on the same node scrolls it ([input](input.md)).

### Spacing, padding and margin

| Property | Meaning |
| :--- | :--- |
| `padding` | Inside the node's box, around its children or text |
| `margin` | Outside the box; part of the room the node takes in its parent |
| `spacing` | Gap between visible children of a `row`, `column` or `list`. Negative values overlap them |

`padding` and `margin` take a number for all four edges or `{ top, right, bottom, left }` with
missing edges 0. Neither is range-checked, so negatives are accepted. A hidden child adds no gap.

### Alignment

`align_h` and `align_v` take `"Start"`, `"Center"`, `"End"` or `"Stretch"`, default `"Start"`. What
they do depends on the axis.

| Where | `align_h` / `align_v` does |
| :--- | :--- |
| A child in a stacking parent | Places the child in the parent's content box on that axis |
| A child in a flow, across its main axis | Places the child across the row's height or the column's width |
| A child in a flow, along the main axis | Ignored: the parent packs that axis |
| A `row`'s own `align_h`, a `column`'s own `align_v` | Packs its children along the main axis (`"Stretch"` packs like `"Start"`). The same value also places the container itself in its parent |

`"Stretch"` across an axis fills the slot and overrides a fixed size on that axis. To space items
out along a row, use `"Fill"` children as spacers.

`text_align` is a different thing: it places lines inside a text node's own box, and matters only
when that box is wider than the text.

### Identity and reconciliation

The tree is rebuilt from Lua on every pass, then matched against the nodes already on screen, one
parent at a time. A matched node keeps its state: running tweens, a held image, a capture stream, a
text field's draft. An unmatched old node is removed, and plays its `animate.exit` first
if it has one ([animation](animation.md)).

| Child | Matches |
| :--- | :--- |
| With an `id` | The old sibling with the same `id`, wherever it moved. No such sibling: a new node |
| Without an `id` | The old id-less siblings, in order |
| Either, with a different kind | Nothing: the old one is removed and a new one built |

An `id` is a plain UTF-8 string, unique among its siblings (a duplicate is refused), never a
signal. In a `list`, `key` supplies it. Give a node a stable `id` when:

- Siblings before it come and go. A position shift pairs it with the wrong old node.
- It holds state across changes: an `image` with `retain` or `transition`, a `capture`, a `textfield`.
- It replaces another node of the same kind and should not reuse it. When switched views are both a
  `column` without ids, the new view is the old node with new properties, so no exit or entry
  plays. Distinct ids make it a real replacement.

### Showing, hiding and switching

`visible = false` takes a node out of layout, paint and input, with no gap. Its subtree stays in
memory, frozen: no signal under it is read and no `list` builds, until it shows again. Use it for a
section you toggle in place. For views that replace each other, bind the parent's `children`
([switching views](signals.md#switching-views)). Only the current view is built.

`opacity = 0` is different: the node still takes space and still takes input.

## Kinds

Every node kind accepts the common properties. Box kinds also accept the [box paint](#box-properties)
properties. Any other name raises an error that lists what the kind accepts, so a typo such as
`aling_v` fails loudly.

| Kind | Common | Box | Own properties |
| :--- | :---: | :---: | :--- |
| [`rect`](#rect) | ✓ | ✓ | `children` |
| [`row`, `column`](#row-and-column) | ✓ | ✓ | `children`, `spacing`, `scroll` |
| [`button`](#button) | ✓ | ✓ | `children`, `on_click`, `on_drag`, `on_wheel`, `submit` |
| [`text`](#text) | ✓ | | `content`, `font`, `font_size`, `foreground`, `text_align`, `elide`, `wrap`, `max_lines`, `on_link` |
| [`icon`](#icon) | ✓ | | `name`, `size`, `foreground` |
| [`image`](#image) | ✓ | | `source`, `fit`, `async`, `retain`, `transition`, `source_blur` |
| [`capture`](#capture) | ✓ | | `output`, `fit`, `live`, `region`, `paint_cursor` |
| [`shader`](#shader) | ✓ | | `source`, `progress`, `params` |
| [`list`](#list) | ✓ | | `source`, `itemfn`, `key`, `limit`, `direction`, `spacing`, `scroll` |
| [`textfield`](#textfield) | ✓ | | `placeholder`, `font_size`, `foreground`, `text_align`, `autofocus`, `on_change`, `on_submit`, `on_cancel`, `on_navigate`, `secure_submit`, `mask_character` |

Any property can hold a [signal](signals.md) except `id` and callbacks. A signal nested inside a
property table does not resolve: derive the whole table.

### Common properties

| Property | Values | Default |
| :--- | :--- | :--- |
| `id` | String, unique among siblings; see [identity](#identity-and-reconciliation) | None |
| `width`, `height` | See [sizes](#sizes) | Content |
| `min_width`, `min_height`, `max_width`, `max_height` | Pixels `[0, 8192]` | None |
| `padding`, `margin` | Number or `{ top, right, bottom, left }` | 0 |
| `align_h`, `align_v` | `"Start"`, `"Center"`, `"End"`, `"Stretch"`; see [alignment](#alignment) | `"Start"` |
| `visible` | Boolean; `false` removes the node and freezes its subtree | `true` |
| `opacity` | `[0, 1]`, multiplied down the tree | 1 |
| `z` | Finite number. Siblings paint and hit-test in ascending `z`, ties in declaration order. Layout and focus order ignore it; it cannot animate | 0 |
| `cursor` | One of the [cursor names](#cursor-names). The innermost node under the pointer that sets one wins | `"pointer"` on a `button` with a handler or `submit` and over a link, `"text"` on a `textfield`, else the arrow |
| `scale` | Number or `{ x, y }`, `[0, 64]` | 1 |
| `rotate` | Degrees clockwise, `[-8192, 8192]` | 0 |
| `translate` | `{ x, y }` px, `[-8192, 8192]`, applied after scale and rotate | `{ x = 0, y = 0 }` |
| `origin` | `{ x, y }` fractions of the box `[0, 1]`: the pivot for scale and rotate | `{ x = 0.5, y = 0.5 }` |
| `hover` | A `hover(name)` signal the engine sets while the pointer is over this node or its children ([input](input.md#hover)) | None |
| `on_hover(inside)` | Called with `true`/`false` when the pointer crosses the node's edge. Refused without `hover` on the same node | None |
| `geometry` | A `geometry(name)` signal the pass writes this node's surface-local rect into ([signals](signals.md#geometry-read-a-nodes-laid-out-rect)) | None |
| `animate` | Per-property tweens and an `exit` block ([animation](animation.md)) | None |
| `shadow_color`, `shadow_blur`, `shadow_offset`, `shadow_spread`, `content_blur` | See [shadows](paint.md#shadows) and [blurs](paint.md#blurs) | None |

`scale`, `rotate` and `translate` are paint-only, like CSS `transform`: the node and its subtree
draw moved, but layout, siblings and `geometry` see the untransformed box. Hit-testing follows the
painted box. Tween them for motion that does not re-lay out the surface.

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

### Box properties

`rect`, `row`, `column`, `button` and the four surface roles also take `background`, `radius`,
`corner_shape`, `border_color`, `border_width`, `clip`, `mask`, `blur`, `backdrop_blur` and
`shadow_mode`. They are documented on [paint](paint.md#box-properties). `clip` decides what this
node's children are cut to; the default `"Box"` cuts them to its rectangle.

## rect

A plain box that stacks its children: each one gets the whole content box and places itself with
`align_h`/`align_v`. Use it for a filled shape, a background behind something, or layering.

| Property | Values | Default |
| :--- | :--- | :--- |
| `children` | Array of nodes, up to 10000; a `nil` hole ends it. A signal of an array switches views | None |

## row and column

| Property | Values | Default |
| :--- | :--- | :--- |
| `children` | Array of nodes, up to 10000; a hole ends it. A signal of an array switches views | None |
| `spacing` | Px between visible children | 0 |
| `scroll` | A `scroll(name)` signal: makes the node a scrolling viewport along its main axis ([input](input.md)) | None |

A meter: a `"Fill"`-wide track with a percentage-wide fill that follows a signal.

```lua
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
```

## button

A `rect` that takes the pointer. It stacks its children like a `rect`, so put a `row` inside for an
icon and a label side by side. A `button` with no handler and no `submit` does not take clicks; they
fall through to what is under it. `rect` in the callbacks is the button's surface-local box
`{ x, y, width, height }`.

| Property | Values | Default |
| :--- | :--- | :--- |
| `children` | As on `rect` | None |
| `on_click(rect, button)` | `button` is `"left"`, `"right"` or `"middle"`. Fires on release over the button that was pressed | None |
| `on_drag(rect, pointer, phase)` | Left-button drag. `pointer` is `{ x, y }` relative to the button, unclamped; `phase` is `"start"`, `"move"` or `"end"` | None |
| `on_wheel(rect, steps)` | Vertical wheel in notches, positive away from the user, fractional on touchpads | None |
| `submit` | `true`: a click also submits the armed [secure field](input.md#secure-fields), like Enter | `false` |

The full pointer rules (cancel, drag end, wheel routing) are on [input](input.md#pointer).

## text

| Property | Values | Default |
| :--- | :--- | :--- |
| `content` | A string, or an array of up to 10000 runs `{ text, bold?, italic?, underline?, color?, href? }` drawn as one paragraph | `""` |
| `font` | A family name placed before the [`fonts`](scripting.md#fonts) chain for this node. An unknown family falls back to the chain; `""` is refused | The chain |
| `font_size` | `[1, 8192]` | 12 |
| `foreground` | Colour | `"#FFFFFF"` |
| `text_align` | `"Start"`, `"Center"`, `"End"`. `"Start"` and `"End"` follow each line's reading direction | `"Start"` |
| `wrap` | `"None"`: one line. `"Word"`: break at words, mid-word for a word wider than the box | `"None"` |
| `max_lines` | Line cap under `wrap = "Word"`; 0 is unlimited. Ignored without `wrap` | 0 |
| `elide` | `"None"` or `"End"`: end an over-long line with `…`. Under `wrap`, applies to the last kept line | `"None"` |
| `on_link(href)` | Called when a run with an `href` is clicked. The engine never opens the link | None |

A text node sizes to its content. A run's `color` overrides `foreground`, and `bold` or `italic`
use the family's bold or italic face when one exists. Empty runs are skipped.

`wrap` and `elide` need a box narrower than the text, so give the node a `width`, `"Fill"`, or a
stretched cross axis (a text in a fixed-width `column` wraps at the column's width). In a
content-sized `row`, the text measures one line and overflows instead.

A card with an icon and two lines: the title elides, the body wraps to two lines and elides the
second.

```lua
local function card(icon_name, title, body)
    return row {
        width = "Fill",
        padding = 12,
        spacing = 10,
        radius = 12,
        background = "#313244",
        children = {
            icon { name = icon_name, size = 32, align_v = "Center" },
            column { width = "Fill", align_v = "Center", spacing = 2, children = {
                text { content = title, width = "Fill", font_size = 14, elide = "End" },
                text { content = body, width = "Fill", foreground = "#A6ADC8",
                       wrap = "Word", max_lines = 2, elide = "End" },
            } },
        },
    }
end
```

The middle column is `"Fill"` so the texts have a bounded width; the icon keeps its 32 px.

## icon

| Property | Values | Default |
| :--- | :--- | :--- |
| `name` | An icon theme name (`"firefox"`, `"audio-volume-high-symbolic"`), or an absolute image path | `""`, drawing nothing |
| `size` | Px; the node is `size` × `size` | 12 |
| `foreground` | Colour for the SVG's `currentColor`, which tints symbolic icons. Full-colour icons ignore it | The file's own colours |

An explicit `width` or `height` overrides that axis of the square; the icon draws at the shorter
side.

## image

| Property | Values | Default |
| :--- | :--- | :--- |
| `source` | A file path, written absolute (`mantle.config_dir .. "/img/a.png"`). Never a theme name; use `icon` for those. Animated GIFs loop | `""`, drawing nothing |
| `fit` | `"cover"` (fill and crop), `"contain"` (fit inside), `"stretch"` | `"cover"` |
| `async` | `true` decodes on a worker thread and draws nothing until the picture is ready. `false` decodes in the frame that first draws it | `false` |
| `retain` | Keep drawing the previous picture while a new `source` decodes, and when a decode fails. Needs `async = true` and a stable identity | `false` |
| `transition` | `{ duration, easing?, shader?, params? }`: cross from the held picture to each newly decoded `source`. Implies `retain`. Unknown keys are refused | None |
| `source_blur` | Blur sigma in px baked into the pixels once at decode (three box passes approximating a Gaussian), `[0, 8192]`; see [blurs](paint.md#blurs) | 0 |

An image has no intrinsic size: without `width` and `height` it is 0 × 0. Use `async` for large
files or many thumbnails, since an inline decode runs on the thread that draws the shell.

`transition` fields: `duration` in ms `[1, 60000]` (required); `easing` as in
[animation](animation.md), default `"InOutQuad"`; `shader`, an absolute `.frag` path replacing the
built-in cross-dissolve; `params`, uniforms for that shader (refused without `shader`). The first
picture appears without a transition. A transition shader gets everything a
[shader node](#shader) gets, plus:

| Name | What |
| :--- | :--- |
| `mantle_from(uv)`, `mantle_to(uv)` | Outgoing and incoming picture at a box coordinate, premultiplied, already placed by `fit`; transparent outside the picture |
| `u_from_rect`, `u_to_rect` | Each picture's `(x, y, w, h)` in box fractions; may pass `0..1` under `"cover"` |

`u_progress` is the eased progress `0..1`. A transition shader that fails to build logs once and
the node falls back to the cross-dissolve.

A wallpaper that crossfades when the path changes:

```lua
local path = state("wallpaper", "/usr/share/backgrounds/a.jpg")

return { panel {
    id = "wallpaper",
    layer = "Background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "Fill",
    height = "Fill",
    child = image {
        id = "wallpaper_image", -- keeps the node, and so the held picture, across source changes
        source = path,
        async = true,
        transition = { duration = 600, easing = "InOutCubic" },
        width = "Fill",
        height = "Fill",
    },
} }
```

## capture

A live preview of one output (monitor), through the compositor's screen-capture protocol
(`ext-image-copy-capture-v1`, or `wlr-screencopy`).

| Property | Values | Default |
| :--- | :--- | :--- |
| `output` | Connector name, e.g. `"DP-1"`. A name that is not connected draws nothing and logs one warning | `""`, drawing nothing |
| `fit` | As for `image` | `"cover"` |
| `live` | `false`: one frame on show and on each `output` change. `true`: every frame, one in flight. A number: at most that many frames per second, `(0, 1000]` | `false` |
| `region` | `{ x, y, width, height }` in the output's logical px: every key required, each `[0, 8192]`, size non-zero. Placed by `fit` as if it were the whole frame | The whole output |
| `paint_cursor` | Include the pointer in the frame | `false` |

It has no intrinsic size: without `width` and `height` it draws nothing. Capture pauses while the
node is hidden or its surface unmapped, and starts fresh when it shows again. New frames arrive only
when the screen changes.

## shader

Runs a fragment shader from the config over the node's box: a glow, a gradient animation, a
procedural pattern. It reads no textures and takes no input; wrap it in a `button` to click it.

| Property | Values | Default |
| :--- | :--- | :--- |
| `source` | Absolute `.frag` path; a relative one is refused | `""`, drawing nothing |
| `progress` | `u_progress`, `[-8192, 8192]`. There is no clock uniform: animate this for motion | 0 |
| `params` | `{ name = number \| { 2 to 4 numbers } }`: values for the shader's own `float` and `vec2` to `vec4` uniforms | `{}` |

It has no intrinsic size: without `width` and `height` it draws nothing. `opacity`, transforms,
`shadow_*` and `content_blur` apply to it.

What the `.frag` file is: GLSL ES 3.00 without the header. The engine prepends
`#version 300 es`, `precision highp float` and these declarations, then compiles your file as
written. Error line numbers count from your first line.

| Name | Type | What |
| :--- | :--- | :--- |
| `v_uv` | `in vec2` | Box coordinate, `0..1`, top-left origin, y down |
| `fragColor` | `out vec4` | Write premultiplied RGBA. The engine multiplies it by the node's opacity afterwards |
| `u_progress` | `float` | The node's `progress` |
| `u_size` | `vec2` | The node's size in logical px |
| Your own `uniform float`/`vec2`/`vec3`/`vec4` | | Set from `params` by name. Missing ones are 0; `params` names the shader has no uniform for are ignored; a wrong component count is padded or truncated and logged once |

Write `void main()`. Names starting `u_` or `mantle_` are reserved. A uniform of any other type
(an `int`, a `sampler2D`) refuses the shader. A shader that fails to compile or link logs once and
draws nothing until the file changes; saving the file recompiles it. A shader that hangs the GPU
hangs the session.

`mantle check` has no GPU and does not compile shaders. The first compile happens in the running
shell, and errors appear in `mantle log`.

A band that glows in when `pulse_on` turns true:

```lua
local pulse_on = state("pulse_on", false)

local glow = shader {
    width = 200,
    height = 40,
    source = mantle.config_dir .. "/shaders/glow.frag",
    progress = pulse_on:map(function(on) return on and 1 or 0 end),
    params = { tint = { 0.54, 0.71, 0.98 } },
    animate = { progress = 400 },
}
```

`shaders/glow.frag` in the config directory:

```glsl
uniform vec3 tint;

void main() {
    // Distance from the horizontal centre line, 0 at the middle, 1 at the edges.
    float edge = abs(v_uv.y - 0.5) * 2.0;
    float alpha = (1.0 - edge) * u_progress;
    fragColor = vec4(tint * alpha, alpha); // premultiplied
}
```

## list

A `row` or `column` whose children come from data: one `itemfn(item)` call per element of `source`.

| Property | Values | Default |
| :--- | :--- | :--- |
| `source` | Array (required); bind a signal to rebuild on change. More than 10000 elements is refused unless `limit` caps it | Required |
| `itemfn(item)` | Returns one node for an element (required) | Required |
| `key(item)` | Returns a unique UTF-8 string per element, used as that item's `id`. Duplicates are refused. Without it, items match by position | None |
| `limit` | Non-negative integer: build at most this many items. Values above 10000 act as 10000 | None |
| `direction` | `"Vertical"` or `"Horizontal"` | `"Vertical"` |
| `spacing` | Px between items | 0 |
| `scroll` | A `scroll(name)` signal, as on `row` and `column` | None |

A list builds every item it is given on every pass that re-resolves it, including items scrolled
out of view. `key` makes matching cheap and keeps each item's state across reorders; it does not
skip `itemfn`. Bound long lists with `limit` (a launcher showing the top 50 matches), or hide them
while closed so they freeze.

Workspace buttons, keyed by workspace id so a new workspace does not shift the others' state:

```lua
local strip = list {
    direction = "Horizontal",
    spacing = 4,
    align_v = "Center",
    source = mantle.workspaces:map(function(workspaces)
        local output = workspaces and (workspaces.outputs or {})[1]
        return output and output.workspaces or {}
    end),
    key = function(workspace) return tostring(workspace.id) end,
    itemfn = function(workspace)
        return button {
            width = 24,
            height = 24,
            radius = 12,
            background = workspace.populated and "#45475A" or "#00000000",
            on_click = function() mantle.workspaces:invoke("focus", workspace.id) end,
            children = { text { content = tostring(workspace.idx), align_h = "Center", align_v = "Center" } },
        }
    end,
}
```

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

## textfield

A single-line text input. It has no intrinsic size, so give it `width` and `height`. The text is
vertically centred in the box and drawn in the `fonts` chain (no `font` property). Focus, editing
keys and the draft's lifetime are on [input](input.md#text-fields).

| Property | Values | Default |
| :--- | :--- | :--- |
| `placeholder` | Shown while the draft is empty, focused or not. Never submitted | `""` |
| `font_size` | `[1, 8192]` | 12 |
| `foreground` | Colour of the text and placeholder | `"#FFFFFF"` |
| `text_align` | `"Start"`, `"Center"`, `"End"` | `"Start"` |
| `autofocus` | `true`: take the keyboard, empty, when the surface gets it or the field appears | `false` |
| `on_change(text)` | Whole draft after every edit | None |
| `on_submit(text)` | Enter, with the whole draft; the draft then clears | None |
| `on_cancel(cleared)` | Escape; `cleared` says whether text was removed | None |
| `on_navigate(key)` | `"up"`, `"down"`, `"left"`, `"right"`, `"page_up"`, `"page_down"`, `"tab"`, `"backtab"`: keys the field does not use | None |
| `secure_submit` | `{ capability, action }`: `lock`/`authenticate`, `polkit`/`authenticate` or `network`/`connect` are handled; any other pair is accepted and its secret discarded. Makes the field masked; keys never reach Lua ([secure fields](input.md#secure-fields)) | None |
| `mask_character` | Drawn once per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length | `"•"` |

A field with none of `on_change`, `on_submit` and `secure_submit` never takes focus.

## Switching views with ids

Views swapped through a `children` signal, each with its own `id` so the outgoing one fades out
while the incoming one fades in. The parent is a `rect`, so the two overlap during the swap instead
of stacking in a column.

```lua
local tab = state("tab", "wifi")

local function page(name, label)
    return column {
        id = name, -- a new id per view: the old view leaves (and fades) instead of being reused
        padding = 12,
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
```

## How do I…

| Task | Recipe |
| :--- | :--- |
| Centre something | [Below](#centre-something) |
| Split a bar into left, centre and right | The [bar at the top](#nodes): two `"Fill"` rows around a content-sized middle |
| Truncate long text | `width` (or `"Fill"`) plus `elide = "End"`; for several lines add `wrap = "Word"` and `max_lines`. See the [card](#text) |
| Show a progress bar | The [meter](#row-and-column): a percentage-width `rect` in a `"Fill"` track |
| Lay out a grid | The [thumbnail grid](#list): a `list` of `row`s, several items per row |
| Scroll a long list | [Below](#scroll-a-long-list) |
| Show an app's icon | [Below](#show-an-apps-icon) |
| Round an image's corners | [Below](#round-an-images-corners) |
| Switch between tabs | [Switching views with ids](#switching-views-with-ids) |
| Make something clickable | Wrap it in a [`button`](#button) with `on_click` |

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

### Scroll a long list

Bound the size on the scrolling axis, then bind a [`scroll`](input.md#scroll) signal. `max_height`
lets the list shrink to fit a few items and scroll past 200 px.

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

### Show an app's icon

`icon { name }` takes a theme name. [`mantle.applications`](capabilities.md#applications) maps a
window's `app_id` to its desktop entry, whose `icon` is that name. This shows the focused window's
icon and title:

```lua
local focused_icon = computed({ mantle.applications, mantle.workspaces }, function(apps, workspaces)
    local client = workspaces and workspaces.active_client
    if apps == nil or client == nil then return "" end
    local index = apps.by_app_id[client.class] or apps.by_app_id[string.lower(client.class)]
    return index and apps.entries[index].icon or ""
end)

local app_badge = row { spacing = 6, align_v = "Center", children = {
    icon { name = focused_icon, size = 18, align_v = "Center" },
    text { content = mantle.workspaces:map(function(w)
        return w and w.active_client and w.active_client.title or ""
    end), width = 200, elide = "End", align_v = "Center" },
} }
```

### Round an image's corners

An `image` has no `radius`. Put it in a box with `radius` and `clip = "Rounded"`
([clip](paint.md#clip)):

```lua
local cover = rect {
    width = 96,
    height = 96,
    radius = 12,
    clip = "Rounded", -- cut the image to the corners
    children = {
        image { source = "/usr/share/backgrounds/a.jpg", fit = "cover", async = true, width = "Fill", height = "Fill" },
    },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `width = "Content"` is refused | Omit the property; content size is the default |
| `mantle check` passes a tree with a misspelled or malformed node property | `check` evaluates the config and validates each surface's own properties, but never lays out, so nothing under `child` is parsed. Node errors appear when the running shell lays the tree out (in the bar or `mantle log`) |
| A `wrap = "Word"` text runs off the edge on one line | Wrapping needs a bounded width: set `width`, `"Fill"`, or put it in a fixed-width column. A content-sized row offers none |
| `elide = "End"` never ellipsizes | Same cause: the box is as wide as the text. Bound the width |
| An `image`, `capture`, `shader` or `textfield` does not appear | They have no intrinsic size. Give `width` and `height`, or `"Fill"` in a sized parent |
| A `"Fill"` child is 0 wide | Its parent is content-sized along that axis, or fixed siblings already overflow. Size the parent |
| `"50%"` resolves to 0 | The parent has no definite size on that axis |
| `align_h = "Center"` on a child of a `row` does nothing | The row packs its main axis: set `align_h` on the row, or use `"Fill"` spacers |
| Items in a `row` inside a `button` overlap | `button` and `rect` stack their children; put a `row` inside for side by side |
| An image flashes blank when its `source` changes despite `retain` | `retain` needs `async = true` and a node that survives: give it a stable `id` |
| A switched view snaps in without its entry or exit animation | Same kind at the same position is reused, not replaced. Give each view its own `id` |
| A 2000-item `list` makes every update slow | Every item is built on every pass, visible or not. Cap it with `limit`, filter the `source`, or hide the list while it is closed |
| A `list` of more than 10000 elements is refused | Set `limit`, or page the `source` |
| `duplicate id` error | Sibling ids, and `list` keys, must be unique |
| A shader draws nothing and `check` passed | `check` does not compile GLSL. Read `mantle log` for the compile error, and check the node has a size and an absolute `source` |

Source: [node vocabulary](../../renderer/src/lua/nodes.rs),
[layout solver](../../renderer/src/layout/scene/solver.rs),
[pass and reconciliation](../../renderer/src/layout/scene/pass.rs),
[geometry parsers](../../renderer/src/layout/node/style/mod.rs),
[transforms](../../renderer/src/layout/node/style/transform.rs),
[content parsers](../../renderer/src/layout/node/content.rs),
[lists](../../renderer/src/layout/node/spec.rs),
[image transition](../../renderer/src/layout/node/animate/transition.rs),
[shader stage](../../renderer/src/layout/image_shader/mod.rs),
[cursor parse](../../renderer/src/layout/node/style/mod.rs) (names from the `cursor-icon` crate),
[capture](../../renderer/src/wayland/capture/mod.rs),
[check](../../renderer/src/check.rs).

See also: [surfaces](surfaces.md) (where a tree lives), [signals](signals.md) (live properties),
[paint](paint.md), [animation](animation.md), [input](input.md), [capabilities](capabilities.md).
