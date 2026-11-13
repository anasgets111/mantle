//! Scroll physics and fling forwarding.

use std::time::Instant;

use super::RendererClient;
use crate::layout::scene::WheelScroll;
use crate::lua::signal::Signal;

impl RendererClient {
    /// A wheel's `delta` for `signal` ([`crate::layout::Scene::wheel`]): the instances to repaint now.
    pub fn wheel(&mut self, signal: &Signal, delta: f32, notch: bool, now: Instant) -> Vec<String> {
        match self.scene.wheel(signal, delta, notch, now) {
            WheelScroll::Eased(animating) => {
                self.publish_marks();
                animating
            }
            WheelScroll::Write(offset) => self.write_scroll(signal, offset),
        }
    }

    /// Writes `settled` after a scroll run starts or ends outside a pass or tick, owing its readers a pass.
    fn publish_marks(&mut self) {
        self.scene.publish_marks(self.loader.lua());
        self.mark_layout_readers();
    }

    /// Fingers lifted from `signal` at `velocity` px/s ([`crate::layout::Scene::fling`]): the instances to animate.
    pub fn fling(&mut self, signal: &Signal, velocity: f32, now: Instant) -> Vec<String> {
        let flung = self.scene.fling(signal, velocity, now);
        self.publish_marks();
        flung
    }

    /// Stops every run on `signal` ([`crate::layout::Scene::stop_scroll`]).
    pub fn stop_scroll(&mut self, signal: &Signal) {
        self.scene.stop_scroll(signal);
        self.publish_marks();
    }

    /// One frame of every smooth scroll in `due`, ahead of the pass so a reader lays out from the
    /// offset this frame draws: the instances moved in place.
    pub fn advance_scrolls(&mut self, due: &[String], now: Instant) -> Vec<String> {
        let mut moved = Vec::new();
        for (signal, offset) in self.scene.advance_scrolls(due, now) {
            moved.extend(self.write_scroll(&signal, offset));
        }
        self.publish_marks();
        moved
    }

    /// In place when only `scroll` slots read the signal, else a write this turn's pass reads.
    fn write_scroll(&mut self, signal: &Signal, offset: f32) -> Vec<String> {
        if let Some(moved) = self.scroll_in_place(signal, offset) {
            return moved;
        }
        if let Some(handle) = signal.scroll_handle() {
            handle.set_changed(mlua::Value::Number(f64::from(offset)));
        }
        Vec::new()
    }

