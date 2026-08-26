pub mod egl;

use std::error::Error;
use std::ffi::c_void;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
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
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;

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
    /// § 15's "surface_id": `role.label()` for `main_bar`/`overlay_canvas`,
    /// `"wallpaper_layer@{name}"` per wallpaper instance -- resolved once at creation time (see
    /// [`create_wallpaper_layers`](App::create_wallpaper_layers)), not recomputed later.
    surface_id: String,
    /// Set once this surface's null buffer has been committed (PBA candidate mode only, § 15.2
    /// points 2-3). Irrelevant, always `false`, outside candidate mode.
    null_buffered: bool,
    /// The most recent `configure` event's size, remembered so [`App::activate_draw`] has a real
    /// size to bind its EGL window surface to -- in candidate mode, the first configure doesn't
    /// bind EGL at all (see [`App::bind_and_clear`]), so this is the only place that size
    /// survives until `ActivateDraw` arrives.
    configured_size: (u32, u32),
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
    /// `OBLISK_PBA_CANDIDATE` is set (build-steps.md Phase 14, § 15.2) -- read once in [`run`]
    /// and stored here rather than re-reading the env var on every configure event.
    is_pba_candidate: bool,
    /// Set once [`App::maybe_send_ready_signal`] has sent `ReadySignal` -- a one-time signal,
    /// never resent even if a later spurious configure re-triggers the check.
    ready_signal_sent: bool,
    ready_tx: std::sync::mpsc::Sender<Vec<String>>,
    presented_tx: std::sync::mpsc::Sender<shared::PresentationEvidence>,
    presentation_time: PresentationTimeState,
    /// Cloned once in [`run`] so [`App::activate_draw`] (called from the poll loop, not a
    /// `Dispatch` callback) can still request `wp_presentation_feedback` -- `QueueHandle` is a
    /// cheap, `Clone`, reference-counted handle.
    queue_handle: QueueHandle<App>,
    /// The `ActivateDraw` nonce currently being drawn, if any -- tags every
    /// `wp_presentation_feedback` `presented` event reported while it's in flight. PBA only
    /// drives one handshake at a time (docs/adr/0025 item 5), so one field, not a per-surface
    /// map, is enough.
    active_nonce: Option<u64>,
}

pub fn run(
    ready_tx: std::sync::mpsc::Sender<Vec<String>>,
    presented_tx: std::sync::mpsc::Sender<shared::PresentationEvidence>,
    activate_rx: std::sync::mpsc::Receiver<u64>,
) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    // Stable protocol, no `staging`/`unstable` Cargo feature needed -- `PresentationTimeState::
    // bind` tolerates a compositor that doesn't advertise it (later `feedback()` calls fail with
    // `GlobalError::MissingGlobal` instead of failing this whole bind).
    let presentation_time = PresentationTimeState::bind(&globals, &qh);

    let egl_state = egl::init(conn.backend().display_ptr() as *mut c_void)?;

    let is_pba_candidate = std::env::var("OBLISK_PBA_CANDIDATE").is_ok();

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
        is_pba_candidate,
        ready_signal_sent: false,
        ready_tx,
        presented_tx,
        presentation_time,
        queue_handle: qh.clone(),
        active_nonce: None,
    };

    // Outputs arrive as a burst of registry + wl_output events after binding; two
    // roundtrips is enough to have the full initial output list before we create
    // one wallpaper_layer surface per monitor.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    app.create_main_bar(&qh);
    app.create_overlay_canvas(&qh);
    app.create_wallpaper_layers(&qh);

    // Replaces `event_queue.blocking_dispatch(&mut app)?` (used through Phase 13): a real
    // Wayland event might not arrive for a long time after `ActivateDraw` is sent, since nothing
    // else happens on these mostly-static surfaces once staged -- this loop also checks
    // `activate_rx` on a bounded latency instead of blocking indefinitely on the Wayland
    // connection's fd alone. The existing immediate-draw behavior on first configure (non-
    // candidate mode) is unaffected -- it still happens synchronously inside the `configure`
    // handler, which `dispatch_pending` still calls.
    loop {
        event_queue.dispatch_pending(&mut app)?;
        if app.exit {
            break;
        }
        if let Ok(nonce) = activate_rx.try_recv() {
            app.activate_draw(nonce);
            if app.exit {
                break;
            }
        }
        event_queue.flush()?;
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)];
            // 15ms: bounded latency for activate_rx, irrelevant next to PBA's second-scale
            // ready/evidence timeouts (supervisor/src/main.rs's `PBA_TIMINGS`).
            if matches!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(15u16)), Ok(n) if n > 0) {
                guard.read()?;
            }
            // guard drops here either way; if nothing was read, dispatch_pending above simply
            // finds nothing new next iteration.
        }
    }

    Ok(())
}

