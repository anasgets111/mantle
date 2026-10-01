//! `mantle.keyboard` snapshot payload.

use serde::Serialize;

/// `mantle.keyboard`'s payload (ADR-0034). Lock keys read `false` when no source resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeyboardState {
    /// Keyboard backlight, `0` to `100`, or `-1` without a backlight device or readable level.
    /// Refreshes on hardware hotkeys and `set_backlight` only, not on other software writes.
    pub backlight_pct: i32,
    /// Caps Lock is on.
    pub caps_lock: bool,
    /// Num Lock is on.
    pub num_lock: bool,
    /// Scroll Lock is on.
    pub scroll_lock: bool,
    /// Layout display name, e.g. `"English (US)"`; empty before the compositor answers or without one.
    pub active_layout: String,
    /// 0-based position of the active layout, as `switch_layout` takes it.
    pub active_layout_index: u32,
    /// Configured layout count; below `2` there is nothing to switch.
    pub layout_count: u32,
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self {
            backlight_pct: -1,
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
            active_layout: String::new(),
            active_layout_index: 0,
            layout_count: 0,
        }
    }
}
