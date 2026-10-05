# image

A picture from a file: wallpapers, album art, avatars, thumbnails. It can decode off-thread, hold
the previous picture while a new one loads, and cross-fade or run a shader between them. For theme
icons, use an [`icon`](icon.md).

SVG and SVGZ `<text>` use the fonts loaded by Mantle. Declare required families in `fonts { ... }`;
text without a family uses the chain's primary font. Generic names use the families declared in
the chain. An unavailable family falls back to the loaded serif family, or the primary if none
is loaded. SVG text uses SVG layout and shaping, so its spacing can differ from a `text` node. Images, icons and file masks
share this behavior, including background decoding.

A wallpaper that crossfades when the path changes:

<!-- shot-alt: A landscape illustration with layered hills and a sun. -->
```lua,shot
local path = state("wallpaper", "/usr/share/backgrounds/a.jpg")

return { panel {
    id = "wallpaper",
    layer = "background",
    anchor = { top = true, bottom = true, left = true, right = true },
    width = "fill",
    height = "fill",
    child = image {
        id = "wallpaper_image", -- keeps the node, and so the held picture, across source changes
        source = path,
        async = true,
        transition = { duration = 600, easing = "in_out_cubic" },
        width = "fill",
        height = "fill",
    },
} }
```

## Properties

`image` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `source` | `string\|Bound` | `""` | A file path (`mantle.config_dir .. "/img/a.png"`), never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop |
| `fit` | `"cover"\|"contain"\|"stretch"\|Bound` | `"cover"` | `"cover"` fills the box and crops, `"contain"` fits inside it, `"stretch"` distorts to it |
| `async` | `boolean\|Bound` | `false` | `false` decodes in the frame that first draws it. `true` decodes on a worker and draws nothing until the first decode lands. A resize keeps drawing the previous size, scaled, until the new size decodes; use it for many or large images |
| `retain` | `boolean\|Bound` | `false` | Keep drawing the last picture while a new `source` decodes, and on a failed decode. Needs `async = true` and a stable `id` |
| `transition` | `Transition\|Bound` | None | Cross from the held picture to each newly decoded `source`. Implies `retain`; needs `async = true` and a stable `id`. Unknown keys are refused. See [transition](#transition) |
| `source_blur` | `number\|Bound`, `[0, 8192]` | `0` | Blur sigma in px, baked into the pixels once at decode (three box passes approximating a Gaussian); see [blurs](../guide/paint.md#blurs). Animated GIFs ignore it. Under `async`, a change blanks the image until the re-decode lands; `retain` does not cover it |
| `radius` | `number\|Corners\|Bound`, `[0, 8192]` | `0` | Corner radius px of the drawn picture, as `rect.radius`: a number sets all four corners, a missing corner is `0`, corners too big for a side shrink together. Rounds the visible picture, so `"contain"` rounds the fitted picture, not the box. Hit-testing ignores it |
| `corner_smoothing` | `number\|Bound`, `[0, 1]` | `0` | Continuous corners on the drawn picture, as `rect.corner_smoothing`. The shader that rounds a `transition` approximates a smoothed corner by a superellipse through its endpoints and midpoint |
<!-- End of the generated table. -->

### transition

| Field | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `duration` | ms, `[1, 60000]` | Required | Length of the cross |
| `easing` | An [easing](../guide/animation.md) | `"in_out_quad"` | Drives `u_progress` |
| `shader` | Absolute `.frag` path | Built-in cross-dissolve | Replaces the dissolve. Recompiled when the file changes |
| `params` | `{ name = number \| { up to 4096 numbers } }` | `{}` | Uniforms for that shader, as on a [shader node](shader.md). Refused without `shader` |
| `images` | `{ name = "/abs/path.png" }` | `{}` | Up to 8 rasters to sample by name, with the rules of a [shader node's `images`](shader.md#images); they take the units after `u_from` and `u_to`. Refused without `shader` |

The first picture appears without a transition. A transition shader gets everything a
[shader node](shader.md#the-frag-file) gets, plus:

| Name | What |
| :--- | :--- |
| `mantle_from(uv)`, `mantle_to(uv)` | Outgoing and incoming picture at a box coordinate, premultiplied, already placed by `fit`; transparent outside the picture |
| `u_from_rect`, `u_to_rect` | Each picture's `(x, y, w, h)` in box fractions; may pass `0..1` under `"cover"` |

`u_progress` is the eased progress `0..1`. A transition shader that fails to build logs once and
the node falls back to the cross-dissolve.

## How do I…

| Task | Answer |
| :--- | :--- |
| Crossfade a wallpaper | The example above: `async`, `transition` and a stable `id` |
| Wipe instead of fade | `transition = { duration = 700, shader = mantle.config_dir .. "/shaders/wipe.frag" }` with a `.frag` that mixes `mantle_from` and `mantle_to` |
| Round an image's corners | [Below](#round-an-images-corners) |
| Make a circular avatar | The same, with `radius` half the size ([paint](../guide/paint.md#circular-avatar)) |
| Show many thumbnails without stutter | `async = true` on each, in a [`list`](list.md) |
| Blur a wallpaper once | `source_blur = 20` |
| Show a file that ships with the config | `source = mantle.config_dir .. "/img/logo.png"` |

### Round an image's corners

`radius` rounds the picture itself, as on a [`rect`](rect.md), so it needs no clipping wrapper. It
rounds what shows: with `fit = "contain"` that is the fitted picture, not the box.

```lua
image {
    width = 96,
    height = 96,
    radius = 12,
    source = "/usr/share/backgrounds/a.jpg",
    fit = "cover",
    async = true,
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| The image does not appear | It has no intrinsic size. Give `width` and `height` |
| An image flashes blank when its `source` changes despite `retain` | `retain` needs `async = true` and a node that survives: give it a stable `id` |
| The shell stutters while images load | Inline decode blocks drawing. Set `async = true` |
| `source = "firefox"` draws nothing | `source` is a path. Use [`icon`](icon.md) for theme names |
| A relative `source` draws nothing | It resolves against the Renderer's working directory, not the config. Build paths from `mantle.config_dir` |
| `transition.params` or `transition.images` is refused | Both need a `shader` |

See also: [icon](icon.md), [shader](shader.md), [paint](../guide/paint.md), [animation](../guide/animation.md).

Source: [vocabulary](../../renderer/src/lua/nodes/properties.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[transition](../../renderer/src/layout/node/animate/transition.rs),
[shader stage](../../renderer/src/layout/image_shader/mod.rs), [decode](../../renderer/src/image/decode.rs).
