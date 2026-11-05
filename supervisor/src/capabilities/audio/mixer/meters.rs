//! Peak meters for `mantle.audio_levels` (#51): one capture stream per metered sink or source,
//! each buffer reduced to its loudest sample. No metered id, no stream.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::{Object, Pod, Value};
use shared::state::audio_levels::{AudioLevel, AudioLevelsState};
use shared::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;

use super::state::{DefaultDevice, MixerState};
use crate::capabilities::audio::master::serialize_props;
use crate::capabilities::scale::percent_from_fraction;

/// The shortest gap between two pushes: 30 a second.
const INTERVAL: Duration = Duration::from_millis(33);
/// Percentage points a level must move before it is pushed again.
const STEP: f64 = 1.0;
/// The stream's sample rate. With `resample.peaks` each sample is the peak of the graph samples
/// it replaces, so the stream carries 60 values a second rather than 48000.
const RATE: u32 = 60;

/// One metered device's stream. Dropping it destroys the stream and takes its level out.
pub(super) struct Meter {
    id: u32,
    levels: Rc<RefCell<Levels>>,
    _listener: pw::stream::StreamListener<()>,
    stream: pw::stream::StreamRc,
}

impl Meter {
    /// The stream's node id, so privacy can leave this meter out; `SPA_ID_INVALID` until bound.
    pub(super) fn node_id(&self) -> u32 {
        self.stream.node_id()
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.levels.borrow_mut().forget(self.id);
    }
}

/// The metered peaks and the push throttle, shared by every [`Meter`] of one connection.
pub(super) struct Levels {
    updates: UnboundedSender<AudioLevelsState>,
    /// The loudest peak per id since the window opened.
    window: BTreeMap<u32, f64>,
    sent: BTreeMap<u32, f64>,
    last: Option<Instant>,
}

impl Levels {
    pub(super) fn new(updates: UnboundedSender<AudioLevelsState>) -> Self {
        Self { updates, window: BTreeMap::new(), sent: BTreeMap::new(), last: None }
    }

    /// Folds one buffer's `peak` in; pushes once [`INTERVAL`] has passed since the last push and a
    /// level moved by [`STEP`] or more, so a quiet or steady device sends nothing.
    fn record(&mut self, id: u32, peak: f64, now: Instant) {
        let held = self.window.entry(id).or_insert(0.0);
        *held = held.max(peak);
        if self.last.is_some_and(|last| now.duration_since(last) < INTERVAL) {
            return;
        }
        let window = std::mem::take(&mut self.window);
        if window.iter().all(|(id, peak)| self.sent.get(id).is_some_and(|sent| (sent - peak).abs() < STEP)) {
            return;
        }
        self.sent.extend(window);
        self.last = Some(now);
        self.publish();
    }

    fn forget(&mut self, id: u32) {
        self.window.remove(&id);
        if self.sent.remove(&id).is_some() {
            self.publish();
        }
    }

    fn publish(&self) {
        let levels = self.sent.iter().map(|(&id, &peak)| AudioLevel { id, peak }).collect();
        let _ = self.updates.send(AudioLevelsState { levels });
    }
}

/// The loudest little-endian `f32` sample in `bytes`, in percent of full scale.
fn peak(bytes: &[u8]) -> f64 {
    let loudest =
        bytes.as_chunks::<4>().0.iter().map(|sample| f32::from_le_bytes(*sample).abs()).fold(0.0_f32, f32::max);
    percent_from_fraction(f64::from(loudest)).min(100.0)
}

/// Meters exactly `ids`: drops the meters of ids no longer listed, opens one for each new id that
/// names a sink or source, and pushes the list, so the first call or an emptied one answers at once.
/// ponytail: an id that is not a device yet is skipped, not waited for; the config calls again once
/// it sees the device (upgrade: open pending meters as devices appear).
pub(super) fn sync(state: &RefCell<MixerState>, core: &pw::core::CoreRc, levels: &Rc<RefCell<Levels>>, ids: &[u32]) {
    let mut state = state.borrow_mut();
    state.meters.retain(|id, _| ids.contains(id));
    for &id in ids {
        if state.meters.contains_key(&id) {
            continue;
        }
        let Some(kind) = state.kind_of(id) else {
            debug!("set_level_meters: {id} names no sink or source; skipped");
            continue;
        };
        let node_name = state.device_entries(kind)[&id].names.node_name.clone();
        match open(core, levels, id, &node_name, kind == DefaultDevice::Sink) {
            Ok(meter) => {
                state.meters.insert(id, meter);
            }
            Err(err) => warn!("cannot meter {node_name}: {err}"),
        }
    }
    levels.borrow().publish();
}

