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
    Bound, Callback, Color, Field, Flag, Focus, Handle, Id, Name, Num, OneOf, Path, Pixels, Prop, Refused, Resets,
    Structural, Text,
};
use crate::layout::node::{
    Align, Anchor, AnchorRect, Animations, Axes, Background, Blend, Caret, Children, ClipShape, ColorOrEdges,
    ConstraintAdjustment, Content, CornerShape, Cursor, Decorations, Direction, Effects, Elide, Exclusive, Fill, Font,
    FontVariations, Images, Items, KeyboardInteractivity, LayerKind, LayoutError, Limit, Live, Mask, MaxLines,
    NumberOrCorners, NumberOrEdges, Params, PathCommands, PopupAnchor, PopupExtent, PopupOffset, Region, Root, Scale,
    SecureSubmitTarget, ShadowMode, Shadows, SizeHint, SizeMode, StrokeCap, StrokeJoin, TextAlign, TransitionSpec,
    TrimAxis, Wrap,
};
use crate::lua::VirtualNode;
use crate::lua::luacats::{LuaType, Spelling, fun, spelling};
use crate::text::snap::LogicalRect;
use crate::wayland::{DragPhase, Escape, KeyPress, MouseButton};
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
#[cfg_attr(not(test), expect(dead_code, reason = "`ty` and `doc` are for `stubs.rs`, a test"))]
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
    /// [`Prop::RAW`]: copied past `resolve_declared` as written.
    pub raw: bool,
    /// [`Prop::REFUSED`]: declared only to refuse.
    pub refused: bool,
    /// [`Prop::read`] without its output, for a pass that drops a refused value.
    pub check: fn(&Property, Option<&Value>) -> Result<(), LayoutError>,
}

fn check<T: Prop>(row: &Property, value: Option<&Value>) -> Result<(), LayoutError> {
    T::read(row, value).map(drop)
}

/// A row for a field of type `T` with the name, kinds and `///` block `props!` hands it.
pub(crate) const fn row<T: Prop>(name: &'static str, kinds: u16, doc: &'static str) -> Property {
    // `stringify!(r#async)`.
    let name = if let [b'r', b'#', ..] = name.as_bytes() { name.split_at(2).1 } else { name };
    Property {
        name,
        kinds,
        ty: T::lua,
        choices: T::CHOICES,
        range: None,
        absent: Unset,
        doc,
        raw: T::RAW,
        refused: T::REFUSED,
        check: check::<T>,
    }
}

