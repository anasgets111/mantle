---@meta
-- The eleven node kinds and their properties. Surface roles live in `surfaces.lua`.
--
-- HAND-WRITTEN: `just stubs` does not touch it. `renderer/src/lua/nodes.rs`'s `meta_stub_tests`
-- checks constructors and each class's `---@field` names against `NODE_PROPERTIES`; `just types`
-- checks `share/starter` against these types (ADR-0081).
--
-- `Bound` in a union means the property also takes a signal, resolved once per pass. It is
-- `userdata`, not `Signal`, so table payloads are not mistaken for signals. `id` and callbacks take
-- no signal; `hover`, `scroll` and `geometry` take the handle itself.

---@alias Node table A node table, as one of the constructors below returns it.
---@alias Align "Start"|"Center"|"End"|"Stretch"
---@alias Cursor "default"|"pointer"|"text"|"not-allowed"|"grab"|"grabbing"|"move"|"crosshair"|"wait"|"progress"|"help"|"context-menu"|"cell"|"vertical-text"|"alias"|"copy"|"no-drop"|"zoom-in"|"zoom-out"|"all-scroll"|"col-resize"|"row-resize"|"n-resize"|"e-resize"|"s-resize"|"w-resize"|"ne-resize"|"nw-resize"|"se-resize"|"sw-resize"|"ew-resize"|"ns-resize"|"nesw-resize"|"nwse-resize" CSS cursor name (same as `wp_cursor_shape_v1`).
---@alias Edges { top?: number, right?: number, bottom?: number, left?: number } Per-edge pixels; a missing edge is `0`.
---@alias Percent string `"NN%"` of the parent's box (the output's, on a panel). Not checked by the language server.
---@alias Length number|"Fill"|Percent Pixels `[0, 8192]`, the remaining space, or a percent.
---@alias Color string `"#RRGGBB"` or `"#RRGGBBAA"`. No shorthand or names.
---@alias BorderColors { top?: Color, right?: Color, bottom?: Color, left?: Color } Per-edge colours; a signal inside is refused.
---@alias Axes { x?: number, y?: number } A missing axis takes the property's default.
---@alias GradientStop [number, Color] Position `[0, 1]` and colour. Positions ascend.
---@alias Gradient { gradient: "Linear"|"Radial"|"Conic", angle?: number, stops: GradientStop[] } At least 2 stops. `angle` is degrees clockwise from the top: Linear default `180`, Conic default `0`, Radial refuses it.
---@alias Mask { gradient?: "Linear"|"Radial"|"Conic", angle?: number, stops?: GradientStop[], source?: string, invert?: boolean } Exactly one of a `Gradient` or an image `source` path (alpha only, stretched over the box). `invert` swaps kept and cut.
---@alias EasingName "Linear"|"InQuad"|"OutQuad"|"InOutQuad"|"InCubic"|"OutCubic"|"InOutCubic"|"InQuart"|"OutQuart"|"InOutQuart"|"InQuint"|"OutQuint"|"InOutQuint"|"InSine"|"OutSine"|"InOutSine"|"InExpo"|"OutExpo"|"InOutExpo"|"InCirc"|"OutCirc"|"InOutCirc"|"InBack"|"OutBack"|"InOutBack"|"InElastic"|"OutElastic"|"InOutElastic"|"InBounce"|"OutBounce"|"InOutBounce" `Back` and `Elastic` overshoot, as does a Bezier `y` outside `[0, 1]`; the property's range clamps them.
---@alias Easing EasingName|[number, number, number, number]|{ steps: integer } A name, CSS `cubic-bezier` `{ x1, y1, x2, y2 }` with `x1`, `x2` in `[0, 1]`, or `{ steps = n }`, `n` in `[1, 1000]` (ADR-0151).
---@alias Keyframe number|string|Edges|Axes|{ value: number|string|Edges|Axes, duration?: number, easing?: Easing } A bare value, or a frame with its own timing. `duration = 0` jumps; repeating the previous value holds.
---@alias Spring { stiffness: number, damping: number } Both required: `stiffness` `(0, 100000]`, `damping` `(0, 10000]`; `2 * math.sqrt(stiffness)` is critical damping. Keeps its velocity when the target changes (ADR-0154).
---@alias Animation number|{ duration?: number, delay?: number, easing?: Easing, from?: number|string|Edges|Axes, spring?: Spring, keyframes?: Keyframe[], loops?: integer|"Infinite" } A bare number is `duration`.
--- - `duration`: ms `[1, 60000]`, required unless `spring`. `easing` defaults to `"InOutQuad"`.
--- - `delay`: ms `[0, 60000]` before it starts; offsets a sequence once, not per loop (ADR-0153).
--- - `from`: start value when the node did not display the property last pass (a new node, or one that lacked it); otherwise the first value snaps (ADR-0146). Refused beside `keyframes`.
--- - `spring`: replaces `duration`, `easing`, `keyframes` and `loops`, which are refused beside it.
--- - `keyframes`: at least 2 values, no holes, at least one segment with time; walks instead of easing to the resolved value (ADR-0152). `loops` `[1, 10000]` or `"Infinite"`, default `1`, only with `keyframes`. Bind `animate` to start or stop one.
---@alias Animations table<string, Animation> Property name to animation. Names the node does not accept, `z` and `animate` are refused. Numbers, percents, colours and numeric `Edges`/`Axes` tween against the same shape; anything else snaps.
---@alias Exit { duration?: number, delay?: number, easing?: Easing, spring?: Spring, [string]: any } `animate.exit`: timing as in `Animation` (`duration` or `spring` required once a target is named) plus `property = target` pairs the node eases to after a pass drops it (ADR-0150). A target starts from the shown value, or from the identity: `1` for `opacity`/`scale`, `0.5` for `origin`, alpha 0 for a colour, `0` otherwise.

