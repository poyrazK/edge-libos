//! The `Kernel` struct — the per-store state container.
//!
//! Every host syscall accesses the kernel through `Caller::data()` /
//! `Caller::data_mut()`. The `Kernel` owns the linear memory reference, the
//! fd table, the linear allocator, the rng, and the process-startup state.
//!
//! Step 4 of the P0 build order fleshes this out; the skeleton here is what
//! the dispatch table needs to compile.

use std::sync::atomic::{AtomicBool, AtomicI32};
use std::time::Instant;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Notify;

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use wasmtime::{Memory, SharedMemory, StoreContext, StoreContextMut};

/// P2-D1: 32-byte seed captured at construction so the RNG can be
/// deterministically reconstructed by `apply_snapshot`. Fits inside
/// postcard-encoded `KernelSnapshot::rng_seed` directly.
pub type RngSeed = [u8; 32];

/// P2-D3.4: the C conformance tests (`tests/conformance/syscall.h`)
/// write their `PASS`/`FAIL:<reason>` marker at offset 4096 in linear
/// memory, and `edge-cli trace` reads it back. See also
/// `tests/conformance/syscall.h:228` (parallel literal in C — kept
/// in sync manually).
pub const MARKER_ADDR: usize = 4096;

/// Length of the marker region the conformance tests may write into.
/// Bumped from 64 if the C side ever grows past it.
pub const MARKER_LEN: usize = 64;

use crate::fd::FdTable;
use crate::mm::LinearAllocator;
use crate::sys::futex::FutexTable;
use crate::sys::resolver::{ResolverConfig, ResolverState};
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClockState {
    pub boot_monotonic_ns: u64,
}

/// P3 Tier-8 v2 — `ProcessState`: the per-process (vs per-thread)
/// state container. Held by every thread in the process as
/// `Arc<ProcessState>` so writes from one thread (PID allocation,
/// child-exit registration, signal queue, futex table, tgid
/// registry) are visible to the others.
///
/// The split between per-process and per-thread state is described
/// in detail in the M3 commit message of branch
/// `p3-v2-fork-clone-threads` (see also ADR 0006 §1). The TL;DR:
/// every field on `Kernel` that is shared across threads in the
/// same process moves to `ProcessState`. Every field that is
/// per-thread (or per-`Store`) stays on `Kernel`. The split is
/// enforced by the contract of `clone()` on `Kernel`: per-thread
/// fields deep-copy, per-process fields `Arc::clone`.
///
/// ADR 0006 §1 documents the full table.
pub struct ProcessState {
    /// P3 Tier-4: monotonic PID counter for `clone()` and `fork()`.
    /// Starts at 2 because PID 1 is reserved for the init kernel
    /// (`getpid()` returns 1, matching Linux convention).
    pub next_pid: AtomicI32,
    /// P3 Tier-6 + M2: children table for `wait4`. Keyed by child
    /// PID. `Arc<Mutex<HashMap>>` so the child thread can register
    /// its own entry on the parent's map (the fork round-trip).
    pub children: Arc<parking_lot::Mutex<HashMap<i32, ChildExitStatus>>>,
    /// P3 Tier-6: per-kernel notifier for any-child wakeups. Fired
    /// by `exit()` / `exit_group()`; wakes any `wait4(-1)` parked
    /// on this kernel.
    pub child_event: Arc<Notify>,
    /// P3 — ADR 0001 §2: wait/wake storage keyed by guest-address.
    /// Multiple threads in the same process must wake the same
    /// address; the table is shared.
    pub futex_table: parking_lot::Mutex<FutexTable>,
    /// P3 Tier-8 v2 / M6: per-process pending-signal queue.
    /// Currently recorded-only; the v2.5 deliver path will drain
    /// this into a per-thread `signals.pending`. `kill(pid, sig)`
    /// and `tgkill(tgid, tid, sig)` append here.
    pub signals_pending: parking_lot::Mutex<Vec<i32>>,
    /// P3 Tier-8 v2 / M6: registry of tids in this process. Used by
    /// `kill(pid, sig)` to find all threads whose tgid matches
    /// `pid`. All threads in a process share the same tgid.
    pub tgid_registry: parking_lot::Mutex<HashSet<i32>>,
    /// P3 Tier-8 v2 / M6: the thread-group id (TGID) of this
    /// process. All threads in the same process share the same
    /// tgid; the init kernel has `tgid = 1`.
    pub tgid: i32,
    /// Signal-delivery (ADR 0007): per-tid wake handles. When a
    /// thread parks in a blocking syscall it clones its tid's
    /// `Arc<Notify>` out (lazy-created here) and adds it as a
    /// `select!` arm; `kill` / `tgkill` fire it after enqueuing the
    /// signal so the parked syscall re-checks `deliverable()` and
    /// returns `-EINTR`. Per-process scope is required because the
    /// signal *sender* runs on a different fiber than the target
    /// and cannot reach the target's `Kernel`. Runtime-only; never
    /// serialized (like `child_event`).
    pub signal_wakes: parking_lot::Mutex<HashMap<i32, Arc<Notify>>>,
    /// Signal-delivery (ADR 0007): host-driven freeze quiescence.
    /// `None` for a normal `run`; `edge-cli freeze` installs an
    /// `Arc<Notify>` here and fires it from a `SIGUSR1` listener.
    /// Blocking syscalls race it as an extra `select!` arm *only*
    /// when `Some`, and on that wake continue normally (NOT
    /// `-EINTR`) — the guest is left at a well-defined in-syscall
    /// quiescent point for the snapshot. Runtime-only; never
    /// serialized.
    pub quiesce_notify: Option<Arc<Notify>>,
    /// P2-DNS (ADR 0007): per-process resolver state. Cache +
    /// denylist + lazy backend are shared across all threads in the
    /// same process via this Arc, mirroring `futex_table`. Snapshot
    /// non-persistent: `apply_snapshot` rebuilds an empty
    /// `ResolverState::default()` on restore; the first NR_RESOLVE
    /// call after `serve` rebuilds the backend.
    pub resolver: parking_lot::Mutex<ResolverState>,
}

