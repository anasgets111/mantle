//! Schemas used by stub generation and the Renderer build.

mod samples;
pub use samples::check_samples;

pub use crate::capability::capability_schemas;
use serde_json::Value;

/// Each action in an action enum's schema as `(name, branch)`. The names are serde's, renames
/// included, so they are the Lua method names. External tagging makes a fieldless action an `enum`
/// string or, once documented, a `const` branch; any other is a one-key object whose key is its
/// name. schemars puts the undocumented fieldless actions first, then the rest in declaration order.
pub fn action_variants(actions: &Value) -> Vec<(&str, &Value)> {
    let branches = actions.get("oneOf").and_then(Value::as_array).map_or_else(|| vec![actions], |b| b.iter().collect());
    let mut variants = Vec::new();
    for branch in branches {
        if let Some(names) = branch.get("enum").and_then(Value::as_array) {
            variants.extend(names.iter().map(|name| (name.as_str().expect("a unit variant name"), branch)));
        } else if let Some(name) = branch.get("const") {
            variants.push((name.as_str().expect("a unit variant name"), branch));
        } else {
            let properties = branch["properties"].as_object().filter(|p| p.len() == 1);
            let name = properties.and_then(|p| p.keys().next()).expect("a one-key object");
            variants.push((name.as_str(), branch));
        }
    }
    variants
}

#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum Probe {
        PlainUnit,
        /// A documented unit variant becomes a `const` branch.
        DocumentedUnit,
        StructVariant {
            value: u8,
        },
        #[serde(rename = "renamed")]
        Original,
    }

    #[test]
    fn action_variants_names_every_shape_with_renames() {
        let schema = schemars::schema_for!(Probe);
        let names: Vec<&str> = super::action_variants(schema.as_value()).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["plain_unit", "renamed", "documented_unit", "struct_variant"]);
    }
}
