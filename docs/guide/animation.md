# Animation

`animate` makes a node's properties move to a new value instead of snapping: on a hover, a level
or a toggle, as a node enters or leaves the tree, or in a loop like a spinner. A *tween* is one
property moving from the value on screen to the value a new [pass](../nodes/index.md) resolves.
The engine runs every tween on its own surface's compositor frames, so a panel on a 60 Hz output
moves at 60 Hz beside one at 165 Hz; no Lua runs between the pass that starts a tween and its last
frame.

Six knobs slide 200 px over 600 ms. Each uses a different easing; `"out_back"` passes the end and
comes back:

<!-- shot-alt: Six coloured knobs slide along their tracks under different easings; the out_back knob overshoots the end and settles back. -->
<!-- shot: frames=0..900/30 -->
```lua,shot
local go = state("go", false)

local function lane(label, easing, color)
    return row {
        spacing = 12,
        children = {
            text { content = label, width = 88, font_size = 13, foreground = "#bac2de" },
            rect {
                width = 216,
                height = 16,
                radius = 8,
                background = "#313244",
                children = {
                    rect {
                        width = 16,
                        height = 16,
                        radius = 8,
                        background = color,
                        translate = go:map(function(on) return { x = on and 200 or 0 } end),
                        animate = { translate = { duration = 600, easing = easing } },
                    },
                },
            },
        },
    }
end

return column {
    padding = 16,
    spacing = 10,
    radius = 12,
    background = "#1e1e2e",
    children = {
        text { content = "Six easings, 600 ms", font_size = 14, font_weight = 700, foreground = "#cdd6f4" },
        lane("linear", "linear", "#89b4fa"),
        lane("in_out_quad", "in_out_quad", "#cba6f7"),
        lane("out_cubic", "out_cubic", "#a6e3a1"),
        lane("out_back", "out_back", "#fab387"),
        lane("out_bounce", "out_bounce", "#f38ba8"),
        lane("steps = 4", { steps = 4 }, "#f9e2af"),
    },
}
```

Run `mantle set go true` to start the race in your shell, then `mantle set go false` to reset it.

## How a tween starts

`animate` is a table from property names to entries. When a pass resolves a different value for a
named property, the node moves from the value on screen to the new one. A pass that re-resolves the
same value leaves a running tween alone, so an unrelated signal does not restart the motion.
`animate` itself may be a signal (`animate = shown:map(...)`), and so may an entry or a field
inside one. An entry's timing read from a signal applies to the run already under way: a new
`duration` mid-tween moves where the run is.