impl ProcessState {
    /// Signal-delivery (ADR 0007 §3): return the `Arc<Notify>` a thread
    /// with tid `tid` parks on, creating it on first use. A blocking
    /// syscall clones this out before `.await`ing so `kill`/`tgkill` can
    /// wake it; the sender and the target run on different fibers, so the
    /// handle must live on the shared `ProcessState`.
    pub fn signal_wake_for(&self, tid: i32) -> Arc<Notify> {
        self.signal_wakes
            .lock()
            .entry(tid)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Signal-delivery (ADR 0007 §3): wake the thread with tid `tid` (if
    /// it has ever parked). Mirrors the `reap_all_children`
    /// clone-then-drop-then-notify discipline — the `Arc<Notify>` is
    /// cloned out under the lock, the guard dropped, then
    /// `notify_one()` fires outside the lock (ADR 0001 §2). We
    /// use `notify_one()` instead of `notify_waiters()` because the
    /// sender (kill/tgkill) and the target (a blocking syscall's
    /// select! arm) often race — kill may fire before the target
    /// has registered its `.notified()` future. `notify_one()`
    /// stores a permit the *next* `notified()` poll consumes; that
    /// matches the pre-armed-signal test pattern (kill, then
    /// nanosleep).
    ///
    /// Lazy create: if no Notify exists for `tid` yet (i.e., the
    /// target hasn't parked in any blocking syscall), we create
    /// one. Without this, the very first kill before any blocking
    /// syscall would have nothing to fire on, and the wake would
    /// be lost. The created Notify is never garbage-collected
    /// (small constant cost — one Arc per tid that ever had a
    /// signal delivered) which is acceptable.
    pub fn wake_signal(&self, tid: i32) {
        let notify = self
            .signal_wakes
            .lock()
            .entry(tid)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone();
        notify.notify_one();
    }
}

pub struct Kernel {
    /// Linear memory reference. Attached post-instantiation.
    ///
    /// P3 Tier-3: the field holds a [`MemoryKind`] (regular `Memory` or
    /// `SharedMemory`) so the kernel can host guests that declare
    /// `(memory … shared)` (used by `i32.atomic.wait` /
    /// `memory.atomic.notify`). The legacy `memory()` accessor still
    /// returns `&Memory` and returns `-EINVAL` on the shared variant —
    /// syscall handlers don't care about the variant; only the snapshot
    /// read/write paths need both.
    pub memory: Option<MemoryKind>,
    pub fds: FdTable,
    pub vfs: Vfs,
    pub mm: LinearAllocator,
    pub clock: ClockState,
    pub brk: u32,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rng: SmallRng,
    /// P2-D1: 32-byte seed backing `rng` so snapshots are deterministic.
    /// Captured at construction; `apply_snapshot` rebuilds `rng` from it.
    pub rng_seed: RngSeed,
    pub signals: SignalState,
    pub started_at: Instant,
    /// Set by exit() / exit_group() syscalls. The host driver inspects this
    /// after each call returns and surfaces the code in its own exit code.
    pub exit_code: Option<i32>,
    /// Signal-delivery (ADR 0007): set to `true` when a
    /// default-terminating signal is delivered. `dispatch()` checks
    /// it at the top and short-circuits every subsequent syscall to
    /// `0`, so the guest's libc unwinds and the run path surfaces
    /// `exit_code` (`128 + signo`). Distinct from `exit_code`
    /// because an explicit `exit(0)` must NOT be treated as a
    /// signal-termination (this flag stays `false` for normal exit).
    /// Per-thread; not serialized.
    pub exit_requested: AtomicBool,
    /// P2-C2: prctl(PR_SET_NAME) writes here; PR_GET_NAME reads from here.
    pub comm: [u8; 16],
    /// P3 Tier-8 v2 / M3: per-thread thread id. `gettid()` reads
    /// from here. Set on construction (`Kernel::new*` sets it from
    /// `ProcessState::next_pid`); updated by `fork()` /
    /// `clone()` to the freshly-allocated pid.
    pub tid: i32,
    /// P3 Tier-8 v2 / M3: per-thread thread-group id. All threads
    /// in the same process share the same tgid. The init kernel
    /// has `tgid = tid = 1`. `getpid()` reads from here.
    pub tgid: i32,
    /// P3 Tier-8 v2 / M3: per-process state. Cloned (Arc-clone)
    /// into every thread in the same process. Replaces the v1
    /// direct fields (`children`, `child_event`, `futex_table`,
    /// `next_pid`) which all moved to `ProcessState` per ADR 0006
    /// §1.
    pub process_state: Arc<ProcessState>,
    /// P2 metering (ADR 0004 §4): monotonic CPU time consumed by
    /// the guest since the last `set_fuel` reset. Reported in `serve`'s
    /// per-request log line and in `bench`'s per-iter print; snapshotted
    /// so `serve` carries usage across restore.
    /// SNAPSHOT: include.
    pub cpu_ns: u64,
}

/// P3 Tier-3: the linear-memory handle stored on the kernel.
///
/// P3 final-bundle (see `docs/adr/0003-p3-live-migration.md` + this PR's
/// sub-deliverable 2) lets one `Kernel` host either a regular
/// `wasmtime::Memory` (the default — for guests without `(memory …
/// shared)`) or a `wasmtime::SharedMemory` (for guests that declare
/// `(memory … shared)` to use `i32.atomic.wait` /
/// `memory.atomic.notify`). Both variants expose the same byte-buffer
/// surface; the difference is in the wasmtime API: `Memory::data`
/// takes a `Store` reference (per-Store), while `SharedMemory::data`
/// returns `&[UnsafeCell<u8>]` (cross-Store safe). `MemoryKind`
/// abstracts over both with a single byte-buffer API that the
/// snapshot read/write paths can consume.
///
/// `MemoryKind` is a live-state field on `Kernel` and is **not**
/// part of `KernelSnapshot` — the snapshot carries the page
/// bytes (per ADR 0002 §3 sparse per-page layout), and the
/// memory handle itself is rebuilt by attaching the freshly-
/// instantiated `Memory` (or `SharedMemory`) via
/// `attach_memory` / `attach_shared_memory` after restore.
#[derive(Debug)]
pub enum MemoryKind {
    Owned(Memory),
    Shared(SharedMemory),
}

impl MemoryKind {
    /// Borrow the inner [`Memory`]. Returns `None` if this is the
    /// `Shared` variant.
    pub fn as_memory(&self) -> Option<&Memory> {
        match self {
            Self::Owned(m) => Some(m),
            Self::Shared(_) => None,
        }
    }

    /// Borrow the inner [`SharedMemory`]. Returns `None` if this is the
    /// `Owned` variant.
    pub fn as_shared_memory(&self) -> Option<&SharedMemory> {
        match self {
            Self::Owned(_) => None,
            Self::Shared(m) => Some(m),
        }
    }

    /// Borrow the linear-memory bytes as `&[u8]`. The `Owned` variant
    /// requires a `Store` (matching wasmtime's `Memory::data`
    /// signature); the `Shared` variant ignores the store argument
    /// (matching wasmtime's `SharedMemory::data` which returns
    /// `&[UnsafeCell<u8>]` without a store — safe because the backing
    /// pointer is stable for the lifetime of the `SharedMemory`).
    ///
    /// # Safety (Shared variant)
    ///
    /// The caller must treat the returned slice as if it were
    /// `&[UnsafeCell<u8>]` — concurrent guest fibers may modify
    /// the bytes. Snapshot/restore paths are single-threaded by
    /// construction (the freeze CLI is at a quiescent point;
    /// restore is on a fresh kernel with no live guest), so
    /// non-atomic access is safe there.
    pub fn data<'a, T: 'static>(&self, store: impl Into<StoreContext<'a, T>>) -> &'a [u8] {
        match self {
            Self::Owned(m) => m.data(store),
            Self::Shared(m) => unsafe {
                std::slice::from_raw_parts(m.data().as_ptr() as *const u8, m.data_size())
            },
        }
    }

    /// Borrow the linear-memory bytes as `&mut [u8]`. Same safety
    /// contract as [`Self::data`].
    pub fn data_mut<'a, T: 'static>(
        &self,
        store: impl Into<StoreContextMut<'a, T>>,
    ) -> &'a mut [u8] {
        match self {
            Self::Owned(m) => m.data_mut(store),
            Self::Shared(m) => unsafe {
                std::slice::from_raw_parts_mut(m.data().as_ptr() as *mut u8, m.data_size())
            },
        }
    }

    /// Grow the linear memory by `delta` wasm pages. `Owned` requires
    /// a store (per `Memory::grow`); `Shared` ignores it (per
    /// `SharedMemory::grow`, which mutates the shared backing
    /// directly).
    pub fn grow<T: 'static>(
        &self,
        store: impl wasmtime::AsContextMut<Data = T>,
        delta: u64,
    ) -> anyhow::Result<u64> {
        match self {
            Self::Owned(m) => m
                .grow(store, delta)
                .map_err(|e| anyhow::anyhow!("Memory::grow failed: {e:?}")),
            Self::Shared(m) => m
                .grow(delta)
                .map_err(|e| anyhow::anyhow!("SharedMemory::grow failed: {e}")),
        }
    }

    /// Byte length of the linear memory. `Owned` requires a store;
    /// `Shared` does not (per `SharedMemory::data_size`).
    pub fn data_size<T: 'static>(&self, store: impl wasmtime::AsContext<Data = T>) -> usize {
        match self {
            Self::Owned(m) => m.data_size(store),
            Self::Shared(m) => m.data_size(),
        }
    }
}

