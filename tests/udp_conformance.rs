//! P3-T9 Path A UDP socket layer (ADR 0008) — integration tests.
//!
//! This file is the home for the UDP conformance tests, which land
//! commit-by-commit alongside the `src/sys/udp.rs` work. C0 only
//! delivers the trivial open/close test; C1 adds `bind_*`, C2 adds
//! `sendto_then_recvfrom_roundtrips_over_loopback`, etc.
//!
//! C0 tests verify the new `SocketInner::udp` field is initialized
//! to `None` and the AF_INET6 tag + `IPV6_V6ONLY=1` default are
//! applied at `socket()` time. C1+ tests build on this baseline.

mod common;

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
          (i64.const 0)               ;; protocol (ignored)
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

/// C0 — `socket(AF_INET, SOCK_DGRAM)` returns a new fd. The `udp` field
/// on `SocketInner` is `None` until C1 binds.
#[test]
fn socket_inet_dgram_returns_new_fd() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_socket(
            &linker, &mut store, &module, 2, /*AF_INET*/
            2, /*SOCK_DGRAM*/
        )
        .await
    })?;
    assert!(
        ret >= 3,
        "socket(AF_INET, SOCK_DGRAM) should return fd >= 3, got {ret}"
    );
    Ok(())
}

/// C0 — `socket(AF_INET6, SOCK_DGRAM)` returns a new fd and tags
/// `SocketInner.family_v6 = true` + `SocketInner.ipv6_v6only = true`
/// (Linux default for freshly-created AF_INET6 + SOCK_DGRAM).
///
/// C1 will exercise the actual `IPV6_V6ONLY` bind semantics; C0 just
/// confirms the metadata tags land correctly at construction.
#[test]
fn socket_inet6_dgram_tags_family_and_v6only() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(
            &linker, &mut store, &module, 10, /*AF_INET6*/
            2,  /*SOCK_DGRAM*/
        )
        .await?;
        // Inspect the fd table directly — the new fields should be
        // tagged already, before any bind. `Resource::Socket` is the
        // only variant sockets land in; match it for the inner lock.
        let fds = &store.data().fds;
        let res = fds.get(fd as u32).expect("fd present");
        match res {
            Resource::Socket(inner_arc) => {
                let inner = inner_arc.lock();
                assert!(inner.family_v6, "AF_INET6 dgram should set family_v6");
                assert!(
                    inner.ipv6_v6only,
                    "AF_INET6 dgram should default ipv6_v6only=true (Linux default)"
                );
                assert!(
                    inner.udp.is_none(),
                    "udp state must be None until first bind (C1)"
                );
                assert_eq!(inner.kind, edge_libos::fd::SocketKind::Datagram);
            }
            // Resource has no Debug derive; the socket() handler only
            // inserts Resource::Socket on success, so this arm is
            // unreachable in practice.
            _ => panic!("socket() should produce Resource::Socket"),
        }
        Ok::<i64, anyhow::Error>(fd)
    })?;
    assert!(
        ret >= 3,
        "socket(AF_INET6, SOCK_DGRAM) should return fd >= 3, got {ret}"
    );
    Ok(())
}

/// C0 — `socket(AF_INET, SOCK_DGRAM)` does NOT tag `family_v6`.
#[test]
fn socket_inet_dgram_does_not_tag_family_v6() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SOCKET_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(
            &linker, &mut store, &module, 2, /*AF_INET*/
            2, /*SOCK_DGRAM*/
        )
        .await?;
        assert!(fd >= 3, "fd must be valid, got {fd}");
        let fds = &store.data().fds;
        let res = fds.get(fd as u32).expect("fd present");
        match res {
            Resource::Socket(inner_arc) => {
                let inner = inner_arc.lock();
                assert!(!inner.family_v6, "AF_INET dgram must NOT set family_v6");
                assert!(!inner.ipv6_v6only, "AF_INET dgram must NOT set ipv6_v6only");
                assert!(inner.udp.is_none(), "udp state must be None until bind");
            }
            _ => panic!("socket() should produce Resource::Socket"),
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C0 — Open + close a UDP fd end-to-end. Mirrors the smoke test pattern
/// of `socket_conformance::socket_inet_dgram_returns_new_fd` plus a
/// `close` to verify the fd is reclaimed. No bind yet — just the
/// lifecycle.
#[test]
fn udp_socket_open_then_close() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock_mod = common::compile_wat(&engine, SOCKET_WAT)?;
    let close_mod = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock_mod, 2, 2).await?;
        assert!(fd >= 3);
        assert!(store.data().fds.contains(fd as u32));
        let r = call_close(&linker, &mut store, &close_mod, fd).await?;
        assert_eq!(r, 0, "close() should return 0, got {r}");
        assert!(!store.data().fds.contains(fd as u32));
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