| Situation | Result |
| :--- | :--- |
| Number, `"NN%"` size, `"#rrggbb[aa]"` colour, number edge table `{ top, right, bottom, left }`, corner table `{ top_left, top_right, bottom_right, bottom_left }`, `{ x, y }` table | Tweens against a new value of the same shape. A missing edge, corner or axis reads as `0` (`1` for `scale`, `0.5` for `origin`) |
| A `path`'s `commands` | Tweens point by point against a list with the same ops and `hole` flags in the same order; any other list snaps. A spring retargeted mid-flight restarts from rest |
| `"fill"`, booleans, strings that are not colours, per-edge colour tables, gradients, or a change of shape (`2` to `{ x = 2 }` or `{ top_left = 2 }`, `"50%"` to `"fill"`) | Snaps |
| A `width` or `height` you did not set (sized to its content) with an eased entry | After each pass the engine measures the content and eases the box from its size on screen to the measured one, laying siblings and children out at every frame. Children are cut to the moving box only under `clip = "box"` or `"rounded"`, or in a scroll viewport or surface. A first layout, a spring and keyframe entries snap. A root axis the compositor sizes (`window`, `lock`, a `panel` anchored at both opposite edges) never eases |
| New node, or a property the node did not set last pass | Starts at the entry's `from`, else snaps. `from` needs the node to set the property itself |
| Target changes mid-flight | An eased tween returning to its prior endpoint shortens the run according to the progress already covered. Other eased targets and keyframe entries start over from the value on screen. A spring keeps its velocity ([spring](#spring)) |
| Property removed from `animate` | Its tween stops and the property snaps to the resolved value |
| Hidden subtree (`visible = false`) | Tweens freeze and request no frames; they settle when it shows again. Showing the subtree cancels its moves |
| `z`, `animate`, or a name the node kind does not accept | Refused: the pass fails with an error naming the entry. `move`, `exit` and `scroll` are special entries |

### What can animate

Any property the node's kind accepts ([nodes](../nodes/index.md)) can be named; the value's
shape decides whether it moves. The ones that do:

| Shape | Properties |
| :--- | :--- |
| Number | `width`, `height`, `min_*`, `max_*`, `padding`, `margin`, `opacity`, `scale`, `rotate`; on boxes `radius`, `border_width`; `spacing` on `row`, `column`, `list`; `font_size` on `text` and `textfield`; `size` on `icon`; `progress` on `shader`; `stroke_width` on `path` |
| `"NN%"` | `width`, `height` |
| Colour | `background`, `border_color` (single colour), `foreground`; `fill` and `stroke` on `path` |
| `{ top, right, bottom, left }` | `padding`, `margin`, `border_width` as tables |
| `{ top_left, top_right, bottom_right, bottom_left }` | `radius` as a table |
| `{ x, y }` | `translate`, `scale`, `origin` |
| `{ blur, saturate, brightness, contrast, backdrop = { .. } }` | `effect`; a key only one side sets tweens from or to its off value, `0` for a blur and `1` for a colour filter |
| Shadow list | `shadows`, layer by layer ([layered shadows](paint.md#layered-shadows)) |
| Path commands | `commands` on `path` ([morphing](../nodes/path.md)) |

An `image` crossfading between sources uses its own `transition` property, not `animate`
([image](../nodes/image.md)).

### Range clamp

Every frame's value is clamped to the property's [range](runtime.md#limits-and-budgets), which
catches overshoot from `Back`, `Elastic`, a Bezier with `y` outside `[0, 1]`, or a spring. Only
`margin`, `translate`, `rotate`, `progress` and a `shadows` layer's `offset` and `spread` may go negative.
`padding`, `spacing` and icon `size` have no range as plain values but tween within `[0, 8192]`.
Path coordinates stay within `[-8192, 8192]` and arc radii at 0 or more.

### Layout cost

| Tween on | Each frame |
| :--- | :--- |
| `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `progress`, `commands`, `fill`, `stroke`, `stroke_width`, `shadows`, `effect` | Repaints; no layout pass |
| Anything else: `width`, `height`, `margin`, `padding`, `spacing`, `font_size`, … | Lays the surface out again |

Slide with `translate` and grow on hover with `scale` when surrounding nodes should stay put.
Both skip layout; `width` and `margin` lay out the surface on every animation frame.

### Lua cost

Let `animate` drive motion. A `timer` that sets a signal every frame runs Lua and resolves the
surface on every tick. `animate` computes each frame in the engine, about 20 times cheaper, at the
display's refresh rate.

A progress ring that fills forever, with no Lua per frame:

<!-- shot-alt: A card reading "Syncing photos" beside a blue ring that fills clockwise over its grey track, then starts over. -->
<!-- shot: frames=0..1140/60 -->
```lua,shot
local function ring(sweep)
    return { { op = "A", points = { 16, 16, 13, -90, sweep } } }
end

return row {
    padding = 16,
    spacing = 12,
    radius = 12,
    background = "#1e1e2e",
    children = {
        rect {
            width = 32,
            height = 32,
            align_v = "center",
            children = {
                path { width = 32, height = 32, stroke = "#313244", stroke_width = 4, commands = ring(360) },
                path {
                    width = 32,
                    height = 32,
                    stroke = "#89b4fa",
                    stroke_width = 4,
                    commands = ring(0),
                    animate = {
                        commands = { duration = 1200, easing = "linear", keyframes = { ring(0), ring(360) }, loops = "infinite" },
                    },
                },
            },
        },
        text { content = "Syncing photos", font_size = 14, foreground = "#cdd6f4", align_v = "center" },
    },
}
```

Not a timer that redraws it 30 times a second:

```lua
local tick = state("tick", 0)
interval(33, function() tick:set(tick:get() + 1) end)

return path {
    width = 32,
    height = 32,
    stroke = "#89b4fa",
    stroke_width = 4,
    commands = tick:map(function(n) return { { op = "A", points = { 16, 16, 13, -90, n % 36 * 10 } } } end),
}
```

Live data, like a volume level, still comes from a signal: give it an `animate` entry and the
engine eases between updates.

## Entry keys

An entry is a bare number (a duration in ms with the default easing) or a table. Every entry picks
one of three motions: eased (`duration`), keyframes (`keyframes` + `duration`) or spring
(`spring`). Beside `keyframes`, a `spring` is each segment's curve instead.

| Key | Values | Rules |
| :--- | :--- | :--- |
| `duration` | Whole ms, `[1, 60000]` | Required unless `spring` is set. With `keyframes` it is the default length of each segment |
| `easing` | A name, `{ x1, y1, x2, y2 }`, or `{ steps = n }` | Default `"in_out_quad"`. Not with `spring` |
| `delay` | Whole ms, `[0, 60000]` | Holds the start value first, like CSS `transition-delay`. Offsets a keyframe run once, not each loop |
| `from` | A value of the property's shape | Start value for a property with nothing on screen yet. Refused with `keyframes` |
| `spring` | `{ stiffness, damping }` | `stiffness` in `(0, 100000]`, `damping` in `(0, 10000]`, both required. Refuses `easing`, and `duration` and `loops` unless `keyframes` is set ([spring segments](#spring-segments)) |
| `keyframes` | At least 2 frames | See [keyframes](#keyframes) |
| `loops` | Whole count `[1, 10000]` or `"infinite"` | Default 1. Only with `keyframes` |

A `duration` or `delay` that is not a number (`"200"`) is refused rather than read as absent.

**Easing names.** A name is case-sensitive; an unknown one is refused with the list.

| Family | Names |
| :--- | :--- |
| Linear | `"linear"` |
| Quad, Cubic, Quart, Quint | `"in_quad"`, `"out_quad"`, `"in_out_quad"`, `"in_cubic"`, `"out_cubic"`, `"in_out_cubic"`, `"in_quart"`, `"out_quart"`, `"in_out_quart"`, `"in_quint"`, `"out_quint"`, `"in_out_quint"` |
| Sine, Expo, Circ | `"in_sine"`, `"out_sine"`, `"in_out_sine"`, `"in_expo"`, `"out_expo"`, `"in_out_expo"`, `"in_circ"`, `"out_circ"`, `"in_out_circ"` |
| Back, Elastic, Bounce | `"in_back"`, `"out_back"`, `"in_out_back"`, `"in_elastic"`, `"out_elastic"`, `"in_out_elastic"`, `"in_bounce"`, `"out_bounce"`, `"in_out_bounce"` |

`in_` starts slow, `out_` ends slow, `in_out_` does both. The `back` and `elastic` families
overshoot, and the range clamp above catches it; `bounce` stays inside the range.

| Table easing | Meaning |
| :--- | :--- |
| `{ x1, y1, x2, y2 }` | CSS `cubic-bezier`. `x1` and `x2` in `[0, 1]`; `y` is free, so a curve may overshoot |
| `{ steps = n }` | `n` equal jumps, whole `n` in `[1, 1000]`, like CSS `steps(n, end)`: the target lands only at the end |

## Spring

A spring has no duration: `stiffness` and `damping` decide how it settles. Use one for a target
that changes mid-flight, like a held volume key or a pointer-following highlight. The spring
carries its velocity into the new motion; an eased tween restarts from a standstill and lags
behind. The carried velocity is capped at 100 times the new distance per second, so a target set
almost where the value already is cannot fling it past. A spring that replaces an eased tween starts
at rest.

| Damping | Behaviour |
| :--- | :--- |
| `< 2 * sqrt(stiffness)` | Overshoots and rings |
| `= 2 * sqrt(stiffness)` | Critical: the fastest settle with no overshoot |
| `> 2 * sqrt(stiffness)` | Crawls in without crossing the target |

There is no `mass`: it would only rescale the other two. A spring stops within a thousandth of its
travel and never runs longer than 60 s.

The same `translate` change on three springs of `stiffness = 400`, where critical damping is 40.
The underdamped knob passes the others' resting point and swings back:

<!-- shot-alt: Three knobs spring along their tracks: the underdamped one overshoots and rings, the critical one settles cleanly, the overdamped one crawls in. -->
<!-- shot: frames=0..1500/50 -->
```lua,shot
local go = state("go", false)

local function lane(label, damping, color)
    return row {
        spacing = 12,
        children = {
            text { content = label, width = 150, font_size = 13, foreground = "#bac2de" },
            rect {
                width = 216,
                height = 16,
                radius = 8,
                background = "#313244",
                children = {
                    rect {
                        width = 16,
                        height = 16,
                        radius = 8,
                        background = color,
                        translate = go:map(function(on) return { x = on and 160 or 0 } end),
                        animate = { translate = { spring = { stiffness = 400, damping = damping } } },
                    },
                },
            },
        },
    }
end

return column {
    padding = 16,
    spacing = 10,
    radius = 12,
    background = "#1e1e2e",
    children = {
        text { content = "Springs, stiffness 400", font_size = 14, font_weight = 700, foreground = "#cdd6f4" },
        lane("damping = 12, rings", 12, "#cba6f7"),
        lane("damping = 40, critical", 40, "#a6e3a1"),
        lane("damping = 120, crawls", 120, "#fab387"),
    },
}
```

## Keyframes

A `keyframes` entry walks a list of values instead of easing to the resolved one. While it runs, it
owns the property: the value the pass resolves is ignored.

| Rule | Detail |
| :--- | :--- |
| Frames | A bare value, or `{ value = v, duration = ms, easing = e }` overriding the entry's `duration` and `easing` for the segment that arrives at it. `spring = { stiffness, damping }` in place of `easing` springs that segment |
| First frame | Where the run starts; its own `duration` and `easing` are never read |
| Jump | A frame with `duration = 0` (allowed only on a frame) cuts straight to its value |
| Hold | A segment between two equal values holds still for its duration |
| List | At least 2 frames, no holes (`{ [1] = 0, [3] = 1 }` is refused), at least one segment that takes time |
| End | A counted run holds its last frame as long as the entry stays. An `"infinite"` run never ends |
| Continuity | The same list on the next pass is the same run; any change to the frames, timing or `loops` starts a new run from the first frame |

To replay a finished run, take the entry away and put it back. [`pulse`](signals.md#pulse-mark-a-change) does both in
one expression: it reads `true` for a window after its source changes.

<!-- shot-alt: A yellow star button swells, dips below its size, and settles in a short bounce. -->
<!-- shot: frames=0..540/30 -->
```lua,shot
local taps = state("taps", 0)
-- Three 120 ms segments: `duration` times each one, so the run takes 360 ms.
local BOUNCE = { scale = { duration = 120, easing = "out_quad", keyframes = { 1, 1.25, 0.9, 1 } } }

return panel {
    id = "bar",
    layer = "top",
    anchor = { top = true },
    padding = 8, -- room for the overshoot: a scaled node paints past its box
    child = rect {
        width = 48,
        height = 48,
        radius = 12,
        background = "#313244",
        on_click = function() taps:set(taps:get() + 1) end,
        -- pulse is true for 400 ms after each tap: the entry appears, plays once, then goes.
        animate = pulse(taps, 400):map(function(on) return on and BOUNCE or {} end),
        children = { icon { name = "starred-symbolic", size = 24, foreground = "#f9e2af", align_h = "center", align_v = "center" } },
    },
}
```

An endless spinner needs no signal. A hidden spinner stops requesting frames by itself:

<!-- shot-alt: A pill reading "Checking for updates" with a blue refresh icon turning endlessly. -->
<!-- shot: frames=0..950/50 -->
```lua,shot
local busy = state("busy", true)
local SPIN = { rotate = { duration = 1000, easing = "linear", keyframes = { 0, 360 }, loops = "infinite" } }

return panel {
    id = "bar",
    layer = "top",
    anchor = { top = true },
    child = row {
        padding = 10,
        spacing = 8,
        radius = 18,
        background = "#1e1e2e",
        visible = busy,
        children = {
            icon { name = "view-refresh-symbolic", size = 16, foreground = "#89b4fa", align_v = "center", animate = SPIN },
            text { content = "Checking for updates", font_size = 13, foreground = "#cdd6f4", align_v = "center" },
        },
    },
}
```

### Spring segments

A `spring` on a keyframe entry, or on one frame, replaces `easing` for those segments. Each
segment starts the spring from rest and runs it in real time, so `stiffness` and `damping` look the
same as on a plain [spring](#spring) and overshoot meets the same range clamp. The segment's
`duration` still decides when the next frame starts: the frame lands exactly at that instant, and
a spring that has not settled by then jumps the rest of the way. Give a ringing step a `duration`
of at least its settle time, roughly `14 / damping` seconds, to avoid the jump.

```lua
-- A quarter turn every 800 ms, overshooting and settling on each; 14 / 18 is under 0.8 s.
local TICK = {
    rotate = {
        duration = 800,
        spring = { stiffness = 300, damping = 18 },
        keyframes = { 0, 90, 180, 270, 360 },
        loops = "infinite",
    },
}

return icon { name = "view-refresh-symbolic", size = 16, animate = TICK }
```

## Move

`animate.move` eases a matched node when a scene pass changes its laid-out position. It takes a
duration in milliseconds, or `{ duration = 180, easing = "out_cubic", delay = 0 }`. The easing
and delay have the same meanings as a property tween. `from`, `keyframes`, `loops` and `spring`
are refused. A new node has no previous position, so it appears at its new rect.

```lua
local card = rect {
    id = "notice-42",
    width = 280,
    height = 60,
    animate = { move = { duration = 180, easing = "out_cubic" } },
}
return card
```

Use a stable [`id`](../nodes/index.md#identity-and-reconciliation) on a child that can change
position in its parent's `children`, or a [`list` key](../nodes/list.md) for each item. Without
one, id-less siblings match by position; removing the first item can pair the next item with its
old rect. A removed child's `animate.exit` paints from its current position while live siblings
move into the gap. It still takes no flow space or input.

A move tracks the node's position relative to its parent. If an ancestor shifts, give that
ancestor `animate.move` too.

The solver, `geometry` and pointer callbacks report the destination rect throughout the move.
Paint, hit-testing, text links, carets, input regions and background blur follow the moving
pixels. A second layout change starts from the last painted position. Scrolling and paint-only
property tweens do not start a move; an already running move keeps advancing on compositor frames.
A parent with `clip`, a scroll viewport or the surface can cut moving pixels; leave room there.

## Scroll

`animate.scroll` on a `row`, `column` or `list` bound to a [`scroll`](input.md#scroll) signal eases
each mouse-wheel notch. It takes a duration in milliseconds, an eased entry or a `spring`;
`keyframes` are refused. Notches add to a target that stops at either end of the content, and a
notch mid-run eases on from the offset on screen, carrying a spring's velocity. The signal holds
the offset on screen, so a `map` of it reflows with every frame. `:reveal` eases the same way, to
the least move from the target that shows the child, as do `:scroll_to(offset)` and
`:scroll_by(delta)`, which adds to the target. A touchpad or a high-resolution wheel, which
report fractions of a notch, and `reset_on_close` move the offset at once and stop the run. The
offset never passes the ends, even under an overshooting easing or content that shrinks mid-run;
content that grows mid-run does not move the target. A run hidden with its container finishes
while hidden: shown again, it lands on its target on the next frame.

```lua
local rows = {}
for i = 1, 40 do rows[i] = text { content = "Row " .. i } end
return column {
    width = 300,
    height = 200,
    scroll = scroll("feed"),
    animate = { scroll = { duration = 160, easing = "out_cubic" } },
    children = rows,
}
```

## Exit

`animate.exit` animates a child after its parent stops returning it: a notification removed from a
`list`, or a card dropped from `children`. The block holds one timing for every property, and the
values to ease to.

```lua
exit = { duration = 150, easing = "in_quad", opacity = 0, translate = { y = 16 } }
```

| Rule | Detail |
| :--- | :--- |
| Keys | `duration` or `spring`, plus optional `easing` and `delay`, all as in [entry keys](#entry-keys). Every other key is a property name and its target value |
| Checked | On every pass while the node is still in the tree, so a typo fails before the node leaves. A block with no targets is a legal no-op; one with targets needs `duration` or `spring` |
| Start value | The value on screen. A property never set starts at its identity: `1` for `opacity` and `scale`, `0.5` for `origin`, `"0%"` for a percent, the target colour at alpha 0 for a colour, `0` otherwise |
| Running tweens | Stop where they are. The exit block alone decides how long the node lives |
| What moves | Everything painted: `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `shadows`, blurs, `progress`, and pixel `width`/`height`. `margin`, `padding` and `spacing` change nothing visible |
| While leaving | Painted at its last rect and scroll offset, above live siblings of the same `z`. It takes no space in the flow (siblings close up at once), though a content-sized parent keeps room for its last rect until it is gone. It takes no pointer or keyboard input and no `geometry` writes. Its subtree is frozen: a resized box does not reflow its children, and text keeps the string it was fitted to |
| Identity | A leaving node is never matched again. Returning the same `id` builds a new node beside it |
| Scope | Only the dropped child runs its block; descendants leave with it and their own blocks never run |
| Not triggered by | `visible = false`, a surface closing, or a child dropped while an ancestor was hidden |

Hiding a surface skips the exit, so drop the child from `children` and hold the surface open with
[`delay`](signals.md#delay-hold-a-value) until the exit has played. The card below slides up and
fades in on show; the shot plays the hide, down and out over 150 ms:

<!-- shot-alt: A volume card with a speaker icon, a level bar at 42% and its percentage slides down and fades away. -->
<!-- shot: frames=0@900,30,60,90,120,150,180@250 -->
```lua,shot
local shown = state("osd_shown", false)
-- Keep the surface mapped 150 ms past `shown`, so the card's exit can play.
local mapped = computed({ shown, delay(shown, 150) }, function(now, was)
    return now == true or was == true
end)

local card = row {
    width = 260,
    padding = 14,
    spacing = 12,
    radius = 14,
    background = "#1e1e2ee6",
    opacity = 1,
    translate = { y = 0 },
    children = {
        icon { name = "audio-volume-medium-symbolic", size = 20, foreground = "#89b4fa", align_v = "center" },
        rect {
            width = "fill",
            height = 6,
            radius = 3,
            background = "#313244",
            align_v = "center",
            children = { rect { width = "42%", height = 6, radius = 3, background = "#89b4fa" } },
        },
        text { content = "42%", font_size = 13, foreground = "#cdd6f4", align_v = "center" },
    },
    animate = {
        opacity = { duration = 200, from = 0 },
        translate = { duration = 200, easing = "out_cubic", from = { y = 16 } },
        exit = { duration = 150, easing = "in_quad", opacity = 0, translate = { y = 16 } },
    },
}

return panel {
    id = "osd",
    layer = "overlay",
    anchor = { bottom = true },
    width = 260,
    height = 64, -- room for the exit's 16 px slide
    visible = mapped,
    child = column {
        height = "fill",
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
    panel { id = "bar", layer = "top", anchor = { top = true }, child = text { content = "12:30", padding = 8, hover = over } },
    popup {
        id = "clock_tooltip",
        parent = "bar",
        anchor_rect = hover_rect("clock"),
        anchor = "bottom",
        gravity = "bottom",
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

<!-- shot-alt: Three notification cards, each with a coloured icon, title and detail line, fade and slide in one at a time. -->
<!-- shot: frames=0@150,40,80,120,160,200,240,280,320,360,400,440,480,520,560,600,640,680,720,760,800,840@1600 -->
```lua,shot
local go = state("go", false)
local NOTES = {
    { icon = "battery-caution-symbolic", color = "#fab387", title = "Battery low", body = "12% remaining" },
    { icon = "view-refresh-symbolic", color = "#89b4fa", title = "Update ready", body = "Restart to install" },
    { icon = "notification-symbolic", color = "#a6e3a1", title = "Download complete", body = "photos.zip, 48 MB" },
}

local function card(index, note)
    local wait = (index - 1) * 250
    return row {
        width = 260,
        padding = 12,
        spacing = 12,
        radius = 12,
        background = "#1e1e2e",
        opacity = 1,
        translate = { x = 0 },
        animate = {
            opacity = { duration = 300, delay = wait, from = 0 },
            translate = { duration = 300, delay = wait, easing = "out_cubic", from = { x = -24 } },
        },
        children = {
            icon { name = note.icon, size = 20, foreground = note.color, align_v = "center" },
            column {
                spacing = 2,
                children = {
                    text { content = note.title, font_size = 13, font_weight = 700, foreground = "#cdd6f4" },
                    text { content = note.body, font_size = 12, foreground = "#a6adc8" },
                },
            },
        },
    }
end

return column {
    spacing = 8,
    children = go:map(function(on)
        local cards = {}
        for index, note in ipairs(on and NOTES or {}) do
            cards[index] = card(index, note)
        end
        return cards
    end),
}
```

### Slide a notification out

Dropping a child makes it leave. Here each card sits in a slot of fixed height, so a dismissed card
slides out in place and the others stay put. Remove the item from a keyed
[`list`](../nodes/list.md) instead to have the rest close up at once.

<!-- shot-alt: Of three notification cards, the middle one slides right and fades out, leaving its place empty while the other two stay put. -->
<!-- shot: frames=0@900,30,60,90,120,150,180,210,240@1400 -->
```lua,shot
local dismissed = state("dismissed", {})
local NOTES = {
    { icon = "battery-caution-symbolic", color = "#fab387", title = "Battery low", body = "12% remaining" },
    { icon = "view-refresh-symbolic", color = "#89b4fa", title = "Update ready", body = "Restart to install" },
    { icon = "notification-symbolic", color = "#a6e3a1", title = "Download complete", body = "photos.zip, 48 MB" },
}

local function dismiss(title)
    local gone = { [title] = true }
    for other in pairs(dismissed:get()) do gone[other] = true end
    dismissed:set(gone)
end

local function card(note)
    return row {
        width = 280,
        height = 60,
        padding = 12,
        spacing = 12,
        radius = 12,
        background = "#1e1e2e",
        border_width = 1,
        border_color = "#45475a",
        on_click = function() dismiss(note.title) end,
        animate = { exit = { duration = 200, easing = "in_cubic", opacity = 0, translate = { x = 300 } } },
        children = {
            icon { name = note.icon, size = 20, foreground = note.color, align_v = "center" },
            column {
                spacing = 2,
                align_v = "center",
                children = {
                    text { content = note.title, font_size = 13, font_weight = 700, foreground = "#cdd6f4" },
                    text { content = note.body, font_size = 12, foreground = "#a6adc8" },
                },
            },
        },
    }
end

-- The slot keeps its height after its card leaves.
local function slot(note)
    return rect {
        width = 280,
        height = 60,
        children = dismissed:map(function(gone) return gone[note.title] and {} or { card(note) } end),
    }
end

return panel {
    id = "notifications",
    layer = "overlay",
    anchor = { top = true, right = true },
    width = 300,
    height = 400,
    child = column { spacing = 8, children = { slot(NOTES[1]), slot(NOTES[2]), slot(NOTES[3]) } },
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
| Removing a keyed item makes later items snap into the gap | Put `animate.move` on each item |
| A looping motion driven by a `timer` costs CPU on every tick | Use `keyframes` with `loops = "infinite"` ([Lua cost](#lua-cost)) |
| `width` will not overshoot below `0` with `"out_back"` | The property's range clamps every frame. Use `margin` or `translate` for motion that must go negative |

See also: [signals](signals.md) (`pulse`, `delay`, `hover`), [nodes](../nodes/index.md) (properties and
identity), [input](input.md) (hover and clicks that drive motion), [paint](paint.md) (what the
painted properties draw).

Source: [animate](../../renderer/src/layout/node/animate/mod.rs), [easing](../../renderer/src/layout/node/animate/easing.rs),
[spring](../../renderer/src/layout/node/animate/spring.rs), [keyframes](../../renderer/src/layout/node/animate/sequence.rs),
[leaving nodes](../../renderer/src/layout/scene/tick.rs), [range clamps](../../renderer/src/layout/node/style/mod.rs).
