//! Viewport scroll signal methods, requests, and coordinate validation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::Value;

use super::{DirtyFlag, LiveSignalHandle, Signal, SignalKind, next_cell_id};

/// A move Lua asks of a `scroll` signal's containers; the pass turns it into an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollRequest {
    /// The least move that shows this 1-based visible child.
    Reveal(usize),
    To(f32),
    /// Added to the run's target, like a wheel notch, so repeated calls mid-run accumulate.
    By(f32),
}

/// `offset` as a pixel request for `method`, refusing NaN and infinity.
pub(super) fn finite(method: &str, offset: f64) -> mlua::Result<f32> {
    let pixels = offset as f32;
    if !pixels.is_finite() {
        return Err(mlua::Error::runtime(format!(
            "signal:{method}(): offset must be finite, got {}",
            crate::lua::marshal::number_word(offset)
        )));
    }
    Ok(pixels)
}

impl Signal {
    /// Scroll offset starting at top (ADR-0069 decision 2). Plain number, not a hover-like pair: no
    /// scrollbar uses content extent yet, so the first such config can define its shape.
    pub(super) fn new_scroll(dirty: DirtyFlag) -> Self {
        Signal(SignalKind::Scroll {
            id: next_cell_id(),
            cell: Rc::new(RefCell::new(Value::Number(0.0))),
            dirty,
            request: Rc::new(Cell::new(None)),
        })
    }

    /// Asks the next positioning pass for `asked`, marking dirty (ADR-0112). The last request
    /// wins, except that a `By` adds to a pending `To` or `By`. Other kinds return false for the
    /// method's named refusal.
    fn request_scroll(&self, asked: ScrollRequest) -> bool {
        let SignalKind::Scroll { id, request, dirty, .. } = &self.0 else { return false };
        request.set(Some(match (request.get(), asked) {
            (Some(ScrollRequest::To(to)), ScrollRequest::By(by)) => ScrollRequest::To(to + by),
            (Some(ScrollRequest::By(first)), ScrollRequest::By(by)) => ScrollRequest::By(first + by),
            _ => asked,
        }));
        dirty.note_scroll_request(*id, request);
        dirty.mark_cell(*id);
        true
    }

    /// The request waiting for the next positioning pass, left in place.
    pub(crate) fn pending_scroll(&self) -> Option<ScrollRequest> {
        if let SignalKind::Scroll { request, .. } = &self.0 { request.get() } else { None }
    }

    /// Consumes the request in `layout::scene`'s positioning pass, so a later wheel event does not
    /// fight an already honored one.
    pub(crate) fn take_scroll(&self) -> Option<ScrollRequest> {
        if let SignalKind::Scroll { request, .. } = &self.0 { request.take() } else { None }
    }

    pub(super) fn scroll_request(&self, method: &str, asked: ScrollRequest) -> mlua::Result<()> {
        if self.request_scroll(asked) {
            return Ok(());
        }
        Err(mlua::Error::runtime(format!(
            "signal:{method}() is only valid on a scroll(name) signal, and this is {} signal",
            self.0.describe()
        )))
    }

    /// Scroll write end for wheel and positioning clamp; `None` for other kinds keeps wheels off
    /// capability signals.
    pub(crate) fn scroll_handle(&self) -> Option<LiveSignalHandle> {
        let SignalKind::Scroll { id, cell, dirty, .. } = &self.0 else { return None };
        Some(LiveSignalHandle(*id, Rc::clone(cell), dirty.clone()))
    }

    /// Scroll offset without `Lua`: `layout::scene` clamps deep in a pass holding no VM reference,
    /// and threading one through every layout frame just to read a `RefCell` would add a parameter.
    pub(crate) fn scroll_offset(&self) -> Option<f32> {
        let SignalKind::Scroll { cell, .. } = &self.0 else { return None };
        Some(match *cell.borrow() {
            Value::Number(n) => n as f32,
            Value::Integer(n) => n as f32,
            _ => 0.0,
        })
    }
}
