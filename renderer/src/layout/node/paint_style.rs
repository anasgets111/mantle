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
use crate::text::snap::LogicalRect;

use super::*;
use fields::{capture, icon, image, paint, path, shader, text, textfield};

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
        radius: f32,
        colors: BorderColor,
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
        font_size: f32,
        line_height: f32,
        letter_spacing: f32,
        font_weight: f32,
        italic: bool,
        /// The family this node named, or `None` for the declared chain (ADR-0144).
        font: Option<Arc<str>>,
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
        /// logical pixels. `0.0` is off. Distinct from `blur` (ADR-0195), which asks the
        /// compositor to blur the desktop *behind* a box instead of blurring the node's own
        /// pixels, and which `image` does not accept.
        source_blur: f32,
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
        font_size: f32,
        color: Rgba,
        align: TextAlign,
    },
}

/// Parses an already-resolved kind. `Ok(None)` means the kind draws nothing; an error fails apply.
pub fn paint_style(kind: &str, properties: &PropMap) -> Result<Option<PaintStyle>, LayoutError> {
    let style = match kind {
        // All containers and surface roles paint as a box.
        "rect" | "row" | "column" | "panel" | "window" | "popup" | "lock" => PaintStyle::Box {
            background: paint::background.read(properties)?,
            radius: parse_radius(properties)?,
            colors: paint::border_color.read(properties)?,
            widths: paint::border_width.read(properties)?,
            clip: paint::clip.read(properties)?,
            mask: paint::mask.read(properties)?,
        },
        "path" => PaintStyle::Path(VectorPath {
            commands: path::commands.read(properties)?,
            fill: path::fill.read(properties)?,
            stroke: path::stroke.read(properties)?,
            stroke_width: path::stroke_width.read(properties)?,
        }),
        "text" => {
            let (content, runs) = text::content.read(properties)?;
            let font_size = text::font_size.read(properties)?;
            PaintStyle::Text {
                content: content.into(),
                runs,
                font_size,
                line_height: font_size * text::line_height.read(properties)?,
                letter_spacing: text::letter_spacing.read(properties)?,
                font_weight: text::font_weight.read(properties)?,
                italic: text::italic.read(properties)?,
                font: text::font.read(properties)?,
                color: text::foreground.read(properties)?.expect("`foreground` has a default"),
                align: text::text_align.read(properties)?,
                elide: text::elide.read(properties)?,
                wrap: text::wrap.read(properties)?,
                max_lines: text::max_lines.read(properties)?,
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
            // Only a click reads `focus`; read here too so a value that is not a handle fails the pass.
            textfield::focus.read(properties)?;
            PaintStyle::TextField {
                target: textfield::secure_submit.read(properties)?,
                placeholder: textfield::placeholder.read(properties)?,
                // Drawn once per typed character: `""` draws nothing, a longer string its first one.
                mask: textfield::mask_character.read(properties)?.chars().next().map(String::from).unwrap_or_default(),
                font_size: textfield::font_size.read(properties)?,
                color: textfield::foreground.read(properties)?.expect("`foreground` has a default"),
                align: textfield::text_align.read(properties)?,
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

    /// `focus = "search"` would otherwise leave a field no click can ever focus.
    #[test]
    fn a_textfield_focus_that_is_not_a_handle_fails_the_pass() {
        let lua = Lua::new();
        let err = style(&lua, r#"return { kind = "textfield", focus = "search" }"#).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "focus"), "got {err:?}");
    }

    /// A `text` that says nothing draws in the declared chain, which is most nodes.
    #[test]
    fn a_text_node_without_a_font_property_draws_in_the_declared_chain() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi" }"#).unwrap().unwrap();
        assert!(matches!(parsed, PaintStyle::Text { font: None, .. }), "got {parsed:?}");
    }

    /// The family reaches paint as the config wrote it, not normalised: the painter keys its
    /// chains on the name the node asked for, so any rewriting here would miss the chain.
    #[test]
    fn a_text_node_carries_the_family_name_it_was_given() {
        let lua = Lua::new();
        let parsed = style(&lua, r#"return { kind = "text", content = "hi", font = "JetBrainsMono Nerd Font Mono" }"#)
            .unwrap()
            .unwrap();
        let PaintStyle::Text { font, .. } = parsed else { panic!("expected text") };
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
    fn a_kind_that_draws_nothing_has_no_style() {
        let lua = Lua::new();
        assert_eq!(style(&lua, "return { kind = 'list', direction = 'row' }").unwrap(), None);
    }

    #[test]
    fn a_malformed_background_fails_the_pass_instead_of_defaulting() {
        let lua = Lua::new();
        assert!(style(&lua, "return { kind = 'rect', background = 5 }").is_err());
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
        let rect = style(&lua, "return { kind = 'rect', background = '#112233', radius = 4 }").unwrap();
        for role in ["panel", "window", "popup", "lock"] {
            let src = format!("return {{ kind = '{role}', background = '#112233', radius = 4 }}");
            assert_eq!(style(&lua, &src).unwrap(), rect, "{role}");
        }
    }
}
