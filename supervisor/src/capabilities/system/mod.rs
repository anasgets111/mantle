//! `mantle.system` provides reactive wall-clock time.
//!
//! Read-only. Persisted state lives in `mantle.storage`, where config names the file (ADR-0136).

pub mod controller;

pub use controller::{SystemController, SystemSignal};
