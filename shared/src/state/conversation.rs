//! PAM conversation types `mantle.lock` and `mantle.polkit` share.

use serde::{Deserialize, Serialize};

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
