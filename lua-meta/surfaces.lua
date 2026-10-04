---@meta
-- The four surface roles (ADR-0040), one constructor each. `shell.lua` returns the set, re-read on
-- every reload (ADR-0038). A root takes `rect`'s node and box properties but placement, plus its own topology.
--
-- GENERATED on `nodes.lua`'s terms. Structural fields, the ones without `Bound` (`id`, `layer`,
-- `anchor`, `output`, `namespace`, a popup's `parent`), refuse a `Signal`: they are read once per
-- evaluation (ADR-0216).

---A surface table, as one of the constructors below returns it: what `shell.lua` returns.
---@class Surface
---@field [string] any

---@alias Rect { x: number, y: number, width: number, height: number, [string]: "no such property" } A rectangle in logical pixels.
---@alias PopupAnchor "top"|"bottom"|"left"|"right"|"top_left"|"top_right"|"bottom_left"|"bottom_right"|"center"

---@alias PanelAnimations { accessible_name?: Animation, anchor?: Animation, backdrop_blur?: Animation, background?: Animation, behind_blur?: Animation, border_color?: Animation, border_width?: Animation, child?: Animation, clip?: Animation, content_blur?: Animation, corner_shape?: Animation, cursor?: Animation, exclusive_zone?: Animation, focus_ring?: Animation, focused?: Animation, geometry?: Animation, height?: Animation, hittable?: Animation, hover?: Animation, id?: Animation, keyboard_interactivity?: Animation, layer?: Animation, margin?: Animation, mask?: Animation, max_height?: Animation, max_width?: Animation, min_height?: Animation, min_width?: Animation, namespace?: Animation, on_click?: Animation, on_drag?: Animation, on_escape?: Animation, on_hover?: Animation, on_wheel?: Animation, opacity?: Animation, origin?: Animation, output?: Animation, padding?: Animation, radius?: Animation, reset_on_close?: Animation, rotate?: Animation, scale?: Animation, shadow_mode?: Animation, shadows?: Animation, submit?: Animation, translate?: Animation, visible?: Animation, width?: Animation, exit?: Exit, move?: MoveAnimation, [string]: "no such property" }
---@class PanelProps: NodeBase, BoxBase
---@field animate? PanelAnimations|Bound Tween named properties to each newly resolved value without running Lua (ADR-0145). `move` eases a matched node to its new parent-relative layout position; an ancestor that shifts needs its own `move`. `exit` runs after removal. Only a node already on screen animates, unless an entry has `from`.
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id`.
---@field layer "background"|"bottom"|"top"|"overlay" Required. Stacking level, bottom to top. `"overlay"` draws over fullscreen windows.
---@field anchor? { top?: boolean, bottom?: boolean, left?: boolean, right?: boolean, [string]: "no such property" } Default: all `false`. Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge.
---@field output? string Default `"all"`. A connector name, `"all"`, or `"active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing.
---@field namespace? string Default `"mantle-{id}"`. The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`).
---@field width? Length|Bound `[0, 8192]`, default: content. Omitted measures content unless both edges of that axis are anchored; then the compositor spans the surface and its configured extent fills the root too. `"NN%"` is of the output. `max_width`/`max_height` cap the root.
---@field height? Length|Bound `[0, 8192]`, default: content. As `width`, against `top`/`bottom`. `"fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning.
---@field exclusive_zone? boolean|integer|"ignore"|Bound Default `false`. `false` reserves nothing, a positive integer reserves that many px, `"ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing.
---@field keyboard_interactivity? "none"|"on_demand"|"exclusive"|Bound Default `"none"`. Whether it takes the keyboard.
---@field margin? number|Edges|Bound Default `0`. Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing.
---@field visible? boolean|Bound Default `true`. Hiding destroys the layer surface; showing recreates it (ADR-0088).
---@field child? Node|fun(output: string): Node?|Bound The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.
---@field on_escape? fun() Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins, with no order promised among sibling popups. Never on a surface without `keyboard_interactivity`.
---@field reset_on_close? (StateSignal<any>|ScrollSignal)[] Default `{}`. `state` and `scroll` handles written back when this surface stops being shown: `visible` turning false, a reload removing it, its last output leaving, or its parent closing (a popup). A state returns to its declared `initial`, running its `on_change`; a scroll to the top. Anything else in the list fails the evaluation (ADR-0289).

---@alias WindowAnimations { accessible_name?: Animation, app_id?: Animation, backdrop_blur?: Animation, background?: Animation, behind_blur?: Animation, border_color?: Animation, border_width?: Animation, child?: Animation, clip?: Animation, content_blur?: Animation, corner_shape?: Animation, cursor?: Animation, focus_ring?: Animation, focused?: Animation, geometry?: Animation, height?: Animation, hittable?: Animation, hover?: Animation, id?: Animation, mask?: Animation, max_height?: Animation, max_size?: Animation, max_width?: Animation, min_height?: Animation, min_size?: Animation, min_width?: Animation, on_click?: Animation, on_close?: Animation, on_drag?: Animation, on_escape?: Animation, on_hover?: Animation, on_wheel?: Animation, opacity?: Animation, origin?: Animation, padding?: Animation, radius?: Animation, reset_on_close?: Animation, rotate?: Animation, scale?: Animation, shadow_mode?: Animation, shadows?: Animation, submit?: Animation, title?: Animation, translate?: Animation, visible?: Animation, width?: Animation, exit?: Exit, move?: MoveAnimation, [string]: "no such property" }
---@class WindowProps: NodeBase, BoxBase
---@field animate? WindowAnimations|Bound Tween named properties to each newly resolved value without running Lua (ADR-0145). `move` eases a matched node to its new parent-relative layout position; an ancestor that shifts needs its own `move`. `exit` runs after removal. Only a node already on screen animates, unless an entry has `from`.
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id`.
---@field title? string|Bound Default `""`. The window title.
---@field app_id? string|Bound Default `"mantle-{id}"`. What compositor window rules match.
---@field min_size? { width: number, height: number, [string]: "no such property" }|Bound `[0, 8192]`. Advisory; layout does not enforce it. Both keys required, `0` leaves an axis unconstrained. Also the opening size when the compositor leaves it to the client, else 640x480.
---@field max_size? { width: number, height: number, [string]: "no such property" }|Bound `[0, 8192]`. Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size.
---@field on_close? fun() The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing.
---@field visible? boolean|Bound Default `true`. Opens and closes the window; state and `id` survive (ADR-0049).
---@field width? Length|Bound `[0, 8192]`, default: fill the window. The root's size inside the window, not the window's.
---@field height? Length|Bound `[0, 8192]`, default: fill the window. As `width`.
---@field on_escape? fun() Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins, with no order promised among sibling popups. Never on a surface without `keyboard_interactivity`.
---@field reset_on_close? (StateSignal<any>|ScrollSignal)[] Default `{}`. `state` and `scroll` handles written back when this surface stops being shown: `visible` turning false, a reload removing it, its last output leaving, or its parent closing (a popup). A state returns to its declared `initial`, running its `on_change`; a scroll to the top. Anything else in the list fails the evaluation (ADR-0289).
---@field child? Node|Bound The one root node; a function `child` is refused.

