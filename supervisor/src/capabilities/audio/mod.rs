//! PipeWire-backed audio state. `mixer` tracks the registry lists; `master` holds the pure
//! parsing/resolution logic (ADR-0053 decision 3).
//!
//! Write actions dispatch here, including source-side volume/mute actions that filled the
//! gap beside `set_muted`; see [`dispatch`] for the two still unbuilt actions.
//!
//! BlueZ codec control lives here as well, because PipeWire, not BlueZ, picks the codec: each
//! BlueZ device's profiles are its codecs (ADR-0030).

pub mod master;
pub mod mixer;

use mixer::{AudioCommand, AudioCommandSender};
use shared::action::AudioAction;

/// Unlike every other capability's adapter, this has no controller to call. It dispatches each
/// action as an [`AudioCommand`] on the PipeWire thread (ADR-0037); no result is awaited here.
///
/// `play_sound(sound)` and `set_event_sounds_enabled(en)` are not actions: they'd need a sound
/// player, event-sound theme, and toggle storage, none of which exists.
pub fn dispatch(commands: &AudioCommandSender, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<AudioAction>(&envelope.params) else { return };
    let command = match action {
        AudioAction::SetVolume { volume } => AudioCommand::SetMasterVolume(volume),
        AudioAction::SetMuted { muted } => AudioCommand::SetMasterMuted(muted),
        AudioAction::ToggleMute => AudioCommand::ToggleMasterMute,
        AudioAction::SetBalance { balance } => AudioCommand::SetBalance(balance),
        AudioAction::SetDefaultSink { id } => AudioCommand::SetDefaultSink(id),
        AudioAction::SetDefaultSource { id } => AudioCommand::SetDefaultSource(id),
        AudioAction::SetSourceVolume { volume } => AudioCommand::SetSourceVolume(volume),
        AudioAction::SetSourceMuted { muted } => AudioCommand::SetSourceMuted(muted),
        AudioAction::ToggleSourceMute => AudioCommand::ToggleSourceMute,
        AudioAction::SetAppVolume { id, volume } => AudioCommand::SetAppVolume { id, volume },
        AudioAction::SetAppMuted { id, muted } => AudioCommand::SetAppMuted { id, muted },
        AudioAction::SetBluetoothProfile { device, index } => AudioCommand::SetBluetoothProfile { device, index },
    };
    if commands.send(command).is_err() {
        shared::error!("the PipeWire command channel is closed; {} was dropped", envelope.params.action);
    }
}
