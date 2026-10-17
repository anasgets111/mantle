//! The engine's Gaussian blur passes (ADR-0262).

use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use super::ShaderStage;
use super::state::{WHOLE, attach, quad_pass, used_box};
use crate::layout::node;

/// The engine's Gaussian, one axis a pass (ADR-0262). `u_step` is one source texel along that
/// axis, and `u_extent` the source coordinate at the target's far corner; sigma `0` is one
/// bilinear read, which halves or restores a size. `u_tone` recolours the result by the three
/// colour filters (ADR-0334); the pass that writes a blur's target carries it. `u_box` places the
/// source's pixels in its pooled texture, and `read` clamps to their edge (ADR-0370).
const BLUR: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 fragColor;
uniform sampler2D u_source;
uniform vec2 u_step;
uniform vec2 u_extent;
uniform float u_sigma;
uniform bool u_tone;
uniform mat3 u_saturate;
uniform vec2 u_gain;
uniform vec4 u_box;
vec4 read(vec2 uv) {
    vec2 p = clamp(vec2(uv.x, 1.0 - uv.y) * u_box.xy, u_box.zw, u_box.xy - u_box.zw);
    return texture(u_source, vec2(p.x, 1.0 - p.y));
}
void main() {
    vec2 uv = v_uv * u_extent;
    vec4 sum = read(uv);
    float total = 1.0;
    int taps = int(ceil(3.0 * u_sigma));
    for (int i = 1; i <= taps; i++) {
        float weight = exp(-0.5 * float(i * i) / (u_sigma * u_sigma));
        vec2 offset = float(i) * u_step;
        sum += weight * (read(uv - offset) + read(uv + offset));
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

/// One [`ShaderStage::blur`] pass: `source` up to `extent` of its size drawn over the whole of
/// `target`, blurred by `sigma` source texels along `axis`, `[1, 0]` or `[0, 1]`. Both sizes are
/// the parts of the pooled textures they use, `sizes` the source's, then the target's.
pub struct BlurPass {
    pub source: ImageId,
    pub target: ImageId,
    pub sizes: [(usize, usize); 2],
    pub extent: [f32; 2],
    pub axis: [f32; 2],
    pub sigma: f32,
    pub tone: node::Tone,
}

/// [`BLUR`] linked, and its uniforms.
pub(super) struct Blur {
    pub(super) program: glow::Program,
    source: Option<glow::UniformLocation>,
    step: Option<glow::UniformLocation>,
    extent: Option<glow::UniformLocation>,
    sigma: Option<glow::UniformLocation>,
    tone: Option<glow::UniformLocation>,
    saturate: Option<glow::UniformLocation>,
    gain: Option<glow::UniformLocation>,
    used: Option<glow::UniformLocation>,
}

impl ShaderStage {
    /// Runs `passes` in order, and answers `false`, drawing nothing, when [`BLUR`] will not build
    /// or a texture is gone (ADR-0262). Every target is pooled, so a frame allocates nothing.
    ///
    /// # Safety
    ///
    /// As [`ShaderStage::draw`].
    pub unsafe fn blur(&mut self, gl: &glow::Context, canvas: &mut Canvas<OpenGl>, passes: &[BlurPass]) -> bool {
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
            quad_pass(gl, canvas, (blur.program, framebuffer), (vao, buffer), &WHOLE, |gl| {
                gl.active_texture(glow::TEXTURE0);
                gl.uniform_1_i32(blur.source.as_ref(), 0);
                for (pass, (source, target, step, used, (size, to))) in passes.iter().zip(textures) {
                    attach(gl, target, size, to);
                    gl.uniform_4_f32_slice(blur.used.as_ref(), &used);
                    gl.bind_texture(glow::TEXTURE_2D, Some(source));
                    gl.uniform_2_f32(blur.step.as_ref(), step[0], step[1]);
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
            let program = self.link(gl, Path::new("<engine blur>"), BLUR)?;
            let named = |name: &str| gl.get_uniform_location(program, name);
            Some(Blur {
                program,
                source: named("u_source"),
                step: named("u_step"),
                extent: named("u_extent"),
                sigma: named("u_sigma"),
                tone: named("u_tone"),
                saturate: named("u_saturate"),
                gain: named("u_gain"),
                used: named("u_box"),
            })
        }
    }
}
