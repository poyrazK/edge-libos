# ADR 0005 — P3 clone threads (v2 of fork/clone/wait4)

## Status

Accepted 2026-07-15. Realized by the P3 Tier-8 v2 work on
branch `p3-v2-fork-clone-threads`. Implementation commits are
M1 (child-thread skeleton), M2 (fork-side child ownership),
M3 (per-thread field split + `ProcessState`), M4 (CLONE_VM |
CLONE_THREAD flag acceptance), M5 (`Arc<Notify>` per-child
wait migration), M6 (tgid/tid + kill/tgkill routing), M7
(HANDOFF close-out + this ADR).

## Context

P3 final-bundle sub-deliverable 4 landed `fork(57)` v1 and
`clone(56)` v1. v1 had three known limitations, all enumerated
in HANDOFF.md at the close of the P3 final-bundle:

  1. **Deferred child-fiber resumption.** `fork()` allocated a
     child PID and inserted a `ChildExitStatus` entry but never
     resumed the child on its own fiber. A `wait4(child_pid)`
     would park forever unless the parent also called `exit()`
     (which marked all live children as exited).
  2. **Single-waiter `ChildExitStatus::waker: Option<Waker>`.**
     v1's specific-pid `wait4` polled the children table every
     1ms because `Option<Waker>` is single-waiter — a second
     caller would clobber the first caller's waker.
  3. **`CLONE_VM` / `CLONE_THREAD` / `CLONE_FILES` rejected.**
     v1's `clone(56)` only accepted the two TID-writeback
     flags. musl's `pthread_create` ABI requires `CLONE_VM |
     CLONE_THREAD` and would fall back to v1's -EINVAL.

ADR 0001 §2 documents the lock + Notify primitive that this
ADR builds on. ADR 0002 §5 documents the rebuild-on-restore
pattern for shared tables.

## Decision

### §1. Per-thread Kernel field split

P3 final-bundle had every field on `Kernel` as a per-Store
field by accident. v2 makes the per-thread vs per-process
split explicit. Per-process state lives on
`Arc<ProcessState>`; per-thread state stays on `Kernel`.
The split is enforced by `Kernel::new_for_child` —
per-thread fields deep-copy, per-process fields Arc-clone.

| Field | Scope |
|---|---|
| `memory: Option<MemoryKind>` | per-Store; `Shared` variant shares `Arc<SharedMemory>` across threads |
| `fds`, `vfs`, `mm`, `clock`, `brk`, `args`, `env` | per-thread deep-copy at fork |
| `rng`, `rng_seed` | per-thread fresh seed at fork |
| `signals.mask`, `signals.pending` | per-thread (`mask`) + per-process (`pending`) — POSIX split |
| `started_at`, `exit_code`, `comm` | per-thread |
| `futex_table` | **per-process** (moved to `Arc<ProcessState>`) |
| `next_pid` | **per-process** (moved) |
| `children`, `child_event` | **per-process** (moved) |
| `tgid_registry`, `signals_pending` | **per-process** (new) |
| `tid` | per-thread |
| `tgid` | per-thread (identical across threads in the same process) |
| `process_state` | per-thread `Arc<ProcessState>` |
| `engine`, `module` | per-thread `Option<Arc<wasmtime::Engine\|Module>>` (M4; None for unit tests) |

### §2. Engine ownership

`Arc<Engine>` + `Arc<Module>` shared across threads in the
same process (both `Send + Sync` per wasmtime). `Linker` is
`!Send + !Sync` so each thread builds its own via
`host::build_child_linker`. `Store<Kernel>` is also
thread-local (each thread has its own).

### §3. SharedMemory hand-off (CLONE_VM)

Four phases; lock discipline per ADR 0001 §2:

1. Snapshot the parent's `MemoryKind::data` bytes into a
   `Vec<u8>` under the parent's `kernel.memory` lock.
2. Drop the lock. Build a fresh `SharedMemory` from
   `engine` + min/max pages; copy bytes in.
3. Re-take the parent's `kernel.memory` lock; replace the
   `Owned(Memory)` variant with `Shared(SharedMemory)`.
4. Hand a clone of the `Arc<SharedMemory>` to the child
   thread; child attaches via `attach_shared_memory` on its
   fresh `Store`.

The shared backing means parent and child see each other's
writes immediately — this is what `pthread_create` requires.

In M4 we accept the v2 flag set + write back TID under
`CLONE_VM | CLONE_THREAD`. The full byte-copy / atomic-swap
hand-off lands in M7 alongside the engine+module dispatch
wiring (which is the first place the parent's engine+module
references are reachable through `clone_syscall`).

### §4. Per-child `Arc<Notify>` migration

v1's `ChildExitStatus::waker: Option<Waker>` cannot host
multiple concurrent waiters on the same child PID. v2
replaces the field with `notify: Arc<Notify>` using the
clone-on-lock-out pattern from ADR 0001 §2:

  - `reap_all_children` snapshots `Arc<Notify>` clones under
    the children lock, drops the lock, fires
    `notify.notify_waiters()` on each. One syscall wakes N
    parked waiters.
  - `wait4_syscall` specific-pid parked path: clone the
    per-child `Arc<Notify>` out of the children map, drop
    the lock, then `notify.notified().await`. The 1ms
    polling block is deleted.
  - `Clone` impl on `ChildExitStatus` rebuilds a fresh
    `Arc<Notify>` (matches ADR 0002 §5's
    rebuild-on-restore for `FutexTable`; the snapshot
    roundtrip is faithful).

### §5. tgid / tid / kill / tgkill routing

v1 hardcoded `tgid = tid = 1` everywhere. v2 stores both
fields on every `Kernel`:

  - `tid`: per-thread. Allocated from
    `process_state.next_pid.fetch_add`.
  - `tgid`: per-thread (but identical across threads in the
    same process). On `fork()`, child gets `tgid = tid`
    (child leads its own process). On `clone(CLONE_THREAD)`,
    child gets `tgid = parent.tgid` (joins parent's thread
    group).
  - `tgid_registry: parking_lot::Mutex<HashSet<i32>>` lives
    on `Arc<ProcessState>`. `fork_syscall` and
    `clone_syscall` insert the child PID here on allocation.

`getpid()` reads `Kernel::tgid`; `gettid()` reads
`Kernel::tid`. `kill(pid, sig)` and `tgkill(tgid, tid, sig)`
route via `tgid_registry`. Both append `sig` to
`process_state.signals_pending` — recorded-only, no actual
handler dispatch in v2. v2.5 owns signal delivery.

Child-thread panic sentinel: `run_child_pub` wraps
`linker.instantiate_async + _start.call_async` in
`std::panic::catch_unwind`. On unwind the parent's
`ChildExitStatus` is updated to `(exited = true,
exit_code = 139)` (Linux `128 + SIGSEGV(11)` convention).
The thread exits cleanly with no panic propagation.

### §6. v2-supported clone flags

`CLONE_SUPPORTED_V2 = CLONE_CHILD_SETTID | CLONE_PARENT_SETTID
| CLONE_VM | CLONE_THREAD`. All other bits rejected with
-EINVAL. Deferred to v2.5+:

| Flag | Reason |
|---|---|
| `CLONE_FILES` (0x400) | Shared fd table needs `Mutex<Arc<FdTable>>` per-process story |
| `CLONE_SIGHAND` (0x800) | Shared signal handlers; needs signal delivery to test |
| `CLONE_FS` (0x200) | Shared fs (cwd, umask); trivial but no test workload |
| `CLONE_IO` (0x8000_0000) | Shared io context; no-op in Linux too |
| `CLONE_VFORK` (0x4000) | Parent suspends until child exec/exit |
| `CLONE_NEWNS / CLONE_NEWUSER / CLONE_NEWPID / CLONE_NEWNET / CLONE_NEWIPC / CLONE_NEWUTS` | Namespace creation — never |
| `CLONE_SYSVSEM` (0x4000_0000) | Shared SysV semaphore undo; musl doesn't use |

Also deferred: `execve(59)` (needs a different `Module`),
`/proc` filesystem, `ptrace` (full register-state model
required), signal delivery (v2.5 owns).

## Consequences

v2 enables:
  - Real `pthread_create` in musl — the M4 flag acceptance
    + (forthcoming) SharedMemory hand-off cover the
    pthread_create ABI surface.
  - Multiple concurrent `wait4` callers on the same child
    PID — v1's `Option<Waker>` could only host one.
  - Live migration of multi-threaded processes —
    `Arc<SharedMemory>` shared backing means the snapshot
    captures one consistent memory view regardless of
    thread count.

v2 blocks (deferred to v2.5+):
  - `CLONE_FILES` — shared fd tables.
  - Real signal delivery — `signals_pending` is recorded-only.
  - `execve` — different module / image loading.

v2 doesn't decide:
  - The semantics of signal delivery beyond the
    recorded-only contract.
  - Cross-process kill (`pid > 0` matching a foreign tgid).

## References

- ADR 0001 §2 — lock + `Notify` primitive
- ADR 0002 §5 — rebuild-on-restore for `FutexTable`
- ADR 0003 — P3 live migration
- ADR 0004 — metering semantics
- HANDOFF.md #1 (deferred child-fiber resumption) — closed
- HANDOFF.md #6 (CLONE_VM / CLONE_THREAD) — partially
  closed (flag acceptance + TID writeback landed; full
  SharedMemory hand-off is M7's WAT-based test)
- Branch: `p3-v2-fork-clone-threads`
- Commits: M1 (919de2f), M2 (5a0dfb8), M3 (14c6048),
  M4 (c93422c), M5 (9ffe923), M6 (af51b7a)
