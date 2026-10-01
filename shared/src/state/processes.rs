//! `mantle.processes` snapshot payload.

use serde::Serialize;
use std::collections::BTreeMap;

/// `mantle.processes` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessesState {
    /// One entry per `session_process` name; an undeclared name is `nil`.
    pub sessions: BTreeMap<String, SessionProcess>,
}

/// One declared program: its current run, or what is left of its last one.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionProcess {
    /// Whether it is up now. Otherwise the fields below describe the last run.
    pub running: bool,
    /// Process id, also its process group id; kept after exit, `nil` before a spawn or after a failed `start`.
    pub pid: Option<u32>,
    /// Unix seconds when the run began; `nil` before a spawn or after a failed `start`.
    pub started_at: Option<i64>,
    /// Exit status of the last run; `nil` while running, before any run, or when a signal
    /// killed it.
    pub exit_code: Option<i32>,
    /// Why the last `start` failed to spawn, e.g. a `cmd` not on `PATH`; empty when it spawned.
    pub start_error: String,
}
