//! `mantle.audio_levels` snapshot payload.

use serde::Serialize;

/// `mantle.audio_levels`' payload: the peaks of the devices `mantle.audio:set_level_meters` names,
/// pushed at most 30 times a second and only when one moves by 1 % or more.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioLevelsState {
    /// One entry per metered device that has sent audio since the call, ordered by `id`.
    pub levels: Vec<AudioLevel>,
}

/// One metered sink or source.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioLevel {
    /// The `sinks[].id` or `sources[].id`.
    pub id: u32,
    /// The loudest sample since the last push, in percent of full scale, `0` to `100`; linear, not
    /// the cubic scale of `volume`. A sink's is read from its monitor, which PipeWire takes before the
    /// sink's own volume unless the sink sets `monitor.channel-volumes`.
    pub peak: f64,
}
