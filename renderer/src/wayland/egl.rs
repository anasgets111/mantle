use std::ffi::c_void;

use khronos_egl as egl;

/// `eglGetConfigAttrib`'s candidate fields, kept separate from `egl::Config` so
/// `satisfies_requirements` stays pure and testable without a live EGL display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigAttribs {
    pub surface_type: egl::Int,
    pub renderable_type: egl::Int,
    pub red_size: egl::Int,
    pub green_size: egl::Int,
    pub blue_size: egl::Int,
    pub alpha_size: egl::Int,
}

/// Requires an on-screen window surface, GLES3, and exactly 8-bit-per-channel ARGB.
///
/// `eglChooseConfig` promises matches but returns bitmask supersets. Drivers disagree on exact
/// versus minimum component sizes, so revalidate every candidate instead of trusting the first.
pub fn satisfies_requirements(attrs: ConfigAttribs) -> bool {
    attrs.surface_type & egl::WINDOW_BIT != 0
        && attrs.renderable_type & egl::OPENGL_ES3_BIT != 0
        && attrs.red_size == 8
        && attrs.green_size == 8
        && attrs.blue_size == 8
        && attrs.alpha_size == 8
}

/// Shared by every static surface: one display, config, and GLES3 context; one EGL surface per
/// `wl_surface`.
pub struct EglState {
    pub instance: egl::Instance<egl::Static>,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    /// `None` swaps with plain `eglSwapBuffers`, which damages the whole surface.
    pub swap_with_damage: Option<super::egl_ext::SwapBuffersWithDamage>,
    /// Tells a tiling GPU what a partial repaint redraws, so it reloads only the rest (ADR-0258).
    pub set_damage_region: Option<super::egl_ext::SwapBuffersWithDamage>,
    /// Keeps alive the `wl_display` EGL holds a raw pointer to.
    _connection: wayland_client::Connection,
}

impl EglState {
    /// `eglMakeCurrent` for this display. khronos-egl's wrapper `unwrap`s an unrecognized
    /// `eglGetError` (khronos-egl#25); this reports it instead.
    pub fn make_current(&self, surface: Option<egl::Surface>, context: Option<egl::Context>) -> Result<(), String> {
        use egl::api::EGL1_0;
        let surface = surface.map_or(egl::NO_SURFACE, |surface| surface.as_ptr());
        let context = context.map_or(egl::NO_CONTEXT, |context| context.as_ptr());
        // SAFETY: `display` is live; the handles are this state's own or null.
        egl_result(unsafe { egl::Static.eglMakeCurrent(self.display.as_ptr(), surface, surface, context) })
    }

    /// `eglSwapInterval`, with the same raw error read as [`Self::make_current`].
    pub fn swap_interval(&self, interval: egl::Int) -> Result<(), String> {
        use egl::api::EGL1_1;
        // SAFETY: `display` is live.
        egl_result(unsafe { egl::Static.eglSwapInterval(self.display.as_ptr(), interval) })
    }

    /// `eglCreateWindowSurface` on this state's display and config.
    ///
    /// # Safety
    /// `window` must be a live `wl_egl_window*`.
    pub unsafe fn create_window_surface(&self, window: *mut c_void) -> Result<egl::Surface, String> {
        use egl::api::EGL1_0;
        // SAFETY: the caller vouches for `window`; display and config are this state's own.
        let raw = unsafe {
            egl::Static.eglCreateWindowSurface(self.display.as_ptr(), self.config.as_ptr(), window, std::ptr::null())
        };
        egl_result(egl::Boolean::from(raw != egl::NO_SURFACE))?;
        // SAFETY: `raw` is the non-null surface just created.
        Ok(unsafe { egl::Surface::from_ptr(raw) })
    }

    pub fn destroy_surface(&self, surface: egl::Surface) -> Result<(), String> {
        use egl::api::EGL1_0;
        // SAFETY: `display` is live; EGL rejects a stale surface with an error code.
        egl_result(unsafe { egl::Static.eglDestroySurface(self.display.as_ptr(), surface.as_ptr()) })
    }

    pub fn query_surface(&self, surface: egl::Surface, attribute: egl::Int) -> Result<egl::Int, String> {
        use egl::api::EGL1_0;
        let mut value = 0;
        // SAFETY: `value` is out-only, sized for `EGLint`.
        egl_result(unsafe {
            egl::Static.eglQuerySurface(self.display.as_ptr(), surface.as_ptr(), attribute, &mut value)
        })?;
        Ok(value)
    }
}

