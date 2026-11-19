//! `mantle.lock`: session-lock commands and the lock-screen state (ADR-0042, ADR-0052 decisions 1
//! and 4).
//!
//! The Renderer holds `ext_session_lock_v1` and paints it; `Supervisor` decides every
//! `SetSessionLock`. This owns `mantle.lock` and its PAM conversations. [`LockEvent`]s go through
//! pure [`state::apply`], testable without socket or PAM.

pub mod controller;
pub mod flag;
pub mod logind;
pub(crate) mod session;
pub mod sleep;
pub mod state;

pub use controller::{LockController, dispatch};
pub use flag::SessionLockedFlag;
pub use state::LockEvent;
pub(crate) use state::error_for_outcome;
