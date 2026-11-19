//! A bounds check that keeps a peer's pod away from libspa 0.10.1's `PodDeserializer` until the
//! sizes it declares agree with its bytes. That deserializer trusts them: a Choice with child size
//! 0 divides by zero, a String, Array, Object or Pointer under its minimum size underflows, a
//! Pointer that is not 4 or 8 bytes hits `panic!`, an Array's length sizes `Vec::with_capacity`
//! before any element is read, a fixed-size pod is read at its type's size whatever it declares, so
//! a Struct or Object whose children overrun its declared body underflows its remaining count, and
//! nesting recurses without a limit.
//!
//! [`readable`] walks a pod the way `deserialize_any` reads it and refuses each of those shapes at
//! any depth. Everything it admits tiles exactly: every child ends inside its parent.

use pipewire::spa::sys as spa_sys;

/// Real `Props`, `Route` and `EnumRoute` objects nest four deep at most.
/// ponytail: 16 levels bound a recursion libspa leaves unbounded; upgrade: drop this walk once
/// libspa returns errors at its panic sites.
const MAX_DEPTH: u8 = 16;

/// Whether `PodDeserializer::deserialize_any` can read `bytes` without panicking.
pub(super) fn readable(bytes: &[u8]) -> bool {
    pod(bytes, 0).is_some()
}

/// The bytes the deserializer consumes for the pod at the start of `bytes`, padding included.
fn pod(bytes: &[u8], depth: u8) -> Option<usize> {
    let size = word(bytes, 0)?;
    let body = bytes.get(8..8 + size)?;
    let fits = match word(bytes, 4)? as u32 {
        spa_sys::SPA_TYPE_String => size > 0,
        spa_sys::SPA_TYPE_Bytes => true,
        spa_sys::SPA_TYPE_Pointer => size == 12 || size == 16,
        spa_sys::SPA_TYPE_Array => size >= 8 && elements(word(body, 4)?, word(body, 0)?, size - 8),
        spa_sys::SPA_TYPE_Choice => {
            size >= 16 && word(body, 8)? > 0 && elements(word(body, 12)?, word(body, 8)?, size - 16)
        }
        spa_sys::SPA_TYPE_Struct => depth < MAX_DEPTH && tiles(body, depth, 0),
        spa_sys::SPA_TYPE_Object => depth < MAX_DEPTH && body.get(8..).is_some_and(|props| tiles(props, depth, 8)),
        kind => fixed_size(kind) == Some(size),
    };
    fits.then_some(8 + size.next_multiple_of(8))
}

/// Whether `body` is a run of pods, each after a `prefix` of bytes, ending exactly at its end.
fn tiles(body: &[u8], depth: u8, prefix: usize) -> bool {
    let mut at = 0;
    while at < body.len() {
        let Some(len) = body.get(at + prefix..).and_then(|rest| pod(rest, depth + 1)) else { return false };
        at += prefix + len;
    }
    at == body.len()
}

/// Whether an Array's or Choice's `len` bytes of values are whole elements the deserializer reads
/// at `child_size` each (none at size 0); it refuses a child type it cannot read anyway.
fn elements(child_type: usize, child_size: usize, len: usize) -> bool {
    fixed_size(child_type as u32) == Some(child_size) && len.is_multiple_of(child_size)
}

/// The size `deserialize_fixed_sized_pod` reads for `kind`, whatever the pod declares.
fn fixed_size(kind: u32) -> Option<usize> {
    match kind {
        spa_sys::SPA_TYPE_None => Some(0),
        spa_sys::SPA_TYPE_Bool | spa_sys::SPA_TYPE_Id | spa_sys::SPA_TYPE_Int | spa_sys::SPA_TYPE_Float => Some(4),
        spa_sys::SPA_TYPE_Long
        | spa_sys::SPA_TYPE_Double
        | spa_sys::SPA_TYPE_Rectangle
        | spa_sys::SPA_TYPE_Fraction
        | spa_sys::SPA_TYPE_Fd => Some(8),
        _ => None,
    }
}

fn word(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
}
