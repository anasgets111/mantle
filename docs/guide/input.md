# Input

Pointer and keyboard input: clicks, drags and the wheel on a `button`, hover, scrolling
containers, and typing into a `textfield`, including password fields whose keys never reach Lua.
There is no key-handler property and no touch input; keys reach a config only through a focused
`textfield`. A handler usually writes a [named state](signals.md#named-state), and the next
[pass](signals.md#how-re-resolution-works) shows the result.

```lua
local clicks = state("clicks", 0)

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = button {
        padding = 8,
        background = "#313244",
        on_click = function(rect, which)
            if which == "left" then clicks:set(clicks:get() + 1) end
        end,
        children = { text { content = clicks:map(function(n) return "Clicked " .. n end) } },
    },
}
```

## Hit testing

Every pointer event asks which nodes lie under the pointer, from the surface down.

| Rule | Detail |
| :--- | :--- |
| Transforms | A node is hit where it is painted, after `scale`, `rotate` and `translate` |
| Stacking | Siblings are asked topmost first: higher `z`, then later in declaration order |
| Clipping | A point outside a node reaches none of its children, unless the node has `clip = "None"` |
| Skipped | `visible = false` subtrees and nodes playing an [exit](animation.md#exit). `opacity = 0` is still hit |
| Edges | Half-open: two buttons sharing an edge never both take it |
| Rects | Every `rect` argument and `hover_rect` value is the node's surface-local `{ x, y, width, height }` laid-out box, before transforms |

## Pointer

Only a `button` takes clicks, drags and the wheel. For each event the innermost button with a
handler for that event wins; a button without one is transparent, so a handle inside a draggable
track leaves the track draggable.

| Handler | Arguments | Contract |
| :--- | :--- | :--- |
| `on_click(rect, button)` | `button` is `"left"`, `"right"` or `"middle"` | Fires on release over the same button that was pressed, with the same mouse button. Other mouse buttons are ignored |
| `on_drag(rect, pointer, phase)` | `pointer` is `{ x, y }` relative to the button, unclamped; `phase` is `"start"`, `"move"` or `"end"` | Left button only. See below |
| `on_wheel(rect, steps)` | `steps` is a number of wheel notches | Vertical wheel only. See below |
| `submit = true` | — | Sends the armed [secure field](#secure-fields) on click, like Enter; works without `on_click` and runs before it |

**Click.** A press arms the click and the release fires it. Leaving the button and coming back
before release still clicks; the pointer leaving the surface cancels. The click also cancels if the
button's laid-out box moved between press and release, so give press feedback with `scale` or
`translate` rather than `width` or `margin`. A press on a `textfield` never clicks the button
around it, and a link in a `text` (`on_link`) takes the click before any button around it.

**Drag.** A left press on an `on_drag` button calls `"start"` at once, so clicking a slider track
also seeks. Every pointer motion on that surface then calls `"move"`, wherever the pointer is.
`"end"` comes on the left release, when the pointer leaves the surface, or when the surface closes.
`rect` stays the box from the press for the whole drag. On release, `"end"` fires first and the
click (if the button also has `on_click`) after it; a leave ends the drag and cancels the click.

**Wheel.** `steps` is positive away from the user (scroll up) and negative toward. One notch is
`1`; high-resolution wheels send fractions of a notch, and touchpads send distance divided by one
notch's 39 px. Horizontal motion never reaches `on_wheel`. The innermost `on_wheel` button or
[scroll container](#scroll) under the pointer takes the whole event, with no chaining to a parent.

```lua,shot
local level = state("level", 0.5)
local function clamp(value) return math.max(0, math.min(1, value)) end

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = button {
        width = 200,
        height = 12,
        radius = 6,
        clip = "Rounded",
        background = "#45475a",
        -- A press is "start", so clicking the track also seeks.
        on_drag = function(rect, pointer, phase) level:set(clamp(pointer.x / rect.width)) end,
        on_wheel = function(_, steps) level:set(clamp(level:get() + steps * 0.05)) end,
        children = {
            rect {
                height = "Fill",
                background = "#89b4fa",
                width = level:map(function(value) return string.format("%d%%", math.floor(value * 100 + 0.5)) end),
            },
        },
    },
}
```

## Hover

`hover(name)` returns a read-only boolean signal, `false` until the pointer first arrives. Bind it
to a node's `hover` and the engine writes it as the pointer moves; read the same signal anywhere
else to react. The name is the identity: every `hover("wifi")` call returns the same signal, and it
survives reloads.

| API | Contract |
| :--- | :--- |
| `hover = hover(name)` | Any node kind. `true` while the pointer is over the node or any of its children (hit-tested, so clipping and stacking apply). Pointer leaving the surface, or the surface closing, turns every hover off. When layout moves nodes under a still pointer, hover follows |
| `on_hover(inside)` | Called with `true`/`false` on each crossing caused by the pointer. Layout moving nodes under a still pointer updates `hover` but does not call it. Refused unless the same node has `hover` |
| `hover_rect(name)` | Read-only signal of the node's rect from the last time its hover turned on. It keeps that rect after the pointer leaves, reads `{ x = 0, y = 0, width = 1, height = 1 }` before the first hover, and is updated before `on_hover` runs. Use it as a tooltip `popup`'s `anchor_rect` |
| `cursor` | The pointer shape over a node: one of the [cursor names](../nodes/index.md#cursor-names), defaults in [common properties](../nodes/index.md#common-properties). The innermost node that sets one wins, and an explicit one beats a kind's default |

```lua
local over = hover("wifi")

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = button {
        padding = 6,
        radius = 6,
        hover = over,
        background = over:map(function(on) return on and "#45475a" or "#313244" end),
        on_hover = function(inside) log.debug("wifi hovered:", inside) end,
        on_click = function() process.detach("nm-connection-editor", {}) end,
        children = { icon { name = "network-wireless-symbolic", size = 16 } },
    },
}
```

## Scroll

`scroll(name)` returns a read-only signal holding a scroll offset in px, `0` at first. Bind it to
the `scroll` property of a `row`, `column` or `list` and the wheel moves that container's children.
Like `hover`, the name is the identity and survives reloads.

| Rule | Detail |
| :--- | :--- |
| Axis | A `column` or vertical `list` scrolls with the vertical wheel, a `row` or horizontal `list` with the horizontal one only |
| Distance | One wheel notch is 39 px; a touchpad scrolls the distance it reports |
| Bound | Layout clamps the offset to `[0, content − viewport]` and writes the clamped value back. The container needs a bounded size on its axis (fixed, `"Fill"` or `max_*`); one sized by its content has nothing to scroll |
| Cost | While only `scroll` properties read the signal, the wheel moves the laid-out children without a layout pass. A `map` or `:get()` of it, or a `scroll` inside a `list` item, costs a pass per wheel event |
| `:reveal(index)` | On the next pass, scrolls the least distance that shows the `index`-th visible child (1-based; a `list`'s items in source order). An index past the end does nothing; below 1 raises. Only a `scroll` signal has it |

## Text fields

A `textfield` is a single-line text input. The engine holds what the user types (the *draft*); Lua
sees it only through callbacks and cannot set it. The field with *focus* is the one keys go to. It
has no size of its own, so give it `width` and `height` ([nodes](../nodes/textfield.md)).

A field takes the keyboard only when both hold:

| Condition | Detail |
| :--- | :--- |
| The surface has keyboard focus | A `panel` needs `keyboard_interactivity = "OnDemand"` or `"Exclusive"` ([keyboard focus](../surfaces/panel.md#keyboard-focus)); a popup shown under the focused surface shares its keys |
| The field can use keys | It has `secure_submit`, `on_change` or `on_submit`. A field with none of them (even with `on_cancel` or `on_navigate`) never takes focus, and a press on it acts like a press on empty space |

A press on the field focuses it and puts the caret under the pointer. `autofocus` focuses it without
a press.

| Property | Contract |
| :--- | :--- |
| `on_change(text)` | Every edit that changes the text, with the whole draft. Caret moves call nothing |
| `on_submit(text)` | Enter, with the whole draft (possibly `""`). The draft then clears and `on_change("")` follows; the field keeps focus. A held Enter does not repeat |
| `on_cancel(cleared)` | Escape. The draft clears, the field drops focus, `on_change("")` fires if there was text, then `on_cancel` gets whether text was removed. Without `on_cancel`, Escape clears and the field keeps focus |
| `on_navigate(key)` | `"up"`, `"down"`, `"page_up"`, `"page_down"`, `"tab"`, `"backtab"`, and `"left"`/`"right"` when the caret cannot move that way and Shift is up. Repeats while held. The draft is untouched |
| `autofocus` | `true`: take the keys, with an empty draft and a call to `on_change("")`, when the surface gains keyboard focus or the field appears under it. The first visible such field in document order wins. It never takes over from a field that is already typing, and never re-takes a field the user just clicked away from |
| `secure_submit`, `mask_character` | See [secure fields](#secure-fields) |
| `placeholder`, `font_size`, `foreground`, `text_align` | Appearance; see [textfield](../nodes/textfield.md) |

| Key | Plain field | Secure field |
| :--- | :--- | :--- |
| Text | Inserts at the caret, replacing a selection; `on_change` | Appends |
| Enter | `on_submit`, then clears | Sends |
| Escape | Clears; with `on_cancel`, also drops focus | Clears, stays armed, `on_cancel` |
| Backspace, Delete | One character, or the selection | Backspace only |
| Ctrl+Backspace, Ctrl+Delete | One word | Nothing |
| Left, Right, Home, End | Move the caret; Ctrl+Left/Right by word; Shift selects. Left/Right with nowhere to go (and no Shift) call `on_navigate` | Nothing |
| Ctrl+A | Selects all | Nothing |
| Up, Down, Page Up, Page Down, Tab, Shift+Tab | `on_navigate` (`"backtab"` for Shift+Tab) | Nothing |
| Any other Ctrl chord | Left to the compositor | Same |

**Selection and clipboard.** Dragging or Shift+clicking with the pointer selects too. There is no
clipboard in a field, and Tab does not move focus between fields; it only reaches `on_navigate`.
Every key but Enter repeats while held.

**Draft lifetime.** Clicking elsewhere, or the surface losing the keyboard, stops typing but keeps
the draft; clicking the field again resumes it. Enter and Escape clear it. An `autofocus` arm
starts it empty. It is dropped when the field's node leaves the tree or its surface closes.

```lua
local APPS = { "firefox", "foot", "nautilus", "pavucontrol", "thunderbird", "zed" }
local query = state("query", "")
local selected = state("selected", 1)
local LIST = scroll("results")
local open = state("launcher_open", true)

local results = query:map(function(needle)
    local found = {}
    for _, app in ipairs(APPS) do
        if fuzzy(app, needle) then found[#found + 1] = app end
    end
    return found
end)

local function pick(index)
    selected:set(index)
    LIST:reveal(index)
end

local search = textfield {
    width = "Fill",
    height = 32,
    placeholder = "Search apps",
    autofocus = true,
    on_change = function(text)
        query:set(text)
        pick(1)
    end,
    on_submit = function()
        local app = results:get()[selected:get()]
        if app then process.detach(app, {}) end
    end,
    -- First Escape clears the text; a second one, on an empty field, closes.
    on_cancel = function(cleared)
        if not cleared then open:set(false) end
    end,
    on_navigate = function(key)
        local step = ({ up = -1, down = 1 })[key]
        if step then pick(math.max(1, math.min(#results:get(), selected:get() + step))) end
    end,
}

local function app_row(app)
    return row {
        width = "Fill",
        padding = 6,
        background = computed({ results, selected }, function(found, index)
            return found[index] == app and "#45475a" or nil
        end),
        children = { text { content = app } },
    }
end

return panel {
    id = "launcher",
    layer = "Overlay",
    keyboard_interactivity = "OnDemand",
    width = 320,
    visible = open,
    child = column {
        width = "Fill",
        children = {
            search,
            list { width = "Fill", height = 120, scroll = LIST, source = results, itemfn = app_row, key = function(app) return app end },
        },
    },
}
```

## Secure fields

`secure_submit = { capability, action }` turns a `textfield` into a password field. Its keys go to
a native buffer and leave the Renderer as one message to that target; no Lua value ever holds the
secret or its length.

| Target | Effect |
| :--- | :--- |
| `{ capability = "lock", action = "authenticate" }` | Checked by PAM; success unlocks the session. A `lock` surface needs exactly one reachable field with this target ([lock](../surfaces/lock.md)) |
| `{ capability = "polkit", action = "authenticate" }` | Answers the current polkit request ([polkit](../capabilities/polkit.md)) |
| `{ capability = "network", action = "connect" }` | The password for the network being joined ([network](../capabilities/network.md)) |
| Any other pair | Refused when the field is laid out, so no password is typed into nowhere |

| Rule | Detail |
| :--- | :--- |
| Arming | When the surface gains keyboard focus, the sole visible secure field in it (and in popups shown under it) is armed with no click. With two or more, a press picks one. A field revealed later under existing focus arms if none is armed |
| Keys | Typed text appends, Backspace removes one character, Escape clears the buffer, stays armed and calls the field's `on_cancel(cleared)`. There is no caret, selection or `on_navigate`; `on_change` and `on_submit` never fire |
| Sending | Enter, or a click on a `submit = true` button, sends the buffer and wipes it. An empty buffer is sent only to `network`/`connect`, where it joins an open network |
| Focus | A click on anything but a field keeps the field armed, so a submit button works. Focusing another field, plain or secure, or the keyboard leaving the surface, disarms it and wipes the buffer |
| Priority | While a secure field is armed, plain fields in the same focus take no keys |
| `mask_character` | Drawn once per typed character. Default `"•"`; only the first character counts; `""` draws nothing and hides the length. Only secure fields draw it. An empty field shows its `placeholder` |

```lua
return lock {
    id = "lock",
    child = column {
        width = "Fill",
        height = "Fill",
        align_h = "Center",
        align_v = "Center",
        spacing = 12,
        background = "#11111b",
        children = {
            textfield {
                width = 280,
                height = 40,
                placeholder = "Password",
                mask_character = "•",
                secure_submit = { capability = "lock", action = "authenticate" },
            },
            button {
                padding = 8,
                radius = 8,
                background = "#89b4fa",
                submit = true,
                children = { text { content = "Unlock", foreground = "#11111b" } },
            },
        },
    },
}
```

## How do I…

| Task | Answer |
| :--- | :--- |
| Make a slider | The example under [pointer](#pointer) |
| Move a selection through a list with the arrow keys | The launcher under [text fields](#text-fields): `on_navigate` plus `scroll(name):reveal` |
| Close a search box on a second Escape | The same launcher: `on_cancel(cleared)` closes only when `cleared` is `false` |
| Ask for a password | The lock example under [secure fields](#secure-fields) |
| Show a tooltip on hover | [Tooltip](../surfaces/popup.md), with `hover_rect` as the anchor |
| Open a menu on right click | Below |
| Reorder a list by dragging | Below |

**Right-click menu.** `on_click` reports the mouse button and the button's rect, which is what a
[popup](../surfaces/popup.md)'s `anchor_rect` wants. The click is a pointer release, so the popup may
take its grab. The menu hangs from the button, not the click point: no handler reports the pointer
position of a click.

```lua,shot
local menu_open = state("context_open", false)
local menu_at = state("context_at", { x = 0, y = 0, width = 1, height = 1 })

local function item(label, run)
    return button {
        width = "Fill",
        padding = 6,
        radius = 4,
        on_click = function()
            menu_open:set(false)
            run()
        end,
        children = { text { content = label } },
    }
end

local files = button {
    padding = 8,
    on_click = function(rect, which)
        if which == "right" then
            menu_at:set(rect)
            menu_open:set(true)
        else
            process.detach("nautilus", {})
        end
    end,
    children = { text { content = "Files" } },
}

return {
    panel { id = "bar", layer = "Top", anchor = { top = true }, child = files },
    popup {
        id = "files_menu",
        parent = "bar",
        anchor_rect = menu_at,
        anchor = "Bottom",
        gravity = "Bottom",
        visible = menu_open,
        on_dismiss = function() menu_open:set(false) end,
        child = column {
            width = 140,
            padding = 4,
            background = "#1e1e2e",
            children = {
                item("New window", function() process.detach("nautilus", { "--new-window" }) end),
                item("Downloads", function() process.detach("nautilus", { os.getenv("HOME") .. "/Downloads" }) end),
            },
        },
    },
}
```

**Drag to reorder.** Each row is an `on_drag` button in a keyed [`list`](../nodes/list.md). The drag
keeps the row's box from the press, so `pointer.y` divided by the row pitch counts the rows moved.
The dragged row jumps slot by slot rather than following the pointer; to make it follow, also bind
its `translate` to the drag offset. There is no drag-and-drop between surfaces or applications, and
no drag image.

```lua
local items = state("order", { "Music", "Mail", "Files", "Terminal" })
local STEP = 32 -- row height 28 + spacing 4
local started_at = 1

local function index_of(name)
    for index, other in ipairs(items:get()) do
        if other == name then return index end
    end
end

local function row_for(name)
    return button {
        width = 200,
        height = 28,
        padding = 6,
        background = "#313244",
        cursor = "grab",
        on_drag = function(_, pointer, phase)
            if phase == "start" then
                started_at = index_of(name)
                return
            end
            -- `pointer` is relative to the row's box at the press, so the offset counts rows moved.
            local order = { table.unpack(items:get()) }
            local target = math.max(1, math.min(#order, started_at + math.floor(pointer.y / STEP)))
            local now = index_of(name)
            if target ~= now then
                table.insert(order, target, table.remove(order, now))
                items:set(order)
            end
        end,
        children = { text { content = name } },
    }
end

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = list { spacing = 4, source = items, itemfn = row_for, key = function(name) return name end },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A `textfield` is invisible or cannot be clicked | It has no intrinsic size. Give it `width` and `height` |
| A field in a panel shows no caret and takes no keys | Set the panel's `keyboard_interactivity` to `"OnDemand"` (or `"Exclusive"` for a modal) |
| A field with only `on_navigate`/`on_cancel` ignores clicks | Add `on_change` or `on_submit` |
| The mouse wheel does nothing over a scrolling `row` | Rows scroll on the horizontal axis. Use a `column`, or an `on_wheel` button that moves the row |
| A `scroll` container never scrolls | Bound its size on the scroll axis; content-sized means nothing overflows |
| `on_hover` is refused | Add `hover = hover("name")` on the same node |
| A container's hover stays on while the pointer is over a child | Hover covers the whole subtree. Give the child its own `hover` for innermost-only behaviour |
| A click is lost when the button grows on press | The release must land on the same laid-out box. Animate `scale` instead |
| A tooltip or menu anchored to a scaled button is off | Rects are laid-out boxes before transforms. Anchor on an untransformed parent |
| Lua needs to prefill or clear a field | Not possible: the draft belongs to the engine. `autofocus` re-arms empty; Escape and Enter clear |
| A typed password shows up in `on_change` | It cannot: a `secure_submit` field never calls it. Plain fields also stop taking keys while a secure field is armed |

See also: [nodes](../nodes/index.md) ([`button`](../nodes/button.md), [`textfield`](../nodes/textfield.md), [`list`](../nodes/list.md)), [surfaces](../surfaces/index.md)
(`keyboard_interactivity`, popups, lock), [signals](signals.md) (state the handlers write),
[animation](animation.md) (press and hover motion), [capabilities](../capabilities/index.md) ([`lock`](../capabilities/lock.md),
[`polkit`](../capabilities/polkit.md), [`network`](../capabilities/network.md)).

Source: [pointer](../../renderer/src/wayland/input/pointer/mod.rs), [wheel](../../renderer/src/wayland/input/pointer/wheel.rs),
[keyboard](../../renderer/src/wayland/input/keyboard/mod.rs), [text fields](../../renderer/src/wayland/input/keyboard/plain.rs),
[secure fields](../../renderer/src/wayland/input/keyboard/secure.rs), [hit testing](../../renderer/src/layout/hit.rs),
[hover](../../renderer/src/layout/hover.rs), [scroll](../../renderer/src/layout/scene/scroll.rs).
