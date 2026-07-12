//! Process / startup / control. P0 covers all stubs the libc pokes at startup.

use wasmtime::Caller;

use crate::kernel::Kernel;

// Linux x86-64 syscall numbers (`unistd_64.h`).
pub const NR_EXIT: u32 = 60;
pub const NR_EXIT_GROUP: u32 = 231;
pub const NR_GETPID: u32 = 39;
pub const NR_GETTID: u32 = 186;
pub const NR_SET_TID_ADDRESS: u32 = 218;
pub const NR_SET_ROBUST_LIST: u32 = 273;
pub const NR_ARCH_PRCTL: u32 = 158;
pub const NR_RSEQ: u32 = 334;

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
    }

    #[test]
    fn identity_returns_one() {
        assert_eq!(getpid(), 1);
        assert_eq!(gettid(), 1);
    }
}
