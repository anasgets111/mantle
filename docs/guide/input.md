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
| Clipping | A point outside a node that clips (`clip = "box"` or `"rounded"`, a `mask`, a scroll viewport, a surface) reaches none of its children; a child overflowing any other node is hit where it paints |
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
| `on_click(rect, button, pointer, modifiers)` | `button` is `"left"`, `"right"` or `"middle"`; `pointer` is `{ x, y }` in the node's own untransformed box (mapped back through its transforms and its ancestors'), unclamped | Fires on release over the same node that was pressed, with the same mouse button. Other mouse buttons are ignored |
| `on_press(rect, button, pointer, modifiers)` | As `on_click` | Fires at press, for any of the three buttons, before the release and so before `on_click`. Not called for a press on a `textfield`. See below |
| `on_drag(rect, pointer, phase, modifiers)` | `pointer` is `{ x, y }` in the node's own untransformed box (mapped back through its transforms and its ancestors'), unclamped; `phase` is `"start"`, `"move"` or `"end"` | Left button only. See below |
| `on_wheel(rect, steps, modifiers)` | `steps` is a number of wheel notches | Vertical wheel only. See below |
| `submit = true` | — | Sends the armed [secure field](#secure-fields) on click, like Enter; works without `on_click` and runs before it |

**Click.** A press arms the click and the release fires it. Leaving the node and coming back
before release still clicks; the pointer leaving the surface cancels. The click also cancels if the
node's laid-out box moved between press and release, so give press feedback with `scale` or
`translate` rather than `width` or `margin`. A press on a `textfield` that takes the keyboard never
clicks, not even an `on_click` on the field or around it, and a link in a `text` (`on_link`) takes
the click before any `on_click`, the text's own included.

**Modifiers.** Each pointer handler ends with `modifiers`, the `{ ctrl, shift, alt, super }` booleans
`on_key` reports, so a list can toggle on Ctrl-click or extend on Shift-click. Enter and Space
activation report the keyboard's modifiers too. Wayland tells a client the modifiers only while one
of its surfaces has keyboard focus; otherwise all four read `false`.

```lua
local picked = state("picked", false)
return panel {
    id = "bar",
    layer = "top",
    anchor = { top = true },
    child = rect {
        padding = 8,
        background = picked:map(function(on) return on and "#89b4fa" or "#313244" end),
        on_click = function(_, _, _, modifiers)
            picked:set(modifiers.ctrl and not picked:get() or not modifiers.ctrl)
        end,
        children = { text { content = "Ctrl-click toggles" } },
    },
}
```

**Press.** `on_press` is the one place a callback can start a window move, resize or menu with
[`toplevel(id)`](../surfaces/window.md#custom-title-bar): the compositor checks those requests
against the press, and `on_click` fires on release, too late. An `on_drag` `"start"` works too.
After the compositor takes the pointer it may send no release, so the engine ends the drag and the
armed click; `on_drag` gets no `"end"` and `on_click` does not fire.

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

## Pointer position

`pointer(name)` returns a read-only signal of the pointer's `{ x, y }` in logical pixels from the
top-left corner of one node, or `nil` while the pointer is outside it. Bind it to a node's `pointer`;
the name is the identity, as for `hover`.

| API | Contract |
| :--- | :--- |
| `pointer = pointer(name)` | Any node kind. A table while the pointer is over the node or any of its children (the same rule as `hover`), `nil` otherwise, including after it leaves the surface. Coordinates are in the node's own untransformed space and unclamped, like `on_click`'s `pointer`. Updated once per motion batch and when layout moves the node under a still pointer |
| Cost | Motion writes it only while something reads it. A signal nothing reads costs the hit walk the pointer already does; a read one re-resolves only the instances that read it. One output's instance holds the position while the pointer is on it, as the name is one signal across outputs |

A `pointer` read only inside a callback (`on_click`, `on_press`, `on_key`) is always `nil`: nothing
read it, so motion never wrote it. Bind it to a node's `pointer`, or read it in a computed that a node
reads.

```lua
local at = pointer("canvas")

return panel {
    id = "bar",
    layer = "top",
    anchor = { top = true },
    child = rect {
        width = 200,
        height = 60,
        pointer = at,
        children = {
            rect {
                width = 8,
                height = 8,
                radius = 4,
                background = "#f5c2e7",
                visible = at:map(function(p) return p ~= nil end),
                translate = at:map(function(p) return { x = p and p.x - 4 or 0, y = p and p.y - 4 or 0 } end),
            },
        },
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
| Bound | The offset stays within `[0, content − viewport]`: the wheel stops at the ends the last layout measured, and layout clamps again and writes the clamped value back. A `map` of it that read an offset layout then moved (content that shrank, or a `:reveal` or `:scroll_to`) lays out again before the frame is drawn. The container needs a bounded size on its axis (fixed, `"fill"` or `max_*`); one sized by its content has nothing to scroll |
| Cost | While only `scroll` properties read the signal, the wheel moves the laid-out children without a layout pass. A `map` or `:get()` of it, or a `scroll` inside a `list` item, costs a pass per wheel event, or per frame while an eased notch runs |
| Smooth | `animate = { scroll = 160 }` on the container eases each wheel notch: notches add to a target that stops at the ends, and the offset follows it frame by frame. The signal holds the offset on screen, so a `map` of it lays out each frame from what that frame draws. `:reveal`, `:scroll_to` and `:scroll_by` ease too; a touchpad, a high-resolution wheel and `reset_on_close` move it at once. See [animation](animation.md#scroll) |
| `:reveal(index)` | On the next pass, scrolls the least distance that shows the `index`-th visible child (1-based; a `list`'s items in source order). An index past the end does nothing; below 1 raises. Only a `scroll` signal has it |
| `:scroll_to(offset)` | On the next pass, moves to `offset` px, clamped to the bound. NaN and infinity raise. Only a `scroll` signal has it |
| `:scroll_by(delta)` | Like `:scroll_to`, but adds `delta` to where the offset is headed, as a wheel notch does: during an eased run the signal reads the offset on screen, so `:scroll_to(s:get() + step)` would fall short, while clicks of `:scroll_by(step)` add up. A request waits for its container to be shown, and sums with an earlier one in the same turn before clamping (`:scroll_to(900)` then `:scroll_by(-50)` asks for 850). `reset_on_close` drops it, and so does the next pass when no container in any surface holds the signal (a hidden one still does), so an area built later starts at the top; a request made in the same turn as the area it scrolls lands. A container that is leaving (playing its [exit](animation.md#exit)) does not hold it |

Arrow buttons for a carousel; `strip:scroll_to(0)` would rewind it:

```lua
local strip = scroll("carousel")
local cards = {}
for i = 1, 12 do cards[i] = rect { width = 120, height = 80, radius = 8, background = "#313244" } end
local function arrow(label, step)
    return rect {
        padding = 6,
        accessible_name = label,
        on_click = function() strip:scroll_by(step) end,
        children = { text { content = label } },
    }
end

return panel {
    id = "carousel",
    layer = "top",
    child = row { spacing = 8, children = {
        arrow("<", -128),
        row { width = 376, spacing = 8, scroll = strip, animate = { scroll = 200 }, children = cards },
        arrow(">", 128),
    } },
}
```

## Keyboard controls and accessibility

A node with `on_click`, `submit = true` or `on_key` becomes a keyboard control when it has a nonempty
`accessible_name`. The name is announced through AT-SPI. Give every `textfield` an
`accessible_name` too; fields take focus through their existing callbacks or `secure_submit`.
Keys reach controls only on the surface with keyboard focus. Panels use
`keyboard_interactivity = "on_demand"` or `"exclusive"`; shown popups under that surface share its
focus scope.

Tab moves to the next control in document order across the focused surface and its shown popups.
Shift+Tab moves backward, and both wrap. Hidden, leaving and zero-size controls are skipped.
When fewer than two controls can take focus, Tab reaches `on_key`.
Enter or Space activates a named control once per press, calling `on_click(rect, "left", pointer)`
with `pointer` at the node's centre, and running `submit` when set. Pointer presses also focus
named controls.

A `panel`, `window` or `popup` takes `on_escape()` for dismissing without a field. It fires once per
Escape press (not on repeat) on the innermost shown popup under the focused surface that declares
it (no order is promised among sibling popups), else the surface itself, and never on a surface
without keyboard focus. A focused field keeps its own Escape while it has text to clear (or a
composition) or declares `on_cancel`; otherwise Escape reaches `on_escape`, so a launcher can clear
on the first Escape and close on the second.

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

### Key handlers

`on_key(key)` on any node or surface hears the keys of a custom control: a grid, a menu, a slider.
`key` is `{ name, text, modifiers = { ctrl, shift, alt, super }, repeat }`.

| Field | Meaning |
| :--- | :--- |
| `name` | The xkb keysym name for the key under the active layout: `"a"` (`"A"` with Shift), `"Return"`, `"Escape"`, `"space"`, `"Down"`, `"Page_Down"`, `"F5"`, `"KP_Enter"`. `on_key` names are case-sensitive. `mantle input key` takes the same names in any case |
| `text` | What the key types, `nil` when it types nothing: Return, arrows, F-keys and every Ctrl chord |
| `modifiers` | Which of Ctrl, Shift, Alt and Super are held. `mantle input key` sets Ctrl and Shift only |
| `repeat` | `true` for an auto-repeat while the key is held, `false` for the press |

A key goes to the focused node first, then each ancestor up to the surface, which hears it last
whether or not it holds a control. A handler that returns `true` stops it; returning nothing or
`false` passes it up. A node without `on_key` is skipped. Without focus on a control, only the
surface's own `on_key` hears keys, while the surface has the keyboard. A control's `on_key` runs
before its Enter/Space activation, and a handled Escape never reaches `on_escape`.

A node with `on_key` takes Tab focus, a press and the outline like any named control, but only with an
`accessible_name`, so a screen reader can announce it. Without one it is not focusable, and hears just
what its descendants pass up. That is how a container sees the keys of the field inside it.

A focused plain `textfield` takes the keys it edits with (characters, Backspace, Delete, Enter, caret
motion, Ctrl+A/Z/Y, and Escape, unless `escape = "pass"` or there is nothing to clear and no `on_cancel`) and passes the rest up:
Up, Down, paging, Tab with fewer than two controls, an arrow at the caret's edge, F-keys and other
Ctrl chords (Ctrl+C and Ctrl+V copy and paste while a field takes them, and reach `on_key` otherwise). Tab moves control focus when it can, so
`on_key` hears Tab only where nothing moves. Repeats arrive with `repeat = true`.

A key typed into a `secure_submit` field never reaches `on_key`, and while one is armed no `on_key` on
that surface is called.

```lua
local picked = state("picked", 1)

return panel {
    id = "grid",
    layer = "top",
    keyboard_interactivity = "on_demand",
    child = row {
        accessible_name = "Picker",
        on_key = function(key)
            if key.name == "Right" and not key.modifiers.ctrl then
                picked:set(picked:get() + 1)
                return true
            elseif key.name == "Left" then
                picked:set(math.max(1, picked:get() - 1))
                return true
            end
        end,
        children = { text { content = picked:map(tostring) } },
    },
}
```

The engine draws a black and white outline around the focused control only when Tab, Shift+Tab or
an assistive-technology action moved focus there. Focus from a press, `autofocus` or
`focus_target(name):request()` from a click draws none, and a press hides an outline Tab drew. `focus_ring = false`
keeps the outline off a node.

`focused(name)` returns a read-only boolean signal, `false` until the bound node first holds focus.
Bind it to a node's `focused` and the engine sets it `true` while that node or any node inside it
holds control focus, however focus got there. Like `hover(name)`, the name is the identity and
survives reloads. It is not `focus_target(name)`, the handle `:request()` uses to move focus.

`focus_visible(name)` is the same signal under the rule the engine's outline follows (CSS
`:focus-visible`): `true` only while the bound node or one inside it holds control focus that Tab,
Shift+Tab, an assistive-technology action or a `:request()` from a key callback or an Enter or Space
activation moved there, and `false` after a press, `autofocus` or a `:request()` from a click, while `focused(name)` stays `true`. It ignores `focus_ring`. Bind it to
a node's `focus_visible`, and draw a ring that shows for keyboard users only:

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

```lua
local ring = focus_visible("save")

return panel {
    id = "dialog",
    layer = "top",
    keyboard_interactivity = "on_demand",
    child = rect {
        padding = 4,
        radius = 8,
        border_width = 2,
        focus_visible = ring,
        border_color = ring:map(function(on) return on and "#89b4fa" or "#00000000" end),
        children = {
            rect {
                padding = 6,
                background = "#45475a",
                accessible_name = "Save",
                focus_ring = false,
                on_click = function() end,
                children = { text { content = "Save" } },
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
sees it through callbacks, and sets it only with `:set_text`. The field with *focus* is the one keys
go to. It has no intrinsic width, so give it `width`; `height` defaults to one line
([nodes](../nodes/textfield.md)).

A field takes the keyboard only when both hold:

| Condition | Detail |
| :--- | :--- |
| The surface has keyboard focus | A `panel` needs `keyboard_interactivity = "on_demand"` or `"exclusive"` ([keyboard focus](../surfaces/panel.md#keyboard-focus)); a popup shown under the focused surface shares its keys |
| The field can use keys | It has `secure_submit`, `on_change` or `on_submit`. A field with none of them (even with `on_cancel`) never takes focus, and a press on it acts like a press on empty space |

A press on the field focuses it and puts the caret under the pointer. `autofocus` focuses it without
a press.

| Property | Contract |
| :--- | :--- |
| `on_change(text)` | Every edit that changes the text, with the whole draft. Caret moves call nothing |
| `on_submit(text)` | Enter, with the whole draft (possibly `""`). The draft then clears and `on_change("")` follows; the field keeps focus. A held Enter does not repeat |
| `on_cancel(cleared)` | Escape. The draft clears, the field drops focus, `on_change("")` fires if there was text, then `on_cancel` gets whether text was removed. Without `on_cancel`, Escape clears and the field keeps focus |
| `on_key(key)` | The keys the field does not edit with, from its own `on_key` up ([key handlers](#key-handlers)): Up, Down, paging, Left and Right when the caret cannot move that way and Shift is up, and Tab when fewer than two controls can take focus. The draft is untouched |
| `escape` | What Escape does: `"clear"` (the default) as under `on_cancel`; `"blur"` keeps the draft and drops focus, then calls `on_cancel(false)`; `"pass"` keeps the draft and focus and does not take the key, which goes up through `on_key` and then to the surface's `on_escape`. A `secure_submit` field ignores it |
| `autofocus` | `true`: take the keys, with the draft reset to `initial_text` (`""` when unset) and a call to `on_change` with it, when the surface gains keyboard focus or the field appears under it. The first visible such field or [focusable control](#keyboard-controls-and-accessibility) in document order wins. A field never takes over from one already typing, nor re-takes one the user just clicked away from. A control arms only while no control holds focus, once per appearance or keyboard enter |
| `focus_target` | A `focus_target(name)` handle. `:request()` from an `on_click`, `on_key`, or an edit typed or committed into the field (`on_change`, `on_submit`, `on_cancel`) focuses the first visible plain field with that name on the same keyboard-focused surface or a popup under it, after the callback's state changes appear. It keeps that field's draft and caret and does not call `on_change`. A key callback's request shows the focus outline |
| `focus_target(name):set_text(text)` | Replaces the draft of every plain field with that `focus_target` and an `on_change` or `on_submit`, hidden ones too, from any callback, once it returns: caret at the end, undo history cleared, `on_change` not called, composition discarded. A field without focus keeps the text for when it takes the keys. Never reaches a `secure_submit` field. Control characters or over 64 KiB raise |
| `initial_text` | Seeds the draft once, when the field enters the tree (a new node: a changed `id` or `key` counts as new, and a field that leaves and returns is seeded again from the value then). Plain fields only; `secure_submit` refuses it. Like `set_text`: cut at `max_length`, caret at the end, no undo history, no `on_change`; hidden and disabled fields are seeded too. Later changes to the value are ignored and an emptied field stays empty: use `set_text` to push new text. Read without subscribing: writing the signal alone does not re-resolve the field |
| `disabled` | `true`: the field draws as usual but takes no focus. Tab skips it, a press and `autofocus` pass over it, `:request()` finds nothing, and no caret shows. A focused field that becomes disabled loses focus and keeps its draft. A disabled `secure_submit` field is not a destination. `set_text` still reaches it. Dim it by binding colours to the same signal |
| `max_length` | Most grapheme clusters the field holds; `0` is unlimited. Typing, paste, IME commits and `set_text` cut the insert at the limit, secure fields included. Lowering it below the current text keeps that text: only the insert is limited, so edits can then only shorten it. The cut is silent: a limit below a password's length truncates it, on a lock field too |
| `secure_submit`, `mask_character` | See [secure fields](#secure-fields) |
| `placeholder`, `placeholder_color`, `font_size`, `foreground`, `text_align` | Appearance; see [textfield](../nodes/textfield.md) |

| Key | Plain field | Secure field |
| :--- | :--- | :--- |
| Text | Inserts at the caret, replacing a selection; `on_change` | Appends |
| Enter | `on_submit`, then clears | Sends |
| Escape | Clears; with `on_cancel`, also drops focus | Clears, stays armed, `on_cancel` |
| Backspace, Delete | One character, or the selection | Backspace only |
| Ctrl+Backspace, Ctrl+Delete | One word | Nothing |
| Left, Right, Home, End | Move the caret; Ctrl+Left/Right by word; Shift selects. Left/Right with nowhere to go (and no Shift) pass up to `on_key` | Nothing |
| Ctrl+A | Selects all | Nothing |
| Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y | Undo or redo the last plain edits; `on_change` | Nothing |
| Ctrl+C | Copies selected text | Nothing |
| Ctrl+V | Replaces selection with clipboard text; `on_change` | Appends clipboard text to the native buffer |
| Up, Down, Page Up, Page Down | Pass up to `on_key` | Nothing |
| Tab, Shift+Tab | Moves between controls when at least two are available; otherwise passes up to `on_key` (`"ISO_Left_Tab"` for Shift+Tab) | Moves between controls when at least two are available |
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
Restoring a draft is silent: `on_change` does not fire, so reset a field with `set_text`.

To return typing to a field after a click changes the view, give the field the handle and call
`:request()` from the `on_click`. `focus_target("")` and a `focus_target` property that is not a handle raise.
`on_key`, and edits typed or committed into a field (`on_change`, `on_submit`, `on_cancel`), can request too; the
`on_change` an `autofocus` arm or `set_text` fires cannot. Requests from anywhere else, to a hidden or masked field, or
to a surface other than the focused one and its popups do nothing.

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

`focus_target` and `autofocus` also work on any [focusable control](#keyboard-controls-and-accessibility). A request
from a key callback shows the outline, so Lua can rove focus with the arrow keys. On a node that takes no focus they
do nothing, and `set_text` does nothing on a control.

```lua
local next_button = focus_target("next")

return panel {
    id = "toolbar",
    layer = "top",
    keyboard_interactivity = "on_demand",
    child = row { children = {
        rect {
            accessible_name = "First",
            autofocus = true,
            width = 40,
            height = 32,
            on_key = function(key)
                if key.name == "Right" then next_button:request() return true end
            end,
        },
        rect { accessible_name = "Next", focus_target = next_button, width = 40, height = 32, on_key = function() end },
    } },
}
```

A search field that picks from a list moves the selection with `on_key` and closes on a second
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
    on_key = function(key)
        local step = ({ Up = -1, Down = 1 })[key.name]
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
| Keys | Typed text appends, Backspace removes one character, Escape clears the buffer, stays armed and calls the field's `on_cancel(cleared)`. There is no caret or selection, and no `on_key`; `on_change` and `on_submit` never fire |
| Sending | Enter, or a click on a `submit = true` node, sends the buffer and wipes it. An empty buffer is sent only to `network`/`connect`, where it joins an open network |
| Focus | A click on anything but a field keeps the field armed, so a `submit = true` node works. Tab to a named button keeps the buffer too: typing stops, Enter on a `submit = true` button sends it, and Escape still clears it and calls `on_cancel`. Focusing another field, plain or secure, or the keyboard leaving the surface, disarms it and wipes the buffer |
| Priority | While a secure field is armed, plain fields in the same focus take no keys |
| `on_escape` | A surface's `on_escape` fires when the buffer is empty and the field has no `on_cancel`, so it reveals that one bit (ADR-0320) |
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
| Move a selection through a list with the arrow keys | `on_key` on the field ([key handlers](#key-handlers)); the [app launcher](../cookbook/launcher.md) adds `scroll(name):reveal` |
| Close a search box on a second Escape | The same example: `on_cancel(cleared)` closes only when `cleared` is `false` |
| Let Escape reach the surface from a field | `escape = "pass"` on the `textfield` |
| Handle arrows or shortcuts on a custom control | `on_key` ([key handlers](#key-handlers)) |
| Close a dialog or menu on Escape | `on_escape` on the surface ([keyboard controls](#keyboard-controls-and-accessibility)) |
| Ask for a password | The lock example under [secure fields](#secure-fields) |
| Reach a button by keyboard or screen reader | Give the node with `on_click` or `submit` an `accessible_name` ([keyboard controls](#keyboard-controls-and-accessibility)) |
| Hide or restyle the focus outline | `focus_ring = false` on the control, and style a wrapper from `focus_visible(name)`, or `focused(name)` to track any focus ([keyboard controls](#keyboard-controls-and-accessibility)) |
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
| A field with only `on_key`/`on_cancel` ignores clicks | Add `on_change` or `on_submit` |
| Tab skips a button, or Tab does nothing | Give the button an `accessible_name`, and the panel a `keyboard_interactivity` other than `"none"` |
| A clicked or `autofocus` field shows no focus outline | The outline follows keyboard navigation only. Bind `focus_visible(name)` to draw your own ring by the same rule, or `focused(name)` for a style that tracks any focus |
| Tab stopped reaching `on_key` | With two or more controls in the focus scope, Tab moves focus instead. It reaches `on_key` only when the field is the sole control |
| The mouse wheel does nothing over a scrolling `row` | Rows scroll on the horizontal axis. Use a `column`, or an `on_wheel` on a node around it that moves the row |
| A `scroll` container never scrolls | Bound its size on the scroll axis; content-sized means nothing overflows |
| `on_hover` is refused | Add `hover = hover("name")` on the same node |
| A container's hover stays on while the pointer is over a child | Hover covers the whole subtree. Give the child its own `hover` for innermost-only behaviour |
| A click is lost when the node grows on press | The release must land on the same laid-out box. Animate `scale` instead |
| A tooltip or menu anchored to a scaled node is off | Rects are laid-out boxes before transforms. Anchor on an untransformed parent |
| Lua needs to prefill or clear a field | Call `focus_target(name):set_text(text)`; `""` clears. `autofocus` also re-arms empty, and Escape and Enter clear |
| A typed password shows up in `on_change` | It cannot: a `secure_submit` field never calls it. Plain fields also stop taking keys while a secure field is armed |

See also: [nodes](../nodes/index.md) ([`textfield`](../nodes/textfield.md), [`list`](../nodes/list.md)), [surfaces](../surfaces/index.md)
(`keyboard_interactivity`, popups, lock), [signals](signals.md) (state the handlers write),
[animation](animation.md) (press and hover motion), [capabilities](../capabilities/index.md) ([`lock`](../capabilities/lock.md),
[`polkit`](../capabilities/polkit.md), [`network`](../capabilities/network.md)).

Source: [pointer](../../renderer/src/wayland/input/pointer/mod.rs), [wheel](../../renderer/src/wayland/input/pointer/wheel.rs),
[keyboard](../../renderer/src/wayland/input/keyboard/mod.rs), [text fields](../../renderer/src/wayland/input/keyboard/plain.rs),
[secure fields](../../renderer/src/wayland/input/keyboard/secure.rs), [hit testing](../../renderer/src/layout/hit.rs),
[hover](../../renderer/src/layout/hover.rs), [scroll](../../renderer/src/layout/scene/scroll.rs).
