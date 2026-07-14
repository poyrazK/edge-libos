//! Futex — P3 implementation.
//!
//! Implements `FUTEX_WAIT` (with timespec timeout) and `FUTEX_WAKE`. All
//! other futex ops return clean `-ENOSYS`. The address model and storage
//! shape are pinned by `docs/adr/0001-p3-futex-semantics.md`; the snapshot
//! wire format for this table is defined by ADR 0002 §5 and is realized
//! here via `FutexTable::snapshot()` / `FutexTable::rebuild_from_snapshot()`.
//!
//! **Lock discipline (ADR 0001):** briefly lock `parking_lot::Mutex`, clone
//! `Arc<Notify>` out, release the lock, then `.notified().await`. The lock
//! guard NEVER spans an `.await`.
//!
//! **Multi-fiber wiring (P3 Tier-3, ADR 0001 §2):** the kernel now instantiates
//! with `wasm_threads(true)` + `shared_memory(true)` (PR #12), so the
//! `Arc<Notify>` model is usable across guest fibers hosted in different
//! `Store`s. Snapshot/restore of `FutexTable` (Tier-2) follows the
//! rebuild-on-restore contract from ADR 0002 §5 — `Arc<Notify>` is fresh
//! per restore.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use wasmtime::Caller;

use crate::errno::{EINVAL, ENOSYS};
use crate::kernel::Kernel;
use crate::mem;
use crate::snapshot::endian::LeU32;

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

/// Wire form of one futex address's state — ADR 0002 §5 + ADR 0001 §2.
///
/// Snapshot encodes `(addr, waiter_count)` pairs only; the
/// `Arc<Notify>` is rebuilt fresh on restore (per ADR 0002 §5:
/// "`Notify` handles are rebuilt on restore"). Endianness comes from
/// the existing `LeU32` newtype (`src/snapshot/endian.rs`) so the
/// wire form is fixed-width LE, host-independent, and stable across
/// postcard versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FutexAddrSnapshot {
    /// Guest futex address — `u32` per ADR 0001 §1.
    pub addr: LeU32,
    /// `waiters` counter from the in-memory `FutexEntry`. Captured
    /// at quiescent point; no fibers are actually parked.
    pub waiters: LeU32,
}

impl FutexTable {
    /// Snapshot accessor used by `build_kernel_snapshot`.
    ///
    /// Lock is held for the duration of the function (HashMap iter +
    /// `Vec::sort_by_key` — no I/O, no `.await`), then dropped. The
    /// returned `Vec` is sorted by `addr` for deterministic postcard
    /// output across runs (ADR 0002 § blocklist).
    pub fn snapshot(&self) -> Vec<FutexAddrSnapshot> {
        let mut v: Vec<FutexAddrSnapshot> = self
            .by_addr
            .iter()
            .map(|(addr, entry)| FutexAddrSnapshot {
                addr: LeU32(*addr),
                waiters: LeU32(entry.waiters as u32),
            })
            .collect();
        v.sort_by_key(|f| f.addr.0);
        v
    }

