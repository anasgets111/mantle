//! The engine's Gaussian blur passes (ADR-0262).

use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use super::ShaderStage;
use super::state::{PASS_HEAD, WHOLE, attach, quad_pass, used_box};
use crate::layout::node;

/// The engine's Gaussian, one axis a pass (ADR-0262). `u_step` is one source texel along that
/// axis, `u_origin` and `u_extent` map the target's used rect into the source's; sigma `0` is one
/// bilinear read, which halves or restores a size. `u_pad` scales and clamps when a pass fills the
/// target's pooled slack with its clamped edges. `u_tone` recolours the result by the three colour
/// filters (ADR-0334); the pass that writes a blur's target carries it.
const BLUR: &str = r#"
uniform sampler2D u_source;
uniform vec2 u_step;
uniform vec2 u_origin;
uniform vec2 u_extent;
uniform vec4 u_pad;
uniform float u_sigma;
uniform bool u_tone;
uniform mat3 u_saturate;
uniform vec2 u_gain;
uniform vec4 u_box;
void main() {
    vec2 used_uv = vec2(v_uv.x * u_pad.x, 1.0 - (1.0 - v_uv.y) * u_pad.y);
    used_uv = clamp(used_uv, u_pad.zw, vec2(1.0) - u_pad.zw);
    vec2 uv = u_origin + used_uv * u_extent;
    vec4 sum = read(u_source, u_box, uv);
    float total = 1.0;
    int taps = int(ceil(3.0 * u_sigma));
    for (int i = 1; i <= taps; i++) {
        float weight = exp(-0.5 * float(i * i) / (u_sigma * u_sigma));
        vec2 offset = float(i) * u_step;
        sum += weight * (read(u_source, u_box, uv - offset) + read(u_source, u_box, uv + offset));
        total += 2.0 * weight;
    }
    vec4 color = sum / total;
    if (u_tone) {
        // CSS `saturate()`, `brightness()`, `contrast()` on straight sRGB, each clamped as CSS's are.
        vec3 rgb = color.a > 0.0 ? color.rgb / color.a : vec3(0.0);
        rgb = clamp(u_saturate * rgb, 0.0, 1.0);
        rgb = clamp(rgb * u_gain.x, 0.0, 1.0);
        rgb = clamp((rgb - 0.5) * u_gain.y + 0.5, 0.0, 1.0);
        color.rgb = rgb * color.a;
    }
    fragColor = color;
}
"#;

/// One [`ShaderStage::blur`] pass: `source` mapped by `origin` + `extent` drawn over the whole of
/// `target`, blurred by `sigma` source texels along `axis`, `[1, 0]` or `[0, 1]`. `sizes` are the
/// used parts of the source, then the target. When `pad_slack` is set, the pass draws the full
/// allocated target with clamped edge texels in its slack so femtovg can sample it under transforms.
pub struct BlurPass {
    pub source: ImageId,
    pub target: ImageId,
    pub sizes: [(usize, usize); 2],
    pub origin: [f32; 2],
    pub extent: [f32; 2],
    pub axis: [f32; 2],
    pub sigma: f32,
    pub tone: node::Tone,
    pub pad_slack: bool,
}

/// [`BLUR`] linked, and its uniforms.
pub(super) struct Blur {
    pub(super) program: glow::Program,
    source: Option<glow::UniformLocation>,
    step: Option<glow::UniformLocation>,
    origin: Option<glow::UniformLocation>,
    extent: Option<glow::UniformLocation>,
    pad: Option<glow::UniformLocation>,
    sigma: Option<glow::UniformLocation>,
    tone: Option<glow::UniformLocation>,
    saturate: Option<glow::UniformLocation>,
    gain: Option<glow::UniformLocation>,
    used: Option<glow::UniformLocation>,
}

