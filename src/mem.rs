//! Guest linear-memory access helpers. The EFAULT choke point.
//!
//! Every syscall that takes a pointer+len routes through these. Bounds-check
//! against `Memory::data(&store).len()` and return `-EFAULT` on overflow.
//!
//! A host that segfaults on a bad guest pointer is a sandbox escape, not a
//! bug (spec §8).

use wasmtime::Caller;
use wasmtime::Memory;

use crate::kernel::Kernel;

/// Borrow a slice of guest memory.
///
/// Returns `Err(-EFAULT)` if the pointer+len would overflow linear memory
/// or the pointer is negative. Always bounds-check; never trust the guest.
pub fn guest_slice<'a>(
    caller: &'a Caller<'_, Kernel>,
    ptr: i64,
    len: i64,
) -> Result<&'a [u8], i64> {
    if ptr < 0 || len < 0 {
        return Err(-(crate::errno::EFAULT));
    }
    let (p, l) = (ptr as usize, len as usize);
    let end = p.checked_add(l).ok_or(-(crate::errno::EFAULT))?;
    // `Memory` is a `Copy` handle into the store, so we can clone it and
    // drop the `&Kernel` borrow before using it.
    let mem = *caller.data().memory()?;
    let base = mem.data(caller);
    base.get(p..end).ok_or(-(crate::errno::EFAULT))
}

/// Borrow a mutable slice of guest memory. Same semantics as `guest_slice`.
pub fn guest_slice_mut<'a>(
    caller: &'a mut Caller<'_, Kernel>,
    ptr: i64,
    len: i64,
) -> Result<&'a mut [u8], i64> {
    if ptr < 0 || len < 0 {
        return Err(-(crate::errno::EFAULT));
    }
    let (p, l) = (ptr as usize, len as usize);
    let end = p.checked_add(l).ok_or(-(crate::errno::EFAULT))?;
    // Same Copy-handle trick: clone the `Memory` handle so the `&Kernel`
    // borrow ends before we re-borrow `caller` mutably.
    let mem = *caller.data().memory()?;
    let base = mem.data_mut(caller);
    base.get_mut(p..end).ok_or(-(crate::errno::EFAULT))
}

/// Like `guest_slice_mut`, but for callers that already cloned the `Memory`
/// handle (e.g. when they need to access `Kernel` mutable state between
/// snapshotting the handle and writing). The lifetime on the returned slice
/// is tied to `caller`, not to the `Kernel` borrow.
pub fn guest_slice_mut_via<'a>(
    mem: &'a Memory,
    caller: &'a mut Caller<'_, Kernel>,
    ptr: i64,
    len: i64,
) -> Result<&'a mut [u8], i64> {
    if ptr < 0 || len < 0 {
        return Err(-(crate::errno::EFAULT));
    }
    let (p, l) = (ptr as usize, len as usize);
    let end = p.checked_add(l).ok_or(-(crate::errno::EFAULT))?;
    let base = mem.data_mut(caller);
    base.get_mut(p..end).ok_or(-(crate::errno::EFAULT))
}

/// Read a NUL-terminated UTF-8 string from guest memory. `max_len` caps the
/// search to avoid scanning to the end of linear memory on a missing NUL.
pub fn guest_str<'a>(
    caller: &'a Caller<'_, Kernel>,
    ptr: i64,
    max_len: i64,
) -> Result<&'a str, i64> {
    let bytes = guest_slice(caller, ptr, max_len)?;
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..nul]).map_err(|_| -(crate::errno::EINVAL))
}
