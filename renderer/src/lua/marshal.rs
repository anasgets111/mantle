//! Rust-Lua marshalling boundary, enforcing strict, non-coercive rules for `f64`, `i64`/`u64`,
//! and `String`. `mlua` maps shape but does not reject NaN, unsafe integers, or oversized
//! strings.

/// `2^53 - 1`, the largest exact integer in Lua's IEEE-754-double `number`, even when its integer
/// subtype holds it.
const MAX_SAFE_INTEGER: i64 = (1i64 << 53) - 1;
const MIN_SAFE_INTEGER: i64 = -MAX_SAFE_INTEGER;

/// Maximum string size: 64KB.
pub(crate) const MAX_STRING_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarshalError {
    #[error("number must be finite, got NaN or Inf")]
    NotFinite,
    #[error("integer outside [{MIN_SAFE_INTEGER}, {MAX_SAFE_INTEGER}]")]
    IntegerOutOfRange,
    #[error("string is {len} bytes, over the {MAX_STRING_BYTES}-byte cap")]
    StringTooLong { len: usize },
}

pub fn check_number(value: f64) -> Result<f64, MarshalError> {
    if value.is_finite() { Ok(value) } else { Err(MarshalError::NotFinite) }
}

pub fn check_integer(value: i64) -> Result<i64, MarshalError> {
    if (MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
        Ok(value)
    } else {
        Err(MarshalError::IntegerOutOfRange)
    }
}

pub fn check_string(value: &str) -> Result<&str, MarshalError> {
    if value.len() <= MAX_STRING_BYTES { Ok(value) } else { Err(MarshalError::StringTooLong { len: value.len() }) }
}

/// Refuses a key of a config sub-table (`padding`, `anchor`, a `session_process` spec, ...) that
/// is not in `keys`: nothing would read it, so a typo there did nothing, silently. The error names
/// the lowest-sorting unknown key, so two typos always report the same one; the caller adds whose
/// table it is.
pub(crate) fn only_keys(table: &mlua::Table, keys: &[&str]) -> Result<(), String> {
    let mut unknown: Vec<String> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        match pair.map_err(|e| e.to_string())?.0 {
            mlua::Value::String(key) if keys.iter().any(|known| key.as_bytes() == known.as_bytes()) => {}
            mlua::Value::String(key) => unknown.push(format!("`{}`", key.to_string_lossy())),
            other => unknown.push(format!("{other:?}")),
        }
    }
    let Some(first) = unknown.into_iter().min() else { return Ok(()) };
    let keys: Vec<String> = keys.iter().map(|key| format!("`{key}`")).collect();
    Err(format!("unknown key {first}; it takes {}", keys.join(", ")))
}

/// A config list's entries `1..=n` in order, ending at the first hole, which reads as `nil` so
/// the caller's per-entry check names its index. `ipairs` and `sequence_values` stop at a hole and
/// `#` may count past it, so either silently drops or pads entries; walking every key catches
/// `{ a, nil, c }`. Refuses a named key, which no reader of a list sees. ponytail: a trailing `nil`
/// leaves no key behind and stays undetectable.
pub(crate) fn list_entries(table: &mlua::Table) -> Result<Vec<mlua::Value>, String> {
    let mut entries: Vec<(i64, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        match pair.map_err(|e| e.to_string())? {
            (mlua::Value::Integer(index), value) if index >= 1 => entries.push((index, value)),
            (mlua::Value::String(key), _) => {
                return Err(format!("key `{}` is not a list index", key.to_string_lossy()));
            }
            (mlua::Value::Integer(index), _) => return Err(format!("key {index} is not a list index")),
            (mlua::Value::Number(index), _) => return Err(format!("key {index} is not a list index")),
            (other, _) => return Err(format!("{} key is not a list index", a_type(&other))),
        }
    }
    entries.sort_unstable_by_key(|(index, _)| *index);
    let mut list = Vec::with_capacity(entries.len());
    for (position, (index, value)) in (1..).zip(entries) {
        if index != position {
            list.push(mlua::Value::Nil);
            break;
        }
        list.push(value);
    }
    Ok(list)
}

/// `value`'s type with its article: "nil", "an integer", "a string".
pub(crate) fn a_type(value: &mlua::Value) -> String {
    match value.type_name() {
        "nil" => "nil".into(),
        name @ ("integer" | "error") => format!("an {name}"),
        name => format!("a {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_is_not_a_list_index_is_named_as_lua_writes_it() {
        let lua = mlua::Lua::new();
        let refused = |source: &str| list_entries(&lua.load(source).eval().unwrap()).unwrap_err();
        assert_eq!(refused("return { [0] = 1 }"), "key 0 is not a list index");
        assert_eq!(refused("return { [1.5] = 1 }"), "key 1.5 is not a list index");
        assert_eq!(refused("return { [{}] = 1 }"), "a table key is not a list index");
    }

    #[test]
    fn only_keys_names_the_first_unknown_key_and_what_the_table_takes() {
        let lua = mlua::Lua::new();
        let table: mlua::Table = lua.load("return { top = 1, topp = 2, bottum = 3 }").eval().unwrap();
        assert_eq!(
            only_keys(&table, &["top", "bottom"]).unwrap_err(),
            "unknown key `bottum`; it takes `top`, `bottom`"
        );
        assert_eq!(only_keys(&table, &["top", "topp", "bottum"]), Ok(()));
    }

    #[test]
    fn check_number_rejects_nan_and_infinity() {
        assert_eq!(check_number(f64::NAN), Err(MarshalError::NotFinite));
        assert_eq!(check_number(f64::INFINITY), Err(MarshalError::NotFinite));
        assert_eq!(check_number(f64::NEG_INFINITY), Err(MarshalError::NotFinite));
    }

    #[test]
    fn check_number_accepts_finite_values() {
        assert_eq!(check_number(0.75), Ok(0.75));
    }

    #[test]
    fn check_integer_rejects_outside_the_2_pow_53_safe_range() {
        assert_eq!(check_integer(MAX_SAFE_INTEGER + 1), Err(MarshalError::IntegerOutOfRange));
        assert_eq!(check_integer(MIN_SAFE_INTEGER - 1), Err(MarshalError::IntegerOutOfRange));
    }

    #[test]
    fn check_integer_accepts_the_safe_range_boundary() {
        assert_eq!(check_integer(MAX_SAFE_INTEGER), Ok(MAX_SAFE_INTEGER));
        assert_eq!(check_integer(MIN_SAFE_INTEGER), Ok(MIN_SAFE_INTEGER));
    }

    #[test]
    fn check_string_rejects_over_64kb() {
        let oversized = "a".repeat(MAX_STRING_BYTES + 1);
        assert_eq!(check_string(&oversized), Err(MarshalError::StringTooLong { len: MAX_STRING_BYTES + 1 }));
    }

    #[test]
    fn check_string_accepts_exactly_64kb() {
        let exact = "a".repeat(MAX_STRING_BYTES);
        assert!(check_string(&exact).is_ok());
    }
}
