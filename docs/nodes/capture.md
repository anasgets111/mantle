# capture

A preview of an output or a window. Outputs use `ext-image-copy-capture-v1`, with
`wlr-screencopy` as a fallback. Window capture currently requires Hyprland's toplevel address
mapping and ext capture protocols. Without the required protocols it draws nothing and warns once
per target.

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

Preview the focused window using its [`mantle.windows`](../capabilities/windows.md) ID:

```lua
local focused_window = mantle.windows:map(function(state)
    for _, entry in ipairs(state and state.windows or {}) do
        if entry.focused then return entry.id end
    end
    return ""
end)

return capture { window = focused_window, live = 15, fit = "contain", width = 320, height = 180 }
```

Window capture reads the toplevel itself, including when another window covers it. It does not
crop an output. Pass the ID unchanged; names and titles are not identifiers.

| Windows backend | Window capture |
| :--- | :--- |
| Hyprland | Supported when the mapping and ext toplevel capture protocols are advertised |
| Niri | Unsupported. Its foreign-toplevel identifiers match its window IDs, but its ext capture implementation currently handles outputs only |
| wlr | Unsupported. Mantle assigns connection-local window IDs; the protocol provides no exact mapping to the renderer's foreign-toplevel handles |

Niri's [identifier implementation](https://github.com/niri-wm/niri/blob/1f03391ea644c2a43597de7f637269e26d1e1b49/src/window/mapped.rs)
and [capture handler](https://github.com/niri-wm/niri/blob/1f03391ea644c2a43597de7f637269e26d1e1b49/src/handlers/image_copy_capture.rs)
confirm these are separate requirements. Non-Hyprland window capture remains on the
[roadmap](../roadmap.md).

## Properties

`capture` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `output` | `string\|Bound` | `""` | Connector name, e.g. `"DP-1"`; `""` draws nothing. An unknown name draws nothing and warns once. Changing it starts a fresh capture |
| `window` | `string\|Bound` | `""` | A `mantle.windows` entry's `id`; `""` draws nothing. Currently requires Hyprland's exact toplevel mapping and ext capture protocols. Cannot combine with a nonempty `output` or `region`. A closed window clears its preview |
| `fit` | `"cover"\|"contain"\|"stretch"\|Bound` | `"cover"` | As on [`image`](image.md) |
| `live` | `boolean\|number\|Bound` | `false` | `false`: capture on show and on each target change. `true`: every frame, one in flight. A number: at most that many fps, `(0, 1000]`. Hiding the node or unmapping its surface drops the capture; showing starts a fresh one |
| `region` | `Rect\|Bound`, `[0, 8192]` | The whole output | Part of the output in its logical px, placed by `fit` as the whole frame. Every key is required and in that range; the size is non-zero |
| `paint_cursor` | `boolean\|Bound` | `false` | Include the pointer in the frame |
<!-- End of the generated table. -->

It has no intrinsic size: without `width` and `height` it draws nothing. A hidden node or unmapped
surface drops its capture and starts a fresh one when it shows again. A live capture gets a new
frame only when its source changes. A resized source renegotiates its buffers and keeps going.
A stopped live output session keeps its last frame and restarts when its frame-rate cap allows,
even without a repaint. A completed one-shot keeps its frame. Any other failed output capture
retries when the output list changes. A failed or closed window capture clears or stops its preview
and remains stopped until the target changes or the node hides and shows again. An unrelated
output change cannot revive a closed window's ID. A one-shot captures the current frame; use
`live` to follow opening animations.

A `region` prefers `wlr-screencopy`, which crops at the source. Through `ext-image-copy-capture-v1`
the engine crops instead, and on a rotated or flipped output it cannot: it draws the whole output
and logs one warning.

## How do I…

| Task | Answer |
| :--- | :--- |
| Preview a monitor | The first example above |
| Preview a window | `window = entry.id` from `mantle.windows`; currently Hyprland only |
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
| `window` and `output` are both set | Only one may be nonempty; `region` is output-only |

See also: [image](image.md), [surfaces](../surfaces/index.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[capture](../../renderer/src/wayland/capture/mod.rs).
