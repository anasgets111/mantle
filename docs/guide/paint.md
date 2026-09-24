# Paint

How a node looks: fills, gradients, corners, borders, clipping, masks, shadows and the four
blurs. Reach for this page once a layout is in place and you want it to look like something.
Layout and per-kind properties are on [Nodes](../nodes/index.md); easing any of these values is on
[Animation](animation.md).

```lua
column {
    padding = 16,
    spacing = 8,
    background = "#1E1E2EF2",
    radius = 12,
    border_width = 1,
    border_color = "#FFFFFF1A",
    shadow_color = "#00000099",
    shadow_blur = 18,
    shadow_offset = { x = 0, y = 8 },
    children = {
        text { content = "Battery", font_size = 14, foreground = "#CDD6F4" },
        text { content = "82% · 3 h 10 min left", foreground = "#A6ADC8" },
    },
}
```

A card: a translucent rounded fill, a hairline border, and a soft shadow that falls below it.

## Terms

| Term | Meaning |
| :--- | :--- |
| Box kind | A node that paints a box: `rect`, `row`, `column`, `button` and the four [surface](../surfaces/index.md) roles (`panel`, `window`, `popup`, `lock`) |
| Repaint | Mantle redraws the changed part of a surface's buffer after a change; unchanged surfaces are not redrawn |
| Offscreen pass | The subtree is drawn into a temporary texture, filtered or masked, then composited back. Costs a texture and an extra draw |
| Layer | The offscreen pass that `content_blur` and some shadows use; unlike other offscreen passes it is kept and reused while it doesn't change |
| Glass | A box with `backdrop_blur` |
| Sigma | A Gaussian blur's standard deviation in logical px. The blur reaches about 3 sigma |

## Who takes what

A property on a kind that does not take it is refused with the list of what it does take.

| Properties | Taken by |
| :--- | :--- |
| `shadow_color`, `shadow_blur`, `shadow_offset`, `shadow_spread`, `content_blur`, `opacity` | Every node, including `text`, `icon`, `image`, `list`, `textfield` |
| `background`, `radius`, `corner_shape`, `border_color`, `border_width`, `clip`, `mask`, `shadow_mode`, `backdrop_blur`, `blur` | Box kinds only |
| `source_blur` | `image` only |
| `foreground` (`text`, `icon`, `textfield`), `z`, `scale`, `rotate`, `translate`, `origin`, `visible` | Also affect paint; documented on [Nodes](../nodes/index.md) |

