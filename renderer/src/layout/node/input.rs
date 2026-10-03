//! A field's Rust type supplies its parser, Lua stub spelling and error's expected type,
//! so the three cannot disagree. Nested signals are refused.

use std::path::PathBuf;
use std::time::Duration;

use mlua::Value;

use super::prop::Keyword;
use super::{LayoutError, Rgba, checked_string, invalid, parse_hex_color, preview_for_error, value_as_f32};
use crate::lua::luacats::LuaType;

/// A Lua value read as `Self`, inside a table a parser reads.
pub(crate) trait Input: LuaType + Sized {
    /// `None` when `value` is not this type; [`read`] then names what `key` expected. An `Err` is
    /// a value of this type that is still wrong, like a malformed colour.
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError>;
}

/// `value`, the entry at `key` inside `property`, as a `T`; a nested signal is refused.
pub(crate) fn read<T: Input>(property: &str, key: &str, value: Value) -> Result<T, LayoutError> {
    let value = plain(property, key, value)?;
    T::from_value(property, key, &value)?
        .ok_or_else(|| invalid(property, format!("`{key}` must be {}, got {}", T::lua(), preview_for_error(&value))))
}

/// `table[key]` as a `T`.
pub(crate) fn field<T: Input>(property: &str, table: &mlua::Table, key: &str) -> Result<T, LayoutError> {
    read(property, key, table.get(key).map_err(|e| invalid(property, e.to_string()))?)
}

/// `value` unless it is a signal, which only a property's top level may hold.
fn plain(property: &str, key: impl std::fmt::Display, value: Value) -> Result<Value, LayoutError> {
    match value {
        Value::UserData(_) => Err(LayoutError::UnsupportedSignalProperty(format!("{property}.{key}"))),
        other => Ok(other),
    }
}

impl Input for f32 {
    fn from_value(property: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        value_as_f32(property, value)
    }
}

impl Input for u32 {
    fn from_value(property: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        Ok(value_as_f32(property, value)?
            .filter(|n| *n >= 0.0 && *n <= u32::MAX as f32 && n.fract() == 0.0)
            .map(|n| n as u32))
    }
}

/// Distinguish nil from non-numbers so a typo cannot take a default; `value_as_f32` returns `None` for both.
/// Whole milliseconds avoid `from_secs_f32` turning 200 ms into 200.000003 ms.
pub(super) fn parse_millis(
    field: &str,
    what: &str,
    value: &Value,
    least: u64,
) -> Result<Option<Duration>, LayoutError> {
    if value.is_nil() {
        return Ok(None);
    }
    let millis = value_as_f32(field, value)?
        .ok_or_else(|| invalid(field, format!("expected a {what} in ms, got {}", preview_for_error(value))))?;
    checked_millis(field, what, millis, least).map(Some)
}

fn checked_millis(field: &str, what: &str, millis: f32, least: u64) -> Result<Duration, LayoutError> {
    // Check the rounded minimum so 0.1 ms cannot create a zero-length tween.
    let rounded = millis.round() as u64;
    if !(0.0..=60_000.0).contains(&millis) || rounded < least {
        return Err(invalid(field, format!("{what} must be within [{least}, 60000] ms, got {millis}")));
    }
    Ok(Duration::from_millis(rounded))
}

pub(super) fn required_duration(field: &str, duration: Option<Duration>) -> Result<Duration, LayoutError> {
    let duration = duration.ok_or_else(|| invalid(field, "expected a duration in ms"))?;
    checked_millis(field, "duration", duration.as_millis() as f32, 1)
}

impl Input for Duration {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        parse_millis(property, key, value, 0)
    }
}

impl Input for Value {
    fn from_value(_: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        Ok(Some(value.clone()))
    }
}

impl Input for bool {
    fn from_value(_: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        Ok(value.as_boolean())
    }
}

impl Input for String {
    fn from_value(property: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Value::String(s) = value else { return Ok(None) };
        checked_string(property, s).map(Some)
    }
}

impl Input for PathBuf {
    fn from_value(property: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Value::String(path) = value else { return Ok(None) };
        Ok(Some(PathBuf::from(path.to_str().map_err(|e| invalid(property, e.to_string()))?.as_ref())))
    }
}

impl Input for Rgba {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let name_edge = |error| match error {
            LayoutError::InvalidProperty { detail, .. } => invalid(property, format!("`{key}`: {detail}")),
            other => other,
        };
        let Some(hex) = String::from_value(property, key, value).map_err(name_edge)? else { return Ok(None) };
        parse_hex_color(property, &hex).map_err(name_edge).map(Some)
    }
}

impl<E: Keyword + LuaType> Input for E {
    fn from_value(_: &str, _: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        Ok(value.as_string().and_then(|name| E::find(&name.as_bytes())))
    }
}

impl<T: Input> Input for Option<T> {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        match value {
            Value::Nil => Ok(Some(None)),
            value => Ok(T::from_value(property, key, value)?.map(Some)),
        }
    }
}

/// A dense list, each item named `key[i]`.
impl<T: Input> Input for Vec<T> {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Value::Table(list) = value else { return Ok(None) };
        let len = array_len(property, list, super::MAX_ARRAY_ELEMENTS)?;
        let items = (1..=len).map(|at| {
            read(property, &format!("{key}[{at}]"), list.raw_get(at).map_err(|e| invalid(property, e.to_string()))?)
        });
        items.collect::<Result<_, _>>().map(Some)
    }
}

/// `#table` once it is checked dense: keys exactly `1..=#table`, at most `limit` of them. Reading
/// to the first `nil` would silently drop every entry after a hole.
pub(crate) fn array_len(name: &str, table: &mlua::Table, limit: usize) -> Result<usize, LayoutError> {
    let len = table.raw_len();
    if len > limit {
        return Err(invalid(name, format!("at most {limit} entries")));
    }
    let mut count = 0;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| invalid(name, e.to_string()))?;
        if !matches!(key, Value::Integer(i) if i > 0 && i as usize <= len) {
            return Err(invalid(name, "expected a dense array with no named keys"));
        }
        count += 1;
        if count > limit {
            return Err(invalid(name, format!("at most {limit} entries")));
        }
    }
    if count != len {
        return Err(invalid(name, "expected a dense array"));
    }
    Ok(len)
}

/// A `{ a, b }` pair.
impl<A: Input, B: Input> Input for (A, B) {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Value::Table(pair) = value else { return Ok(None) };
        let at = |index: usize| -> Result<Value, LayoutError> {
            pair.get(index).map_err(|e| invalid(property, e.to_string()))
        };
        let a = read(property, &format!("{key}[1]"), at(1)?)?;
        let b = read(property, &format!("{key}[2]"), at(2)?)?;
        Ok(Some((a, b)))
    }
}
