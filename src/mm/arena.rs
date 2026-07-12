//! 256 KiB arena matching CPython's `obmalloc` (arenas are 256 KiB).
//!
//! Bump-allocate with a free-list for holes from `munmap`. Each arena is
//! embedded in a contiguous slice of guest linear memory.
//!
//! Full implementation lands in Step 6.

pub const ARENA_SIZE: usize = 256 * 1024; // MUST match CPython's obmalloc