/// P3 Tier-6 + Tier-8 v2: per-child exit status recorded in
/// `Kernel.children`.
///
/// M5 replaces the v1 `waker: Option<Waker>` field with
/// `notify: Arc<Notify>`. Rationale: v1's single-waiter
/// `Option<Waker>` cannot safely host concurrent `wait4`
/// callers on the same child — a second caller would clobber
/// the first waker on `wait4_syscall`'s "park new waker"
/// branch. The `Arc<Notify>` clone-on-lock-out pattern (ADR
/// 0001 §2) supports multiple parked waiters: each waiter
/// clones an `Arc` out of the children map under the lock,
/// drops the lock, then calls `notify.notified().await`.
/// `notify_waiters()` (fired on exit / exit_group) wakes every
/// currently-registered waiter in one shot.
///
/// `Clone` rebuilds a fresh `Arc<Notify>` (per ADR 0002 §5
/// rebuild-on-restore) so the snapshot roundtrip path stays
/// correct.
pub struct ChildExitStatus {
    pub exit_code: i32,
    pub exited: bool,
    /// P3 Tier-8 v2 / M5: per-child notify, multiple waiters
    /// welcome. The handle is `Clone` (it's an `Arc`), so
    /// parked waiters can take a clone out of the children
    /// map under the mutex guard without holding the lock
    /// across `.await` (per ADR 0001 §2 lock discipline).
    pub notify: Arc<Notify>,
}

