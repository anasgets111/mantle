//! Every property a node kind accepts, with its type, closed choices, range, default and docs: the one
//! table the name check, the shared parsers, `lua-meta/{nodes,surfaces}.lua` and the docs' property
//! tables all read (`stubs.rs` renders the last two).
//!
//! ponytail: the shared parsers read choices, ranges and defaults from here (`parse_keyword`,
//! `parse_number`, `parse_bool`, `style::range_of`). `foreground`, `monitor` and `namespace`
//! (string defaults), the half-open ranges (a popup's `(0, 8192]`, `live`'s `(0, 1000]`) and
//! `style::axis_default` keep their own literals, so those rows' `range`/`absent` are documentation
//! only. Upgrade: an `Absent::Text` and a half-open `range` when one of them next changes.

use Absent::{Bool, Choice, Lua, Number, Prose, Required, Unset};

/// What an absent key means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Absent {
    /// Nothing: the property does nothing until set.
    Unset,
    Required,
    Number(f32),
    Bool(bool),
    /// One of the row's `choices`.
    Choice(&'static str),
    /// Any other Lua literal, as written.
    Lua(&'static str),
    /// A behaviour rather than a value, lower case: `"content"`.
    Prose(&'static str),
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "`ty`, `doc` and `behaviour` are for `stubs.rs`, a test"))]
pub(crate) struct Property {
    pub name: &'static str,
    /// Bits of [`KINDS`].
    pub kinds: u16,
    /// LuaCATS, after the `choices` union when there is one. `Bound` marks a signal-taking property.
    pub ty: &'static str,
    pub choices: &'static [&'static str],
    /// Closed, and enforced by the parser that calls `style::within`.
    pub range: Option<(f32, f32)>,
    pub absent: Absent,
    /// The stub's one-line description, after the range and default it renders itself.
    pub doc: &'static str,
    /// The docs table's cell when it says more than `doc`, with links. Empty falls back to `doc`.
    pub behaviour: &'static str,
}

const fn p(name: &'static str, kinds: u16, ty: &'static str, doc: &'static str) -> Property {
    Property { name, kinds, ty, choices: &[], range: None, absent: Unset, doc, behaviour: "" }
}

impl Property {
    const fn choices(self, choices: &'static [&'static str]) -> Self {
        Self { choices, ..self }
    }
    const fn range(self, low: f32, high: f32) -> Self {
        Self { range: Some((low, high)), ..self }
    }
    const fn absent(self, absent: Absent) -> Self {
        Self { absent, ..self }
    }
    const fn behaviour(self, behaviour: &'static str) -> Self {
        Self { behaviour, ..self }
    }
}

/// Every node kind, in constructor order, with the line its constructor's stub opens with. The last
/// four are root roles (ADR-0040); declaring a `lock` does not lock (ADR-0052 decision 2).
pub(crate) const KINDS: [(&str, &str); 15] = [
    ("rect", ""),
    ("row", ""),
    ("column", ""),
    ("text", ""),
    ("icon", ""),
    ("image", ""),
    ("capture", "Live preview of one output (ADR-0248). No intrinsic size: without `width`/`height` it draws nothing."),
    (
        "shader",
        "A config fragment shader over the node's box, with no input textures (ADR-0253). No intrinsic size and no input; wrap it for clicks. Reads `v_uv`, `u_size` and `u_progress` as in `Transition.shader`, writes premultiplied `fragColor`; `opacity`, `shadow_*` and `content_blur` apply.",
    ),
    ("button", ""),
    ("list", ""),
    (
        "textfield",
        "Single-line text input. Reads `wl_keyboard`, not an input method, so no CJK composition or dead keys. With `secure_submit` it is masked: keys never reach Lua and go to the capability (ADR-0005, ADR-0092). Otherwise `on_change` or `on_submit` makes it plain; with neither it never takes focus. A press focuses it; the surface needs `keyboard_interactivity`. The draft lives as long as the node; losing focus keeps it (ADR-0108). No intrinsic size: set `width`/`height`.",
    ),
    ("panel", "A layer surface (`zwlr_layer_surface_v1`): bar, dock, wallpaper, OSD, launcher."),
    ("window", "An `xdg_toplevel`: settings window, dialog."),
    ("popup", "An `xdg_popup` on its parent: dropdown, context menu, tooltip. No Wayland object while hidden."),
    (
        "lock",
        "An `ext_session_lock_surface_v1` per output, shown while the session is locked. Declaring one does not lock (ADR-0052). At most one per config.",
    ),
];

