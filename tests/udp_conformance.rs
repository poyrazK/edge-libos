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
