//! `delay` and `pulse`: signals driven by a clock rather than a write. Each keeps only its clock
//! in Rust and the values it holds as user values on its userdata (see `SignalKind::Delayed`);
//! a read arms a wake, which the poll loop turns into a write of the signal's own cell.

use std::cell::Cell;
use std::time::{Duration, Instant};

use mlua::{Lua, Value};

use super::{CellId, CpuBudget, FIRST_SOURCE_SLOT, HELD_SLOT, PENDING_SLOT, note_read, source_at};

struct DelayCell {
    held: Value,
    pending: Option<(Value, Instant)>,
}

struct PulseCell {
    /// The source value this signal last read. Seeded at construction, so a pulse starts low and
    /// fires on the first change rather than on the pass that built it.
    seen: Value,
    /// When the window closes, while one is open.
    until: Option<Instant>,
}

/// The earliest moment a clock-driven signal has to be re-read -- a `delay`'s hold coming due or
/// a `pulse`'s window closing -- read by the poll loop as its timeout; `None` keeps the loop
/// timeout-free (ADR-0124). One entry per signal: its due wake writes its cell, so only its
/// readers resolve again (ADR-0275).
#[derive(Default)]
struct WakeDeadline(Vec<(Instant, CellId)>);

fn arm_wake(lua: &Lua, due: Instant, cell: CellId) {
    let mut slot = crate::lua::app_data_or_default::<WakeDeadline>(lua);
    slot.0.retain(|(_, armed)| *armed != cell);
    slot.0.push((due, cell));
}

/// When the poll loop has to wake for a pending `delay` or `pulse`, if any.
pub fn next_wake_deadline(lua: &Lua) -> Option<Instant> {
    lua.app_data_ref::<WakeDeadline>().and_then(|slot| slot.0.iter().map(|(due, _)| *due).min())
}

/// Takes the cells of the signals come due; the caller writes them.
pub fn take_due_wake(lua: &Lua, now: Instant) -> Vec<CellId> {
    let Some(mut slot) = lua.app_data_mut::<WakeDeadline>() else { return Vec::new() };
    let (due, later): (Vec<_>, Vec<_>) = slot.0.drain(..).partition(|(at, _)| *at <= now);
    slot.0 = later;
    due.into_iter().map(|(_, cell)| cell).collect()
}

/// A `delay`'s read: the held value, adopting a pending one once it has held for `hold`.
pub(super) fn read_delayed(
    lua: &Lua,
    ud: &mlua::AnyUserData,
    hold: Duration,
    due: &Cell<Option<Instant>>,
    own: CellId,
) -> mlua::Result<Value> {
    // Recurses into the source, so it claims a nesting level for the reason `Computed` does.
    // Unguarded, a long enough chain exhausted the Rust stack and aborted `mantle check` before
    // any cap could answer.
    let _budget = CpuBudget::enter(lua)?;
    let fresh = source_at(ud, FIRST_SOURCE_SLOT)?.get_value(lua)?;
    let pending = due.get().map(|at| mlua::Result::Ok((ud.nth_user_value(PENDING_SLOT)?, at))).transpose()?;
    let mut cell = DelayCell { held: ud.nth_user_value(HELD_SLOT)?, pending };
    note_read(lua, own);
    let answer = cell.follow(fresh, hold, Instant::now(), |at| arm_wake(lua, at, own));
    let (pending, at) = cell.pending.unzip();
    due.set(at);
    ud.set_nth_user_value(HELD_SLOT, cell.held)?;
    ud.set_nth_user_value(PENDING_SLOT, pending)?;
    Ok(answer)
}

/// A `pulse`'s read: whether its window is open, opening or restarting it on a changed source.
pub(super) fn read_pulse(
    lua: &Lua,
    ud: &mlua::AnyUserData,
    hold: Duration,
    until: &Cell<Option<Instant>>,
    own: CellId,
) -> mlua::Result<Value> {
    // Recurses into the source too, so it claims a nesting level as `read_delayed` does.
    let _budget = CpuBudget::enter(lua)?;
    let fresh = source_at(ud, FIRST_SOURCE_SLOT)?.get_value(lua)?;
    let mut cell = PulseCell { seen: ud.nth_user_value(HELD_SLOT)?, until: until.get() };
    note_read(lua, own);
    let open = cell.fire(fresh, hold, Instant::now(), |at| arm_wake(lua, at, own));
    until.set(cell.until);
    ud.set_nth_user_value(HELD_SLOT, cell.seen)?;
    Ok(Value::Boolean(open))
}

impl DelayCell {
    /// One read: the value to answer now, and whether to arm a wake for later. Split from the
    /// signal so the clock is a parameter.
    fn follow(&mut self, fresh: Value, hold: Duration, now: Instant, arm: impl FnOnce(Instant)) -> Value {
        if fresh == self.held {
            self.pending = None;
            return self.held.clone();
        }
        let due = match &self.pending {
            Some((pending, due)) if *pending == fresh => *due,
            _ => now + hold,
        };
        if now >= due {
            self.held = fresh;
            self.pending = None;
        } else {
            self.pending = Some((fresh, due));
            arm(due);
        }
        self.held.clone()
    }
}

