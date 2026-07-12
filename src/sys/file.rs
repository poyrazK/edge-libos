//! File / VFS syscalls. P0 covers read/write/openat/close/lseek/fstat/getdents64
//! plus pipe2 and fcntl (needed for asyncio startup).

use wasmtime::Caller;

use crate::errno::to_ret;
use crate::kernel::Kernel;
use crate::mem;

pub const NR_READ: u32 = 0;
pub const NR_WRITE: u32 = 1;
pub const NR_OPENAT: u32 = 257;
pub const NR_CLOSE: u32 = 3;
pub const NR_LSEEK: u32 = 8;
pub const NR_FSTAT: u32 = 5;
pub const NR_NEWFSTATAT: u32 = 262;
pub const NR_GETDENTS64: u32 = 217;
pub const NR_PIPE2: u32 = 293;
pub const NR_FCNTL: u32 = 72;

pub async fn read(c: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    // NR_READ(fd, buf, len): bound-check the output buffer first so a bad
    // guest pointer is a clean -EFAULT, not a sandbox escape.
    if let Err(e) = mem::guest_slice_mut(c, a[1], a[2]) {
        return e;
    }
    to_ret(crate::errno::ENOSYS)
}

pub async fn write(c: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    // NR_WRITE(fd, buf, len): bound-check the input buffer. The actual
    // write lands in Step 14.
    if let Err(e) = mem::guest_slice(c, a[1], a[2]) {
        return e;
    }
    to_ret(crate::errno::ENOSYS)
}

pub async fn openat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn close(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn lseek(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn fstat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn newfstatat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn getdents64(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn pipe2(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
pub async fn fcntl(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
