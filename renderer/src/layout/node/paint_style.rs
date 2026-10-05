//! Paint properties are parsed during `Scene::apply`, not in `layout::paint`: keeping this type
//! here avoids making `scene` depend on a module that already depends on it. Display-list builds
//! run every dirty turn because list equality controls repaint (ADR-0063 decision 1), while applies
//! run at capability-push cadence (ADR-0044 decision 2). This also makes malformed values fail once
//! through `mantle.rescue` instead of painting with a default every frame. Geometry already fails
//! `apply` and reaches `mantle.rescue`; one resolved map cannot give paint a second opinion on
//! malformed values. Paint-time work remains arithmetic needing scale or focus; `icon.size` stays
//! geometry for the scene's measure callback.

use std::sync::Arc;

use crate::image::{Fit, Load};
use crate::text::shaping::{FontRun, ShapeRequest, ShapingStyle, Variations};
use crate::text::snap::LogicalRect;

use super::*;
use fields::{capture, icon, image, paint, path, shader, text, text_flow, textfield, typeface};

/// Exactly one capture source. Window IDs stay opaque outside their compositor adapter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CaptureTarget {
    Output(String),
    Window(String),
}

impl CaptureTarget {
    pub fn name(&self) -> &str {
        match self {
            Self::Output(name) | Self::Window(name) => name,
        }
    }

    /// Outputs are opaque and outlive their session; windows keep alpha and close.
    pub fn is_output(&self) -> bool {
        matches!(self, Self::Output(_))
    }
}

/// A `text`'s or `textfield`'s typography: measure, paint, caret and press hit-test all read it.
#[derive(Debug, Clone, PartialEq)]
pub struct Typeface {
    pub font_size: f32,
    /// Px, `font_size` times the `line_height` ratio.
    pub line_height: f32,
    pub letter_spacing: f32,
    pub font_weight: f32,
    pub italic: bool,
    pub variations: Variations,
    /// The family this node named, or `None` for the declared chain (ADR-0144).
    pub font: Option<Arc<str>>,
}

impl Typeface {
    fn read(properties: &PropMap) -> Result<Self, LayoutError> {
        let font_size = typeface::font_size.read(properties)?;
        Ok(Self {
            font_size,
            line_height: font_size * typeface::line_height.read(properties)?,
            letter_spacing: typeface::letter_spacing.read(properties)?,
            font_weight: typeface::font_weight.read(properties)?,
            italic: typeface::italic.read(properties)?,
            variations: typeface::font_variations.read(properties)?,
            font: typeface::font.read(properties)?,
        })
    }

    pub fn request(&self, text: String, max_width: Option<f32>, runs: Vec<FontRun>) -> ShapeRequest {
        ShapeRequest {
            text,
            font_size: self.font_size,
            line_height: self.line_height,
            letter_spacing: self.letter_spacing,
            font_weight: self.font_weight,
            italic: self.italic,
            variations: self.variations.clone(),
            max_width,
            runs,
            font: self.font.clone(),
        }
    }

    pub fn shaping_style(&self) -> ShapingStyle<'_> {
        ShapingStyle {
            font_size: self.font_size,
            line_height: self.line_height,
            letter_spacing: self.letter_spacing,
            font_weight: self.font_weight,
            italic: self.italic,
            variations: &self.variations,
        }
    }
}

/// A `textfield`'s caret bar, resolved against its font.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretStyle {
    pub color: Rgba,
    /// Logical px.
    pub width: f32,
    /// Logical px, or a fraction of the line height when `1` or less; `None` is the whole line.
    pub height: Option<f32>,
    pub radius: f32,
}

impl CaretStyle {
    /// The bar a field draws when `caret` says nothing.
    pub fn plain(font_size: f32, color: Rgba) -> Self {
        Self { color, width: crate::text::shaping::caret_thickness(font_size), height: None, radius: 0.0 }
    }

