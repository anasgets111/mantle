# text

One paragraph drawn in the [`fonts`](../guide/scripting.md#fonts) chain or a named family: labels,
clocks, notification bodies. It sizes to its content, wraps and elides inside a bounded width, and
mixes bold, italic, colour and links through [runs](#runs). For typing, use a [`textfield`](textfield.md).

Two notification cards. In the first, the title elides and the body wraps to two lines, eliding the
second; the second card's short texts fit.

```lua,shot
local function card(icon_name, title, body)
    return row {
        width = "Fill",
        padding = 12,
        spacing = 10,
        radius = 12,
        background = "#313244",
        children = {
            icon { name = icon_name, size = 32, foreground = "#CDD6F4", align_v = "Center" },
            column { width = "Fill", align_v = "Center", spacing = 2, children = {
                text { content = title, width = "Fill", font_size = 14, elide = "End" },
                text { content = body, width = "Fill", foreground = "#A6ADC8",
                       wrap = "Word", max_lines = 2, elide = "End" },
            } },
        },
    }
end

return column { width = 340, spacing = 8, children = {
    card("dialog-information-symbolic", "Firmware update ready for the USB-C dock",
        "3 packages can be installed. Restart to finish the kernel upgrade and load the new graphics driver."),
    card("battery-caution-symbolic", "Battery low", "12% left"),
} }
```

The middle column is `"Fill"` so the texts have a bounded width; the icon keeps its 32 px.

## Properties

`text` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `content` | `string\|TextRun[]\|Bound` | `""` | A string, or an array of up to 10000 [runs](#runs), drawn as one paragraph |
| `font` | `string\|Bound` | The `fonts` chain | Family placed before the `fonts` chain. `""` raises; an unknown family falls back to the chain |
| `font_size` | `number\|Bound`, `[1, 8192]` | `12` | Each line is `1.2 × font_size` tall |
| `foreground` | `Color\|Bound` | `"#FFFFFF"` | A [colour](../guide/paint.md#colours); a run's `color` overrides it |
| `text_align` | `"Start"\|"Center"\|"End"\|Bound` | `"Start"` | Aligns lines inside the node's own box; `Start`/`End` follow each line's reading direction. Matters only when the box is wider than the text |
| `wrap` | `"None"\|"Word"\|Bound` | `"None"` | `"Word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"Fill"` or a stretched cross axis) |
| `max_lines` | `number\|Bound` | `0` | Line cap under `wrap = "Word"`; `0` is unlimited, a negative value is refused. Ignored without `wrap` |
| `elide` | `"None"\|"End"\|Bound` | `"None"` | `"End"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line |
| `on_link` | `fun(href: string)` | None | Click on a run with an `href`; the engine never opens it. Takes the click from any ancestor `button`; plain text passes it through |
<!-- End of the generated table. -->

### Runs

Each run in a `content` array is a table:

| Field | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `text` | String | Required | A run without it is refused; an empty one is skipped |
| `bold`, `italic` | Boolean | `false` | Uses the family's bold or italic face when one exists |
| `underline` | Boolean | `false` | Underline in the run's colour |
| `color` | [Colour](../guide/paint.md#colours) | The node's `foreground` | |
| `href` | String | None | Handed to `on_link` on click; the pointer shows `"pointer"` over it. `""` is no link |

A run also takes `kind = "text"`, so a [notification](../capabilities/notifications.md) body's
text spans pass through unchanged. Drop its image spans, which have no `text`. A `nil` hole ends the
array.

```lua
local body = text {
    width = 280,
    wrap = "Word",
    content = {
        { text = "Update ready. " },
        { text = "3 packages", bold = true },
        { text = " can be installed. " },
        { text = "Release notes", underline = true, color = "#89B4FA", href = "https://example.org/notes" },
    },
    on_link = function(href) process.detach("xdg-open", { href }) end,
}
```

### Size

A text node measures its content: one line per paragraph line, `1.2 × font_size` each, as wide as
the widest line. `wrap` and `elide` need a box narrower than the text, so give the node a `width`,
`"Fill"`, or a stretched cross axis (a text in a fixed-width `column` wraps at the column's width).
In a content-sized `row`, the text measures one line and overflows instead.

## How do I…

| Task | Answer |
| :--- | :--- |
| Truncate a long title | `width` (or `"Fill"`) plus `elide = "End"` |
| Show at most two lines | `wrap = "Word"`, `max_lines = 2`, `elide = "End"` and a bounded width, as in the card |
| Bold one word | A [run](#runs) with `bold = true` |
| Make a clickable link | A run with `href` plus `on_link` on the node ([`process.detach`](../guide/processes.md#processdetach) to open it) |
| Use an icon font glyph | `font = "Symbols Nerd Font"` (any installed family) with the glyph as `content` |
| Centre text in a fixed-width box | `text_align = "Center"` with a `width` |
| Show a live value | Bind `content` to a signal: `content = volume:map(function(v) return v and tostring(v) or "" end)` |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A `wrap = "Word"` text runs off the edge on one line | Wrapping needs a bounded width: set `width`, `"Fill"`, or put it in a fixed-width column. A content-sized row offers none |
| `elide = "End"` never ellipsizes | Same cause: the box is as wide as the text. Bound the width |
| `max_lines` has no effect | It applies only under `wrap = "Word"` |
| `text_align = "Center"` does nothing | The box is exactly as wide as the text. Give it a `width`, or centre the node with `align_h` |
| `font = ""` raises | Omit `font` to use the chain |
| A link run is not clickable | Links need `on_link` on the same `text`; without it the click goes to the button around it |
| `content = 42` raises | `content` takes a string or runs: `tostring(n)` |

See also: [textfield](textfield.md), [fonts](../guide/scripting.md#fonts), [icon](icon.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[measure](../../renderer/src/layout/scene/solver.rs), [line height](../../renderer/src/text/shaping/mod.rs),
[link hit testing](../../renderer/src/layout/hit.rs).