/// One `wallpaper_layer` instance's surface_id: `"wallpaper_layer@{name}"` when the compositor
/// reports a real output name, `"wallpaper_layer@output-{index}"` (a stable positional fallback)
/// when it doesn't. Pure so it's directly unit-testable -- `wayland/mod.rs` otherwise has no
/// test seam (Wayland-protocol-integration code with no headless test harness in this repo).
fn wallpaper_surface_id(name: Option<&str>, index: usize) -> String {
    match name {
        Some(name) => format!("wallpaper_layer@{name}"),
        None => format!("wallpaper_layer@output-{index}"),
    }
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
            surface_id: SurfaceRole::MainBar.label().to_string(),
            null_buffered: false,
            configured_size: (0, 0),
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
            surface_id: SurfaceRole::OverlayCanvas.label().to_string(),
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    fn create_wallpaper_layers(&mut self, qh: &QueueHandle<App>) {
        // ponytail: fixed two-roundtrip output snapshot (see run()), no dynamic
        // add/remove -- an output that appears after boot never gets a wallpaper_layer,
        // and a removed output's surface is never torn down. Upgrade path: wire
        // OutputHandler::new_output/output_destroyed to spawn/despawn wallpaper_layer
        // surfaces as outputs come and go instead of enumerating once here.
        for (index, output) in self.output_state.outputs().collect::<Vec<_>>().into_iter().enumerate() {
            let name = self.output_state.info(&output).and_then(|info| info.name);
            let surface_id = wallpaper_surface_id(name.as_deref(), index);
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
                surface_id,
                null_buffered: false,
                configured_size: (0, 0),
            });
        }
    }

    /// First configure for a surface: bind its wl_egl_window to a real EGL window
    /// surface against the shared context, make it current, and prove the pipeline
    /// is live with one clear + swap. No draw loop -- that's Phase 4.
    ///
    /// PBA candidate mode (`self.is_pba_candidate`, build-steps.md Phase 14, § 15.2 points 2-3)
    /// branches here instead: a first configure commits a null buffer directly on the raw
    /// `wl_surface` rather than binding EGL at all -- the Candidate stays invisible, occupying
    /// zero on-screen coordinates, until [`App::activate_draw`] does the real EGL bind later.
    /// Non-candidate mode (today's existing behavior) is entirely unaffected by this branch.
    fn bind_and_clear(&mut self, layer: &LayerSurface, width: u32, height: u32) {
        let Some(tracked) = self.surfaces.iter_mut().find(|s| &s.layer == layer) else {
            return;
        };

        if self.is_pba_candidate {
            tracked.configured_size = (width, height);
            if !tracked.null_buffered {
                // verified against wayland_client::protocol::wl_surface::WlSurface's generated
                // API: `attach(&self, buffer: Option<&wl_buffer::WlBuffer>, x: i32, y: i32)`,
                // `commit(&self)`.
                tracked.layer.wl_surface().attach(None, 0, 0);
                tracked.layer.wl_surface().commit();
                tracked.null_buffered = true;
            }
            self.maybe_send_ready_signal();
            return;
        }

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

    /// § 15.2 points 2-3: once every tracked surface has committed its null buffer, computes
    /// the full surface_id list (in `self.surfaces`' order) and sends it once via `ready_tx`.
    /// A no-op if it's already been sent, or if some surface hasn't staged yet -- called on
    /// every candidate-mode configure, since any of them might be the one that completes the
    /// set.
    fn maybe_send_ready_signal(&mut self) {
        if self.ready_signal_sent || !self.surfaces.iter().all(|s| s.null_buffered) {
            return;
        }
        self.ready_signal_sent = true;
        let surfaces = self.surfaces.iter().map(|s| s.surface_id.clone()).collect();
        if let Err(e) = self.ready_tx.send(surfaces) {
            eprintln!("[oblisk-renderer] failed to send ReadySignal to the socket thread: {e}");
        }
    }

    /// § 15.3: draws every tracked surface's first real frame in response to `ActivateDraw`,
    /// requesting `wp_presentation_feedback` for each. `nonce` is remembered as `active_nonce`
    /// so the later `presented` callback (this file's `PresentationTimeHandler` impl) knows
    /// which handshake attempt to tag its evidence with.
    fn activate_draw(&mut self, nonce: u64) {
        self.active_nonce = Some(nonce);
        for index in 0..self.surfaces.len() {
            self.activate_draw_one(index, nonce);
            if self.exit {
                return;
            }
        }
        // Promotion completes this process's PBA handshake -- from now on it behaves like an
        // ordinary (non-candidate) authoritative generation for the rest of its life, so a later
        // `configure` (resize, output change, a duplicate ack round trip -- all routine on a live
        // compositor) must fall through to `bind_and_clear`'s ordinary EGL-bind/resize path
        // instead of re-taking the null-buffer-staging branch forever (Correctness review: that
        // branch no-ops once `null_buffered` is already `true`, permanently disabling resize).
        // `activate_draw_one` already populated `tracked.bound` in the exact shape the
        // non-candidate path expects, so flipping this alone is enough -- no other state needs
        // adjusting.
        self.is_pba_candidate = false;
    }

    /// One tracked surface's `ActivateDraw` response: the same EGL-bind-and-clear (main_bar
    /// also draws the proof text) `bind_and_clear`'s non-candidate first-configure path does,
    /// plus a `wp_presentation_feedback` request placed immediately before `swap_buffers` so it
    /// associates with the commit `swap_buffers` performs. Indexes into `self.surfaces` rather
    /// than holding a `&mut TrackedSurface` across the whole body -- this needs `&mut self` for
    /// EGL/GL state and `self.text_painter` at several points, which a held borrow of one
    /// surface would conflict with (same reasoning `draw_main_bar_proof_text` already documents
    /// for the analogous first-configure path).
    fn activate_draw_one(&mut self, index: usize, nonce: u64) {
        let role = self.surfaces[index].role;
        let (width, height) = self.surfaces[index].configured_size;
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;

        let native_window = match WlEglSurface::new(self.surfaces[index].layer.wl_surface().id(), width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(role, "WlEglSurface::new", e);
                self.exit = true;
                return;
            }
        };

        let egl_surface = unsafe {
            self.egl.instance.create_window_surface(self.egl.display, self.egl.config, native_window.ptr() as *mut c_void, None)
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(role, "eglCreateWindowSurface", e);
                self.exit = true;
                return;
            }
        };

        if let Err(e) = self.egl.instance.make_current(self.egl.display, Some(egl_surface), Some(egl_surface), Some(self.egl.context)) {
            log_bind_failure(role, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        let gl = self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| self.egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void))
        });

        unsafe {
            use glow::HasContext;
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        if role == SurfaceRole::MainBar && !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
            self.exit = true;
            return;
        }

        // § 15.3 point 2: request presentation feedback before swap_buffers, so the request
        // associates with the commit swap_buffers performs -- confirmed against
        // `wayland-client-0.31.15`'s own client examples' placement convention; verify with
        // `WAYLAND_DEBUG=1` during a manual smoke test that `feedback` appears on the wire
        // before the corresponding `commit`.
        if let Err(e) = self.presentation_time.feedback(self.surfaces[index].layer.wl_surface(), &self.queue_handle) {
            // Not fatal to the whole candidate -- the Supervisor's evidence_timeout is what
            // catches a surface that never presents (docs/adr/0025 item 6); don't invent a
            // second failure-reporting path here.
            log_bind_failure(role, "wp_presentation::feedback", e);
        }

        if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
            log_bind_failure(role, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }

        eprintln!(
            "[oblisk-renderer] {} activated: {width}x{height}, presentation feedback requested (nonce={nonce})",
            self.surfaces[index].role.label()
        );

        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    /// § 15.3 point 4: the compositor confirmed `surface`'s committed frame physically hit the
    /// screen. Forwards a `shared::PresentationEvidence` to the socket thread.
    fn presented(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
        _outputs: Vec<wl_output::WlOutput>,
        _time: PresentTime,
        _refresh: u32,
        _seq: u64,
        _flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let Some(nonce) = self.active_nonce else {
            eprintln!("[oblisk-renderer] presented event arrived with no active ActivateDraw nonce; dropping");
            return;
        };
        let Some(surface_id) = self.surface_id_for(surface).map(str::to_string) else {
            eprintln!("[oblisk-renderer] presented event for an untracked surface; dropping");
            return;
        };
        if let Err(e) = self.presented_tx.send(shared::PresentationEvidence { nonce, surface_id }) {
            eprintln!("[oblisk-renderer] failed to send PresentationEvidence to the socket thread: {e}");
        }
    }

    /// The content update was never displayed. Logged only -- not a distinct fast-fail signal;
    /// the Supervisor's `evidence_timeout` is what catches this surface never presenting
    /// (docs/adr/0025 item 6). Deliberately does **not** send anything into `presented_tx`.
    fn discarded(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
    ) {
        let label = self.surface_id_for(surface).unwrap_or("<untracked surface>");
        eprintln!("[oblisk-renderer] presentation feedback discarded for {label}");
    }
}

impl App {
    /// Resolves a raw `wl_surface` (as handed back by a `wp_presentation_feedback` callback)
    /// to its `surface_id` -- shared by `presented`/`discarded`, which both used to inline this
    /// same lookup independently (Standards review).
    fn surface_id_for(&self, surface: &wl_surface::WlSurface) -> Option<&str> {
        self.surfaces.iter().find(|s| s.layer.wl_surface() == surface).map(|s| s.surface_id.as_str())
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
        max_width: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_surface_id_uses_the_real_output_name_when_present() {
        assert_eq!(wallpaper_surface_id(Some("DP-1"), 0), "wallpaper_layer@DP-1");
        assert_eq!(wallpaper_surface_id(Some("eDP-1"), 3), "wallpaper_layer@eDP-1");
    }

    #[test]
    fn wallpaper_surface_id_falls_back_to_a_stable_index_when_name_is_none() {
        assert_eq!(wallpaper_surface_id(None, 0), "wallpaper_layer@output-0");
        assert_eq!(wallpaper_surface_id(None, 2), "wallpaper_layer@output-2");
    }
}
