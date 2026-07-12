//! Anonymous-mmap-over-linear-memory allocator.
//!
//! The spec's "single most important non-obvious piece" (§1.2): since Wasm
//! has no virtual address space, `mmap(MAP_ANONYMOUS)` cannot create new
//! address space. Instead, the host reserves high regions of linear memory
//! and carves allocations out of them, growing via `Memory::grow` as needed.
//!
//! P0 mirrors CPython's obmalloc: 256 KiB arenas, bump + free-list inside
//! each arena. The full `LinearAllocator` lands in Step 7; this skeleton
//! has just enough surface for the `Kernel` constructor.

pub mod arena;

#[allow(dead_code)]
pub struct LinearAllocator {
    /// High-water mark — next arena base we will carve from.
    high_water: u32,
}

impl LinearAllocator {
    pub const FIRST_ARENA_BASE: u32 = 0x0010_0000; // 1 MiB up — leave 0..1 MiB for CPython static

    pub fn new() -> Self {
        Self {
            high_water: Self::FIRST_ARENA_BASE,
        }
    }
}

impl Default for LinearAllocator {
    fn default() -> Self {
        Self::new()
    }
}
