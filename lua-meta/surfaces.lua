---@meta
-- The four surface roles (ADR-0040), one constructor each. `shell.lua` returns the set, re-read on
-- every reload (ADR-0038). A root takes `rect`'s node and box properties, plus its own topology.
--
-- GENERATED on `nodes.lua`'s terms. Structural fields, the ones without `Bound` (`id`, `layer`,
-- `anchor`, `monitor`, `namespace`, a popup's `parent`), refuse a `Signal`: they are read once per
-- evaluation (ADR-0216).

---@alias Rect { x: number, y: number, width: number, height: number, [string]: "no such property" }
---@alias PopupAnchor "Top"|"Bottom"|"Left"|"Right"|"TopLeft"|"TopRight"|"BottomLeft"|"BottomRight"|"Center"

---@class PanelProps: NodeBase, BoxBase
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
---@field layer "Background"|"Bottom"|"Top"|"Overlay" Required. Stacking level, bottom to top. `"Overlay"` draws over fullscreen windows.
---@field anchor? { top?: boolean, bottom?: boolean, left?: boolean, right?: boolean, [string]: "no such property" } Default: all `false`. Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge.
---@field monitor? string Default `"All"`. A connector name, `"All"`, or `"Active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing.
---@field namespace? string Default `"mantle-{id}"`. The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`).
---@field width? Length|Bound `[0, 8192]`, default: content. Omitted measures the content, capped by the output less the anchored edges' margins; `"NN%"` is of the output. On an axis anchored to both edges, omitted and `"Fill"` both size the surface to the compositor's span; the root node stays content-sized, so give the child `width = "Fill"` to cover it.
---@field height? Length|Bound `[0, 8192]`, default: content. As `width`, against `top`/`bottom`. `"Fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning.
---@field exclusive? boolean|integer|"Ignore"|Bound Default `false`. `false` reserves nothing, a positive integer reserves that many px, `"Ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing.
---@field keyboard_interactivity? "None"|"OnDemand"|"Exclusive"|Bound Default `"None"`. Whether it takes the keyboard.
---@field margin? number|Edges|Bound Default `0`. Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing.
---@field visible? boolean|Bound Default `true`. Hiding destroys the layer surface; showing recreates it (ADR-0088).
---@field child? Node|fun(output: string): Node?|Bound The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.

---@class WindowProps: NodeBase, BoxBase
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
---@field title? string|Bound Default `""`. The window title.
---@field app_id? string|Bound Default `"mantle-{id}"`. What compositor window rules match.
---@field min_size? { width: number, height: number, [string]: "no such property" }|Bound `[0, 8192]`. Advisory; layout does not enforce it. Both keys required, `0` leaves an axis unconstrained. Also the opening size when the compositor leaves it to the client, else 640x480.
---@field max_size? { width: number, height: number, [string]: "no such property" }|Bound `[0, 8192]`. Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size.
---@field on_close? fun() The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing.
---@field visible? boolean|Bound Default `true`. Opens and closes the window; state and `id` survive (ADR-0049).
---@field width? Length|Bound `[0, 8192]`, default: fill the window. The root's size inside the window, not the window's.
---@field height? Length|Bound `[0, 8192]`, default: fill the window. As `width`.
---@field child? Node|Bound The one root node; a function `child` is refused.

---@class PopupProps: NodeBase, BoxBase
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
---@field parent string Required. The `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first. A change applies at the next open; a `lock` cannot be a parent.
---@field anchor_rect Rect|Bound Required. In the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes.
---@field anchor? PopupAnchor|Bound Default `"Center"`. The point on `anchor_rect` the popup hangs from.
---@field gravity? PopupAnchor|Bound Default `"Center"`. The direction it extends from that point: `"Bottom"` hangs it below, `"BottomRight"` below and to the right.
---@field constraint_adjustment? ("SlideX"|"SlideY"|"FlipX"|"FlipY"|"ResizeX"|"ResizeY")[]|Bound Default `{ "FlipY", "SlideX" }`. How the compositor may keep it on screen; `{}` for none, order is ignored.
---@field offset? { x?: number, y?: number, [string]: "no such property" }|Bound Default `{ x = 0, y = 0 }`. Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left.
---@field width? number|Bound Default: content. Pixels in `(0, 8192]`; no `"Fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+).
---@field height? number|Bound Default: content. As `width`; each axis is independent.
---@field grab? boolean|Bound Default `true`. Takes an input grab so an outside click dismisses it; it needs a click to grab from, and a denied grab dismisses the popup. `false` for a hover tooltip.
---@field on_dismiss? fun() The compositor closed it (click outside, denied grab, parent gone); not called when the config hides it. Set `visible = false` here, or it reopens on the next click (ADR-0051).
---@field visible? boolean|Bound Default `true`. Opens and closes the popup; state and `id` survive (ADR-0049).
---@field child? Node|Bound The one root node; a function `child` is refused.

---@class LockProps: NodeBase, BoxBase
---@field id string Required. The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
---@field child? Node|fun(output: string): Node?|Bound The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.
---@field width? nil Refused: the lock covers each output (ADR-0052).
---@field height? nil Refused, as `width`.
---@field visible? nil Refused: the session lock decides when it shows.

---A layer surface (`zwlr_layer_surface_v1`): bar, dock, wallpaper, OSD, launcher.
---[docs](https://anasgets111.github.io/mantle/surfaces/panel.html)
---@param props PanelProps
---@return Node
function panel(props) end

---An `xdg_toplevel`: settings window, dialog.
---[docs](https://anasgets111.github.io/mantle/surfaces/window.html)
---@param props WindowProps
---@return Node
function window(props) end

---An `xdg_popup` on its parent: dropdown, context menu, tooltip. No Wayland object while hidden.
---[docs](https://anasgets111.github.io/mantle/surfaces/popup.html)
---@param props PopupProps
---@return Node
function popup(props) end

---An `ext_session_lock_surface_v1` per output, shown while the session is locked. Declaring one does not lock (ADR-0052). At most one per config.
---[docs](https://anasgets111.github.io/mantle/surfaces/lock.html)
---@param props LockProps
---@return Node
function lock(props) end