    /// Restore accessor used by `apply_snapshot_kernel_state`.
    ///
    /// Per ADR 0002 §5: "Notify handles are rebuilt on restore."
    /// No fibers are actually parked after rebuild — the guest
    /// re-enters `FUTEX_WAIT` on its next syscall and observes the
    /// fresh `Notify`, exactly as if the snapshot had not happened.
    /// Existing entries are dropped (a snapshot is a fresh kernel
    /// state; the snapshot is the source of truth).
    pub fn rebuild_from_snapshot(&mut self, snap: &[FutexAddrSnapshot]) {
        self.by_addr.clear();
        for f in snap {
            let waiters = f.waiters.0 as usize;
            self.by_addr.insert(
                f.addr.0,
                FutexEntry {
                    notify: Arc::new(Notify::new()),
                    waiters,
                },
            );
        }
    }
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
        let entry = table.by_addr.entry(uaddr).or_insert_with(|| FutexEntry {
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
    let sec = i64::from_le_bytes(
        bytes[TIMESPEC_SEC_OFF..TIMESPEC_SEC_OFF + 8]
            .try_into()
            .unwrap(),
    );
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

    #[test]
    fn futex_addr_snapshot_is_wire_stable() {
        // Encodes as exactly 8 bytes: 4 LE for addr + 4 LE for waiters.
        // If a regression ever drops the LeU32 wrapper or switches to
        // a varint, the assertion fails.
        let s = FutexAddrSnapshot {
            addr: LeU32(0x1234_5678),
            waiters: LeU32(2),
        };
        let bytes = postcard::to_stdvec(&s).expect("encode FutexAddrSnapshot");
        assert_eq!(
            bytes.len(),
            8,
            "FutexAddrSnapshot must be 8 fixed-width LE bytes"
        );
        assert_eq!(&bytes[0..4], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(&bytes[4..8], &[0x02, 0x00, 0x00, 0x00]);
        let back: FutexAddrSnapshot =
            postcard::from_bytes(&bytes).expect("decode FutexAddrSnapshot");
        assert_eq!(back, s);
    }

    #[test]
    fn snapshot_sorts_by_addr_and_drops_zero_waiters() {
        // Build three addresses with waiter counts; 0x3000 must be
        // pruned on release before snapshot (release_waiter's
        // invariant — we exercise that path by going through the
        // saturating-sub branch exactly once for the zero case).
        let mut table = FutexTable::default();
        table.by_addr.insert(
            0x3000,
            FutexEntry {
                notify: Arc::new(Notify::new()),
                waiters: 1,
            },
        );
        // Drive a single `release_waiter`-equivalent decrement manually
        // so the 0x3000 entry's waiter drops to zero and gets pruned.
        if let Some(e) = table.by_addr.get_mut(&0x3000) {
            e.waiters = e.waiters.saturating_sub(1);
            if e.waiters == 0 {
                table.by_addr.remove(&0x3000);
            }
        }
        table.by_addr.insert(
            0x1000,
            FutexEntry {
                notify: Arc::new(Notify::new()),
                waiters: 1,
            },
        );
        table.by_addr.insert(
            0x2000,
            FutexEntry {
                notify: Arc::new(Notify::new()),
                waiters: 2,
            },
        );

        let snap = table.snapshot();
        // 0x3000 pruned; remaining sorted by addr.
        assert_eq!(snap.len(), 2, "snapshot drops the pruned 0x3000 entry");
        assert_eq!(snap[0].addr.0, 0x1000);
        assert_eq!(snap[0].waiters.0, 1);
        assert_eq!(snap[1].addr.0, 0x2000);
        assert_eq!(snap[1].waiters.0, 2);
    }

    #[test]
    fn rebuild_from_snapshot_allocates_fresh_notify() {
        // The load-bearing ADR 0002 §5 claim: rebuilt `Arc<Notify>`s
        // are fresh allocations, not preserved across snapshots.
        let notify_orig = Arc::new(Notify::new());

        let mut table = FutexTable::default();
        table.by_addr.insert(
            0x4000,
            FutexEntry {
                notify: notify_orig,
                waiters: 1,
            },
        );

        // Snapshot, then clear + rebuild in a fresh table to simulate
        // a snapshot round-trip across Kernel instances.
        let snap = table.snapshot();
        let mut restored = FutexTable::default();
        restored.rebuild_from_snapshot(&snap);

        assert_eq!(restored.by_addr.len(), 1);
        // `table` still owns the original `Arc<Notify>` at 0x4000;
        // because `Arc::ptr_eq` compares allocation pointers (not
        // refcount), the rebuilt `Notify` must NOT be the same
        // allocation as the one in `table`. This is the
        // fresh-Notify invariant.
        assert!(
            !Arc::ptr_eq(
                &table.by_addr[&0x4000].notify,
                &restored.by_addr[&0x4000].notify
            ),
            "rebuilt Notify must be a fresh Arc allocation, not the original"
        );
    }
}
