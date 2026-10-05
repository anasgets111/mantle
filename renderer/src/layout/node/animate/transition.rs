use std::path::PathBuf;
use std::time::{Duration, Instant};

use mlua::Value;

use super::easing::Easing;
use crate::layout::node::input::required_duration;
use crate::layout::node::prop::Prop;
use crate::layout::node::{LayoutError, invalid, preview_for_error, value_as_f32};
use crate::lua::luacats::{lua_shape, spelled};
use crate::lua::nodes::properties::Property;

// ADR-0181: how a `retain`ing image crosses from the picture it is holding to the one that has
// just landed.
lua_shape! {
    /// `image.transition`. Unknown keys are refused.
    #[class = "Transition"]
    #[derive(Debug, Clone, PartialEq)]
    pub struct TransitionInput {
        /// Required, ms `[1, 60000]`.
        pub duration: Duration,
        /// Default `"in_out_quad"`; drives `u_progress`.
        pub easing: Option<Easing>,
        // The config's own file: `layout::image_shader` compiles it and owns nothing about what it
        // draws.
        /// Absolute `.frag` path replacing the built-in dissolve, e.g. `mantle.config_dir .. "/shaders/wipe.frag"` (ADR-0184). Recompiled when the file changes.
        ///
        /// Shader contract. The engine prepends `#version 300 es`, `highp` precision, its declarations and `#line 1`; write `void main()`:
        /// - `v_uv`: box coordinates `0..1`, top-left origin, y down.
        /// - `u_progress`: eased progress, clamped to `0..1`. `u_size`: node size in logical px.
        /// - `mantle_from(uv)`, `mantle_to(uv)`: outgoing and incoming pictures, premultiplied and already placed by `fit`; transparent outside the picture.
        /// - `u_from_rect`, `u_to_rect`: each picture's `(x, y, w, h)` in box fractions (may exceed `0..1` under `"cover"`).
        /// - Output: premultiplied RGBA in `fragColor`, same colour space as the inputs. The engine applies `opacity` after.
        /// - Names starting `u_` or `mantle_` are reserved. A shader that fails to compile or link, or declares a uniform other than `float`/`vec2`-`vec4` or an array of one, logs once and falls back to the dissolve. A shader that hangs the GPU hangs the session.
        pub shader: Option<PathBuf>,
        /// Uniform values by name: a finite number for `float`, a list of up to 4096 for `vec2`-`vec4` or an array of either, flattened. Missing uniforms are `0`; unknown names are ignored. Refused without `shader`.
        pub params: Value as Option<Params>,
        /// Sampler names to absolute `.png`, `.jpg`, `.jpeg` or `.webp` paths, as `shader.images`; each declares `uniform sampler2D name` and `uniform vec2 name_size`. Refused without `shader`.
        pub images: Value as Option<Images>,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub duration: Duration,
    pub easing: Easing,
    pub shader: Option<PathBuf>,
    pub params: Vec<ShaderParam>,
    pub images: Vec<ShaderImage>,
}

spelled!(TransitionSpec => TransitionInput::lua());

/// `transition = { duration = 700, easing = "in_out_cubic" }` on an `image`. The `duration` is
/// required: a dissolve with no length is a snap, and `retain` on its own is already that.
impl Prop for TransitionSpec {
    type Out = Option<TransitionSpec>;
    fn read(_: &Property, value: Option<&Value>) -> Result<Option<TransitionSpec>, LayoutError> {
        parse_transition(value)
    }
}

fn parse_transition(value: Option<&Value>) -> Result<Option<TransitionSpec>, LayoutError> {
    let Some(value) = value else { return Ok(None) };
    let Value::Table(table) = value else {
        return Err(invalid(
            "transition",
            format!("expected a table of transition fields, got {}", preview_for_error(value)),
        ));
    };
    TransitionInput::read("transition", table)?.into_transition().map(Some)
}

impl TransitionInput {
    fn into_transition(self) -> Result<TransitionSpec, LayoutError> {
        let Self { duration, easing, shader, params, images } = self;
        let duration = required_duration("transition", Some(duration))?;
        if let Some(path) = &shader
            && !path.is_absolute()
        {
            return Err(invalid("transition.shader", format!("expected an absolute path, got `{}`", path.display())));
        }
        let params = parse_shader_params("transition.params", &params)?;
        if shader.is_none() && !params.is_empty() {
            return Err(invalid("transition.params", "there is no `shader` for these to reach"));
        }
        let images = parse_shader_images("transition.images", &images)?;
        if shader.is_none() && !images.is_empty() {
            return Err(invalid("transition.images", "there is no `shader` for these to reach"));
        }
        let easing = easing.unwrap_or_default();
        Ok(TransitionSpec { duration, easing, shader, params, images })
    }
}

/// A uniform name and the numbers the config wrote; the compiled uniform's type and array length
/// decide how many reach the shader, and a different count is logged.
pub type ShaderParam = (String, Vec<f32>);

/// The most numbers one `params` entry takes: `vec4[1024]`, past any driver's uniform budget.
const MAX_PARAM_NUMBERS: usize = 4096;

/// A `shader`'s `params`, and a `transition`'s: [`parse_shader_params`].
pub(crate) struct Params;

spelled!(Params => format!("table<{}, {}|{}>", String::lua(), f32::lua(), Vec::<f32>::lua()));

impl Prop for Params {
    type Out = Vec<ShaderParam>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Vec<ShaderParam>, LayoutError> {
        parse_shader_params(row.name, value.unwrap_or(&Value::Nil))
    }
}

/// Reads a table of names to values, sorted by name so the list is a value two runs can compare.
fn parse_named(
    what: &str,
    value: &Value,
    expected: &str,
    mut read: impl FnMut(&str, &str, &Value) -> Result<(), LayoutError>,
) -> Result<(), LayoutError> {
    let table = match value {
        Value::Nil => return Ok(()),
        Value::Table(table) => table,
        other => {
            return Err(invalid(what, format!("expected a table of {expected}, got {}", preview_for_error(other))));
        }
    };
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| invalid(what, e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid(what, format!("keys are names, got {}", preview_for_error(&key))));
        };
        let name = key.to_str().map_err(|e| invalid(what, e.to_string()))?.to_string();
        read(&name, &format!("{what}.{name}"), &value)?;
    }
    Ok(())
}

