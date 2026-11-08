//! `mantle.lock` snapshot payload.

use serde::Serialize;

use super::conversation::{PamNotice, PamPrompt};

/// `mantle.lock`'s payload (ADR-0052).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LockState {
    /// The Renderer confirmed the session locked; a requested lock stays `false` until then.
    pub active: bool,
    /// PAM's pending question (ADR-0381), or `nil` while it owes nothing.
    pub prompt: Option<PamPrompt>,
    /// PAM's info and error lines, oldest first, at most 8. Cleared by the next answer and when
    /// the lock ends.
    pub messages: Vec<PamNotice>,
    /// An answer is with PAM, or typed before its prompt and held for it. A second submit is
    /// refused while true.
    pub authenticating: bool,
    /// Rejected passwords since this lock was confirmed; reset by the next lock.
    pub attempts: u32,
    /// Last failure to draw: PAM's verdict (`"authentication failed"`, `"too many attempts"`, or a
    /// PAM or worker error) or a refused lock's reason. Cleared by a correct password, `lock`, a
    /// confirmed lock, and unlock.
    pub error: String,
    /// The fingerprint conversation beside the password one (ADR-0382), or `nil` while none runs:
    /// no `mantle-fingerprint` stack, or it ended without a match. A password submit and the next
    /// lock start it again. `prompt`, `messages`, `attempts` and `error` stay the password's.
    pub fingerprint: Option<FingerprintState>,
    /// PAM said yes and the lock is still up: the window for an out-animation (ADR-0190).
    pub unlocking: bool,
    /// `SetSessionLock { locked: true }` is in flight before Renderer confirmation.
    /// `#[serde(skip)]`: bookkeeping, not payload. `active` must mean only Renderer confirmation,
    /// but a respawn must still retake a lock that was asked for and not yet confirmed (ADR-0058).
    #[serde(skip)]
    pub requested: bool,
    /// Acquisition number, bumped only on Renderer `Locked`. `#[serde(skip)]` like `requested`.
    /// A delayed release names it, so a lock that ended while the timer slept is not the one
    /// released (ADR-0190). PAM answers are bound by their conversation's serial instead.
    #[serde(skip)]
    pub acquisition: u64,
}

/// `mantle.lock.fingerprint` while its conversation runs (ADR-0382).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FingerprintState {
    /// pam_fprintd's lines, such as "Place your finger" or "Failed to match", oldest first, at
    /// most 8. Translated: show them, never match on them.
    pub messages: Vec<PamNotice>,
}
