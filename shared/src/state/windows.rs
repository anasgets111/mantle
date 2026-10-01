//! `mantle.windows` snapshot payload.

use serde::Serialize;

/// `mantle.windows` payload; `nil` with no niri, Hyprland or wlr-foreign-toplevel backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WindowsState {
    /// `"niri"`, `"hyprland"`, or `"wlr_foreign_toplevel"`.
    pub source: String,
    /// Sorted by `workspace_id`, then backend order; windows without one last.
    pub windows: Vec<WindowEntry>,
}

/// One toplevel window. `nil` optional fields are ones the backend does not report.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    pub workspace_id: Option<u64>,
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
}
