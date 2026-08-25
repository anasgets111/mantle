pub mod egl;

use std::error::Error;
use std::ffi::c_void;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use khronos_egl::Surface as EglSurface;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_egl::WlEglSurface;

use crate::text::atlas::TextPainter;
use crate::text::shaping::{ShapeRequest, ShapingHandle};
use crate::text::snap::LogicalRect;

/// The three static surfaces from ADR-0007 / build-steps.md Phase 3, point 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceRole {
    MainBar,
    OverlayCanvas,
    WallpaperLayer,
}

impl SurfaceRole {
    fn label(self) -> &'static str {
        match self {
            SurfaceRole::MainBar => "main_bar",
            SurfaceRole::OverlayCanvas => "overlay_canvas",
            SurfaceRole::WallpaperLayer => "wallpaper_layer",
        }
    }
}

/// A live window surface bound to the shared EGL context, once its first configure
/// has arrived. Holds the native window alongside the EGL surface: per wayland-egl's
/// contract, `WlEglSurface` must outlive the EGL surface built from it -- fields are
/// declared in the order Rust drops them (top to bottom), so `egl_surface` goes first.
struct BoundSurface {
    #[allow(dead_code)]
    egl_surface: EglSurface,
    #[allow(dead_code)]
    native_window: WlEglSurface,
}

/// Logs an EGL/Wayland bind-time failure in a consistent shape across `bind_and_clear`'s
/// fallible steps.
fn log_bind_failure(role: SurfaceRole, stage: &str, err: impl std::fmt::Display) {
    eprintln!("[oblisk-renderer] {}: {stage} failed: {err}", role.label());
}

struct TrackedSurface {
    role: SurfaceRole,
    layer: LayerSurface,
    bound: Option<BoundSurface>,
}

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    egl: egl::EglState,
    gl: Option<glow::Context>,
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    let egl_state = egl::init(conn.backend().display_ptr() as *mut c_void)?;

    let mut app = App {
        registry_state,
        output_state,
        compositor_state,
        layer_shell,
        egl: egl_state,
        gl: None,
        shaping: ShapingHandle::spawn(),
        text_painter: None,
        surfaces: Vec::new(),
        exit: false,
    };

    // Outputs arrive as a burst of registry + wl_output events after binding; two
    // roundtrips is enough to have the full initial output list before we create
    // one wallpaper_layer surface per monitor.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    app.create_main_bar(&qh);
    app.create_overlay_canvas(&qh);
    app.create_wallpaper_layers(&qh);

    loop {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit {
            break;
        }
    }

    Ok(())
}

/// Parameters for [`App::spawn_layer`]; bundled so the helper stays under clippy's
/// argument-count limit while still taking each of the three surfaces' divergent bits.
struct LayerSpec<'a> {
    layer_type: Layer,
    name: &'a str,
    output: Option<&'a wl_output::WlOutput>,
    anchor: Anchor,
    size: (u32, u32),
    exclusive_zone: i32,
}

