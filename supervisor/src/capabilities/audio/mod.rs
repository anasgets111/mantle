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
mod virtual_devices;

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use mixer::{AudioCommand, AudioCommandSender};
use shared::action::AudioAction;
use tokio::sync::mpsc::UnboundedSender;

/// The mixer thread's command channel and the virtual devices' task.
pub struct Audio {
    commands: AudioCommandSender,
    virtual_devices: tokio::sync::mpsc::UnboundedSender<virtual_devices::Request>,
    /// The generation whose `set_level_meters` runs the meters, or [`NO_OWNER`].
    meters_owner: AtomicU64,
    /// The virtual devices' conf, removed here on exit: the task is cancelled, not finished.
    conf: Option<std::path::PathBuf>,
}

/// No generation runs the meters; generation ids are `u32`.
const NO_OWNER: u64 = u64::MAX;

impl Audio {
    /// Starts the PipeWire mixer thread and the virtual devices' task.
    pub fn start(
        updates: UnboundedSender<mixer::AudioState>,
        privacy_updates: tokio::sync::watch::Sender<mixer::PrivacySources>,
        level_updates: UnboundedSender<shared::state::audio_levels::AudioLevelsState>,
    ) -> Self {
        let (commands, receiver) = mixer::command_channel();
        let virtual_devices = virtual_devices::spawn(commands.clone());
        let restarts = virtual_devices.clone();
        std::thread::spawn(move || mixer::run(updates, privacy_updates, level_updates, receiver, restarts));
        Self { commands, virtual_devices, meters_owner: AtomicU64::new(NO_OWNER), conf: virtual_devices::conf_path() }
    }

    /// Removes the virtual devices and stops the meters `generation` asked for, unless a newer one
    /// asked again.
    pub fn forget(&self, generation: u32) {
        let _ = self.virtual_devices.send(virtual_devices::Request::Forget(generation));
        if self.meters_owner.compare_exchange(u64::from(generation), NO_OWNER, SeqCst, SeqCst).is_ok() {
            let _ = self.commands.send(AudioCommand::SetLevelMeters(Vec::new()));
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        if let Some(conf) = &self.conf {
            let _ = std::fs::remove_file(conf);
        }
    }
}

/// Unlike every other capability's adapter, this has no controller to call. It dispatches each
/// action as an [`AudioCommand`] on the PipeWire thread (ADR-0037); no result is awaited here.
/// Lua volumes are percents; PipeWire's are fractions.
///
/// `play_sound(sound)` and `set_event_sounds_enabled(en)` are not actions: they'd need a sound
/// player, event-sound theme, and toggle storage, none of which exists.
pub fn dispatch(audio: &Audio, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<AudioAction>(&envelope.params) else { return };
    let sent = match command(action) {
        Ok(command) => {
            if matches!(command, AudioCommand::SetLevelMeters(_)) {
                audio.meters_owner.store(u64::from(envelope.params.generation_id), SeqCst);
            }
            audio.commands.send(command).is_ok()
        }
        Err(devices) => {
            let generation = envelope.params.generation_id;
            audio.virtual_devices.send(virtual_devices::Request::Declare { generation, devices }).is_ok()
        }
    };
    if !sent {
        shared::error!("the audio command channel is closed; {} was dropped", envelope.params.action);
    }
}

/// The mixer's command, or the devices for `virtual_devices`, which partitions them.
fn command(action: AudioAction) -> Result<AudioCommand, Vec<shared::action::VirtualDevice>> {
    Ok(match action {
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
        AudioAction::SetAppTarget { id, device } => AudioCommand::SetAppTarget { id, device },
        AudioAction::SetCardProfile { device, index } => AudioCommand::SetCardProfile { device, index },
        AudioAction::SetPort { card, index } => AudioCommand::SetPort { card, index },
        AudioAction::SetVirtualDevices { devices } => return Err(devices),
        AudioAction::SetLevelMeters { ids } => AudioCommand::SetLevelMeters(ids),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The meters belong to the last generation that set them: a departed older one leaves them,
    /// and the owner's departure stops them once.
    #[test]
    fn only_the_generation_that_set_the_meters_stops_them_on_departure() {
        pipewire::init();
        let main_loop = pipewire::main_loop::MainLoopRc::new(None).unwrap();
        let (commands, receiver) = mixer::command_channel();
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let into = std::rc::Rc::clone(&sent);
        let _attached = receiver.attach(main_loop.loop_(), move |command| into.borrow_mut().push(command));
        let (virtual_devices, _requests) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("devices.conf");
        std::fs::write(&conf, "").unwrap();
        let audio =
            Audio { commands, virtual_devices, meters_owner: AtomicU64::new(NO_OWNER), conf: Some(conf.clone()) };
        let meter = |generation_id| shared::CommandEnvelope {
            params: shared::CommandParams {
                generation_id,
                capability: "audio".into(),
                action: "set_level_meters".into(),
                arguments: vec![serde_json::json!([59])],
                legacy_expected_revision: 0,
            },
            id: 0,
        };
        let drain = || {
            main_loop.loop_().iterate(pipewire::loop_::Timeout::Finite(std::time::Duration::from_millis(50)));
            std::mem::take(&mut *sent.borrow_mut())
        };

        dispatch(&audio, &meter(1));
        dispatch(&audio, &meter(2));
        audio.forget(1);
        assert_eq!(drain(), [AudioCommand::SetLevelMeters(vec![59]), AudioCommand::SetLevelMeters(vec![59])]);
        audio.forget(2);
        audio.forget(2);
        assert_eq!(drain(), [AudioCommand::SetLevelMeters(Vec::new())], "the owner's departure stops them once");

        drop(audio);
        assert!(!conf.exists(), "dropping the audio capability removes its conf");
    }

    #[test]
    fn volumes_arrive_as_percents_and_leave_as_fractions() {
        let decode = |action, arguments: &[serde_json::Value]| {
            command(shared::action::decode(action, arguments).unwrap()).unwrap()
        };
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
        assert_eq!(decode("set_port", &[51.into(), 2.into()]), AudioCommand::SetPort { card: 51, index: 2 });
        assert_eq!(
            decode("set_app_target", &[101.into(), 59.into()]),
            AudioCommand::SetAppTarget { id: 101, device: Some(59) }
        );
        let back = AudioCommand::SetAppTarget { id: 101, device: None };
        assert_eq!(decode("set_app_target", &[101.into(), serde_json::Value::Null]), back, "nil clears");
        assert_eq!(decode("set_app_target", &[101.into()]), back, "and so does leaving it out");
    }
}
