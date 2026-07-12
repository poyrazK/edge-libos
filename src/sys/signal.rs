//! Signal syscalls. P0 records dispositions only — no real delivery (spec
//! §4.8). The functions exist because CPython's libc installs SIGINT and
//! SIGPIPE handlers at startup; failing those calls makes libc abort.

use std::collections::HashMap;

use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::kernel::Kernel;
use crate::mem;

pub const NR_RT_SIGACTION: u32 = 13;
pub const NR_RT_SIGPROCMASK: u32 = 14;

/// `rt_sigaction`'s `how` argument values.
const SIG_BLOCK: i64 = 0;
const SIG_UNBLOCK: i64 = 1;
const SIG_SETMASK: i64 = 2;

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

/// Real layout on wasm32-musl is:
///   sa_handler (4) | sa_flags (4) | sa_mask (16) | sa_restorer (4) | pad (4) = 32
const SIGACTION_SIZE: i64 = 32;
const SIG_HANDLER_REAL_OFF: usize = 0;
const SIG_FLAGS_REAL_OFF: usize = 4;
const SIG_MASK_REAL_OFF: usize = 8;
const SIG_RESTORER_REAL_OFF: usize = 24;

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

/// `rt_sigaction(signum, act, oldact, sigsetsize)`.
///
/// `act` may be NULL to query without changing; `oldact` may be NULL to
/// discard the old disposition.
pub fn rt_sigaction(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let signum = a[0] as i32;
    let act = a[1];
    let oldact = a[2];
    let _sigsetsize = a[3];

    if !(1..=64).contains(&signum) {
        return -EINVAL;
    }

    // Snapshot the previously recorded action (if any) before taking the
    // mutable borrow on Kernel to record a new one.
    let prev = caller
        .data()
        .signals
        .actions
        .get(&signum)
        .copied()
        .unwrap_or_default();

    if oldact != 0 {
        let mem = match caller.data().memory() {
            Ok(m) => m.clone(),
            Err(e) => return e,
        };
        let bytes = match mem::guest_slice_mut_via(&mem, caller, oldact, SIGACTION_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let handler = prev.handler as u32;
        let flags = prev.flags as u32;
        bytes[SIG_HANDLER_REAL_OFF..SIG_HANDLER_REAL_OFF + 4]
            .copy_from_slice(&handler.to_le_bytes());
        bytes[SIG_FLAGS_REAL_OFF..SIG_FLAGS_REAL_OFF + 4]
            .copy_from_slice(&flags.to_le_bytes());
        bytes[SIG_MASK_REAL_OFF..SIG_MASK_REAL_OFF + 8]
            .copy_from_slice(&prev.mask.to_le_bytes());
        bytes[SIG_RESTORER_REAL_OFF..SIG_RESTORER_REAL_OFF + 4]
            .copy_from_slice(&(prev.restorer as u32).to_le_bytes());
    }

    if act != 0 {
        let bytes = match mem::guest_slice(caller, act, SIGACTION_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let handler = u32::from_le_bytes(
            bytes[SIG_HANDLER_REAL_OFF..SIG_HANDLER_REAL_OFF + 4]
                .try_into()
                .unwrap(),
        ) as u64;
        let flags = u32::from_le_bytes(
            bytes[SIG_FLAGS_REAL_OFF..SIG_FLAGS_REAL_OFF + 4]
                .try_into()
                .unwrap(),
        ) as u64;
        let mask = u64::from_le_bytes(
            bytes[SIG_MASK_REAL_OFF..SIG_MASK_REAL_OFF + 8]
                .try_into()
                .unwrap(),
        );
        let restorer = u32::from_le_bytes(
            bytes[SIG_RESTORER_REAL_OFF..SIG_RESTORER_REAL_OFF + 4]
                .try_into()
                .unwrap(),
        ) as u64;
        caller.data_mut().signals.actions.insert(
            signum,
            SigAction {
                handler,
                flags,
                restorer,
                mask,
            },
        );
    }

    0
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`.
pub fn rt_sigprocmask(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let how = a[0];
    let set = a[1];
    let oldset = a[2];
    let _sigsetsize = a[3];

    // Snapshot the existing mask BEFORE taking any mutable borrow.
    let prev_mask = caller.data().signals.mask;

    if oldset != 0 {
        let mem = match caller.data().memory() {
            Ok(m) => m.clone(),
            Err(e) => return e,
        };
        let bytes = match mem::guest_slice_mut_via(&mem, caller, oldset, 8) {
            Ok(b) => b,
            Err(e) => return e,
        };
        bytes.copy_from_slice(&prev_mask.to_le_bytes());
    }

    if set != 0 {
        let new_mask_bytes = match mem::guest_slice(caller, set, 8) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let new_mask = u64::from_le_bytes(new_mask_bytes.try_into().unwrap());
        let signals = &mut caller.data_mut().signals;
        match how {
            SIG_BLOCK => signals.mask |= new_mask,
            SIG_UNBLOCK => signals.mask &= !new_mask,
            SIG_SETMASK => signals.mask = new_mask,
            _ => return -EINVAL,
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_RT_SIGACTION, 13);
        assert_eq!(NR_RT_SIGPROCMASK, 14);
    }

    #[test]
    fn sigaction_layout_fits_in_32_bytes() {
        assert_eq!(SIGACTION_SIZE, 32);
    }
}
