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
}

/// One `Type=Application` desktop entry; display data only, argv stays private (ADR-0061).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppSummary {
    /// Desktop file id, e.g. `"org.telegram.desktop"`; the argument of `"launch"`.
    pub id: String,
    /// `Name=`, unlocalized: `Name[xx]` is not read (ADR-0061).
    pub name: String,
    /// `Icon=` as written, a theme name or absolute path, both accepted by `icon { name }`; `nil`
    /// without the key.
    pub icon: Option<String>,
    /// `NoDisplay=true`: omit from launchers, but keep its name and icon for window lookup.
    #[cfg_attr(feature = "schema", schemars(extend("examples" = [false])))]
    pub no_display: bool,
    /// `Comment=`, unlocalized, e.g. `"Web Browser"` (ADR-0112); `nil` without the key.
    pub comment: Option<String>,
    /// `GenericName=`, unlocalized, e.g. `"Text Editor"`; `nil` without the key.
    pub generic_name: Option<String>,
    /// `Keywords=` split on `;`, for search; empty without the key.
    pub keywords: Vec<String>,
}
