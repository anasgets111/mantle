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
    use crate::capabilities::test_support::{dispatcher, envelope};
    use pipewire::main_loop::MainLoopRc;
    use serde_json::json;
    use shared::Capability;
    use virtual_devices::Request;

    /// The mixer end of [`Audio::commands`]. It is a `pipewire::channel`, readable only from a loop,
    /// so this is a local loop with no context: it never reaches a PipeWire daemon.
    struct Mixer<'l> {
        main_loop: &'l MainLoopRc,
        sent: std::rc::Rc<std::cell::RefCell<Vec<AudioCommand>>>,
        _attached: pipewire::channel::AttachedReceiver<'l, AudioCommand>,
    }

    impl Mixer<'_> {
        /// What `dispatch` sent; it sends synchronously, so one iteration reads it all.
        fn drain(&self) -> Vec<AudioCommand> {
            self.main_loop.loop_().iterate(pipewire::loop_::Timeout::Finite(std::time::Duration::from_millis(50)));
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    fn rig(
        main_loop: &MainLoopRc,
        conf: Option<std::path::PathBuf>,
    ) -> (Audio, Mixer<'_>, tokio::sync::mpsc::UnboundedReceiver<Request>) {
        let (commands, receiver) = mixer::command_channel();
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let into = std::rc::Rc::clone(&sent);
        let _attached = receiver.attach(main_loop.loop_(), move |command| into.borrow_mut().push(command));
        let (virtual_devices, requests) = tokio::sync::mpsc::unbounded_channel();
        let audio = Audio { commands, virtual_devices, meters_owner: AtomicU64::new(NO_OWNER), conf };
        (audio, Mixer { main_loop, sent, _attached }, requests)
    }

    fn main_loop() -> MainLoopRc {
        pipewire::init();
        MainLoopRc::new(None).unwrap()
    }

    fn at(generation_id: u32, action: &str, arguments: Vec<serde_json::Value>) -> shared::CommandEnvelope {
        let mut envelope = envelope(Capability::Audio, action, arguments);
        envelope.params.generation_id = generation_id;
        envelope
    }

    /// The meters belong to the last generation that set them: a departed older one leaves them,
    /// and the owner's departure stops them once.
    #[test]
    fn only_the_generation_that_set_the_meters_stops_them_on_departure() {
        let main_loop = main_loop();
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("devices.conf");
        std::fs::write(&conf, "").unwrap();
        let (audio, mixer, _requests) = rig(&main_loop, Some(conf.clone()));
        let meter = |generation| at(generation, "set_level_meters", vec![json!([59])]);

        dispatch(&audio, &meter(1));
        dispatch(&audio, &at(2, "set_volume", vec![59.into(), 50.into()]));
        audio.forget(2);
        let volume = AudioCommand::SetVolume { id: 59, volume: 0.5 };
        assert_eq!(mixer.drain(), [AudioCommand::SetLevelMeters(vec![59]), volume], "another write takes no meters");
        dispatch(&audio, &meter(2));
        audio.forget(1);
        assert_eq!(mixer.drain(), [AudioCommand::SetLevelMeters(vec![59])]);
        audio.forget(2);
        audio.forget(2);
        assert_eq!(mixer.drain(), [AudioCommand::SetLevelMeters(Vec::new())], "the owner's departure stops them once");

        drop(audio);
        assert!(!conf.exists(), "dropping the audio capability removes its conf");
    }

    /// Each mixer action reaches the mixer as its command, percents as fractions; clamping is the
    /// mixer's (`master`).
    #[test]
    fn dispatch_sends_each_mixer_action_as_its_command() {
        let main_loop = main_loop();
        let (audio, mixer, mut requests) = rig(&main_loop, None);
        let send = dispatcher(Capability::Audio, dispatch, &audio);
        send("set_volume", vec![59.into(), 150.into()]);
        send("set_muted", vec![60.into(), true.into()]);
        send("toggle_mute", vec![61.into()]);
        send("set_balance", vec![62.into(), (-0.5).into()]);
        send("set_default_sink", vec![63.into()]);
        send("set_default_source", vec![64.into()]);
        send("set_channel_volume", vec![65.into(), 1.into(), 150.into()]);
        send("set_app_volume", vec![66.into(), 25.into()]);
        send("set_app_muted", vec![67.into(), true.into()]);
        send("set_app_target", vec![68.into(), 59.into()]);
        send("set_app_target", vec![69.into(), serde_json::Value::Null]);
        send("set_app_target", vec![70.into()]);
        send("set_card_profile", vec![71.into(), 3.into()]);
        send("set_port", vec![72.into(), 2.into()]);
        send("set_level_meters", vec![json!([73, 74])]);
        assert_eq!(
            mixer.drain(),
            [
                AudioCommand::SetVolume { id: 59, volume: 1.5 },
                AudioCommand::SetMuted { id: 60, muted: true },
                AudioCommand::ToggleMute(61),
                AudioCommand::SetBalance { id: 62, balance: -0.5 },
                AudioCommand::SetDefaultSink(63),
                AudioCommand::SetDefaultSource(64),
                AudioCommand::SetChannelVolume { id: 65, index: 1, volume: 1.5 },
                AudioCommand::SetAppVolume { id: 66, volume: 0.25 },
                AudioCommand::SetAppMuted { id: 67, muted: true },
                AudioCommand::SetAppTarget { id: 68, device: Some(59) },
                AudioCommand::SetAppTarget { id: 69, device: None },
                AudioCommand::SetAppTarget { id: 70, device: None },
                AudioCommand::SetCardProfile { device: 71, index: 3 },
                AudioCommand::SetPort { card: 72, index: 2 },
                AudioCommand::SetLevelMeters(vec![73, 74]),
            ]
        );
        assert!(requests.try_recv().is_err(), "none of them declares devices");
    }

    /// `set_virtual_devices` goes to the devices' task with its generation, an unknown action goes
    /// nowhere.
    #[test]
    fn virtual_devices_go_to_their_task_and_unknown_actions_nowhere() {
        let main_loop = main_loop();
        let (audio, mixer, mut requests) = rig(&main_loop, None);
        let devices = vec![json!([{ "kind": "null_sink", "name": "a" }])];
        dispatch(&audio, &at(5, "set_virtual_devices", devices));
        dispatch(&audio, &at(5, "play_sound", vec!["bell".into()]));
        dispatch(&audio, &at(5, "toggle_mute", vec![61.into()]));
        assert_eq!(mixer.drain(), [AudioCommand::ToggleMute(61)], "only the known mixer action reaches the mixer");
        let Ok(Request::Declare { generation, devices }) = requests.try_recv() else { panic!("no declaration") };
        let device = shared::action::VirtualDevice {
            kind: shared::action::VirtualKind::NullSink,
            name: "a".into(),
            description: None,
            args: serde_json::Map::new(),
        };
        assert_eq!((generation, devices), (5, vec![device]));
        assert!(requests.try_recv().is_err(), "the unknown action declared nothing");
    }
}
