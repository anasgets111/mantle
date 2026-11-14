//! System tray host (`mantle.tray`, ADR-0031).
//!
//! Hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` and handles registered
//! `org.kde.StatusNotifierItem`s and optional `com.canonical.dbusmenu` menus. Hand-written proxies
//! avoid `system-tray`'s source-verified pixmap-squaring bug (ADR-0031); a keyed registry and one
//! abortable forwarder per item keep snapshots live.
//!
//! SNI has no unregister signal. `NameOwnerChanged` supplies liveness; one global forwarder removes
//! every entry for a unique name when it drops off the bus.
//!
//! ponytail: `TrayController::new` never fails outright (ADR-0030). No other tray host, common on
//! niri/sway, is not an error; losing `RequestName` to Plasma/GNOME is expected (ADR-0031).

pub use shared::state::tray::TrayState;

use shared::action::TrayAction;

/// Well-known bus name and object path for `org.kde.StatusNotifierWatcher`.
const WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";
/// Fixed item path when `RegisterStatusNotifierItem`'s `service` is a bus name (ADR-0031).
const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";
/// Maximum accepted ARGB pixmap dimension (ADR-0031).
const MAX_PIXMAP_DIMENSION: i32 = 128;

/// Cap on every string an item supplies: `Title`, `Id`, `Status`, both `ToolTip` halves, the three
/// `IconName`s, and each DBusMenu node's `label`, `type`, `icon-name` and `toggle-type`.
///
/// SNI and DBusMenu bound none of these, and a tray item is any application on the session bus, so
/// without a cap one `GetLayout` reply or `Title` change sizes an allocation in a Supervisor that
/// never restarts. Truncating is safe even for identifiers: a cut `IconName` names no icon, as
/// dropping it would.
const MAX_TRAY_TEXT_BYTES: usize = 256;

/// Cap on an item's registered D-Bus object path (`TrayItem.id` is `unique_name + object_path`).
/// Real SNI paths are tens of bytes (`/StatusNotifierItem`, `/org/ayatana/NotificationItem/...`);
/// bounding this prevents a peer from registering a multi-megabyte object path as its `id`.
const MAX_OBJECT_PATH_BYTES: usize = 512;

/// Total [`menu::MenuItem`]s one `GetLayout` reply may produce.
///
/// `menu::MAX_MENU_DEPTH` bounds nesting but not breadth, so a single level of a million
/// siblings is within it. Real menus hold tens of entries; a thousand is already far past any
/// application's intent, and past it the reply is truncated rather than allocated.
const MAX_MENU_NODES: usize = 1024;

/// ponytail: at most 32 tray items and 4096 menu nodes across all items together, so the
/// worst-case `mantle.tray` snapshot stays under [`shared::framing::MAX_FRAME_LEN`] (16,777,216 B,
/// 16 MiB) even when every string hits `MAX_TRAY_TEXT_BYTES` (6x JSON-escaped) and
/// `MAX_THEME_PATH_BYTES`:
/// - One `TrayItem` without menu: ~88,670 B (88,725 B for a single-item frame; 32 items ~2.71 MiB).
/// - One `MenuItem`: ~3,224 B (4096 nodes ~12.59 MiB).
/// - Total worst case: 16,041,628 B (~15.30 MiB) < 16,777,216 B
///   (`the_largest_tray_snapshot_fits_in_one_frame`).
///
/// One item can hold up to `MAX_MENU_NODES` (`1024`, ~3.15 MiB), so giving every item a full
/// unshared menu would cap the tray at 4 items. Sharing `MAX_TOTAL_MENU_NODES` across the whole
/// tray keeps 32 slots usable while still allowing four full 1024-node menus at once.
/// Upgrade: delta-update or paginate menus, or raise the frame cap if more items are needed.
pub(crate) const MAX_ITEMS: usize = 32;
const MAX_TOTAL_MENU_NODES: usize = 4096;
/// Maximum deferred registrations queued while [`MAX_ITEMS`] slots are full.
const MAX_WAITING_ITEMS: usize = 64;

/// `IconPixmap`'s D-Bus wire shape (`a(iiay)`): width, height, raw ARGB32 bytes.
type RawIconPixmap = (i32, i32, Vec<u8>);
/// `ToolTip`'s D-Bus wire shape (`(sa(iiay)ss)`): icon name, icon pixmaps, title, text.
type RawToolTip = (String, Vec<RawIconPixmap>, String, String);