impl std::fmt::Debug for ChildExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildExitStatus")
            .field("exit_code", &self.exit_code)
            .field("exited", &self.exited)
            .field("notify_refcount", &Arc::strong_count(&self.notify))
            .finish()
    }
}

impl Clone for ChildExitStatus {
    /// Clone with a fresh `Arc<Notify>` — the snapshot rebuild
    /// path takes a `ChildExitStatus` by value and re-inserts
    /// it into the live map. The new entry must be observable
    /// to a fresh `wait4` caller; the rebuilt `Arc<Notify>`
    /// gives it a fresh notify primitive (matches ADR 0002
    /// §5's rebuild-on-restore for `FutexTable`).
    fn clone(&self) -> Self {
        Self {
            exit_code: self.exit_code,
            exited: self.exited,
            notify: Arc::new(Notify::new()),
        }
    }
}

impl ChildExitStatus {
    /// Fresh child entry — not yet exited, fresh notify. Use
    /// this for all kernel-side insertions (`fork`, `clone`,
    /// test setup).
    pub fn new(exit_code: i32) -> Self {
        Self {
            exit_code,
            exited: false,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Fresh child entry that's already reaped (test fixture
    /// helper — equivalent to `new(code)` then `mark_exited`).
    pub fn reaped(exit_code: i32) -> Self {
        Self {
            exit_code,
            exited: true,
            notify: Arc::new(Notify::new()),
        }
    }
}

impl Kernel {
    pub fn new(args: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self::new_with_preopen(
            args,
            env,
            std::env::current_dir().unwrap_or_else(|_| "/".into()),
        )
    }

    /// Build a Kernel with a specific preopen directory. The current working
    /// directory starts at the preopen.
    pub fn new_with_preopen(
        args: Vec<String>,
        env: Vec<(String, String)>,
        preopen: impl Into<std::path::PathBuf>,
    ) -> Self {
        let vfs = Vfs::new(preopen).unwrap_or_else(|_| Vfs {
            root: "/".into(),
            cwd: "/".into(),
        });
        Self::new_inner(args, env, vfs)
    }

    /// Construct a Kernel with no preloaded stdio. Tests that don't
    /// need guest I/O use this.
    pub fn new_without_stdio(args: Vec<String>, env: Vec<(String, String)>) -> Self {
        let vfs = Vfs {
            root: "/".into(),
            cwd: "/".into(),
        };
        Self::new_inner(args, env, vfs)
    }

    fn new_inner(args: Vec<String>, env: Vec<(String, String)>, vfs: Vfs) -> Self {
        let now = Instant::now();
        // P2-D1: capture the 32-byte seed used to construct the RNG.
        // Restoring from a snapshot feeds the same seed back through
        // `SmallRng::from_seed` to reproduce the same RNG state.
        let rng_seed = Self::fresh_rng_seed();
        let rng = SmallRng::from_seed(rng_seed);

        // M3: build a fresh `ProcessState` for the init kernel.
        // `tgid = 1` matches Linux convention (PID 1 is init, its
        // thread group is itself). The init kernel gets the first
        // process-state, all subsequent threads (fork/clone
        // threads) get an `Arc::clone` of this one.
        let process_state = Arc::new(ProcessState {
            next_pid: AtomicI32::new(2),
            children: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            child_event: Arc::new(Notify::new()),
            futex_table: parking_lot::Mutex::new(FutexTable::default()),
            signals_pending: parking_lot::Mutex::new(Vec::new()),
            tgid_registry: parking_lot::Mutex::new(HashSet::from([1])),
            tgid: 1,
            signal_wakes: parking_lot::Mutex::new(HashMap::new()),
            quiesce_notify: None,
            resolver: parking_lot::Mutex::new(ResolverState::default()),
        });

        Self {
            memory: None,
            fds: FdTable::with_buffered_stdio(),
            vfs,
            mm: LinearAllocator::new(),
            clock: ClockState {
                boot_monotonic_ns: 0,
            },
            brk: 0,
            args,
            env,
            rng,
            rng_seed,
            signals: SignalState::new(),
            started_at: now,
            exit_code: None,
            exit_requested: AtomicBool::new(false),
            comm: [0; 16],
            tid: 1,
            tgid: 1,
            process_state,
            cpu_ns: 0,
        }
    }

    /// M3: build a `Kernel` for a forked/cloned child thread.
    /// Differs from `new_inner` in that:
    ///   * `process_state` is the parent's `Arc<ProcessState>`
    ///     (shared, not a fresh one);
    ///   * `tid` is allocated from the parent's `next_pid`;
    ///   * `tgid` is the caller's `tgid` for `clone(CLONE_THREAD)`,
    ///     or `tid` for `fork()` (i.e. the child is its own thread
    ///     group leader). The decision is made by the caller;
    ///     this helper takes `tgid` as a parameter.
    ///   * `comm`, `exit_code`, `signals` follow the per-thread
    ///     clone contract from ADR 0006 §1.
    ///   * `args` / `env` / `rng_seed` are fresh per-thread.
    ///
    /// The caller (fork_syscall / clone_syscall) is responsible
    /// for updating the parent's `process_state.tgid_registry`
    /// and `process_state.next_pid`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_child(
        args: Vec<String>,
        env: Vec<(String, String)>,
        vfs: Vfs,
        process_state: Arc<ProcessState>,
        tid: i32,
        tgid: i32,
    ) -> Self {
        let now = Instant::now();
        let rng_seed = Self::fresh_rng_seed();
        let rng = SmallRng::from_seed(rng_seed);
        Self {
            memory: None,
            fds: FdTable::with_buffered_stdio(),
            vfs,
            mm: LinearAllocator::new(),
            clock: ClockState {
                boot_monotonic_ns: 0,
            },
            brk: 0,
            args,
            env,
            rng,
            rng_seed,
            signals: SignalState::new(),
            started_at: now,
            exit_code: None,
            exit_requested: AtomicBool::new(false),
            comm: [0; 16],
            tid,
            tgid,
            process_state,
            cpu_ns: 0,
        }
    }

    /// Generate a fresh 32-byte seed from the OS RNG.
    fn fresh_rng_seed() -> RngSeed {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        seed
    }

    /// P2-DNS (ADR 0007): apply the operator-supplied resolver
    /// configuration (denylist, TTL, timeout). Called from
    /// `src/cli/{run,serve}.rs` after parsing `EDGE_RESOLVER_*`
    /// env vars but before the guest runs.
    ///
    /// Must be called pre-`fork`/`clone` — uses `Arc::get_mut` on
    /// `process_state` to ensure the per-process resolver state
    /// isn't already shared with another thread. Mirrors the
    /// `Arc::get_mut` contract used elsewhere for per-Kernel
    /// one-shot setters.
    pub fn attach_resolver_config(&mut self, cfg: ResolverConfig) {
        let ps = Arc::get_mut(&mut self.process_state)
            .expect("attach_resolver_config: kernel already shared");
        let mut state = ps.resolver.lock();
        state.denylist = cfg.denylist;
        state.ttl_ms = cfg.ttl_ms;
        state.timeout_ms = cfg.timeout_ms;
    }

    /// Test-only: install a custom resolver backend (e.g.
    /// `StubResolver`). Used by `tests/resolve_conformance.rs` to
    /// exercise the resolve() handler without depending on real DNS
    /// or `TokioResolver`. Same pre-fork precondition as
    /// `attach_resolver_config`.
    pub fn attach_resolver_backend(
        &mut self,
        backend: Arc<dyn crate::sys::resolver::ResolverBackend>,
    ) {
        let ps = Arc::get_mut(&mut self.process_state)
            .expect("attach_resolver_backend: kernel already shared");
        ps.resolver.lock().backend = Some(backend);
    }

    /// Attach the linear memory. Called from instantiation setup for
    /// guests that declare a regular `(memory N)` (no `shared` flag).
    pub fn attach_memory(&mut self, mem: Memory) {
        self.memory = Some(MemoryKind::Owned(mem));
    }

    /// Attach a shared linear memory. Called from instantiation setup
    /// for guests that declare `(memory N M shared)` — required for
    /// `i32.atomic.wait` / `memory.atomic.notify`. The `SharedMemory`
    /// type is wasmtime's cross-Store-safe handle.
    pub fn attach_shared_memory(&mut self, mem: SharedMemory) {
        self.memory = Some(MemoryKind::Shared(mem));
    }

    /// P2-D3.5 (ADR 0004 §2): attach pre-opened TCP listener fds
    /// inherited from the parent process — typically via
    /// systemd-style socket activation. Each input pair is
    /// `(target_fd, source_fd)`:
    ///
    /// - `target_fd` — the kernel fd slot the inherited
    ///   listener will live at. This MUST match the fd number
    ///   the snapshot was taken at, since the guest's
    ///   `accept4(inherited_fd, ...)` reads back that exact
    ///   number from linear memory (the WAT freeze fixture
    ///   stores it at `memory\[300\]` for example).
    /// - `source_fd` — the parent's OS fd. We `dup` it (the
    ///   parent retains the original after we exit; matches
    ///   systemd's `dup2(2)`-on-inherit contract).
    ///
    /// For each pair we:
    ///   1. `dup` the source fd so we own an independent
    ///      handle.
    ///   2. Wrap the dup'd fd in a `tokio::net::TcpListener`
    ///      via `std::net::TcpListener::from_raw_fd` +
    ///      `tokio::net::TcpListener::from_std`.
    ///   3. Build a `SocketInner::from_inherited_listener`
    ///      (no bind step, `so_reuseaddr = true`,
    ///      `is_acceptor = true`) and wrap it in a
    ///      `SharedSocket`.
    ///   4. Insert as `Resource::Socket` at `target_fd` via
    ///      `FdTable::insert_at`.
    ///
    /// Returns a `Vec<(u32, SharedSocket)>` of the constructed
    /// listeners keyed by `target_fd` so callers can re-attach
    /// them after `apply_snapshot_kernel_state` resets `self.fds`
    /// — see
    /// [`crate::snapshot::apply_snapshot_inherited_listeners`].
    ///
    /// Lock discipline: `parking_lot::Mutex` on `self.fds`
    /// (already enforced by `FdTable::insert_at`); the fds lock
    /// is never held across `.await`. `libc::dup` is a sync
    /// syscall.
    #[allow(unsafe_code)]
    pub fn attach_inherited_listeners(
        &mut self,
        fds: &[(u32, i32)],
    ) -> Vec<(u32, crate::fd::SharedSocket)> {
        use crate::fd::{Resource, SockAddr, SocketInner};
        use std::os::unix::io::FromRawFd;
        let mut out = Vec::new();
        for &(target_fd, source_fd) in fds {
            if source_fd < 0 {
                continue;
            }
            // SAFETY: `libc::dup(source_fd)` returns a fresh owned
            // fd that we transfer ownership of into the
            // std::net::TcpListener below via `from_raw_fd`.
            // On drop, the TcpListener will close that fd.
            let dup_fd = unsafe { libc::dup(source_fd) };
            if dup_fd < 0 {
                continue;
            }
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(dup_fd) };
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let bound = match listener.local_addr() {
                Ok(std::net::SocketAddr::V4(v4)) => SockAddr::V4 {
                    port: v4.port(),
                    addr: v4.ip().octets(),
                },
                Ok(std::net::SocketAddr::V6(v6)) => SockAddr::V6 {
                    port: v6.port(),
                    addr: v6.ip().octets(),
                },
                Err(_) => continue,
            };
            let inner = SocketInner::from_inherited_listener(listener, bound);
            let shared: crate::fd::SharedSocket =
                std::sync::Arc::new(parking_lot::Mutex::new(inner));
            // `insert_at` returns `Err` if the fd is already
            // occupied; we silently skip those (the operator
            // inherited a duplicate, which is a config error
            // we don't want to crash on).
            let _ = self
                .fds
                .insert_at(target_fd, Resource::Socket(shared.clone()));
            out.push((target_fd, shared));
        }
        out
    }