---@class NodeBase
---@field width? Length|Bound Omitted sizes to content.
---@field height? Length|Bound Omitted sizes to content.
---@field max_width? number|Bound Pixel ceiling `[0, 8192]`, CSS `max-width`. Content past it overflows; `scroll` on the same node scrolls it.
---@field max_height? number|Bound Pixel ceiling `[0, 8192]`, as `max_width`.
---@field min_width? number|Bound Pixel floor `[0, 8192]`, CSS `min-width`; wins over a lower `max_width`.
---@field min_height? number|Bound Pixel floor `[0, 8192]`, as `min_width`.
---@field margin? number|Edges|Bound Outer spacing; a number sets all four edges. Default `0`.
---@field padding? number|Edges|Bound Inner spacing; a number sets all four edges. Default `0`.
---@field align_h? Align|Bound Default `"Start"`. Places the node in its parent: both axes under a stacking parent, only the cross axis under a `row`/`column`/`list`. On a `row` it also packs the children, which ignore their own (`"Stretch"` packs as `"Start"`). `"Stretch"` overrides a pixel size; `"Fill"` off the parent's flow axis overrides alignment.
---@field align_v? Align|Bound Default `"Start"`. As `align_h` with the axes swapped: packs a `column`'s children.
---@field visible? boolean|Bound Default `true`. `false` removes the node from layout, paint and spacing but keeps its subtree frozen in memory (ADR-0124); to switch views, bind the parent's `children`.
---@field opacity? number|Bound `[0, 1]`, default `1`, multiplied down the tree. At `0` the node still takes space and input.
---@field z? number|Bound Sibling paint and hit order, default `0`. Higher paints later and hits first; ties keep declaration order. Layout and focus ignore it; cannot animate (ADR-0259).
---@field scale? number|Axes|Bound `[0, 64]`, default `1`, about `origin`. Paint only: layout and `geometry` see the unscaled box; hit-testing follows the painted one (ADR-0149).
---@field rotate? number|Bound Degrees clockwise about `origin`, `[-8192, 8192]`, default `0`. Paint only.
---@field translate? Axes|Bound Pixel offset `[-8192, 8192]` per axis, applied after `scale` and `rotate`. Paint only.
---@field origin? Axes|Bound Pivot for `scale` and `rotate` as box fractions `[0, 1]`. Default `{ x = 0.5, y = 0.5 }`.
---@field shadow_color? Color|Bound Default `"#000000"`. Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set. Clipped at the parent's box: pad the parent or give it `clip = "None"` (ADR-0254).
---@field shadow_blur? number|Bound CSS `box-shadow` blur radius in px, `[0, 8192]`, default `0` (ADR-0262).
---@field shadow_offset? Axes|Bound Shadow offset in px, `[-8192, 8192]` per axis. Follows the node's transform.
---@field shadow_spread? number|Bound Px the shadow grows (or shrinks) per side, `[-8192, 8192]`. On non-box content it scales the shadow about the box centre.
---@field content_blur? number|Bound Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()`, `[0, 8192]`, default `0`. Clipped like a shadow (ADR-0254).
---@field animate? Animations|Bound Tween named properties to each newly resolved value without running Lua (ADR-0145). The `exit` key is an `Exit` block. Only a node already on screen animates, unless the entry has `from`.
---@field id? string Unique among siblings; matches this node across passes. Siblings without one match by position (ADR-0045).
---@field hover? Bound A `hover(name)` signal; this node's box is its region.
---@field geometry? Bound A `geometry(name)` signal; layout writes this node's surface-local rect into it (ADR-0147).
---@field cursor? Cursor|Bound Pointer shape over this node; the innermost node that sets one wins. Default: `"pointer"` on a `button` with a handler or `submit` and on a link, `"text"` on a `textfield`, else the arrow (ADR-0107).
---@field on_hover? fun(hovered: boolean) Called on each hover edge from pointer Enter, Motion or Leave; layout changes under a still pointer do not call it. Refused without `hover` on the same node.

---Box paint for `rect`, `row`, `column`, `button` and every surface role.
---@class BoxBase
---@field background? Color|Gradient|Bound Default none, which draws nothing (unlike `"#00000000"`). A gradient snaps under `animate`.
---@field mask? Mask|Bound Multiplies the alpha of this node and its subtree (ADR-0255). Cut to the box, or to `radius` under `clip = "Rounded"`. Hit-testing and `blur` ignore it.
---@field radius? number|Bound Corner radius px `[0, 8192]`, default `0`.
---@field corner_shape? "Round"|"Scoop"|Bound Default `"Round"`. `"Scoop"` cuts each corner inward, centred on the corner point; fill, clip and blur follow.
---@field border_color? Color|BorderColors|Bound A string sets all four edges. No default: an edge draws only with both a colour and a width.
---@field border_width? number|Edges|Bound Px `[0, 8192]` per edge; a number sets all four. Default `0`.
---@field blur? boolean|Bound Ask the compositor to blur the desktop behind this box, `ext-background-effect-v1` (ADR-0195). Default `false`; never inferred from a translucent background. Silently nothing without compositor support; strength is the compositor's.
---@field backdrop_blur? number|Bound Gaussian sigma in px over what this surface already painted under the box, CSS `backdrop-filter`, `[0, 8192]`, default `0` (ADR-0256). Never sees the desktop; cut to `radius`/`corner_shape`.
---@field shadow_mode? "Box"|"Content"|Bound Default `"Box"`: CSS `box-shadow` of the box shape, not drawn under the box. `"Content"`: CSS `drop-shadow` of everything painted (ADR-0260).
---@field clip? "Box"|"Rounded"|"None"|Bound Default `"Box"`: children cut to the rectangle. `"Rounded"` also cuts to `radius`, at the cost of an offscreen pass. `"None"` leaves children on the parent's clip (ADR-0257).

---@class RectProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order, at most 10000. A hole ends the array.

---@class RowProps: NodeBase, BoxBase
---@field spacing? number|Bound Px between visible children, default `0`.
---@field children? Node[] Drawn left to right, at most 10000. A hole ends the array.
---@field scroll? Bound A `scroll(name)` signal; makes this a scrolling viewport.

---@class ColumnProps: NodeBase, BoxBase
---@field spacing? number|Bound Px between visible children, default `0`.
---@field children? Node[] Drawn top to bottom, at most 10000. A hole ends the array.
---@field scroll? Bound A `scroll(name)` signal; makes this a scrolling viewport.

---One styled stretch of `text.content` (ADR-0104). A notification body's text spans fit as-is;
---drop image spans, which have no `text` and are refused.
---@class TextRun
---@field text string Empty runs are skipped.
---@field bold? boolean Uses the family's bold face when fontconfig has one.
---@field italic? boolean Uses the family's italic face when fontconfig has one.
---@field underline? boolean Underline in the run's colour.
---@field color? Color Overrides the node's `foreground`.
---@field href? string Passed to the node's `on_link` when clicked; never opened by the engine (ADR-0106).

---@class TextProps: NodeBase
---@field content? string|TextRun[]|Bound A string, or up to 10000 runs drawn as one paragraph. Default `""`.
---@field font_size? number|Bound `[1, 8192]`, default `12`.
---@field font? string|Bound Family placed before the `fonts` chain; absent uses the chain (ADR-0144). `""` raises; an unknown family falls back to the chain.
---@field foreground? Color|Bound Default `"#FFFFFF"`.
---@field elide? "None"|"End"|Bound Default `"None"`. `"End"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line.
---@field wrap? "None"|"Word"|Bound Default `"None"`. `"Word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"Fill"` or a stretched cross axis).
---@field max_lines? number|Bound Line cap under `wrap = "Word"`; `0` or absent is unlimited. Ignored without `wrap`.
---@field text_align? "Start"|"Center"|"End"|Bound Default `"Start"`. Aligns lines inside the node's own box; `Start`/`End` follow each line's reading direction (ADR-0211).
---@field on_link? fun(href: string) Click on a run with an `href` (ADR-0106). Takes the click from any ancestor `button`; plain text passes it through.