    /// [`crate::layout::Scene::scroll_in_place`], with a pass owed to the readers of each `geometry` rect it moved.
    pub(super) fn scroll_in_place(&mut self, signal: &Signal, asked: f32) -> Option<Vec<String>> {
        let moved = self.scene.scroll_in_place(signal, asked, self.loader.lua());
        self.publish_marks();
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{client_for, reload, run_startup};
    use super::super::*;

    /// Six 50 px tiles in a 100 px row (200 px of room) scrolled by `s`, and a getter of `s` beside it
    /// counting its runs. `animate` is the row's.
    fn scrolled_row(animate: &str) -> (RendererClient, crate::lua::signal::Signal, tempfile::TempDir) {
        let (dir, _, mut client, _) = client_for(&format!(
            r#"
                s = scroll("s")
                n = state("n", 6)
                runs = 0
                return panel {{ id = "bar", layer = "top", reset_on_close = {{ s }}, child = column {{ children = {{
                    row {{ width = 100, height = 20, scroll = s, animate = {animate},
                        children = n:map(function(n)
                            local tiles = {{}}
                            for i = 1, n do tiles[i] = rect {{ width = 50, height = 20 }} end
                            return tiles
                        end) }},
                    rect {{ height = 1, width = s:map(function(o) runs = runs + 1 return o + 1 end) }},
                }} }} }}
                "#
        ));
        assert!(run_startup(&mut client));
        let signal = crate::lua::signal::from_userdata(&client.loader.lua().globals().get("s").unwrap()).unwrap();
        (client, signal, dir)
    }

    fn runs(client: &RendererClient) -> i64 {
        client.loader.lua().globals().get("runs").unwrap()
    }

    /// The getter's width less one, and the first tile's x: the offset Lua saw and the one drawn.
    fn shown(client: &RendererClient) -> (f32, f32) {
        let column = &client.scene.surface("bar@TEST").unwrap().children[0];
        (column.children[1].rect.width - 1.0, -column.children[0].children[0].rect.x)
    }

    /// Smooth wheel scrolling: a notch eases the offset over frames, a second notch mid-run retargets
    /// from what is on screen, the target stops at an end, and a reader sees each frame's offset in
    /// that frame with one evaluation.
    #[test]
    fn a_notch_eases_the_offset_frame_by_frame_and_its_readers_follow() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert_eq!(client.wheel(&signal, 40.0, true, t0), ["bar@TEST"]);
        assert!(!client.re_resolve_if_dirty(), "the notch writes nothing until a frame");
        assert_eq!(frame(&mut client, ms(50)), (1, (20.0, 20.0)), "halfway, read and drawn in one frame");
        // A second notch at 20 heads for 80 from 20: no jump.
        client.wheel(&signal, 40.0, true, ms(50));
        // A pass that resolves the row again keeps its run.
        client.loader.lua().load("n:set(7)").exec().unwrap();
        assert_eq!(frame(&mut client, ms(100)), (1, (50.0, 50.0)));
        assert_eq!(frame(&mut client, ms(150)), (1, (80.0, 80.0)), "settled on the target");
        assert_eq!(frame(&mut client, ms(200)), (0, (80.0, 80.0)), "a settled run asks nothing more");
        assert!(!client.scene.surface("bar@TEST").unwrap().animating());

        client.wheel(&signal, 900.0, true, ms(200));
        assert_eq!(frame(&mut client, ms(250)), (1, (165.0, 165.0)), "halfway to the end, not to 980");
        assert_eq!(frame(&mut client, ms(300)), (1, (250.0, 250.0)), "the target stops at the end of seven tiles");
        assert_eq!(signal.scroll_offset(), Some(250.0));

        // A touchpad follows the finger: it stops the run and moves from what is on screen.
        client.wheel(&signal, -100.0, true, ms(300));
        assert_eq!(frame(&mut client, ms(350)), (1, (200.0, 200.0)));
        client.wheel(&signal, 5.0, false, ms(350));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(shown(&client), (205.0, 205.0));
        assert_eq!(frame(&mut client, ms(400)), (0, (205.0, 205.0)), "the run is gone");
    }

    /// A smooth scroll run is motion: `settled` clears when a notch starts it, sets when a frame
    /// ends it or a jump stops it, with no pass or tick to write it.
    #[test]
    fn a_scroll_run_clears_settled_and_its_end_or_stop_sets_it() {
        let (mut client, signal, _dir) =
            scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }, settled = settled("w")"#);
        let settled =
            |client: &RendererClient| client.lua().load(r#"return settled("w"):get()"#).eval::<bool>().unwrap();
        let t0 = std::time::Instant::now();
        assert!(settled(&client));
        client.wheel(&signal, 40.0, true, t0);
        assert!(!settled(&client), "the notch started a run");
        client.advance_scrolls(&["bar@TEST".to_string()], t0 + std::time::Duration::from_millis(100));
        assert!(settled(&client), "the frame that ends it");

        client.wheel(&signal, 40.0, true, t0);
        assert!(!settled(&client));
        client.stop_scroll(&signal);
        assert!(settled(&client), "a stopped run");

        client.loader.lua().load("s:reveal(5)").exec().unwrap();
        client.stop_scroll(&signal);
        assert!(signal.pending_scroll().is_some(), "a press stops runs, not a request queued this turn");
    }

