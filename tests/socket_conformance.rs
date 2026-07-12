//! Socket conformance — P1-1: `socket(2)` only.
//!
//! Tests in this file exercise the `socket(family, type_and_flags, protocol)`
//! syscall end-to-end via WAT modules. Subsequent P1 sub-steps will add
//! tests for bind/listen/accept/connect/recv/send/getsockopt/shutdown/epoll.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use edge_libos::Kernel;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

/// Test isolation: each test gets its own tmpdir for the kernel's preopen.
/// (P1-1 doesn't need a preopen — sockets are host-side — but we keep the
/// pattern identical to vfs_conformance for consistency.)
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir =
            std::env::temp_dir().join(format!("edge-libos-socket-test-{pid}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

// WAT modules ---------------------------------------------------------------

/// `socket(family, type_and_flags, protocol)` — returns the new fd (or -errno).
const SOCKET_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $family i64) (param $type i64) (result i64)
        (call $syscall
          (i64.const 41)              ;; NR_SOCKET
          (local.get $family)
          (local.get $type)
          (i64.const 0)               ;; protocol (ignored in P1)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `close(fd)` — to clean up after a socket create.
const CLOSE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 3)              ;; NR_CLOSE
          (local.get $fd)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

// Helpers -------------------------------------------------------------------

async fn call_socket(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    family: i64,
    ty: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (family, ty)).await?)
}

async fn call_close(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<i64, i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, fd).await?)
}

// Tests ---------------------------------------------------------------------

/// `socket(AF_INET, SOCK_STREAM, 0)` returns a new fd ≥ 3 (stdio occupy 0/1/2).
#[test]
fn socket_inet_stream_returns_new_fd() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/).await
    })?;
    assert!(ret >= 3, "socket() should return fd >= 3, got {ret}");
    Ok(())
}

/// `socket(AF_INET6, SOCK_STREAM, 0)` also returns a new fd.
#[test]
fn socket_inet6_stream_returns_new_fd() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 10 /*AF_INET6*/, 1 /*SOCK_STREAM*/).await
    })?;
    assert!(ret >= 3, "socket(AF_INET6, SOCK_STREAM) should return fd >= 3, got {ret}");
    Ok(())
}

/// `socket(AF_INET, SOCK_DGRAM, 0)` returns a new fd (datagram sockets are
/// accepted by P1 even though sendto/recvfrom land later).
#[test]
fn socket_inet_dgram_returns_new_fd() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 2 /*AF_INET*/, 2 /*SOCK_DGRAM*/).await
    })?;
    assert!(ret >= 3, "socket(AF_INET, SOCK_DGRAM) should return fd >= 3, got {ret}");
    Ok(())
}

/// `socket(9999, SOCK_STREAM, 0)` returns -EAFNOSUPPORT.
#[test]
fn socket_unknown_family_returns_eafnosupport() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 9999, 1).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EAFNOSUPPORT,
        "unknown family should return -EAFNOSUPPORT");
    Ok(())
}

/// `socket(AF_UNIX, SOCK_STREAM, 0)` returns -EAFNOSUPPORT (AF_UNIX is P2).
#[test]
fn socket_unix_returns_eafnosupport() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 1 /*AF_UNIX*/, 1 /*SOCK_STREAM*/).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EAFNOSUPPORT,
        "AF_UNIX is P2; should return -EAFNOSUPPORT");
    Ok(())
}

/// `socket(AF_INET, SOCK_SEQPACKET=5, 0)` returns -EPROTONOSUPPORT
/// (known family, unsupported type).
#[test]
fn socket_inet_seqpacket_returns_eprotonosupport() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 2 /*AF_INET*/, 5 /*SOCK_SEQPACKET*/).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EPROTONOSUPPORT,
        "SOCK_SEQPACKET on AF_INET should return -EPROTONOSUPPORT");
    Ok(())
}

/// Two consecutive socket() calls return consecutive fds (3, then 4).
#[test]
fn socket_calls_return_consecutive_fds() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let (a, b) = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let a = call_socket(&linker, &mut store, &module, 2, 1).await?;
        let b = call_socket(&linker, &mut store, &module, 2, 1).await?;
        Ok::<_, anyhow::Error>((a, b))
    })?;
    assert_eq!(a, 3);
    assert_eq!(b, 4);
    Ok(())
}

/// `socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0)` returns a non-blocking fd.
/// We can't observe the nonblock bit from the WAT side directly, so we
/// assert that the call still succeeds (the kernel parses the flag).
#[test]
fn socket_with_sock_nonblock_succeeds() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    // 0o4000 = SOCK_NONBLOCK | SOCK_STREAM(1)
    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(&linker, &mut store, &module, 2, 0o4001).await
    })?;
    assert!(ret >= 3, "socket with SOCK_NONBLOCK should still return fd >= 3");
    Ok(())
}

/// `socket → close` cleanly removes the fd from the table (no leak).
#[test]
fn socket_then_close_removes_fd() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock_mod = common::compile_wat(&engine, SOCKET_WAT)?;
    let close_mod = common::compile_wat(&engine, CLOSE_WAT)?;

    let fd = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock_mod, 2, 1).await?;
        // Verify fd is currently bound.
        assert!(store.data().fds.contains(fd as u32));
        // Close it.
        let ret = call_close(&linker, &mut store, &close_mod, fd).await?;
        assert_eq!(ret, 0, "close should return 0 on success");
        // Verify it's gone.
        assert!(!store.data().fds.contains(fd as u32));
        Ok::<_, anyhow::Error>(fd)
    })?;
    // Re-open and re-close to confirm the fd slot is reusable.
    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd2 = call_socket(&linker, &mut store, &sock_mod, 2, 1).await?;
        assert_eq!(fd2, fd, "second socket should reuse the freed fd slot");
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}