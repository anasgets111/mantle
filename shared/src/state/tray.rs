//! `mantle.tray` snapshot payload.

use serde::Serialize;

/// One `tray.items[].menu` entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MenuItem {
    /// DBusMenu id, the second argument of `activate_menu_item` and `menu_will_show`.
    pub id: i32,
    /// `"standard"` (the default) or `"separator"`, as the application sent it.
    pub menu_type: String,
    /// Entry text as sent, or `nil`. `_` mnemonic markers remain (`"_Quit"`); strip them to draw.
    pub label: Option<String>,
    /// `false` for a greyed-out entry; draw it, but clicking does nothing.
    pub enabled: bool,
    /// Theme icon name, a spooled PNG path from raw `icon-data`, or `nil`.
    pub icon_name: Option<String>,
    /// `"checkmark"`, `"radio"`, or `nil` for an entry that is not a toggle.
    pub toggle_type: Option<String>,
    /// `0` off, `1` on, `-1` indeterminate or unreported; `nil` exactly when `toggle_type` is.
    pub toggle_state: Option<i32>,
    /// Submenu entries, empty for a leaf. An app that fills submenus lazily sends them only after
    /// `menu_will_show`.
    pub children: Vec<MenuItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrayState {
    /// Registered items in registration order, oldest first; updates never reorder them.
    pub items: Vec<TrayItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrayItem {
    /// Item identity for every `tray` action, e.g. `"1.234/StatusNotifierItem"`. Opaque.
    pub id: String,
    /// SNI `Title`, or its `Id` when the title is empty.
    pub name: String,
    /// Theme icon name for `icon { name = ... }`. At most one of it and `icon_path` is set.
    pub icon_name: Option<String>,
    /// Icon file for `image { source = ... }`: one from the item's `IconThemePath`, or its pixmap
    /// spooled to a PNG.
    pub icon_path: Option<String>,
    /// Artwork to draw while `status == "NeedsAttention"`, paired with `attention_icon_path` like
    /// the base icon; both `nil` when unset.
    pub attention_icon_name: Option<String>,
    /// File half of the attention artwork.
    pub attention_icon_path: Option<String>,
    /// Badge to draw over the icon's corner, paired with `overlay_icon_path`; both `nil` when unset.
    pub overlay_icon_name: Option<String>,
    /// File half of the badge.
    pub overlay_icon_path: Option<String>,
    /// Tooltip title and text joined by a newline, or `nil` when both are empty.
    pub tooltip: Option<String>,
    /// `"Active"`, `"Passive"` (the item asks to be hidden) or `"NeedsAttention"`, as the item sent it.
    pub status: String,
    /// Left click should open `menu` instead of `activate`.
    pub item_is_menu: bool,
    /// Top-level menu entries, or `nil` when the item exports no DBusMenu or its first fetch failed.
    pub menu: Option<Vec<MenuItem>>,
    /// Digests of the base, attention and overlay pixmaps behind the `*_path` PNGs, so new pixels
    /// at an unchanged path still compare unequal and push.
    #[serde(skip)]
    pub pixmap_digests: [Option<u64>; 3],
}
