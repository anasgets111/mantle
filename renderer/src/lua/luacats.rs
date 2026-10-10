//! Rust types' LuaCATS spellings. What `lua-meta` declares for a node property's value, a global's
//! parameter or its return comes from the Rust type the engine reads or hands back, through
//! [`LuaType`]; only the stub tests call it. [`lua_fn!`] and [`lua_class!`] define a global or a
//! handle from a Rust signature, so its stub is that signature's.

use std::marker::PhantomData;

use mlua::{AnyUserData, FromLua, Function, IntoLua, Lua, LuaString, Table, Value, Variadic};

/// What a function's argument errors name: its path and its parameters, for [`Args`].
pub(crate) trait ArgNames {
    const FUNCTION: &'static str;
    const NAMES: &'static [&'static str];
}

/// A function's typed arguments. Converting is mlua's own stack path, so a call costs nothing
/// over a bare tuple; only a failure is reworded, as the author reads it:
/// `file:3: fonts: bad argument #1 (chain): expected a table, got integer`.
pub(crate) struct Args<T, N>(pub T, pub PhantomData<N>);

impl<T: mlua::FromLuaMulti, N: ArgNames> mlua::FromLuaMulti for Args<T, N> {
    fn from_lua_multi(values: mlua::MultiValue, lua: &Lua) -> mlua::Result<Self> {
        T::from_lua_multi(values, lua).map(|args| Self(args, PhantomData))
    }

    unsafe fn from_stack_multi(nvals: std::ffi::c_int, lua: &mlua::state::RawLua) -> mlua::Result<Self> {
        // SAFETY: mlua's own arguments, forwarded unchanged.
        unsafe { T::from_stack_multi(nvals, lua) }.map(|args| Self(args, PhantomData))
    }

    unsafe fn from_stack_args(
        nargs: std::ffi::c_int,
        index: usize,
        to: Option<&str>,
        lua: &mlua::state::RawLua,
    ) -> mlua::Result<Self> {
        // SAFETY: mlua's own arguments, forwarded unchanged.
        unsafe { T::from_stack_args(nargs, index, to, lua) }
            .map(|args| Self(args, PhantomData))
            .map_err(|err| bad_argument(N::FUNCTION, N::NAMES, err))
    }
}

// ponytail: mlua converts arguments before our code runs and gives no `&Lua`, so a `pcall`-caught error has no
// `file:N:` lead; `describe` hoists it for logs. Upgrade: an mlua API exposing `RawLua::lua()`, or `unsafe Lua::get_or_init_from_ptr`.
fn bad_argument(function: &str, names: &[&str], err: mlua::Error) -> mlua::Error {
    let mlua::Error::BadArgument { pos, cause, .. } = &err else { return err };
    let mlua::Error::FromLuaConversionError { from, to, message } = &**cause else { return err };
    let name = names.get(pos - 1).map(|name| format!(" ({name})")).unwrap_or_default();
    let want = rust_type_in_lua(to);
    // mlua's own cause, such as `out of range`, says why a right-typed value was refused.
    let cause = message.as_ref().map(|message| format!(" ({message})")).unwrap_or_default();
    mlua::Error::runtime(format!("{function}: bad argument #{pos}{name}: expected {want}, got {from}{cause}"))
}

/// A Rust type name as an article and a Lua noun; an unknown one is `a valid value`, never a Rust path.
pub(crate) fn rust_type_in_lua(rust: &str) -> &str {
    match rust {
        "u8" | "u16" | "u32" | "u64" | "usize" => "a non-negative integer",
        "i8" | "i16" | "i32" | "i64" | "isize" => "an integer",
        "f32" | "f64" => "a number",
        "bool" => "a boolean",
        "String" | "string" | "&str" => "a string",
        "table" => "a table",
        "function" => "a function",
        word if word.bytes().all(|b| b.is_ascii_lowercase()) || word.starts_with("a ") || word.starts_with("an ") => {
            word
        }
        _ => "a valid value",
    }
}

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
spelled!(String, &str, LuaString => "string");
spelled!(Value => "any");
spelled!(Table => "table");
spelled!(Function => "function");
spelled!(AnyUserData => "userdata");
spelled!(() => "");
spelled!(std::time::Duration => f32::lua());
spelled!(std::path::PathBuf => String::lua());
spelled!(crate::layout::hit::LogicalPoint => "{ x: number, y: number }");
spelled!(super::VirtualNode => "Node");
/// The `Modifiers` class the pointer handlers end with; the value is built by `marshal::modifiers_table`.
pub(crate) struct Modifiers;
spelled!(Modifiers => "Modifiers");

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

