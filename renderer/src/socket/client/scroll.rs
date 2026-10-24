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