pub mod controller;
pub mod icon;
pub mod item;
pub mod menu;
pub mod proxies;
pub mod registration;
pub mod registry;
pub mod watcher;

pub use controller::TrayController;

/// `mantle.tray` dispatch (ADR-0037): spawns every write action (ADR-0031).
pub fn dispatch(controller: &TrayController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<TrayAction>(&envelope.params) else { return };
    let controller = controller.clone();
    tokio::spawn(async move {
        match action {
            TrayAction::Activate { id, x, y } => controller.activate(&id, x, y).await,
            TrayAction::ContextMenu { id, x, y } => controller.context_menu(&id, x, y).await,
            TrayAction::SecondaryActivate { id, x, y } => controller.secondary_activate(&id, x, y).await,
            TrayAction::Scroll { id, delta, orientation } => controller.scroll(&id, delta, orientation).await,
            TrayAction::ActivateMenuItem { id, menu_item_id } => controller.activate_menu_item(&id, menu_item_id).await,
            TrayAction::MenuWillShow { id, submenu_id } => controller.menu_will_show(&id, submenu_id).await,
        }
    });
}

#[cfg(test)]
mod tests {
    use shared::state::tray::{MenuItem, MenuType, ToggleType, TrayItem, TrayStatus};

    use super::*;
    use crate::capabilities::test_support::{snapshot_frame_len, worst_case_text};

    /// Proof that no set of tray items and menus can push `mantle.tray` past the 16 MiB frame cap:
    /// `MAX_ITEMS` items with every string at its cap in characters JSON escapes to six bytes, plus
    /// `MAX_TOTAL_MENU_NODES` worst-case menu nodes distributed up to `MAX_MENU_NODES - 1` per item.
    #[test]
    fn the_largest_tray_snapshot_fits_in_one_frame() {
        let text = || worst_case_text(MAX_TRAY_TEXT_BYTES);
        let theme_path = worst_case_text(icon::MAX_THEME_PATH_BYTES + 1 + MAX_TRAY_TEXT_BYTES + 4);
        // D-Bus unique names are at most 255 bytes (`:` stripped -> 254) and unescaped ASCII;
        // `object_path` is capped at `MAX_OBJECT_PATH_BYTES` and is also unescaped ASCII.
        let worst_id = format!("{}/{}", "1".repeat(254), "p".repeat(MAX_OBJECT_PATH_BYTES - 1));
        assert_eq!(worst_id.len(), 254 + MAX_OBJECT_PATH_BYTES);

        let menu_item = MenuItem {
            id: i32::MIN,
            menu_type: MenuType::Separator,
            label: Some(text()),
            enabled: false,
            icon_name: Some(text()),
            toggle_type: Some(ToggleType::Checkmark),
            toggle_state: Some(i32::MIN),
            children: Vec::new(),
        };

        let mut remaining_nodes = MAX_TOTAL_MENU_NODES;
        let items: Vec<TrayItem> = (0..MAX_ITEMS)
            .map(|_| {
                let take = remaining_nodes.min(MAX_MENU_NODES - 1);
                remaining_nodes -= take;
                // Every field spelled out so a field added later fails to compile until sized.
                TrayItem {
                    id: worst_id.clone(),
                    name: text(),
                    icon_name: Some(text()),
                    icon_path: Some(theme_path.clone()),
                    attention_icon_name: Some(text()),
                    attention_icon_path: Some(theme_path.clone()),
                    overlay_icon_name: Some(text()),
                    overlay_icon_path: Some(theme_path.clone()),
                    tooltip: Some(format!("{}\n{}", text(), text())),
                    status: TrayStatus::NeedsAttention,
                    item_is_menu: true,
                    menu: Some(vec![menu_item.clone(); take]),
                    pixmap_digests: [Some(u64::MAX); 3],
                }
            })
            .collect();
        assert_eq!(remaining_nodes, 0, "all MAX_TOTAL_MENU_NODES nodes must be allocated across items");

        let len = snapshot_frame_len(shared::Capability::Tray, &TrayState { items });

        assert!(len < shared::framing::MAX_FRAME_LEN, "{len} bytes");
    }
}
