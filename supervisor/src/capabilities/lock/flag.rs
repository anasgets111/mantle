use shared::warn;

use super::state::SessionLock;

/// The one lock fact outliving Supervisor (ADR-0060): `$XDG_RUNTIME_DIR/obelisk/session-locked`
/// exists exactly while the compositor is locked. After restart `LockState::default()` would make `active` false
/// and paint behind an invisible fallback (ADR-0058, 0059). A file suffices for this boolean and
/// survives `SIGKILL`; runtime dir bounds staleness to the last session.
pub struct SessionLockedFlag {
    path: std::path::PathBuf,
}

impl SessionLockedFlag {
    /// Marker at an explicit path. `main.rs` uses [`shared::session_locked_flag_path`]; tests use a
    /// temporary directory.
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Whether the compositor was locked at the last write. Read errors mean "not locked" on
    /// purpose; the alternative relocks at every boot when one file cannot be read.
    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    /// Idempotent both ways: repeated `Locked` reports and `Finished` after `Unlocked` are normal.
    /// Log and swallow failures; a missed set costs a possible relock, a missed clear one password
    /// prompt.
    pub fn apply(&self, change: SessionLock) {
        match change {
            SessionLock::Taken => {
                if let Err(err) = std::fs::File::create(&self.path) {
                    warn!(
                        "could not write {} ; a Supervisor restart will not know the session is locked: {err}",
                        self.path.display()
                    );
                }
            }
            SessionLock::Released => {
                if let Err(err) = std::fs::remove_file(&self.path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(
                        "could not remove {} ; the next Supervisor start will lock the screen: {err}",
                        self.path.display()
                    );
                }
            }
            SessionLock::Unchanged => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::lock::state::{LockEvent, LockState, apply, compositor_lock_change};

    #[test]
    fn a_renderer_that_died_holding_the_lock_leaves_the_marker_set() {
        // RendererLost clears active because this shell holds nothing, but the compositor stays
        // locked. The marker is driven off LockOutcome, not active, so RendererLost (not a
        // Reported) cannot reach it.
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("session-locked"));
        flag.apply(compositor_lock_change(&shared::LockOutcome::Locked));

        let mut state = LockState::default();
        apply(&mut state, LockEvent::Reported(shared::LockOutcome::Locked));
        apply(&mut state, LockEvent::RendererLost);

        assert!(!state.active, "the crash means this shell holds nothing");
        assert!(flag.is_set(), "but the session is still locked, and the marker is what says so");
    }

    #[test]
    fn the_marker_survives_the_process_that_wrote_it_and_reads_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-locked");
        assert!(!SessionLockedFlag::at(path.clone()).is_set(), "a fresh login has no marker");

        SessionLockedFlag::at(path.clone()).apply(SessionLock::Taken);
        // A different SessionLockedFlag value entirely -- what a restarted Supervisor is.
        assert!(SessionLockedFlag::at(path.clone()).is_set());

        SessionLockedFlag::at(path.clone()).apply(SessionLock::Released);
        assert!(!SessionLockedFlag::at(path).is_set());
    }

    #[test]
    fn applying_the_same_change_twice_is_not_an_error() {
        // Both directions repeat in ordinary use. Removing a file that isn't there must not be
        // treated as a failure to clear.
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("session-locked"));
        flag.apply(SessionLock::Released);
        assert!(!flag.is_set());
        flag.apply(SessionLock::Taken);
        flag.apply(SessionLock::Taken);
        assert!(flag.is_set());
        flag.apply(SessionLock::Released);
        flag.apply(SessionLock::Released);
        assert!(!flag.is_set());
    }

    #[test]
    fn an_unchanged_verdict_touches_nothing_in_either_direction() {
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("session-locked"));
        flag.apply(SessionLock::Unchanged);
        assert!(!flag.is_set());
        flag.apply(SessionLock::Taken);
        flag.apply(SessionLock::Unchanged);
        assert!(flag.is_set(), "a refusal after a real lock must not erase it");
    }
}