impl App {
    /// Creates and configures (but does not commit) a layer-shell surface. The three
    /// static surfaces share this skeleton; each call site handles its own divergent
    /// setup (overlay's input region, wallpaper's per-output loop) before committing.
    fn spawn_layer(&mut self, qh: &QueueHandle<App>, spec: LayerSpec) -> LayerSurface {
        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            spec.layer_type,
            Some(spec.name),
            spec.output,
        );
        layer.set_anchor(spec.anchor);
        layer.set_size(spec.size.0, spec.size.1);
        layer.set_exclusive_zone(spec.exclusive_zone);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer
    }

    fn create_main_bar(&mut self, qh: &QueueHandle<App>) {
        let layer = self.spawn_layer(
            qh,
            LayerSpec {
                layer_type: Layer::Top,
                name: "oblisk-main-bar",
                output: None,
                anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                size: (0, 32),
                exclusive_zone: 32,
            },
        );
        layer.commit();

        self.surfaces.push(TrackedSurface {
            role: SurfaceRole::MainBar,
            layer,
            bound: None,
        });
    }

    fn create_overlay_canvas(&mut self, qh: &QueueHandle<App>) {
        let layer = self.spawn_layer(
            qh,
            LayerSpec {
                layer_type: Layer::Overlay,
                name: "oblisk-overlay-canvas",
                output: None,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                size: (0, 0),
                exclusive_zone: 0,
            },
        );

        // build-steps.md Phase 3, point 4: commit an empty input region on boot so
        // clicks pass through to windows below until a Lua-authored overlay child
        // claims a bounding box (later phase). The region is destroyed immediately
        // after the request; wl_surface.set_input_region copies its contents.
        // overlay_canvas's entire purpose is this click-through guarantee, so a
        // failure here is as fatal as an EGL bind failure, not a silent no-op.
        match Region::new(&self.compositor_state) {
            Ok(region) => layer.set_input_region(Some(region.wl_region())),
            Err(e) => {
                log_bind_failure(SurfaceRole::OverlayCanvas, "wl_compositor::create_region", e);
                self.exit = true;
                return;
            }
        }

        layer.commit();

        self.surfaces.push(TrackedSurface {
            role: SurfaceRole::OverlayCanvas,
            layer,
            bound: None,
        });
    }

    fn create_wallpaper_layers(&mut self, qh: &QueueHandle<App>) {
        // ponytail: fixed two-roundtrip output snapshot (see run()), no dynamic
        // add/remove -- an output that appears after boot never gets a wallpaper_layer,
        // and a removed output's surface is never torn down. Upgrade path: wire
        // OutputHandler::new_output/output_destroyed to spawn/despawn wallpaper_layer
        // surfaces as outputs come and go instead of enumerating once here.
        for output in self.output_state.outputs().collect::<Vec<_>>() {
            let layer = self.spawn_layer(
                qh,
                LayerSpec {
                    layer_type: Layer::Background,
                    name: "oblisk-wallpaper",
                    output: Some(&output),
                    anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                    size: (0, 0),
                    exclusive_zone: 0,
                },
            );
            layer.commit();

            self.surfaces.push(TrackedSurface {
                role: SurfaceRole::WallpaperLayer,
                layer,
                bound: None,
            });
        }
    }

    /// First configure for a surface: bind its wl_egl_window to a real EGL window
    /// surface against the shared context, make it current, and prove the pipeline
    /// is live with one clear + swap. No draw loop -- that's Phase 4.
    fn bind_and_clear(&mut self, layer: &LayerSurface, width: u32, height: u32) {
        let Some(tracked) = self.surfaces.iter_mut().find(|s| &s.layer == layer) else {
            return;
        };

        let width = width.max(1) as i32;
        let height = height.max(1) as i32;

        if let Some(egl_surface) = tracked.bound.as_ref().map(|b| b.egl_surface) {
            // Repeat configure (e.g. a resize) on an already-bound surface. The
            // wl_egl_window/EGL surface were created once and don't need recreating,
            // but only main_bar has per-frame state (the FemtoVG canvas) that must
            // track the new size -- the other two surfaces have nothing left to do.
            if tracked.role != SurfaceRole::MainBar {
                return;
            }
            let role = tracked.role;

            // Another surface's own bind_and_clear may have made a different EGL
            // surface current on this thread since main_bar's last draw -- the
            // context is shared across all three surfaces, so it must be
            // re-established here rather than assumed still current.
            if let Err(e) = self.egl.instance.make_current(
                self.egl.display,
                Some(egl_surface),
                Some(egl_surface),
                Some(self.egl.context),
            ) {
                log_bind_failure(role, "eglMakeCurrent", e);
                self.exit = true;
                return;
            }

            if !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
                self.exit = true;
                return;
            }

            if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
                log_bind_failure(role, "eglSwapBuffers", e);
                self.exit = true;
                return;
            }

            return;
        }

        let native_window = match WlEglSurface::new(layer.wl_surface().id(), width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(tracked.role, "WlEglSurface::new", e);
                self.exit = true;
                return;
            }
        };

        let egl_surface = unsafe {
            self.egl.instance.create_window_surface(
                self.egl.display,
                self.egl.config,
                native_window.ptr() as *mut c_void,
                None,
            )
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(tracked.role, "eglCreateWindowSurface", e);
                self.exit = true;
                return;
            }
        };

        if let Err(e) = self.egl.instance.make_current(
            self.egl.display,
            Some(egl_surface),
            Some(egl_surface),
            Some(self.egl.context),
        ) {
            log_bind_failure(tracked.role, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        let gl = self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                self.egl
                    .instance
                    .get_proc_address(s)
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        unsafe {
            use glow::HasContext;
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Phase 4 integration proof, main_bar only: shape+draw one static string to
        // prove the cosmic-text/FemtoVG pipeline is live end to end. No draw loop or
        // Lua-driven content -- that's a future scene-graph phase.
        if tracked.role == SurfaceRole::MainBar {
            // Free function, not a `&mut self` method: `tracked` is still borrowed from
            // `self.surfaces` here, so this takes the three disjoint fields it actually
            // needs directly, rather than the whole `self` a method call would require.
            // Safe to build/use a FemtoVG `Canvas` here: dispatch is single-threaded, the
            // `eglMakeCurrent` a few lines above is the only context switch on this
            // thread, and this branch only ever runs for `main_bar`, so the context
            // that's current at this point is always the one `text_painter` was built
            // against -- no other surface's `bind_and_clear` can interleave here.
            if !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
                self.exit = true;
                return;
            }
        }

        if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
            log_bind_failure(tracked.role, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }

        eprintln!(
            "[oblisk-renderer] {} up: {width}x{height}, EGL context current, buffer cleared+swapped",
            tracked.role.label()
        );

        tracked.bound = Some(BoundSurface {
            egl_surface,
            native_window,
        });
    }
}

