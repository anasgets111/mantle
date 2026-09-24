# button

A box that takes the pointer: clicks, left-button drags and the wheel. It stacks its children like a
[`rect`](rect.md), so put a `row` inside for an icon and a label side by side. A `button` is the only
kind that takes these events; the rules for which button wins, cancelling and drag end are on
[input](../guide/input.md#pointer).

A volume chip: left click mutes, the wheel changes the level.

```lua
local muted = state("muted", false)
local volume = state("volume", 0.5)

local mute_button = button {
    padding = { left = 10, right = 10, top = 6, bottom = 6 },
    radius = 8,
    background = muted:map(function(m) return m and "#F38BA8" or "#313244" end),
    on_click = function(_, which)
        if which == "left" then muted:set(not muted:get()) end
    end,
    on_wheel = function(_, steps)
        volume:set(math.max(0, math.min(1, volume:get() + steps * 0.05)))
    end,
    children = { row { spacing = 6, align_v = "Center", children = {
        icon { name = "audio-volume-high-symbolic", size = 16, foreground = "#CDD6F4", align_v = "Center" },
        text { content = volume:map(function(v) return string.format("%d%%", math.floor(v * 100 + 0.5)) end),
               align_v = "Center" },
    } } },
}
```

## Properties

`button` takes the [common](index.md#common-properties) and [box](index.md#box-properties)
properties, plus the ones below. `rect` in the callbacks is the button's surface-local laid-out box
`{ x, y, width, height }`, before transforms.

| Property | Values | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `children` | As on [`rect`](rect.md) | None | Stacked |
| `on_click(rect, button)` | Function; `button` is `"left"`, `"right"` or `"middle"` | None | Fires on release over the same button that was pressed, with the same mouse button |
| `on_drag(rect, pointer, phase)` | Function; `pointer` is `{ x, y }` relative to the button, unclamped; `phase` is `"start"`, `"move"` or `"end"` | None | Left-button drag. `"start"` on press, `"end"` on release (before `on_click`) or when the pointer leaves the surface |
| `on_wheel(rect, steps)` | Function; `steps` is wheel notches, positive away from the user, fractional on touchpads | None | Vertical wheel. The innermost handler or scroll container wins |
| `submit` | `true` | `false` | A click also submits the armed [secure field](../guide/input.md#secure-fields), like Enter. Works without `on_click` and runs before it |

A `button` with none of `on_click`, `on_drag`, `on_wheel` and `submit = true` does not take the
pointer: clicks fall through to what is under it, and it sets no cursor. With one, the cursor defaults to
`"pointer"`.

## How do I…

| Task | Answer |
| :--- | :--- |
| Toggle something on click | The example above |
| Open a menu on right click | Check `button == "right"` and open a [popup](../surfaces/popup.md) at `rect` ([input](../guide/input.md#how-do-i)) |
| Make a slider | `on_drag` for the value, `on_wheel` for steps ([input](../guide/input.md#pointer)) |
| Show press or hover feedback | Tween `scale` or `background` on a `hover` signal ([paint](../guide/paint.md#card-that-lifts-on-hover)) |
| Submit a password with a button | `submit = true` ([secure fields](../guide/input.md#secure-fields)) |
| Change the cursor | `cursor = "grab"` or any [cursor name](index.md#cursor-names) |
| Make a whole row clickable | Make the `button` the row's parent, `width = "Fill"`, with a `row` inside |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| Icon and label overlap | A `button` stacks its children. Put a `row` inside |
| A click lands on the node behind the button | The button has no handler and no `submit = true`, so it is transparent to the pointer |
| A click is lost when the button moves or resizes on press | A click cancels if the laid-out box moved between press and release. Give press feedback with `scale` or `translate`, not `width` or `margin` |
| `on_click` never fires for a link inside the button | A `text` with [`on_link`](text.md) takes the click on a link run first |
| A press on a `textfield` inside the button does not click | Presses on a field go to the field |

See also: [input](../guide/input.md), [rect](rect.md), [surfaces: popup](../surfaces/popup.md).

Source: [vocabulary](../../renderer/src/lua/nodes.rs), [pointer dispatch](../../renderer/src/wayland/input/pointer/mod.rs),
[hit testing](../../renderer/src/layout/hit.rs), [takes_pointer](../../renderer/src/layout/scene/mod.rs).
