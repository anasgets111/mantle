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
<!-- shot: frames=0@600,30,60,90,120,150,180,210,240,270,300,330,360,390,420,450,480,510,540,570,600@1500 -->
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

## Mental model

A motion has two inputs: the **target**, the value a pass resolves, and the **entry**, how to get
there (`duration`, `easing`, `delay`, `from`, `keyframes` or `spring`). Both are ordinary Lua
values, so either may be a signal.

- A changed target starts a tween from the value on screen. The same target resolved again does not.
- A changed entry retimes the run under way; it starts nothing by itself. Keyframes are the
  exception: new frames, timing, `loops` or `run` start a new run.
- An entry field read from a signal is how one property gets different timing per direction
  ([recipe](#use-different-timing-for-opening-and-closing)).

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
| `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `progress`, `commands`, `fill`, `stroke`, `stroke_width`, `shadows`, `ring`, `effect` | Repaints; no layout pass |
| Anything else: `width`, `height`, `margin`, `padding`, `spacing`, `font_size`, … | Lays the surface out again |

Slide with `translate` and grow on hover with `scale` when surrounding nodes should stay put.
Both skip layout; `width` and `margin` lay out the surface on every animation frame.

### Lua cost

Let `animate` drive motion. A `timer` that sets a signal every frame runs Lua and resolves the
surface on every tick. `animate` computes each frame in the engine, about 20 times cheaper, at the
display's refresh rate.

A progress ring that fills forever, with no Lua per frame:

<!-- shot-alt: A card reading "Syncing photos" beside a blue ring that fills clockwise over its grey track, then starts over. -->
<!-- shot: frames=0..1140/60@60 -->
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
| `from` | A value of the property's shape | Start value for a property with nothing on screen yet. Refused with `keyframes`, whose first frame is the start (`"current"` there takes the value on screen) |
| `spring` | `{ stiffness, damping }` | `stiffness` in `(0, 100000]`, `damping` in `(0, 10000]`, both required. Refuses `easing`, and `duration` and `loops` unless `keyframes` is set ([spring segments](#spring-segments)) |
| `keyframes` | At least 2 frames | See [keyframes](#keyframes) |
| `loops` | Whole count `[1, 10000]` or `"infinite"` | Default 1. Only with `keyframes` |
| `run` | A number | A different value starts a new run, even with the same frames and mid-run. Only with `keyframes` |

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
<!-- shot: frames=0..1200/50 -->
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
| First frame | Where the run starts; its own `duration` and `easing` are never read. The bare string `"current"` starts on the value on screen instead: mid-flight, the tween's last sample, else the value the node rests on. It is read once as the run starts, so each loop returns to that value, and it is only the first frame, never a table form. Works for every shape that tweens. A property the node does not set has no value on screen, so the run does not start |
| Jump | A frame with `duration = 0` (allowed only on a frame) cuts straight to its value |
| Hold | A segment between two equal values holds still for its duration |
| List | At least 2 frames, no holes (`{ [1] = 0, [3] = 1 }` is refused), at least one segment that takes time |
| End | A counted run holds its last frame as long as the entry stays. An `"infinite"` run never ends |
| Continuity | The same list on the next pass is the same run; any change to the frames, timing, `loops` or `run` starts a new run from the first frame |

To replay a run, change `run`: a counter that goes up on each trigger starts the same frames again, whether
the last run is over or not, and the property never snaps in between. [`pulse`](signals.md#pulse-mark-a-change)
is the other way: it adds the entry for a window after its source changes and takes it away again, so the
property snaps back when the window closes.

<!-- shot-alt: A yellow star button swells, dips below its size, and settles in a short bounce. -->
<!-- shot: frames=0@800,30,60,90,120,150,180,210,240,270,300,330,360@1000 -->
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
<!-- shot: frames=0..950/50@50 -->
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

<!-- shot-alt: A refresh icon turning a quarter at a time, overshooting slightly and settling before each next turn. -->
<!-- shot: frames=0..3150/50@50 -->
```lua,shot
-- A quarter turn every 800 ms, overshooting and settling on each; 14 / 18 is under 0.8 s.
local TICK = {
    rotate = {
        duration = 800,
        spring = { stiffness = 300, damping = 18 },
        keyframes = { 0, 90, 180, 270, 360 },
        loops = "infinite",
    },
}

return icon { name = "view-refresh-symbolic", size = 24, foreground = "#cdd6f4", animate = TICK }
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
| What moves | Everything painted: `opacity`, colours, `radius`, `translate`, `scale`, `rotate`, `origin`, `shadows`, `ring`, blurs, `progress`, and pixel `width`/`height`. `margin`, `padding` and `spacing` change nothing visible |
| While leaving | Painted at its last rect and scroll offset, above live siblings of the same `z`. It takes no space in the flow (siblings close up at once), though a content-sized parent keeps room for its last rect until it is gone. It takes no pointer or keyboard input and no `geometry` writes. Its subtree is frozen: a resized box does not reflow its children, and text keeps the string it was fitted to |
| Identity | A leaving node is never matched again. Returning the same `id` builds a new node beside it |
| Scope | Only the dropped child runs its block; descendants leave with it and their own blocks never run |
| Not triggered by | `visible = false`, a surface closing, or a child dropped while an ancestor was hidden |

Hiding a surface skips the exit, so drop the child from `children` and hold the surface open with
[`delay`](signals.md#delay-hold-a-value) until the exit has played. The card below slides up and
fades in on show; the shot plays the hide, down and out over 150 ms:

<!-- shot-alt: A volume card with a speaker icon, a level bar at 42% and its percentage slides down and fades away. -->
<!-- shot: frames=0@900,30,60,90,120,140@400 -->
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

## settled: know when a node stops moving

`settled = settled("name")` on a node gives a read-only signal the engine writes: `false` while any
`animate` tween on that node is running, `true` otherwise. Swap content, unmount a node or start a
second motion when the first ends, without a `timer` that guesses its duration.

| Rule | Detail |
| :--- | :--- |
| Initial | `true`. The pass that starts a tween writes `false`; the frame that ends the last writes `true` |
| Scope | The node's own `animate` entries, `move`, and a wheel or fling scroll run on it. A descendant's tween does not count; declare `settled` on the node that moves |
| Writes | On the edge only, never per frame, and a retarget mid-run writes nothing. A reader re-resolves in one follow-up pass before the frame is drawn, like [`geometry`](signals.md#geometry-read-a-nodes-laid-out-rect) |
| Keyframes | `false` for the whole run, a hold between equal values and a `delay` lead-in included. A counted run is `true` on its last frame; `loops = "infinite"` never is |
| Hidden or leaving | `true`. A hidden subtree is frozen and a leaving node is not counted, so its exit tween does not clear the signal. A hidden subtree that shows again is `false` if a tween resumes |

One name is one signal across reloads. Nodes that share it read `true` only when none of them moves.
Nothing is walked until the shell has called `settled(...)` or `elided(...)` in its current evaluation.
Do not drive a node's own animated target from its own `settled`: the target changes when the motion
ends, which starts the next motion, and the node bounces once per tween end.

The sheet below collapses over 300 ms; the note appears when the collapse ends, not when it starts. In
the pass that sets `open`, `sheet_still` still reads `true`, but the follow-up pass runs before the
frame is drawn, so the note never paints early:

```lua
local open = state("sheet_open", true)
local sheet_still = settled("sheet")

return column {
    spacing = 8,
    children = {
        rect {
            width = 200,
            height = open:map(function(on) return on and 120 or 24 end),
            background = "#313244",
            settled = sheet_still,
            on_click = function() open:set(not open:get()) end,
            animate = { height = { duration = 300, easing = "out_cubic" } },
        },
        text {
            content = "Collapsed",
            foreground = "#cdd6f4",
            visible = computed({ open, sheet_still }, function(on, still) return not on and still end),
        },
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
| Fade content in after a panel grows, but out at once on close | [Different timing per direction](#use-different-timing-for-opening-and-closing) |
| Run phases in order: content out, container resizes, content swaps | [Sequence phases](#sequence-phases) |
| Grow a card out of a small shape | [Grow a container](#grow-a-container-out-of-a-small-shape) |

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
<!-- shot: frames=0@900,30,60,90,120,150,180,210@1400 -->
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
        animate = { exit = { duration = 200, easing = "in_cubic", opacity = 0, translate = { x = 16 } } },
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

### Use different timing for opening and closing

`delay`, `duration` and the rest of an entry accept a signal, read on the same pass that moves the
target. Derive the timing from the state that moves it. Here the label waits 200 ms for the pill to
grow, then fades in; on close the same entry resolves `delay = 0`, so it fades out while the pill
shrinks.

<!-- shot-alt: A round pill with a bell icon grows to the right into a wide card, then the text "New message" fades in once the card has finished growing. -->
<!-- shot: frames=0..480/30@1000 -->
```lua,shot
local open = state("open", false)
-- 200 ms to wait for the grow on the way in, none on the way out.
local wait = open:map(function(on) return on and 200 or 0 end)

return rect {
    width = open:map(function(on) return on and 200 or 56 end),
    height = 56,
    radius = 28,
    background = "#1e1e2e",
    clip = "rounded",
    animate = { width = { duration = 200, easing = "out_cubic" } },
    children = {
        row {
            width = 200,
            height = 56,
            padding = 16,
            spacing = 10,
            children = {
                icon { name = "notification-symbolic", size = 24, foreground = "#89b4fa", align_v = "center" },
                text {
                    content = "New message",
                    font_size = 14,
                    foreground = "#cdd6f4",
                    align_v = "center",
                    opacity = open:map(function(on) return on and 1 or 0 end),
                    animate = { opacity = { duration = 150, delay = wait } },
                },
            },
        },
    },
}
```

The row keeps its full width, so the label never rewraps as the pill moves; `clip = "rounded"` cuts
what the pill has not grown over yet.

### Sequence phases

An `exit` cannot do this: a leaving node takes no flow space, so its parent closes up at once and
there is nothing left to resize around. Keep the container's size in state instead and stage each
phase with `delay(signal, ms)`, which lags a state by a fixed time. Each phase is a signal that
changes `ms` later than the one before; the phase durations must add up to those offsets. A fade
that only needs to wait can take `delay` in its own entry instead, as in the
[recipe above](#use-different-timing-for-opening-and-closing).

| Time | Signal | Moves |
| :--- | :--- | :--- |
| 0 ms | `page` | Content fades out (150 ms) |
| 150 ms | `delay(page, 150)` | Container height eases to the new page's (250 ms) |
| 400 ms | `delay(page, 400)` | Content swaps while invisible, then fades in (150 ms) |

<!-- shot-alt: A card fades its text out, grows taller, then fades in three lines of text in place of the one. -->
<!-- shot: frames=0..700/25 -->
```lua,shot
local page = state("page", "short")
local PAGES = {
    short = { height = 64, lines = { "Sync finished" } },
    tall = { height = 112, lines = { "Sync finished", "48 files uploaded", "2 conflicts need review" } },
}

local resized = delay(page, 150)
local swapped = delay(page, 400)

return rect {
    width = 240,
    height = resized:map(function(p) return PAGES[p].height end),
    radius = 14,
    background = "#1e1e2e",
    clip = "rounded",
    animate = { height = { duration = 250, easing = "out_cubic" } },
    children = {
        column {
            padding = 16,
            spacing = 4,
            -- Visible only while the page and the swapped content agree.
            opacity = computed({ page, swapped }, function(now, shown) return now == shown and 1 or 0 end),
            animate = { opacity = 150 },
            children = swapped:map(function(p)
                local out = {}
                for i, line in ipairs(PAGES[p].lines) do
                    out[i] = text { content = line, font_size = i == 1 and 14 or 12, foreground = "#cdd6f4" }
                end
                return out
            end),
        },
    },
}
```

### Grow a container out of a small shape

Animate `width` and `height` with `from` set to the small size. `from` applies to a node's first
layout, so the card is built when the state turns on. Eased runs end exactly on the size; a
`spring` with damping below critical overshoots it and rings. Give the children a fixed size so they
do not rewrap while the box moves, and set `clip = "rounded"` so they are cut to the moving box and
its `radius`.

<!-- shot-alt: Two dark cards grow from small circles at the centre of their slots. The left eases to its size and stops; the right overshoots its size and settles back. -->
<!-- shot: frames=0..600/30@1000 -->
```lua,shot
local go = state("go", false)
local EASED = { duration = 350, easing = "out_cubic", from = 40 }
local SPRING = { spring = { stiffness = 300, damping = 16 }, from = 40 }

local function card(label, timing)
    return rect {
        width = 176,
        height = 96,
        radius = 20,
        background = "#1e1e2e",
        clip = "rounded",
        align_h = "center",
        align_v = "center",
        animate = { width = timing, height = timing },
        children = {
            column {
                width = 176,
                height = 96,
                padding = 16,
                spacing = 4,
                children = {
                    text { content = label, font_size = 14, font_weight = 700, foreground = "#cdd6f4" },
                    text { content = "Grows from 40 px", font_size = 12, foreground = "#a6adc8" },
                },
            },
        },
    }
end

local function slot(label, timing)
    return rect {
        width = 216,
        height = 136,
        children = go:map(function(on) return on and { card(label, timing) } or {} end),
    }
end

return row { spacing = 8, children = { slot("Eased", EASED), slot("Spring", SPRING) } }
```

### Replay a keyframe run from where it is

A pupil glances aside and back on each click. `run` counts the clicks, so identical frames play again on
every one, and `"current"` starts each glance from wherever the pupil is, mid-glance or not. No timer takes
the entry away and puts it back:

```lua
local looks = state("looks", 0)

return rect {
    width = 12,
    height = 12,
    radius = 6,
    background = "#cdd6f4",
    translate = { x = 0, y = 0 },
    on_click = function() looks:set(looks:get() + 1) end,
    animate = looks:map(function(n)
        local glance = { "current", { x = 6, y = -4 }, { x = 0, y = 0 } }
        return { translate = { duration = 150, easing = "out_cubic", run = n, keyframes = glance } }
    end),
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A node's first value snaps; nothing fades in | Give the entry `from` |
| `from` does nothing | The node must set the property too: `opacity = 1` beside `opacity = { from = 0, ... }` |
| An exit never plays | Exit runs only when the parent stops returning the child. Switch `children`, and keep the surface up with `delay` |
| `animate.exit` never plays and the log says `shell.lua:120 replaced shell.lua:95 in place` | `children` swapped one id-less node for another of its kind, so the old node was reused. Give the siblings distinct `id`s |
| A held key makes an eased value trail behind | Use a `spring`; it keeps its velocity through each new target |
| A keyframe run plays once and never again | Same list, same run. Give the entry a `run` and count it up ([recipe](#replay-a-keyframe-run-from-where-it-is)), or gate the entry with `pulse` |
| A `pulse`-driven run is cut short | Removing the entry snaps the property. Make the window at least `delay` plus every segment's `duration` times `loops` |
| A second click inside the `pulse` window does not replay the run | The click only extends the window; the entry never leaves, so the run does not restart. Use `run` |
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
