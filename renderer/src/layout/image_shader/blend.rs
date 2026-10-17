//! The engine's blend pass: CSS `mix-blend-mode`, and Apple's `plus-lighter` and `plus-darker`.

use std::path::Path;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use super::ShaderStage;
use super::state::{WHOLE, attach, quad_pass, used_box};

/// One W3C Compositing formula on premultiplied pixels: `u_source` over `u_backdrop` with the
/// blend `B(cb, cs)` of `u_mode`, a [`Blend`](crate::layout::node::Blend)'s index, on straight sRGB. `u_map` takes a target
/// coordinate to the source's, which is clear outside it. Plus-lighter and plus-darker are
/// Apple's sums, clamped. [`PRESENT`] reads `u_backdrop` as a program's output and `u_source`
/// as the ground it lands on, unknown where its alpha is 0. Each `_box` places a texture's pixels
/// in its pooled texture, and `read` clamps to their edge (ADR-0370).
const BLEND: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 fragColor;
uniform sampler2D u_backdrop;
uniform sampler2D u_source;
uniform mat3 u_map;
uniform int u_mode;
uniform vec4 u_backdrop_box;
uniform vec4 u_source_box;
vec4 read(sampler2D tex, vec4 box, vec2 uv) {
    vec2 p = clamp(vec2(uv.x, 1.0 - uv.y) * box.xy, box.zw, box.xy - box.zw);
    return texture(tex, vec2(p.x, 1.0 - p.y));
}
// The most a presented pixel may cover. The next capture is unmixed by dividing by 1 - alpha, which
// multiplies its 8-bit rounding by 1 / (1 - alpha): 8x at this cap, 25x at 0.96, where the glass
// went black in a reference engine. A pixel that differs from the ground keeps a 12% ghost of it live.
const float MAX_ALPHA = 0.88;
// One 8-bit step: the output's rounding moves what it shows by up to this, and equal is not different.
const float TOL = 1.5 / 255.0;

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
    vec4 b = read(u_backdrop, u_backdrop_box, v_uv);
    vec2 at = (u_map * vec3(v_uv, 1.0)).xy;
    vec4 s = at == clamp(at, 0.0, 1.0) ? read(u_source, u_source_box, at) : vec4(0.0);
    float a = min(b.a + s.a, 1.0);
    if (u_mode == 16) {
        fragColor = min(b + s, 1.0);
    } else if (u_mode == 17) {
        fragColor = vec4(max(vec3(0.0), a - (b.a - b.rgb) - (s.a - s.rgb)), a);
    } else if (u_mode == 18) {
        // The output as presented over the ground: nothing where it shows the ground, else itself held
        // to the cap. It never depends on the ground per pixel, so the ground's noise stays off screen.
        vec3 g = s.rgb / max(s.a, 1e-6);
        bool same = s.a >= 0.5 && all(lessThanEqual(abs(b.rgb + (1.0 - b.a) * g - g), vec3(TOL)));
        fragColor = same ? vec4(0.0) : b * min(1.0, MAX_ALPHA / max(b.a, 1e-6));
    } else {
        vec3 cb = b.a > 0.0 ? b.rgb / b.a : vec3(0.0);
        vec3 cs = s.a > 0.0 ? s.rgb / s.a : vec3(0.0);
        vec3 mixed = s.rgb * (1.0 - b.a) + b.rgb * (1.0 - s.a) + s.a * b.a * mode(cb, cs);
        fragColor = vec4(mixed, s.a + b.a * (1.0 - s.a));
    }
}
"#;

/// [`BlendPass::mode`] for the pass that turns a program's output over an opaque ground into the
/// pixel presented over it: nothing where the output shows the ground, else the output held to the cap.
pub const PRESENT: i32 = 18;

/// One [`ShaderStage::blend`] pass: `source` blended over `backdrop` into `target`, which is
/// `backdrop`'s size; `map` is the column-major matrix from a target coordinate to `source`'s.
/// `sizes` are the parts of the pooled textures they use: `backdrop`'s and `target`'s, then
/// `source`'s.
pub struct BlendPass {
    pub backdrop: ImageId,
    pub source: ImageId,
    pub target: ImageId,
    pub sizes: [(usize, usize); 2],
    pub map: [f32; 9],
    /// A [`Blend`](crate::layout::node::Blend)'s index, or [`PRESENT`].
    pub mode: i32,
}

/// [`BLEND`] linked, and its uniforms.
pub(super) struct Blending {
    pub(super) program: glow::Program,
    backdrop: Option<glow::UniformLocation>,
    source: Option<glow::UniformLocation>,
    map: Option<glow::UniformLocation>,
    mode: Option<glow::UniformLocation>,
    backdrop_box: Option<glow::UniformLocation>,
    source_box: Option<glow::UniformLocation>,
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
        // SAFETY: caller's contract.
        let Some(framebuffer) = (unsafe { self.ensure_framebuffer(gl) }) else { return false };
        let Some(Some(blending)) = &self.blending else { return false };
        let (Ok(backdrop), Ok(source), Ok(target)) = (
            canvas.get_native_texture(pass.backdrop),
            canvas.get_native_texture(pass.source),
            canvas.get_native_texture(pass.target),
        ) else {
            return false;
        };
        let [used, source_used] = pass.sizes;
        let (Ok(backdrop_size), Ok(source_size), Ok(size)) =
            (canvas.image_size(pass.backdrop), canvas.image_size(pass.source), canvas.image_size(pass.target))
        else {
            return false;
        };
        // SAFETY: caller's contract.
        unsafe {
            quad_pass(gl, canvas, (blending.program, framebuffer), (vao, buffer), &WHOLE, |gl| {
                attach(gl, target, size, used);
                gl.uniform_4_f32_slice(blending.backdrop_box.as_ref(), &used_box(backdrop_size, used));
                gl.uniform_4_f32_slice(blending.source_box.as_ref(), &used_box(source_size, source_used));
                gl.active_texture(glow::TEXTURE1);
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(backdrop));
                gl.uniform_1_i32(blending.backdrop.as_ref(), 0);
                gl.uniform_1_i32(blending.source.as_ref(), 1);
                gl.uniform_matrix_3_f32_slice(blending.map.as_ref(), false, &pass.map);
                gl.uniform_1_i32(blending.mode.as_ref(), pass.mode);
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
                backdrop_box: named("u_backdrop_box"),
                source_box: named("u_source_box"),
            })
        }
    }
}
