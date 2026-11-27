//! EGL binding and painting: bind a surface's `wl_egl_window`, paint its display list where the
//! back buffer lacks it, and schedule repaints.

use std::time::{Duration, Instant};

use shared::{debug, warn};

use super::*;

/// Timing breakdown of what [`App::paint_surface`] spent across phases.
#[derive(Clone, Copy, Default, Debug)]
pub struct RepaintSplit {
    pub build: Duration,
    pub gl: Duration,
    pub text: Duration,
    pub icons: Duration,
    pub boxes: Duration,
    pub paths: Duration,
    pub flush: Duration,
    pub swap: Duration,
}

impl std::ops::AddAssign for RepaintSplit {
    fn add_assign(&mut self, rhs: Self) {
        self.build += rhs.build;
        self.gl += rhs.gl;
        self.text += rhs.text;
        self.icons += rhs.icons;
        self.boxes += rhs.boxes;
        self.paths += rhs.paths;
        self.flush += rhs.flush;
        self.swap += rhs.swap;
    }
}

/// The pixels a back buffer `age` frames old lacks (ADR-0258): `frame`, this paint's damage, and
/// the `age - 1` frames before it. `None` repaints the whole surface.
fn repaint_bounds(
    age: usize,
    frame: Option<&[PhysicalRect]>,
    history: &[Option<Vec<PhysicalRect>>],
) -> Option<Vec<PhysicalRect>> {
    let older = history.get(..age.checked_sub(1)?)?.iter().map(Option::as_deref);
    std::iter::once(frame).chain(older).try_fold(Vec::new(), |mut rects, frame| {
        rects.extend_from_slice(frame?);
        Some(rects)
    })
}

/// When this surface next owes a paint (ADR-0233). A paint it did not owe may have skipped an
/// animated image outside its region, so the deadline that image left stands.
fn next_stale(owed: bool, stale: Option<Instant>, deferred: Option<Instant>) -> Option<Instant> {
    if owed { deferred } else { stale.into_iter().chain(deferred).min() }
}

/// What a drawn surface's swap still needs (#116).
pub(super) struct Drawn {
    egl_surface: EglSurface,
    paint_key: PaintKey,
    list: layout::paint::DisplayList,
    damage: Option<Vec<PhysicalRect>>,
    owed: bool,
    deferred: Option<Instant>,
    animating: bool,
}

/// EGL's bottom-left `x, y, w, h` quadruples for `rects` on a surface `height` tall.
fn egl_rects(rects: &[PhysicalRect], height: u32) -> Vec<i32> {
    rects.iter().flat_map(|r| [r.x0, height as i32 - r.y1, r.x1 - r.x0, r.y1 - r.y0]).collect()
}

/// If `list` is a single borderless, unrounded solid colour fill covering `surface` and `(width,
/// height)` in buffer pixels, returns its colour for the single-pixel Wayland buffer path (#191).
fn solid_color(
    list: &layout::paint::DisplayList,
    surface: PhysicalRect,
    (width, height): (f32, f32),
) -> Option<node::Rgba> {
    use layout::paint::{Draw, DrawCmd};
    let [DrawCmd { rect, clip, draw: Draw::Box { background, radius, widths, .. } }] = list.commands.as_slice() else {
        return None;
    };
    let [node::Fill::Color(color)] = background.as_slice() else { return None };
    (surface.x0 == 0
        && surface.y0 == 0
        && clip.contains(surface)
        && rect.x <= 0.0
        && rect.y <= 0.0
        && rect.x + rect.width >= width
        && rect.y + rect.height >= height
        && radius.is_zero()
        && *widths == node::EdgeInsets::default())
    .then_some(*color)
}

/// Premultiplied `u32` RGBA channels for `wp_single_pixel_buffer_manager_v1.create_u32_rgba_buffer`.
fn single_pixel_rgba(color: node::Rgba) -> (u32, u32, u32, u32) {
    let channel = |value: f32| (f64::from(value.clamp(0.0, 1.0)) * f64::from(u32::MAX)).round() as u32;
    let a = color.a.clamp(0.0, 1.0);
    let alpha = channel(a);
    let premul = |value: f32| channel(value.clamp(0.0, 1.0) * a).min(alpha);
    (premul(color.r), premul(color.g), premul(color.b), alpha)
}

impl App {
    /// Presents a static solid background panel with a 1×1 compositor-scaled buffer instead of
    /// binding EGL (#191). Returns `false` when the compositor lacks single-pixel or viewport
    /// support, letting the caller fall back to EGL.
    fn present_solid(&self, index: usize, logical: (u32, u32), color: node::Rgba) -> bool {
        let Some(buffers) = self.scale_globals.as_ref().and_then(|globals| globals.solid_buffers.as_ref()) else {
            return false;
        };
        let tracked = &self.surfaces[index];
        if !tracked.scale.fractional() || logical.0 == 0 || logical.1 == 0 {
            return false;
        }
        let Some(surface) = tracked.role.wl_surface() else { return false };
        let (r, g, b, a) = single_pixel_rgba(color);
        let buffer = buffers.create_u32_rgba_buffer(r, g, b, a, &self.queue_handle, ());
        tracked.scale.set_destination(logical);
        surface.attach(Some(&buffer), 0, 0);
        let (w, h) = (logical.0.min(i32::MAX as u32) as i32, logical.1.min(i32::MAX as u32) as i32);
        surface.damage(0, 0, w, h);
        surface.commit();
        buffer.destroy();
        debug!("{}: presented a single-pixel buffer at {}x{}", tracked.surface_id, logical.0, logical.1);
        true
    }

