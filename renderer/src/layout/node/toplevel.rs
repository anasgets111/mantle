//! `xdg_toplevel`/`xdg_positioner` specs, including window and popup value types. These describe
//! live `xdg_shell` objects, not layer-shell or session-lock surfaces. `title`, `app_id` and the
//! size hints are live requests on a mapped toplevel, so a signal updates them in place (ADR-0044
//! decision 1); `app_id` defaults to `"mantle-{id}"`.

use mlua::Value;

use crate::text::snap::LogicalRect;

use super::prop::{Keyword, keywords};
use super::*;
use fields::{popup, window};

/// The advisory `{ width, height }` size hint. Layout does not enforce it; Wayland receives it
/// through `set_min_size`/`set_max_size`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeHint {
    pub width: f32,
    pub height: f32,
}

spelled!(SizeHint => r#"{ width: number, height: number, [string]: "no such property" }"#);

/// `None` means no request; `Some(0, 0)` sends an unconstrained request. A present hint must name
/// both axes; use `0` for an unconstrained axis.
impl Prop for SizeHint {
    type Out = Option<SizeHint>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<SizeHint>, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(None);
        };
        let Value::Table(table) = value else {
            return Err(invalid(
                property,
                format!("expected a `{{ width, height }}` table, got {}", preview_for_error(value)),
            ));
        };
        only_keys(property, table, &["width", "height"])?;
        let axis = |key: &str| -> Result<f32, LayoutError> {
            let n = input::field::<Option<f32>>(property, table, key)?.ok_or_else(|| {
                invalid(
                    property,
                    format!("`{key}` is required -- a size hint names both axes, or use 0 for an unconstrained one"),
                )
            })?;
            // Negative values make the request fail (`invalid_size`).
            let (low, high) = row.range.expect("a size hint has a range");
            if !(low..=high).contains(&n) {
                return Err(invalid(
                    property,
                    format!("`{key}` {}", marshal::out_of_range(low, high, marshal::number_word(n))),
                ));
            }
            Ok(n)
        };
        Ok(Some(SizeHint { width: axis("width")?, height: axis("height")? }))
    }
}

/// Checks `set_max_size`'s `max >= min` rule before Wayland sees it. Zero means unset, so it is not
/// below the minimum; a bad pair becomes a `LayoutError` in `rescue`, not `invalid_size`
/// on the Wayland connection.
fn check_max_size_above_min(min: Option<SizeHint>, max: Option<SizeHint>) -> Result<(), LayoutError> {
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(());
    };
    for (axis, min_n, max_n) in [("width", min.width, max.width), ("height", min.height, max.height)] {
        if max_n > 0.0 && max_n < min_n {
            return Err(invalid(
                "max_size",
                format!(
                    "`{axis}` is {max_n}, below `min_size`'s {min_n} -- a maximum under the minimum raises xdg_toplevel's invalid_size"
                ),
            ));
        }
    }
    Ok(())
}

/// The one live `xdg_toplevel` spec for a top-level `window` (ADR-0040 decision 1). There is
/// no `WindowTopology`: title, app id, and size hints update the live object; `visible` creates and
/// destroys it rather than swapping the generation. Only `id` changes the declared set and swaps
/// generation (ADR-0001, ADR-0049 decisions 1-3). A window has one object regardless of outputs
/// (ADR-0038 decision 3); callbacks and visibility remain scene properties.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// Advisory: parsed and carried, never enforced against the resolved tree.
    pub min_size: Option<SizeHint>,
    pub max_size: Option<SizeHint>,
    pub decorations: Decorations,
    /// The band between the buffer's edge and the window geometry, for a client-drawn shadow.
    pub geometry_inset: EdgeInsets,
}

keywords! {
    /// Who draws a window's frame: the compositor (`zxdg_decoration_manager_v1`) or the app.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Decorations {
        #[default]
        Server,
        Client,
    }
}

