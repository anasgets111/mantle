//! `mantle.system` provides reactive wall-clock time.
//!
//! Read-only. Persisted state lives in `mantle.storage`, where config names the file (ADR-0136).
//!
//! `system:find_icon` remains an undispatched IDL row because nothing calls it yet.

pub mod controller;

pub use controller::{SystemController, SystemSignal};
