# shader

Runs a fragment shader from the config over the node's box: a glow, a gradient animation, a
procedural pattern. It reads no textures and takes no input; wrap it in a [`button`](button.md) to
click it. To run a shader between two pictures, use an [image transition](image.md#transition).

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

## Properties

`shader` takes the [common properties](index.md#common-properties), plus:

| Property | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | Absolute `.frag` path; a relative one is refused | `""`, drawing nothing | The shader to run. Saving the file recompiles it |
| `progress` | Number, `[-8192, 8192]` | 0 | Becomes `u_progress`. There is no clock uniform: [animate](../guide/animation.md) this for motion; the wide range lets a spring overshoot |
| `params` | `{ name = number \| { 2 to 4 numbers } }`, finite numbers | `{}` | Values for the shader's own `float` and `vec2` to `vec4` uniforms. Not tweened |

It has no intrinsic size: without `width` and `height` it draws nothing. `opacity`, transforms,
`shadow_*` and `content_blur` apply to it.

## The .frag file

GLSL ES 3.00 without the header. The engine prepends `#version 300 es`, `precision highp float` and
these declarations, then compiles your file as written. Error line numbers count from your first
line.

| Name | Type | What |
| :--- | :--- | :--- |
| `v_uv` | `in vec2` | Box coordinate, `0..1`, top-left origin, y down |
| `fragColor` | `out vec4` | Write premultiplied RGBA. The engine multiplies it by the node's opacity afterwards |
| `u_progress` | `float` | The node's `progress` |
| `u_size` | `vec2` | The node's size in logical px |
| Your own `uniform float`/`vec2`/`vec3`/`vec4` | | Set from `params` by name. Missing ones are 0; `params` names the shader has no uniform for are ignored; a wrong component count is padded or truncated and logged once |

Write `void main()`. Names starting `u_` or `mantle_` are reserved. A uniform of any other type
(an `int`, a `sampler2D`) refuses the shader. A shader that fails to compile or link logs once and
draws nothing until the file changes. A shader that hangs the GPU hangs the session.

`mantle check` has no GPU and does not compile shaders. The first compile happens in the running
shell, and errors appear in `mantle log`.

## How do I…

| Task | Answer |
| :--- | :--- |
| Fade an effect in and out | Bind `progress` to 0 or 1 and tween it with `animate`, as above |
| Loop an animation | `animate = { progress = { keyframes = { 0, 1 }, duration = 2000, loops = "Infinite" } }` ([keyframes](../guide/animation.md#keyframes)) |
| Pass a colour | A `vec3`/`vec4` uniform with `params = { tint = { r, g, b } }` in `0..1` |
| Keep the effect in proportion | Divide by `u_size` in the shader |
| Click a shader | Wrap it in a [`button`](button.md) |
| Round its corners | Wrap it in a `rect` with `radius` and `clip = "Rounded"` ([clip](../guide/paint.md#clip)) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A shader draws nothing and `check` passed | `check` does not compile GLSL. Read `mantle log` for the compile error, and check the node has a size and an absolute `source` |
| The shader is static | There is no time uniform. Animate `progress` |
| A `uniform int` breaks the shader | Only `float` and `vec2`-`vec4` uniforms are allowed; pass integers as floats |
| Colours look too bright at the edges | `fragColor` is premultiplied: multiply RGB by alpha |
| Changing `params` jumps | `params` is not tweened. Drive the change through `progress` |

See also: [image transitions](image.md#transition), [animation](../guide/animation.md), [paint](../guide/paint.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[params](../../renderer/src/layout/node/animate/transition.rs), [shader stage](../../renderer/src/layout/image_shader/mod.rs).
