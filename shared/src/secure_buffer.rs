//! Native buffer for typed secrets that must not enter the Lua VM heap (ADR-0005). For a focused
//! `textfield` `secure_submit`, the Renderer reads the keyboard and pushes bytes here, bypassing
//! Lua strings and input methods.
//!
//! ADR-0005 requires callers to call `.zeroize()` immediately after the one sanctioned read
//! (`expose_secret` into an outgoing IPC envelope), because `Drop` may be delayed by an early
//! return. `ZeroizeOnDrop` backs that up, and only that: release is `panic = "abort"`, so on a
//! panic no destructor runs and the explicit call is the only scrub.

use unicode_segmentation::UnicodeSegmentation;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Growable bytes whose full backing allocation is zeroized explicitly and again on `Drop`
/// (ADR-0005).
#[derive(Default, Zeroize, ZeroizeOnDrop)]
pub struct SecureBuffer {
    bytes: Vec<u8>,
}

impl SecureBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends UTF-8 bytes. Called once per `commit_string` edit diff (input method, ADR-0009) and
    /// once per lock-screen keystroke.
    ///
    /// Growth is manual because `Vec` reallocates by freeing the old block without zeroing it,
    /// leaving a plaintext prefix beyond the current allocation that later `.zeroize()`/`Drop`
    /// cannot reach. Copy into new storage, zeroize the old block, then drop it.
    pub fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    /// [`Self::push_str`] for callers holding bytes rather than a `str`, which is what
    /// `shared::framing`'s serializer sink has. The growth rule lives here so there is one
    /// implementation of it: a second copy elsewhere is a second place to get it wrong.
    ///
    /// Grows by doubling rather than to exactly `needed`, because a serializer appends in many
    /// small writes and growing per write would copy-and-scrub on each one.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let mut grown = Vec::with_capacity(needed.max(self.bytes.capacity() * 2));
            grown.extend_from_slice(&self.bytes);
            // `Vec<u8>: Zeroize` clears in place without reallocating, so this cannot repeat the
            // reallocation bug guarded against here.
            let mut old = std::mem::replace(&mut self.bytes, grown);
            old.zeroize();
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Backspace on `secure_submit`: zeroizes the last grapheme cluster in place before shortening
    /// the length.
    ///
    /// `Vec::truncate` would leave deleted bytes live while the user keeps typing; the submit's
    /// later `.zeroize()` is too late because a lock screen holds this buffer through corrections
    /// (ADR-0005).
    ///
    /// Delete what the user sees as one character (ADR-0236): an `e` and its combining acute are
    /// two scalars and one keystroke, and `expose_secret` sends bytes straight into an IPC envelope
    /// with no second decode, so a cut anywhere but a cluster boundary would reach the wire.
    ///
    /// Returns `false` for Backspace on an empty field. [`Self::push_bytes`] can hold non-UTF-8
    /// for a serializer sink, but only keystrokes are ever deleted, and those arrive as `str`.
    pub fn pop_grapheme(&mut self) -> bool {
        let Some((start, _)) = self.text().and_then(|text| text.grapheme_indices(true).next_back()) else {
            return false;
        };
        // `truncate` neither reallocates nor frees, so scrubbed bytes stay in this allocation.
        self.bytes[start..].zeroize();
        self.bytes.truncate(start);
        true
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Number of typed characters, so a masked field draws one glyph per character.
    ///
    /// Clusters, not [`Self::len`]'s bytes and not scalars: the count has to move by exactly one
    /// per keystroke, and [`Self::pop_grapheme`] deletes by cluster. This is the only read besides
    /// `expose_secret`; it discloses only the length already shown by the dots.
    pub fn grapheme_count(&self) -> usize {
        self.text().map_or(0, |text| text.graphemes(true).count())
    }

    /// The buffer as text, for the two reads that need cluster boundaries. `None` for bytes a
    /// serializer sink pushed, which no masked field ever draws or deletes.
    fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// The one sanctioned trust-boundary read, for serialization into an outgoing IPC envelope.
    /// Callers must call `.zeroize()` immediately after (ADR-0005), not rely on `Drop` alone.
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn growth_scrubs_the_block_it_leaves_behind() {
        // The trap this exists for: a plain `Vec` frees the old block with the secret still in it,
        // beyond the reach of zeroizing the final buffer.
        let mut buffer = SecureBuffer::new();
        buffer.push_bytes(b"secret");
        let first_block = buffer.expose_secret().as_ptr();
        buffer.push_bytes(&[b'x'; 4096]);
        assert_ne!(
            buffer.expose_secret().as_ptr(),
            first_block,
            "the append must have forced a reallocation for this to be testing anything"
        );
        assert!(buffer.expose_secret().starts_with(b"secretxxx"), "growth must preserve what was already there");
    }

    #[test]
    fn push_str_and_push_bytes_are_the_same_append() {
        let mut from_str = SecureBuffer::new();
        from_str.push_str("hello");
        let mut from_bytes = SecureBuffer::new();
        from_bytes.push_bytes(b"hello");
        assert_eq!(from_str.expose_secret(), from_bytes.expose_secret());
    }
    use super::*;

    #[test]
    fn grapheme_count_counts_characters_not_bytes() {
        let mut buf = SecureBuffer::new();
        buf.push_str("pa\u{00df}w\u{00f6}rd");
        assert_eq!(buf.len(), 9, "two of these seven characters are two bytes each");
        assert_eq!(buf.grapheme_count(), 7, "a masked field must draw one dot per keystroke, not per byte");
    }

    #[test]
    fn grapheme_count_follows_a_backspace() {
        let mut buf = SecureBuffer::new();
        buf.push_str("ab\u{00e9}");
        assert_eq!(buf.grapheme_count(), 3);
        assert!(buf.pop_grapheme());
        assert_eq!(buf.grapheme_count(), 2, "deleting one multi-byte character removes exactly one dot");
    }

    #[test]
    fn an_empty_buffer_has_no_characters() {
        assert_eq!(SecureBuffer::new().grapheme_count(), 0);
    }

    #[test]
    fn push_str_is_readable_via_expose_secret() {
        let mut buf = SecureBuffer::new();
        buf.push_str("hunter2");
        assert_eq!(buf.expose_secret(), b"hunter2");
        assert_eq!(buf.len(), 7);
        assert!(!buf.is_empty());
    }

    #[test]
    fn new_buffer_is_empty() {
        let buf = SecureBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.expose_secret(), b"");
    }

    /// After `.zeroize()`, the entire backing allocation, including bytes past `len()` that
    /// `.clear()` would leave on the heap, reads as zero. Reading through `capacity()` is defined
    /// while the Vec still owns the allocation, unlike reading after drop (see
    /// `shared/tests/secure_buffer_drop_zeroizes.rs`).
    #[test]
    fn explicit_zeroize_clears_the_full_backing_allocation() {
        let mut buf = SecureBuffer::new();
        buf.push_str("correct horse battery staple");
        let capacity = buf.bytes.capacity();
        assert!(capacity > 0);

        buf.zeroize();

        assert!(buf.is_empty());
        assert_eq!(buf.expose_secret(), b"");
        // SAFETY: `zeroize` clears but does not deallocate, so the whole capacity is live and
        // initialised. Read after the mutation, not before: `Vec::as_ptr` is invalidated by a
        // later `&mut` to the buffer, and `zeroize` takes one.
        let backing = unsafe { std::slice::from_raw_parts(buf.bytes.as_ptr(), capacity) };
        assert!(backing.iter().all(|&b| b == 0), "backing allocation was not fully zeroed");
    }

    /// A lock screen keeps the buffer live while typing, so deleted characters must not remain
    /// readable from the heap. This is why [`SecureBuffer::pop_grapheme`] is not bare `truncate`.
    #[test]
    fn pop_grapheme_zeroizes_the_bytes_it_removes() {
        let mut buf = SecureBuffer::new();
        buf.push_str("hunter2");
        assert!(buf.pop_grapheme());

        assert_eq!(buf.expose_secret(), b"hunter");
        // SAFETY: `pop_grapheme` truncates without deallocating, so the byte past the new length is
        // live and initialised. Read after the mutation, for the reason above.
        assert_eq!(unsafe { *buf.bytes.as_ptr().add(6) }, 0, "the removed byte was left in the backing allocation");
    }

    /// A multi-byte character is one Backspace, not one byte; `expose_secret` sends it straight to
    /// the wire without a second decode to catch a split scalar (ADR-0005).
    #[test]
    fn pop_grapheme_removes_a_whole_character_and_reports_an_empty_buffer() {
        let mut buf = SecureBuffer::new();
        buf.push_str("a\u{e9}");
        assert_eq!(buf.len(), 3);

        assert!(buf.pop_grapheme());
        assert_eq!(buf.expose_secret(), b"a");

        assert!(buf.pop_grapheme());
        assert!(buf.is_empty());
        assert!(!buf.pop_grapheme());
    }

    /// One keystroke, one deletion, whatever the typist composed: a base letter plus its combining
    /// mark is two scalars, and a flag is two. Scalar-wise deletion left the acute behind on a
    /// stripped `e` and half a flag (ADR-0236).
    #[test]
    fn pop_grapheme_deletes_a_composed_character_whole() {
        let mut buf = SecureBuffer::new();
        buf.push_str("e\u{301}\u{1F1E9}\u{1F1EA}");
        assert_eq!(buf.grapheme_count(), 2);

        assert!(buf.pop_grapheme());
        assert_eq!(buf.expose_secret(), "e\u{301}".as_bytes(), "the flag's two regional indicators go together");

        assert!(buf.pop_grapheme());
        assert!(buf.is_empty(), "the combining acute leaves with the letter it sits on");
    }
}
