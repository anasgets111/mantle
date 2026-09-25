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

/// EGL's bottom-left `x, y, w, h` quadruples for `rects` on a surface `height` tall.
fn egl_rects(rects: &[PhysicalRect], height: u32) -> Vec<i32> {
    rects.iter().flat_map(|r| [r.x0, height as i32 - r.y1, r.x1 - r.x0, r.y1 - r.y0]).collect()
}

impl App {
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
            let _ = egl.instance.make_current(egl.display, None, None, None);
            self.current_egl_surface = None;
            if let Err(err) = egl.instance.destroy_surface(egl.display, bound.egl_surface) {
                log_bind_failure(&self.surfaces[index].surface_id, "eglDestroySurface", err);
            }
        }
        // `BoundSurface`'s drop sends `wl_egl_window_destroy`.
        drop(bound);
        self.surfaces[index].configured_size = (0, 0);
        self.pending_trim = true;
    }

    /// Lazily builds the process-wide EGL state on the first drawable surface (ADR-0071). Failure
    /// is fatal: the Renderer exits and the Supervisor respawns it (ADR-0058, ADR-0071 decision 3).
    fn ensure_egl(&mut self, surface_id: &str) -> bool {
        if self.egl.is_some() {
            return true;
        }
        match egl::init(&self.conn) {
            Ok(state) => {
                self.egl = Some(state);
                true
            }
            Err(err) => {
                log_bind_failure(surface_id, "egl::init", err);
                self.exit = true;
                false
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
        let (width, height) = self.surfaces[index].configured_size;
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;
        let Some(surface_object_id) = self.surfaces[index].role.wl_surface().map(Proxy::id) else {
            // The window was hidden between requesting and performing the bind; there is nothing
            // to bind, and the map-state guard already stopped painting.
            return false;
        };

        // Only the first drawable surface pays for Mesa (ADR-0071); hidden windows took the cheap
        // bails above.
        if !self.ensure_egl(&surface_id) {
            return false;
        }
        let egl = self.egl.as_ref().expect("ensure_egl returned true, so the state is built");

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
        let egl_surface = unsafe {
            egl.instance.create_window_surface(egl.display, egl.config, native_window.ptr() as *mut c_void, None)
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(&surface_id, "eglCreateWindowSurface", e);
                self.exit = true;
                return false;
            }
        };

        if let Err(e) = egl.instance.make_current(egl.display, Some(egl_surface), Some(egl_surface), Some(egl.context))
        {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            // `native_window` drops on this return. `khronos_egl::Surface` has no `Drop`, so
            // without this the driver keeps a surface bound to a freed `wl_egl_window`.
            if let Err(err) = egl.instance.destroy_surface(egl.display, egl_surface) {
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
        if let Err(e) = egl.instance.swap_interval(egl.display, 0) {
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
        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
        // A new EGL surface has empty buffers, so the next paint is unconditional.
        self.surfaces[index].last_painted = None;
        true
    }

    /// Paint a bound surface's display list where its back buffer lacks it (ADR-0258), and swap. One
    /// `TextPainter` and EGL context serve all surfaces: GL objects stay valid across framebuffers,
    /// viewport size is per surface, and one canvas per surface is the fallback if that proves wrong.
    /// An absent tree still clears/swaps, or the compositor keeps the last frame.
    /// ponytail: paint scale is hardcoded `1.0`, so HiDPI outputs are upscaled. Upgrade:
    /// `set_buffer_scale` and matching `WlEglSurface::resize` together.
    pub(super) fn paint_surface(&mut self, index: usize) {
        // Unmapped or pre-configure surfaces cannot attach a buffer; `swap_buffers` is both attach
        // and commit, so this prevents `visible = false` from remapping (ADR-0038 decision 2).
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }
        let Some(egl_surface) = self.surfaces[index].bound.as_ref().map(|b| b.egl_surface) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].configured_size;
        let (width, height) = (width.max(1), height.max(1));

        let tree = self.client.scene().surface(&surface_id);
        let mut animating = tree.is_some_and(layout::ResolvedNode::animating);

        let is_clean = self.surfaces[index].is_clean() && self.field_focus_for(&surface_id).is_none();
        if is_clean
            && !self.surfaces[index].owes_a_paint()
            && self.surfaces[index].last_painted.as_ref().is_some_and(|(s, _)| *s == (width, height))
        {
            if animating && let Some(surface) = self.surfaces[index].role.wl_surface() {
                surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
                surface.commit();
            }
            return;
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
            let focus = self.field_focus_for(&surface_id);
            tree.as_ref().map(|tree| layout::paint::build(tree, 1.0, focus.as_ref())).unwrap_or_default()
        };
        // What the compositor re-blurs and recomposites behind this surface; `None` is the whole
        // surface (ADR-0063 amendment).
        let owed = self.surfaces[index].owes_a_paint();
        let surface_rect = PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 };
        let damage = match &self.surfaces[index].last_painted {
            Some((size, painted)) if *size == (width, height) => Some(
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
            tracked.last_painted = Some(((width, height), list));
            if animating && let Some(surface) = tracked.role.wl_surface() {
                surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
                surface.commit();
            }
            return;
        }

        let t_gl = timing.then(Instant::now);
        let Some(egl) = self.egl.as_ref() else {
            return;
        };
        // Re-establish the surface when switching between surfaces; bound surfaces share EGL state.
        // ponytail: skip make_current when this exact surface is already current on the GL context.
        if self.current_egl_surface != Some(egl_surface) {
            if let Err(e) =
                egl.instance.make_current(egl.display, Some(egl_surface), Some(egl_surface), Some(egl.context))
            {
                log_bind_failure(&surface_id, "eglMakeCurrent", e);
                self.exit = true;
                return;
            }
            self.current_egl_surface = Some(egl_surface);
        }

        // 0, unknown, where the driver has no buffer age.
        let age = egl.instance.query_surface(egl.display, egl_surface, super::egl_ext::EGL_BUFFER_AGE_EXT).unwrap_or(0);
        let history = &self.surfaces[index].damage_history;
        let regions = match repaint_bounds(usize::try_from(age).unwrap_or(0), damage.as_deref(), history) {
            Some(rects) => {
                let grown = rects.into_iter().map(|rect| list.repaint_region(rect).intersect(surface_rect));
                layout::paint::coalesce(grown.collect())
            }
            None => vec![surface_rect],
        };
        if let Some(set_damage_region) = egl.set_damage_region {
            let rects = egl_rects(&regions, height);
            // SAFETY: `egl_surface` is current on `egl.display` and its age was queried this frame,
            // as `EGL_KHR_partial_update` requires; `rects` holds `regions.len()` whole quadruples.
            unsafe {
                set_damage_region(egl.display.as_ptr(), egl_surface.as_ptr(), rects.as_ptr(), rects.len() as i32 / 4)
            };
        }

        if self.text_painter.is_none() {
            match TextPainter::new(
                |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
                width,
                height,
                self.shaping.clone(),
            ) {
                Ok(painter) => self.text_painter = Some(painter),
                Err(e) => {
                    log_bind_failure(&surface_id, "FemtoVG init", e);
                    self.exit = true;
                    return;
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
                1.0,
                (width as f32, height as f32),
                &regions,
                shaders,
            );
            self.repaint_split.text += split.text;
            self.repaint_split.icons += split.icons;
            self.repaint_split.boxes += split.boxes;
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
                animating |= self.client.scene().surface(&surface_id).is_some_and(layout::ResolvedNode::animating);
            }
        }
        // Straight after this surface's `execute` and before any other paint runs, which is what
        // scopes a cache-wide flag to the surface that earned it (ADR-0185).
        let deferred = self.image_cache.take_deferred();

        // Before the swap, which is the commit it has to precede. Requested only while a tween is
        // running, so an idle shell arms nothing and the loop's timeout-free poll stays that way
        // (ADR-0124, ADR-0130 decision 3).
        if animating && let Some(surface) = self.surfaces[index].role.wl_surface() {
            surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
        }
        // ponytail: only the swap is guarded; khronos-egl's other wrappers (make_current etc.) still unwrap (upstream #25).
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
        if swapped == khronos_egl::FALSE {
            // SAFETY: reads this thread's last EGL error.
            let error = unsafe { khronos_egl::Static.eglGetError() };
            log_bind_failure(&surface_id, "eglSwapBuffers", format!("EGL error {error:#x}"));
            self.exit = true;
            return;
        }
        if let Some(t_swap) = t_swap {
            self.repaint_split.swap += t_swap.elapsed();
        }
        // Record only after swap; otherwise an unpresented frame could make the next identical list
        // skip the paint the screen never received.
        self.surfaces[index].last_painted = Some(((width, height), list));
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
        // Images absent from every current list are idle (ADR-0123); queue eviction for the next
        // paint.
        let surfaces = &self.surfaces;
        self.image_cache.trim(|| {
            let mut pinned = Vec::new();
            for surface in surfaces {
                if let Some((_, list)) = &surface.last_painted {
                    list.drawn_images(&mut pinned);
                }
            }
            pinned
        });
    }

    /// Marks stale every surface whose last-painted list matches `stale_because`. Shared by
    /// [`Self::forget_painted_lists_drawing`] and [`Self::mark_surfaces_stale_for_captures`], which
    /// differ only in what a landed frame names.
    fn mark_surfaces_stale_where(&mut self, stale_because: impl Fn(&layout::paint::DisplayList) -> bool) {
        for surface in &mut self.surfaces {
            if surface.last_painted.as_ref().is_some_and(|(_, list)| stale_because(list)) {
                // Marked, not cleared: this surface still shows the old texture until it
                // repaints, so its list has to keep pinning it (ADR-0182).
                surface.stale = Some(std::time::Instant::now());
                surface.dirty = true;
            }
        }
    }

    /// Repaint every mapped surface after a changed scene because ADR-0044 decision 2 has one
    /// global dirty flag. `paint_surface` skips unchanged lists, so protocol-only updates need
    /// their own commit in [`App::apply_spec_change`]. A decoded image invalidates any list that
    /// draws its file (ADR-0122), making the next repaint upload the new texture.
    pub(in crate::wayland) fn forget_painted_lists_drawing(&mut self, files: &[std::path::PathBuf]) {
        self.mark_surfaces_stale_where(|list| list.draws_any_of(files));
    }

    /// A landed capture frame changes no `Draw::Capture` field (ADR-0248), so the surface showing
    /// it needs the same nudge a landed decode gets above: `list` still names the right node, the
    /// texture behind it is just new.
    pub(in crate::wayland) fn mark_surfaces_stale_for_captures(&mut self, nodes: &[layout::NodeId]) {
        self.mark_surfaces_stale_where(|list| list.captures_any_of(nodes));
    }

    pub(in crate::wayland) fn repaint_mapped_surfaces(&mut self) {
        self.repaint_mapped_surfaces_where(|_| true);
    }

    /// The surfaces this turn's pass, tick or keystroke named, plus any surface already `stale`.
    ///
    /// A tick changes only the trees it names, so the others would each build a display list and
    /// have it rejected as equal to the one they last painted. That build is not free: a text draw
    /// copies its content and style runs, an image or icon its name.
    pub(in crate::wayland) fn repaint_surfaces_named(&mut self, named: &[&[String]]) {
        self.repaint_mapped_surfaces_where(|s| turn::narrowed_repaint_covers(named, &s.surface_id, s.owes_a_paint()));
    }

    pub(in crate::wayland) fn take_repaint_split(&mut self) -> RepaintSplit {
        std::mem::take(&mut self.repaint_split)
    }

    /// Whether any mapped surface owes a repaint its tree cannot ask for. The main loop's repaint
    /// selection needs this: with nothing ticked, nothing typed and nothing landed, it would
    /// otherwise reach no repaint at all and a deferred decode would never be asked for again.
    pub(in crate::wayland) fn has_stale_surfaces(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.owes_a_paint() && surface.map_state == MapState::Mapped)
    }

    /// When the earliest owed repaint comes due, for the poll loop's only timeout. A GIF's next
    /// frame is owed by no tree, no signal and no frame callback, so nothing else would wake for
    /// it (ADR-0233).
    pub(in crate::wayland) fn next_stale_deadline(&self) -> Option<std::time::Instant> {
        // Bound too: an unbound surface is skipped by the repaint that would clear this, so a
        // deadline in the past would spin the poll loop instead of arming one wake.
        self.surfaces
            .iter()
            .filter(|s| s.map_state == MapState::Mapped && s.bound.is_some())
            .filter_map(|s| s.stale)
            .min()
    }

    fn repaint_mapped_surfaces_where(&mut self, wanted: impl Fn(&TrackedSurface) -> bool) {
        let drawn = self.surfaces_drawn;
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].map_state != MapState::Mapped {
                continue;
            }
            if !wanted(&self.surfaces[index]) {
                continue;
            }
            if self.surfaces[index].bound.is_none() {
                // A panel shown after starting hidden was configured without EGL; bind here because
                // no further configure is coming.
                if !self.ensure_bound(index) {
                    continue;
                }
            }
            self.paint_surface(index);
            if self.exit {
                return;
            }
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

    /// ADR-0258. A GIF's next-frame deadline outlives a paint that did not owe it: that paint's
    /// region may have skipped the GIF, which then deferred nothing. A paint it owed draws the GIF,
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
}
