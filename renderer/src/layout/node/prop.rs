//! A node property's value type: the parser the engine reads it with and, through [`LuaType`], the
//! type `lua-meta` declares for it. `lua::nodes::properties` declares every property as a
//! [`Field`] of one, so the stub cannot name a type the parser does not read. The value types
//! particular to one property live beside the code that uses them; the shared ones are here.

use std::marker::PhantomData;

use mlua::{Function, Value};

use super::{
    LayoutError, PropMap, Rgba, checked_string, invalid, parse_hex_color, preview_for_error,
    reject_signal_in_structural_field, value_as_f32,
};
use crate::lua::luacats::{LuaType, spelled};
use crate::lua::nodes::properties::{Absent, Property, kind_of};
use crate::lua::signal::{self, is_signal};

pub(crate) trait Prop: LuaType {
    type Out;
    /// A closed set's names, which the stubs spell as a union or an alias ahead of [`LuaType::lua`].
    const CHOICES: &'static [&'static str] = &[];
    /// Copied past `resolve_properties` as written, signal and all: [`Structural`] and [`Handle`].
    const RAW: bool = false;
    /// `value` is the property's entry, `None` when absent; `row` carries its name, range and default.
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError>;
    /// What a [`Bound`] signal reads as in `surface_specs`, which may call no getter (ADR-0049's second amendment).
    fn deferred(row: &Property) -> Result<Self::Out, LayoutError> {
        Self::read(row, None)
    }
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

impl<T: Prop<Out = String>> Field<T> {
    /// A [`Name`] whose default spells the surface's `id` as `{id}`: only the default (also the
    /// placeholder for a signal) is formatted; a written string is taken as written.
    pub(crate) fn read_with_id(&self, properties: &PropMap, id: &str) -> Result<String, LayoutError> {
        let name = self.read(properties)?;
        Ok(match properties.get(self.row.name) {
            Some(Value::String(_)) => name,
            _ => name.replace("{id}", id),
        })
    }
}

/// `T`, or a signal of one, resolved at most once per pass (ADR-0044) and kept until it is written
/// (ADR-0270). The stubs spell it `T|Bound`.
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
        match value {
            Some(Value::UserData(ud)) if is_signal(ud) => T::deferred(row),
            value => T::read(row, value),
        }
    }
}

/// `T`, never a signal: read once per evaluation to make a structural decision (a surface's
/// placement, a node's identity), which a value changing between passes would leave stale.
pub(crate) struct Structural<T>(PhantomData<T>);

impl<T: LuaType> LuaType for Structural<T> {
    fn lua() -> String {
        T::lua()
    }
}

impl<T: Prop> Prop for Structural<T> {
    type Out = T::Out;
    const CHOICES: &'static [&'static str] = T::CHOICES;
    const RAW: bool = true;
    fn read(row: &Property, value: Option<&Value>) -> Result<T::Out, LayoutError> {
        if let Some(value) = value {
            reject_signal_in_structural_field(row.name, value)?;
        }
        T::read(row, value)
    }
}

/// The signal handle itself, which the engine writes (`hover`, `geometry`) or reads and clamps
/// (`scroll`); any other value is inert, since the handles refuse every kind they must not write.
pub(crate) struct Handle;

spelled!(Handle => "Bound");

impl Prop for Handle {
    type Out = Option<signal::Signal>;
    const RAW: bool = true;
    fn read(_: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        Ok(match value {
            Some(Value::UserData(ud)) => signal::from_userdata(ud),
            _ => None,
        })
    }
}

/// A number, the row's default when absent and within its range when it has one.
pub(crate) struct Num;

spelled!(Num => f32::lua());

impl Prop for Num {
    type Out = f32;
    fn read(row: &Property, value: Option<&Value>) -> Result<f32, LayoutError> {
        let Some(value) = value else {
            let Absent::Number(n) = row.absent else { panic!("`{}` has no default number", row.name) };
            return Ok(n);
        };
        let n = value_as_f32(row.name, value)?
            .ok_or_else(|| invalid(row.name, format!("expected a number, got {}", preview_for_error(value))))?;
        within(row, n)
    }
}

