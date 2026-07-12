//! Memory syscalls. P0 covers mmap/munmap/mprotect/brk/madvise.

use wasmtime::Caller;

use crate::errno::to_ret;
use crate::kernel::Kernel;

pub const NR_MMAP: u32 = 9;
pub const NR_MUNMAP: u32 = 11;
pub const NR_MPROTECT: u32 = 10;
pub const NR_MADVISE: u32 = 28;
pub const NR_BRK: u32 = 12;

pub async fn mmap(_caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}

pub async fn munmap(_caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}

pub fn mprotect() -> i64 {
    // mprotect is a no-op in the wasm memory model (spec §1.2). Returning
    // success keeps allocators that call it (musl pthreads, jemalloc) happy.
    0
}

pub fn madvise() -> i64 {
    0
}

pub fn brk(_caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    // Real implementation: return the high-water mark of the linear
    // allocator. P0 stub: any non-zero value is acceptable; the linear
    // allocator lands in Step 7.
    0
}
