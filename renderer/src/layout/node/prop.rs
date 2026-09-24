//! A node property's value type: the parser the engine reads it with and, through [`LuaType`], the
//! type `lua-meta` declares for it. `lua::nodes::properties` declares every property as a
//! [`Field`] of one, so the stub cannot name a type the parser does not read.

use std::marker::PhantomData;

use mlua::{Function, Value};

use super::{LayoutError, PropMap, Rgba, invalid, parse_hex_color, preview_for_error, value_as_f32};
use crate::lua::luacats::LuaType;
use crate::lua::nodes::properties::{Absent, Property};

pub(crate) trait Prop: LuaType {
    type Out;
    /// A closed set's names, which the stubs spell as a union or an alias ahead of [`LuaType::lua`].
    const CHOICES: &'static [&'static str] = &[];
    /// `value` is the property's entry, `None` when absent; `row` carries its name, range and default.
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError>;
}

/// One declared property: its row and, in the type, how it parses.
pub(crate) struct Field<T> {
    pub row: Property,
    of: PhantomData<fn() -> T>,
}

impl<T: Prop> Field<T> {
    pub(crate) const fn new(row: Property) -> Self {
        Self { row, of: PhantomData }
    }

    pub(crate) fn read(&self, properties: &PropMap) -> Result<T::Out, LayoutError> {
        T::read(&self.row, properties.get(self.row.name))
    }
}

/// `T`, or a signal of one, resolved once per pass (ADR-0044). The stubs spell it `T|Bound`.
pub(crate) struct Bound<T>(PhantomData<T>);

impl<T: LuaType> LuaType for Bound<T> {
    fn lua() -> String {
        match T::lua() {
            inner if inner.is_empty() => "Bound".to_string(),
            inner => format!("{inner}|Bound"),
        }
    }
}

impl<T: Prop> Prop for Bound<T> {
    type Out = T::Out;
    const CHOICES: &'static [&'static str] = T::CHOICES;
    fn read(row: &Property, value: Option<&Value>) -> Result<T::Out, LayoutError> {
        T::read(row, value)
    }
}

/// A number, the row's default when absent and within its range when it has one.
pub(crate) struct Num;

impl LuaType for Num {
    fn lua() -> String {
        f32::lua()
    }
}

impl Prop for Num {
    type Out = f32;
    fn read(row: &Property, value: Option<&Value>) -> Result<f32, LayoutError> {
        let n = match value {
            None => match row.absent {
                Absent::Number(n) => n,
                _ => panic!("`{}` has no default number", row.name),
            },
            Some(value) => value_as_f32(row.name, value)?
                .ok_or_else(|| invalid(row.name, format!("expected a number, got {}", preview_for_error(value))))?,
        };
        within(row, n)
    }
}

/// `n` inside `row`'s closed range, if it has one.
pub(crate) fn within(row: &Property, n: f32) -> Result<f32, LayoutError> {
    match row.range {
        Some((low, high)) if !(low..=high).contains(&n) => {
            Err(invalid(row.name, format!("must be within [{low}, {high}], got {n}")))
        }
        _ => Ok(n),
    }
}

/// A boolean, the row's default when absent.
pub(crate) struct Flag;

impl LuaType for Flag {
    fn lua() -> String {
        bool::lua()
    }
}

impl Prop for Flag {
    type Out = bool;
    fn read(row: &Property, value: Option<&Value>) -> Result<bool, LayoutError> {
        match value {
            None => match row.absent {
                Absent::Bool(b) => Ok(b),
                _ => panic!("`{}` has no default boolean", row.name),
            },
            Some(Value::Boolean(b)) => Ok(*b),
            Some(other) => Err(invalid(row.name, format!("expected a boolean, got {}", preview_for_error(other)))),
        }
    }
}

/// A string under the 64 KiB cap, `""` when absent.
pub(crate) struct Text;

impl LuaType for Text {
    fn lua() -> String {
        String::lua()
    }
}

