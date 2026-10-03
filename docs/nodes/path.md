# path

Experimental vector leaf with no intrinsic size. `width` and `height` define its layout box;
commands use logical pixels relative to the box's top-left corner. Resizing does not scale
coordinates. Use the existing `scale` transform to scale a drawing.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `commands` | `PathCommand[]\|Bound` | `{}` | Up to 4096 commands. Each has op M/L/Q/C/A/Z and points containing 2/2/4/6/5/0 numbers. Begin each subpath with M or A. Coordinates are in [-8192, 8192]; arc angles need only be finite |
| `fill` | `Color\|Gradient\|Bound` | None | Fill colour or gradient across the node box. Open subpaths close for filling |
| `stroke` | `Color\|Gradient\|Bound` | None | Stroke colour or gradient across the node box. Butt caps and miter joins |
| `stroke_width` | `number\|Bound`, `[0, 8192]` | `1` | Stroke width in logical pixels; centered on the path |
<!-- End of the generated table. -->

Each command has `op` and `points`. `M` moves, `L` draws a line, `Q` takes a control point and
endpoint, `C` takes two control points and an endpoint, and `Z` closes the subpath. `Z` still
needs `points = {}`. After closing, start another subpath with `M` or `A`. Empty commands draw
nothing. Arrays must be dense.

`A` draws a circular arc: `points = { cx, cy, r, start, sweep }`, with the centre, a radius of at
least 0, and angles in degrees from the +x axis, any finite value. A positive sweep turns clockwise on screen, a
negative one anticlockwise, and a sweep of 360 or more draws the full circle. `A` can begin a
subpath; inside one, a line joins the current point to the arc's start. This is not SVG's
elliptical `A`.

Subpaths are solid whatever their winding, so overlapping ones merge. Set `hole = true` on the
command that begins a subpath to cut it out of the fill instead. Each solid subpath counts +1 and
each hole -1 at a point, and any count but 0 paints: a hole outside every solid subpath, or two
holes overlapping inside one, fills.

Strokes use butt caps and miter joins. Open paths close for filling. The layout box does not grow
to include strokes; leave padding inside it. Ancestor clipping, masks, opacity, transforms,
shadows and content blur use the existing paint pipeline.

A painted path claims its whole box for input, not the drawn shape; an unpainted one is
click-through unless it has a pointer handler ([input region](../surfaces/index.md#input-region)).

A chart component builds commands from values without writing an SVG file:

```lua
local function chart(values)
    local commands = {}
    for i, value in ipairs(values) do
        commands[i] = {
            op = i == 1 and "M" or "L",
            points = { 4 + (i - 1) * 16, 60 - value * 56 },
        }
    end
    return path {
        width = 120,
        height = 64,
        stroke = "#80c0ff",
        stroke_width = 3,
        commands = commands,
    }
end

return chart { 0.2, 0.5, 0.3, 0.9, 0.6, 0.8, 0.7 }
```

A progress ring is a full-circle track with a hole, and a stroked arc for the value:

<!-- shot-alt: A storage card: a dim ring with a blue arc over its first 70 percent, clockwise from the top, beside the label and usage. -->
```lua,shot
local function ring(value)
    return rect {
        width = 48,
        height = 48,
        children = {
            path {
                width = 48,
                height = 48,
                fill = "#ffffff20",
                commands = {
                    { op = "A", points = { 24, 24, 22, 0, 360 } },
                    { op = "Z", points = {} },
                    { op = "A", points = { 24, 24, 16, 0, 360 }, hole = true },
                },
            },
            path {
                width = 48,
                height = 48,
                stroke = "#89b4fa",
                stroke_width = 6,
                commands = { { op = "A", points = { 24, 24, 19, -90, value * 360 } } },
            },
        },
    }
end

return row {
    padding = 14,
    spacing = 14,
    radius = 12,
    background = "#1e1e2e",
    children = {
        ring(0.7),
        column {
            align_v = "center",
            spacing = 2,
            children = {
                text { content = "Storage", font_weight = 700, foreground = "#cdd6f4" },
                text { content = "358 of 512 GB used", font_size = 12, foreground = "#a6adc8" },
            },
        },
    },
}
```

Bind `commands` to a signal to replace a path. `animate` tweens `commands` point by point when the
old and new lists have the same ops and `hole` flags in the same order; any other change snaps.
Arc angles tween too, so a ring can grow its sweep. To morph between shapes, give each one the
same command layout. This loader samples three outlines at the same 60 angles and loops through
them as keyframes, with no Lua running per frame:

```lua
local function outline(radius)
    local commands = {}
    for i = 0, 59 do
        local a = math.rad(i * 6 - 90)
        local r = radius(i, a)
        commands[i + 1] = { op = i == 0 and "M" or "L", points = { 24 + r * math.cos(a), 24 + r * math.sin(a) } }
    end
    commands[61] = { op = "Z", points = {} }
    return commands
end

local star = outline(function(i) return 22 - 12 * (1 - math.abs(i % 12 / 6 - 1)) end)
local square = outline(function(_, a) return 17 / math.max(math.abs(math.cos(a)), math.abs(math.sin(a))) end)
local circle = outline(function() return 20 end)

return path {
    width = 48,
    height = 48,
    fill = "#fab387",
    animate = {
        commands = {
            duration = 600,
            easing = "in_out_cubic",
            keyframes = { star, square, circle, star },
            loops = "infinite",
        },
    },
}
```
