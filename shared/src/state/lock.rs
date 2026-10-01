//! `mantle.lock` snapshot payload.

use serde::Serialize;

/// `mantle.lock`'s payload (ADR-0052).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LockState {
    /// The Renderer confirmed the session locked; a requested lock stays `false` until then.
    pub active: bool,
    /// A password is with PAM. A second submit is refused while true.
    pub authenticating: bool,
    /// Rejected passwords since this lock was confirmed; reset by the next lock.
    pub attempts: u32,
    /// Last failure to draw: PAM's verdict (`"authentication failed"`, `"too many attempts"`, or a
    /// PAM or worker error) or a refused lock's reason. Cleared by a correct password, `lock`, a
    /// confirmed lock, and unlock.
    pub error: String,
    /// PAM said yes and the lock is still up: the window for an out-animation (ADR-0190).
    pub unlocking: bool,
    /// `SetSessionLock { locked: true }` is in flight before Renderer confirmation.
    /// `#[serde(skip)]`: bookkeeping, not payload. `active` must mean only Renderer confirmation,
    /// but a respawn must still retake a lock that was asked for and not yet confirmed (ADR-0058).
    #[serde(skip)]
    pub requested: bool,
    /// Acquisition number, bumped only on Renderer `Locked`. `#[serde(skip)]` like `requested`.
    /// PAM can outlive its lock (`pam_unix` ~1s, `PAM_EXCHANGE_TIMEOUT` 30s): `finished` after
    /// `locked` (also `loginctl unlock-session`) can end N while an idle timer takes N+1. The
    /// number binds an answer to its question and blocks stale success (`accepts_outcome`).
    #[serde(skip)]
    pub acquisition: u64,
}
