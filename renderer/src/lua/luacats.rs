//! Rust types' LuaCATS spellings. What `lua-meta` declares for a node property's value, a global's
//! parameter or its return comes from the Rust type the engine reads or hands back, through
//! [`LuaType`]; only the stub tests call it. [`lua_fn!`] and [`lua_class!`] define a global or a
//! handle from a Rust signature, so its stub is that signature's.

use std::marker::PhantomData;

use mlua::{AnyUserData, FromLua, Function, IntoLua, Lua, LuaString, Table, Value, Variadic};

/// A Rust type as LuaCATS spells it.
pub(crate) trait LuaType {
    /// The LuaCATS type, `""` for none (`()`).
    fn lua() -> String;
    /// `Option`: `name?` on a parameter, `T?` on a return.
    const OPTIONAL: bool = false;
    /// `Variadic`: the parameter is `...`.
    const VARIADIC: bool = false;
    /// Names [`Generic`]'s `T`, which the function then declares with `---@generic T`.
    const GENERIC: bool = false;
    /// The `---@class` blocks this type needs declared before a stub uses it, as a handle's class
    /// before the function returning it.
    fn classes(_out: &mut Vec<String>) {}
}

/// Implements [`LuaType`] for each type as `lua`, a spelling.
macro_rules! spelled {
    ($($ty:ty),+ => $lua:expr) => {
        $(impl $crate::lua::luacats::LuaType for $ty {
            fn lua() -> String {
                ($lua).to_string()
            }
        })+
    };
}
pub(crate) use spelled;

spelled!(bool => "boolean");
spelled!(f32, f64 => "number");
spelled!(i32, i64, u32, u64, usize => "integer");
spelled!(String, LuaString => "string");
spelled!(Value => "any");
spelled!(Table => "table");
spelled!(Function => "function");
spelled!(AnyUserData => "userdata");
spelled!(() => "");
spelled!(std::time::Duration => f32::lua());
spelled!(std::path::PathBuf => String::lua());
spelled!(crate::layout::hit::LogicalPoint => "{ x: number, y: number }");
spelled!(super::VirtualNode => "Node");

impl<T: LuaType> LuaType for Option<T> {
    fn lua() -> String {
        T::lua()
    }
    const OPTIONAL: bool = true;
    const GENERIC: bool = T::GENERIC;
    fn classes(out: &mut Vec<String>) {
        T::classes(out);
    }
}

impl<T: LuaType> LuaType for Vec<T> {
    fn lua() -> String {
        let item = spelling::<T>();
        if item.contains('|') { format!("({item})[]") } else { format!("{item}[]") }
    }
    const GENERIC: bool = T::GENERIC;
    fn classes(out: &mut Vec<String>) {
        T::classes(out);
    }
}

impl<T: LuaType> LuaType for Variadic<T> {
    fn lua() -> String {
        T::lua()
    }
    const VARIADIC: bool = true;
}

/// `T`'s spelling with its `?`, for a type inside another.
pub(crate) fn spelling<T: LuaType>() -> String {
    optional(T::lua(), T::OPTIONAL)
}

/// `text?` when `optional`: a name or a type that may be `nil`.
pub(crate) fn optional(text: String, optional: bool) -> String {
    if optional { text + "?" } else { text }
}

/// A Lua value whose type the caller picks, `T` in the stub: `state`'s `initial`, `delay`'s source.
pub(crate) struct Generic(pub Value);

impl LuaType for Generic {
    fn lua() -> String {
        "T".to_string()
    }
    const GENERIC: bool = true;
}

impl FromLua for Generic {
    fn from_lua(value: Value, _: &Lua) -> mlua::Result<Self> {
        Ok(Generic(value))
    }
}

/// A signal of `T`, as the Rust value `S` holding it: `Signal<T>` in the stub.
pub(crate) struct SignalOf<T, S = crate::lua::signal::Signal>(pub S, PhantomData<T>);

impl<T, S> SignalOf<T, S> {
    pub(crate) fn new(signal: S) -> Self {
        Self(signal, PhantomData)
    }
}

