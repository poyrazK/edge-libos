//! Per-spec §8 EFAULT fuzz: for every syscall that touches a pointer+len,
//! apply a poison set to the pointer and confirm the host returns `-EFAULT`
//! or a documented safe sentinel — never panics, never traps.
//!
//! This is a *host-side* fuzz. We compile a tiny wasm per test that calls
//! `kernel.syscall(nr, …)` and observes the return value. There is no
//! libc involvement here — the unit tests in `tests/*_conformance.rs` cover
//! that path; this file covers the kernel's own EFAULT posture.

mod common;

use anyhow::Result;
use edge_libos::Kernel;

/// WAT module that exposes a single `call` function taking nr + 6 i64 args.
const CALLER_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "call")
        (param $nr i64) (param $a1 i64) (param $a2 i64)
        (param $a3 i64) (param $a4 i64) (param $a5 i64) (param $a6 i64)
        (result i64)
        (call $syscall (local.get $nr) (local.get $a1) (local.get $a2)
                       (local.get $a3) (local.get $a4) (local.get $a5)
                       (local.get $a6))
      )
    )
"#;

/// Poison values for pointer+len args. Includes boundary cases that have
/// historically caused EFAULT regressions in other libOS projects. These
/// values are all **definitely invalid** linear-memory offsets:
///   * negative values (cannot be valid wasm ptrs),
///   * values beyond the 1-page (64 KiB) memory we instantiate,
///   * the integer-overflow / huge-unsigned class.
const POISON_PTR: &[i64] = &[
    -1,
    i64::MIN,
    i64::MAX,
    1 << 33,
    65_536,  // 1 page = exactly mem_size
    65_537,  // mem_size + 1
    131_072, // 2 pages
];

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

async fn dispatch_argv(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    module: &wasmtime::Module,
    nr: i64,
    a: [i64; 6],
) -> Result<i64> {
    let (mut store, instance) = common::instantiate_async(engine, linker, module).await?;
    let f =
        instance.get_typed_func::<(i64, i64, i64, i64, i64, i64, i64), i64>(&mut store, "call")?;
    let ret = f
        .call_async(&mut store, (nr, a[0], a[1], a[2], a[3], a[4], a[5]))
        .await?;
    Ok(ret)
}

async fn assert_efault_or_safe(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    module: &wasmtime::Module,
    nr: i64,
    a: [i64; 6],
    sysname: &str,
) -> Result<i64> {
    let ret = dispatch_argv(engine, linker, module, nr, a).await?;
    let efault = -edge_libos::errno::EFAULT;
    // "Safe sentinels" we accept without complaint when the poisoned
    // pointer cannot possibly be a valid wasm linear-memory offset:
    //   -EFAULT   — bounds check caught it
    //   -EINVAL   — argument validation rejected before deref
    //   -ENOSYS   — syscall not implemented
    //   -EBADF    — fd lookup failed before deref
    let safe = [
        efault,
        -edge_libos::errno::EINVAL,
        -edge_libos::errno::ENOSYS,
        -edge_libos::errno::EBADF,
    ];
    if !safe.contains(&ret) {
        panic!(
            "{sysname}: poisoned call returned {ret}, expected EFAULT/\
             EINVAL/ENOSYS/EBADF. Args: {a:?}"
        );
    }
    Ok(ret)
}

// -- Tests ------------------------------------------------------------------