impl<A: LuaType, B: LuaType> LuaType for (A, B) {
    fn lua() -> String {
        format!("[{}, {}]", spelling::<A>(), spelling::<B>())
    }
}

impl<T: LuaType, const N: usize> LuaType for [T; N] {
    fn lua() -> String {
        format!("[{}]", vec![spelling::<T>(); N].join(", "))
    }
}

impl<K: LuaType, V: LuaType> LuaType for std::collections::BTreeMap<K, V> {
    fn lua() -> String {
        format!("table<{}, {}>", spelling::<K>(), spelling::<V>())
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
            $crate::lua::luacats::lua_fn!(@value $lua; $path; $how; $($name: $ty),*),
        )
    }};
    (@value $lua:expr; $path:expr; (body $l:ident; $out:ty; $body:block); $($name:ident: $ty:ty),*) => {
        {
            struct Names;
            impl $crate::lua::luacats::ArgNames for Names {
                const FUNCTION: &'static str = $path;
                const NAMES: &'static [&'static str] = &[$(stringify!($name)),*];
            }
            $lua.create_function(move |$l, $crate::lua::luacats::Args(($($name,)*), _): $crate::lua::luacats::Args<($($ty,)*), Names>|
                    -> mlua::Result<$out> {
                let run = || -> mlua::Result<$out> { $body };
                run().map_err(|err| $crate::lua::location::located($l, err))
            })?
        }
    };
    (@value $lua:expr; $path:expr; (value $value:expr); $($name:ident: $ty:ty),*) => {
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

/// A method name as Lua spells it: `r#move` for a Lua name that is a Rust keyword.
pub(crate) const fn unraw(ident: &str) -> &str {
    match ident.as_bytes() {
        [b'r', b'#', ..] => ident.split_at(2).1,
        _ => ident,
    }
}

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
        $($(#[doc = $method_doc:literal])* fn $method:ident($l:ident, $this:ident $(, $(#[doc = $param_doc:literal])* $param:ident: $param_ty:ty)* $(,)?) $(-> $ret:ty)? $body:block)*
    }) => {
        impl mlua::UserData for $class {
            fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
                $(methods.add_method($crate::lua::luacats::unraw(stringify!($method)), |$l, $this, ($($param,)*): ($($param_ty,)*)| -> mlua::Result<$crate::lua::luacats::lua_class!(@ret $($ret)?)> { $body });)*
            }
        }

        impl $crate::lua::luacats::LuaType for $class {
            fn lua() -> String {
                stringify!($class).to_string()
            }
            #[cfg(test)]
            fn classes(out: &mut Vec<String>) {
                const METHODS: &[(&str, $crate::lua::luacats::Signature)] = &[$(($crate::lua::luacats::unraw(stringify!($method)), $crate::lua::luacats::Signature {
                    doc: concat!($($method_doc, "\n",)* ""),
                    params: &[$($crate::lua::luacats::param!(stringify!($param), [$($param_doc)*], $param_ty, <$param_ty as $crate::lua::luacats::LuaType>::lua)),*],
                    returns: &[$($crate::lua::luacats::param!("", [], $ret, <$ret as $crate::lua::luacats::LuaType>::lua))?],
                })),*];
                $crate::lua::luacats::class(out, stringify!($class), concat!($($doc, "\n",)* ""), "", METHODS);
            }
        }
    };
    (@ret) => { () };
    (@ret $ret:ty) => { $ret };
}
pub(crate) use lua_class;