---@alias PopupAnimations { accessible_name?: Animation, anchor?: Animation, anchor_rect?: Animation, backdrop_blur?: Animation, background?: Animation, behind_blur?: Animation, border_color?: Animation, border_width?: Animation, child?: Animation, clip?: Animation, constraint_adjustment?: Animation, content_blur?: Animation, corner_shape?: Animation, cursor?: Animation, focus_ring?: Animation, focused?: Animation, geometry?: Animation, grab?: Animation, gravity?: Animation, height?: Animation, hittable?: Animation, hover?: Animation, id?: Animation, mask?: Animation, max_height?: Animation, max_width?: Animation, min_height?: Animation, min_width?: Animation, offset?: Animation, on_click?: Animation, on_dismiss?: Animation, on_drag?: Animation, on_escape?: Animation, on_hover?: Animation, on_wheel?: Animation, opacity?: Animation, origin?: Animation, padding?: Animation, parent?: Animation, radius?: Animation, reset_on_close?: Animation, rotate?: Animation, scale?: Animation, shadow_mode?: Animation, shadows?: Animation, submit?: Animation, translate?: Animation, visible?: Animation, width?: Animation, exit?: Exit, move?: MoveAnimation, [string]: "no such property" }
---@class PopupProps: NodeBase, BoxBase
---@field animate? PopupAnimations|Bound Tween named properties to each newly resolved value without running Lua (ADR-0145). `move` eases a matched node to its new parent-relative layout position; an ancestor that shifts needs its own `move`. `exit` runs after removal. Only a node already on screen animates, unless an entry has `from`.
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id`.
---@field parent string Required. The `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first. A change applies at the next open; a `lock` cannot be a parent.
---@field anchor_rect Rect|Bound Required. In the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes.
---@field anchor? PopupAnchor|Bound Default `"center"`. The point on `anchor_rect` the popup hangs from.
---@field gravity? PopupAnchor|Bound Default `"center"`. The direction it extends from that point: `"bottom"` hangs it below, `"bottom_right"` below and to the right.
---@field constraint_adjustment? ("slide_x"|"slide_y"|"flip_x"|"flip_y"|"resize_x"|"resize_y")[]|Bound Default `{ "flip_y", "slide_x" }`. How the compositor may keep it on screen; `{}` for none, order is ignored.
---@field offset? { x?: number, y?: number, [string]: "no such property" }|Bound Default `{ x = 0, y = 0 }`. Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left.
---@field width? number|Bound Default: content. Pixels in `(0, 8192]`; no `"fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+).
---@field height? number|Bound Default: content. As `width`; each axis is independent.
---@field grab? boolean|Bound Default `true`. Takes an input grab so an outside click dismisses it; it needs a click to grab from, and a denied grab dismisses the popup. `false` for a hover tooltip.
---@field on_dismiss? fun() The compositor closed it (click outside, denied grab, parent gone); not called when the config hides it. Set `visible = false` here, or it reopens on the next click (ADR-0051).
---@field visible? boolean|Bound Default `true`. Opens and closes the popup; state and `id` survive (ADR-0049).
---@field on_escape? fun() Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins, with no order promised among sibling popups. Never on a surface without `keyboard_interactivity`.
---@field reset_on_close? (StateSignal<any>|ScrollSignal)[] Default `{}`. `state` and `scroll` handles written back when this surface stops being shown: `visible` turning false, a reload removing it, its last output leaving, or its parent closing (a popup). A state returns to its declared `initial`, running its `on_change`; a scroll to the top. Anything else in the list fails the evaluation (ADR-0289).
---@field child? Node|Bound The one root node; a function `child` is refused.

---@alias LockAnimations { accessible_name?: Animation, backdrop_blur?: Animation, background?: Animation, behind_blur?: Animation, border_color?: Animation, border_width?: Animation, child?: Animation, clip?: Animation, content_blur?: Animation, corner_shape?: Animation, cursor?: Animation, focus_ring?: Animation, focused?: Animation, geometry?: Animation, hittable?: Animation, hover?: Animation, id?: Animation, mask?: Animation, max_height?: Animation, max_width?: Animation, min_height?: Animation, min_width?: Animation, on_click?: Animation, on_drag?: Animation, on_hover?: Animation, on_wheel?: Animation, opacity?: Animation, origin?: Animation, padding?: Animation, radius?: Animation, rotate?: Animation, scale?: Animation, shadow_mode?: Animation, shadows?: Animation, submit?: Animation, translate?: Animation, exit?: Exit, move?: MoveAnimation, [string]: "no such property" }
---@class LockProps: NodeBase, BoxBase
---@field animate? LockAnimations|Bound Tween named properties to each newly resolved value without running Lua (ADR-0145). `move` eases a matched node to its new parent-relative layout position; an ancestor that shifts needs its own `move`. `exit` runs after removal. Only a node already on screen animates, unless an entry has `from`.
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id`.
---@field child? Node|fun(output: string): Node?|Bound The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.
---@field width? nil Refused: the lock covers each output (ADR-0052).
---@field height? nil Refused, as `width`.
---@field visible? nil Refused: the session lock decides when it shows.

---A layer surface (`zwlr_layer_surface_v1`): bar, dock, wallpaper, OSD, launcher.
---[docs](https://anasgets111.github.io/mantle/surfaces/panel.html)
---@param props PanelProps
---@return Surface
function panel(props) end

---An `xdg_toplevel`: settings window, dialog.
---[docs](https://anasgets111.github.io/mantle/surfaces/window.html)
---@param props WindowProps
---@return Surface
function window(props) end

---An `xdg_popup` on its parent: dropdown, context menu, tooltip. No Wayland object while hidden.
---[docs](https://anasgets111.github.io/mantle/surfaces/popup.html)
---@param props PopupProps
---@return Surface
function popup(props) end

---An `ext_session_lock_surface_v1` per output, shown while the session is locked. Declaring one does not lock (ADR-0052). At most one per config.
---[docs](https://anasgets111.github.io/mantle/surfaces/lock.html)
---@param props LockProps
---@return Surface
function lock(props) end
