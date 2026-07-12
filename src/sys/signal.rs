//! Signal syscalls. P0 records dispositions only — no real delivery (spec
//! §4.8). The functions exist because CPython's libc installs SIGINT and
//! SIGPIPE handlers at startup; failing those calls makes libc abort.

use std::collections::HashMap;

use wasmtime::Caller;

use crate::errno::to_ret;
use crate::kernel::Kernel;

pub const NR_RT_SIGACTION: u32 = 13;
pub const NR_RT_SIGPROCMASK: u32 = 14;

/// Recorded signal disposition. Just the shape CPython's libc pokes at us;
/// we never actually deliver in v1 (spec §4.8).
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

#[derive(Debug, Default)]
pub struct SignalState {
    pub actions: HashMap<i32, SigAction>,
    pub mask: u64,
}

impl SignalState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn rt_sigaction(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}

pub fn rt_sigprocmask(_c: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    to_ret(crate::errno::ENOSYS)
}
