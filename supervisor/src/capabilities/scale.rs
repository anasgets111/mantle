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
    fn percent_from_raw_scales_zero_to_max_across_zero_to_one_hundred() {
        assert_eq!(percent_from_raw(0, 3), Some(0));
        assert_eq!(percent_from_raw(3, 3), Some(100));
    }

    #[test]
    fn percent_from_raw_rounds_half_away_from_zero_on_a_coarse_scale() {
        // 1/3 -> 33; 2/3 -> 67 (this machine's real max is 3).
        assert_eq!(percent_from_raw(1, 3), Some(33));
        assert_eq!(percent_from_raw(2, 3), Some(67));
    }

    #[test]
    fn percent_from_raw_is_none_when_max_is_not_positive() {
        assert_eq!(percent_from_raw(0, 0), None);
        assert_eq!(percent_from_raw(5, -1), None);
    }

    #[test]
    fn percent_from_raw_does_not_overflow_on_a_malformed_near_i32_max_reading() {
        assert_eq!(percent_from_raw(i32::MAX, i32::MAX), Some(100));
        assert_eq!(percent_from_raw(i32::MAX / 2, i32::MAX), Some(50));
    }

    #[test]
    fn percent_from_raw_clamps_an_out_of_range_brightness() {
        assert_eq!(percent_from_raw(99, 3), Some(100));
        assert_eq!(percent_from_raw(-5, 3), Some(0));
    }

    #[test]
    fn raw_from_percent_scales_zero_to_one_hundred_across_zero_to_max() {
        assert_eq!(raw_from_percent(0.0, 3), 0);
        assert_eq!(raw_from_percent(100.0, 3), 3);
    }

    #[test]
    fn raw_from_percent_rounds_half_away_from_zero() {
        // 33% of 3 -> 1; 67% -> 2.
        assert_eq!(raw_from_percent(33.0, 3), 1);
        assert_eq!(raw_from_percent(67.0, 3), 2);
    }

    #[test]
    fn raw_from_percent_clamps_a_percent_above_one_hundred() {
        assert_eq!(raw_from_percent(150.0, 3), 3);
    }

    #[test]
    fn raw_from_percent_takes_a_fraction_and_clamps_a_negative_or_non_finite_percent() {
        assert_eq!(raw_from_percent(12.5, 8), 1);
        assert_eq!(raw_from_percent(-30.0, 3), 0);
        assert_eq!(raw_from_percent(f64::INFINITY, 3), 3);
        assert_eq!(raw_from_percent(f64::NAN, 3), 0);
    }

    #[test]
    fn raw_from_percent_does_not_overflow_on_a_malformed_near_i32_max_max() {
        assert_eq!(raw_from_percent(100.0, i32::MAX), i32::MAX);
        assert_eq!(raw_from_percent(50.0, i32::MAX), i32::MAX / 2 + 1);
    }

    #[test]
    fn raw_from_percent_is_zero_when_max_is_not_positive() {
        assert_eq!(raw_from_percent(50.0, 0), 0);
    }
}
