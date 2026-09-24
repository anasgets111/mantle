//! Every property a node kind accepts, declared as a typed [`Field`]: the one table the name check,
//! the parsers, `lua-meta/{nodes,surfaces}.lua` and the docs' property tables all read
//! (`stubs.rs` renders the last two). A field's Rust type is how the engine parses it
//! ([`Prop::read`]) and what the stubs declare ([`LuaType::lua`]); its `///` block is the stub's
//! words and, in a `Book:` paragraph, the docs table's cell; a group's `///` block opens its kind's
//! constructor stub.
//!
//! ponytail: the half-open ranges (a popup's `(0, 8192]`, `live`'s `(0, 1000]`) and
//! `style::axis_default` keep their own literals, so those rows' `range`/`absent` are documentation
//! only. Upgrade: a half-open `range` and an `Absent::Axes` when one of them next changes.

use Absent::{Bool, Choice, Lua, Number, Prose, Required, Unset};

use crate::image::Fit;
use crate::layout::hit::LogicalPoint;
use crate::layout::node::prop::{
    Bound, Callback, Color, Field, Flag, Handle, Id, Name, Num, OneOf, Path, Pixels, Prop, Refused, Structural, Text,
};
use crate::layout::node::{
    Align, Anchor, AnchorRect, Animations, Axes, Children, ClipShape, ColorOrEdges, ConstraintAdjustment, Content,
    CornerShape, Cursor, Direction, Elide, Exclusive, Fill, Font, Items, KeyboardInteractivity, LayerKind, Limit, Live,
    Mask, MaxLines, NumberOrEdges, Params, PopupAnchor, PopupExtent, PopupOffset, Region, Root, Scale,
    SecureSubmitTarget, ShadowMode, SizeHint, SizeMode, TextAlign, TransitionSpec, Wrap,
};
use crate::lua::VirtualNode;
use crate::lua::luacats::{LuaType, Spelling, fun, spelling};
use crate::text::snap::LogicalRect;
use crate::wayland::{DragPhase, MouseButton, NavigateKey};
use mlua::Value;

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
    /// Any other Lua literal, as written. A string literal is also what [`Text`], [`Name`] and
    /// [`Color`] read an absent key as.
    Lua(&'static str),
    /// A behaviour rather than a value, lower case: `"content"`.
    Prose(&'static str),
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "`ty`, `doc` and `raw` are for `stubs.rs`, a test"))]
pub(crate) struct Property {
    pub name: &'static str,
    /// Bits of [`KINDS`].
    pub kinds: u16,
    /// The field's type in LuaCATS, after the `choices` union when there is one. `Bound` marks a
    /// signal-taking property.
    pub ty: Spelling,
    pub choices: &'static [&'static str],
    /// Closed, and enforced by the field's type.
    pub range: Option<(f32, f32)>,
    pub absent: Absent,
    /// The field's `///` block: the stub's description, after the range and default it renders
    /// itself, and a `Book:` paragraph when the docs table's cell says more, with links.
    pub doc: &'static str,
    /// [`Prop::RAW`]: copied past `resolve_properties` as written.
    pub raw: bool,
}

/// A row for a field of type `T` with the name, kinds and `///` block `props!` hands it.
pub(crate) const fn row<T: Prop>(name: &'static str, kinds: u16, doc: &'static str) -> Property {
    // `stringify!(r#async)`.
    let name = if let [b'r', b'#', ..] = name.as_bytes() { name.split_at(2).1 } else { name };
    Property { name, kinds, ty: T::lua, choices: T::CHOICES, range: None, absent: Unset, doc, raw: T::RAW }
}

impl Property {
    const fn range(self, low: f32, high: f32) -> Self {
        Self { range: Some((low, high)), ..self }
    }
    const fn absent(self, absent: Absent) -> Self {
        Self { absent, ..self }
    }
}

