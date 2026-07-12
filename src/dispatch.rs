//! The single async host function `kernel.syscall`.
//!
//! P0 dispatches to per-syscall handlers under `crate::sys`. The default arm
//! is `-ENOSYS` (clean, not a crash) — per `impelementationplan` §9, this
//! turns "mysterious runtime hang" into "build-time / import-time error we
//! can show the user."

use anyhow::Result;
use wasmtime::{FuncType, Linker, Val, ValType};

use crate::errno::{to_ret, ENOSYS};
use crate::kernel::Kernel;
use crate::sys;

/// Number of i64 params `kernel.syscall` accepts.
const N_PARAMS: usize = 7;
/// Return type: i64.
const N_RESULTS: usize = 1;

/// Register the dispatch function on the linker.
pub fn register(linker: &mut Linker<Kernel>) -> Result<()> {
    // wasmtime 45.0.3 FuncType::new takes (&Engine, params, results) where
    // params/results are `impl IntoIterator<Item = ValType>`. `ValType` is
    // not `Copy`, so we use const-block initializers for the repeated array.
    let engine = linker.engine();
    let params: [ValType; N_PARAMS] = [const { ValType::I64 }; N_PARAMS];
    let results: [ValType; N_RESULTS] = [const { ValType::I64 }; N_RESULTS];
    let func_ty = FuncType::new(engine, params, results);

    linker.func_new_async("kernel", "syscall", func_ty, |caller, params, results| {
        Box::new(async move {
            let nr = params[0].unwrap_i64() as u32;
            let a: [i64; 6] = std::array::from_fn(|i| params[i + 1].unwrap_i64());

            let ret = dispatch(caller, nr, a).await;
            results[0] = Val::I64(ret);
            Ok(())
        })
    })?;

    Ok(())
}

/// Match a syscall number onto its handler. The default is `-ENOSYS`.
///
/// This function is `async` so P1 socket work drops in without re-architecture.
/// Sync syscalls simply return immediately inside the future.
async fn dispatch(
    mut caller: wasmtime::Caller<'_, Kernel>,
    nr: u32,
    a: [i64; 6],
) -> i64 {
    match nr {
        // Process
        sys::process::NR_EXIT => sys::process::exit(&mut caller, a).await,
        sys::process::NR_EXIT_GROUP => sys::process::exit_group(&mut caller, a).await,
        sys::process::NR_GETPID => sys::process::getpid(),
        sys::process::NR_GETTID => sys::process::gettid(),
        sys::process::NR_SET_TID_ADDRESS => sys::process::set_tid_address(&mut caller, a),
        sys::process::NR_SET_ROBUST_LIST => sys::process::set_robust_list(),
        sys::process::NR_ARCH_PRCTL => to_ret(crate::errno::ENOSYS),
        sys::process::NR_RSEQ => to_ret(crate::errno::ENOSYS),

        // Memory
        sys::memory::NR_MMAP => sys::memory::mmap(&mut caller, a).await,
        sys::memory::NR_MUNMAP => sys::memory::munmap(&mut caller, a).await,
        sys::memory::NR_MPROTECT => sys::memory::mprotect(),
        sys::memory::NR_MADVISE => sys::memory::madvise(),
        sys::memory::NR_BRK => sys::memory::brk(&mut caller, a),

        // Filesystem / VFS
        sys::file::NR_READ => sys::file::read(&mut caller, a).await,
        sys::file::NR_WRITE => sys::file::write(&mut caller, a).await,
        sys::file::NR_OPENAT => sys::file::openat(&mut caller, a).await,
        sys::file::NR_CLOSE => sys::file::close(&mut caller, a).await,
        sys::file::NR_LSEEK => sys::file::lseek(&mut caller, a).await,
        sys::file::NR_FSTAT => sys::file::fstat(&mut caller, a).await,
        sys::file::NR_NEWFSTATAT => sys::file::newfstatat(&mut caller, a).await,
        sys::file::NR_GETDENTS64 => sys::file::getdents64(&mut caller, a).await,
        sys::file::NR_PIPE2 => sys::file::pipe2(&mut caller, a).await,
        sys::file::NR_FCNTL => sys::file::fcntl(&mut caller, a).await,

        // Identity (stubs)
        sys::identity::NR_GETUID => sys::identity::getuid(),
        sys::identity::NR_GETEUID => sys::identity::geteuid(),
        sys::identity::NR_GETGID => sys::identity::getgid(),
        sys::identity::NR_GETEGID => sys::identity::getegid(),

        // Time
        sys::time::NR_CLOCK_GETTIME => sys::time::clock_gettime(&mut caller, a).await,
        sys::time::NR_GETTIMEOFDAY => sys::time::gettimeofday(&mut caller, a).await,
        sys::time::NR_NANOSLEEP => sys::time::nanosleep(&mut caller, a).await,

        // Random
        sys::random::NR_GETRANDOM => sys::random::getrandom(&mut caller, a).await,

        // Signals (record-only in v1)
        sys::signal::NR_RT_SIGACTION => sys::signal::rt_sigaction(&mut caller, a),
        sys::signal::NR_RT_SIGPROCMASK => sys::signal::rt_sigprocmask(&mut caller, a),

        // Anything else
        _ => {
            tracing::trace!(nr, "kernel.syscall: ENOSYS");
            to_ret(ENOSYS)
        }
    }
}
