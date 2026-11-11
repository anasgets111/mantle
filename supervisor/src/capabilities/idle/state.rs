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

/// Cap on a holder's `who` and `why`, which any session-bus or system-bus client supplies. Like a
/// tray title, they are drawn, never matched.
const MAX_INHIBITOR_TEXT_BYTES: usize = 256;

/// ponytail: logind allows 8192 inhibitors (`InhibitorsMax`); past 64 only the first are listed,
/// though `inhibited` still counts them all. Upgrade: publish how many were left out.
pub(crate) const MAX_LISTED_INHIBITORS: usize = 64;

/// A holder as published, its text capped.
pub(crate) fn inhibitor(who: &str, why: &str) -> IdleInhibitor {
    IdleInhibitor {
        who: crate::capabilities::truncate_utf8_bytes(who, MAX_INHIBITOR_TEXT_BYTES),
        why: crate::capabilities::truncate_utf8_bytes(why, MAX_INHIBITOR_TEXT_BYTES),
    }
}

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
        .take(MAX_LISTED_INHIBITORS)
        .map(|(_, who, why, _, _, _)| inhibitor(&who, &why))
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

    /// logind allows 8192 inhibitors (`InhibitorsMax`), each with any `who` and `why`.
    #[test]
    fn the_listed_logind_inhibitors_are_capped_in_count_and_text() {
        let long = format!("{}é{}", "a".repeat(255), "b".repeat(1024));
        let rows = vec![row("idle", &long, &long, "block"); MAX_LISTED_INHIBITORS + 1];

        let listed = foreign_idle_inhibitors(rows);

        assert_eq!(listed.len(), MAX_LISTED_INHIBITORS);
        assert_eq!(listed[0], IdleInhibitor { who: "a".repeat(255), why: "a".repeat(255) }, "é straddles byte 256");
    }

    /// The proof that no set of holders can push `mantle.idle` past the frame cap: every list at its
    /// limit, plus the compositor's row, every string at its cap in characters JSON escapes to six bytes.
    #[test]
    fn the_largest_idle_snapshot_fits_in_one_frame() {
        let text = || crate::capabilities::test_support::worst_case_text(MAX_INHIBITOR_TEXT_BYTES);
        let rows = MAX_LISTED_INHIBITORS + super::super::inhibit::MAX_SCREENSAVER_HOLDS + 1;
        let state = IdleState {
            inhibited: true,
            compositor_hold_stale: true,
            inhibitors: vec![IdleInhibitor { who: text(), why: text() }; rows],
        };

        let len = crate::capabilities::test_support::snapshot_frame_len(shared::Capability::Idle, &state);

        assert!(len < shared::framing::MAX_FRAME_LEN, "{len} bytes");
    }
}