/// An optional pixel bound: `max_width`/`max_height` cap a `Content`-sized node's growth, leaving
/// the overflow for `scroll`; `min_width`/`min_height` floor it. Percent and `"Fill"` bounds add no
/// meaning beyond a fixed size.
pub(crate) struct Pixels;

spelled!(Pixels => f32::lua());

impl Prop for Pixels {
    type Out = Option<f32>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<f32>, LayoutError> {
        let Some(value) = value else { return Ok(None) };
        match value_as_f32(row.name, value)? {
            Some(n) => within(row, n).map(Some),
            None => Err(invalid(row.name, format!("expected a number of pixels, got {}", preview_for_error(value)))),
        }
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

spelled!(Flag => bool::lua());

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

/// The row's `Absent::Lua` string literal without its quotes, `""` when it has none.
fn literal(row: &Property) -> &'static str {
    match row.absent {
        Absent::Lua(literal) => literal.trim_matches('"'),
        _ => "",
    }
}

/// A string under the 64 KiB cap; absent, the row's literal default. An absent `content` or
/// `source` is empty for the pre-first-push nil rule (ADR-0044): a capability signal reads `nil`
/// until its first snapshot.
pub(crate) struct Text;

spelled!(Text => String::lua());

impl Prop for Text {
    type Out = String;
    fn read(row: &Property, value: Option<&Value>) -> Result<String, LayoutError> {
        match value {
            None => Ok(literal(row).to_string()),
            Some(Value::String(s)) => checked_string(row.name, s),
            Some(other) => Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(other)))),
        }
    }
}

/// [`Text`] that is an absolute path or empty (ADR-0253): a relative one would resolve against
/// whatever directory the Renderer started in.
pub(crate) struct Path;

spelled!(Path => String::lua());

impl Prop for Path {
    type Out = String;
    fn read(row: &Property, value: Option<&Value>) -> Result<String, LayoutError> {
        let path = Text::read(row, value)?;
        if !path.is_empty() && !path.starts_with('/') {
            return Err(invalid(row.name, format!("expected an absolute path, got `{path}`")));
        }
        Ok(path)
    }
}

/// A surface's string field: `id`, `monitor`, `namespace`, `parent`, `title`, `app_id`. Absent, the
/// row's literal default, where `{id}` stands for the surface's `id`, or an error when it is
/// required. Not capped like [`Text`]: these name things, and a name is compared whole.
pub(crate) struct Name;

spelled!(Name => String::lua());

impl Prop for Name {
    type Out = String;
    fn read(row: &Property, value: Option<&Value>) -> Result<String, LayoutError> {
        match value {
            None if row.absent == Absent::Required => {
                Err(invalid(row.name, format!("surface node requires `{}`", row.name)))
            }
            None => Ok(literal(row).to_string()),
            Some(Value::String(s)) => Ok(s.to_string_lossy()),
            Some(other) => Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(other)))),
        }
    }
}

/// The optional `id` on every node kind, one level below a surface's root (ADR-0045 decisions 1-2).
/// `None` means no id: `pair_children_by_id_then_position` pairs an id-less child positionally
/// against its id-less siblings. Non-UTF-8 is refused rather than converted: `to_string_lossy` maps
/// `"\xFF"` and `"\xFE"` both to `U+FFFD`, so distinct ids would compare equal and a fresh child
/// could claim the wrong counterpart. Scoping and duplicates are the pairing's to check.
pub(crate) struct Id;

spelled!(Id => String::lua());

impl Prop for Id {
    type Out = Option<String>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<String>, LayoutError> {
        match value {
            None => Ok(None),
            Some(Value::String(s)) => s.to_str().map(|s| Some((*s).to_owned())).map_err(|_| {
                invalid(
                    row.name,
                    "must be valid UTF-8 -- an id is compared for equality, so it cannot be converted lossily",
                )
            }),
            Some(other) => Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(other)))),
        }
    }
}

