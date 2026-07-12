//! Socket conformance — P1-1 onward: socket(2), bind(2), listen(2),
//! accept4(2). P1-4 lands the first async-suspending socket syscall.
//!
//! Tests exercise `socket/family, type_and_flags, protocol`,
//! `bind`, `listen`, and `accept4` end-to-end via WAT modules. Later
//! P1 sub-steps will add tests for connect/recvfrom/sendto/getsockopt/
//! shutdown/poll/epoll.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use edge_libos::fd::Resource;
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

async fn call_bind(
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

async fn call_listen(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    backlog: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, backlog)).await?)
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
// ---------------------------------------------------------------------------
// P1-2: bind(2) + listen(2)
// ---------------------------------------------------------------------------

// sockaddr_in layout: u16 family | u16 port (BE) | u32 addr (BE) | u8 pad[8].
// We place one at offset 4096 for bind WATs.

/// `bind(fd, addr@4096, 16)` — uses a hardcoded INET sockaddr with
/// family=AF_INET(2), port=8080 (BE), addr=127.0.0.1.
const BIND_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; struct sockaddr_in at offset 4096 (16 bytes):
      ;; 4096: sin_family = 2 (AF_INET)
      ;; 4098: sin_port = 8080 BE
      ;; 4100: sin_addr = 127.0.0.1
      ;; 4104: pad[8] = 0
      (data (i32.const 4096)
        "\02\00"            ;; family = AF_INET (2)
        "\1f\90"            ;; port = 8080 BE
        "\7f\00\00\01"      ;; addr = 127.0.0.1
        "\00\00\00\00\00\00\00\00")
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 49)             ;; NR_BIND
          (local.get $fd)
          (i64.const 4096)           ;; addr pointer
          (i64.const 16)             ;; addrlen
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `listen(fd, backlog)` — no pointer args.
const LISTEN_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (param $backlog i64) (result i64)
        (call $syscall
          (i64.const 50)             ;; NR_LISTEN
          (local.get $fd)
          (local.get $backlog)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

// Reuse SOCKET_WAT, CLOSE_WAT, call_socket, call_close, TmpDir, block_on from above.

/// `socket → bind(127.0.0.1:8080) → listen(5) → close` returns 0 end-to-end.
#[test]
fn bind_listen_loopback_succeeds() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_WAT)?;
    let listen = common::compile_wat(&engine, LISTEN_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        assert!(fd >= 3, "socket fd should be >= 3, got {fd}");

        let bind_ret = call_bind(&linker, &mut store, &bind, fd).await?;
        assert_eq!(bind_ret, 0, "bind() should return 0");

        let listen_ret = call_listen(&linker, &mut store, &listen, fd, 5).await?;
        assert_eq!(listen_ret, 0, "listen() should return 0");

        // Verify the kernel state was actually updated.
        match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => {
                assert!(s.bound.is_some(), "bind should have recorded the address");
                assert_eq!(s.listen_backlog, Some(5), "listen should have recorded the backlog");
                assert!(s.is_listening(), "bind+listen -> is_listening");
            }
            Err(e) => panic!("fd {fd} was missing after bind+listen: {e}"),
            Ok(other) => panic!("fd {fd} was not a Socket resource: found {} variant",
                std::any::type_name::<Resource>()),
        }

        let close_ret = call_close(&linker, &mut store, &close, fd).await?;
        assert_eq!(close_ret, 0, "close should return 0");
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// `listen(fd, -1)` returns -EINVAL.
#[test]
fn listen_negative_backlog_returns_einval() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let listen = common::compile_wat(&engine, LISTEN_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_listen(&linker, &mut store, &listen, fd, -1).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EINVAL, "negative backlog should return -EINVAL");
    Ok(())
}

/// `listen(fd, 5)` without prior `bind()` returns -EDESTADDRREQ.
#[test]
fn listen_without_bind_returns_edestaddrreq() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let listen = common::compile_wat(&engine, LISTEN_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_listen(&linker, &mut store, &listen, fd, 5).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EDESTADDRREQ,
        "listen without bind should return -EDESTADDRREQ");
    Ok(())
}

