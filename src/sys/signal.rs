//! Signal syscalls. P0 records dispositions only — no real delivery (spec
//! §4.8). The functions exist because CPython's libc installs SIGINT and
//! SIGPIPE handlers at startup; failing those calls makes libc abort.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::kernel::Kernel;
use crate::mem;

pub const NR_RT_SIGACTION: u32 = 13;
pub const NR_RT_SIGPROCMASK: u32 = 14;

// P2-C2: sigaltstack, rt_sigreturn.
pub const NR_SIGALTSTACK: u32 = 131;
pub const NR_RT_SIGRETURN: u32 = 15;

// sigaltstack(2) flags (linux/signal.h).
pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;

// `struct sigaltstack` on wasm32-musl: ss_sp(8) + ss_flags(4) + pad(4) + ss_size(8) = 24
pub const SIGALTSTACK_SIZE: i64 = 24;
const SS_SP_OFF: usize = 0;
const SS_FLAGS_OFF: usize = 8;
const SS_SIZE_OFF: usize = 16;

/// `rt_sigaction`'s `how` argument values.
const SIG_BLOCK: i64 = 0;
const SIG_UNBLOCK: i64 = 1;
const SIG_SETMASK: i64 = 2;

/// Recorded signal disposition. Just the shape CPython's libc pokes at us;
/// we never actually deliver in v1 (spec §4.8).
///
/// P2-D1: derives `Serialize`/`Deserialize` so `SignalState` can be
/// captured in `KernelSnapshot` without a custom impl.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// P2-D1: derives `Serialize`/`Deserialize` for snapshot.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SignalState {
    pub actions: HashMap<i32, SigAction>,
    pub mask: u64,
    /// P2-C2: alternate signal stack (sigaltstack). Stored as the raw
    /// bytes the guest wrote via sigaltstack(ss, old_ss).
    pub alt_stack: Option<Vec<u8>>,
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
        bytes[SIG_FLAGS_REAL_OFF..SIG_FLAGS_REAL_OFF + 4].copy_from_slice(&flags.to_le_bytes());
        bytes[SIG_MASK_REAL_OFF..SIG_MASK_REAL_OFF + 8].copy_from_slice(&prev.mask.to_le_bytes());
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

/// `sigaltstack(ss, old_ss)` — read/write the alternate signal stack
/// record. We don't actually deliver signals in v1, but the syscall must
/// succeed so musl's startup doesn't fall over. Layout: ss_sp(8),
/// ss_flags(4)+pad(4), ss_size(8) = 24 bytes on wasm32-musl.
pub fn sigaltstack(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let ss = a[0];
    let old_ss = a[1];

    // Snapshot current alt_stack before any mutable borrow.
    let prev = caller.data().signals.alt_stack.clone();

    if old_ss != 0 {
        let bytes = match mem::guest_slice_mut(caller, old_ss, SIGALTSTACK_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        if let Some(prev_bytes) = prev.as_ref() {
            // Copy the raw 24-byte record.
            for (i, &c) in prev_bytes.iter().enumerate() {
                if i < SIGALTSTACK_SIZE as usize {
                    bytes[i] = c;
                }
            }
        } else {
            // SS_DISABLE: clear the record.
            for i in 0..SIGALTSTACK_SIZE as usize {
                bytes[i] = 0;
            }
        }
    }

    if ss != 0 {
        let bytes = match mem::guest_slice(caller, ss, SIGALTSTACK_SIZE) {
            Ok(b) => b,
            Err(e) => return e,
        };
        // Honor SS_DISABLE explicitly: clear alt_stack.
        let flags = i32::from_le_bytes(bytes[SS_FLAGS_OFF..SS_FLAGS_OFF + 4].try_into().unwrap());
        if flags & SS_DISABLE != 0 {
            caller.data_mut().signals.alt_stack = None;
        } else {
            let mut record = vec![0u8; SIGALTSTACK_SIZE as usize];
            record.copy_from_slice(bytes);
            caller.data_mut().signals.alt_stack = Some(record);
        }
    }

    0
}

/// `rt_sigreturn()` — return from a signal handler. We don't actually
/// deliver signals in v1, so this is a no-op success. Returning 0 keeps
/// musl's libc startup happy when probing the syscall surface.
pub fn rt_sigreturn() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_constants_match_linux_x86_64() {
        assert_eq!(NR_RT_SIGACTION, 13);
        assert_eq!(NR_RT_SIGPROCMASK, 14);
        assert_eq!(NR_SIGALTSTACK, 131);
        assert_eq!(NR_RT_SIGRETURN, 15);
    }

    #[test]
    fn sigaction_layout_fits_in_32_bytes() {
        assert_eq!(SIGACTION_SIZE, 32);
    }

    #[test]
    fn sigaltstack_layout_fits_in_24_bytes() {
        assert_eq!(SIGALTSTACK_SIZE, 24);
    }
}
