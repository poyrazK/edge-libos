# ADR 0001 — P3 futex semantics

- **Status.** Accepted, 2026-07-14 (realized by P3 Tier-3: `wasm_threads` +
  `shared_memory` + `wasm_shared_everything_threads` flip in
  `src/host.rs::build_engine`, with `"threads"` added to the wasmtime
  feature list in `Cargo.toml:22`). The Tier-1 handler landed via PR #10
  on the same date. **§Consequences "What this ADR enables"** — the
  snapshot wire form `(u32 addr, u32 waiter_count)` per pair with
  `Notify` rebuilt on restore — was realized by P3 Tier-2 on branch
  `p3-t2-futex-snapshot`: `FutexTable::snapshot()` /
  `FutexTable::rebuild_from_snapshot()` in `src/sys/futex.rs`, plus
  a new last-of-struct `futex_table` field on `KernelSnapshot` in
  `src/snapshot.rs`.
- **Phase.** P3 (per `impelementationplan` §7).
- **Scope.** `futex(2)` (NR 202) integration contract with the existing
  per-fd `tokio::sync::Notify` scheme established in P1-7.

## Context

The kernel is single-threaded for v1 (`impelementationplan` §1.4): one
guest, one fiber, no `wasm_threads`, no shared memory. P3 turns on
real threads and shared memory so multi-threaded guests (threadpool
libs, Java `Object.wait`, Go runtime, Rust `parking_lot`-style
primitives) can run. `futex(2)` is the kernel-side primitive these
guests hit.

P3 also touches `clone(56)` (which on Linux is the real
`pthread_create`-backing call) and `fork(57)` (CoW over linear memory).
Both depend on the futex hash table being correctly reachable and
addressable, so the futex design has to land first or alongside.

The kernel already has a working notification scheme (P1-7):
`SocketInner` carries `notify_read: Arc<Notify>` and
`notify_write: Arc<Notify>`; `EpollInner` carries `Notify` per entry;
`EventFdInner` carries one `Notify`. The futex table MUST plug into
the same scheme so that:

- existing `epoll_wait` subscribers don't get false wakes from futex
  activity,
- new "futex-aware" subsystems (none in v1, but P3+) can subscribe
  via the same primitive,
- the lock discipline stays `parking_lot::Mutex` for state,
  `tokio::sync::Notify` for wakes — never hold a `Mutex` guard across
  `.await`.

## Decision

Three concrete commitments that P3 implementers MUST honor.

### 1. Address space — `u32` guest addresses only

`futex` operates on `u32` guest addresses only. This matches
`wasm32-musl`'s `sizeof(int) = 4` (see `wasm32-long-32bits` memory).
Reject `0xFFFF_FFFF` as invalid (matches Linux's "kernel treats
`0xFFFF_FFFF` as an invalid address for FUTEX_WAIT" convention).
Reject addresses outside attached linear memory with `-EFAULT` via
the existing `mem::guest_slice` choke point (`src/mem.rs`). Do NOT
support 64-bit futex addresses in v1 — `FUTEX2` with `FUTEX2_SIZE_U64`
is a future extension if anyone asks for it.

### 2. Wait/wake storage — `Mutex<HashMap<u32, Arc<Notify>>>`

A new field on `Kernel`:

```rust
// src/kernel.rs
pub futex_table: parking_lot::Mutex<FutexTable>,

pub struct FutexTable {
    /// One `Notify` per address; multiple waiters on the same address
    /// share it. Lazily inserted on first FUTEX_WAIT, removed on
    /// FUTEX_WAKE when no waiters remain.
    by_addr: HashMap<u32, Arc<tokio::sync::Notify>>,
}
```

Lock discipline (project-wide invariant): briefly with
`parking_lot::Mutex` to insert/lookup; clone the `Arc<Notify>` out;
release the lock; then `.notified().await` on the cloned handle.
Never hold the `Mutex` guard across `.await`.

`Arc<Notify>` is `Send + Sync`; sharing it between guest fibers
requires `wasm_threads(true)` and shared memory in the wasm module
itself. P3 will flip `host.rs::build_engine` to enable threads.

### 3. Integration with epoll — futex wake is NOT an epoll event

A futex wake does NOT fire any existing `epoll_wait` subscriber. If
a future guest wants to await a futex from inside an async event
loop, it goes through a dedicated fd we expose via `eventfd2`-like
primitives, then `epoll_ctl(EPOLL_CTL_ADD, futex_fd, ...)`:

```
futex_wait(addr, ...)         // blocks the calling guest thread
    ↓ (waker observes)
eventfd_write(futex_fd, 1)    // kernel-side bridge
    ↓ (Notify fires)
epoll_wait                    // wakes subscribers
```

v1 does NOT implement the bridge. P3 implementers MAY add it as a
follow-on; doing so MUST NOT cause spurious wakes for existing
`epoll_wait` subscribers on sockets/eventfd.

## Consequences

### What this ADR blocks

- P3 implementers cannot use a per-thread `Notify` (must be per-address
  shared) — this is the whole point of futex being useful for
  pthread mutexes.
- P3 implementers cannot store `tokio::sync::Mutex<HashMap<...>>` —
  the lock has to be `parking_lot::Mutex` to honor the never-across-await
  invariant.
- P3 implementers cannot route futex wakes through the per-fd
  `notify_read`/`notify_write` on `SocketInner` — those are for
  socket readiness, not futex.

### What this ADR enables

- The P2-D snapshot machinery (ADR 0002) can serialize the futex
  table by iterating `by_addr` and writing each `(u32 addr, u32
  waiter_count)` pair — `Notify` itself is rebuilt on restore by
  re-inserting the address with a fresh `Notify`. Waiter counts
  come from how many fibers hold a clone of the `Arc`.
- The P3 fork-via-snapshot implementation can CoW-share the futex
  table between parent and child by `Arc`-cloning the whole table
  (or by deep-copying and noting that `Notify` rebuilds on use).
- The P3 live x86→ARM migration (ADR 0002 consequence 2) can move
  the futex table without translating `Notify` handles, since
  `Notify` is rebuilt on the receiving side.

### What this ADR does NOT decide

- The exact set of `FUTEX_*` operation flags supported (`FUTEX_WAIT`,
  `FUTEX_WAKE`, `FUTEX_WAIT_BITSET`, etc.). P3 should start with
  the minimum CPython-relevant subset and add others behind the
  `-ENOSYS` fence that v1's reservation provides.
- `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE` semantics. Defer to a
  follow-on ADR if any guest needs them.
- Whether `FUTEX_PRIVATE_FLAG` is honored. Linux treats it as a
  hint; P3 should accept it (no-op) for compatibility.

## References

- `impelementationplan` §4.9 (Futex / threads) and §6
  (Snapshot / fork).
- `src/fd.rs::SocketInner::notify_read` / `notify_write` (P1-7).
- `src/sys/epoll.rs::compute_revents` (P1-7 wake primitives).
- `src/mem.rs::guest_slice` (EFAULT choke point).
- Memory: `wasm32-long-32bits` — `long` is 32 bits on wasm32-musl;
  decode with `int64_t` for 8-byte fields, but `futex` addresses
  are 4 bytes.
