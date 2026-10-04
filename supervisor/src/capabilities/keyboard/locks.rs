//! Lock-state monitoring for `mantle.keyboard` (ADR-0034). Sysfs LED `brightness` files do not
//! emit inotify `MODIFY` when `input_leds` changes them; evdev `EV_LED` is primary and sysfs is a
//! read-once fallback.

use std::io;
use std::path::{Path, PathBuf};

/// Resolved sysfs nodes for all three lock indicators. All-or-nothing: a missing sibling makes
/// [`resolve_lock_leds`] return `None` because hardware exposes all three or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockLeds {
    pub caps: PathBuf,
    pub num: PathBuf,
    pub scroll: PathBuf,
}

/// `leds_root` directories ending in `::<suffix>`, sorted by name so the pick does not follow
/// `read_dir` order; LED-class names are `<device>::<function>`.
pub(super) fn find_leds(leds_root: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(leds_root) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(suffix))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// The first device, by name, with all three lock LEDs, so they never mix across keyboards.
pub fn resolve_lock_leds(leds_root: &Path) -> Option<LockLeds> {
    find_leds(leds_root, "::capslock").into_iter().find_map(|caps| {
        let device = caps.file_name()?.to_str()?.strip_suffix("::capslock")?;
        let sibling = |function| {
            let dir = caps.with_file_name(format!("{device}{function}"));
            dir.is_dir().then_some(dir)
        };
        Some(LockLeds { num: sibling("::numlock")?, scroll: sibling("::scrolllock")?, caps })
    })
}

/// Reads one LED's `brightness`: `0` is off, any nonzero value on. Use `!= 0`, since
/// `max_brightness` need not be `1`.
pub fn read_led_on(led_dir: &Path) -> io::Result<bool> {
    let text = std::fs::read_to_string(led_dir.join("brightness"))?;
    Ok(text.trim().parse::<i64>().unwrap_or(0) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_led(dir: &Path, name: &str, brightness: &str) -> PathBuf {
        let led_dir = dir.join(name);
        std::fs::create_dir(&led_dir).unwrap();
        std::fs::write(led_dir.join("brightness"), brightness).unwrap();
        led_dir
    }

    #[test]
    fn resolve_lock_leds_finds_all_three_by_name_suffix() {
        let root = tempfile::tempdir().unwrap();
        let caps = write_led(root.path(), "input3::capslock", "0");
        let num = write_led(root.path(), "input3::numlock", "1");
        let scroll = write_led(root.path(), "input3::scrolllock", "0");

        let leds = resolve_lock_leds(root.path()).expect("all three present");
        assert_eq!(leds, LockLeds { caps, num, scroll });
    }

    #[test]
    fn resolve_lock_leds_is_none_when_any_one_led_is_missing() {
        let root = tempfile::tempdir().unwrap();
        write_led(root.path(), "input3::capslock", "0");
        write_led(root.path(), "input3::numlock", "1");
        // scrolllock deliberately absent.

        assert_eq!(resolve_lock_leds(root.path()), None);
    }

    #[test]
    fn resolve_lock_leds_takes_all_three_from_one_device() {
        let root = tempfile::tempdir().unwrap();
        write_led(root.path(), "input3::capslock", "0");
        write_led(root.path(), "input3::numlock", "0");
        let caps = write_led(root.path(), "input5::capslock", "0");
        let num = write_led(root.path(), "input5::numlock", "0");
        let scroll = write_led(root.path(), "input5::scrolllock", "0");

        assert_eq!(resolve_lock_leds(root.path()), Some(LockLeds { caps, num, scroll }));
    }

    #[test]
    fn resolve_lock_leds_is_none_against_an_empty_or_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(resolve_lock_leds(root.path()), None);
        assert_eq!(resolve_lock_leds(&root.path().join("does-not-exist")), None);
    }

    #[test]
    fn read_led_on_is_true_for_nonzero_brightness() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::capslock", "1");
        assert!(read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_is_false_for_zero_brightness() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::numlock", "0");
        assert!(!read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_treats_a_malformed_brightness_value_as_off_rather_than_erroring() {
        let root = tempfile::tempdir().unwrap();
        let led = write_led(root.path(), "input3::scrolllock", "not-a-number\n");
        assert!(!read_led_on(&led).unwrap());
    }

    #[test]
    fn read_led_on_errors_when_the_brightness_file_does_not_exist() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_led_on(root.path()).is_err());
    }
}
