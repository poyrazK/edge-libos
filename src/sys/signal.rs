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
const SS_FLAGS_OFF: usize = 8;

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

// ---------------------------------------------------------------------------
// Signal delivery (ADR 0007)
// ---------------------------------------------------------------------------

/// Standard signal numbers we special-case in the default-action table.
/// Only the ones whose *default* disposition is "ignore", plus the two
/// uncatchable signals, need naming — every other signal defaults to
/// terminate.
pub const SIGKILL: i32 = 9;
pub const SIGSTOP: i32 = 19;
const SIGCHLD: i32 = 17;
const SIGCONT: i32 = 18;
const SIGURG: i32 = 23;
const SIGWINCH: i32 = 28;

/// `sa_handler` sentinel values (`<signal.h>`): `SIG_DFL` / `SIG_IGN`.
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// What a pending signal should do to the current thread, computed by
/// [`deliverable`]. We never invoke a guest-registered `sa_handler` in v1
/// (spec §4.8) — a custom handler downgrades to [`DeliveryAction::Interrupt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryAction {
    /// Nothing to deliver — no unmasked, non-ignored signal was pending.
    Ignore,
    /// An unmasked signal is pending; interrupt the blocked syscall with
    /// `-EINTR`. Covers signals with a custom handler (which we consume but
    /// do NOT run) and any interrupting default.
    Interrupt,
    /// A default-terminating signal (or SIGKILL/SIGSTOP) fired; the guest
    /// must be torn down with exit code `128 + signo`.
    Terminate(i32),
}

