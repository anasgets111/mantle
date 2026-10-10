//! Capability actions: the `(action, [arguments])` a config call sends, decoded into its
//! capability's enum. Both binaries decode (ADR-0291): the Renderer to raise at the config line,
//! the Supervisor because the Renderer is untrusted (ADR-0114).

mod catalog;

pub use catalog::*;
use std::cell::Cell;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Capability;

/// The action name, its arguments, and where to note which argument failed to decode.
struct Invocation<'a>(&'a str, &'a [Value], &'a Cell<Option<usize>>);
struct Arguments<'a>(&'a [Value], &'a Cell<Option<usize>>);

/// Hands out the arguments in order and records the slot of the one that fails.
struct Positional<'a> {
    rest: std::slice::Iter<'a, Value>,
    next: usize,
    failed: &'a Cell<Option<usize>>,
}

impl<'de> serde::de::SeqAccess<'de> for Positional<'de> {
    type Error = serde_json::Error;
    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some(value) = self.rest.next() else { return Ok(None) };
        self.next += 1;
        seed.deserialize(value).map(Some).inspect_err(|_| self.failed.set(Some(self.next - 1)))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.rest.len())
    }
}

impl<'de> serde::de::EnumAccess<'de> for Invocation<'de> {
    type Error = serde_json::Error;
    type Variant = Arguments<'de>;
    fn variant_seed<S: serde::de::DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self::Variant), Self::Error> {
        use serde::de::IntoDeserializer;
        let Invocation(action, mut arguments, failed) = self;
        // Lua sends a trailing `nil` as `null`: an omitted argument, not an extra one.
        while let [rest @ .., Value::Null] = arguments {
            arguments = rest;
        }
        Ok((seed.deserialize(action.into_deserializer())?, Arguments(arguments, failed)))
    }
}

impl<'de> serde::de::VariantAccess<'de> for Arguments<'de> {
    type Error = serde_json::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.0.len() {
            0 => Ok(()),
            n => Err(serde::de::Error::invalid_length(n, &"no arguments")),
        }
    }
    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(self, _: T) -> Result<T::Value, Self::Error> {
        Err(serde::de::Error::custom("an action names its fields"))
    }
    fn tuple_variant<V: serde::de::Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Self::Error> {
        let mut seq = Positional { rest: self.0.iter(), next: 0, failed: self.1 };
        let decoded = visitor.visit_seq(&mut seq)?;
        match seq.rest.len() {
            0 => Ok(decoded),
            _ => Err(serde::de::Error::invalid_length(self.0.len(), &&*format!("{} elements", seq.next))),
        }
    }
    fn struct_variant<V: serde::de::Visitor<'de>>(
        self,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.tuple_variant(0, visitor)
    }
}

/// Decodes `action` and its positional `arguments` into `A`'s variant (ADR-0037). Serde owns the
/// accepted spellings and argument types; `supervisor/src/stubs.rs` generates Lua from the same enum.
pub fn decode<A: DeserializeOwned>(action: &str, arguments: &[Value]) -> Result<A, serde_json::Error> {
    decode_at(action, arguments, &Cell::new(None))
}

fn decode_at<A: DeserializeOwned>(
    action: &str,
    arguments: &[Value],
    failed: &Cell<Option<usize>>,
) -> Result<A, serde_json::Error> {
    A::deserialize(serde::de::value::EnumAccessDeserializer::new(Invocation(action, arguments, failed)))
}

/// Whether `arguments` fit `capability`'s `action`. Only the shape: a check that needs live state,
/// such as whether a profile or MAC exists, stays with the Supervisor. An error carries the index of
/// the argument that failed to decode, `None` for a wrong count or an unknown action.
pub fn check(
    capability: Capability,
    action: &str,
    arguments: &[Value],
) -> Result<(), (Option<usize>, serde_json::Error)> {
    let failed = Cell::new(None);
    macro_rules! fits {
        ($ty:ty) => {
            decode_at::<$ty>(action, arguments, &failed).map(drop).map_err(|err| (failed.get(), err))
        };
    }
    match capability {
        Capability::Applications => fits!(ApplicationsAction),
        Capability::Audio => fits!(AudioAction),
        Capability::Bluetooth => fits!(BluetoothAction),
        Capability::Brightness => fits!(BrightnessAction),
        Capability::Files => fits!(FilesAction),
        Capability::Processes => fits!(ProcessesAction),
        Capability::Keyboard => fits!(KeyboardAction),
        Capability::Lock => fits!(LockAction),
        Capability::Mpris => fits!(MprisAction),
        Capability::Network => fits!(NetworkAction),
        Capability::Notifications => fits!(NotificationsAction),
        Capability::Radio => fits!(RadioAction),
        Capability::Power => fits!(PowerAction),
        Capability::Sysinfo => fits!(SysinfoAction),
        Capability::Storage => fits!(StorageAction),
        Capability::Polkit => fits!(PolkitAction),
        Capability::Tray => fits!(TrayAction),
        Capability::Updates => fits!(UpdatesAction),
        Capability::Workspaces => fits!(WorkspacesAction),
        Capability::Windows => fits!(WindowsAction),
        Capability::System => fits!(SystemAction),
        Capability::Appearance | Capability::Battery | Capability::Idle | Capability::Privacy | Capability::Secrets => {
            Err((None, serde::de::Error::custom("it has no actions")))
        }
    }
}

