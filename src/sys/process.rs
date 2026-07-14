//! Process / startup / control. P0 covers all stubs the libc pokes at startup.

use wasmtime::Caller;

use crate::errno::{EINVAL, EPERM, ESRCH};
use crate::kernel::Kernel;
use crate::mem;

// Linux x86-64 syscall numbers (`unistd_64.h`).
pub const NR_EXIT: u32 = 60;
pub const NR_EXIT_GROUP: u32 = 231;
pub const NR_GETPID: u32 = 39;
pub const NR_GETTID: u32 = 186;
pub const NR_SET_TID_ADDRESS: u32 = 218;
pub const NR_SET_ROBUST_LIST: u32 = 273;
pub const NR_ARCH_PRCTL: u32 = 158;
pub const NR_RSEQ: u32 = 334;

// P2-C2: sched_yield, sched_getaffinity, prctl, kill, tgkill.
pub const NR_SCHED_YIELD: u32 = 24;
pub const NR_SCHED_GETAFFINITY: u32 = 204;
pub const NR_PRCTL: u32 = 157;
pub const NR_KILL: u32 = 62;
pub const NR_TGKILL: u32 = 234;

// P3 reservation: clone / fork / wait4. P2-D snapshot machinery will
// back fork() as CoW; clone() needs futex support (see ADR 0001).
pub const NR_CLONE: u32 = 56;
pub const NR_FORK: u32 = 57;
pub const NR_WAIT4: u32 = 61;

// prctl(2) options we recognize (subset — others return -EINVAL).
pub const PR_SET_NAME: i32 = 15;
pub const PR_GET_NAME: i32 = 16;
pub const PR_GET_DUMPABLE: i32 = 3;
pub const PR_SET_DUMPABLE: i32 = 4;
pub const PR_GET_NO_NEW_PRIVS: i32 = 39;
pub const PR_SET_NO_NEW_PRIVS: i32 = 38;

/// `exit(code)`: record the exit code in the kernel. The host driver
/// inspects `Kernel::exit_code` after each top-level wasm call and
/// surfaces it. We don't trap here because musl's `exit` path may still
/// flush stdio AFTER the syscall returns — a trap would skip the flush.
pub async fn exit(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    caller.data_mut().exit_code = Some(a[0] as i32);
    0
}

/// `exit_group(code)`: same semantics as `exit` in single-threaded v1.
pub async fn exit_group(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    caller.data_mut().exit_code = Some(a[0] as i32);
    0
}

pub fn getpid() -> i64 {
    1
}

pub fn gettid() -> i64 {
    1
}

pub fn set_tid_address(_caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    1
}

pub fn set_robust_list() -> i64 {
    0
}

/// `sched_yield()` → 0. CPython sometimes calls this in poll loops; we
/// yield to the executor via `tokio::task::yield_now`.
pub async fn sched_yield() -> i64 {
    tokio::task::yield_now().await;
    0
}

/// `sched_getaffinity(pid, len, mask_ptr)` — fill the cpu mask with
/// "all CPUs" (a single 1 bit at position 0). Accepts self pid (0 or 1)
/// only; other pids → -ESRCH.
pub async fn sched_getaffinity(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let pid = a[0];
    let len = a[1];
    let mask_ptr = a[2];
    if pid != 0 && pid != 1 {
        return -ESRCH;
    }
    // Write min(len, 8) bytes — kernel returns the actual length.
    let to_write = std::cmp::min(len, 8).max(0);
    if to_write == 0 {
        return -EINVAL;
    }
    let bytes = match mem::guest_slice_mut(caller, mask_ptr, to_write) {
        Ok(b) => b,
        Err(e) => return e,
    };
    bytes[0] = 0x01; // CPU 0 only
    bytes[1..to_write as usize].fill(0);
    to_write
}

/// `prctl(option, ...)` — minimum set: PR_SET/GET_NAME, PR_GET/SET_DUMPABLE,
/// PR_GET/SET_NO_NEW_PRIVS. Anything else returns -EINVAL.
pub async fn prctl(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let option = a[0] as i32;
    let arg2 = a[1];
    let arg3 = a[2];
    let arg4 = a[3];
    let arg5 = a[4];

    match option {
        PR_SET_NAME => {
            // Read up to 16 bytes (comm name) from arg2.
            if arg2 == 0 {
                return -EINVAL;
            }
            // Copy out the comm bytes via a shared borrow first, then
            // release the borrow before taking a mutable one on caller.
            let mut new_comm = [0u8; 16];
            {
                let bytes = match mem::guest_slice(caller, arg2, 16) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(16);
                for i in 0..16 {
                    new_comm[i] = if i < nlen { bytes[i] } else { 0 };
                }
            }
            caller.data_mut().comm = new_comm;
            0
        }
        PR_GET_NAME => {
            if arg2 == 0 {
                return -EINVAL;
            }
            // Snapshot current comm via shared borrow, drop it, then
            // write via the mutable slice.
            let cur = caller.data().comm;
            let bytes = match mem::guest_slice_mut(caller, arg2, 16) {
                Ok(b) => b,
                Err(e) => return e,
            };
            bytes.copy_from_slice(&cur);
            0
        }
        PR_GET_DUMPABLE => 0,
        PR_SET_DUMPABLE => {
            let _ = arg2; // ignored
            0
        }
        PR_GET_NO_NEW_PRIVS => 1,
        PR_SET_NO_NEW_PRIVS => {
            let _ = (arg2, arg3, arg4, arg5);
            0
        }
        _ => -EINVAL,
    }
}

/// `kill(pid, sig)` — single-process v1 only. We treat all pids as self;
/// non-self pids return -ESRCH. The signal is recorded but not delivered
/// (matching the rest of the signal surface).
pub async fn kill(_caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let pid = a[0];
    let sig = a[1];
    if pid != 0 && pid != 1 {
        return -ESRCH;
    }
    if sig < 0 || !(0..=64).contains(&sig) {
        return -EINVAL;
    }
    // We don't actually deliver in v1 — return success.
    0
}

/// `tgkill(tgid, tid, sig)` — same as kill for our single-process model.
/// Non-self tgids/tids → -ESRCH.
pub async fn tgkill(_caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let tgid = a[0];
    let tid = a[1];
    let sig = a[2];
    if (tgid != 0 && tgid != 1) || (tid != 0 && tid != 1) {
        return -ESRCH;
    }
    if sig < 0 || !(0..=64).contains(&sig) {
        return -EINVAL;
    }
    0
}

#[allow(dead_code)]
fn _kill_perm() -> i64 {
    -EPERM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_EXIT, 60);
        assert_eq!(NR_EXIT_GROUP, 231);
        assert_eq!(NR_GETPID, 39);
        assert_eq!(NR_GETTID, 186);
        assert_eq!(NR_SET_TID_ADDRESS, 218);
        assert_eq!(NR_SET_ROBUST_LIST, 273);
        assert_eq!(NR_ARCH_PRCTL, 158);
        assert_eq!(NR_RSEQ, 334);
        assert_eq!(NR_SCHED_YIELD, 24);
        assert_eq!(NR_SCHED_GETAFFINITY, 204);
        assert_eq!(NR_PRCTL, 157);
        assert_eq!(NR_KILL, 62);
        assert_eq!(NR_TGKILL, 234);
        assert_eq!(NR_CLONE, 56);
        assert_eq!(NR_FORK, 57);
        assert_eq!(NR_WAIT4, 61);
    }

    #[test]
    fn identity_returns_one() {
        assert_eq!(getpid(), 1);
        assert_eq!(gettid(), 1);
    }
}
