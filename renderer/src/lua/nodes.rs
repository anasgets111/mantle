//! Node constructors and `VirtualNode`, the loader's shallow table-to-Rust conversion.
//!
//! ponytail: shallow by design. `deserialize_lua_table` reads `kind`, refuses one with no
//! `properties::KINDS` row or a key no `properties` field declares, copies other keys unchanged,
//! never recurses into `children`/`child` (reconciliation's job), and does not validate shapes such
//! as `width` being an integer or `"Fill"` (the layout engine is the only typed-property consumer).

use mlua::{Lua, Table, Value};

use super::fuzzy::closest;
use crate::layout::node::PropMap;

pub(crate) mod properties;
#[cfg(test)]
mod stubs;

pub(crate) use properties::range;
use properties::{KINDS, Property, kind_bit, properties};

/// The node kinds, one global constructor each.
fn node_kinds() -> impl Iterator<Item = &'static str> {
    KINDS.iter().copied()
}

/// The table's own `&'static str` for `kind` and its bit, or `None` if it is not a node kind.
fn kind_entry(kind: &str) -> Option<(&'static str, u16)> {
    let bit = kind_bit(kind)?;
    Some((KINDS[bit.trailing_zeros() as usize], bit))
}

/// The row for `property`, whose `&'static str` name is what a [`PropMap`] keys by. A scan of
/// the ~130 rows: ADR-0219 priced the per-property lookup at 10 ns against a 2.65 ms pass.
fn row_in(bit: u16, property: &str) -> Option<&'static Property> {
    properties().find(|row| row.kinds & bit != 0 && row.name == property)
}

/// [`row_in`] for a caller holding only the kind.
pub(crate) fn accepted(kind: &str, property: &str) -> Option<&'static Property> {
    row_in(kind_bit(kind)?, property)
}

/// Accepted properties, sorted for errors.
fn accepted_properties(kind: &str) -> Vec<&'static str> {
    let bit = kind_bit(kind).unwrap_or(0);
    let mut names: Vec<&'static str> = properties().filter(|row| row.kinds & bit != 0).map(|row| row.name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Lua node table tagged with `kind`, carrying other properties unchanged. Not the final scene
/// node.
#[derive(Debug, Clone)]
pub struct VirtualNode {
    pub kind: &'static str,
    pub properties: PropMap,
}

#[derive(Debug, thiserror::Error)]
pub enum DeserializeError {
    #[error(transparent)]
    Lua(#[from] mlua::Error),
    #[error("node table has no `kind` field")]
    MissingKind,
    #[error("node table's `kind` field is not a string")]
    KindNotAString,
    /// A key no parser for this `kind` reads; rejected instead of copied through
    /// (`properties::properties`).
    #[error("`{kind}` has no property `{property}`; {hint}")]
    UnknownProperty { kind: String, property: String, hint: String },
    /// A kind with no `properties::KINDS` row, and so no vocabulary to key a map by (ADR-0219).
    #[error("`{0}` is not a node kind")]
    UnsupportedKind(String),
}

/// Registers each [`node_kinds`] entry as a constructor that tags its props table with `kind`.
pub fn register_node_constructors(lua: &Lua) -> mlua::Result<()> {
    for kind in node_kinds() {
        lua.globals().set(
            kind,
            lua.create_function(move |_, props: Table| {
                props.set("kind", kind)?;
                Ok(props)
            })?,
        )?;
    }
    Ok(())
}

/// Converts one Lua node table into a [`VirtualNode`]: pulls out `kind`, copies every other
/// key-value pair into `properties` as-is. Does not recurse into `children`/`child`.
pub fn deserialize_lua_table(table: &Table) -> Result<VirtualNode, DeserializeError> {
    let (kind, bit) = match table.get::<Value>("kind")? {
        // Borrowed for the lookup: the static the row hands back is what the node keeps, so a
        // supported kind allocates nothing. Only a refusal copies the spelling (ADR-0219).
        Value::String(s) => match s.to_str().ok().and_then(|text| kind_entry(&text)) {
            Some(entry) => entry,
            None => return Err(DeserializeError::UnsupportedKind(s.to_string_lossy())),
        },
        Value::Nil => return Err(DeserializeError::MissingKind),
        _ => return Err(DeserializeError::KindNotAString),
    };

    let mut properties = PropMap::default();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let name = match &key {
            Value::String(s) => match s.to_str() {
                Ok(text) if &*text == "kind" => continue,
                Ok(text) => row_in(bit, &text).map(|row| row.name),
                Err(_) => None,
            },
            _ => None,
        };
        let Some(name) = name else {
            // Lossy: `Value::to_string` refuses the key this arm exists to name.
            let property = match &key {
                Value::String(s) => s.to_string_lossy(),
                other => other.to_string()?,
            };
            let accepted = accepted_properties(kind);
            let hint = match closest(&property, accepted.iter().copied()) {
                Some(near) => format!("did you mean `{near}`?"),
                None => format!("it accepts {}", accepted.join(", ")),
            };
            return Err(DeserializeError::UnknownProperty { kind: kind.to_string(), property, hint });
        };
        properties.insert(name, value);
    }

