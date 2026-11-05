//! PipeWire-backed audio state. `mixer` tracks the registry lists; `master` holds the pure
//! volume/mute parsing and default-device resolution (ADR-0053 decision 3); `route` builds and
//! extracts ALSA hardware `Route` pods.
//!
//! Write actions dispatch here; see [`dispatch`] for the two unbuilt actions.
//!
//! `profile` parses ALSA and BlueZ card profiles. BlueZ codecs live here too, because PipeWire, not
//! BlueZ, picks the codec: each BlueZ device's profiles are its codecs (ADR-0030).

pub mod master;
pub mod mixer;
mod profile;
mod route;

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
        AudioAction::SetVolume { id, volume } => AudioCommand::SetVolume { id, volume: volume / 100.0 },
        AudioAction::SetMuted { id, muted } => AudioCommand::SetMuted { id, muted },
        AudioAction::ToggleMute { id } => AudioCommand::ToggleMute(id),
        AudioAction::SetBalance { id, balance } => AudioCommand::SetBalance { id, balance },
        AudioAction::SetDefaultSink { id } => AudioCommand::SetDefaultSink(id),
        AudioAction::SetDefaultSource { id } => AudioCommand::SetDefaultSource(id),
        AudioAction::SetChannelVolume { id, index, volume } => {
            AudioCommand::SetChannelVolume { id, index, volume: volume / 100.0 }
        }
        AudioAction::SetAppVolume { id, volume } => AudioCommand::SetAppVolume { id, volume: volume / 100.0 },
        AudioAction::SetAppMuted { id, muted } => AudioCommand::SetAppMuted { id, muted },
        AudioAction::SetCardProfile { device, index } => AudioCommand::SetCardProfile { device, index },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_arrive_as_percents_and_leave_as_fractions() {
        let decode =
            |action, arguments: &[serde_json::Value]| command(shared::action::decode(action, arguments).unwrap());
        assert_eq!(decode("set_volume", &[59.into(), 150.into()]), AudioCommand::SetVolume { id: 59, volume: 1.5 });
        assert_eq!(
            decode("set_channel_volume", &[7.into(), 1.into(), 150.into()]),
            AudioCommand::SetChannelVolume { id: 7, index: 1, volume: 1.5 }
        );
        assert_eq!(decode("toggle_mute", &[60.into()]), AudioCommand::ToggleMute(60));
        assert_eq!(decode("set_muted", &[60.into(), true.into()]), AudioCommand::SetMuted { id: 60, muted: true });
        assert_eq!(
            decode("set_app_volume", &[7.into(), 25.into()]),
            AudioCommand::SetAppVolume { id: 7, volume: 0.25 }
        );
        assert_eq!(
            decode("set_card_profile", &[51.into(), 3.into()]),
            AudioCommand::SetCardProfile { device: 51, index: 3 }
        );
    }
}