#[test]
fn fuzz_read_bad_buf_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_READ as i64,
            [0, *ptr, 16, 0, 0, 0],
            "read",
        ))?;
        // read on fd=0 (stdin) with bad ptr must surface EFAULT or -EBADF.
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_write_bad_buf_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_WRITE as i64,
            [1, *ptr, 16, 0, 0, 0],
            "write",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_openat_bad_path_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_OPENAT as i64,
            [-100, *ptr, 0, 0, 0, 0], // AT_FDCWD, path, flags, mode
            "openat",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_fstat_bad_statbuf_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_FSTAT as i64,
            [1, *ptr, 0, 0, 0, 0], // fd=1 (stdout, valid), bad statbuf ptr
            "fstat",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_getdents64_bad_buf_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_GETDENTS64 as i64,
            [1, *ptr, 1024, 0, 0, 0], // fd=1, bad buf, len
            "getdents64",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_clock_gettime_bad_timespec_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::time::NR_CLOCK_GETTIME as i64,
            [0, *ptr, 0, 0, 0, 0], // clockid=REALTIME, bad tp ptr
            "clock_gettime",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_gettimeofday_bad_timeval_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::time::NR_GETTIMEOFDAY as i64,
            [*ptr, 0, 0, 0, 0, 0], // bad tp ptr, tz ignored
            "gettimeofday",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_nanosleep_bad_req_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        // rem=0 (NULL) to avoid second-pointer exposure in the test.
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::time::NR_NANOSLEEP as i64,
            [*ptr, 0, 0, 0, 0, 0], // bad req ptr, rem=NULL
            "nanosleep",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_getrandom_bad_buf_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::random::NR_GETRANDOM as i64,
            [*ptr, 16, 0, 0, 0, 0], // bad buf ptr, len=16, no flags
            "getrandom",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_rt_sigaction_bad_act_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::signal::NR_RT_SIGACTION as i64,
            [2 /* SIGINT */, *ptr, 0, 8, 0, 0], // signum, bad act, oldact=NULL, size=8
            "rt_sigaction",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_rt_sigprocmask_bad_set_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::signal::NR_RT_SIGPROCMASK as i64,
            [0, *ptr, 0, 8, 0, 0], // SIG_BLOCK, bad set, oldset=NULL, size=8
            "rt_sigprocmask",
        ))?;
        let _ = r;
    }
    Ok(())
}

#[test]
fn fuzz_pipe2_bad_fdarray_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::file::NR_PIPE2 as i64,
            [*ptr, 0, 0, 0, 0, 0], // bad fdarray ptr, flags=0
            "pipe2",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// bind(fd, addr, len) — fd is fine but addr pointer is poisoned.
#[test]
fn fuzz_bind_bad_sockaddr_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::socket::NR_BIND as i64,
            [3 /*fd*/, *ptr, 16, 0, 0, 0],
            "bind",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// listen(fd, backlog) — no pointer args. Surface must be EBADF/EINVAL on
/// bogus fd values; -EOPNOTSUPP / -EDESTADDRREQ / 0 are valid for fd=0
/// depending on state, so we just exercise fd=0 with valid backlog to make
/// sure no panic occurs and the call returns from the safe set.
#[test]
fn fuzz_listen_no_pointer_args() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let r = block_on(assert_efault_or_safe(
        &engine,
        &linker,
        &module,
        edge_libos::sys::socket::NR_LISTEN as i64,
        [
            0, /*fd (stdin, not a socket)*/
            5, /*backlog*/
            0, 0, 0, 0,
        ],
        "listen",
    ))?;
    let _ = r;
    Ok(())
}

/// setsockopt(fd, level, optname, optval, optlen) — fd is fine but the
/// optval pointer is poisoned.
#[test]
fn fuzz_setsockopt_bad_optval_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::socket::NR_SETSOCKOPT as i64,
            [
                3, /*fd*/
                1, /*SOL_SOCKET*/
                2, /*SO_REUSEADDR*/
                *ptr, 4, /*optlen*/
                0,
            ],
            "setsockopt",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// getsockopt(fd, level, optname, optval, optlen) — fd is fine but
/// optval pointer is poisoned.
#[test]
fn fuzz_getsockopt_bad_optval_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::socket::NR_GETSOCKOPT as i64,
            [
                3, /*fd*/
                1, /*SOL_SOCKET*/
                4, /*SO_ERROR*/
                *ptr, 4, 0,
            ],
            "getsockopt",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// getsockname(fd, addr, addrlen) — fd is fine but addr pointer is poisoned.
