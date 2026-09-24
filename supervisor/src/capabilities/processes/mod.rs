//! `mantle.processes` owns the programs declared with `session_process`: long-running things whose
//! lifetime is the shell's rather than a generation's.
//!
//! Sibling of `storage` in shape -- a config declares a name, the Supervisor owns what sits behind
//! it, and the state comes back keyed by that name -- and its opposite in what it holds. `storage`
//! keeps a file the config could have read itself; this keeps a handle the config *cannot* hold,
//! because the VM holding it goes with any Renderer replacement.

pub mod controller;

pub use controller::{ProcessesController, ProcessesSignal};

use nix::sys::signal::Signal;

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProcessesAction {
    /// Registers `name` (required before `start`) and sets its stop signal, default `TERM`.
    /// Redeclaring updates the signal without touching a running program.
    Declare {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        name: String,
        #[serde(default)]
        stop_signal: Option<SignalName>,
    },
    /// Runs `cmd` with `args` (no shell) as its own process group. No-op while `running` or when
    /// `name` is undeclared.
    Start {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        name: String,
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        cmd: String,
        #[serde(default, deserialize_with = "crate::capabilities::lua_list")]
        args: Vec<String>,
    },
    /// Sends `signal` to the program's process (not its group); no-op when not running.
    Signal {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        name: String,
        signal: SignalName,
    },
    /// Sends the declared stop signal to the process group, then `KILL` if it is still up 5 s
    /// later; no-op when not running.
    Stop {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        name: String,
    },
}

/// A signal name without the `SIG` prefix.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "UPPERCASE")]
pub enum SignalName {
    Term,
    Int,
    Hup,
    Quit,
    Usr1,
    Usr2,
    Kill,
    Stop,
    Cont,
}

impl From<SignalName> for Signal {
    fn from(name: SignalName) -> Self {
        match name {
            SignalName::Term => Signal::SIGTERM,
            SignalName::Int => Signal::SIGINT,
            SignalName::Hup => Signal::SIGHUP,
            SignalName::Quit => Signal::SIGQUIT,
            SignalName::Usr1 => Signal::SIGUSR1,
            SignalName::Usr2 => Signal::SIGUSR2,
            SignalName::Kill => Signal::SIGKILL,
            SignalName::Stop => Signal::SIGSTOP,
            SignalName::Cont => Signal::SIGCONT,
        }
    }
}

/// `mantle.processes` action dispatch (ADR-0037). Synchronous: each action touches the entry map
/// and hands the work to the per-program task, which is where every await lives.
pub fn dispatch(controller: &ProcessesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<ProcessesAction>(&envelope.params) else { return };
    match action {
        ProcessesAction::Declare { name, stop_signal } => {
            controller.declare(&name, stop_signal.map_or(Signal::SIGTERM, Signal::from))
        }
        ProcessesAction::Start { name, cmd, args } => controller.start(&name, &cmd, &args),
        ProcessesAction::Signal { name, signal } => controller.signal(&name, signal.into()),
        ProcessesAction::Stop { name } => controller.stop(&name),
    }
}
