//! Proves `push_str` never hands plaintext back to the allocator unscrubbed when its
//! internal storage has to grow (ADR-0005, ADR-0014).
//!
//! `push_str` is documented as being called once per edit diff, not once per keystroke --
//! but that still means multiple calls per secret, each one potentially needing more
//! capacity than the buffer currently has. If growth were left to `Vec`'s own reallocation,
//! each grow would allocate a new block, copy the existing bytes over, and free the old
//! block without zeroing it first: a plaintext prefix of the secret sitting in a freed
//! block that neither the later `.zeroize()` call nor `Drop` ever touches again, since both
//! only ever scrub the *current* backing allocation.
//!
//! Same allocator-hook technique as `secure_buffer_drop_zeroizes.rs`, but scanning every
//! block handed to `dealloc` (not just one watched pointer) for a plaintext prefix of the
//! secret, since growth can free several intermediate blocks over the course of building
//! up one secret.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use shared::SecureBuffer;

/// Deliberately distinctive so a match in freed memory can only be this test's own data,
/// not an incidental collision with unrelated allocations elsewhere in the process.
const SECRET: &str = "Tr0ub4dor&3-zK9qLpXwRt-hunter2exposed-3F7dM1sQ";

/// Shortest prefix length worth flagging. Below this, a chance collision in unrelated
/// heap data becomes plausible; at and above it, only this test's own secret should match.
const MIN_LEAK_LEN: usize = 6;

static WATCHING: AtomicBool = AtomicBool::new(false);
static LEAK_FOUND: AtomicBool = AtomicBool::new(false);
/// One watch window at a time: the flags above are process-wide and tests run in parallel.
static SERIAL: Mutex<()> = Mutex::new(());

struct LeakCheckingAllocator;

// SAFETY: `alloc`/`dealloc` delegate every allocation to `System`, adding only a read of memory
// that is still live. Blocks come zeroed, so every byte `dealloc` reads is initialized even where
// the program never wrote one. The `GlobalAlloc` contract -- returning correctly aligned blocks for the
// requested layout, and freeing only what it handed out -- is `System`'s, unchanged.
unsafe impl GlobalAlloc for LeakCheckingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is whatever the caller asked for and is forwarded untouched, which
        // is exactly what `System`'s own `alloc_zeroed` requires.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCHING.load(Ordering::SeqCst) && !ptr.is_null() && layout.size() > 0 {
            // Safety: `ptr` is valid for `layout.size()` bytes until this call returns it
            // to the allocator -- this read happens before that handback completes.
            let freed = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            if contains_secret_prefix(freed) {
                LEAK_FOUND.store(true, Ordering::SeqCst);
            }
        }
        // SAFETY: `ptr`/`layout` are the pair the caller received from `alloc` above and are
        // forwarded unchanged; the reads before this point do not alter either.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// True if `haystack` contains any prefix of `SECRET` at least `MIN_LEAK_LEN` bytes long.
/// `push_str` is called one character at a time below, so any leaked intermediate block
/// would hold exactly a prefix of the secret, not an arbitrary substring.
fn contains_secret_prefix(haystack: &[u8]) -> bool {
    let secret = SECRET.as_bytes();
    (MIN_LEAK_LEN..=secret.len())
        .map(|len| &secret[..len])
        .any(|prefix| haystack.windows(prefix.len()).any(|window| window == prefix))
}

#[global_allocator]
static ALLOCATOR: LeakCheckingAllocator = LeakCheckingAllocator;

#[test]
fn push_str_growth_never_leaks_plaintext_to_freed_memory() {
    let _serial = SERIAL.lock().unwrap();
    LEAK_FOUND.store(false, Ordering::SeqCst);
    let mut buf = SecureBuffer::new();

    WATCHING.store(true, Ordering::SeqCst);
    for ch in SECRET.chars() {
        // One character at a time: the realistic pattern per the module's own doc
        // comment, and the pattern that forces repeated internal growth.
        buf.push_str(&ch.to_string());
    }
    WATCHING.store(false, Ordering::SeqCst);

    assert_eq!(buf.expose_secret(), SECRET.as_bytes());
    assert!(
        !LEAK_FOUND.load(Ordering::SeqCst),
        "a plaintext prefix of the secret was found in memory freed during push_str's internal growth"
    );

    drop(buf);
}

/// A decoded secret grows the same way, and a malformed one must not drop its partial copy
/// unscrubbed either.
#[test]
fn decoding_a_secret_never_leaks_plaintext_to_freed_memory() {
    let _serial = SERIAL.lock().unwrap();
    LEAK_FOUND.store(false, Ordering::SeqCst);
    let numbers: Vec<String> = SECRET.bytes().map(|byte| byte.to_string()).collect();
    let valid = format!(r#"{{"Response":{{"secret":[{}]}}}}"#, numbers.join(","));
    let malformed = format!(r#"{{"Response":{{"secret":[{},"x"]}}}}"#, numbers.join(","));

    WATCHING.store(true, Ordering::SeqCst);
    let decoded: shared::PamMessage = serde_json::from_str(&valid).unwrap();
    assert!(serde_json::from_str::<shared::PamMessage>(&malformed).is_err());
    WATCHING.store(false, Ordering::SeqCst);

    let mut decoded = decoded;
    assert_eq!(decoded, shared::PamMessage::Response { secret: SECRET.as_bytes().to_vec() });
    zeroize::Zeroize::zeroize(&mut decoded);
    assert!(!LEAK_FOUND.load(Ordering::SeqCst), "decoding freed a block holding a plaintext prefix");
}
