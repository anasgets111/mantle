//! The engine's Gaussian blur passes (ADR-0262).

use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use super::ShaderStage;
use super::state::State;

/// The engine's Gaussian, one axis a pass (ADR-0262). `u_step` is one source texel along that
/// axis, and `u_extent` the source coordinate at the target's far corner; sigma `0` is one
/// bilinear read, which halves or restores a size.
const BLUR: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 fragColor;
uniform sampler2D u_source;
uniform vec2 u_step;
uniform vec2 u_extent;
uniform float u_sigma;
void main() {
    vec2 uv = v_uv * u_extent;
    vec4 sum = texture(u_source, uv);
    float total = 1.0;
    int taps = int(ceil(3.0 * u_sigma));
    for (int i = 1; i <= taps; i++) {
        float weight = exp(-0.5 * float(i * i) / (u_sigma * u_sigma));
        vec2 offset = float(i) * u_step;
        sum += weight * (texture(u_source, uv - offset) + texture(u_source, uv + offset));
        total += 2.0 * weight;
    }
    fragColor = sum / total;
}
"#;

/// One [`ShaderStage::blur`] pass: `source` up to `extent` of its size drawn over the whole of
/// `target`, blurred by `sigma` source texels along `axis`, `[1, 0]` or `[0, 1]`.
pub struct BlurPass {
    pub source: ImageId,
    pub target: ImageId,
    pub extent: [f32; 2],
    pub axis: [f32; 2],
    pub sigma: f32,
}

/// [`BLUR`] linked, its uniforms, and the framebuffer its passes draw through.
pub(super) struct Blur {
    pub(super) program: glow::Program,
    pub(super) framebuffer: glow::Framebuffer,
    source: Option<glow::UniformLocation>,
    step: Option<glow::UniformLocation>,
    extent: Option<glow::UniformLocation>,
    sigma: Option<glow::UniformLocation>,
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
        let Some(Some(blur)) = &self.blur else { return false };
        let textures: Option<Vec<_>> = passes
            .iter()
            .map(|pass| {
                let source = canvas.get_native_texture(pass.source).ok()?;
                let target = canvas.get_native_texture(pass.target).ok()?;
                let (width, height) = canvas.image_size(pass.source).ok()?;
                let (target_width, target_height) = canvas.image_size(pass.target).ok()?;
                let step = [pass.axis[0] / width as f32, pass.axis[1] / height as f32];
                Some((source, target, step, (target_width as i32, target_height as i32)))
            })
            .collect();
        let Some(textures) = textures else { return false };
        crate::layout::paint::flush(canvas);

        // SAFETY: caller's contract. The framebuffer and viewport femtovg left are put back with
        // the rest below, before it records another command.
        unsafe {
            let saved = State::capture(gl);
            let framebuffer = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            gl.use_program(Some(blur.program));
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
            // The whole target reads the whole source; both keep GL's row order, so `FLIP_Y` is moot.
            let corners: [f32; 16] =
                [-1.0, -1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0];
            let bytes: Vec<u8> = corners.iter().flat_map(|value| value.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STREAM_DRAW);
            for slot in [glow::BLEND, glow::DEPTH_TEST, glow::STENCIL_TEST, glow::CULL_FACE, glow::SCISSOR_TEST] {
                gl.disable(slot);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(blur.framebuffer));
            gl.active_texture(glow::TEXTURE0);
            gl.uniform_1_i32(blur.source.as_ref(), 0);
            for (pass, (source, target, step, (width, height))) in passes.iter().zip(textures) {
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(target),
                    0,
                );
                gl.viewport(0, 0, width, height);
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                gl.uniform_2_f32(blur.step.as_ref(), step[0], step[1]);
                gl.uniform_2_f32(blur.extent.as_ref(), pass.extent[0], pass.extent[1]);
                gl.uniform_1_f32(blur.sigma.as_ref(), pass.sigma);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            // A deleted texture still attached to a framebuffer keeps its storage.
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
            saved.restore(gl);
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
            let Ok(framebuffer) = gl.create_framebuffer() else {
                gl.delete_program(program);
                return None;
            };
            let named = |name: &str| gl.get_uniform_location(program, name);
            Some(Blur {
                program,
                framebuffer,
                source: named("u_source"),
                step: named("u_step"),
                extent: named("u_extent"),
                sigma: named("u_sigma"),
            })
        }
    }
}
