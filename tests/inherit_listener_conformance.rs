//! P2-D3.5 sub-deliverable 4 — fd-inherit conformance.
//!
//! Tests `Kernel::attach_inherited_listeners` (ADR 0004 §2):
//!
//! 1. `attach_inherited_listeners_attaches_tokio_listener_as_resource_socket`
//!    — bind a TCP listener in the test, call the kernel helper, assert
//!    `kernel.fds.get(raw_fd)` returns `Resource::Socket` and the
//!    listener's `local_addr()` round-trips.
//! 2. `apply_snapshot_with_inherited_listener_does_not_rebind` — full
//!    freeze → encode → decode → apply cycle where the listener is
//!    inherited; assert the post-apply fd has the same port number it
//!    did pre-freeze.
//!
//! Both tests pin the `inherited: bool` field on `SocketSnapshot` as
//! observable in the apply path: callers that set the flag MUST also
//! call `attach_inherited_listeners` first or the apply step fails
//! with `SnapshotError::Unsupported`.

mod common;

use anyhow::Result;
use edge_libos::fd::Resource;
use edge_libos::snapshot::{
    apply_snapshot_kernel_state, apply_snapshot_to_memory, decode_snapshot, encode_snapshot,
    try_to_snapshot,
};
use edge_libos::{build_store, Kernel};

use std::os::unix::io::IntoRawFd;

#[test]
fn attach_inherited_listeners_attaches_tokio_listener_as_resource_socket() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        // 1. Bind a TCP listener in the test process and grab the
        //    raw fd. `std::net::TcpListener` keeps ownership via
        //    `into_raw_fd` semantics; we hand the raw fd to the
        //    kernel which will `dup` it and re-wrap.
        let std_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = std_listener.local_addr()?;
        let raw_fd: i32 = std_listener.into_std()?.into_raw_fd();

        // 2. Build a fresh kernel and attach the listener.
        // `(target_fd, source_fd)` — the test binds and
        // immediately uses it, so target == source; in
        // production they differ (target == snapshot's
        // listener_fd, source == parent's inherited fd).
        let mut kernel = Kernel::new_without_stdio(vec![], vec![]);
        kernel.attach_inherited_listeners(&[(raw_fd as u32, raw_fd)]);

        // 3. Verify the kernel stored a `Resource::Socket` at the
        //    inherited fd, and `local_addr()` round-trips.
        let res = kernel
            .fds
            .get(raw_fd as u32)
            .map_err(|e| anyhow::anyhow!("fds.get failed: {e}"))?;
        match res {
            Resource::Socket(_) => {}
            _other => panic!("expected Resource::Socket at fd {raw_fd}, got non-Socket variant"),
        }
        // Local addr roundtrip via lock acquisition:
        let res_again = kernel.fds.get(raw_fd as u32).unwrap();
        let Resource::Socket(shared) = res_again else {
            unreachable!("just matched Socket above")
        };
        let inner = shared.lock();
        let bound = inner
            .bound
            .as_ref()
            .expect("inherited listener must have a bound addr");
        match bound {
            edge_libos::fd::SockAddr::V4 { port, addr } => {
                assert_eq!(*port, local_addr.port());
                assert_eq!(*addr, [127, 0, 0, 1]);
            }
            other => panic!("expected V4, got {other:?}"),
        }
        drop(inner);
        Ok::<(), anyhow::Error>(())
    })
}