---@class IconProps: NodeBase
---@field name? string|Bound Icon theme name, or an absolute image path (ADR-0054). Default `""`, drawing nothing.
---@field size? number|Bound Box side in px, default `12`.
---@field foreground? Color|Bound Colour for the SVG's `currentColor` (CSS `color`), which tints symbolic icons (ADR-0072). Full-colour icons ignore it. Absent keeps the file's colours.

---`image.transition`. Unknown keys are refused.
---@class Transition
---@field duration number Required, ms `[1, 60000]`.
---@field easing? Easing Default `"InOutQuad"`; drives `u_progress`.
---@field shader? string Absolute `.frag` path replacing the built-in dissolve, e.g. `mantle.config_dir .. "/shaders/wipe.frag"` (ADR-0184). Recompiled when the file changes.
--- Shader contract. The engine prepends `#version 300 es`, `highp` precision, its declarations and `#line 1`; write `void main()`:
--- - `v_uv`: box coordinates `0..1`, top-left origin, y down.
--- - `u_progress`: eased progress, clamped to `0..1`. `u_size`: node size in logical px.
--- - `mantle_from(uv)`, `mantle_to(uv)`: outgoing and incoming pictures, premultiplied and already placed by `fit`; transparent outside the picture.
--- - `u_from_rect`, `u_to_rect`: each picture's `(x, y, w, h)` in box fractions (may exceed `0..1` under `"cover"`).
--- - Output: premultiplied RGBA in `fragColor`, same colour space as the inputs. The engine applies `opacity` after.
--- - Names starting `u_` or `mantle_` are reserved. A shader that fails to compile or link, or declares a uniform other than `float`/`vec2`-`vec4`, logs once and falls back to the dissolve. A shader that hangs the GPU hangs the session.
---@field params? table<string, number|number[]> Uniform values by name: a finite number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing uniforms are `0`; unknown names are ignored. Refused without `shader`.

