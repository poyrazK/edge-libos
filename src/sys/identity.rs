//! Identity stubs. Per spec §4.7 we report a fixed uid/gid of 1000.

use wasmtime::Caller;

use crate::errno::{EINVAL, EOPNOTSUPP, EPERM};
use crate::kernel::Kernel;
use crate::mem;

pub const UID: i64 = 1000;
pub const GID: i64 = 1000;

pub const NR_GETUID: u32 = 102;
pub const NR_GETEUID: u32 = 107;
pub const NR_GETGID: u32 = 104;
pub const NR_GETEGID: u32 = 108;

// P2-C2: getppid, uname, prlimit64, getrlimit, setsid, getsid, getgroups.
pub const NR_GETPPID: u32 = 110;
pub const NR_UNAME: u32 = 63;
pub const NR_PRLIMIT64: u32 = 302;
pub const NR_GETRLIMIT: u32 = 97;
pub const NR_SETSID: u32 = 112;
pub const NR_GETSID: u32 = 124;
pub const NR_GETGROUPS: u32 = 115;

/// `struct utsname` (Linux): 6 × 65 bytes = 390 bytes total.
const UTSNAME_SIZE: i64 = 390;
const UTSNAME_FIELD: usize = 65;
const UTSNAME_FIELDS: usize = 6;

/// `struct rlimit64` (the same 16-byte shape).
const RLIMIT64_SIZE: i64 = 16;

pub fn getuid() -> i64 {
    UID
}
pub fn geteuid() -> i64 {
    UID
}
pub fn getgid() -> i64 {
    UID
}
pub fn getegid() -> i64 {
    UID
}

/// `getppid()` → 1 (single-process v1; parent is also us).
pub fn getppid() -> i64 {
    1
}

/// `uname(buf)` — fill a `struct utsname` (6×65 bytes) with our
/// fixed values. Matches `linux/utsname.h` field order: sysname, nodename,
/// release, version, machine, domainname.
pub async fn uname(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let buf_ptr = a[0];
    let bytes = match mem::guest_slice_mut(caller, buf_ptr, UTSNAME_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let fields: [&[u8]; UTSNAME_FIELDS] = [
        b"Linux",
        b"edge-libos",
        b"6.0.0",
        b"#1 wasm32",
        b"wasm32",
        b"(none)",
    ];
    for (i, field) in fields.iter().enumerate() {
        let off = i * UTSNAME_FIELD;
        for (j, &c) in field.iter().enumerate() {
            bytes[off + j] = c;
        }
        // NUL terminator
        bytes[off + field.len()] = 0;
        // Pad the rest with 0
        for j in (field.len() + 1)..UTSNAME_FIELD {
            bytes[off + j] = 0;
        }
    }
    0
}

/// `prlimit64(pid, resource, new_limit, old_limit)` — read rlimits for
/// self (pid 0 or 1). We model RLIMIT_STACK (3) and RLIMIT_NOFILE (7) as
/// the only ones CPython inspects. Other resources return 0 (no change)
/// and write back defaults: rlim_cur=rlim_max=u64::MAX.
pub async fn prlimit64(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let pid = a[0];
    let _resource = a[1] as i32;
    let _new_limit = a[2];
    let old_limit_ptr = a[3];
    if pid != 0 && pid != 1 {
        return -EPERM;
    }
    if old_limit_ptr != 0 {
        let bytes = match mem::guest_slice_mut(caller, old_limit_ptr, RLIMIT64_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        // Defaults: infinity
        bytes[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    }
    0
}

/// `getrlimit(resource, rlim)` — legacy (32-bit) rlimit. Same semantics as
/// prlimit64 for the resources we model.
pub async fn getrlimit(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    prlimit64(caller, [0, a[0], 0, a[1], 0, 0]).await
}

/// `setsid()` — single-process v1 has no session; return -EPERM.
pub fn setsid() -> i64 {
    -EPERM
}

/// `getsid(pid)` — return 1 (the "session" id == our pid).
pub fn getsid(_a: [i64; 6]) -> i64 {
    1
}

/// `getgroups(size, list)` — return 0 groups; if `list` is non-NULL and
/// `size >= 0`, write nothing; if `size == 0`, return the count (0).
/// `size < 0` → -EINVAL.
pub async fn getgroups(_caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let size = a[0];
    let list_ptr = a[1];
    if size < 0 {
        return -EINVAL;
    }
    if size == 0 {
        return 0; // count of groups = 0
    }
    if list_ptr == 0 {
        return -EOPNOTSUPP;
    }
    // size > 0 with a list: we have no groups, so write 0 groups.
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utsname_layout() {
        assert_eq!(UTSNAME_SIZE, 390);
        assert_eq!(UTSNAME_FIELD * UTSNAME_FIELDS, 390);
    }

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_GETUID, 102);
        assert_eq!(NR_GETEUID, 107);
        assert_eq!(NR_GETGID, 104);
        assert_eq!(NR_GETEGID, 108);
        assert_eq!(NR_GETPPID, 110);
        assert_eq!(NR_UNAME, 63);
        assert_eq!(NR_PRLIMIT64, 302);
        assert_eq!(NR_GETRLIMIT, 97);
        assert_eq!(NR_SETSID, 112);
        assert_eq!(NR_GETSID, 124);
        assert_eq!(NR_GETGROUPS, 115);
    }
}
