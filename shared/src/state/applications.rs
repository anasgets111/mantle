//! `mantle.applications` snapshot payload.

use serde::Serialize;
use std::collections::BTreeMap;

/// `mantle.applications` payload (ADR-0061, ADR-0252).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApplicationsState {
    /// Installed entries, sorted by `name` (byte order), including `NoDisplay` entries for window
    /// lookup. Launchers omit entries with `no_display = true`. `Hidden` entries are excluded.
    /// A change under an applications directory rescans 250 ms after the last event.
    pub entries: Vec<AppSummary>,
    /// Window `app_id` to its 1-based index: `entries[by_app_id[app_id]]`. Keys are exact
    /// `StartupWMClass` and desktop ids, then lowercased and last-dot-segment guesses.
    pub by_app_id: BTreeMap<String, usize>,
    /// Live Unity `LauncherEntry` badge, progress and urgency state keyed by desktop id (e.g.
    /// `"org.telegram.desktop"`). Entries drop when their D-Bus sender leaves the session bus.
    pub badges: BTreeMap<String, AppBadge>,
}

/// Live badge, progress and urgency published over `com.canonical.Unity.LauncherEntry`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppBadge {
    /// Unread or item count; negative values from senders are clamped to `0`.
    pub count: i64,
    /// Whether `count` should be displayed.
    pub count_visible: bool,
    /// Progress fraction in `0.0..=1.0`.
    pub progress: f64,
    /// Whether `progress` should be displayed.
    pub progress_visible: bool,
    /// Whether the application is asking for attention.
    pub urgent: bool,
}

/// One `Type=Application` desktop entry; display data only, argv stays private (ADR-0061).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppSummary {
    /// Desktop file id, e.g. `"org.telegram.desktop"`; the argument of `"launch"`.
    pub id: String,
    /// Localized `Name=`, following `LC_ALL`/`LC_MESSAGES`/`LANG` (`ll_CC@mod`, `ll_CC`, `ll@mod`,
    /// `ll`, bare).
    pub name: String,
    /// Localized `Icon=` as written, a theme name or absolute path, both accepted by `icon { name }`;
    /// `nil` without the key.
    pub icon: Option<String>,
    /// `NoDisplay=true`: omit from launchers, but keep its name and icon for window lookup.
    pub no_display: bool,
    /// Localized `Comment=`, e.g. `"Web Browser"` (ADR-0112); `nil` without the key.
    pub comment: Option<String>,
    /// Localized `GenericName=`, e.g. `"Text Editor"`; `nil` without the key.
    pub generic_name: Option<String>,
    /// Localized `Keywords=` split on `;`, for search; empty without the key.
    pub keywords: Vec<String>,
    /// Additional actions declared in `Actions=`, in declared order; empty without the key or any
    /// launchable `[Desktop Action ...]` group.
    pub actions: Vec<AppAction>,
}

/// One `[Desktop Action <key>]` declared by an application entry (e.g. "New private window").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppAction {
    /// Action identifier from `Actions=`, e.g. `"new-private-window"`; passed to `launch_action`.
    pub key: String,
    /// Localized `Name=` for this action.
    pub name: String,
    /// Optional localized `Icon=` for this action, a theme name or absolute path; `nil` without the
    /// key.
    pub icon: Option<String>,
}
