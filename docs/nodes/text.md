# text

One paragraph drawn in the [`fonts`](../guide/scripting.md#fonts) chain or a named family: labels,
clocks, notification bodies. It sizes to its content, wraps and elides inside a bounded width, and
sets weight and italic style for the whole node, and mixes bold, italic, colour and links through
[runs](#runs). For typing, use a [`textfield`](textfield.md).

Two notification cards. In the first, the title elides and the body wraps to two lines, eliding the
second; the second card's short texts fit.

<!-- shot-alt: Two notification cards showing wrapped and truncated text. -->
```lua,shot
local function card(icon_name, title, body)
    return row {
        width = "fill",
        padding = 12,
        spacing = 10,
        radius = 12,
        background = "#313244",
        children = {
            icon { name = icon_name, size = 32, foreground = "#CDD6F4", align_v = "center" },
            column { width = "fill", align_v = "center", spacing = 2, children = {
                text { content = title, width = "fill", font_size = 14, elide = "end" },
                text { content = body, width = "fill", foreground = "#A6ADC8",
                       wrap = "word", max_lines = 2, elide = "end" },
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

The middle column is `"fill"` so the texts have a bounded width; the icon keeps its 32 px.

These node-level styles change line spacing, character spacing, and the selected font face:

<!-- shot-alt: A card comparing line spacing, letter spacing, bold, and italic text. -->
```lua,shot
return column { width = 320, padding = 16, spacing = 10, radius = 12,
    background = "#313244", children = {
    text { content = "Default: One line\nSecond line", font_size = 16 },
    text { content = "Taller lines: One line\nSecond line", font_size = 16,
           line_height = 1.7, foreground = "#A6E3A1" },
    text { content = "Wide letters", font_size = 16, letter_spacing = 3,
           foreground = "#89B4FA" },
    text { content = "Bold italic", font_size = 16, font_weight = 700,
           italic = true, foreground = "#F9E2AF" },
} }
```

## Properties

`text` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `content` | `string\|TextRun[]\|Bound` | `""` | A string, or an array of up to 10000 [runs](#runs), drawn as one paragraph |
| `font` | `string\|Bound` | The `fonts` chain | Family placed before the `fonts` chain. `""` raises; an unknown family falls back to the chain |
| `font_size` | `number\|Bound`, `[1, 8192]` | `12` | Text size in logical pixels |
| `line_height` | `number\|Bound`, `[0.1, 10]` | `1.2` | Line height as a multiple of `font_size` |
| `letter_spacing` | `number\|Bound`, `[-100, 100]` | `0` | Extra space between characters in logical pixels. Negative values tighten text |
| `font_weight` | `number\|Bound`, `[1, 1000]` | `400` | Font weight from 1 to 1000. A run with `bold = true` uses weight 700 |
| `italic` | `boolean\|Bound` | `false` | Use the family's italic face when available. A run with `italic = true` stays italic |
| `foreground` | `Color\|Bound` | `"#FFFFFF"` | A [colour](../guide/paint.md#colours); a run's `color` overrides it |
| `text_align` | `"start"\|"center"\|"end"\|Bound` | `"start"` | Aligns lines inside the node's own box; `"start"`/`"end"` follow each line's reading direction. Matters only when the box is wider than the text |
| `wrap` | `"none"\|"word"\|Bound` | `"none"` | `"word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"fill"` or a stretched cross axis) |
| `max_lines` | `number\|Bound` | `0` | Line cap under `wrap = "word"`; `0` is unlimited, a negative value is refused. Ignored without `wrap` |
| `elide` | `"none"\|"end"\|Bound` | `"none"` | `"end"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line |
| `elided` | `Bound` | None | An `elided(name)` signal; layout writes whether `elide` or `max_lines` removed content |
| `on_link` | `fun(href: string)` | None | Click on a run with an `href`; the engine never opens it. Takes the click from any `on_click`, the text's own included; plain words pass it on |
<!-- End of the generated table. -->

### Runs

Each run in a `content` array is a table:

| Field | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `text` | String | Required | A run without it is refused; an empty one is skipped |
| `bold`, `italic` | Boolean | `false` | `bold` uses weight 700; `italic` enables italic. Other node-level styles carry through |
| `underline` | Boolean | `false` | Underline in the run's colour |
| `color` | [Colour](../guide/paint.md#colours) | The node's `foreground` | |
| `href` | String | None | Handed to `on_link` on click; the pointer shows `"pointer"` over it. `""` is no link |

A run also takes `kind = "text"`, so a [notification](../capabilities/notifications.md) body's
text spans pass through unchanged. Drop its image spans, which have no `text`. A `nil` hole ends the
array.

```lua
local body = text {
    width = 280,
    wrap = "word",
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

A text node measures its content: one line per paragraph line, `line_height × font_size` each, as wide as
the widest line. The default ratio is `1.2`. `letter_spacing` changes both glyph placement and
the width used for wrapping. `wrap` and `elide` need a box narrower than the text, so give the node a `width`,
`"fill"`, or a stretched cross axis (a text in a fixed-width `column` wraps at the column's width).
In a content-sized `row`, the text measures one line and overflows instead.

Bind an [`elided(name)` signal](../guide/signals.md#elided-read-text-truncation) as `elided` to
detect content removed by `elide` or `max_lines`. Wrapping alone keeps it `false`.
Elision cuts at grapheme boundaries, preserving combining marks and emoji sequences.
With `wrap = "word"`, `max_lines = N` and `elide = "end"`, the preview keeps the first N shaped
lines and adds `…` to the last retained line. It shortens that line only as needed to fit the
ellipsis; it never merges content from later lines into the preview.

## How do I…

| Task | Answer |
| :--- | :--- |
| Truncate a long title | `width` (or `"fill"`) plus `elide = "end"` |
| Show at most two lines | `wrap = "word"`, `max_lines = 2`, `elide = "end"` and a bounded width, as in the card |
| Bold one word | A [run](#runs) with `bold = true` |
| Make a clickable link | A run with `href` plus `on_link` on the node ([`process.detach`](../guide/processes.md#processdetach) to open it) |
| Use an icon font glyph | `font = "Symbols Nerd Font"` (any installed family) with the glyph as `content` |
| Centre text in a fixed-width box | `text_align = "center"` with a `width` |
| Show a live value | Bind `content` to a signal: `content = volume:map(function(v) return v and tostring(v) or "" end)` |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A `wrap = "word"` text runs off the edge on one line | Wrapping needs a bounded width: set `width`, `"fill"`, or put it in a fixed-width column. A content-sized row offers none |
| `elide = "end"` never ellipsizes | Same cause: the box is as wide as the text. Bound the width |
| `max_lines` has no effect | It applies only under `wrap = "word"` |
| `text_align = "center"` does nothing | The box is exactly as wide as the text. Give it a `width`, or centre the node with `align_h` |
| `font = ""` raises | Omit `font` to use the chain |
| A link run is not clickable | Links need `on_link` on the same `text`; without it the click goes to the `on_click` around it |
| `content = 42` raises | `content` takes a string or runs: `tostring(n)` |

See also: [textfield](textfield.md), [fonts](../guide/scripting.md#fonts), [icon](icon.md).

Source: [vocabulary](../../renderer/src/lua/nodes/properties.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[measure](../../renderer/src/layout/scene/solver.rs), [line height](../../renderer/src/text/shaping/mod.rs),
[link hit testing](../../renderer/src/layout/hit.rs).