/// `"#RRGGBB"` or `"#RRGGBBAA"`; absent, the row's literal default or `None`.
pub(crate) struct Color;

spelled!(Color => "Color");

impl Prop for Color {
    type Out = Option<Rgba>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Rgba>, LayoutError> {
        let Some(value) = value else {
            return match literal(row) {
                "" => Ok(None),
                hex => parse_hex_color(row.name, hex).map(Some),
            };
        };
        let Value::String(s) = value else {
            return Err(invalid(row.name, format!("expected a string, got {}", preview_for_error(value))));
        };
        parse_hex_color(row.name, &checked_string(row.name, s)?).map(Some)
    }
}

/// A closed set of names, each one Rust value. [`keywords!`] declares one from an enum.
pub(crate) trait Keyword: Copy + 'static {
    const NAMES: &'static [&'static str];
    const VALUES: &'static [Self];

    /// The Lua name of this value.
    fn name(self) -> &'static str
    where
        Self: PartialEq,
    {
        Self::NAMES[Self::VALUES.iter().position(|value| *value == self).expect("every value has a name")]
    }
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
            return match row.absent {
                Absent::Choice(name) => {
                    Ok(find(name.as_bytes()).expect("`every_choice_default_is_one_of_its_choices`"))
                }
                _ => Err(invalid(row.name, format!("surface node requires `{}`", row.name))),
            };
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

/// An enum whose variants are a closed set of Lua names: `Cover = "cover"` where the name is not
/// the variant's. Spelled in LuaCATS as the union of its names.
macro_rules! keywords {
    ($(#[$attr:meta])* $vis:vis enum $name:ident { $($(#[$variant_attr:meta])* $variant:ident $(= $lua:literal)?),+ $(,)? }) => {
        $(#[$attr])*
        $vis enum $name { $($(#[$variant_attr])* $variant),+ }

        impl $crate::layout::node::prop::Keyword for $name {
            const NAMES: &'static [&'static str] = &[$($crate::layout::node::prop::keywords!(@name $variant $($lua)?)),+];
            const VALUES: &'static [Self] = &[$(Self::$variant),+];
        }

        impl $crate::lua::luacats::LuaType for $name {
            fn lua() -> String {
                <Self as $crate::layout::node::prop::Keyword>::NAMES
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join("|")
            }
        }
    };
    (@name $variant:ident) => { stringify!($variant) };
    (@name $variant:ident $lua:literal) => { $lua };
}
pub(crate) use keywords;

/// A function the engine calls; its signature is the row's (`props!`'s `name(param: Type)` form).
pub(crate) struct Callback;

spelled!(Callback => Function::lua());

impl Prop for Callback {
    type Out = Option<Function>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Option<Function>, LayoutError> {
        match value {
            None if row.absent == Absent::Required => {
                Err(invalid(row.name, format!("required for `{}`, got nothing", kind_of(row.kinds))))
            }
            None => Ok(None),
            Some(Value::Function(function)) => Ok(Some(function.clone())),
            Some(other) => Err(invalid(row.name, format!("expected a function, got {}", preview_for_error(other)))),
        }
    }
}

/// A `lock` property the role declares only to refuse: a lock surface covers every connected
/// output for exactly as long as the compositor holds the session locked (ADR-0042, ADR-0052
/// decision 2). A silent no-op is still an error, and a signal is refused like a literal.
pub(crate) struct Refused;

spelled!(Refused => "nil");

impl Prop for Refused {
    type Out = ();
    fn read(row: &Property, value: Option<&Value>) -> Result<(), LayoutError> {
        match value {
            None => Ok(()),
            Some(_) => Err(invalid(
                row.name,
                format!(
                    "a `lock` takes no `{}`: a lock surface covers every connected output, for exactly as long as the compositor holds \
                     the session locked, and none of that is the config's to set",
                    row.name
                ),
            )),
        }
    }
}
