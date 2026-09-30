//! `mantle.processes` owns the programs declared with `session_process`: long-running things whose
//! lifetime is the shell's rather than a generation's.
//!
//! Sibling of `storage` in shape -- a config declares a name, the Supervisor owns what sits behind
//! it, and the state comes back keyed by that name -- and its opposite in what it holds. `storage`
//! keeps a file the config could have read itself; this keeps a handle the config *cannot* hold,
//! because the VM holding it goes with any Renderer replacement.

pub mod controller;

pub use controller::ProcessesController;

use nix::sys::signal::Signal;
use shared::action::{ProcessesAction, SignalName};

fn signal_of(name: SignalName) -> Signal {
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

/// `mantle.processes` action dispatch (ADR-0037). Synchronous: each action touches the entry map
/// and hands the work to the per-program task, which is where every await lives.
pub fn dispatch(controller: &ProcessesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<ProcessesAction>(&envelope.params) else { return };
    match action {
        ProcessesAction::Declare { name, stop_signal } => {
            controller.declare(&name, stop_signal.map_or(Signal::SIGTERM, signal_of))
        }
        ProcessesAction::Start { name, cmd, args } => controller.start(&name, &cmd, &args),
        ProcessesAction::Signal { name, signal } => controller.signal(&name, signal_of(signal)),
        ProcessesAction::Stop { name } => controller.stop(&name),
    }
}
