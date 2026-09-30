//! A config table's keys read into typed Rust values. A key's Rust type is its parser and, through
//! [`LuaType`], the stub's spelling and the error's "expected", so the three cannot disagree.
//! `lua_shape!`'s `#[input]` form reads a whole table this way.

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

/// One field of a config table, unread; a nested signal is refused.
pub(super) fn table_field(
    property: &str,
    table: &mlua::Table,
    key: impl mlua::IntoLua + std::fmt::Display + Copy,
) -> Result<Value, LayoutError> {
    plain(property, key, table.get(key).map_err(|e| invalid(property, e.to_string()))?)
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

impl Input for Rgba {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Some(hex) = String::from_value(property, key, value)? else { return Ok(None) };
        parse_hex_color(property, &hex).map(Some)
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

/// A sequence up to its first `nil`, each item named `key[i]`.
impl<T: Input> Input for Vec<T> {
    fn from_value(property: &str, key: &str, value: &Value) -> Result<Option<Self>, LayoutError> {
        let Value::Table(list) = value else { return Ok(None) };
        let items = list.sequence_values::<Value>().enumerate().map(|(at, item)| {
            read(property, &format!("{key}[{}]", at + 1), item.map_err(|e| invalid(property, e.to_string()))?)
        });
        items.collect::<Result<_, _>>().map(Some)
    }
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
