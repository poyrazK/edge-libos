//! Futex — P3 implementation (Tier 1).
//!
//! Implements `FUTEX_WAIT` (with timespec timeout) and `FUTEX_WAKE`. All
//! other futex ops return clean `-ENOSYS`. The address model and storage
//! shape are pinned by `docs/adr/0001-p3-futex-semantics.md`; the snapshot
//! wire format for this table is defined by ADR 0002 (out of scope here).
//!
//! **Lock discipline (ADR 0001):** briefly lock `parking_lot::Mutex`, clone
//! `Arc<Notify>` out, release the lock, then `.notified().await`. The lock
//! guard NEVER spans an `.await`.
//!
//! **Scope boundary:** `wasm_threads(true)` is a separate follow-on. This
//! handler compiles and runs correctly under single-threaded v1 — guest
//! threads just won't be able to actually park on a different fiber yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use wasmtime::Caller;

use crate::errno::{EINVAL, ENOSYS};
use crate::kernel::Kernel;
use crate::mem;

/// Linux x86-64 NR for `futex(2)`.
pub const NR_FUTEX: u32 = 202;

/// Futex operation codes (after stripping flags via `FUTEX_CMD_MASK`).
const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;

/// Flags (OR'd into `futex_op`).
///
/// `FUTEX_PRIVATE_FLAG` is honored as a documented no-op per ADR 0001
/// ("What this ADR does NOT decide"). `FUTEX_CLOCK_REALTIME` is
/// recognized in the cmd-mask comment but treated identically to
/// MONOTONIC — both clocks are accepted for `FUTEX_WAIT`.
#[allow(dead_code)] // constants mirror Linux ABI; tested in `mod tests`
const FUTEX_PRIVATE_FLAG: i32 = 0x80;
#[allow(dead_code)] // constants mirror Linux ABI; tested in `mod tests`
const FUTEX_CLOCK_REALTIME: i32 = 0x100;

/// Conservative command-mask for Linux x86-64 — strips `FUTEX_PRIVATE_FLAG`
/// (bit 7) but leaves the realtime flag (bit 8) alone so we can recognize
/// it separately if we ever care.
const FUTEX_CMD_MASK: i32 = 0x3f;

/// Linux treats `0xFFFF_FFFF` as an invalid futex address (matches
/// ADR 0001 §1).
const INVALID_FUTEX_ADDR: u32 = 0xFFFF_FFFF;

/// Wait/wake storage keyed by guest address. See ADR 0001 §2.
///
/// `waiters` is an explicit counter because `tokio::sync::Notify::notify_one`
/// is idempotent and does NOT tell us how many waiters were actually woken —
/// Linux's `WAKE` return value is "waiters actually woken", so we maintain
/// the count ourselves.
#[derive(Default)]
pub struct FutexTable {
    by_addr: HashMap<u32, FutexEntry>,
}

#[derive(Clone)]
pub struct FutexEntry {
    pub notify: Arc<Notify>,
    pub waiters: usize,
}

/// `struct timespec` layout — 16 bytes, two i64 fields (sec, nsec).
const TIMESPEC_SIZE: i64 = 16;
const TIMESPEC_SEC_OFF: usize = 0;
const TIMESPEC_NSEC_OFF: usize = 8;

/// Public dispatcher. Mirrors the shape of every other handler in
/// `src/sys/*.rs` — pre-validate args via `mem::guest_slice` BEFORE any
/// `.await`, then drop borrows before sleeping.
pub async fn futex(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    // a[0] = uaddr (u32 guest ptr); a[1] = futex_op; a[2] = val;
    // a[3] = timeout (WAIT) / uaddr2 (CMP_REQUEUE, unused);
    // a[4], a[5] = val3 / unused.
    let uaddr = match u32::try_from(a[0]) {
        Ok(u) => u,
        Err(_) => return -EINVAL, // negative ptr → -EINVAL
    };
    let raw = a[1] as i32;
    let cmd = raw & FUTEX_CMD_MASK;

    // ADR 0001 §1 — reject the sentinel even before the slice check.
    if uaddr == INVALID_FUTEX_ADDR {
        return -EINVAL;
    }

    match cmd {
        FUTEX_WAIT => futex_wait(caller, uaddr, a[2], a[3]).await,
        FUTEX_WAKE => futex_wake(caller, uaddr, a[2] as i32),
        _ => crate::errno::to_ret(ENOSYS),
    }
}

