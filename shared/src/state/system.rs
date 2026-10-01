//! `mantle.system` snapshot payload.

use serde::Serialize;

/// `mantle.system`'s payload, pushed on each tick of `interval`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SystemState {
    /// Unix epoch seconds, as `os.date` takes them.
    pub time: i64,
    /// Seconds since `system` was first used, as of the last push; excludes suspend. Take durations
    /// from it, since NTP moves `time`.
    // ponytail: `Instant` is `CLOCK_MONOTONIC`; suspend-inclusive timing wants a `CLOCK_BOOTTIME` field.
    pub monotonic: i64,
}