---@class ImageProps: NodeBase
---@field source? string|Bound File path, never a theme name. Default `""`. Animated GIFs loop (ADR-0233).
---@field fit? "cover"|"contain"|"stretch"|Bound Default `"cover"`. No intrinsic size: set `width`/`height`.
---@field async? boolean|Bound Default `false`, decoding in the frame that first draws it. `true` decodes on a worker and draws nothing until ready (ADR-0122); use it for many or large images.
---@field retain? boolean|Bound Default `false`. Keep drawing the last picture while a new `source` decodes, and on a failed decode (ADR-0180, ADR-0183). Needs `async = true` and a stable `id`.
---@field transition? Transition|Bound Cross-fade from the held picture to a newly decoded `source` (ADR-0181, ADR-0186). Implies `retain`; needs `async = true` and a stable `id`. The first picture appears without one.
---@field source_blur? number|Bound Blur sigma in px (a fast box approximation), applied once at decode, `[0, 8192]`, default `0` (ADR-0240). Runs on the decoding thread, so pair large images with `async`; under `async` a change blanks the image until the re-decode lands, and `retain` does not cover it (same `source`). Animated GIFs ignore it.

---Live preview of one output (ADR-0248). No intrinsic size: without `width`/`height` it draws nothing.
---@class CaptureProps: NodeBase
---@field output? string|Bound Connector name, e.g. `"DP-1"`. Default `""`. An unknown name draws nothing and warns once.
---@field fit? "cover"|"contain"|"stretch"|Bound Default `"cover"`, as `image.fit`.
---@field live? boolean|number|Bound Default `false`: capture on show and on each `output` change. `true`: every frame, one in flight. A number: at most that many fps, `(0, 1000]` (ADR-0263). Pauses while hidden or unmapped.
---@field region? Rect|Bound Part of the output in its logical px, placed by `fit` as the whole frame. All keys required, each `[0, 8192]`, size non-zero.
---@field paint_cursor? boolean|Bound Default `false`. Include the pointer in the frame.

