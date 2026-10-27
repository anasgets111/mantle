//! [`KeyboardController`] owns `mantle.keyboard` state and write actions. Backlight, lock state,
//! and layout share one `Arc<Mutex<KeyboardState>>` and signal channel (ADR-0034).

pub use shared::state::keyboard::KeyboardState;

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use shared::{debug, error};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;

use crate::compositor::{Compositor, CompositorKind, backend_or_unsupported};

use super::super::brightness::controller::set_brightness;
use super::super::read_parsed;
use super::super::scale::{raw_from_percent, read_percent};
use super::locks::{find_leds, watch_locks};

/// A `*::kbd_backlight` LED and its `max_brightness`, which does not change at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedBacklight {
    dir: PathBuf,
    max: i32,
}

#[derive(Clone)]
pub struct KeyboardController {
    state: Arc<Mutex<KeyboardState>>,
    backlight: Option<LedBacklight>,
    compositor: &'static dyn Compositor,
    system_bus: zbus::Connection,
    events: UnboundedSender<()>,
    pub(super) writes: super::super::LatestWrites,
}

impl super::super::Writer for KeyboardController {
    /// `keyboard:set_backlight(pct)`. Logs and returns without keyboard-backlight hardware.
    async fn write(&self, pct: f64) {
        let Some(led) = self.backlight.as_ref() else {
            debug!("set_backlight called but this machine has no keyboard backlight; ignored");
            return;
        };
        let name = led.dir.file_name().unwrap_or_default().to_string_lossy();
        let raw = raw_from_percent(pct, led.max) as u32;
        if let Err(err) = set_brightness(&self.system_bus, "leds", &name, raw).await {
            debug!("SetBrightness(leds, {name}, {raw}) failed: {err}");
        }
        // `brightness_hw_changed` reports only hardware changes, so read this write back.
        self.state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
        let _ = self.events.send(());
    }
}

impl KeyboardController {
    /// `system_bus` carries logind backlight writes. `leds_root` (default `/sys/class/leds`) is
    /// test-injected and holds the backlight and the sysfs lock fallback. `state` is shared with the
    /// compositor reader, which writes layout; `compositor` picks the backend for
    /// `switch_layout`.
    pub fn new(
        system_bus: zbus::Connection,
        leds_root: &Path,
        state: Arc<Mutex<KeyboardState>>,
        compositor: Option<CompositorKind>,
        events_tx: UnboundedSender<()>,
    ) -> Self {
        let backlight = find_backlight(leds_root);
        match &backlight {
            Some(led) => watch_backlight(led.clone(), Arc::clone(&state), events_tx.clone()),
            None => {
                debug!("no usable *::kbd_backlight LED under {leds_root:?}; backlight reporting disabled for this run")
            }
        }
        watch_locks(leds_root, Arc::clone(&state), events_tx.clone());
        Self {
            state,
            backlight,
            compositor: backend_or_unsupported(compositor, "layout"),
            system_bus,
            events: events_tx,
            writes: Default::default(),
        }
    }

    /// `keyboard:switch_layout(index)`; synchronous because `Compositor::switch_layout` is
    /// fire-and-forget.
    pub fn switch_layout(&self, index: usize) {
        self.compositor.switch_layout(index);
    }

    pub fn snapshot(&self) -> KeyboardState {
        self.state.lock().expect("mutex poisoned").clone()
    }
}

/// First `*::kbd_backlight` by name with `max_brightness > 0`; a dead sibling must not hide a usable LED.
fn find_backlight(leds_root: &Path) -> Option<LedBacklight> {
    find_leds(leds_root, "::kbd_backlight").into_iter().find_map(|dir| {
        let max = read_parsed(&dir, "max_brightness").filter(|max| *max > 0)?;
        Some(LedBacklight { dir, max })
    })
}

/// Opens `brightness_hw_changed` before the initial read so a hotkey in between is not lost, then
/// re-reads on each `POLLPRI`, which the kernel raises only for hardware changes (ADR-0034.1).
fn watch_backlight(led: LedBacklight, state: Arc<Mutex<KeyboardState>>, events: UnboundedSender<()>) {
    let watch = std::fs::File::open(led.dir.join("brightness_hw_changed"))
        // SAFETY: the AsyncFd owns the file, so its fd stays open and unchanged until the AsyncFd drops.
        .and_then(|file| Ok(unsafe { AsyncFd::register_with_interest(file, Interest::PRIORITY) }?));
    state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
    let watch = match watch {
        Ok(watch) => watch,
        Err(err) => {
            error!("cannot watch {:?}/brightness_hw_changed; hotkey changes will not show: {err}", led.dir);
            return;
        }
    };
    tokio::spawn(async move {
        loop {
            match watch.ready(Interest::PRIORITY).await {
                Ok(mut guard) => guard.clear_ready(),
                Err(err) => {
                    error!("backlight watch failed; hotkey changes will no longer show: {err}");
                    break;
                }
            }
            // Reading from offset 0 re-arms kernfs's `POLLPRI`.
            let _ = watch.get_ref().read_at(&mut [0; 8], 0);
            state.lock().expect("mutex poisoned").backlight_percent = read_percent(&led.dir, led.max);
            if events.send(()).is_err() {
                break;
            }
        }
    });
}

/// `keyboard`'s state as the compositor reader sees it. The reader writes layout from its first
/// event, and signals once `keyboard` has started and set `events`.
#[derive(Clone, Default)]
pub struct LayoutSink {
    pub state: Arc<Mutex<KeyboardState>>,
    pub events: Arc<OnceLock<UnboundedSender<()>>>,
}

impl LayoutSink {
    pub fn write(&self, active_layout: String, active_layout_index: u32, layout_count: u32) {
        {
            let mut guard = self.state.lock().expect("mutex poisoned");
            guard.active_layout = active_layout;
            guard.active_layout_index = active_layout_index;
            guard.layout_count = layout_count;
        }
        if let Some(events) = self.events.get() {
            let _ = events.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_backlight_skips_lock_leds_and_scales_brightness() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("input3::capslock")).unwrap();
        let dir = root.path().join("asus::kbd_backlight");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("max_brightness"), "0\n").unwrap();
        assert_eq!(find_backlight(root.path()), None);

        // Sorts after the dead one, which must not hide it.
        let dir = root.path().join("tpacpi::kbd_backlight");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("max_brightness"), "3\n").unwrap();
        std::fs::write(dir.join("brightness"), "2\n").unwrap();
        let led = find_backlight(root.path()).expect("kbd_backlight with max > 0");
        assert_eq!(read_percent(&led.dir, led.max), Some(67));
    }
}
