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
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub duration: Duration,
    pub easing: Easing,
    pub shader: Option<PathBuf>,
    pub params: Vec<ShaderParam>,
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
        let Self { duration, easing, shader, params } = self;
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
        let easing = easing.unwrap_or_default();
        Ok(TransitionSpec { duration, easing, shader, params })
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

/// `params = { softness = 0.1, tint = { 1, 0.5, 0, 1 } }`: uniform names to a number or a list of
/// up to [`MAX_PARAM_NUMBERS`] (ADR-0184, vectors ADR-0253, arrays ADR-0300). Sorted, so the list is a value two runs can compare.
pub(in crate::layout::node) fn parse_shader_params(what: &str, value: &Value) -> Result<Vec<ShaderParam>, LayoutError> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => {
            return Err(invalid(
                what,
                format!(
                    "expected a table of uniform names to a number or a list of numbers, got {}",
                    preview_for_error(other)
                ),
            ));
        }
    };
    let mut out = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| invalid(what, e.to_string()))?;
        let Value::String(key) = key else {
            return Err(invalid(what, format!("keys are uniform names, got {}", preview_for_error(&key))));
        };
        let name = key.to_str().map_err(|e| invalid(what, e.to_string()))?.to_string();
        let field = format!("{what}.{name}");
        let finite = |value: &Value| {
            value_as_f32(&field, value)?
                .filter(|number| number.is_finite())
                .ok_or_else(|| invalid(&field, format!("expected a finite number, got {}", preview_for_error(value))))
        };
        let numbers = match &value {
            Value::Table(list) => {
                let len = list.raw_len();
                if !(1..=MAX_PARAM_NUMBERS).contains(&len) {
                    return Err(invalid(&field, format!("expected 1 to {MAX_PARAM_NUMBERS} numbers, got {len}")));
                }
                (1..=len)
                    .map(|index| finite(&list.raw_get(index).map_err(|e| invalid(&field, e.to_string()))?))
                    .collect::<Result<_, _>>()?
            }
            scalar => vec![finite(scalar)?],
        };
        out.push((name, numbers));
    }
    // Sorted uniform names make equivalent shader runs compare equal; the shader ignores names it lacks.
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

/// `images = { normal = "/abs/normal.png" }`: sampler names to raster files. Each name becomes a
/// GLSL identifier, so it is checked here against the prelude's own, which `params` never needed.
/// Sorted, so the list is a value two runs can compare.
pub(in crate::layout::node) fn parse_shader_images(what: &str, value: &Value) -> Result<Vec<ShaderImage>, LayoutError> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => {
            return Err(invalid(
                what,
                format!("expected a table of sampler names to absolute paths, got {}", preview_for_error(other)),
            ));
        }
    };
    let mut out = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| invalid(what, e.to_string()))?;
        let (Value::String(key), Value::String(path)) = (&key, &value) else {
            return Err(invalid(what, format!("expected `name = \"/abs/path\"`, got {}", preview_for_error(&value))));
        };
        let name = key.to_str().map_err(|e| invalid(what, e.to_string()))?.to_string();
        let field = format!("{what}.{name}");
        if let Some(why) = sampler_name_error(&name) {
            return Err(invalid(&field, why));
        }
        let path = path.to_str().map_err(|e| invalid(&field, e.to_string()))?.to_string();
        if !path.starts_with('/') {
            return Err(invalid(&field, format!("expected an absolute path, got `{path}`")));
        }
        // Rasterizing a vector at the sampler's 8192px box would be a 256 MiB texture.
        if crate::image::is_vector(std::path::Path::new(&path)) {
            return Err(invalid(&field, "expected a raster file (PNG, JPEG, WebP), not an SVG"));
        }
        out.push((name, path));
        if out.len() > MAX_SHADER_IMAGES {
            return Err(invalid(what, format!("expected at most {MAX_SHADER_IMAGES} images")));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

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
}