const RECT: u16 = 1;
const ROW: u16 = 1 << 1;
const COLUMN: u16 = 1 << 2;
const TEXT: u16 = 1 << 3;
const ICON: u16 = 1 << 4;
const IMAGE: u16 = 1 << 5;
const CAPTURE: u16 = 1 << 6;
const SHADER: u16 = 1 << 7;
const BUTTON: u16 = 1 << 8;
const LIST: u16 = 1 << 9;
const TEXTFIELD: u16 = 1 << 10;
const PANEL: u16 = 1 << 11;
const WINDOW: u16 = 1 << 12;
const POPUP: u16 = 1 << 13;
const LOCK: u16 = 1 << 14;
pub(crate) const SURFACES: u16 = PANEL | WINDOW | POPUP | LOCK;
/// The common rows: every kind takes them, and `layout::scene` reads them without checking kind. A
/// root role's own row of the same name overrides one in its stub and docs.
pub(crate) const ALL: u16 = (1 << KINDS.len()) - 1;
/// The box-paint rows: `node::paint_style`'s first arm paints these kinds alike.
pub(crate) const BOX: u16 = RECT | ROW | COLUMN | BUTTON | SURFACES;

pub(crate) const ALIGN: &[&str] = &["Start", "Center", "End", "Stretch"];
pub(crate) const POPUP_ANCHOR: &[&str] =
    &["Top", "Bottom", "Left", "Right", "TopLeft", "TopRight", "BottomLeft", "BottomRight", "Center"];
const TEXT_ALIGN: &[&str] = &["Start", "Center", "End"];
const FIT: &[&str] = &["cover", "contain", "stretch"];

