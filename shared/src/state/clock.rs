//! `mantle.clock` snapshot payload.

use serde::Serialize;

/// `mantle.clock`'s payload, pushed on each tick of `interval`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClockState {
    /// Unix epoch seconds, as `os.date` takes them.
    pub time: i64,
    /// System-wide `CLOCK_MONOTONIC` seconds since boot, as of the last push; excludes suspend.
    /// Take durations from it, since NTP moves `time`. `mantle.clock:monotonic()` reads it live.
    // ponytail: suspend-inclusive timing wants a `CLOCK_BOOTTIME` field.
    pub monotonic: f64,
}

/// `CLOCK_MONOTONIC` in seconds, the clock `ClockState::monotonic` and `mantle.clock:monotonic()`
/// share. Fractional so a sub-second elapsed term stays honest; f64 holds microseconds for decades.
pub fn monotonic_seconds() -> f64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a valid out-pointer; `CLOCK_MONOTONIC` always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as f64 + now.tv_nsec as f64 * 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_seconds_is_since_boot_and_never_goes_back() {
        let first = monotonic_seconds();
        assert!(first > 0.0);
        assert!(monotonic_seconds() >= first);
    }
}