Every property can be a [signal](signals.md). A signal nested inside a table (a gradient stop, one
border edge) is refused, so derive the whole table with `:map`. A malformed value fails the pass
instead of drawing a default: the previous scene stays and the error goes to `mantle log`
([runtime](runtime.md#evaluation-reload-and-generations)).

## Colours

Colours are strings `"#RRGGBB"` or `"#RRGGBBAA"`, hex digits in either case. There are no named
colours and no short `#RGB` form.

## Box properties

| Property | Values | Default |
| :--- | :--- | :--- |
| `background` | Colour or [gradient](#gradients). Absent draws nothing; `"#00000000"` is an explicit transparent fill | None |
| `radius` | px `[0, 8192]`. Above half the shorter side it clamps, so `radius = 999` makes a pill or circle | 0 |
| `corner_shape` | `"Round"`, or `"Scoop"`: each corner is a quarter circle cut inward, centred on the corner point. Fill, clip, glass, shadow and `blur` region follow it | `"Round"` |
| `border_color` | Colour, or `{ top, right, bottom, left }` of colours; a missing edge has none | None |
| `border_width` | Number, or `{ top, right, bottom, left }` with missing edges 0; each `[0, 8192]` | 0 |
| `clip` | See [Clip](#clip) | `"Box"` |
| `mask` | See [Mask](#mask) | None |
| `shadow_mode` | See [Shadows](#shadows) | `"Box"` |
| `backdrop_blur`, `blur` | See [Blurs](#blurs) | 0, `false` |

Borders draw inside the box and take no layout space, so give the box padding at least as wide
as the border. An edge draws only when it has both a colour and a width. A uniform border (same
width and colour on all four edges) follows `radius`; anything else is drawn as four straight
rectangles with square corners.

## Gradients

`background` and `mask` take a gradient table.

```lua
background = {
    gradient = "Linear",
    angle = 90,
    stops = { { 0, "#CBA6F7" }, { 0.5, "#F38BA8" }, { 1, "#89B4FA" } },
}
```

| Key | Rule |
| :--- | :--- |
| `gradient` | `"Linear"`, `"Radial"` or `"Conic"` |
| `angle` | Degrees clockwise from the top, as in CSS. `Linear` default 180 (top to bottom), `Conic` default 0 (starts at twelve o'clock). `Radial` refuses it |
| `stops` | At least 2 `{ position, colour }` pairs. Positions in `[0, 1]`, never descending; two equal positions make a hard edge |

| Shape | Geometry |
| :--- | :--- |
| `Linear` | Along `angle` through the centre, long enough that the corners take the end stops (CSS) |
| `Radial` | An ellipse from the centre out to the box's edges, not its corners |
| `Conic` | A turn around the centre, starting at `angle` |

## Clip

`clip` decides what a box cuts its children to.

| Value | Children are cut to | Cost |
| :--- | :--- | :--- |
| `"Box"` | The box's rectangle | Free (a scissor) |
| `"Rounded"` | The box's `radius` and `corner_shape`. With `radius = 0` it is `"Box"` | An offscreen pass every repaint of the box |
| `"None"` | Whatever the parent cuts to, so children and their shadows can overflow this box | Free |

A rounded clip draws in the order fill, children, border, so the border stays on top of children
that reach the arc.

## Mask

`mask` multiplies the alpha of the node's own fill and border and of its whole subtree.

| Form | Alpha taken from |
| :--- | :--- |
| A [gradient](#gradients) table | The gradient's colours' alpha, laid over the box. RGB is ignored |
| `{ source = "/path.png" }` | The image's alpha, stretched over the box. A file that fails to load leaves the node unmasked |
| Either, plus `invert = true` | The complement: kept and cut swap |

Name exactly one of `source` or a gradient. A masked box draws its subtree offscreen every repaint
and always cuts children to its box (to `radius` too under `clip = "Rounded"`), even with
`clip = "None"`.

```lua
column {
    height = 240,
    spacing = 6,
    scroll = scroll("feed"),
    mask = {
        gradient = "Linear",
        stops = { { 0, "#00000000" }, { 0.08, "#000000" }, { 0.92, "#000000" }, { 1, "#00000000" } },
    },
    children = items,
}
```

A scrolling list whose rows fade out at the top and bottom edges.

## Shadows

A shadow draws when `shadow_color` has alpha above 0 and at least one of `shadow_blur`,
`shadow_offset` or `shadow_spread` is set. The terms are CSS's `box-shadow`.

| Property | Values | Default |
| :--- | :--- | :--- |
| `shadow_color` | Colour | `"#000000"` |
| `shadow_blur` | CSS blur radius in px `[0, 8192]`; the Gaussian's sigma is half of it | 0 |
| `shadow_offset` | `{ x, y }` px, each `[-8192, 8192]`, missing axis 0 | `{ x = 0, y = 0 }` |
| `shadow_spread` | px `[-8192, 8192]` the shape grows (negative shrinks) per side. On a non-box shadow it scales the shadow about the box centre instead | 0 |
| `shadow_mode` | Box kinds only. `"Box"`: CSS `box-shadow`, cast by the box's shape and cut out under the box. `"Content"`: CSS `drop-shadow`, cast by everything the node and its subtree paint | `"Box"` |

Non-box nodes (`text`, `icon`, `image`, ...) have no box to cast, so their shadow is always the
content's: text gets a glyph-shaped shadow.

| Case | How it draws |
| :--- | :--- |
| Box mode, `radius >= 0`, any fill | One gradient quad around the box, cheap. On a translucent box it is cut out under the box, so it never shows through the fill |
| An opaque box (solid colour fill with alpha 1, no mask, no `content_blur`, `opacity` 1), either mode | The same gradient quad; the box covers what is under it |
| Content mode on anything else, any non-box node, an opaque scoop | An offscreen layer: the subtree is drawn, blurred and tinted `shadow_color` |
| Box mode on a translucent scoop | A layer of the scoop's silhouette, cut out under the box |

## Blurs

Four properties blur four different things. Sigmas are in logical px, `[0, 8192]`, 0 is off.
`source_blur` is a fast box approximation; the others are Gaussian.

| Property | Reads | When it runs | Cost | Pick it for |
| :--- | :--- | :--- | :--- | :--- |
| `blur = true` (box kinds) | The desktop behind the surface: other windows and the wallpaper, not this surface's own pixels | Continuously, in the compositor | The compositor's | A translucent bar or panel over windows |
| `backdrop_blur = sigma` (box kinds) | What this surface has already painted under the box: ancestors, earlier siblings, lower `z`. Never the desktop | Every repaint that touches the box or what it reads, on the GPU | A copy and a blur per repaint; not cached | Glass over the surface's own wallpaper, image or animated content |
| `content_blur = sigma` (every node) | The node's own subtree | On repaint, on the GPU, into an offscreen layer | A blur when the subtree changes; an unchanged layer is reused. Large sigmas downsample first | A blurred or blur-in element, tweened with `animate` |
| `source_blur = sigma` (`image`) | The image file's pixels | Once, on the CPU, when the source decodes | Nothing per frame | A static blurred picture on a surface that repaints often |

**`blur = true`.** Mantle sends the compositor a region, through `ext-background-effect-v1`, made
of every `blur = true` box on the surface: rounded to `radius` (or scooped), cut by ancestor
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
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 36,
    exclusive = true,
    background = "#1E1E2E99",
    blur = true,
    child = row { width = "Fill", padding = { left = 12, right = 12 }, children = { clock } },
}
```

A bar whose 60% fill tints the compositor-blurred desktop behind it.

```lua
rect {
    width = 320,
    height = 180,
    children = {
        image { source = "/usr/share/backgrounds/default.png", width = "Fill", height = "Fill", async = true },
        row {
            align_h = "Center",
            align_v = "Center",
            padding = { left = 14, right = 14, top = 6, bottom = 6 },
            radius = 999,
            background = "#FFFFFF1F",
            border_width = 1,
            border_color = "#FFFFFF33",
            backdrop_blur = 12,
            children = { text { content = "12:45", font_size = 18, foreground = "#FFFFFF" } },
        },
    },
}
```

A frosted pill: the image is painted first, so the pill's `backdrop_blur` blurs the image under
its rounded shape, and the fill tints it. The same pattern over a full-screen image frosts a lock
screen's wallpaper.

## Combining effects

One node paints in this order, each step over the last:

1. **Backdrop** (`backdrop_blur`): replaces the pixels under the box with their blur.
2. **Shadow**, when it is a gradient quad or a silhouette.
3. **Body**: fill, children in `z` order, border. With a `mask` or a `clip = "Rounded"` the body
   goes through an offscreen pass.
4. **Layer**: for `content_blur` or a layered shadow, the body is drawn offscreen, its shadow cast
   from it, then the body blurred.
5. **Transform** (`scale`, `rotate`, `translate`) wraps all of the above.

| Combination | What happens | Do this |
| :--- | :--- | :--- |
| `mask` and `backdrop_blur` on one node | The mask fades the fill, border and subtree, not the node's own glass or box shadow | Put the glass on a child of the masked node |
| `content_blur` and `backdrop_blur` on one node | The glass stays sharp; only the fill, border and subtree blur | Expected |
| `backdrop_blur` inside a parent with `mask`, `content_blur` or a Content-mode shadow | The glass sees only what that parent has drawn so far, not what is under the parent | Move the glass out of the effect parent, or accept it |
| `backdrop_blur` inside `clip = "Rounded"` without a mask | The glass sees what is under the parent, as without the clip | Nothing to do |
| `backdrop_blur` on a surface root | Nothing is under it on the surface, so it blurs transparency | Use `blur = true` for the desktop |
| `blur = true` and `backdrop_blur` on one box | The compositor blurs the desktop; the backdrop blurs this surface's pixels. Neither sees the other | Pick by what is underneath: desktop or own content |
| Shadow and `content_blur` on one node | The shadow is cast from the sharp content, then the content is blurred | Expected |
| Box-mode shadow on a translucent box | One gradient quad, cut out under the box; children do not cast | `shadow_mode = "Content"` to cast from what is painted |
| Content-mode shadow on a masked node | Cast from the masked result | Expected |
| Content-mode shadow or `content_blur` over an `image`, `icon`, `capture`, image `mask` or glass | The layer is redrawn every repaint instead of reused | Keep those out of animated layers, or accept the cost |
| Anything under a glass changes | The glass repaints, and so does everything in the area it reads (3 sigma past its box) | Keep glass away from constantly animating content, or keep sigma small |
| Shadow or `content_blur` near the parent's edge | Cut at the parent's clip, like any child paint | Give the parent padding, or `clip = "None"` on it |
| `opacity` on a node with effects | Multiplied into every draw once; layers and clips composite at full alpha, so nothing fades twice | Expected |
| `opacity < 1` on a group whose children overlap | Each child fades on its own, so overlaps show through each other (not CSS group opacity) | For a group fade, give the parent a uniform `mask` (e.g. both stops `"#00000080"`); it costs an offscreen pass |
| A transform on a node with a glass or shadow | The backdrop, shadow and body move together; the glass reads under its transformed position | Expected |

## How do I…

| Task | Answer |
| :--- | :--- |
| Frosted glass panel over windows | [Glass sheet](#frosted-glass-panel) below, or the [blur bar](#blurs) |
| Frost a picture inside my own surface | The [frosted pill](#blurs): an `image`, then a sibling with `backdrop_blur` |
| Card with a shadow | The [card](#paint) at the top; [lift on hover](#card-that-lifts-on-hover) below |
| Pill button | [Pill button](#pill-button) |
| Gradient border | [Gradient ring](#gradient-border) |
| Fade a list's edges | The [edge-fade mask](#mask) |
| Circular avatar | [Avatar](#circular-avatar) |
| Dim the background behind a modal | [Scrim](#dim-the-background-behind-a-modal) |
| Tint a gradient from a signal | Map the whole table; see [Gotchas](#gotchas) |

### Frosted glass panel

```lua
panel {
    id = "sheet",
    layer = "Top",
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
        blur = true,
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
    shadow_color = "#00000099",
    shadow_blur = lifted:map(function(on) return on and 36 or 12 end),
    shadow_offset = lifted:map(function(on) return { x = 0, y = on and 20 or 6 } end),
    animate = { shadow_blur = 200, shadow_offset = 200 },
    children = { text { content = "Hover me" } },
}
```

[`hover`](input.md#hover) drives the shadow and [`animate`](animation.md) eases it. Leave room
around the card: the parent clips the shadow.

### Pill button

```lua
local hovered = hover("save_hover")
button {
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

```lua
rect {
    padding = 2,
    radius = 14,
    background = { gradient = "Linear", angle = 135, stops = { { 0, "#CBA6F7" }, { 1, "#89B4FA" } } },
    children = {
        column {
            padding = 14,
            radius = 12,
            background = "#1E1E2E",
            children = { text { content = "Pro", foreground = "#CDD6F4" } },
        },
    },
}
```

`border_color` takes only flat colours, so paint the gradient as an outer fill and cover all but a
2px ring with an opaque inner box. Keep the inner radius the outer radius minus the ring width.

### Circular avatar

```lua
rect {
    width = 64,
    height = 64,
    radius = 32,
    clip = "Rounded",
    border_width = 2,
    border_color = "#89B4FA",
    children = { image { source = "/var/lib/AccountsService/icons/user", width = "Fill", height = "Fill" } },
}
```

`clip = "Rounded"` cuts the image to the circle, and the border paints over the image's edge.

### Dim the background behind a modal

```lua
panel {
    id = "modal",
    layer = "Overlay",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = "Ignore",
    keyboard_interactivity = "OnDemand",
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            rect { width = "Fill", height = "Fill", background = "#11111B99" },
            column {
                align_h = "Center",
                align_v = "Center",
                width = 360,
                padding = 24,
                radius = 16,
                background = "#1E1E2EE0",
                blur = true,
                shadow_color = "#00000080",
                shadow_blur = 32,
                children = { text { content = "Log out?", font_size = 18, foreground = "#CDD6F4" } },
            },
        },
    },
}
```

A full-screen [panel](../surfaces/panel.md) whose first child is a translucent scrim and whose
second is the dialog. Dim with a colour rather than `blur = true` on the scrim: the compositor's
blur does not fade with `opacity`, so a fading scrim would blur at full strength until it hits 0.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A shadow is cut off at one edge | The parent clips it. Pad the parent, or set `clip = "None"` on it |
| `backdrop_blur` shows no desktop behind a translucent panel | It only reads this surface's pixels. Use `blur = true` |
| `blur = true` does nothing | The compositor lacks `ext-background-effect-v1` or its blur capability. No error is raised |
| A `blur = true` box fades out but its blur stays at full strength | The blur region ignores `opacity` until it reaches 0. Dim with a translucent colour, or let the blurred box pop |
| A per-edge border or a border on a scoop has square corners | Only a uniform border follows `radius`, and a scoop's border is always square |
| A border covers content | Borders take no layout space. Add padding at least the border's width |
| `clip = "Rounded"` changes nothing | It needs a non-zero `radius`, and only clips children |
| Children still clipped with `clip = "None"` and a `mask` | A mask always cuts to its box |
| A gradient or a per-edge `border_color` jumps instead of easing under `animate` | Only single colours ease; see [Animation](animation.md) |
| Rounded corners, scoops and masks still take clicks in the cut-away area | Hit-testing uses the rectangle. Shrink the `button` or accept it |
| A signal inside a gradient stop or border edge is refused | Map the whole table: `background = accent:map(function(c) return { gradient = "Linear", stops = { { 0, c }, { 1, "#00000000" } } } end)` |

See also: [nodes](../nodes/index.md), [surfaces](../surfaces/index.md), [animation](animation.md), [input](input.md#hit-testing), [glossary](../glossary.md).

Source: [allowlist](../../renderer/src/lua/nodes.rs),
[parsers](../../renderer/src/layout/node/style/mod.rs),
[paint style](../../renderer/src/layout/node/paint_style.rs),
[paint order](../../renderer/src/layout/paint/build.rs),
[canvas](../../renderer/src/layout/paint/canvas/mod.rs),
[effects](../../renderer/src/layout/paint/canvas/effects.rs),
[shapes](../../renderer/src/layout/paint/canvas/shape.rs),
[blur region](../../renderer/src/layout/region.rs),
[compositor push](../../renderer/src/wayland/surface/resolved.rs).
