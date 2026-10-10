//! `mantle.windows` snapshot payload.

use serde::Serialize;

/// `mantle.windows` payload; `nil` with no niri, Hyprland, sway, mango or wlr-foreign-toplevel
/// backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WindowsState {
    /// `"niri"`, `"hyprland"`, `"sway"`, `"mango"`, or `"wlr_foreign_toplevel"`.
    pub source: String,
    /// Sorted by numeric `workspace_id`, then Hyprland named ones. Windows with a non-numeric id
    /// (sway names such as `1:web`, mango's `DP-1:3`) or none come last, in backend order.
    pub windows: Vec<WindowEntry>,
}

/// One toplevel window. `nil` optional fields are ones the backend does not report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WindowEntry {
    /// Opaque, backend-shaped id for the `windows` actions; compare it, never parse it.
    pub id: String,
    /// Window title; empty when unset.
    pub title: String,
    /// Wayland `app_id` (Hyprland's `class`); empty when unset.
    pub app_id: String,
    /// `WorkspaceEntry.id`; `nil` on wlr and on Hyprland special workspaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Connector name; `nil` when unknown. On wlr, the earliest-entered output the window is still on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Whether the window floats rather than tiles; `nil` on wlr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating: Option<bool>,
    /// Whether the window is fullscreen; `nil` on niri.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    /// Whether the window is minimized; `nil` except on wlr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimized: Option<bool>,
    /// Whether the window is maximized; `nil` on niri.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximized: Option<bool>,
    /// Whether the window is asking for attention; always `false` on wlr, which has no such state.
    pub urgent: bool,
}
