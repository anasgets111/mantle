# textfield

A single-line text input: a search box, a launcher query, a password. The engine holds what the user
types (the *draft*); Lua sees it through callbacks and sets it only with
[`focus_target(name):set_text`](../guide/input.md#text-fields). Focus, editing keys, the draft's
lifetime and password fields are on [input](../guide/input.md#text-fields).

A launcher: the field filters a list as the user types, the arrow keys move a selection, Enter
launches.

<!-- shot-alt: A search field with a list of matching apps below it. -->
```lua,shot
local apps = { "Firefox", "Files", "Terminal", "Text Editor", "Settings" }
local query = state("query", "")
local selected = state("selected", 1)

local matches = query:map(function(q)
    local out = {}
    for _, name in ipairs(apps) do
        if name:lower():find((q or ""):lower(), 1, true) then out[#out + 1] = name end
    end
    return out
end)

local launcher = column { width = 320, padding = 12, spacing = 8, background = "#1E1E2E", radius = 12, children = {
    rect { width = "fill", padding = { left = 10, right = 10 }, radius = 8, background = "#313244", children = {
        textfield {
            width = "fill",
            height = 36,
            font_size = 14,
            foreground = "#CDD6F4",
            placeholder = "Search…",
            autofocus = true,
            on_change = function(text) query:set(text); selected:set(1) end,
            on_key = function(key)
                if key.name == "Down" then selected:set(math.min(#matches:get(), selected:get() + 1))
                elseif key.name == "Up" then selected:set(math.max(1, selected:get() - 1)) end
            end,
            on_submit = function() print("launch", matches:get()[selected:get()]) end,
        },
    } },
    list {
        width = "fill",
        source = matches,
        key = function(name) return name end,
        itemfn = function(name)
            return rect {
                width = "fill", padding = { left = 10, right = 10, top = 6, bottom = 6 }, radius = 8,
                background = computed({ matches, selected }, function(found, index)
                    return found[index] == name and "#45475A" or "#00000000"
                end),
                children = { text { content = name, foreground = "#CDD6F4" } },
            }
        end,
    },
} }

return { panel { id = "launcher", layer = "top", anchor = { top = true },
    keyboard_interactivity = "on_demand", child = launcher } }
```

The image shows the empty search field and unfiltered list. Typing updates the list through
`on_change`. The panel needs `keyboard_interactivity` for the field to get keys
([panel](../surfaces/panel.md)).

## Properties

`textfield` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `font` | `string\|Bound` | The `fonts` chain | Family placed before the `fonts` chain. `""` raises; an unknown family falls back to the chain |
| `font_size` | `number\|Bound`, `[1, 8192]` | `12` | Text size in logical pixels. A `textfield` sizes its placeholder with it |
| `line_height` | `number\|Bound`, `[0.1, 10]` | `1.2` | Line height as a multiple of `font_size` |
| `letter_spacing` | `number\|Bound`, `[-100, 100]` | `0` | Extra space between characters in logical pixels. Negative values tighten text |
| `font_weight` | `number\|Bound`, `[1, 1000]` | `400` | Font weight from 1 to 1000. A run with `bold = true` uses weight 700 |
| `italic` | `boolean\|Bound` | `false` | Use the family's italic face when available. A run with `italic = true` stays italic |
| `font_variations` | `table<string, number>\|Bound` | `{}` | OpenType variation axes by 4-character tag (`{ FILL = 1, GRAD = -25, opsz = 24 }`), as CSS `font-variation-settings`. Values clamp to each face's range; axes a face lacks are ignored. An explicit `wght` overrides `font_weight` and bold runs. Changes snap; see [variable fonts](text.md#variable-fonts) |
| `foreground` | `Color\|Bound` | `"#FFFFFF"` | A [colour](../guide/paint.md#colours); a run's `color` overrides it, and a `textfield`'s placeholder takes it unless `placeholder_color` is set |
| `text_align` | `"start"\|"center"\|"end"\|Bound` | `"start"` | Aligns lines inside the node's own box; `"start"`/`"end"` follow each line's reading direction. Matters only when the box is wider than the text |
| `focus_target` | `FocusHandle` | None | A `focus_target(name)` handle. An `on_click` can call `:request()` to return keys after its state change; the field must be visible on that click's keyboard-focused surface or a popup under it. Any other value fails the pass |
| `placeholder` | `string\|Bound` | `""` | Shown while the field is empty, focused or not. Never submitted |
| `placeholder_color` | `Color\|Bound` | `foreground` | Colour of the placeholder |
| `caret` | `Caret\|Bound` | None | The caret bar: `{ color, width, height, radius }`. `color` defaults to `foreground`; the selection highlight keeps `foreground`. `width` is px, default a sixteenth of `font_size` rounded, at least `1`. `height` is px, or a fraction of the line height when `1` or less; default the whole line, centred on it. `radius` is px, default `0`. Each key takes a signal. Paint only: `animate` snaps it |
| `disabled` | `boolean\|Bound` | `false` | Renders like a field but takes no keyboard focus (Tab skips it, a press does not focus it, `focus_target` requests and `autofocus` pass over it) and draws no caret; `set_text` still reaches it. A focused field that becomes disabled loses focus and keeps its draft. Dim it yourself by binding colours to the same signal |
| `max_length` | `number\|Bound` | `0` | Most grapheme clusters the field holds; `0` is unlimited and a negative value is refused. Typing, paste, IME commits and `focus_target(name):set_text(text)` cut what they insert at the limit, secure fields included. Lowering it below the current text keeps that text; edits can then only shorten it. The cut is silent, so a limit below a password's length truncates it |
| `initial_text` | `string\|Bound` | `""` | Plain fields only: seeds the draft once, when the field enters the tree (a new node: a changed `id` or `key` counts as new), with the value at that moment, read without subscribing: writing the signal alone does not re-resolve the field. Later changes are ignored and an emptied field stays empty; `set_text` pushes new text. Like `set_text`: cut at `max_length`, caret at the end, no undo history, no `on_change`; hidden and disabled fields are seeded too. Refused with `secure_submit`, control characters and over 64 KiB |
| `autofocus` | `boolean\|Bound` | `false` | Plain fields only: take the keyboard, with the draft reset to `initial_text` (`""` when unset) and `on_change` called with it, when the surface gets it or the field appears. The first in document order wins; never steals from a field already typing or one a press just left |
| `on_change` | `fun(text: string)` | None | Full text after every edit |
| `on_submit` | `fun(text: string)` | None | Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field |
| `escape` | `"clear"\|"blur"\|"pass"\|Bound` | `"clear"` | What Escape does in a plain field. `"clear"` empties the draft (`on_change("")` if it had text), then gives up focus if `on_cancel` is set. `"blur"` keeps the draft and gives up focus. `"pass"` keeps both and does not take the key: it goes up through `on_key`, then to the surface's `on_escape`. A `secure_submit` field ignores it and always scrubs and stays armed. Read when the field takes focus |
| `on_cancel` | `fun(cleared: boolean)` | None | Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus |
| `secure_submit` | `{ capability: string, action: string, name?: string }\|Bound` | None | Makes the field masked; bytes never reach Lua. Targets: `lock`/`authenticate`, `polkit`/`authenticate`, `network`/`connect`, `network`/`vpn_secret` with a request id and key in `name`, `secrets`/`store` with a public `name`, or `bluetooth`/`pair` with a request id and MAC in `name` ([secure fields](../guide/input.md#secure-fields)) |
| `mask_character` | `string\|Bound` | `"•"` | Drawn per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length |
<!-- End of the generated table. -->

The field has no intrinsic width, so give it `width`; without `height` it is one line
tall (`font_size` times `line_height`). It draws one line of text and a caret, vertically centred,
with the same typography properties as [`text`](text.md), the draft, placeholder and mask alike, in the
[`fonts`](../guide/scripting.md#fonts) chain unless `font` names a family. Plain fields use
`zwp_text_input_v3` for composition when the compositor offers it and text-input enters the field's
own surface. Raw keys stay active between compositions and are suppressed during pending or active
composition. Preedit text is underlined; commits and surrounding deletions call `on_change`. Secure
fields read `wl_keyboard` and never send their
draft to an input method.

A field with none of `on_change`, `on_submit` and `secure_submit` never takes focus. The draft
follows the node, so give the field a stable `id` when siblings before it come and go
([identity](index.md#identity-and-reconciliation)).

## How do I…

| Task | Answer |
| :--- | :--- |
| Filter a list as the user types | The example above: `on_change` sets a state, the list's `source` maps it |
| Move a selection with the arrow keys | `on_key` on the field, as above; pair it with `scroll(name):reveal` to keep the row in view ([input](../guide/input.md#text-fields)) |
| Focus the field when a panel opens | `autofocus = true` and a panel with `keyboard_interactivity` |
| Return keys to the field after a click | One `local h = focus_target("name")`: `focus_target = h` on the field, `h:request()` in the `on_click` ([input](../guide/input.md#text-fields)) |
| Close on a second Escape | `on_cancel(cleared)`: close only when `cleared` is `false` |
| Ask for a password | `secure_submit = { capability = "lock", action = "authenticate" }` ([secure fields](../guide/input.md#secure-fields)) |
| Submit a password from a button | `submit = true` on the clickable node |
| Style the box around the field | Wrap it in a `rect` with `background`, `radius` and `border_*`; the field draws only text and caret |
| Debounce a search | [signals](../guide/signals.md#debounce-a-search) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| The field does not appear | It has no intrinsic width. Give it `width` |
| Typing does nothing | The surface needs keyboard focus (`keyboard_interactivity` on a panel), and the field needs `on_change`, `on_submit` or `secure_submit` |
| `on_cancel` alone never fires | It does not make the field focusable. Add `on_change` or `on_submit` |
| You cannot read the draft from Lua | It arrives only through `on_change` and `on_submit`. `focus_target(name):set_text` writes it; Enter and Escape clear it; removing the node drops it |
| `on_submit` never fires on a password field | A `secure_submit` field sends to its capability instead |
| `font` on a `textfield` is refused | Fields use the `fonts` chain |
| `background` on a `textfield` is refused | It is not a box. Wrap it in a `rect` |

See also: [input](../guide/input.md), [list](list.md), [text](text.md), [surfaces: panel](../surfaces/panel.md).

Source: [vocabulary](../../renderer/src/lua/nodes/properties.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[secure_submit](../../renderer/src/layout/node/spec.rs), [plain fields](../../renderer/src/wayland/input/keyboard/plain.rs),
[focus](../../renderer/src/wayland/input/keyboard/mod.rs).
