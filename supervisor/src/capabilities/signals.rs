use shared::Capability;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::applications::ApplicationsSignal;
use super::audio::mixer::AudioState;
use super::battery::BatterySignal;
use super::bluetooth::BluetoothState;
use super::brightness::BrightnessSignal;
use super::files::FilesSignal;
use super::idle::IdleState;
use super::keyboard::KeyboardState;
use super::mpris::MprisSignal;
use super::network::NetworkState;
use super::notifications::NotificationsSignal;
use super::power::PowerSignal;
use super::privacy::PrivacySignal;
use super::processes::ProcessesSignal;
use super::storage::StorageSignal;
use super::sysinfo::SysinfoSignal;
use super::system::SystemSignal;
use super::tray::TraySignal;
use super::updates::UpdatesSignal;
use super::windows::WindowsSignal;
use super::workspaces::WorkspacesSignal;

/// A received signal waiting for [`Capabilities::push`](super::Capabilities::push).
/// [`Signals::next`] only awaits `recv()`, so losing the `tokio::select!` race drops no signal.
#[derive(Debug)]
pub enum Signal {
    /// Carries mixer state directly, not a controller name.
    Audio(AudioState),
    /// Worker capabilities send finished state, like `Audio`.
    Network(NetworkState),
    Bluetooth(BluetoothState),
    Tray,
    Mpris,
    Notifications,
    Sysinfo,
    Keyboard(KeyboardState),
    Battery,
    Brightness,
    Workspaces,
    Windows,
    Power,
    Applications,
    Files,
    Storage,
    Processes,
    System,
    Privacy,
    Updates,
    /// Carries inhibitor state directly; no controller reads it back (ADR-0141).
    Idle(IdleState),
}

/// The newest value queued on `rx`. Cancel-safe: `recv` is the only await.
async fn recv_latest<T>(rx: &mut UnboundedReceiver<T>) -> Option<T> {
    let mut latest = rx.recv().await?;
    while let Ok(newer) = rx.try_recv() {
        latest = newer;
    }
    Some(latest)
}

/// Declares each capability channel once, generating [`Signals`], [`Senders`], cancel-safe
/// [`Signals::next`], and their constructor.
///
/// One macro replaces four hand-kept lists. Adding `shared::Capability` then fails `start` and
/// `dispatch`; filling them without a signal path would otherwise build while Lua reads `nil`, the
/// silent failure ADR-0076 removes.
///
/// Every roster variant appears in `channels` or `without_channel`; the exhaustive helper makes a
/// missing row an `E0004` here. `Signal` stays hand-written because variants choose their payload;
/// per-variant doc comments would flatten to punctuation in a macro row, while
/// [`Capabilities::push`](super::Capabilities::push) guards the mapping exhaustively.
///
/// The chain is exhaustive: a new roster variant fails `start`, `dispatch`, and the channel helper;
/// its channel row then fails on `Signal`, and that variant fails `push`. Each compiler error names
/// the next required edit instead of allowing silent loss after the second.
macro_rules! capability_channels {
    (
        channels {
            $($variant:ident => $field:ident : $payload:ty, $pattern:pat => $signal:expr;)+
        }
        without_channel { $($no_channel:ident),* $(,)? }
    ) => {
        /// Receiving half of each capability channel.
        pub struct Signals {
            $($field: UnboundedReceiver<$payload>,)+
        }

        /// Sending half handed to controllers.
        pub(super) struct Senders {
            $(pub(super) $field: UnboundedSender<$payload>,)+
        }

        impl Senders {
            /// Builds both halves for each channel-bearing roster entry.
            pub(super) fn channels() -> (Self, Signals) {
                // Bind both halves once, then partially move each into its struct.
                $(let $field = unbounded_channel();)+
                (Self { $($field: $field.0,)+ }, Signals { $($field: $field.1,)+ })
            }
        }

        impl Signals {
            /// Awaits the first signal, folding whatever else its channel has queued into it: each
            /// push reads current state, so a burst of install output lines is one push, not
            /// hundreds. [`recv_latest`] is cancel-safe; `None` requires all senders to drop, which
            /// cannot happen while [`Capabilities`](super::Capabilities) lives.
            pub async fn next(&mut self) -> Option<Signal> {
                tokio::select! {
                    $($pattern = recv_latest(&mut self.$field) => Some($signal),)+
                    else => None,
                }
            }
        }

        /// Exhaustiveness guard for roster entries without a channel.
        #[allow(dead_code)]
        fn every_capability_has_a_channel_row(capability: Capability) {
            match capability {
                $(Capability::$variant => {})+
                $(Capability::$no_channel => {})*
            }
        }
    };
}