    Ok(VirtualNode { kind, properties })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_constructors() -> Lua {
        let lua = Lua::new();
        register_node_constructors(&lua).unwrap();
        lua
    }

    #[test]
    fn a_node_constructor_tags_the_props_table_with_its_kind() {
        let lua = lua_with_constructors();
        let table: Table =
            lua.load(r##"return rect { background = "#11111B", width = "Fill", height = 32 }"##).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "rect");
        assert_eq!(table.get::<String>("background").unwrap(), "#11111B");
    }

    /// Per-kind check: `layer` is root-role topology, not a `rect` property.
    #[test]
    fn a_property_of_another_kind_is_refused_too() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return rect { layer = "Top" }"#).eval().unwrap();
        assert!(deserialize_lua_table(&table).unwrap_err().to_string().contains("layer"));
    }

    /// ADR-0260: only a box has a box to cast, so text's shadow is always its content's.
    #[test]
    fn shadow_mode_is_a_box_property() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return text { shadow_mode = "Box" }"#).eval().unwrap();
        let err = deserialize_lua_table(&table).unwrap_err().to_string();
        assert!(err.contains("`text` has no property `shadow_mode`"), "{err}");
        let table: mlua::Table = lua.load(r#"return rect { shadow_mode = "Content" }"#).eval().unwrap();
        assert!(deserialize_lua_table(&table).is_ok());
    }

    /// A surface root takes the base and box properties, like `rect` paint.
    #[test]
    fn a_surface_root_takes_the_base_properties_and_the_box_ones() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua
            .load(r##"return panel { id = "bar", layer = "Top", padding = { top = 4 }, radius = 8, opacity = 0.5 }"##)
            .eval()
            .unwrap();
        assert!(deserialize_lua_table(&table).is_ok());
    }

