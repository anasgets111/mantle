//! Visual effects, shadows, and filter shaders for nodes and backdrops (ADR-0254, ADR-0256, ADR-0336).

use mlua::Value;

use super::fill::{Mask, MaskSource};
use super::range_of;
#[cfg(test)]
use crate::layout::node::animate::{Images, Params};
use crate::layout::node::animate::{ShaderImage, ShaderParam, parse_shader_images, parse_shader_params};
use crate::layout::node::prop::{keywords, within_range};
use crate::layout::node::{
    Axes, LayoutError, Prop, PropMap, Property, Rgba, fields, input, invalid, preview_for_error, preview_str,
};
use crate::lua::luacats::{lua_shape, spelled};

/// A drop shadow in logical pixels, CSS `box-shadow`'s terms: `blur` is the radius (sigma is half
/// of it), `spread` grows the shape before blurring (ADR-0254).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub color: Rgba,
    pub blur: f32,
    pub offset: (f32, f32),
    pub spread: f32,
    /// CSS `box-shadow: inset`: cast inside the padding box instead of around the box (ADR-0331).
    pub inset: bool,
    /// How the layer composites onto what is under it.
    pub blend: Blend,
}

impl Shadow {
    /// Qt's `MultiEffect` rule: a shadow shows once it has alpha and a blur, an offset or a spread.
    fn shows(&self) -> bool {
        self.color.a > 0.0 && (self.blur > 0.0 || self.spread != 0.0 || self.offset != (0.0, 0.0))
    }
}

/// What a node's own painted output is filtered by (ADR-0254). `shadows` are the layers that
/// show, first on top; `blur` is `effect.blur`, CSS `filter: blur()`'s sigma; `backdrop` is
/// `effect.backdrop.blur`, `backdrop-filter: blur()`'s (ADR-0256). `0` is off. `tone` and
/// `backdrop_tone` are the colour filters after each blur (ADR-0334). `content_shadow` is
/// `shadow_mode = "content"`: the shadows are cast by the painted subtree, not the box's shape
/// (ADR-0260).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Effect {
    pub shadows: Vec<Shadow>,
    /// The `inset` layers, first on top; they draw inside the box, so they never open a layer or grow its reach.
    pub inset: Vec<Shadow>,
    /// A program run over the painted subtree, before `blur` and `tone` (ADR-0336).
    pub shader: Option<EffectShader>,
    /// A program run over what the surface painted under the node, drawn before the node.
    pub backdrop_shader: Option<EffectShader>,
    pub blur: f32,
    pub tone: Tone,
    pub backdrop: f32,
    pub backdrop_tone: Tone,
    /// `effect.backdrop.mask`: scales the glass's coverage.
    pub backdrop_mask: Option<Mask>,
    /// `input = "desktop"`: `backdrop_shader` reads the desktop behind the surface under its input.
    pub desktop: bool,
    pub content_shadow: bool,
    /// `blend`: how the finished subtree composites onto the backdrop, after every filter.
    pub blend: Blend,
}

impl Effect {
    /// The `shadows` list as an effect keeps it: the layers that show, inset ones apart.
    pub fn set_shadows(&mut self, layers: &[Shadow]) -> Result<(), LayoutError> {
        let (inset, shadows): (Vec<_>, Vec<_>) =
            layers.iter().copied().filter(Shadow::shows).partition(|shadow| shadow.inset);
        if self.content_shadow && !inset.is_empty() {
            return Err(invalid("shadows", "an `inset` layer needs `shadow_mode = \"box\"`"));
        }
        (self.inset, self.shadows) = (inset, shadows);
        Ok(())
    }

    /// An animated `effect`'s `[blur, saturate, brightness, contrast]` for the content and the
    /// backdrop, then `shader.progress`, in place of what `parse_effect` read from the map.
    pub fn set_levels(&mut self, levels: &[f32; 9]) {
        self.blur = levels[0];
        self.tone = Tone::from([levels[1], levels[2], levels[3]]);
        self.backdrop = levels[4];
        self.backdrop_tone = Tone::from([levels[5], levels[6], levels[7]]);
        for shader in [&mut self.shader, &mut self.backdrop_shader].into_iter().flatten() {
            shader.progress = levels[8];
        }
    }

