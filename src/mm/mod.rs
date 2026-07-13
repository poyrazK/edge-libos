//! Anonymous-mmap-over-linear-memory allocator.
//!
//! The spec's "single most important non-obvious piece" (§1.2): since Wasm
//! has no virtual address space, `mmap(MAP_ANONYMOUS)` cannot create new
//! address space. Instead, the host reserves high regions of linear memory
//! and carves allocations out of them, growing via `Memory::grow` as needed.
//!
//! P0 mirrors CPython's obmalloc: 256 KiB arenas, bump + free-list inside
//! each arena.
//!
//! ## Why `mmap` is sync (not async) and takes pre-zeroed data
//!
//! The borrow checker makes it very hard to call `Memory::grow` (which needs
//! `&mut Store`) while simultaneously borrowing `Kernel::mm` (which lives
//! behind `Caller::data_mut()`). The clean solution: separate the *grow*
//! concern from the *placement* concern. `mmap` is a pure decision that
//! returns either an address or a "you need to grow by N pages first"
//! signal. The caller grows, then calls `mmap` again.

use wasmtime::AsContext;

pub mod arena;

pub use arena::{Arena, ARENA_SIZE};

const PAGE_SIZE: u32 = 64 * 1024; // Wasm default page size (1<<16)

/// Linux mmap flags we honour.
pub const MAP_ANONYMOUS: i32 = 0x20;
pub const MAP_PRIVATE: i32 = 0x02;
#[allow(dead_code)]
pub const MAP_FIXED: i32 = 0x10;

/// Linux mmap prot bits.
pub const PROT_READ: i32 = 0x1;
pub const PROT_WRITE: i32 = 0x2;
pub const PROT_EXEC: i32 = 0x4;
#[allow(dead_code)]
pub const PROT_NONE: i32 = 0x0;

/// The orchestrator. Holds the arena list and high-water mark.
pub struct LinearAllocator {
    arenas: Vec<Arena>,
    high_water: u32,
}

/// Result of a (possibly split) mmap decision.
#[derive(Debug)]
pub enum MmapResult {
    /// Allocation succeeded; `addr` is the absolute offset.
    Ok(u32),
    /// Need to grow linear memory by this many pages, then call `mmap` again.
    NeedGrow(u64),
    /// Invalid args.
    Err(i64),
}

impl LinearAllocator {
    pub const FIRST_ARENA_BASE: u32 = 0x0010_0000;

    pub fn new() -> Self {
        Self {
            arenas: Vec::new(),
            high_water: Self::FIRST_ARENA_BASE,
        }
    }

    /// Decide where to place an allocation. Pure: no memory writes happen
    /// here. The caller is responsible for zero-filling the returned range
    /// (and growing memory if the result is `NeedGrow`).
    ///
    /// `cur_mem_size` is the current linear-memory size in bytes, used to
    /// detect whether a grow is needed.
    pub fn mmap(
        &mut self,
        cur_mem_size: usize,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i64,
        off: i64,
    ) -> MmapResult {
        let _ = prot;

        if len == 0 {
            return MmapResult::Err(-EINVAL);
        }
        if fd != -1 {
            return MmapResult::Err(-ENOSYS);
        }
        if flags & MAP_ANONYMOUS == 0 {
            return MmapResult::Err(-ENOSYS);
        }
        if flags & MAP_PRIVATE == 0 {
            return MmapResult::Err(-ENOSYS);
        }
        if off != 0 {
            return MmapResult::Err(-ENOSYS);
        }
        if len > ARENA_SIZE {
            return MmapResult::Err(-ENOSYS);
        }

        // 1. Try existing arenas.
        for arena in self.arenas.iter_mut() {
            if let Some(addr) = arena.alloc(len, 8) {
                return MmapResult::Ok(addr);
            }
        }

        // 2. Need a fresh arena; check if we have room.
        let needed = self.high_water as usize + ARENA_SIZE;
        if needed > cur_mem_size {
            let pages =
                ((needed - cur_mem_size + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize) as u64;
            return MmapResult::NeedGrow(pages);
        }

        // 3. Place the new arena.
        let base = self.high_water;
        self.high_water += ARENA_SIZE as u32;
        let mut arena = Arena::new(base);
        let addr = match arena.alloc(len, 8) {
            Some(a) => a,
            None => return MmapResult::Err(-ENOMEM),
        };
        self.arenas.push(arena);
        MmapResult::Ok(addr)
    }

    /// Free a previously-mmapped range.
    pub fn munmap(&mut self, addr: u32, len: usize) -> i64 {
        if len == 0 {
            return 0;
        }
        for arena in self.arenas.iter_mut() {
            if arena.contains(addr, len) {
                arena.free(addr, len);
                return 0;
            }
        }
        -EINVAL
    }

    /// P2-C2 `mremap` identity: extend `(old, old_len)` to `new_len` bytes
    /// in the same arena. Returns the new (or unchanged) base address on
    /// success; `-ENOMEM` if the arena cannot fit the larger size.
    ///
    /// Strategy: if the existing range lives in an arena and the arena
    /// has free space at the end (or in the free list adjacent to it),
    /// just bump `used`. Otherwise return `-ENOMEM`.
    pub fn grow_in_place(&mut self, old: u32, old_len: usize, new_len: usize) -> Result<u32, i64> {
        if new_len <= old_len {
            return Ok(old);
        }
        let extra = new_len - old_len;
        for arena in self.arenas.iter_mut() {
            let rel = match old.checked_sub(arena.base) {
                Some(v) => v as usize,
                None => continue,
            };
            // The existing allocation must be at the top of the arena
            // (rel + old_len == arena.used) for us to safely grow it.
            if rel + old_len == arena.used && arena.used + extra <= ARENA_SIZE {
                arena.used += extra;
                return Ok(old);
            }
        }
        Err(-ENOMEM)
    }

    pub fn mprotect(&self, _addr: u32, _len: usize, _prot: i32) -> i64 {
        0
    }

    pub fn brk(&self) -> u32 {
        self.high_water
    }
}

const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const ENOMEM: i64 = 12;

impl Default for LinearAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// Re-export for the test crate (kept here so tests can use the consts).
#[allow(unused_imports)]
pub use wasmtime::Memory as _Memory;
#[allow(dead_code)]
fn _check_as_context<S: AsContext>(_: &S) {}