    /// Borrow the linear memory (compatibility shim), or `-EFAULT` if
    /// not yet attached. Returns `-EINVAL` if the variant is
    /// `MemoryKind::Shared` — syscall handlers that don't take
    /// shared-memory args can keep using this accessor and surface
    /// `-EINVAL` consistently. The snapshot read/write paths use
    /// [`Kernel::memory_kind`] instead.
    pub fn memory(&self) -> Result<&Memory, i64> {
        match self.memory.as_ref() {
            None => Err(-(crate::errno::EFAULT)),
            Some(MemoryKind::Owned(m)) => Ok(m),
            Some(MemoryKind::Shared(_)) => Err(-(crate::errno::EINVAL)),
        }
    }

    /// Borrow the [`MemoryKind`] enum, or `-EFAULT` if not yet attached.
    /// Used by the snapshot read/write paths, which need to handle both
    /// the `Owned` and `Shared` variants.
    pub fn memory_kind(&self) -> Result<&MemoryKind, i64> {
        self.memory.as_ref().ok_or(-(crate::errno::EFAULT))
    }

    /// Clone the stdout buffer Arc (for draining after the guest exits).
    /// Returns None if fd=1 has been closed or replaced.
    pub fn stdout_buf(
        &self,
    ) -> Option<std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<u8>>>> {
        match self.fds.get(crate::fd::STDOUT) {
            Ok(crate::fd::Resource::Stdout(w)) => Some(w.buf.clone()),
            _ => None,
        }
    }

