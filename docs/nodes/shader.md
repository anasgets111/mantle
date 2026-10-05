# shader

Runs a fragment shader from the config over the node's box: a glow, an animated gradient, a
procedural pattern. It reads only the [`images`](#images) it names and takes no input. To run a shader between two pictures,
use an [image transition](image.md#transition); to run one over a node's painted subtree, use
[`effect.shader`](../guide/paint.md#shader-effects), which shares this page's `.frag` contract.

A band that glows in over 400 ms when `pulse_on` turns true:

<!-- shot-alt: A horizontal blue glow band brightens over 400 milliseconds. -->
<!-- shot: frames=0..420/60 -->
```lua,shot
local pulse_on = state("pulse_on", false)

local glow = shader {
    width = 200,
    height = 40,
    source = mantle.config_dir .. "/shaders/glow.frag",
    progress = pulse_on:map(function(on) return on and 1 or 0 end),
    params = { tint = { 0.54, 0.71, 0.98 } },
    animate = { progress = 400 },
}

return glow
```

`shaders/glow.frag` in the config directory:

<!-- file: shaders/glow.frag -->
```glsl
uniform vec3 tint;

void main() {
    // Distance from the horizontal centre line, 0 at the middle, 1 at the edges.
    float edge = abs(v_uv.y - 0.5) * 2.0;
    float alpha = (1.0 - edge) * u_progress;
    fragColor = vec4(tint * alpha, alpha); // premultiplied
}
```

## Properties

`shader` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | `string\|Bound` | `""` | Absolute `.frag` path; relative is refused, `""` draws nothing. Compiling, errors and reloads: [the .frag file](#the-frag-file) |
| `progress` | `number\|Bound`, `[-8192, 8192]` | `0` | Becomes `u_progress`. There is no clock uniform: [animate](../guide/animation.md) this for motion; the wide range lets a spring overshoot |
| `params` | `table<string, number\|number[]>\|Bound` | `{}` | Uniforms by name: a finite number for `float`, a list of up to 4096 for `vec2`-`vec4` or an array of either, flattened. Missing ones are `0`. Not tweened |
| `images` | `table<string, string>\|Bound` | `{}` | Up to 8 absolute PNG, JPEG or WebP paths by sampler name: [images](#images). Missing ones sample transparent black. Not tweened |
<!-- End of the generated table. -->

It has no intrinsic size: without `width` and `height` it draws nothing. `opacity`, transforms,
`shadows` and `effect.blur` apply to it. Place animated boxes with `behind_blur = true` beneath a
shader when the desktop behind it should blur ([blurs](../guide/paint.md#blurs)).

## The .frag file

GLSL ES 3.00 without the header. The engine prepends `#version 300 es`, `precision highp float` and
the declarations below, then compiles the file as written. Error line numbers count from the file's
first line.

| Name | Type | What |
| :--- | :--- | :--- |
| `v_uv` | `in vec2` | Box coordinate, `0..1`, top-left origin, y down |
| `fragColor` | `out vec4` | Premultiplied RGBA. The engine multiplies it by the node's opacity afterwards |
| `u_progress` | `float` | The node's `progress` |
| `u_size` | `vec2` | The node's size in logical px |
| `uniform float`, `vec2`, `vec3`, `vec4` of your own, or arrays of them | | Set from `params` by name, `0` when `params` leaves one out. An array takes one flat list, element after element. A `params` name with no uniform is ignored; a wrong component count is padded or truncated and logged once |

`mantle_sdf` and `mantle_input` exist only for [`effect.shader`](../guide/paint.md#shader-effects).
Write `void main()`. `params` never sets a uniform named `u_*` or `mantle_*`. A uniform the shader reads of any
other type, such as an `int` or a `sampler2D`, refuses the whole shader.

| Event | Result |
| :--- | :--- |
| Compile or link fails | Logged once, draws nothing until the file changes |
| A `.frag` under the config directory is saved | The config reloads, which recompiles it. A file elsewhere recompiles at the surface's next pass |
| `mantle check` | Passes: it has no GPU and compiles no GLSL. The first compile is in the running shell |
| The shader hangs the GPU | The session hangs. It is config code, as trusted as `process.run` |

## Images

`images` binds raster files for the shader to sample: a normal or displacement map, a noise texture, a gradient LUT.

```lua
local glass = shader {
    width = 200,
    height = 120,
    source = mantle.config_dir .. "/shaders/lens.frag",
    images = { lens = mantle.config_dir .. "/lens.png" },
}
```

```glsl
void main() {
    vec2 offset = (texture(lens, v_uv).rg - 0.5) * 0.1;
    fragColor = vec4(offset + 0.5, 0.0, 1.0);
    // lens_size is the file's size in pixels, so a texel is 1.0 / lens_size.
}
```

| Rule | Detail |
| :--- | :--- |
| Declared for you | `uniform sampler2D <name>;` and `uniform vec2 <name>_size;` (pixels). Do not declare them |
| Names | A GLSL identifier of up to 64 characters, not `u_*`, `mantle_*`, `gl_*`, `v_uv`, `fragColor` or `main`, with no `__` and not ending in `_size`. Others are refused |
| Files | Absolute PNG, JPEG or WebP, at most 8192 px a side; at most 8 entries. An SVG is refused |
| Sampling | `texture(name, uv)` with `uv` in `0..1`, top-left origin like `v_uv`. Linear filter, clamped to the edge, no mipmaps. Colour is premultiplied, as `fragColor` is |
| Missing or undecodable file | Samples `vec4(0.0)` with `name_size` of `vec2(1.0)`, logged once per path. The node still draws |
| Changed file | A new modification time or length reloads it, as an `image` does |
| Elsewhere | `effect.shader` takes the same key, on `input = "content"` and `"backdrop"` |
| Sharing | Two nodes naming one path share one texture, held by the image cache and freed with it |

## How do I…

| Task | Answer |
| :--- | :--- |
| Fade an effect in and out | Bind `progress` to 0 or 1 and tween it with `animate`, as above |
| Loop an animation | `animate = { progress = { keyframes = { 0, 1 }, duration = 2000, loops = "infinite" } }` ([keyframes](../guide/animation.md#keyframes)) |
| Pass a colour | A `vec3` or `vec4` uniform, `params = { tint = { r, g, b } }` in `0..1` |
| Pass many values, like a visualizer's bars | `uniform vec4 bars[64]` takes 256 numbers, `params = { bars = levels }`; read bar `i` as `bars[i / 4][i % 4]`. Drivers may count each `float` array element as a whole `vec4` against the uniform limit, so pack into `vec4`s |
| Work in pixels | `v_uv * u_size` is the fragment's position in logical px |
| Click a shader | Give it `on_click`; without a handler it is transparent to the pointer |
| Round its corners | Wrap it in a `rect` with `radius` and `clip = "rounded"` ([clip](../guide/paint.md#clip)) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Draws nothing, and `check` passed | Read `mantle log` for the compile error. Check the node has a size and an absolute `source` |
| The shader is static | There is no time uniform. Animate `progress` |
| A `uniform int` refuses the shader | Declare it `float` and pass the integer as a number |
| Colours glow too bright where alpha is low | `fragColor` is premultiplied: multiply RGB by alpha |
| A `params` change jumps | `params` is not tweened. Drive the change through `progress` |

See also: [image transitions](image.md#transition), [animation](../guide/animation.md), [paint](../guide/paint.md).

Source: [vocabulary](../../renderer/src/lua/nodes/properties.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[params](../../renderer/src/layout/node/animate/transition.rs), [shader stage](../../renderer/src/layout/image_shader/mod.rs),
[`.frag` reloads](../../supervisor/src/watcher.rs).
