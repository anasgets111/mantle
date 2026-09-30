# path

Experimental vector leaf with no intrinsic size. `width` and `height` define its layout box;
commands use logical pixels relative to the box's top-left corner. Resizing does not scale
coordinates. Use the existing `scale` transform to scale a drawing.

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `commands` | `PathCommand[]\|Bound` | `{}` | Up to 4096 commands. Each has op M/L/Q/C/Z and points containing 2/2/4/6/0 numbers. Begin each subpath with M. Coordinates are in [-8192, 8192] |
| `fill` | `Color\|Gradient\|Bound` | None | Fill colour or gradient across the node box. Open subpaths close for filling |
| `stroke` | `Color\|Gradient\|Bound` | None | Stroke colour or gradient across the node box. Butt caps and miter joins |
| `stroke_width` | `number\|Bound`, `[0, 8192]` | `1` | Stroke width in logical pixels; centered on the path |
<!-- End of the generated table. -->

Each command has `op` and `points`. `M` moves, `L` draws a line, `Q` takes a control point and
endpoint, `C` takes two control points and an endpoint, and `Z` closes the subpath. After
closing, start another subpath with `M`. Empty commands draw nothing. Arrays must be dense.

Subpaths are solid; hole and fill-rule controls are unavailable. Strokes use butt caps and
miter joins. Open paths close for filling.
The layout box does not grow to include strokes; leave padding inside it. Ancestor clipping,
masks, opacity, transforms, shadows and content blur use the existing paint pipeline.

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

Bind `commands` to a signal to replace a path. Commands are not interpolated by `animate`.