impl Property {
    const fn range(self, low: f32, high: f32) -> Self {
        Self { range: Some((low, high)), ..self }
    }
    const fn absent(self, absent: Absent) -> Self {
        Self { absent, ..self }
    }
    const fn only(self, kinds: u16) -> Self {
        Self { kinds, ..self }
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
    "list",
    "textfield",
    "path",
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
const LIST: u16 = 1 << 8;
const TEXTFIELD: u16 = 1 << 9;
const PATH: u16 = 1 << 10;
const PANEL: u16 = 1 << 11;
const WINDOW: u16 = 1 << 12;
const POPUP: u16 = 1 << 13;
const LOCK: u16 = 1 << 14;
pub(crate) const SURFACES: u16 = PANEL | WINDOW | POPUP | LOCK;
/// The common rows: every kind takes them, and `layout::scene` reads them without checking kind. A
/// root role's own row of the same name overrides one in its stub and docs.
pub(crate) const ALL: u16 = (1 << KINDS.len()) - 1;
/// The rows every node but a surface root takes: placement in a parent, which a root lacks.
pub(crate) const PLACED: u16 = ALL & !SURFACES;
/// The box-paint rows: `node::paint_style`'s first arm paints these kinds alike.
pub(crate) const BOX: u16 = RECT | ROW | COLUMN | SURFACES;

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
        margin: Bound<NumberOrEdges> = absent(Number(0.0)).only(PLACED);
        /// Inner spacing; a number sets all four edges. Each edge must be within `[0, 8192]`.
        ///
        /// Book: Inside the box, around its children or text. A number sets all four edges; each edge is within `[0, 8192]` ([spacing](#spacing-padding-and-margin))
        padding: Bound<NumberOrEdges> = range(0.0, 8192.0).absent(Number(0.0));
        /// Places the node in its parent: both axes under a stacking parent, only the cross axis under a `row`/`column`/`list`. On a `row` it also packs the children, which ignore their own (`"stretch"` packs as `"start"`). `"stretch"` overrides a pixel size; `"fill"` off the parent's flow axis overrides alignment.
        ///
        /// Book: See [alignment](#alignment)
        align_h: Bound<OneOf<Align>> = absent(Choice("start")).only(PLACED);
        /// As `align_h` with the axes swapped: packs a `column`'s children.
        ///
        /// Book: See [alignment](#alignment)
        align_v: Bound<OneOf<Align>> = absent(Choice("start")).only(PLACED);
        /// `false` removes the node from layout, paint and spacing but keeps its subtree frozen in memory (ADR-0124); to switch views, bind the parent's `children`.
        ///
        /// Book: `false` removes the node from layout, paint and spacing and freezes its subtree ([showing and hiding](#showing-hiding-and-switching))
        visible: Bound<Flag> = absent(Bool(true));
        /// Multiplied down the tree. At `0` the node still takes space and input.
        opacity: Bound<Num> = range(0.0, 1.0).absent(Number(1.0));
        /// Sibling paint and hit order. Higher paints later and hits first; ties keep declaration order. Layout and focus ignore it; `animate` refuses it (ADR-0259).
        z: Bound<Num> = absent(Number(0.0));
        /// About `origin`; a missing axis is `1`. Paint only: layout and `geometry` see the unscaled box; hit-testing follows the painted one (ADR-0149).
        scale: Bound<Scale> = range(0.0, 64.0).absent(Number(1.0));
        /// Degrees clockwise about `origin`. Paint only.
        rotate: Bound<Num> = range(-8192.0, 8192.0).absent(Number(0.0));
        /// Pixel offset per axis, a missing one `0`, applied after `scale` and `rotate`. Paint only.
        translate: Bound<Axes> = range(-8192.0, 8192.0).absent(Lua("{ x = 0, y = 0 }"));
        /// Pivot for `scale` and `rotate` as box fractions; a missing axis is `0.5`.
        origin: Bound<Axes> = range(0.0, 1.0).absent(Lua("{ x = 0.5, y = 0.5 }"));
        /// Drop shadows, CSS `box-shadow`'s list: the first draws on top; at most 16. Each layer is `{ color, blur, offset = { x, y }, spread, inset, blend }`: `blend` (a `Blend` mode, default `"normal"`) composites the layer onto what is under it; `color` defaults to `"#000000"`, `blur` (CSS blur radius in px) to `0` within `[0, 8192]`, `offset` (px per axis, following the node's transform) to `{ x = 0, y = 0 }` and `spread` (px the shadow grows per side; negative shrinks it, and on non-box content scales it about the box centre) to `0`, each in `[-8192, 8192]`. A layer draws when alpha > 0 and `blur`, `offset` or `spread` is set. Cut by a clipping ancestor (`clip`, a scroll viewport, the surface): pad it (ADR-0254).
        ///
        /// Book: Drop shadows, the first on top ([shadows](../guide/paint.md#shadows)). Each layer is `{ color, blur, offset, spread, inset, blend }`; at most 16. A layer draws when alpha > 0 and `blur`, `offset` or `spread` is set
        shadows: Bound<Shadows>;
        /// Pixel filters, CSS `filter` and `backdrop-filter`: `{ blur, saturate, brightness, contrast, backdrop = { blur, saturate, brightness, contrast } }`. A blur is a Gaussian sigma in px within `[0, 8192]`, default `0`; a colour filter is a factor within `[0, 8]`, default `1`, on straight sRGB as CSS's. The top level filters this node's painted subtree and is clipped like a shadow (ADR-0254); `backdrop` filters what this surface already painted under the box, never the desktop, cut to `radius`/`corner_shape` (ADR-0256), and is a box kind's only. `shader = { source, input, params, images, padding }` runs a fragment shader over the node's painted subtree (ADR-0336). `source` is an absolute `.frag` path, `input` is `"content"` (the default) or `"backdrop"` (what this surface painted under the box; the output replaces it before the node paints, so return `mantle_input(uv)` to leave a pixel; a box kind's only), `params` are uniforms by name and `images` are samplers by name, both as on a `shader` node, and `padding` is logical px `[0, 512]` the program may read and draw past the box. Applied in a fixed order: the backdrop filters and a backdrop shader first, then the node over them, a content shader over that, then the blur and `saturate`, `brightness`, `contrast` (ADR-0334), then `blend`. A shader that fails to build logs once per revision and leaves the node as painted. Saving the `.frag` recompiles it.
        ///
        /// Book: Pixel filters: `blur` (sigma in px, `[0, 8192]`) and the colour filters `saturate`, `brightness`, `contrast` (`[0, 8]`, `1` is off), at the top level and in `backdrop`, and a `shader` over the node's subtree or its backdrop; see [Blurs](../guide/paint.md#blurs) and [Shader effects](../guide/paint.md#shader-effects). `backdrop` and `input = "backdrop"` are for box kinds only
        effect: Bound<Effects>;
        /// How this node's finished subtree composites onto what the surface painted under it: CSS `mix-blend-mode`, plus Apple's `"plus_lighter"` and `"plus_darker"`. Anything but `"normal"` draws the subtree offscreen, copies the pixels under it and runs one blend pass; it never sees the desktop behind the surface. Snaps under `animate`.
        ///
        /// Book: CSS `mix-blend-mode` onto what this surface painted under the node; see [Blend modes](../guide/paint.md#blend-modes)
        blend: Bound<OneOf<Blend>> = absent(Choice("normal"));
        /// Tween named properties to each newly resolved value without running Lua (ADR-0145). `move` eases a matched node to its new parent-relative layout position; an ancestor that shifts needs its own `move`. `exit` runs after removal. Only a node already on screen animates, unless an entry has `from`.
        ///
        /// Book: Per-property tweens, parent-relative layout `move` and an `exit` block ([animation](../guide/animation.md)). An ancestor that shifts needs its own `move`. Only a node already on screen animates, unless a property entry has `from`
        animate: Bound<Animations>;
        /// Unique among siblings; matches this node across passes. Siblings without one match by position (ADR-0045).
        ///
        /// Book: Unique among siblings; matches this node across passes ([identity](#identity-and-reconciliation)). Never a signal
        id: Structural<Id>;
        /// Spoken name for a control. A node with `on_click`, `submit` or `on_key` becomes keyboard focusable when this is set. Give each textfield a name for screen readers.
        accessible_name: Bound<Text> = absent(Lua(r#""""#));
        /// `false` keeps the engine's focus outline off this node; style it from `focused(name)` instead.
        ///
        /// Book: `false` keeps the engine's [focus outline](../guide/input.md#keyboard-controls-and-accessibility) off this node
        focus_ring: Bound<Flag> = absent(Bool(true));
        /// A `focused(name)` signal; true while this node or a node inside it holds keyboard control focus.
        ///
        /// Book: A `focused(name)` signal the engine sets while this node or its children hold [control focus](../guide/input.md#keyboard-controls-and-accessibility)
        focused: Handle;
        /// A `hover(name)` signal; this node's box is its region.
        ///
        /// Book: A `hover(name)` signal the engine sets while the pointer is over this node or its children ([hover](../guide/input.md#hover))
        hover: Handle;
        /// A `pointer(name)` signal; the engine sets it to the pointer's `{ x, y }` from this node's top-left corner while the pointer is over this node or its children, `nil` otherwise.
        ///
        /// Book: A `pointer(name)` signal the engine sets to the pointer's node-local `{ x, y }` while it is over this node or its children ([pointer position](../guide/input.md#pointer-position))
        pointer: Handle;
        /// A `geometry(name)` signal; layout writes this node's surface-local rect into it (ADR-0147).
        ///
        /// Book: A `geometry(name)` signal the pass writes this node's surface-local rect into ([geometry](../guide/signals.md#geometry-read-a-nodes-laid-out-rect))
        geometry: Handle;
        /// Pointer shape over this node; the innermost node that sets one wins (ADR-0107).
        ///
        /// Book: One of the [cursor names](#cursor-names). The innermost node under the pointer that sets one wins
        cursor: Bound<Cursor> = absent(Prose(r#"`"pointer"` on a node with `on_click`, `on_press`, `on_drag`, `on_wheel` or `submit` and on a link, `"text"` on a `textfield`, else the arrow"#));
        /// Called on each hover edge from pointer Enter, Motion or Leave; layout changes under a still pointer do not call it. Refused without `hover` on the same node.
        on_hover(hovered: bool);
        /// A key pressed or repeating while the keyboard is on this node or one inside it, or, on a surface root, anywhere on that surface. It goes to the focused node first, then each ancestor, then the surface root; a handler that returns `true` stops it. A focused `textfield` takes the keys it edits with and passes on the rest (arrows at the caret's edge, Up, Down, paging, Tab, Ctrl chords); Tab moves control focus when it can. Never called while a `secure_submit` field is armed, or for a bare modifier. Without `accessible_name` the node is not focusable and hears only what its descendants pass up.
        ///
        /// Book: A key press or repeat, from the focused node up through its ancestors to the surface; `true` stops it ([key handlers](../guide/input.md#key-handlers)). Needs `accessible_name` to take focus
        on_key(key: KeyPress) -> Option<bool>;
    }
    /// The pointer handlers. The innermost node under the pointer with a handler for the event takes it (ADR-0050); a node with no handler for the event is skipped by that scan.
    mod pointer(ALL) {
        /// On release over the same node that was pressed, with the same mouse button. `rect` is the node's surface-local box, before transforms. `pointer` is node-local and unclamped like `on_drag`'s; Enter, Space and screen-reader activation report the node's centre. A press on a `textfield` that takes the keyboard goes to the field instead.
        on_click(rect: LogicalRect, button: MouseButton, pointer: LogicalPoint);
        /// On press of any mouse button, before `on_click` and before the release. `rect` and `pointer` are as in `on_click`. The only place `toplevel(id)` can move, resize or open the window menu, since compositors honour those for the press serial; an `on_drag` `"start"` works too. Not called for a press on a `textfield`.
        on_press(rect: LogicalRect, button: MouseButton, pointer: LogicalPoint);
        /// Left-button drag (ADR-0116). `pointer` is node-local and unclamped. `"start"` on press, `"end"` on release (before `on_click`) or when the pointer leaves the surface.
        on_drag(rect: LogicalRect, pointer: LogicalPoint, phase: DragPhase);
        /// Vertical wheel in notches, positive away from the user, fractional on touchpads (ADR-0116). The innermost handler or scroll container wins; on one node, the `scroll`.
        on_wheel(rect: LogicalRect, steps: f64);
        /// A click also submits the armed `secure_submit` field, like Enter (ADR-0114). Works without `on_click` and runs before it.
        ///
        /// Book: A click also submits the armed [secure field](../guide/input.md#secure-fields), like Enter. Works without `on_click` and runs before it
        submit: Bound<Flag> = absent(Bool(false));
        /// `false` makes this node and every descendant transparent to the pointer: no click, drag, wheel, hover or cursor, and its box claims no input region, so what is underneath gets them. Inherited; a descendant that sets `true` is hit again, and this node's own handlers, cursor and hover then still apply to it.
        ///
        /// Book: `false` lets the pointer through this node and its descendants to what is underneath; inherited, and a descendant's `true` takes it back, and this node's handlers still apply to it ([pass-through](../guide/input.md#hit-testing))
        hittable: Bound<Flag> = absent(Prose("inherited; `true` at the root"));
    }
    mod paint(BOX) {
        /// A colour, a gradient, or up to 16 layers, first on top; a layer's table form `{ fill = .., blend = .. }` composites it onto everything under it by a `Blend` mode. Absent or `{}` draws nothing, unlike an explicit transparent `"#00000000"`. A gradient and a `blend` snap under `animate`.
        ///
        /// Book: A colour, a [gradient](#gradients) or a list of [layers](#background-layers). Absent draws nothing; `"#00000000"` is an explicit transparent fill. Colour layers tween under `animate`; a gradient snaps
        background: Bound<Background>;
        /// Multiplies the alpha of this node and its subtree (ADR-0255). Cut to the box, or to `radius` under `clip = "rounded"`. Hit-testing and `behind_blur` ignore it.
        ///
        /// Book: Multiplies the alpha of this node and its subtree; see [Mask](#mask)
        mask: Bound<Mask>;
        /// Corner radius px; a number sets all four corners, a missing corner is `0`. Corners too big for a side shrink together, so `radius = 999` makes a pill or circle. Shadows round by the mean corner.
        radius: Bound<NumberOrCorners> = range(0.0, 8192.0).absent(Number(0.0));
        /// `"scoop"` cuts each corner inward as a quarter circle centred on the corner point; fill, clip, glass, shadow and the `behind_blur` region follow.
        corner_shape: Bound<OneOf<CornerShape>> = absent(Choice("round"));
        /// Continuous corners, as Figma's corner smoothing: `0` is the circular arc, `0.6` is close to iOS. A smoothed corner spreads up to `(1 + corner_smoothing) * radius` along each side, less where the side is short. Refused with `corner_shape = "scoop"`. Fill, border, clip, mask, `effect.backdrop` and the `behind_blur` region follow; a `"box"` shadow stays the circular mean-radius approximation.
        corner_smoothing: Bound<Num> = range(0.0, 1.0).absent(Number(0.0));
        /// A string sets all four edges; a missing edge has none. An edge draws only with both a colour and a width. A gradient runs along the whole outline and refuses a per-edge one; it snaps under `animate`.
        border_color: Bound<ColorOrEdges>;
        /// Px per edge; a number sets all four, a missing edge is `0`. Borders draw inside the box and take no layout space.
        border_width: Bound<NumberOrEdges> = range(0.0, 8192.0).absent(Number(0.0));
        /// Ask the compositor to blur the desktop behind this box, `ext-background-effect-v1` (ADR-0195). Never inferred from a translucent background. Silently nothing without compositor support; strength is the compositor's.
        ///
        /// Book: Ask the compositor to blur the desktop behind this box; see [Blurs](#blurs). Never inferred from a translucent background
        behind_blur: Bound<Flag> = absent(Bool(false));
        /// `"box"`: CSS `box-shadow` of the box shape, not drawn under the box. `"content"`: CSS `drop-shadow` of everything painted (ADR-0260).
        ///
        /// Book: `"box"`: CSS `box-shadow` of the box shape. `"content"`: CSS `drop-shadow` of everything painted. See [Shadows](#shadows)
        shadow_mode: Bound<OneOf<ShadowMode>> = absent(Choice("box"));
        /// `"box"`: children cut to the rectangle. `"rounded"` also cuts to `radius`, at the cost of an offscreen pass. `"none"` leaves children on the parent's clip (ADR-0328). A `mask` cuts to the box regardless.
        ///
        /// Book: `"box"` cuts children to the rectangle, `"rounded"` also to `radius`, `"none"` leaves them on the parent's clip; a `mask` cuts to the box regardless. See [Clip](#clip)
        clip: Bound<OneOf<ClipShape>> = absent(Prose(r#"`"box"` on a surface or a `scroll` viewport, else `"none"`"#));
    }
    mod stack(RECT) {
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
    mod flow_layout(ROW | COLUMN) {
        /// `true` gives every visible child one equal main-axis slot, as GTK's `homogeneous`. Content-sized: the slot is the largest child's size with its margins. Sized, `"fill"` or a percent: the slots share the axis after `spacing`, whatever the content. A child with a pixel size keeps it, at the slot's start; any other child fills its slot.
        ///
        /// Book: `true` gives every visible child one equal main-axis slot, as GTK's `homogeneous`; see [equal slots](row-column.md#equal-slots)
        homogeneous: Bound<Flag> = absent(Bool(false));
        /// `true` flows children onto new lines when the next does not fit the main axis, CSS `flex-wrap`. `spacing` sits within a line, `line_spacing` between lines, and the container's `align_*` pack each line and the lines. With `homogeneous`, every cell is the largest child's size across all lines. Needs a bounded main axis (`width`, `"fill"`, `max_*` or a stretching cross axis); content-sized it is one line. Refused with `scroll`.
        ///
        /// Book: `true` flows children onto new lines when the next does not fit the main axis, CSS `flex-wrap`; see [wrapping](row-column.md#wrapping). Refused with `scroll`
        wrap: Bound<Flag> = absent(Bool(false));
        /// Px between lines under `wrap`, `0` by default; negative values overlap them. Ignored without `wrap`.
        line_spacing: Bound<Num> = absent(Number(0.0));
    }
    mod text(TEXT) {
        /// A string, or up to 10000 runs, drawn as one paragraph.
        ///
        /// Book: A string, or an array of up to 10000 [runs](#runs), drawn as one paragraph
        content: Bound<Content> = absent(Lua(r#""""#));
    }
    mod typeface(TEXT | TEXTFIELD) {
        /// Family placed before the `fonts` chain (ADR-0144). `""` raises; an unknown family falls back to the chain.
        font: Bound<Font> = absent(Prose("the `fonts` chain"));
        /// Text size in logical pixels. A `textfield` sizes its placeholder with it.
        font_size: Bound<Num> = range(1.0, 8192.0).absent(Number(12.0));
        /// Line height as a multiple of `font_size`.
        line_height: Bound<Num> = range(0.1, 10.0).absent(Number(1.2));
        /// Extra space between characters in logical pixels. Negative values tighten text.
        letter_spacing: Bound<Num> = range(-100.0, 100.0).absent(Number(0.0));
        /// Font weight from 1 to 1000. A run with `bold = true` uses weight 700.
        font_weight: Bound<Num> = range(1.0, 1000.0).absent(Number(400.0));
        /// Use the family's italic face when available. A run with `italic = true` stays italic.
        italic: Bound<Flag> = absent(Bool(false));
        /// OpenType variation axes by 4-character tag, as CSS `font-variation-settings`: `{ FILL = 1, GRAD = -25, opsz = 24 }`. Values clamp to each face's range; axes a face lacks are ignored. An explicit `wght` overrides `font_weight` and bold runs. Changes snap; `animate` does not tween it.
        ///
        /// Book: OpenType variation axes by 4-character tag (`{ FILL = 1, GRAD = -25, opsz = 24 }`), as CSS `font-variation-settings`. Values clamp to each face's range; axes a face lacks are ignored. An explicit `wght` overrides `font_weight` and bold runs. Changes snap; see [variable fonts](text.md#variable-fonts)
        font_variations: Bound<FontVariations> = absent(Lua("{}"));
        /// A run's `color` overrides it. A `textfield`'s placeholder takes it unless `placeholder_color` is set.
        ///
        /// Book: A [colour](../guide/paint.md#colours); a run's `color` overrides it, and a `textfield`'s placeholder takes it unless `placeholder_color` is set
        foreground: Bound<Color> = absent(Lua(r##""#FFFFFF""##));
        /// Aligns lines inside the node's own box; `"start"`/`"end"` follow each line's reading direction (ADR-0211). Matters only when the box is wider than the text.
        text_align: Bound<OneOf<TextAlign>> = absent(Choice("start"));
    }
    mod text_flow(TEXT) {
        /// `"word"` breaks at words, mid-word when one word is too wide. Needs a bounded width (`width`, `"fill"` or a stretched cross axis).
        wrap: Bound<OneOf<Wrap>> = absent(Choice("none"));
        /// Line cap under `wrap = "word"`; `0` is unlimited, a negative value is refused. Ignored without `wrap`.
        max_lines: Bound<MaxLines> = absent(Number(0.0));
        /// `"end"` ends an over-long line with an ellipsis; under `wrap` it applies to the last kept line.
        elide: Bound<OneOf<Elide>> = absent(Choice("none"));
        /// An `elided(name)` signal; layout writes whether `elide` or `max_lines` removed content.
        elided: Handle;
        /// Click on a run with an `href` (ADR-0106); the engine never opens it. Takes the click from any `on_click`, the text's own included; plain words pass it on.
        on_link(href: String);
    }
    mod icon(ICON) {
        /// Icon theme name, or an absolute image path (ADR-0054); `""` draws nothing.
        ///
        /// Book: An icon theme name (`"firefox"`, `"audio-volume-high-symbolic"`), looked up at the drawn size, or an absolute image path, used as is. `""` or a name the theme lacks draws nothing
        name: Bound<Text> = absent(Lua(r#""""#));
        /// The box is `size` × `size` px; not range-checked.
        size: Bound<Num> = absent(Number(12.0));
        /// Colour for the SVG's `currentColor` (CSS `color`), which tints symbolic icons (ADR-0072); a `-symbolic` icon with no `currentColor` is recoloured whole, hard-coded fills included. Full-colour icons ignore it.
        foreground: Bound<Color> = absent(Prose("the file's own colours"));
    }
    mod image(IMAGE) {
        /// File path, never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop (ADR-0233).
        ///
        /// Book: A file path (`mantle.config_dir .. "/img/a.png"`), never a theme name; `""` draws nothing. PNG, JPEG, WebP, GIF, SVG or SVGZ; animated GIFs loop
        source: Bound<Text> = absent(Lua(r#""""#));
        /// `"cover"` fills the box and crops, `"contain"` fits inside it, `"stretch"` distorts to it. No intrinsic size: set `width`/`height`.
        ///
        /// Book: `"cover"` fills the box and crops, `"contain"` fits inside it, `"stretch"` distorts to it
        fit: Bound<OneOf<Fit>> = absent(Choice("cover"));
        /// `false` decodes in the frame that first draws it. `true` decodes on a worker and draws nothing until the first decode lands (ADR-0122). A resize keeps drawing the previous size, scaled, until the new size decodes; use it for many or large images.
        r#async: Bound<Flag> = absent(Bool(false));
        /// Keep drawing the last picture while a new `source` decodes, and on a failed decode (ADR-0180, ADR-0183). Needs `async = true` and a stable `id`.
        retain: Bound<Flag> = absent(Bool(false));
        /// Cross-fade from the held picture to a newly decoded `source` (ADR-0181, ADR-0186). Implies `retain`; needs `async = true` and a stable `id`. The first picture appears without one.
        ///
        /// Book: Cross from the held picture to each newly decoded `source`. Implies `retain`; needs `async = true` and a stable `id`. Unknown keys are refused. See [transition](#transition)
        transition: Bound<TransitionSpec>;
        /// Blur sigma in px (a fast box approximation), applied once at decode (ADR-0240). Runs on the decoding thread, so pair large images with `async`; under `async` a change blanks the image until the re-decode lands, and `retain` does not cover it (same `source`). Animated GIFs ignore it.
        ///
        /// Book: Blur sigma in px, baked into the pixels once at decode (three box passes approximating a Gaussian); see [blurs](../guide/paint.md#blurs). Animated GIFs ignore it. Under `async`, a change blanks the image until the re-decode lands; `retain` does not cover it
        source_blur: Bound<Num> = range(0.0, 8192.0).absent(Number(0.0));
        /// Corner radius px of the drawn picture, as `rect.radius`: a number sets all four corners, a missing corner is `0`, corners too big for a side shrink together. Rounds the visible picture, so `"contain"` rounds the fitted picture, not the box. Hit-testing ignores it.
        radius: Bound<NumberOrCorners> = range(0.0, 8192.0).absent(Number(0.0));
        /// Continuous corners on the drawn picture, as `rect.corner_smoothing`. The shader that rounds a `transition` approximates a smoothed corner by a superellipse through its endpoints and midpoint.
        corner_smoothing: Bound<Num> = range(0.0, 1.0).absent(Number(0.0));
    }
    /// Preview of one output or window (ADR-0248). No intrinsic size: without `width`/`height` it draws nothing.
    mod capture(CAPTURE) {
        /// Connector name, e.g. `"DP-1"`; `""` draws nothing. An unknown name draws nothing and warns once. Changing it starts a fresh capture.
        output: Bound<Text> = absent(Lua(r#""""#));
        /// A `mantle.windows` entry's `id`; `""` draws nothing. Currently requires Hyprland's exact toplevel mapping and ext capture protocols. Cannot combine with a nonempty `output` or `region`. A closed window clears its preview.
        window: Bound<Text> = absent(Lua(r#""""#));
        /// As `image.fit`.
        ///
        /// Book: As on [`image`](image.md)
        fit: Bound<OneOf<Fit>> = absent(Choice("cover"));
        /// `false`: capture on show and on each target change. `true`: every frame, one in flight. A number: at most that many fps, `(0, 1000]` (ADR-0263). Hiding the node or unmapping its surface drops the capture; showing starts a fresh one.
        live: Bound<Live> = absent(Bool(false));
        /// Part of the output in its logical px, placed by `fit` as the whole frame. Every key is required and in that range; the size is non-zero.
        region: Bound<Region> = range(0.0, 8192.0).absent(Prose("the whole output"));
        /// Include the pointer in the frame.
        paint_cursor: Bound<Flag> = absent(Bool(false));
    }
    /// A config fragment shader over the node's box, with no input textures (ADR-0253). No intrinsic size; without a pointer handler it is transparent to the pointer. Reads `v_uv`, `u_size` and `u_progress` as in `Transition.shader`, writes premultiplied `fragColor`; `opacity`, `shadows` and `effect.blur` apply.
    mod shader(SHADER) {
        /// Absolute `.frag` path; relative is refused, `""` draws nothing. Saving the file recompiles it; one that fails to build logs once and draws nothing.
        ///
        /// Book: Absolute `.frag` path; relative is refused, `""` draws nothing. Compiling, errors and reloads: [the .frag file](#the-frag-file)
        source: Bound<Path> = absent(Lua(r#""""#));
        /// `u_progress`. There is no clock uniform: animate this for motion; the wide range lets a spring overshoot.
        ///
        /// Book: Becomes `u_progress`. There is no clock uniform: [animate](../guide/animation.md) this for motion; the wide range lets a spring overshoot
        progress: Bound<Num> = range(-8192.0, 8192.0).absent(Number(0.0));
        /// Uniforms by name: a finite number for `float`, a list of up to 4096 for `vec2`-`vec4` or an array of either, flattened. Missing ones are `0`. Not tweened.
        params: Bound<Params> = absent(Lua("{}"));
        /// Raster images a shader samples, by name: up to 8 absolute PNG, JPEG or WebP paths, each a `uniform sampler2D <name>` plus `uniform vec2 <name>_size` in pixels. The name is a GLSL identifier, not `u_*`, `mantle_*`, `gl_*` or `*_size`, and has no `__`. A missing or undecodable file samples transparent black and is logged once. Saving the file reloads it. Not tweened.
        ///
        /// Book: Up to 8 absolute PNG, JPEG or WebP paths by sampler name: [images](#images). Missing ones sample transparent black. Not tweened
        images: Bound<Images> = absent(Lua("{}"));
    }
    /// A vector path in node-local logical pixels. Set width and height; there is no intrinsic size.
    mod path(PATH) {
        /// Up to 4096 commands. Each has op M/L/Q/C/A/Z and points containing 2/2/4/6/5/0 numbers. Begin each subpath with M or A. Coordinates are in [-8192, 8192]; arc angles need only be finite.
        commands: Bound<PathCommands> = absent(Lua("{}"));
        /// Fill colour or gradient across the node box. Open subpaths close for filling.
        fill: Bound<Fill>;
        /// Stroke colour or gradient across the node box.
        stroke: Bound<Fill>;
        /// Stroke width in logical pixels; centered on the path.
        stroke_width: Bound<Num> = range(0.0, 8192.0).absent(Number(1.0));
        /// How each open stroke end, trimmed ones included, finishes.
        stroke_cap: Bound<OneOf<StrokeCap>> = absent(Choice("butt"));
        /// How stroked segments meet at a corner.
        stroke_join: Bound<OneOf<StrokeJoin>> = absent(Choice("miter"));
        /// Where the stroke starts, as a fraction of the length of every subpath in order, closing segments included. The fill is untrimmed.
        trim_start: Bound<Num> = range(0.0, 1.0).absent(Number(0.0));
        /// Where the stroke ends, as `trim_start`; at or before `trim_start` draws no stroke.
        trim_end: Bound<Num> = range(0.0, 1.0).absent(Number(1.0));
        /// What `trim_start` and `trim_end` measure. `"length"`: fractions of the path's length. `"x"`: fractions of the node's width; the stroke keeps what lies inside that band of the box, cut at its edges with `stroke_cap` on every cut end, and the band stays put while `shift` moves the geometry through it (ADR-0332).
        trim_axis: Bound<OneOf<TrimAxis>> = absent(Choice("length"));
        /// Pixel offset of the geometry inside the node, applied before trimming and stroking. The box, the `trim_axis = "x"` band and the fill gradient do not move. Paint only.
        shift: Bound<Axes> = range(-8192.0, 8192.0).absent(Lua("{ x = 0, y = 0 }"));
    }
    mod list(LIST) {
        /// Array; bind a signal to rebuild on change. Missing or `nil` (a capability before its first push) is an empty list; a `nil` hole ends it. More than 10000 items without `limit` is an error.
        source: Items = absent(Prose("empty"));
        /// Builds a node for every built item, visible or not.
        itemfn(item: Value) -> VirtualNode = absent(Required);
        /// Unique UTF-8 key per item; replaces the node's `id`. Duplicates are refused. Without it items match by position.
        key(item: Value) -> String;
        /// Build at most this many items; above 10000 acts as 10000, `0` builds none.
        limit: Bound<Limit>;
        /// Lays out as a `column` or a `row`.
        direction: Bound<OneOf<Direction>> = absent(Choice("vertical"));
        /// Px between visible items along `direction`; negative values overlap them.
        spacing: Bound<Num> = absent(Number(0.0));
        /// A `scroll(name)` signal; makes this a scrolling viewport along `direction`.
        ///
        /// Book: A `scroll(name)` signal; makes the list a scrolling viewport along `direction` ([scroll](../guide/input.md#scroll))
        scroll: Handle;
    }
    /// Single-line text input. Plain fields read `wl_keyboard` and compose through text-input-v3 when available on their keyboard-focused surface. With `secure_submit` it is masked: keys never reach Lua and go to the capability (ADR-0005, ADR-0092). Otherwise `on_change` or `on_submit` makes it plain; with neither it never takes focus. A press focuses it; the surface needs `keyboard_interactivity`. The draft lives as long as the node; losing focus keeps it (ADR-0108). Intrinsic height is one line, `font_size` times `line_height`; `width` has none, so set it.
    mod textfield(TEXTFIELD) {
        /// A `focus_target(name)` handle. An `on_click` can call `:request()` to return keys after its state change; the field must be visible on that click's keyboard-focused surface or a popup under it. Any other value fails the pass.
        focus_target: Focus;
        /// Shown while the field is empty, focused or not (ADR-0135). Never submitted.
        placeholder: Bound<Text> = absent(Lua(r#""""#));
        /// Colour of the placeholder.
        placeholder_color: Bound<Color> = absent(Prose("`foreground`"));
        /// The caret bar: `{ color, width, height, radius }`. `color` defaults to `foreground`; the selection highlight keeps `foreground`. `width` is px, default a sixteenth of `font_size` rounded, at least `1`. `height` is px, or a fraction of the line height when `1` or less; default the whole line, centred on it. `radius` is px, default `0`. Each key takes a signal. Paint only: `animate` snaps it.
        caret: Bound<Caret>;
        /// Renders like a field but takes no keyboard focus (Tab skips it, a press does not focus it, `focus_target` requests and `autofocus` pass over it) and draws no caret; `set_text` still reaches it. A focused field that becomes disabled loses focus and keeps its draft. Dim it yourself by binding colours to the same signal.
        disabled: Bound<Flag> = absent(Bool(false));
        /// Most grapheme clusters the field holds; `0` is unlimited and a negative value is refused. Typing, paste, IME commits and `focus_target(name):set_text(text)` cut what they insert at the limit, secure fields included. Lowering it below the current text keeps that text; edits can then only shorten it. The cut is silent, so a limit below a password's length truncates it.
        max_length: Bound<MaxLines> = absent(Number(0.0));
        /// Plain fields only: seeds the draft once, when the field enters the tree (a new node: a changed `id` or `key` counts as new), with the value at that moment, read without subscribing: writing the signal alone does not re-resolve the field. Later changes are ignored and an emptied field stays empty; `set_text` pushes new text. Like `set_text`: cut at `max_length`, caret at the end, no undo history, no `on_change`; hidden and disabled fields are seeded too. Refused with `secure_submit`, control characters and over 64 KiB.
        initial_text: Bound<Text> = absent(Lua(r#""""#));
        /// Plain fields only: take the keyboard, with the draft reset to `initial_text` (`""` when unset) and `on_change` called with it, when the surface gets it or the field appears. The first in document order wins; never steals from a field already typing or one a press just left (ADR-0112).
        autofocus: Bound<Flag> = absent(Bool(false));
        /// Full text after every edit.
        on_change(text: String);
        /// Enter with the full text; the field stays focused and clears. Never fires on a `secure_submit` field.
        on_submit(text: String);
        /// What Escape does in a plain field. `"clear"` empties the draft (`on_change("")` if it had text), then gives up focus if `on_cancel` is set. `"blur"` keeps the draft and gives up focus. `"pass"` keeps both and does not take the key: it goes up through `on_key`, then to the surface's `on_escape`. A `secure_submit` field ignores it and always scrubs and stays armed. Read when the field takes focus.
        escape: Bound<OneOf<Escape>> = absent(Choice("clear"));
        /// Escape; `cleared` says whether it removed text. A plain field clears (firing `on_change("")` only if there was text), gives up focus, then calls this. A `secure_submit` field scrubs and stays armed. Without it Escape clears and keeps focus (ADR-0102).
        on_cancel(cleared: bool);
        /// Native target for the secret: `lock`/`authenticate`, `polkit`/`authenticate`, `network`/`connect`, `secrets`/`store` with a `name`, or `bluetooth`/`pair` with the request id and MAC in `name`. Makes the field masked.
        ///
        /// Book: Makes the field masked; bytes never reach Lua. Targets: `lock`/`authenticate`, `polkit`/`authenticate`, `network`/`connect`, `network`/`vpn_secret` with a request id and key in `name`, `secrets`/`store` with a public `name`, or `bluetooth`/`pair` with a request id and MAC in `name` ([secure fields](../guide/input.md#secure-fields))
        secure_submit: Bound<SecureSubmitTarget>;
        /// Drawn per typed character in a `secure_submit` field. Only the first character counts; `""` hides the length.
        mask_character: Bound<Text> = absent(Lua(r#""•""#));
    }
    mod surface(SURFACES) {
        /// The surface's identity across reloads, unique among surfaces. A `panel`'s or `lock`'s per-output instances are `"{id}@{output}"`; `output = "active"` keeps the bare `id`.
        id: Structural<Name> = absent(Required);
    }
    /// A layer surface (`zwlr_layer_surface_v1`): bar, dock, wallpaper, OSD, launcher.
    mod panel(PANEL) {
        /// Stacking level, bottom to top. `"overlay"` draws over fullscreen windows.
        layer: Structural<OneOf<LayerKind>> = absent(Required);
        /// Edges to pin to; an absent edge is `false`. None pinned centres the surface; one edge centres it along that edge.
        anchor: Structural<Anchor> = absent(Prose("all `false`"));
        /// A connector name, `"all"`, or `"active"`: one instance on the output the compositor picks at each show, refusing a `"NN%"` size and a function `child` (ADR-0246). An unknown connector warns and creates nothing.
        ///
        /// Book: A connector name, `"all"` or `"active"`: which outputs get an instance ([output](#output))
        output: Structural<Name> = absent(Lua(r#""all""#));
        /// The layer namespace compositor rules match (Hyprland `layerrule`, niri `layer-rule`).
        namespace: Structural<Name> = absent(Lua(r#""mantle-{id}""#));
        /// Omitted measures content unless both edges of that axis are anchored; then the compositor spans the surface and its configured extent fills the root too. `"NN%"` is of the output. `max_width`/`max_height` cap the root.
        ///
        /// Book: The surface's size ([size](#size))
        width: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// As `width`, against `top`/`bottom`. `"fill"` without both edges of its axis anchored is a protocol error: the surface stays hidden with a warning.
        ///
        /// Book: The surface's size ([size](#size))
        height: Bound<SizeMode> = range(0.0, 8192.0).absent(Prose("content"));
        /// `false` reserves nothing, a positive integer reserves that many px, `"ignore"` also overlaps others' zones. `true` reserves the configured height when exactly one of `top`/`bottom` is anchored and `left`/`right` match (both or neither), the width in the transposed case, else nothing.
        ///
        /// Book: The space reserved from other windows ([exclusive zones](#exclusive-zones))
        exclusive_zone: Bound<Exclusive> = absent(Bool(false));
        /// Whether it takes the keyboard.
        ///
        /// Book: Whether it takes the keyboard ([keyboard focus](#keyboard-focus))
        keyboard_interactivity: Bound<OneOf<KeyboardInteractivity>> = absent(Choice("none"));
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
        /// Book: Advisory hint to the compositor; layout does not enforce it. Both keys required, `0` leaves that axis unconstrained. Also the opening size on an axis the compositor leaves to the client ([size](#size))
        min_size: Bound<SizeHint> = range(0.0, 8192.0);
        /// Advisory, as `min_size`. A non-zero axis below `min_size`'s is refused; also clamps the opening size.
        max_size: Bound<SizeHint> = range(0.0, 8192.0);
        /// Who draws the window's frame: `"server"` asks the compositor for its decorations, `"client"` leaves the frame to the app. A compositor without `zxdg_decoration_manager_v1` always leaves it to the app; `toplevel(id):state().decoration` says which was chosen.
        decorations: Bound<OneOf<Decorations>> = absent(Choice("server"));
        /// Room around the window's frame for its shadow, rounded to whole px: the root fills the frame plus this band, and the compositor sizes, tiles and snaps by the frame alone. Sizes from the compositor and `min_size`/`max_size` are the frame's. A number sets all four edges.
        ///
        /// Book: Room around the frame for a client-drawn shadow; the compositor sizes and tiles by the frame alone. A number sets all four edges ([client-side decoration](#client-side-decoration))
        geometry_inset: Bound<NumberOrEdges> = range(0.0, 256.0).absent(Number(0.0));
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
        anchor: Bound<OneOf<PopupAnchor>> = absent(Choice("center"));
        /// The direction it extends from that point: `"bottom"` hangs it below, `"bottom_right"` below and to the right.
        gravity: Bound<OneOf<PopupAnchor>> = absent(Choice("center"));
        /// How the compositor may keep it on screen; `{}` for none, order is ignored.
        constraint_adjustment: Bound<ConstraintAdjustment> = absent(Lua(r#"{ "flip_y", "slide_x" }"#));
        /// Pixel nudge after `anchor` and `gravity`; an absent axis is `0`, negative moves up or left.
        offset: Bound<PopupOffset> = absent(Lua("{ x = 0, y = 0 }"));
        /// Pixels in `(0, 8192]`; no `"fill"` or `%`. Omitted sizes to the content, capped at the first output's size and the root's `max_width`/`max_height`; an open popup follows it through `xdg_popup.reposition` (xdg-shell v3+).
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
    mod closable(PANEL | WINDOW | POPUP) {
        /// Escape pressed while this surface or a popup under it has the keyboard and no focused field took it: a field with text to clear or an `on_cancel` keeps its own Escape. Once per press; the innermost shown popup declaring it wins, with no order promised among sibling popups. Never on a surface without `keyboard_interactivity`.
        on_escape();
        /// `state` and `scroll` handles written back when this surface stops being shown: `visible` turning false, a reload removing it, its last output leaving, or its parent closing (a popup). A state returns to its declared `initial`, running its `on_change`; a scroll to the top. Anything else in the list fails the evaluation (ADR-0289).
        ///
        /// Book: `state` and `scroll` handles written back when the surface stops being shown: a state to its `initial`, a scroll to the top ([reset on close](index.md#reset-on-close))
        reset_on_close: Resets = absent(Lua("{}"));
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
            // Same name, other type (`wrap` is a string on a `text`): nothing to agree on.
            for b in rows[index + 1..].iter().filter(|b| b.name == a.name && (b.ty)() == (a.ty)()) {
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