    /// Frees rendering, EGL, and `wl_egl_window`, leaving the role object untouched. The caller
    /// owns its later drop; hidden windows keep their tracking entry (ADR-0049 decision 1).
    pub(super) fn release_bound(&mut self, index: usize) {
        let Some(bound) = self.surfaces[index].bound.take() else {
            return;
        };
        // `khronos_egl::Surface` has no `Drop`; destroy it before `wl_egl_window`, or each
        // unplugged monitor/closed window leaks an EGL surface. `ensure_bound` creates both, but
        // keep the guard so a mismatch leaks rather than panics.
        if let Some(egl) = self.egl.as_ref() {
            // Unbind first. egl-wayland2 frees a destroyed surface even while current, the next
            // `eglCreateWindowSurface` can get the same handle back, and `eglMakeCurrent` then keeps
            // the dead one bound: the next swap failed with EGL_BAD_SURFACE and quit the Renderer.
            // Every paint makes its own surface current again.
            // The painter's images need a current context to delete; this surface is still alive.
            if let Some(painter) = self.text_painter.as_mut()
                && egl.make_current(Some(bound.egl_surface), Some(egl.context)).is_ok()
            {
                painter.release_surface(&self.surfaces[index].surface_id);
            }
            let _ = egl.make_current(None, None);
            self.current_egl_surface = None;
            if let Err(err) = egl.destroy_surface(bound.egl_surface) {
                log_bind_failure(&self.surfaces[index].surface_id, "eglDestroySurface", err);
            }
        }
        // `BoundSurface`'s drop sends `wl_egl_window_destroy`.
        drop(bound);
        self.trim.request(std::time::Instant::now());
    }

    /// Lazily builds the process-wide EGL state on the first drawable surface (ADR-0071). Failure
    /// is fatal: the Renderer exits and the Supervisor respawns it (ADR-0058, ADR-0071 decision 3).
    fn ensure_egl(&mut self, surface_id: &str) {
        if self.egl.is_some() {
            return;
        }
        match egl::init(&self.conn) {
            Ok(state) => self.egl = Some(state),
            Err(err) => {
                log_bind_failure(surface_id, "egl::init", err);
                self.exit = true;
            }
        }
    }

    /// Creates the surface's `wl_egl_window`/EGL surface against shared context, initializing EGL
    /// and `glow` on their first use. Failure is fatal (`self.exit`).
    pub(super) fn ensure_bound(&mut self, index: usize) -> bool {
        if self.surfaces[index].bound.is_some() {
            return true;
        }
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].scale.physical_size(self.surfaces[index].configured_size);
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;
        let Some(surface_object_id) = self.surfaces[index].role.wl_surface().map(Proxy::id) else {
            // The window was hidden between requesting and performing the bind; there is nothing
            // to bind, and the map-state guard already stopped painting.
            return false;
        };

        // Only the first drawable surface pays for Mesa (ADR-0071); hidden windows took the cheap
        // bails above.
        self.ensure_egl(&surface_id);
        let Some(egl) = self.egl.as_ref() else { return false };

