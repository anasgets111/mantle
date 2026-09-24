# Animation

`animate` makes a node's properties move to a new value instead of snapping: on a hover, a level
or a toggle, as a node enters or leaves the tree, or in a loop like a spinner. A *tween* is one
property moving from the value on screen to the value a new [pass](../nodes/index.md) resolves.
The engine runs every tween on compositor frames; no Lua runs between the pass that starts a tween
and its last frame.

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
`margin`, `translate`, `rotate`, `progress`, `shadow_offset` and `shadow_spread` may go negative.
`padding`, `spacing` and icon `size` have no range as plain values but tween within `[0, 8192]`.

### Layout cost

| Tween on | Each frame |
| :--- | :--- |
| `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `progress`, `shadow_*`, `content_blur`, `backdrop_blur` | Repaints; no layout pass |
| Anything else: `width`, `height`, `margin`, `padding`, `spacing`, `font_size`, … | Lays the surface out again |

Slide with `translate`, not `margin`.

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

`In` starts slow, `Out` ends slow, `InOut` does both. `Back` and `Elastic` overshoot, and the
range clamp above catches it; `Bounce` stays inside the range.

| Table easing | Meaning |
| :--- | :--- |
| `{ x1, y1, x2, y2 }` | CSS `cubic-bezier`. `x1` and `x2` in `[0, 1]`; `y` is free, so a curve may overshoot |
| `{ steps = n }` | `n` equal jumps, whole `n` in `[1, 1000]`, like CSS `steps(n, end)`: the target lands only at the end |

The same 600 ms width change under six easings. `OutBack` passes the target and comes back:

<!-- shot: frames=0..630/35 -->
```lua,shot
local go = state("go", false)

local function race(label, easing)
    return row {
        spacing = 8,
        children = {
            text { content = label, width = 80, font_size = 12, foreground = "#a6adc8" },
            rect {
                height = 12,
                radius = 6,
                background = "#89b4fa",
                width = go:map(function(on) return on and 200 or 12 end),
                animate = { width = { duration = 600, easing = easing } },
            },
        },
    }
end

return column {
    spacing = 6,
    children = {
        race("Linear", "Linear"),
        race("InOutQuad", "InOutQuad"),
        race("OutCubic", "OutCubic"),
        race("OutBack", "OutBack"),
        race("OutBounce", "OutBounce"),
        race("steps = 4", { steps = 4 }),
    },
}
```

## Spring

A spring has no duration: `stiffness` and `damping` decide how it settles. Use one for a target
that changes mid-flight, like a held volume key or a pointer-following highlight. The spring
carries its velocity into the new motion; an eased tween restarts from a standstill and lags
behind. A spring that replaces an eased tween starts at rest.

| Damping | Behaviour |
| :--- | :--- |
| `< 2 * sqrt(stiffness)` | Overshoots and rings |
| `= 2 * sqrt(stiffness)` | Critical: the fastest settle with no overshoot |
| `> 2 * sqrt(stiffness)` | Crawls in without crossing the target |

There is no `mass`: it would only rescale the other two. A spring stops within a thousandth of its
travel and never runs longer than 60 s.

The same `translate` change on three springs of `stiffness = 400`, where critical damping is 40.
The underdamped knob passes the others' resting point and swings back:

<!-- shot: frames=0..1200/40 -->
```lua,shot
local go = state("go", false)

local function knob(label, damping)
    return row {
        spacing = 8,
        children = {
            text { content = label, width = 130, font_size = 12, foreground = "#a6adc8" },
            rect {
                width = 176,
                height = 16,
                radius = 8,
                background = "#313244",
                children = {
                    rect {
                        width = 16,
                        height = 16,
                        radius = 8,
                        background = "#cba6f7",
                        translate = go:map(function(on) return { x = on and 100 or 0 } end),
                        animate = { translate = { spring = { stiffness = 400, damping = damping } } },
                    },
                },
            },
        },
    }
end