    /// Whether the node's own output needs an offscreen: a shadow, a shader, a blur, a colour filter or a blend.
    pub fn layers(&self) -> bool {
        !self.shadows.is_empty()
            || self.shader.is_some()
            || self.blur > 0.0
            || !self.tone.is_identity()
            || self.blend != Blend::Normal
    }
}

/// `effect.shader`: an absolute `.frag` path, its `params`, and `padding` logical px around the box
/// the program may read and draw (ADR-0336).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectShader {
    pub source: std::path::PathBuf,
    pub params: Vec<ShaderParam>,
    pub images: Vec<ShaderImage>,
    pub padding: f32,
    /// `u_progress`, as on a `shader` node.
    pub progress: f32,
}

keywords! {
    /// `effect.shader.input`: the pixels the program reads as `u_input`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) enum ShaderInput {
        /// The node's painted subtree.
        #[default]
        Content,
        /// What the surface painted under the node.
        Backdrop,
        /// What shows under the node: the surface's pixels over the desktop behind the surface.
        Desktop,
    }
}

keywords! {
    /// CSS `mix-blend-mode`, and Apple's `plus-lighter` and `plus-darker`, for a node, a
    /// `background` layer or a `shadows` layer. The order is the blend program's `u_mode`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Blend {
        #[default]
        Normal,
        Multiply,
        Screen,
        Overlay,
        Darken,
        Lighten,
        ColorDodge,
        ColorBurn,
        HardLight,
        SoftLight,
        Difference,
        Exclusion,
        Hue,
        Saturation,
        Color,
        Luminosity,
        PlusLighter,
        PlusDarker,
    }
}

/// The most `effect.shader.padding` grows the offscreen, in logical px.
const SHADER_PADDING: (f32, f32) = (0.0, 512.0);

/// CSS `saturate()`, `brightness()` and `contrast()` in that order, each `1` for off, applied to
/// straight sRGB as the CSS shorthands do (ADR-0334).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tone {
    pub saturate: f32,
    pub brightness: f32,
    pub contrast: f32,
}

impl From<[f32; 3]> for Tone {
    fn from([saturate, brightness, contrast]: [f32; 3]) -> Self {
        Self { saturate, brightness, contrast }
    }
}

impl Default for Tone {
    fn default() -> Self {
        Self { saturate: 1.0, brightness: 1.0, contrast: 1.0 }
    }
}

impl Tone {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// The Filter Effects `saturate()` matrix, column-major for GL: `s` on the diagonal and
    /// `1 - s` times the luma weights everywhere.
    pub fn saturate_columns(&self) -> [f32; 9] {
        const LUMA: [f32; 3] = [0.213, 0.715, 0.072];
        let s = self.saturate;
        std::array::from_fn(|i| (1.0 - s) * LUMA[i / 3] + if i % 3 == i / 3 { s } else { 0.0 })
    }
}

keywords! {
    /// `shadow_mode`: CSS `box-shadow` of the box shape, or `drop-shadow` of everything painted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ShadowMode {
        Box,
        Content,
    }
}

// ponytail: 16 layers, each a gradient quad or, in content mode, a blur pass; raise it with a measured budget.
const MAX_SHADOWS: usize = 16;
/// A layer's `blur` and each `effect` blur, and a layer's `offset` and `spread`, in px; the tween clamps into the same ranges.
pub(in crate::layout::node) const SHADOW_BLUR: (f32, f32) = (0.0, 8192.0);
/// Each `effect` colour filter's factor; past 8 `saturate` and `contrast` have clipped every channel.
pub(in crate::layout::node) const TONE: (f32, f32) = (0.0, 8.0);
pub(in crate::layout::node) const SHADOW_REACH: (f32, f32) = (-8192.0, 8192.0);

