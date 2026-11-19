//! Hand-written proxies for SNI, DBusMenu, and the watcher (ADR-0031: no maintained zbus proxy
//! crate).

#[cfg(test)]
use std::collections::HashMap;

use zbus::names::OwnedBusName;
use zbus::zvariant::{OwnedObjectPath, Value};

#[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
pub(super) trait StatusNotifierItem {
    #[zbus(name = "Activate")]
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Context-menu activation at screen coordinates `x`, `y`.
    #[zbus(name = "ContextMenu")]
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Middle-click, a separate spec method exported by Telegram, Chromium, and Qt tray.
    #[zbus(name = "SecondaryActivate")]
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    /// Scroll over the icon. `orientation` is `"vertical"` or `"horizontal"`; `delta` carries sign
    /// and magnitude.
    #[zbus(name = "Scroll")]
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    // Properties go through `capabilities::get_property` and `get_all`: generated getters decode an
    // `OwnedValue`, one `Value` per byte of an array.

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
    #[zbus(signal, name = "NewMenu")]
    fn new_menu(&self);
}

/// `GetLayout`'s `(ia{sv}av)` root node as test peers serve it: id, properties, children.
#[cfg(test)]
pub(super) type RawMenuLayout = (i32, HashMap<String, zbus::zvariant::OwnedValue>, Vec<zbus::zvariant::OwnedValue>);

/// `GetLayout` is called by `menu::fetch_menu_via`, which decodes the reply with typed seeds.
#[zbus::proxy(interface = "com.canonical.dbusmenu")]
pub(super) trait DBusMenu {
    #[zbus(name = "Event", no_reply)]
    fn event(&self, id: i32, event_id: &str, data: &Value<'_>, timestamp: u32) -> zbus::Result<()>;

    #[zbus(name = "AboutToShow")]
    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;

    #[zbus(signal, name = "LayoutUpdated")]
    fn layout_updated(&self, revision: u32, parent: i32);

    /// Its arguments go unread: the refetch rereads the whole layout.
    #[zbus(signal, name = "ItemsPropertiesUpdated")]
    fn items_properties_updated(&self);
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