pub fn window_spec(properties: &PropMap) -> Result<WindowSpec, LayoutError> {
    let id = fields::surface::id.read(properties)?;
    let app_id = window::app_id.read_with_id(properties, &id)?;
    let min_size = window::min_size.read(properties)?;
    let max_size = window::max_size.read(properties)?;
    check_max_size_above_min(min_size, max_size)?;
    let decorations = window::decorations.read(properties)?;
    let geometry_inset = window::geometry_inset.read(properties)?;
    Ok(WindowSpec {
        id,
        title: window::title.read(properties)?,
        app_id,
        min_size,
        max_size,
        decorations,
        geometry_inset,
    })
}

keywords! {
    /// The shared `anchor`/`gravity` values. `Center` maps to protocol `none`, which centers an
    /// unspecified axis; `crate::wayland` maps this local enum to the positioner protocol.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum PopupAnchor {
        Top,
        Bottom,
        Left,
        Right,
        TopLeft,
        TopRight,
        BottomLeft,
        BottomRight,
        /// The protocol's `none`, which is also its default for both requests.
        #[default]
        Center,
    }
}

keywords! {
    /// One `constraint_adjustment` permission.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Adjustment {
        SlideX,
        SlideY,
        FlipX,
        FlipY,
        ResizeX,
        ResizeY,
    }
}

/// The six independent adjustment permissions. Array order is irrelevant because the compositor
/// applies fixed Flip, Slide, Resize precedence and the request is a bitmask. Default is
/// `{ "flip_y", "slide_x" }`, not the protocol's empty default (ADR-0040 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintAdjustment {
    pub slide_x: bool,
    pub slide_y: bool,
    pub flip_x: bool,
    pub flip_y: bool,
    pub resize_x: bool,
    pub resize_y: bool,
}

impl ConstraintAdjustment {
    /// Protocol default: no adjustment. An empty array parses here.
    pub const NONE: Self =
        Self { slide_x: false, slide_y: false, flip_x: false, flip_y: false, resize_x: false, resize_y: false };
}

impl Default for ConstraintAdjustment {
    fn default() -> Self {
        Self { flip_y: true, slide_x: true, ..Self::NONE }
    }
}

spelled!(ConstraintAdjustment => Vec::<Adjustment>::lua());

impl Prop for ConstraintAdjustment {
    type Out = ConstraintAdjustment;
    fn read(_: &Property, value: Option<&Value>) -> Result<ConstraintAdjustment, LayoutError> {
        let Some(value) = value else {
            return Ok(ConstraintAdjustment::default());
        };
        let Value::Table(table) = value else {
            return Err(invalid(
                "constraint_adjustment",
                format!("expected an array table, got {}", preview_for_error(value)),
            ));
        };
        let mut adjustment = ConstraintAdjustment::NONE;
        for at in 1..=input::array_len("constraint_adjustment", table, MAX_ARRAY_ELEMENTS)? {
            let entry: Value = table.raw_get(at).map_err(|e| invalid("constraint_adjustment", e.to_string()))?;
            let Value::String(s) = entry else {
                return Err(invalid(
                    "constraint_adjustment",
                    format!("expected a string entry, got {}", preview_for_error(&entry)),
                ));
            };
            // Flags make repeats no-ops; array position has no meaning.
            let name = checked_string("constraint_adjustment", &s)?;
            let Some(at) = Adjustment::NAMES.iter().position(|known| *known == name) else {
                let message =
                    crate::lua::marshal::expected_one_of(Adjustment::NAMES, &name, &format!("string {name:?}"));
                return Err(invalid("constraint_adjustment", message));
            };
            match Adjustment::VALUES[at] {
                Adjustment::SlideX => adjustment.slide_x = true,
                Adjustment::SlideY => adjustment.slide_y = true,
                Adjustment::FlipX => adjustment.flip_x = true,
                Adjustment::FlipY => adjustment.flip_y = true,
                Adjustment::ResizeX => adjustment.resize_x = true,
                Adjustment::ResizeY => adjustment.resize_y = true,
            }
        }
        Ok(adjustment)
    }
}

