//! Sample payloads for the config checker, derived from the stub schemas.

use serde_json::{Map, Value, json};

/// Which values a sample set picks; each set is one `mantle check` pass (ADR-0267).
#[derive(Clone, Copy, PartialEq)]
enum Set {
    /// Every array and map one entry, every enum its first variant, every boolean `true`.
    First,
    /// As `First`, but every enum its last variant and every boolean `false`.
    Alternate,
    /// As `First`, but every array and map empty.
    Empty,
}

/// Every sample set keyed by `first`, `alternate` and `empty`, each holding every capability's
/// payload keyed by name, as JSON for build-time embedding.
pub fn check_samples() -> String {
    let sets: Map<String, Value> = [("first", Set::First), ("alternate", Set::Alternate), ("empty", Set::Empty)]
        .into_iter()
        .map(|(name, set)| {
            let samples: Map<String, Value> = super::capability_schemas()
                .into_iter()
                .map(|(capability, payload, _)| {
                    let root = payload.as_value();
                    (capability.to_string(), sample(root, root, set, &mut Vec::new()).unwrap_or(Value::Null))
                })
                .collect();
            (name.to_string(), Value::Object(samples))
        })
        .collect();
    format!("{:#}\n", Value::Object(sets))
}

/// A value `fragment` accepts, chosen by `set`. Every `Option` is `Some`.
/// `None` for a type already being built above it, so a recursive type (a menu of menus) ends in
/// an empty array rather than looping.
fn sample<'a>(fragment: &'a Value, root: &'a Value, set: Set, path: &mut Vec<&'a str>) -> Option<Value> {
    if let Some(reference) = fragment.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next()?;
        if path.contains(&name) {
            return None;
        }
        path.push(name);
        let value = sample(root.get("$defs")?.get(name)?, root, set, path);
        path.pop();
        return value;
    }
    if let Some(value) = fragment.get("const") {
        return Some(value.clone());
    }
    if let Some(variant) = fragment.get("enum").and_then(Value::as_array).and_then(|e| pick(e, set, |v| !v.is_null())) {
        return Some(variant.clone());
    }
    // `Option<Struct>` and documented or tagged enums: a branch that is not `null`.
    if let Some(branches) = fragment.get("anyOf").or_else(|| fragment.get("oneOf")).and_then(Value::as_array) {
        return pick(branches, set, |b| b.get("type").and_then(Value::as_str) != Some("null")).and_then(|b| {
            // A tagged variant's `kind` sits in the branch, the shared fields beside `oneOf`.
            let mut value = sample(b, root, set, path)?;
            if let Some(own) = value.as_object_mut() {
                fill(own, fragment, root, set, path);
            }
            Some(value)
        });
    }
    let type_name = match fragment.get("type") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).find(|n| *n != "null"),
        Some(name) => name.as_str(),
        None => None,
    };
    Some(match type_name {
        Some("string") => json!("sample"),
        Some("integer") => json!(1),
        Some("number") => json!(0.5),
        Some("boolean") => json!(set != Set::Alternate),
        Some("array") if set == Set::Empty => json!([]),
        Some("array") => {
            Value::Array(fragment.get("items").and_then(|item| sample(item, root, set, path)).into_iter().collect())
        }
        Some("object") => {
            let mut object = Map::new();
            fill(&mut object, fragment, root, set, path);
            Value::Object(object)
        }
        // `serde_json::Value`: any shape, so the one every capability shares.
        _ => json!("sample"),
    })
}

/// The first variant `keep` accepts, or the last one for [`Set::Alternate`].
fn pick(variants: &[Value], set: Set, keep: impl Fn(&Value) -> bool) -> Option<&Value> {
    if set == Set::Alternate { variants.iter().rfind(|v| keep(v)) } else { variants.iter().find(|v| keep(v)) }
}

/// `fragment`'s own fields into `object`, plus one entry when it is a map outside [`Set::Empty`].
fn fill<'a>(object: &mut Map<String, Value>, fragment: &'a Value, root: &'a Value, set: Set, path: &mut Vec<&'a str>) {
    for (field, property) in fragment.get("properties").and_then(Value::as_object).into_iter().flatten() {
        object.extend(sample(property, root, set, path).map(|value| (field.clone(), value)));
    }
    if set != Set::Empty
        && let Some(value) = fragment.get("additionalProperties").and_then(|v| sample(v, root, set, path))
    {
        object.insert("sample".into(), value);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn samples_cover_the_roster() {
        let sets: serde_json::Value = serde_json::from_str(&super::check_samples()).unwrap();
        for set in ["first", "alternate", "empty"] {
            for capability in crate::Capability::ALL {
                assert!(sets[set][capability.as_str()].is_object(), "{set} {capability}");
            }
        }
        assert_eq!(sets["first"]["applications"]["by_app_id"]["sample"], 1);
    }

    /// Each shape a `*State` uses, with a recursive `$ref` that must stop rather than loop.
    #[test]
    fn a_sample_fills_every_option_array_and_map_and_stops_at_recursion() {
        let root = json!({
            "type": "object",
            "properties": {
                "name": { "type": ["string", "null"] },
                "level": { "type": "integer" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "by_id": { "type": "object", "additionalProperties": { "type": "number" } },
                "urgency": { "enum": ["low", "normal"] },
                "menu": { "anyOf": [{ "$ref": "#/$defs/Menu" }, { "type": "null" }] }
            },
            "$defs": {
                "Menu": { "type": "object", "properties": {
                    "label": { "type": "string" },
                    "children": { "type": "array", "items": { "$ref": "#/$defs/Menu" } }
                } }
            }
        });
        assert_eq!(
            super::sample(&root, &root, super::Set::First, &mut Vec::new()),
            Some(json!({
                "name": "sample",
                "level": 1,
                "tags": ["sample"],
                "by_id": { "sample": 0.5 },
                "urgency": "low",
                "menu": { "label": "sample", "children": [] }
            }))
        );
    }

    /// `Alternate` takes the other end of every choice; `Empty` drops every array and map entry.
    #[test]
    fn the_alternate_and_empty_sets_take_the_other_value() {
        let root = json!({
            "type": "object",
            "properties": {
                "on": { "type": "boolean" },
                "urgency": { "enum": ["low", "normal", "critical"] },
                "kind": { "oneOf": [{ "const": "wired" }, { "const": "wireless" }] },
                "tags": { "type": "array", "items": { "type": "string" } },
                "by_id": { "type": "object", "additionalProperties": { "type": "number" } }
            }
        });
        let sample = |set| super::sample(&root, &root, set, &mut Vec::new());
        assert_eq!(
            sample(super::Set::Alternate),
            Some(
                json!({ "on": false, "urgency": "critical", "kind": "wireless", "tags": ["sample"], "by_id": { "sample": 0.5 } })
            )
        );
        assert_eq!(
            sample(super::Set::Empty),
            Some(json!({ "on": true, "urgency": "low", "kind": "wired", "tags": [], "by_id": {} }))
        );
    }
}
