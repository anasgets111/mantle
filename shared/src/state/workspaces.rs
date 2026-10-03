//! `mantle.workspaces` snapshot payload.

use serde::Serialize;

/// `mantle.workspaces` payload; `nil` without niri or Hyprland.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkspacesState {
    /// `"niri"` or `"hyprland"` (ADR-0119).
    pub compositor: String,
    /// One entry per output, sorted by connector name.
    pub outputs: Vec<OutputWorkspaces>,
    /// The focused window, or `nil` when none has focus. One per session, not per output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_client: Option<ActiveClient>,
    /// Hyprland special workspaces, sorted by name (ADR-0119). `nil` on niri; empty means none
    /// exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub special: Option<Vec<SpecialWorkspace>>,
    /// Whether niri's overview is open; `nil` on Hyprland, which has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview_open: Option<bool>,
}

/// One Hyprland special workspace (ADR-0119).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpecialWorkspace {
    /// Full name, `"special:scratch"` or `"special"`; the argument of `"toggle_special"`.
    pub name: String,
    /// Whether at least one window sits on it.
    pub populated: bool,
    /// `app_id` of its representative window, chosen as `WorkspaceEntry.app_id` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// `window_id` of its representative window, chosen as `WorkspaceEntry.app_id` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    /// Connector showing it, or `nil` while hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shown_on: Option<String>,
}

/// One output's workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputWorkspaces {
    /// Connector name, e.g. `"eDP-1"`, as in `mantle.screens` and a surface's `monitor`.
    pub name: String,
    /// `WorkspaceEntry.id` shown on this output.
    pub active_workspace: u64,
    /// `WorkspaceEntry.id` with focus, present only on the focused output (ADR-0056).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_workspace: Option<u64>,
    /// Workspaces on this output, sorted by `WorkspaceEntry.idx`.
    pub workspaces: Vec<WorkspaceEntry>,
}

/// One workspace. Draw `idx`, send `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkspaceEntry {
    /// Stable id, the argument of `"focus"`. Hyprland's workspace number; opaque on niri.
    pub id: u64,
    /// Label number: niri's 1-based position on the output, renumbered on reorder; Hyprland's
    /// workspace number, equal to `id` up to `255`, where it saturates.
    pub idx: u8,
    /// Workspace name; `nil` when unnamed, or on Hyprland when the name is just the number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether a window sits here (ADR-0117).
    pub populated: bool,
    /// `app_id` of a window here (ADR-0117): Hyprland's most recently focused one with an `app_id`;
    /// on niri the focused one, else the lowest id, `nil` if that one has no `app_id`. `nil` when empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// `window_id` of a window here, chosen as `WorkspaceEntry.app_id` is. `nil` when empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}

/// The focused window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ActiveClient {
    /// Window title; empty when unset.
    pub title: String,
    /// Wayland `app_id`, e.g. `"firefox"`; the key of `applications.by_app_id`. Empty when unset.
    pub app_id: String,
    /// Whether the window floats rather than tiles.
    pub floating: bool,
    /// Whether the window is fullscreen (maximized is `false`); `nil` on niri, which does not
    /// report it (ADR-0056).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
}
