//! Capability actions: the `(action, [arguments])` a config call sends, decoded into its
//! capability's enum. Both binaries decode (ADR-0291): the Renderer to raise at the config line,
//! the Supervisor because the Renderer is untrusted (ADR-0114).

mod catalog;

pub use catalog::*;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Capability;

struct Invocation<'a>(&'a str, &'a [Value]);
struct Arguments<'a>(&'a [Value]);

/// Hands out the arguments in order; a failing one's error leads with `argument N: `, which the
/// Renderer reads back to name the slot.
struct Positional<'a>(std::iter::Enumerate<std::slice::Iter<'a, Value>>);

impl<'de> serde::de::SeqAccess<'de> for Positional<'de> {
    type Error = serde_json::Error;
    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some((index, value)) = self.0.next() else { return Ok(None) };
        let at = |err| serde::de::Error::custom(format_args!("argument {}: {err}", index + 1));
        seed.deserialize(value).map(Some).map_err(at)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
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
        let Invocation(action, mut arguments) = self;
        // Lua sends a trailing `nil` as `null`: an omitted argument, not an extra one.
        while let [rest @ .., Value::Null] = arguments {
            arguments = rest;
        }
        Ok((seed.deserialize(action.into_deserializer())?, Arguments(arguments)))
    }
}

impl<'de> serde::de::VariantAccess<'de> for Arguments<'de> {
    type Error = serde_json::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.0.len() {
            0 => Ok(()),
            n => Err(serde::de::Error::invalid_length(n, &"0 elements")),
        }
    }
    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(self, _: T) -> Result<T::Value, Self::Error> {
        Err(serde::de::Error::custom("an action names its fields"))
    }
    fn tuple_variant<V: serde::de::Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Self::Error> {
        let mut seq = Positional(self.0.iter().enumerate());
        let decoded = visitor.visit_seq(&mut seq)?;
        match seq.0.len() {
            0 => Ok(decoded),
            left => Err(serde::de::Error::invalid_length(self.0.len(), &&*format!("{} elements", self.0.len() - left))),
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
    A::deserialize(serde::de::value::EnumAccessDeserializer::new(Invocation(action, arguments)))
}

/// Whether `arguments` fit `capability`'s `action`. Only the shape: a check that needs live state,
/// such as whether a profile or MAC exists, stays with the Supervisor.
pub fn check(capability: Capability, action: &str, arguments: &[Value]) -> Result<(), serde_json::Error> {
    fn fits<A: DeserializeOwned>(action: &str, arguments: &[Value]) -> Result<(), serde_json::Error> {
        decode::<A>(action, arguments).map(drop)
    }
    match capability {
        Capability::Applications => fits::<ApplicationsAction>(action, arguments),
        Capability::Audio => fits::<AudioAction>(action, arguments),
        Capability::Bluetooth => fits::<BluetoothAction>(action, arguments),
        Capability::Brightness => fits::<BrightnessAction>(action, arguments),
        Capability::Files => fits::<FilesAction>(action, arguments),
        Capability::Processes => fits::<ProcessesAction>(action, arguments),
        Capability::Keyboard => fits::<KeyboardAction>(action, arguments),
        Capability::Lock => fits::<LockAction>(action, arguments),
        Capability::Mpris => fits::<MprisAction>(action, arguments),
        Capability::Network => fits::<NetworkAction>(action, arguments),
        Capability::Notifications => fits::<NotificationsAction>(action, arguments),
        Capability::Radio => fits::<RadioAction>(action, arguments),
        Capability::Power => fits::<PowerAction>(action, arguments),
        Capability::Sysinfo => fits::<SysinfoAction>(action, arguments),
        Capability::Storage => fits::<StorageAction>(action, arguments),
        Capability::Polkit => fits::<PolkitAction>(action, arguments),
        Capability::Tray => fits::<TrayAction>(action, arguments),
        Capability::Updates => fits::<UpdatesAction>(action, arguments),
        Capability::Workspaces => fits::<WorkspacesAction>(action, arguments),
        Capability::Windows => fits::<WindowsAction>(action, arguments),
        Capability::System => fits::<SystemAction>(action, arguments),
        Capability::Appearance | Capability::Battery | Capability::Idle | Capability::Privacy | Capability::Secrets => {
            Err(serde::de::Error::custom("it has no actions"))
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
            (decode::<NetworkAction>("scan", json!([1])), Err("invalid length 1, expected 0 elements")),
            (
                decode::<NetworkAction>("connect", json!(["Home", true])),
                Ok(r#"Connect { ssid: "Home", hidden: true }"#),
            ),
            (decode::<NetworkAction>("connect", json!([1, true])), Err("argument 1: invalid type")),
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
