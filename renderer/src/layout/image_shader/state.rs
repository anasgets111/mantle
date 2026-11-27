//! The GL state a shader run changes, saved and put back.

use femtovg::Canvas;
use femtovg::renderer::OpenGl;
use glow::HasContext;

use super::ShaderStage;
use crate::layout::node::MAX_SHADER_IMAGES;

/// A quad over a whole target, in GL's row order: clip x, y, then the texture's u, v per corner.
pub(super) const WHOLE: [f32; 16] =
    [-1.0, -1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0];

impl ShaderStage {
    /// The framebuffer every pass draws through, created on first use.
    ///
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn ensure_framebuffer(&mut self, gl: &glow::Context) -> Option<glow::Framebuffer> {
        if self.framebuffer.is_none() {
            // SAFETY: caller's contract.
            self.framebuffer = unsafe { gl.create_framebuffer() }.ok();
        }
        self.framebuffer
    }
}

/// Draws into the `used` top-left of `target`, allocated at `size`: its last GL rows, as femtovg
/// draws a `FLIP_Y` image's (ADR-0370). [`quad_pass`] detaches it after.
///
/// # Safety
///
/// Inside a [`quad_pass`] `draw`, with `target` a live texture of the context.
pub(super) unsafe fn attach(gl: &glow::Context, target: glow::Texture, size: (usize, usize), used: (usize, usize)) {
    // SAFETY: caller's contract.
    unsafe {
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(target), 0);
        gl.viewport(0, (size.1 - used.1) as i32, used.0 as i32, used.1 as i32);
    }
}

/// The `vec4` a pass reads a texture allocated at `size` through when only its `used` top-left
/// holds pixels: that part's span, then half a texel, both as fractions of the texture. A read
/// clamped to the span inset by half a texel is `CLAMP_TO_EDGE` at the used part's edge (ADR-0370).
pub(super) fn used_box(size: (usize, usize), used: (usize, usize)) -> [f32; 4] {
    let (width, height) = (size.0 as f32, size.1 as f32);
    [used.0 as f32 / width, used.1 as f32 / height, 0.5 / width, 0.5 / height]
}

/// The engine passes' GLSL head: `read` samples `uv` of a texture's used part, GL's row order, through
/// its [`used_box`].
pub(super) const PASS_HEAD: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 fragColor;
vec4 read(sampler2D tex, vec4 box, vec2 uv) {
    vec2 p = clamp(vec2(uv.x, 1.0 - uv.y) * box.xy, box.zw, box.xy - box.zw);
    return texture(tex, vec2(p.x, 1.0 - p.y));
}
"#;

/// Uses `program` with the quad bound and `corners` uploaded to it.
///
/// # Safety
///
/// The context is current, and `program`, `vao` and `buffer` belong to it.
pub(super) unsafe fn bind_quad(
    gl: &glow::Context,
    program: glow::Program,
    (vao, buffer): (glow::VertexArray, glow::Buffer),
    corners: &[f32; 16],
) {
    // SAFETY: caller's contract.
    unsafe {
        gl.use_program(Some(program));
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, corners.map(f32::to_ne_bytes).as_flattened(), glow::STREAM_DRAW);
    }
}