/// Is `signo`'s *default* action to ignore it? (SIGCHLD/SIGURG/SIGWINCH/
/// SIGCONT.) Every other signal defaults to terminate.
fn default_is_ignore(signo: i32) -> bool {
    matches!(signo, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

/// Is `signo` currently blocked by the per-thread mask? Bit `signo - 1`.
fn is_masked(mask: u64, signo: i32) -> bool {
    if !(1..=64).contains(&signo) {
        return false;
    }
    mask & (1u64 << (signo - 1)) != 0
}

/// Drain the per-process pending-signal queue and decide what the current
/// thread should do, per ADR 0007. Pure w.r.t. the guest: it only mutates
/// `process_state.signals_pending` (draining consumed signals) and reads
/// `kernel.signals` (mask + dispositions).
///
/// Scanning rules, in order, for each dequeued signal:
///   * sig `0` — not a real signal (`kill(pid, 0)` is a permission probe);
///     drop and continue.
///   * SIGKILL / SIGSTOP — uncatchable: bypass mask + disposition, return
///     [`DeliveryAction::Terminate`] immediately.
///   * masked — keep it queued (preserving FIFO order) and continue.
///   * `SIG_IGN`, or `SIG_DFL` whose default is ignore — drop and continue.
///   * `SIG_DFL` whose default is terminate — [`DeliveryAction::Terminate`].
///   * custom handler — consume and return [`DeliveryAction::Interrupt`]
///     (handler NOT invoked in v1).
///
/// Lock discipline (ADR 0001 §2): the `signals_pending` guard is taken and
/// fully released inside this synchronous function — callers never hold it
/// across an `.await`.
pub fn deliverable(kernel: &Kernel) -> DeliveryAction {
    let mask = kernel.signals.mask;

    let mut pending = kernel.process_state.signals_pending.lock();
    if pending.is_empty() {
        return DeliveryAction::Ignore;
    }

    // Rebuild the queue keeping only the signals we didn't act on (masked
    // signals stay pending; consumed/ignored ones are dropped), preserving
    // FIFO order.
    let drained: Vec<i32> = std::mem::take(&mut *pending);
    let mut result = DeliveryAction::Ignore;
    let mut leftover: Vec<i32> = Vec::new();
    let mut iter = drained.into_iter();

    for signo in iter.by_ref() {
        if signo == 0 {
            continue;
        }
        if signo == SIGKILL || signo == SIGSTOP {
            result = DeliveryAction::Terminate(signo);
            break;
        }
        if is_masked(mask, signo) {
            leftover.push(signo);
            continue;
        }
        let handler = kernel
            .signals
            .actions
            .get(&signo)
            .map(|a| a.handler)
            .unwrap_or(SIG_DFL);
        match handler {
            SIG_IGN => continue,
            SIG_DFL => {
                if default_is_ignore(signo) {
                    continue;
                }
                result = DeliveryAction::Terminate(signo);
                break;
            }
            _ => {
                // Custom handler: we do not synthesize a call into it (v1
                // spec §4.8). Consume the signal and interrupt the syscall.
                result = DeliveryAction::Interrupt;
                break;
            }
        }
    }

    // Any signals we hadn't examined yet (because we broke early) plus the
    // masked leftovers go back on the queue, preserving order.
    for signo in iter {
        leftover.push(signo);
    }
    *pending = leftover;

    result
}

/// Apply the terminating side-effect of a [`DeliveryAction::Terminate`]
/// to the kernel: set `exit_code = 128 + signo` and flip
/// `exit_requested = true` so the dispatch pre-check (Commit 2)
/// short-circuits subsequent syscalls. No-op for non-terminating
/// actions. Centralized here so every blocking-syscall integration
/// point calls the same helper. The actual `128 + signo` arithmetic
/// lands in Commit 8 — the stub returns silently today.
pub fn apply_terminate_if_needed(_kernel: &Kernel, _action: &Option<DeliveryAction>) {
    // Filled in by Commit 8 (terminating default action).
}

/// Re-arm a `tokio::select!` block on the calling thread's per-tid
/// signal-wake `Notify`. Returns the `Notify` clone so the caller
/// can add it as a `select!` arm. Centralizes the lazy-get-or-create
/// pattern (Commit 1 §3) so each blocking syscall site is one line.
pub fn signal_wake_for(kernel: &Kernel) -> std::sync::Arc<tokio::sync::Notify> {
    kernel.process_state.signal_wake_for(kernel.tid)
}

/// Result of [`handle_signal_arm`]: whether the caller's blocked
/// syscall should return now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// No signal delivered; the caller's select! arm was spurious.
    /// Caller should re-park (wait4-style loop).
    None,
    /// An unmasked signal was pending; caller should return `-EINTR`.
    /// Also drives [`apply_terminate_if_needed`] internally.
    Interrupted,
}

/// Run `deliverable()` against `kernel` and translate the result into
/// a [`SignalOutcome`]. Used by every blocking-syscall signal arm.
/// (Terminate is treated as `Interrupted` for return-value purposes
/// because the dispatch pre-check will unwind the guest; the
/// exit-code side-effect is applied via [`apply_terminate_if_needed`].)
pub fn handle_signal_arm(kernel: &Kernel) -> SignalOutcome {
    let action = deliverable(kernel);
    match action {
        DeliveryAction::Ignore => SignalOutcome::None,
        DeliveryAction::Interrupt | DeliveryAction::Terminate(_) => {
            apply_terminate_if_needed(kernel, &Some(action));
            SignalOutcome::Interrupted
        }
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
            Ok(m) => *m,
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
            Ok(m) => *m,
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
            bytes[..SIGALTSTACK_SIZE as usize].fill(0);
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

    // --- deliverable() (ADR 0007) ---------------------------------------

    fn test_kernel() -> Kernel {
        Kernel::new_without_stdio(vec![], vec![])
    }

    fn enqueue(k: &Kernel, signo: i32) {
        k.process_state.signals_pending.lock().push(signo);
    }

    #[test]
    fn deliverable_empty_queue_is_ignore() {
        let k = test_kernel();
        assert_eq!(deliverable(&k), DeliveryAction::Ignore);
    }

    #[test]
    fn deliverable_sigterm_default_terminates() {
        let k = test_kernel();
        enqueue(&k, 15); // SIGTERM
        assert_eq!(deliverable(&k), DeliveryAction::Terminate(15));
        // Consumed.
        assert!(k.process_state.signals_pending.lock().is_empty());
    }

    #[test]
    fn deliverable_sigkill_bypasses_mask_and_disposition() {
        let mut k = test_kernel();
        // Block everything and install SIG_IGN for SIGKILL — both must be
        // ignored for the uncatchable signal.
        k.signals.mask = u64::MAX;
        k.signals.actions.insert(
            SIGKILL,
            SigAction {
                handler: SIG_IGN,
                ..Default::default()
            },
        );
        enqueue(&k, SIGKILL);
        assert_eq!(deliverable(&k), DeliveryAction::Terminate(SIGKILL));
    }

    #[test]
    fn deliverable_default_ignore_signals_are_dropped() {
        let k = test_kernel();
        enqueue(&k, SIGCHLD);
        enqueue(&k, SIGWINCH);
        assert_eq!(deliverable(&k), DeliveryAction::Ignore);
        assert!(k.process_state.signals_pending.lock().is_empty());
    }

    #[test]
    fn deliverable_sig_ign_is_dropped() {
        let mut k = test_kernel();
        k.signals.actions.insert(
            15,
            SigAction {
                handler: SIG_IGN,
                ..Default::default()
            },
        );
        enqueue(&k, 15);
        assert_eq!(deliverable(&k), DeliveryAction::Ignore);
    }

    #[test]
    fn deliverable_custom_handler_interrupts_without_running() {
        let mut k = test_kernel();
        k.signals.actions.insert(
            15,
            SigAction {
                handler: 0x4000, // some guest function pointer
                ..Default::default()
            },
        );
        enqueue(&k, 15);
        assert_eq!(deliverable(&k), DeliveryAction::Interrupt);
    }

    #[test]
    fn deliverable_masked_signal_stays_pending() {
        let mut k = test_kernel();
        k.signals.mask = 1u64 << (15 - 1); // block SIGTERM
        enqueue(&k, 15);
        assert_eq!(deliverable(&k), DeliveryAction::Ignore);
        // Still queued so a later rt_sigprocmask unblock can deliver it.
        assert_eq!(&*k.process_state.signals_pending.lock(), &[15]);
    }

    #[test]
    fn deliverable_sig_zero_is_probe_not_delivered() {
        let k = test_kernel();
        enqueue(&k, 0);
        assert_eq!(deliverable(&k), DeliveryAction::Ignore);
        assert!(k.process_state.signals_pending.lock().is_empty());
    }

    #[test]
    fn deliverable_preserves_fifo_after_early_terminate() {
        // A masked signal ahead of a terminate, plus an unexamined tail
        // signal, must both survive on the queue in order.
        let mut k = test_kernel();
        k.signals.mask = 1u64 << (10 - 1); // block SIGUSR1(10)
        enqueue(&k, 10); // masked → leftover
        enqueue(&k, 15); // SIGTERM → terminate, breaks the loop
        enqueue(&k, 12); // unexamined tail → leftover
        assert_eq!(deliverable(&k), DeliveryAction::Terminate(15));
        assert_eq!(&*k.process_state.signals_pending.lock(), &[10, 12]);
    }
}