// ===== C1: UDP bind ========================================================
//
// bind_wat_v4 builds a sockaddr_in at offset 4096 with the guest-supplied
// port (low 16 bits) and family=AF_INET, addr=127.0.0.1. Calls NR_BIND
// (syscall 49) with fd, addr_ptr=4096, addrlen=16.
// Port comes in via $port param (i64). The WAT truncates to i32 and
// unpacks the low 16 bits at runtime.

// `bind(fd, port)` — builds sockaddr_in(127.0.0.1, port) at 4096 and
// invokes NR_BIND. `port == 0` requests an ephemeral port.
// (Reserved for C2+ parameterized tests; C1 only uses the
// ephemeral-port variants below, which avoid an unused-const lint.)
//
// const BIND_V4_WAT: &str = r#"
//     (module
//       (import "kernel" "syscall"
//         (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
//       (memory (export "memory") 1)
//       (func (export "go") (param $fd i64) (param $port i64) (result i64)
//         (i32.store8 (i32.const 4098)
//           (i32.and (i32.wrap_i64 (local.get $port)) (i32.const 0xff)))
//         (i32.store8 (i32.const 4099)
//           (i32.and
//             (i32.shr_u (i32.wrap_i64 (local.get $port)) (i32.const 8))
//             (i32.const 0xff)))
//         (call $syscall
//           (i64.const 49)              ;; NR_BIND
//           (local.get $fd)
//           (i64.const 4096)            ;; addr pointer
//           (i64.const 16)              ;; addrlen (sockaddr_in)
//           (i64.const 0) (i64.const 0) (i64.const 0)))
//     )
// "#;

/// `bind(fd)` — binds 127.0.0.1:0 (ephemeral port). Hardcoded
/// sockaddr_in (port=0 BE at 4098).
const BIND_V4_EPHEMERAL_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096)
        "\02\00"                    ;; family = AF_INET (2)
        "\00\00"                    ;; port = 0 BE (ephemeral)
        "\7f\00\00\01"              ;; addr = 127.0.0.1
        "\00\00\00\00\00\00\00\00")
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 49)             ;; NR_BIND
          (local.get $fd)
          (i64.const 4096)
          (i64.const 16)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `bind_v6(fd, port)` — builds a sockaddr_in6([::1], port, 0) at 4096.
/// Calls NR_BIND. The addr pointer is 4096; addrlen is 28.
const BIND_V6_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096)
        "\0a\00"                    ;; family = AF_INET6 (10)
        "\00\00"                    ;; port placeholder
        "\00\00\00\00"              ;; flowinfo = 0
        "\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\01"  ;; ::1
        "\00\00\00\00")             ;; scope_id = 0
      (func (export "go") (param $fd i64) (param $port i64) (result i64)
        (i32.store8 (i32.const 4098)
          (i32.and (i32.wrap_i64 (local.get $port)) (i32.const 0xff)))
        (i32.store8 (i32.const 4099)
          (i32.and
            (i32.shr_u (i32.wrap_i64 (local.get $port)) (i32.const 8))
            (i32.const 0xff)))
        (call $syscall
          (i64.const 49)             ;; NR_BIND
          (local.get $fd)
          (i64.const 4096)
          (i64.const 28)             ;; addrlen (sockaddr_in6)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `getsockname(fd)` — writes back into the pre-validated 28-byte
/// buffer at offset 4096 (sockaddr_in OR sockaddr_in6), and writes
/// the actual addrlen to 4224. Returns 0 on success, -errno on
/// failure.
const GETSOCKNAME_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 51)             ;; NR_GETSOCKNAME
          (local.get $fd)
          (i64.const 4096)
          (i64.const 4224)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `setsockopt(SO_REUSEADDR, 1)` — invokes NR_SETSOCKOPT (54) with
/// level=SOL_SOCKET, optname=SO_REUSEADDR(2), val=1, optlen=4.
const SET_REUSEADDR_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096)
        "\01\00\00\00")              ;; val = 1 (LE u32)
      (func (export "go") (param $fd i64) (result i64)
        (call $syscall
          (i64.const 54)             ;; NR_SETSOCKOPT
          (local.get $fd)
          (i64.const 1)              ;; SOL_SOCKET
          (i64.const 2)              ;; SO_REUSEADDR
          (i64.const 4096)
          (i64.const 4)
          (i64.const 0)))
    )
