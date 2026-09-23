//! `TrayItem` hydration from `StatusNotifierItem` properties; `menu` is fetched separately.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

use serde::Serialize;
use shared::debug;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::icon::{
    IconPixmap, IconSource, icon_filename_stem, largest_valid_pixmap, resolve_icon_source, write_icon_png,
};
use super::menu::MenuItem;
use super::proxies::StatusNotifierItemProxy;
use super::registration::item_id;
use super::{MAX_TRAY_TEXT_BYTES, RawIconPixmap, RawToolTip};
use crate::capabilities::truncate_utf8_bytes;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TrayItem {
    /// Sanitized D-Bus unique name with the item's object path appended, e.g.
    /// `"1.234/StatusNotifierItem"`. Used by every `tray:` command.
    pub id: String,
    /// Display name: `Title`, falling back to `Id` when `Title` is empty.
    pub name: String,
    /// Theme icon name for `icon { name = ... }`; exclusive with [`TrayItem::icon_path`].
    pub icon_name: Option<String>,
    /// Decoded, bounds-checked PNG in the runtime directory for `image { source = ... }`; set when
    /// the item sent pixels instead of a theme name.
    pub icon_path: Option<String>,
    /// `NeedsAttention` artwork, resolved like `icon_name`/`icon_path`; draw it instead of the base
    /// pair when `status == "NeedsAttention"`. Both are `nil` when undeclared.
    pub attention_icon_name: Option<String>,
    /// File half of the attention artwork, matching `attention_icon_name`.
    pub attention_icon_path: Option<String>,
    /// Badge to draw over the base icon's corner. Carried, not composited, because a `stack` node
    /// overlays images and the Supervisor has no canvas. Both are `nil` when undeclared.
    pub overlay_icon_name: Option<String>,
    /// File half of the badge, matching `overlay_icon_name`.
    pub overlay_icon_path: Option<String>,
    /// Tooltip title and text flattened to one string, or `nil` when absent.
    pub tooltip: Option<String>,
    /// SNI status: `"Active"`, `"Passive"`, or `"NeedsAttention"`. `"Passive"` asks config to hide
    /// the item.
    pub status: String,
    /// `true` means left click opens the menu instead of activating the item.
    pub item_is_menu: bool,
    /// Top-level menu entries, or `nil` without `com.canonical.dbusmenu`. Fetched at registration
    /// and on layout updates.
    pub menu: Option<Vec<MenuItem>>,
    /// Digests of the base, attention and overlay pixmaps behind the `*_path` PNGs, so new pixels
    /// at an unchanged path still compare unequal and push.
    #[serde(skip)]
    pub(super) pixmap_digests: [Option<u64>; 3],
}

/// [`MAX_TRAY_TEXT_BYTES`] applied to one application-supplied property.
fn capped(value: String) -> String {
    truncate_utf8_bytes(&value, MAX_TRAY_TEXT_BYTES)
}

/// Resolves `TrayItem.name`: `Title`, falling back to `Id` when empty (ADR-0031).
fn resolve_display_name(title: &str, id: &str) -> String {
    if title.is_empty() { id.to_string() } else { title.to_string() }
}

/// Flattens `ToolTip`'s title and text (ADR-0031 leaves exact formatting open).
fn flatten_tooltip(title: &str, text: &str) -> Option<String> {
    match (title.is_empty(), text.is_empty()) {
        (true, true) => None,
        (false, true) => Some(title.to_string()),
        (true, false) => Some(text.to_string()),
        (false, false) => Some(format!("{title}\n{text}")),
    }
}

/// `{X}IconName`, `{X}IconPixmap` and spool suffix for the base, attention and overlay variants, in
/// [`TrayItem::pixmap_digests`] order. The suffixes keep the three PNGs distinct (ADR-0074).
const VARIANTS: [(&str, &str, &str); 3] = [
    ("IconName", "IconPixmap", ""),
    ("AttentionIconName", "AttentionIconPixmap", "-attention"),
    ("OverlayIconName", "OverlayIconPixmap", "-overlay"),
];

/// Removes `name` from a `GetAll` reply as `T`; absent or mistyped reads as `None`.
fn take<T: TryFrom<OwnedValue>>(all: &mut HashMap<String, OwnedValue>, name: &str) -> Option<T> {
    all.remove(name).and_then(|value| T::try_from(value).ok())
}

