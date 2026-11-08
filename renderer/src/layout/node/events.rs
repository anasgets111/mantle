//! Input values shared by node properties and platform event delivery.

use mlua::{IntoLua, Lua, Value};

use super::prop::keywords;
use crate::lua::luacats::spelled;

keywords! {
    /// What Escape does in a plain `textfield`; a `secure_submit` field always scrubs and stays armed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Escape {
        /// Empty the draft, then leave the field if `on_cancel` is set.
        Clear,
        /// Keep the draft and leave the field.
        Blur,
        /// Keep the draft and the focus; the key goes up through `on_key` and `on_escape`.
        Pass,
    }
}

keywords! {
    /// The chord that submits a `multiline` field; the other Return inserts a newline.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SubmitKey {
        /// Ctrl+Return submits; Return inserts a newline.
        CtrlReturn = "ctrl+return",
        /// Return submits; Shift+Return inserts a newline.
        Return,
    }
}

keywords! {
    /// The mouse buttons `on_click` reports.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum MouseButton {
        Left,
        Right,
        Middle,
        Back,
        Forward,
    }
}

keywords! {
    /// Where an `on_drag` call falls in its drag (ADR-0116).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DragPhase {
        Start,
        Move,
        End,
    }
}

/// One key press as `on_key` receives it: `---@class KeyPress` in `surfaces.lua`.
pub(crate) struct KeyPress {
    pub(crate) name: String,
    pub(crate) text: Option<String>,
    /// `(ctrl, shift, alt, super)`.
    pub(crate) modifiers: [bool; 4],
    /// `None` for a release, which has no `repeat` field.
    pub(crate) repeat: Option<bool>,
}

/// One key release as `on_key_release` receives it: a [`KeyPress`] without `repeat`.
pub(crate) struct KeyRelease(pub(crate) KeyPress);

spelled!(KeyPress => "KeyPress");
spelled!(KeyRelease => "KeyRelease");

impl IntoLua for KeyPress {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        let table = lua.create_table()?;
        table.set("name", self.name)?;
        table.set("text", self.text)?;
        table.set("modifiers", crate::lua::marshal::modifiers_table(lua, self.modifiers)?)?;
        if let Some(repeat) = self.repeat {
            table.set("repeat", repeat)?;
        }
        Ok(Value::Table(table))
    }
}

impl IntoLua for KeyRelease {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        self.0.into_lua(lua)
    }
}
