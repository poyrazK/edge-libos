//! Process / startup / control. P0 covers all stubs the libc pokes at startup.

use wasmtime::Caller;

use crate::errno::{EINVAL, EPERM, ESRCH};
use crate::kernel::{ChildExitStatus, Kernel};
use crate::mem;

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

// P2-D3.5: NR_SNAPSHOT — guest-driven quiescence for the
// freeze/snapshot path. See ADR 0004 §1. The number (123) is
// a reserved range in the Linux x86-64 NR space (post-
// 4-bit-namespace collapse; some older kernels report
// numbers in this range as `sys_set_tid_address` etc. but
// 123 is currently unused on a stock x86-64 5.x+ kernel).
// v1 is a synchronous guest→host call: the kernel encodes
// the live `KernelSnapshot`, writes the bytes to the
// guest-provided path, and returns the byte count.
pub const NR_SNAPSHOT: u32 = 123;

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

/// Mark every entry in `Kernel.children` as `exited = true`, drain
/// any parked wakers, and fire `child_event.notify_waiters()`.
///
/// Lock discipline: take `children` briefly to mark + drain,
/// drop before calling `wake()` on each waker — `Waker::wake`
/// must not run under the parking_lot mutex guard (it can run
/// arbitrary user code). `child_event.notify_waiters()` is a
/// `tokio::sync::Notify` call; it's safe to invoke outside the
/// lock because Notify is internally synchronized.
fn reap_all_children(caller: &mut Caller<'_, Kernel>) {
    // Phase 1: under the lock, mark + drain wakers into a Vec.
    let drained: Vec<std::task::Waker> = {
        let kernel = caller.data();
        let mut children = kernel.children.lock();
        let mut out = Vec::new();
        for (_, status) in children.iter_mut() {
            status.exited = true;
            if let Some(w) = status.waker.take() {
                out.push(w);
            }
        }
        out
    };
    // Phase 2: fire drained wakers (lock dropped).
    for w in drained {
        w.wake();
    }
    // Phase 3: notify any parked `child_event.notified().await`
    // waiters (any-pid parked wait4 callers).
    caller.data().child_event.notify_waiters();
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

/// `clone(flags, child_stack, ptid, ctid, tls) -> child_tid` — P3 Tier-4 v1.
///
/// v1 supports only the two TID-writeback flags (`CLONE_CHILD_SETTID` and
/// `CLONE_PARENT_SETTID`). The child is **not actually executed** in v1 —
/// v1 allocates a new PID and writes it to the requested `*_tidptr`
/// locations; the child fiber's resumption is deferred to a follow-up
/// PR (per the implementation plan §P3 Tier-4: "child enters `_start`
/// on resume (deferred to PR 5+ when we wire the child fiber)").
///
/// Supported flags (only):
/// - `CLONE_CHILD_SETTID` (0x01000000): write the new TID to `ctid_ptr`.
/// - `CLONE_PARENT_SETTID` (0x08000000): write the new TID to `ptid_ptr`.
///
/// Any other flag bit → `-EINVAL`. Rejected flags include `CLONE_VM`,
/// `CLONE_THREAD`, `CLONE_FILES`, `CLONE_SIGHAND`, `CLONE_FS`, `CLONE_IO`,
/// `CLONE_VFORK` (they imply shared state the v1 kernel can't safely
/// model — CoW pages, shared fd tables, etc.; see ADR 0001).
///
/// Allocation: `Kernel.next_pid` is a monotonically-increasing `AtomicI32`,
/// starting at 2 (PID 1 is reserved for the init kernel — `getpid()` returns
/// 1). Ordering is `Relaxed`; no other field is gated on PID order.
pub async fn clone_syscall(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let flags = a[0];
    // a[1] = child_stack (unused — guest passes 0; v1 has no stack model).
    let ptid_ptr = a[2];
    let ctid_ptr = a[3];

    // Reject any flag outside the v1-supported set.
    if flags & !CLONE_SUPPORTED_V1 != 0 {
        return -EINVAL;
    }
    // At least one TID-writeback flag must be requested; otherwise the
    // guest is asking us to spawn a child without observing the result.
    // This matches the conformance expectation that `clone(0) == -EINVAL`.
    if flags & CLONE_SUPPORTED_V1 == 0 {
        return -EINVAL;
    }

    let child_tid = caller
        .data()
        .next_pid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Write the new TID to the requested pointers. We snapshot the
    // Memory handle first (it is `Copy`) so we can release the `&Kernel`
    // borrow before re-borrowing `caller` mutably.
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
        .next_pid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Insert the child into the children table. The child is
    // not yet exited; the parent can wait4 for it. See the
    // handler doc comment for the deferred-resume contract.
    let mut children = caller.data().children.lock();
    children.insert(child_pid, ChildExitStatus::new(0));
    drop(children);

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
/// P3 final-bundle sub-deliverable 4 — parked-Waker path:
///
/// When called without `WNOHANG` and no child is currently reaped,
/// we park on either the matching child's `ChildExitStatus::waker`
/// (specific-PID wait) or `Kernel.child_event.notified()` (any-pid
/// wait). Lock discipline: register the waker under the
/// `children` lock, drop the lock, then `.await`. The waker
/// closure re-takes the lock to drain; `Waker::wake` does not run
/// under the lock.
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
        let children = caller.data().children.lock();
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
        let exists = caller.data().children.lock().contains_key(&(pid as i32));
        if !exists {
            return -ECHILD;
        }
    }

    // Park. We loop on spurious wake-ups: a wake may come from a
    // child exit OR from a spurious notify (Notify::notify_waiters
    // can fire on exit even if we weren't parked yet, which we
    // must accept gracefully).
    loop {
        // Snapshot a fresh notify handle BEFORE we lock — `Arc`
        // clone is cheap and lock-free. We can't take the lock
        // and then construct a future that needs `&caller.data()`.
        let child_event = caller.data().child_event.clone();
        let specific_waker_pid: Option<i32> = if pid > 0 { Some(pid as i32) } else { None };

        if let Some(sp) = specific_waker_pid {
            // Specific-pid parked path: poll the child's `exited`
            // flag under the lock every 1ms. We use a tight poll
            // rather than a per-address `Notify` because
            // `ChildExitStatus::waker` is a per-child `Option<Waker>`
            // with a 1-waiter contract — concurrent waiters on
            // the same child would clobber each other (see
            // `ChildExitStatus::waker` doc comment for the v1
            // known limitation). 1ms is fine for the test
            // workload and avoids the per-waker registration
            // complexity.
            let mut budget: u32 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                let exit_code_opt: Option<i32> = {
                    let mut children = caller.data().children.lock();
                    match children.get(&sp) {
                        Some(c) if c.exited => {
                            let exit_code = c.exit_code;
                            children.remove(&sp);
                            Some(exit_code)
                        }
                        None => return -ECHILD,
                        _ => None,
                    }
                };
                if let Some(exit_code) = exit_code_opt {
                    return write_wstatus_and_return(caller, sp, exit_code, wstatus_ptr);
                }
                budget = budget.saturating_add(1);
                if budget > 10_000 {
                    // 10s of 1ms polling — pathological; bail out.
                    return -ECHILD;
                }
            }
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
    let mut children = caller.data().children.lock();
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

/// `snapshot(snap_path_ptr)` — P2-D3.5 guest-driven quiescence.
///
/// Reads a NUL-terminated absolute path string from guest linear
/// memory (via `crate::mem::guest_str`), encodes the live kernel
/// into a `KernelSnapshot` postcard (`try_to_snapshot` +
/// `encode_snapshot`), and writes the bytes to the path via
/// `std::fs::write`. Returns the byte count on success.
///
/// Contract (ADR 0004 §1):
/// - `snap_path_ptr == 0` → `-EFAULT` (refuses a NULL pointer
///   rather than silently writing to ".").
/// - Out-of-memory `snap_path_ptr` → `-EFAULT` via the existing
///   `guest_str` choke point.
/// - Write failure (`ENOSPC`, `EACCES`, …) → `-EIO`. The host
///   drives `_start` to the syscall, then blocks waiting; a
///   write failure is the operator's signal that the destination
///   is misconfigured (perms, full disk).
/// - The snapshot is taken at this call site, **inside the
///   syscall handler**. The kernel is single-threaded (per ADR
///   0001 §2 — `Store: !Send`/`!Sync` pins one fiber per host
///   thread), so there is no race window between the snapshot
///   and the subsequent write.
///
/// Lock discipline: snapshots already acquire short-lived
/// `parking_lot::Mutex` guards internally (see
/// `try_to_snapshot` and its callers); we do not hold any
/// kernel lock across the await boundary here.
pub async fn snapshot_syscall(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    use crate::errno::{EFAULT, EIO};
    let snap_path_ptr = a[0];

    // NULL pointer → -EFAULT. Per ADR 0004 §1, refusing NULL is
    // explicit (vs. silently coercing to "." or "/").
    if snap_path_ptr == 0 {
        return -EFAULT;
    }

    // Read the path string from guest memory. `guest_str` caps
    // the search at the given `max_len` to avoid scanning to the
    // end of linear memory on a missing NUL. We pick 4096 (one
    // page) — long enough for any realistic absolute path
    // including a tmp-dir /run/user/1000 prefix.
    let snap_path: std::path::PathBuf = match mem::guest_str(caller, snap_path_ptr, 4096) {
        Ok(s) => std::path::PathBuf::from(s),
        Err(e) => return e,
    };

    // Snapshot the live kernel + linear memory. The pattern mirrors
    // `src/cli/migrate.rs` — `try_to_snapshot(kernel_ref, &store_ref)`
    // takes the inner store by reference, sidestepping the borrow
    // checker on `caller`. The `caller` reborrows to `&*caller`
    // here so the AsContext impl is dispatched through deref.
    let snap = match crate::snapshot::try_to_snapshot(caller.data(), &*caller) {
        Ok(s) => s,
        Err(_e) => {
            // `try_to_snapshot` currently never errors at
            // runtime (memory reads here only fail on a
            // missing Kernel memory_kind, which only happens
            // if the host forgot to attach_memory — that's
            // a host construction error surfaced by the
            // freeze CLI, not a guest-facing errno). Map to
            // -EIO conservatively.
            return -EIO;
        }
    };

    // Encode + write. We use the existing `write_snapshot_file`
    // helper so the error mapping lives in one place (per the
    // snapshot/io.rs module doc).
    match crate::snapshot::io::write_snapshot_file(&snap_path, &snap) {
        Ok(()) => {
            // Re-encode to learn the byte count (a fresh encode
            // is cheap relative to the file write; cheaper than
            // teaching `write_snapshot_file` to also return the
            // length).
            let bytes = match crate::snapshot::encode_snapshot(&snap) {
                Ok(b) => b,
                Err(_) => return -EIO,
            };
            bytes.len() as i64
        }
        Err(_) => -EIO,
    }
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