    #[test]
    fn deserialize_lua_table_pulls_kind_out_and_keeps_every_other_field() {
        let lua = lua_with_constructors();
        let table: Table = lua.load(r#"return text { content = "hi", font_size = 14 }"#).eval().unwrap();

        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(node.kind, "text");
        assert!(!node.properties.contains_key("kind"), "kind must be pulled out, not duplicated into properties");
        assert_eq!(node.properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "hi");
        assert_eq!(node.properties.get("font_size").unwrap().as_integer().unwrap(), 14);
    }

    #[test]
    fn deserialize_lua_table_rejects_a_table_with_no_kind_field() {
        let lua = Lua::new();
        let table: Table = lua.create_table().unwrap();
        table.set("width", 32).unwrap();

        let err = deserialize_lua_table(&table).unwrap_err();
        assert!(matches!(err, DeserializeError::MissingKind));
    }

    /// Refused when the table is read, not when the tree is applied (ADR-0219). That leaves
    /// `layout::scene::ensure_supported_kind` unreachable from a deserialized tree; it stays because
    /// `children_of`'s `unreachable!` is what it makes sound.
    #[test]
    fn an_unsupported_top_level_kind_is_still_rejected() {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua.load(r#"{ kind = "dialog", id = "s", child = rect {} }"#).eval().unwrap();

        let err = deserialize_lua_table(&table).unwrap_err();

        assert!(matches!(&err, DeserializeError::UnsupportedKind(k) if k == "dialog"), "{err}");
    }

    #[test]
    fn deserialize_lua_table_leaves_a_nested_child_table_unconverted() {
        let lua = lua_with_constructors();
        let table: Table = lua
            .load(r##"return panel { id = "bar", layer = "Top", child = rect { background = "#000000" } }"##)
            .eval()
            .unwrap();

        let node = deserialize_lua_table(&table).unwrap();
        let child = node.properties.get("child").unwrap();
        assert!(matches!(child, Value::Table(_)), "child stays a raw Lua table, not a converted VirtualNode");
    }

    /// `Value::to_string` refuses a key that is not UTF-8, so the refusal has to name it lossily or
    /// name nothing at all.
    #[test]
    fn a_property_key_that_is_not_utf8_is_refused_by_name() {
        let lua = lua_with_constructors();
        let table: Table = lua.load("return rect {}").eval().unwrap();
        table.set(lua.create_string(b"widt\xffh").unwrap(), 1).unwrap();

        let err = deserialize_lua_table(&table).unwrap_err();

        let DeserializeError::UnknownProperty { property, hint, .. } = err else {
            panic!("a key that is not UTF-8 is an unknown property, got: {err}")
        };
        assert!(property.contains('\u{fffd}'), "the key must be named lossily, got `{property}`");
        assert_eq!(hint, "did you mean `width`?");
    }

    /// A near miss names the one property; the full list only helps when nothing is close.
    #[test]
    fn an_unknown_property_names_the_close_match_or_else_every_accepted_name() {
        let lua = lua_with_constructors();
        let unknown = |source: &str| deserialize_lua_table(&lua.load(source).eval().unwrap()).unwrap_err().to_string();

        assert_eq!(
            unknown(r#"return text { contnet = "x" }"#),
            "`text` has no property `contnet`; did you mean `content`?"
        );
        let far = unknown(r#"return text { zzz = 1 }"#);
        assert!(far.starts_with("`text` has no property `zzz`; it accepts "), "{far}");
        assert!(far.contains("content"), "{far}");
    }

    #[test]
    fn every_node_kind_constructs_and_tags_correctly() {
        let lua = lua_with_constructors();
        for kind in node_kinds() {
            let table: Table = lua.load(format!("return {kind} {{}}")).eval().unwrap();
            assert_eq!(table.get::<String>("kind").unwrap(), kind);
        }
    }
}

/// Checks `lua-meta/nodes.lua` and `lua-meta/surfaces.lua`, which `stubs.rs` generates from the
/// property table, against the engine: the generator's class split against the accepted names, and
/// every declared type against a real apply. The capability check stays here because this crate owns
/// `shared::Capability::ALL`'s Lua spelling.
#[cfg(test)]
mod meta_stub_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn meta(file: &str) -> String {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../lua-meta").join(file);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is missing or unreadable: {err}", path.display()))
    }

    /// Every kind's inherited `---@field` set matches [`super::accepted_properties`]: the generator's
    /// `NodeBase`/`BoxBase`/own split loses and invents nothing. Editor stubs offering a refused name
    /// are worse than omission.
    #[test]
    fn the_stubs_declare_the_same_properties_the_engine_accepts() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let classes = parse_classes(&source);
        for kind in super::node_kinds() {
            let class = format!("{}Props", capitalize(kind));
            let declared = fields_of(&classes, &class);
            let expected: BTreeSet<String> = super::accepted_properties(kind).into_iter().map(str::to_string).collect();
            assert_eq!(declared, expected, "lua-meta's {class} is out of step with the property table for `{kind}`");

            // `animate` takes the kind's own names but `z` and `animate`, plus `exit`.
            let alias = format!("---@alias {}Animations {{ ", capitalize(kind));
            let line = source.lines().find_map(|line| line.strip_prefix(alias.as_str())).expect("an Animations alias");
            let keys: BTreeSet<String> =
                line.split(", ").filter_map(|entry| entry.split_once("?: ")).map(|(key, _)| key.to_string()).collect();
            let mut expected: BTreeSet<String> =
                expected.into_iter().filter(|name| !matches!(name.as_str(), "z" | "animate")).collect();
            expected.insert("exit".to_string());
            assert_eq!(keys, expected, "lua-meta's `animate` keys for `{kind}`");
        }
    }

    /// Parser reads and accepted names match both ways: a read no kind accepts is refused first, an
    /// accepted name nothing reads is silently ignored. Source grep because names live in literals.
    #[test]
    fn every_property_a_parser_reads_is_accepted_by_some_kind() {
        let accepted: BTreeSet<String> =
            super::node_kinds().flat_map(super::accepted_properties).map(str::to_string).collect();
        let mut read = BTreeSet::new();
        for source in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&source).expect("a source file this build compiled is readable");
            read.extend(property_literals(&without_test_modules(&text)));
        }
        let unreachable: Vec<&String> = read.difference(&accepted).collect();
        assert!(
            unreachable.is_empty(),
            "these parsers read a property no kind accepts, so `deserialize_lua_table` refuses it first: {unreachable:?}"
        );
        let unread: Vec<&String> = accepted.difference(&read).collect();
        assert!(unread.is_empty(), "these properties are accepted but no parser reads them: {unread:?}");
    }

    /// Drops top-level `#[cfg(test)] mod … { … }` blocks, whose fixtures may name anything. Other
    /// `#[cfg(test)]` items often sit above production code, so they stay.
    fn without_test_modules(text: &str) -> String {
        let mut out = String::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line == "#[cfg(test)]" && lines.peek().is_some_and(|next| next.contains("mod ") && next.ends_with('{')) {
                // rustfmt closes a top-level block with `}` in column 0.
                lines.by_ref().find(|line| *line == "}");
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// `rect` -> `Rect`.
    fn capitalize(name: &str) -> String {
        let mut chars = name.chars();
        chars.next().map(|first| first.to_ascii_uppercase().to_string() + chars.as_str()).unwrap_or_default()
    }

    /// Each `---@class Name: Parent, Parent` and its own `---@field` names, less the `[string]` guard.
    fn parse_classes(source: &str) -> Vec<(String, Vec<String>, BTreeSet<String>)> {
        let mut classes: Vec<(String, Vec<String>, BTreeSet<String>)> = Vec::new();
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("---@class ") {
                let (name, parents) = match rest.split_once(':') {
                    Some((name, parents)) => (name.trim(), parents.split(',').map(|p| p.trim().to_string()).collect()),
                    None => (rest.trim(), Vec::new()),
                };
                classes.push((name.to_string(), parents, BTreeSet::new()));
            } else if let Some(rest) = line.strip_prefix("---@field ")
                && let Some((name, _)) = rest.split_once(char::is_whitespace)
                && !name.starts_with('[')
                && let Some(current) = classes.last_mut()
            {
                current.2.insert(name.trim_end_matches('?').to_string());
            }
        }
        classes
    }

    /// One class's fields plus every parent's, which is what completion offers on it.
    fn fields_of(classes: &[(String, Vec<String>, BTreeSet<String>)], name: &str) -> BTreeSet<String> {
        let Some((_, parents, own)) = classes.iter().find(|(class, ..)| class == name) else {
            panic!("lua-meta declares no `{name}` class");
        };
        let mut fields = own.clone();
        for parent in parents {
            fields.extend(fields_of(classes, parent));
        }
        fields
    }

    fn rust_sources(root: PathBuf) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Property literals in `properties.get("x")`, as the named parser argument in
    /// `fields::common::align_v.read(properties)`, or through `keyboard/mod.rs`'s `function("on_cancel")`.
    fn property_literals(text: &str) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = text
            .split("function(\"")
            .skip(1)
            .filter_map(|rest| rest.split_once("\")"))
            .map(|(name, _)| name.to_string())
            .filter(|name| name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .collect();
        // A typed field's read, `image::source.read(properties)` or `.read_with_id(`.
        for (index, _) in text.match_indices("::") {
            let rest = text[index + 2..].trim_start_matches("r#");
            let name: String = rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == '_').collect();
            if !name.is_empty() && rest[name.len()..].starts_with(".read") {
                names.insert(name);
            }
        }
        for (index, _) in text.match_indices("properties") {
            let rest = &text[index + "properties".len()..];
            let head: String = rest.chars().take(40).collect();
            let opener = match head.trim_start().chars().next() {
                // `properties.get("x")` and its `remove`/`contains_key` siblings.
                Some('.') => head.find('"'),
                // `(properties, "x")`, the trailing argument of a name-taking parser.
                Some(',') => head.find('"'),
                _ => None,
            };
            let Some(quote) = opener else { continue };
            let Some(end) = head[quote + 1..].find('"') else { continue };
            let name = &head[quote + 1..quote + 1 + end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                names.insert(name.to_string());
            }
        }
        names
    }

    /// Feeds every `lua-meta` type through the evaluation-time surface specs, a real `Scene::apply`,
    /// and the resolved specs. Names are checked elsewhere; this catches type claims that the engine
    /// rejects, the ADR-0081 gap that once affected 21 properties, plus a closed literal set the
    /// engine does not close, an `integer` it lets take a fraction, and a required field it does
    /// not require. One-way by design: `just types` catches engine-accepted fields missing from the
    /// stub. Missing `sample` rows fail.
    /// ponytail: a row's `ty` is written, not derived from its parser, so this is what holds them
    /// together. Upgrade: per-kind props structs, which rewrites parsing and trades
    /// property-specific errors for serde's.
    #[test]
    fn every_type_the_stubs_declare_is_accepted_by_the_engine() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let classes = parse_typed_classes(&source);
        let aliases: BTreeMap<&str, &str> = source
            .lines()
            .filter_map(|line| line.strip_prefix("---@alias ")?.split_once(' '))
            .map(|(name, rest)| (name, declared_type(rest)))
            .collect();

        let mut report = Report::default();
        for kind in super::node_kinds() {
            let class = format!("{}Props", capitalize(kind));
            let fields = typed_fields(&classes, &class);
            let mut required: Vec<(String, String)> = Vec::new();
            for field in fields.iter().filter(|field| field.required) {
                match split_top(&field.ty, '|').into_iter().find_map(|member| sample(&field.name, member)) {
                    Some(literal) => required.push((field.name.clone(), literal)),
                    None => report.unsampled.push(format!("  {kind}.{}: `{}`", field.name, field.ty)),
                }
            }
            for (name, _) in &required {
                if apply_one(kind, &required, name, None).is_ok() {
                    report.failures.push(format!("  {kind}.{name} is declared required, engine accepts it absent"));
                }
            }
            for Field { name, ty, .. } in &fields {
                probe(&aliases, kind, &required, name, ty, &|literal| literal.to_string(), &mut report);
            }
        }
        // Alias internals no field declares directly; `@` is the probed slot.
        let easing = typed_fields(&classes, "Transition")
            .into_iter()
            .find(|f| f.name == "easing")
            .expect("Transition.easing")
            .ty;
        let steps = shape_field(&aliases, &easing, "steps").expect("Easing declares `{ steps }`");
        let loops = shape_field(&aliases, "Animation", "loops").expect("Animation declares `loops`");
        let span = typed_fields(&classes, "TextRun").into_iter().find(|f| f.name == "kind").expect("TextRun.kind").ty;
        for (kind, field, ty, around) in [
            ("image", "transition", easing.as_str(), "{ duration = 400, easing = @ }"),
            ("image", "transition", steps, "{ duration = 400, easing = { steps = @ } }"),
            ("rect", "animate", loops, "{ opacity = { duration = 200, keyframes = { 0, 1 }, loops = @ } }"),
            ("text", "content", span.as_str(), "{ { text = \"a\", kind = @ } }"),
        ] {
            probe(&aliases, kind, &[], field, ty, &|literal| around.replace('@', literal), &mut report);
        }
        let Report { failures, unsampled } = report;
        assert!(
            failures.is_empty(),
            "{} declared type(s) the engine refuses:\n{}",
            failures.len(),
            failures.join("\n")
        );
        // A skip is a hole, so fail; add its spelling to `sample`. No "cannot be probed" arm.
        assert!(
            unsampled.is_empty(),
            "{} declared type(s) have no sample, so nothing checked them. Add a row to `sample`:\n{}",
            unsampled.len(),
            unsampled.join("\n")
        );
    }

    /// Literal sets refused without a keyword list, so checked one way only: `cursor`
    /// parses `cursor_icon`'s names, which it cannot enumerate; the rest mix one keyword into a
    /// number (`"Fill"`, `"Ignore"`, `animate`'s `loops = "Infinite"`).
    const ONE_WAY: [&str; 5] = ["animate", "cursor", "exclusive", "height", "width"];

    #[derive(Default)]
    struct Report {
        failures: Vec<String>,
        unsampled: Vec<String>,
    }

    /// Applies every member of `ty` as `field = around(literal)`. Union aliases and `(…)[]` expand,
    /// so each declared literal is probed. Returns the first literal, for `Bound` to wrap.
    fn probe(
        aliases: &BTreeMap<&str, &str>,
        kind: &str,
        required: &[(String, String)],
        field: &str,
        ty: &str,
        around: &dyn Fn(&str) -> String,
        report: &mut Report,
    ) -> Option<String> {
        let mut members = expand(aliases, ty);
        members.sort_by_key(|member| *member == "Bound");
        let mut first: Option<String> = None;
        for member in &members {
            if let Some(inner) = member.strip_prefix('(').and_then(|m| m.strip_suffix(")[]")) {
                let nested = probe(aliases, kind, required, field, inner, &|l| around(&format!("{{ {l} }}")), report);
                first = first.or(nested);
                continue;
            }
            let literal = match *member {
                // The engine resolves a handle before sibling rules apply, so wrap a sibling's
                // literal: `image.source` needs a string signal, `list.source` an array. The bare
                // `hover`/`geometry`/`scroll` fall back to their own rows.
                "Bound" => first.as_ref().map(|l| format!("state(\"probe\", {l})")).or_else(|| sample(field, member)),
                _ => sample(field, member).map(|l| around(&l)),
            };
            let Some(literal) = literal else {
                report.unsampled.push(format!("  {kind}.{field}: `{member}`"));
                continue;
            };
            if let Err(err) = apply_one(kind, required, field, Some(&literal)) {
                report.failures.push(format!("  {kind}.{field} declares `{member}`, engine says: {err}"));
            }
            if *member == "integer" && apply_one(kind, required, field, Some(&around("8.5"))).is_ok() {
                report
                    .failures
                    .push(format!("  {kind}.{field} declares `integer`, engine accepts `{}`", around("8.5")));
            }
            first.get_or_insert(literal);
        }
        // The refusal's keyword list must equal the declared literals, making the check two-way.
        let bogus = around("\"mantle_bogus\"");
        let declared: BTreeSet<&str> = members.iter().filter_map(|m| m.strip_prefix('"')?.strip_suffix('"')).collect();
        if !declared.is_empty() && !members.contains(&"string") {
            let listed = apply_one(kind, required, field, Some(&bogus)).map(|()| None).unwrap_or_else(|err| {
                let list = err.split_once("expected one of ")?.1.split(", got").next()?;
                Some(list.split(", ").map(|name| name.trim_matches('`').to_string()).collect::<BTreeSet<_>>())
            });
            match listed {
                Some(listed) if listed.iter().map(String::as_str).eq(declared.iter().copied()) => {}
                Some(listed) => {
                    report.failures.push(format!("  {kind}.{field} declares {declared:?}, engine lists {listed:?}"))
                }
                None if ONE_WAY.contains(&field) => {}
                None => report
                    .failures
                    .push(format!("  {kind}.{field} declares a closed literal set, engine lists none for `{bogus}`")),
            }
        }
        first
    }

    /// Lua literal for a declared type; `None` skips rather than guesses. Field name matters when
    /// spelling shares a type but not a domain: `opacity` is `[0, 1]`, `size` is pixels, and
    /// `border_color`'s `Edges` holds colors while `margin`'s holds lengths.
    fn sample(field: &str, ty: &str) -> Option<String> {
        match (field, ty) {
            ("opacity", _) => return Some("0.5".to_string()),
            ("animate", ty) if ty.ends_with("Animations") => return Some("{ opacity = 200.5 }".to_string()),
            ("scale", "Axes") | ("translate" | "shadow_offset", _) => return Some("{ x = 1, y = 2 }".to_string()),
            ("scale", _) => return Some("1.5".to_string()),
            ("rotate", _) => return Some("7.5".to_string()),
            ("origin", _) => return Some("{ x = 0.5, y = 0.5 }".to_string()),
            ("transition", "Transition") => return Some("{ duration = 400.5, easing = \"InOutCubic\" }".to_string()),
            ("border_color", "BorderColors") => return Some("{ top = \"#112233\" }".to_string()),
            ("background", "Gradient") | ("mask", "Mask") => {
                return Some(
                    "{ gradient = \"Conic\", angle = 45, stops = { { 0, \"#112233\" }, { 1, \"#11223300\" } } }".into(),
                );
            }
            // Inline table shapes have no alias.
            ("anchor", shape) if shape.starts_with('{') => return Some("{ top = true, left = true }".to_string()),
            ("min_size" | "max_size", _) => return Some("{ width = 8.5, height = 8.5 }".to_string()),
            ("offset", _) => return Some("{ x = 1.5, y = 1 }".to_string()),
            // `shader.source` refuses a relative path; `image.source` takes either.
            ("source", "string") => return Some("\"/x\"".to_string()),
            ("params", _) => return Some("{ a = 0.5, b = { 1, 2, 3, 4 } }".to_string()),
            ("secure_submit", _) => {
                return Some("{ capability = \"lock\", action = \"authenticate\" }".to_string());
            }
            // Parsers check callbacks only as functions, except `list`'s two layout calls, which
            // use the return value. These three bare `Bound` fields take the handle itself.
            ("hover", _) => return Some("hover(\"probe\")".to_string()),
            ("geometry", _) => return Some("geometry(\"probe\")".to_string()),
            ("scroll", _) => return Some("scroll(\"probe\")".to_string()),
            // Literal-array `list.source` is fixed for the pass; real lists therefore use the
            // adjacent signal (ADR-0113 decision 3).
            ("source", "any[]") => return Some("{ 1, 2 }".to_string()),
            ("itemfn", _) => return Some("function(item) return rect {} end".to_string()),
            // `panel`/`lock` per-output builder (ADR-0121), called with `"PROBE"` here.
            ("child", shape) if shape.contains("fun(") => {
                return Some("function(output) return rect {} end".to_string());
            }
            ("key", _) => return Some("function(item) return tostring(item) end".to_string()),
            (_, shape) if shape.starts_with("fun(") || shape.starts_with("fun()") => {
                return Some("function() end".to_string());
            }
            _ => {}
        }
        Some(
            match ty {
                // A `lock`'s refused `NodeBase` fields.
                "nil" => "nil",
                "integer" => "8",
                "number" => "8.5",
                "string" => "\"x\"",
                "boolean" => "true",
                "Color" => "\"#112233\"",
                "Percent" => "\"50%\"",
                "Edges" => "{ top = 1.5 }",
                "[number, number, number, number]" => "{ 0.25, 0.1, 0.25, 1 }",
                "{ steps: integer, [string]: \"no such property\" }" => "{ steps = 4 }",
                "Node" => "rect {}",
                "Node[]" => "{ rect {} }",
                "TextRun[]" => {
                    "{ { text = \"x\", bold = true, underline = true, color = \"#112233\", href = \"https://x/\" } }"
                }
                // No bare `Bound` row: callers wrap sibling samples, while the three bare fields
                // are handled above. A row would shadow both and feed every property the wrong
                // value.
                "Rect" => "{ x = 0, y = 0, width = 1, height = 1 }",
                literal if literal.starts_with('"') => literal,
                _ => return None,
            }
            .to_string(),
        )
    }

    /// Field companions, distinct from kind requirements. `on_hover` needs a same-node `hover`
    /// slot (ADR-0095), or a probe tests pairing rather than its declared type.
    fn companions(field: &str) -> &'static [(&'static str, &'static str)] {
        match field {
            "on_hover" => &[("hover", "hover(\"probe\")")],
            _ => &[],
        }
    }

    /// Applies `kind { required..., field = literal }` (`None` omits `field`) the way a reload does:
    /// `surface_specs`, `Scene::apply`, then the resolved spec `wayland::surface` builds. Surface
    /// roles are roots; other kinds hang under a minimal `panel`.
    fn apply_one(kind: &str, required: &[(String, String)], field: &str, literal: Option<&str>) -> Result<(), String> {
        let mut props: Vec<String> = required
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain(companions(field).iter().copied())
            .filter(|(name, _)| *name != field)
            .map(|(name, value)| format!("{name} = {value}"))
            .collect();
        props.extend(literal.map(|literal| format!("{field} = {literal}")));
        let node = format!("{kind} {{ {} }}", props.join(", "));
        let surface = if SURFACE_KINDS.contains(&kind) {
            node
        } else {
            format!("panel {{ id = \"probe\", layer = \"Top\", child = {node} }}")
        };

        let lua = mlua::Lua::new();
        super::register_node_constructors(&lua).map_err(|e| e.to_string())?;
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).map_err(|e| e.to_string())?;
        let table: mlua::Table = lua.load(format!("return {surface}")).eval().map_err(|e| e.to_string())?;
        let virtual_node = super::deserialize_lua_table(&table).map_err(|e| format!("{e:?}"))?;
        // Topology (`layer`, popup `anchor`, ...) is validated here and never read by `Scene::apply`.
        crate::lua::surfaces::surface_specs(&crate::lua::LoadOutput { surfaces: vec![virtual_node.clone()] })
            .map_err(|e| e.to_string())?;
        let mut scene = crate::layout::scene::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        // Keep `scene::tests::apply_at` `pub(super)`; widening a test helper is what the justfile's
        // `docs` baseline discourages. One instance and output suffice for this probe.
        let declared =
            crate::layout::node::fields::surface::id.read(&virtual_node.properties).map_err(|e| format!("{e:?}"))?;
        let instance_id = format!("{declared}@PROBE");
        let instances = [crate::layout::instance::SurfaceInstance {
            instance_id: instance_id.clone(),
            declared_id: declared,
            output: "PROBE".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
            measured_axes: (false, false),
        }];
        scene.apply(std::slice::from_ref(&virtual_node), &instances, &shaping, &lua).map_err(|e| format!("{e:?}"))?;
        // A signal defers the evaluation-time spec, so the resolved one is where a bound value is checked.
        let resolved = &scene.surface(&instance_id).ok_or("the probe surface was not retained")?.properties;
        match virtual_node.kind {
            "panel" => crate::layout::node::panel_spec(resolved).map(drop),
            "window" => crate::layout::node::window_spec(resolved).map(drop),
            "popup" => crate::layout::node::popup_spec(resolved).map(drop),
            _ => crate::layout::node::lock_spec(resolved).map(drop),
        }
        .map_err(|e| format!("{e:?}"))
    }

    const SURFACE_KINDS: [&str; 4] = ["panel", "window", "popup", "lock"];

    #[derive(Clone)]
    struct Field {
        name: String,
        ty: String,
        /// No `?` on the name.
        required: bool,
    }

    /// One `---@class`: name, parents, and own fields in order.
    type TypedClass = (String, Vec<String>, Vec<Field>);

    /// [`parse_classes`] with each field's declared type.
    fn parse_typed_classes(source: &str) -> Vec<TypedClass> {
        let mut classes: Vec<TypedClass> = Vec::new();
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("---@class ") {
                let (name, parents) = match rest.split_once(':') {
                    Some((name, parents)) => (name.trim(), parents.split(',').map(|p| p.trim().to_string()).collect()),
                    None => (rest.trim(), Vec::new()),
                };
                classes.push((name.to_string(), parents, Vec::new()));
            } else if let Some(rest) = line.strip_prefix("---@field ")
                && let Some(current) = classes.last_mut()
                && let Some((name, rest)) = rest.split_once(' ')
                && !name.starts_with('[')
            {
                current.2.push(Field {
                    name: name.trim_end_matches('?').to_string(),
                    ty: declared_type(rest).to_string(),
                    required: !name.ends_with('?'),
                });
            }
        }
        classes
    }

    /// The type at the start of `rest`: up to the first space outside brackets, reading a
    /// `fun(..): R` return past its colon.
    fn declared_type(rest: &str) -> &str {
        let (mut depth, mut prev) = (0, ' ');
        for (index, c) in rest.char_indices() {
            match c {
                '(' | '{' | '[' | '<' => depth += 1,
                ')' | '}' | ']' | '>' => depth -= 1,
                ' ' if depth == 0 && prev != ':' => return &rest[..index],
                _ => {}
            }
            prev = c;
        }
        rest
    }

    /// `ty` split on top-level `sep`; `("SlideX"|...)[]` and inline shapes stay whole.
    fn split_top(ty: &str, sep: char) -> Vec<&str> {
        let (mut depth, mut start, mut out) = (0, 0, Vec::new());
        for (index, c) in ty.char_indices() {
            match c {
                '(' | '{' | '[' | '<' => depth += 1,
                ')' | '}' | ']' | '>' => depth -= 1,
                c if c == sep && depth == 0 => {
                    out.push(&ty[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
        out.push(&ty[start..]);
        out
    }

    /// `ty`'s union members, with union aliases replaced by theirs.
    fn expand<'a>(aliases: &BTreeMap<&str, &'a str>, ty: &'a str) -> Vec<&'a str> {
        split_top(ty, '|')
            .into_iter()
            .flat_map(|member| match aliases.get(member) {
                Some(def) if split_top(def, '|').len() > 1 => expand(aliases, def),
                _ => vec![member],
            })
            .collect()
    }

    /// `key`'s type inside the `{ key: T, ... }` members `ty` expands to.
    fn shape_field<'a>(aliases: &BTreeMap<&str, &'a str>, ty: &'a str, key: &str) -> Option<&'a str> {
        expand(aliases, ty)
            .into_iter()
            .filter_map(|member| member.strip_prefix("{ ")?.strip_suffix(" }"))
            .flat_map(|body| split_top(body, ','))
            .find_map(|entry| {
                let (name, ty) = entry.trim().split_once(": ")?;
                (name.trim_end_matches('?') == key).then_some(ty)
            })
    }

    /// One class's fields plus every parent's.
    fn typed_fields(classes: &[TypedClass], name: &str) -> Vec<Field> {
        let Some((_, parents, own)) = classes.iter().find(|(class, ..)| class == name) else {
            panic!("lua-meta declares no `{name}` class");
        };
        let mut out = Vec::new();
        for parent in parents {
            out.extend(typed_fields(classes, parent));
        }
        // A redeclared field replaces the parent's, as the language server reads it.
        out.retain(|field: &Field| !own.iter().any(|mine| mine.name == field.name));
        out.extend(own.iter().cloned());
        out
    }

    /// Every `shared::Capability::ALL` name as an `Mantle` field.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("mantle.lua");
        // The `---@field` block under `---@class Mantle`, not `MantleVersion`.
        let class = source.split("---@class Mantle\n").nth(1).expect("mantle.lua declares a Mantle class");
        // Off-roster members lack a `StateSnapshot` and roster entry (`lua::namespace::build`).
        // `idle` left this list under ADR-0141: it is now a roster capability wrapped for three
        // callbacks that cannot cross the wire.
        let off_roster = ["screens", "rescue", "version", "config_dir"];
        let declared: BTreeSet<&str> = class
            .lines()
            .take_while(|line| line.starts_with("---@field"))
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| !off_roster.contains(name))
            .collect();
        let expected: BTreeSet<&str> = shared::Capability::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(declared, expected, "lua-meta/mantle.lua is out of step with shared::Capability::ALL");
    }
}