    /// Under `animate.scroll` a `:reveal` eases the run to the child like a notch would.
    #[test]
    fn a_reveal_eases_under_animate_scroll() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:reveal(5)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        // After the pass that started the run, so its clock cannot be ahead of this one.
        let t0 = std::time::Instant::now();
        assert_eq!(shown(&client), (0.0, 0.0), "the pass starts the run and moves nothing yet");
        client.advance_scrolls(&["bar@TEST".to_string()], t0 + std::time::Duration::from_millis(50));
        client.re_resolve_if_dirty();
        let (read, drawn) = shown(&client);
        assert!(read > 0.0 && read < 150.0 && read == drawn, "{read} on its way to 150, read as drawn");
        client.advance_scrolls(&["bar@TEST".to_string()], t0 + std::time::Duration::from_secs(1));
        client.re_resolve_if_dirty();
        assert_eq!(shown(&client), (150.0, 150.0), "tile 5 ends flush with the right edge");
        assert_eq!(signal.scroll_offset(), Some(150.0));
    }

    /// One frame of the smooth scrolls on `bar@TEST`, then the pass, as the main loop orders them:
    /// the reader's evaluations that frame and the offsets read and drawn.
    fn frame(client: &mut RendererClient, at: std::time::Instant) -> (i64, (f32, f32)) {
        let before = runs(client);
        client.advance_scrolls(&["bar@TEST".to_string()], at);
        client.re_resolve_if_dirty();
        (runs(client) - before, shown(client))
    }

    /// `reset_on_close` writes the top while a run is in flight: the run stops instead of dragging
    /// the offset back on the next frame.
    #[test]
    fn a_reset_on_close_stops_a_run() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        client.reset_closed_surfaces(&["bar@TEST"]);
        client.wheel(&signal, 40.0, true, t0);
        assert_eq!(frame(&mut client, ms(50)).1, (20.0, 20.0));
        client.reset_closed_surfaces(&[]);
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(80)).1, (0.0, 0.0));
        assert!(!client.scene.surface("bar@TEST").unwrap().animating());
    }

    /// An easing that overshoots its target at the end never reports an offset past the room: one
    /// evaluation a frame, no follow-up pass to pull a reader back.
    #[test]
    fn an_overshooting_run_at_the_end_never_reads_past_the_room() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "out_back" } }"#);
        client.wheel(&signal, 160.0, false, std::time::Instant::now());
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        client.wheel(&signal, 120.0, true, t0);
        for at in [40, 60, 80, 100] {
            let (runs, (read, drawn)) = frame(&mut client, t0 + std::time::Duration::from_millis(at));
            assert!(runs <= 1 && read <= 200.0 && read == drawn, "{at} ms: {runs} runs, read {read}, drawn {drawn}");
        }
        assert_eq!(shown(&client), (200.0, 200.0));
    }

    /// Content that shrinks under a run: frames stop at the new end, and a notch back mid-run moves
    /// from that end rather than from the target the content no longer reaches.
    #[test]
    fn a_run_whose_content_shrinks_stops_at_the_new_end() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        client.wheel(&signal, 900.0, true, t0);
        assert_eq!(frame(&mut client, ms(50)).1, (100.0, 100.0), "halfway to 200");
        client.loader.lua().load("n:set(3)").exec().unwrap();
        assert_eq!(frame(&mut client, ms(60)).1, (50.0, 50.0), "three tiles leave 50 px");
        assert_eq!(frame(&mut client, ms(70)), (0, (50.0, 50.0)), "held at the end, nothing to re-read");
        client.wheel(&signal, -40.0, true, ms(70));
        assert_eq!(frame(&mut client, ms(170)).1, (10.0, 10.0), "back one notch from the end");
    }

    /// A spring in flight hands its speed to the run a second notch starts, so the motion bends
    /// instead of starting again from still.
    #[test]
    fn a_notch_mid_spring_keeps_its_speed() {
        let spring = r#"{ scroll = { spring = { stiffness = 200, damping = 20 } } }"#;
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        let (mut moving, signal, _dir) = scrolled_row(spring);
        moving.wheel(&signal, 40.0, true, t0);
        let (_, (from, _)) = frame(&mut moving, ms(30));
        moving.wheel(&signal, 40.0, true, ms(30));
        let carried = frame(&mut moving, ms(40)).1.0 - from;

        let (mut still, signal, _dir) = scrolled_row(spring);
        still.wheel(&signal, from, false, t0);
        assert!(still.re_resolve_if_dirty());
        still.wheel(&signal, 80.0 - from, true, ms(30));
        let rested = frame(&mut still, ms(40)).1.0 - from;
        assert!(carried > rested, "from {from}: moving {carried} px in 10 ms, from still {rested}");
    }

    /// Without `animate.scroll` a notch writes its offset at once, as before.
    #[test]
    fn a_notch_without_animate_scroll_moves_at_once() {
        let (mut client, signal, _dir) = scrolled_row("{}");
        assert!(client.wheel(&signal, 40.0, true, std::time::Instant::now()).is_empty());
        assert!(client.re_resolve_if_dirty());
        assert_eq!(shown(&client), (40.0, 40.0));
    }

    /// `:scroll_to` under `animate.scroll` eases like a notch, its reader evaluated once a frame,
    /// and settles on the target clamped to the room.
    #[test]
    fn a_scroll_to_eases_under_animate_scroll_and_stops_at_the_ends() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:scroll_to(900)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert_eq!(shown(&client), (0.0, 0.0), "the pass starts the run and moves nothing yet");
        let (evaluations, (read, drawn)) = frame(&mut client, ms(50));
        assert!(evaluations == 1 && (100.0..200.0).contains(&read) && read == drawn, "{evaluations}: {read}, {drawn}");
        assert_eq!(frame(&mut client, ms(1000)), (1, (200.0, 200.0)), "six tiles leave 200 px");
        assert_eq!(signal.scroll_offset(), Some(200.0));
        client.loader.lua().load("s:scroll_to(-50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(2000)).1, (0.0, 0.0));
    }

    /// Without `animate.scroll` both requests land in the pass, clamped; bad arguments raise.
    #[test]
    fn a_scroll_request_without_animate_scroll_moves_at_once() {
        let (mut client, _signal, _dir) = scrolled_row("{}");
        for (src, used) in [
            ("s:scroll_to(120)", 120.0),
            ("s:scroll_by(30)", 150.0),
            ("s:scroll_by(900)", 200.0),
            ("s:scroll_to(-5)", 0.0),
        ] {
            client.loader.lua().load(src).exec().unwrap();
            assert!(client.re_resolve_if_dirty());
            assert_eq!(shown(&client), (used, used), "{src}");
        }
        for src in ["s:scroll_to(0/0)", "s:scroll_by(math.huge)", "n:scroll_to(1)"] {
            let err = client.loader.lua().load(src).exec().unwrap_err().to_string();
            assert!(err.contains("must be finite") || err.contains("only valid on a scroll"), "{src}: {err}");
        }
        assert!(!client.re_resolve_if_dirty(), "a refused request asks for nothing");
    }

    /// A page of `bar` holding a 200 px column (100 px of room) scrolled by `s`; the other page holds
    /// none. `side` holds a hidden column scrolled by `s` when `extra` is set.
    fn paged(extra: bool) -> (RendererClient, tempfile::TempDir) {
        let side = if extra {
            r#", panel { id = "side", layer = "top", child = column { width = 100, height = 100, scroll = s,
                visible = shown, children = tiles(10) } }"#
        } else {
            ""
        };
        let (dir, _, mut client, _) = client_for(&format!(
            r#"
                s = scroll("s")
                page = state("page", 1)
                shown = state("shown", false)
                function tiles(n)
                    local out = {{}}
                    for i = 1, n do out[i] = rect {{ width = 10, height = 20 }} end
                    return out
                end
                return {{ panel {{ id = "bar", layer = "top", child = column {{ children = page:map(function(p)
                    if p == 1 then return {{ column {{ width = 100, height = 100, scroll = s, children = tiles(10) }} }} end
                    return {{ rect {{ width = 5, height = 5 }} }}
                end) }} }} {side} }}
                "#
        ));
        assert!(run_startup(&mut client));
        (client, dir)
    }

    /// Runs `src`, then passes until nothing is dirty; the offset `s` reports.
    fn then_offset(client: &mut RendererClient, src: &str) -> f32 {
        client.loader.lua().load(src).exec().unwrap();
        while client.re_resolve_if_dirty() {}
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        crate::lua::signal::from_userdata(&signal).unwrap().scroll_offset().unwrap()
    }

    /// A request made while no container holds the signal is dropped by the next pass, so the area
    /// that comes back starts at the top.
    #[test]
    fn a_scroll_request_with_no_holder_is_dropped_by_the_next_pass() {
        let (mut client, _dir) = paged(false);
        then_offset(&mut client, "page:set(2)");
        then_offset(&mut client, "s:scroll_by(50)");
        assert_eq!(then_offset(&mut client, "page:set(1)"), 0.0);
    }

    /// Adding the area and scrolling it in one handler still lands: the pass that builds it
    /// consumes the request before the check.
    #[test]
    fn a_request_made_with_the_area_it_scrolls_lands() {
        let (mut client, _dir) = paged(false);
        then_offset(&mut client, "page:set(2)");
        assert_eq!(then_offset(&mut client, "page:set(1) s:scroll_to(900)"), 100.0, "clamped to its room");
    }

    /// A container hidden in another surface still holds the signal: the request waits for its show.
    #[test]
    fn a_request_held_by_another_surface_waits_for_it() {
        let (mut client, _dir) = paged(true);
        then_offset(&mut client, "page:set(2)");
        then_offset(&mut client, "s:scroll_to(60)");
        assert_eq!(then_offset(&mut client, "shown:set(true)"), 60.0);
    }

    /// A surface shown in the turn that scrolls it keeps its tree while hidden, so the request waits
    /// for the pass that shows it instead of being dropped as orphaned.
    #[test]
    fn a_request_made_as_its_surface_opens_lands() {
        let (_dir, _, mut client, _) = client_for(
            r#"
            s = scroll("s")
            open = state("open", false)
            local tiles = {}
            for i = 1, 10 do tiles[i] = rect { width = 10, height = 20 } end
            return {
                panel { id = "bar", layer = "top", child = rect { width = 10, height = 10 } },
                panel { id = "side", layer = "top", visible = open,
                    child = column { width = 100, height = 100, scroll = s, children = tiles } },
            }
            "#,
        );
        assert!(run_startup(&mut client));
        assert_eq!(then_offset(&mut client, "open:set(true) s:scroll_to(60)"), 60.0);
    }

    /// A named scroll survives a reload, and so does its request: the reload's own pass decides it.
    #[test]
    fn a_pending_request_survives_a_reload_that_keeps_its_area() {
        let (mut client, dir) = paged(false);
        client.loader.lua().load("s:scroll_to(60)").exec().unwrap();
        let path = dir.path().join("shell.lua");
        assert!(reload(&mut client, &path, &std::fs::read_to_string(&path).unwrap()));
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        while client.re_resolve_if_dirty() {}
        assert_eq!(signal.scroll_offset(), Some(60.0));
    }

    /// The same reload without the area: the first pass drops the request.
    #[test]
    fn a_pending_request_is_dropped_by_the_reload_that_removes_its_area() {
        let (mut client, dir) = paged(false);
        client.loader.lua().load("s:scroll_to(60)").exec().unwrap();
        let path = dir.path().join("shell.lua");
        assert!(reload(
            &mut client,
            &path,
            r#"s = scroll("s") return panel { id = "bar", layer = "top", child = rect {} }"#
        ));
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        assert!(crate::lua::signal::from_userdata(&signal).unwrap().pending_scroll().is_none());
    }

    /// An arrow's `:scroll_by` adds to the run's target, not to the drawn offset, so clicks mid-run
    /// add up, and two in one turn both count.
    #[test]
    fn repeated_scroll_by_mid_run_adds_to_the_target() {
        let (mut client, _signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:scroll_by(50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert!(frame(&mut client, ms(50)).1.0 < 50.0, "mid-run");
        client.loader.lua().load("s:scroll_by(50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(1000)).1, (100.0, 100.0));
        client.loader.lua().load("s:scroll_by(30) s:scroll_by(30)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(2000)).1, (160.0, 160.0));
    }

    /// The wheel clamps against the room the last layout measured, so a getter reads the used offset
    /// in one evaluation; the follow-up pass is left for content that resized in the same turn.
    #[test]
    fn a_wheel_past_an_end_evaluates_its_reader_once() {
        let (mut client, signal, _dir) = scrolled_row("{}");
        let before = runs(&client);
        client.wheel(&signal, 900.0, false, std::time::Instant::now());
        assert!(client.re_resolve_if_dirty());
        assert_eq!((runs(&client) - before, shown(&client)), (1, (200.0, 200.0)));

        let before = runs(&client);
        client.wheel(&signal, -50.0, false, std::time::Instant::now());
        client.loader.lua().load("n:set(4)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!((runs(&client) - before, shown(&client)), (2, (100.0, 100.0)), "four tiles leave 100 px");
    }
}