/// One quad pass into `framebuffer`: `program` draws `corners` with blend and every test off, and
/// the framebuffer, viewport and state femtovg left are put back before it records again. `draw`
/// attaches each target, sets its viewport, textures and uniforms, and draws. `flush` drains
/// femtovg first when `draw` reads a texture femtovg drew this frame; `texture_units` is how many
/// units from `TEXTURE0` `draw` binds.
///
/// # Safety
///
/// The context is current, and `program`, `quad` and `framebuffer` belong to it.
pub(super) unsafe fn quad_pass(
    gl: &glow::Context,
    canvas: &mut Canvas<OpenGl>,
    (program, framebuffer): (glow::Program, glow::Framebuffer),
    (vao, buffer): (glow::VertexArray, glow::Buffer),
    corners: &[f32; 16],
    (flush, texture_units): (bool, usize),
    draw: impl FnOnce(&glow::Context),
) {
    if flush {
        crate::layout::paint::flush(canvas);
    }
    // SAFETY: caller's contract.
    unsafe {
        let saved = State::capture_quad(gl, texture_units);
        let previous = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
        let mut viewport = [0; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
        bind_quad(gl, program, (vao, buffer), corners);
        for slot in [glow::BLEND, glow::DEPTH_TEST, glow::STENCIL_TEST, glow::CULL_FACE, glow::SCISSOR_TEST] {
            gl.disable(slot);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        draw(gl);
        // A deleted texture still attached to a framebuffer keeps its storage.
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
        gl.bind_framebuffer(glow::FRAMEBUFFER, previous);
        gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
        saved.restore(gl);
    }
}

/// Every piece of GL state one run changes, read before and put back after.
///
/// femtovg keeps its own idea of what is bound and re-binds lazily, so anything left changed here
/// is a draw it makes later against state it never set. The framebuffer bindings are deliberately
/// absent: this draws into whichever target is already current, and never touches that. Full draws
/// ([`State::capture`]) also capture the scissor box and blend functions/equations; offscreen quad
/// passes ([`State::capture_quad`]) skip those and unused texture units because they never touch them.
///
/// The colour mask is absent too, and not by oversight. femtovg masks colour writes while it lays
/// down a stencil and puts the mask back within the same operation (`renderer/opengl.rs`), so after
/// the flush this run begins with, all four channels are on. Nothing here changes it, so there is
/// nothing to put back -- and `glow` has no four-channel read for it, so querying would mean
/// storing one channel's answer and restoring it to all four.
pub(super) struct State {
    program: Option<glow::Program>,
    vertex_array: Option<glow::VertexArray>,
    array_buffer: Option<glow::Buffer>,
    active_texture: u32,
    texture_units: usize,
    textures: [Option<glow::Texture>; MAX_SHADER_IMAGES + 2],
    blend: bool,
    depth_test: bool,
    stencil_test: bool,
    cull_face: bool,
    scissor_test: bool,
    /// `(scissor_box, [src_rgb, dst_rgb, src_alpha, dst_alpha, eq_rgb, eq_alpha])`, captured only
    /// by full draws that alter blending and the scissor box.
    blend_and_scissor: Option<([i32; 4], [i32; 6])>,
}

impl State {
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn capture(gl: &glow::Context) -> Self {
        // SAFETY: caller's contract.
        unsafe { Self::capture_with(gl, MAX_SHADER_IMAGES + 2, true) }
    }

    /// Captures only the texture units and toggles [`quad_pass`] mutates, skipping unused texture
    /// unit queries, blend equations and the scissor box.
    ///
    /// # Safety
    ///
    /// The context is current.
    unsafe fn capture_quad(gl: &glow::Context, texture_units: usize) -> Self {
        // SAFETY: caller's contract.
        unsafe { Self::capture_with(gl, texture_units, false) }
    }

    /// # Safety
    ///
    /// The context is current.
    unsafe fn capture_with(gl: &glow::Context, texture_units: usize, with_blend_and_scissor: bool) -> Self {
        let texture_units = texture_units.min(MAX_SHADER_IMAGES + 2);
        // SAFETY: caller's contract. Every query below is a plain `glGet` on the current context.
        unsafe {
            // Zero is GL's "nothing bound", and every `Native*` newtype wraps a `NonZeroU32`,
            // so the check and the conversion are the same step.
            let name = |slot: u32| std::num::NonZeroU32::new(gl.get_parameter_i32(slot) as u32);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let mut textures = [None; MAX_SHADER_IMAGES + 2];
            for (unit, slot) in textures.iter_mut().take(texture_units).enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                *slot = name(glow::TEXTURE_BINDING_2D).map(glow::NativeTexture);
            }
            gl.active_texture(active_texture);
            let blend_and_scissor = with_blend_and_scissor.then(|| {
                let mut scissor_box = [0; 4];
                gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
                let blend = [
                    gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                    gl.get_parameter_i32(glow::BLEND_DST_RGB),
                    gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                    gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                    gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                    gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                ];
                (scissor_box, blend)
            });
            Self {
                program: name(glow::CURRENT_PROGRAM).map(glow::NativeProgram),
                vertex_array: name(glow::VERTEX_ARRAY_BINDING).map(glow::NativeVertexArray),
                array_buffer: name(glow::ARRAY_BUFFER_BINDING).map(glow::NativeBuffer),
                active_texture,
                texture_units,
                textures,
                blend: gl.is_enabled(glow::BLEND),
                depth_test: gl.is_enabled(glow::DEPTH_TEST),
                stencil_test: gl.is_enabled(glow::STENCIL_TEST),
                cull_face: gl.is_enabled(glow::CULL_FACE),
                scissor_test: gl.is_enabled(glow::SCISSOR_TEST),
                blend_and_scissor,
            }
        }
    }

    /// # Safety
    ///
    /// The context is current and is the one [`State::capture`] read.
    pub(super) unsafe fn restore(self, gl: &glow::Context) {
        // SAFETY: caller's contract.
        unsafe {
            gl.use_program(self.program);
            gl.bind_vertex_array(self.vertex_array);
            gl.bind_buffer(glow::ARRAY_BUFFER, self.array_buffer);
            for unit in (0..self.texture_units).rev() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, self.textures[unit]);
            }
            gl.active_texture(self.active_texture);
            let toggle = |enabled: bool, slot: u32| {
                if enabled {
                    gl.enable(slot);
                } else {
                    gl.disable(slot);
                }
            };
            toggle(self.blend, glow::BLEND);
            toggle(self.depth_test, glow::DEPTH_TEST);
            toggle(self.stencil_test, glow::STENCIL_TEST);
            toggle(self.cull_face, glow::CULL_FACE);
            toggle(self.scissor_test, glow::SCISSOR_TEST);
            if let Some(([x, y, width, height], [src_rgb, dst_rgb, src_a, dst_a, eq_rgb, eq_a])) =
                self.blend_and_scissor
            {
                gl.blend_func_separate(src_rgb as u32, dst_rgb as u32, src_a as u32, dst_a as u32);
                gl.blend_equation_separate(eq_rgb as u32, eq_a as u32);
                gl.scissor(x, y, width, height);
            }
        }
    }
}
