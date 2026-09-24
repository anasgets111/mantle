//! The GL state a shader run changes, saved and put back.

use glow::HasContext;

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
    texture_0: Option<glow::Texture>,
    texture_1: Option<glow::Texture>,
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
            gl.active_texture(glow::TEXTURE0);
            let texture_0 = name(glow::TEXTURE_BINDING_2D).map(glow::NativeTexture);
            gl.active_texture(glow::TEXTURE1);
            let texture_1 = name(glow::TEXTURE_BINDING_2D).map(glow::NativeTexture);
            gl.active_texture(active_texture);
            let mut scissor_box = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            Self {
                program: name(glow::CURRENT_PROGRAM).map(glow::NativeProgram),
                scissor_box,
                vertex_array: name(glow::VERTEX_ARRAY_BINDING).map(glow::NativeVertexArray),
                array_buffer: name(glow::ARRAY_BUFFER_BINDING).map(glow::NativeBuffer),
                active_texture,
                texture_0,
                texture_1,
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
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, self.texture_1);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, self.texture_0);
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
