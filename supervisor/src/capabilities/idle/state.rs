//! What `mantle.idle` publishes: whether anything holds the session awake, and who.
//!
//! ADR-0139 kept the gate out of Lua: a read-state roster would buy only push machinery because
//! `register_threshold` callbacks cannot cross the wire. That hid why a countdown stopped while
//! the Supervisor dropped every threshold event: observed live with `systemd-inhibit --what=idle
//! --who=mpv`, the screen said nothing. ADR-0141 adds the roster.
//!
//! [`IdleState::inhibited`] is true for any kind of holder: a logind inhibitor named by
//! `Manager.BlockInhibited`, a Wayland surface inhibitor detected from the compositor's silence
//! (ADR-0160), or an `org.freedesktop.ScreenSaver` client this shell answers for (ADR-0231).
//! [`IdleState::inhibitors`] names holders. The shell's own hold is excluded (see
//! [`foreign_idle_inhibitors`]), which config can explain better than the `why` it passed down.
//! Thus `inhibited` true with an empty list means only this shell holds the session awake.

pub use shared::state::idle::{IdleInhibitor, IdleState};

/// One `Manager.ListInhibitors` row: `what`, `who`, `why`, `mode`, `uid`, `pid`.
pub(crate) type InhibitorRow = (String, String, String, String, u32, u32);

/// Rows that block idling, excluding this shell.
///
/// `delay` only requests grace before sleep; it does not stop seat idling, so only `block` counts.
/// `what` uses [`super::gate::blocks_idle`], keeping the drawn list and gate's decision identical.
///
/// Compare `who` with [`super::inhibit::INHIBIT_WHO`], not pid: the Supervisor holds the fd and
/// logind records this process's pid either way.
pub(crate) fn foreign_idle_inhibitors(rows: Vec<InhibitorRow>) -> Vec<IdleInhibitor> {
    rows.into_iter()
        .filter(|(what, who, _, mode, _, _)| {
            mode == "block" && super::gate::blocks_idle(what) && who != super::inhibit::INHIBIT_WHO
        })
        .map(|(_, who, why, _, _, _)| IdleInhibitor { who, why })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(what: &str, who: &str, why: &str, mode: &str) -> InhibitorRow {
        (what.to_string(), who.to_string(), why.to_string(), mode.to_string(), 1000, 42)
    }

    #[test]
    fn only_a_foreign_block_mode_idle_inhibitor_is_listed() {
        let own = super::super::inhibit::INHIBIT_WHO;
        // (case, what, who, why, mode, listed): this shell's own hold is excluded, an empty why is kept.
        for (case, what, who, why, mode, listed) in [
            ("block idle", "idle", "mpv", "Playing video", "block", true),
            ("delay mode does not stop idling", "idle", "mpv", "Playing video", "delay", false),
            ("blocks something other than idle", "handle-power-key:handle-lid-switch", "gdm", "", "block", false),
            ("idle inside a colon-separated what", "sleep:idle", "steam", "Game running", "block", true),
            ("own hold", "idle", own, "manual + video", "block", false),
            ("empty why", "idle", "systemd-inhibit", "", "block", true),
        ] {
            let expected = if listed { vec![IdleInhibitor { who: who.into(), why: why.into() }] } else { vec![] };
            assert_eq!(foreign_idle_inhibitors(vec![row(what, who, why, mode)]), expected, "{case}");
        }
    }
}
