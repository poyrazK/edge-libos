//! The `Kernel` struct — the per-store state container.
//!
//! Every host syscall accesses the kernel through `Caller::data()` /
//! `Caller::data_mut()`. The `Kernel` owns the linear memory reference, the
//! fd table, the linear allocator, the rng, and the process-startup state.
//!
//! Step 4 of the P0 build order fleshes this out; the skeleton here is what
//! the dispatch table needs to compile.

use std::time::Instant;

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use wasmtime::Memory;

/// P2-D1: 32-byte seed captured at construction so the RNG can be
/// deterministically reconstructed by `apply_snapshot`. Fits inside
/// postcard-encoded `KernelSnapshot::rng_seed` directly.
pub type RngSeed = [u8; 32];

use crate::fd::FdTable;
use crate::mm::LinearAllocator;
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClockState {
    pub boot_monotonic_ns: u64,
}

pub struct Kernel {
    /// Linear memory reference. Attached post-instantiation.
    pub memory: Option<Memory>,
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
}

impl Kernel {
    pub fn new(args: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self::new_with_preopen(args, env, std::env::current_dir().unwrap_or_else(|_| "/".into()))
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

    fn new_inner(
        args: Vec<String>,
        env: Vec<(String, String)>,
        vfs: Vfs,
    ) -> Self {
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
        }
    }

    /// Generate a fresh 32-byte seed from the OS RNG.
    fn fresh_rng_seed() -> RngSeed {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        seed
    }

    /// Attach the linear memory. Called from instantiation setup.
    pub fn attach_memory(&mut self, mem: Memory) {
        self.memory = Some(mem);
    }

    /// Borrow the linear memory, or `-EFAULT` if not yet attached.
    pub fn memory(&self) -> Result<&Memory, i64> {
        self.memory.as_ref().ok_or(-(crate::errno::EFAULT))
    }

    /// Clone the stdout buffer Arc (for draining after the guest exits).
    /// Returns None if fd=1 has been closed or replaced.
    pub fn stdout_buf(&self) -> Option<std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<u8>>>> {
        match self.fds.get(crate::fd::STDOUT) {
            Ok(crate::fd::Resource::Stdout(w)) => Some(w.buf.clone()),
            _ => None,
        }
    }

    /// Clone the stderr buffer Arc (for draining after the guest exits).
    pub fn stderr_buf(&self) -> Option<std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<u8>>>> {
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