/// `params = { softness = 0.1, tint = { 1, 0.5, 0, 1 } }`: uniform names to a number or a list of
/// up to [`MAX_PARAM_NUMBERS`] (ADR-0184, vectors ADR-0253, arrays ADR-0300).
pub(in crate::layout::node) fn parse_shader_params(what: &str, value: &Value) -> Result<Vec<ShaderParam>, LayoutError> {
    let mut out = Vec::new();
    parse_named(what, value, "uniform names to a number or a list of numbers", |name, field, value| {
        let finite = |value: &Value| {
            value_as_f32(field, value)?
                .filter(|number| number.is_finite())
                .ok_or_else(|| invalid(field, format!("expected a finite number, got {}", preview_for_error(value))))
        };
        let numbers = match value {
            Value::Table(list) => {
                let len = list.raw_len();
                if !(1..=MAX_PARAM_NUMBERS).contains(&len) {
                    return Err(invalid(field, format!("expected 1 to {MAX_PARAM_NUMBERS} numbers, got {len}")));
                }
                (1..=len)
                    .map(|index| finite(&list.raw_get(index).map_err(|e| invalid(field, e.to_string()))?))
                    .collect::<Result<_, _>>()?
            }
            scalar => vec![finite(scalar)?],
        };
        out.push((name.to_string(), numbers));
        Ok(())
    })?;
    // The shader ignores names it lacks.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// A sampler name and the absolute path of the image behind it.
pub type ShaderImage = (String, String);

/// The most `images` one `shader` takes: GL ES guarantees 16 fragment texture units.
pub const MAX_SHADER_IMAGES: usize = 8;

/// A `shader`'s `images`: [`parse_shader_images`].
pub(crate) struct Images;

spelled!(Images => format!("table<{}, {}>", String::lua(), String::lua()));

impl Prop for Images {
    type Out = Vec<ShaderImage>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Vec<ShaderImage>, LayoutError> {
        parse_shader_images(row.name, value.unwrap_or(&Value::Nil))
    }
}

/// `images = { normal = "/abs/normal.png" }`: names become GLSL identifiers, so they are checked here.
pub(in crate::layout::node) fn parse_shader_images(what: &str, value: &Value) -> Result<Vec<ShaderImage>, LayoutError> {
    let mut out = Vec::new();
    parse_named(what, value, "sampler names to absolute paths", |name, field, value| {
        let Value::String(path) = value else {
            return Err(invalid(field, format!("expected an absolute path, got {}", preview_for_error(value))));
        };
        if let Some(why) = sampler_name_error(name) {
            return Err(invalid(field, why));
        }
        let path = path.to_str().map_err(|e| invalid(field, e.to_string()))?.to_string();
        if !path.starts_with('/') {
            return Err(invalid(field, format!("expected an absolute path, got `{path}`")));
        }
        // No GIF: an animated sampler has no frame tick.
        let raster = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["png", "jpg", "jpeg", "webp"].iter().any(|allowed| e.eq_ignore_ascii_case(allowed)));
        if !raster {
            return Err(invalid(field, "expected a `.png`, `.jpg`, `.jpeg` or `.webp` file"));
        }
        out.push((name.to_string(), path));
        Ok(())
    })?;
    if out.len() > MAX_SHADER_IMAGES {
        return Err(invalid(what, format!("expected at most {MAX_SHADER_IMAGES} images")));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// GLSL ES 3.0 keywords and the builtins a sampler name would redefine.
const GLSL_RESERVED: &[&str] = &[
    "attribute",
    "bool",
    "break",
    "bvec2",
    "bvec3",
    "bvec4",
    "case",
    "centroid",
    "const",
    "continue",
    "default",
    "discard",
    "do",
    "else",
    "false",
    "float",
    "flat",
    "for",
    "highp",
    "if",
    "in",
    "inout",
    "int",
    "invariant",
    "isampler2D",
    "ivec2",
    "ivec3",
    "ivec4",
    "layout",
    "lowp",
    "mat2",
    "mat3",
    "mat4",
    "mediump",
    "out",
    "precision",
    "return",
    "sampler2D",
    "sampler3D",
    "samplerCube",
    "smooth",
    "struct",
    "switch",
    "true",
    "uint",
    "uniform",
    "uvec2",
    "uvec3",
    "uvec4",
    "varying",
    "vec2",
    "vec3",
    "vec4",
    "void",
    "while",
    "texture",
    "texelFetch",
    "mix",
    "min",
    "max",
    "clamp",
    "step",
    "smoothstep",
    "fract",
    "floor",
    "ceil",
    "mod",
    "abs",
    "sign",
    "pow",
    "exp",
    "log",
    "sqrt",
    "sin",
    "cos",
    "length",
    "dot",
    "cross",
    "normalize",
    "reflect",
];

/// Why `name` cannot be declared as `uniform sampler2D name` and `name_size` beside the prelude's
/// `u_*`, `mantle_*`, `v_uv` and `fragColor`, or `None`.
fn sampler_name_error(name: &str) -> Option<&'static str> {
    let mut chars = name.chars();
    let identifier = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !identifier || name.len() > 64 {
        return Some("expected a GLSL identifier of up to 64 letters, digits and `_`, not starting with a digit");
    }
    if name.starts_with("u_") || name.starts_with("mantle_") || name.starts_with("gl_") || name.contains("__") {
        return Some("`u_*`, `mantle_*`, `gl_*` and names with `__` are reserved");
    }
    if name.ends_with("_size") || matches!(name, "v_uv" | "fragColor" | "main") {
        return Some("`*_size` is another image's pixel size, and `v_uv`, `fragColor` and `main` are the prelude's");
    }
    if GLSL_RESERVED.contains(&name) {
        return Some("a GLSL keyword or builtin function cannot name a sampler");
    }
    None
}

/// One cross-dissolve in flight on an `image` (ADR-0181), started by the frame the incoming texture
/// landed on and dropped the moment its duration is up.
#[derive(Debug, Clone, PartialEq)]
pub struct Dissolve {
    /// The source being crossed away from: what the node displayed when the incoming was first
    /// drawn. `ResolvedNode::displayed_source` has already moved on to the incoming by then, so
    /// the outgoing has nowhere else to live.
    pub from: String,
    /// The source being crossed to. Held rather than read off the node, because a pass may resolve
    /// a third source while this run is still going and a run whose destination moved under it
    /// drops the picture it was halfway to (ADR-0183). The successor waits for this run to end.
    pub to: String,
    pub started: Instant,
    pub spec: TransitionSpec,
    /// Eased 0..1 as of the last advance, and what `layout::paint` draws the incoming at. Held
    /// rather than read off the clock at paint time for the reason a tween writes its value into
    /// `properties`: the display list is built once and compared for equality, so the number in it
    /// has to be a number a pass decided, not one that moves under the comparison.
    pub progress: f32,
}

impl Dissolve {
    pub fn start(from: String, to: String, spec: TransitionSpec, now: Instant) -> Self {
        Self { from, to, started: now, spec, progress: 0.0 }
    }

    /// Advances to `now`. `false` once the dissolve is over, which is the caller's cue to drop it
    /// and leave the node drawing the source it named.
    pub fn advance(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.spec.duration {
            return false;
        }
        // Clamped, unlike a tween's value: `Easing::apply` clamps its input and not its output, so
        // Back, Elastic and Bounce all leave [0, 1], and this number is drawn as an alpha rather
        // than handed to a property parser that would refuse it (ADR-0183).
        let eased = self.spec.easing.apply(elapsed.as_secs_f32() / self.spec.duration.as_secs_f32());
        self.progress = eased.clamp(0.0, 1.0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::animate::*;

    /// ADR-0181. A dissolve is a clock and a curve: nothing to write back, nothing to refuse, and
    /// it reports its own end rather than resting at 1.0 the way a played-out sequence does.
    #[test]
    fn a_dissolve_eases_across_its_duration_and_reports_when_it_is_over() {
        let spec = TransitionSpec {
            duration: Duration::from_millis(400),
            easing: Easing::Linear,
            shader: None,
            params: Vec::new(),
            images: Vec::new(),
        };
        let started = Instant::now();
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), spec.clone(), started);
        assert_eq!(dissolve.progress, 0.0, "it opens on the outgoing picture");

        assert!(dissolve.advance(started + Duration::from_millis(100)));
        assert!((dissolve.progress - 0.25).abs() < 1e-5);
        assert!(dissolve.advance(started + Duration::from_millis(300)));
        assert!((dissolve.progress - 0.75).abs() < 1e-5);

        assert!(!dissolve.advance(started + Duration::from_millis(400)), "the end is the end, not a rest at 1.0");

        // An overshooting curve is clamped before it is stored: this number is drawn as an alpha,
        // and `Easing::apply` clamps its input, not its output (ADR-0183).
        let overshoot = TransitionSpec {
            duration: Duration::from_millis(400),
            easing: Easing::OutBack,
            shader: None,
            params: Vec::new(),
            images: Vec::new(),
        };
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), overshoot, started);
        for millis in [40, 120, 200, 280, 360] {
            assert!(dissolve.advance(started + Duration::from_millis(millis)));
            assert!((0.0..=1.0).contains(&dissolve.progress), "{millis}ms gave {}", dissolve.progress);
        }
        assert!(!dissolve.advance(started + Duration::from_secs(9)));

        // A clock that has gone backwards saturates rather than wrapping into a huge progress.
        let mut dissolve = Dissolve::start("/tmp/a.png".into(), "/tmp/b.png".into(), spec.clone(), started);
        assert!(dissolve.advance(started - Duration::from_millis(50)));
        assert_eq!(dissolve.progress, 0.0);
    }

    /// A transition's `images` parse as a `shader`'s do, and need a `shader` to reach.
    #[test]
    fn a_transition_takes_images_only_beside_a_shader() {
        let lua = mlua::Lua::new();
        let parse = |src: &str| parse_transition(Some(&lua.load(format!("return {src}")).eval::<Value>().unwrap()));
        let spec = parse(r#"{ duration = 100, shader = "/s.frag", images = { b = "/b.png", a = "/a.png" } }"#)
            .unwrap()
            .unwrap();
        assert_eq!(spec.images, [("a".to_string(), "/a.png".to_string()), ("b".to_string(), "/b.png".to_string())]);
        for (src, property) in [
            (r#"{ duration = 100, images = { a = "/a.png" } }"#, "transition.images"),
            (r#"{ duration = 100, shader = "/s.frag", images = { u_a = "/a.png" } }"#, "transition.images.u_a"),
        ] {
            let err = parse(src).unwrap_err();
            assert!(matches!(&err, LayoutError::InvalidProperty { property: p, .. } if p == property), "{err:?}");
        }
    }
}
