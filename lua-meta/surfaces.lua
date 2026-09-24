---@meta
-- The four surface roles (ADR-0040), one constructor each. `shell.lua` returns the set, re-read on
-- every reload (ADR-0038). Hand-written on `nodes.lua`'s terms.
--
-- Structural fields (`id`, `layer`, `anchor`, `monitor`, `namespace`, a popup's `parent`) refuse a
-- `Signal`: they are read once per evaluation (ADR-0216).

---@alias Rect { x: number, y: number, width: number, height: number }
---@alias PopupAnchor "Top"|"Bottom"|"Left"|"Right"|"TopLeft"|"TopRight"|"BottomLeft"|"BottomRight"|"Center"

---A surface root takes `rect`'s node and box properties, plus its own topology.
---@class PanelProps: NodeBase, BoxBase
---@field id string Required; the surface's identity across reloads (duplicates are not checked). Each output's instance is `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
---@field layer "Background"|"Bottom"|"Top"|"Overlay" Required, no default.
---@field anchor? { top?: boolean, bottom?: boolean, left?: boolean, right?: boolean } Edges to pin to; an absent edge is `false`. None pinned centres the surface.
---@field width? Length|Bound Live. Omitted measures the content, capped by the output less the anchored edges' margins; `"NN%"` is of the output. On an axis anchored to both edges, omitted and `"Fill"` both size the surface to the compositor's span; the root node stays content-sized, so give the child `width = "Fill"` to cover it.
---@field height? Length|Bound As `width`, against `top`/`bottom`. `"Fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning.
---@field exclusive? boolean|integer|"Ignore"|Bound `false` (default) reserves nothing, a positive integer reserves that many px, `"Ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing.
---@field margin? number|Edges|Bound Offset from the anchored edges. Live.
---@field monitor? string A connector name, `"All"` (default), or `"Active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing.
---@field namespace? string The layer namespace compositor rules match. Default `"mantle-{id}"`.
---@field keyboard_interactivity? "None"|"OnDemand"|"Exclusive"|Bound Default `"None"`. Live.
---@field visible? boolean|Bound Default `true`. Hiding destroys the layer surface; showing recreates it (ADR-0088).
---@field child? Node|fun(output: string): Node? The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.

---@class WindowProps: NodeBase, BoxBase
---@field id string Required; the surface's identity across reloads.
---@field title? string|Bound Live. Default `""`.
---@field app_id? string|Bound Live; what compositor rules match. Default `"mantle-{id}"`.
---@field min_size? { width: number, height: number }|Bound Advisory. Both axes required, each `[0, 8192]`, `0` unconstrained. Also the opening size when the compositor leaves it to the client, else 640x480.
---@field max_size? { width: number, height: number }|Bound Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused.
---@field on_close? fun() The user asked to close. The window stays open until the config sets `visible = false`.
---@field visible? boolean|Bound Default `true`. Opens and closes the window; state and `id` survive (ADR-0049).
---@field child? Node The one root node.

---@class PopupProps: NodeBase, BoxBase
---@field id string Required; the surface's identity across reloads.
---@field parent string Required: the `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first.
---@field anchor_rect Rect|Bound Required, in the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes.
---@field width? number|Bound Pixels in `(0, 8192]`; no `"Fill"` or `%`. Omitted sizes to the content; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+).
---@field height? number|Bound As `width`; each axis is independent.
---@field anchor? PopupAnchor|Bound The point on `anchor_rect` the popup hangs from. Default `"Center"`.
---@field gravity? PopupAnchor|Bound The direction it extends from that point. Default `"Center"`.
---@field constraint_adjustment? ("SlideX"|"SlideY"|"FlipX"|"FlipY"|"ResizeX"|"ResizeY")[]|Bound How the compositor may keep it on screen. Default `{ "FlipY", "SlideX" }`, `{}` for none; order is ignored.
---@field offset? { x?: number, y?: number }|Bound Pixel nudge after `anchor` and `gravity`; an absent axis is `0`.
---@field grab? boolean|Bound Default `true`, which needs a click to grab from; a denied grab dismisses the popup. `false` for a hover tooltip.
---@field on_dismiss? fun() The compositor closed it (click outside, denied grab); not called when the config hides it. Set `visible = false` here, or it reopens on the next click (ADR-0051).
---@field visible? boolean|Bound Default `true`. Opens and closes the popup; state and `id` survive (ADR-0049).
---@field child? Node The one root node.

---@class LockProps: NodeBase, BoxBase
---@field id string Required; the surface's identity across reloads.
---@field width? nil Refused: the lock covers each output (ADR-0052).
---@field height? nil Refused, as `width`.
---@field visible? nil Refused: the session lock decides when it shows.
---@field child? Node|fun(output: string): Node? The one root node; a function runs per output, as on a `panel`.

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

---An `ext_session_lock_surface_v1` per output, shown while the session is locked. Declaring one
---does not lock (ADR-0052). At most one per config.
---[docs](https://anasgets111.github.io/mantle/surfaces/lock.html)
---@param props LockProps
---@return Node
function lock(props) end
