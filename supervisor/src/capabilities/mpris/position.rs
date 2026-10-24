//! MPRIS position extrapolation, Firefox/zen zero-position anomaly filtering, and monotonic timing.

use shared::debug;
use shared::state::mpris::{PlayState, PlayerState};

/// Cross-process-comparable `CLOCK_MONOTONIC` microseconds, matching the IDL's timestamp field;
/// opaque `std::time::Instant` would not. Known gap (ADR-0036): the clock's `time` is 1 Hz, too
/// coarse for this resolution; Lua reads `mantle.clock:now()` instead.
fn monotonic_micros() -> i64 {
    let now: std::time::Duration = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(std::time::Duration::from)
        .unwrap_or_default();
    i64::try_from(now.as_micros()).unwrap_or(i64::MAX)
}

/// The `Position` to publish, and the `CLOCK_MONOTONIC` moment it was read.
///
/// Two answers must not be believed. A **failed** read means the player did not answer, not that the
/// track is at zero (ADR-0036). A **zero** on a track we were already minutes into means the same
/// thing: Firefox and zen answer `Position` with `0` for several seconds after any `SetPosition` or
/// `Seek`, then report the true offset again once they catch up. Measured by seeking to 340s and
/// reading `Position` back as 340s, then `0`, then the true 363s while `PlaybackStatus` stayed
/// `Playing` throughout. Publishing that zero restarts every progress bar at the start of the track
/// while the video plays on.
///
/// Either way the last real reading is kept **with its own timestamp**. A stale position under a
/// fresh stamp tells a client extrapolating from the pair that the track jumped backwards, which is
/// worse than either half alone. This is the retention `album_art_path` and `length` already do
/// across a same-track update, for the same reason: the player stopped describing something it had
/// not actually changed.
///
/// A zero on a track we were *not* already inside is published, because that is where a new one
/// begins.
///
/// A reading up to 1 s short of where the last one has got to is that position rounded down:
/// Firefox and zen answer `Position` in whole seconds while their `Seeked` carries microseconds
/// (busctl: `439000000` against `Seeked` `439069817`). Believing it puts the clock up to a second
/// behind after every play/pause. The last reading is kept with its stamp under an unchanged
/// `play_state`, so a re-sent `Metadata` compares equal and is not pushed; a changed one publishes
/// where the last reading had got to, restamped so extrapolation restarts from now.
pub(super) fn resolve_position(
    read: zbus::Result<i64>,
    previous: Option<&PlayerState>,
    same_track: bool,
    play_state: PlayState,
    bus_name: &str,
) -> (Option<i64>, i64) {
    let last = previous.map(|p| (p.position, p.position_updated_at));
    let settled = previous.is_some_and(|p| p.play_state == play_state);
    let now = monotonic_micros();
    let reached = previous.filter(|_| same_track).and_then(|p| reached(p, now));
    match read {
        Ok(0) if same_track && matches!(last, Some((Some(position), _)) if position > 0) => last.unwrap_or((None, 0)),
        // ponytail: a seek back under 1 s that sends no `Seeked` is ignored; MPRIS requires `Seeked`.
        Ok(position) if reached.is_some_and(|reached| (0..1_000_000).contains(&(reached - position))) => {
            if settled {
                last.unwrap_or((None, 0))
            } else {
                (reached, now)
            }
        }
        // A negative offset is a player bug, unknown like an unread one, as `length` treats it.
        Ok(position) => (Some(position).filter(|p| *p >= 0), now),
        Err(err) => {
            debug!("Position read failed for {bus_name}; keeping the last known reading this round: {err}");
            // Only the track the reading belongs to. Publishing the previous track's offset under
            // the new one's metadata is worse than admitting we do not know: a 30-second track
            // would inherit a 5:40 position and every bar would draw it past its own end.
            if same_track { last.unwrap_or((None, 0)) } else { (None, 0) }
        }
    }
}

/// Where `previous`'s reading has got to by `now`, as a client extrapolating it would draw it.
fn reached(previous: &PlayerState, now: i64) -> Option<i64> {
    let position = previous.position?;
    if previous.play_state != PlayState::Playing {
        return Some(position);
    }
    Some(position.saturating_add(((now - previous.position_updated_at) as f64 * previous.rate) as i64))
}