return column {
    spacing = 8,
    children = {
        knob("damping = 12, rings", 12),
        knob("damping = 40, critical", 40),
        knob("damping = 120, crawls", 120),
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

To replay a finished run, take the entry away and put it back. [`pulse`](signals.md#pulse-mark-a-change) does both in
one expression: it reads `true` for a window after its source changes.

<!-- shot: frames=0..360/30 -->
```lua,shot
local taps = state("taps", 0)
-- Three 120 ms segments: `duration` times each one, so the run takes 360 ms.
local BOUNCE = { scale = { duration = 120, easing = "OutQuad", keyframes = { 1, 1.25, 0.9, 1 } } }

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    padding = 4, -- room for the overshoot: a scaled node paints past its box
    child = button {
        width = 32,
        height = 32,
        radius = 8,
        background = "#313244",
        on_click = function() taps:set(taps:get() + 1) end,
        -- pulse is true for 400 ms after each tap: the entry appears, plays once, then goes.
        animate = pulse(taps, 400):map(function(on) return on and BOUNCE or {} end),
        children = { icon { name = "starred-symbolic", size = 16, foreground = "#CDD6F4", align_h = "Center", align_v = "Center" } },
    },
}
```

An endless spinner needs no signal. A hidden spinner stops requesting frames by itself:

<!-- shot: frames=0..950/50 -->
```lua,shot
local busy = state("busy", true)
local SPIN = { rotate = { duration = 1000, easing = "Linear", keyframes = { 0, 360 }, loops = "Infinite" } }

return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true },
    padding = 4, -- room for the corners as it turns
    child = icon {
        name = "view-refresh-symbolic",
        size = 16,
        foreground = "#CDD6F4",
        visible = busy,
        animate = SPIN,
    },
}
```

## Exit

`animate.exit` animates a child after its parent stops returning it: a notification removed from a
`list`, or a card dropped from `children`. The block holds one timing for every property, and the
values to ease to.

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
| While leaving | Painted at its last rect and scroll offset, above live siblings of the same `z`. It takes no space in the flow (siblings close up at once), though a content-sized parent keeps room for its last rect until it is gone. It takes no pointer or keyboard input and no `geometry` writes. Its subtree is frozen: a resized box does not reflow its children, and text keeps the string it was fitted to |
| Identity | A leaving node is never matched again. Returning the same `id` builds a new node beside it |
| Scope | Only the dropped child runs its block; descendants leave with it and their own blocks never run |
| Not triggered by | `visible = false`, a surface closing, or a child dropped while an ancestor was hidden |

Hiding a surface skips the exit, so drop the child from `children` and hold the surface open with
[`delay`](signals.md#delay-hold-a-value) until the exit has played. The card below slides up and
fades in on show; the shot plays the hide, down and out over 150 ms:

<!-- shot: frames=0..210/30 -->
```lua,shot
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
    children = { text { content = "Volume 42%", align_h = "Center", align_v = "Center", foreground = "#CDD6F4" } },
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
    width = 240,
    height = 64, -- room for the exit's 16 px slide
    visible = mapped,
    child = column {
        height = "Fill",
        children = shown:map(function(on) return on and { card } or {} end),
    },
}
```

## How do I…

| Task | Answer |
| :--- | :--- |
| Grow a button on hover | `scale = hover("b"):map(function(on) return on and 1.1 or 1 end)`, `hover = hover("b")` and `animate = { scale = { spring = { stiffness = 400, damping = 40 } } }` |
| Show a loading spinner | The spinner under [keyframes](#keyframes) |
| Bounce on click | The `pulse` example under [keyframes](#keyframes) |
| Slide an on-screen display in and out | The example under [exit](#exit) |
| Fade a popup in and out | [Fade a tooltip](#fade-a-tooltip-popup-in-and-out) |
| Stagger a list's entrance | [Stagger](#stagger-a-lists-entrance) |
| Slide a notification out when dismissed | [Slide out](#slide-a-notification-out) |

### Fade a tooltip popup in and out

A [popup](../surfaces/popup.md) closing skips the exit, so
switch the card out of `children` and let `delay` hold the popup open while it fades.

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
        visible = mapped,
        child = rect {
            children = over:map(function(on) return on and { card } or {} end),
        },
    },
}
```

### Stagger a list's entrance

Give each item a `delay` that grows with its index. `delay` holds
the `from` value, so a card waits invisible for its turn.

<!-- shot: frames=0..420/30 -->
```lua,shot
local go = state("go", false)
local titles = { "Battery low", "Update ready", "Download complete" }

local function card(index, title)
    local wait = (index - 1) * 80
    return rect {
        width = 200,
        padding = 10,
        radius = 8,
        background = "#1e1e2e",
        opacity = 1,
        translate = { x = 0 },
        animate = {
            opacity = { duration = 200, delay = wait, from = 0 },
            translate = { duration = 200, delay = wait, easing = "OutCubic", from = { x = -24 } },
        },
        children = { text { content = title, foreground = "#cdd6f4" } },
    }
end

return column {
    spacing = 6,
    children = go:map(function(on)
        local cards = {}
        for index, title in ipairs(on and titles or {}) do
            cards[index] = card(index, title)
        end
        return cards
    end),
}
```

### Slide a notification out

Removing an item from a keyed [`list`](../nodes/list.md) makes it
leave. The remaining cards close up at once; only the leaving one moves.

<!-- shot: frames=0..210/30 -->
```lua,shot
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
        padding = 12,
        radius = 12,
        background = "#1e1e2e",
        border_width = 1,
        border_color = "#45475a",
        on_click = function() dismiss(title) end,
        animate = { exit = { duration = 200, easing = "InCubic", opacity = 0, translate = { x = 300 } } },
        children = { text { content = title, foreground = "#cdd6f4" } },
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
| A held key makes an eased value trail behind | Use a `spring`; it keeps its velocity through each new target |
| A keyframe run plays once and never again | Same list, same run. Toggle the entry off and on, for example with `pulse` |
| A `pulse`-driven run is cut short | Removing the entry snaps the property. Make the window at least `delay` plus every segment's `duration` times `loops` |
| A second click inside the `pulse` window does not replay the run | The click only extends the window; the entry never leaves, so the run does not restart |
| Sliding with `margin` stutters on a large surface | Tween `translate`: it skips layout |
| `width` will not overshoot below `0` with `OutBack` | The property's range clamps every frame. Use `margin` or `translate` for motion that must go negative |

See also: [signals](signals.md) (`pulse`, `delay`, `hover`), [nodes](../nodes/index.md) (properties and
identity), [input](input.md) (hover and clicks that drive motion), [paint](paint.md) (what the
painted properties draw).

Source: [animate](../../renderer/src/layout/node/animate/mod.rs), [easing](../../renderer/src/layout/node/animate/easing.rs),
[spring](../../renderer/src/layout/node/animate/spring.rs), [keyframes](../../renderer/src/layout/node/animate/sequence.rs),
[leaving nodes](../../renderer/src/layout/scene/tick.rs), [range clamps](../../renderer/src/layout/node/style/mod.rs).
