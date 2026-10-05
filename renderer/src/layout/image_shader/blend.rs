//! The engine's blend pass: CSS `mix-blend-mode`, and Apple's `plus-lighter` and `plus-darker`.

use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use super::ShaderStage;
use super::state::{WHOLE, quad_pass};
use crate::layout::node::Blend;

/// One W3C Compositing formula on premultiplied pixels: `u_source` over `u_backdrop` with the
/// blend `B(cb, cs)` of `u_mode`, a [`Blend`]'s index, on straight sRGB. `u_map` takes a target
/// coordinate to the source's, which is clear outside it. Plus-lighter and plus-darker are
/// Apple's sums, clamped.
const BLEND: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 fragColor;
uniform sampler2D u_backdrop;
uniform sampler2D u_source;
uniform mat3 u_map;
uniform int u_mode;

float lum(vec3 c) { return dot(c, vec3(0.3, 0.59, 0.11)); }
float sat(vec3 c) { return max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b); }
vec3 set_lum(vec3 c, float l) {
    c += l - lum(c);
    l = lum(c);
    float n = min(min(c.r, c.g), c.b), x = max(max(c.r, c.g), c.b);
    if (n < 0.0) c = l + (c - l) * l / (l - n);
    if (x > 1.0) c = l + (c - l) * (1.0 - l) / (x - l);
    return c;
}
vec3 set_sat(vec3 c, float s) {
    float range = sat(c);
    return range > 0.0 ? (c - min(min(c.r, c.g), c.b)) * s / range : vec3(0.0);
}
vec3 hard_light(vec3 b, vec3 s) { return mix(2.0 * b * s, 1.0 - 2.0 * (1.0 - b) * (1.0 - s), step(0.5, s)); }
vec3 mode(vec3 b, vec3 s) {
    switch (u_mode) {
        case 1: return b * s;
        case 2: return b + s - b * s;
        case 3: return hard_light(s, b);
        case 4: return min(b, s);
        case 5: return max(b, s);
        case 6: return step(1e-6, b) * min(vec3(1.0), b / max(1.0 - s, 1e-6));
        case 7: return 1.0 - min(vec3(1.0), (1.0 - b) / max(s, 1e-6));
        case 8: return hard_light(b, s);
        case 9: {
            vec3 d = mix(sqrt(b), ((16.0 * b - 12.0) * b + 4.0) * b, step(b, vec3(0.25)));
            return mix(b - (1.0 - 2.0 * s) * b * (1.0 - b), b + (2.0 * s - 1.0) * (d - b), step(0.5, s));
        }
        case 10: return abs(b - s);
        case 11: return b + s - 2.0 * b * s;
        case 12: return set_lum(set_sat(s, sat(b)), lum(b));
        case 13: return set_lum(set_sat(b, sat(s)), lum(b));
        case 14: return set_lum(s, lum(b));
        case 15: return set_lum(b, lum(s));
    }
    return s;
}
void main() {
    vec4 b = texture(u_backdrop, v_uv);
    vec2 at = (u_map * vec3(v_uv, 1.0)).xy;
    vec4 s = at == clamp(at, 0.0, 1.0) ? texture(u_source, at) : vec4(0.0);
    float a = min(b.a + s.a, 1.0);
    if (u_mode == 16) {
        fragColor = min(b + s, 1.0);
    } else if (u_mode == 17) {
        fragColor = vec4(max(vec3(0.0), a - (b.a - b.rgb) - (s.a - s.rgb)), a);
    } else {
        vec3 cb = b.a > 0.0 ? b.rgb / b.a : vec3(0.0);
        vec3 cs = s.a > 0.0 ? s.rgb / s.a : vec3(0.0);
        vec3 mixed = s.rgb * (1.0 - b.a) + b.rgb * (1.0 - s.a) + s.a * b.a * mode(cb, cs);
        fragColor = vec4(mixed, s.a + b.a * (1.0 - s.a));
    }
}
"#;

/// One [`ShaderStage::blend`] pass: `source` blended over `backdrop` into `target`, which is
/// `backdrop`'s size; `map` is the column-major matrix from a target coordinate to `source`'s.
pub struct BlendPass {
    pub backdrop: ImageId,
    pub source: ImageId,
    pub target: ImageId,
    pub map: [f32; 9],
    pub mode: Blend,
}

/// [`BLEND`] linked, and its uniforms.
pub(super) struct Blending {
    pub(super) program: glow::Program,
    backdrop: Option<glow::UniformLocation>,
    source: Option<glow::UniformLocation>,
    map: Option<glow::UniformLocation>,
    mode: Option<glow::UniformLocation>,
}

impl ShaderStage {
    /// Runs `pass`, writing every pixel of its target, and answers `false`, drawing nothing, when
    /// [`BLEND`] will not build or a texture is gone.
    ///
    /// # Safety
    ///
    /// As [`ShaderStage::draw`].
    pub unsafe fn blend(&mut self, gl: &glow::Context, canvas: &mut Canvas<OpenGl>, pass: &BlendPass) -> bool {
        // SAFETY: caller's contract.
        let Some((vao, buffer)) = (unsafe { self.ensure_quad(gl) }) else { return false };
        if self.blending.is_none() {
            // SAFETY: caller's contract.
            self.blending = Some(unsafe { self.build_blend(gl) });
        }
        if self.framebuffer.is_none() {
            // SAFETY: caller's contract.
            self.framebuffer = unsafe { gl.create_framebuffer() }.ok();
        }
        let (Some(Some(blending)), Some(framebuffer)) = (&self.blending, self.framebuffer) else { return false };
        let (Ok(backdrop), Ok(source), Ok(target), Ok((width, height))) = (
            canvas.get_native_texture(pass.backdrop),
            canvas.get_native_texture(pass.source),
            canvas.get_native_texture(pass.target),
            canvas.image_size(pass.target),
        ) else {
            return false;
        };
        // SAFETY: caller's contract.
        unsafe {
            quad_pass(gl, canvas, (blending.program, framebuffer), (vao, buffer), &WHOLE, |gl| {
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(target),
                    0,
                );
                gl.viewport(0, 0, width as i32, height as i32);
                gl.active_texture(glow::TEXTURE1);
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(backdrop));
                gl.uniform_1_i32(blending.backdrop.as_ref(), 0);
                gl.uniform_1_i32(blending.source.as_ref(), 1);
                gl.uniform_matrix_3_f32_slice(blending.map.as_ref(), false, &pass.map);
                gl.uniform_1_i32(blending.mode.as_ref(), pass.mode as i32);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            });
        }
        true
    }

    /// # Safety
    ///
    /// The context is current.
    unsafe fn build_blend(&mut self, gl: &glow::Context) -> Option<Blending> {
        // SAFETY: caller's contract.
        unsafe {
            let program = self.link(gl, Path::new("<engine blend>"), BLEND)?;
            let named = |name: &str| gl.get_uniform_location(program, name);
            Some(Blending {
                program,
                backdrop: named("u_backdrop"),
                source: named("u_source"),
                map: named("u_map"),
                mode: named("u_mode"),
            })
        }
    }
}