pub(super) fn publish_seeked_position(state: &mut PlayerState, position: i64) {
    state.position = Some(position).filter(|p| *p >= 0);
    state.position_updated_at = monotonic_micros();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous_at(position: i64) -> PlayerState {
        PlayerState { position: Some(position), position_updated_at: 42, ..PlayerState::default() }
    }

    #[test]
    fn resolve_position_keeps_the_stamp_of_a_kept_reading_and_restamps_a_believed_one() {
        let paused = || PlayerState { play_state: PlayState::Paused, ..previous_at(340_000_000) };
        let playing = PlayerState {
            play_state: PlayState::Playing,
            rate: 1.0,
            position_updated_at: monotonic_micros() - 100_000,
            ..previous_at(439_069_817)
        };
        // (label, read, previous, same_track, play_state, expected position, expected stamp: None = a fresh one)
        let cases = [
            // Firefox answers `0` for several seconds after a seek while playing on from the target.
            // Believing it restarts every progress bar at the beginning of the track.
            (
                "a zero mid-track keeps the last reading and its timestamp",
                Ok(0),
                Some(previous_at(340_000_000)),
                true,
                PlayState::Stopped,
                Some(340_000_000),
                Some(42),
            ),
            // A track we were not already inside legitimately begins at zero.
            (
                "a zero on a new track is published with a fresh stamp",
                Ok(0),
                Some(previous_at(340_000_000)),
                false,
                PlayState::Stopped,
                Some(0),
                None,
            ),
            // A first reading has no previous to keep.
            ("a zero with nothing to fall back on is published", Ok(0), None, true, PlayState::Stopped, Some(0), None),
            (
                "a real reading always wins",
                Ok(363_000_000),
                Some(previous_at(340_000_000)),
                true,
                PlayState::Stopped,
                Some(363_000_000),
                None,
            ),
            (
                "an unchanged reading keeps its stamp",
                Ok(340_000_000),
                Some(paused()),
                true,
                PlayState::Paused,
                Some(340_000_000),
                Some(42),
            ),
            // Resuming restarts extrapolation from now.
            (
                "an unchanged reading is restamped when the play state moves",
                Ok(340_000_000),
                Some(paused()),
                true,
                PlayState::Playing,
                Some(340_000_000),
                None,
            ),
            // Firefox floors `Position` to the second; its `Seeked` gave the precise offset.
            (
                "a reading rounded down to the second keeps the precise one",
                Ok(3_000_000),
                Some(PlayerState { play_state: PlayState::Paused, ..previous_at(3_789_773) }),
                true,
                PlayState::Paused,
                Some(3_789_773),
                Some(42),
            ),
            (
                "a rounded reading while playing keeps the reading it trails",
                Ok(439_000_000),
                Some(playing.clone()),
                true,
                PlayState::Playing,
                Some(439_069_817),
                Some(playing.position_updated_at),
            ),
            (
                "resuming on a rounded reading keeps the precise offset under a fresh stamp",
                Ok(439_000_000),
                Some(PlayerState { play_state: PlayState::Paused, ..previous_at(439_069_985) }),
                true,
                PlayState::Playing,
                Some(439_069_985),
                None,
            ),
            (
                "a reading a second or more behind is a seek",
                Ok(338_000_000),
                Some(paused()),
                true,
                PlayState::Paused,
                Some(338_000_000),
                None,
            ),
            // ADR-0036: unavailable is not zero. Nothing has ever been read here.
            (
                "an unread position is nil rather than a fabricated zero",
                Err(zbus::Error::InvalidReply),
                None,
                true,
                PlayState::Stopped,
                None,
                Some(0),
            ),
        ];
        for (label, read, previous, same_track, play_state, want_position, want_stamp) in cases {
            let (position, updated_at) = resolve_position(read, previous.as_ref(), same_track, play_state, "test");
            assert_eq!(position, want_position, "{label}");
            match want_stamp {
                Some(stamp) => assert_eq!(updated_at, stamp, "{label}"),
                None => assert_ne!(updated_at, 42, "{label}: a believed reading carries the moment it was taken"),
            }
        }
    }

    #[test]
    fn a_seeked_signal_to_zero_overrides_the_transient_zero_filter() {
        let mut state = previous_at(340_000_000);
        publish_seeked_position(&mut state, 0);
        assert_eq!(state.position, Some(0));
        assert_ne!(state.position_updated_at, 42);
        publish_seeked_position(&mut state, -1);
        assert_eq!(state.position, None, "a negative offset is unknown, as on the property read");
    }
}
