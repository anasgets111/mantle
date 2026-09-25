//! The Renderer's main loop: bind the globals, build `App`, then poll Wayland, the Supervisor
//! socket and every timer until exit (ADR-0039).

use shared::SupervisorFrame;
use wayland_client::globals::registry_queue_init;

use super::lock::{EXIT_SUPERVISOR_GONE, supervisor_gone_report};
use super::output::{geometries_from, screens_payload};
use super::*;
use crate::lua::signal::thread_cpu_time;
use crate::socket::FrameOutcome;

/// Renderer main thread: Wayland, EGL, Lua, the retained `Scene`, and live signals (ADR-0039).
/// `inbound_rx` carries socket-decoded `SupervisorFrame`s; `outbound_tx` carries every frame this
/// thread sends back, including replies and lock reports. Ends
/// the process on a dead Wayland connection, like the `EXIT_SUPERVISOR_GONE` arm below and for the
/// same reason: `std::process::exit` skips destructors. Returning an error instead unwinds `App`,
/// whose EGL surfaces and `wl_surface`s talk to the compositor that just left, which is how a log
/// out became a `khronos-egl` `unwrap()` panic and exit code 101.
fn exit_because_the_compositor_is_gone(what_failed: &str, err: &dyn std::fmt::Display) -> ! {
    error!("{what_failed} failed ({err}); there is no compositor to talk to, so exiting");
    std::process::exit(shared::EXIT_COMPOSITOR_GONE);
}