/// `bind(fd, truncated_sockaddr_in, 8)` returns -EINVAL because
/// `parse_sockaddr` requires the full 16 bytes for `sockaddr_in`.
#[test]
fn bind_truncated_sockaddr_returns_einval() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;

    // Same `bind` shape as BIND_WAT but with addrlen=8 (too short for sockaddr_in).
    let bind_trunc_wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096)
            "\02\00"            ;; family = AF_INET (2)
            "\1f\90"            ;; port = 8080 BE
            "\7f\00\00\01")     ;; addr = 127.0.0.1
          (func (export "go") (param $fd i64) (result i64)
            (call $syscall
              (i64.const 49)
              (local.get $fd)
              (i64.const 4096)
              (i64.const 8)            ;; addrlen = 8 (truncated)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let bind = common::compile_wat(&engine, bind_trunc_wat)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_bind(&linker, &mut store, &bind, fd).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EINVAL,
        "truncated sockaddr_in should return -EINVAL");
    Ok(())
}

/// `bind` against a non-socket fd (close one to get an EBADF path) returns -EBADF.
/// We use the stdin fd (0), which is a Stdin resource, not a Socket.
#[test]
fn bind_on_non_socket_returns_ebadf() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let bind = common::compile_wat(&engine, BIND_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_bind(&linker, &mut store, &bind, 0 /*stdin*/).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EBADF, "bind on stdin should return -EBADF");
    Ok(())
}

// ---------------------------------------------------------------------------
// P1-4: accept4(2) + accept(2) — first async-suspending socket syscall
// ---------------------------------------------------------------------------

/// `accept4(fd, addr_ptr, addrlen_ptr, flags)` — flags bit 12 = SOCK_NONBLOCK,
/// 25 = SOCK_CLOEXEC. Returns new fd on success.
const ACCEPT4_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; Output sockaddr_in goes to 4096 (16B). addrlen goes to 4112 (4B).
      (func (export "go")
        (param $fd i64) (param $flags i64)
        (result i64)
        (call $syscall
          (i64.const 288)             ;; NR_ACCEPT4
          (local.get $fd)
          (i64.const 4096)            ;; addr ptr
          (i64.const 4112)            ;; addrlen ptr
          (local.get $flags)
          (i64.const 0) (i64.const 0)))
    )
"#;

/// `accept(fd, addr_ptr, addrlen_ptr)` — legacy shim.
const ACCEPT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 43)              ;; NR_ACCEPT
          (local.get $fd)
          (i64.const 4096)
          (i64.const 4112)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

async fn call_accept4(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    flags: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, flags)).await?)
}

async fn call_accept(
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

/// `accept4` on an fd that isn't a Socket returns -EBADF.
#[test]
fn accept4_on_non_socket_returns_ebadf() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let acc = common::compile_wat(&engine, ACCEPT4_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_accept4(&linker, &mut store, &acc, 0 /*stdin*/, 0).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EBADF);
    Ok(())
}

/// `accept4` on a Socket that hasn't been bound+listened returns -EINVAL.
#[test]
fn accept4_on_unbound_socket_returns_einval() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let acc = common::compile_wat(&engine, ACCEPT4_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_accept4(&linker, &mut store, &acc, fd, 0).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EINVAL);
    Ok(())
}

