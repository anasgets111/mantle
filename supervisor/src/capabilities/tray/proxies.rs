//! Hand-written proxies for SNI, DBusMenu, and the watcher (ADR-0031: no maintained zbus proxy
//! crate).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use std::collections::HashMap;

use zbus::names::OwnedBusName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
pub(super) trait StatusNotifierItem {
    #[zbus(name = "Activate")]
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Middle-click, a separate spec method exported by Telegram, Chromium, and Qt tray.
    #[zbus(name = "SecondaryActivate")]
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Scroll over the icon. `orientation` is normally `"vertical"`/`"horizontal"`; `delta` carries
    /// sign and magnitude.
    #[zbus(name = "Scroll")]
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    // The rest of the properties arrive through one `GetAll` in `item::fetch_tray_item_base`.
    #[zbus(property, name = "Status")]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Menu")]
    fn menu(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal, name = "NewTitle")]
    fn new_title(&self);
    #[zbus(signal, name = "NewIcon")]
    fn new_icon(&self);
    #[zbus(signal, name = "NewAttentionIcon")]
    fn new_attention_icon(&self);
    #[zbus(signal, name = "NewOverlayIcon")]
    fn new_overlay_icon(&self);
    #[zbus(signal, name = "NewToolTip")]
    fn new_tool_tip(&self);
    #[zbus(signal, name = "NewStatus")]
    fn new_status(&self, status: String);
}

/// `GetLayout`'s `(ia{sv}av)` reply: id, properties, children. Typed rather than one `OwnedValue`,
/// whose signature is always `"v"` and fails `Body::deserialize`'s check against the real one.
pub(super) type RawMenuLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

#[zbus::proxy(interface = "com.canonical.dbusmenu")]
pub(super) trait DBusMenu {
    #[zbus(name = "GetLayout")]
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: &[&str],
    ) -> zbus::Result<(u32, RawMenuLayout)>;

    #[zbus(name = "Event", no_reply)]
    fn event(&self, id: i32, event_id: &str, data: &Value<'_>, timestamp: u32) -> zbus::Result<()>;

    #[zbus(name = "AboutToShow")]
    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;

    #[zbus(signal, name = "LayoutUpdated")]
    fn layout_updated(&self, revision: u32, parent: i32);
}

/// Calls `RegisterStatusNotifierHost` at the well-known watcher name, so D-Bus routes to whichever
/// process owns it (ADR-0031).
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
pub(super) trait StatusNotifierWatcherClient {
    #[zbus(name = "RegisterStatusNotifierHost")]
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;
}

/// Binds to `destination`, not the item's unique name: Chromium answers only its registered
/// well-known name (ADR-0072).
/// Uncached: an item announces changes with SNI's own `NewX` signals, never `PropertiesChanged`,
/// the only thing zbus's cache invalidates on.
pub(super) async fn bind_item(
    connection: &zbus::Connection,
    destination: &OwnedBusName,
    path: &OwnedObjectPath,
) -> zbus::Result<StatusNotifierItemProxy<'static>> {
    StatusNotifierItemProxy::builder(connection)
        .destination(destination.clone())?
        .path(path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

pub(super) async fn bind_dbusmenu(
    connection: &zbus::Connection,
    destination: &OwnedBusName,
    path: &OwnedObjectPath,
) -> zbus::Result<DBusMenuProxy<'static>> {
    DBusMenuProxy::builder(connection).destination(destination.clone())?.path(path.clone())?.build().await
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::Type;

    use super::*;

    #[test]
    fn raw_menu_layout_signature_matches_the_real_dbusmenu_wire_shape() {
        // The DBusMenu wire signature must match or every real `GetLayout` fails, even if local
        // tests pass.
        assert_eq!(RawMenuLayout::SIGNATURE.to_string(), "(ia{sv}av)");
    }
}
