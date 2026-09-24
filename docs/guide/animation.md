# Animation

`animate` makes a node's properties ease to a new value instead of snapping. Add it whenever a
signal-driven change (a hover, a level, a toggle) should move rather than jump, when a node should
fade in or out as the tree gains or drops it, or when something loops, like a spinner. The engine
runs every tween on compositor frames; no Lua runs between the pass that starts a tween and its
last frame. A *tween* is one property moving from the value on screen to the value a new
[pass](../nodes/index.md) resolves.

```lua
local open = hover("tray")

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = row {
        hover = open,
        height = 28,
        radius = 14,
        background = "#313244",
        width = open:map(function(on) return on and 160 or 28 end),
        spacing = open:map(function(on) return on and 6 or 0 end),
        animate = { width = { duration = 200, easing = "OutCubic" }, spacing = 200 },
        children = {
            icon { name = "network-wireless-symbolic", size = 16, margin = 6 },
            icon { name = "bluetooth-active-symbolic", size = 16, margin = 6 },
        },
    },
}
```

## How a tween starts

`animate` is a table from property names to entries. When a pass resolves a different value for a
named property, the node moves from the value on screen to the new one. A pass that re-resolves the
same value leaves a running tween alone, so an unrelated signal does not restart the motion.
`animate` itself may be a signal (`animate = shown:map(...)`), but the entries inside it are plain
values: a signal nested in an entry does not resolve.