/// Phase 4 integration proof: shape a static string off-thread via cosmic-text, then
/// rasterize+draw it with FemtoVG, snapping its box to physical pixels. A free
/// function taking each field it needs directly (see the one call site in
/// `bind_and_clear`) rather than a `&mut self` method, so it doesn't need the whole
/// `App` borrowed while a `TrackedSurface` from `self.surfaces` is still live there.
/// Returns `false` on a FemtoVG init failure, so the caller can treat it exactly like
/// every other EGL/GL bind failure in this file (fatal, not logged-and-ignored).
fn draw_main_bar_proof_text(
    shaping: &ShapingHandle,
    egl: &egl::EglState,
    text_painter: &mut Option<TextPainter>,
    width: i32,
    height: i32,
) -> bool {
    const PROOF_TEXT: &str = "Oblisk";
    const FONT_SIZE: f32 = 14.0;

    let shaped = shaping.shape(ShapeRequest {
        text: PROOF_TEXT.into(),
        font_size: FONT_SIZE,
        line_height: FONT_SIZE * 1.2,
    });

    if text_painter.is_none() {
        let font_bytes = shaping.default_font_bytes();
        let painter = TextPainter::new(
            |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
            width as u32,
            height as u32,
            &font_bytes,
        );
        match painter {
            Ok(p) => *text_painter = Some(p),
            Err(e) => {
                log_bind_failure(SurfaceRole::MainBar, "FemtoVG init", e);
                return false;
            }
        }
    }

    if let Some(painter) = text_painter.as_mut() {
        // The surface can resize after the painter was first built; refresh the
        // canvas's viewport every frame rather than trusting the size from init.
        painter.resize(width as u32, height as u32);
        painter.draw_line(
            PROOF_TEXT,
            LogicalRect { x: 8.0, y: 0.0, width: shaped.width, height: shaped.height },
            FONT_SIZE,
            1.0,
        );
        eprintln!(
            "[oblisk-renderer] main_bar: shaped \"{PROOF_TEXT}\" to {}x{} (logical), drew+flushed via FemtoVG",
            shaped.width, shaped.height
        );
    }

    true
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        self.bind_and_clear(layer, width, height);
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);