    /// Clone the stderr buffer Arc (for draining after the guest exits).
    pub fn stderr_buf(
        &self,
    ) -> Option<std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<u8>>>> {
        match self.fds.get(crate::fd::STDERR) {
            Ok(crate::fd::Resource::Stderr(w)) => Some(w.buf.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_seed_is_recorded_and_replays_state() {
        let mut k = Kernel::new_without_stdio(vec![], vec![]);
        let seed = k.rng_seed;
        // The same seed must produce the same RNG output on reconstruction.
        let mut replay = SmallRng::from_seed(seed);
        let mut live = SmallRng::from_seed(seed);
        let mut buf_replay = [0u8; 8];
        let mut buf_live = [0u8; 8];
        replay.fill_bytes(&mut buf_replay);
        live.fill_bytes(&mut buf_live);
        assert_eq!(buf_replay, buf_live, "replay RNG diverges from live");
        // And the seed captured on the kernel is itself the one used
        // to build the kernel's RNG, so re-seeding must match the live RNG.
        let mut should_be_live = SmallRng::from_seed(seed);
        let mut other = [0u8; 8];
        let mut ours = [0u8; 8];
        should_be_live.fill_bytes(&mut other);
        k.rng.fill_bytes(&mut ours);
        assert_eq!(ours, other, "kernel rng differs from from_seed(rng_seed)");
    }

    #[test]
    fn distinct_kernels_get_distinct_seeds() {
        let a = Kernel::new_without_stdio(vec![], vec![]);
        let b = Kernel::new_without_stdio(vec![], vec![]);
        assert_ne!(
            a.rng_seed, b.rng_seed,
            "two kernels should have distinct seeds"
        );
    }

    /// M3: two `Kernel`s that share an `Arc<ProcessState>` see the
    /// same `futex_table`. We exercise this indirectly by taking
    /// the lock on both kernels and verifying the `Arc` identity
    /// — the `Mutex<...>` pointer inside `process_state.futex_table`
    /// must be the SAME on both kernels. (`FutexTable` is a
    /// private struct in `sys::futex`; we can't poke into its
    /// entries from this test without exposing internals, but the
    /// shared-Mutex identity is sufficient for the M3 contract.)
    #[test]
    fn process_state_shares_futex_table_across_threads() {
        let parent = Kernel::new_without_stdio(vec![], vec![]);
        let child_ps = Arc::clone(&parent.process_state);
        let child = Kernel::new_for_child(
            vec![],
            vec![],
            crate::vfs::Vfs {
                root: "/".into(),
                cwd: "/".into(),
            },
            child_ps,
            2,
            2,
        );

        // The Mutex inside `futex_table` is the same pointer for
        // both kernels (because both share `process_state`).
        let p = &parent.process_state.futex_table as *const _;
        let c = &child.process_state.futex_table as *const _;
        assert_eq!(
            p, c,
            "futex_table Mutex<...> must be shared between threads in the same process"
        );

        // The child_event Notify must also share identity.
        let np = Arc::as_ptr(&parent.process_state.child_event);
        let nc = Arc::as_ptr(&child.process_state.child_event);
        assert_eq!(np, nc, "child_event must be the same Arc<Notify>");

        // The children Arc<Mutex<HashMap>> too.
        let cp = Arc::as_ptr(&parent.process_state.children);
        let cc = Arc::as_ptr(&child.process_state.children);
        assert_eq!(cp, cc, "children map must be the same Arc");
    }

    /// M3: a child kernel built via `new_for_child` shares the
    /// same `Arc<ProcessState>` as the parent. `Arc::strong_count`
    /// is exactly 2 after construction.
    #[test]
    fn fork_copies_per_process_state_to_child() {
        let parent = Kernel::new_without_stdio(vec![], vec![]);
        let parent_count = Arc::strong_count(&parent.process_state);
        let child = Kernel::new_for_child(
            vec![],
            vec![],
            crate::vfs::Vfs {
                root: "/".into(),
                cwd: "/".into(),
            },
            Arc::clone(&parent.process_state),
            2,
            2,
        );
        assert_eq!(
            Arc::strong_count(&parent.process_state),
            parent_count + 1,
            "Arc<ProcessState> must be shared between parent and child"
        );
        assert_eq!(
            child.process_state.tgid, parent.process_state.tgid,
            "clone(CLONE_THREAD) path: child tgid == parent tgid"
        );
        assert_eq!(
            child
                .process_state
                .next_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            parent
                .process_state
                .next_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            "next_pid is per-process; child sees parent's atomic"
        );
    }

    /// M3: a forked child has a fresh `rng_seed` — the parent's
    /// and child's RNG streams diverge even though they share
    /// `process_state`.
    #[test]
    fn fork_does_not_share_per_thread_rng_seed() {
        let parent = Kernel::new_without_stdio(vec![], vec![]);
        let parent_seed = parent.rng_seed;
        let mut child = Kernel::new_for_child(
            vec![],
            vec![],
            crate::vfs::Vfs {
                root: "/".into(),
                cwd: "/".into(),
            },
            Arc::clone(&parent.process_state),
            2,
            2,
        );
        assert_ne!(
            parent_seed, child.rng_seed,
            "child must get a fresh rng_seed, not the parent's"
        );

        // And the child's first random byte sequence is not
        // derivable from the parent's seed.
        let mut parent_replay = SmallRng::from_seed(parent_seed);
        let mut parent_buf = [0u8; 8];
        parent_replay.fill_bytes(&mut parent_buf);
        let mut child_buf = [0u8; 8];
        child.rng.fill_bytes(&mut child_buf);
        assert_ne!(
            parent_buf, child_buf,
            "child RNG output must differ from parent's first-8-bytes"
        );
    }
}
