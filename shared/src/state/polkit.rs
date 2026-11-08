//! `mantle.polkit` snapshot payload.

use serde::Serialize;

use super::conversation::{PamNotice, PamPrompt};

/// `mantle.polkit`'s payload (ADR-0114). Every other field is empty while `active` is false.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PolkitState {
    /// polkitd is waiting for the user to authenticate.
    pub active: bool,
    /// The action's prompt, e.g. `"Authentication is required to ..."`, in `en_US`: the locale the
    /// agent registers with.
    pub message: String,
    /// Action being authorized, e.g. `org.freedesktop.systemd1.manage-units`.
    pub action_id: String,
    /// Themed icon name, or empty when the caller set none.
    pub icon_name: String,
    /// The accounts polkit accepts, as it offered them; groups arrive already expanded.
    pub identities: Vec<PolkitIdentity>,
    /// The uid the next helper authenticates: this session's user if offered, else the first.
    pub identity: u32,
    /// The helper's pending question, as on `mantle.lock`. `nil` until the first submit starts it.
    pub prompt: Option<PamPrompt>,
    /// The helper's info and error lines, as on `mantle.lock`.
    pub messages: Vec<PamNotice>,
    /// An answer is with PAM, or typed before its prompt and held for it. A second submit is
    /// refused while true.
    pub authenticating: bool,
    /// Drawable reason for the last failure, e.g. `"authentication failed"`. The prompt stays open to retry.
    pub error: String,
}

/// One account a polkit challenge accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PolkitIdentity {
    pub uid: u32,
    /// Login name, or empty when the uid has no passwd entry.
    pub name: String,
}