lua_shape! {
    /// One `shadows` layer.
    #[alias = "ShadowLayer"]
    pub(crate) struct ShadowLayer {
        color: Option<Rgba>,
        blur: Option<f32>,
        offset: Option<Axes>,
        spread: Option<f32>,
        inset: Option<bool>,
        blend: Option<Blend> as Option<BlendAlias>,
    }
}

/// `Blend` by its stub alias.
#[cfg(test)]
struct BlendAlias;
#[cfg(test)]
spelled!(BlendAlias => "Blend");

lua_shape! {
    /// `effect.backdrop`: CSS `backdrop-filter`, a box kind's only.
    #[alias = "BackdropEffect"]
    #[derive(Default)]
    pub(crate) struct BackdropKeys {
        pub(crate) blur: Option<f32>,
        pub(crate) saturate: Option<f32>,
        pub(crate) brightness: Option<f32>,
        pub(crate) contrast: Option<f32>,
        pub(crate) mask: Option<Mask>,
    }
}

lua_shape! {
    /// `effect.shader`: a config fragment shader over the node's painted subtree or its backdrop.
    #[alias = "EffectShader"]
    pub(crate) struct ShaderKeys {
        pub(crate) source: std::path::PathBuf,
        pub(crate) input: Option<ShaderInput>,
        pub(crate) params: Value as Option<Params>,
        pub(crate) images: Value as Option<Images>,
        pub(crate) padding: Option<f32>,
        pub(crate) progress: Option<f32>,
    }
}

lua_shape! {
    /// `effect`: CSS `filter` for a node.
    #[alias = "Effect"]
    #[derive(Default)]
    pub(crate) struct EffectKeys {
        pub(crate) blur: Option<f32>,
        pub(crate) saturate: Option<f32>,
        pub(crate) brightness: Option<f32>,
        pub(crate) contrast: Option<f32>,
        pub(crate) shader: Option<ShaderKeys>,
        pub(crate) backdrop: Option<BackdropKeys>,
    }
}

/// `effect`: the node's pixel filters, each blur in `[0, 8192]` and each colour filter in `[0, 8]`.
/// Absent keys are off.
pub(crate) struct Effects;

spelled!(Effects => EffectKeys::lua());

impl Prop for Effects {
    type Out = EffectKeys;
    fn read(row: &Property, value: Option<&Value>) -> Result<EffectKeys, LayoutError> {
        let table = match value {
            None => return Ok(EffectKeys::default()),
            Some(Value::Table(table)) => table,
            Some(value) => {
                return Err(invalid(row.name, format!("expected a table, got {}", preview_for_error(value))));
            }
        };
        let keys = EffectKeys::read(row.name, table)?;
        let within = |key: &str, range, n: Option<f32>| {
            n.map(|n| within_range(&format!("{}.{key}", row.name), range, n)).transpose()
        };
        let backdrop = match keys.backdrop {
            Some(b) => Some(BackdropKeys {
                blur: within("backdrop.blur", SHADOW_BLUR, b.blur)?,
                saturate: within("backdrop.saturate", TONE, b.saturate)?,
                brightness: within("backdrop.brightness", TONE, b.brightness)?,
                contrast: within("backdrop.contrast", TONE, b.contrast)?,
                mask: match b.mask {
                    Some(Mask { source: MaskSource::Node(_), .. }) => {
                        return Err(invalid("effect.backdrop.mask", "takes a `gradient` or a `source`, not a `node`"));
                    }
                    mask => mask,
                },
            }),
            None => None,
        };
        let shader = match keys.shader {
            Some(shader) => {
                if !shader.source.is_absolute() {
                    return Err(invalid(
                        "effect.shader.source",
                        format!("expected an absolute path, got {}", preview_str(&shader.source.to_string_lossy())),
                    ));
                }
                Some(ShaderKeys {
                    padding: within("shader.padding", SHADER_PADDING, shader.padding)?,
                    progress: within("shader.progress", range_of("progress"), shader.progress)?,
                    ..shader
                })
            }
            None => None,
        };
        Ok(EffectKeys {
            blur: within("blur", SHADOW_BLUR, keys.blur)?,
            shader,
            saturate: within("saturate", TONE, keys.saturate)?,
            brightness: within("brightness", TONE, keys.brightness)?,
            contrast: within("contrast", TONE, keys.contrast)?,
            backdrop,
        })
    }
}

