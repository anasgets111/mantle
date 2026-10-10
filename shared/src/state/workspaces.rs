//! `mantle.workspaces` snapshot payload.

use serde::Serialize;

/// `mantle.workspaces` payload; `nil` without niri, Hyprland, sway or mango.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkspacesState {
    /// `"niri"`, `"hyprland"`, `"sway"` or `"mango"` (ADR-0119).
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
    /// Connector name, e.g. `"eDP-1"`, as in `mantle.screens` and a panel's `output`.
    pub name: String,
    /// `WorkspaceEntry.id` shown on this output.
    pub active_workspace: String,
    /// `WorkspaceEntry.id` with focus, present only on the focused output (ADR-0056).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_workspace: Option<String>,
    /// Workspaces on this output: niri by position, Hyprland numbered ones by `number`, then named
    /// ones by name; mango's tags by number.
    pub workspaces: Vec<WorkspaceEntry>,
}

/// One workspace. Draw `number` or `name`, send `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkspaceEntry {
    /// Opaque string, only passed back to actions such as `"focus"`. Hyprland's workspace id in
    /// decimal, so a numbered workspace's id is its number and focusing an unlisted number
    /// creates it; named workspaces have negative ids. niri's id in decimal; sway's workspace
    /// name; mango's tag on its output, `"<output>:<tag>"` (`"DP-1:3"`).
    pub id: String,
    /// The number a keybind targets: niri's 1-based position on the output, renumbered on
    /// reorder; Hyprland's workspace number; sway's leading number; mango's tag number. `nil` for
    /// a Hyprland named or non-numeric sway workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// Workspace name; `nil` when unnamed, always on mango, or on Hyprland when the name is just
    /// the number.
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
    /// Whether a window here is asking for attention. Clears when the compositor clears it, on
    /// Hyprland when that window gains focus. Hyprland special workspaces carry none; their
    /// windows report it in `windows`.
    pub urgent: bool,
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
