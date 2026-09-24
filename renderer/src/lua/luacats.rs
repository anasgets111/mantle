//! Rust types' LuaCATS spellings. What `lua-meta` declares for a node property's value, a global's
//! parameter or its return comes from the Rust type the engine reads or hands back, through
//! [`LuaType`]; only the stub tests call it.

use mlua::{AnyUserData, Function, LuaString, Table, Value, Variadic};

/// A Rust type as LuaCATS spells it.
pub(crate) trait LuaType {
    /// The LuaCATS type, `""` for none (`()`).
    fn lua() -> String;
    /// `Option`: `name?` on a parameter, `T?` on a return.
    const OPTIONAL: bool = false;
    /// The `---@class` blocks this type needs declared before a stub uses it, as a handle's class
    /// before the function returning it.
    fn classes(_out: &mut Vec<String>) {}
}

macro_rules! spelled {
    ($lua:literal: $($ty:ty),+) => {
        $(impl LuaType for $ty {
            fn lua() -> String {
                $lua.to_string()
            }
        })+
    };
}

spelled!("boolean": bool);
spelled!("number": f32, f64);
spelled!("integer": i32, i64, u32, u64, usize);
spelled!("string": String, LuaString);
spelled!("any": Value);
spelled!("table": Table);
spelled!("function": Function);
spelled!("userdata": AnyUserData);
spelled!("": ());
spelled!("Rect": crate::text::snap::LogicalRect);
spelled!("{ x: number, y: number }": crate::layout::hit::LogicalPoint);
spelled!("Node": super::VirtualNode);

impl<T: LuaType> LuaType for Option<T> {
    fn lua() -> String {
        T::lua()
    }
    const OPTIONAL: bool = true;
    fn classes(out: &mut Vec<String>) {
        T::classes(out);
    }
}

impl<T: LuaType> LuaType for Vec<T> {
    fn lua() -> String {
        let item = T::lua();
        if item.contains('|') { format!("({item})[]") } else { format!("{item}[]") }
    }
    fn classes(out: &mut Vec<String>) {
        T::classes(out);
    }
}

/// `...`: the parameter's name is the ellipsis, the type its element's.
impl<T: LuaType> LuaType for Variadic<T> {
    fn lua() -> String {
        T::lua()
    }
}

/// A Lua function a global takes, whose signature the declaring macro spells.
pub(crate) struct Fun(pub Function);

/// Only reached for `OPTIONAL` and `classes`: the macro spells a [`Fun`]'s signature itself.
impl LuaType for Fun {
    fn lua() -> String {
        Function::lua()
    }
}

impl mlua::FromLua for Fun {
    fn from_lua(value: Value, lua: &mlua::Lua) -> mlua::Result<Self> {
        Function::from_lua(value, lua).map(Fun)
    }
}

/// `fun(name: T, ...): R`.
pub(crate) fn fun(params: &[(&str, Spelling)], ret: Option<(Spelling, bool)>) -> String {
    let params: Vec<String> = params.iter().map(|(name, ty)| format!("{name}: {}", ty())).collect();
    let ret = ret.map_or_else(String::new, |(ty, optional)| format!(": {}{}", ty(), if optional { "?" } else { "" }));
    format!("fun({}){ret}", params.join(", "))
}

/// [`LuaType::lua`] of some type.
pub(crate) type Spelling = fn() -> String;

/// A parameter or a return of a global or a method, as its declaring macro saw it.
pub(crate) struct Param {
    /// `""` for an unnamed return.
    pub name: &'static str,
    /// The `///` block above it.
    pub doc: &'static str,
    pub ty: Spelling,
    pub optional: bool,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the globals golden, a test"))]
    pub classes: fn(&mut Vec<String>),
}

/// A global function's or a method's `///` block and signature.
pub(crate) struct Signature {
    pub doc: &'static str,
    pub params: &'static [Param],
    pub returns: &'static [Param],
}

/// A `///` block's lines, trimmed of the space `///` leaves.
fn lines(doc: &str) -> impl Iterator<Item = &str> {
    doc.lines().map(|line| line.strip_prefix(' ').unwrap_or(line)).skip_while(|line| line.is_empty())
}

/// A `///` block joined onto one line, for a `---@param` or `---@return`.
fn one_line(doc: &str) -> String {
    lines(doc).map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
}

impl Signature {
    /// The classes its parameters and returns name, first seen first.
    #[cfg(test)]
    pub(crate) fn classes(&self, out: &mut Vec<String>) {
        for param in self.params.iter().chain(self.returns) {
            (param.classes)(out);
        }
    }