/// Same as above, but binds to a fixed port and uses port-discovery via
/// the kernel's lazy listener. This is the canonical P1-4 integration test.
///
/// Implementation: open a host-side TcpListener, capture its bound port,
/// then bind the guest socket to that exact port. The guest accept4 will
/// race against a host-side connect. The lazy listener in the kernel will
/// race-bind to the same port; if the host wins, the guest accept4
/// receives our peer.
#[test]
fn accept4_after_host_connect_returns_valid_fd() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;

    // Open the host listener first so we know the port. We do this OUTSIDE
    // the runtime because we only need the ephemeral port number; the
    // actual listener is dropped before we drop into the runtime.
    let host_listener_std = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind host listener");
    let port = host_listener_std.local_addr().unwrap().port();
    drop(host_listener_std);

    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_WAT)?;
    let listen = common::compile_wat(&engine, LISTEN_WAT)?;
    let acc = common::compile_wat(&engine, ACCEPT4_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    // Custom BIND_WAT that takes a port (16-bit) in addition to fd:
    // builds sockaddr_in at offset 4096 with the guest-supplied port.
    // Family = AF_INET(2), addr = 127.0.0.1.
    let bind_param_wat = format!(r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          ;; Build sockaddr_in at offset 4096 at module-instantiation time.
          ;; family = AF_INET (2 LE).
          ;; port = BE-encoded from $port param (we patch the WAT string).
          ;; addr = 127.0.0.1.
          ;; patch: replace PATCH_PORT with the BE-encoded port bytes.
          (data (i32.const 4096)
            "\02\00PATCH_PORT\7f\00\00\01"
            "\00\00\00\00\00\00\00\00")
          (func (export "go") (param $fd i64) (result i64)
            (call $syscall
              (i64.const 49)
              (local.get $fd)
              (i64.const 4096)
              (i64.const 16)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#);
    let port_be = port.to_be_bytes();
    let bind_param_wat = bind_param_wat.replace(
        "PATCH_PORT",
        &format!("\\{:02x}\\{:02x}", port_be[0], port_be[1]),
    );
    let bind_param = common::compile_wat(&engine, &bind_param_wat)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        let bind_ret = call_bind(&linker, &mut store, &bind_param, fd).await?;
        assert_eq!(bind_ret, 0, "bind to {port} should return 0");
        let listen_ret = call_listen(&linker, &mut store, &listen, fd, 1).await?;
        assert_eq!(listen_ret, 0);

        // Spawn a host connect that races against the guest's lazy listener.
        // The host-side std listener was already dropped above (we only
        // needed the port number). The kernel will lazily TcpListener::bind
        // to that exact port inside accept4.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Connect to the port we know the kernel is about to bind to.
        let connect_task = tokio::spawn(async move {
            tokio::net::TcpStream::connect(("127.0.0.1", port)).await
        });

        // Race: accept4 has 3 seconds. If the kernel bind succeeds and
        // our connect lands first, we get a real fd with stream=Some.
        let new_fd_res = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            call_accept4(&linker, &mut store, &acc, fd, 0),
        ).await;

        match new_fd_res {
            Ok(Ok(new_fd)) => {
                assert!(new_fd >= 3, "accept4 returned {new_fd}");
                let _ = connect_task.await;
                match store.data().fds.get(new_fd as u32) {
                    Ok(Resource::Socket(s)) => {
                        assert!(s.stream.is_some(),
                            "accepted fd must have stream=Some");
                    }
                    Ok(_) => panic!("fd {new_fd} was not a Socket resource"),
                    Err(e) => panic!("fd {new_fd} lookup failed: {e}"),
                }
                let _ = call_close(&linker, &mut store, &close, new_fd).await?;
                let _ = call_close(&linker, &mut store, &close, fd).await?;
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                let _ = connect_task.await;
                panic!("accept4 timed out after 3s — kernel bind or connect failed");
            }
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// P1-5: connect + sendto + recvfrom — the data path
// ---------------------------------------------------------------------------

/// `connect(fd, addr_ptr, addrlen)` — takes a pre-built sockaddr_in
/// (assumed to already be at offset 4096 with family=AF_INET, port=0,
/// addr=127.0.0.1).
const CONNECT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096)
        "\02\00"                   ;; family = AF_INET (2)
        "\00\00"                   ;; port = 0
        "\7f\00\00\01"             ;; addr = 127.0.0.1
        "\00\00\00\00\00\00\00\00")
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 42)              ;; NR_CONNECT
          (local.get $fd)
          (i64.const 4096)            ;; addr pointer
          (i64.const 16)              ;; addrlen
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `sendto(fd, buf_ptr, len, flags, addr, addrlen)` — flags/addr/addrlen are
/// unused for TCP; we accept them but ignore.
const SENDTO_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (param $len i64) (result i64)
        (call $syscall
          (i64.const 44)              ;; NR_SENDTO
          (local.get $fd)
          (i64.const 4096)            ;; buf pointer
          (local.get $len)
          (i64.const 0)               ;; flags
          (i64.const 0)               ;; addr (ignored for TCP)
          (i64.const 0)))             ;; addrlen (ignored for TCP)
    )
"#;