/// Declares each group of rows as a module of typed [`Field`]s, one `const` per property named as
/// in Lua, `ROWS`, the group's rows in order, and `DOC`, its `///` block. A row is
/// `name: Type = meta;` or, for a callback, `name(param: Type, ...) -> Return;`, where `meta`
/// chains [`Property`]'s builders (`range(0.0, 1.0).absent(Number(1.0))`).
macro_rules! props {
    ($($(#[doc = $group_doc:literal])* mod $group:ident($kinds:expr) { $($rows:tt)* })*) => {
        $(
            $(#[doc = $group_doc])*
            #[allow(non_upper_case_globals)]
            pub(crate) mod $group {
                use super::*;
                pub(crate) const DOC: &str = concat!($($group_doc, "\n",)* "");
                props!(@rows $kinds; []; $($rows)*);
            }
        )*
        /// Every group's kinds, `///` block and rows, in declaration order.
        const GROUPS: &[(u16, &str, &[Property])] = &[$(($kinds, $group::DOC, $group::ROWS)),*];
    };
    (@rows $kinds:expr; [$($done:ident)*];) => {
        pub(crate) const ROWS: &[Property] = &[$($done.row),*];
    };
    (@rows $kinds:expr; [$($done:ident)*];
        $(#[doc = $doc:literal])* $name:ident($($param:ident: $param_ty:ty),*) $(-> $ret:ty)? $(= $($meta:ident($($arg:expr),*)).+)?;
        $($rest:tt)*
    ) => {
        pub(crate) const $name: Field<Callback> = Field::new(
            Property {
                ty: || fun(&[$((stringify!($param), spelling::<$param_ty>)),*], props!(@ret $($ret)?)),
                ..row::<Callback>(stringify!($name), $kinds, concat!($($doc, "\n",)* ""))
            } $($(.$meta($($arg),*))+)?
        );
        props!(@rows $kinds; [$($done)* $name]; $($rest)*);
    };
    (@rows $kinds:expr; [$($done:ident)*];
        $(#[doc = $doc:literal])* $name:ident: $ty:ty $(= $($meta:ident($($arg:expr),*)).+)?;
        $($rest:tt)*
    ) => {
        pub(crate) const $name: Field<$ty> =
            Field::new(row::<$ty>(stringify!($name), $kinds, concat!($($doc, "\n",)* "")) $($(.$meta($($arg),*))+)?);
        props!(@rows $kinds; [$($done)* $name]; $($rest)*);
    };
    (@ret) => { None };
    (@ret $ret:ty) => { Some((<$ret as LuaType>::lua, <$ret as LuaType>::OPTIONAL)) };
}

/// Every node kind, in constructor order. The last four are root roles (ADR-0040); declaring a
/// `lock` does not lock (ADR-0052 decision 2).
pub(crate) const KINDS: [&str; 15] = [
    "rect",
    "row",
    "column",
    "text",
    "icon",
    "image",
    "capture",
    "shader",
    "button",
    "list",
    "textfield",
    "panel",
    "window",
    "popup",
    "lock",
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

// A key in no row for its kind is refused, so a misspelled `aling_v` raises instead of being read by
// nothing. Rows sharing a name agree on `choices`, `range` and a parser-read default
// (`rows_sharing_a_name_agree`); the stubs and docs keep the order here.

props! {
    mod common(ALL) {
        /// Book: See [sizes](#sizes)
        width: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// As `width`.
        ///
        /// Book: See [sizes](#sizes)
        height: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// Pixel ceiling, CSS `max-width`. Content past it overflows; `scroll` on the same node scrolls it.
        ///
        /// Book: Pixel ceiling, CSS `max-width`. Content past it overflows; a `scroll` on the same node scrolls it ([sizes](#sizes))
        max_width: Bound<Pixels> = range(0.0, 8192.0);
        /// Pixel ceiling, as `max_width`.
        max_height: Bound<Pixels> = range(0.0, 8192.0);
        /// Pixel floor, CSS `min-width`; wins over a lower `max_width`.
        min_width: Bound<Pixels> = range(0.0, 8192.0);
        /// Pixel floor, as `min_width`.
        min_height: Bound<Pixels> = range(0.0, 8192.0);
        /// Outer spacing; a number sets all four edges. Not range-checked.
        ///
        /// Book: Outside the box; part of the room the node takes in its parent. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin))
        margin: Bound<NumberOrEdges> = absent(Number(0.0));
        /// Inner spacing; a number sets all four edges. Not range-checked.
        ///
        /// Book: Inside the box, around its children or text. A number sets all four edges; not range-checked ([spacing](#spacing-padding-and-margin))
        padding: Bound<NumberOrEdges> = absent(Number(0.0));
        /// Places the node in its parent: both axes under a stacking parent, only the cross axis under a `row`/`column`/`list`. On a `row` it also packs the children, which ignore their own (`"Stretch"` packs as `"Start"`). `"Stretch"` overrides a pixel size; `"Fill"` off the parent's flow axis overrides alignment.
        ///
        /// Book: See [alignment](#alignment)
        align_h: Bound<OneOf<Align>> = absent(Choice("Start"));
        /// As `align_h` with the axes swapped: packs a `column`'s children.
        ///
        /// Book: See [alignment](#alignment)
        align_v: Bound<OneOf<Align>> = absent(Choice("Start"));
        /// `false` removes the node from layout, paint and spacing but keeps its subtree frozen in memory (ADR-0124); to switch views, bind the parent's `children`.
        ///
        /// Book: `false` removes the node from layout, paint and spacing and freezes its subtree ([showing and hiding](#showing-hiding-and-switching))
        visible: Bound<Flag> = absent(Bool(true));
        /// Multiplied down the tree. At `0` the node still takes space and input.
        opacity: Bound<Num> = range(0.0, 1.0).absent(Number(1.0));
        /// Sibling paint and hit order. Higher paints later and hits first; ties keep declaration order. Layout and focus ignore it; cannot animate (ADR-0259).
        z: Bound<Num> = absent(Number(0.0));
        /// About `origin`; a missing axis is `1`. Paint only: layout and `geometry` see the unscaled box; hit-testing follows the painted one (ADR-0149).
        scale: Bound<Scale> = range(0.0, 64.0).absent(Number(1.0));
        /// Degrees clockwise about `origin`. Paint only.
        rotate: Bound<Num> = range(-8192.0, 8192.0).absent(Number(0.0));
        /// Pixel offset per axis, a missing one `0`, applied after `scale` and `rotate`. Paint only.
        translate: Bound<Axes> = range(-8192.0, 8192.0).absent(Lua("{ x = 0, y = 0 }"));
        /// Pivot for `scale` and `rotate` as box fractions; a missing axis is `0.5`.
        origin: Bound<Axes> = range(0.0, 1.0).absent(Lua("{ x = 0.5, y = 0.5 }"));
        /// Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set. Clipped at the parent's box: pad the parent or give it `clip = "None"` (ADR-0254).
        ///
        /// Book: A drop shadow ([shadows](../guide/paint.md#shadows)). Draws when alpha > 0 and `shadow_blur`, `shadow_offset` or `shadow_spread` is set
        shadow_color: Bound<Color> = absent(Lua(r##""#000000""##));
        /// CSS `box-shadow` blur radius in px (ADR-0262).
        shadow_blur: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
        /// Shadow offset in px per axis. Follows the node's transform.
        shadow_offset: Bound<Axes> = range(-8192.0, 8192.0).absent(Lua("{ x = 0, y = 0 }"));
        /// Px the shadow grows (or shrinks) per side. On non-box content it scales the shadow about the box centre.
        shadow_spread: Bound<Num> = range(-8192.0, 8192.0).absent(Number(0.0));
        /// Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()`. Clipped like a shadow (ADR-0254).
        ///
        /// Book: Gaussian sigma in px over this node's painted subtree, CSS `filter: blur()` ([blurs](../guide/paint.md#blurs)). Clipped like a shadow
        content_blur: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
        /// Tween named properties to each newly resolved value without running Lua (ADR-0145). The `exit` key is an `Exit` block. Only a node already on screen animates, unless the entry has `from`.
        ///
        /// Book: Per-property tweens and an `exit` block ([animation](../guide/animation.md)). Only a node already on screen animates, unless the entry has `from`
        animate: Bound<Animations>;
        /// Unique among siblings; matches this node across passes. Siblings without one match by position (ADR-0045).
        ///
        /// Book: Unique among siblings; matches this node across passes ([identity](#identity-and-reconciliation)). Never a signal
        id: Structural<Id>;
        /// A `hover(name)` signal; this node's box is its region.
        ///
        /// Book: A `hover(name)` signal the engine sets while the pointer is over this node or its children ([hover](../guide/input.md#hover))
        hover: Handle;
        /// A `geometry(name)` signal; layout writes this node's surface-local rect into it (ADR-0147).
        ///
        /// Book: A `geometry(name)` signal the pass writes this node's surface-local rect into ([geometry](../guide/signals.md#geometry-read-a-nodes-laid-out-rect))
        geometry: Handle;
        /// Pointer shape over this node; the innermost node that sets one wins (ADR-0107).
        ///
        /// Book: One of the [cursor names](#cursor-names). The innermost node under the pointer that sets one wins
        cursor: Bound<Cursor> = absent(Prose(r#"`"pointer"` on a `button` with a handler or `submit` and on a link, `"text"` on a `textfield`, else the arrow"#));
        /// Called on each hover edge from pointer Enter, Motion or Leave; layout changes under a still pointer do not call it. Refused without `hover` on the same node.
        on_hover(hovered: bool);
    }
    mod paint(BOX) {
        /// Absent draws nothing, unlike an explicit transparent `"#00000000"`. A gradient snaps under `animate`.
        ///
        /// Book: A colour or [gradient](#gradients). Absent draws nothing; `"#00000000"` is an explicit transparent fill. A gradient snaps under `animate`
        background: Bound<Fill>;
        /// Multiplies the alpha of this node and its subtree (ADR-0255). Cut to the box, or to `radius` under `clip = "Rounded"`. Hit-testing and `blur` ignore it.
        ///
        /// Book: Multiplies the alpha of this node and its subtree; see [Mask](#mask)
        mask: Bound<Mask>;
        /// Corner radius px. Above half the shorter side it clamps, so `radius = 999` makes a pill or circle.
        radius: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
        /// `"Scoop"` cuts each corner inward as a quarter circle centred on the corner point; fill, clip, glass, shadow and the `blur` region follow.
        corner_shape: Bound<OneOf<CornerShape>> = absent(Choice("Round"));
        /// A string sets all four edges; a missing edge has none. An edge draws only with both a colour and a width.
        border_color: Bound<ColorOrEdges>;
        /// Px per edge; a number sets all four, a missing edge is `0`. Borders draw inside the box and take no layout space.
        border_width: Bound<NumberOrEdges> = range(0.0, 8192.0).absent(Number(0.0));
        /// Ask the compositor to blur the desktop behind this box, `ext-background-effect-v1` (ADR-0195). Never inferred from a translucent background. Silently nothing without compositor support; strength is the compositor's.
        ///
        /// Book: Ask the compositor to blur the desktop behind this box; see [Blurs](#blurs). Never inferred from a translucent background
        blur: Bound<Flag> = absent(Bool(false));
        /// Gaussian sigma in px over what this surface already painted under the box, CSS `backdrop-filter` (ADR-0256). Never sees the desktop; cut to `radius`/`corner_shape`.
        ///
        /// Book: Gaussian sigma in px over what this surface already painted under the box, CSS `backdrop-filter`; see [Blurs](#blurs)
        backdrop_blur: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
        /// `"Box"`: CSS `box-shadow` of the box shape, not drawn under the box. `"Content"`: CSS `drop-shadow` of everything painted (ADR-0260).
        ///
        /// Book: `"Box"`: CSS `box-shadow` of the box shape. `"Content"`: CSS `drop-shadow` of everything painted. See [Shadows](#shadows)
        shadow_mode: Bound<OneOf<ShadowMode>> = absent(Choice("Box"));
        /// `"Box"`: children cut to the rectangle. `"Rounded"` also cuts to `radius`, at the cost of an offscreen pass. `"None"` leaves children on the parent's clip (ADR-0257).
        ///
        /// Book: `"Box"` cuts children to the rectangle, `"Rounded"` also to `radius`, `"None"` leaves them on the parent's clip. See [Clip](#clip)
        clip: Bound<OneOf<ClipShape>> = absent(Choice("Box"));
    }
    mod stack(RECT | BUTTON) {
        /// Stacked in order: later children paint over earlier ones. At most 10000; a `nil` or `false` entry is an error.
        ///
        /// Book: Array of node tables, up to 10000; a `nil` or `false` entry is an error. Stacked in order: later children paint over earlier ones. Bind a signal of an array to [switch views](index.md#switching-views-with-ids)
        children: Bound<Children>;
    }
    mod flow(ROW | COLUMN) {
        /// Laid out in order along the main axis, at most 10000; a `nil` or `false` entry is an error.
        ///
        /// Book: Array of node tables, up to 10000; a `nil` or `false` entry is an error. Laid out in order along the main axis. Bind a signal of an array to [switch views](index.md#switching-views-with-ids)
        children: Bound<Children>;
        /// Px between visible children; negative values overlap them. Not range-checked.
        spacing: Bound<Num> = absent(Number(0.0));
        /// A `scroll(name)` signal; makes this a scrolling viewport along its main axis.
        ///
        /// Book: A `scroll(name)` signal; makes the node a scrolling viewport along its main axis ([scroll](../guide/input.md#scroll))
        scroll: Handle;
    }
    mod text(TEXT) {
        /// A string, or up to 10000 runs, drawn as one paragraph.
        ///
        /// Book: A string, or an array of up to 10000 [runs](#runs), drawn as one paragraph
        content: Bound<Content> = absent(Lua(r#""""#));
        /// Family placed before the `fonts` chain (ADR-0144). `""` raises; an unknown family falls back to the chain.
        font: Bound<Font> = absent(Prose("the `fonts` chain"));
        /// Each line is `1.2 × font_size` tall.
        font_size: Bound<Num> = range(1.0, 8192.0).absent(Number(12.0));
        /// A run's `color` overrides it.
        ///
        /// Book: A [colour](../guide/paint.md#colours); a run's `color` overrides it
        foreground: Bound<Color> = absent(Lua(r##""#FFFFFF""##));
        /// Aligns lines inside the node's own box; `Start`/`End` follow each line's reading direction (ADR-0211). Matters only when the box is wider than the text.
        text_align: Bound<OneOf<TextAlign>> = absent(Choice("Start"));
        /// `"Word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"Fill"` or a stretched cross axis).
        wrap: Bound<OneOf<Wrap>> = absent(Choice("None"));
        /// Line cap under `wrap = "Word"`; `0` is unlimited, a negative value is refused. Ignored without `wrap`.
        max_lines: Bound<MaxLines> = absent(Number(0.0));
        /// `"End"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line.
        elide: Bound<OneOf<Elide>> = absent(Choice("None"));
        /// Click on a run with an `href` (ADR-0106); the engine never opens it. Takes the click from any ancestor `button`; plain text passes it through.
        on_link(href: String);
    }
    mod icon(ICON) {
        /// Icon theme name, or an absolute image path (ADR-0054); `""` draws nothing.
        ///
        /// Book: An icon theme name (`"firefox"`, `"audio-volume-high-symbolic"`), looked up at the drawn size, or an absolute image path, used as is. `""` or a name the theme lacks draws nothing
        name: Bound<Text> = absent(Lua(r#""""#));
        /// The box is `size` × `size` px; not range-checked.
        size: Bound<Num> = absent(Number(12.0));
        /// Colour for the SVG's `currentColor` (CSS `color`), which tints symbolic icons (ADR-0072). Full-colour icons ignore it.
        foreground: Bound<Color> = absent(Prose("the file's own colours"));
    }
    mod image(IMAGE) {
        /// File path, never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop (ADR-0233).
        ///
        /// Book: A file path (`mantle.config_dir .. "/img/a.png"`), never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop
        source: Bound<Text> = absent(Lua(r#""""#));
        /// `"cover"` fills the box and crops, `"contain"` fits inside it, `"stretch"` distorts to it. No intrinsic size: set `width`/`height`.
        fit: Bound<OneOf<Fit>> = absent(Choice("cover"));
        /// `false` decodes in the frame that first draws it. `true` decodes on a worker and draws nothing until ready (ADR-0122); use it for many or large images.
        r#async: Bound<Flag> = absent(Bool(false));
        /// Keep drawing the last picture while a new `source` decodes, and on a failed decode (ADR-0180, ADR-0183). Needs `async = true` and a stable `id`.
        retain: Bound<Flag> = absent(Bool(false));
        /// Cross-fade from the held picture to a newly decoded `source` (ADR-0181, ADR-0186). Implies `retain`; needs `async = true` and a stable `id`. The first picture appears without one.
        ///
        /// Book: Cross from the held picture to each newly decoded `source`. Implies `retain`; needs `async = true` and a stable `id`. Unknown keys are refused. See [transition](#transition)
        transition: Bound<TransitionSpec>;
        /// Blur sigma in px (a fast box approximation), applied once at decode (ADR-0240). Runs on the decoding thread, so pair large images with `async`; under `async` a change blanks the image until the re-decode lands, and `retain` does not cover it (same `source`). Animated GIFs ignore it.
        ///
        /// Book: Blur sigma in px, baked into the pixels once at decode (three box passes approximating a Gaussian); see [blurs](../guide/paint.md#blurs). Animated GIFs ignore it
        source_blur: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
    }
    /// Live preview of one output (ADR-0248). No intrinsic size: without `width`/`height` it draws nothing.
    mod capture(CAPTURE) {
        /// Connector name, e.g. `"DP-1"`; `""` draws nothing. An unknown name draws nothing and warns once. Changing it starts a fresh capture.
        output: Bound<Text> = absent(Lua(r#""""#));
        /// As `image.fit`.
        ///
        /// Book: As on [`image`](image.md)
        fit: Bound<OneOf<Fit>> = absent(Choice("cover"));
        /// `false`: capture on show and on each `output` change. `true`: every frame, one in flight. A number: at most that many fps, `(0, 1000]` (ADR-0263). Pauses while hidden or unmapped.
        live: Bound<Live> = absent(Bool(false));
        /// Part of the output in its logical px, placed by `fit` as the whole frame. Every key is required and in that range; the size is non-zero.
        region: Bound<Region> = range(0.0, 8192.0).absent(Prose("the whole output"));
        /// Include the pointer in the frame.
        paint_cursor: Bound<Flag> = absent(Bool(false));
    }
    /// A config fragment shader over the node's box, with no input textures (ADR-0253). No intrinsic size and no input; wrap it for clicks. Reads `v_uv`, `u_size` and `u_progress` as in `Transition.shader`, writes premultiplied `fragColor`; `opacity`, `shadow_*` and `content_blur` apply.
    mod shader(SHADER) {
        /// Absolute `.frag` path; relative is refused, `""` draws nothing. Saving the file recompiles it; one that fails to build logs once and draws nothing.
        source: Bound<Path> = absent(Lua(r#""""#));
        /// `u_progress`. There is no clock uniform: animate this for motion; the wide range lets a spring overshoot.
        ///
        /// Book: Becomes `u_progress`. There is no clock uniform: [animate](../guide/animation.md) this for motion; the wide range lets a spring overshoot
        progress: Bound<Num> = range(-8192.0, 8192.0).absent(Number(0.0));
        /// Uniforms by name: a finite number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing ones are `0`. Not tweened.
        params: Bound<Params> = absent(Lua("{}"));
    }
    mod button(BUTTON) {
        /// On release over the same button that was pressed, with the same mouse button. `rect` is the button's surface-local box, before transforms.
        on_click(rect: LogicalRect, button: MouseButton);
        /// Left-button drag (ADR-0116). `pointer` is button-local and unclamped. `"start"` on press, `"end"` on release (before `on_click`) or when the pointer leaves the surface.
        on_drag(rect: LogicalRect, pointer: LogicalPoint, phase: DragPhase);
        /// Vertical wheel in notches, positive away from the user, fractional on touchpads (ADR-0116). The innermost handler or scroll container wins.
        on_wheel(rect: LogicalRect, steps: f64);
        /// A click also submits the armed `secure_submit` field, like Enter (ADR-0114). Works without `on_click` and runs before it.
        ///
        /// Book: A click also submits the armed [secure field](../guide/input.md#secure-fields), like Enter. Works without `on_click` and runs before it
        submit: Bound<Flag> = absent(Bool(false));
    }
    mod list(LIST) {
        /// Array; bind a signal to rebuild on change. Missing or `nil` (a capability before its first push) is an empty list; a `nil` hole ends it. More than 10000 items without `limit` is an error.
        source: Bound<Items> = absent(Prose("empty"));
        /// Builds a node for every built item, visible or not.
        itemfn(item: Value) -> VirtualNode = absent(Required);
        /// Unique UTF-8 key per item; replaces the node's `id`. Duplicates are refused. Without it items match by position.
        key(item: Value) -> String;
        /// Build at most this many items; above 10000 acts as 10000, `0` builds none.
        limit: Bound<Limit>;
        /// Lays out as a `column` or a `row`.
        direction: Bound<OneOf<Direction>> = absent(Choice("Vertical"));
        /// Px between visible items along `direction`; negative values overlap them.
        spacing: Bound<Num> = absent(Number(0.0));
        /// A `scroll(name)` signal; makes this a scrolling viewport along `direction`.
        ///
        /// Book: A `scroll(name)` signal; makes the list a scrolling viewport along `direction` ([scroll](../guide/input.md#scroll))
        scroll: Handle;
    }
    /// Single-line text input. Reads `wl_keyboard`, not an input method, so no CJK composition or dead keys. With `secure_submit` it is masked: keys never reach Lua and go to the capability (ADR-0005, ADR-0092). Otherwise `on_change` or `on_submit` makes it plain; with neither it never takes focus. A press focuses it; the surface needs `keyboard_interactivity`. The draft lives as long as the node; losing focus keeps it (ADR-0108). No intrinsic size: set `width`/`height`.
    mod textfield(TEXTFIELD) {
        /// Shown while the field is empty, focused or not (ADR-0135). Never submitted.
        placeholder: Bound<Text> = absent(Lua(r#""""#));
        /// Size of the text and placeholder.
        font_size: Bound<Num> = range(1.0, 8192.0).absent(Number(12.0));
        /// Colour of the text and placeholder.
        foreground: Bound<Color> = absent(Lua(r##""#FFFFFF""##));
        /// Aligns the text inside the field's box.
        text_align: Bound<OneOf<TextAlign>> = absent(Choice("Start"));
        /// Plain fields only: take the keyboard, empty, when the surface gets it or the field appears, calling `on_change("")`. The first in document order wins; never steals from a field already typing or one a press just left (ADR-0112).
        autofocus: Bound<Flag> = absent(Bool(false));
        /// Full text after every edit.
        on_change(text: String);
        /// Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field.
        on_submit(text: String);
        /// Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus (ADR-0102).
        on_cancel(cleared: bool);
        /// Keys a single-line field does not use, for moving a list selection; repeats while held. `"left"`/`"right"` only when the caret cannot move that way and Shift is up (ADR-0236).
        on_navigate(key: NavigateKey);
        /// Native target for the secret: `lock`/`authenticate`, `polkit`/`authenticate` or `network`/`connect` (ADR-0027); any other pair or key is an error. Makes the field masked.
        ///
        /// Book: Makes the field masked; keys never reach Lua. Both non-empty UTF-8 strings: only `lock`/`authenticate`, `polkit`/`authenticate` and `network`/`connect`; any other pair or key is an error ([secure fields](../guide/input.md#secure-fields))
        secure_submit: Bound<SecureSubmitTarget>;
        /// Drawn per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length.
        mask_character: Bound<Text> = absent(Lua(r#""•""#));
    }
    mod surface(SURFACES) {
        /// The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `monitor = "Active"` keeps the bare `id`.
        id: Structural<Name> = absent(Required);
    }
    /// A layer surface (`zwlr_layer_surface_v1`): bar, dock, wallpaper, OSD, launcher.
    mod panel(PANEL) {
        /// Stacking level, bottom to top. `"Overlay"` draws over fullscreen windows.
        layer: Structural<OneOf<LayerKind>> = absent(Required);
        /// Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge.
        anchor: Structural<Anchor> = absent(Prose("all `false`"));
        /// A connector name, `"All"`, or `"Active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing.
        ///
        /// Book: A connector name, `"All"` or `"Active"`: which outputs get an instance ([monitor](#monitor))
        monitor: Structural<Name> = absent(Lua(r#""All""#));
        /// The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`).
        namespace: Structural<Name> = absent(Lua(r#""mantle-{id}""#));
        /// Omitted measures the content, capped by the output less the anchored edges' margins; `"NN%"` is of the output. On an axis anchored to both edges, omitted and `"Fill"` both size the surface to the compositor's span; the root node stays content-sized, so give the child `width = "Fill"` to cover it.
        ///
        /// Book: The surface's size ([size](#size))
        width: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// As `width`, against `top`/`bottom`. `"Fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning.
        ///
        /// Book: The surface's size ([size](#size))
        height: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// `false` reserves nothing, a positive integer reserves that many px, `"Ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing.
        ///
        /// Book: The space reserved from other windows ([exclusive zones](#exclusive-zones))
        exclusive: Bound<Exclusive> = absent(Bool(false));
        /// Whether it takes the keyboard.
        ///
        /// Book: Whether it takes the keyboard ([keyboard focus](#keyboard-focus))
        keyboard_interactivity: Bound<OneOf<KeyboardInteractivity>> = absent(Choice("None"));
        /// Offset from the anchored edges, not layout margin; one on an edge the panel is not anchored to does nothing.
        margin: Bound<NumberOrEdges> = absent(Number(0.0));
        /// Hiding destroys the layer surface; showing recreates it (ADR-0088).
        visible: Bound<Flag> = absent(Bool(true));
    }
    mod root(PANEL | LOCK) {
        /// The one root node. A function runs per output instance with its connector name (ADR-0121); `nil` leaves that instance empty.
        ///
        /// Book: The root's content. A function runs per output instance with its connector name; `nil` leaves that instance empty ([per-output child](index.md#per-output-child))
        child: Bound<Root>;
    }
    /// An `xdg_toplevel`: settings window, dialog.
    mod window(WINDOW) {
        /// The window title.
        title: Bound<Name> = absent(Lua(r#""""#));
        /// What compositor window rules match.
        app_id: Bound<Name> = absent(Lua(r#""mantle-{id}""#));
        /// Advisory; layout does not enforce it. Both keys required, `0` leaves an axis unconstrained. Also the opening size when the compositor leaves it to the client, else 640x480.
        ///
        /// Book: Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size ([size](#size))
        min_size: Bound<SizeHint> = range(0.0, 8192.0);
        /// Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size.
        max_size: Bound<SizeHint> = range(0.0, 8192.0);
        /// The user asked to close. The window stays open until the config sets `visible = false`; without a handler a close request does nothing.
        on_close();
        /// Opens and closes the window; state and `id` survive (ADR-0049).
        visible: Bound<Flag> = absent(Bool(true));
        /// The root's size inside the window, not the window's.
        ///
        /// Book: The root's size inside the window, not the window's ([size](#size))
        width: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("fill the window"));
        /// As `width`.
        height: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("fill the window"));
    }
    /// An `xdg_popup` on its parent: dropdown, context menu, tooltip. No Wayland object while hidden.
    mod popup(POPUP) {
        /// The `id` of a shown `panel`, `window` or `popup`; hiding the parent closes this popup. On a per-output panel it opens on the clicked instance, else the first. A change applies at the next open; a `lock` cannot be a parent.
        parent: Structural<Name> = absent(Required);
        /// In the parent's surface coordinates; `width`/`height` in `(0, 8192]`, `x`/`y` default `0`. Usually the rect `on_click` passes.
        anchor_rect: Bound<AnchorRect> = absent(Required);
        /// The point on `anchor_rect` the popup hangs from.
        anchor: Bound<OneOf<PopupAnchor>> = absent(Choice("Center"));
        /// The direction it extends from that point: `"Bottom"` hangs it below, `"BottomRight"` below and to the right.
        gravity: Bound<OneOf<PopupAnchor>> = absent(Choice("Center"));
        /// How the compositor may keep it on screen; `{}` for none, order is ignored.
        constraint_adjustment: Bound<ConstraintAdjustment> = absent(Lua(r#"{ "FlipY", "SlideX" }"#));
        /// Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left.
        offset: Bound<PopupOffset> = absent(Lua("{ x = 0, y = 0 }"));
        /// Pixels in `(0, 8192]`; no `"Fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+).
        width: Bound<PopupExtent> = absent(Prose("content"));
        /// As `width`; each axis is independent.
        height: Bound<PopupExtent> = absent(Prose("content"));
        /// Takes an input grab so an outside click dismisses it; it needs a click to grab from, and a denied grab dismisses the popup. `false` for a hover tooltip.
        ///
        /// Book: Takes an input grab so an outside click dismisses it ([grab](#grab)). `false` for a tooltip
        grab: Bound<Flag> = absent(Bool(true));
        /// The compositor closed it (click outside, denied grab, parent gone); not called when the config hides it. Set `visible = false` here, or it reopens on the next click (ADR-0051).
        on_dismiss();
        /// Opens and closes the popup; state and `id` survive (ADR-0049).
        visible: Bound<Flag> = absent(Bool(true));
    }
    mod toplevel(WINDOW | POPUP) {
        /// The one root node; a function `child` is refused.
        child: Bound<VirtualNode>;
    }
    /// An `ext_session_lock_surface_v1` per output, shown while the session is locked. Declaring one does not lock (ADR-0052). At most one per config.
    mod lock(LOCK) {
        /// Refused: the lock covers each output (ADR-0052).
        width: Refused;
        /// Refused, as `width`.
        height: Refused;
        /// Refused: the session lock decides when it shows.
        visible: Refused;
    }
}

/// Every row, in declaration order: what the name check, the lookups below and the stubs read.
pub(crate) fn properties() -> impl Iterator<Item = &'static Property> + Clone {
    GROUPS.iter().flat_map(|(_, _, rows)| rows.iter())
}

/// The `///` block of the group whose kinds are exactly `bit`, which opens that kind's constructor
/// stub: empty for a kind with no group of its own.
#[cfg(test)]
pub(crate) fn kind_doc(bit: u16) -> &'static str {
    GROUPS.iter().find(|(kinds, ..)| *kinds == bit).map_or("", |(_, doc, _)| doc)
}

/// `kind`'s bit, or `None` if it is not a node kind.
pub(crate) fn kind_bit(kind: &str) -> Option<u16> {
    KINDS.iter().position(|name| *name == kind).map(|index| 1 << index)
}

/// The kind whose bit is the lowest one in `kinds`.
pub(crate) fn kind_of(kinds: u16) -> &'static str {
    KINDS[kinds.trailing_zeros() as usize]
}

/// `property`'s closed range, if it has one: the first row of that name with one.
pub(crate) fn range(property: &str) -> Option<(f32, f32)> {
    properties().find(|row| row.name == property && row.range.is_some())?.range
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parser reads the first row of a name, so a second that disagrees would document what no
    /// parser does.
    #[test]
    fn rows_sharing_a_name_agree() {
        let rows: Vec<&Property> = properties().collect();
        for (index, a) in rows.iter().enumerate() {
            for b in rows[index + 1..].iter().filter(|b| b.name == a.name) {
                let both = |x: bool, y: bool| !(x && y);
                assert!(both(!a.choices.is_empty(), !b.choices.is_empty()) || a.choices == b.choices, "{}", a.name);
                assert!(both(a.range.is_some(), b.range.is_some()) || a.range == b.range, "{}", a.name);
                let parsed = |row: &Property| matches!(row.absent, Number(_) | Bool(_) | Choice(_));
                assert!(both(parsed(a), parsed(b)) || a.absent == b.absent, "{}", a.name);
                assert!(a.kinds & b.kinds == 0 || a.kinds == ALL || b.kinds == ALL, "`{}` twice for one kind", a.name);
            }
        }
    }

    /// A choice default names a choice, so `OneOf` finds its index.
    #[test]
    fn every_choice_default_is_one_of_its_choices() {
        for row in properties() {
            if let Choice(name) = row.absent {
                assert!(row.choices.contains(&name), "`{}` defaults to `{name}`, not a choice", row.name);
            }
        }
    }
}
