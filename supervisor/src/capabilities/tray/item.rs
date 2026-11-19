//! `TrayItem` hydration from `StatusNotifierItem` properties; `menu` is fetched separately.

pub use shared::state::tray::TrayItem;
use shared::state::tray::TrayStatus;

use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

use serde::de::MapAccess;
use shared::debug;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::{OwnedObjectPath, Type};

use super::icon::{
    IconPixmap, IconSource, icon_filename_stem, largest_valid_pixmap, resolve_icon_source, write_icon_png,
};
use super::proxies::StatusNotifierItemProxy;
use super::registration::item_id;
use super::{MAX_TRAY_TEXT_BYTES, RawIconPixmap, RawToolTip};
use crate::capabilities::{DictFields, Hint, get_all, get_property, truncate_utf8_bytes};

/// [`MAX_TRAY_TEXT_BYTES`] applied to one application-supplied property.
fn capped(value: &str) -> String {
    truncate_utf8_bytes(value, MAX_TRAY_TEXT_BYTES)
}

/// Resolves `TrayItem.name`: `Title`, falling back to `Id` when empty (ADR-0031).
fn resolve_display_name(title: &str, id: &str) -> String {
    if title.is_empty() { id.to_string() } else { title.to_string() }
}

/// SNI `Status` by its spec spelling; anything else reads as `Active`.
fn parse_status(status: &str) -> TrayStatus {
    match status {
        "Passive" => TrayStatus::Passive,
        "NeedsAttention" => TrayStatus::NeedsAttention,
        _ => TrayStatus::Active,
    }
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

/// The SNI properties `tray.items` reads, each `Some` when the item sent it: its value, or the
/// type's default when mistyped. Decoded as [`Dict`](crate::capabilities::Dict), so a pixmap's bytes land in a `Vec<u8>`
/// rather than one `zvariant::Value` each.
#[derive(Default)]
struct Props {
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    tool_tip: Option<RawToolTip>,
    item_is_menu: Option<bool>,
    theme_path: Option<String>,
    /// `{X}IconName` and `{X}IconPixmap`, in [`VARIANTS`] order.
    names: [Option<String>; 3],
    pixmaps: [Option<Vec<RawIconPixmap>>; 3],
}

impl DictFields for Props {
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        fn sent<'de, T: serde::Deserialize<'de> + Type + Default, A: MapAccess<'de>>(
            map: &mut A,
        ) -> Result<Option<T>, A::Error> {
            Ok(Some(map.next_value_seed(Hint::new())?.unwrap_or_default()))
        }
        match key {
            "Id" => self.id = sent(map)?,
            "Title" => self.title = sent(map)?,
            "Status" => self.status = sent(map)?,
            "ToolTip" => self.tool_tip = sent(map)?,
            "ItemIsMenu" => self.item_is_menu = sent(map)?,
            "IconThemePath" => self.theme_path = sent(map)?,
            key => match VARIANTS.iter().position(|&(name, pixmap, _)| key == name || key == pixmap) {
                Some(index) if key == VARIANTS[index].0 => self.names[index] = sent(map)?,
                Some(index) => self.pixmaps[index] = sent(map)?,
                None => return Ok(false),
            },
        }
        Ok(true)
    }
}

/// Every SNI property in one `GetAll` round trip, or with `text_only` the four a title, tooltip or
/// status signal can change as concurrent `Get`s, skipping the icon pixmaps (a failed one is left
/// out). An item that refuses reads as all defaults.
async fn get_props(item: &StatusNotifierItemProxy<'static>, text_only: bool) -> Props {
    let proxy = item.inner();
    if !text_only {
        return get_all(proxy).await.unwrap_or_else(|err| {
            debug!("property read failed for {}: {err}", proxy.destination());
            Props::default()
        });
    }
    /// Its value, or the type's default when mistyped; `None` when the read failed.
    fn sent<T: Default>(name: &str, read: zbus::Result<Option<T>>, item: &zbus::names::BusName) -> Option<T> {
        read.inspect_err(|err| debug!("Get {name} failed for {item}: {err}")).ok().map(Option::unwrap_or_default)
    }
    let (id, title, status, tool_tip) = futures_util::join!(
        get_property(proxy, "Id"),
        get_property(proxy, "Title"),
        get_property(proxy, "Status"),
        get_property(proxy, "ToolTip"),
    );
    let item = proxy.destination();
    Props {
        id: sent("Id", id, item),
        title: sent("Title", title, item),
        status: sent("Status", status, item),
        tool_tip: sent("ToolTip", tool_tip, item),
        ..Props::default()
    }
}