/// The signed pixel nudge after anchor and gravity; negative values move up or left.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PopupOffset {
    pub x: f32,
    pub y: f32,
}

spelled!(PopupOffset => r#"{ x?: number, y?: number, [string]: "no such property" }"#);

impl Prop for PopupOffset {
    type Out = PopupOffset;
    fn read(_: &Property, value: Option<&Value>) -> Result<PopupOffset, LayoutError> {
        let Some(value) = value else {
            return Ok(PopupOffset::default());
        };
        let Value::Table(table) = value else {
            return Err(invalid("offset", format!("expected an `{{ x, y }}` table, got {}", preview_for_error(value))));
        };
        only_keys("offset", table, &["x", "y"])?;
        let axis = |key: &str| -> Result<f32, LayoutError> {
            Ok(input::field::<Option<f32>>("offset", table, key)?.unwrap_or(0.0))
        };
        Ok(PopupOffset { x: axis("x")?, y: axis("y")? })
    }
}

/// A deferred `anchor_rect` or popup size: 1x1, because zero leaves the positioner incomplete.
/// `App::apply_resolved_state` replaces it before creation (ADR-0049's second amendment), so
/// `expand_instances` measures a signal-sized popup against 1x1 until its first configure.
const DEFERRED_POPUP_EXTENT: f32 = 1.0;

/// The parent-local `anchor_rect`, reused from `on_click` (ADR-0050 decision 3). `x`/`y` default
/// to 0, but `width`/`height` must be in `(0, 8192]`: negative sizes raise `invalid_input`, zero
/// leaves the positioner incomplete and raises `invalid_positioner` at `get_popup`.
pub(crate) struct AnchorRect;

/// `n` if `xdg_positioner::set_size` accepts it, else an error opening with `subject`.
fn popup_extent(property: &str, subject: &str, n: f32, bad: &str) -> Result<f32, LayoutError> {
    if !(n > 0.0 && n <= 8192.0) {
        return Err(invalid(
            property,
            format!("{subject} be within (0, 8192], got {} -- {bad} is a protocol error", marshal::number_word(n)),
        ));
    }
    Ok(n)
}

spelled!(AnchorRect => LogicalRect::lua());

impl Prop for AnchorRect {
    type Out = LogicalRect;
    fn read(_: &Property, value: Option<&Value>) -> Result<LogicalRect, LayoutError> {
        let value = value.ok_or_else(|| {
            invalid(
                "anchor_rect",
                "required for `popup`, got nothing -- a popup with no anchor rectangle raises invalid_positioner at get_popup",
            )
        })?;
        let Value::Table(table) = value else {
            return Err(invalid(
                "anchor_rect",
                format!("expected an `{{ x, y, width, height }}` table, got {}", preview_for_error(value)),
            ));
        };
        only_keys("anchor_rect", table, LogicalRect::KEYS)?;
        let origin = |key: &str| -> Result<f32, LayoutError> {
            Ok(input::field::<Option<f32>>("anchor_rect", table, key)?.unwrap_or(0.0))
        };
        let extent = |key: &str| -> Result<f32, LayoutError> {
            let n = input::field::<Option<f32>>("anchor_rect", table, key)?.ok_or_else(|| {
                invalid("anchor_rect", format!("`{key}` is required and must be greater than 0 -- a zero-size anchor rectangle raises invalid_positioner"))
            })?;
            popup_extent("anchor_rect", &format!("`{key}` must"), n, "a zero size")
        };
        Ok(LogicalRect { x: origin("x")?, y: origin("y")?, width: extent("width")?, height: extent("height")? })
    }

    fn deferred(_: &Property) -> Result<LogicalRect, LayoutError> {
        Ok(LogicalRect { x: 0.0, y: 0.0, width: DEFERRED_POPUP_EXTENT, height: DEFERRED_POPUP_EXTENT })
    }
}

/// Popup `width`/`height`, narrower than [`SizeMode`]: no `"fill"` and no percent, because
/// there is no parent box for either to mean anything against -- `xdg_positioner::set_size` takes a
/// number, and the compositor places the popup rather than fitting it into something.
///
/// Omitted is [`SizeMode::Content`], on the same terms as every other node: [`SizeMode`]'s own
/// error says content sizing has no literal and the property is left off instead. That axis is then
/// whatever the resolved tree measures, and `wayland::surface::App::apply_resolved_state` reads it
/// off the root's box on the pass that opens the popup. A number is still a number, and still has
/// to be in `(0, 8192]`: `set_size` raises `invalid_input` on a zero or negative size.
pub(crate) struct PopupExtent;

spelled!(PopupExtent => f32::lua());

impl Prop for PopupExtent {
    type Out = SizeMode;
    fn read(row: &Property, value: Option<&Value>) -> Result<SizeMode, LayoutError> {
        let property = row.name;
        let Some(value) = value else {
            return Ok(SizeMode::Content);
        };
        let n = value_as_f32(property, value)?.ok_or_else(|| {
            invalid(
                property,
                format!(
                    "expected a number, got {} -- a popup has no \"fill\" and no percent; omit the property to size it to its content",
                    preview_for_error(value)
                ),
            )
        })?;
        popup_extent(property, "must", n, "a zero or negative size").map(SizeMode::Pixels)
    }

    fn deferred(_: &Property) -> Result<SizeMode, LayoutError> {
        Ok(SizeMode::Pixels(DEFERRED_POPUP_EXTENT))
    }
}

/// One top-level popup's `xdg_popup`/`xdg_positioner` fields. Its parent is a surface id,
/// because the protocol roots the popup through `xdg_surface.get_popup` or
/// `zwlr_layer_surface_v1.get_popup`. The object exists only while shown and the positioner is
/// consumed at `get_popup`, so all fields re-read on open; only declaration changes topology
/// (ADR-0001, ADR-0049 decisions 1 and 3). `grab` defaults to `true` so outside clicks dismiss a
/// dropdown (ADR-0040 decision 2); taking it needs a real input serial for one poll turn, and the
/// compositor may deny it (ADR-0049 amendment).
#[derive(Debug, Clone, PartialEq)]
pub struct PopupSpec {
    pub id: String,
    /// The `id` of the `panel` or `window` this popup anchors to. Read with the same lossy
    /// conversion the surface `id` uses on the other side of the match, so the two agree.
    pub parent: String,
    pub anchor_rect: LogicalRect,
    /// [`SizeMode::Pixels`] for a declared number, [`SizeMode::Content`] for an omitted axis. Only
    /// those two: [`PopupExtent`] admits nothing else. A `Content` axis carries no number
    /// here because there is none until the tree is solved; `wayland::surface` resolves it against
    /// the root's measured box before the positioner is built.
    pub width: SizeMode,
    pub height: SizeMode,
    pub anchor: PopupAnchor,
    pub gravity: PopupAnchor,
    pub constraint_adjustment: ConstraintAdjustment,
    pub offset: PopupOffset,
    pub grab: bool,
}

pub fn popup_spec(properties: &PropMap) -> Result<PopupSpec, LayoutError> {
    // `parent` is structural: `get_popup` pins this popup to one parent instance
    // (ADR-0051 decision 1).
    let parent = popup::parent.read(properties)?;
    if parent.is_empty() {
        return Err(invalid("parent", "must name the `id` of the `panel` or `window` this popup anchors to"));
    }
    Ok(PopupSpec {
        id: fields::surface::id.read(properties)?,
        parent,
        anchor_rect: popup::anchor_rect.read(properties)?,
        width: popup::width.read(properties)?,
        height: popup::height.read(properties)?,
        anchor: popup::anchor.read(properties)?,
        gravity: popup::gravity.read(properties)?,
        constraint_adjustment: popup::constraint_adjustment.read(properties)?,
        offset: popup::offset.read(properties)?,
        grab: popup::grab.read(properties)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::signal_lua;

    #[test]
    fn window_spec_reads_every_toplevel_field_in_one_pass() {
        let lua = mlua::Lua::new();
        let spec = window_spec(&props_from_src(
            &lua,
            r#"return { kind = "window", id = "settings", title = "Mantle Settings", app_id = "mantle.{id}",
                                min_size = { width = 320, height = 240 }, max_size = { width = 1280, height = 960 } }"#,
        ))
        .unwrap();
        assert_eq!(
            spec,
            WindowSpec {
                id: "settings".to_string(),
                title: "Mantle Settings".to_string(),
                app_id: "mantle.{id}".to_string(),
                min_size: Some(SizeHint { width: 320.0, height: 240.0 }),
                max_size: Some(SizeHint { width: 1280.0, height: 960.0 }),
                decorations: Decorations::Server,
                geometry_inset: EdgeInsets::default(),
            }
        );
    }

    #[test]
    fn a_bad_window_field_is_rejected_naming_it() {
        let lua = mlua::Lua::new();
        // (fields, the property the error names, a word its detail holds)
        for (fields, property, detail) in [
            (r#"title = "x""#, "id", ""), // the same refusal a panel without an id gets
            (r#"id = "w", decorations = "none""#, "decorations", ""),
            (r#"id = "w", min_size = { width = -1, height = 240 }"#, "min_size", ""),
            (r#"id = "w", min_size = { width = 320 }"#, "min_size", "height"),
            (r#"id = "w", max_size = 800"#, "max_size", ""),
            (
                r#"id = "w", min_size = { width = 800, height = 600 }, max_size = { width = 400, height = 600 }"#,
                "max_size",
                "",
            ),
        ] {
            let err =
                window_spec(&props_from_src(&lua, format!(r#"return {{ kind = "window", {fields} }}"#))).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: got, detail: d, .. } if got == property && d.contains(detail)),
                "{fields}: {err}"
            );
        }
    }

    #[test]
    fn geometry_inset_takes_a_number_or_per_edge_table_and_refuses_negative_or_huge_bands() {
        let lua = mlua::Lua::new();
        let read = |inset: &str| {
            window_spec(&props_from_src(
                &lua,
                format!(r#"return {{ kind = "window", id = "w", geometry_inset = {inset} }}"#),
            ))
            .map(|spec| spec.geometry_inset)
        };
        assert_eq!(read("24").unwrap(), EdgeInsets { top: 24.0, right: 24.0, bottom: 24.0, left: 24.0 });
        assert_eq!(
            read("{ left = 8, bottom = 30 }").unwrap(),
            EdgeInsets { left: 8.0, bottom: 30.0, ..Default::default() }
        );
        for bad in ["-1", "{ top = 257 }", r#""wide""#] {
            assert!(
                matches!(
                    read(bad).unwrap_err(),
                    LayoutError::InvalidProperty { property, .. } if property == "geometry_inset"
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_window_takes_an_empty_title_its_mantle_app_id_no_size_hints_and_server_decorations() {
        let lua = mlua::Lua::new();
        let spec = window_spec(&props_from_src(&lua, r#"return { kind = "window", id = "settings" }"#)).unwrap();
        assert_eq!(spec.title, "", "not the id");
        assert_eq!(spec.app_id, "mantle-settings");
        assert_eq!((spec.min_size, spec.max_size), (None, None));
        assert_eq!(spec.decorations, Decorations::Server);
        let spec = window_spec(&props_from_src(
            &lua,
            r#"return { kind = "window", id = "settings", decorations = "client" }"#,
        ))
        .unwrap();
        assert_eq!(spec.decorations, Decorations::Client);
    }

    #[test]
    fn a_zero_max_size_axis_means_unset_and_does_not_collide_with_min_size() {
        let lua = mlua::Lua::new();
        assert_eq!(
            window_spec(&props_from_src(
                &lua,
                r#"return { kind = "window", id = "settings", min_size = { width = 800, height = 600 },
                                max_size = { width = 0, height = 0 } }"#,
            ))
            .unwrap()
            .max_size,
            Some(SizeHint { width: 0.0, height: 0.0 })
        );
    }

    #[test]
    fn a_signal_in_a_window_title_or_app_id_resolves_because_both_requests_are_valid_on_a_live_toplevel() {
        let lua = signal_lua();
        for (field, value) in [("title", "Now Playing"), ("app_id", "mantle.later")] {
            let signal = crate::lua::signal::Signal::new_live(
                Value::String(lua.create_string(value).unwrap()),
                crate::lua::signal::DirtyFlag::new(),
            )
            .0;
            lua.globals().set("s", signal).unwrap();
            let src = format!(r#"return {{ kind = "window", id = "w", {field} = s }}"#);
            let spec =
                window_spec(&resolve_declared(&props_from_src(&lua, src), "window", false, &lua).unwrap()).unwrap();
            assert_eq!(if field == "title" { spec.title } else { spec.app_id }, value);
        }
    }

    /// Every popup fixture needs the four required properties, so only the property under test
    /// varies. `extra` is spliced in as further table entries, and a key it repeats *overrides* the
    /// default above it -- a Lua table constructor performs its assignments in order, so
    /// `{ width = 200, width = 0 }` is a table with `width == 0`. That is how a test declares one
    /// bad value without restating the other three good ones.
    fn popup_props(lua: &mlua::Lua, extra: &str) -> PropMap {
        props_from_src(
            lua,
            format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                 anchor_rect = {{ x = 10, y = 0, width = 24, height = 24 }},
                                 width = 200, height = 300 {extra} }}"#
            ),
        )
    }

    #[test]
    fn popup_spec_reads_every_positioner_field_in_one_pass() {
        let lua = mlua::Lua::new();
        let props = popup_props(
            &lua,
            r#", anchor = "bottom_left", gravity = "bottom_right",
                    constraint_adjustment = { "slide_y", "resize_x" }, offset = { x = -4, y = 2 }, grab = false"#,
        );
        let spec = popup_spec(&props).unwrap();
        assert_eq!(
            spec,
            PopupSpec {
                id: "menu".to_string(),
                parent: "bar".to_string(),
                anchor_rect: LogicalRect { x: 10.0, y: 0.0, width: 24.0, height: 24.0 },
                width: SizeMode::Pixels(200.0),
                height: SizeMode::Pixels(300.0),
                anchor: PopupAnchor::BottomLeft,
                gravity: PopupAnchor::BottomRight,
                constraint_adjustment: ConstraintAdjustment {
                    slide_y: true,
                    resize_x: true,
                    ..ConstraintAdjustment::NONE
                },
                offset: PopupOffset { x: -4.0, y: 2.0 },
                grab: false,
            }
        );
    }

    #[test]
    fn an_anchor_rect_omitting_x_and_y_defaults_them_to_zero() {
        let lua = mlua::Lua::new();
        let props = popup_props(&lua, r#", anchor_rect = { width = 24, height = 24 }"#);
        let rect = popup_spec(&props).unwrap().anchor_rect;
        assert_eq!(rect, LogicalRect { x: 0.0, y: 0.0, width: 24.0, height: 24.0 });
    }

    #[test]
    fn an_omitted_popup_width_or_height_is_measured_from_the_tree_rather_than_refused() {
        // An omitted axis is `Content` here as it is on every other node, so a tooltip needs no
        // hand-guessed size; `wayland::surface::popup_requested_size` turns it into the number
        // the positioner needs, off the box the pass measured.
        let lua = mlua::Lua::new();
        for (present, omitted) in [("width", "height"), ("height", "width")] {
            let spec = popup_spec(&props_from_src(
                &lua,
                format!(
                    r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                     anchor_rect = {{ width = 24, height = 24 }}, {present} = 200 }}"#
                ),
            ))
            .expect("an omitted axis is not an error");
            let (declared, measured) =
                if omitted == "height" { (spec.width, spec.height) } else { (spec.height, spec.width) };
            assert_eq!(declared, SizeMode::Pixels(200.0), "the declared axis keeps its number");
            assert_eq!(measured, SizeMode::Content, "the omitted `{omitted}` is measured");
        }
    }

    #[test]
    fn popup_spec_defaults() {
        let lua = mlua::Lua::new();
        let spec = popup_spec(&popup_props(&lua, "")).unwrap();
        assert_eq!((spec.anchor, spec.gravity), (PopupAnchor::Center, PopupAnchor::Center));
        assert_eq!(
            spec.constraint_adjustment,
            ConstraintAdjustment { flip_y: true, slide_x: true, ..ConstraintAdjustment::NONE }
        );
        assert_eq!(spec.offset, PopupOffset { x: 0.0, y: 0.0 });
        assert!(spec.grab);
    }

    #[test]
    fn a_bad_popup_field_is_rejected_naming_it() {
        let lua = mlua::Lua::new();
        for (extra, property) in [
            (r#", width = "fill""#, "width"), // a popup has no fill
            (r#", offset = 4"#, "offset"),
            (r#", parent = """#, "parent"),
            (r#", constraint_adjustment = { "flip_y", "SlideZ" }"#, "constraint_adjustment"),
            (r#", anchor = "Middle""#, "anchor"),
            (r#", gravity = "Downward""#, "gravity"),
            (r#", width = 0"#, "width"),
            (r#", height = 0"#, "height"),
            (r#", anchor_rect = { x = 0, y = 0, width = 0, height = 24 }"#, "anchor_rect"),
            (r#", anchor_rect = { x = 0, y = 0, width = 24, height = 0 }"#, "anchor_rect"),
            (r#", anchor_rect = { x = 0, y = 0, width = -24, height = 24 }"#, "anchor_rect"),
            (r#", anchor_rect = 24"#, "anchor_rect"),
            (r#", constraint_adjustment = "flip_y""#, "constraint_adjustment"),
            (r#", grab = 1"#, "grab"),
        ] {
            let err = popup_spec(&popup_props(&lua, extra)).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: got, .. } if got == property),
                "{extra}: {err}"
            );
        }
        // A required property left out, which `popup_props` cannot do.
        for (source, property) in [
            (r#"id = "menu", anchor_rect = { width = 1, height = 1 }, width = 8, height = 8"#, "parent"),
            (r#"id = "menu", parent = "bar", width = 8, height = 8"#, "anchor_rect"),
        ] {
            let err =
                popup_spec(&props_from_src(&lua, format!(r#"return {{ kind = "popup", {source} }}"#))).unwrap_err();
            assert!(matches!(&err, LayoutError::InvalidProperty { property: got, .. } if got == property), "{err}");
        }
    }

    #[test]
    fn constraint_adjustment_reads_a_set_of_names_refuses_a_hole_and_takes_an_explicit_empty_as_none() {
        let lua = mlua::Lua::new();
        let props = popup_props(
            &lua,
            r#", constraint_adjustment = { "slide_x", "slide_y", "flip_x", "flip_y", "resize_x", "resize_y" }"#,
        );
        assert_eq!(
            popup_spec(&props).unwrap().constraint_adjustment,
            ConstraintAdjustment {
                slide_x: true,
                slide_y: true,
                flip_x: true,
                flip_y: true,
                resize_x: true,
                resize_y: true
            }
        );

        // A set, so order and repetition do not change it.
        let ordered = popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "flip_y", "slide_x" }"#)).unwrap();
        let reversed =
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "slide_x", "flip_y", "slide_x" }"#)).unwrap();
        assert_eq!(ordered.constraint_adjustment, reversed.constraint_adjustment);

        // An adjustment after a hole is refused rather than dropped.
        let err =
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = { "flip_y", nil, "slide_x" }"#)).unwrap_err();
        assert!(err.to_string().contains("constraint_adjustment"), "{err}");

        // An explicitly empty list is the protocol's own no-adjustment.
        assert_eq!(
            popup_spec(&popup_props(&lua, r#", constraint_adjustment = {}"#)).unwrap().constraint_adjustment,
            ConstraintAdjustment::NONE
        );
    }

    #[test]
    fn a_signal_in_a_popup_anchor_rect_resolves_because_the_positioner_is_rebuilt_on_every_open() {
        let lua = signal_lua();
        let resolved = resolve_declared(
            &props_from_src(
                &lua,
                r#"return { kind = "popup", id = "menu", parent = "bar", width = 200, height = 300,
                                anchor_rect = state("menu_anchor", { x = 4, y = 8, width = 16, height = 24 }) }"#,
            ),
            "popup",
            false,
            &lua,
        )
        .unwrap();
        assert_eq!(
            popup_spec(&resolved).unwrap().anchor_rect,
            LogicalRect { x: 4.0, y: 8.0, width: 16.0, height: 24.0 }
        );
    }

    /// The same fixture as [`popup_props`] but with every property under test bound to a live
    /// signal instead of a literal, and *not* run through [`resolve_declared`] -- which is
    /// exactly the map `crate::socket`'s `surface_specs` parses.
    fn unresolved_popup_props(lua: &mlua::Lua, extra: &str) -> PropMap {
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        props_from_src(
            lua,
            format!(
                r#"return {{ kind = "popup", id = "menu", parent = "bar",
                                 anchor_rect = state("a", {{ x = 4, y = 8, width = 16, height = 24 }}),
                                 width = 200, height = 300 {extra} }}"#
            ),
        )
    }

    #[test]
    fn a_signal_bound_popup_property_is_deferred_rather_than_rejected_before_it_resolves() {
        let lua = mlua::Lua::new();
        let spec = popup_spec(&unresolved_popup_props(
            &lua,
            r#", width = state("w", 200), height = state("h", 300), anchor = state("an", "top"),
                    gravity = state("g", "bottom"), constraint_adjustment = state("c", {}),
                    offset = state("o", { x = 3, y = 3 }), grab = state("gr", false)"#,
        ))
        .unwrap();
        assert_eq!(spec.anchor_rect, LogicalRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert_eq!((spec.width, spec.height), (SizeMode::Pixels(1.0), SizeMode::Pixels(1.0)));
        assert_eq!((spec.anchor, spec.gravity), (PopupAnchor::Center, PopupAnchor::Center));
        assert_eq!(spec.constraint_adjustment, ConstraintAdjustment::default());
        assert_eq!(spec.offset, PopupOffset::default());
        assert!(spec.grab, "a deferred `grab` takes its declared default, not the signal's current value");
    }

    #[test]
    fn a_literal_typo_or_a_popup_parent_signal_still_fails_on_the_evaluation_pass() {
        let lua = mlua::Lua::new();
        // A literal typo beside a deferred signal.
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", anchor = "Middle""#)).unwrap_err(),
            LayoutError::InvalidProperty { property, .. } if property == "anchor"
        ));
        // A signal in a popup parent is rejected.
        assert!(matches!(
            popup_spec(&unresolved_popup_props(&lua, r#", parent = state("p", "bar")"#)).unwrap_err(),
            LayoutError::UnsupportedSignalProperty(p) if p == "parent"
        ));
    }

    #[test]
    fn a_signal_in_a_window_title_app_id_or_size_hint_is_deferred_on_the_evaluation_pass_too() {
        let lua = signal_lua();
        let spec = window_spec(&props_from_src(
            &lua,
            r#"return { kind = "window", id = "w", title = state("t", "Now Playing"),
                                app_id = state("a", "mantle.later"),
                                min_size = state("mn", { width = 320, height = 240 }),
                                max_size = state("mx", { width = 1280, height = 800 }) }"#,
        ))
        .unwrap();
        assert_eq!(spec.title, "", "the placeholder is what a toplevel that never sends set_title has");
        assert_eq!(spec.app_id, "mantle-w", "the same default an absent `app_id` takes");
        assert_eq!((spec.min_size, spec.max_size), (None, None), "absent means the request is simply not sent");
    }
}
