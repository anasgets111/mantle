//! `renderer/src/check_samples.json`: one sample payload per capability, which `mantle check` pushes
//! before its second layout so every `list` has a row for its `itemfn` (ADR-0267).
//!
//! Walked from the same schemas as the stubs, so a new field or capability restales the file and
//! `the_generated_stub_matches_what_is_checked_in` names it.

use serde_json::{Map, Value, json};

/// Every capability's sample payload, keyed by name, as the checked-in JSON.
pub(super) fn render() -> String {
    let samples: Map<String, Value> = super::capability_schemas()
        .into_iter()
        .map(|(capability, payload, _)| {
            let root = payload.as_value();
            (capability.to_string(), sample(root, root, &mut Vec::new()).unwrap_or(Value::Null))
        })
        .collect();
    format!("{:#}\n", Value::Object(samples))
}

/// A value `fragment` accepts: every array holds one element, every `Option` is `Some`, every map
/// one entry, every enum its first variant. `None` for a type already being built above it, so a
/// recursive type (a menu of menus) ends in an empty array rather than looping.
fn sample<'a>(fragment: &'a Value, root: &'a Value, path: &mut Vec<&'a str>) -> Option<Value> {
    if let Some(reference) = fragment.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next()?;
        if path.contains(&name) {
            return None;
        }
        path.push(name);
        let value = sample(root.get("$defs")?.get(name)?, root, path);
        path.pop();
        return value;
    }
    if let Some(value) = fragment.get("const") {
        return Some(value.clone());
    }
    if let Some(first) = fragment.get("enum").and_then(Value::as_array).and_then(|e| e.iter().find(|v| !v.is_null())) {
        return Some(first.clone());
    }
    // `Option<Struct>` and documented or tagged enums: the first branch that is not `null`.
    if let Some(branches) = fragment.get("anyOf").or_else(|| fragment.get("oneOf")).and_then(Value::as_array) {
        return branches.iter().find(|b| b.get("type").and_then(Value::as_str) != Some("null")).and_then(|b| {
            // A tagged variant's `kind` sits in the branch, the shared fields beside `oneOf`.
            let mut value = sample(b, root, path)?;
            if let Some(own) = value.as_object_mut() {
                fill(own, fragment, root, path);
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
        Some("boolean") => json!(true),
        Some("array") => {
            Value::Array(fragment.get("items").and_then(|item| sample(item, root, path)).into_iter().collect())
        }
        Some("object") => {
            let mut object = Map::new();
            fill(&mut object, fragment, root, path);
            Value::Object(object)
        }
        // `serde_json::Value`: any shape, so the one every capability shares.
        _ => json!("sample"),
    })
}

/// `fragment`'s own fields into `object`, plus one entry when it is a map.
fn fill<'a>(object: &mut Map<String, Value>, fragment: &'a Value, root: &'a Value, path: &mut Vec<&'a str>) {
    for (field, property) in fragment.get("properties").and_then(Value::as_object).into_iter().flatten() {
        object.extend(sample(property, root, path).map(|value| (field.clone(), value)));
    }
    if let Some(value) = fragment.get("additionalProperties").and_then(|v| sample(v, root, path)) {
        object.insert("sample".into(), value);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
            super::sample(&root, &root, &mut Vec::new()),
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
}
