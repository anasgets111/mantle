//! `obelisk.lock`: session-lock commands and the lock-screen state (ADR-0042, ADR-0052 decisions 1
//! and 4).
//!
//! The Renderer holds `ext_session_lock_v1` and paints it. This owns acquisition, outcome state,
//! and the only unlock call site. [`LockEvent`]s go through pure [`state::apply`], testable without
//! socket or PAM.

pub mod controller;
pub mod flag;
pub mod logind;
pub mod state;

pub use controller::{LockController, dispatch};
pub use flag::SessionLockedFlag;
pub(crate) use state::error_for_outcome;
pub use state::{LockEvent, SessionLock, compositor_lock_change};