/// `shadows`: CSS `box-shadow`'s list, first on top. `None` when absent.
pub(crate) struct Shadows;

spelled!(Shadows => Vec::<ShadowLayer>::lua());

impl Prop for Shadows {
    type Out = Option<Vec<Shadow>>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let table = match value {
            None => return Ok(None),
            Some(Value::Table(table)) => table,
            Some(value) => {
                return Err(invalid(row.name, format!("expected a layer array, got {}", preview_for_error(value))));
            }
        };
        let within = |n: f32, range| within_range(row.name, range, n);
        let len = input::array_len(row.name, table, MAX_SHADOWS)?;
        let mut shadows = Vec::with_capacity(len);
        for i in 1..=len {
            let name = format!("{}[{i}]", row.name);
            let layer: mlua::Table = table.raw_get(i).map_err(|e| invalid(&name, e.to_string()))?;
            let ShadowLayer { color, blur, offset, spread, inset, blend } = ShadowLayer::read(&name, &layer)?;
            let Axes { x, y } = offset.unwrap_or(Axes { x: None, y: None });
            shadows.push(Shadow {
                color: color.unwrap_or(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                blur: within(blur.unwrap_or(0.0), SHADOW_BLUR)?,
                offset: (within(x.unwrap_or(0.0), SHADOW_REACH)?, within(y.unwrap_or(0.0), SHADOW_REACH)?),
                spread: within(spread.unwrap_or(0.0), SHADOW_REACH)?,
                inset: inset.unwrap_or(false),
                blend: blend.unwrap_or_default(),
            });
        }
        Ok(Some(shadows))
    }
}