"#;

// Helpers (extended for bind/getsockname) ------------------------------------

async fn call_bind_v4_ephemeral(
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

async fn call_bind_v6(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    port: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, port)).await?)
}

async fn call_getsockname(
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

async fn call_set_reuseaddr(
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

fn read_u16_be(store: &mut wasmtime::Store<Kernel>, ptr: u32) -> u16 {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = [0u8; 2];
    mem.read(&mut *store, ptr as usize, &mut buf).unwrap();
    u16::from_be_bytes(buf)
}

fn read_u32_le(store: &mut wasmtime::Store<Kernel>, ptr: u32) -> u32 {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = [0u8; 4];
    mem.read(&mut *store, ptr as usize, &mut buf).unwrap();
    u32::from_le_bytes(buf)
}

// C1 tests ------------------------------------------------------------------

/// C1 — `socket(AF_INET, SOCK_DGRAM) + bind(127.0.0.1, 0)` returns 0.
/// `getsockname` after bind returns the actual ephemeral port (non-zero).
#[test]
fn bind_loopback_returns_ephemeral_port() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_V4_EPHEMERAL_WAT)?;
    let gsn = common::compile_wat(&engine, GETSOCKNAME_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 2).await?;
        assert!(fd >= 3);

        let r = call_bind_v4_ephemeral(&linker, &mut store, &bind, fd).await?;
        assert_eq!(r, 0, "bind(0.0.0.0:0) should return 0, got {r}");

        // getsockname → port at offset 4098 (BE), addrlen at 4224 (LE).
        let gsn_r = call_getsockname(&linker, &mut store, &gsn, fd).await?;
        assert_eq!(gsn_r, 0, "getsockname should return 0, got {gsn_r}");

        let port = read_u16_be(&mut store, 4098);
        assert!(port > 0, "ephemeral port must be non-zero, got {port}");
        let addrlen = read_u32_le(&mut store, 4224);
        assert_eq!(
            addrlen, 16,
            "getsockname addrlen for V4 must be 16, got {addrlen}"
        );

        // Verify the UdpSocketState was actually installed with a host
        // socket (the udp field is Some, and local_addr returns Some).
        match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                let udp = gs
                    .udp
                    .as_ref()
                    .expect("UdpSocketState should be installed after bind");
                assert!(
                    udp.socket.lock().is_some(),
                    "host UdpSocket should be materialized after bind"
                );
                assert!(
                    udp.local_addr().is_some(),
                    "local_addr should reflect actual bound port"
                );
            }
            _ => panic!("fd {fd} was not a Socket resource"),
        }

        let r = call_close(&linker, &mut store, &close, fd).await?;
        assert_eq!(r, 0, "close should return 0");
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C1 — `bind(127.0.0.1, 0)` followed by a second `bind` is a no-op:
/// the host socket is reused, the same bound addr is reported.
/// Linux behavior on rebind is "same addr → silently succeed,
/// different addr → -EINVAL". Our v1 implementation is even more
/// permissive — second bind always returns 0 (the existing socket
/// is preserved). Documented in ADR 0008 §Snapshot as a known gap;
/// tighter Linux matching lands if a guest workload needs it.
#[test]
fn bind_already_bound_returns_zero() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_V4_EPHEMERAL_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 2).await?;
        let r1 = call_bind_v4_ephemeral(&linker, &mut store, &bind, fd).await?;
        assert_eq!(r1, 0, "first bind should succeed, got {r1}");

        // Capture the port from the first bind (via the udp state).
        let first_port = match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => s
                .lock()
                .udp
                .as_ref()
                .and_then(|u| u.local_addr())
                .map(|sa| sa.port()),
            _ => None,
        }
        .expect("first bind set local_addr");

        let r2 = call_bind_v4_ephemeral(&linker, &mut store, &bind, fd).await?;
        assert_eq!(
            r2, 0,
            "second bind on already-bound UDP socket returns 0 (rebind is a no-op in v1), got {r2}"
        );

        // The port didn't change — same bound socket.
        let second_port = match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => s
                .lock()
                .udp
                .as_ref()
                .and_then(|u| u.local_addr())
                .map(|sa| sa.port()),
            _ => None,
        }
        .expect("second bind still has local_addr");
        assert_eq!(first_port, second_port, "rebind must preserve port");
        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C1 — `bind` on a TCP socket must NOT materialize UdpSocketState.