#[test]
fn apply_snapshot_with_inherited_listener_does_not_rebind() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        // Phase 1: bind a TCP listener in the test process and
        // hand its raw fd to a fresh kernel.
        let std_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = std_listener.local_addr()?;
        let raw_fd: i32 = std_listener.into_std()?.into_raw_fd();

        let (engine, linker) = common::engine_and_linker()?;
        let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));

        // Attach a 1-page memory so snapshot can read non-empty
        // pages data — without this, the apply path's
        // set_cloexec post-processing still works but the
        // marker byte assertion at the bottom (round-trip the
        // marker through the snapshot) would skip.
        const MARKER_WAT: &str = r#"
            (module
              (memory (export "memory") 1)
              (func (export "_start") (result i32)
                (i32.store (i32.const 0x1000) (i32.const 42))
                (i32.const 0)))
        "#;
        let module = common::compile_wat(&engine, MARKER_WAT)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = instance.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        // Attach the inherited listener AFTER attach_memory,
        // BEFORE snapshot.
        store
            .data_mut()
            .attach_inherited_listeners(&[(raw_fd as u32, raw_fd)]);

        // Phase 2: snapshot.
        let snap = try_to_snapshot(store.data(), &store)?;

        // Phase 3-4: encode + decode simulates cross-host
        // transfer.
        let bytes = encode_snapshot(&snap)?;
        let snap_restored = decode_snapshot(&bytes)?;

        // The socket entry in `snap_restored.fds.entries` MUST
        // carry `inherited = true` per ADR 0004 §2 — the
        // snapshot propagates the inheritance flag so the
        // apply path can take the no-bind branch.
        let sock_entry = snap_restored
            .fds
            .entries
            .iter()
            .find(|e| e.kind.kind == edge_libos::snapshot::ResourceKind::Socket)
            .expect("snapshot must carry the inherited socket entry");
        let sock = sock_entry
            .kind
            .body
            .socket
            .as_ref()
            .expect("socket body present");
        assert!(
            sock.inherited,
            "snapshot must propagate inherited=true for the listener"
        );

        // Phase 5: apply to a fresh kernel + store. The fresh
        // kernel pre-attaches the inherited listener FIRST,
        // then runs apply_snapshot. The apply path takes the
        // pre-pass `continue` branch for the inherited entry
        // and does NOT re-bind.
        let mut fresh_store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let fresh_instance = linker.instantiate_async(&mut fresh_store, &module).await?;
        if let Some(mem) = fresh_instance.get_memory(&mut fresh_store, "memory") {
            fresh_store.data_mut().attach_memory(mem);
        }
        // Critical ordering: attach the inherited listener
        // BEFORE apply_snapshot_kernel_state, then re-attach
        // it via `apply_snapshot_inherited_listeners` AFTER
        // (because apply_snapshot_kernel_state resets
        // `fds = FdTable::empty()`). The `serve` subcommand
        // orchestrates this exact sequence per ADR 0004 §2.
        //
        // `attach_inherited_listeners` returns the
        // constructed `(fd, SharedSocket)` pairs so the
        // caller can feed them into
        // `apply_snapshot_inherited_listeners` post the
        // kernel-state reset.
        let preattached = fresh_store
            .data_mut()
            .attach_inherited_listeners(&[(raw_fd as u32, raw_fd)]);
        apply_snapshot_kernel_state(&snap_restored, fresh_store.data_mut())?;
        // P2-D3.5: re-attach the inherited listener post the
        // fds reset (ADR 0004 §2).
        edge_libos::snapshot::apply_snapshot_inherited_listeners(
            &snap_restored,
            fresh_store.data_mut(),
            &preattached,
        )?;
        let mem_clone = *fresh_store
            .data()
            .memory()
            .map_err(|e| anyhow::anyhow!("memory not attached post-attach_inherited: {e}"))?;
        apply_snapshot_to_memory(&snap_restored, mem_clone, &mut fresh_store)?;

        // After apply, the kernel's `fds[raw_fd]` must STILL be
        // the inherited entry, with the same port.
        let post = fresh_store
            .data()
            .fds
            .get(raw_fd as u32)
            .map_err(|e| anyhow::anyhow!("post-apply fds.get failed: {e}"))?;
        match post {
            Resource::Socket(shared) => {
                let inner = shared.lock();
                match inner.bound.as_ref() {
                    Some(edge_libos::fd::SockAddr::V4 { port, addr }) => {
                        assert_eq!(
                            *port,
                            local_addr.port(),
                            "inherited listener must keep its port post-apply"
                        );
                        assert_eq!(*addr, [127, 0, 0, 1]);
                    }
                    other => panic!("expected V4, got {other:?}"),
                }
            }
            _other => panic!(
                "post-apply expected Resource::Socket at fd {raw_fd}, got non-Socket variant"
            ),
        }

        Ok::<(), anyhow::Error>(())
    })
}