---A config fragment shader over the node's box, with no input textures (ADR-0253). No intrinsic
---size and no input; wrap it for clicks. Reads `v_uv`, `u_size` and `u_progress` as in
---`Transition.shader`, writes premultiplied `fragColor`; `opacity`, `shadow_*` and `content_blur` apply.
---@class ShaderProps: NodeBase
---@field source? string|Bound Absolute `.frag` path; relative is refused. Default `""`. A shader that fails to build logs once and draws nothing.
---@field progress? number|Bound `u_progress`, `[-8192, 8192]`, default `0`. Animate this for motion; springs may overshoot.
---@field params? table<string, number|number[]>|Bound Uniforms by name: a number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing ones are `0`. Not tweened.

---@class ButtonProps: NodeBase, BoxBase
---@field children? Node[] Drawn in order, at most 10000. A hole ends the array.
---@field submit? boolean|Bound A click also submits the armed `secure_submit` field, like Enter (ADR-0114). Works without `on_click`.
---@field on_click? fun(rect: Rect, button: "left"|"right"|"middle") On release over the same button that was pressed. `rect` is the button's surface-local box.
---@field on_drag? fun(rect: Rect, pointer: { x: number, y: number }, phase: "start"|"move"|"end") Left-button drag (ADR-0116). `pointer` is button-local and unclamped. `"start"` on press, `"end"` on release (before `on_click`) or when the pointer leaves the surface.
---@field on_wheel? fun(rect: Rect, steps: number) Vertical wheel in notches, positive away from the user, fractional on touchpads (ADR-0116). The innermost handler or scroll container wins.

---@class ListProps: NodeBase
---@field source any[]|Bound Required array; bind a signal to rebuild on change. More than 10000 items without `limit` is an error.
---@field itemfn fun(item: any): Node Required. Builds a node for every built item, visible or not.
---@field key? fun(item: any): string Unique UTF-8 key per item; replaces the node's `id`. Duplicates are refused. Without it items match by position.
---@field limit? integer|Bound Build at most this many items, capped at 10000.
---@field direction? "Vertical"|"Horizontal"|Bound Default `"Vertical"`.
---@field spacing? number|Bound Px between visible items along `direction`, default `0`.
---@field scroll? Bound A `scroll(name)` signal; makes this a scrolling viewport.

---Single-line text input. Reads `wl_keyboard`, not an input method, so no CJK composition or dead
---keys. With `secure_submit` it is masked: keys never reach Lua and go to the capability
---(ADR-0005, ADR-0092). Otherwise `on_change` or `on_submit` makes it plain; with neither it never
---takes focus. A press focuses it; the surface needs `keyboard_interactivity`. The draft lives as
---long as the node; losing focus keeps it (ADR-0108).
---@class TextfieldProps: NodeBase
---No intrinsic size: set `width`/`height`.
---@field placeholder? string|Bound Shown while the field is empty, focused or not (ADR-0135). Never submitted.
---@field mask_character? string|Bound Drawn per typed character in a `secure_submit` field. Default `"•"`; only the first character counts; `""` hides the length.
---@field secure_submit? { capability: string, action: string }|Bound Native target for the secret: `lock`/`authenticate`, `polkit`/`authenticate` or `network`/`connect` (ADR-0027). Makes the field masked.
---@field on_change? fun(text: string) Full text after every edit.
---@field on_submit? fun(text: string) Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field.
---@field autofocus? boolean|Bound Plain fields only: take the keyboard, empty, when the surface gets it or the field appears, calling `on_change("")`. The first in document order wins; never steals from a field already typing or one a press just left (ADR-0112).
---@field on_navigate? fun(key: "up"|"down"|"left"|"right"|"page_up"|"page_down"|"tab"|"backtab") Keys a single-line field does not use, for moving a list selection; repeats while held. `"left"`/`"right"` only when the caret cannot move that way and Shift is up (ADR-0236).
---@field on_cancel? fun(cleared: boolean) Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus (ADR-0102).
---@field font_size? number|Bound `[1, 8192]`, default `12`.
---@field foreground? Color|Bound Default `"#FFFFFF"`.
---@field text_align? "Start"|"Center"|"End"|Bound Default `"Start"`. Aligns the text inside the field's box.

---@param props RectProps
---@return Node
function rect(props) end

---@param props RowProps
---@return Node
function row(props) end

---@param props ColumnProps
---@return Node
function column(props) end

---@param props TextProps
---@return Node
function text(props) end

---@param props IconProps
---@return Node
function icon(props) end

---@param props ImageProps
---@return Node
function image(props) end

---@param props CaptureProps
---@return Node
function capture(props) end

---@param props ShaderProps
---@return Node
function shader(props) end

---@param props ButtonProps
---@return Node
function button(props) end

---@param props ListProps
---@return Node
function list(props) end

---@param props TextfieldProps
---@return Node
function textfield(props) end