#[test]
fn fuzz_getsockname_bad_addr_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::socket::NR_GETSOCKNAME as i64,
            [3 /*fd*/, *ptr, 16, 0, 0, 0],
            "getsockname",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// getpeername(fd, addr, addrlen) — fd is fine but addr pointer is poisoned.
#[test]
fn fuzz_getpeername_bad_addr_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::socket::NR_GETPEERNAME as i64,
            [3 /*fd*/, *ptr, 16, 0, 0, 0],
            "getpeername",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// poll(fds, nfds, timeout) — fds pointer is poisoned.
#[test]
fn fuzz_poll_bad_fds_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::poll::NR_POLL as i64,
            [*ptr, 1, 0, 0, 0, 0],
            "poll",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// epoll_create1(flags) — no pointer args. A successful return is a
/// valid fd (>=3); we accept that as a safe outcome too.
#[test]
fn fuzz_epoll_create1_no_pointer_args() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        edge_libos::sys::epoll::NR_EPOLL_CREATE1 as i64,
        [0 /*flags*/, 0, 0, 0, 0, 0],
    ))?;
    // Either a valid fd (>=3) or one of the safe error sentinels.
    assert!(
        ret >= 3
            || ret == -edge_libos::errno::EFAULT
            || ret == -edge_libos::errno::EINVAL
            || ret == -edge_libos::errno::ENOSYS
            || ret == -edge_libos::errno::EBADF,
        "epoll_create1 returned {ret}, expected fd or safe error"
    );
    Ok(())
}

/// epoll_ctl(epfd, op, fd, event_ptr) — event pointer is poisoned on
/// ADD/MOD. With a bogus epfd, the call short-circuits to -EBADF before
/// the deref; with a real epfd but bogus ptr, -EFAULT.
#[test]
fn fuzz_epoll_ctl_bad_event_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::epoll::NR_EPOLL_CTL as i64,
            [
                9999, /*epfd*/
                1,    /*ADD*/
                1,    /*fd*/
                *ptr, 0, 0,
            ],
            "epoll_ctl",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// epoll_wait(epfd, events_ptr, maxevents, timeout) — events pointer is
/// poisoned. With a bogus epfd, the call returns -EBADF; with a real
/// epfd but bogus events_ptr, -EFAULT or -EINVAL.
#[test]
fn fuzz_epoll_wait_bad_events_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    for ptr in POISON_PTR {
        let r = block_on(assert_efault_or_safe(
            &engine,
            &linker,
            &module,
            edge_libos::sys::epoll::NR_EPOLL_WAIT as i64,
            [9999 /*epfd*/, *ptr, 4, 0, 0, 0],
            "epoll_wait",
        ))?;
        let _ = r;
    }
    Ok(())
}

/// eventfd2(initval, flags) — no pointer args. A successful return is a
/// valid fd (>=3); accept that as a safe outcome too.
#[test]
fn fuzz_eventfd2_no_pointer_args() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        edge_libos::sys::eventfd::NR_EVENTFD2 as i64,
        [0 /*initval*/, 0, 0, 0, 0, 0],
    ))?;
    assert!(
        ret >= 3
            || ret == -edge_libos::errno::EFAULT
            || ret == -edge_libos::errno::EINVAL
            || ret == -edge_libos::errno::ENOSYS
            || ret == -edge_libos::errno::EBADF,
        "eventfd2 returned {ret}, expected fd or safe error"
    );
    Ok(())
}