impl<T: LuaType, S> LuaType for SignalOf<T, S> {
    fn lua() -> String {
        format!("Signal<{}>", spelling::<T>())
    }
    const GENERIC: bool = T::GENERIC;
}

impl<T, S: IntoLua> IntoLua for SignalOf<T, S> {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

impl<T, S: FromLua> FromLua for SignalOf<T, S> {
    fn from_lua(value: Value, lua: &Lua) -> mlua::Result<Self> {
        S::from_lua(value, lua).map(Self::new)
    }
}

/// `A|B`: what a function the Lua runtime implements returns.
pub(crate) struct Or<A, B>(PhantomData<(A, B)>);

impl<A: LuaType, B: LuaType> LuaType for Or<A, B> {
    fn lua() -> String {
        format!("{}|{}", spelling::<A>(), spelling::<B>())
    }
}

/// A value read or handed back as `R` and declared as `S`: a parameter's own parser checks what the
/// Rust type `R` cannot say, with messages of its own.
pub(crate) struct As<R, S>(pub R, pub PhantomData<S>);

impl<R, S: LuaType> LuaType for As<R, S> {
    fn lua() -> String {
        S::lua()
    }
    const OPTIONAL: bool = S::OPTIONAL;
    const GENERIC: bool = S::GENERIC;
    fn classes(out: &mut Vec<String>) {
        S::classes(out);
    }
}

impl<R: FromLua, S> FromLua for As<R, S> {
    fn from_lua(value: Value, lua: &Lua) -> mlua::Result<Self> {
        R::from_lua(value, lua).map(|read| As(read, PhantomData))
    }
}

impl<R: IntoLua, S> IntoLua for As<R, S> {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}

/// A Lua function a global takes, whose signature the declaring macro spells.
pub(crate) struct Fun(pub Function);

impl FromLua for Fun {
    fn from_lua(value: Value, lua: &Lua) -> mlua::Result<Self> {
        Function::from_lua(value, lua).map(Fun)
    }
}

/// `fun(name: T, ...): R`.
pub(crate) fn fun(params: &[(&str, Spelling)], ret: Option<(Spelling, bool)>) -> String {
    let params: Vec<String> = params.iter().map(|(name, ty)| format!("{name}: {}", ty())).collect();
    let ret = ret.map_or_else(String::new, |(ty, nil)| format!(": {}", optional(ty(), nil)));
    format!("fun({}){ret}", params.join(", "))
}

/// [`LuaType::lua`] of some type.
pub(crate) type Spelling = fn() -> String;

/// `name` without a raw identifier's `r#`, or `...` for a [`Variadic`] parameter.
pub(crate) const fn param_name<T: LuaType>(name: &'static str) -> &'static str {
    match name.as_bytes() {
        _ if T::VARIADIC => "...",
        [b'r', b'#', ..] => name.split_at(2).1,
        _ => name,
    }
}

/// A parameter or a return of a global or a method, as its declaring macro saw it.
#[cfg_attr(not(test), expect(dead_code, reason = "read by the globals golden, a test"))]
pub(crate) struct Param {
    /// `""` for an unnamed return.
    pub name: &'static str,
    /// The `///` block above it.
    pub doc: &'static str,
    pub ty: Spelling,
    pub optional: bool,
    pub generic: bool,
    pub classes: fn(&mut Vec<String>),
}

/// A global function's or a method's `///` block and signature.
#[cfg_attr(not(test), expect(dead_code, reason = "read by the globals golden, a test"))]
pub(crate) struct Signature {
    pub doc: &'static str,
    pub params: &'static [Param],
    pub returns: &'static [Param],
}

/// A `///` block's lines, trimmed of the space `///` leaves.
#[cfg(test)]
pub(crate) fn lines(doc: &str) -> impl Iterator<Item = &str> {
    doc.lines().map(|line| line.strip_prefix(' ').unwrap_or(line)).skip_while(|line| line.is_empty())
}

/// A `///` block as `---` comment lines.
#[cfg(test)]
pub(crate) fn comment(doc: &str) -> String {
    lines(doc).map(|line| format!("---{line}\n")).collect()
}

/// A `///` block joined onto one line, for a `---@param` or `---@return`.
#[cfg(test)]
pub(crate) fn one_line(doc: &str) -> String {
    lines(doc).map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
impl Signature {
    /// The classes its parameters and returns name, first seen first.
    pub(crate) fn classes(&self, out: &mut Vec<String>) {
        for param in self.params.iter().chain(self.returns) {
            (param.classes)(out);
        }
    }

    /// The `---` block above the declaration: the doc's lines, `---@generic T` when a type names
    /// it, then each `@param` and `@return`.
    pub(crate) fn stub(&self) -> String {
        let mut out = comment(self.doc);
        if self.params.iter().chain(self.returns).any(|param| param.generic) {
            out += "---@generic T\n";
        }
        let words = |doc: &str| match one_line(doc) {
            words if words.is_empty() => String::new(),
            words => format!(" {words}"),
        };
        for param in self.params {
            let name = optional(param.name.to_string(), param.optional);
            out += &format!("---@param {name} {}{}\n", (param.ty)(), words(param.doc));
        }
        for ret in self.returns {
            let ty = optional((ret.ty)(), ret.optional);
            out += &match (ret.name, one_line(ret.doc)) {
                ("", doc) if doc.is_empty() => format!("---@return {ty}\n"),
                ("", doc) => format!("---@return {ty} # {doc}\n"),
                (name, _) => format!("---@return {ty} {name}{}\n", words(ret.doc)),
            };
        }
        out
    }

    /// The parameter list of the Lua declaration.
    pub(crate) fn names(&self) -> String {
        self.params.iter().map(|param| param.name).collect::<Vec<_>>().join(", ")
    }
}

/// A `Param` for `$ty`, whose LuaCATS is `$lua`.
macro_rules! param {
    ($name:expr, [$($doc:literal)*], $ty:ty, $lua:expr) => {
        $crate::lua::luacats::param!($name, [$($doc)*], $ty, $lua, <$ty as $crate::lua::luacats::LuaType>::classes)
    };
    ($name:expr, [$($doc:literal)*], $ty:ty, $lua:expr, $classes:expr) => {
        $crate::lua::luacats::Param {
            name: $crate::lua::luacats::param_name::<$ty>($name),
            doc: concat!($($doc, "\n",)* ""),
            ty: $lua,
            optional: <$ty as $crate::lua::luacats::LuaType>::OPTIONAL,
            generic: <$ty as $crate::lua::luacats::LuaType>::GENERIC,
            classes: $classes,
        }
    };
}
pub(crate) use param;

/// Defines a global function from a Rust one, its stub derived from the signature:
///
/// ```ignore
/// lua_fn!(lua,
///     /// What it does.
///     fn timer(lua, /// Param doc.
///         ms: u64, callback: fn()) -> TimerHandle { ... });
/// ```
///
/// The first parameter binds the `&Lua`. A parameter typed `fn(name: T, ...) -> R` is a Lua
/// function, [`Fun`] in Rust. Returns are a type, or `(name: T, ...)` for several named ones; a
/// `///` block may precede each. `fn path(params) -> Ret = value` declares `value`, a function the
/// Lua runtime already has, under that signature: its types are the stub's, not enforced.
macro_rules! lua_fn {
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?)
        -> ($($(#[doc = $ret_doc:literal])* $ret:ident: $ret_ty:ty),+ $(,)?) $(as $out:ty)? $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*);
            [$($crate::lua::luacats::param!(stringify!($ret), [$($ret_doc)*], $ret_ty, <$ret_ty as $crate::lua::luacats::LuaType>::lua)),+];
            (body $l; $crate::lua::luacats::lua_fn!(@or [($($ret_ty,)+)] $($out)?); $body)
        } [] $($($params)*)?)
    };
    (@or [$default:ty]) => { $default };
    (@or [$default:ty] $out:ty) => { $out };
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?)
        -> $(#[doc = $ret_doc:literal])* $ret:ty $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*);
            [$crate::lua::luacats::param!("", [$($ret_doc)*], $ret, <$ret as $crate::lua::luacats::LuaType>::lua)];
            (body $l; $ret; $body)
        } [] $($($params)*)?)
    };
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?) $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*); []; (body $l; (); $body)
        } [] $($($params)*)?)
    };
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($($params:tt)*)
        -> $(#[doc = $ret_doc:literal])* $ret:ty = $value:expr) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*);
            [$crate::lua::luacats::param!("", [$($ret_doc)*], $ret, <$ret as $crate::lua::luacats::LuaType>::lua)];
            (value $value)
        } [] $($params)*)
    };
    (@params $ctx:tt [$($done:tt)*]) => {
        $crate::lua::luacats::lua_fn!(@emit $ctx $($done)*)
    };
    (@params $ctx:tt [$($done:tt)*] $(#[doc = $doc:literal])* $name:ident: fn($($arg:ident: $arg_ty:ty),*) $(-> $ret:ty)?
        $(, $($rest:tt)*)?) => {
        $crate::lua::luacats::lua_fn!(@params $ctx [$($done)* {
            $name; $crate::lua::luacats::Fun;
            $crate::lua::luacats::param!(
                stringify!($name),
                [$($doc)*],
                mlua::Function,
                || $crate::lua::luacats::fun(
                    &[$(($crate::lua::luacats::param_name::<$arg_ty>(stringify!($arg)), $crate::lua::luacats::spelling::<$arg_ty>)),*],
                    $crate::lua::luacats::lua_fn!(@ret $($ret)?),
                ),
                |_out| { $(<$arg_ty as $crate::lua::luacats::LuaType>::classes(_out);)* $(<$ret as $crate::lua::luacats::LuaType>::classes(_out);)? }
            )
        }] $($($rest)*)?)
    };
    (@params $ctx:tt [$($done:tt)*] $(#[doc = $doc:literal])* $name:ident: $ty:ty $(, $($rest:tt)*)?) => {
        $crate::lua::luacats::lua_fn!(@params $ctx [$($done)* {
            $name; $ty; $crate::lua::luacats::param!(stringify!($name), [$($doc)*], $ty, <$ty as $crate::lua::luacats::LuaType>::lua)
        }] $($($rest)*)?)
    };
    (@ret) => { None };
    (@ret $ret:ty) => {
        Some((<$ret as $crate::lua::luacats::LuaType>::lua, <$ret as $crate::lua::luacats::LuaType>::OPTIONAL))
    };
    (@emit { $lua:expr; [$($doc:literal)*]; $path:expr; [$($returns:expr),*]; $how:tt }
        $({ $name:ident; $ty:ty; $param:expr })*) => {{
        const SIGNATURE: $crate::lua::luacats::Signature = $crate::lua::luacats::Signature {
            doc: concat!($($doc, "\n",)* ""),
            params: &[$($param),*],
            returns: &[$($returns),*],
        };
        $crate::lua::define(
            $lua,
            $path,
            $crate::lua::Stub::Fn(&SIGNATURE),
            $crate::lua::luacats::lua_fn!(@value $lua; $how; $($name: $ty),*),
        )
    }};
    (@value $lua:expr; (body $l:ident; $out:ty; $body:block); $($name:ident: $ty:ty),*) => {
        $lua.create_function(move |$l, ($($name,)*): ($($ty,)*)| -> mlua::Result<$out> { $body })?
    };
    (@value $lua:expr; (value $value:expr); $($name:ident: $ty:ty),*) => {
        $value
    };
}
pub(crate) use lua_fn;

/// Defines a global table, a new empty one unless `= value` gives it, its stub its `///` block and
/// `: class`, the LuaCATS class it extends: `lua_table!(lua, /// Doc. os: oslib = &kept)`.
macro_rules! lua_table {
    ($lua:expr, $(#[doc = $doc:literal])* $name:ident $(: $class:ident)? $(= $value:expr)?) => {
        $crate::lua::define(
            $lua,
            stringify!($name),
            $crate::lua::Stub::Table { doc: concat!($($doc, "\n",)* ""), class: None $(.or(Some(stringify!($class))))? },
            $crate::lua::luacats::lua_table!(@value $lua $(, $value)?),
        )
    };
    (@value $lua:expr) => { $lua.create_table()? };
    (@value $lua:expr, $value:expr) => { $value };
}
pub(crate) use lua_table;

/// Implements `UserData` for a handle from its methods, and its `---@class` stub from theirs:
///
/// ```ignore
/// lua_class! {
///     impl TimerHandle {
///         /// What it does.
///         fn cancel(lua, this) { ... }
///     }
/// }
/// ```
macro_rules! lua_class {
    ($(#[doc = $doc:literal])* impl $class:ident {
        $($(#[doc = $method_doc:literal])* fn $method:ident($l:ident, $this:ident $(, $(#[doc = $param_doc:literal])* $param:ident: $param_ty:ty)* $(,)?) $body:block)*
    }) => {
        impl mlua::UserData for $class {
            fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
                $(methods.add_method(stringify!($method), |$l, $this, ($($param,)*): ($($param_ty,)*)| -> mlua::Result<()> { $body });)*
            }
        }

        impl $crate::lua::luacats::LuaType for $class {
            fn lua() -> String {
                stringify!($class).to_string()
            }
            #[cfg(test)]
            fn classes(out: &mut Vec<String>) {
                const METHODS: &[(&str, $crate::lua::luacats::Signature)] = &[$((stringify!($method), $crate::lua::luacats::Signature {
                    doc: concat!($($method_doc, "\n",)* ""),
                    params: &[$($crate::lua::luacats::param!(stringify!($param), [$($param_doc)*], $param_ty, <$param_ty as $crate::lua::luacats::LuaType>::lua)),*],
                    returns: &[],
                })),*];
                $crate::lua::luacats::class(out, stringify!($class), concat!($($doc, "\n",)* ""), "", METHODS);
            }
        }
    };
}
pub(crate) use lua_class;

/// A struct handed to Lua as a table of its fields, and its `---@class` stub from theirs.
macro_rules! lua_record {
    ($(#[doc = $doc:literal])* $vis:vis struct $name:ident { $($(#[doc = $field_doc:literal])* $field:ident: $ty:ty),+ $(,)? }) => {
        $(#[doc = $doc])*
        $vis struct $name { $($(#[doc = $field_doc])* $field: $ty),+ }

        impl mlua::IntoLua for $name {
            fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                let table = lua.create_table()?;
                $(table.set(stringify!($field), self.$field)?;)+
                Ok(mlua::Value::Table(table))
            }
        }

        impl $crate::lua::luacats::LuaType for $name {
            fn lua() -> String {
                stringify!($name).to_string()
            }
            #[cfg(test)]
            fn classes(out: &mut Vec<String>) {
                let fields = [$(format!(
                    "---@field {} {} {}\n",
                    $crate::lua::luacats::optional(stringify!($field).to_string(), <$ty as $crate::lua::luacats::LuaType>::OPTIONAL),
                    <$ty as $crate::lua::luacats::LuaType>::lua(),
                    $crate::lua::luacats::one_line(concat!($($field_doc, "\n",)* "")),
                )),+].concat();
                $crate::lua::luacats::class(out, stringify!($name), concat!($($doc, "\n",)* ""), &fields, &[]);
            }
        }
    };
}
pub(crate) use lua_record;

/// A struct a parser reads a Lua table into, key for key: `KEYS` for `only_keys`, and its stub from
/// its fields and `///` blocks. `#[alias = "Name"]` spells it as an inline table, which LuaLS checks
/// inside a union where a class admits any table, and `| T` puts a bare `T` before it;
/// `#[class = "Name"]` as a class whose fields carry their own words. `key?: T` is a key Lua may
/// leave out that Rust holds defaulted, `key: T as S` one Lua spells as `S`, and fields after a `;`
/// are not keys.
macro_rules! lua_shape {
    ($(#[doc = $doc:literal])* #[alias = $name:literal $(| $bare:ty)?] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct [$($doc)*] false $name [$($bare)?] $($rest)*);
    };
    ($(#[doc = $doc:literal])* #[class = $name:literal] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct [$($doc)*] true $name [] $($rest)*);
    };
    // `$q` never matches: it gives the `?` a metavariable to repeat by.
    (@struct [$($doc:literal)*] $class:literal $name:literal [$($bare:ty)?] $(#[$attr:meta])* $vis:vis struct $ty:ident {
        $($(#[doc = $field_doc:literal])* $field_vis:vis $field:ident $(? $([$q:tt])?)?: $field_ty:ty $(as $lua:ty)?),+ $(,)?
        $(; $($extra:tt)*)?
    }) => {
        $(#[doc = $doc])* $(#[$attr])* $vis struct $ty { $($(#[doc = $field_doc])* $field_vis $field: $field_ty,)+ $($($extra)*)? }

        impl $ty {
            pub(crate) const KEYS: &'static [&'static str] = &[$(stringify!($field)),+];
        }

        impl $crate::lua::luacats::LuaType for $ty {
            fn lua() -> String {
                $name.to_string()
            }
            #[cfg(test)]
            fn classes(out: &mut Vec<String>) {
                let keys = [$((
                    $crate::lua::luacats::optional(
                        stringify!($field).to_string(),
                        $crate::lua::luacats::lua_shape!(@optional $(? $([$q])?)?)
                            || <$crate::lua::luacats::lua_shape!(@lua $field_ty $(, $lua)?) as $crate::lua::luacats::LuaType>::OPTIONAL,
                    ),
                    <$crate::lua::luacats::lua_shape!(@lua $field_ty $(, $lua)?) as $crate::lua::luacats::LuaType>::lua(),
                    concat!($($field_doc, "\n",)* ""),
                )),+];
                let bare = String::new() $(+ &<$bare as $crate::lua::luacats::LuaType>::lua() + "|")?;
                out.push($crate::lua::luacats::shape($class, $name, &bare, concat!($($doc, "\n",)* ""), &keys));
            }
        }
    };
    (@optional) => { false };
    (@optional ?) => { true };
    (@lua $ty:ty) => { $ty };
    (@lua $ty:ty, $lua:ty) => { $lua };
}
pub(crate) use lua_shape;

/// A `///` block's first paragraph on one line, then each later line as its own `---` line.
#[cfg(test)]
fn paragraphs(doc: &str) -> (String, String) {
    let (first, rest) = doc.split_once("\n\n").unwrap_or((doc, ""));
    (one_line(first), rest.lines().map(|line| format!("---{line}\n")).collect())
}

/// A [`lua_shape!`]'s stub: `---@alias Name bare{ key: T, ... } words`, or its doc and a `---@class`
/// of `---@field`s. `keys` holds each key's name (`?` when optional), type and `///` block. An
/// unknown key is a type error, as the parser refuses it.
#[cfg(test)]
pub(crate) fn shape(class: bool, name: &str, bare: &str, doc: &str, keys: &[(String, String, &str)]) -> String {
    const UNKNOWN: &str = r#""no such property""#;
    if !class {
        let (words, more) = paragraphs(doc);
        let keys: String = keys.iter().map(|(key, ty, _)| format!("{key}: {ty}, ")).collect();
        return format!("---@alias {name} {bare}{{ {keys}[string]: {UNKNOWN} }} {words}\n{more}");
    }
    let mut out = format!("{}---@class {name}\n", comment(doc));
    for (key, ty, doc) in keys {
        let (words, more) = paragraphs(doc);
        out += &format!("---@field {key} {ty} {words}\n{more}");
    }
    out + &format!("---@field [string] {UNKNOWN}\n")
}

/// Pushes a `---@class` block, its `fields`, then for a handle `local Name = {}` and one stub per
/// method, unless `out` already holds it.
#[cfg(test)]
pub(crate) fn class(out: &mut Vec<String>, name: &str, doc: &str, fields: &str, methods: &[(&str, Signature)]) {
    let mut class = format!("---@class {name}\n{}{fields}", comment(doc));
    if !methods.is_empty() {
        class += &format!("local {name} = {{}}\n");
    }
    for (method, signature) in methods {
        class += &format!("\n{}function {name}:{method}({}) end\n", signature.stub(), signature.names());
    }
    if !out.contains(&class) {
        out.push(class);
    }
}