/// `shadows` and `effect`, every kind, and a box's `shadow_mode`. Only layers that would draw are
/// kept, so paint never opens an offscreen for one.
pub fn parse_effect(properties: &PropMap) -> Result<Effect, LayoutError> {
    use fields::{common, paint};
    let layers = common::shadows.read(properties)?.unwrap_or_default();
    let content_shadow = paint::shadow_mode.read(properties)? == ShadowMode::Content;
    let filters = common::effect.read(properties)?;
    let tone = |saturate: Option<f32>, brightness: Option<f32>, contrast: Option<f32>| {
        Tone::from([saturate.unwrap_or(1.0), brightness.unwrap_or(1.0), contrast.unwrap_or(1.0)])
    };
    let backdrop = filters.backdrop.unwrap_or_default();
    let (mut shader, mut backdrop_shader, mut desktop) = (None, None, false);
    if let Some(keys) = filters.shader {
        let program = EffectShader {
            source: keys.source,
            params: parse_shader_params("effect.shader.params", &keys.params)?,
            images: parse_shader_images("effect.shader.images", &keys.images)?,
            padding: keys.padding.unwrap_or(0.0),
            progress: keys.progress.unwrap_or(0.0),
        };
        match keys.input.unwrap_or_default() {
            ShaderInput::Content => shader = Some(program),
            ShaderInput::Backdrop => backdrop_shader = Some(program),
            ShaderInput::Desktop => (backdrop_shader, desktop) = (Some(program), true),
        }
    }
    let mut effect = Effect {
        shadows: Vec::new(),
        inset: Vec::new(),
        shader,
        backdrop_shader,
        blur: filters.blur.unwrap_or(0.0),
        tone: tone(filters.saturate, filters.brightness, filters.contrast),
        backdrop: backdrop.blur.unwrap_or(0.0),
        backdrop_tone: tone(backdrop.saturate, backdrop.brightness, backdrop.contrast),
        backdrop_mask: backdrop.mask,
        desktop,
        content_shadow,
        blend: common::blend.read(properties)?,
    };
    effect.set_shadows(&layers)?;
    Ok(effect)
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::*;
    use crate::layout::node::{LayoutError, Rgba, rect_props};

    /// ADR-0336. `effect.shader` needs an absolute `source`, defaults `input` and `padding`, reads
    /// `params` as a `shader` node does, and refuses what it does not know.
    #[test]
    fn effect_shader_parses_its_keys_and_refuses_the_rest() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        let shader = |padding| EffectShader {
            source: "/s.frag".into(),
            params: vec![("a".into(), vec![2.0]), ("b".into(), vec![1.0, 2.0])],
            images: vec![("map".into(), "/m.png".into())],
            padding,
            progress: -0.5,
        };
        let full = r#"return { effect = { shader = { source = "/s.frag", input = "content", padding = 12, progress = -0.5,
            params = { a = 2, b = { 1, 2 } }, images = { map = "/m.png" } } } }"#;
        assert_eq!(parse(full).unwrap(), Effect { shader: Some(shader(12.0)), ..Effect::default() });
        let bare = parse(r#"return { effect = { shader = { source = "/s.frag" } } }"#).unwrap();
        let shader = bare.shader.as_ref().unwrap();
        assert_eq!((shader.padding, shader.params.len(), shader.progress), (0.0, 0, 0.0));
        assert!(bare.layers(), "a shader alone needs the offscreen");
        // A backdrop shader draws before the node, from a copy, and needs no offscreen of its own.
        let under =
            parse(r#"return { effect = { shader = { source = "/s.frag", input = "backdrop", padding = 4 } } }"#);
        let under = under.unwrap();
        assert_eq!((under.shader.is_none(), under.backdrop_shader.as_ref().map(|s| s.padding)), (true, Some(4.0)));
        assert!(!under.layers() && !under.desktop);
        // The desktop is a backdrop shader's input with the capture behind the surface under it.
        let desktop = parse(r#"return { effect = { shader = { source = "/s.frag", input = "desktop" } } }"#).unwrap();
        assert!(desktop.desktop && desktop.backdrop_shader.is_some() && desktop.shader.is_none());
        let tilde = parse(r#"return { effect = { shader = { source = "~/s.frag" } } }"#);
        match shared::expand_tilde("~/s.frag") {
            Ok(expanded) => {
                assert_eq!(tilde.unwrap().shader.unwrap().source, std::path::PathBuf::from(expanded.into_owned()))
            }
            Err(_) => assert!(tilde.is_err(), "no $HOME, no expansion"),
        }
        for (src, property) in [
            (r#"return { effect = { shader = { source = "/s.frag", input = "behind" } } }"#, "effect.shader"),
            (r#"return { blend = "plus-lighter" }"#, "blend"),
            (r#"return { blend = 1 }"#, "blend"),
            (r#"return { shadows = { { blur = 1, blend = "x" } } }"#, "shadows[1]"),
            (r#"return { effect = { shader = { source = "s.frag" } } }"#, "effect.shader.source"),
            (r#"return { effect = { shader = { source = "" } } }"#, "effect.shader.source"),
            (r#"return { effect = { shader = {} } }"#, "effect.shader"),
            (r#"return { effect = { shader = "/s.frag" } }"#, "effect"),
            (r#"return { effect = { shader = { source = "/s.frag", glow = 1 } } }"#, "effect.shader"),
            (r#"return { effect = { shader = { source = "/s.frag", padding = -1 } } }"#, "effect.shader.padding"),
            (r#"return { effect = { shader = { source = "/s.frag", padding = 513 } } }"#, "effect.shader.padding"),
            (r#"return { effect = { shader = { source = "/s.frag", progress = 8193 } } }"#, "effect.shader.progress"),
            (r#"return { effect = { shader = { source = "/s.frag", progress = -8193 } } }"#, "effect.shader.progress"),
            (
                r#"return { effect = { shader = { source = "/s.frag", params = { a = "x" } } } }"#,
                "effect.shader.params.a",
            ),
            (
                r#"return { effect = { shader = { source = "/s.frag", images = { u_map = "/m.png" } } } }"#,
                "effect.shader.images.u_map",
            ),
            (
                r#"return { effect = { shader = { source = "/s.frag", images = { map = "m.png" } } } }"#,
                "effect.shader.images.map",
            ),
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: got, .. } if got == property),
                "{src}: {err:?}"
            );
        }
    }

    /// Qt's `MultiEffect` defaults: a shadow is opaque black until coloured, and is absent until
    /// a blur, an offset or a spread would show it.
    #[test]
    fn a_shadow_is_black_until_coloured_and_absent_until_it_would_show() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        assert_eq!(parse("return {}").unwrap(), Effect::default());
        assert!(parse(r##"return { shadows = { { color = "#ff000080" } } }"##).unwrap().shadows.is_empty());
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let shadow =
            Shadow { color: black, blur: 8.0, offset: (0.0, -2.0), spread: -1.0, inset: false, blend: Blend::Normal };
        assert_eq!(
            parse("return { shadows = { { blur = 8, offset = { y = -2 }, spread = -1 } } }").unwrap().shadows,
            [shadow]
        );
        let screen = parse(r#"return { shadows = { { blur = 8, blend = "screen" } } }"#).unwrap().shadows;
        assert_eq!(screen[0].blend, Blend::Screen);
        let node = parse(r#"return { blend = "plus_darker" }"#).unwrap();
        assert_eq!(node, Effect { blend: Blend::PlusDarker, ..Effect::default() });
        assert!(node.layers(), "a blended node draws offscreen");
        // Each layer takes the defaults, keeps its order, and drops when it would not show.
        let layers = r##"return { shadows = { { blur = 8, offset = { y = -2 }, spread = -1 }, { color = "#ff0000" },
            { color = "#ff000000", blur = 4 }, { offset = { x = 3 } } } }"##;
        let below = Shadow { blur: 0.0, offset: (3.0, 0.0), spread: 0.0, ..shadow };
        assert_eq!(parse(layers).unwrap().shadows, [shadow, below]);
        assert_eq!(parse("return { effect = { blur = 3 } }").unwrap(), Effect { blur: 3.0, ..Effect::default() });
        assert_eq!(
            parse("return { effect = { backdrop = { blur = 8 } } }").unwrap(),
            Effect { backdrop: 8.0, ..Effect::default() }
        );
        let tone = |saturate, brightness, contrast| Tone { saturate, brightness, contrast };
        assert_eq!(
            parse("return { effect = { saturate = 2, contrast = 0 } }").unwrap(),
            Effect { tone: tone(2.0, 1.0, 0.0), ..Effect::default() }
        );
        assert_eq!(
            parse("return { effect = { backdrop = { saturate = 8, brightness = 0.5 } } }").unwrap(),
            Effect { backdrop_tone: tone(8.0, 0.5, 1.0), ..Effect::default() }
        );
        assert!(parse("return { effect = { saturate = 1, backdrop = { contrast = 1 } } }").unwrap().tone.is_identity());
        let content = Effect { content_shadow: true, ..Effect::default() };
        assert_eq!(parse(r#"return { shadow_mode = "content" }"#).unwrap(), content);
        assert_eq!(parse(r#"return { shadow_mode = "box" }"#).unwrap(), Effect::default());
        for (src, property) in [
            ("return { effect = { blur = -1 } }", "effect.blur"),
            ("return { effect = { backdrop = { blur = 8193 } } }", "effect.backdrop.blur"),
            ("return { effect = { saturate = -0.1 } }", "effect.saturate"),
            ("return { effect = { brightness = 8.5 } }", "effect.brightness"),
            ("return { effect = { backdrop = { contrast = 9 } } }", "effect.backdrop.contrast"),
            ("return { effect = { backdrop = { saturate = -1 } } }", "effect.backdrop.saturate"),
            ("return { effect = { backdrop = { brightness = 8.5 } } }", "effect.backdrop.brightness"),
            ("return { effect = { contrast = 0.5, hue = 2 } }", "effect"),
            ("return { effect = { blur = 1, glow = 2 } }", "effect"),
            ("return { effect = { backdrop = { blur = 1, glow = 2 } } }", "effect.backdrop"),
            ("return { effect = { backdrop_blur = 2 } }", "effect"),
            (r#"return { effect = { backdrop = { mask = { node = "m" } } } }"#, "effect.backdrop.mask"),
            ("return { effect = 3 }", "effect"),
            ("return { shadows = { { color = 3, blur = 1 } } }", "shadows[1]"),
            (r#"return { shadow_mode = "Drop" }"#, "shadow_mode"),
            ("return { shadows = { { blur = -1 } } }", "shadows"),
            ("return { shadows = { { blur = 1, glow = 2 } } }", "shadows[1]"),
            ("return { shadows = { {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} } }", "shadows"),
            (r#"return { shadow_mode = "content", shadows = { { blur = 4, inset = true } } }"#, "shadows"),
            ("return { shadows = { { blur = 4, inset = 1 } } }", "shadows[1]"),
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property),
                "{src}: {err:?}"
            );
        }
    }

    /// ADR-0331: `inset` splits a layer off `shadows`, keeping its order and the same defaults, and a
    /// node takes 16 layers, inset and outer together.
    #[test]
    fn an_inset_layer_is_split_from_the_outer_ones_and_the_cap_is_16() {
        let lua = Lua::new();
        let parse = |src: &str| parse_effect(&rect_props(&lua, src));
        let effect =
            parse("return { shadows = { { blur = 2 }, { blur = 4, inset = true }, { spread = 1, inset = true } } }")
                .unwrap();
        assert_eq!(effect.shadows.iter().map(|s| (s.blur, s.inset)).collect::<Vec<_>>(), [(2.0, false)]);
        assert_eq!(
            effect.inset.iter().map(|s| (s.blur, s.spread, s.inset)).collect::<Vec<_>>(),
            [(4.0, 0.0, true), (0.0, 1.0, true)]
        );
        let only = parse("return { shadows = { { blur = 4, inset = true } } }").unwrap();
        assert!(!only.layers(), "an inset layer opens no offscreen");
        let layers = |n: usize| format!("return {{ shadows = {{ {} }} }}", "{ blur = 1, inset = true },".repeat(n));
        assert_eq!(parse(&layers(16)).unwrap().inset.len(), 16);
        let err = parse(&layers(17)).unwrap_err();
        assert!(err.to_string().contains("shadows"), "{err}");
    }

    /// Filter Effects `saturate()`: the matrix on `(192, 96, 64)` gives the spec's own numbers, `0`
    /// is the luma grey and `1` leaves the colour alone.
    #[test]
    fn the_saturate_matrix_is_the_filter_effects_one() {
        let apply = |s: f32, rgb: [f32; 3]| {
            let m = Tone { saturate: s, ..Tone::default() }.saturate_columns();
            std::array::from_fn::<f32, 3, _>(|row| (0..3).map(|col| m[col * 3 + row] * rgb[col]).sum())
        };
        let close = |got: [f32; 3], want: [f32; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3);
        let colour = [192.0, 96.0, 64.0];
        assert!(close(apply(2.0, colour), [269.856, 77.856, 13.856]), "{:?}", apply(2.0, colour));
        assert!(close(apply(0.0, colour), [114.144; 3]), "{:?}", apply(0.0, colour));
        assert!(close(apply(1.0, colour), colour));
        assert!(Tone::default().is_identity() && !Tone { contrast: 0.5, ..Tone::default() }.is_identity());
    }
}