fn config_attrib(display: egl::Display, config: egl::Config, attribute: egl::Int) -> Result<egl::Int, String> {
    use egl::api::EGL1_0;
    let mut value = 0;
    // SAFETY: `value` is out-only, sized for `EGLint`.
    egl_result(unsafe { egl::Static.eglGetConfigAttrib(display.as_ptr(), config.as_ptr(), attribute, &mut value) })?;
    Ok(value)
}

fn egl_result(ok: egl::Boolean) -> Result<(), String> {
    use egl::api::EGL1_0;
    // SAFETY: reads this thread's last EGL error.
    (ok != egl::FALSE).then_some(()).ok_or_else(|| format!("EGL error {:#x}", unsafe { egl::Static.eglGetError() }))
}

/// Initializes EGL against Wayland and picks the first candidate that satisfies
/// [`satisfies_requirements`], not merely the first returned by `eglChooseConfig`.
pub fn init(connection: &wayland_client::Connection) -> Result<EglState, String> {
    let instance = egl::Instance::new(egl::Static);

    // SAFETY: a live `wl_display*`, kept alive past this call by the returned state's clone of
    // `connection`, which EGL requires for as long as the display is used.
    let display = unsafe { instance.get_display(connection.backend().display_ptr() as *mut c_void) }
        .ok_or("eglGetDisplay returned no display for the Wayland connection")?;

    instance.initialize(display).map_err(|e| format!("eglInitialize failed: {e}"))?;

    instance.bind_api(egl::OPENGL_ES_API).map_err(|e| format!("eglBindAPI(EGL_OPENGL_ES_API) failed: {e}"))?;

    let query_attribs = [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES3_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        // femtovg fills a concave path and strokes a translucent one through the stencil buffer;
        // without one a concave fill covers its bounds and a stroke paints its overlaps twice.
        egl::STENCIL_SIZE,
        8,
        egl::NONE,
    ];

    let count = instance
        .matching_config_count(display, &query_attribs)
        .map_err(|e| format!("eglChooseConfig (count) failed: {e}"))?;
    let mut candidates = Vec::with_capacity(count);
    instance
        .choose_config(display, &query_attribs, &mut candidates)
        .map_err(|e| format!("eglChooseConfig failed: {e}"))?;

    let config = candidates
        .into_iter()
        .find(|candidate| {
            let attrs = query_config_attribs(display, *candidate);
            attrs.is_ok_and(satisfies_requirements)
        })
        .ok_or("no EGL config matches window+GLES3+8-bit-ARGB requirements")?;

    let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
    let context = instance
        .create_context(display, config, None, &context_attribs)
        .map_err(|e| format!("eglCreateContext (GLES3) failed: {e}"))?;

    let swap_with_damage = super::egl_ext::swap_buffers_with_damage(&instance, display);
    let set_damage_region = super::egl_ext::set_damage_region(&instance, display);
    let _connection = connection.clone();
    Ok(EglState { instance, display, config, context, swap_with_damage, set_damage_region, _connection })
}

fn query_config_attribs(display: egl::Display, config: egl::Config) -> Result<ConfigAttribs, String> {
    Ok(ConfigAttribs {
        surface_type: config_attrib(display, config, egl::SURFACE_TYPE)?,
        renderable_type: config_attrib(display, config, egl::RENDERABLE_TYPE)?,
        red_size: config_attrib(display, config, egl::RED_SIZE)?,
        green_size: config_attrib(display, config, egl::GREEN_SIZE)?,
        blue_size: config_attrib(display, config, egl::BLUE_SIZE)?,
        alpha_size: config_attrib(display, config, egl::ALPHA_SIZE)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_match() -> ConfigAttribs {
        ConfigAttribs {
            surface_type: egl::WINDOW_BIT,
            renderable_type: egl::OPENGL_ES3_BIT,
            red_size: 8,
            green_size: 8,
            blue_size: 8,
            alpha_size: 8,
        }
    }

    #[test]
    fn accepts_exact_window_gles3_argb8888_config() {
        assert!(satisfies_requirements(full_match()));
    }

    #[test]
    fn accepts_config_with_extra_bits_set() {
        let mut attrs = full_match();
        attrs.surface_type |= egl::PBUFFER_BIT;
        attrs.renderable_type |= egl::OPENGL_ES2_BIT;
        assert!(satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_missing_window_bit() {
        let mut attrs = full_match();
        attrs.surface_type = egl::PBUFFER_BIT;
        assert!(!satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_missing_gles3_bit() {
        let mut attrs = full_match();
        attrs.renderable_type = egl::OPENGL_ES2_BIT;
        assert!(!satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_non_8_bit_channel() {
        let mut attrs = full_match();
        attrs.red_size = 5;
        assert!(!satisfies_requirements(attrs));

        let mut attrs = full_match();
        attrs.alpha_size = 0;
        assert!(!satisfies_requirements(attrs));
    }
}