capability_channels! {
    channels {
        // Mixer sends state directly (see `Signal::Audio`).
        Audio => audio: AudioState, Some(state) => Signal::Audio(state);
        // Workers send finished state (see `spawn_worker`).
        Network => network: NetworkState, Some(state) => Signal::Network(state);
        Bluetooth => bluetooth: BluetoothState, Some(state) => Signal::Bluetooth(state);
        // Other `Changed` signals collapse to unit `Signal` variants.
        Tray => tray: TraySignal, Some(TraySignal::RegistryChanged) => Signal::Tray;
        Mpris => mpris: MprisSignal, Some(MprisSignal::Changed) => Signal::Mpris;
        Notifications => notifications: NotificationsSignal,
            Some(NotificationsSignal::Changed) => Signal::Notifications;
        Sysinfo => sysinfo: SysinfoSignal, Some(SysinfoSignal::Changed) => Signal::Sysinfo;
        Keyboard => keyboard: KeyboardState, Some(state) => Signal::Keyboard(state);
        Privacy => privacy: PrivacySignal, Some(PrivacySignal::Changed) => Signal::Privacy;
        Updates => updates: UpdatesSignal, Some(UpdatesSignal::Changed) => Signal::Updates;
        Battery => battery: BatterySignal, Some(BatterySignal::Changed) => Signal::Battery;
        System => system: SystemSignal, Some(SystemSignal::Changed) => Signal::System;
        Brightness => brightness: BrightnessSignal, Some(BrightnessSignal::Changed) => Signal::Brightness;
        Workspaces => workspaces: WorkspacesSignal, Some(WorkspacesSignal::Changed) => Signal::Workspaces;
        Windows => windows: WindowsSignal, Some(WindowsSignal::Changed) => Signal::Windows;
        Power => power: PowerSignal, Some(PowerSignal::Changed) => Signal::Power;
        Applications => applications: ApplicationsSignal,
            Some(ApplicationsSignal::Changed) => Signal::Applications;
        Files => files: FilesSignal, Some(FilesSignal::Changed) => Signal::Files;
        Storage => storage: StorageSignal, Some(StorageSignal::Changed) => Signal::Storage;
        Processes => processes: ProcessesSignal, Some(ProcessesSignal::Changed) => Signal::Processes;
            // `idle/controller.rs`'s inhibitor watch owns and sends state, like `Audio` (ADR-0141).
        Idle => idle: IdleState, Some(state) => Signal::Idle(state);
    }
    // `lock` has no channel: boot-built in `main.rs` for relock (ADR-0060), it reports through
    // existing `LockOutcome` frames, not a `StateSnapshot` (ADR-0052 decision 4).
    without_channel { Lock, Polkit }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_burst_on_one_channel_is_one_signal_carrying_the_newest_state() {
        let (senders, mut signals) = Senders::channels();
        for n in 1..=3 {
            senders.updates.send(UpdatesSignal::Changed).unwrap();
            senders.keyboard.send(KeyboardState { backlight_pct: n, ..KeyboardState::default() }).unwrap();
        }
        let mut seen = vec![signals.next().await.unwrap(), signals.next().await.unwrap()];
        seen.sort_by_key(|signal| matches!(signal, Signal::Updates));
        assert!(matches!(&seen[0], Signal::Keyboard(state) if state.backlight_pct == 3), "{seen:?}");
        assert!(matches!(seen[1], Signal::Updates));
        let pending = tokio::time::timeout(std::time::Duration::from_millis(10), signals.next()).await;
        assert!(pending.is_err(), "the burst must leave nothing queued");
    }
}