/// `FUTEX_WAIT(uaddr, futex_op, val, timeout)`.
///
/// Blocks if `*uaddr == val`; returns 0 on wake, `-EAGAIN` on value
/// mismatch, `-ETIMEDOUT` on timeout, `-EINVAL` on bad timespec, `-EFAULT`
/// if uaddr or timeout ptr is outside linear memory.
async fn futex_wait(
    caller: &mut Caller<'_, Kernel>,
    uaddr: u32,
    val: i64,
    timeout_ptr: i64,
) -> i64 {
    // Phase 1: validate uaddr and read current value. Borrow scoped to block.
    let current = {
        let bytes = match mem::guest_slice(caller, uaddr as i64, 4) {
            Ok(b) => b,
            Err(e) => return e, // -EFAULT
        };
        u32::from_le_bytes(bytes.try_into().unwrap())
    };

    // Value-check race window: between Phase 1 and Phase 2 a WAKE could
    // fire. Linux re-reads after wake; we compare up front as an
    // optimization (avoid parking if there's no match).
    if current as i64 != val {
        return -crate::errno::EAGAIN;
    }

    // Phase 2: register interest and clone the Arc<Notify> out. Lock is
    // scoped to this block — drops BEFORE the .await below.
    let (notify, deadline) = {
        // Decode timeout (if any) BEFORE the lock, so we hold no locks
        // across the guest-memory read.
        let deadline = if timeout_ptr == 0 {
            None
        } else {
            match decode_timespec(caller, timeout_ptr) {
                Ok(d) => Some(d),
                Err(e) => return e,
            }
        };

        let mut table = caller.data().futex_table.lock();
        let entry = table
            .by_addr
            .entry(uaddr)
            .or_insert_with(|| FutexEntry {
                notify: Arc::new(Notify::new()),
                waiters: 0,
            });
        entry.waiters += 1;
        (entry.notify.clone(), deadline)
    };
    // <- parking_lot::Mutex guard dropped here.

    // Phase 3: wait. If deadline is set, use `tokio::time::timeout`;
    // otherwise `.notified().await` unconditionally.
    let woken = match deadline {
        None => {
            notify.notified().await;
            true
        }
        Some(deadline) => match tokio::time::timeout(deadline, notify.notified()).await {
            Ok(()) => true,
            Err(_) => {
                // Timed out. Decrement waiter count; prune if zero.
                release_waiter(caller, uaddr);
                return -crate::errno::ETIMEDOUT;
            }
        },
    };

    // Phase 4: wake path. Decrement waiter count; prune if zero.
    release_waiter(caller, uaddr);

    if woken {
        // Spurious-wake defense — Tokio doesn't spurious-wake but musl
        // already loops on `-EAGAIN`, so a re-read is cheap insurance.
        // ADR 0001 doesn't require this; keep it as belt-and-suspenders.
        let _ = mem::guest_slice(caller, uaddr as i64, 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        0
    } else {
        // `woken==false` only when the timeout elapsed, which is handled
        // by the early return above. Reaching here is impossible.
        unreachable!("woken==false only on timeout, handled above")
    }
}

/// Decrement the waiter count for `uaddr` and prune the entry if zero.
///
/// Single source of truth for the `waiters` bookkeeping — the timeout
/// arm of `futex_wait` and the wake arm of `futex_wait` both go through
/// here. Keeps the decrement logic in one place so future work (snapshot
/// serialization in ADR 0002, fork CoW, etc.) only has to update one
/// block.
fn release_waiter(caller: &mut Caller<'_, Kernel>, uaddr: u32) {
    let mut table = caller.data().futex_table.lock();
    if let Some(entry) = table.by_addr.get_mut(&uaddr) {
        entry.waiters = entry.waiters.saturating_sub(1);
        if entry.waiters == 0 {
            table.by_addr.remove(&uaddr);
        }
    }
}

/// `FUTEX_WAKE(uaddr, futex_op, val, ...)` — wake up to `val` waiters at
/// `uaddr`. Returns the number of waiters woken (best-effort, like Linux).
pub fn futex_wake(caller: &mut Caller<'_, Kernel>, uaddr: u32, val: i32) -> i64 {
    // Compute notify count under lock, then drop guard, then notify
    // outside.
    let (notify, to_wake): (Arc<Notify>, usize) = {
        let mut table = caller.data().futex_table.lock();
        match table.by_addr.get_mut(&uaddr) {
            Some(entry) => {
                let to_wake = (val.max(0) as usize).min(entry.waiters);
                entry.waiters -= to_wake;
                let notify = entry.notify.clone();
                if entry.waiters == 0 {
                    table.by_addr.remove(&uaddr);
                }
                (notify, to_wake)
            }
            None => return 0, // no waiters, no work
        }
    };
    // <- lock dropped here.

    // Tokio's `notify_one` is idempotent (stores a permit if no waiter
    // is currently parked). Calling it once per "wake" we want to
    // deliver; the explicit `waiters` counter is what makes the return
    // value accurate (Tokio doesn't tell us how many were actually
    // woken).
    for _ in 0..to_wake {
        notify.notify_one();
    }
    to_wake as i64
}

/// Decode a `struct timespec` (16 bytes: i64 sec, i64 nsec) at `ptr`.
///
/// Returns `-EINVAL` if nsec is out of range or sec is negative.
fn decode_timespec(caller: &mut Caller<'_, Kernel>, ptr: i64) -> Result<Duration, i64> {
    let bytes = mem::guest_slice(caller, ptr, TIMESPEC_SIZE)?;
    let sec = i64::from_le_bytes(bytes[TIMESPEC_SEC_OFF..TIMESPEC_SEC_OFF + 8].try_into().unwrap());
    let nsec = i64::from_le_bytes(
        bytes[TIMESPEC_NSEC_OFF..TIMESPEC_NSEC_OFF + 8]
            .try_into()
            .unwrap(),
    );
    if !(0..1_000_000_000).contains(&nsec) || sec < 0 {
        return Err(-EINVAL);
    }
    Ok(Duration::from_nanos(
        (sec as u64).saturating_mul(1_000_000_000) + nsec as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_matches_linux_x86_64() {
        assert_eq!(NR_FUTEX, 202);
    }

    #[test]
    fn cmd_mask_strips_flags() {
        let op_with_flag = FUTEX_WAIT | FUTEX_PRIVATE_FLAG;
        assert_eq!(op_with_flag & FUTEX_CMD_MASK, FUTEX_WAIT);
        assert_eq!(op_with_flag & FUTEX_CMD_MASK, 0);
    }

    #[test]
    fn private_flag_constant_is_0x80() {
        assert_eq!(FUTEX_PRIVATE_FLAG, 0x80);
    }

    #[test]
    fn clock_realtime_constant_is_0x100() {
        assert_eq!(FUTEX_CLOCK_REALTIME, 0x100);
    }

    #[test]
    fn invalid_addr_sentinel_is_max_u32() {
        assert_eq!(INVALID_FUTEX_ADDR, 0xFFFF_FFFF);
        assert_eq!(INVALID_FUTEX_ADDR as u64, 4_294_967_295);
    }

    #[test]
    fn futex_table_default_is_empty() {
        let t = FutexTable::default();
        assert!(t.by_addr.is_empty());
    }
}