pub fn run(
    generation_id: u32,
    mut inbound_rx: tokio::sync::mpsc::Receiver<SupervisorFrame>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    waker: crate::wake::Waker,
) -> Result<(), Box<dyn Error>> {
    // A missing socket is not a failure to report up: at session end the Supervisor outlives the
    // compositor briefly and respawns into a session that is already gone, which is where the three
    // `Error: NoCompositor` generations came from. Nothing is built yet, so there is nothing to
    // skip unwinding past; this is the same answer for the same reason.
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => exit_because_the_compositor_is_gone("connecting to the Wayland display", &err),
    };
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    // Optional: panel-only configs work without `xdg_wm_base`; `create_surfaces` logs any window
    // left unbuilt.
    let xdg_shell =
        XdgShell::bind(&globals, &qh).inspect_err(|err| log_bind_failure("<xdg-shell>", "xdg_wm_base::bind", err)).ok();
    // Optional, and quiet when absent: a compositor with no blur is not a broken session. Its
    // `GlobalProxy` reports a missing global only when a blur region is actually asked for.
    // Version 1 is the only version; `capabilities` arrives on the queue right after this.
    let background_effect = BackgroundEffectState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    // Mandatory: every compositor advertises `wl_shm`.
    let shm = Shm::bind(&globals, &qh)?;
    // Cannot fail: its `GlobalProxy` reports a missing lock global only when a lock is requested
    // (ADR-0052 decision 4).
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let captures = CaptureRegistry::bind(&globals, &qh);
    // One process-wide shaping handle; `RendererClient` gets a clone (ADR-0039 decision 3).
    // `Loader::new()` stays here because `mlua::Lua` is `!Send`.
    let shaping = ShapingHandle::spawn();
    let client = RendererClient::start(shaping.clone(), outbound_tx.clone(), generation_id, waker.clone())?;

    let mut app = App {
        registry_state,
        output_state,
        shader_stage: crate::layout::image_shader::ShaderStage::default(),
        compositor_state,
        seat_state,
        layer_shell,
        background_effect,
        blur_supported: false,
        xdg_shell,
        session_lock_state,
        session_lock: None,
        egl: None,
        gl: None,
        conn,
        shaping,
        text_painter: None,
        image_cache: ImageCache::with_waker(waker.clone()),
        capture_cache: CaptureCache::default(),
        captures,
        client,
        surfaces: Vec::new(),
        exit: false,
        startup_complete: false,
        outbound_tx,
        generation_id,
        queue_handle: qh.clone(),
        pointer: None,
        cursor_shown: None,
        pointer_at: None,
        shm,
        keyboard: None,
        keyboard_focus: None,
        armed: None,
        drag: None,
        input_serial: None,
        pointer_input_count: 0,
        reposition_token: 0,
        focused_secure_submit: None,
        focused_text_field: None,
        secure_buffer: shared::SecureBuffer::new(),
        shift_held: false,
        ctrl_held: false,
        repeat_info: None,
        repeating: None,
        field_input_surfaces: Vec::new(),
        caret_blink: input::caret_blink(),
        caret_epoch: std::time::Instant::now(),
        caret_painted_on: true,
        animation_frames_due: Vec::new(),
        surfaces_drawn: 0,
        repaint_split: surface::RepaintSplit::default(),
        current_egl_surface: None,
    };

    // Binding delivers outputs and seat capabilities as a burst; two roundtrips populate the
    // initial output list (`monitor = "All"` expands per monitor) and keyboard capability.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    // Seed `screens` before evaluation (ADR-0041 decision 2): configs loop over it during the
    // first pass, so seeding after evaluation would declare no per-monitor panels.
    let screens = app.screens(None);
    let outputs = geometries_from(&screens);
    app.image_cache.set_texture_budget(output::texture_budget(&screens));
    app.capture_cache.set_texture_budget(output::texture_budget(&screens));
    app.client.set_screens(screens_payload(&screens));
    let specs = app.client.run_startup_evaluation().unwrap_or_default();
    // Set the declared font chain after evaluation but before first paint; `TextPainter` loads it
    // lazily, and `set_chain` rebuilds instead of respawning. No declaration keeps the default.
    // A family a node names by hand is not resolved here: it lands on first sight (ADR-0144).
    app.shaping.set_chain(&crate::lua::fonts::declared_chain(app.client.lua()));
    let instances = expand_instances(&specs, &outputs);
    warn_unmatched_monitors(&specs, &outputs);
    app.client.set_instances(instances.clone());
    // The first resolve validates only: it runs before any surface binds, so instances use output
    // logical sizes and are never painted. Evaluation/apply already log and
    // set rescue; this adds the consequence.
    if !app.client.apply_instances() {
        warn!(
            "no scene was applied at startup; surfaces still bind, and paint nothing until a reload or a push produces one"
        );
    }

    app.create_surfaces(&qh, &specs, &instances);
    // Output events can now reconcile against an evaluated scene.
    app.startup_complete = true;

    // Both `None` unless `mantle --profile`.
    let mut profile = idle_profile::IdleProfile::from_env();
    let mut memory = memory_profile::MemoryProfile::from_env();
    let mut was_active = false;

    loop {
        // `then` leaves the clock unread while the profile is off, as `idle_profile` promises.
        let dispatch_started = profile.is_some().then(thread_cpu_time).flatten();
        let dispatched = match event_queue.dispatch_pending(&mut app) {
            Ok(count) => count > 0,
            // Only an I/O failure means the connection itself is gone. A `BadMessage` or a
            // `Protocol` error is this Renderer's own bug against a compositor that is still there,
            // so it keeps propagating and stays a reportable crash.
            Err(wayland_client::DispatchError::Backend(wayland_client::backend::WaylandError::Io(err))) => {
                exit_because_the_compositor_is_gone("dispatching Wayland events", &err)
            }
            Err(err) => return Err(err.into()),
        };
        if let Some(started) = dispatch_started
            && let Some(ended) = thread_cpu_time()
            && let Some(profile) = profile.as_mut()
        {
            profile.dispatch(ended.saturating_sub(started));
        }
        if app.exit {
            break;
        }
        // Before the turn reads `field_input_changed`, so a repeat lands in this turn's repaint
        // rather than waiting for the next wake.
        app.fire_due_repeat();
        app.request_due_captures();
        // Drain every `SupervisorFrame` (ADR-0039), coalescing snapshot bursts into one wake. A
        // dead socket is distinct from an empty one (ADR-0059 decision 1), or the Renderer could
        // block in `poll` with no capability source.
        loop {
            let frame = match inbound_rx.try_recv() {
                Ok(frame) => frame,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                // `std::process::exit` skips `SessionLockInner::Drop`, whose bare destroy is
                // `invalid_destroy` after `locked`; skipping the destructor closes the connection
                // instead, logged as an ordinary lock client death. Dropping `App` would kill it.
                // Use `is_some()`, not SCTK's dispatch-lagging `is_locked()`: over-reporting a VT
                // is safer than claiming the shell died behind an inaccessible lock screen.
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // Flush decided requests before exit: `lock` only enqueues, and the normal
                    // flush is below this drain. Otherwise `session_lock = Some` could outlive an
                    // unsent request. `std::process::exit` skips SCTK's destructor.
                    if let Err(err) = event_queue.flush() {
                        warn!(
                            "the last flush before exiting failed ({err}); a session lock requested in this same turn may never have reached the compositor"
                        );
                    }
                    error!("{}", supervisor_gone_report(app.session_lock.is_some()));
                    std::process::exit(EXIT_SUPERVISOR_GONE);
                }
            };
            match app.client.handle_frame(frame) {
                FrameOutcome::Handled => {}
                FrameOutcome::ApplyPending => app.apply_pending(&qh, None),
                // Service immediately: lock declaration is tracked-surface state, not a
                // capability-push result (ADR-0052 decision 3), and deferring weakens "secure now".
                FrameOutcome::SetSessionLock(locked) => {
                    // Round-trip only before unlock: SCTK gates `unlock` on dispatched
                    // `locked`, not sent. Without it, `unlock` can no-op and `Drop` sends forbidden
                    // `destroy` (`invalid_destroy`) (ADR-0052). Acquire needs no round-trip because
                    // this thread owns its inputs. Do not return on a failed round-trip: that would
                    // strand the session locked; trying the unlock costs at most one failed flush.
                    if !locked && let Err(err) = event_queue.roundtrip(&mut app) {
                        warn!(
                            "the round trip before an unlock failed ({err}); attempting the unlock anyway rather than exiting with the session locked"
                        );
                    }
                    app.set_session_lock(&qh, locked);
                }
            }
            if app.exit {
                break;
            }
        }
        if app.exit {
            break;
        }
        // Once after the drain, `DirtyFlag::take` coalesces snapshot bursts into one `Scene::apply`
        // (ADR-0044 decision 2). Do not use `wl_surface::frame()`: it would block idle instead of
        // waking on the 15 ms poll, while ADR-0124 makes the push itself the wakeup. Staging and
        // repainting are one commit: the latter's `swap_buffers` carries layer/input/map changes;
        // per-field commits would show the compositor a half-updated surface.
        // Profiling adds three `clock_gettime` calls per turn for the resolve/repaint split.
        let mut phases = idle_profile::Phases::start(profile.is_some());
        app.client.fire_due_timers();
        app.client.poll_palette();
        app.client.wake_due_signals();
        // `apply_instances` and `handle_apply_pending` also resolve, from dispatch, where `ms
        // resolve` is not running. Dropped rather than reported, so the split stays a breakdown of
        // the phase and never sums past it.
        let _ = app.client.take_resolve_split();
        // Kept as its own name, not folded into `re_resolved` below: "a pass ran" and "something
        // changed" answer different questions. Only a pass can change any tree, so only a pass
        // rules out the narrowed repaint, and only a pass makes every surface's protocol state
        // worth re-deriving.
        let passed = app.client.re_resolve_if_dirty();
        let targeted_instances = if passed { app.client.take_last_resolved() } else { None };
        phases.mark_resolve();
        phases.mark_resolve_split(app.client.take_resolve_split());
        // A frame callback is the tween clock (ADR-0145). Taken every turn so a callback that
        // arrives with a push is answered by this repaint, not repeated next turn.
        let due = std::mem::take(&mut app.animation_frames_due);
        let ticked =
            if due.is_empty() { Vec::new() } else { app.client.tick_animations(&due, std::time::Instant::now()) };
        phases.mark_tick();
        phases.mark_tick_split(app.client.take_tick_split());
        let re_resolved = passed || !ticked.is_empty();
        // Take unconditionally so a keystroke arriving with a push is covered by this repaint, not
        // repeated next turn.
        app.repaint_caret_if_it_flipped();
        let typed_surfaces = std::mem::take(&mut app.field_input_surfaces);
        let typed = !typed_surfaces.is_empty();
        // A decode changes neither retained properties nor a list that names the file, so it is
        // its own repaint/invalidation (ADR-0122).
        let landed = app.image_cache.poll();
        if !landed.is_empty() {
            // The cue to repaint, and nothing more: what each node is now showing is settled by
            // the paint that follows, which is the only thing holding the exact cache keys
            // (ADR-0183).
            app.forget_painted_lists_drawing(&landed);
        }
        // A capture frame lands from a `Dispatch` callback during `dispatch_pending` above, with
        // no GL context to upload it (ADR-0039); this is its repaint cue, the same role `landed`
        // plays for a decode.
        let captured = app.capture_cache.poll();
        if !captured.is_empty() {
            app.mark_surfaces_stale_for_captures(&captured);
        }
        // What protocol state this turn owes; `surface_state_for_turn` carries the reasoning.
        let state = turn::surface_state_for_turn(
            passed,
            targeted_instances.is_some(),
            !ticked.is_empty(),
            app.input_serial.is_some(),
        );
        match state.scope {
            turn::StateScope::Everything => app.apply_resolved_surface_state(),
            turn::StateScope::Targeted => {
                if let Some(ref ids) = targeted_instances {
                    app.apply_resolved_surface_state_for(&[ids, &ticked]);
                }
            }
            turn::StateScope::Ticked => app.apply_resolved_surface_state_for(&[&ticked]),
            turn::StateScope::Nothing => {}
        }
        if state.popup_latch {
            app.apply_popup_visibility_for_armed_input();
        }
        if re_resolved {
            // Hover signals follow layout; `on_hover` follows the pointer (ADR-0112 amendment).
            app.refresh_hover_after_layout();
        }
        phases.mark_surface_state();
        // Which surfaces this turn owes the screen; `repaint_for_turn` carries the reasoning.
        match turn::repaint_for_turn(turn::TurnChanges {
            passed,
            targeted: targeted_instances.is_some(),
            ticked: !ticked.is_empty(),
            stale: app.has_stale_surfaces(),
            typed,
        }) {
            turn::Repaint::Narrowed => app.repaint_surfaces_named(&[
                targeted_instances.as_deref().unwrap_or_default(),
                &ticked,
                &typed_surfaces,
            ]),
            turn::Repaint::Everything => app.repaint_mapped_surfaces(),
            turn::Repaint::Nothing => {}
        }
        phases.mark_repaint();
        phases.mark_repaint_split(app.take_repaint_split());
        // Skip focus maintenance on a truly idle turn (ADR-0124): it walks the focused scope's trees
        // for fields.
        let active = dispatched || re_resolved || typed || !landed.is_empty();
        if active {
            was_active = true;
        }
        // Disarm after the turn, not only when active: `dispatch_pending` armed this serial and
        // `apply_resolved_surface_state` is its only reader. This enforces ADR-0049's one-turn
        // real-input window.
        app.input_serial = None;
        // Once per active turn, scrub a secure field whose surface was torn down before a later
        // keystroke notices. `App::apply_secure_key` remains the load-bearing check.
        if active {
            let focus_started = profile.is_some().then(thread_cpu_time).flatten();
            app.drop_secure_focus_if_its_surface_is_gone();
            // Sample after the cleanup above: clearing secure focus can enable a search. Failing
            // these guards excludes the turn from `searched`, not from `focus_turns` or its CPU.
            let searched = app.keyboard_focus.is_some() && app.focused_secure_submit.is_none();
            // Also arm fields that appeared under already-arrived keyboard focus. One walk serves
            // both: nothing between them moves focus or popups.
            if searched {
                let scope = app.keyboard_focus_scope();
                app.arm_secure_focus_if_the_scope_now_declares_one(&scope);
                app.arm_autofocus_if_nothing_is_typing(&scope);
            }
            if let Some(started) = focus_started
                && let Some(ended) = thread_cpu_time()
                && let Some(profile) = profile.as_mut()
            {
                profile.focus(ended.saturating_sub(started), searched);
            }
        }
        if let Some(profile) = profile.as_mut() {
            // After focus maintenance so a turn's focus cost reports in its own window, and
            // still before breaking, so an exiting turn is reported.
            profile.turn(
                idle_profile::Turn {
                    dispatched,
                    re_resolved: passed,
                    ticked: !ticked.is_empty(),
                    typed,
                    decoded: !landed.is_empty(),
                    painted: re_resolved || typed || !landed.is_empty(),
                    drawn: std::mem::take(&mut app.surfaces_drawn),
                },
                phases,
            );
        }
        if let Some(memory) = memory.as_mut() {
            // The closure keeps the scene walk and the cache locks off every turn but the one
            // that reports; see `memory_profile`.
            memory.maybe_report(|| census(&app));
        }
        if app.exit {
            break;
        }
        // This is the flush that actually caught the log out: it propagated, `run` returned, and
        // `App`'s destructor then drove EGL into a compositor that was gone.
        match event_queue.flush() {
            Err(wayland_client::backend::WaylandError::Io(err)) => {
                exit_because_the_compositor_is_gone("flushing the Wayland queue", &err)
            }
            // A protocol error has already closed the connection; ignored, this loop spun on it.
            Err(err) => return Err(err.into()),
            Ok(()) => {}
        }
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            // No timeout while idle (ADR-0124): Wayland events use the connection fd; Supervisor
            // frames, landed decodes, and socket-thread exit use the waker. The one timeout is a
            // pending `delay(signal, ms)` or an open `pulse(signal, ms)` window (ADR-0146,
            // ADR-0153) or an animated image's next frame (ADR-0233), armed only while one is
            // running, the way a frame callback is requested only while a tween is.
            let mut fds = [
                nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN),
            ];
            let deadline = app
                .client
                .next_wake_deadline()
                .into_iter()
                .chain(app.next_stale_deadline())
                .chain(app.next_repeat_deadline())
                .chain(app.next_caret_deadline())
                .chain(app.captures.next_request_deadline())
                .min();
            let timeout = deadline.map_or(nix::poll::PollTimeout::NONE, |due| {
                // Rounded up: `as_millis` on the last fraction of a hold is 0, and a zero timeout
                // returns at once to a turn that finds the deadline still a few hundred
                // microseconds away, hundreds of times over.
                let millis = due.saturating_duration_since(std::time::Instant::now()).as_micros().div_ceil(1000);
                nix::poll::PollTimeout::try_from(millis.min(i32::MAX as u128) as i32)
                    .unwrap_or(nix::poll::PollTimeout::NONE)
            });
            // Collect Lua garbage and hand glibc's free lists back to the OS before entering
            // indefinite sleep (ADR-0124). Keystroke bursts and animations pay zero trims while
            // running, and trim exactly once when settling into idle.
            if was_active && timeout == nix::poll::PollTimeout::NONE {
                let _ = app.client.lua().gc_collect();
                // SAFETY: plain one-integer FFI. `malloc_trim` locks the arenas itself and only
                // `madvise`s pages the allocator already holds free, never live chunks.
                unsafe {
                    libc::malloc_trim(0);
                }
                was_active = false;
            }
            let woke = matches!(nix::poll::poll(&mut fds, timeout), Ok(n) if n > 0);
            let wayland_ready = woke && fds[0].any().unwrap_or(false);
            if let Some(profile) = profile.as_mut() {
                profile
                    .wake(idle_profile::Wake { wayland: wayland_ready, waker: woke && fds[1].any().unwrap_or(false) });
            }
            if woke {
                if wayland_ready {
                    match guard.read() {
                        // The read side, and the one a killed compositor actually reaches first:
                        // `poll` reports the fd readable because the peer closed it, and the read
                        // that follows is what sees the broken pipe.
                        Err(wayland_client::backend::WaylandError::Io(err)) => {
                            exit_because_the_compositor_is_gone("reading from the Wayland connection", &err)
                        }
                        Err(err) => return Err(err.into()),
                        Ok(_) => {}
                    }
                }
                // Drain before the turn; a wake arriving during the turn remains counted.
                waker.drain();
            }
            // The guard drops here; an unread guard yields no events next iteration.
        }
    }

    // While a context is still current, and only here: a config shader's program outlives every
    // generation, so nothing earlier owns its end (ADR-0184). A context already gone took its
    // objects with it, which is why this is an orderly teardown and not a recovery.
    if let (Some(egl), Some(gl)) = (app.egl.as_ref(), app.gl.as_ref()) {
        use glow::HasContext;
        let active = if egl.instance.get_current_context() == Some(egl.context) {
            true
        } else {
            let surf = app.surfaces.iter().find_map(|s| s.bound.as_ref().map(|b| b.egl_surface));
            egl.instance.make_current(egl.display, surf, surf, Some(egl.context)).is_ok()
        };
        // SAFETY: active is true only when the GL context was confirmed current on this thread.
        let not_lost = active && unsafe { gl.get_error() } != glow::CONTEXT_LOST;
        if not_lost {
            // SAFETY: this is the context every paint bound, on the one thread that ever bound it.
            unsafe { app.shader_stage.destroy(gl) };
        }
    }
    Err("stopped on an EGL/GPU failure, the only thing that sets `app.exit`".into())
}

/// Reads every subsystem that owns heap into one [`memory_profile::Census`], at one instant so the
/// columns are comparable. `malloc` is left default: `MemoryProfile` reads `mallinfo2` itself,
/// after this returns, so the arena totals include whatever this walk allocated rather than
/// missing it.
fn census(app: &App) -> (memory_profile::Census, memory_profile::Surfaces) {
    let image = app.image_cache.census();
    let (shape_entries, shape_bytes) = app.shaping.census();
    let (surfaces, nodes, properties) = app.client.scene().census();
    let census = memory_profile::Census {
        image_bytes: image.resident_bytes as u64,
        image_ready: image.ready as u64,
        image_pending: image.pending as u64,
        image_failed: image.failed as u64,
        image_evicted: image.evicted as u64,
        image_landed: image.landed as u64,
        shape_entries: shape_entries as u64,
        shape_bytes: shape_bytes as u64,
        lua_bytes: app.client.lua().used_memory() as u64,
        scene_surfaces: surfaces as u64,
        scene_nodes: nodes as u64,
        scene_properties: properties as u64,
        malloc: shared::Malloc::default(),
    };
    (census, memory_profile::Surfaces(app.client.scene().census_by_surface()))
}
