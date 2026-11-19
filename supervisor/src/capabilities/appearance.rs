//! `mantle.appearance` follows xdg-desktop-portal's `org.freedesktop.appearance` settings on the
//! session bus. Read-only: the portal owns the preferences and shells only follow them.
//!
//! A missing portal or key leaves its field at the default, and the portal gaining an owner later
//! is re-read, so the capability never fails.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{StreamExt, stream_select};
use serde::de::MapAccess;
use shared::debug;
pub use shared::state::appearance::{AppearanceState, ColorScheme, Contrast};
use tokio::sync::mpsc::UnboundedSender;
use zbus::proxy::CacheProperties;

use super::{Dict, DictFields, Hint, publish, variant_reply};

const NAMESPACE: &str = "org.freedesktop.appearance";
const INTERFACE: &str = "org.gnome.desktop.interface";
const ICON_THEME: &str = "icon-theme";

/// The `org.freedesktop.appearance` keys [`parse`] reads, decoded straight to their types: any
/// session client may own the portal's name while the portal is absent, and a `zvariant::Value`
/// holds an array as one `Value` per byte.
#[derive(Default)]
struct Appearance {
    color_scheme: Option<u32>,
    accent: Option<(f64, f64, f64)>,
    contrast: Option<u32>,
    reduced_motion: Option<u32>,
}

