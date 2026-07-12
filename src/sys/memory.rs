//! Memory syscalls. P0 covers mmap/munmap/mprotect/brk/madvise.

use wasmtime::Caller;

use crate::kernel::Kernel;
use crate::mm::{MmapResult, MAP_ANONYMOUS, MAP_PRIVATE};

pub const NR_MMAP: u32 = 9;
pub const NR_MUNMAP: u32 = 11;
pub const NR_MPROTECT: u32 = 10;
pub const NR_MADVISE: u32 = 28;
pub const NR_BRK: u32 = 12;

/// Helper: read current memory size in bytes via a shared borrow of `caller`.
fn mem_size(caller: &Caller<'_, Kernel>) -> usize {
    // Already a shared borrow; Kernel::memory is &-only.
    caller
        .data()
        .memory()
        .map(|m| m.data(caller).len())
        .unwrap_or(0)
}

/// NR_MMAP(addr, len, prot, flags, fd, off). P0 supports
/// `MAP_ANONYMOUS | MAP_PRIVATE` only; everything else returns -ENOSYS.
pub async fn mmap(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let _addr_hint = a[0];
    let len = match usize::try_from(a[1]) {
        Ok(n) if n > 0 => n,
        _ => return -crate::errno::EINVAL,
    };
    let prot = a[2] as i32;
    let flags = a[3] as i32;
    let fd = a[4];
    let off = a[5];

    // Snapshot the Memory handle (it's Copy) so we don't hold a &Kernel
    // borrow across the data_mut reborrow.
    let mem = match caller.data().memory() {
        Ok(m) => m.clone(),
        Err(e) => return e,
    };
    let cur = mem_size(caller);

    // First decision: may say "need to grow".
    let decision = {
        let mm = &mut caller.data_mut().mm;
        mm.mmap(cur, len, prot, flags, fd, off)
    };
    if let MmapResult::NeedGrow(pages) = decision {
        if mem.grow(&mut *caller, pages).is_err() {
            return -crate::errno::ENOMEM;
        }
    }

    // Second decision (or final). Read `cur` BEFORE the mutable borrow so we
    // don't borrow `caller` both ways at once.
    let cur2 = mem_size(caller);
    let result_addr = {
        let mm = &mut caller.data_mut().mm;
        match mm.mmap(cur2, len, prot, flags, fd, off) {
            MmapResult::Ok(addr) => addr,
            MmapResult::Err(e) => return e,
            MmapResult::NeedGrow(_) => return -crate::errno::ENOMEM,
        }
    };

    // Zero-fill the new range.
    {
        let bytes = mem.data_mut(&mut *caller);
        let start = result_addr as usize;
        let end = start + len;
        if end <= bytes.len() {
            bytes[start..end].fill(0);
        }
    }

    let _ = (MAP_ANONYMOUS, MAP_PRIVATE);
    result_addr as i64
}

/// NR_MUNMAP(addr, len). Returns 0 on success, -EINVAL if the range is not
/// owned by any arena.
pub async fn munmap(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let addr = match u32::try_from(a[0]) {
        Ok(v) => v,
        Err(_) => return -crate::errno::EINVAL,
    };
    let len = match usize::try_from(a[1]) {
        Ok(n) => n,
        Err(_) => return -crate::errno::EINVAL,
    };
    let mm = &mut caller.data_mut().mm;
    mm.munmap(addr, len)
}

/// mprotect is a no-op (spec §1.2).
pub fn mprotect() -> i64 {
    0
}

pub fn madvise() -> i64 {
    0
}

/// brk(0) returns the high-water mark.
pub fn brk(caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    caller.data().mm.brk() as i64
}
