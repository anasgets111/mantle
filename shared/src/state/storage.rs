//! `mantle.storage` snapshot payload.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// `mantle.storage` payload (ADR-0136).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StorageState {
    /// Each declared `persistent_table`'s contents, keyed by its absolute file path; `nil` until
    /// declared. Another writer's change to the file replaces it, unsaved writes included.
    pub files: BTreeMap<String, Value>,
}
