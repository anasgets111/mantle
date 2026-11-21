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
//! [`foreign_blockers`]), which config can explain better than the `why` it passed down.
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

/// Rows that block `target` (`"idle"` or `"sleep"`), excluding this shell.
///
/// `delay` only requests grace before sleep; it does not block idling or suspend, so only `block`
/// counts. `what` uses [`super::gate::blocks`], keeping the drawn list and gate's decision
/// identical.
///
/// Compare `who` with [`super::inhibit::INHIBIT_WHO`], not pid: the Supervisor holds the fd and
/// logind records this process's pid either way.
pub(crate) fn foreign_blockers(rows: &[InhibitorRow], target: &str) -> Vec<IdleInhibitor> {
    rows.iter()
        .filter(|(what, who, _, mode, _, _)| {
            mode == "block" && super::gate::blocks(what, target) && who != super::inhibit::INHIBIT_WHO
        })
        .take(MAX_LISTED_INHIBITORS)
        .map(|(_, who, why, _, _, _)| inhibitor(who, why))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(what: &str, who: &str, why: &str, mode: &str) -> InhibitorRow {
        (what.to_string(), who.to_string(), why.to_string(), mode.to_string(), 1000, 42)
    }

    #[test]
    fn only_a_foreign_block_mode_inhibitor_for_the_target_is_listed() {
        let own = super::super::inhibit::INHIBIT_WHO;
        // (case, target, what, who, why, mode, listed): this shell's own hold is excluded, an empty why is kept.
        for (case, target, what, who, why, mode, listed) in [
            ("block idle", "idle", "idle", "mpv", "Playing video", "block", true),
            ("delay idle", "idle", "idle", "mpv", "Playing video", "delay", false),
            ("other token for idle", "idle", "handle-power-key:handle-lid-switch", "gdm", "", "block", false),
            ("colon-separated idle", "idle", "sleep:idle", "steam", "Game running", "block", true),
            ("own idle hold", "idle", "idle", own, "manual + video", "block", false),
            ("empty why", "idle", "idle", "systemd-inhibit", "", "block", true),
            ("block sleep", "sleep", "sleep", "backup", "Backing up", "block", true),
            ("colon-separated sleep", "sleep", "idle:sleep", "steam", "Downloading", "block", true),
            ("delay sleep", "sleep", "sleep", own, "Lock the screen before sleep", "delay", false),
            ("own sleep block", "sleep", "sleep", own, "manual", "block", false),
            ("idle only for sleep", "sleep", "idle", "mpv", "Playing video", "block", false),
            ("unrelated token for sleep", "sleep", "handle-lid-switch", "upower", "", "block", false),
        ] {
            let expected = if listed { vec![IdleInhibitor { who: who.into(), why: why.into() }] } else { vec![] };
            assert_eq!(foreign_blockers(&[row(what, who, why, mode)], target), expected, "{case}");
        }
    }

    /// logind allows 8192 inhibitors (`InhibitorsMax`), each with any `who` and `why`.
    #[test]
    fn the_listed_logind_inhibitors_are_capped_in_count_and_text() {
        let long = format!("{}é{}", "a".repeat(255), "b".repeat(1024));
        let rows = vec![row("idle:sleep", &long, &long, "block"); MAX_LISTED_INHIBITORS + 1];

        for target in ["idle", "sleep"] {
            let listed = foreign_blockers(&rows, target);
            assert_eq!(listed.len(), MAX_LISTED_INHIBITORS, "{target}");
            assert_eq!(listed[0], IdleInhibitor { who: "a".repeat(255), why: "a".repeat(255) }, "é straddles byte 256");
        }
    }

    /// The proof that no set of holders can push `mantle.idle` past the frame cap: every list at its
    /// limit, plus the compositor's row, every string at its cap in characters JSON escapes to six bytes.
    #[test]
    fn the_largest_idle_snapshot_fits_in_one_frame() {
        let text = || crate::capabilities::test_support::worst_case_text(MAX_INHIBITOR_TEXT_BYTES);
        let rows = MAX_LISTED_INHIBITORS + super::super::inhibit::MAX_BUS_HOLDS + 1;
        let sleep_rows = MAX_LISTED_INHIBITORS + super::super::inhibit::MAX_BUS_HOLDS;
        let state = IdleState {
            inhibited: true,
            compositor_hold_stale: true,
            inhibitors: vec![IdleInhibitor { who: text(), why: text() }; rows],
            sleep_inhibited: true,
            sleep_inhibitors: vec![IdleInhibitor { who: text(), why: text() }; sleep_rows],
        };

        let len = crate::capabilities::test_support::snapshot_frame_len(shared::Capability::Idle, &state);

        assert!(len < shared::framing::MAX_FRAME_LEN, "{len} bytes");
    }
}
