//! Program lifecycle: compile, link, cache and delete the GL objects a stage owns.

use super::*;

impl ShaderStage {
    /// Compiles [`contract::FADE`] on first use. Reported once if it will not build, which would mean the
    /// engine's own shader is broken rather than a config's.
    ///
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn ensure_fade(&mut self, gl: &glow::Context) {
        if self.fade.is_some() {
            return;
        }
        // SAFETY: caller's contract.
        let built = unsafe { self.build(gl, Path::new("<engine cross-dissolve>"), FADE, Variant::Cross, &[]) };
        self.fade = Some(built);
    }

    /// The shared quad, created on first use.
    ///
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn ensure_quad(&mut self, gl: &glow::Context) -> Option<(glow::VertexArray, glow::Buffer)> {
        if self.quad.is_none() {
            // SAFETY: caller's contract.
            self.quad = Some(unsafe { make_quad(gl) }?);
        }
        self.quad
    }

    /// Compiles and links `path` if this version of it is not already known. Answers whether a
    /// program is ready.
    ///
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn ensure_program(
        &mut self,
        gl: &glow::Context,
        key: &ProgramKey,
        images: &[(&str, Option<ImageId>)],
    ) -> bool {
        let (path, variant, _) = key;
        let version = crate::image::FileVersion::read(path);
        if let Some((known, program)) = self.programs.get(key)
            && *known == version
        {
            return program.is_some();
        }
        // A superseded program is deleted here, while the context is current, rather than left for
        // teardown: a config iterating on an effect would otherwise leak one program per save.
        if let Some((_, Some(stale))) = self.programs.remove(key) {
            // SAFETY: caller's contract.
            unsafe { gl.delete_program(stale.program) };
        }
        let built = match std::fs::read_to_string(path) {
            // SAFETY: caller's contract.
            Ok(source) => unsafe { self.build(gl, path, &source, *variant, images) },
            Err(err) => {
                error!("{}: {err}", path.display());
                None
            }
        };
        let ready = built.is_some();
        self.programs.insert(key.clone(), (version, built));
        ready
    }

    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn build(
        &mut self,
        gl: &glow::Context,
        path: &Path,
        source: &str,
        variant: Variant,
        images: &[(&str, Option<ImageId>)],
    ) -> Option<Program> {
        // SAFETY: caller's contract. Every object created here is deleted on the paths that fail
        // after creating it, and by `destroy` at teardown.
        unsafe {
            let program = self.link(gl, path, &assemble(source, variant, images))?;
            let named = |name: &str| gl.get_uniform_location(program, name);
            let mut params = HashMap::default();
            for index in 0..gl.get_active_uniforms(program) {
                let Some(uniform) = gl.get_active_uniform(program, index) else { continue };
                if uniform.name.starts_with("u_") || uniform.name.starts_with("mantle_") {
                    continue;
                }
                // An image's sampler and `_size` are the engine's to set, not `params`'.
                if images.iter().any(|(image, _)| uniform.name == *image || uniform.name == format!("{image}_size")) {
                    continue;
                }
                let width = match uniform.utype {
                    glow::FLOAT => 1,
                    glow::FLOAT_VEC2 => 2,
                    glow::FLOAT_VEC3 => 3,
                    glow::FLOAT_VEC4 => 4,
                    _ => {
                        error!(
                            "{}: `{}` is not a `float` or `vec2`-`vec4`, or an array of one, which is all `params` carries",
                            path.display(),
                            uniform.name
                        );
                        gl.delete_program(program);
                        return None;
                    }
                };
                // An array reports as `name[0]`, whose location is the first element's.
                if let Some(location) = gl.get_uniform_location(program, &uniform.name) {
                    let name = uniform.name.strip_suffix("[0]").unwrap_or(&uniform.name).to_string();
                    params.insert(name, (location, width, uniform.size.max(1) as usize, std::cell::Cell::new(false)));
                }
            }
            Some(Program {
                program,
                from: named("u_from"),
                to: named("u_to"),
                input: named("u_input"),
                input_blurred: named("u_input_blurred"),
                input_rect: named("u_input_rect"),
                progress: named("u_progress"),
                size: named("u_size"),
                from_rect: named("u_from_rect"),
                to_rect: named("u_to_rect"),
                fill: named("u_fill"),
                opacity: named("mantle_opacity"),
                radii: named("mantle_radii"),
                reach: named("mantle_reach"),
                power: named("mantle_power"),
                round: named("mantle_round"),
                shape: named("mantle_shape"),
                shape_len: named("mantle_shape_len"),
                params,
                images: images.iter().map(|(image, _)| (named(image), named(&format!("{image}_size")))).collect(),
            })
        }
    }