/// A stereo capture of `node_name` (the peak of both channels, so a hard-panned signal reads at full
/// level; mono would mix them first), its monitor for a sink. `stream.monitor` keeps it out of `apps`;
/// `node.dont-reconnect` lets it end with its device instead of moving to another.
fn open(
    core: &pw::core::CoreRc,
    levels: &Rc<RefCell<Levels>>,
    id: u32,
    node_name: &str,
    sink: bool,
) -> Result<Meter, pw::Error> {
    let mut props = pw::properties::properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "node.name" => "mantle.level-meter",
        "target.object" => node_name,
        "stream.monitor" => "true",
        "resample.peaks" => "true",
        "node.dont-reconnect" => "true",
    };
    if sink {
        props.insert("stream.capture.sink", "true");
    }
    let stream = pw::stream::StreamRc::new(core.clone(), "mantle level meter", props)?;
    let levels_for_process = Rc::clone(levels);
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data().and_then(|bytes| bytes.get(offset..offset.saturating_add(size))) else {
                return;
            };
            levels_for_process.borrow_mut().record(id, peak(bytes), Instant::now());
        })
        .register()?;
    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::F32LE);
    format.set_channels(2);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    position[..2].copy_from_slice(&[pw::spa::sys::SPA_AUDIO_CHANNEL_FL, pw::spa::sys::SPA_AUDIO_CHANNEL_FR]);
    format.set_position(position);
    format.set_rate(RATE);
    let object = Value::Object(Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    });
    let bytes = serialize_props(&object).ok_or(pw::Error::CreationFailed)?;
    let pod = Pod::from_bytes(&bytes).ok_or(pw::Error::CreationFailed)?;
    stream.connect(
        pw::spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    Ok(Meter { id, levels: Rc::clone(levels), _listener: listener, stream })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_reduces_to_its_loudest_sample_in_percent() {
        let bytes: Vec<u8> = [0.1_f32, -0.5, 0.25, f32::NAN].iter().flat_map(|sample| sample.to_le_bytes()).collect();
        assert_eq!(peak(&bytes), 50.0, "the magnitude of a negative sample counts, NaN does not");
        assert_eq!(peak(&1.5_f32.to_le_bytes()), 100.0, "an over-full sample reads full scale");
        assert_eq!(peak(&[0, 0, 128]), 0.0, "a partial sample is ignored");
    }

    /// Pushes come 33 ms apart at most, carry the window's loudest peak, and only after a 1 point move.
    #[test]
    fn levels_push_at_most_every_interval_and_only_on_a_one_point_move() {
        let (updates, mut pushed) = tokio::sync::mpsc::unbounded_channel();
        let mut levels = Levels::new(updates);
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let peaks =
            |state: AudioLevelsState| state.levels.iter().map(|level| (level.id, level.peak)).collect::<Vec<_>>();

        levels.record(59, 40.0, at(0));
        assert_eq!(peaks(pushed.try_recv().unwrap()), [(59, 40.0)], "the first reading goes out at once");
        levels.record(59, 70.0, at(10));
        levels.record(61, 20.0, at(20));
        levels.record(59, 10.0, at(30));
        assert!(pushed.try_recv().is_err(), "nothing inside the interval");
        levels.record(59, 10.0, at(33));
        assert_eq!(peaks(pushed.try_recv().unwrap()), [(59, 70.0), (61, 20.0)], "the window's loudest");

        levels.record(59, 70.5, at(80));
        levels.record(61, 20.9, at(81));
        assert!(pushed.try_recv().is_err(), "moves under a point send nothing");
        levels.record(61, 21.0, at(82));
        assert_eq!(peaks(pushed.try_recv().unwrap()), [(59, 70.0), (61, 21.0)], "a move of exactly a point");

        levels.forget(61);
        assert_eq!(peaks(pushed.try_recv().unwrap()), [(59, 70.0)], "a dropped meter leaves the list");
    }
}