impl PulseCell {
    /// One read: whether the window is open now, and whether to arm a wake for its close. Split
    /// from the signal so the clock is a parameter, the way [`DelayCell::follow`] is.
    ///
    /// A change while a window is already open restarts it rather than extending the old one,
    /// which is what `restart()` does to a running `SequentialAnimation`. The window is not
    /// re-armed once it has closed, so a source that holds its new value pulses once.
    fn fire(&mut self, fresh: Value, hold: Duration, now: Instant, arm: impl FnOnce(Instant)) -> bool {
        if fresh != self.seen {
            self.seen = fresh;
            self.until = Some(now + hold);
        }
        if let Some(until) = self.until.filter(|until| now < *until) {
            arm(until);
            true
        } else {
            self.until = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::lua_with_state;
    use super::*;

    #[test]
    fn a_delayed_signal_answers_the_old_value_until_the_source_has_held_the_new_one() {
        let mut cell = DelayCell { held: Value::Boolean(true), pending: None };
        let t0 = Instant::now();
        let hold = Duration::from_millis(147);
        let mut armed = None;
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0, |due| armed = Some(due)), Value::Boolean(true));
        assert_eq!(armed, Some(t0 + hold), "the first read of a change arms the wake");
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0 + hold / 2, |_| ()), Value::Boolean(true));
        assert_eq!(cell.follow(Value::Boolean(false), hold, t0 + hold, |_| ()), Value::Boolean(false));
        // A change that returns before its hold elapses is cancelled outright.
        let later = t0 + hold + Duration::from_millis(1);
        cell.follow(Value::Boolean(true), hold, later, |_| ());
        assert_eq!(
            cell.follow(Value::Boolean(false), hold, later + Duration::from_millis(1), |_| ()),
            Value::Boolean(false)
        );
        assert!(cell.pending.is_none());
        assert_eq!(cell.follow(Value::Boolean(false), hold, later + hold, |_| ()), Value::Boolean(false));
    }

    #[test]
    fn delay_is_a_global_that_holds_a_state_write_and_arms_the_poll_deadline() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"open = state("open", false) held = delay(open, 1)"#).exec().unwrap();
        lua.load("open:set(true)").exec().unwrap();
        assert!(!lua.load("return held:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_some());
        std::thread::sleep(Duration::from_millis(5));
        assert!(!take_due_wake(&lua, Instant::now()).is_empty());
        assert!(lua.load("return held:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_none(), "an adopted value leaves nothing armed");
        for refused in ["delay(open, 0)", "delay(open, 0.1)"] {
            // 0.1 ms rounds to no milliseconds at all, so a hold that reads as positive would
            // adopt on the very next poll and never hold anything.
            let err = lua.load(refused).exec().unwrap_err().to_string();
            assert!(err.contains("[1, 60000]"), "{refused}: {err}");
        }
    }

    /// The clock is a parameter, so the window is exercised without sleeping through it.
    #[test]
    fn a_pulse_opens_on_a_change_restarts_on_the_next_one_and_closes_by_itself() {
        let hold = Duration::from_millis(100);
        let start = Instant::now();
        let mut cell = PulseCell { seen: Value::Integer(0), until: None };

        assert!(!cell.fire(Value::Integer(0), hold, start, |_| ()), "an unchanged source never fires");
        let mut armed = None;
        assert!(cell.fire(Value::Integer(1), hold, start, |due| armed = Some(due)));
        assert_eq!(armed, Some(start + hold), "an open window arms the close");
        // The source holds its new value: the window stays open on its own, then shuts once.
        assert!(cell.fire(Value::Integer(1), hold, start + Duration::from_millis(50), |_| ()));
        assert!(!cell.fire(Value::Integer(1), hold, start + hold, |_| ()), "the window closes at its due time");
        assert!(!cell.fire(Value::Integer(1), hold, start + hold * 2, |_| ()), "and does not reopen");

        // A second change mid-window restarts it rather than extending the first, which is what
        // `restart()` does to a running animation.
        let reopened = start + hold * 2;
        assert!(cell.fire(Value::Integer(2), hold, reopened, |_| ()));
        let mut armed = None;
        assert!(cell.fire(Value::Integer(3), hold, reopened + Duration::from_millis(60), |due| armed = Some(due)));
        assert_eq!(armed, Some(reopened + Duration::from_millis(60) + hold));
    }

    #[test]
    fn pulse_is_a_global_that_starts_low_fires_on_a_write_and_arms_the_poll_deadline() {
        let (lua, _dirty) = lua_with_state();
        lua.load(r#"clicks = state("clicks", 0) flashing = pulse(clicks, 50)"#).exec().unwrap();
        assert!(!lua.load("return flashing:get()").eval::<bool>().unwrap(), "a pulse starts low");
        assert!(next_wake_deadline(&lua).is_none(), "and arms nothing until something changes");

        lua.load("clicks:set(1)").exec().unwrap();
        assert!(lua.load("return flashing:get()").eval::<bool>().unwrap());
        assert!(next_wake_deadline(&lua).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(!take_due_wake(&lua, Instant::now()).is_empty());
        assert!(!lua.load("return flashing:get()").eval::<bool>().unwrap(), "the window closed");

        for refused in ["pulse(clicks, 0)", "pulse(clicks, 0.1)"] {
            let err = lua.load(refused).exec().unwrap_err().to_string();
            assert!(err.contains("[1, 60000]"), "{refused}: {err}");
        }
        lua.globals().set("handle", lua.create_any_userdata(7u32).unwrap()).unwrap();
        let err = lua.load("pulse(handle, 50)").exec().unwrap_err().to_string();
        assert!(err.contains("Signal"), "{err}");

        // Read-only for the same reason every other engine-written signal is: the only thing that
        // may open the window is the source changing.
        let err = lua.load("flashing:set(true)").exec().unwrap_err().to_string();
        assert!(err.contains("a pulse"), "{err}");
    }
}
