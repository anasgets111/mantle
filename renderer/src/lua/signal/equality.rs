//! Lua literal equality and edit-detection for state signals.

use mlua::Value;

/// Whether a `state` literal is comparable across evaluations. Tables, functions, and userdata use
/// pointer identity; fresh values would always look edited.
pub(super) fn is_comparable_literal(value: &Value) -> bool {
    matches!(value, Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::String(_))
}

/// Whether writing `b` over `a` changes nothing a reader sees. Strict by variant, since Lua 5.4
/// prints `1` and `1.0` differently, and floats by bits, so `-0.0` differs from `0.0` and NaN matches itself.
fn same_scalar(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a.to_bits() == b.to_bits(),
        (Value::String(a), Value::String(b)) => a.as_bytes() == b.as_bytes(),
        _ => false,
    }
}

/// Entries [`same_value`] walks before calling two tables different.
pub(super) const SAME_TABLE_ENTRIES: usize = 256;

/// [`same_scalar`], plus two different plain-data tables (no metatable, scalar keys, scalar or
/// plain-table values) equal entry for entry. The same table, anywhere in either, is a change: a
/// writer that mutated it in place and wrote it again is signalling one.
pub(super) fn same_value(a: &Value, b: &Value) -> bool {
    fn same_table(a: &mlua::Table, b: &mlua::Table, budget: &mut usize) -> bool {
        if a == b || a.metatable().is_some() || b.metatable().is_some() {
            return false;
        }
        let mut entries = 0;
        for pair in a.pairs::<Value, Value>() {
            let Ok((key, value)) = pair else { return false };
            if *budget == 0 || !is_comparable_literal(&key) {
                return false;
            }
            *budget -= 1;
            entries += 1;
            let Ok(other) = b.raw_get::<Value>(key) else { return false };
            let same = match (&value, &other) {
                (Value::Table(value), Value::Table(other)) => same_table(value, other, budget),
                _ => same_scalar(&value, &other),
            };
            if !same {
                return false;
            }
        }
        b.pairs::<Value, Value>().count() == entries
    }
    match (a, b) {
        (Value::Table(a), Value::Table(b)) => same_table(a, b, &mut { SAME_TABLE_ENTRIES }),
        _ => same_scalar(a, b),
    }
}

/// Whether the `state` literal changed. `None` means no edit per ADR-0044's amendment. This keeps
/// a table literal, such as a popup's anchor rect, from looking edited on every reload and snapping
/// the popup to the corner. `Value`'s own `PartialEq` compares an `Integer` against a `Number` the way
/// Lua `==` does, so `0` and `0.0` match; scalar types differing do not.
pub(super) fn literal_was_edited(current: &Value, seeded: &Value) -> Option<bool> {
    if !is_comparable_literal(current) || !is_comparable_literal(seeded) {
        return None;
    }
    Some(current != seeded)
}
