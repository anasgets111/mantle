//! The GL state a shader run changes, saved and put back.

use femtovg::Canvas;
use femtovg::renderer::OpenGl;
use glow::HasContext;

use crate::layout::node::MAX_SHADER_IMAGES;

/// A quad over a whole target, in GL's row order: clip x, y, then the texture's u, v per corner.
pub(super) const WHOLE: [f32; 16] =
    [-1.0, -1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0];

/// One quad pass into `framebuffer`: `program` draws `corners` with blend and every test off, and
/// the framebuffer, viewport and state femtovg left are put back before it records again. `draw`
/// attaches each target, sets its viewport, textures and uniforms, and draws.
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
    draw: impl FnOnce(&glow::Context),
) {
    crate::layout::paint::flush(canvas);
    // SAFETY: caller's contract.
    unsafe {
        let saved = State::capture(gl);
        let previous = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
        let mut viewport = [0; 4];
        gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
        gl.use_program(Some(program));
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, corners.map(f32::to_ne_bytes).as_flattened(), glow::STREAM_DRAW);
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
/// absent: this draws into whichever target is already current, and never touches that. The scissor
/// box *is* present, because this stage sets one: femtovg clips its own paths through a uniform its
/// shader reads, so a quad drawn here is clipped by nothing unless GL scissors it.
///
/// The colour mask is absent too, and not by oversight. femtovg masks colour writes while it lays
/// down a stencil and puts the mask back within the same operation (`renderer/opengl.rs`), so after
/// the flush this run begins with, all four channels are on. Nothing here changes it, so there is
/// nothing to put back -- and `glow` has no four-channel read for it, so querying would mean
/// storing one channel's answer and restoring it to all four.
pub(super) struct State {
    program: Option<glow::Program>,
    scissor_box: [i32; 4],
    vertex_array: Option<glow::VertexArray>,
    array_buffer: Option<glow::Buffer>,
    active_texture: u32,
    textures: [Option<glow::Texture>; MAX_SHADER_IMAGES + 2],
    blend: bool,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_equation_rgb: i32,
    blend_equation_alpha: i32,
    depth_test: bool,
    stencil_test: bool,
    cull_face: bool,
    scissor_test: bool,
}

impl State {
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn capture(gl: &glow::Context) -> Self {
        // SAFETY: caller's contract. Every query below is a plain `glGet` on the current context.
        unsafe {
            // Zero is GL's "nothing bound", and every `Native*` newtype wraps a `NonZeroU32`,
            // so the check and the conversion are the same step.
            let name = |slot: u32| std::num::NonZeroU32::new(gl.get_parameter_i32(slot) as u32);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let textures = std::array::from_fn(|unit| {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                name(glow::TEXTURE_BINDING_2D).map(glow::NativeTexture)
            });
            gl.active_texture(active_texture);
            let mut scissor_box = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            Self {
                program: name(glow::CURRENT_PROGRAM).map(glow::NativeProgram),
                scissor_box,
                vertex_array: name(glow::VERTEX_ARRAY_BINDING).map(glow::NativeVertexArray),
                array_buffer: name(glow::ARRAY_BUFFER_BINDING).map(glow::NativeBuffer),
                active_texture,
                textures,
                blend: gl.is_enabled(glow::BLEND),
                blend_src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                blend_dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB),
                blend_src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                blend_equation_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                blend_equation_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                depth_test: gl.is_enabled(glow::DEPTH_TEST),
                stencil_test: gl.is_enabled(glow::STENCIL_TEST),
                cull_face: gl.is_enabled(glow::CULL_FACE),
                scissor_test: gl.is_enabled(glow::SCISSOR_TEST),
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
            for (unit, texture) in self.textures.into_iter().enumerate().rev() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, texture);
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
            gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            gl.blend_equation_separate(self.blend_equation_rgb as u32, self.blend_equation_alpha as u32);
            toggle(self.depth_test, glow::DEPTH_TEST);
            toggle(self.stencil_test, glow::STENCIL_TEST);
            toggle(self.cull_face, glow::CULL_FACE);
            toggle(self.scissor_test, glow::SCISSOR_TEST);
            let [x, y, width, height] = self.scissor_box;
            gl.scissor(x, y, width, height);
        }
    }
}
