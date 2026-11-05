//! `mantle.lock` snapshot payload.

use serde::{Deserialize, Serialize};

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

/// A question a PAM module asked (ADR-0381).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PamPrompt {
    /// Unique for this session, so two identical prompts in a row still differ.
    pub id: u64,
    /// PAM's own text, e.g. `"Password: "` or `"Verification code:"`.
    pub text: String,
    /// The answer is meant to be seen: set the secure field's `masked` to `not echo`.
    pub echo: bool,
}

/// What a `PamNotice` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PamNoticeKind {
    Info,
    Error,
}

/// One line a PAM module showed without asking anything, e.g. a fingerprint hint or a lockout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PamNotice {
    /// `"info"` or `"error"`.
    pub kind: PamNoticeKind,
    /// What the module said, as it said it.
    pub text: String,
}
