//! File / VFS syscalls. P0 covers read/write against the buffered stdio
//! pipes; openat/close/lseek/fstat/getdents64 land in Step 14.

use wasmtime::Caller;

use crate::errno::{EBADF, EAGAIN, EIO, EINVAL, ENOSYS};
use crate::fd::Resource;
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

/// `read(fd, buf, len)`. Reads up to `len` bytes from `fd` into `buf`.
/// For P0 only stdin / pipe-read fds are wired; everything else returns
/// -ENOSYS or -EBADF.
pub async fn read(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    // Bounds-check the guest buffer FIRST so negative len and overflowing
    // ptr+len return -EFAULT (the spec §8 contract) before any kernel state
    // is touched.
    let buf_ptr = a[1];
    let buf_len_raw = a[2];
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, buf_len_raw) {
        return e;
    }
    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EAGAIN,
    };
    if len == 0 {
        return 0;
    }

    // Drain into an owned Vec<u8>. We use Vec<u8> (not VecDeque) so we can
    // copy_from_slice into the guest buffer without contiguity gymnastics.
    let mut tmp: Vec<u8> = Vec::new();
    let mut eof = false;
    {
        let fds = &mut caller.data_mut().fds;
        let res = match fds.get_mut(fd) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match res {
            Resource::Stdin(r) | Resource::PipeRead(r) => {
                let mut q = r.buf.lock();
                for _ in 0..len {
                    match q.pop_front() {
                        Some(b) => tmp.push(b),
                        None => break,
                    }
                }
                eof = tmp.is_empty() && *r.closed.lock();
            }
            Resource::File(_) => return -ENOSYS,
            _ => return -EBADF,
        }
    }
    if eof {
        return 0;
    }
    if tmp.is_empty() {
        return -EAGAIN;
    }
    let n = tmp.len();
    // Re-resolve the guest slice now that the FdTable borrow is gone.
    let bytes = match mem::guest_slice_mut(caller, buf_ptr, len as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[..n].copy_from_slice(&tmp);
    n as i64
}

/// `write(fd, buf, len)`. Writes `len` bytes from `buf` to `fd`.
pub async fn write(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    // Bounds-check the guest buffer FIRST. This catches negative len and
    // overflowing ptr+len before we touch any other kernel state. -EFAULT
    // is the spec's required return for any poisoned guest pointer (spec §8).
    let bytes = match mem::guest_slice(caller, a[1], a[2]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let len = bytes.len();
    if len == 0 {
        return 0;
    }
    let bytes = bytes.to_vec();

    let written = {
        let fds = &mut caller.data_mut().fds;
        let res = match fds.get_mut(fd) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match res {
            Resource::Stdout(w) | Resource::Stderr(w) | Resource::PipeWrite(w) => {
                let mut q = w.buf.lock();
                q.extend(bytes.iter().copied());
                bytes.len()
            }
            Resource::File(f) => {
                use std::io::Write;
                match f.write(&bytes) {
                    Ok(n) => n,
                    Err(_) => return -EIO,
                }
            }
            _ => return -EBADF,
        }
    };
    written as i64
}

pub async fn openat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn close(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn lseek(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn fstat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn newfstatat(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn getdents64(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn pipe2(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
pub async fn fcntl(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    -(crate::errno::ENOSYS)
}
