# capture

A live preview of one output (monitor), through the compositor's screen-capture protocol
(`ext-image-copy-capture-v1`, or `wlr-screencopy`). Reach for it for an overview, a monitor picker
or a screenshot preview. Without either protocol it draws nothing and logs one warning.

A rounded preview of the first screen at up to 30 frames per second:

```lua
local first_output = mantle.screens:map(function(screens)
    return screens and screens[1] and screens[1].name or ""
end)

local preview = rect {
    width = 320,
    height = 180,
    radius = 8,
    clip = "Rounded",
    background = "#000000",
    children = {
        capture { output = first_output, live = 30, fit = "contain", width = "Fill", height = "Fill" },
    },
}
```

[`mantle.screens`](../capabilities/index.md#renderer-members) lists the connected outputs by connector name.

## Properties

`capture` takes the [common properties](index.md#common-properties), plus:

| Property | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `output` | Connector name, e.g. `"DP-1"` | `""`, drawing nothing | A name that is not connected draws nothing and logs one warning. Changing it starts a fresh capture |
| `fit` | `"cover"`, `"contain"`, `"stretch"` | `"cover"` | As on [`image`](image.md) |
| `live` | `false`, `true`, or frames per second `(0, 1000]` | `false` | `false`: one frame on show and on each `output` change. `true`: every frame, one in flight. A number: at most that many frames per second |
| `region` | `{ x, y, width, height }` in the output's logical px | The whole output | Every key required, each `[0, 8192]`, size non-zero. Placed by `fit` as if it were the whole frame |
| `paint_cursor` | Boolean | `false` | Include the pointer in the frame |

It has no intrinsic size: without `width` and `height` it draws nothing. Capture pauses while the
node is hidden or its surface unmapped, and starts fresh when it shows again. New frames arrive only
when the screen changes. A capture that fails pauses until the output list changes.

A `region` prefers `wlr-screencopy`, which crops at the source. Through `ext-image-copy-capture-v1`
the engine crops instead, and on a rotated or flipped output it cannot: it draws the whole output
and logs one warning.

## How do I…

| Task | Answer |
| :--- | :--- |
| Preview a monitor | The example above |
| Preview every monitor | A [`list`](list.md) over `mantle.screens`, `key` = the screen's `name`, one `capture` per item |
| Show a part of the screen | `region = { x = 0, y = 0, width = 960, height = 540 }` |
| Keep CPU low | Leave `live = false` for a still, or cap it: `live = 10` |
| Include the mouse pointer | `paint_cursor = true` |
| Round the corners | Wrap it in a `rect` with `radius` and `clip = "Rounded"`, as above |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Nothing draws | Give it a size; check `output` against `mantle.screens` names; check `mantle log` for a missing-protocol warning |
| The preview is frozen | `live` is `false`, which captures once. Set `true` or a frame rate |
| `live = 0` is refused | Use `false` for a single frame |
| A `region` shows the whole output | The output is rotated or flipped and only `ext-image-copy-capture-v1` is offered |
| `region = { width = 100, height = 100 }` is refused | All four keys are required |

See also: [image](image.md), [surfaces](../surfaces/index.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[capture](../../renderer/src/wayland/capture/mod.rs).
