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
/// Lua volumes are percents; PipeWire's are fractions.
///
/// `play_sound(sound)` and `set_event_sounds_enabled(en)` are not actions: they'd need a sound
/// player, event-sound theme, and toggle storage, none of which exists.
pub fn dispatch(commands: &AudioCommandSender, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<AudioAction>(&envelope.params) else { return };
    if commands.send(command(action)).is_err() {
        shared::error!("the PipeWire command channel is closed; {} was dropped", envelope.params.action);
    }
}

fn command(action: AudioAction) -> AudioCommand {
    match action {
        AudioAction::SetVolume { volume } => AudioCommand::SetMasterVolume(volume / 100.0),
        AudioAction::SetMuted { muted } => AudioCommand::SetMasterMuted(muted),
        AudioAction::ToggleMute => AudioCommand::ToggleMasterMute,
        AudioAction::SetBalance { balance } => AudioCommand::SetBalance(balance),
        AudioAction::SetDefaultSink { id } => AudioCommand::SetDefaultSink(id),
        AudioAction::SetDefaultSource { id } => AudioCommand::SetDefaultSource(id),
        AudioAction::SetSinkChannelVolume { id, index, volume } => {
            AudioCommand::SetSinkChannelVolume { id, index, volume: volume / 100.0 }
        }
        AudioAction::SetSourceChannelVolume { id, index, volume } => {
            AudioCommand::SetSourceChannelVolume { id, index, volume: volume / 100.0 }
        }
        AudioAction::SetSourceVolume { volume } => AudioCommand::SetSourceVolume(volume / 100.0),
        AudioAction::SetSourceMuted { muted } => AudioCommand::SetSourceMuted(muted),
        AudioAction::ToggleSourceMute => AudioCommand::ToggleSourceMute,
        AudioAction::SetAppVolume { id, volume } => AudioCommand::SetAppVolume { id, volume: volume / 100.0 },
        AudioAction::SetAppMuted { id, muted } => AudioCommand::SetAppMuted { id, muted },
        AudioAction::SetBluetoothProfile { device, index } => AudioCommand::SetBluetoothProfile { device, index },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_arrive_as_percents_and_leave_as_fractions() {
        let decode =
            |action, arguments: &[serde_json::Value]| command(shared::action::decode(action, arguments).unwrap());
        assert_eq!(decode("set_volume", &[150.into()]), AudioCommand::SetMasterVolume(1.5));
        assert_eq!(decode("set_source_volume", &[50.into()]), AudioCommand::SetSourceVolume(0.5));
        assert_eq!(
            decode("set_sink_channel_volume", &[7.into(), 1.into(), 150.into()]),
            AudioCommand::SetSinkChannelVolume { id: 7, index: 1, volume: 1.5 }
        );
        assert_eq!(
            decode("set_source_channel_volume", &[8.into(), 0.into(), 40.into()]),
            AudioCommand::SetSourceChannelVolume { id: 8, index: 0, volume: 0.4 }
        );
        assert_eq!(
            decode("set_app_volume", &[7.into(), 25.into()]),
            AudioCommand::SetAppVolume { id: 7, volume: 0.25 }
        );
    }
}