/// A struct whose fields are the keys of a Lua table, and its stub from their types and `///`
/// blocks, which are Lua-facing words. `#[alias = "Name"]` is a table a parser reads, spelled inline
/// for a header's `---@alias Name {Name}` line: LuaLS checks it inside a union, where a class admits
/// any table, and it has no place for a field's words. `#[class = "Name"]` is one read as a
/// `---@class` whose fields carry their words. Both refuse an unknown key, and `KEYS` is what the
/// parser's `only_keys` accepts. Alias and class fields are read through `Input`, and each also
/// takes a signal, which `resolve_declared` reads. `#[shared = "Name"]` is an alias also handed
/// to Lua, so its fields are spelled without one. `#[record = "Name"]` is a class handed to Lua.
/// `key: T as S` changes only the Lua spelling.
macro_rules! lua_shape {
    ($(#[doc = $doc:literal])* #[alias = $name:literal] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct alias [$($doc)*] $name $($rest)*);
        $crate::lua::luacats::lua_shape!(@read $($rest)*);
    };
    ($(#[doc = $doc:literal])* #[shared = $name:literal] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct shared [$($doc)*] $name $($rest)*);
        $crate::lua::luacats::lua_shape!(@read $($rest)*);
    };
    ($(#[doc = $doc:literal])* #[class = $name:literal] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct class [$($doc)*] $name $($rest)*);
        $crate::lua::luacats::lua_shape!(@read $($rest)*);
    };
    ($(#[doc = $doc:literal])* #[record = $name:literal] $($rest:tt)*) => {
        $crate::lua::luacats::lua_shape!(@struct record [$($doc)*] $name $($rest)*);
    };
    (@struct $form:ident [$($doc:literal)*] $name:literal $(#[$attr:meta])* $vis:vis struct $ty:ident {
        $($(#[doc = $field_doc:literal])* $field_vis:vis $field:ident: $field_ty:ty $(as $lua:ty)?),+ $(,)?
    }) => {
        $(#[doc = $doc])* $(#[$attr])* $vis struct $ty { $($(#[doc = $field_doc])* $field_vis $field: $field_ty),+ }

        $crate::lua::luacats::lua_shape!(@$form $ty $($field)+);

        #[cfg(test)]
        impl $ty {
            pub(crate) fn lua_fields() -> Vec<$crate::lua::luacats::ShapeField> {
                vec![$((
                    stringify!($field),
                    <$crate::lua::luacats::lua_shape!(@lua $field_ty $(, $lua)?) as $crate::lua::luacats::LuaType>::OPTIONAL,
                    $crate::lua::luacats::field_spelling(
                        stringify!($form),
                        <$crate::lua::luacats::lua_shape!(@lua $field_ty $(, $lua)?) as $crate::lua::luacats::LuaType>::lua(),
                    ),
                    concat!($($field_doc, "\n",)* ""),
                )),+]
            }
        }

        impl $crate::lua::luacats::LuaType for $ty {
            fn lua() -> String {
                $name.to_string()
            }
            #[cfg(test)]
            fn classes(out: &mut Vec<String>) {
                $crate::lua::luacats::lua_shape!(@nested $form out $(($field_ty $(, $lua)?))+);
                let keys = Self::lua_fields();
                let stub = $crate::lua::luacats::shape_stub(stringify!($form), $name, concat!($($doc, "\n",)* ""), &keys);
                if !out.contains(&stub) {
                    out.push(stub);
                }
            }
        }
    };
    (@alias $ty:ident $($field:ident)+) => { $crate::lua::luacats::lua_shape!(@class $ty $($field)+); };
    (@shared $ty:ident $($field:ident)+) => { $crate::lua::luacats::lua_shape!(@class $ty $($field)+); };
    (@class $ty:ident $($field:ident)+) => {
        impl $ty {
            pub(crate) const KEYS: &'static [&'static str] = &[$(stringify!($field)),+];
        }
    };
    (@record $ty:ident $($field:ident)+) => {
        impl mlua::IntoLua for $ty {
            fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                let table = lua.create_table()?;
                $(table.set(stringify!($field), self.$field)?;)+
                Ok(mlua::Value::Table(table))
            }
        }
    };
    (@read $(#[$attr:meta])* $vis:vis struct $ty:ident {
        $($(#[doc = $field_doc:literal])* $field_vis:vis $field:ident: $field_ty:ty $(as $lua:ty)?),+ $(,)?
    }) => {
        impl $ty {
            /// `table`, refusing a key not in [`Self::KEYS`], each key read as its field's type.
            pub(crate) fn read(
                property: &str,
                table: &mlua::Table,
            ) -> Result<Self, $crate::layout::node::LayoutError> {
                $crate::layout::node::only_keys(property, table, Self::KEYS)?;
                Self::read_fields(property, table)
            }

            /// For a table that mixes shape fields with its own keys, such as `animate.exit`.
            pub(crate) fn read_fields(
                property: &str,
                table: &mlua::Table,
            ) -> Result<Self, $crate::layout::node::LayoutError> {
                Ok(Self { $($field: $crate::layout::node::input::field::<$field_ty>(property, table, stringify!($field))?),+ })
            }
        }
        impl $crate::layout::node::input::Input for $ty {
            fn from_value(
                property: &str,
                key: &str,
                value: &mlua::Value,
            ) -> Result<Option<Self>, $crate::layout::node::LayoutError> {
                match value {
                    mlua::Value::Table(table) => Self::read(key, table).map(Some)
                        .map_err(|error| error.under(&format!("{property}."))),
                    _ => Ok(None),
                }
            }
        }
    };
    // A record's nested records are declared before it; an alias spells its fields inline.
    (@nested record $out:ident $(($ty:ty $(, $lua:ty)?))+) => {
        $(<$crate::lua::luacats::lua_shape!(@lua $ty $(, $lua)?) as $crate::lua::luacats::LuaType>::classes($out);)+
    };
    (@nested $other:ident $out:ident $($rest:tt)*) => {};
    (@lua $ty:ty) => { $ty };
    (@lua $ty:ty, $lua:ty) => { $lua };
}
pub(crate) use lua_shape;

#[cfg(test)]
pub(crate) type ShapeField = (&'static str, bool, String, &'static str);

/// A shape field's type, `|Bound` where a property table's field takes a signal.
#[cfg(test)]
pub(crate) fn field_spelling(form: &str, ty: String) -> String {
    if matches!(form, "alias" | "class") { format!("{ty}|Bound") } else { ty }
}

/// A `///` block's first paragraph on one line after a space, then each later line as its own `---`
/// line.
#[cfg(test)]
fn paragraphs(doc: &str) -> (String, String) {
    let (first, rest) = doc.split_once("\n\n").unwrap_or((doc, ""));
    let words = one_line(first);
    let words = if words.is_empty() { words } else { format!(" {words}") };
    (words, rest.lines().map(|line| format!("---{line}\n")).collect())
}

/// A [`lua_shape!`]'s stub, from each key's name, `Option`-ness, type and `///` block: an alias's
/// `{ key: T, ... } words`, or a class's doc and `---@class` of `---@field`s. An alias or a class
/// declares an unknown key a type error, as the parser refuses it.
#[cfg(test)]
pub(crate) fn shape_stub(form: &str, name: &str, doc: &str, keys: &[(&str, bool, String, &str)]) -> String {
    const UNKNOWN: &str = r#""no such property""#;
    let (words, more) = paragraphs(doc);
    if matches!(form, "alias" | "shared") {
        let keys: String = keys
            .iter()
            .map(|(key, nil, ty, doc)| {
                assert!(doc.is_empty(), "alias {name} has no place for `{key}`'s words");
                format!("{}: {ty}, ", optional(key.to_string(), *nil))
            })
            .collect();
        return format!("{{ {keys}[string]: {UNKNOWN} }}{words}\n{more}");
    }
    let mut out = format!("{}---@class {name}\n", comment(doc));
    for (key, nil, ty, doc) in keys {
        let (words, more) = paragraphs(doc);
        out += &format!("---@field {} {ty}{words}\n{more}", optional(key.to_string(), *nil));
    }
    if form == "class" {
        out += &format!("---@field [string] {UNKNOWN}\n");
    }
    out
}

/// Pushes a `---@class` block, its `fields`, then for a handle `local Name = {}` and one stub per
/// method, unless `out` already holds it.
#[cfg(test)]
pub(crate) fn class(out: &mut Vec<String>, name: &str, doc: &str, fields: &str, methods: &[(&str, Signature)]) {
    let mut class = format!("---@class {name}\n{}{fields}", comment(doc));
    if !methods.is_empty() {
        class += &format!("local {name} = {{}}\n");
    }
    for (_, signature) in methods {
        signature.classes(out);
    }
    for (method, signature) in methods {
        class += &format!("\n{}function {name}:{method}({}) end\n", signature.stub(), signature.names());
    }
    if !out.contains(&class) {
        out.push(class);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[allow(clippy::arc_with_non_send_sync)] // mlua's `BadArgument` demands an `Arc`.
    fn refused(from: &'static str, to: &str, message: Option<&str>) -> String {
        let cause = mlua::Error::FromLuaConversionError { from, to: to.into(), message: message.map(Into::into) };
        let err = mlua::Error::BadArgument { to: None, pos: 1, name: None, cause: Arc::new(cause) };
        bad_argument("f", &["x"], err).to_string()
    }

    #[test]
    fn a_refused_conversion_keeps_mlua_s_cause_and_never_prints_a_rust_path() {
        assert!(
            refused("integer", "u8", Some("out of range"))
                .ends_with("(x): expected a non-negative integer, got integer (out of range)")
        );
        assert!(
            refused("table", "alloc::vec::Vec<mantle::Thing>", None).ends_with("expected a valid value, got table")
        );
    }
}
