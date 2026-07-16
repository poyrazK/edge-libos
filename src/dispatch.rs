//! The single async host function `kernel.syscall`.
//!
//! P0 dispatches to per-syscall handlers under `crate::sys`. The default arm
//! is `-ENOSYS` (clean, not a crash) — per `impelementationplan` §9, this
//! turns "mysterious runtime hang" into "build-time / import-time error we
//! can show the user."
//!
//! P2-A2: an optional `SyscallObserver` is invoked before and after the
//! handler runs. The default observer is a no-op. Tools like
//! `trace-host` install a thread-local observer that records each call.

use std::sync::Arc;

use anyhow::Result;
use wasmtime::{FuncType, Linker, Val, ValType};

use crate::errno::{to_ret, ENOSYS};
use crate::kernel::Kernel;
use crate::sys;

/// Number of i64 params `kernel.syscall` accepts.
const N_PARAMS: usize = 7;
/// Return type: i64.
const N_RESULTS: usize = 1;

/// Hook for tools (e.g. trace-host) that need to observe every syscall.
///
/// `on_enter` runs before the handler; `on_exit` runs after. Both must
/// be non-blocking. `on_enter` receives a snapshot of the syscall args
/// (the array is owned by the observer). `on_exit` receives the return
/// value (the same `i64` that the wasm guest sees).
pub trait SyscallObserver: Send + Sync {
    fn on_enter(&self, _nr: u32, _args: [i64; 6]) {}
    fn on_exit(&self, _nr: u32, _ret: i64) {}
}

impl<T: SyscallObserver + ?Sized> SyscallObserver for Arc<T> {
    fn on_enter(&self, nr: u32, args: [i64; 6]) {
        (**self).on_enter(nr, args);
    }
    fn on_exit(&self, nr: u32, ret: i64) {
        (**self).on_exit(nr, ret);
    }
}

thread_local! {
    /// Thread-local observer installed by trace-host (or any other tool).
    /// `None` is the fast path (no observer).
    static OBSERVER: std::cell::RefCell<Option<Arc<dyn SyscallObserver>>> =
        const { std::cell::RefCell::new(None) };
}

/// Install an observer for the current thread. The previous observer
/// (if any) is returned. Pass `None` to clear.
pub fn install_observer(o: Option<Arc<dyn SyscallObserver>>) -> Option<Arc<dyn SyscallObserver>> {
    OBSERVER.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), o))
}

/// Read-only access to the current thread's observer (if any).
pub fn with_observer<F, R>(f: F) -> R
where
    F: FnOnce(Option<&dyn SyscallObserver>) -> R,
{
    OBSERVER.with(|cell| {
        let borrow = cell.borrow();
        match &*borrow {
            Some(o) => f(Some(o.as_ref())),
            None => f(None),
        }
    })
}

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

            with_observer(|o| {
                if let Some(o) = o {
                    o.on_enter(nr, a);
                }
            });

            let ret = dispatch(caller, nr, a).await;
            results[0] = Val::I64(ret);

            with_observer(|o| {
                if let Some(o) = o {
                    o.on_exit(nr, ret);
                }
            });
            Ok(())
        })
    })?;

    Ok(())
}

