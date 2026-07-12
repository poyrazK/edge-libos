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
use rand::SeedableRng;
use wasmtime::Memory;

use crate::fd::FdTable;
use crate::mm::LinearAllocator;
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

#[derive(Debug)]
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
    pub signals: SignalState,
    pub started_at: Instant,
    /// Set by exit() / exit_group() syscalls. The host driver inspects this
    /// after each call returns and surfaces the code in its own exit code.
    pub exit_code: Option<i32>,
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
            rng: SmallRng::from_entropy(),
            signals: SignalState::new(),
            started_at: now,
            exit_code: None,
        }
    }

    /// Attach the linear memory. Called from instantiation setup.
    pub fn attach_memory(&mut self, mem: Memory) {
        self.memory = Some(mem);
    }

    /// Borrow the linear memory, or `-EFAULT` if not yet attached.
    pub fn memory(&self) -> Result<&Memory, i64> {
        self.memory.as_ref().ok_or(-(crate::errno::EFAULT))
    }
}