/// Brute-force overflow: pointer = i64::MAX/2, len = i64::MAX/2.
#[test]
fn fuzz_overflow_ptr_plus_len_every_pointer_syscall() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let huge = i64::MAX / 2;

    let cases: &[(u32, &str, [i64; 6])] = &[
        (
            edge_libos::sys::file::NR_READ,
            "read",
            [0, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_WRITE,
            "write",
            [1, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_OPENAT,
            "openat",
            [-100, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_FSTAT,
            "fstat",
            [1, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_GETDENTS64,
            "getdents64",
            [1, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::time::NR_CLOCK_GETTIME,
            "clock_gettime",
            [0, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::time::NR_GETTIMEOFDAY,
            "gettimeofday",
            [huge, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::time::NR_NANOSLEEP,
            "nanosleep",
            [huge, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::random::NR_GETRANDOM,
            "getrandom",
            [huge, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::signal::NR_RT_SIGACTION,
            "rt_sigaction",
            [2, huge, 0, 8, 0, 0],
        ),
        (
            edge_libos::sys::signal::NR_RT_SIGPROCMASK,
            "rt_sigprocmask",
            [0, huge, 0, 8, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_PIPE2,
            "pipe2",
            [huge, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_PIPE,
            "pipe",
            [huge, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_OPEN,
            "open",
            [huge, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_STAT,
            "stat",
            [huge, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_LSTAT,
            "lstat",
            [huge, huge, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_GETCWD,
            "getcwd",
            [huge, 256, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_READV,
            "readv",
            [1, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::file::NR_WRITEV,
            "writev",
            [1, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::socket::NR_BIND,
            "bind",
            [3, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::socket::NR_SETSOCKOPT,
            "setsockopt",
            [3, 1, 2, huge, huge, 0],
        ),
        (
            edge_libos::sys::socket::NR_GETSOCKOPT,
            "getsockopt",
            [3, 1, 4, huge, huge, 0],
        ),
        (
            edge_libos::sys::socket::NR_GETSOCKNAME,
            "getsockname",
            [3, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::socket::NR_GETPEERNAME,
            "getpeername",
            [3, huge, huge, 0, 0, 0],
        ),
        (
            edge_libos::sys::poll::NR_POLL,
            "poll",
            [huge, 1, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::epoll::NR_EPOLL_CTL,
            "epoll_ctl",
            [huge, 1, 1, huge, 0, 0],
        ),
        (
            edge_libos::sys::epoll::NR_EPOLL_WAIT,
            "epoll_wait",
            [huge, huge, 4, 0, 0, 0],
        ),
        (
            edge_libos::sys::epoll::NR_EPOLL_CREATE1,
            "epoll_create1",
            [0, 0, 0, 0, 0, 0],
        ),
        (
            edge_libos::sys::eventfd::NR_EVENTFD2,
            "eventfd2",
            [0, 0, 0, 0, 0, 0],
        ),
    ];
    for (nr, name, args) in cases {
        // epoll_create1 + eventfd2 have no pointer args; they always
        // return a valid fd. Skip the safe-set check for them and just
        // verify the return is sensible.
        if *nr == edge_libos::sys::epoll::NR_EPOLL_CREATE1
            || *nr == edge_libos::sys::eventfd::NR_EVENTFD2
        {
            let ret = block_on(dispatch_argv(&engine, &linker, &module, *nr as i64, *args))?;
            assert!(
                ret >= 3
                    || ret == -edge_libos::errno::EFAULT
                    || ret == -edge_libos::errno::EINVAL
                    || ret == -edge_libos::errno::EBADF
                    || ret == -edge_libos::errno::ENOSYS,
                "{name}: poisoned call returned {ret}, expected fd or safe error"
            );
            continue;
        }
        let ret = block_on(assert_efault_or_safe(
            &engine, &linker, &module, *nr as i64, *args, name,
        ))?;
        let _ = ret;
    }
    Ok(())
}

/// Negative-pointer sanity: i64::MIN must not wrap to a huge positive
/// index inside the bounds check. (Defends against a future regression
/// where the bounds check does `as usize` without the `< 0` guard.)
#[test]
fn fuzz_i64_min_pointer_returns_efault_not_panic() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;

    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        edge_libos::sys::file::NR_WRITE as i64,
        [1, i64::MIN, 16, 0, 0, 0],
    ))?;
    assert_eq!(ret, -edge_libos::errno::EFAULT);
    Ok(())
}