/// Match a syscall number onto its handler. The default is `-ENOSYS`.
///
/// P2-A2: `pub` so tools can dispatch directly (used by unit tests
/// and the trace-host refactor).
///
/// This function is `async` so P1 socket work drops in without re-architecture.
/// Sync syscalls simply return immediately inside the future.
pub async fn dispatch(mut caller: wasmtime::Caller<'_, Kernel>, nr: u32, a: [i64; 6]) -> i64 {
    // Signal-delivery (ADR 0007 §4): drop Ignore-class signals at
    // dispatch entry (SIG_IGN, default-ignore disposition) so they
    // don't linger in the queue. Terminate + Interrupt signals stay
    // queued — the blocking-syscall `select!` arm consumes them with
    // full side effects. See `dispatch_signal_drain` for the full
    // reasoning and the failed-experiments comment.
    crate::sys::signal::dispatch_signal_drain(caller.data_mut());
    // Once a default-terminating signal has been delivered, every
    // subsequent syscall short-circuits to 0 so the guest's libc
    // unwinds toward exit. `exit_code` is already set to `128 +
    // signo`; the run path surfaces it. `exit_requested` is set
    // ONLY by signal termination, never by an explicit `exit(0)`.
    if caller
        .data()
        .exit_requested
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return 0;
    }
    match nr {
        // Process
        sys::process::NR_EXIT => sys::process::exit(&mut caller, a).await,
        sys::process::NR_EXIT_GROUP => sys::process::exit_group(&mut caller, a).await,
        sys::process::NR_GETPID => sys::process::getpid(&mut caller, a),
        sys::process::NR_GETTID => sys::process::gettid(&mut caller, a),
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
        sys::file::NR_OPEN => sys::file::open(&mut caller, a).await,
        sys::file::NR_OPENAT => sys::file::openat(&mut caller, a).await,
        sys::file::NR_CLOSE => sys::file::close(&mut caller, a).await,
        sys::file::NR_STAT => sys::file::stat(&mut caller, a).await,
        sys::file::NR_LSTAT => sys::file::lstat(&mut caller, a).await,
        sys::file::NR_LSEEK => sys::file::lseek(&mut caller, a).await,
        sys::file::NR_FSTAT => sys::file::fstat(&mut caller, a).await,
        sys::file::NR_NEWFSTATAT => sys::file::newfstatat(&mut caller, a).await,
        sys::file::NR_STATX => sys::file::statx(&mut caller, a).await,
        sys::file::NR_GETDENTS64 => sys::file::getdents64(&mut caller, a).await,
        sys::file::NR_PIPE => sys::file::pipe(&mut caller, a).await,
        sys::file::NR_PIPE2 => sys::file::pipe2(&mut caller, a).await,
        sys::file::NR_FCNTL => sys::file::fcntl(&mut caller, a).await,
        sys::file::NR_DUP => sys::file::dup(&mut caller, a).await,
        sys::file::NR_DUP2 => sys::file::dup2(&mut caller, a).await,
        sys::file::NR_DUP3 => sys::file::dup3(&mut caller, a).await,
        sys::file::NR_GETCWD => sys::file::getcwd(&mut caller, a).await,
        sys::file::NR_READV => sys::file::readv(&mut caller, a).await,
        sys::file::NR_WRITEV => sys::file::writev(&mut caller, a).await,
        // P2-C1 part 1: mkdir / mkdirat / rmdir / unlink / unlinkat.
        sys::file::NR_MKDIR => sys::file::mkdir(&mut caller, a).await,
        sys::file::NR_MKDIRAT => sys::file::mkdirat(&mut caller, a).await,
        sys::file::NR_RMDIR => sys::file::rmdir(&mut caller, a).await,
        sys::file::NR_UNLINK => sys::file::unlink(&mut caller, a).await,
        sys::file::NR_UNLINKAT => sys::file::unlinkat(&mut caller, a).await,
        // P2-C1 part 2: rename / renameat / renameat2 / truncate / ftruncate.
        sys::file::NR_RENAME => sys::file::rename(&mut caller, a).await,
        sys::file::NR_RENAMEAT => sys::file::renameat(&mut caller, a).await,
        sys::file::NR_RENAMEAT2 => sys::file::renameat2(&mut caller, a).await,
        sys::file::NR_TRUNCATE => sys::file::truncate(&mut caller, a).await,
        sys::file::NR_FTRUNCATE => sys::file::ftruncate(&mut caller, a).await,
        // P2-C1 part 3: readlink / symlink / link / utimensat / chmod /
        // faccessat / chdir / chroot (+at variants).
        sys::file::NR_READLINK => sys::file::readlink(&mut caller, a).await,
        sys::file::NR_READLINKAT => sys::file::readlinkat(&mut caller, a).await,
        sys::file::NR_SYMLINK => sys::file::symlink(&mut caller, a).await,
        sys::file::NR_SYMLINKAT => sys::file::symlinkat(&mut caller, a).await,
        sys::file::NR_LINK => sys::file::link(&mut caller, a).await,
        sys::file::NR_LINKAT => sys::file::linkat(&mut caller, a).await,
        sys::file::NR_UTIMENSAT => sys::file::utimensat(&mut caller, a).await,
        sys::file::NR_CHMOD => sys::file::chmod(&mut caller, a).await,
        sys::file::NR_FCHMOD => sys::file::fchmod(&mut caller, a).await,
        sys::file::NR_FCHMODAT => sys::file::fchmodat(&mut caller, a).await,
        sys::file::NR_FACCESSAT => sys::file::faccessat(&mut caller, a).await,
        sys::file::NR_FACCESSAT2 => sys::file::faccessat2(&mut caller, a).await,
        sys::file::NR_CHDIR => sys::file::chdir(&mut caller, a).await,
        sys::file::NR_CHROOT => sys::file::chroot(&mut caller, a).await,

        // Sockets (P1-1: socket only; bind/listen/accept/connect/recv/send
        // land in later sub-steps).
        sys::socket::NR_SOCKET => sys::socket::socket(&mut caller, a).await,
        sys::socket::NR_BIND => sys::socket::bind(&mut caller, a).await,
        sys::socket::NR_LISTEN => sys::socket::listen(&mut caller, a).await,
        sys::socket::NR_ACCEPT => sys::socket::accept(&mut caller, a).await,
        sys::socket::NR_ACCEPT4 => sys::socket::accept4(&mut caller, a).await,
        sys::socket::NR_CONNECT => sys::socket::connect(&mut caller, a).await,
        sys::socket::NR_SENDTO => sys::socket::sendto(&mut caller, a).await,
        sys::socket::NR_RECVFROM => sys::socket::recvfrom(&mut caller, a).await,
        // P2-C3 part 1: sendmsg / recvmsg.
        sys::socket::NR_SENDMSG => sys::socket::sendmsg(&mut caller, a).await,
        sys::socket::NR_RECVMSG => sys::socket::recvmsg(&mut caller, a).await,
        sys::socket::NR_SETSOCKOPT => sys::socket::setsockopt(&mut caller, a).await,
        sys::socket::NR_GETSOCKOPT => sys::socket::getsockopt(&mut caller, a).await,
        sys::socket::NR_GETSOCKNAME => sys::socket::getsockname(&mut caller, a).await,
        sys::socket::NR_GETPEERNAME => sys::socket::getpeername(&mut caller, a).await,
        sys::socket::NR_SHUTDOWN => sys::socket::shutdown(&mut caller, a).await,
        // P2-C3 part 2: socketpair (AF_UNIX pair).
        sys::socket::NR_SOCKETPAIR => sys::socket::socketpair(&mut caller, a).await,

        // poll(2) — P1-6 synchronous readiness scan.
        sys::poll::NR_POLL => sys::poll::poll(&mut caller, a).await,
        // P2-C3 part 1: ppoll, select.
        sys::poll::NR_PPOLL => sys::poll::ppoll(&mut caller, a).await,
        sys::poll::NR_SELECT => sys::poll::select(&mut caller, a).await,

        // P1-7: the async pivot — epoll + eventfd.
        sys::epoll::NR_EPOLL_CREATE1 => sys::epoll::epoll_create1(&mut caller, a).await,
        sys::epoll::NR_EPOLL_CTL => sys::epoll::epoll_ctl(&mut caller, a).await,
        sys::epoll::NR_EPOLL_WAIT => sys::epoll::epoll_wait(&mut caller, a).await,
        // P2-C3 part 1: epoll_pwait, legacy eventfd.
        sys::epoll::NR_EPOLL_PWAIT => sys::epoll::epoll_pwait(&mut caller, a).await,
        sys::eventfd::NR_EVENTFD2 => sys::eventfd::eventfd2(&mut caller, a).await,
        sys::eventfd::NR_EVENTFD => sys::eventfd::eventfd(&mut caller, a).await,

        // Identity (stubs)
        sys::identity::NR_GETUID => sys::identity::getuid(),
        sys::identity::NR_GETEUID => sys::identity::geteuid(),
        sys::identity::NR_GETGID => sys::identity::getgid(),
        sys::identity::NR_GETEGID => sys::identity::getegid(),

        // P2-C2: identity (extended)
        sys::identity::NR_GETPPID => sys::identity::getppid(),
        sys::identity::NR_UNAME => sys::identity::uname(&mut caller, a).await,
        sys::identity::NR_PRLIMIT64 => sys::identity::prlimit64(&mut caller, a).await,
        sys::identity::NR_GETRLIMIT => sys::identity::getrlimit(&mut caller, a).await,
        sys::identity::NR_SETSID => sys::identity::setsid(),
        sys::identity::NR_GETSID => sys::identity::getsid(a),
        sys::identity::NR_GETGROUPS => sys::identity::getgroups(&mut caller, a).await,

        // P2-C2: process
        sys::process::NR_SCHED_YIELD => sys::process::sched_yield().await,
        sys::process::NR_SCHED_GETAFFINITY => sys::process::sched_getaffinity(&mut caller, a).await,
        sys::process::NR_PRCTL => sys::process::prctl(&mut caller, a).await,
        sys::process::NR_KILL => sys::process::kill(&mut caller, a).await,
        sys::process::NR_TGKILL => sys::process::tgkill(&mut caller, a).await,

        // P2-C2: signal
        sys::signal::NR_SIGALTSTACK => sys::signal::sigaltstack(&mut caller, a),
        sys::signal::NR_RT_SIGRETURN => sys::signal::rt_sigreturn(),

        // P2-C2: time
        sys::time::NR_CLOCK_GETRES => sys::time::clock_getres(&mut caller, a).await,
        sys::time::NR_CLOCK_NANOSLEEP => sys::time::clock_nanosleep(&mut caller, a).await,

        // P2 closing: sysinfo + times stubs.
        sys::time::NR_SYSINFO => sys::time::sysinfo(&mut caller, a).await,
        sys::time::NR_TIMES => sys::time::times(&mut caller, a).await,

        // P3 reservation: clone / fork / wait4. Each returns -ENOSYS; real
        // impls land in P3 after P2-D snapshot machinery and wasm_threads
        // support are in place. See ADR 0002 for the fork snapshot story.
        // P3 Tier-1: futex(2) FUTEX_WAIT/FUTEX_WAKE shipped — see ADR 0001.
        // P3 Tier-4: clone(56) v1 supports CLONE_CHILD_SETTID |
        // CLONE_PARENT_SETTID only; other flags → -EINVAL. fork(57) ships
        // as v1 in P3 final-bundle sub-deliverable 5 (allocates PID +
        // inserts ChildExitStatus, does NOT resume child fiber — see
        // `fork_syscall` doc for the deferred-resume contract).
        // P3 Tier-6: wait4(61) supports WNOHANG + blocking parked path.
        sys::process::NR_CLONE => sys::process::clone_syscall(&mut caller, a).await,
        sys::process::NR_FORK => sys::process::fork_syscall(&mut caller, a).await,
        sys::process::NR_WAIT4 => sys::process::wait4_syscall(&mut caller, a).await,
        sys::futex::NR_FUTEX => sys::futex::futex(&mut caller, a).await,

        // P2-D3.5: NR_SNAPSHOT — guest-driven quiescence. The guest
        // passes a path pointer; the kernel writes a postcard snapshot
        // to it. See ADR 0004 §1 and `sys::process::snapshot_syscall`.
        sys::process::NR_SNAPSHOT => sys::process::snapshot_syscall(&mut caller, a).await,

        // P2-DNS: NR_RESOLVE — project-private getaddrinfo(3)
        // replacement (NR 400, upstream-reserved range 387-423). See
        // ADR 0007. Until commit 3 wires CLI env vars, the denylist /
        // TTL / timeout come from `ResolverState::default`.
        sys::resolver::NR_RESOLVE => sys::resolver::resolve(&mut caller, a).await,

        // P2-C2: memory
        sys::memory::NR_MREMAP => sys::memory::mremap(&mut caller, a),

        // P2-C2: ioctl
        sys::ioctl::NR_IOCTL => sys::ioctl::ioctl(&mut caller, a).await,

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

/// Resolve a syscall number to a short name. Returns `"?"` for unknown.
/// Used by the trace-host binary for human-friendly JSON output.
pub fn syscall_name(nr: u32) -> &'static str {
    match nr {
        sys::process::NR_EXIT => "exit",
        sys::process::NR_EXIT_GROUP => "exit_group",
        sys::process::NR_GETPID => "getpid",
        sys::process::NR_GETTID => "gettid",
        sys::process::NR_SET_TID_ADDRESS => "set_tid_address",
        sys::process::NR_SET_ROBUST_LIST => "set_robust_list",
        sys::process::NR_ARCH_PRCTL => "arch_prctl",
        sys::process::NR_RSEQ => "rseq",

        sys::memory::NR_MMAP => "mmap",
        sys::memory::NR_MUNMAP => "munmap",
        sys::memory::NR_MPROTECT => "mprotect",
        sys::memory::NR_MADVISE => "madvise",
        sys::memory::NR_BRK => "brk",

        sys::file::NR_READ => "read",
        sys::file::NR_WRITE => "write",
        sys::file::NR_OPEN => "open",
        sys::file::NR_OPENAT => "openat",
        sys::file::NR_CLOSE => "close",
        sys::file::NR_STAT => "stat",
        sys::file::NR_LSTAT => "lstat",
        sys::file::NR_LSEEK => "lseek",
        sys::file::NR_FSTAT => "fstat",
        sys::file::NR_NEWFSTATAT => "newfstatat",
        sys::file::NR_STATX => "statx",
        sys::file::NR_GETDENTS64 => "getdents64",
        sys::file::NR_PIPE => "pipe",
        sys::file::NR_PIPE2 => "pipe2",
        sys::file::NR_FCNTL => "fcntl",
        sys::file::NR_DUP => "dup",
        sys::file::NR_DUP2 => "dup2",
        sys::file::NR_DUP3 => "dup3",
        sys::file::NR_GETCWD => "getcwd",
        sys::file::NR_READV => "readv",
        sys::file::NR_WRITEV => "writev",
        sys::file::NR_MKDIR => "mkdir",
        sys::file::NR_MKDIRAT => "mkdirat",
        sys::file::NR_RMDIR => "rmdir",
        sys::file::NR_UNLINK => "unlink",
        sys::file::NR_UNLINKAT => "unlinkat",
        sys::file::NR_RENAME => "rename",
        sys::file::NR_RENAMEAT => "renameat",
        sys::file::NR_RENAMEAT2 => "renameat2",
        sys::file::NR_TRUNCATE => "truncate",
        sys::file::NR_FTRUNCATE => "ftruncate",
        // P2-C1 part 3
        sys::file::NR_READLINK => "readlink",
        sys::file::NR_READLINKAT => "readlinkat",
        sys::file::NR_SYMLINK => "symlink",
        sys::file::NR_SYMLINKAT => "symlinkat",
        sys::file::NR_LINK => "link",
        sys::file::NR_LINKAT => "linkat",
        sys::file::NR_UTIMENSAT => "utimensat",
        sys::file::NR_CHMOD => "chmod",
        sys::file::NR_FCHMOD => "fchmod",
        sys::file::NR_FCHMODAT => "fchmodat",
        sys::file::NR_FACCESSAT => "faccessat",
        sys::file::NR_FACCESSAT2 => "faccessat2",
        sys::file::NR_CHDIR => "chdir",
        sys::file::NR_CHROOT => "chroot",

        sys::socket::NR_SOCKET => "socket",
        sys::socket::NR_BIND => "bind",
        sys::socket::NR_LISTEN => "listen",
        sys::socket::NR_ACCEPT => "accept",
        sys::socket::NR_ACCEPT4 => "accept4",
        sys::socket::NR_CONNECT => "connect",
        sys::socket::NR_SENDTO => "sendto",
        sys::socket::NR_RECVFROM => "recvfrom",
        sys::socket::NR_SENDMSG => "sendmsg",
        sys::socket::NR_RECVMSG => "recvmsg",
        sys::socket::NR_SETSOCKOPT => "setsockopt",
        sys::socket::NR_GETSOCKOPT => "getsockopt",
        sys::socket::NR_GETSOCKNAME => "getsockname",
        sys::socket::NR_GETPEERNAME => "getpeername",
        sys::socket::NR_SHUTDOWN => "shutdown",
        sys::socket::NR_SOCKETPAIR => "socketpair",

        sys::poll::NR_POLL => "poll",
        sys::poll::NR_PPOLL => "ppoll",
        sys::poll::NR_SELECT => "select",

        sys::epoll::NR_EPOLL_CREATE1 => "epoll_create1",
        sys::epoll::NR_EPOLL_CTL => "epoll_ctl",
        sys::epoll::NR_EPOLL_WAIT => "epoll_wait",
        sys::epoll::NR_EPOLL_PWAIT => "epoll_pwait",
        sys::eventfd::NR_EVENTFD2 => "eventfd2",
        sys::eventfd::NR_EVENTFD => "eventfd",

        sys::identity::NR_GETUID => "getuid",
        sys::identity::NR_GETEUID => "geteuid",
        sys::identity::NR_GETGID => "getgid",
        sys::identity::NR_GETEGID => "getegid",
        // P2-C2 identity
        sys::identity::NR_GETPPID => "getppid",
        sys::identity::NR_UNAME => "uname",
        sys::identity::NR_PRLIMIT64 => "prlimit64",
        sys::identity::NR_GETRLIMIT => "getrlimit",
        sys::identity::NR_SETSID => "setsid",
        sys::identity::NR_GETSID => "getsid",
        sys::identity::NR_GETGROUPS => "getgroups",
        // P2-C2 process
        sys::process::NR_SCHED_YIELD => "sched_yield",
        sys::process::NR_SCHED_GETAFFINITY => "sched_getaffinity",
        sys::process::NR_PRCTL => "prctl",
        sys::process::NR_KILL => "kill",
        sys::process::NR_TGKILL => "tgkill",
        // P2-C2 signal
        sys::signal::NR_SIGALTSTACK => "sigaltstack",
        sys::signal::NR_RT_SIGRETURN => "rt_sigreturn",
        // P2-C2 time
        sys::time::NR_CLOCK_GETRES => "clock_getres",
        sys::time::NR_CLOCK_NANOSLEEP => "clock_nanosleep",
        // P2-C2 memory
        sys::memory::NR_MREMAP => "mremap",
        // P2-C2 ioctl
        sys::ioctl::NR_IOCTL => "ioctl",

        sys::time::NR_CLOCK_GETTIME => "clock_gettime",
        sys::time::NR_GETTIMEOFDAY => "gettimeofday",
        sys::time::NR_NANOSLEEP => "nanosleep",
        sys::time::NR_SYSINFO => "sysinfo",
        sys::time::NR_TIMES => "times",

        // P3 reservation: see ADR 0001 (futex) + ADR 0002 (fork).
        sys::process::NR_CLONE => "clone",
        sys::process::NR_FORK => "fork",
        sys::process::NR_WAIT4 => "wait4",
        sys::futex::NR_FUTEX => "futex",
        // P2-D3.5: NR_SNAPSHOT (123) — see ADR 0004 §1.
        sys::process::NR_SNAPSHOT => "snapshot",
        // P2-DNS: NR_RESOLVE (400) — see ADR 0007.
        sys::resolver::NR_RESOLVE => "resolve",

        sys::random::NR_GETRANDOM => "getrandom",

        sys::signal::NR_RT_SIGACTION => "rt_sigaction",
        sys::signal::NR_RT_SIGPROCMASK => "rt_sigprocmask",
        _ => "?",
    }
}
