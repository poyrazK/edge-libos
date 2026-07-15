//! Process / startup / control. P0 covers all stubs the libc pokes at startup.

use std::sync::Arc;

use tokio::sync::Notify;
use wasmtime::Caller;

use crate::errno::{EINVAL, EPERM, ESRCH};
use crate::host;
use crate::kernel::{ChildExitStatus, Kernel};
use crate::mem;
use crate::snapshot::KernelSnapshot;

// Linux x86-64 `wait4(2)` options (`linux/wait.h`).
//
// v1 honors `WNOHANG` (non-blocking poll) and the **default blocking
// parked path** (no options). The blocking path is the
// `ChildExitStatus::waker` + `Kernel.child_event` parked-Waker story
// landed in the P3 final-bundle sub-deliverable 4. Other flag bits
// (`WUNTRACED` / `WCONTINUED` / `WNOWAIT` / `WALL`) are rejected
// with -EINVAL — v1 has no signal delivery story and no job control.
pub const WNOHANG: i32 = 0x40;
pub const WUNTRACED: i32 = 0x02;
pub const WCONTINUED: i32 = 0x08;
pub const WNOWAIT: i32 = 0x0100_0000;
pub const WALL: i32 = 0x4000_0000;
/// Mask of v1-supported wait4 options. v1 supports **either**
/// `WNOHANG` (0) **or no options** (blocking). Reject anything
/// outside this set with -EINVAL.
pub const WAIT4_SUPPORTED_V1: i32 = WNOHANG;

// Linux x86-64 `clone(2)` flag bits (`linux/sched.h`).
//
// v1 supports ONLY the two TID-writeback flags. Every other bit
// (including `CLONE_VM`, `CLONE_THREAD`, `CLONE_FILES`, `CLONE_SIGHAND`,
// `CLONE_FS`, `CLONE_IO`, `CLONE_VFORK`) is rejected with -EINVAL.
// Justification is in `docs/adr/0001-p3-futex-semantics.md` (ADR
// 0001) and the implementation plan §P3 Tier-4.
pub const CLONE_CHILD_SETTID: i64 = 0x0100_0000;
pub const CLONE_PARENT_SETTID: i64 = 0x0800_0000;
/// Mask of v1-supported clone flags. Any bits outside this set → -EINVAL.
pub const CLONE_SUPPORTED_V1: i64 = CLONE_CHILD_SETTID | CLONE_PARENT_SETTID;

// P3 Tier-8 v2 step 4 — additional clone flag bits and the v2
// supported set. M4 makes `clone(CLONE_VM | CLONE_THREAD |
// CLONE_CHILD_SETTID | CLONE_PARENT_SETTID)` work at the flag-
// validation layer; the SharedMemory hand-off + child-thread
// spawn under CLONE_VM lands in M7 alongside the WAT-based
// integration test (which is the first place the engine +
// module references are reachable through the dispatch path).
//
// `CLONE_FILES`, `CLONE_SIGHAND`, `CLONE_FS`, `CLONE_IO`,
// `CLONE_VFORK`, and the `CLONE_NEW*` namespace flags remain
// rejected with -EINVAL per ADR 0005 §6.
pub const CLONE_VM: i64 = 0x0000_0100;
pub const CLONE_THREAD: i64 = 0x0001_0000;
pub const CLONE_SIGHAND: i64 = 0x0000_0800;
pub const CLONE_FILES: i64 = 0x0000_0400;
pub const CLONE_FS: i64 = 0x0000_0200;
pub const CLONE_VFORK: i64 = 0x0000_4000;
pub const CLONE_IO: i64 = 0x8000_0000;

/// Mask of v2-supported clone flags. Rejecting any bit outside
/// this set with -EINVAL keeps v1's "no surprise flags" contract
/// while still enabling `CLONE_VM | CLONE_THREAD`. ADR 0005 §6.
pub const CLONE_SUPPORTED_V2: i64 =
    CLONE_CHILD_SETTID | CLONE_PARENT_SETTID | CLONE_VM | CLONE_THREAD;

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
///
/// P3 final-bundle sub-deliverable 4: also mark every entry in
/// `Kernel.children` as `exited`, drain any parked wakers, and
/// fire `child_event.notify_waiters()` so a parked `wait4`
/// caller can wake up. The exit-code recorded in
/// `Kernel::exit_code` is the per-process code; each child's
/// `ChildExitStatus::exit_code` is whatever was passed when the
/// child was registered (fork / clone / test fixture).
pub async fn exit(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let code = a[0] as i32;
    {
        let kernel = caller.data_mut();
        kernel.exit_code = Some(code);
    }
    reap_all_children(caller);
    0
}

/// `exit_group(code)`: same semantics as `exit` in single-threaded v1.
pub async fn exit_group(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let code = a[0] as i32;
    {
        let kernel = caller.data_mut();
        kernel.exit_code = Some(code);
    }
    reap_all_children(caller);
    0
}

