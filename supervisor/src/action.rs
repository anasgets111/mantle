use shared::warn;

struct Invocation(String, Vec<serde_json::Value>);
struct Arguments(Vec<serde_json::Value>);

impl<'de> serde::de::EnumAccess<'de> for Invocation {
    type Error = serde_json::Error;
    type Variant = Arguments;
    fn variant_seed<S: serde::de::DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Arguments), Self::Error> {
        use serde::de::IntoDeserializer;
        let Invocation(action, mut arguments) = self;
        // Lua sends a trailing `nil` as `null`: an omitted argument, not an extra one.
        while arguments.last().is_some_and(serde_json::Value::is_null) {
            arguments.pop();
        }
        Ok((seed.deserialize(action.into_deserializer())?, Arguments(arguments)))
    }
}

impl<'de> serde::de::VariantAccess<'de> for Arguments {
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
        serde::Deserializer::deserialize_any(serde::de::value::SeqDeserializer::new(self.0.into_iter()), visitor)
    }
    fn struct_variant<V: serde::de::Visitor<'de>>(
        self,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.tuple_variant(0, visitor)
    }
}

/// Decodes a command into its capability's action enum, logging and returning `None` when the
/// action is unknown or its arguments do not fit the variant (ADR-0037). Serde owns the accepted
/// spellings and argument types; `supervisor/src/stubs.rs` generates Lua from the same enum.
pub(crate) fn parse_action<A: serde::de::DeserializeOwned>(params: &shared::CommandParams) -> Option<A> {
    let invocation = Invocation(params.action.clone(), params.arguments.clone());
    A::deserialize(serde::de::value::EnumAccessDeserializer::new(invocation))
        .map_err(|err| {
            warn!(
                "malformed {}.{} command from generation {}: {err}; arguments {:?}",
                params.capability, params.action, params.generation_id, params.arguments
            )
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire stays `(action, [arguments])`; each case is a coercion the hand parsers once got
    /// wrong or a Lua marshalling quirk a config really sends.
    #[test]
    fn a_command_decodes_positionally_into_its_typed_variant_or_says_why_not() {
        use crate::capabilities::{
            audio::AudioAction, files::FilesAction, lock::controller::LockAction, network::NetworkAction,
            notifications::NotificationsAction, processes::ProcessesAction, storage::StorageAction, tray::TrayAction,
            updates::UpdatesAction,
        };
        use serde_json::json;

        fn decode<A: serde::de::DeserializeOwned + std::fmt::Debug>(
            action: &str,
            arguments: serde_json::Value,
        ) -> Result<String, String> {
            let serde_json::Value::Array(arguments) = arguments else { panic!("arguments are a list") };
            A::deserialize(serde::de::value::EnumAccessDeserializer::new(Invocation(action.to_string(), arguments)))
                .map(|a| format!("{a:?}"))
                .map_err(|e| e.to_string())
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