    /// The bar's height in a line `line_height` tall.
    pub fn bar_height(&self, line_height: f32) -> f32 {
        match self.height {
            None => line_height,
            Some(h) if h <= 1.0 => h * line_height,
            Some(h) => h,
        }
    }
}

/// Parsed paint properties with no `mlua::Value`. A kind admitted by
/// `layout::scene::ensure_supported_kind` but absent here draws nothing. Lua tables compare by
/// identity, so keeping one here would make a signal-resolved table repaint forever (ADR-0063).
#[derive(Debug, Clone, PartialEq)]
pub enum PaintStyle {
    Path(VectorPath),
    /// Box fill/border for containers and all four surface roles. `clip` travels with `radius`
    /// because it changes how the node's shape clips descendants.
    /// A negative `radius` is a scoop (`node::parse_radius`). `mask` covers the node's own paint
    /// and its subtree (ADR-0255).
    Box {
        background: Option<Fill>,
        radius: Radii,
        border: BorderPaint,
        widths: EdgeInsets,
        clip: ClipShape,
        mask: Option<Mask>,
    },
    /// Text before/after `layout::scene::pass::finish` rewrites it to an ellipsized prefix under `elide` or
    /// wrapped lines joined by `\n`; display-list paint may therefore receive `\n`-joined lines.
    /// `elide`, `wrap`, and `max_lines` survive for that rewrite but are dead to `layout::paint`.
    Text {
        content: Arc<str>,
        /// Styled stretches of `content`, remapped when the scene rewrites it (ADR-0104).
        runs: Vec<StyleRun>,
        face: Typeface,
        color: Rgba,
        align: TextAlign,
        elide: Elide,
        wrap: Wrap,
        max_lines: Option<usize>,
        /// Whether fitting removed source text, before any ancestor clip or paint transform.
        elided: bool,
    },
    /// Theme name; `layout::paint::execute` resolves it, keeping filesystem access out of parsing
    /// and display-list building.
    Icon {
        name: String,
        /// `foreground` for `currentColor` fills (ADR-0072); `None` preserves file colours.
        color: Option<Rgba>,
    },
    Image {
        source: String,
        fit: Fit,
        /// `async = true` (ADR-0122): decode on the pool and draw nothing until it lands.
        load: Load,
        /// `retain = true` (ADR-0180): cover that gap with the source this node last had pixels
        /// for, rather than with nothing. Inert under [`Load::Inline`], which leaves no gap.
        /// Implied by `transition`, which has nothing to cross from without it.
        retain: bool,
        /// `transition` (ADR-0181): cross from the covering source to the landed one over a
        /// duration, instead of swapping between them in one frame.
        transition: Option<TransitionSpec>,
        /// `source_blur` (ADR-0240): a static blur run once when the source's decode lands, in
        /// logical pixels. `0.0` is off. Distinct from `behind_blur` (ADR-0195), which asks the
        /// compositor to blur the desktop *behind* a box instead of blurring the node's own
        /// pixels, and which `image` does not accept.
        source_blur: f32,
        /// `radius`: rounds the visible picture, not the box.
        radius: Radii,
    },
    /// `capture` (ADR-0248): one output or window. An empty or unavailable target draws nothing.
    Capture {
        target: CaptureTarget,
        fit: Fit,
        live: Option<f32>,
        paint_cursor: bool,
        region: Option<LogicalRect>,
    },
    /// `shader` (ADR-0253): a config fragment shader with no inputs but `progress` and `params`.
    Shader {
        source: String,
        progress: f32,
        params: Vec<ShaderParam>,
    },
    /// `target` is `None` when no `secure_submit` is declared. Malformed targets fail here, not at
    /// the press path.
    TextField {
        target: Option<SecureSubmitTarget>,
        placeholder: String,
        mask: String,
        face: Typeface,
        color: Rgba,
        /// `color` unless `placeholder_color` is set.
        placeholder_color: Rgba,
        caret: CaretStyle,
        align: TextAlign,
        disabled: bool,
        /// Grapheme-cluster cap on the draft; `None` is unlimited.
        max_length: Option<usize>,
        /// What Escape does in a plain field; read here so a bad name fails the pass.
        escape: crate::wayland::Escape,
    },
}