/// `recvfrom(fd, buf_ptr, len, flags, addr_ptr, addrlen_ptr)` — reads up
/// to `len` bytes; addr/addrlen written back as the peer.
const RECVFROM_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (param $len i64) (result i64)
        (call $syscall
          (i64.const 45)              ;; NR_RECVFROM
          (local.get $fd)
          (i64.const 4200)            ;; buf pointer (separate from bind's 4096)
          (local.get $len)
          (i64.const 0)               ;; flags
          (i64.const 0)               ;; addr ptr (skip peer write-back)
          (i64.const 0)))             ;; addrlen ptr (skip peer write-back)
    )
"#;

async fn call_connect(
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

async fn call_sendto(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    len: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, len)).await?)
}

/// Variant of call_sendto that reuses an existing instance — needed when
/// the test has written payload bytes into the guest memory after the
/// instance was created (each `instantiate_async` creates a fresh memory).
async fn call_sendto_reuse(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    instance: &wasmtime::Instance,
    fd: i64,
    len: i64,
) -> Result<i64> {
    let f = instance.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, len)).await?)
}

async fn call_recvfrom(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    len: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, len)).await?)
}

/// Variant of call_recvfrom that reuses an existing instance — needed
/// when the test wants to read the bytes back from the same memory the
/// recvfrom wrote into.
async fn call_recvfrom_reuse(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    instance: &wasmtime::Instance,
    fd: i64,
    len: i64,
) -> Result<i64> {
    let f = instance.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, len)).await?)
}

/// `connect` on a fd without a stream returns -ENOTCONN only after we've
/// already established it's a Socket — but connect is a one-shot setup,
/// so the proper negative test is: connect on a non-socket fd → -EBADF.
#[test]
fn connect_on_non_socket_returns_ebadf() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let conn = common::compile_wat(&engine, CONNECT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_connect(&linker, &mut store, &conn, 0 /*stdin*/).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EBADF);
    Ok(())
}

/// `connect` to a closed port returns -ECONNREFUSED.
#[test]
fn connect_to_closed_port_returns_ECONNREFUSED() -> Result<()> {
    let _d = TmpDir::new();
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let conn = common::compile_wat(&engine, CONNECT_WAT)?;

    // Build a custom CONNECT_WAT that uses port 1 (privileged, almost
    // certainly closed) on 127.0.0.1. The bound BIND_WAT uses port 8080 —
    // we need a new WAT for connect.
    let conn_port1_wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096)
            "\02\00"                   ;; family = AF_INET (2)
            "\00\01"                   ;; port = 1 (BE)
            "\7f\00\00\01"             ;; addr = 127.0.0.1
            "\00\00\00\00\00\00\00\00")
          (func (export "go") (param $fd i64) (result i64)
            (call $syscall
              (i64.const 42)
              (local.get $fd)
              (i64.const 4096)
              (i64.const 16)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let conn_p1 = common::compile_wat(&engine, conn_port1_wat)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        // Note: this could also hit -EADDRNOTAVAIL on some systems, or
        // -ETIMEDOUT if the firewall drops. We accept any of the three.
        call_connect(&linker, &mut store, &conn_p1, fd).await
    })?;
    let _ = conn; // silence unused-warning
    assert!(
        ret == -edge_libos::errno::ECONNREFUSED
            || ret == -edge_libos::errno::ETIMEDOUT
            || ret == -edge_libos::errno::EIO,
        "expected ECONNREFUSED/ETIMEDOUT/EIO, got {ret}"
    );
    Ok(())
}

