# shader

Runs a fragment shader from the config over the node's box: a glow, an animated gradient, a
procedural pattern. It reads no textures and takes no input. To run a shader between two pictures,
use an [image transition](image.md#transition).

A band that glows in over 400 ms when `pulse_on` turns true:

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
| `params` | `table<string, number\|number[]>\|Bound` | `{}` | Uniforms by name: a finite number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing ones are `0`. Not tweened |
<!-- End of the generated table. -->

It has no intrinsic size: without `width` and `height` it draws nothing. `opacity`, transforms,
`shadow_*` and `content_blur` apply to it.

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
| `uniform float`, `vec2`, `vec3`, `vec4` of your own | | Set from `params` by name, `0` when `params` leaves one out. A `params` name with no uniform is ignored; a wrong component count is padded or truncated and logged once |

Write `void main()`. `params` never sets a uniform named `u_*` or `mantle_*`. A uniform the shader reads of any
other type, such as an `int` or a `sampler2D`, refuses the whole shader.

| Event | Result |
| :--- | :--- |
| Compile or link fails | Logged once, draws nothing until the file changes |
| A `.frag` under the config directory is saved | The config reloads, which recompiles it. A file elsewhere recompiles at the surface's next pass |
| `mantle check` | Passes: it has no GPU and compiles no GLSL. The first compile is in the running shell |
| The shader hangs the GPU | The session hangs. It is config code, as trusted as `process.run` |

## How do I…

| Task | Answer |
| :--- | :--- |
| Fade an effect in and out | Bind `progress` to 0 or 1 and tween it with `animate`, as above |
| Loop an animation | `animate = { progress = { keyframes = { 0, 1 }, duration = 2000, loops = "Infinite" } }` ([keyframes](../guide/animation.md#keyframes)) |
| Pass a colour | A `vec3` or `vec4` uniform, `params = { tint = { r, g, b } }` in `0..1` |
| Work in pixels | `v_uv * u_size` is the fragment's position in logical px |
| Click a shader | Wrap it in a [`button`](button.md) |
| Round its corners | Wrap it in a `rect` with `radius` and `clip = "Rounded"` ([clip](../guide/paint.md#clip)) |

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
