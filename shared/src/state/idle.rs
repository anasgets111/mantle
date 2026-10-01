//! `mantle.idle` snapshot payload.

use serde::Serialize;

/// One holder blocking idle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdleInhibitor {
    /// Free-text holder name, e.g. `"mpv"`; draw it, never match it.
    pub who: String,
    /// Free-text reason, e.g. `"Playing video"`; often empty.
    pub why: String,
}

/// `mantle.idle`'s payload (ADR-0141).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdleState {
    /// True when logind blocks idle, a `ScreenSaver` client holds it (ADR-0231), or the compositor
    /// was last observed withholding idle notifications (ADR-0160). The compositor answer can
    /// remain true after a release while the seat is active; the next idle threshold refreshes it.
    pub inhibited: bool,
    /// The compositor last withheld idle notifications, but input resumed before it could answer
    /// again. Its hold may have ended; a fresh idle period clears this uncertainty.
    pub compositor_hold_stale: bool,
    /// Holders other than this shell, `ScreenSaver` clients included. The compositor's hold has an
    /// empty `who`; draw `why` then. An unconfirmed compositor hold is omitted.
    pub inhibitors: Vec<IdleInhibitor>,
}
