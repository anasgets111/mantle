use shared::{error, warn};

/// The one lock fact outliving Supervisor (ADR-0060): `$XDG_RUNTIME_DIR/mantle/session-locked-$WAYLAND_DISPLAY`
/// exists while that compositor may be locked. After restart `LockState::default()` would make `active` false
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

    /// Whether the compositor may be locked: the last write said so, or the marker cannot be read.
    /// An extra relock costs a password; a missed one leaves a lock with nothing drawn on it.
    pub fn is_set(&self) -> bool {
        self.path.try_exists().unwrap_or(true)
    }

    /// Idempotent both ways. Set before a lock is sent, so a Supervisor that dies before the answer
    /// still leaves it (the compositor may have granted the lock); cleared only by a report that
    /// proves the lock gone. A missed set leaves a restarted Supervisor blind to a lock, a missed
    /// clear costs one password prompt; both are logged.
    pub fn set(&self, locked: bool) {
        if locked {
            if let Err(err) = std::fs::File::create(&self.path) {
                // ponytail: only logged, so a crash leaves the successor idle; upgrade: retry, show in rescue.
                error!(
                    "could not write {} ; a Supervisor restart will not know the session is locked: {err}",
                    self.path.display()
                );
            }
        } else if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!("could not remove {} ; the next Supervisor start will lock the screen: {err}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_survives_the_process_that_wrote_it_and_reads_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-locked");
        assert!(!SessionLockedFlag::at(path.clone()).is_set(), "a fresh login has no marker");

        SessionLockedFlag::at(path.clone()).set(true);
        // A different SessionLockedFlag value entirely -- what a restarted Supervisor is.
        assert!(SessionLockedFlag::at(path.clone()).is_set());

        SessionLockedFlag::at(path.clone()).set(false);
        assert!(!SessionLockedFlag::at(path).is_set());
    }

    #[test]
    fn a_marker_that_cannot_be_read_reads_as_set() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::File::create(&file).unwrap();
        assert!(SessionLockedFlag::at(file.join("session-locked")).is_set());
    }

    #[test]
    fn repeated_verdicts_are_not_errors() {
        // Both directions repeat in ordinary use: removing a file that isn't there is not a failure to clear.
        let dir = tempfile::tempdir().unwrap();
        let flag = SessionLockedFlag::at(dir.path().join("session-locked"));
        flag.set(false);
        assert!(!flag.is_set());
        flag.set(true);
        flag.set(true);
        assert!(flag.is_set());
        flag.set(false);
        flag.set(false);
        assert!(!flag.is_set());
    }
}