/// A key in no row for its kind is refused, so a misspelled `aling_v` raises instead of being read by
/// nothing. Rows sharing a name agree on `choices`, `range` and a parser-read default
/// (`rows_sharing_a_name_agree`); the stubs and docs keep the order here.
pub(crate) const PROPERTIES: &[Property] = &[
    // Common.
    p("width", ALL, "Length|Bound", "").range(0.0, 8192.0).absent(Prose("content")).behaviour("See [sizes](#sizes)"),
    p("height", ALL, "Length|Bound", "As `width`.").range(0.0, 8192.0).absent(Prose("content")).behaviour("See [sizes](#sizes)"),
    p("max_width", ALL, "number|Bound", "Pixel ceiling, CSS `max-width`. Content past it overflows; `scroll` on the same node scrolls it.")
        .range(0.0, 8192.0)
        .behaviour("Pixel ceiling, CSS `max-width`. Content past it overflows; a `scroll` on the same node scrolls it ([sizes](#sizes))"),
    p("max_height", ALL, "number|Bound", "Pixel ceiling, as `max_width`.").range(0.0, 8192.0),
    p("min_width", ALL, "number|Bound", "Pixel floor, CSS `min-width`; wins over a lower `max_width`.").range(0.0, 8192.0),
    p("min_height", ALL, "number|Bound", "Pixel floor, as `min_width`.").range(0.0, 8192.0),
    p("margin", ALL, "number|Edges|Bound", "Outer spacing; a number sets all four edges. Not range-checked.")
        .absent(Number(0.0))
        .behaviour("Outside the box; part of the room the node takes in its parent. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin))"),
    p("padding", ALL, "number|Edges|Bound", "Inner spacing; a number sets all four edges. Not range-checked.")
        .absent(Number(0.0))
        .behaviour("Inside the box, around its children or text. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin))"),
    p("align_h", ALL, "Bound", r#"Places the node in its parent: both axes under a stacking parent, only the cross axis under a `row`/`column`/`list`. On a `row` it also packs the children, which ignore their own (`"Stretch"` packs as `"Start"`). `"Stretch"` overrides a pixel size; `"Fill"` off the parent's flow axis overrides alignment."#)
        .choices(ALIGN)
        .absent(Choice("Start"))
        .behaviour("See [alignment](#alignment)"),
    p("align_v", ALL, "Bound", "As `align_h` with the axes swapped: packs a `column`'s children.")
        .choices(ALIGN)
        .absent(Choice("Start"))
        .behaviour("See [alignment](#alignment)"),
    p("visible", ALL, "boolean|Bound", "`false` removes the node from layout, paint and spacing but keeps its subtree frozen in memory (ADR-0124); to switch views, bind the parent's `children`.")
        .absent(Bool(true))
        .behaviour("`false` removes the node from layout, paint and spacing and freezes its subtree ([showing and hiding](#showing-hiding-and-switching))"),
    p("opacity", ALL, "number|Bound", "Multiplied down the tree. At `0` the node still takes space and input.").range(0.0, 1.0).absent(Number(1.0)),
    p("z", ALL, "number|Bound", "Sibling paint and hit order. Higher paints later and hits first; ties keep declaration order. Layout and focus ignore it; cannot animate (ADR-0259).")
        .absent(Number(0.0)),
    p("scale", ALL, "number|Axes|Bound", "About `origin`; a missing axis is `1`. Paint only: layout and `geometry` see the unscaled box; hit-testing follows the painted one (ADR-0149).")
        .range(0.0, 64.0)
        .absent(Number(1.0)),
    p("rotate", ALL, "number|Bound", "Degrees clockwise about `origin`. Paint only.").range(-8192.0, 8192.0).absent(Number(0.0)),
    p("translate", ALL, "Axes|Bound", "Pixel offset per axis, a missing one `0`, applied after `scale` and `rotate`. Paint only.")
        .range(-8192.0, 8192.0)
        .absent(Lua("{ x = 0, y = 0 }")),
    p("origin", ALL, "Axes|Bound", "Pivot for `scale` and `rotate` as box fractions; a missing axis is `0.5`.")
        .range(0.0, 1.0)
        .absent(Lua("{ x = 0.5, y = 0.5 }")),
    p("shadow_color", ALL, "Color|Bound", r#"Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set. Clipped at the parent's box: pad the parent or give it `clip = "None"` (ADR-0254)."#)
        .absent(Lua(r##""#000000""##))
        .behaviour("A drop shadow ([shadows](../guide/paint.md#shadows)). Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set"),
    p("shadow_blur", ALL, "number|Bound", "CSS `box-shadow` blur radius in px (ADR-0262).").range(0.0, 8192.0).absent(Number(0.0)),
    p("shadow_offset", ALL, "Axes|Bound", "Shadow offset in px per axis. Follows the node's transform.")
        .range(-8192.0, 8192.0)
        .absent(Lua("{ x = 0, y = 0 }")),
    p("shadow_spread", ALL, "number|Bound", "Px the shadow grows (or shrinks) per side. On non-box content it scales the shadow about the box centre.")
        .range(-8192.0, 8192.0)
        .absent(Number(0.0)),
    p("content_blur", ALL, "number|Bound", "Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()`. Clipped like a shadow (ADR-0254).")
        .range(0.0, 8192.0)
        .absent(Number(0.0))
        .behaviour("Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()` ([blurs](../guide/paint.md#blurs)). Clipped like a shadow"),
    p("animate", ALL, "Animations|Bound", "Tween named properties to each newly resolved value without running Lua (ADR-0145). The `exit` key is an `Exit` block. Only a node already on screen animates, unless the entry has `from`.")
        .behaviour("Per-property tweens and an `exit` block ([animation](../guide/animation.md)). Only a node already on screen animates, unless the entry has `from`"),
    p("id", ALL, "string", "Unique among siblings; matches this node across passes. Siblings without one match by position (ADR-0045).")
        .behaviour("Unique among siblings; matches this node across passes ([identity](#identity-and-reconciliation)). Never a signal"),
    p("hover", ALL, "Bound", "A `hover(name)` signal; this node's box is its region.")
        .behaviour("A `hover(name)` signal the engine sets while the pointer is over this node or its children ([hover](../guide/input.md#hover))"),
    p("geometry", ALL, "Bound", "A `geometry(name)` signal; layout writes this node's surface-local rect into it (ADR-0147).")
        .behaviour("A `geometry(name)` signal the pass writes this node's surface-local rect into ([geometry](../guide/signals.md#geometry-read-a-nodes-laid-out-rect))"),
    p("cursor", ALL, "Cursor|Bound", "Pointer shape over this node; the innermost node that sets one wins (ADR-0107).")
        .absent(Prose(r#"`"pointer"` on a `button` with a handler or `submit` and on a link, `"text"` on a `textfield`, else the arrow"#))
        .behaviour("One of the [cursor names](#cursor-names). The innermost node under the pointer that sets one wins"),
    p("on_hover", ALL, "fun(hovered: boolean)", "Called on each hover edge from pointer Enter, Motion or Leave; layout changes under a still pointer do not call it. Refused without `hover` on the same node."),
    // Box paint.
    p("background", BOX, "Color|Gradient|Bound", r##"Absent draws nothing, unlike an explicit transparent `"#00000000"`. A gradient snaps under `animate`."##)
        .behaviour(r##"A colour or [gradient](#gradients). Absent draws nothing; `"#00000000"` is an explicit transparent fill. A gradient snaps under `animate`"##),
    p("mask", BOX, "Mask|Bound", r#"Multiplies the alpha of this node and its subtree (ADR-0255). Cut to the box, or to `radius` under `clip = "Rounded"`. Hit-testing and `blur` ignore it."#)
        .behaviour("Multiplies the alpha of this node and its subtree; see [Mask](#mask)"),
    p("radius", BOX, "number|Bound", "Corner radius px. Above half the shorter side it clamps, so `radius = 999` makes a pill or circle.").range(0.0, 8192.0).absent(Number(0.0)),
    p("corner_shape", BOX, "Bound", r#"`"Scoop"` cuts each corner inward as a quarter circle centred on the corner point; fill, clip, glass, shadow and the `blur` region follow."#)
        .choices(&["Round", "Scoop"])
        .absent(Choice("Round")),
    p("border_color", BOX, "Color|BorderColors|Bound", "A string sets all four edges; a missing edge has none. An edge draws only with both a colour and a width."),
    p("border_width", BOX, "number|Edges|Bound", "Px per edge; a number sets all four, a missing edge is `0`. Borders draw inside the box and take no layout space.")
        .range(0.0, 8192.0)
        .absent(Number(0.0)),
    p("blur", BOX, "boolean|Bound", "Ask the compositor to blur the desktop behind this box, `ext-background-effect-v1` (ADR-0195). Never inferred from a translucent background. Silently nothing without compositor support; strength is the compositor's.")
        .absent(Bool(false))
        .behaviour("Ask the compositor to blur the desktop behind this box; see [Blurs](#blurs). Never inferred from a translucent background"),
    p("backdrop_blur", BOX, "number|Bound", "Gaussian sigma in px over what this surface already painted under the box, CSS `backdrop-filter` (ADR-0256). Never sees the desktop; cut to `radius`/`corner_shape`.")
        .range(0.0, 8192.0)
        .absent(Number(0.0))
        .behaviour("Gaussian sigma in px over what this surface already painted under the box, CSS `backdrop-filter`; see [Blurs](#blurs)"),
    p("shadow_mode", BOX, "Bound", r#"`"Box"`: CSS `box-shadow` of the box shape, not drawn under the box. `"Content"`: CSS `drop-shadow` of everything painted (ADR-0260)."#)
        .choices(&["Box", "Content"])
        .absent(Choice("Box"))
        .behaviour(r#"`"Box"`: CSS `box-shadow` of the box shape. `"Content"`: CSS `drop-shadow` of everything painted. See [Shadows](#shadows)"#),
    p("clip", BOX, "Bound", r#"`"Box"`: children cut to the rectangle. `"Rounded"` also cuts to `radius`, at the cost of an offscreen pass. `"None"` leaves children on the parent's clip (ADR-0257)."#)
        .choices(&["Box", "Rounded", "None"])
        .absent(Choice("Box"))
        .behaviour(r#"`"Box"` cuts children to the rectangle, `"Rounded"` also to `radius`, `"None"` leaves them on the parent's clip. See [Clip](#clip)"#),
    // rect, button, row, column.
    p("children", RECT | BUTTON, "Node[]|Bound", "Stacked in order: later children paint over earlier ones. At most 10000; a `nil` or `false` entry is an error.")
        .behaviour("Array of node tables, up to 10000; a `nil` or `false` entry is an error. Stacked in order: later children paint over earlier ones. Bind a signal of an array to [switch views](index.md#switching-views-with-ids)"),
    p("children", ROW | COLUMN, "Node[]|Bound", "Laid out in order along the main axis, at most 10000; a `nil` or `false` entry is an error.")
        .behaviour("Array of node tables, up to 10000; a `nil` or `false` entry is an error. Laid out in order along the main axis. Bind a signal of an array to [switch views](index.md#switching-views-with-ids)"),
    p("spacing", ROW | COLUMN, "number|Bound", "Px between visible children; negative values overlap them. Not range-checked.").absent(Number(0.0)),
    p("scroll", ROW | COLUMN, "Bound", "A `scroll(name)` signal; makes this a scrolling viewport along its main axis.")
        .behaviour("A `scroll(name)` signal; makes the node a scrolling viewport along its main axis ([scroll](../guide/input.md#scroll))"),
    // text.
    p("content", TEXT, "string|TextRun[]|Bound", "A string, or up to 10000 runs, drawn as one paragraph.")
        .absent(Lua(r#""""#))
        .behaviour("A string, or an array of up to 10000 [runs](#runs), drawn as one paragraph"),
    p("font", TEXT, "string|Bound", r#"Family placed before the `fonts` chain (ADR-0144). `""` raises; an unknown family falls back to the chain."#)
        .absent(Prose("the `fonts` chain")),
    p("font_size", TEXT, "number|Bound", "Each line is `1.2 × font_size` tall.").range(1.0, 8192.0).absent(Number(12.0)),
    p("foreground", TEXT, "Color|Bound", "A run's `color` overrides it.")
        .absent(Lua(r##""#FFFFFF""##))
        .behaviour("A [colour](../guide/paint.md#colours); a run's `color` overrides it"),
    p("text_align", TEXT, "Bound", "Aligns lines inside the node's own box; `Start`/`End` follow each line's reading direction (ADR-0211). Matters only when the box is wider than the text.")
        .choices(TEXT_ALIGN)
        .absent(Choice("Start")),
    p("wrap", TEXT, "Bound", r#"`"Word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"Fill"` or a stretched cross axis)."#)
        .choices(&["None", "Word"])
        .absent(Choice("None")),
    p("max_lines", TEXT, "number|Bound", r#"Line cap under `wrap = "Word"`; `0` is unlimited, a negative value is refused. Ignored without `wrap`."#).absent(Number(0.0)),
    p("elide", TEXT, "Bound", r#"`"End"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line."#)
        .choices(&["None", "End"])
        .absent(Choice("None")),
    p("on_link", TEXT, "fun(href: string)", "Click on a run with an `href` (ADR-0106); the engine never opens it. Takes the click from any ancestor `button`; plain text passes it through."),
    // icon.
    p("name", ICON, "string|Bound", r#"Icon theme name, or an absolute image path (ADR-0054); `""` draws nothing."#)
        .absent(Lua(r#""""#))
        .behaviour(r#"An icon theme name (`"firefox"`, `"audio-volume-high-symbolic"`), looked up at the drawn size, or an absolute image path, used as is. `""` or a name the theme lacks draws nothing"#),
    p("size", ICON, "number|Bound", "The box is `size` × `size` px; not range-checked.").absent(Number(12.0)),
    p("foreground", ICON, "Color|Bound", "Colour for the SVG's `currentColor` (CSS `color`), which tints symbolic icons (ADR-0072). Full-colour icons ignore it.")
        .absent(Prose("the file's own colours")),
    // image.
    p("source", IMAGE, "string|Bound", r#"File path, never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop (ADR-0233)."#)
        .absent(Lua(r#""""#))
        .behaviour(r#"A file path (`mantle.config_dir .. "/img/a.png"`), never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop"#),
    p("fit", IMAGE, "Bound", r#"`"cover"` fills the box and crops, `"contain"` fits inside it, `"stretch"` distorts to it. No intrinsic size: set `width`/`height`."#)
        .choices(FIT)
        .absent(Choice("cover")),
    p("async", IMAGE, "boolean|Bound", "`false` decodes in the frame that first draws it. `true` decodes on a worker and draws nothing until ready (ADR-0122); use it for many or large images.")
        .absent(Bool(false)),
    p("retain", IMAGE, "boolean|Bound", "Keep drawing the last picture while a new `source` decodes, and on a failed decode (ADR-0180, ADR-0183). Needs `async = true` and a stable `id`.")
        .absent(Bool(false)),
    p("transition", IMAGE, "Transition|Bound", "Cross-fade from the held picture to a newly decoded `source` (ADR-0181, ADR-0186). Implies `retain`; needs `async = true` and a stable `id`. The first picture appears without one.")
        .behaviour("Cross from the held picture to each newly decoded `source`. Implies `retain`; needs `async = true` and a stable `id`. Unknown keys are refused. See [transition](#transition)"),
    p("source_blur", IMAGE, "number|Bound", "Blur sigma in px (a fast box approximation), applied once at decode (ADR-0240). Runs on the decoding thread, so pair large images with `async`; under `async` a change blanks the image until the re-decode lands, and `retain` does not cover it (same `source`). Animated GIFs ignore it.")
        .range(0.0, 8192.0)
        .absent(Number(0.0))
        .behaviour("Blur sigma in px, baked into the pixels once at decode (three box passes approximating a Gaussian); see [blurs](../guide/paint.md#blurs). Animated GIFs ignore it"),
    // capture.
    p("output", CAPTURE, "string|Bound", r#"Connector name, e.g. `"DP-1"`; `""` draws nothing. An unknown name draws nothing and warns once. Changing it starts a fresh capture."#)
        .absent(Lua(r#""""#)),
    p("fit", CAPTURE, "Bound", "As `image.fit`.").choices(FIT).absent(Choice("cover")).behaviour("As on [`image`](image.md)"),
    p("live", CAPTURE, "boolean|number|Bound", "`false`: capture on show and on each `output` change. `true`: every frame, one in flight. A number: at most that many fps, `(0, 1000]` (ADR-0263). Pauses while hidden or unmapped.")
        .absent(Bool(false)),
    p("region", CAPTURE, "Rect|Bound", "Part of the output in its logical px, placed by `fit` as the whole frame. Every key is required and in that range; the size is non-zero.")
        .range(0.0, 8192.0)
        .absent(Prose("the whole output")),
    p("paint_cursor", CAPTURE, "boolean|Bound", "Include the pointer in the frame.").absent(Bool(false)),
    // shader.
    p("source", SHADER, "string|Bound", r#"Absolute `.frag` path; relative is refused, `""` draws nothing. Saving the file recompiles it; one that fails to build logs once and draws nothing."#)
        .absent(Lua(r#""""#)),
    p("progress", SHADER, "number|Bound", "`u_progress`. There is no clock uniform: animate this for motion; the wide range lets a spring overshoot.")
        .range(-8192.0, 8192.0)
        .absent(Number(0.0))
        .behaviour("Becomes `u_progress`. There is no clock uniform: [animate](../guide/animation.md) this for motion; the wide range lets a spring overshoot"),
    p("params", SHADER, "table<string, number|number[]>|Bound", "Uniforms by name: a finite number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing ones are `0`. Not tweened.")
        .absent(Lua("{}")),
    // button.
    p("on_click", BUTTON, r#"fun(rect: Rect, button: "left"|"right"|"middle")"#, "On release over the same button that was pressed, with the same mouse button. `rect` is the button's surface-local box, before transforms."),
    p("on_drag", BUTTON, r#"fun(rect: Rect, pointer: { x: number, y: number }, phase: "start"|"move"|"end")"#, r#"Left-button drag (ADR-0116). `pointer` is button-local and unclamped. `"start"` on press, `"end"` on release (before `on_click`) or when the pointer leaves the surface."#),
    p("on_wheel", BUTTON, "fun(rect: Rect, steps: number)", "Vertical wheel in notches, positive away from the user, fractional on touchpads (ADR-0116). The innermost handler or scroll container wins."),
    p("submit", BUTTON, "boolean|Bound", "A click also submits the armed `secure_submit` field, like Enter (ADR-0114). Works without `on_click` and runs before it.")
        .absent(Bool(false))
        .behaviour("A click also submits the armed [secure field](../guide/input.md#secure-fields), like Enter. Works without `on_click` and runs before it"),
    // list.
    p("source", LIST, "any[]|Bound", "Array; bind a signal to rebuild on change. Missing or `nil` (a capability before its first push) is an empty list; a `nil` hole ends it. More than 10000 items without `limit` is an error.")
        .absent(Prose("empty")),
    p("itemfn", LIST, "fun(item: any): Node", "Builds a node for every built item, visible or not.").absent(Required),
    p("key", LIST, "fun(item: any): string", "Unique UTF-8 key per item; replaces the node's `id`. Duplicates are refused. Without it items match by position."),
    p("limit", LIST, "integer|Bound", "Build at most this many items; above 10000 acts as 10000, `0` builds none."),
    p("direction", LIST, "Bound", "Lays out as a `column` or a `row`.").choices(&["Vertical", "Horizontal"]).absent(Choice("Vertical")),
    p("spacing", LIST, "number|Bound", "Px between visible items along `direction`; negative values overlap them.").absent(Number(0.0)),
    p("scroll", LIST, "Bound", "A `scroll(name)` signal; makes this a scrolling viewport along `direction`.")
        .behaviour("A `scroll(name)` signal; makes the list a scrolling viewport along `direction` ([scroll](../guide/input.md#scroll))"),
    // textfield.
    p("placeholder", TEXTFIELD, "string|Bound", "Shown while the field is empty, focused or not (ADR-0135). Never submitted.").absent(Lua(r#""""#)),
    p("font_size", TEXTFIELD, "number|Bound", "Size of the text and placeholder.").range(1.0, 8192.0).absent(Number(12.0)),
    p("foreground", TEXTFIELD, "Color|Bound", "Colour of the text and placeholder.").absent(Lua(r##""#FFFFFF""##)),
    p("text_align", TEXTFIELD, "Bound", "Aligns the text inside the field's box.").choices(TEXT_ALIGN).absent(Choice("Start")),
    p("autofocus", TEXTFIELD, "boolean|Bound", r#"Plain fields only: take the keyboard, empty, when the surface gets it or the field appears, calling `on_change("")`. The first in document order wins; never steals from a field already typing or one a press just left (ADR-0112)."#)
        .absent(Bool(false)),
    p("on_change", TEXTFIELD, "fun(text: string)", "Full text after every edit."),
    p("on_submit", TEXTFIELD, "fun(text: string)", "Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field."),
    p("on_cancel", TEXTFIELD, "fun(cleared: boolean)", r#"Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus (ADR-0102)."#),
    p("on_navigate", TEXTFIELD, r#"fun(key: "up"|"down"|"left"|"right"|"page_up"|"page_down"|"tab"|"backtab")"#, r#"Keys a single-line field does not use, for moving a list selection; repeats while held. `"left"`/`"right"` only when the caret cannot move that way and Shift is up (ADR-0236)."#),
    p("secure_submit", TEXTFIELD, r#"{ capability: string, action: string, [string]: "no such property" }|Bound"#, "Native target for the secret: `lock`/`authenticate`, `polkit`/`authenticate` or `network`/`connect` (ADR-0027); any other pair or key is an error. Makes the field masked.")
        .behaviour("Makes the field masked; keys never reach Lua. Both non-empty UTF-8 strings: only `lock`/`authenticate`, `polkit`/`authenticate` and `network`/`connect`; any other pair or key is an error ([secure fields](../guide/input.md#secure-fields))"),
    p("mask_character", TEXTFIELD, "string|Bound", r#"Drawn per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length."#)
        .absent(Lua(r#""•""#)),
    // Surface roles. Rows without `Bound` are structural: read once per evaluation (ADR-0216).
    p("id", SURFACES, "string", r#"The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`."#)
        .absent(Required),
    p("layer", PANEL, "", r#"Stacking level, bottom to top. `"Overlay"` draws over fullscreen windows."#)
        .choices(&["Background", "Bottom", "Top", "Overlay"])
        .absent(Required),
    p("anchor", PANEL, r#"{ top?: boolean, bottom?: boolean, left?: boolean, right?: boolean, [string]: "no such property" }"#, "Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge.")
        .absent(Prose("all `false`")),
    p("monitor", PANEL, "string", r#"A connector name, `"All"`, or `"Active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing."#)
        .absent(Lua(r#""All""#))
        .behaviour(r#"A connector name, `"All"` or `"Active"`: which outputs get an instance ([monitor](#monitor))"#),
    p("namespace", PANEL, "string", "The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`).").absent(Lua(r#""mantle-{id}""#)),
    p("width", PANEL, "Length|Bound", r#"Omitted measures the content, capped by the output less the anchored edges' margins; `"NN%"` is of the output. On an axis anchored to both edges, omitted and `"Fill"` both size the surface to the compositor's span; the root node stays content-sized, so give the child `width = "Fill"` to cover it."#)
        .absent(Prose("content"))
        .behaviour("The surface's size ([size](#size))"),
    p("height", PANEL, "Length|Bound", r#"As `width`, against `top`/`bottom`. `"Fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning."#)
        .absent(Prose("content"))
        .behaviour("The surface's size ([size](#size))"),
    p("exclusive", PANEL, r#"boolean|integer|"Ignore"|Bound"#, r#"`false` reserves nothing, a positive integer reserves that many px, `"Ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing."#)
        .absent(Bool(false))
        .behaviour("The space reserved from other windows ([exclusive zones](#exclusive-zones))"),
    p("keyboard_interactivity", PANEL, "Bound", "Whether it takes the keyboard.")
        .choices(&["None", "OnDemand", "Exclusive"])
        .absent(Choice("None"))
        .behaviour("Whether it takes the keyboard ([keyboard focus](#keyboard-focus))"),
    p("margin", PANEL, "number|Edges|Bound", "Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing.").absent(Number(0.0)),
    p("visible", PANEL, "boolean|Bound", "Hiding destroys the layer surface; showing recreates it (ADR-0088).").absent(Bool(true)),
    p("child", PANEL | LOCK, "Node|fun(output: string): Node?", "The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.")
        .behaviour("The root's content. A function runs per output instance with its connector name; `nil` leaves that instance empty ([per-output child](index.md#per-output-child))"),
    p("title", WINDOW, "string|Bound", "The window title.").absent(Lua(r#""""#)),
    p("app_id", WINDOW, "string|Bound", "What compositor window rules match.").absent(Lua(r#""mantle-{id}""#)),
    p("min_size", WINDOW, r#"{ width: number, height: number, [string]: "no such property" }|Bound"#, "Advisory; layout does not enforce it. Both keys required, `0` leaves an axis unconstrained. Also the opening size when the compositor leaves it to the client, else 640x480.")
        .range(0.0, 8192.0)
        .behaviour("Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size ([size](#size))"),
    p("max_size", WINDOW, r#"{ width: number, height: number, [string]: "no such property" }|Bound"#, "Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size.")
        .range(0.0, 8192.0),
    p("on_close", WINDOW, "fun()", "The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing."),
    p("visible", WINDOW, "boolean|Bound", "Opens and closes the window; state and `id` survive (ADR-0049).").absent(Bool(true)),
    p("width", WINDOW, "Length|Bound", "The root's size inside the window, not the window's.")
        .range(0.0, 8192.0)
        .absent(Prose("fill the window"))
        .behaviour("The root's size inside the window, not the window's ([size](#size))"),
    p("height", WINDOW, "Length|Bound", "As `width`.").range(0.0, 8192.0).absent(Prose("fill the window")),
    p("parent", POPUP, "string", "The `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first. A change applies at the next open; a `lock` cannot be a parent.")
        .absent(Required),
    p("anchor_rect", POPUP, "Rect|Bound", "In the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes.").absent(Required),
    p("anchor", POPUP, "Bound", "The point on `anchor_rect` the popup hangs from.").choices(POPUP_ANCHOR).absent(Choice("Center")),
    p("gravity", POPUP, "Bound", r#"The direction it extends from that point: `"Bottom"` hangs it below, `"BottomRight"` below and to the right."#)
        .choices(POPUP_ANCHOR)
        .absent(Choice("Center")),
    p("constraint_adjustment", POPUP, r#"("SlideX"|"SlideY"|"FlipX"|"FlipY"|"ResizeX"|"ResizeY")[]|Bound"#, "How the compositor may keep it on screen; `{}` for none, order is ignored.")
        .absent(Lua(r#"{ "FlipY", "SlideX" }"#)),
    p("offset", POPUP, r#"{ x?: number, y?: number, [string]: "no such property" }|Bound"#, "Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left.")
        .absent(Lua("{ x = 0, y = 0 }")),
    p("width", POPUP, "number|Bound", r#"Pixels in `(0, 8192]`; no `"Fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+)."#)
        .absent(Prose("content")),
    p("height", POPUP, "number|Bound", "As `width`; each axis is independent.").absent(Prose("content")),
    p("grab", POPUP, "boolean|Bound", "Takes an input grab so an outside click dismisses it; it needs a click to grab from, and a denied grab dismisses the popup. `false` for a hover tooltip.")
        .absent(Bool(true))
        .behaviour("Takes an input grab so an outside click dismisses it ([grab](#grab)). `false` for a tooltip"),
    p("on_dismiss", POPUP, "fun()", "The compositor closed it (click outside, denied grab, parent gone); not called when the config hides it. Set `visible = false` here, or it reopens on the next click (ADR-0051)."),
    p("visible", POPUP, "boolean|Bound", "Opens and closes the popup; state and `id` survive (ADR-0049).").absent(Bool(true)),
    p("child", WINDOW | POPUP, "Node", "The one root node; a function `child` is refused."),
    p("width", LOCK, "nil", "Refused: the lock covers each output (ADR-0052)."),
    p("height", LOCK, "nil", "Refused, as `width`."),
    p("visible", LOCK, "nil", "Refused: the session lock decides when it shows."),
];

/// `kind`'s bit, or `None` if it is not a node kind.
pub(crate) fn kind_bit(kind: &str) -> Option<u16> {
    KINDS.iter().position(|(name, _)| *name == kind).map(|index| 1 << index)
}

/// The first row named `name` that `has` holds for.
fn row(name: &str, has: impl Fn(&Property) -> bool) -> Option<&'static Property> {
    PROPERTIES.iter().find(|row| row.name == name && has(row))
}

/// `property`'s closed range, if it has one.
pub(crate) fn range(property: &str) -> Option<(f32, f32)> {
    row(property, |row| row.range.is_some())?.range
}

/// `property`'s choices and its default among them, `None` when it is required.
pub(crate) fn keyword(property: &str) -> (&'static [&'static str], Option<&'static str>) {
    let row = row(property, |row| !row.choices.is_empty()).unwrap_or_else(|| panic!("`{property}` has no choices"));
    (row.choices, if let Choice(name) = row.absent { Some(name) } else { None })
}

/// `property`'s default number.
pub(crate) fn default_number(property: &str) -> f32 {
    match row(property, |row| matches!(row.absent, Number(_))).map(|row| row.absent) {
        Some(Number(n)) => n,
        _ => panic!("`{property}` has no default number"),
    }
}

/// `property`'s default boolean.
pub(crate) fn default_bool(property: &str) -> bool {
    match row(property, |row| matches!(row.absent, Bool(_))).map(|row| row.absent) {
        Some(Bool(b)) => b,
        _ => panic!("`{property}` has no default boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parser reads the first row of a name, so a second that disagrees would document what no
    /// parser does.
    #[test]
    fn rows_sharing_a_name_agree() {
        for (index, a) in PROPERTIES.iter().enumerate() {
            for b in PROPERTIES[index + 1..].iter().filter(|b| b.name == a.name) {
                let both = |x: bool, y: bool| !(x && y);
                assert!(both(!a.choices.is_empty(), !b.choices.is_empty()) || a.choices == b.choices, "{}", a.name);
                assert!(both(a.range.is_some(), b.range.is_some()) || a.range == b.range, "{}", a.name);
                let parsed = |row: &Property| matches!(row.absent, Number(_) | Bool(_) | Choice(_));
                assert!(both(parsed(a), parsed(b)) || a.absent == b.absent, "{}", a.name);
                assert!(a.kinds & b.kinds == 0 || a.kinds == ALL || b.kinds == ALL, "`{}` twice for one kind", a.name);
            }
        }
    }

    /// A choice default names a choice, so `keyword` finds its index.
    #[test]
    fn every_choice_default_is_one_of_its_choices() {
        for row in PROPERTIES {
            if let Choice(name) = row.absent {
                assert!(row.choices.contains(&name), "`{}` defaults to `{name}`, not a choice", row.name);
            }
        }
    }
}