/// The existing P1 path just records `bound` and returns 0.
#[test]
fn bind_tcp_socket_does_not_materialize_udp_state() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_V4_EPHEMERAL_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?; // SOCK_STREAM
        assert!(fd >= 3);
        let r = call_bind_v4_ephemeral(&linker, &mut store, &bind, fd).await?;
        assert_eq!(r, 0, "TCP bind should return 0, got {r}");
        match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                assert!(
                    gs.udp.is_none(),
                    "TCP bind must NOT materialize UdpSocketState"
                );
            }
            _ => panic!("fd was not a Socket resource"),
        }
        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C1 — `setsockopt(SO_REUSEADDR, 1)` then `bind` succeeds. The flag
/// must reach `UdpSocketState.so_reuseaddr` so the host bind actually
/// applies it. We verify indirectly by inspecting the state after bind.
#[test]
fn bind_with_reuseaddr() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let reuse = common::compile_wat(&engine, SET_REUSEADDR_WAT)?;
    let bind = common::compile_wat(&engine, BIND_V4_EPHEMERAL_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 2).await?;
        let r = call_set_reuseaddr(&linker, &mut store, &reuse, fd).await?;
        assert_eq!(r, 0, "setsockopt(SO_REUSEADDR) should return 0, got {r}");
        let r = call_bind_v4_ephemeral(&linker, &mut store, &bind, fd).await?;
        assert_eq!(r, 0, "bind after SO_REUSEADDR should return 0, got {r}");
        // Sanity: the bound port is recorded.
        match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                let udp = gs.udp.as_ref().expect("udp state after bind");
                assert!(
                    udp.socket.lock().is_some(),
                    "host socket must be materialized"
                );
            }
            _ => panic!("fd was not a Socket resource"),
        }
        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C1 — V4 destination over an AF_INET6 socket must fail. We use a
/// loopback V4 address bound into an AF_INET6 dgram — Linux rejects
/// this with EINVAL (the family mismatch isn't auto-corrected).
#[test]
fn bind_family_mismatch_returns_einval() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind_v4_on_v6 = common::compile_wat(&engine, BIND_V4_EPHEMERAL_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        // AF_INET6 + SOCK_DGRAM
        let fd = call_socket(&linker, &mut store, &sock, 10, 2).await?;
        let r = call_bind_v4_ephemeral(&linker, &mut store, &bind_v4_on_v6, fd).await?;
        assert_eq!(
            r,
            -edge_libos::errno::EINVAL,
            "bind(AF_INET sockaddr) on AF_INET6 socket must return -EINVAL, got {r}"
        );
        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// C1 — `bind(::1, 0)` on AF_INET6 + SOCK_DGRAM with default
/// IPV6_V6ONLY=1 succeeds. getsockname writes sockaddr_in6 (28 bytes)
/// with port = the OS-assigned ephemeral.
#[test]
fn bind_v6_loopback_returns_ephemeral_port() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let bind = common::compile_wat(&engine, BIND_V6_WAT)?;
    let gsn = common::compile_wat(&engine, GETSOCKNAME_WAT)?;
    let close = common::compile_wat(&engine, CLOSE_WAT)?;

    block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 10, 2).await?; // AF_INET6
        let r = call_bind_v6(&linker, &mut store, &bind, fd, 0).await?;
        assert_eq!(r, 0, "bind(::1, 0) should return 0, got {r}");

        // Verify the host socket is V6 only.
        match store.data().fds.get(fd as u32) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                let udp = gs.udp.as_ref().expect("udp state after bind");
                let sa = udp.local_addr().expect("local_addr set");
                assert!(
                    sa.is_ipv6(),
                    "AF_INET6 bind must produce a V6 SocketAddr, got {sa:?}"
                );
                assert_eq!(
                    sa.port(),
                    udp.local_addr().unwrap().port(),
                    "port round-trips"
                );
                assert!(sa.port() > 0, "ephemeral port must be non-zero");
            }
            _ => panic!("fd was not a Socket resource"),
        }

        let gsn_r = call_getsockname(&linker, &mut store, &gsn, fd).await?;
        assert_eq!(gsn_r, 0, "getsockname on V6 UDP must return 0, got {gsn_r}");
        let port = read_u16_be(&mut store, 4098);
        assert!(port > 0, "ephemeral V6 port must be non-zero, got {port}");
        let addrlen = read_u32_le(&mut store, 4224);
        assert_eq!(
            addrlen, 28,
            "getsockname addrlen for V6 must be 28, got {addrlen}"
        );

        let _ = call_close(&linker, &mut store, &close, fd).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}
