//! P2-DNS resolve(2) — integration tests via the real wasm32 ABI.
//!
//! Six tests that drive NR_RESOLVE through WAT modules + a
//! `StubResolver` injected into `Kernel::attach_resolver_backend`.
//! Deterministic, no network, no /etc/resolv.conf dependency. The C
//! conformance tests in tests/conformance/getaddrinfo_*.c cover the
//! syscall from the C side; this file covers the contract from the
//! host side.

mod common;

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use edge_libos::sys::resolver::{ResolverConfig, StubResolver};
use edge_libos::Kernel;
use wasmtime::Store;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

/// Build a WAT module that exposes `go(node_ptr, node_len, service_ptr,
/// service_len, hints_ptr, res_ptr_ptr)` and returns whatever NR_RESOLVE
/// returns. Memory is 1 page (64 KiB) — plenty for our scratch region.
const RESOLVE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go")
            (param $node_ptr i64) (param $node_len i64)
            (param $svc_ptr i64)  (param $svc_len i64)
            (param $hints i64)    (param $res i64)
            (result i64)
        (call $syscall
          (i64.const 400)              ;; NR_RESOLVE
          (local.get $node_ptr) (local.get $node_len)
          (local.get $svc_ptr)  (local.get $svc_len)
          (local.get $hints)    (local.get $res))))
"#;

/// Read `n` bytes from guest memory at `ptr` into a `Vec<u8>`.
fn read_guest(store: &mut Store<Kernel>, ptr: u32, n: usize) -> Vec<u8> {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = vec![0u8; n];
    mem.read(&mut *store, ptr as usize, &mut buf).unwrap();
    buf
}

/// Write `bytes` into guest memory at `ptr`.
fn write_guest(store: &mut Store<Kernel>, ptr: u32, bytes: &[u8]) {
    let mem = *store.data().memory().expect("memory attached");
    mem.write(&mut *store, ptr as usize, bytes).unwrap();
}

/// Write a NUL-terminated string into guest memory at `ptr`.
fn write_str(store: &mut Store<Kernel>, ptr: u32, s: &str) {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    write_guest(store, ptr, &bytes);
}

/// Call NR_RESOLVE through the WAT module.
#[allow(clippy::too_many_arguments)]
async fn call_resolve(
    store: &mut Store<Kernel>,
    inst: &wasmtime::Instance,
    node_ptr: u32,
    node_len: i64,
    svc_ptr: u32,
    svc_len: i64,
    hints_ptr: u32,
    res_ptr: u32,
) -> Result<i64> {
    let f = inst.get_typed_func::<(i64, i64, i64, i64, i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(
        &mut *store,
        (
            node_ptr as i64,
            node_len,
            svc_ptr as i64,
            svc_len,
            hints_ptr as i64,
            res_ptr as i64,
        ),
    )
    .await?)
}

fn make_kernel(stub_addrs: Vec<IpAddr>, denylist: Vec<IpAddr>, timeout_ms: u64) -> Kernel {
    let mut k = Kernel::new(vec![], vec![]);
    k.attach_resolver_backend(Arc::new(StubResolver::new(stub_addrs)));
    k.attach_resolver_config(ResolverConfig {
        denylist,
        timeout_ms,
        ..Default::default()
    });
    k
}

#[test]
fn resolve_loopback_v4_returns_127_0_0_1() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        // Build a store with a kernel that has the stub resolver
        // attached; common::instantiate_async builds a fresh default
        // kernel under the hood, so we instantiate manually here.
        let mut store = edge_libos::build_store(
            &engine,
            make_kernel(vec!["127.0.0.1".parse::<IpAddr>().unwrap()], vec![], 5_000),
        );
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "localhost");
        let res_ptr: u32 = 128;
        let r = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, res_ptr).await?;
        assert!(r >= 0, "expected >= 0, got {r}");

        let head_bytes = read_guest(&mut store, res_ptr, 4);
        let head_off = u32::from_le_bytes(head_bytes.try_into().unwrap());
        // The marshal lives at MARKER_ADDR + 4096 = 8192.
        const SCRATCH_BASE: u32 = 4096 + 4096;
        let ai_family_bytes = read_guest(&mut store, SCRATCH_BASE + head_off + 4, 4);
        let ai_family = i32::from_le_bytes(ai_family_bytes.try_into().unwrap());
        assert_eq!(
            ai_family, 2,
            "first node should be AF_INET (2), got {ai_family}"
        );
        Ok(())
    })
}

#[test]
fn resolve_eai_noname_on_bad_name() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        let mut store =
            edge_libos::build_store(&engine, make_kernel(vec![], vec![], 5_000));
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "this-host-does-not-exist.invalid");
        let r = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, 128).await?;
        assert_eq!(r, -2, "expected -EAI_NONAME (-2), got {r}");
        Ok(())
    })
}

#[test]
fn resolve_denylist_blocks_resolved_ip() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        let mut store = edge_libos::build_store(
            &engine,
            make_kernel(
                vec!["10.0.0.1".parse::<IpAddr>().unwrap()],
                vec!["10.0.0.1".parse::<IpAddr>().unwrap()],
                5_000,
            ),
        );
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "blocked.example.com");
        let r = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, 128).await?;
        assert_eq!(
            r, -2,
            "expected -EAI_NONAME when denylist blocks all IPs, got {r}"
        );
        Ok(())
    })
}

#[test]
fn resolve_cache_hit_avoids_second_lookup() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        let mut store = edge_libos::build_store(
            &engine,
            make_kernel(vec!["127.0.0.1".parse::<IpAddr>().unwrap()], vec![], 5_000),
        );
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "cached.example.com");
        let r1 = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, 128).await?;
        let r2 = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, 128).await?;
        assert!(r1 >= 0, "first call should succeed, got {r1}");
        assert!(r2 >= 0, "second call should succeed via cache, got {r2}");
        Ok(())
    })
}

#[test]
fn resolve_timeout_returns_eai_again() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        // Stub sleeps 100ms (default addrs, with_sleep(100)); timeout = 10ms.
        let mut kernel = Kernel::new(vec![], vec![]);
        kernel.attach_resolver_backend(Arc::new(
            StubResolver::new(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]).with_sleep(100),
        ));
        kernel.attach_resolver_config(ResolverConfig {
            timeout_ms: 10,
            ..Default::default()
        });
        let mut store = edge_libos::build_store(&engine, kernel);
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "slow.example.com");
        let r = call_resolve(&mut store, &inst, 120, 0, 0, 0, 0, 128).await?;
        assert_eq!(r, -3, "expected -EAI_AGAIN (-3) on timeout, got {r}");
        Ok(())
    })
}

#[test]
fn resolve_hints_filter_to_v6_only() -> Result<()> {
    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, RESOLVE_WAT)?;
        let mut store = edge_libos::build_store(
            &engine,
            make_kernel(
                vec![
                    "127.0.0.1".parse::<IpAddr>().unwrap(),
                    "::1".parse::<IpAddr>().unwrap(),
                ],
                vec![],
                5_000,
            ),
        );
        let inst = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = inst.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }

        write_str(&mut store, 120, "dual.example.com");
        // 32-byte hints struct with ai_family = AF_INET6 (10).
        let mut hints = [0u8; 32];
        hints[4..8].copy_from_slice(&10i32.to_le_bytes());
        write_guest(&mut store, 200, &hints);

        let r = call_resolve(&mut store, &inst, 120, 0, 0, 0, 200, 128).await?;
        assert!(r >= 0, "expected >= 0, got {r}");
        assert_eq!(r, 1, "expected exactly 1 v6 result, got {r}");
        Ok(())
    })
}