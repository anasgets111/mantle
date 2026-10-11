//! Raw-brightness/percent conversion for `keyboard`'s LED backlight (ADR-0034) and
//! `brightness`'s sysfs backlight (ADR-0053), both scaled from `[0, max]` to `[0, 100]`.

/// Converts raw `[0, max]` to `[0, 100]`, rounding half away from zero. `max <= 0` is `None`,
/// not a divide-by-zero or fabricated `0` (ADR-0307); `brightness` filters such devices.
pub fn percent_from_raw(brightness: i32, max: i32) -> Option<u8> {
    if max <= 0 {
        return None;
    }
    // i64 avoids `i32` overflow once `max`/`brightness` exceeds `i32::MAX / 100`.
    let brightness = i64::from(brightness.clamp(0, max));
    let max64 = i64::from(max);
    let scaled = 100 * brightness;
    let half = max64 / 2;
    Some((((scaled + half) / max64) as i32).clamp(0, 100) as u8)
}

/// `brightness` under `dir` (the last requested value, not `actual_brightness`) as a percent of `max`;
/// missing or malformed reads are `None`.
pub fn read_percent(dir: &std::path::Path, max: i32) -> Option<u8> {
    percent_from_raw(super::read_parsed(dir, "brightness")?, max)
}

/// Converts `[0, 100]` percent to the raw `[0, max]` scale for `SetBrightness`, rounding half
/// away from zero. `pct` is unvalidated: Lua numbers arrive as floats, and out-of-range values
/// clamp here. NaN reads as 0 (the `as` cast), which the wire cannot carry anyway.
pub fn raw_from_percent(pct: f64, max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    ((pct.clamp(0.0, 100.0) * f64::from(max) / 100.0).round() as i32).clamp(0, max)
}

/// A `1.0 = 100%` fraction as the percent Lua reads, to two decimals: `0.3 * 100.0` is
/// `30.000000000000004`, and f32 readings carry more noise (ADR-0308).
pub fn percent_from_fraction(fraction: f64) -> f64 {
    (fraction * 10_000.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_from_raw_scales_rounds_clamps_and_survives_malformed_readings() {
        // (raw, max, expected): 1/3 and 2/3 are this machine's real max-3 scale.
        for (raw, max, expected) in [
            (0, 3, Some(0)),
            (3, 3, Some(100)),
            (1, 3, Some(33)),
            (2, 3, Some(67)),
            (0, 0, None),
            (5, -1, None),
            (i32::MAX, i32::MAX, Some(100)),
            (i32::MAX / 2, i32::MAX, Some(50)),
            (99, 3, Some(100)),
            (-5, 3, Some(0)),
        ] {
            assert_eq!(percent_from_raw(raw, max), expected, "{raw}/{max}");
        }
    }

    #[test]
    fn raw_from_percent_scales_rounds_clamps_and_survives_malformed_maxes() {
        // (percent, max, expected): 33% of 3 -> 1, 67% -> 2.
        for (percent, max, expected) in [
            (0.0, 3, 0),
            (100.0, 3, 3),
            (33.0, 3, 1),
            (67.0, 3, 2),
            (150.0, 3, 3),
            (12.5, 8, 1),
            (-30.0, 3, 0),
            (f64::INFINITY, 3, 3),
            (f64::NAN, 3, 0),
            (100.0, i32::MAX, i32::MAX),
            (50.0, i32::MAX, i32::MAX / 2 + 1),
            (50.0, 0, 0),
        ] {
            assert_eq!(raw_from_percent(percent, max), expected, "{percent}% of {max}");
        }
    }
}