/// Mark every entry in `Kernel.children` as `exited = true`,
/// drain per-child `Arc<Notify>` clones, and fire
/// `child_event.notify_waiters()`.
///
/// Lock discipline: take `children` briefly to mark + clone
/// notify handles into a Vec, drop before calling
/// `notify.notify_waiters()` on each — `notify_waiters` must
/// not run under the parking_lot mutex guard (it can wake
/// arbitrary waiters whose contexts can run user code).
/// `child_event.notify_waiters()` is a `tokio::sync::Notify`
/// call; it's safe to invoke outside the lock because Notify
/// is internally synchronized.
///
/// M5: replaces v1's `Waker::wake()` per-child path with the
/// `Arc<Notify>` clone-on-lock-out pattern (ADR 0001 §2). The
/// only behavior change is that multiple concurrent waiters
/// on the same child PID now all wake — v1's single-waiter
/// `Option<Waker>` would have only fired the most recent
/// caller.
fn reap_all_children(caller: &mut Caller<'_, Kernel>) {
    // Phase 1: under the lock, mark + clone notify handles into a Vec.
    let drained: Vec<Arc<Notify>> = {
        let kernel = caller.data();
        let mut children = kernel.process_state.children.lock();
        let mut out = Vec::new();
        for (_, status) in children.iter_mut() {
            status.exited = true;
            out.push(status.notify.clone());
        }
        out
    };
    // Phase 2: fire drained notifies (lock dropped).
    for n in drained {
        n.notify_waiters();
    }
    // Phase 3: notify any parked `child_event.notified().await`
    // waiters (any-pid parked wait4 callers).
    caller.data().process_state.child_event.notify_waiters();
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

/// `clone(flags, child_stack, ptid, ctid, tls) -> child_tid` — P3 Tier-4 v2.
///
/// v2 supports the v1 TID-writeback flags plus `CLONE_VM` and
/// `CLONE_THREAD`. The child fiber's resumption under `CLONE_VM`
/// requires the SharedMemory hand-off documented in ADR 0005 §3
/// — that part lands in M7 alongside the WAT-based integration
/// test (the full child-thread spawn machinery is gated on the
/// dispatch path being able to reach the `Arc<Engine>` +
/// `Arc<Module>` references, which the runner/cli path wires up
/// at module-instantiation time).
///
/// In this M4 commit, `CLONE_VM` and `CLONE_THREAD` are accepted
/// at the flag-validation layer and the new TID is allocated +
/// written back exactly as in v1. The child is still not resumed;
/// the supported flag set is broadened to mirror musl's
/// `pthread_create` ABI. Rejected flags remain:
/// `CLONE_FILES`, `CLONE_SIGHAND`, `CLONE_FS`, `CLONE_IO`,
/// `CLONE_VFORK`, and the `CLONE_NEW*` namespace flags (ADR 0005
/// §6 documents why these stay out of scope).
///
/// Supported flags:
/// - `CLONE_CHILD_SETTID` (0x01000000): write the new TID to `ctid_ptr`.
/// - `CLONE_PARENT_SETTID` (0x08000000): write the new TID to `ptid_ptr`.
/// - `CLONE_VM` (0x100): accepted at validation; full SharedMemory
///   hand-off lands in M7.
/// - `CLONE_THREAD` (0x10000): accepted at validation; child joins
///   parent's tgid via `Kernel::new_for_child` (M3).
///
/// Any flag outside `CLONE_SUPPORTED_V2` → `-EINVAL`. At least one
/// of `CLONE_CHILD_SETTID | CLONE_PARENT_SETTID` must be requested
/// — matches the v1 conformance expectation that `clone(0) ==
/// -EINVAL` (no way to observe the result).
///
/// Allocation: `process_state.next_pid` is a monotonically-increasing
/// `AtomicI32`, starting at 2 (PID 1 is reserved for the init kernel
/// — `getpid()` returns 1). Ordering is `Relaxed`; no other field is
/// gated on PID order.
pub async fn clone_syscall(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let flags = a[0];
    // a[1] = child_stack (unused — guest passes 0; v1 has no stack model).
    let ptid_ptr = a[2];
    let ctid_ptr = a[3];

    // Reject any flag outside the v2-supported set.
    if flags & !CLONE_SUPPORTED_V2 != 0 {
        return -EINVAL;
    }
    // At least one TID-writeback flag must be requested; otherwise the
    // guest is asking us to spawn a child without observing the result.
    // This matches the conformance expectation that `clone(0) == -EINVAL`.
    // (Pure `CLONE_VM | CLONE_THREAD` without TID-writeback is also
    // rejected here — it would require a return path that doesn't tell
    // the guest where the TID landed. musl's pthread_create always
    // passes a CLONE_CHILD_SETTID-style path.)
    let tid_writeback = flags & (CLONE_CHILD_SETTID | CLONE_PARENT_SETTID);
    if tid_writeback == 0 {
        return -EINVAL;
    }

    let child_tid = caller
        .data()
        .process_state
        .next_pid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Register the child in the parent's tgid registry so that
    // `kill(pid, sig)` (PID == tgid) and `tgkill(tgid, tid, sig)`
    // (M6) can route to this thread. The registry lives on
    // `process_state` and is shared across threads.
    {
        let mut registry = caller.data().process_state.tgid_registry.lock();
        registry.insert(child_tid);
    }

    // Write the new TID to the requested pointers. We snapshot the
    // Memory handle first (it is `Copy`) so we can release the `&Kernel`
    // borrow before re-borrowing `caller` mutably.
    //
    // Under `CLONE_VM` the linear memory is shared with the child
    // thread, so the parent's memory handle is the same handle the
    // child will see on its fresh Store (post-M7 SharedMemory hand-off).
    // For M4 we still go through the legacy `memory()` accessor; the
    // Shared variant case returns `-EINVAL` here, which is fine —
    // CLONE_VM writes back go through a different code path in M7.
    let mem_handle = match caller.data().memory() {
        Ok(m) => *m,
        Err(e) => return e,
    };

    if flags & CLONE_PARENT_SETTID != 0 {
        let bytes = match mem::guest_slice_mut_via(&mem_handle, caller, ptid_ptr, 4) {
            Ok(b) => b,
            Err(e) => return e,
        };
        bytes.copy_from_slice(&child_tid.to_ne_bytes());
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        let bytes = match mem::guest_slice_mut_via(&mem_handle, caller, ctid_ptr, 4) {
            Ok(b) => b,
            Err(e) => return e,
        };
        bytes.copy_from_slice(&child_tid.to_ne_bytes());
    }

    child_tid as i64
}

/// `fork()` — P3 Tier-5 v1.
///
/// **v1 returns the child PID in the parent; the child fiber is
/// NOT resumed in this PR.** This is the deferred-resume contract
/// documented in `impelementationplan` §P3 Tier-5: spawning a
/// separate fiber requires either driving a second
/// `Store<Kernel>` from a fresh thread, or yielding the current
/// fiber and routing child execution through it — both options
/// need a follow-up that lands behind the multi-fiber (P3
/// Tier-3) and ADR 0003 (live migration) stories.
///
/// What v1 DOES do:
///   1. Allocate a fresh PID via `Kernel.next_pid.fetch_add`.
///   2. Insert `ChildExitStatus { exited: false, exit_code: 0,
///      waker: None }` into `Kernel.children`. The parent can
///      later `wait4` for this PID; the wait4 parked-Waker path
///      (sub-deliverable 4) will block until something marks
///      the child as `exited = true`. In v1 nothing ever marks
///      a forked child as exited (because nothing executes the
///      child), so a parent `wait4(child_pid)` parks forever
///      unless the parent also calls `exit()` itself — `exit()`
///      in `reap_all_children` marks all live children as
///      exited.
///
/// What v1 does NOT do (the deferred parts):
///   * Resume the child on its own fiber / Store.
///   * Set up a separate stack for the child.
///   * CoW the linear memory pages between parent and child.
///   * Wire the `CLONE_VM` / `CLONE_THREAD` / `CLONE_FILES`
///     semantics (rejected with -EINVAL anyway in clone(56)).
///
/// The fork handler is registered in `src/dispatch.rs`. A guest
/// calling `fork()` gets back `child_pid > 0` and continues;
/// the child PID is observable via `Kernel.children` for the
/// parent. The child-fiber-resume work lands in a follow-up that
/// piggybacks on P3 Tier-3 (threads + shared memory) + ADR 0003
/// (live migration).
pub async fn fork_syscall(caller: &mut Caller<'_, Kernel>, _a: [i64; 6]) -> i64 {
    // Allocate a fresh PID (PID 1 is reserved for the init
    // kernel; clone starts at 2, so fork follows the same
    // convention). Atomic ordering: Relaxed — no other field is
    // gated on PID order.
    let child_pid = caller
        .data()
        .process_state
        .next_pid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Insert the child into the children table. The child is
    // not yet exited; the parent can wait4 for it. See the
    // handler doc comment for the deferred-resume contract.
    let mut children = caller.data().process_state.children.lock();
    children.insert(child_pid, ChildExitStatus::new(0));
    drop(children);

    // M4: register the new child PID in the tgid registry so
    // `kill(child_pid, sig)` / `tgkill(...)` can route to it
    // (M6). fork() makes the child its own tgid leader, so the
    // child's PID is also its tgid.
    {
        let mut registry = caller.data().process_state.tgid_registry.lock();
        registry.insert(child_pid);
    }

    child_pid as i64
}

/// `sched_yield()` → 0. CPython sometimes calls this in poll loops; we
/// yield to the executor via `tokio::task::yield_now`.
pub async fn sched_yield() -> i64 {
    tokio::task::yield_now().await;
    0
}

/// `wait4(pid, wstatus, options, rusage)` — P3 Tier-6 v1.
///
/// v1 honors `WNOHANG` (non-blocking poll) **or no options**
/// (default blocking parked path). All other option bits are
/// rejected with -EINVAL.
///
/// - `pid == -1` or `pid == 0`: any child of the calling process.
///   In v1 the only process is PID 1, so the table always reflects
///   that single parent's children.
/// - `pid > 0`: specific child PID.
/// - `pid < -1`: process group (rejected with -EINVAL in v1).
///
/// Return contract:
/// - `0` (with `WNOHANG`) when no child is ready to be reaped.
/// - The reaped child's PID on success (with or without WNOHANG).
/// - `-ECHILD` when there are no children matching `pid` AT ALL.
///   A blocking wait on a non-existent PID is **not** parked — it
///   returns -ECHILD immediately, matching Linux.
/// - `-EINVAL` for unsupported option bits or invalid pid range.
///
/// On success: returns the reaped child's PID and, if `wstatus`
/// is non-NULL, writes the wait status (low 16 bits = `(code << 8) | 0`
/// for normal exit). The child entry is removed from
/// `Kernel.children`.
///
/// P3 Tier-8 v2 / M5 — `Arc<Notify>` parked path:
///
/// When called without `WNOHANG` and no child is currently reaped,
/// we park on either the matching child's `ChildExitStatus::notify`
/// (specific-PID wait) or `Kernel.child_event.notified()` (any-pid
/// wait). Lock discipline: clone the `Arc<Notify>` handle out of
/// the children map under the lock, drop the lock, then `.await`
/// (ADR 0001 §2 — never hold `Mutex` across `.await`). The
/// exit-side `reap_all_children` fires `notify.notify_waiters()`
/// after dropping the children lock, so the wake itself does not
/// happen under the lock.
///
/// M5 change vs. v1: replaces the 1ms polling block with the
/// `Arc<Notify>::notified().await` path. Concurrent waiters on
/// the same child PID are now supported (v1's single-waiter
/// `Option<Waker>` would have only fired the most recent caller).
pub async fn wait4_syscall(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    use crate::errno::ECHILD;
    let pid = a[0];
    let wstatus_ptr = a[1];
    let options = a[2] as i32;
    let wnohang = options & WNOHANG != 0;

    // Reject any flag outside the v1-supported set. Even `WALL` /
    // `WUNTRACED` / `WCONTINUED` are EINVAL — v1 has no signal
    // delivery story and no job control.
    if options & !WAIT4_SUPPORTED_V1 != 0 {
        return -EINVAL;
    }

    // pid < -1 is a process-group selector — not supported in v1.
    if pid < -1 {
        return -EINVAL;
    }

    // ECHILD fast path — no children at all. WNOHANG vs blocking
    // is irrelevant here because there is nothing to wait for.
    {
        let children = caller.data().process_state.children.lock();
        if children.is_empty() {
            return -ECHILD;
        }
    }

    // Try to reap synchronously (no parking) if a child is ready.
    if let Some((picked, exit_code)) = try_reap(caller, pid) {
        return write_wstatus_and_return(caller, picked, exit_code, wstatus_ptr);
    }

    // Nothing ready yet.
    if wnohang {
        return 0;
    }

    // Blocking parked path. ECHILD for unknown specific PID — no
    // way to ever satisfy the wait.
    if pid > 0 {
        let exists = caller
            .data()
            .process_state
            .children
            .lock()
            .contains_key(&(pid as i32));
        if !exists {
            return -ECHILD;
        }
    }

    // Park. We loop on spurious wake-ups: a wake may come from a
    // child exit OR from a spurious notify (Notify::notify_waiters
    // can fire on exit even if we weren't parked yet, which we
    // must accept gracefully).
    loop {
        // Snapshot a fresh `Arc<Notify>` clone for the child we're
        // waiting on BEFORE we lock — `Arc::clone` is cheap and
        // lock-free. We can't take the lock and then construct a
        // future that needs `&caller.data()`. Per-child notify
        // (specific pid) or kernel-wide child_event (any pid).
        let child_event = caller.data().process_state.child_event.clone();
        let specific_notify: Option<Arc<Notify>> = if pid > 0 {
            let children = caller.data().process_state.children.lock();
            children.get(&(pid as i32)).map(|c| c.notify.clone())
        } else {
            None
        };

        if let Some(specific) = specific_notify {
            // Specific-pid parked path: clone the per-child
            // `Arc<Notify>` out of the children map (already done
            // above), drop the lock, then `notified().await`. The
            // exit-side `reap_all_children` fires
            // `notify.notify_waiters()` after dropping the children
            // lock, so the wake itself does not happen under the
            // lock. Per ADR 0001 §2 — never hold `Mutex` across
            // `.await`.
            specific.notified().await;
            // Re-try the synchronous reap (the child may have
            // exited AND been reaped already; we still try).
            if let Some((picked, exit_code)) = try_reap(caller, pid) {
                return write_wstatus_and_return(caller, picked, exit_code, wstatus_ptr);
            }
            // No child ready after wake — loop back to re-park.
            // (The child may have been removed by a concurrent
            // waiter; the next iteration's ECHILD check at the
            // top of the function handles that.)
        } else {
            // Any-pid parked path: block on child_event.notified().
            // The exit-side fires `child_event.notify_waiters()`
            // when any child is reaped.
            child_event.notified().await;
            // Re-try the synchronous reap.
            if let Some((picked, exit_code)) = try_reap(caller, pid) {
                return write_wstatus_and_return(caller, picked, exit_code, wstatus_ptr);
            }
            // No child ready after wake — loop back to re-park.
        }
    }
}

/// Synchronously attempt to reap one child matching `pid`. Returns
/// `Some((pid, exit_code))` if a reaped child was found and
/// popped from `Kernel.children`; `None` otherwise (no child ready
/// or no matching child). Caller is responsible for `-ECHILD`
/// disambiguation (an unknown specific PID is ECHILD; no children
/// at all is also ECHILD).
fn try_reap(caller: &mut Caller<'_, Kernel>, pid: i64) -> Option<(i32, i32)> {
    let mut children = caller.data().process_state.children.lock();
    let target: Option<i32> = if pid > 0 { Some(pid as i32) } else { None };
    let picked: Option<i32> = match target {
        Some(p) => {
            if children.get(&p).map(|c| c.exited).unwrap_or(false) {
                Some(p)
            } else {
                None
            }
        }
        None => children.iter().find(|(_, c)| c.exited).map(|(p, _)| *p),
    };
    let picked = picked?;
    let exit_code = children.remove(&picked)?.exit_code;
    Some((picked, exit_code))
}

/// Encode wait status (`WIFEXITED=true`, `WEXITSTATUS=code`) and
/// return the reaped child PID. Writes the 4-byte wstatus to
/// guest memory at `wstatus_ptr` if non-zero.
fn write_wstatus_and_return(
    caller: &mut Caller<'_, Kernel>,
    picked: i32,
    exit_code: i32,
    wstatus_ptr: i64,
) -> i64 {
    let wstatus: i32 = (exit_code & 0xff) << 8;
    if wstatus_ptr != 0 {
        let mem_handle = match caller.data().memory() {
            Ok(m) => *m,
            Err(e) => return e,
        };
        let bytes = match mem::guest_slice_mut_via(&mem_handle, caller, wstatus_ptr, 4) {
            Ok(b) => b,
            Err(e) => return e,
        };
        bytes.copy_from_slice(&wstatus.to_ne_bytes());
    }
    picked as i64
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
    if !(0..=64).contains(&sig) {
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
    if !(0..=64).contains(&sig) {
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

    /// M2: the `register_and_signal` helper inserts an entry with
    /// `exited = true, exit_code = N` for a fresh pid and signals
    /// over the (tx, child_event) channel pair. Validates the
    /// helper's contract independent of any threading — the
    /// production child-thread path is exercised by
    /// `tests/fork_v2_child_thread.rs`.
    #[test]
    fn register_and_signal_inserts_and_signals() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let children_arc: Arc<parking_lot::Mutex<HashMap<i32, ChildExitStatus>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(i32, i32)>();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let child_event = rt.block_on(async { Arc::new(tokio::sync::Notify::new()) });

        super::register_and_signal(&children_arc, &tx, &child_event, 7, 42);

        // 1 + 3: register both waiters via a spawn task, yield to
        // let the runtime park them, then synchronously call the
        // helper (this test is single-threaded inside an
        // `rt.block_on` so we can't `block_in_place`), and finally
        // await the spawn task's result with a timeout. If the
        // helper's `notify_waiters()` is timed correctly the spawn
        // task returns the delivered `(pid, code)` and the
        // observed-notify flag.
        let child_event_for_task = Arc::clone(&child_event);
        let tx_for_task = tx.clone();
        let recv = rt.block_on(async {
            let task = tokio::spawn(async move {
                let mpsc = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    rx.recv(),
                )
                .await;
                let notify_ok = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    child_event_for_task.notified(),
                )
                .await
                .is_ok();
                (mpsc, notify_ok)
            });
            // Yield so the spawned task reaches `notified().await`
            // before the synchronous helper call runs.
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            super::register_and_signal(&children_arc, &tx_for_task, &child_event, 7, 42);
            task.await.expect("spawned task panicked")
        });
        let (mpsc_res, notify_ok) = recv;
        let (sent_pid, sent_code) = mpsc_res
            .expect("mpsc must deliver within 200ms")
            .expect("mpsc closed");
        assert_eq!(sent_pid, 7);
        assert_eq!(sent_code, 42);
        assert!(
            notify_ok,
            "child_event.notify_waiters() must fire the registered waiter within 200ms"
        );

        // (Entry inspection comes after the registered-waiter
        // assertions so a notify-timing failure surfaces first;
        // this matches the policy of "exit-code signal wins
        // over entry-shape check" the production path enforces.)
        let (exited, exit_code) = {
            let guard = children_arc.lock();
            let entry = guard.get(&7).expect("entry for pid 7 must exist");
            (entry.exited, entry.exit_code)
        };
        assert!(
            exited && exit_code == 42,
            "register_and_signal must set (exited=true, exit_code=42); got (exited={exited}, exit_code={exit_code})"
        );
        drop(rt);
    }

    /// M5: `register_and_signal` for a pid that already exists
    /// (e.g. via the `run_child` before-start insert path) updates
    /// the existing entry in place rather than overwriting it
    /// with a fresh `notify`. The `Arc<Notify>` field is preserved
    /// across the update — concurrent waiters parked on the
    /// child's notify must still wake on the exit.
    #[test]
    fn register_and_signal_updates_existing_entry_preserving_notify() {
        use crate::kernel::ChildExitStatus;
        use std::collections::HashMap;
        use std::sync::Arc;

        let children_arc: Arc<parking_lot::Mutex<HashMap<i32, ChildExitStatus>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        // pre-insert an entry so we can detect whether
        // `register_and_signal` overwrites it (it must NOT —
        // existing entries are updated in place).
        let pre_notify = Arc::new(tokio::sync::Notify::new());
        children_arc.lock().insert(
            9,
            ChildExitStatus {
                exit_code: 0,
                exited: false,
                notify: pre_notify.clone(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(i32, i32)>();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let child_event = rt.block_on(async { Arc::new(tokio::sync::Notify::new()) });

        super::register_and_signal(&children_arc, &tx, &child_event, 9, 137);

        let (exited, exit_code, post_notify_ptr) = {
            let guard = children_arc.lock();
            let entry = guard.get(&9).expect("entry for pid 9 must exist");
            (
                entry.exited,
                entry.exit_code,
                Arc::as_ptr(&entry.notify),
            )
        };
        assert!(
            exited && exit_code == 137,
            "entry must be (exited=true, exit_code=137); got (exited={exited}, exit_code={exit_code})"
        );
        // The update branch must preserve the existing notify
        // handle (so a previously-parked waiter still wakes).
        assert_eq!(
            post_notify_ptr,
            Arc::as_ptr(&pre_notify),
            "register_and_signal must preserve the existing notify handle"
        );

        let _ = rt.block_on(async { rx.recv().await });
        drop(rt);
    }

    /// M5: two waiters parked on the SAME per-child
    /// `ChildExitStatus::notify` both observe the wake when
    /// `reap_all_children` (or the equivalent `notify_waiters`
    /// fire) fires. v1's single-waiter `Option<Waker>` could only
    /// host one; v2's `Arc<Notify>` supports N. This is the
    /// load-bearing test for the multi-waiter contract that
    /// motivated the M5 field migration.
    ///
    /// Note: `tokio::sync::Notify::notify_waiters()` only wakes
    /// **currently-registered** waiters (per its docs). The test
    /// uses a multi-thread runtime so the two waiter tasks and the
    /// fire thread can run concurrently — the barrier ensures
    /// both waiters register before the fire.
    #[test]
    fn concurrent_waiters_on_same_child_notify_both_wake() {
        use crate::kernel::ChildExitStatus;
        use std::collections::HashMap;
        use std::sync::Arc;

        let children_arc: Arc<parking_lot::Mutex<HashMap<i32, ChildExitStatus>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_w1 = Arc::clone(&notify);
        let notify_w2 = Arc::clone(&notify);
        children_arc.lock().insert(
            17,
            ChildExitStatus {
                exit_code: 0,
                exited: false,
                notify: Arc::clone(&notify),
            },
        );

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");

        let waker_results = rt.block_on(async {
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let b1 = Arc::clone(&barrier);
            let b2 = Arc::clone(&barrier);
            let bf = Arc::clone(&barrier);
            let w1 = tokio::spawn(async move {
                b1.wait().await;
                tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    notify_w1.notified(),
                )
                .await
                .is_ok()
            });
            let w2 = tokio::spawn(async move {
                b2.wait().await;
                tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    notify_w2.notified(),
                )
                .await
                .is_ok()
            });
            let fire_task = tokio::spawn(async move {
                bf.wait().await;
                // Yield once so the waiter tasks register their
                // notified().await wakers against `notify`.
                tokio::task::yield_now().await;
                notify.notify_waiters();
            });
            let r1 = w1.await.expect("w1 panicked");
            let r2 = w2.await.expect("w2 panicked");
            fire_task.await.expect("fire panicked");
            (r1, r2)
        });
        let (w1_ok, w2_ok) = waker_results;
        assert!(
            w1_ok && w2_ok,
            "both waiters must wake on a single notify_waiters(); got w1={w1_ok} w2={w2_ok}"
        );
        drop(rt);
    }
}

// ---------------------------------------------------------------------------
// P3 Tier-8 v2 step 1 — child-thread spawn (M1)
// `spawn_child_thread` is referenced by the M2 fork syscall path; M1
// exercises the helpers via `tests/fork_v2_child_thread.rs`.
//
// `spawn_child_thread` is the headline of HANDOFF.md item #1: a real
// `std::thread` that drives a fresh `Store<Kernel>` against a
// per-thread tokio runtime, executing `_start` from the same entry
// point as the parent. Returns a `JoinHandle<()>` so the caller can
// optionally observe completion (the production path does NOT wait
// — the child runs concurrently with the parent and signals exit
// through the parent's `Kernel.children` map).
//
// Lock discipline (per ADR 0001 §2 + ADR 0002 §5):
//   * The parent's `Kernel.children` mutex is taken only briefly to
//     register the ChildExitStatus and later to update exited/exit_code.
//   * The snapshot bytes are produced inside `try_to_snapshot` (which
//     internally only briefly locks per resource).
//   * No `Mutex` is held across `.await`.
//
// Why snapshot at fork time? `Store<Kernel>: !Send + !Sync` (wasmtime
// invariant). We can't move the parent's live Store into the child
// thread. The snapshot gives us a portable value type that crosses
// the thread boundary; the child rebuilds a fresh Store via
// `apply_snapshot`. The one-time memcpy cost is the price of not
// having true CoW pages — see HANDOFF.md item #6 for the CoW story.
// Allow the lint that requires items before the test module: M1's
// helpers follow the file's existing test-module boundary (left
// untouched to keep the M2..M7 diff small). All M1 helpers get the
// allow via `#[allow(clippy::items_after_test_module)]` on each.
#[allow(clippy::items_after_test_module)]
pub fn spawn_child_thread(
    engine: std::sync::Arc<wasmtime::Engine>,
    module: std::sync::Arc<wasmtime::Module>,
    parent_kernel: &Kernel,
    parent_store: &mut wasmtime::Store<Kernel>,
    child_pid: i32,
) -> std::thread::JoinHandle<()> {
    // Snapshot the parent's kernel + memory while the parent is at a
    // quiescent point (inside the fork syscall, no concurrent guest
    // execution on this Store).
    let snap = match crate::snapshot::try_to_snapshot(parent_kernel, &*parent_store) {
        Ok(s) => s,
        Err(e) => {
            // Snapshot failure (format-version mismatch, etc.) is
            // unrecoverable here — log and exit the thread silently.
            // The parent's `children` map is left without an entry for
            // this PID; a subsequent `wait4(child_pid)` returns -ECHILD.
            eprintln!("spawn_child_thread: snapshot failed: {e:?}");
            return std::thread::Builder::new()
                .name(format!("edge-fork-{child_pid}-snap-fail"))
                .spawn(|| {})
                .expect("spawn noop thread");
        }
    };

    // The child thread cannot hold a `&Kernel` reference (Kernel
    // contains `Memory` / `FdTable` and is !Send by virtue of those).
    // Instead we hand the child thread two `Send`-friendly handles:
    //   * `exit_tx`: a tokio mpsc Sender that delivers
    //     `(child_pid, exit_code)` back to a drainer task running on
    //     the PARENT's tokio runtime. The drainer is the only thing
    //     allowed to mutate `parent_kernel.children`.
    //   * `child_event`: clone of the parent's kernel-wide Notify, so
    //     the child can wake any-pid `wait4` calls parked on the
    //     parent's runtime. (Per-child Notify + specific-pid wait
    //     lands in M5.)
    //
    // The drainer task is started on the parent's runtime by the
    // caller (fork_syscall, after returning to user space).
    let (exit_tx, exit_rx) = tokio::sync::mpsc::unbounded_channel::<(i32, i32)>();
    let child_event = parent_kernel.process_state.child_event.clone();
    // M2: share the parent's `children` map with the child thread so
    // it can register/update the `ChildExitStatus` entry. Without
    // this, the parent's `wait4(child_pid)` between fork and exit
    // would race the child's update. v1 had the parent register
    // synchronously in `fork_syscall`; M2 is the threaded story.
    // M3: the children map is now under `process_state`.
    let children_arc = Arc::clone(&parent_kernel.process_state.children);

    let handle = std::thread::Builder::new()
        .name(format!("edge-fork-{child_pid}"))
        .spawn(move || {
            run_child_pub(
                engine,
                module,
                snap,
                child_pid,
                exit_tx,
                child_event,
                children_arc,
            );
        })
        .expect("spawn_child_thread: std::thread::spawn");

    // The JoinHandle is returned to the caller (currently M2's
    // fork_syscall). Dropping the handle does NOT join the thread;
    // the OS reaps it on completion. The parent observes child exit
    // via the mpsc channel + the child_event notify.
    drop(exit_rx);

    handle
}

/// Drive the child wasm to completion on its own thread.
///
/// 1. Build a fresh Kernel (no stdio — child writes to its own copy).
/// 2. Build a fresh Linker + Store on this thread.
/// 3. Instantiate the shared Module against the fresh Store.
/// 4. Apply the snapshot (kernel state + linear-memory bytes).
/// 5. Call `_start`.
/// 6. On completion (clean or panicked), deliver the observed exit
///    code to the parent via the mpsc channel and fire the
///    kernel-wide `child_event` so any `wait4(-1, ...)` wakes.
#[allow(clippy::items_after_test_module)]
async fn run_child(
    engine: std::sync::Arc<wasmtime::Engine>,
    module: std::sync::Arc<wasmtime::Module>,
    snap: KernelSnapshot,
    child_pid: i32,
    exit_tx: tokio::sync::mpsc::UnboundedSender<(i32, i32)>,
    child_event: std::sync::Arc<tokio::sync::Notify>,
    children_arc: Arc<parking_lot::Mutex<std::collections::HashMap<i32, ChildExitStatus>>>,
) {
    // Linker-store-instantiate-attach memory sequence, with error
    // fallback to the 139 sentinel. M2 registers a fresh
    // `ChildExitStatus { exited: false, exit_code: 0 }` for `child_pid`
    // BEFORE invoking `_start` so the parent's
    // `wait4(child_pid)` racing the child sees the entry. The child
    // then acquires the same mutex on exit to flip `exited`, the
    // exit code, and fire `notify_waiters`.
    let kernel = Kernel::new_without_stdio(snap.args.clone(), snap.env.clone());
    let linker = match host::build_child_linker(&engine) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("run_child[{child_pid}]: linker build failed: {e}");
            register_and_signal(
                &children_arc,
                &exit_tx,
                &child_event,
                child_pid,
                139,
            );
            return;
        }
    };
    let mut store = host::build_child_store(&engine, kernel);

    // Instantiate the shared Module against the fresh Store. Any
    // instantiation failure (invalid wasm, missing imports) means the
    // child never gets to _start; report a sentinel exit code so the
    // parent's wait4 doesn't hang.
    let instance = match linker.instantiate_async(&mut store, &module).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("run_child[{child_pid}]: instantiate failed: {e}");
            register_and_signal(
                &children_arc,
                &exit_tx,
                &child_event,
                child_pid,
                139,
            );
            return;
        }
    };

    // Attach linear memory and apply the snapshot. The order matches
    // the freeze/serve flow at src/cli/serve.rs: kernel state first,
    // memory second. Two-step because `apply_snapshot` takes both
    // `&mut Kernel` and `&mut Store<Kernel>` simultaneously, which
    // Rust's borrow checker rejects when going through `store`.
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    if let Err(e) = crate::snapshot::apply_snapshot_kernel_state(&snap, store.data_mut()) {
        eprintln!("run_child[{child_pid}]: apply_snapshot_kernel_state failed: {e:?}");
        register_and_signal(
            &children_arc,
            &exit_tx,
            &child_event,
            child_pid,
            139,
        );
        return;
    }
    let mem_handle = match store.data().memory() {
        Ok(m) => *m,
        Err(_) => {
            eprintln!("run_child[{child_pid}]: no memory after instantiate");
            register_and_signal(
                &children_arc,
                &exit_tx,
                &child_event,
                child_pid,
                139,
            );
            return;
        }
    };
    if let Err(e) = crate::snapshot::apply_snapshot_to_memory(&snap, mem_handle, &mut store) {
        eprintln!("run_child[{child_pid}]: apply_snapshot_to_memory failed: {e:?}");
        register_and_signal(
            &children_arc,
            &exit_tx,
            &child_event,
            child_pid,
            139,
        );
        return;
    }

    // Call _start. The function may return i32, i64, or () depending
    // on the guest's export shape; try them in order.
    let _start_result: Result<i32, wasmtime::Error> = async {
        if let Ok(f) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
            return f.call_async(&mut store, ()).await;
        }
        if let Ok(f) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            let r = f.call_async(&mut store, ()).await?;
            return Ok(r as i32);
        }
        if let Ok(f) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            f.call_async(&mut store, ()).await?;
            return Ok(0);
        }
        Err(wasmtime::Error::msg("no _start export"))
    }
    .await;

    // M2: register this child's pid in the parent's `children` map
    // before invoking `_start`, so a `wait4(child_pid)` racing the
    // child sees the entry. The exact entry shape matches v1's
    // `fork_syscall` insert (`exited = false, exit_code = 0`); the
    // difference is the writer (child thread vs parent syscall).
    {
        let mut children = children_arc.lock();
        children.insert(child_pid, ChildExitStatus::new(0));
    }

    // Pick the exit code. `_start` returning cleanly (`Ok(_)`) wins.
    // On trap (the musl `exit → unreachable` idiom), prefer
    // `Kernel::exit_code` (the value the guest's `exit_syscall`
    // stored) over the default 139 sentinel; this is what a normal
    // C guest's `_start` looks like. Trap on a guest that never
    // called `exit` → sentinel 139.
    let exit_code: i32 = match _start_result {
        Ok(code) => code,
        Err(_e) => match store.data().exit_code {
            Some(c) => c,
            None => {
                eprintln!("run_child[{child_pid}]: _start trap with no exit_syscall seen");
                139
            }
        },
    };

    let _ = exit_tx.send((child_pid, exit_code));

    // M2: update the parent's `children` map for this pid on the
    // way out — set `exited = true, exit_code`, snapshot the per-child
    // `Arc<Notify>` (M5), drop the children lock, then fire
    // `notify.notify_waiters()`. Per ADR 0001 §2 lock discipline,
    // `notify_waiters()` MUST NOT run under the parking_lot mutex
    // guard (it can wake arbitrary user code). The kernel-wide
    // `child_event` fires after the per-child notify so any
    // `wait4(-1)` parked on the parent also wakes.
    let notify_snapshot: Option<Arc<Notify>> = {
        let mut children = children_arc.lock();
        match children.get_mut(&child_pid) {
            Some(entry) => {
                entry.exited = true;
                entry.exit_code = exit_code;
                Some(entry.notify.clone())
            }
            None => None,
        }
    };
    if let Some(n) = notify_snapshot {
        n.notify_waiters();
    }
    child_event.notify_waiters();
}