impl Prop for Text {
    type Out = String;
    fn read(row: &Property, value: Option<&Value>) -> Result<String, LayoutError> {
        match value {
            None => Ok(String::new()),
            Some(Value::String(s)) => super::checked_string(row.name, s),
            Some(other) => Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(other)))),
        }
    }
}

/// `"#RRGGBB"` or `"#RRGGBBAA"`; absent, the row's literal default or `None`.
pub(crate) struct Color;

impl LuaType for Color {
    fn lua() -> String {
        "Color".to_string()
    }
}

impl Prop for Color {
    type Out = Option<Rgba>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Rgba>, LayoutError> {
        let Some(value) = value else {
            return match row.absent {
                Absent::Lua(literal) => parse_hex_color(row.name, literal.trim_matches('"')).map(Some),
                _ => Ok(None),
            };
        };
        let Value::String(s) = value else {
            return Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(value))));
        };
        parse_hex_color(row.name, &super::checked_string(row.name, s)?).map(Some)
    }
}

/// A closed set of names, each one Rust value. [`keywords!`] declares one from an enum.
pub(crate) trait Keyword: Copy + 'static {
    const NAMES: &'static [&'static str];
    const VALUES: &'static [Self];
}

/// One of `E`'s names; the row's `Absent::Choice` when absent.
pub(crate) struct OneOf<E>(PhantomData<E>);

impl<E> LuaType for OneOf<E> {
    /// Nothing past the union the stubs build from [`Prop::CHOICES`].
    fn lua() -> String {
        String::new()
    }
}

impl<E: Keyword> Prop for OneOf<E> {
    type Out = E;
    const CHOICES: &'static [&'static str] = E::NAMES;
    fn read(row: &Property, value: Option<&Value>) -> Result<E, LayoutError> {
        let find = |name: &[u8]| E::NAMES.iter().position(|choice| choice.as_bytes() == name).map(|at| E::VALUES[at]);
        let Some(value) = value else {
            let Absent::Choice(name) = row.absent else { panic!("`{}` has no default choice", row.name) };
            return Ok(find(name.as_bytes()).expect("`every_choice_default_is_one_of_its_choices`"));
        };
        let Value::String(s) = value else {
            return Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(value))));
        };
        find(&s.as_bytes()).ok_or_else(|| {
            let names: Vec<String> = E::NAMES.iter().map(|name| format!("`{name}`")).collect();
            invalid(row.name, format!("expected one of {}, got {}", names.join(", "), preview_for_error(value)))
        })
    }
}

/// An enum whose variants are a property's names: `Cover = "cover"` where the name is not the
/// variant's.
macro_rules! keywords {
    ($(#[$attr:meta])* $vis:vis enum $name:ident { $($(#[$variant_attr:meta])* $variant:ident $(= $lua:literal)?),+ $(,)? }) => {
        $(#[$attr])*
        $vis enum $name { $($(#[$variant_attr])* $variant),+ }

        impl $crate::layout::node::prop::Keyword for $name {
            const NAMES: &'static [&'static str] = &[$($crate::layout::node::prop::keywords!(@name $variant $($lua)?)),+];
            const VALUES: &'static [Self] = &[$(Self::$variant),+];
        }
    };
    (@name $variant:ident) => { stringify!($variant) };
    (@name $variant:ident $lua:literal) => { $lua };
}
pub(crate) use keywords;

/// A function the engine calls; its signature is the row's (`props!`'s `name(param: Type)` form).
#[expect(dead_code, reason = "the callback rows move onto fields with the rest of the table")]
pub(crate) struct Callback;

impl LuaType for Callback {
    fn lua() -> String {
        Function::lua()
    }
}

impl Prop for Callback {
    type Out = Option<Function>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Function>, LayoutError> {
        match value {
            None => Ok(None),
            Some(Value::Function(function)) => Ok(Some(function.clone())),
            Some(other) => Err(invalid(row.name, format!("expected a function, got {}", preview_for_error(other)))),
        }
    }
}
