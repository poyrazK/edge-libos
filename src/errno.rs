//! Linux errno constants as i64.
//!
//! These match `<errno.h>` on Linux x86-64. The host returns `-errno` for any
//! failure (kernel convention), so `EPERM` is the *positive* value 1, the host
//! returns `-1` for it, and the guest libc translates that back into `errno`.
//!
//! Keep this list in sync with the P0 syscalls we actually return. Anything
//! we don't list here is `ENOSYS` by default.

#![allow(dead_code)] // populated lazily as syscalls land

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const EBADF: i64 = 9;
pub const ECHILD: i64 = 10;
pub const EAGAIN: i64 = 11;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EBUSY: i64 = 16;
pub const EEXIST: i64 = 17;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const EMFILE: i64 = 24;
pub const ENOTTY: i64 = 25;
pub const EFBIG: i64 = 27;
pub const ENOSPC: i64 = 28;
pub const ESPIPE: i64 = 29;
pub const EROFS: i64 = 30;
pub const EPIPE: i64 = 32;
pub const ELOOP: i64 = 40;
pub const ERANGE: i64 = 34;
pub const ENOSYS: i64 = 38;
pub const EDESTADDRREQ: i64 = 89;
pub const EOPNOTSUPP: i64 = 95;
pub const EAFNOSUPPORT: i64 = 97;
pub const EPROTONOSUPPORT: i64 = 93;

/// Return a host-style "negative errno" from a positive errno value.
#[inline]
pub const fn to_ret(e: i64) -> i64 {
    -e
}