    /// The `---` block above the declaration: the doc's lines, then each `@param` and `@return`.
    pub(crate) fn stub(&self) -> String {
        let mut out: String = lines(self.doc).map(|line| format!("---{line}\n")).collect();
        let words = |doc: &str| match one_line(doc) {
            words if words.is_empty() => String::new(),
            words => format!(" {words}"),
        };
        for param in self.params {
            let name = if param.optional { format!("{}?", param.name) } else { param.name.to_string() };
            out += &format!("---@param {name} {}{}\n", (param.ty)(), words(param.doc));
        }
        for ret in self.returns {
            let ty = format!("{}{}", (ret.ty)(), if ret.optional { "?" } else { "" });
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
        $crate::lua::luacats::Param {
            name: $name,
            doc: concat!($($doc, "\n",)* ""),
            ty: $lua,
            optional: <$ty as $crate::lua::luacats::LuaType>::OPTIONAL,
            classes: <$ty as $crate::lua::luacats::LuaType>::classes,
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
/// `///` block may precede each.
macro_rules! lua_fn {
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?)
        -> ($($(#[doc = $ret_doc:literal])* $ret:ident: $ret_ty:ty),+ $(,)?) $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*); $l;
            [$($crate::lua::luacats::param!(stringify!($ret), [$($ret_doc)*], $ret_ty, <$ret_ty as $crate::lua::luacats::LuaType>::lua)),+];
            ($($ret_ty,)+); $body
        } [] $($($params)*)?)
    };
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?)
        -> $(#[doc = $ret_doc:literal])* $ret:ty $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*); $l;
            [$crate::lua::luacats::param!("", [$($ret_doc)*], $ret, <$ret as $crate::lua::luacats::LuaType>::lua)];
            $ret; $body
        } [] $($($params)*)?)
    };
    ($lua:expr, $(#[doc = $doc:literal])* fn $first:ident $(. $more:ident)* ($l:ident $(, $($params:tt)*)?) $body:block) => {
        $crate::lua::luacats::lua_fn!(@params {
            $lua; [$($doc)*]; concat!(stringify!($first) $(, ".", stringify!($more))*); $l; []; (); $body
        } [] $($($params)*)?)
    };
    (@params $ctx:tt [$($done:tt)*]) => {
        $crate::lua::luacats::lua_fn!(@emit $ctx $($done)*)
    };
    (@params $ctx:tt [$($done:tt)*] $(#[doc = $doc:literal])* $name:ident: fn($($arg:ident: $arg_ty:ty),*) $(-> $ret:ty)?
        $(, $($rest:tt)*)?) => {
        $crate::lua::luacats::lua_fn!(@params $ctx [$($done)* {
            $name; $crate::lua::luacats::Fun;
            $crate::lua::luacats::param!(stringify!($name), [$($doc)*], $crate::lua::luacats::Fun, || $crate::lua::luacats::fun(
                &[$((stringify!($arg), <$arg_ty as $crate::lua::luacats::LuaType>::lua)),*],
                $crate::lua::luacats::lua_fn!(@ret $($ret)?),
            ))
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
    (@emit { $lua:expr; [$($doc:literal)*]; $path:expr; $l:ident; [$($returns:expr),*]; $out:ty; $body:block }
        $({ $name:ident; $ty:ty; $param:expr })*) => {{
        const SIGNATURE: $crate::lua::luacats::Signature = $crate::lua::luacats::Signature {
            doc: concat!($($doc, "\n",)* ""),
            params: &[$($param),*],
            returns: &[$($returns),*],
        };
        $crate::lua::define(
            $lua,
            $path,
            &SIGNATURE,
            $lua.create_function(move |$l, ($($name,)*): ($($ty,)*)| -> mlua::Result<$out> { $body })?,
        )
    }};
}
pub(crate) use lua_fn;

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
            fn classes(out: &mut Vec<String>) {
                const METHODS: &[(&str, $crate::lua::luacats::Signature)] = &[$((stringify!($method), $crate::lua::luacats::Signature {
                    doc: concat!($($method_doc, "\n",)* ""),
                    params: &[$($crate::lua::luacats::param!(stringify!($param), [$($param_doc)*], $param_ty, <$param_ty as $crate::lua::luacats::LuaType>::lua)),*],
                    returns: &[],
                })),*];
                let class = $crate::lua::luacats::class(stringify!($class), concat!($($doc, "\n",)* ""), METHODS);
                if !out.contains(&class) {
                    out.push(class);
                }
            }
        }
    };
}
pub(crate) use lua_class;

/// A handle's `---@class` block: `local Name = {}` and one stub per method.
pub(crate) fn class(name: &str, doc: &str, methods: &[(&str, Signature)]) -> String {
    let mut out = format!("---@class {name}\n");
    out += &lines(doc).map(|line| format!("---{line}\n")).collect::<String>();
    out += &format!("local {name} = {{}}\n");
    for (method, signature) in methods {
        out += &format!("\n{}function {name}:{method}({}) end\n", signature.stub(), signature.names());
    }
    out
}
