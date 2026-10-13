//! `mantle.secrets.rs` snapshot payload.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
/// Status of one named write. `unavailable` is a missing or failing service, a dismissed prompt, or
/// a service that exits mid-prompt; `timed_out` is a service call over 25 seconds, is final, and
/// the name accepts a new write.
#[serde(rename_all = "snake_case")]
pub enum SecretStatus {
    Pending,
    Stored,
    Unavailable,
    TimedOut,
}

#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SecretsState {
    /// Public lookup names and the result of their latest write.
    pub entries: BTreeMap<String, SecretStatus>,
}