        let native_window = match WlEglSurface::new(surface_object_id, width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(&surface_id, "WlEglSurface::new", e);
                self.exit = true;
                return false;
            }
        };

        // SAFETY: `native_window.ptr()` is the live `wl_egl_window*` just built for this EGL
        // display/config, exactly what `eglCreateWindowSurface` requires.
        let egl_surface = unsafe { egl.create_window_surface(native_window.ptr() as *mut c_void) };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(&surface_id, "eglCreateWindowSurface", e);
                self.exit = true;
                return false;
            }
        };

        if let Err(e) = egl.make_current(Some(egl_surface), Some(egl.context)) {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            // `native_window` drops on this return. `khronos_egl::Surface` has no `Drop`, so
            // without this the driver keeps a surface bound to a freed `wl_egl_window`.
            if let Err(err) = egl.destroy_surface(egl_surface) {
                log_bind_failure(&surface_id, "eglDestroySurface", err);
            }
            self.exit = true;
            return false;
        }
        self.current_egl_surface = Some(egl_surface);

        // Request non-blocking swaps on each newly current surface. EGL defaults to 1, which would
        // stall this same thread's Wayland dispatch, Supervisor reads, and input. Today the loop
        // paints only on dirty pushes, so pacing is unnecessary. Measured cost was 0.24-0.89 ms
        // per swap across five swaps in 25 s; it matters when ADR-0130 adds per-frame animation.
        // Failure is non-fatal and leaves EGL's current blocking default.
        if let Err(e) = egl.swap_interval(0) {
            warn!("{surface_id}: eglSwapInterval(0) failed ({e}); swaps on this surface keep EGL's blocking default");
        }

        // SAFETY: `eglMakeCurrent` directly above binds the loader's context on this single
        // dispatch thread, with no intervening context switch.
        self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        debug!("{surface_id} up: {width}x{height}, EGL context current");
        self.surfaces[index].bound =
            Some(BoundSurface { egl_surface, native_window, size: (width as u32, height as u32) });
        // A new EGL surface has empty buffers, so the next paint is unconditional.
        self.surfaces[index].last_painted = None;
        true
    }

    /// Paint a bound surface's display list where its back buffer lacks it (ADR-0258), and swap. One
    /// `TextPainter` and EGL context serve all surfaces: GL objects stay valid across framebuffers,
    /// viewport size is per surface, and one canvas per surface is the fallback if that proves wrong.
    /// An absent tree still clears/swaps, or the compositor keeps the last frame.
    pub(super) fn paint_surface(&mut self, index: usize) {
        if let Some(drawn) = self.draw_surface(index, false) {
            self.swap_surface(index, drawn);
            self.trim_images();
        }
    }

    /// Draws a surface's display list into its back buffer; `None` when nothing is left to swap.
    /// `batched` swaps later, after other surfaces drew (#116).
    fn draw_surface(&mut self, index: usize, batched: bool) -> Option<Drawn> {
        // Unmapped or pre-configure surfaces cannot attach a buffer; `swap_buffers` is both attach
        // and commit, so this prevents `visible = false` from remapping (ADR-0038 decision 2).
        if self.surfaces[index].map_state != MapState::Mapped {
            return None;
        }
        let surface_id = self.surfaces[index].surface_id.clone();
        let logical = self.surfaces[index].configured_size;
        let (width, height) = self.surfaces[index].scale.physical_size(logical);
        let scale = self.surfaces[index].scale.factor_120() as f32 / 120.0;
        let paint_key = PaintKey {
            physical: (width, height),
            scale_120: self.surfaces[index].scale.factor_120(),
            logical,
            fonts: self.shaping.font_generation(),
        };

        // Whatever this paint commits answers a deferred repaint, and it asks for its own callback.
        self.surfaces[index].paint_deferred = false;
        self.surfaces[index].frame_pending = false;
        let mut animating = self.frames_wanted(index, &surface_id);
        let tree = self.client.scene().surface(&surface_id);

        let control = self.outline_control(&surface_id);
        let is_clean = !self.surfaces[index].dirty && self.field_focus_for(&surface_id).is_none() && control.is_none();
        if is_clean
            && !self.surfaces[index].owes_a_paint()
            && self.surfaces[index].last_painted.as_ref().is_some_and(|(s, _)| *s == paint_key)
        {
            if animating && let Some(surface) = self.surfaces[index].request_frame(&self.queue_handle) {
                surface.commit();
            }
            return None;
        }

        // Build before GL work so an unchanged surface costs one tree walk, not make-current,
        // clear, draw calls, and swap. This stops a 1920x1200 wallpaper redrawing every second
        // because the clock's seconds digit advanced (ADR-0044 decision 2's global dirty flag).
        // An absent tree becomes an empty list and still reaches clear/swap to erase old contents.
        // End the immutable field-focus borrow before mutably borrowing the painter; `Draw::Text`
        // owns its string.
        let timing = crate::layout::scene::timing_on();
        let t_build = timing.then(Instant::now);
        let list = {
            // The focused field first, so it beats its own parked entry.
            let parked = self.parked_drafts.iter().filter(|((surface, _), _)| *surface == surface_id).map(
                |((_, id), (text, _))| layout::paint::FieldFocus::Plain { id: *id, text, caret: None, caret_on: false },
            );
            let focus: Vec<_> = self.field_focus_for(&surface_id).into_iter().chain(parked).collect();
            tree.as_ref()
                .map(|tree| layout::paint::build_with_control(tree, scale, &focus, control, &self.shaping))
                .unwrap_or_default()
        };
        // What the compositor re-blurs and recomposites behind this surface; `None` is the whole
        // surface (ADR-0063 amendment).
        let owed = self.surfaces[index].owes_a_paint();
        let surface_rect = PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 };
        let damage = match &self.surfaces[index].last_painted {
            Some((size, painted)) if *size == paint_key => Some(
                list.damage_since(painted, owed)
                    .into_iter()
                    .map(|rect| rect.intersect(surface_rect))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let damage = damage.map(layout::paint::coalesce);
        if let Some(t_build) = t_build {
            self.repaint_split.build += t_build.elapsed();
        }
        if damage.as_ref().is_some_and(Vec::is_empty) {
            // An unchanged list, or a change with nothing in view (ADR-0258). A mid-tween surface
            // still has to commit: a frame callback is only answered after one, and a tween whose
            // tick moved nothing visible would otherwise never get its next (ADR-0145). What it
            // does not have to do is draw the same pixels again. A commit with no new buffer
            // re-commits the state the surface already has, which is what makes the frame request
            // below effective, so a hold, a lead-in `delay`, or a step easing sitting on one value
            // costs a commit instead of make-current, clear, every draw call, and a swap.
            let tracked = &mut self.surfaces[index];
            tracked.dirty = false;
            tracked.stale = next_stale(owed, tracked.stale, None);
            tracked.last_painted = Some((paint_key, list));
            // An owed paint with no tween still commits, so double-buffered state staged for it (the
            // blur region) lands; it asks for no callback, so idle stays idle.
            let surface = if animating {
                tracked.request_frame(&self.queue_handle)
            } else if owed {
                tracked.role.wl_surface()
            } else {
                None
            };
            if let Some(surface) = surface {
                surface.commit();
            }
            return None;
        }

        let tracked = &self.surfaces[index];
        // ponytail: once a solid background panel promotes to EGL (e.g. a colour tween or added
        // child), it stays on EGL until remapped; upgrade by releasing EGL on return to a static fill.
        if !animating
            && tracked.bound.is_none()
            && matches!(&tracked.role, TrackedRole::Panel { spec, .. } if spec.topology.layer == node::LayerKind::Background)
            && let Some(color) = solid_color(&list, surface_rect, (logical.0 as f32 * scale, logical.1 as f32 * scale))
            && self.present_solid(index, logical, color)
        {
            let tracked = &mut self.surfaces[index];
            tracked.last_painted = Some((paint_key, list));
            tracked.stale = next_stale(owed, tracked.stale, None);
            tracked.dirty = false;
            self.surfaces_drawn += 1;
            return None;
        }
        // A single-pixel background panel can acquire other content without remapping; EGL has no prior frame.
        let damage = if tracked.bound.is_none() { None } else { damage };
        let t_gl = timing.then(Instant::now);
        if !self.ensure_bound(index) {
            return None;
        }
        let bound = self.surfaces[index].bound.as_mut()?;
        if bound.size != (width, height) {
            bound.native_window.resize(width as i32, height as i32, 0, 0);
            bound.size = (width, height);
        }
        let egl_surface = bound.egl_surface;
        self.place_desktop_reads(index, &list);
        if !self.make_surface_current(egl_surface, &surface_id) {
            return None;
        }
        let egl = self.egl.as_ref()?;

        // 0, unknown, where the driver has no buffer age.
        let age = egl.query_surface(egl_surface, super::egl_ext::EGL_BUFFER_AGE_EXT).unwrap_or(0);
        let history = &self.surfaces[index].damage_history;
        let regions = match repaint_bounds(usize::try_from(age).unwrap_or(0), damage.as_deref(), history) {
            Some(rects) => {
                let grown = rects.into_iter().map(|rect| list.repaint_region(rect).intersect(surface_rect));
                layout::paint::coalesce(grown.collect())
            }
            None => vec![surface_rect],
        };
        // ponytail: a batched surface skips partial update, since nothing promises its region
        // survives other surfaces becoming current before its swap; buffer age alone stays correct.
        // Upgrade: set it on batched surfaces too once Mesa and NVIDIA are shown to keep it.
        if !batched && let Some(set_damage_region) = egl.set_damage_region {
            let rects = egl_rects(&regions, height);
            // SAFETY: `egl_surface` is current on `egl.display` and its age was queried this frame,
            // as `EGL_KHR_partial_update` requires; `rects` holds `regions.len()` whole quadruples.
            unsafe {
                set_damage_region(egl.display.as_ptr(), egl_surface.as_ptr(), rects.as_ptr(), rects.len() as i32 / 4)
            };
        }

        // Always `Some` here: `ensure_bound` sets `gl` before it marks the surface `bound`.
        if self.text_painter.is_none()
            && let Some(gl) = self.gl.as_ref()
        {
            match TextPainter::new(
                |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
                gl,
                width,
                height,
                self.shaping.clone(),
            ) {
                Ok(painter) => self.text_painter = Some(painter),
                Err(e) => {
                    log_bind_failure(&surface_id, "FemtoVG init", e);
                    self.exit = true;
                    return None;
                }
            }
        }

        if let Some(painter) = self.text_painter.as_mut() {
            painter.resize(width, height);
            // A family a node named for the first time was resolved on the shaping worker while
            // this list was being measured; femtovg has to be given those faces before the list
            // that names them is drawn (ADR-0144). One atomic load on the frames where nothing
            // changed, which is all of them after startup.
            painter.sync();
        }
        if let Some(t_gl) = t_gl {
            self.repaint_split.gl += t_gl.elapsed();
        }

        if let Some(painter) = self.text_painter.as_mut() {
            // The context is current from `make_current` above: this is the one point a dma-buf
            // texture may be imported (ADR-0039, ADR-0248 amendment), and it must run before
            // `execute` reads `capture_cache` for this paint.
            super::capture::import_ready_dmabufs(
                &mut self.captures,
                &mut self.capture_cache,
                self.egl.as_ref(),
                self.gl.as_ref(),
                painter.canvas_mut(),
            );
            // The context is current from `make_current` above, so a config shader can take a
            // cross this frame; without one every cross falls back to the dissolve (ADR-0184).
            let shaders = self.gl.as_ref().map(|gl| layout::paint::Shaders { gl, stage: &mut self.shader_stage });
            let (drawn, split) = layout::paint::execute(
                &surface_id,
                painter,
                &mut self.image_cache,
                &mut self.capture_cache,
                &list,
                scale,
                (width as f32, height as f32),
                &regions,
                shaders,
            );
            self.repaint_split.text += split.text;
            self.repaint_split.icons += split.icons;
            self.repaint_split.boxes += split.boxes;
            self.repaint_split.paths += split.paths;
            self.repaint_split.flush += split.flush;
            // After the draws that answered it, before the swap: the tree this reads is the one
            // the next build walks, so a `retain` cover ends and a `transition` starts on the
            // frame paint proved the texture exists (ADR-0183).
            if !drawn.is_empty() {
                self.client.note_drawn_images(&surface_id, &drawn, std::time::Instant::now());
                // Re-read: a dissolve that started in this very paint was not running when
                // `animating` was taken above, and the frame callback below is the only thing that
                // will ever advance it. Missing this is the tween-gate mistake again: motion
                // begun where nothing was looking for it (ADR-0183).
                animating |= self
                    .client
                    .scene()
                    .surface(&surface_id)
                    .is_some_and(|tree| tree.needs_frames(std::time::Instant::now(), &mut None));
            }
        }
        self.captures.read_presented(self.gl.as_ref(), &list, surface_rect);
        // Straight after this surface's `execute` and before any other paint runs, which is what
        // scopes a cache-wide flag to the surface that earned it (ADR-0185).
        let deferred = self.image_cache.take_deferred();
        Some(Drawn { egl_surface, paint_key, list, damage, owed, deferred, animating })
    }

    /// Swaps what [`App::draw_surface`] drew, which attaches and commits it.
    fn swap_surface(
        &mut self,
        index: usize,
        Drawn { egl_surface, paint_key, list, damage, owed, deferred, animating }: Drawn,
    ) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let (logical, height) = (paint_key.logical, paint_key.physical.1);
        // A batch drew other surfaces since; EGL swaps only the current draw surface.
        if !self.make_surface_current(egl_surface, &surface_id) {
            return;
        }
        let Some(egl) = self.egl.as_ref() else { return };
        let timing = crate::layout::scene::timing_on();

        // Before the swap, which is the commit it has to precede. Requested only while a tween is
        // running, so an idle shell arms nothing and the loop's timeout-free poll stays that way
        // (ADR-0124, ADR-0130 decision 3).
        if animating {
            self.surfaces[index].request_frame(&self.queue_handle);
        }
        if let Some(surface) = self.surfaces[index].role.wl_surface() {
            self.surfaces[index].scale.set_destination(logical);
            if !self.surfaces[index].scale.fractional() && surface.version() >= 3 {
                surface.set_buffer_scale(self.surfaces[index].scale.buffer_scale());
            }
        }
        use khronos_egl::api::EGL1_0;
        let t_swap = timing.then(Instant::now);
        let swapped = match (egl.swap_with_damage, &damage) {
            (Some(swap), Some(rects)) => {
                let rects = egl_rects(rects, height);
                // SAFETY: `egl_surface` was made current on `egl.display` above; `rects` holds the
                // `n_rects` whole quadruples it promises.
                unsafe { swap(egl.display.as_ptr(), egl_surface.as_ptr(), rects.as_ptr(), (rects.len() / 4) as i32) }
            }
            // SAFETY: `egl_surface` was made current on `egl.display` above.
            _ => unsafe { khronos_egl::Static.eglSwapBuffers(egl.display.as_ptr(), egl_surface.as_ptr()) },
        };
        if let Err(error) = egl::egl_result(swapped) {
            log_bind_failure(&surface_id, "eglSwapBuffers", error);
            self.exit = true;
            return;
        }
        if let Some(t_swap) = t_swap {
            self.repaint_split.swap += t_swap.elapsed();
        }
        // Record only after swap; otherwise an unpresented frame could make the next identical list
        // skip the paint the screen never received.
        self.surfaces[index].last_painted = Some((paint_key, list));
        let history = &mut self.surfaces[index].damage_history;
        history.insert(0, damage);
        // ponytail: an age past 4 repaints whole; Mesa's Wayland platform cycles at most 4 buffers.
        // Upgrade path: keep history to the largest age seen.
        history.truncate(3);
        // A request turned away for pool capacity recorded no slot, so asking again is the whole
        // retry, and only a repaint asks. Staying `stale` is what stops the next turn skipping
        // this surface on an unchanged list, and `repaint_mapped_surfaces_where` is what stops a
        // narrowed repaint passing it over (ADR-0185).
        self.surfaces[index].stale = next_stale(owed, self.surfaces[index].stale, deferred);
        self.surfaces[index].dirty = false;
        self.surfaces_drawn += 1;
    }

    /// Makes `egl_surface` current unless it already is; a failure is fatal (`self.exit`).
    fn make_surface_current(&mut self, egl_surface: EglSurface, surface_id: &str) -> bool {
        let Some(egl) = self.egl.as_ref() else { return false };
        if self.current_egl_surface != Some(egl_surface) {
            if let Err(e) = egl.make_current(Some(egl_surface), Some(egl.context)) {
                log_bind_failure(surface_id, "eglMakeCurrent", e);
                self.exit = true;
                return false;
            }
            self.current_egl_surface = Some(egl_surface);
        }
        true
    }

    fn trim_images(&mut self) {
        // Images absent from every current list are idle (ADR-0123); queue eviction for the next
        // paint.
        let surfaces = &self.surfaces;
        self.image_cache.trim(|| {
            let mut pinned = Vec::new();
            for surface in surfaces {
                if let Some((_, list)) = &surface.last_painted {
                    list.image_requests(&mut pinned);
                }
            }
            pinned
        });
    }

    /// Marks stale every surface whose last-painted list matches `stale_because`. A decoded image
    /// invalidates any list that draws its file (ADR-0122), making the next repaint upload the new
    /// texture. A landed capture frame changes no `Draw::Capture` field (ADR-0248): `list` still
    /// names the right node, the texture behind it is just new.
    pub(in crate::wayland) fn mark_surfaces_stale_where(
        &mut self,
        stale_because: impl Fn(&layout::paint::DisplayList) -> bool,
    ) {
        for surface in &mut self.surfaces {
            if surface.last_painted.as_ref().is_some_and(|(_, list)| stale_because(list)) {
                // Marked, not cleared: this surface still shows the old texture until it
                // repaints, so its list has to keep pinning it (ADR-0182).
                surface.mark_stale();
            }
        }
    }

    /// The surfaces this turn's pass, tick or keystroke named, plus any surface already `stale`.
    ///
    /// A tick changes only the trees it names, so the others would each build a display list and
    /// have it rejected as equal to the one they last painted. That build is not free: a text draw
    /// copies its content and style runs, an image or icon its name.
    pub(in crate::wayland) fn repaint_surfaces_named(&mut self, named: &[&[String]]) {
        self.repaint_mapped_surfaces_where(|s| turn::narrowed_repaint_covers(named, &s.surface_id, s.due_a_repaint()));
    }

    /// Whether `surface_id`'s tree needs frame callbacks; a tree that only holds arms its wake instead.
    fn frames_wanted(&mut self, index: usize, surface_id: &str) -> bool {
        let mut hold = None;
        let now = std::time::Instant::now();
        let wanted = self.client.scene().surface(surface_id).is_some_and(|tree| tree.needs_frames(now, &mut hold));
        self.surfaces[index].hold_until = if wanted { None } else { hold };
        wanted
    }

    /// When the earliest holding tween moves again, so the poll loop sleeps through a hold.
    pub(in crate::wayland) fn next_hold_deadline(&self) -> Option<std::time::Instant> {
        self.surfaces.iter().filter(|s| s.map_state == MapState::Mapped).filter_map(|s| s.hold_until).min()
    }

    /// Answers a hold that ended as the frame callback would have: the surface ticks this turn.
    pub(in crate::wayland) fn wake_holds(&mut self, now: std::time::Instant) {
        for tracked in self.surfaces.iter_mut().filter(|s| s.hold_until.is_some_and(|at| at <= now)) {
            tracked.hold_until = None;
            if !self.animation_frames_due.contains(&tracked.surface_id) {
                self.animation_frames_due.push(tracked.surface_id.clone());
            }
        }
    }

    pub(in crate::wayland) fn take_repaint_split(&mut self) -> RepaintSplit {
        std::mem::take(&mut self.repaint_split)
    }

    /// Whether any mapped surface owes a repaint its tree cannot ask for. The main loop's repaint
    /// selection needs this: with nothing ticked, nothing typed and nothing landed, it would
    /// otherwise reach no repaint at all and a deferred decode would never be asked for again.
    pub(in crate::wayland) fn has_stale_surfaces(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.due_a_repaint() && surface.map_state == MapState::Mapped)
    }

    /// When the earliest owed repaint comes due, for the poll loop's only timeout. An animation's next
    /// frame is owed by no tree, no signal and no frame callback, so nothing else would wake for
    /// it (ADR-0233).
    pub(in crate::wayland) fn next_stale_deadline(&self) -> Option<std::time::Instant> {
        // Mapped and not awaiting a frame callback: a pending callback defers painting until the
        // compositor answers it, so a deadline in the past would spin the poll loop instead of
        // arming one wake. A surface hidden behind a session lock gets none until unlock.
        self.surfaces
            .iter()
            .filter(|s| s.map_state == MapState::Mapped && !s.frame_pending)
            .filter_map(|s| s.stale)
            .min()
    }

    /// Repaint every wanted mapped surface after a changed scene because ADR-0044 decision 2 has
    /// one global dirty flag. `paint_surface` skips unchanged lists, so protocol-only updates need
    /// their own commit in [`App::apply_spec_change`].
    pub(in crate::wayland) fn repaint_mapped_surfaces_where(&mut self, wanted: impl Fn(&TrackedSurface) -> bool) {
        let drawn = self.surfaces_drawn;
        let mut batch = Vec::new();
        for index in 0..self.surfaces.len() {
            let removed =
                self.reload_removals.as_ref().is_some_and(|ids| ids.contains(&self.surfaces[index].surface_id));
            if self.surfaces[index].map_state != MapState::Mapped || !wanted(&self.surfaces[index]) || removed {
                continue;
            }
            // Painted at the callback instead; the compositor still shows this surface's last frame.
            if self.surfaces[index].frame_pending {
                self.surfaces[index].paint_deferred = true;
                continue;
            }
            if self.reload_removals.is_some() {
                batch.extend(self.draw_surface(index, true).map(|drawn| (index, drawn)));
            } else {
                self.paint_surface(index);
            }
            if self.exit {
                return;
            }
        }
        // A reload's surfaces swap back to back, so their commits leave together (#116).
        let batched = !batch.is_empty();
        for (index, drawn) in batch {
            self.swap_surface(index, drawn);
            if self.exit {
                return;
            }
        }
        if batched {
            self.trim_images();
        }
        // Once per repaint rather than per surface: each call walks every surface's list.
        if self.surfaces_drawn != drawn {
            self.sync_captures();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #116. A reload's removed surface is neither painted nor destroyed until the turn's swaps are
    /// done. The mock output has no size, so the reload removes every panel; the window survives.
    #[test]
    fn a_reload_destroys_its_removed_surfaces_after_the_turns_repaint() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r##"return { panel { id = "gone", layer = "top", output = "TEST", width = 100, height = 32,
                child = rect { width = "fill", height = "fill", background = "#ff0000ff" } },
                window { id = "win", child = rect { width = 10, height = 10 } } }"##,
        );
        assert_eq!(app.surfaces[0].surface_id, "gone@TEST");
        // Both await a frame callback, so a repaint that reaches one defers it.
        for surface in &mut app.surfaces {
            surface.map_state = MapState::Mapped;
            surface.frame_pending = true;
        }
        let wl = app.surfaces[0].role.wl_surface().unwrap().id().protocol_id();
        let destroyed = || mock.requests.lock().unwrap().contains(&(wl, 0, 0));
        assert!(app.client.reevaluate());
        let qh = app.queue_handle.clone();
        app.apply_pending(&qh, None);
        app.repaint_mapped_surfaces_where(|_| true);
        queue.roundtrip(&mut app).unwrap();
        assert!(!app.surfaces[0].paint_deferred, "the removed surface was painted");
        assert!(app.surfaces[1].paint_deferred, "the survivor was not reached");
        assert!(!destroyed(), "destroyed before the turn's swaps");
        app.finish_reload();
        queue.roundtrip(&mut app).unwrap();
        assert!(destroyed() && app.surfaces.len() == 1);
    }

    /// ADR-0258. An animation's next-frame deadline outlives a paint that did not owe it: that paint's
    /// region may have skipped the animation, which then deferred nothing. A paint it owed draws it,
    /// so what that paint deferred replaces the deadline.
    #[test]
    fn a_paint_that_owed_no_texture_keeps_the_gifs_deadline() {
        let now = Instant::now();
        let (gif, sooner) = (now + Duration::from_millis(100), now + Duration::from_millis(10));
        assert_eq!(next_stale(false, Some(gif), None), Some(gif), "a shader elsewhere repainted");
        assert_eq!(next_stale(false, Some(gif), Some(sooner)), Some(sooner));
        assert_eq!(next_stale(true, Some(now), Some(gif)), Some(gif), "the GIF drew and deferred its next");
        assert_eq!(next_stale(true, Some(now), None), None);
    }

    /// ADR-0258. A back buffer `age` frames old lacks this frame's damage and the `age - 1` before
    /// it. An unknown age, a history too short, or a whole-surface frame in that window repaints all.
    #[test]
    fn a_repaint_covers_the_damage_since_the_back_buffer_was_drawn() {
        let rect = |x0| PhysicalRect { x0, y0: 0, x1: x0 + 10, y1: 10 };
        let history = [Some(vec![rect(20)]), Some(vec![rect(40), rect(60)]), None];
        let frame = Some(&[rect(0)][..]);
        assert_eq!(repaint_bounds(1, frame, &history), Some(vec![rect(0)]));
        assert_eq!(repaint_bounds(3, frame, &history), Some(vec![rect(0), rect(20), rect(40), rect(60)]));
        assert_eq!(repaint_bounds(0, frame, &history), None, "unknown age");
        assert_eq!(repaint_bounds(4, frame, &history), None, "a whole-surface frame in the window");
        assert_eq!(repaint_bounds(5, frame, &history), None, "older than the history");
        assert_eq!(repaint_bounds(1, None, &history), None);
    }

    /// #191. `solid_color` accepts only a single full-surface, unrounded, borderless solid fill
    /// (including odd logical sizes at fractional scales whether `ceil(w*s)` rounds up past
    /// `physical_size` or matches it) and rejects partial, bordered, rounded, gradient, or
    /// multi-command display lists.
    #[test]
    fn solid_color_matches_only_full_surface_borderless_unrounded_fills() {
        use layout::paint::{DisplayList, Draw, DrawCmd};
        let fill = node::Rgba { r: 0.125, g: 0.25, b: 0.5, a: 0.75 };
        let surface = PhysicalRect { x0: 0, y0: 0, x1: 1204, y1: 754 };
        let logical_scaled = (963.0 * 1.25, 603.0 * 1.25); // 1203.75 x 753.75
        let box_cmd = |rect, clip, background, radius, widths| DrawCmd {
            rect,
            clip,
            draw: Draw::Box {
                background,
                radius,
                border: node::BorderPaint::default(),
                widths,
                dash: node::StrokeDash::default(),
            },
        };
        let full_rect = LogicalRect { x: 0.0, y: 0.0, width: logical_scaled.0, height: logical_scaled.1 };
        let ok = DisplayList {
            commands: vec![box_cmd(
                full_rect,
                surface,
                vec![node::Fill::Color(fill)],
                node::Radii::default(),
                node::EdgeInsets::default(),
            )],
        };
        assert_eq!(solid_color(&ok, surface, logical_scaled), Some(fill), "fractional-scale .75 case matches");

        // 2560x1440 at 1.75x -> logical 1463x823: physical_size rounds half-up to 2560x1440 while
        // snap_to_physical ceils 2560.25x1440.25 to 2561x1441.
        let surface_175 = PhysicalRect { x0: 0, y0: 0, x1: 2560, y1: 1440 };
        let clip_175 = PhysicalRect { x0: 0, y0: 0, x1: 2561, y1: 1441 };
        let scaled_175 = (1463.0 * 1.75, 823.0 * 1.75);
        let ok_175 = DisplayList {
            commands: vec![box_cmd(
                LogicalRect { x: 0.0, y: 0.0, width: scaled_175.0, height: scaled_175.1 },
                clip_175,
                vec![node::Fill::Color(fill)],
                node::Radii::default(),
                node::EdgeInsets::default(),
            )],
        };
        assert_eq!(solid_color(&ok_175, surface_175, scaled_175), Some(fill), "fractional-scale .25 case matches");

        let rounded = DisplayList {
            commands: vec![box_cmd(
                full_rect,
                surface,
                vec![node::Fill::Color(fill)],
                node::Radii::from(4.0),
                node::EdgeInsets::default(),
            )],
        };
        assert_eq!(solid_color(&rounded, surface, logical_scaled), None, "rounded corners need EGL");

        let bordered = DisplayList {
            commands: vec![box_cmd(
                full_rect,
                surface,
                vec![node::Fill::Color(fill)],
                node::Radii::default(),
                node::EdgeInsets { top: 1.0, right: 1.0, bottom: 1.0, left: 1.0 },
            )],
        };
        assert_eq!(solid_color(&bordered, surface, logical_scaled), None, "borders need EGL");

        let partial = DisplayList {
            commands: vec![box_cmd(
                LogicalRect { width: 100.0, ..full_rect },
                surface,
                vec![node::Fill::Color(fill)],
                node::Radii::default(),
                node::EdgeInsets::default(),
            )],
        };
        assert_eq!(solid_color(&partial, surface, logical_scaled), None, "partial width needs EGL");

        let two = DisplayList { commands: vec![ok.commands[0].clone(), ok.commands[0].clone()] };
        assert_eq!(solid_color(&two, surface, logical_scaled), None, "multi-command lists need EGL");

        // Premultiplied u32 RGBA channels clamp cleanly and satisfy r,g,b <= a.
        assert_eq!(
            single_pixel_rgba(node::Rgba { r: 1.0, g: 0.0, b: 0.5, a: 1.0 }),
            (u32::MAX, 0, u32::MAX / 2 + 1, u32::MAX)
        );
        let (r, g, b, a) = single_pixel_rgba(node::Rgba { r: 1.5, g: -0.2, b: 1.0, a: 0.5 });
        assert!(r == a && g == 0 && b == a && a == u32::MAX / 2 + 1, "premultiplied and clamped: {:?}", (r, g, b, a));
    }

    /// #191. A static solid background panel presents via `wp_single_pixel_buffer_manager_v1` +
    /// `wp_viewport` without initializing EGL or binding a `wl_egl_window`, skips unchanged
    /// repaints, destroys the temporary `wl_buffer` after commit, and resets `configured_size` on
    /// teardown.
    #[test]
    fn solid_background_panel_presents_a_single_pixel_buffer_without_egl() {
        let (mut app, _dir, mut queue, mock) = App::test_app_with_wire(
            r##"return { panel { id = "bg", layer = "background", output = "TEST",
                width = 1920, height = 1080, background = "#202030ff" } }"##,
        );
        assert_eq!(app.surfaces[0].surface_id, "bg@TEST");
        assert!(app.scale_globals.as_ref().and_then(|g| g.solid_buffers.as_ref()).is_some());
        let wl = app.surfaces[0].role.wl_surface().unwrap().id().protocol_id();
        queue.roundtrip(&mut app).unwrap();
        let commits_before = mock.commits_for(wl);

        // Configure and paint the background panel: must present via single-pixel without EGL.
        app.surfaces[0].map_state = MapState::AwaitingConfigure;
        app.bind_and_clear(0, 1920, 1080);
        queue.roundtrip(&mut app).unwrap();

        assert!(app.egl.is_none(), "single-pixel path must not initialize EGL");
        assert!(app.surfaces[0].bound.is_none(), "single-pixel path must not bind wl_egl_window");
        assert_eq!(app.surfaces_drawn, 1);
        assert_eq!(mock.commits_for(wl), commits_before + 1);
        let reqs = mock.requests.lock().unwrap().clone();
        // `wl_surface.attach` is opcode 1 (arg0 = buffer id); `wl_surface.commit` is opcode 6;
        // `wl_buffer.destroy` is opcode 0.
        let (attach_pos, buf_id) = reqs
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, &(id, op, arg))| (id == wl && op == 1).then_some((i, arg)))
            .expect("wl_surface.attach was sent");
        assert_ne!(buf_id, 0);
        let commit_pos = reqs[attach_pos..]
            .iter()
            .position(|&(id, op, _)| id == wl && op == 6)
            .map(|rel| attach_pos + rel)
            .expect("wl_surface.commit followed attach");
        let destroy_pos =
            reqs.iter().position(|&req| req == (buf_id, 0, 0)).expect("temporary single-pixel wl_buffer was destroyed");
        assert!(destroy_pos > commit_pos, "wl_buffer.destroy must follow wl_surface.commit");

        // Second paint with unchanged display list commits nothing and stays off EGL.
        app.paint_surface(0);
        queue.roundtrip(&mut app).unwrap();
        assert_eq!(app.surfaces_drawn, 1, "unchanged solid panel skips repaint");
        assert_eq!(mock.commits_for(wl), commits_before + 1);
        assert!(app.egl.is_none());

        // Dropping the role object resets configured_size and last_painted even without an EGL bind.
        app.drop_role_object(0);
        assert_eq!(app.surfaces[0].configured_size, (0, 0));
        assert!(app.surfaces[0].last_painted.is_none());
    }
}
