//! `mantle.appearance` follows xdg-desktop-portal's `org.freedesktop.appearance` settings on the
//! session bus. Read-only: the portal owns the preferences and shells only follow them.
//!
//! A missing portal or key leaves its field at the default, and the portal gaining an owner later
//! is re-read, so the capability never fails.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{StreamExt, stream_select};
use shared::debug;
pub use shared::state::appearance::{AppearanceState, ColorScheme, Contrast};
use tokio::sync::mpsc::UnboundedSender;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

use super::publish;

const NAMESPACE: &str = "org.freedesktop.appearance";

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalSettings {
    fn read_all(&self, namespaces: &[&str]) -> zbus::Result<Settings>;

    #[zbus(signal)]
    fn setting_changed(&self, namespace: &str, key: &str, value: OwnedValue) -> zbus::Result<()>;
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

fn parse(settings: &Settings) -> AppearanceState {
    let key = |name: &str| settings.get(NAMESPACE)?.get(name);
    let number = |name: &str| key(name).and_then(|value| u32::try_from(value).ok());
    AppearanceState {
        color_scheme: match number("color-scheme") {
            Some(1) => ColorScheme::Dark,
            Some(2) => ColorScheme::Light,
            _ => ColorScheme::Default,
        },
        accent: key("accent-color")
            .and_then(|value| <(f64, f64, f64)>::try_from(value.try_clone().ok()?).ok())
            .and_then(accent_hex),
        contrast: if number("contrast") == Some(1) { Contrast::High } else { Contrast::Normal },
        reduced_motion: number("reduced-motion") == Some(1),
    }
}

/// Defaults when the portal is absent or the call fails.
async fn read(proxy: &PortalSettingsProxy<'_>) -> AppearanceState {
    match proxy.read_all(&[NAMESPACE]).await {
        Ok(settings) => parse(&settings),
        Err(err) => {
            debug!("portal settings unavailable, using defaults: {err}");
            AppearanceState::default()
        }
    }
}

/// Subscribes before the first read so a change during the round trip is not lost, then re-reads
/// the namespace on each `SettingChanged` in it and whenever the portal changes owner.
async fn run(bus: zbus::Connection, state: Arc<Mutex<AppearanceState>>, events: UnboundedSender<()>) {
    let subscribed = async {
        let proxy = PortalSettingsProxy::builder(&bus).cache_properties(CacheProperties::No).build().await?;
        let changed = proxy.receive_setting_changed().await?.filter_map(|signal| {
            std::future::ready(signal.args().ok().filter(|args| args.namespace == NAMESPACE).map(drop))
        });
        let owner = proxy.inner().receive_owner_changed().await?.map(drop);
        zbus::Result::Ok((proxy, stream_select!(changed.fuse(), owner.fuse())))
    };
    let Ok((proxy, changes)) = subscribed.await.inspect_err(|err| debug!("cannot follow the portal: {err}")) else {
        let _ = events.send(());
        return;
    };
    let mut changes = std::pin::pin!(changes);

    *state.lock().expect("appearance state mutex poisoned") = read(&proxy).await;
    if events.send(()).is_err() {
        return;
    }
    while changes.next().await.is_some() {
        if !publish(&state, &events, read(&proxy).await) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;
    use zbus::zvariant::Value;

    use crate::capabilities::test_support::{PrivateBus, private_bus, within};

    struct FakePortal {
        values: Vec<(&'static str, Value<'static>)>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl FakePortal {
        fn read_all(&self, _namespaces: Vec<String>) -> Settings {
            let values = self.values.iter().map(|(key, v)| ((*key).to_string(), OwnedValue::try_from(v).unwrap()));
            HashMap::from([(NAMESPACE.to_string(), values.collect())])
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
        FakePortal::setting_changed(fake.signal_emitter(), NAMESPACE, "color-scheme", Value::from(2u32)).await.unwrap();
        within(changed.recv()).await;
        assert_eq!(appearance.snapshot().color_scheme, ColorScheme::Light);
        assert!(changed.try_recv().is_err(), "the unrelated namespace must not wake a push");
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

    #[test]
    fn an_accent_outside_zero_to_one_is_unset() {
        assert_eq!(accent_hex((0.0, 1.0, 0.2)), Some("#00ff33".into()));
        assert_eq!(accent_hex((-1.0, -1.0, -1.0)), None);
        assert_eq!(accent_hex((0.5, 1.01, 0.5)), None);
        assert_eq!(accent_hex((f64::NAN, 0.0, 0.0)), None);
    }

    #[test]
    fn an_undefined_enum_value_is_no_preference() {
        let value = |v: u32| OwnedValue::try_from(Value::from(v)).unwrap();
        let settings = HashMap::from([(
            NAMESPACE.to_string(),
            HashMap::from([("color-scheme".to_string(), value(7)), ("contrast".to_string(), value(9))]),
        )]);
        assert_eq!(parse(&settings), AppearanceState::default());
    }
}
