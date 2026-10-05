# Paint

How a node looks: fills, gradients, corners, borders, clipping, masks, shadows and the four
blurs. Layout and per-kind properties are on [Nodes](../nodes/index.md); easing any of these
values is on [Animation](animation.md).

<!-- shot-alt: A rounded battery card with a translucent fill, a faint border and a soft shadow, showing a green level bar at 82% and the time left. -->
```lua,shot
column {
    width = 240,
    padding = 16,
    spacing = 10,
    background = "#1E1E2EF2",
    radius = 12,
    border_width = 1,
    border_color = "#FFFFFF1A",
    shadows = { { color = "#00000099", blur = 18, offset = { x = 0, y = 8 } } },
    children = {
        row {
            width = "fill",
            children = {
                text { content = "Battery", width = "fill", font_size = 14, font_weight = 700, foreground = "#CDD6F4" },
                text { content = "82%", font_size = 14, foreground = "#A6E3A1" },
            },
        },
        rect {
            width = "fill",
            height = 6,
            radius = 3,
            background = "#313244",
            children = { rect { width = "82%", height = 6, radius = 3, background = "#A6E3A1" } },
        },
        text { content = "3 h 10 min left", font_size = 12, foreground = "#A6ADC8" },
    },
}
```

A card: a translucent rounded fill, a hairline border and a soft shadow below it.

## Terms

| Term | Meaning |
| :--- | :--- |
| Box kind | A node that paints a box: `rect`, `row`, `column` and the four [surface](../surfaces/index.md) roles (`panel`, `window`, `popup`, `lock`) |
| Repaint | Mantle redraws the changed part of a surface's buffer; an unchanged surface is not redrawn |
| Offscreen pass | The subtree is drawn into a temporary texture, filtered or masked, then composited back. Costs a texture and an extra draw |
| Layer | The offscreen pass that `effect.shader`, `effect.blur`, a colour filter and some shadows use. Unlike other offscreen passes, Mantle keeps it and reuses it while the subtree does not change |
| Glass | A box with `effect.backdrop.blur` or a backdrop colour filter |
| Sigma | A Gaussian blur's standard deviation in logical px. The blur reaches about 3 sigma |

## Output scale

Node sizes, positions, input and compositor regions use logical pixels. Mantle paints each
surface into a buffer sized for that surface's compositor scale, including fractional scales
when the compositor offers fractional-scale and viewporter together. Moving a surface between
outputs repaints its buffer at the new scale. Text and images gain resolution without changing
the layout. The node `scale` property is a separate paint transform; it does not set output scale.

## Who takes what

A property on a kind that does not take it is refused, naming the closest property the kind takes
or, with none close, listing them all.

| Properties | Taken by |
| :--- | :--- |
| `shadows`, `effect`, `opacity` | Every node, including `text`, `icon`, `image`, `list`, `textfield` |
| `background`, `radius`, `corner_shape`, `outline`, `border_color`, `border_width`, `clip`, `mask`, `shadow_mode`, `behind_blur` | Box kinds only |
| `effect.backdrop` | Box kinds only; the key is refused elsewhere, naming it |
| `source_blur` | `image` only |
| `radius` | Box kinds and `image` |
| `foreground` (`text`, `icon`, `textfield`), `z`, `scale`, `rotate`, `translate`, `origin`, `visible` | Also affect paint; documented on [Nodes](../nodes/index.md) |