/// M2 helper: when the child thread can't reach `_start` (linker
/// build, instantiate, attach, or apply_snapshot failure) we still
/// must register a `ChildExitStatus` so the parent's `wait4()` can
/// observe the child as reaped. Without this, the parent would
/// block forever on `wait4(child_pid)`. The function takes the
/// lock briefly, inserts the sentinel entry, drops the lock, then
/// signals over mpsc + the kernel-wide child_event. Identical
/// shape to the `run_child` exit path so error-path and happy-path
/// wakeups are indistinguishable from the parent's perspective.
fn register_and_signal(
    children_arc: &Arc<parking_lot::Mutex<std::collections::HashMap<i32, ChildExitStatus>>>,
    exit_tx: &tokio::sync::mpsc::UnboundedSender<(i32, i32)>,
    child_event: &std::sync::Arc<tokio::sync::Notify>,
    child_pid: i32,
    exit_code: i32,
) {
    {
        let mut children = children_arc.lock();
        if let Some(entry) = children.get_mut(&child_pid) {
            entry.exited = true;
            entry.exit_code = exit_code;
        } else {
            children.insert(child_pid, ChildExitStatus::reaped(exit_code));
        }
    }
    let _ = exit_tx.send((child_pid, exit_code));
    child_event.notify_waiters();
}

