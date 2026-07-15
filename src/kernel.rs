//! The `Kernel` struct — the per-store state container.
//!
//! Every host syscall accesses the kernel through `Caller::data()` /
//! `Caller::data_mut()`. The `Kernel` owns the linear memory reference, the
//! fd table, the linear allocator, the rng, and the process-startup state.
//!
//! Step 4 of the P0 build order fleshes this out; the skeleton here is what
//! the dispatch table needs to compile.

use std::sync::atomic::AtomicI32;
use std::time::Instant;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Notify;

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use wasmtime::{Memory, SharedMemory, StoreContext, StoreContextMut};

/// P2-D1: 32-byte seed captured at construction so the RNG can be
/// deterministically reconstructed by `apply_snapshot`. Fits inside
/// postcard-encoded `KernelSnapshot::rng_seed` directly.
pub type RngSeed = [u8; 32];

use crate::fd::FdTable;
use crate::mm::LinearAllocator;
use crate::sys::futex::FutexTable;
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClockState {
    pub boot_monotonic_ns: u64,
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
    /// P2-C2: prctl(PR_SET_NAME) writes here; PR_GET_NAME reads from here.
    pub comm: [u8; 16],
    /// P3 — ADR 0001 §2: wait/wake storage keyed by guest-address.
    /// See `docs/adr/0001-p3-futex-semantics.md`.
    pub futex_table: parking_lot::Mutex<FutexTable>,
    /// P3 Tier-4: monotonic PID counter for `clone()` and `fork()`.
    /// Starts at 2 because PID 1 is reserved for the init kernel
    /// (`getpid()` returns 1, matching Linux convention). Allocations
    /// are `Ordering::Relaxed` — no other field is gated on PID order.
    pub next_pid: AtomicI32,
    /// P3 Tier-6: children table for `wait4`. Keyed by child PID.
    /// `ChildExitStatus::exited == true` once the child has called
    /// `exit()` / `exit_group()`; `wait4` returns the cached exit
    /// code and removes the entry. Locked briefly — never held
    /// across `.await` (project lock discipline, see CLAUDE.md).
    pub children: parking_lot::Mutex<HashMap<i32, ChildExitStatus>>,
    /// P3 Tier-6: per-kernel notifier for any-child wakeups. Used
    /// by `wait4` to wait on `pid == -1` / `pid == 0` (any child)
    /// when the calling child hasn't exited yet. Reserved for the
    /// full parked-wait path (PR 3 ships WNOHANG-only semantics; a
    /// follow-up lands the blocking variant once PR 4's child
    /// fiber can actually call `exit`).
    pub child_event: Arc<Notify>,
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
    pub fn data<'a, T: 'static>(
        &self,
        store: impl Into<StoreContext<'a, T>>,
    ) -> &'a [u8] {
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

/// P3 Tier-6: per-child exit status recorded in `Kernel.children`.
///
/// In v1 only `exited` and `exit_code` are populated; a future PR
/// adds `waker` registration for blocking `wait4` (PR 3 ships
/// WNOHANG-only — the plan explicitly defers blocking wait4 until
/// PR 4's child fiber can actually trigger an exit).
#[derive(Debug, Clone)]
pub struct ChildExitStatus {
    pub exit_code: i32,
    pub exited: bool,
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
            comm: [0; 16],
            futex_table: parking_lot::Mutex::new(FutexTable::default()),
            next_pid: AtomicI32::new(2),
            children: parking_lot::Mutex::new(HashMap::new()),
            child_event: Arc::new(Notify::new()),
        }
    }

    /// Generate a fresh 32-byte seed from the OS RNG.
    fn fresh_rng_seed() -> RngSeed {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        seed
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
}