Every property can be a [signal](signals.md), and so can a value inside a property table, such as a
gradient stop's colour or one border edge. A malformed value draws the property's default and goes
to `mantle log` and `mantle.rescue`; a reload and `mantle check` refuse it
([runtime](runtime.md#evaluation-reload-and-generations)).

## Colours

Colours are strings `"#RRGGBB"` or `"#RRGGBBAA"`, hex digits in either case. There are no named
colours and no short `#RGB` form.

## Box properties

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `background` | `Color\|Gradient\|BackgroundLayer[]\|Bound` | None | A colour, a [gradient](#gradients) or a list of [layers](#background-layers). Absent draws nothing; `"#00000000"` is an explicit transparent fill. Colour layers tween under `animate`; a gradient snaps |
| `mask` | `Mask\|Bound` | None | Multiplies the alpha of this node and its subtree; see [Mask](#mask) |
| `radius` | `number\|Corners\|Bound`, `[0, 8192]` | `0` | Corner radius px; a number sets all four corners, a missing corner is `0`. Corners too big for a side shrink together, so `radius = 999` makes a pill or circle. Shadows round by the mean corner |
| `corner_shape` | `"round"\|"scoop"\|Bound` | `"round"` | `"scoop"` cuts each corner inward as a quarter circle centred on the corner point; fill, clip, glass, shadow and the `behind_blur` region follow |
| `corner_smoothing` | `number\|Bound`, `[0, 1]` | `0` | Continuous corners, as Figma's corner smoothing: `0` is the circular arc, `0.6` is close to iOS. A smoothed corner spreads up to `(1 + corner_smoothing) * radius` along each side, less where the side is short. Refused with `corner_shape = "scoop"`. Fill, border, clip, mask, `effect.backdrop` and the `behind_blur` region follow; a `"box"` shadow stays the circular mean-radius approximation |
| `outline` | `Outline\|Bound` | None | The box's shape as one closed contour, in place of `radius`; see [Outline](#outline) |
| `border_color` | `Color\|BorderColors\|Gradient\|Bound` | None | A string sets all four edges; a missing edge has none. An edge draws only with both a colour and a width. A gradient runs along the whole outline and refuses a per-edge one; it snaps under `animate` |
| `border_width` | `number\|Edges\|Bound`, `[0, 8192]` | `0` | Px per edge; a number sets all four, a missing edge is `0`. Borders draw inside the box and take no layout space |
| `behind_blur` | `boolean\|Bound` | `false` | Ask the compositor to blur the desktop behind this box; see [Blurs](#blurs). Never inferred from a translucent background |
| `shadow_mode` | `"box"\|"content"\|Bound` | `"box"` | `"box"`: CSS `box-shadow` of the box shape. `"content"`: CSS `drop-shadow` of everything painted. See [Shadows](#shadows) |
| `clip` | `"box"\|"rounded"\|"none"\|Bound` | `"box"` on a surface or a `scroll` viewport, else `"none"` | `"box"` cuts children to the rectangle, `"rounded"` also to `radius`, `"none"` leaves them on the parent's clip; a `mask` cuts to the box regardless. See [Clip](#clip) |
<!-- End of the generated table. -->

### Continuous corners

`corner_smoothing` is Figma's corner smoothing, the model behind Apple's continuous corners:
`0` is the circular arc, `0.6` is close to iOS, `1` the softest. A smoothed corner spreads up to
`(1 + corner_smoothing) * radius` along each side as a curve that eases into a shorter circular arc,
so the edge leaves the straight with no visible kink. Where a side is too short, the radius stays
and the smoothing gives way; two corners share a side in proportion to their radii. Each corner of
a `radius` table is smoothed by the one value, and a `"scoop"` refuses it. `image` takes it too.

```lua
return rect {
    width = 160, height = 96, radius = 24, corner_smoothing = 0.6,
    background = "#1E1E2E", border_width = 2, border_color = "#89B4FA",
    animate = { corner_smoothing = 200 },
}
```

Fill, border, `clip = "rounded"`, `mask`, `effect.backdrop` and the `behind_blur` region all follow
the one outline. Two things are approximate: a `"box"` shadow still takes the circular mean radius,
and an `image` `transition` shader rounds a smoothed corner by a superellipse through its end points
and middle, exact only at `0`. A border's inner edge is the outer curve scaled to the inner radii.
`corner_smoothing` only reshapes the outline, so an `animate` tween on it runs without a layout.

A border follows the corners, round or scooped, as CSS draws it. Where two edges meet, the corner
splits between their colours in proportion to their widths, so a lone edge curves round both
corners and tapers away:

<!-- shot-alt: Three tiles: a blue rounded border, a mauve border that follows the scooped corners, a green bottom edge that curves up both rounded corners and tapers away, and a border in four colours that meet on the corners. -->
```lua,shot
local function tile(label, color, props)
    props.width, props.height, props.radius = 104, 72, 16
    props.background = "#1E1E2E"
    props.border_color = props.border_color or color
    props.children = { text { content = label, foreground = "#CDD6F4", align_h = "center", align_v = "center" } }
    return rect(props)
end

return row {
    spacing = 16,
    children = {
        tile("round", "#89B4FA", { border_width = 2 }),
        tile("scoop", "#CBA6F7", { corner_shape = "scoop", border_width = 2 }),
        tile("Per-edge", "#A6E3A1", { border_width = { bottom = 3 } }),
        tile("Four colours", nil, {
            border_width = 3,
            border_color = { top = "#89B4FA", right = "#CBA6F7", bottom = "#F38BA8", left = "#A6E3A1" },
        }),
    },
}
```

### Outline

`outline = { commands = { .. } }` replaces the rounded rectangle with any shape: one closed
contour of [`path`](../nodes/path.md) commands: an `M`, at least two more commands, then `Z`, with
no other `M` or `Z`, no `hole` and at most 256 commands. A popover and its arrow,
a speech bubble's tail, a tab joined to its panel or a notched card is one outline, so the fill,
the border, shadows (inset too), `clip = "rounded"`, `mask`, `effect.backdrop`, `effect.shader`'s
`mantle_sdf`, hit testing and the `behind_blur` region run round it without a seam. It refuses
`radius`, `corner_shape` and `corner_smoothing`, and its border takes one width and one colour or
gradient.

Each coordinate follows the box's size, so a shape fits content and `"fill"` widths:

| Form | Means |
| :--- | :--- |
| `12` | px from the box's left or top edge |
| `"50%"` | that share of the box's width or height, up to `"1600%"` |
| `{ from = "right", px = -12 }` | px from `"left"`, `"right"`, `"top"`, `"bottom"`, `"center"` or a `"NN%"` |

`"left"` and `"right"` are x and `"top"` and `"bottom"` are y, so each is refused in the other's slot.
A placed coordinate is held to [-8192, 8192] px.

`{ op = "corner", points = { x, y }, radius = r, corner_smoothing = s }` (`radius` is required) rounds the turn at a point
between the line in and the line out, with the [continuous corners](#continuous-corners) above.
Two corners sharing a side split it, and a radius too big for its sides shrinks. A corner where
the contour turns the other way is a concave fillet, so a tail can flare into its body. Points may
reach past the box; the tail paints, takes clicks and casts a shadow there, and the box keeps its
layout size. Children stay cut to the box rectangle under `clip = "box"`, so a child hanging into
the tail is neither drawn nor hit; under `clip = "rounded"` they follow the contour. A contour
that crosses itself or winds twice is unsupported: paint fills by the non-zero rule, hit testing
agrees, and `mantle_sdf` measures even-odd.

<!-- shot-alt: A dark popover with rounded corners and a blue border, its arrow pointing up from the top centre, the border running round the arrow with no line across its base. -->
```lua,shot
return column {
    padding = { top = 10 },
    children = {
        column {
            padding = 14,
            spacing = 4,
            background = "#1E1E2E",
            border_width = 2,
            border_color = "#89B4FA",
            shadows = { { blur = 12, offset = { y = 4 }, color = "#00000080" } },
            outline = { commands = {
                { op = "M", points = { 0, 20 } },
                { op = "corner", points = { 0, 0 }, radius = 12, corner_smoothing = 0.6 },
                { op = "corner", points = { { from = "center", px = -12 }, 0 }, radius = 4 },
                { op = "corner", points = { "50%", -10 }, radius = 3 },
                { op = "corner", points = { { from = "center", px = 12 }, 0 }, radius = 4 },
                { op = "corner", points = { "100%", 0 }, radius = 12, corner_smoothing = 0.6 },
                { op = "corner", points = { "100%", "100%" }, radius = 12, corner_smoothing = 0.6 },
                { op = "corner", points = { 0, "100%" }, radius = 12, corner_smoothing = 0.6 },
                { op = "Z", points = {} },
            } },
            children = {
                text { content = "Wi-Fi", font_weight = 700, foreground = "#CDD6F4" },
                text { content = "Connected to home", font_size = 12, foreground = "#A6ADC8" },
            },
        },
    },
}
```

`animate = { outline = .. }` tweens point by point, springs included, between two lists of the
same commands; any other change snaps. A point written in px on one side and `"NN%"` on the other
crosses as their resolved positions would. A corner's radius holds at `0` and its smoothing in
`[0, 1]` through an overshoot. The tween runs without a layout or Lua.

`mantle_sdf` measures an outline as a polygon of at most 256 points, each curve cut every 3° of
turn; past 256 points the cut widens to 6°, 12° and so on, so the gradient bends by at most 3° at a
point up to about 8 corners, more beyond (and straight segments past 256 are thinned). Hit testing
reads the exact curves, so it and `mantle_sdf` can differ by a fraction of a pixel. A shadow
on an outline is the silhouette's, blurred offscreen like `shadow_mode = "content"`, and an inset
shadow takes one offscreen blur per paint where a rounded box takes one gradient quad.

## Gradients

`background`, `mask` and `border_color` take a gradient table.

```lua
background = {
    gradient = "linear",
    angle = 90,
    stops = { { 0, "#CBA6F7" }, { 0.5, "#F38BA8" }, { 1, "#89B4FA" } },
}
```

| Key | Rule |
| :--- | :--- |
| `gradient` | `"linear"`, `"radial"` or `"conic"` |
| `angle` | Degrees clockwise from the top, as in CSS. `"linear"` default 180 (top to bottom), `"conic"` default 0 (starts at twelve o'clock). `"radial"` refuses it |
| `stops` | At least 2 `{ position, colour }` pairs. Positions in `[0, 1]`, never descending; two equal positions make a hard edge |

On `border_color` the gradient spans the node's box and shows only where `border_width` draws, so
width, `radius`, `corner_shape` and `corner_smoothing` shape the ring as for a flat colour.

```lua
rect {
    width = 120, height = 40, radius = 12,
    border_width = 2,
    border_color = { gradient = "conic", stops = { { 0, "#CBA6F7" }, { 0.5, "#89B4FA" }, { 1, "#CBA6F7" } } },
}
```

| Shape | Geometry |
| :--- | :--- |
| `"linear"` | Along `angle` through the centre, long enough that the corners take the end stops (CSS) |
| `"radial"` | An ellipse from the centre out to the box's edges, not its corners |
| `"conic"` | A turn around the centre, starting at `angle` |

<!-- shot-alt: The same three stops, mauve to pink to blue, as a linear, a radial and a conic gradient tile. -->
```lua,shot
local stops = { { 0, "#CBA6F7" }, { 0.5, "#F38BA8" }, { 1, "#89B4FA" } }

local function swatch(label, fill)
    return column {
        spacing = 8,
        children = {
            rect { width = 112, height = 80, radius = 12, background = fill },
            text { content = label, font_size = 13, foreground = "#BAC2DE", align_h = "center" },
        },
    }
end

return row {
    spacing = 16,
    children = {
        swatch("linear, 90", { gradient = "linear", angle = 90, stops = stops }),
        swatch("radial", { gradient = "radial", stops = stops }),
        swatch("conic", { gradient = "conic", stops = stops }),
    },
}
```

## Background layers

`background` also takes a list of layers, as CSS multiple backgrounds: the first is on top, like
[`shadows`](#shadows). A layer is a colour, a gradient, or `{ fill = <colour or gradient>, blend = <mode> }`,
where `blend` is one of the [blend modes](#blend-modes). Every layer shares the box's `radius`,
`corner_shape` and `corner_smoothing`. At most 16; `{}` draws nothing. A single colour or gradient
stays valid.

```lua
rect {
    width = 120, height = 40, radius = 12,
    background = {
        { gradient = "linear", stops = { { 0, "#FFFFFF33" }, { 1, "#FFFFFF00" } } },
        "#313244",
    },
}
```

Under `animate`, colour layers tween pairwise and a layer only one side has fades in or out. A
gradient layer and a layer's `blend` snap.

## Clip

`clip` decides what a box cuts its children to. As CSS `overflow: visible`, a box without one cuts
nothing: a child laid out, shadowed or transformed past it paints there and is hit there. A
`row`, `column` or `list` with a [`scroll`](input.md#scroll) signal and every surface default to
`"box"`, since a viewport has to hide what it scrolled out. Set `clip = "box"` on a button whose
ripple or hover scale-up must stay inside it. A `list` takes no `clip`: it cuts only when it
scrolls.

A child's `translate`, `scale` or `rotate` moves its own paint, never the parent's clip: under a
clipping parent, a child that transforms past the parent's box is cut there (a rotated one by the
box's bounds in its own space).

| Value | Children are cut to | Cost |
| :--- | :--- | :--- |
| `"box"` | The box's rectangle | Free (a scissor) |
| `"rounded"` | The box's `radius`, `corner_shape` and `corner_smoothing`, or its `outline`. With `radius = 0` it is `"box"` | An offscreen pass every repaint of the box |
| `"none"` | Whatever the parent cuts to, so children and their shadows can overflow this box. The default but on a scroll viewport or a surface | Free |

A rounded clip draws in the order fill, children, border, so the border stays on top of children
that reach the arc.

## Mask

`mask` multiplies the alpha of the node's own fill and border and of its whole subtree.

| Form | Alpha taken from |
| :--- | :--- |
| A [gradient](#gradients) table | The gradient's colours' alpha, laid over the box. RGB is ignored |
| `{ source = "/path.png" }` | The image's alpha, stretched over the box. A file that fails to load leaves the node unmasked |
| `{ node = "shape" }` | The named direct child's painted subtree; RGB is ignored |
| Any form, plus `invert = true` | The complement: kept and cut swap |

Name exactly one of `source`, `node`, or a gradient. A masked box draws its subtree offscreen every repaint
and always cuts children to its box (to `radius` or the `outline` too under `clip = "rounded"`), even with
`clip = "none"`.

<!-- shot-alt: A scrolling list of Wi-Fi networks in a card; the rows at its top and bottom edges fade out under a gradient mask. -->
```lua,shot
local networks = { "Home", "Office 5G", "Cafe Guest", "Library", "Studio", "Garden", "Lab", "Backup", "Hotspot", "Lobby",
    "Attic", "Garage" }
local rows = {}
for i, name in ipairs(networks) do
    rows[i] = row {
        width = "fill",
        padding = 10,
        spacing = 10,
        radius = 8,
        background = i == 1 and "#89B4FA26" or "#313244",
        children = {
            icon { name = "network-wireless-symbolic", size = 16, foreground = i == 1 and "#89B4FA" or "#A6ADC8", align_v = "center" },
            text { content = name, foreground = "#CDD6F4", align_v = "center" },
        },
    }
end

return column {
    width = 220,
    height = 240,
    padding = 8,
    spacing = 6,
    radius = 12,
    background = "#1E1E2E",
    scroll = scroll("feed"),
    mask = {
        gradient = "linear",
        stops = { { 0, "#00000000" }, { 0.1, "#000000" }, { 0.9, "#000000" }, { 1, "#00000000" } },
    },
    children = rows,
}
```

A scrolling list whose rows fade out at the top and bottom edges.

A mask can name an owned direct child by id with `mask = { node = "shape" }`.
The child keeps its ordinary layout and reactive properties, but paints only into the
mask and receives no input. Its RGB is ignored; transparent or hidden mask content
cuts everything. `invert = true` reverses that alpha. Missing child ids fail the pass.
The child's opacity multiplies the mask; the parent's opacity applies once to content.
Mask subtrees can contain transforms, effects, and further masks.

```lua
rect {
    width = 160, height = 80, background = "#3366ffff",
    mask = { node = "shape" },
    children = {
        rect { id = "shape", width = 80, height = "fill", radius = 24,
            background = "#ffffffff" },
    },
}
```

The mask child contributes to `Content` sizing and flow spacing like other children.
Use a stacking `rect` with explicit dimensions when the mask must not size the content.
Mask alpha does not change the content's input or desktop blur region.

## Shadows

`shadows` is a list of layers, CSS's `box-shadow: a, b`: the first layer is on top and a node takes
at most 16, inset and outer together. A layer is a table, and draws when its `color` has alpha above 0 and at least one of
`blur`, `offset` or `spread` is set. A single shadow is `shadows = { { blur = 8 } }`.

| Layer key | Values | Default |
| :--- | :--- | :--- |
| `color` | Colour | `"#000000"` |
| `blur` | CSS blur radius in px `[0, 8192]`; the Gaussian's sigma is half of it | 0 |
| `offset` | `{ x, y }` px, each `[-8192, 8192]`, missing axis 0 | `{ x = 0, y = 0 }` |
| `spread` | px `[-8192, 8192]` the shape grows (negative shrinks) per side. On a non-box shadow it scales the shadow about the box centre instead. On an `inset` layer it shrinks the unshaded area instead | 0 |
| `inset` | `true`: CSS `box-shadow: inset`, see [Inset shadows](#inset-shadows) | `false` |

| Property | Values | Default |
| :--- | :--- | :--- |
| `shadow_mode` | Box kinds only. `"box"`: CSS `box-shadow`, cast by the box's shape and cut out under the box. `"content"`: CSS `drop-shadow`, cast by everything the node and its subtree paint | `"box"` |

Non-box nodes (`text`, `icon`, `image`, ...) have no box to cast, so their shadow is always the
content's: text gets a glyph-shaped shadow. The same unfilled, bordered box in each mode:

<!-- shot-alt: Two unfilled cards with a blue border, a star and a label on a grey panel: the Box card casts one rounded shadow, the Content card casts shadows of its border ring, star and text. -->
```lua,shot
local function card(label, mode)
    return row {
        padding = 14,
        spacing = 10,
        radius = 12,
        border_width = 2,
        border_color = "#89B4FA",
        shadow_mode = mode,
        shadows = { { color = "#000000", blur = 4, offset = { x = 5, y = 6 } } },
        children = {
            icon { name = "starred-symbolic", size = 22, foreground = "#F9E2AF", align_v = "center" },
            text { content = label, font_size = 20, foreground = "#CDD6F4", align_v = "center" },
        },
    }
end

return row {
    padding = 28,
    spacing = 28,
    radius = 16,
    background = "#6C7086",
    children = { card("box", "box"), card("content", "content") },
}
```

`"box"` casts the rounded box and cuts the shadow out under it; `"content"` casts the border ring
and the glyphs.

| Case | How it draws |
| :--- | :--- |
| Box mode on a round box, any fill | One gradient quad around the box. On a translucent box it is cut out under the box, so it never shows through the fill |
| An opaque box (solid colour fill with alpha 1, no mask, no `effect.blur`, `opacity` 1), either mode | The same gradient quad; the box covers what is under it |
| Content mode on anything else, any non-box node, an opaque scoop | An offscreen layer: the subtree is drawn, blurred and tinted each layer's `color` |
| Box mode on a translucent scoop | A layer of the scoop's silhouette, cut out under the box |

### Layered shadows

A tight key shadow under a wide ambient one:

```lua
rect {
    width = 160,
    height = 64,
    radius = 12,
    background = "#1E1E2E",
    shadows = {
        { color = "#0000004D", blur = 2, offset = { y = 1 } },
        { color = "#00000026", blur = 6, offset = { y = 2 }, spread = 2 },
    },
}
```

### Inset shadows

`inset = true` shades the inside of the box: the padding box is filled with `color` everywhere
outside a hole, the box moved by `offset` and shrunk by `spread`, feathered by `blur`. The box's
shape clips it, and it draws above every `background` layer and under the children and the
border. It needs `shadow_mode = "box"` on a box kind; `text`, `icon`, `image` and the other
non-box kinds refuse it. The hole takes the mean of the box's corner radii.

```lua
rect {
    width = 160,
    height = 64,
    radius = 12,
    background = "#1E1E2E",
    shadows = { { color = "#00000080", blur = 8, offset = { y = 2 }, inset = true } },
}
```

In content mode each layer is a blur pass over the same offscreen, so it costs one blur per layer.
`animate.shadows` tweens layer by layer, and a layer only one side has fades in or out at its own
geometry. A `spring` on `shadows` restarts from rest when its target changes mid-flight.

## Blurs

Four blurs read four different things. Sigmas are in logical px, `[0, 8192]`, 0 is off.
`source_blur` is a fast box approximation; the others are Gaussian.

| Property | Reads | When it runs | Cost | Pick it for |
| :--- | :--- | :--- | :--- | :--- |
| `behind_blur = true` (box kinds) | The desktop behind the surface: other windows and the wallpaper, not this surface's own pixels | Continuously, in the compositor | The compositor's | A translucent bar or panel over windows |
| `effect = { backdrop = { blur = sigma } }` (box kinds) | What this surface has already painted under the box: ancestors, earlier siblings, lower `z`. Never the desktop | Every repaint that touches the box or what it reads, on the GPU | A copy and a blur per repaint; not cached | Glass over the surface's own wallpaper, image or animated content |
| `effect = { blur = sigma }` (every node) | The node's own subtree | On repaint, on the GPU, into an offscreen layer | A blur when the subtree changes; an unchanged layer is reused. Large sigmas downsample first | A blurred or blur-in element, tweened with `animate` |
| `source_blur = sigma` (`image`) | The image file's pixels | Once, on the CPU, when the source decodes | Nothing per frame | A static blurred picture on a surface that repaints often |

**`behind_blur = true`.** Mantle sends the compositor a region, through `ext-background-effect-v1`, made
of every `behind_blur = true` box on the surface: rounded to `radius`, scooped or shaped as its `outline`, cut by ancestor
clips, moved by transforms, and dropped while the node is invisible or at `opacity` 0. It ignores
`mask`. The compositor decides strength, noise, xray and whether to blur at all; a compositor
without the protocol or its blur capability gives nothing, and no error. It is never inferred from
a translucent `background`; the `background` alpha only decides how much of the blurred desktop
shows through.

**`source_blur`.** The blur is baked into the decoded pixels, which are stored cropped to the box
under `fit = "cover"`, so it is exact there. Under `"contain"` or `"stretch"` the stored pixels
are rescaled and the blur with them. Animated GIFs ignore it. Changing it re-decodes; under `async = true` the image draws nothing until that lands, and `retain` does not cover it (the `source` did not change). See
[image](../nodes/image.md).

```lua
panel {
    id = "bar",
    layer = "top",
    anchor = { top = true, left = true, right = true },
    height = 36,
    exclusive_zone = true,
    background = "#1E1E2E99",
    behind_blur = true,
    child = row { width = "fill", padding = { left = 12, right = 12 }, children = { clock } },
}
```

A bar whose 60% fill tints the compositor-blurred desktop behind it.

<!-- shot-alt: A frosted pill with the time and date over a mountain illustration, the scenery blurred behind it. -->
```lua,shot
rect {
    width = 360,
    height = 200,
    radius = 16,
    clip = "rounded",
    children = {
        image { source = "/usr/share/backgrounds/default.png", width = "fill", height = "fill", async = true },
        row {
            align_h = "center",
            align_v = "center",
            padding = { left = 16, right = 16, top = 8, bottom = 8 },
            spacing = 10,
            radius = 999,
            background = "#FFFFFF1F",
            border_width = 1,
            border_color = "#FFFFFF33",
            effect = { backdrop = { blur = 12 } },
            children = {
                text { content = "12:45", font_size = 20, font_weight = 700, foreground = "#FFFFFF", align_v = "center" },
                text { content = "Thu 24 Sep", font_size = 13, foreground = "#FFFFFFCC", align_v = "center" },
            },
        },
    },
}
```

A frosted pill: the image is painted first, so the pill's `effect.backdrop.blur` blurs the image under
its rounded shape, and the fill tints it. The same pattern over a full-screen image frosts a lock
screen's wallpaper.

### Colour filters

`effect` also takes CSS `saturate()`, `brightness()` and `contrast()`, on the node's subtree and, in
`backdrop`, on the pixels under a box. Each is a factor in `[0, 8]`, default `1`, which is off and
costs nothing. At one level they apply in a fixed order: blur, then `saturate`, `brightness`,
`contrast`, whatever order the table lists them in. They work on the straight sRGB colour, as CSS's
do, so a translucent fill keeps its alpha. A colour filter folds into the blur's last pass; a
`backdrop` with only a colour filter still copies what is under the box once, and a node with a
colour filter is drawn into a layer like one with `effect.blur`.

```lua
rect {
    width = 360,
    height = 120,
    radius = 16,
    background = "#FFFFFF1F",
    effect = { backdrop = { blur = 45, saturate = 2 } },
}
```

The usual glass: CSS `backdrop-filter: blur(45px) saturate(2)`.

### Progressive blur

`effect.backdrop.mask` takes the values of a node's [`mask`](#mask) (a `gradient` or a `source`
image; not a `node`) and scales the glass's coverage. Where the mask is clear the ground shows
through untouched, with no blur, colour filter or backdrop shader output; where it is opaque the
glass is whole; between, it is a crossfade of the two. It is the soft edge under a toolbar, or a
panel whose frost feathers out. The mask never touches the node's fill, border or children.

<!-- shot-alt: A mountain illustration whose ridges blur toward the bottom edge, the blur fading to a sharp picture over 130 px. -->
```lua,shot
rect {
    width = 360,
    height = 200,
    radius = 16,
    clip = "rounded",
    children = {
        image { source = "/usr/share/backgrounds/default.png", width = "fill", height = "fill", async = true },
        rect {
            width = "fill",
            height = 130,
            align_v = "end",
            effect = {
                backdrop = {
                    blur = 10,
                    mask = { gradient = "linear", angle = 0, stops = { { 0, "#FFFFFFFF" }, { 1, "#FFFFFF00" } } },
                },
            },
        },
    },
}
```

A crossfade, not a blur radius that falls off: at a large sigma the faded part reads as a blend of
the sharp and the blurred picture. `invert = true` flips the mask as on a node.

### Shader effects

`effect.shader` runs a fragment shader of yours over the node's painted subtree: the fill, children and
border drawn offscreen, which the program reads as a texture and replaces with its output. It is how
a duotone, a chromatic aberration, a ripple or a glow that follows the node's own shape is a config's
to write. A [`shader` node](../nodes/shader.md) draws one quad and reads nothing; this reads the pixels.

```lua
rect {
    width = 240,
    height = 80,
    radius = 20,
    corner_smoothing = 0.6,
    background = "#335577",
    effect = {
        shader = {
            source = mantle.config_dir .. "/shaders/outline.frag",
            params = { width = 3, tint = { 1, 0.8, 0.2, 1 } },
            padding = 4,
        },
    },
}
```

| Key | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | `string` | required | Absolute `.frag` path; relative is refused |
| `input` | `"content"` or `"backdrop"` | `"content"` | What `u_input` holds: the node's painted subtree, or what the surface painted under the box (see [Backdrop input](#backdrop-input)) |
| `params` | `table<string, number\|number[]>` | `{}` | Uniforms by name, as on a [`shader` node](../nodes/shader.md#the-frag-file); missing ones are `0` |
| `images` | `table<string, string>` | `{}` | Raster files to sample, by sampler name, with the rules, `<name>_size` and sampling of a [`shader` node's `images`](../nodes/shader.md#images). Clamped to the edge (`fract(uv)` tiles), premultiplied, sRGB as stored; `.png`, `.jpg`, `.jpeg` or `.webp` only, a side over 2048 px downscaled. They take the units after `u_input` and `u_input_blurred`, so a refraction can read a displacement map |
| `progress` | `number`, `[-8192, 8192]` | `0` | Becomes `u_progress`, as on a [`shader` node](../nodes/shader.md#the-frag-file). [animate](animation.md) `effect` to move it |
| `padding` | `number`, `[0, 512]` | `0` | Logical px around the box the program can read and draw. The layer, its damage and its clip grow by it |

The `.frag` contract is the [`shader` node's](../nodes/shader.md#the-frag-file) with these changes.
Write `void main()` and set `fragColor` to premultiplied RGBA.

| Name | Type | What |
| :--- | :--- | :--- |
| `v_uv` | `in vec2` | Box coordinate, top-left origin, y down. `0..1` over the box, and outside it over the padding |
| `u_size` | `vec2` | The node's box in logical px, without the padding |
| `mantle_input(uv)` | `vec4` | The input at a box coordinate, premultiplied; transparent outside the padded area |
| `mantle_input_blurred(uv)` | `vec4` | The same, through `effect.backdrop`'s blur and colour filters; the plain input when it has none or `input = "content"` |
| `u_input`, `u_input_blurred`, `u_input_rect` | `sampler2D`, `sampler2D`, `vec4` | The textures and where they sit as `(x, y, w, h)` in box fractions. Use the functions above, which handle the textures' orientation |
| `mantle_sdf(p)` | `float` | Signed distance in logical px from `p` (a box position in logical px, `v_uv * u_size`) to the node's outline, negative inside. It follows `radius`, per-corner radii, `corner_smoothing` and `outline`, so a shader can draw a rim, a glow or a clip that matches the shape. Smoothed corners are approximate, as in [Continuous corners](#continuous-corners) |
| `u_progress` | `float` | `progress` |
| `mantle_opacity` | | Not set |

Unlike a `shader` node, no step follows your `main`: the output is not rounded to `radius` and not
multiplied by `opacity`, since the subtree already carries its opacity. Cut to the outline yourself
with `mantle_sdf` when you want to.

<!-- file: shaders/outline.frag -->
```glsl
uniform float width;
uniform vec4 tint;

void main() {
    vec4 body = mantle_input(v_uv);
    float d = mantle_sdf(v_uv * u_size);
    // A rim `width` px deep just inside the outline, over the subtree.
    float rim = smoothstep(-width - 0.5, -width + 0.5, d) * (1.0 - smoothstep(-0.5, 0.5, d));
    fragColor = body * (1.0 - rim * tint.a) + vec4(tint.rgb * tint.a, tint.a) * rim;
}
```

| Case | Result |
| :--- | :--- |
| The shader fails to compile or link | Logged once per revision of the file; the node draws as if it had no `shader` |
| The `.frag` is saved | The config reloads, which recompiles it and repaints the node |
| `params`, `images` or a file change | The layer is redrawn; an unchanged layer is reused |
| `animate` on `effect` | Tweens `blur`, the colour filters and `shader.progress`; the rest of the `shader` table (`source`, `params`, `images`, `padding`) takes the target's value at once. A progress-only step repaints the layer without resolving Lua |
| Loop an animation | `animate = { effect = { keyframes = { at(0), at(1) }, duration = 2000, loops = "infinite" } }`, with `at(p)` returning `{ shader = { source = ..., progress = p } }` ([keyframes](animation.md#keyframes)) |
| Hit testing and input regions | Ignore it: a shader draws pixels, not shape |

Order: the shader reads the node after its fill, children and border, and its output goes through
`shadows` (content mode), then `effect.blur` and the colour filters. `padding` is cut at the same
clips as a shadow, so a node at a surface's edge has no room to pad into.

#### Backdrop input

With `input = "backdrop"` the program reads what this surface already painted under the box and
`padding` around it, never the desktop behind the surface. It runs before the node paints, beside
`effect.backdrop`: one copy of those pixels feeds both, and `mantle_input_blurred` reads the frost
the backdrop filters made from it. The output replaces the whole read area, `padding` included, mixed
with the original pixels by the node's `opacity`; the node's fill, children and border paint over it.
Return `mantle_input(uv)` to leave a pixel as it was, and do so outside the outline, or the pixel
comes out as whatever the program returns. Offsetting the coordinate by the outline's distance is a
refraction. Box kinds only.

```lua
rect {
    width = 200,
    height = 64,
    radius = 32,
    background = "#FFFFFF14",
    effect = {
        backdrop = { blur = 6 },
        shader = { source = mantle.config_dir .. "/shaders/lens.frag", input = "backdrop", padding = 8 },
    },
}
```

<!-- file: shaders/lens.frag -->
```glsl
void main() {
    vec2 p = v_uv * u_size;
    float d = mantle_sdf(p);
    // Up to 8 px of pull toward the centre in the outer 12 px of the shape.
    vec2 toward = normalize(u_size * 0.5 - p + 1e-4);
    vec2 bent = v_uv + toward * smoothstep(-12.0, 0.0, d) * 8.0 / u_size;
    float inside = 1.0 - smoothstep(-0.5, 0.5, d);
    fragColor = mix(mantle_input(v_uv), mantle_input_blurred(bent), inside);
}
```

## Blend modes

`blend` composites a node, a `background` layer (`{ fill = .., blend = .. }`) or a `shadows` layer
onto what this surface painted under it, by CSS `mix-blend-mode`'s formulas: `"normal"` (the
default), `"multiply"`, `"screen"`, `"overlay"`, `"darken"`, `"lighten"`, `"color_dodge"`,
`"color_burn"`, `"hard_light"`, `"soft_light"`, `"difference"`, `"exclusion"`, `"hue"`,
`"saturation"`, `"color"`, `"luminosity"`, and Apple's `"plus_lighter"` and `"plus_darker"`.

```lua
rect {
    width = 120, height = 40, radius = 12,
    blend = "plus_lighter",
    background = { { fill = "#FFFFFF33", blend = "overlay" }, "#1E1E2E" },
    shadows = { { color = "#00000066", blur = 12, blend = "multiply" } },
}
```

| Where | Blends with |
| :--- | :--- |
| `blend` on a node | Its whole painted output, its `shadows` included and after every `effect` filter, against what is under it. The backdrop filters are not part of it |
| A `background` layer | The layers below it and what is under the box, as a Figma fill does; CSS `background-blend-mode` would isolate the box |
| A `shadows` layer | What is under the shadow |

Inside a `mask`, `effect` layer or content-mode shadow the blend sees only what that ancestor drew so
far, as a glass does; `clip = "rounded"` is no barrier. `"normal"` costs nothing. Any other mode
draws the blended part offscreen, copies the pixels under it and runs one pass: about 0.1 ms to
0.2 ms per blended part for a 400x300 box on integrated graphics. A box with one opaque `"normal"`
colour layer is opaque whatever blends above it; a blended node is never opaque. `blend` snaps
under `animate`.

## Combining effects

One node paints in this order, each step over the last. The order is fixed: the keys of `effect` apply in it, whatever order the table lists them in.

1. **Backdrop** (`effect.backdrop`): replaces the pixels under the box with their blur, then
   `saturate`, `brightness` and `contrast`; a backdrop shader instead replaces them with its output,
   from the same copy. It reads what precedes the node, so it comes first.
2. **Shadow**, when it is a gradient quad or a silhouette.
3. **Body**: fill, children in `z` order, border. With a `mask` or a `clip = "rounded"` the body
   goes through an offscreen pass. A blended `background` or `shadows` layer blends as it draws.
4. **Layer**: for a content `effect.shader`, `effect.blur`, a colour filter, a layered shadow or a
   node `blend`, the shadow and body are drawn offscreen, run through the shader, the content
   shadow cast from that, then blurred and recoloured.
5. **Blend**: the finished layer composites onto what is under it by the node's `blend`. It is last
   because it is how the result meets the backdrop, as CSS applies `mix-blend-mode` after `filter`.
6. **Transform** (`scale`, `rotate`, `translate`) wraps all of the above.

| Combination | What happens | Do this |
| :--- | :--- | :--- |
| `mask` and `effect.backdrop.blur` on one node | The mask fades the fill, border and subtree, not the node's own glass or box shadow | `effect.backdrop.mask` for the glass; a child of the masked node for the rest |
| `effect.blur` and `effect.backdrop.blur` on one node | The glass stays sharp; only the fill, border and subtree blur | Expected |
| `effect.backdrop.blur` inside a parent with `mask`, `effect.blur` or a Content-mode shadow | The glass sees only what that parent has drawn so far, not what is under the parent | Move the glass out of the effect parent, or accept it |
| `effect.backdrop.blur` inside `clip = "rounded"` without a mask | The glass sees what is under the parent, as without the clip | Nothing to do |
| `effect.backdrop.blur` on a surface root | It blurs transparency: it never reads the desktop | `behind_blur = true` |
| Shadow and `effect.blur` on one node | The shadow is cast from the sharp content, then the content is blurred | Expected |
| Box-mode shadow on a translucent box | One gradient quad, cut out under the box; children do not cast | `shadow_mode = "content"` to cast from what is painted |
| Content-mode shadow on a masked node | Cast from the masked result | Expected |
| Content-mode shadow or `effect.blur` over an `image`, `icon`, `capture`, image `mask` or glass | The layer is redrawn every repaint instead of reused | Keep those out of animated layers, or accept the cost |
| Anything under a glass changes | The glass repaints, and so does everything in the area it reads (3 sigma past its box) | Keep glass away from constantly animating content, or keep sigma small |
| Shadow or `effect.blur` near a clipping parent's edge (`clip`, a scroll viewport, the surface) | Cut at that clip, like any child paint | Give that parent padding |
| `opacity` on a node with effects | Multiplied into every draw once; layers and clips composite at full alpha, so nothing fades twice | Expected |
| `opacity < 1` on a group whose children overlap | Each child fades on its own, so overlaps show through each other (not CSS group opacity) | For a group fade, give the parent a uniform `mask` (e.g. both stops `"#00000080"`); it costs an offscreen pass |
| A transform on a node with a glass or shadow | The backdrop, shadow and body move together; the glass reads under its transformed position | Expected |

## How do I…

| Task | Answer |
| :--- | :--- |
| Frosted glass panel over windows | [Glass sheet](#frosted-glass-panel) below, or the [blur bar](#blurs) |
| Frost a picture inside my own surface | The [frosted pill](#blurs): an `image`, then a sibling with `effect.backdrop.blur` |
| Card with a shadow | The [card](#paint) at the top; [lift on hover](#card-that-lifts-on-hover) below |
| Pill button | [Pill button](#pill-button) |
| Gradient border | [Gradient ring](#gradient-border) |
| Fade a list's edges | The [edge-fade mask](#mask) |
| Circular avatar | [Avatar](#circular-avatar) |
| Dim the background behind a modal | [Scrim](#dim-the-background-behind-a-modal) |
| Tint a gradient from a signal | Map the whole table; see [who takes what](#who-takes-what) |

### Frosted glass panel

```lua
panel {
    id = "sheet",
    layer = "top",
    anchor = { top = true, right = true },
    margin = 8,
    child = column {
        width = 280,
        padding = 16,
        spacing = 8,
        radius = 16,
        background = "#1E1E2EB3",
        border_width = 1,
        border_color = "#FFFFFF2E",
        behind_blur = true,
        children = { text { content = "Wi-Fi", font_size = 14, foreground = "#CDD6F4" } },
    },
}
```

The compositor blurs the desktop under the rounded sheet only; the rest of the surface stays
clear. A faint light border separates glass from glass.

### Card that lifts on hover

```lua
local lifted = hover("card_hover")
column {
    hover = lifted,
    padding = 16,
    radius = 12,
    background = "#313244",
    shadows = { {
        color = "#00000099",
        blur = lifted:map(function(on) return on and 36 or 12 end),
        offset = lifted:map(function(on) return { x = 0, y = on and 20 or 6 } end),
    } },
    animate = { shadows = 200 },
    children = { text { content = "Hover me" } },
}
```

[`hover`](input.md#hover) drives the shadow and [`animate`](animation.md) eases it. Leave room
around the card when its parent clips: a scroll viewport or the surface cuts the shadow.

### Pill button

```lua
local hovered = hover("save_hover")
rect {
    hover = hovered,
    padding = { left = 16, right = 16, top = 6, bottom = 6 },
    radius = 999,
    background = hovered:map(function(on) return on and "#89B4FA59" or "#89B4FA33" end),
    border_width = 1,
    border_color = "#89B4FA66",
    animate = { background = 150 },
    on_click = function() print("saved") end,
    children = { text { content = "Save", foreground = "#CDD6F4" } },
}
```

A radius past half the height makes the ends round whatever the label's width.

### Gradient border

<!-- shot-alt: An "Upgrade to Pro" button with a star, ringed by a mauve-to-blue gradient border. -->
```lua,shot
rect {
    padding = 2,
    radius = 14,
    background = { gradient = "linear", angle = 135, stops = { { 0, "#CBA6F7" }, { 1, "#89B4FA" } } },
    children = {
        row {
            padding = { left = 18, right = 18, top = 12, bottom = 12 },
            spacing = 8,
            radius = 12,
            background = "#1E1E2E",
            children = {
                icon { name = "starred-symbolic", size = 16, foreground = "#CBA6F7", align_v = "center" },
                text { content = "Upgrade to Pro", font_weight = 700, foreground = "#CDD6F4", align_v = "center" },
            },
        },
    },
}
```

This paints the gradient as an outer fill and covers all but a 2px ring with an opaque inner box;
keep the inner radius the outer radius minus the ring width. For a ring on one box, give
`border_color` the gradient instead ([Gradients](#gradients)).

### Circular avatar

<!-- shot-alt: An account card: a circular avatar cropped from a square image with a blue ring, beside the user's name and status. -->
```lua,shot
row {
    padding = 12,
    spacing = 12,
    radius = 14,
    background = "#1E1E2E",
    children = {
        rect {
            width = 56,
            height = 56,
            radius = 28,
            clip = "rounded",
            border_width = 2,
            border_color = "#89B4FA",
            children = { image { source = "/var/lib/AccountsService/icons/user", width = "fill", height = "fill" } },
        },
        column {
            spacing = 2,
            align_v = "center",
            children = {
                text { content = "user", font_size = 15, font_weight = 700, foreground = "#CDD6F4" },
                text { content = "Signed in", font_size = 12, foreground = "#A6ADC8" },
            },
        },
    },
}
```

`clip = "rounded"` cuts the image to the circle, and the border paints over the image's edge.

### Dim the background behind a modal

```lua
panel {
    id = "modal",
    layer = "overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    exclusive_zone = "ignore",
    keyboard_interactivity = "on_demand",
    child = rect {
        width = "fill",
        height = "fill",
        children = {
            rect { width = "fill", height = "fill", background = "#11111B99" },
            column {
                align_h = "center",
                align_v = "center",
                width = 360,
                padding = 24,
                spacing = 8,
                radius = 16,
                background = "#1E1E2EE0",
                behind_blur = true,
                shadows = { { color = "#00000080", blur = 32 } },
                children = {
                    text { content = "Log out?", font_size = 18, foreground = "#CDD6F4" },
                    text { content = "Unsaved work in open apps will be lost.", foreground = "#A6ADC8" },
                },
            },
        },
    },
}
```

A full-screen [panel](../surfaces/panel.md) whose first child is a translucent scrim and whose
second is the dialog. Dim with a colour rather than `behind_blur = true` on the scrim: the compositor's
blur does not fade with `opacity`, so a fading scrim would blur at full strength until it hits 0.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `clip = "rounded"` changes nothing | It needs a non-zero `radius` or an `outline`, and only clips children |
| A gradient or a per-edge `border_color` jumps instead of easing under `animate` | Only single colours ease; see [Animation](animation.md) |
| Rounded corners, scoops and masks still take clicks in the cut-away area | Hit-testing uses the rectangle, except for an `outline`, which is hit by its contour. Shrink the clickable node or accept it |

See also: [nodes](../nodes/index.md), [surfaces](../surfaces/index.md), [animation](animation.md), [input](input.md#hit-testing), [glossary](../glossary.md).

Source: [allowlist](../../renderer/src/lua/nodes/properties.rs),
[parsers](../../renderer/src/layout/node/style/mod.rs),
[paint style](../../renderer/src/layout/node/paint_style.rs),
[paint order](../../renderer/src/layout/paint/build.rs),
[canvas](../../renderer/src/layout/paint/canvas/mod.rs),
[effects](../../renderer/src/layout/paint/canvas/effects.rs),
[shapes](../../renderer/src/layout/paint/canvas/shape.rs),
[blur region](../../renderer/src/layout/region.rs),
[compositor push](../../renderer/src/wayland/surface/resolved.rs).