impl DictFields for Appearance {
    fn read<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        match key {
            "color-scheme" => self.color_scheme = map.next_value_seed(Hint::new())?,
            "accent-color" => self.accent = map.next_value_seed(Hint::new())?,
            "contrast" => self.contrast = map.next_value_seed(Hint::new())?,
            "reduced-motion" => self.reduced_motion = map.next_value_seed(Hint::new())?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalSettings {
    fn read_all(&self, namespaces: &[&str]) -> zbus::Result<HashMap<String, Dict<Appearance>>>;
}

pub struct AppearanceController {
    state: Arc<Mutex<AppearanceState>>,
}

impl AppearanceController {
    /// Returns at once; the portal is reached in a task. Without a session bus the defaults are
    /// pushed and never change.
    pub fn new(session_bus: Option<zbus::Connection>, events: UnboundedSender<()>) -> Self {
        let state = Arc::new(Mutex::new(AppearanceState::default()));
        match session_bus {
            Some(bus) => {
                tokio::spawn(run(bus, Arc::clone(&state), events));
            }
            None => {
                let _ = events.send(());
            }
        }
        Self { state }
    }

    pub fn snapshot(&self) -> AppearanceState {
        self.state.lock().expect("appearance state mutex poisoned").clone()
    }
}

/// `#rrggbb` for an sRGB triple, or `None` when any channel is outside `[0, 1]` (the portal's
/// "unset"); NaN fails the range check too.
fn accent_hex(rgb: (f64, f64, f64)) -> Option<String> {
    let channels = [rgb.0, rgb.1, rgb.2];
    if !channels.iter().all(|c| (0.0..=1.0).contains(c)) {
        return None;
    }
    let [r, g, b] = channels.map(|c| (c * 255.0).round() as u8);
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

fn parse(read: &Appearance) -> AppearanceState {
    AppearanceState {
        color_scheme: match read.color_scheme {
            Some(1) => ColorScheme::Dark,
            Some(2) => ColorScheme::Light,
            _ => ColorScheme::Default,
        },
        accent: read.accent.and_then(accent_hex),
        contrast: if read.contrast == Some(1) { Contrast::High } else { Contrast::Normal },
        reduced_motion: read.reduced_motion == Some(1),
    }
}

/// `None` when the portal is absent or the call fails, so the caller keeps the last state.
async fn read(proxy: &PortalSettingsProxy<'_>) -> Option<AppearanceState> {
    proxy
        .read_all(&[NAMESPACE])
        .await
        .inspect_err(|err| debug!("portal settings unavailable, keeping state: {err}"))
        .ok()
        .map(|mut settings| parse(&settings.remove(NAMESPACE).map(|Dict(read)| read).unwrap_or_default()))
}

/// The proxy and a stream that yields on each `SettingChanged` in `namespace` (and of `key`, if
/// given) and whenever the portal changes owner. Callers subscribe before the first read so a
/// change during the round trip is not lost.
async fn subscribe(
    bus: &zbus::Connection,
    namespace: &'static str,
    key: Option<&'static str>,
) -> zbus::Result<(PortalSettingsProxy<'static>, impl futures_util::Stream<Item = ()> + use<>)> {
    let proxy = PortalSettingsProxy::builder(bus).cache_properties(CacheProperties::No).build().await?;
    // Filtered on the two leading strings with the value unread, not by a match rule's args: zbus
    // tests those by decoding the whole body to `Value`s.
    let changed = proxy.inner().receive_signal("SettingChanged").await?.filter(move |signal| {
        let body = signal.body();
        let wanted = matches!(body.deserialize_unchecked::<(&str, &str)>(),
            Ok((space, name)) if space == namespace && key.is_none_or(|key| key == name));
        std::future::ready(wanted)
    });
    let changed = changed.map(drop);
    let owner = proxy.inner().receive_owner_changed().await?.map(drop);
    Ok((proxy, stream_select!(changed.fuse(), owner.fuse())))
}

/// Re-reads the namespace on each `SettingChanged` in it and whenever the portal changes owner.
async fn run(bus: zbus::Connection, state: Arc<Mutex<AppearanceState>>, events: UnboundedSender<()>) {
    let subscribed = subscribe(&bus, NAMESPACE, None).await;
    let Ok((proxy, changes)) = subscribed.inspect_err(|err| debug!("cannot follow the portal: {err}")) else {
        let _ = events.send(());
        return;
    };
    let mut changes = std::pin::pin!(changes);

    if let Some(first) = read(&proxy).await {
        *state.lock().expect("appearance state mutex poisoned") = first;
    }
    if events.send(()).is_err() {
        return;
    }
    while changes.next().await.is_some() {
        if let Some(next) = read(&proxy).await
            && !publish(&state, &events, next)
        {
            return;
        }
    }
}

/// Follows the portal's icon theme for the Renderer's icon lookup, outside the lazy capability:
/// icons resolve for every config, not only one that reads `mantle.appearance`.
pub fn spawn_icon_theme(themes: UnboundedSender<Option<String>>) {
    tokio::spawn(async move {
        if let Some(bus) = super::session_bus("icons follow only GTK's settings.ini").await {
            follow_icon_theme(bus, themes).await;
        }
    });
}

/// Sends each change of the theme; `None` is a missing key or an empty or non-string value.
/// Other failed reads keep the last theme, and nothing is sent until the portal answers.
async fn follow_icon_theme(bus: zbus::Connection, themes: UnboundedSender<Option<String>>) {
    let subscribed = subscribe(&bus, INTERFACE, Some(ICON_THEME)).await;
    let Ok((proxy, changes)) = subscribed.inspect_err(|err| debug!("cannot follow the portal's icon theme: {err}"))
    else {
        return;
    };
    let mut changes = std::pin::pin!(changes);
    let mut last = None;
    loop {
        let read = async {
            let reply = proxy.inner().call_method("ReadOne", &(INTERFACE, ICON_THEME)).await?;
            variant_reply::<String>(&reply, format_args!("ReadOne"))
        };
        let theme = match read.await.inspect_err(|err| debug!("no icon theme: {err}")) {
            Ok(theme) => Some(theme.filter(|theme| !theme.is_empty())),
            Err(zbus::Error::MethodError(name, ..)) if name.as_str() == "org.freedesktop.portal.Error.NotFound" => {
                Some(None)
            }
            Err(_) => None,
        };
        if let Some(theme) = theme
            && theme != last
        {
            last.clone_from(&theme);
            if themes.send(theme).is_err() {
                return;
            }
        }
        if changes.next().await.is_none() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;
    use zbus::zvariant::{OwnedValue, Value};

    use crate::capabilities::test_support::{PrivateBus, private_bus, within};

    struct FakePortal {
        values: Vec<(&'static str, Value<'static>)>,
    }

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.portal.Error")]
    enum PortalError {
        NotFound(String),
    }

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl FakePortal {
        fn read_all(&self, _namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
            let values = self.values.iter().map(|(key, v)| ((*key).to_string(), OwnedValue::try_from(v).unwrap()));
            HashMap::from([(NAMESPACE.to_string(), values.collect())])
        }

        fn read_one(&self, namespace: String, key: String) -> Result<OwnedValue, PortalError> {
            let found = self.values.iter().find(|(name, _)| namespace == INTERFACE && *name == key);
            found.map(|(_, v)| OwnedValue::try_from(v).unwrap()).ok_or(PortalError::NotFound(key))
        }

        #[zbus(signal)]
        async fn setting_changed(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            namespace: &str,
            key: &str,
            value: Value<'_>,
        ) -> zbus::Result<()>;
    }

    const PATH: &str = "/org/freedesktop/portal/desktop";

    async fn serve(bus: &PrivateBus, values: Vec<(&'static str, Value<'static>)>) -> zbus::Connection {
        bus.builder()
            .serve_at(PATH, FakePortal { values })
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    fn all_four(accent: (f64, f64, f64)) -> Vec<(&'static str, Value<'static>)> {
        vec![
            ("color-scheme", Value::from(1u32)),
            ("accent-color", Value::from(accent)),
            ("contrast", Value::from(1u32)),
            ("reduced-motion", Value::from(1u32)),
        ]
    }

    #[tokio::test]
    async fn the_first_read_maps_all_four_keys() {
        let bus = private_bus().await;
        let _portal = serve(&bus, all_four((1.0, 0.0, 0.5))).await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let appearance = AppearanceController::new(Some(bus.connection().await), events);
        within(changed.recv()).await;
        assert_eq!(
            appearance.snapshot(),
            AppearanceState {
                color_scheme: ColorScheme::Dark,
                accent: Some("#ff0080".into()),
                contrast: Contrast::High,
                reduced_motion: true,
            }
        );
    }

    /// A megabyte `ay` from whoever owns the portal's name, under a key nobody reads or as the
    /// icon theme, is skipped unbuilt.
    #[tokio::test]
    async fn a_byte_array_from_the_portal_is_skipped() {
        let bus = private_bus().await;
        let bytes = || Value::from(vec![7u8; 1024 * 1024]);
        let mut values = all_four((1.0, 0.0, 0.5));
        values.extend([("x-cover", bytes()), (ICON_THEME, Value::from("Papirus"))]);
        let portal = serve(&bus, values).await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let appearance = AppearanceController::new(Some(bus.connection().await), events);
        within(changed.recv()).await;
        assert_eq!(appearance.snapshot().color_scheme, ColorScheme::Dark);

        let (themes, mut sent) = mpsc::unbounded_channel();
        tokio::spawn(follow_icon_theme(bus.connection().await, themes));
        assert_eq!(within(sent.recv()).await, Some(Some("Papirus".into())));
        let fake = portal.object_server().interface::<_, FakePortal>(PATH).await.unwrap();
        fake.get_mut().await.values.last_mut().unwrap().1 = bytes();
        FakePortal::setting_changed(fake.signal_emitter(), INTERFACE, ICON_THEME, bytes()).await.unwrap();
        assert_eq!(within(sent.recv()).await, Some(None), "a theme that is not a string is none");
    }

    #[tokio::test]
    async fn a_setting_changed_in_the_namespace_is_followed_and_another_namespace_is_not() {
        let bus = private_bus().await;
        let portal = serve(&bus, all_four((0.0, 0.0, 0.0))).await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let appearance = AppearanceController::new(Some(bus.connection().await), events);
        within(changed.recv()).await;

        let fake = portal.object_server().interface::<_, FakePortal>(PATH).await.unwrap();
        fake.get_mut().await.values[0].1 = Value::from(2u32);
        FakePortal::setting_changed(fake.signal_emitter(), "org.example", "color-scheme", Value::from(2u32))
            .await
            .unwrap();
        // The values already changed, so an unfiltered re-read would publish a new state.
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), changed.recv()).await;
        assert!(quiet.is_err(), "the unrelated namespace must not wake a push");
        assert_eq!(appearance.snapshot().color_scheme, ColorScheme::Dark);

        FakePortal::setting_changed(fake.signal_emitter(), NAMESPACE, "color-scheme", Value::from(2u32)).await.unwrap();
        within(changed.recv()).await;
        assert_eq!(appearance.snapshot().color_scheme, ColorScheme::Light);
    }

    #[tokio::test]
    async fn a_portal_that_arrives_later_is_read_and_one_without_the_keys_gives_defaults() {
        let bus = private_bus().await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let appearance = AppearanceController::new(Some(bus.connection().await), events);
        within(changed.recv()).await;
        assert_eq!(appearance.snapshot(), AppearanceState::default(), "no portal yet");

        let _portal = serve(&bus, vec![("color-scheme", Value::from(2u32))]).await;
        within(changed.recv()).await;
        let state = appearance.snapshot();
        assert_eq!(state.color_scheme, ColorScheme::Light);
        assert_eq!((state.accent, state.contrast, state.reduced_motion), (None, Contrast::Normal, false));
    }

    #[tokio::test]
    async fn a_failed_read_keeps_the_last_state() {
        let bus = private_bus().await;
        let portal = serve(&bus, all_four((1.0, 0.0, 0.5))).await;
        let (events, mut changed) = mpsc::unbounded_channel();
        let appearance = AppearanceController::new(Some(bus.connection().await), events);
        within(changed.recv()).await;

        drop(portal);
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), changed.recv()).await;
        assert!(quiet.is_err(), "a portal that vanished is no change");
        assert_eq!(appearance.snapshot().color_scheme, ColorScheme::Dark);
    }

    #[tokio::test]
    async fn the_icon_theme_is_read_from_a_portal_that_arrives_later_and_each_change_is_forwarded() {
        let bus = private_bus().await;
        let (themes, mut sent) = mpsc::unbounded_channel();
        tokio::spawn(follow_icon_theme(bus.connection().await, themes));
        let portal = serve(&bus, vec![(ICON_THEME, Value::from("Papirus"))]).await;
        assert_eq!(within(sent.recv()).await, Some(Some("Papirus".into())));

        let fake = portal.object_server().interface::<_, FakePortal>(PATH).await.unwrap();
        for (theme, expected) in [("Tela-circle-dracula", Some("Tela-circle-dracula".into())), ("", None)] {
            fake.get_mut().await.values[0].1 = Value::from(theme);
            FakePortal::setting_changed(fake.signal_emitter(), INTERFACE, ICON_THEME, Value::from(theme))
                .await
                .unwrap();
            assert_eq!(within(sent.recv()).await, Some(expected));
        }
        fake.get_mut().await.values[0].1 = Value::from("unannounced");
        FakePortal::setting_changed(fake.signal_emitter(), INTERFACE, "cursor-theme", Value::from("x")).await.unwrap();

        drop(fake);
        drop(portal);
        let portal = serve(&bus, vec![(ICON_THEME, Value::from("Adwaita"))]).await;
        assert_eq!(within(sent.recv()).await, Some(Some("Adwaita".into())));

        drop(portal);
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), sent.recv()).await;
        assert!(quiet.is_err(), "a portal that vanished must keep the last icon theme");

        let _portal = serve(&bus, vec![]).await;
        assert_eq!(within(sent.recv()).await, Some(None));
    }

    #[test]
    fn an_accent_outside_zero_to_one_is_unset() {
        assert_eq!(accent_hex((0.0, 1.0, 0.2)), Some("#00ff33".into()));
        assert_eq!(accent_hex((-1.0, -1.0, -1.0)), None);
        assert_eq!(accent_hex((0.5, 1.01, 0.5)), None);
        assert_eq!(accent_hex((f64::NAN, 0.0, 0.0)), None);
    }

    #[test]
    fn an_undefined_enum_value_is_no_preference() {
        let read = Appearance { color_scheme: Some(7), contrast: Some(9), ..Appearance::default() };
        assert_eq!(parse(&read), AppearanceState::default());
    }
}
