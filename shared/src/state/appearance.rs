//! `mantle.appearance` snapshot payload.

use serde::Serialize;

/// The user's preferred colour scheme, from the portal's `color-scheme`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme {
    /// No preference, or a value the portal does not define.
    #[default]
    Default,
    /// The user prefers a dark appearance.
    Dark,
    /// The user prefers a light appearance.
    Light,
}

/// The user's preferred contrast level, from the portal's `contrast`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Contrast {
    /// No preference, or a value the portal does not define.
    #[default]
    Normal,
    /// The user prefers higher contrast.
    High,
}

/// `mantle.appearance`'s payload: the `org.freedesktop.appearance` settings of xdg-desktop-portal.
/// Without a portal, or for a key it does not provide, a field holds its default; the capability
/// is never `nil` after it starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppearanceState {
    /// Preferred colour scheme; `"default"` means no preference.
    pub color_scheme: ColorScheme,
    /// System accent colour as `"#rrggbb"` (sRGB), or `nil` when none is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Preferred contrast level.
    pub contrast: Contrast,
    /// `true` when the user asks for reduced motion; `false` for no preference or an older portal.
    pub reduced_motion: bool,
}