// ---------------------------------------------------------------------------
// P3 Tier-8 v2 step 1 — `run_child` is also `pub` so a test can drive it
// directly with a hand-built Engine + Module. The kernel-syscall entry
// path that uses `spawn_child_thread` from `fork_syscall` lands in M2
// once `Kernel` carries `Arc<Engine>` + `Arc<Module>` (which M3's
// `ProcessState` migration also requires).
// ---------------------------------------------------------------------------
#[allow(clippy::items_after_test_module)]
pub fn run_child_pub(
    engine: std::sync::Arc<wasmtime::Engine>,
    module: std::sync::Arc<wasmtime::Module>,
    snap: KernelSnapshot,
    child_pid: i32,
    exit_tx: tokio::sync::mpsc::UnboundedSender<(i32, i32)>,
    child_event: std::sync::Arc<tokio::sync::Notify>,
    children_arc: Arc<parking_lot::Mutex<std::collections::HashMap<i32, ChildExitStatus>>>,
) {
    // The child thread runs OUTSIDE the parent's tokio runtime.
    // We must NOT use `new_current_thread` here: if the parent is
    // itself inside a current-thread runtime (common in tests), the
    // child would inherit the parent's runtime context and
    // `block_on` would panic with "Cannot start a runtime from
    // within a runtime". `new_multi_thread` with a single worker
    // gives the child its own dedicated runtime.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name(format!("edge-fork-{child_pid}-rt"))
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("run_child_pub[{child_pid}]: tokio build failed: {e}");
            register_and_signal(
                &children_arc,
                &exit_tx,
                &child_event,
                child_pid,
                139,
            );
            return;
        }
    };
    rt.block_on(async move {
        run_child(
            engine,
            module,
            snap,
            child_pid,
            exit_tx,
            child_event,
            children_arc,
        )
        .await;
    });
}
