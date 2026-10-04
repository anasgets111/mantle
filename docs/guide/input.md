# Input

Pointer and keyboard input: clicks, drags and the wheel on any node, hover, scrolling
containers, keyboard activation, and typing into a `textfield`, including password fields whose
keys never reach Lua. A handler usually writes a [named state](signals.md#named-state), and the next
[pass](signals.md#how-re-resolution-works) shows the result.

```lua
local clicks = state("clicks", 0)

return panel {
    id = "bar",
    layer = "top",
    keyboard_interactivity = "on_demand",
    anchor = { top = true },
    child = rect {
        padding = 8,
        background = "#313244",
        accessible_name = "Increase clicks",
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
| Clipping | A point outside a node reaches none of its children, unless the node has `clip = "none"` |
| Skipped | `visible = false` subtrees and nodes playing an [exit](animation.md#exit). `opacity = 0` is still hit |
| Pass-through | `hittable = false` skips a node and its descendants, so a decorative overlay does not swallow the click meant for what is under it. A descendant with `hittable = true` is hit again, and the skipped ancestor's handlers, cursor and hover apply when it is. Hover, cursor, wheel and the surface input region skip it too |
| Edges | Half-open: two nodes sharing an edge never both take it |
| Rects | Every `rect` argument and `hover_rect` value is the node's surface-local `{ x, y, width, height }` laid-out box, before transforms |

## Pointer

Any node takes clicks, drags and the wheel, hit-tested by its own box. For each event the innermost
node with a handler for that event wins; a node without one is transparent, so a handle inside a
draggable track leaves the track draggable, and a `text` with no handler passes the click to the
`row` around it. A node with a handler or `submit = true` shows the `"pointer"` cursor unless it
sets its own [`cursor`](../nodes/index.md#cursor-names).

| Handler | Arguments | Contract |
| :--- | :--- | :--- |
| `on_click(rect, button, pointer)` | `button` is `"left"`, `"right"` or `"middle"`; `pointer` is `{ x, y }` in the node's own untransformed box (mapped back through its transforms and its ancestors'), unclamped | Fires on release over the same node that was pressed, with the same mouse button. Other mouse buttons are ignored |
| `on_drag(rect, pointer, phase)` | `pointer` is `{ x, y }` in the node's own untransformed box (mapped back through its transforms and its ancestors'), unclamped; `phase` is `"start"`, `"move"` or `"end"` | Left button only. See below |
| `on_wheel(rect, steps)` | `steps` is a number of wheel notches | Vertical wheel only. See below |
| `submit = true` | — | Sends the armed [secure field](#secure-fields) on click, like Enter; works without `on_click` and runs before it |

**Click.** A press arms the click and the release fires it. Leaving the node and coming back
before release still clicks; the pointer leaving the surface cancels. The click also cancels if the
node's laid-out box moved between press and release, so give press feedback with `scale` or
`translate` rather than `width` or `margin`. A press on a `textfield` that takes the keyboard never
clicks, not even an `on_click` on the field or around it, and a link in a `text` (`on_link`) takes
the click before any `on_click`, the text's own included.

**Drag.** A left press on an `on_drag` node calls `"start"` at once, so clicking a slider track
also seeks. Every pointer motion on that surface then calls `"move"`, wherever the pointer is.
`"end"` comes on the left release, when the pointer leaves the surface, or when the surface closes.
`rect` stays the box from the press for the whole drag. On release, `"end"` fires first and the
click (if the node also has `on_click`) after it; a leave ends the drag and cancels the click.

**Wheel.** `steps` is positive away from the user (scroll up) and negative toward. One notch is
`1`; high-resolution wheels send fractions of a notch, and touchpads send distance divided by one
notch's 39 px. Horizontal motion never reaches `on_wheel`. The innermost `on_wheel` node or
[scroll container](#scroll) under the pointer takes the whole event, with no chaining to a parent;
on a node that is both, the scroll wins.

<!-- shot-alt: A brightness card with a sun icon, a yellow slider filled to half its track, and the percentage. -->
```lua,shot
local level = state("brightness", 0.5)
local function clamp(value) return math.max(0, math.min(1, value)) end
local percent = level:map(function(value) return string.format("%d%%", math.floor(value * 100 + 0.5)) end)

return panel {
    id = "bar",
    layer = "top",
    anchor = { top = true },
    child = row {
        padding = 14,
        spacing = 12,
        radius = 12,
        background = "#1e1e2e",
        children = {
            icon { name = "display-brightness-symbolic", size = 18, foreground = "#f9e2af", align_v = "center" },
            rect {
                width = 200,
                height = 10,
                radius = 5,
                align_v = "center",
                clip = "rounded",
                background = "#45475a",
                -- A press is "start", so clicking the track also seeks.
                on_drag = function(rect, pointer, phase) level:set(clamp(pointer.x / rect.width)) end,
                on_wheel = function(_, steps) level:set(clamp(level:get() + steps * 0.05)) end,
                children = { rect { height = "fill", background = "#f9e2af", width = percent } },
            },
            text { content = percent, width = 36, foreground = "#cdd6f4", align_v = "center" },
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
    layer = "top",
    anchor = { top = true },
    child = rect {
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
| Bound | Layout clamps the offset to `[0, content − viewport]` and writes the clamped value back. The container needs a bounded size on its axis (fixed, `"fill"` or `max_*`); one sized by its content has nothing to scroll |
| Cost | While only `scroll` properties read the signal, the wheel moves the laid-out children without a layout pass. A `map` or `:get()` of it, or a `scroll` inside a `list` item, costs a pass per wheel event |
| `:reveal(index)` | On the next pass, scrolls the least distance that shows the `index`-th visible child (1-based; a `list`'s items in source order). An index past the end does nothing; below 1 raises. Only a `scroll` signal has it |

## Keyboard controls and accessibility

A node with `on_click` or `submit = true` becomes a keyboard control when it has a nonempty
`accessible_name`. The name is announced through AT-SPI. Give every `textfield` an
`accessible_name` too; fields take focus through their existing callbacks or `secure_submit`.
Keys reach controls only on the surface with keyboard focus. Panels use
`keyboard_interactivity = "on_demand"` or `"exclusive"`; shown popups under that surface share its
focus scope.

Tab moves to the next control in document order across the focused surface and its shown popups.
Shift+Tab moves backward, and both wrap. Hidden, leaving and zero-size controls are skipped.
When fewer than two controls can take focus, Tab reaches a plain field's `on_navigate`.
Enter or Space activates a named control once per press, calling `on_click(rect, "left", pointer)`
with `pointer` at the node's centre, and running `submit` when set. Pointer presses also focus
named controls.

A `panel`, `window` or `popup` takes `on_escape()` for dismissing without a field. It fires once per
Escape press (not on repeat) on the innermost shown popup under the focused surface that declares
it, else the surface itself, and never on a surface without keyboard focus. A focused field keeps
its own Escape while it has text to clear (or a composition) or declares `on_cancel`; otherwise
Escape reaches `on_escape`, so a launcher can clear on the first Escape and close on the second.

```lua
local open = state("menu_open", true)

return panel {
    id = "menu",
    layer = "overlay",
    anchor = { top = true },
    keyboard_interactivity = "on_demand",
    visible = open,
    on_escape = function() open:set(false) end,
    child = text { content = "Menu" },
}
```

The engine draws a black and white outline around the focused control only when Tab, Shift+Tab or
an assistive-technology action moved focus there. Focus from a press, `autofocus` or
`focus_target(name):request()` draws none, and a press hides an outline Tab drew. `focus_ring = false`
keeps the outline off a node.

`focused(name)` returns a read-only boolean signal, `false` until the bound node first holds focus.
Bind it to a node's `focused` and the engine sets it `true` while that node or any node inside it
holds control focus, however focus got there. Like `hover(name)`, the name is the identity and
survives reloads. It is not `focus_target(name)`, the handle a click uses to focus a textfield.

```lua
local search = focused("search")

return panel {
    id = "launcher",
    layer = "top",
    keyboard_interactivity = "on_demand",
    child = rect {
        padding = 6,
        radius = 8,
        border_width = 2,
        focused = search,
        border_color = search:map(function(on) return on and "#89b4fa" or "#45475a" end),
        children = {
            textfield {
                width = 240,
                height = 28,
                autofocus = true,
                focus_ring = false,
                accessible_name = "Search",
                on_change = function(text) end,
            },
        },
    },
}
```

Mantle publishes the resolved node tree through AT-SPI. Text nodes expose their displayed text;
plain fields expose their draft. Secure fields expose a password role but no value or length.
Assistive-technology focus and click actions use the same control path as keyboard input. A
screen reader can also click a button on a surface without keyboard focus; the keyboard stays
where it is.

## Text fields

A `textfield` is a single-line text input. The engine holds what the user types (the *draft*); Lua
sees it through callbacks, and sets it only with `:set_text`. The field with *focus* is the one keys go to. It
has no intrinsic width, so give it `width`; `height` defaults to one line ([nodes](../nodes/textfield.md)).

A field takes the keyboard only when both hold:

| Condition | Detail |
| :--- | :--- |
| The surface has keyboard focus | A `panel` needs `keyboard_interactivity = "on_demand"` or `"exclusive"` ([keyboard focus](../surfaces/panel.md#keyboard-focus)); a popup shown under the focused surface shares its keys |
| The field can use keys | It has `secure_submit`, `on_change` or `on_submit`. A field with none of them (even with `on_cancel` or `on_navigate`) never takes focus, and a press on it acts like a press on empty space |

A press on the field focuses it and puts the caret under the pointer. `autofocus` focuses it without
a press.

| Property | Contract |
| :--- | :--- |
| `on_change(text)` | Every edit that changes the text, with the whole draft. Caret moves call nothing |
| `on_submit(text)` | Enter, with the whole draft (possibly `""`). The draft then clears and `on_change("")` follows; the field keeps focus. A held Enter does not repeat |
| `on_cancel(cleared)` | Escape. The draft clears, the field drops focus, `on_change("")` fires if there was text, then `on_cancel` gets whether text was removed. Without `on_cancel`, Escape clears and the field keeps focus |
| `on_navigate(key)` | `"up"`, `"down"`, `"page_up"`, `"page_down"`, and `"left"`/`"right"` when the caret cannot move that way and Shift is up. Tab and Backtab arrive when fewer than two controls can take focus. Repeats while held. The draft is untouched |
| `autofocus` | `true`: take the keys, with an empty draft and a call to `on_change("")`, when the surface gains keyboard focus or the field appears under it. The first visible such field in document order wins. It never takes over from a field that is already typing, and never re-takes a field the user just clicked away from |
| `focus_target` | A `focus_target(name)` handle. An `on_click` can call `:request()` to focus the first visible plain field with that name on the same keyboard-focused surface or a popup under it, after the click's state changes appear. It keeps that field's draft and caret and does not call `on_change` |
| `focus_target(name):set_text(text)` | Replaces the draft of every visible plain field with that `focus_target`, from any callback, once it returns: caret at the end, undo history cleared, `on_change` not called, composition discarded. A field without focus keeps the text for when it takes the keys. Never reaches a `secure_submit` field. Control characters or over 64 KiB raise |
| `secure_submit`, `mask_character` | See [secure fields](#secure-fields) |
| `placeholder`, `placeholder_color`, `font_size`, `foreground`, `text_align` | Appearance; see [textfield](../nodes/textfield.md) |

| Key | Plain field | Secure field |
| :--- | :--- | :--- |
| Text | Inserts at the caret, replacing a selection; `on_change` | Appends |
| Enter | `on_submit`, then clears | Sends |
| Escape | Clears; with `on_cancel`, also drops focus | Clears, stays armed, `on_cancel` |
| Backspace, Delete | One character, or the selection | Backspace only |
| Ctrl+Backspace, Ctrl+Delete | One word | Nothing |
| Left, Right, Home, End | Move the caret; Ctrl+Left/Right by word; Shift selects. Left/Right with nowhere to go (and no Shift) call `on_navigate` | Nothing |
| Ctrl+A | Selects all | Nothing |
| Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y | Undo or redo the last plain edits; `on_change` | Nothing |
| Ctrl+C | Copies selected text | Nothing |
| Ctrl+V | Replaces selection with clipboard text; `on_change` | Appends clipboard text to the native buffer |
| Up, Down, Page Up, Page Down | `on_navigate` | Nothing |
| Tab, Shift+Tab | Moves between controls when at least two are available; otherwise `on_navigate` (`"backtab"` for Shift+Tab) | Moves between controls when at least two are available |
| Any other Ctrl chord | Left to the compositor | Same |

**Selection and clipboard.** Dragging or Shift+clicking with the pointer selects too. Paste accepts
up to 64 KiB of valid UTF-8 without control characters. A paste is dropped if the selection, field,
or keyboard focus changes before the read ends. Copy works only with a plain-field selection.
Editing keys repeat while held; Escape, undo and redo do not.
The undo stack retains at most 100 snapshots and 1 MiB of saved text. Submit, leaving the
field, and fresh autofocus clear that history. When text-input-v3 enters the field's own surface,
composition appears underlined. Preedit alone does not call
`on_change`; committed text and surrounding deletions do. Raw typing
resumes when composition ends. A pointer press or Escape discards uncommitted composition.
Secure fields do not use an IME.

**Draft lifetime.** Each field keeps its own draft. Clicking elsewhere, moving to another field, or
the surface losing the keyboard stops typing but keeps the draft; focusing the field again resumes
it, though a click that returns to a field puts the caret where it was left. Undo history does not
survive leaving a field. Enter and Escape clear it. An `autofocus` arm
starts it empty. It is dropped when the field's node leaves the tree or its surface closes.

To return typing to a field after a click changes the view, give the field the handle and call
`:request()` from the `on_click`. `focus_target("")` and a `focus_target` property that is not a handle raise.
Requests outside an `on_click`, to a hidden or masked field, or to a surface other than the focused one and its
popups do nothing.

```lua
local search_focus = focus_target("search")

return panel {
    id = "search",
    layer = "top",
    keyboard_interactivity = "on_demand",
    child = row { children = {
        textfield {
            focus_target = search_focus,
            autofocus = true,
            width = 180,
            height = 32,
            on_change = function() end,
        },
        rect {
            on_click = function() search_focus:request() end,
            children = { text { content = "Return to search" } },
        },
    } },
}
```

A search field that picks from a list moves the selection with `on_navigate` and closes on a second
Escape. The [app launcher](../cookbook/launcher.md) recipe is the full version.

```lua
local query = state("query", "")
local selected = state("selected", 1)
local open = state("launcher_open", true)

return textfield {
    width = 240,
    height = 32,
    placeholder = "Search apps",
    autofocus = true,
    on_change = function(text)
        query:set(text)
        selected:set(1)
    end,
    on_navigate = function(key)
        local step = ({ up = -1, down = 1 })[key]
        if step then selected:set(math.max(1, selected:get() + step)) end
    end,
    -- First Escape clears the text; a second one, on an empty field, closes.
    on_cancel = function(cleared)
        if not cleared then open:set(false) end
    end,
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
| `{ capability = "network", action = "vpn_secret", name = request.id .. "/" .. field }` | One secret of a VPN activation's pending request ([network](../capabilities/network.md)) |
| `{ capability = "bluetooth", action = "pair", name = request.id .. "/" .. request.mac }` | Answers a Bluetooth PIN or passkey entry request ([bluetooth](../capabilities/bluetooth.md)) |
| `{ capability = "secrets", action = "store", name = "mail" }` | Stores a named value in the session Secret Service ([secrets](../capabilities/secrets.md)) |
| Any other pair | Refused when the field is laid out, so no password is typed into nowhere |

| Rule | Detail |
| :--- | :--- |
| Arming | When the surface gains keyboard focus, the sole visible secure field in it (and in popups shown under it) is armed with no click. With two or more, a press picks one. A field revealed later under existing focus arms if none is armed |
| Keys | Typed text appends, Backspace removes one character, Escape clears the buffer, stays armed and calls the field's `on_cancel(cleared)`. There is no caret, selection or `on_navigate`; `on_change` and `on_submit` never fire |
| Sending | Enter, or a click on a `submit = true` node, sends the buffer and wipes it. An empty buffer is sent only to `network`/`connect`, where it joins an open network |
| Focus | A click on anything but a field keeps the field armed, so a `submit = true` node works. Tab to a named button keeps the buffer too: typing stops, Enter on a `submit = true` button sends it, and Escape still clears it and calls `on_cancel`. Focusing another field, plain or secure, or the keyboard leaving the surface, disarms it and wipes the buffer |
| Priority | While a secure field is armed, plain fields in the same focus take no keys |
| `mask_character` | Drawn once per typed character. Default `"•"`; only the first character counts; `""` draws nothing and hides the length. Only secure fields draw it. An empty field shows its `placeholder` |

```lua
return lock {
    id = "lock",
    child = column {
        width = "fill",
        height = "fill",
        align_h = "center",
        align_v = "center",
        spacing = 12,
        background = "#11111b",
        children = {
            textfield {
                width = 280,
                height = 40,
                placeholder = "Password",
                accessible_name = "Password",
                mask_character = "•",
                secure_submit = { capability = "lock", action = "authenticate" },
            },
            rect {
                padding = 8,
                radius = 8,
                background = "#89b4fa",
                submit = true,
                accessible_name = "Unlock",
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
| Move a selection through a list with the arrow keys | `on_navigate` under [text fields](#text-fields); the [app launcher](../cookbook/launcher.md) adds `scroll(name):reveal` |
| Close a search box on a second Escape | The same example: `on_cancel(cleared)` closes only when `cleared` is `false` |
| Close a dialog or menu on Escape | `on_escape` on the surface ([keyboard controls](#keyboard-controls-and-accessibility)) |
| Ask for a password | The lock example under [secure fields](#secure-fields) |
| Reach a button by keyboard or screen reader | Give the node with `on_click` or `submit` an `accessible_name` ([keyboard controls](#keyboard-controls-and-accessibility)) |
| Hide or restyle the focus outline | `focus_ring = false` on the control, and style a wrapper from `focused(name)` ([keyboard controls](#keyboard-controls-and-accessibility)) |
| Show a tooltip on hover | [Tooltip](../surfaces/popup.md), with `hover_rect` as the anchor |
| Open a menu on right click | Below |
| Reorder a list by dragging | Below |

**Right-click menu.** `on_click` reports the mouse button and the node's rect, which is what a
[popup](../surfaces/popup.md)'s `anchor_rect` wants. The click is a pointer release, so the popup may
take its grab. The menu hangs from the node, not the click point: no handler reports the pointer
position of a click.

<!-- shot-alt: A context menu with New window and Downloads open beneath a Files button with a folder icon. -->
```lua,shot
local menu_open = state("context_open", false)
local menu_at = state("context_at", { x = 0, y = 0, width = 1, height = 1 })

local function item(label, run)
    return rect {
        width = "fill",
        padding = 8,
        radius = 6,
        on_click = function()
            menu_open:set(false)
            run()
        end,
        children = { text { content = label, foreground = "#cdd6f4" } },
    }
end

local files = row {
    padding = 8,
    spacing = 8,
    radius = 8,
    background = "#1e1e2e",
    on_click = function(rect, which)
        if which == "right" then
            menu_at:set(rect)
            menu_open:set(true)
        else
            process.detach("nautilus", {})
        end
    end,
    children = {
        icon { name = "folder", size = 18, align_v = "center" },
        text { content = "Files", foreground = "#cdd6f4", align_v = "center" },
    },
}

return {
    panel { id = "bar", layer = "top", anchor = { top = true }, child = files },
    popup {
        id = "files_menu",
        parent = "bar",
        anchor_rect = menu_at,
        anchor = "bottom",
        gravity = "bottom_right",
        offset = { y = 4 },
        visible = menu_open,
        on_dismiss = function() menu_open:set(false) end,
        child = column {
            width = 160,
            padding = 4,
            radius = 10,
            background = "#1e1e2e",
            border_width = 1,
            border_color = "#45475a",
            children = {
                item("New window", function() process.detach("nautilus", { "--new-window" }) end),
                item("Downloads", function() process.detach("nautilus", { os.getenv("HOME") .. "/Downloads" }) end),
            },
        },
    },
}
```

**Drag to reorder.** Each row is an `on_drag` node in a keyed [`list`](../nodes/list.md). The drag
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
    return rect {
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
    layer = "top",
    anchor = { top = true },
    child = list { spacing = 4, source = items, itemfn = row_for, key = function(name) return name end },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A `textfield` is invisible or cannot be clicked | It has no intrinsic width. Give it `width` |
| A field in a panel shows no caret and takes no keys | Set the panel's `keyboard_interactivity` to `"on_demand"` (or `"exclusive"` for a modal) |
| A field with only `on_navigate`/`on_cancel` ignores clicks | Add `on_change` or `on_submit` |
| Tab skips a button, or Tab does nothing | Give the button an `accessible_name`, and the panel a `keyboard_interactivity` other than `"none"` |
| A clicked or `autofocus` field shows no focus outline | The outline follows keyboard navigation only. Bind `focused(name)` for a style that tracks any focus |
| Tab stopped reaching `on_navigate` | With two or more controls in the focus scope, Tab moves focus instead. It reaches `on_navigate` only when the field is the sole control |
| The mouse wheel does nothing over a scrolling `row` | Rows scroll on the horizontal axis. Use a `column`, or an `on_wheel` on a node around it that moves the row |
| A `scroll` container never scrolls | Bound its size on the scroll axis; content-sized means nothing overflows |
| `on_hover` is refused | Add `hover = hover("name")` on the same node |
| A container's hover stays on while the pointer is over a child | Hover covers the whole subtree. Give the child its own `hover` for innermost-only behaviour |
| A click is lost when the node grows on press | The release must land on the same laid-out box. Animate `scale` instead |
| A tooltip or menu anchored to a scaled node is off | Rects are laid-out boxes before transforms. Anchor on an untransformed parent |
| Lua needs to prefill or clear a field | Not possible: the draft belongs to the engine. `autofocus` re-arms empty; Escape and Enter clear |
| A typed password shows up in `on_change` | It cannot: a `secure_submit` field never calls it. Plain fields also stop taking keys while a secure field is armed |

See also: [nodes](../nodes/index.md) ([`textfield`](../nodes/textfield.md), [`list`](../nodes/list.md)), [surfaces](../surfaces/index.md)
(`keyboard_interactivity`, popups, lock), [signals](signals.md) (state the handlers write),
[animation](animation.md) (press and hover motion), [capabilities](../capabilities/index.md) ([`lock`](../capabilities/lock.md),
[`polkit`](../capabilities/polkit.md), [`network`](../capabilities/network.md)).

Source: [pointer](../../renderer/src/wayland/input/pointer/mod.rs), [wheel](../../renderer/src/wayland/input/pointer/wheel.rs),
[keyboard](../../renderer/src/wayland/input/keyboard/mod.rs), [text fields](../../renderer/src/wayland/input/keyboard/plain.rs),
[secure fields](../../renderer/src/wayland/input/keyboard/secure.rs), [hit testing](../../renderer/src/layout/hit.rs),
[hover](../../renderer/src/layout/hover.rs), [scroll](../../renderer/src/layout/scene/scroll.rs).
