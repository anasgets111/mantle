//! `mantle.secrets.rs` snapshot payload.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
/// Status of one named write. `unavailable` includes a missing, locked, or failing service;
/// `timed_out` means the 30-second cancellation was requested and the write may still finish.
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