| Situation | Result |
| :--- | :--- |
| Number, `"NN%"` size, `"#rrggbb[aa]"` colour, number edge table `{ top, right, bottom, left }`, `{ x, y }` table | Tweens against a new value of the same shape. A missing edge or axis reads as `0` (`1` for `scale`, `0.5` for `origin`) |
| `"Fill"`, booleans, strings that are not colours, per-edge colour tables, gradients, or a change of shape (`2` to `{ x = 2 }`, `"50%"` to `"Fill"`) | Snaps |
| New node, or a property the node did not set last pass | Starts at the entry's `from`, else snaps. `from` needs the node to set the property itself |
| Target changes mid-flight | Eased and keyframe motion start over from the value on screen. A spring keeps its velocity ([spring](#spring)) |
| Property removed from `animate` | Its tween stops and the property snaps to the resolved value |
| Hidden subtree (`visible = false`) | Tweens freeze and request no frames; they settle when it shows again |
| `z`, `animate`, or a name the node kind does not accept | Refused: the pass fails with an error naming the entry |

### What can animate

Any property the node's kind accepts ([nodes](../nodes/index.md)) can be named; the value's
shape decides whether it moves. The ones that do:

| Shape | Properties |
| :--- | :--- |
| Number | `width`, `height`, `min_*`, `max_*`, `padding`, `margin`, `opacity`, `scale`, `rotate`, `shadow_blur`, `shadow_spread`, `content_blur`; on boxes `radius`, `border_width`, `backdrop_blur`; `spacing` on `row`, `column`, `list`; `font_size` on `text` and `textfield`; `size` on `icon`; `progress` on `shader` |
| `"NN%"` | `width`, `height` |
| Colour | `background`, `border_color` (single colour), `shadow_color`, `foreground` |
| `{ top, right, bottom, left }` | `padding`, `margin`, `border_width` as tables |
| `{ x, y }` | `translate`, `scale`, `origin`, `shadow_offset` |

An `image` crossfading between sources uses its own `transition` property, not `animate`
([image](../nodes/image.md)).

### Range clamp

Every frame's value is clamped to the property's [range](runtime.md#limits-and-budgets), which
catches overshoot from `Back`, `Elastic`, a Bezier with `y` outside `[0, 1]`, or a spring. Only
`margin`, `translate`, `rotate`, `progress`, `shadow_offset` and `shadow_spread` may go negative;
`padding`, `spacing` and icon `size`, unbounded as plain values, tween within `[0, 8192]`.

**Cost.** `opacity`, colours (`background`, `border_color`, `foreground`, `shadow_color`),
`radius`, `translate`, `scale`, `rotate`, `origin`, `progress`, the `shadow_*` numbers,
`content_blur` and `backdrop_blur` move without a layout pass. Tweening `width`, `height`,
`margin`, `padding` or `spacing` lays the surface out again on every frame, so slide with
`translate` rather than `margin`.

## Entry keys

An entry is a bare number (a duration in ms with the default easing) or a table. Every entry picks
one of three motions: eased (`duration`), keyframes (`keyframes` + `duration`) or spring
(`spring`).

| Key | Values | Rules |
| :--- | :--- | :--- |
| `duration` | Whole ms, `[1, 60000]` | Required unless `spring` is set. With `keyframes` it is the default length of each segment |
| `easing` | A name, `{ x1, y1, x2, y2 }`, or `{ steps = n }` | Default `"InOutQuad"`. Not with `spring` |
| `delay` | Whole ms, `[0, 60000]` | Holds the start value first, like CSS `transition-delay`. Offsets a keyframe run once, not each loop |
| `from` | A value of the property's shape | Start value for a property with nothing on screen yet. Refused with `keyframes` |
| `spring` | `{ stiffness, damping }` | `stiffness` in `(0, 100000]`, `damping` in `(0, 10000]`, both required. Refuses `duration`, `easing`, `keyframes` and `loops` |
| `keyframes` | At least 2 frames | See [keyframes](#keyframes) |
| `loops` | Whole count `[1, 10000]` or `"Infinite"` | Default 1. Only with `keyframes` |

A `duration` or `delay` that is not a number (`"200"`) is refused rather than read as absent.

**Easing names.** A name is case-sensitive; an unknown one is refused with the list.

| Family | Names |
| :--- | :--- |
| Linear | `Linear` |
| Quad, Cubic, Quart, Quint | `InQuad`, `OutQuad`, `InOutQuad`, `InCubic`, `OutCubic`, `InOutCubic`, `InQuart`, `OutQuart`, `InOutQuart`, `InQuint`, `OutQuint`, `InOutQuint` |
| Sine, Expo, Circ | `InSine`, `OutSine`, `InOutSine`, `InExpo`, `OutExpo`, `InOutExpo`, `InCirc`, `OutCirc`, `InOutCirc` |
| Back, Elastic, Bounce | `InBack`, `OutBack`, `InOutBack`, `InElastic`, `OutElastic`, `InOutElastic`, `InBounce`, `OutBounce`, `InOutBounce` |

`In` starts slow, `Out` ends slow, `InOut` does both.
`Back` and `Elastic` overshoot, and the range clamp above catches it; `Bounce` stays inside the range.

| Table easing | Meaning |
| :--- | :--- |
| `{ x1, y1, x2, y2 }` | CSS `cubic-bezier`. `x1` and `x2` in `[0, 1]`; `y` is free, so a curve may overshoot |
| `{ steps = n }` | `n` equal jumps, whole `n` in `[1, 1000]`, like CSS `steps(n, end)`: the target lands only at the end |

## Spring

A spring has no duration: `stiffness` and `damping` decide how it settles. Its real advantage is
retargeting. When the value changes mid-flight (a held volume key, a pointer-following highlight),
the spring carries its current velocity into the new motion, while an eased tween restarts from a
standstill and lags behind. A spring that replaces an eased tween starts at rest.

| Damping | Behaviour |
| :--- | :--- |
| `< 2 * sqrt(stiffness)` | Overshoots and rings |
| `= 2 * sqrt(stiffness)` | Critical: the fastest settle with no overshoot |
| `> 2 * sqrt(stiffness)` | Crawls in without crossing the target |

There is no `mass`: it would only rescale the other two. A spring stops within a thousandth of its
travel and never runs longer than 60 s.

```lua
local hovered = hover("launch")

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = button {
        width = 40,
        height = 40,
        radius = 10,
        background = "#313244",
        hover = hovered,
        on_click = function() process.detach("fuzzel", {}) end,
        scale = hovered:map(function(on) return on and 1.1 or 1 end),
        -- 2 * sqrt(400) = 40: critical damping, fast with no overshoot.
        animate = { scale = { spring = { stiffness = 400, damping = 40 } } },
        children = { icon { name = "system-search-symbolic", size = 16, align_h = "Center", align_v = "Center" } },
    },
}
```

## Keyframes

A `keyframes` entry walks a list of values instead of easing to the resolved one. While it runs, it
owns the property: the value the pass resolves is ignored.

| Rule | Detail |
| :--- | :--- |
| Frames | A bare value, or `{ value = v, duration = ms, easing = e }` overriding the entry's `duration` and `easing` for the segment that arrives at it |
| First frame | Where the run starts; its own `duration` and `easing` are never read |
| Jump | A frame with `duration = 0` (allowed only on a frame) cuts straight to its value |
| Hold | A segment between two equal values holds still for its duration |
| List | At least 2 frames, no holes (`{ [1] = 0, [3] = 1 }` is refused), at least one segment that takes time |
| End | A counted run holds its last frame as long as the entry stays. An `"Infinite"` run never ends |
| Continuity | The same list on the next pass is the same run; any change to the frames, timing or `loops` starts a new run from the first frame |

To replay a finished run, take the entry away and put it back. [`pulse`](signals.md) does both in
one expression: it reads `true` for a window after its source changes.

```lua
local taps = state("taps", 0)
local BOUNCE = { scale = { duration = 400, easing = "OutQuad", keyframes = { 1, 1.25, 0.9, 1 } } }

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = button {
        width = 32,
        height = 32,
        radius = 8,
        background = "#313244",
        on_click = function() taps:set(taps:get() + 1) end,
        -- pulse is true for 400 ms after each tap: the entry appears, plays once, then goes.
        animate = pulse(taps, 400):map(function(on) return on and BOUNCE or {} end),
        children = { icon { name = "starred-symbolic", size = 16, align_h = "Center", align_v = "Center" } },
    },
}
```

An endless spinner needs no signal. A hidden spinner stops requesting frames by itself:

```lua
local busy = state("busy", true)
local SPIN = { rotate = { duration = 1000, easing = "Linear", keyframes = { 0, 360 }, loops = "Infinite" } }

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    child = icon {
        name = "view-refresh-symbolic",
        size = 16,
        visible = busy,
        animate = SPIN,
    },
}
```

## Exit

`animate.exit` animates a child after its parent stops returning it: a notification removed from a
`list`, or a card dropped from `children`. The block holds one shared timing and the values to
ease to.

```lua
exit = { duration = 150, easing = "InQuad", opacity = 0, translate = { y = 16 } }
```

| Rule | Detail |
| :--- | :--- |
| Keys | `duration` or `spring`, plus optional `easing` and `delay`, all as in [entry keys](#entry-keys). Every other key is a property name and its target value |
| Checked | On every pass while the node is still in the tree, so a typo fails before the node leaves. A block with no targets is a legal no-op; one with targets needs `duration` or `spring` |
| Start value | The value on screen. A property never set starts at its identity: `1` for `opacity` and `scale`, `0.5` for `origin`, `"0%"` for a percent, the target colour at alpha 0 for a colour, `0` otherwise |
| Running tweens | Stop where they are. The exit block alone decides how long the node lives |
| What moves | Everything painted: `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `shadow_*`, blurs, `progress`, and pixel `width`/`height`. `margin`, `padding` and `spacing` change nothing visible |
| While leaving | Painted at its last rect and scroll offset, above live siblings of the same `z`. It takes no space (siblings close up at once), no pointer or keyboard input and no `geometry` writes. Its subtree is frozen: a resized box does not reflow its children, and text keeps the string it was fitted to |
| Identity | A leaving node is never matched again. Returning the same `id` builds a new node beside it |
| Scope | Only the dropped child runs its block; descendants leave with it and their own blocks never run |
| Not triggered by | `visible = false`, a surface closing, or a child dropped while an ancestor was hidden |

Because hiding a surface skips the exit, drop the child from `children` and hold the surface open
with [`delay`](signals.md) until the exit has played:

```lua
local shown = state("osd_shown", false)
-- Keep the surface mapped 150 ms past `shown`, so the card's exit can play.
local mapped = computed({ shown, delay(shown, 150) }, function(now, was)
    return now == true or was == true
end)

local card = rect {
    width = 240,
    height = 48,
    radius = 12,
    background = "#1e1e2ee6",
    opacity = 1,
    translate = { y = 0 },
    animate = {
        opacity = { duration = 200, from = 0 },
        translate = { duration = 200, easing = "OutCubic", from = { y = 16 } },
        exit = { duration = 150, easing = "InQuad", opacity = 0, translate = { y = 16 } },
    },
}

return panel {
    id = "osd",
    layer = "Overlay",
    anchor = { bottom = true },
    width = 240, -- fixed: a leaving card takes no space
    height = 64,
    visible = mapped,
    child = column {
        children = shown:map(function(on) return on and { card } or {} end),
    },
}
```

## How do I…

| Task | Answer |
| :--- | :--- |
| Grow a button on hover | The [spring](#spring) example |
| Show a loading spinner | The spinner under [keyframes](#keyframes) |
| Bounce on click | The `pulse` example under [keyframes](#keyframes) |
| Slide an on-screen display in and out | The example under [exit](#exit) |
| Fade a popup in and out | Below |
| Slide a notification out when dismissed | Below |

**Fade a tooltip popup in and out.** A [popup](../surfaces/popup.md) closing skips the exit, so the
card is switched out of `children` and `delay` holds the popup open while it fades. The popup has a
fixed size because the leaving card takes no space.

```lua
local over = hover("clock")
-- Stay mapped 120 ms after the pointer leaves, so the card's exit can play.
local mapped = computed({ over, delay(over, 120) }, function(now, was)
    return now == true or was == true
end)

local card = rect {
    width = 180,
    height = 32,
    radius = 6,
    background = "#1e1e2e",
    opacity = 1,
    animate = { opacity = { duration = 120, from = 0 }, exit = { duration = 120, opacity = 0 } },
    children = { text { content = "Thursday, 24 September", padding = 8 } },
}

return {
    panel { id = "bar", layer = "Top", anchor = { top = true }, child = text { content = "12:30", padding = 8, hover = over } },
    popup {
        id = "clock_tooltip",
        parent = "bar",
        anchor_rect = hover_rect("clock"),
        anchor = "Bottom",
        gravity = "Bottom",
        grab = false,
        width = 180, -- fixed: the leaving card takes no space
        height = 32,
        visible = mapped,
        child = rect { children = over:map(function(on) return on and { card } or {} end) },
    },
}
```

**Slide a notification out.** Removing an item from a keyed [`list`](../nodes/list.md) makes it
leave. The remaining cards close up at once; only the leaving one moves.

```lua
local notes = state("notes", { "Battery low", "Update ready", "Download complete" })

local function dismiss(title)
    local kept = {}
    for _, other in ipairs(notes:get()) do
        if other ~= title then kept[#kept + 1] = other end
    end
    notes:set(kept)
end

local function card(title)
    return button {
        width = 280,
        padding = 10,
        radius = 8,
        background = "#1e1e2e",
        on_click = function() dismiss(title) end,
        animate = { exit = { duration = 200, easing = "InCubic", opacity = 0, translate = { x = 300 } } },
        children = { text { content = title } },
    }
end

return panel {
    id = "notifications",
    layer = "Overlay",
    anchor = { top = true, right = true },
    width = 300,
    height = 400,
    child = list { spacing = 8, source = notes, itemfn = card, key = function(title) return title end },
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A node's first value snaps; nothing fades in | Give the entry `from` |
| `from` does nothing | The node must set the property too: `opacity = 1` beside `opacity = { from = 0, ... }` |
| An exit never plays | Exit runs only when the parent stops returning the child. Switch `children`, and keep the surface up with `delay` |
| A content-sized surface collapses while its child exits | A leaving node takes no space. Give the surface a fixed size |
| A held key makes an eased value trail behind | Use a `spring`; it keeps its velocity through each new target |
| A keyframe run plays once and never again | Same list, same run. Toggle the entry off and on, for example with `pulse` |
| A `pulse`-driven run is cut short, or a second click does not replay it | Removing the entry snaps the property, so make the window at least `delay + duration × loops`. A click inside the window only extends it; the entry never leaves, so the run does not restart |
| Sliding with `margin` stutters on a large surface | Tween `translate`: it skips layout |
| `width` will not overshoot below `0` with `OutBack` | The property's range clamps every frame. Use `margin` or `translate` for motion that must go negative |
| A typo in `animate` passes `mantle check` | `check` does not resolve nodes; the running shell refuses it on the first pass that resolves the node. See [cli](cli.md) |

See also: [signals](signals.md) (`pulse`, `delay`, `hover`), [nodes](../nodes/index.md) (properties and
identity), [input](input.md) (hover and clicks that drive motion), [paint](paint.md) (what the
painted properties draw).

Source: [animate](../../renderer/src/layout/node/animate/mod.rs), [easing](../../renderer/src/layout/node/animate/easing.rs),
[spring](../../renderer/src/layout/node/animate/spring.rs), [keyframes](../../renderer/src/layout/node/animate/sequence.rs),
[leaving nodes](../../renderer/src/layout/scene/tick.rs), [range clamps](../../renderer/src/layout/node/style/mod.rs).
