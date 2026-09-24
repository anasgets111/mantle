# textfield

A single-line text input: a search box, a launcher query, a password. The engine holds what the user
types (the *draft*); Lua sees it only through callbacks and cannot set it. Focus, editing keys, the
draft's lifetime and password fields are on [input](../guide/input.md#text-fields).

A launcher: the field filters a list as the user types, the arrow keys move a selection, Enter
launches.

```lua
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
    textfield {
        width = "Fill",
        height = 32,
        font_size = 14,
        placeholder = "Search…",
        autofocus = true,
        on_change = function(text) query:set(text); selected:set(1) end,
        on_navigate = function(key)
            if key == "down" then selected:set(selected:get() + 1)
            elseif key == "up" then selected:set(math.max(1, selected:get() - 1)) end
        end,
        on_submit = function() print("launch", matches:get()[selected:get()]) end,
    },
    list {
        width = "Fill",
        source = matches,
        key = function(name) return name end,
        itemfn = function(name)
            return text { content = name, width = "Fill", padding = 6 }
        end,
    },
} }

return { panel { id = "launcher", layer = "Top", anchor = { top = true },
    keyboard_interactivity = "OnDemand", child = launcher } }
```

The panel needs `keyboard_interactivity` for the field to get keys ([panel](../surfaces/panel.md)).

## Properties

`textfield` takes the [common properties](index.md#common-properties), plus:

<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
| Property | Type | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `placeholder` | `string\|Bound` | `""` | Shown while the field is empty, focused or not. Never submitted |
| `font_size` | `number\|Bound`, `[1, 8192]` | `12` | Size of the text and placeholder |
| `foreground` | `Color\|Bound` | `"#FFFFFF"` | Colour of the text and placeholder |
| `text_align` | `"Start"\|"Center"\|"End"\|Bound` | `"Start"` | Aligns the text inside the field's box |
| `autofocus` | `boolean\|Bound` | `false` | Plain fields only: take the keyboard, empty, when the surface gets it or the field appears, calling `on_change("")`. The first in document order wins; never steals from a field already typing or one a press just left |
| `on_change` | `fun(text: string)` | None | Full text after every edit |
| `on_submit` | `fun(text: string)` | None | Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field |
| `on_cancel` | `fun(cleared: boolean)` | None | Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus |
| `on_navigate` | `fun(key: "up"\|"down"\|"left"\|"right"\|"page_up"\|"page_down"\|"tab"\|"backtab")` | None | Keys a single-line field does not use, for moving a list selection; repeats while held. `"left"`/`"right"` only when the caret cannot move that way and Shift is up |
| `secure_submit` | `{ capability: string, action: string }\|Bound` | None | Makes the field masked; keys never reach Lua. Both non-empty UTF-8 strings: only `lock`/`authenticate`, `polkit`/`authenticate` and `network`/`connect`; any other pair or key is an error ([secure fields](../guide/input.md#secure-fields)) |
| `mask_character` | `string\|Bound` | `"•"` | Drawn per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length |
<!-- End of the generated table. -->

The field has no intrinsic size, so give it `width` and `height`. The text is vertically centred in
the box and drawn in the [`fonts`](../guide/scripting.md#fonts) chain (there is no `font` property).
It reads `wl_keyboard`, not an input method, so there is no CJK composition and no dead keys.

A field with none of `on_change`, `on_submit` and `secure_submit` never takes focus. Give a field a
stable `id` when siblings before it come and go, or its draft may attach to another node
([identity](index.md#identity-and-reconciliation)).

## How do I…

| Task | Answer |
| :--- | :--- |
| Filter a list as the user types | The example above: `on_change` sets a state, the list's `source` maps it |
| Move a selection with the arrow keys | `on_navigate`, as above; pair it with `scroll(name):reveal` to keep the row in view ([input](../guide/input.md#text-fields)) |
| Focus the field when a panel opens | `autofocus = true` and a panel with `keyboard_interactivity` |
| Close on a second Escape | `on_cancel(cleared)`: close only when `cleared` is `false` |
| Ask for a password | `secure_submit = { capability = "lock", action = "authenticate" }` ([secure fields](../guide/input.md#secure-fields)) |
| Submit a password from a button | A [`button`](button.md) with `submit = true` |
| Style the box around the field | Wrap it in a `rect` with `background`, `radius` and `border_*`; the field draws only text and caret |
| Debounce a search | [signals](../guide/signals.md#debounce-a-search) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| The field does not appear | It has no intrinsic size. Give `width` and `height` |
| Typing does nothing | The surface needs keyboard focus (`keyboard_interactivity` on a panel), and the field needs `on_change`, `on_submit` or `secure_submit` |
| `on_cancel` or `on_navigate` alone never fires | Neither makes the field focusable. Add `on_change` or `on_submit` |
| You cannot set or clear the draft from Lua | The draft is the engine's. Enter and Escape clear it; removing the node drops it |
| `on_submit` never fires on a password field | A `secure_submit` field sends to its capability instead |
| `font` on a `textfield` is refused | Fields use the `fonts` chain |
| `background` on a `textfield` is refused | It is not a box. Wrap it in a `rect` |

See also: [input](../guide/input.md), [list](list.md), [text](text.md), [surfaces: panel](../surfaces/panel.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [content parsers](../../renderer/src/layout/node/content.rs),
[secure_submit](../../renderer/src/layout/node/spec.rs), [plain fields](../../renderer/src/wayland/input/keyboard/plain.rs),
[focus](../../renderer/src/wayland/input/keyboard/mod.rs).
