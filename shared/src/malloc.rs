/// glibc's arena totals from `mallinfo2`, in bytes. Both processes report it under `--profile`:
/// `smaps` says how much a process holds, only the in-use/free split says whether it is live.
/// A growing `in_use` is a leak; a growing `free` under a flat `in_use` is glibc holding freed
/// chunks a `malloc_trim` could return.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Malloc {
    /// `arena`: bytes taken from the kernel via `brk`, across every per-thread arena.
    pub arena: u64,
    /// `hblkhd`: bytes in `mmap`ed blocks, which allocations past `M_MMAP_THRESHOLD` take instead.
    pub mmapped: u64,
    /// `uordblks`: bytes handed out and not yet freed.
    pub in_use: u64,
    /// `fordblks`: bytes on glibc's free lists, still charged to the process until a trim.
    pub free: u64,
}

impl Malloc {
    /// Reads every arena's totals, or zeroes on a platform without `mallinfo2`.
    #[cfg(target_env = "gnu")]
    pub fn now() -> Self {
        // SAFETY: plain FFI returning a POD struct by value. `mallinfo2` takes no arguments, locks
        // the arenas itself, and only reads counters.
        let info = unsafe { libc::mallinfo2() };
        Self {
            arena: info.arena as u64,
            mmapped: info.hblkhd as u64,
            in_use: info.uordblks as u64,
            free: info.fordblks as u64,
        }
    }

    #[cfg(not(target_env = "gnu"))]
    pub fn now() -> Self {
        Self::default()
    }
}