/// A list that may be omitted or `nil`; mlua sends an empty Lua table as `{}`.
fn lua_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    match <Option<Value> as serde::Deserialize>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(Value::Object(map)) if map.is_empty() => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value).map_err(serde::de::Error::custom),
    }
}

/// A string argument used as a key, where empty would name nothing.
fn non_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_empty() { Err(serde::de::Error::custom("expected a non-empty string")) } else { Ok(value) }
}

fn absolute<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let path = <String as serde::Deserialize>::deserialize(deserializer)?;
    if path.starts_with('/') { Ok(path) } else { Err(serde::de::Error::custom("expected an absolute path")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire stays `(action, [arguments])`; each case is a coercion the hand parsers once got
    /// wrong or a Lua marshalling quirk a config really sends.
    #[test]
    fn a_command_decodes_positionally_into_its_typed_variant_or_says_why_not() {
        use serde_json::json;

        fn decode<A: DeserializeOwned + std::fmt::Debug>(action: &str, arguments: Value) -> Result<String, String> {
            let Value::Array(arguments) = arguments else { panic!("arguments are a list") };
            super::decode::<A>(action, &arguments).map(|a| format!("{a:?}")).map_err(|e| e.to_string())
        }

        for (decoded, expected) in [
            (decode::<NetworkAction>("scan", json!([])), Ok("Scan")),
            (decode::<NetworkAction>("scan", json!([null])), Ok("Scan")),
            (decode::<NetworkAction>("scan", json!([1])), Err("invalid length 1, expected no arguments")),
            (
                decode::<NetworkAction>("connect", json!(["Home", true])),
                Ok(r#"Connect { ssid: "Home", hidden: true }"#),
            ),
            (decode::<NetworkAction>("connect", json!([1, true])), Err("invalid type")),
            (decode::<NetworkAction>("connect", json!(["Home"])), Err("invalid length 1")),
            (decode::<NetworkAction>("connect", json!(["Home", true, 3])), Err("invalid length 3")),
            (decode::<NetworkAction>("unlock", json!([])), Err("unknown variant")),
            // Lua has one number type, so `1` is a volume.
            (decode::<AudioAction>("set_volume", json!([1])), Ok("SetVolume { volume: 1.0 }")),
            (decode::<AudioAction>("set_default_sink", json!([u64::from(u32::MAX) + 1])), Err("u32")),
            (decode::<TrayAction>("activate_menu_item", json!(["1.42", 1u64 << 31])), Err("i32")),
            (decode::<TrayAction>("scroll", json!(["1.42", 1, "diagonal"])), Err("unknown variant")),
            (
                decode::<MprisAction>("set_loop_status", json!(["mpv", "track"])),
                Ok(r#"SetLoopStatus { id: "mpv", value: Track }"#),
            ),
            (decode::<MprisAction>("set_loop_status", json!(["mpv", "Track"])), Err("unknown variant")),
            (
                decode::<RadioAction>("set_blocked", json!(["bluetooth", true])),
                Ok("SetBlocked { kind: Bluetooth, blocked: true }"),
            ),
            (decode::<RadioAction>("set_blocked", json!(["Wlan", true])), Err("unknown variant")),
            (decode::<RadioAction>("set_all_blocked", json!([false])), Ok("SetAllBlocked { blocked: false }")),
            (decode::<LockAction>("set_unlock_animation", json!([])), Ok("SetUnlockAnimation { ms: None }")),
            (decode::<LockAction>("set_unlock_animation", json!(["fast"])), Err("invalid type")),
            (decode::<NotificationsAction>("hold_expiry", json!([-1])), Err("invalid value")),
            (decode::<NotificationsAction>("invoke_action", json!([7, ""])), Err("non-empty")),
            (
                decode::<ProcessesAction>("start", json!(["rec", "true", {}])),
                Ok(r#"Start { name: "rec", cmd: "true", args: [] }"#),
            ),
            (decode::<ProcessesAction>("start", json!(["rec", ""])), Err("non-empty")),
            (decode::<ProcessesAction>("declare", json!(["rec", "SIGUSR2"])), Err("unknown variant")),
            (decode::<FilesAction>("watch", json!(["/walls", {}])), Ok(r#"Watch { path: "/walls", extensions: [] }"#)),
            (decode::<FilesAction>("watch", json!(["/walls", {"jpg": true}])), Err("invalid type")),
            (decode::<FilesAction>("unwatch", json!(["walls"])), Err("absolute")),
            (
                decode::<UpdatesAction>("configure", json!([{"interval": 3600, "packages": {}}])),
                Ok(
                    "Configure { config: UpdatesConfigure { interval_secs: 3600, checked_at: None, packages: [], aur: false } }",
                ),
            ),
            // A Lua `nil` value arrives as a missing argument and means delete.
            (
                decode::<StorageAction>("set", json!(["/s.json", "theme"])),
                Ok(r#"Set { path: "/s.json", key: "theme", value: Null }"#),
            ),
        ] {
            match (&decoded, expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want),
                (Err(got), Err(want)) => assert!(got.contains(want), "{got:?} does not mention {want:?}"),
                _ => panic!("decoded {decoded:?}, expected {expected:?}"),
            }
        }
    }
}
