//! Lua VM bootstrap and the loader (build-steps.md Phase 10; `CONTEXT.md`, Loader): evaluates
//! `shell.lua` into the top-level `surface` node(s) and their topology, reused for both a
//! candidate's first evaluation and the authoritative generation's re-evaluation on an in-place
//! reload.
//!
//! ponytail: nothing calls [`Loader::evaluate`] outside this module's own tests yet. No real
//! `shell.lua` file location exists (that's the Watcher's territory, Phase 13), and Phase 11's
//! acceptance test -- the loader reading back a real pushed `StateSnapshot` as a `Signal` -- is
//! what gives this its first production caller. Matches `supervisor/src/socket.rs`'s
//! `GenerationRegistry::send_to`, which shipped in Phase 9 the same way: real, tested, and
//! unwired until a later phase gives it a reason to run.

pub mod marshal;
pub mod nodes;
pub mod signal;

pub use nodes::VirtualNode;

use mlua::{Lua, Table, Value};

/// Owns the Lua VM for one generation. `Loader::evaluate` is stateless across calls beyond that
/// -- each call is a fresh evaluation of its `source` argument, not an incremental re-run.
pub struct Loader {
    lua: Lua,
}

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    /// `shell.lua` failed to parse or raised a runtime error while evaluating.
    #[error("shell.lua failed to evaluate: {0}")]
    Eval(#[from] mlua::Error),
    /// The script evaluated cleanly, but its top-level return wasn't a `surface` node or a
    /// non-empty array of `surface` nodes (§ 6.1).
    #[error("shell.lua's top-level return must be a `surface` node or an array of them: {0}")]
    InvalidTopLevelReturn(String),
}

impl From<nodes::DeserializeError> for LoaderError {
    fn from(err: nodes::DeserializeError) -> Self {
        LoaderError::InvalidTopLevelReturn(err.to_string())
    }
}

/// What one `Loader::evaluate` call produces: the top-level `surface` node(s), each still
/// carrying its own topology fields (`id`/`layer`/`anchor`/`monitor`/`exclusive`, § 6.1) directly
/// in its `properties` bag -- readable without walking into `child` (see `nodes.rs`'s doc
/// comment). This is the cheap-to-diff output Phase 13's Watcher will compare across reloads.
#[derive(Debug)]
pub struct LoadOutput {
    pub surfaces: Vec<VirtualNode>,
}

impl Loader {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        nodes::register_node_constructors(&lua)?;
        signal::register(&lua)?;
        Ok(Loader { lua })
    }

    pub fn evaluate(&self, source: &str) -> Result<LoadOutput, LoaderError> {
        let value: Value = self.lua.load(source).eval()?;
        Ok(LoadOutput { surfaces: collect_surfaces(value)? })
    }
}

fn collect_surfaces(value: Value) -> Result<Vec<VirtualNode>, LoaderError> {
    let table = match value {
        Value::Table(t) => t,
        other => {
            return Err(LoaderError::InvalidTopLevelReturn(format!("expected a table, got {}", other.type_name())));
        }
    };

    if table.contains_key("kind")? {
        let node = nodes::deserialize_lua_table(&table)?;
        require_surface(&node)?;
        return Ok(vec![node]);
    }

    let mut surfaces = Vec::new();
    for entry in table.sequence_values::<Table>() {
        let entry = entry.map_err(LoaderError::from)?;
        let node = nodes::deserialize_lua_table(&entry)?;
        require_surface(&node)?;
        surfaces.push(node);
    }
    if surfaces.is_empty() {
        return Err(LoaderError::InvalidTopLevelReturn("the returned table has no `kind` field and no array elements".to_string()));
    }
    Ok(surfaces)
}

fn require_surface(node: &VirtualNode) -> Result<(), LoaderError> {
    if node.kind == "surface" {
        Ok(())
    } else {
        Err(LoaderError::InvalidTopLevelReturn(format!("top-level node must be `surface`, got `{}`", node.kind)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_rejects_a_lua_syntax_error_as_an_eval_error() {
        let loader = Loader::new().unwrap();
        let err = loader.evaluate("this is not lua").unwrap_err();
        assert!(matches!(err, LoaderError::Eval(_)));
    }

    #[test]
    fn evaluate_rejects_a_top_level_return_that_is_not_a_surface() {
        let loader = Loader::new().unwrap();
        let err = loader.evaluate(r#"return rect { background = "red" }"#).unwrap_err();
        assert!(matches!(err, LoaderError::InvalidTopLevelReturn(_)));
    }

    #[test]
    fn evaluate_accepts_a_single_top_level_surface() {
        let loader = Loader::new().unwrap();
        let output = loader.evaluate(r#"return surface { id = "bar", layer = "Top" }"#).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "surface");
    }

    #[test]
    fn evaluate_accepts_an_array_of_top_level_surfaces() {
        let loader = Loader::new().unwrap();
        let output = loader
            .evaluate(
                r#"
                return {
                    surface { id = "bar" },
                    surface { id = "overlay" },
                }
                "#,
            )
            .unwrap();
        assert_eq!(output.surfaces.len(), 2);
    }
}