/// Reads every property `tray.items` needs except `menu`, which uses the caller's bound proxy via
/// [`super::menu::fetch_menu_via`]. A missing property falls back to its empty/default value. `previous` is the
/// item this refresh replaces; pixels it already spooled are not encoded again. With `text_only` it reads just
/// the text properties and keeps the rest of `previous`, including any text property whose read failed.
pub(super) async fn fetch_tray_item_base(
    item: &StatusNotifierItemProxy<'static>,
    unique_name: &OwnedUniqueName,
    object_path: &OwnedObjectPath,
    previous: &TrayItem,
    text_only: bool,
) -> TrayItem {
    let mut all = get_props(item, text_only).await;
    let (got_name, got_status, got_tooltip) =
        (all.id.is_some() && all.title.is_some(), all.status.is_some(), all.tool_tip.is_some());
    // Every string below is whatever application owns this item; cap each on the way in
    // (`MAX_TRAY_TEXT_BYTES`) rather than trusting SNI, which bounds none of them.
    let id_prop = capped(&all.id.unwrap_or_default());
    let title = capped(&all.title.unwrap_or_default());
    let status = parse_status(&all.status.unwrap_or_default());
    let item_is_menu = all.item_is_menu.unwrap_or(false);
    let tooltip = all.tool_tip;
    // Read once for all three icon variants; the directory belongs to the item (ADR-0074). Not
    // capped with the rest: a path cut short names a *different* directory rather than none, so
    // `theme_path_file` bounds it at `PATH_MAX` where it is used instead.
    let theme_path = all.theme_path.unwrap_or_default();

    let name = resolve_display_name(&title, &id_prop);
    let tooltip_flat =
        tooltip.and_then(|(_, _, tt_title, tt_text)| flatten_tooltip(&capped(&tt_title), &capped(&tt_text)));
    if text_only {
        let mut kept = TrayItem { menu: None, ..previous.clone() };
        if got_name {
            kept.name = name;
        }
        if got_status {
            kept.status = status;
        }
        if got_tooltip {
            kept.tooltip = tooltip_flat;
        }
        return kept;
    }
    let id = item_id(unique_name.as_str(), object_path.as_str());
    let stem = icon_filename_stem(&id);

    let previous_paths = [&previous.icon_path, &previous.attention_icon_path, &previous.overlay_icon_path];
    let mut pixmap_digests = [None; 3];
    let [(icon_name, icon_path), (attention_icon_name, attention_icon_path), (overlay_icon_name, overlay_icon_path)] =
        std::array::from_fn(|index| {
            let (resolved, digest) = resolve_variant(
                all.names[index].take().unwrap_or_default(),
                all.pixmaps[index].take().unwrap_or_default(),
                &theme_path,
                &format!("{stem}{}", VARIANTS[index].2),
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
    match resolve_icon_source(&capped(&icon_name_prop), &pixmaps, theme_path) {
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
    use crate::capabilities::Dict;

    #[test]
    fn resolve_display_name_prefers_the_title_and_falls_back_to_the_id() {
        assert_eq!(resolve_display_name("Discord", "discord"), "Discord");
        assert_eq!(resolve_display_name("", "discord"), "discord");
    }

    #[test]
    fn status_maps_the_sni_spellings_and_reads_anything_else_as_active() {
        for (raw, status) in [
            ("Active", TrayStatus::Active),
            ("Passive", TrayStatus::Passive),
            ("NeedsAttention", TrayStatus::NeedsAttention),
            ("needs_attention", TrayStatus::Active),
            ("", TrayStatus::Active),
        ] {
            assert_eq!(parse_status(raw), status, "{raw}");
        }
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

    #[test]
    fn flatten_tooltip_joins_the_non_empty_parts() {
        for (title, text, expected) in [
            ("", "", None),
            ("Battery", "", Some("Battery")),
            ("", "80% charged", Some("80% charged")),
            ("Battery", "80% charged", Some("Battery\n80% charged")),
        ] {
            assert_eq!(flatten_tooltip(title, text).as_deref(), expected, "{title:?} {text:?}");
        }
    }

    /// A title/tooltip/status signal reads four properties and keeps the icon fields, so no pixmap is decoded.
    #[tokio::test]
    async fn a_text_refetch_keeps_the_icons_and_drops_the_menu() {
        use super::super::proxies::bind_item;
        use super::super::watcher::tests::StubStatusNotifierItem;
        use crate::capabilities::test_support::p2p_pair_serving;

        struct UntitledItem;
        #[zbus::interface(name = "org.kde.StatusNotifierItem")]
        impl UntitledItem {
            #[zbus(property, name = "Id")]
            fn id(&self) -> String {
                "untitled".to_string()
            }
        }

        let (connection, _peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/StatusNotifierItem", StubStatusNotifierItem)?.serve_at("/Untitled", UntitledItem)
        })
        .await;
        let destination = zbus::names::OwnedBusName::try_from(":1.5").unwrap();
        let path = OwnedObjectPath::try_from("/StatusNotifierItem").unwrap();
        let item = bind_item(&connection, &destination, &path).await.unwrap();
        let unique_name = OwnedUniqueName::try_from(":1.5").unwrap();
        let previous = TrayItem {
            name: "kept".to_string(),
            icon_name: Some("kept".to_string()),
            icon_path: Some("/kept.png".to_string()),
            menu: Some(Vec::new()),
            status: TrayStatus::Passive,
            ..TrayItem::default()
        };

        let next = fetch_tray_item_base(&item, &unique_name, &path, &previous, true).await;

        assert_eq!(
            (next.name.as_str(), next.status, next.icon_name.as_deref(), next.icon_path.as_deref()),
            ("Stub Item", TrayStatus::Active, Some("kept"), Some("/kept.png"))
        );
        assert_eq!(next.menu, None);

        let untitled = OwnedObjectPath::try_from("/Untitled").unwrap();
        let item = bind_item(&connection, &destination, &untitled).await.unwrap();
        let next = fetch_tray_item_base(&item, &unique_name, &untitled, &previous, true).await;
        assert_eq!(next.name, "kept", "a failed Title read keeps the name, though Id answered");
    }

    #[tokio::test]
    async fn a_get_reply_that_is_not_one_variant_keeps_the_previous_text() {
        use futures_util::StreamExt;
        use zbus::zvariant::Value;
        let (connection, peer) = crate::capabilities::test_support::p2p_pair().await;
        let mut calls = zbus::MessageStream::from(&peer);
        let replier = tokio::spawn(async move {
            while let Some(Ok(call)) = calls.next().await {
                let (_, name) = call.body().deserialize::<(String, String)>().unwrap();
                if name == "ToolTip" {
                    let tip: RawToolTip = (String::new(), Vec::new(), "answered".into(), String::new());
                    let _ = peer.reply(&call.header(), &Value::new(tip)).await;
                } else {
                    // Two variants, the first a valid string: only the body signature tells it from `v`.
                    let _ = peer.reply(&call.header(), &(Value::from("Evil"), Value::from(1u32))).await;
                }
            }
        });
        let destination = zbus::names::OwnedBusName::try_from(":1.5").unwrap();
        let path = OwnedObjectPath::try_from("/StatusNotifierItem").unwrap();
        let item = super::super::proxies::bind_item(&connection, &destination, &path).await.unwrap();
        let previous = TrayItem { name: "kept".to_string(), status: TrayStatus::Passive, ..TrayItem::default() };
        let unique_name = OwnedUniqueName::try_from(":1.5").unwrap();
        let next = fetch_tray_item_base(&item, &unique_name, &path, &previous, true).await;
        // The well-formed reply landing shows the others were answered and refused, not timed out.
        assert_eq!(next.tooltip.as_deref(), Some("answered"));
        assert_eq!((next.name.as_str(), next.status), ("kept", TrayStatus::Passive));
        replier.abort();
    }

    #[test]
    fn get_all_decodes_pixmaps_to_bytes_skips_unknown_keys_and_defaults_a_mistyped_one() {
        use std::collections::HashMap;
        use zbus::zvariant::serialized::Context;
        use zbus::zvariant::{LE, Value, to_bytes};
        let bytes = 256 * 1024;
        let pixmaps: Vec<RawIconPixmap> = vec![(256, 256, vec![7; bytes])];
        let tool_tip: RawToolTip = ("name".into(), vec![(1, 1, vec![1, 2, 3, 4])], "Title".into(), "Text".into());
        let props = HashMap::from([
            ("IconPixmap", Value::new(pixmaps.clone())),
            ("OverlayIconPixmap", Value::new(pixmaps.clone())),
            ("ToolTip", Value::new(tool_tip.clone())),
            ("Title", Value::new(5u32)),
            ("XUnknown", Value::new(vec![0u8; bytes])),
        ]);
        let encoded = to_bytes(Context::new_dbus(LE, 0), &props).unwrap();
        let (Dict(decoded), _): (Dict<Props>, _) = encoded.deserialize().unwrap();
        assert_eq!(decoded.pixmaps, [Some(pixmaps.clone()), None, Some(pixmaps)]);
        assert_eq!(decoded.tool_tip, Some(tool_tip.clone()));
        assert_eq!((decoded.title, decoded.id), (Some(String::new()), None), "mistyped is sent, empty");

        // A `Get` reply is a bare variant.
        let reply = to_bytes(Context::new_dbus(LE, 0), &Value::new(tool_tip.clone())).unwrap();
        assert_eq!(reply.deserialize_with_seed(Hint::<RawToolTip>::new()).unwrap().0, Some(tool_tip));
        let mistyped = to_bytes(Context::new_dbus(LE, 0), &Value::new(vec![0u8; bytes])).unwrap();
        assert_eq!(mistyped.deserialize_with_seed(Hint::<String>::new()).unwrap().0, None);
    }
}