/// End-to-end data path: kernel listen → kernel accept → host connect →
/// host writes "hello" → kernel recvfrom returns 5 → kernel sendto
/// "world" → host reads "world". This is the canonical P1-5 DoD test.
#[test]
fn sendto_then_recvfrom_roundtrips_over_loopback() -> Result<()> {
    let _d = TmpDir::new();

    // Open a host listener for an ephemeral port.
    let host_listener_std = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind host listener");
    let port = host_listener_std.local_addr().unwrap().port();
    drop(host_listener_std);

    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_WAT)?;
    let listen = common::compile_wat(&engine, LISTEN_WAT)?;
    let acc = common::compile_wat(&engine, ACCEPT4_WAT)?;
    let sendto = common::compile_wat(&engine, SENDTO_WAT)?;
    let recvfrom = common::compile_wat(&engine, RECVFROM_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    // Build a bind WAT for the specific port.
    let port_be = port.to_be_bytes();
    let bind_wat = format!(r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096)
            "\02\00PATCH_PORT\7f\00\00\01"
            "\00\00\00\00\00\00\00\00")
          (func (export "go") (param $fd i64) (result i64)
            (call $syscall
              (i64.const 49)
              (local.get $fd)
              (i64.const 4096)
              (i64.const 16)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#);
    let bind_wat = bind_wat.replace(
        "PATCH_PORT",
        &format!("\\{:02x}\\{:02x}", port_be[0], port_be[1]),
    );
    let bind_for_port = common::compile_wat(&engine, &bind_wat)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));

        // Guest: socket + bind + listen.
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        assert!(call_bind(&linker, &mut store, &bind_for_port, fd).await? == 0);
        assert!(call_listen(&linker, &mut store, &listen, fd, 1).await? == 0);

        // Race: spawn host connect and the guest accept4 concurrently.
        // The lazy TcpListener::bind inside accept4 will run first (it's
        // synchronous up until the .await on accept), so by the time the
        // host connect retries, the listener exists.
        let connect_fut = async move {
            // Brief delay to let the kernel listener come up; we'll
            // retry a few times if the host connect gets a fast ECONNREFUSED.
            for _ in 0..20 {
                match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                    Ok(s) => return Ok(s),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
            Err(anyhow::anyhow!("host connect never succeeded"))
        };
        let accept_fut = call_accept4(&linker, &mut store, &acc, fd, 0);

        let (host_res, accepted_res) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(3), connect_fut),
            tokio::time::timeout(std::time::Duration::from_secs(3), accept_fut),
        );
        let host_stream = host_res
            .map_err(|_| anyhow::anyhow!("host connect timed out"))?
            .map_err(|e| anyhow::anyhow!("host connect failed: {e}"))?;
        let accepted = accepted_res
            .map_err(|_| anyhow::anyhow!("guest accept4 timed out"))??;
        assert!(accepted >= 3, "accept4 returned {accepted}");

        let (mut host_rd, mut host_wr) = host_stream.into_split();

        // Host writes "hello".
        use tokio::io::AsyncWriteExt;
        host_wr.write_all(b"hello").await?;

        // Guest recvfrom returns 5. We instantiate the recvfrom module
        // once and reuse it so the guest memory persists for the assertion
        // read-back below (each fresh instantiation gets a new memory).
        let recv_inst = linker.instantiate_async(&mut store, &recvfrom).await?;
        if let Some(mem) = recv_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            call_recvfrom_reuse(&linker, &mut store, &recv_inst, accepted, 16),
        ).await
            .map_err(|_| anyhow::anyhow!("recvfrom timed out"))??;
        assert_eq!(n, 5, "recvfrom should return 5 bytes for 'hello'");

        // Read the bytes back from the same memory (recv_inst's memory).
        let mem = store.data().memory.ok_or_else(|| anyhow::anyhow!("no memory"))?;
        let mut got = [0u8; 5];
        mem.read(&mut store, 4200, &mut got)?;
        assert_eq!(&got, b"hello", "guest buffer should contain 'hello'");

        // Guest sendto "world" — instantiate sendto module, write payload
        // into its memory at offset 4096, then call go.
        let send_inst = linker.instantiate_async(&mut store, &sendto).await?;
        if let Some(mem) = send_inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let mem = store.data().memory.ok_or_else(|| anyhow::anyhow!("no memory"))?;
        let to_send = b"world";
        mem.write(&mut store, 4096, to_send)?;
        let sent = call_sendto_reuse(&linker, &mut store, &send_inst, accepted, 5).await?;
        assert_eq!(sent, 5, "sendto should write 5 bytes");

        // Give tokio a moment to flush the bytes through the kernel stream.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Host reads "world".
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 5];
        let _n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            host_rd.read_exact(&mut buf),
        ).await
            .map_err(|_| anyhow::anyhow!("host read timed out"))??;
        assert_eq!(&buf, b"world", "host peer should receive 'world'");

        // Clean up.
        let _ = call_close(&linker, &mut store, &close, accepted).await?;
        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