impl ShaderStage {
    /// Runs `passes` in order, and answers `false`, drawing nothing, when [`BLUR`] will not build
    /// or a texture is gone (ADR-0262). Every target is pooled, so a frame allocates nothing.
    /// `flush` drains femtovg first when `passes` read a texture femtovg drew this frame.
    ///
    /// # Safety
    ///
    /// As [`ShaderStage::draw`].
    pub unsafe fn blur(
        &mut self,
        gl: &glow::Context,
        canvas: &mut Canvas<OpenGl>,
        passes: &[BlurPass],
        flush: bool,
    ) -> bool {
        // SAFETY: caller's contract.
        let Some((vao, buffer)) = (unsafe { self.ensure_quad(gl) }) else { return false };
        if self.blur.is_none() {
            // SAFETY: caller's contract.
            self.blur = Some(unsafe { self.build_blur(gl) });
        }
        // SAFETY: caller's contract.
        let Some(framebuffer) = (unsafe { self.ensure_framebuffer(gl) }) else { return false };
        let Some(Some(blur)) = &self.blur else { return false };
        let textures: Option<Vec<_>> = passes
            .iter()
            .map(|pass| {
                let source = canvas.get_native_texture(pass.source).ok()?;
                let target = canvas.get_native_texture(pass.target).ok()?;
                let [from, to] = pass.sizes;
                let step = [pass.axis[0] / from.0 as f32, pass.axis[1] / from.1 as f32];
                let used = used_box(canvas.image_size(pass.source).ok()?, from);
                Some((source, target, step, used, (canvas.image_size(pass.target).ok()?, to)))
            })
            .collect();
        let Some(textures) = textures else { return false };
        // SAFETY: caller's contract.
        unsafe {
            quad_pass(gl, canvas, (blur.program, framebuffer), (vao, buffer), &WHOLE, (flush, 1), |gl| {
                gl.active_texture(glow::TEXTURE0);
                gl.uniform_1_i32(blur.source.as_ref(), 0);
                for (pass, (source, target, step, used, (size, to))) in passes.iter().zip(textures) {
                    let pad = if pass.pad_slack && size != to {
                        attach(gl, target, size, size);
                        [size.0 as f32 / to.0 as f32, size.1 as f32 / to.1 as f32, 0.5 / to.0 as f32, 0.5 / to.1 as f32]
                    } else {
                        attach(gl, target, size, to);
                        [1.0, 1.0, 0.0, 0.0]
                    };
                    gl.uniform_4_f32_slice(blur.used.as_ref(), &used);
                    gl.uniform_4_f32_slice(blur.pad.as_ref(), &pad);
                    gl.bind_texture(glow::TEXTURE_2D, Some(source));
                    gl.uniform_2_f32(blur.step.as_ref(), step[0], step[1]);
                    gl.uniform_2_f32(blur.origin.as_ref(), pass.origin[0], pass.origin[1]);
                    gl.uniform_2_f32(blur.extent.as_ref(), pass.extent[0], pass.extent[1]);
                    gl.uniform_1_f32(blur.sigma.as_ref(), pass.sigma);
                    gl.uniform_1_i32(blur.tone.as_ref(), i32::from(!pass.tone.is_identity()));
                    gl.uniform_matrix_3_f32_slice(blur.saturate.as_ref(), false, &pass.tone.saturate_columns());
                    gl.uniform_2_f32(blur.gain.as_ref(), pass.tone.brightness, pass.tone.contrast);
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            });
        }
        true
    }

    /// # Safety
    ///
    /// The context is current.
    unsafe fn build_blur(&mut self, gl: &glow::Context) -> Option<Blur> {
        // SAFETY: caller's contract.
        unsafe {
            let program = self.link(gl, Path::new("<engine blur>"), &format!("{PASS_HEAD}{BLUR}"))?;
            let named = |name: &str| gl.get_uniform_location(program, name);
            Some(Blur {
                program,
                source: named("u_source"),
                step: named("u_step"),
                origin: named("u_origin"),
                extent: named("u_extent"),
                pad: named("u_pad"),
                sigma: named("u_sigma"),
                tone: named("u_tone"),
                saturate: named("u_saturate"),
                gain: named("u_gain"),
                used: named("u_box"),
            })
        }
    }
}