/// Every SNI property in one `GetAll` round trip. An item that refuses it reads as all defaults.
async fn get_all(item: &StatusNotifierItemProxy<'static>) -> HashMap<String, OwnedValue> {
    let proxy = item.inner();
    let reply = async {
        zbus::fdo::PropertiesProxy::builder(proxy.connection())
            .destination(proxy.destination().clone())?
            .path(proxy.path().clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?
            .get_all(proxy.interface().clone())
            .await
            .map_err(zbus::Error::from)
    };
    reply.await.unwrap_or_else(|err| {
        debug!("GetAll failed for {}: {err}", proxy.destination());
        HashMap::new()
    })
}

/// Reads every property `tray.items` needs except `menu`, which uses the caller's bound proxy via
/// [`super::menu::fetch_menu_via`]. A missing property falls back to its empty/default value. `previous` is the
/// item this refresh replaces; pixels it already spooled are not encoded again.
pub(super) async fn fetch_tray_item_base(
    item: &StatusNotifierItemProxy<'static>,
    unique_name: &OwnedUniqueName,
    object_path: &OwnedObjectPath,
    previous: &TrayItem,
) -> TrayItem {
    let mut all = get_all(item).await;
    // Every string below is whatever application owns this item; cap each on the way in
    // (`MAX_TRAY_TEXT_BYTES`) rather than trusting SNI, which bounds none of them.
    let id_prop = capped(take(&mut all, "Id").unwrap_or_default());
    let title = capped(take(&mut all, "Title").unwrap_or_default());
    let status = capped(take(&mut all, "Status").unwrap_or_default());
    let item_is_menu = take(&mut all, "ItemIsMenu").unwrap_or(false);
    let tooltip = take::<RawToolTip>(&mut all, "ToolTip");
    // Read once for all three icon variants; the directory belongs to the item (ADR-0074). Not
    // capped with the rest: a path cut short names a *different* directory rather than none, so
    // `theme_path_file` bounds it at `PATH_MAX` where it is used instead.
    let theme_path: String = take(&mut all, "IconThemePath").unwrap_or_default();

    let id = item_id(unique_name.as_str(), object_path.as_str());
    let stem = icon_filename_stem(&id);
    let name = resolve_display_name(&title, &id_prop);
    let tooltip_flat =
        tooltip.and_then(|(_, _, tt_title, tt_text)| flatten_tooltip(&capped(tt_title), &capped(tt_text)));

    let previous_paths = [&previous.icon_path, &previous.attention_icon_path, &previous.overlay_icon_path];
    let mut pixmap_digests = [None; 3];
    let [(icon_name, icon_path), (attention_icon_name, attention_icon_path), (overlay_icon_name, overlay_icon_path)] =
        std::array::from_fn(|index| {
            let (name_key, pixmap_key, suffix) = VARIANTS[index];
            let (resolved, digest) = resolve_variant(
                take(&mut all, name_key).unwrap_or_default(),
                take(&mut all, pixmap_key).unwrap_or_default(),
                &theme_path,
                &format!("{stem}{suffix}"),
                previous_paths[index].as_deref().zip(previous.pixmap_digests[index]),
            );
            pixmap_digests[index] = digest;
            resolved
        });

    TrayItem {
        id,
        name,
        icon_name,
        icon_path,
        attention_icon_name,
        attention_icon_path,
        overlay_icon_name,
        overlay_icon_path,
        tooltip: tooltip_flat,
        status,
        item_is_menu,
        menu: None,
        pixmap_digests,
    }
}

/// One icon triple (`{X}IconName`, `{X}IconPixmap`, `IconThemePath`) resolved to the config's
/// `(name, path)` pair (ADR-0074), plus the digest of a spooled pixmap. `spooled` is the path and
/// digest this variant spooled last time; the same pixels reuse that file.
fn resolve_variant(
    icon_name_prop: String,
    pixmaps_raw: Vec<RawIconPixmap>,
    theme_path: &str,
    spool_stem: &str,
    spooled: Option<(&str, u64)>,
) -> ((Option<String>, Option<String>), Option<u64>) {
    let pixmaps: Vec<IconPixmap> =
        pixmaps_raw.into_iter().map(|(width, height, bytes)| IconPixmap { width, height, bytes }).collect();
    // Capped here rather than at the three call sites, so no `{X}IconName` can reach a `TrayItem`
    // uncapped by being passed in from a fourth one later.
    match resolve_icon_source(&capped(icon_name_prop), &pixmaps, theme_path) {
        IconSource::ThemePathFile(path) => ((None, Some(path)), None),
        IconSource::Name(name) => ((Some(name), None), None),
        IconSource::Pixmap => {
            let Some(pixmap) = largest_valid_pixmap(&pixmaps) else { return ((None, None), None) };
            let digest = BuildHasherDefault::<DefaultHasher>::default().hash_one(pixmap);
            if let Some((path, _)) = spooled.filter(|&(_, last)| last == digest) {
                return ((None, Some(path.to_string())), Some(digest));
            }
            match write_icon_png(spool_stem, pixmap) {
                Ok(path) => ((None, Some(path)), Some(digest)),
                Err(err) => {
                    debug!("failed to spool icon PNG for {spool_stem}: {err}");
                    ((None, None), None)
                }
            }
        }
        IconSource::None => ((None, None), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_display_name ----

    #[test]
    fn resolve_display_name_prefers_title() {
        assert_eq!(resolve_display_name("Discord", "discord"), "Discord");
    }

    #[test]
    fn resolve_display_name_falls_back_to_id_when_title_is_empty() {
        assert_eq!(resolve_display_name("", "discord"), "discord");
    }

    #[test]
    fn unchanged_pixels_reuse_the_spooled_png_instead_of_encoding_again() {
        let bytes = vec![0xff, 1, 2, 3];
        let digest = BuildHasherDefault::<DefaultHasher>::default().hash_one(IconPixmap {
            width: 1,
            height: 1,
            bytes: bytes.clone(),
        });
        assert_eq!(
            resolve_variant(String::new(), vec![(1, 1, bytes)], "", "stem", Some(("/spool/stem.png", digest))),
            ((None, Some("/spool/stem.png".to_string())), Some(digest))
        );
    }

    // ---- flatten_tooltip ----

    #[test]
    fn flatten_tooltip_is_none_when_both_are_empty() {
        assert_eq!(flatten_tooltip("", ""), None);
    }

    #[test]
    fn flatten_tooltip_uses_title_alone() {
        assert_eq!(flatten_tooltip("Battery", ""), Some("Battery".to_string()));
    }

    #[test]
    fn flatten_tooltip_uses_text_alone() {
        assert_eq!(flatten_tooltip("", "80% charged"), Some("80% charged".to_string()));
    }

    #[test]
    fn flatten_tooltip_joins_title_and_text() {
        assert_eq!(flatten_tooltip("Battery", "80% charged"), Some("Battery\n80% charged".to_string()));
    }
}