/// Parses an already-resolved kind. `Ok(None)` means the kind draws nothing; an error fails apply.
pub fn paint_style(kind: &str, properties: &PropMap) -> Result<Option<PaintStyle>, LayoutError> {
    let style = match kind {
        // All containers and surface roles paint as a box; a `list` carries one for its `clip` (ADR-0328).
        "rect" | "row" | "column" | "list" | "panel" | "window" | "popup" | "lock" => PaintStyle::Box {
            background: paint::background.read(properties)?,
            radius: parse_radius(properties)?,
            border: paint::border_color.read(properties)?,
            widths: paint::border_width.read(properties)?,
            clip: ClipShape::of(kind, properties)?,
            mask: paint::mask.read(properties)?,
        },
        "path" => PaintStyle::Path(VectorPath {
            commands: path::commands.read(properties)?,
            fill: path::fill.read(properties)?,
            stroke: path::stroke.read(properties)?,
            stroke_width: path::stroke_width.read(properties)?,
            stroke_cap: path::stroke_cap.read(properties)?,
            stroke_join: path::stroke_join.read(properties)?,
            trim: (path::trim_start.read(properties)?, path::trim_end.read(properties)?),
            trim_axis: path::trim_axis.read(properties)?,
            shift: path::shift.read(properties)?,
        }),
        "text" => {
            let (content, runs) = text::content.read(properties)?;
            PaintStyle::Text {
                content: content.into(),
                runs,
                face: Typeface::read(properties)?,
                color: typeface::foreground.read(properties)?.expect("`foreground` has a default"),
                align: typeface::text_align.read(properties)?,
                elide: text_flow::elide.read(properties)?,
                wrap: text_flow::wrap.read(properties)?,
                max_lines: text_flow::max_lines.read(properties)?,
                elided: false,
            }
        }
        "icon" => PaintStyle::Icon { name: icon::name.read(properties)?, color: icon::foreground.read(properties)? },
        "image" => {
            let transition = image::transition.read(properties)?;
            PaintStyle::Image {
                source: image::source.read(properties)?,
                fit: image::fit.read(properties)?,
                load: if image::r#async.read(properties)? { Load::Background } else { Load::Inline },
                // A dissolve crosses *from* the picture the node is holding, so declaring one is
                // declaring retention; making a config write both would only let it write one.
                retain: image::retain.read(properties)? || transition.is_some(),
                transition,
                source_blur: image::source_blur.read(properties)?,
                radius: Radii(image::radius.read(properties)?.0, image::corner_smoothing.read(properties)?),
            }
        }
        "capture" => {
            let output = capture::output.read(properties)?;
            let window = capture::window.read(properties)?;
            let region = capture::region.read(properties)?;
            if !window.is_empty() && (!output.is_empty() || region.is_some()) {
                return Err(LayoutError::InvalidProperty {
                    property: "capture.window".into(),
                    detail: "cannot combine a window with output or region".into(),
                });
            }
            PaintStyle::Capture {
                target: if window.is_empty() { CaptureTarget::Output(output) } else { CaptureTarget::Window(window) },
                fit: capture::fit.read(properties)?,
                live: capture::live.read(properties)?,
                paint_cursor: capture::paint_cursor.read(properties)?,
                region,
            }
        }
        "shader" => PaintStyle::Shader {
            source: shader::source.read(properties)?,
            progress: shader::progress.read(properties)?,
            params: shader::params.read(properties)?,
        },
        "textfield" => {
            // Only a click reads `focus_target`; read here too so a value that is not a handle fails the pass.
            textfield::focus_target.read(properties)?;
            let color = typeface::foreground.read(properties)?.expect("`foreground` has a default");
            let face = Typeface::read(properties)?;
            let keys = textfield::caret.read(properties)?;
            let caret = CaretStyle {
                color: keys.color.unwrap_or(color),
                width: keys.width.unwrap_or(crate::text::shaping::caret_thickness(face.font_size)),
                height: keys.height,
                radius: keys.radius.unwrap_or(0.0),
            };
            let target = textfield::secure_submit.read(properties)?;
            if target.is_some() && !textfield::initial_text.read(properties)?.is_empty() {
                return Err(LayoutError::InvalidProperty {
                    property: "textfield.initial_text".into(),
                    detail: "a `secure_submit` field never holds text a config gave it".into(),
                });
            }
            PaintStyle::TextField {
                target,
                placeholder: textfield::placeholder.read(properties)?,
                // Drawn once per typed character: `""` draws nothing, a longer string its first one.
                mask: textfield::mask_character.read(properties)?.chars().next().map(String::from).unwrap_or_default(),
                face,
                color,
                placeholder_color: textfield::placeholder_color.read(properties)?.unwrap_or(color),
                caret,
                align: typeface::text_align.read(properties)?,
                disabled: textfield::disabled.read(properties)?,
                max_length: textfield::max_length.read(properties)?,
                escape: textfield::escape.read(properties)?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(style))
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    use crate::lua::nodes::deserialize_lua_table;

    fn style(lua: &Lua, lua_src: &str) -> Result<Option<PaintStyle>, LayoutError> {
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        paint_style(node.kind, &node.properties)
    }

    #[test]
    fn an_unknown_text_align_fails_the_pass_naming_the_property() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "text", content = "hi", text_align = "Middle" }"#).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "text_align"),
            "got {err:?}"
        );
        let err = style(&lua, r#"return { kind = "text", content = "hi", text_align = 1 }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "text_align"));
    }

    /// `focus_target = "search"` would otherwise leave a field no click can ever focus.
    #[test]
    fn a_textfield_focus_that_is_not_a_handle_fails_the_pass() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "textfield", focus_target = "search" }"#).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "focus_target"),
            "got {err:?}"
        );
    }

    /// A `text` that says nothing draws in the declared chain, which is most nodes.
    #[test]
    fn a_text_node_without_a_font_property_draws_in_the_declared_chain() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi" }"#).unwrap().unwrap();
        assert!(matches!(parsed, PaintStyle::Text { face: Typeface { font: None, .. }, .. }), "got {parsed:?}");
    }

    /// The family reaches paint as the config wrote it, not normalised: the painter keys its
    /// chains on the name the node asked for, so any rewriting here would miss the chain.
    #[test]
    fn a_text_node_carries_the_family_name_it_was_given() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi", font = "JetBrainsMono Nerd Font Mono" }"#)
            .unwrap()
            .unwrap();
        let PaintStyle::Text { face: Typeface { font, .. }, .. } = parsed else { panic!("expected text") };
        assert_eq!(font.as_deref(), Some("JetBrainsMono Nerd Font Mono"));
    }

    /// An empty string would otherwise read as "no family named" and silently draw in the declared
    /// chain with nothing for the reader to point at.
    #[test]
    fn an_empty_or_non_string_font_fails_the_pass_naming_the_property() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "text", content = "hi", font = "" }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "font"), "got {err:?}");
        let err = style(&lua, r#"return { kind = "text", content = "hi", font = 1 }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "font"));
    }

    #[test]
    fn a_list_has_a_box_that_draws_nothing_and_carries_its_clip() {
        let lua = Lua::new();
        let Some(PaintStyle::Box { background: None, clip: ClipShape::None, .. }) =
            style(&lua, "return { kind = 'list', direction = 'row' }").unwrap()
        else {
            panic!("a list paints an empty, unclipping box");
        };
    }

    #[test]
    fn a_malformed_background_fails_the_pass_instead_of_defaulting() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'rect', background = 5 }").is_err());
    }

    #[test]
    fn a_textfield_reads_disabled_and_max_length_and_refuses_a_negative_limit() {
        let lua = Lua::new();
        let read = |src| match style(&lua, src).unwrap() {
            Some(PaintStyle::TextField { disabled, max_length, .. }) => (disabled, max_length),
            _ => unreachable!(),
        };
        assert_eq!(read("return { kind = 'textfield' }"), (false, None));
        assert_eq!(read("return { kind = 'textfield', disabled = true, max_length = 5 }"), (true, Some(5)));
        assert_eq!(
            read("return { kind = 'textfield', max_length = 0 }"),
            (false, None),
            "0 is unlimited, as max_lines"
        );
        assert!(style(&lua, "return { kind = 'textfield', max_length = -1 }").is_err());
    }

    /// A field reads the typography rows `text` does: same keys, ranges and defaults.
    #[test]
    fn a_textfield_takes_text_typography_with_text_defaults_and_ranges() {
        let lua = Lua::new();
        let face = |src| match style(&lua, src).unwrap() {
            Some(PaintStyle::TextField { face, .. }) => face,
            _ => unreachable!(),
        };
        let given = face(
            "return { kind = 'textfield', font = 'Mono', font_size = 20, line_height = 2, letter_spacing = 3, \
             font_weight = 700, italic = true, font_variations = { wght = 650 } }",
        );
        assert_eq!((given.font_size, given.line_height, given.letter_spacing), (20.0, 40.0, 3.0));
        assert_eq!((given.font_weight, given.italic, given.font.as_deref()), (700.0, true, Some("Mono")));
        assert_eq!(&*given.variations, &[(*b"wght", 650.0_f32.to_bits())]);

        let default = face("return { kind = 'textfield' }");
        assert_eq!((default.font_size, default.line_height), (12.0, 12.0 * 1.2));
        assert_eq!((default.letter_spacing, default.font_weight, default.italic), (0.0, 400.0, false));
        assert!(default.font.is_none() && default.variations.is_empty());
        for bad in ["font_weight = 0", "letter_spacing = 101", "line_height = 0", "font = ''"] {
            assert!(style(&lua, &format!("return {{ kind = 'textfield', {bad} }}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_textfield_caret_table_reads_its_keys_and_defaults_to_the_foreground_line() {
        let lua = Lua::new();
        let caret = |src| match style(&lua, src).unwrap() {
            Some(PaintStyle::TextField { caret, .. }) => caret,
            _ => unreachable!(),
        };
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let plain = caret("return { kind = 'textfield', font_size = 32, foreground = '#FF0000' }");
        assert_eq!(plain, CaretStyle { color: red, width: 2.0, height: None, radius: 0.0 });
        let set = caret(
            "return { kind = 'textfield', foreground = '#FF0000', \
             caret = { color = '#00FF00', width = 3, height = 0.5, radius = 1.5 } }",
        );
        assert_eq!(
            set,
            CaretStyle { color: Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }, width: 3.0, height: Some(0.5), radius: 1.5 }
        );
        assert_eq!((set.bar_height(20.0), CaretStyle { height: Some(8.0), ..set }.bar_height(20.0)), (10.0, 8.0));
        assert_eq!(caret("return { kind = 'textfield', caret = { width = 4 } }").width, 4.0);
        for bad in ["{ colour = '#fff' }", "{ width = -1 }", "{ height = 8193 }", "{ radius = -0.5 }", "3"] {
            assert!(style(&lua, &format!("return {{ kind = 'textfield', caret = {bad} }}")).is_err(), "{bad}");
        }
        let table: mlua::Table = lua.load("return { kind = 'textfield', caret_color = '#fff' }").eval().unwrap();
        assert!(deserialize_lua_table(&table).is_err(), "`caret_color` is gone");
    }

    #[test]
    fn a_textfields_absent_secure_submit_is_none_not_an_error() {
        let lua = Lua::new();
        let Some(PaintStyle::TextField { target, .. }) = style(&lua, "return { kind = 'textfield' }").unwrap() else {
            panic!("a `textfield` always has a style");
        };
        assert_eq!(target, None);
    }

    #[test]
    fn a_malformed_secure_submit_names_no_capability_and_fails_the_pass() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'textfield', secure_submit = 'polkit' }").is_err());
    }

    #[test]
    fn window_capture_rejects_output_and_region_but_keeps_live_options() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "capture", window = "0xa11ce", live = 30, paint_cursor = true }"#)
            .unwrap()
            .unwrap();
        assert!(
            matches!(parsed, PaintStyle::Capture { target: CaptureTarget::Window(ref id), live: Some(30.0), paint_cursor: true, region: None, .. } if id == "0xa11ce")
        );
        for fields in [
            r#"window = "0xa11ce", output = "DP-1""#,
            r#"window = "0xa11ce", region = { x = 0, y = 0, width = 20, height = 20 }"#,
            r#"window = 42"#,
            r#"window = "0xa11ce", live = 0"#,
        ] {
            assert!(style(&lua, &format!("return {{ kind = 'capture', {fields} }}")).is_err(), "{fields}");
        }
        assert!(style(&lua, r#"return { kind = "capture", window = "", output = "DP-1", region = { x = 0, y = 0, width = 20, height = 20 } }"#).is_ok());
    }

    #[test]
    fn capture_parses_output_fit_live_and_paint_cursor() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "capture", output = "DP-1", live = true }"#).unwrap().unwrap();
        assert_eq!(
            parsed,
            PaintStyle::Capture {
                target: CaptureTarget::Output("DP-1".to_string()),
                fit: Fit::Cover,
                live: Some(f32::INFINITY),
                paint_cursor: false,
                region: None,
            }
        );
    }

    /// ADR-0253, ADR-0300. A param is a number or a flat list of up to 4096; `progress` may sit
    /// below zero so a spring can undershoot. `source` is absolute or empty: a relative path would
    /// resolve against the Renderer's working directory.
    #[test]
    fn shader_parses_progress_and_float_or_vector_params() {
        let lua = Lua::new();
        let parsed = style(
            &lua,
            r#"return { kind = "shader", source = "/s.frag", progress = -0.1, params = { a = 2, b = { 1, 2, 3 }, c = { 7 } } }"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed,
            PaintStyle::Shader {
                source: "/s.frag".to_string(),
                progress: -0.1,
                params: vec![
                    ("a".to_string(), vec![2.0]),
                    ("b".to_string(), vec![1.0, 2.0, 3.0]),
                    ("c".to_string(), vec![7.0]),
                ],
            }
        );
        let bars = format!("{{ {} }}", vec!["0.5"; 256].join(", "));
        let parsed = style(&lua, &format!(r#"return {{ kind = "shader", params = {{ bars = {bars} }} }}"#));
        assert!(matches!(parsed, Ok(Some(PaintStyle::Shader { params, .. })) if params[0].1 == vec![0.5; 256]));
        let too_long = format!("{{ {} }}", vec!["0"; 4097].join(", "));
        for bad in [too_long.as_str(), "{}", r#"{ 1, "a" }"#, "{ 1, 0/0 }"] {
            let src = format!(r#"return {{ kind = "shader", params = {{ v = {bad} }} }}"#);
            let err = style(&lua, &src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "params.v"),
                "{bad}: {err:?}"
            );
        }
        let err = style(&lua, r#"return { kind = "shader", source = "s.frag" }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "source"), "{err:?}");
        assert!(style(&lua, r#"return { kind = "shader", source = "" }"#).is_ok());
    }

    #[test]
    fn every_surface_role_parses_the_same_box_properties_a_rect_does() {
        let lua = Lua::new();
        let rect = style(&lua, "return { kind = 'rect', background = '#112233', radius = 4, clip = 'box' }").unwrap();
        for role in ["panel", "window", "popup", "lock"] {
            let src = format!("return {{ kind = '{role}', background = '#112233', radius = 4 }}");
            assert_eq!(style(&lua, &src).unwrap(), rect, "{role}");
        }
    }
}
