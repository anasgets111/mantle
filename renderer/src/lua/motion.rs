//! A named 0..1 clock shared by property tweens across nodes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::luacats::{As, lua_class, lua_fn};
use super::signal::{self, CellId, DirtyFlag};
use crate::layout::node::{Easing, parse_easing};

#[derive(Debug)]
struct Clock {
    start: f32,
    target: f32,
    started: Instant,
    duration: Duration,
    easing: Easing,
}

impl Clock {
    fn phase_at(&self, now: Instant) -> f32 {
        let distance = now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32();
        if self.target >= self.start {
            (self.start + distance).min(self.target)
        } else {
            (self.start - distance).max(self.target)
        }
    }

    fn at(&self, now: Instant) -> f32 {
        self.easing.apply(self.phase_at(now))
    }

    fn retarget(&mut self, target: f32, now: Instant) -> bool {
        if self.target == target {
            return false;
        }
        self.start = self.phase_at(now);
        self.target = target;
        self.started = now;
        true
    }
}

#[derive(Clone)]
pub(crate) struct MotionHandle {
    clock: Rc<RefCell<Clock>>,
    dirty: DirtyFlag,
    cell: CellId,
}

impl std::fmt::Debug for MotionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MotionHandle").field("cell", &self.cell).finish()
    }
}

impl PartialEq for MotionHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.clock, &other.clock)
    }
}

impl MotionHandle {
    pub(crate) fn at(&self, now: Instant) -> f32 {
        self.clock.borrow().at(now)
    }

    pub(crate) fn moving(&self, now: Instant) -> bool {
        let clock = self.clock.borrow();
        clock.phase_at(now) != clock.target
    }

    pub(crate) fn note_read(&self, lua: &Lua) {
        signal::note_read(lua, self.cell);
    }

    pub(crate) fn to(&self, target: f64, now: Instant) -> mlua::Result<()> {
        let target = progress("motion:to", target)?;
        if self.clock.borrow_mut().retarget(target, now) {
            self.dirty.mark_cell(self.cell);
        }
        Ok(())
    }
}

lua_class! {
    /// A named clock shared by animated properties and shader progress.
    impl MotionHandle {
        /// Move to 0 or 1 at a fixed full-range rate. Reversing midway keeps the current position.
        fn to(_lua, this, target: f64) {
            this.to(target, Instant::now())
        }
    }
}

#[derive(Default)]
struct Motions(HashMap<String, MotionHandle>);

fn progress(site: &str, value: f64) -> mlua::Result<f32> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(mlua::Error::runtime(format!("{site} needs progress within [0, 1]")));
    }
    Ok(value as f32)
}

pub(crate) fn register(lua: &Lua, dirty: DirtyFlag) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// A named 0..1 clock for `animate = { width = { clock = handle, from = 100 } }`.
        /// [docs](https://anasgets111.github.io/mantle/guide/animation.html#shared-motion)
        fn motion(
            lua,
            /// Clock identity across re-evaluations.
            name: String,
            /// Initial progress, 0..1.
            initial: f64,
            /// Milliseconds for the full 0..1 distance, 1..60000.
            duration: f64,
            /// Optional easing curve, default `"Linear"`. Reversing retraces the same curve.
            easing: Option<As<Value, Easing>>,
        ) -> MotionHandle {
            if name.is_empty() {
                return Err(mlua::Error::runtime("motion() requires a nonempty name"));
            }
            let initial = progress("motion() initial", initial)?;
            if !duration.is_finite() || !(1.0..=60_000.0).contains(&duration) {
                return Err(mlua::Error::runtime("motion() duration must be within [1, 60000] ms"));
            }
            let duration = Duration::from_millis(duration.round() as u64);
            let easing = easing
                .map(|value| parse_easing("motion() easing", &value.0))
                .transpose()
                .map_err(|err| mlua::Error::runtime(err.to_string()))?
                .unwrap_or(Easing::Linear);
            let mut registry = super::app_data_or_default::<Motions>(lua);
            if let Some(existing) = registry.0.get(&name) {
                let now = Instant::now();
                let mut clock = existing.clock.borrow_mut();
                if clock.duration != duration {
                    clock.start = clock.phase_at(now);
                    clock.started = now;
                    clock.duration = duration;
                }
                clock.easing = easing;
                return Ok(existing.clone());
            }
            let handle = MotionHandle {
                clock: Rc::new(RefCell::new(Clock { start: initial, target: initial, started: Instant::now(), duration, easing })),
                dirty: dirty.clone(),
                cell: signal::next_cell_id(),
            };
            registry.0.insert(name, handle.clone());
            Ok(handle)
        }
    )
}

pub(crate) fn from_value(value: &Value) -> Option<MotionHandle> {
    let Value::UserData(ud) = value else { return None };
    ud.borrow::<MotionHandle>().ok().map(|handle| handle.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversal_uses_the_same_position_and_full_range_speed() {
        let start = Instant::now();
        let mut clock = Clock {
            start: 0.0,
            target: 1.0,
            started: start,
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        };
        let near = |a: f32, b: f32| assert!((a - b).abs() < 1e-6, "{a} != {b}");
        near(clock.at(start + Duration::from_millis(80)), 0.4);
        clock.retarget(0.0, start + Duration::from_millis(80));
        near(clock.at(start + Duration::from_millis(80)), 0.4);
        near(clock.at(start + Duration::from_millis(120)), 0.2);
        near(clock.at(start + Duration::from_millis(160)), 0.0);
    }
}