    /// The engine's vertex stage linked with `fragment`, reported once if either will not build.
    ///
    /// # Safety
    ///
    /// The context is current.
    pub(super) unsafe fn link(&mut self, gl: &glow::Context, path: &Path, fragment: &str) -> Option<glow::Program> {
        // SAFETY: caller's contract. Every object created here is deleted on the paths that fail
        // after creating it, and by `destroy` at teardown.
        unsafe {
            let vertex = match self.vertex {
                Some(vertex) => vertex,
                None => {
                    let vertex = compile(gl, glow::VERTEX_SHADER, VERTEX, Path::new("<engine vertex stage>"))?;
                    self.vertex = Some(vertex);
                    vertex
                }
            };
            let fragment = compile(gl, glow::FRAGMENT_SHADER, fragment, path)?;
            let Ok(program) = gl.create_program() else {
                gl.delete_shader(fragment);
                return None;
            };
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
            gl.detach_shader(program, vertex);
            gl.detach_shader(program, fragment);
            gl.delete_shader(fragment);
            if !gl.get_program_link_status(program) {
                error!("{}: {}", path.display(), gl.get_program_info_log(program));
                gl.delete_program(program);
                return None;
            }
            Some(program)
        }
    }

    /// Frees every GL object while the context is still current. A context that has gone away takes
    /// its objects with it, so this is for an orderly teardown and not for recovery.
    ///
    /// # Safety
    ///
    /// `gl` must be the context these were created on, current on this thread.
    pub unsafe fn destroy(&mut self, gl: &glow::Context) {
        // SAFETY: caller's contract.
        unsafe {
            for (_, program) in self.programs.values() {
                if let Some(program) = program {
                    gl.delete_program(program.program);
                }
            }
            if let Some(Some(fade)) = self.fade.take() {
                gl.delete_program(fade.program);
            }
            if let Some(Some(blur)) = self.blur.take() {
                gl.delete_program(blur.program);
            }
            if let Some(Some(blending)) = self.blending.take() {
                gl.delete_program(blending.program);
            }
            if let Some(framebuffer) = self.framebuffer.take() {
                gl.delete_framebuffer(framebuffer);
            }
            if let Some(vertex) = self.vertex.take() {
                gl.delete_shader(vertex);
            }
            if let Some((vao, buffer)) = self.quad.take() {
                gl.delete_vertex_array(vao);
                gl.delete_buffer(buffer);
            }
        }
        self.programs.clear();
    }
}

/// # Safety
///
/// The context is current.
unsafe fn compile(gl: &glow::Context, kind: u32, source: &str, path: &Path) -> Option<glow::Shader> {
    // SAFETY: caller's contract.
    unsafe {
        let shader = gl.create_shader(kind).ok()?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if gl.get_shader_compile_status(shader) {
            return Some(shader);
        }
        // `#line 1` ends the prelude, so a reported line number is the config's own.
        error!("{}: {}", path.display(), gl.get_shader_info_log(shader));
        gl.delete_shader(shader);
        None
    }
}

/// The vertex array and buffer every run reuses. The contents are rewritten per draw, because the
/// corners carry the node's box, target and transform; only the layout is fixed here.
///
/// # Safety
///
/// The context is current.
unsafe fn make_quad(gl: &glow::Context) -> Option<(glow::VertexArray, glow::Buffer)> {
    // SAFETY: caller's contract.
    unsafe {
        let vao = gl.create_vertex_array().ok()?;
        let Ok(buffer) = gl.create_buffer() else {
            gl.delete_vertex_array(vao);
            return None;
        };
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
        // Four vertices of `[x, y, u, v]`, matching the two `layout(location = ...)` inputs the
        // engine's vertex stage declares -- bound explicitly rather than left to the linker.
        let stride = 4 * std::mem::size_of::<f32>() as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 2 * std::mem::size_of::<f32>() as i32);
        gl.bind_vertex_array(None);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        Some((vao, buffer))
    }
}
