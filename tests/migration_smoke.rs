//! P3 final-bundle sub-deliverable 6 — migration smoke tests.
//!
//! Tests the freeze → encode → decode → apply roundtrip:
//!   1. `migration_roundtrip_preserves_kernel_state` — load a
//!      tiny `.wasm` that writes a marker into linear memory,
//!      freeze, encode to bytes, decode, apply to a fresh store,
//!      then read the marker back from the restored kernel.
//!   2. `migration_in_process_via_edge_cli_migrate` — invoke
//!      `run_main_from(["migrate", "<tmp_wasm>"])` (the in-process
//!      path the migrate subcommand uses) and assert it returns
//!      0 + the roundtrip succeeds end-to-end.
//!   3. `migration_smoke_subprocess_roundtrip` — `#[ignore]`'d:
//!      spawns the actual `edge-cli` binary against a tiny wasm
//!      and asserts exit 0. Skipped by default because it pays
//!      for a subprocess spawn + Wasmtime compile; run with
//!      `cargo test --profile ci -- --ignored`.
//!   4. `migration_roundtrip_preserves_shared_memory_state` — same
//!      freeze/encode/decode/apply roundtrip but on a guest that
//!      declares `(memory … shared)`. Exercises the
//!      `apply_snapshot_to_shared_memory` driver that
//!      `dispatch_memory_apply` routes to for the `Shared` variant
//!      of `MemoryKind` (sub-deliverable 2 + sub-deliverable 6).

mod common;

use anyhow::Result;
use edge_libos::snapshot::{
    apply_snapshot_kernel_state, apply_snapshot_to_memory, apply_snapshot_to_shared_memory,
    decode_snapshot, encode_snapshot, try_to_snapshot,
};
use edge_libos::{build_store, Kernel};

const MARKER_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; Writes the 8-byte ASCII marker "MIGRATED" at offset 0x1000,
      ;; then exits cleanly via syscall(NR_EXIT, 0).
      (func (export "_start") (result i64)
        (i64.store (i32.const 0x1000) (i64.const 0x4445_5441_5247_494d))
        (drop (call $syscall
          (i64.const 60) (i64.const 0)
          (i64.const 0) (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0)))
        (i64.const 0)))
"#;

const MARKER_BYTES: i64 = 0x4445_5441_5247_494d;

/// WAT that writes a marker, snapshots it, encodes, decodes,
/// applies, then re-reads the marker. The whole roundtrip happens
/// inside one test process (no subprocess spawn).
#[test]
fn migration_roundtrip_preserves_kernel_state() -> Result<()> {
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio current_thread runtime");
        rt.block_on(f)
    }

    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, MARKER_WAT)?;

        // Phase 1: run the guest on host-A.
        let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let instance = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = instance.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let start = instance.get_typed_func::<(), i64>(&mut store, "_start")?;
        let _ = start.call_async(&mut store, ()).await;

        // Phase 2: snapshot the live kernel.
        let snap = try_to_snapshot(store.data(), &store)?;

        // Phase 3+4: encode + decode (simulates cross-host wire transfer).
        let bytes = encode_snapshot(&snap)?;
        let snap_restored = decode_snapshot(&bytes)?;
        assert_eq!(snap_restored.format_version.0, snap.format_version.0);

        // Phase 5: apply to a fresh kernel + store on host-B.
        let mut fresh_store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let fresh_instance = linker.instantiate_async(&mut fresh_store, &module).await?;
        if let Some(mem) = fresh_instance.get_memory(&mut fresh_store, "memory") {
            fresh_store.data_mut().attach_memory(mem);
        }
        apply_snapshot_kernel_state(&snap_restored, fresh_store.data_mut())?;
        let mem_clone = *fresh_store
            .data()
            .memory()
            .map_err(|e| anyhow::anyhow!("memory not attached: {e}"))?;
        apply_snapshot_to_memory(&snap_restored, mem_clone, &mut fresh_store)?;

        // Read the marker back from the restored kernel's linear
        // memory at offset 0x1000.
        let mem = *fresh_store
            .data()
            .memory()
            .map_err(|e| anyhow::anyhow!("memory not attached: {e}"))?;
        let mut buf = [0u8; 8];
        mem.read(&fresh_store, 0x1000, &mut buf)
            .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
        let restored_marker = i64::from_le_bytes(buf);
        assert_eq!(
            restored_marker, MARKER_BYTES,
            "marker must roundtrip through the encode/decode/apply path"
        );
        Ok::<(), anyhow::Error>(())
    })
}

/// Drive the `migrate` subcommand end-to-end via run_main_from
/// (the test-friendly dispatcher entry point). The wasm is
/// compiled in-process; the migrate subcommand runs the
/// in-process roundtrip directly.
///
/// P2-D3.5: migrate now defaults to the subprocess path; tests
/// that want to exercise the in-process roundtrip opt in via
/// the `MIGRATE_IN_PROCESS=1` env var (the migrate subcommand
/// checks for it at startup). We RAII-guard the env var so the
/// test doesn't leak state to other tests in the same process.
///
/// Plain `#[test]` (not `#[tokio::test]`) because `run_main_from`
/// builds its own current-thread tokio runtime internally; nesting
/// `block_on` inside a `#[tokio::test]` runtime triggers "Cannot
/// start a runtime from within a runtime".
#[test]
fn migration_in_process_via_edge_cli_migrate() -> Result<()> {
    // RAII guard for MIGRATE_IN_PROCESS=1 — set on entry,
    // restored to its prior value on exit. This keeps the test
    // hermetic and prevents cross-test env pollution.
    struct Guard(Option<String>);
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: set_var/remove_var are unsafe in recent
            // Rust but the test process is single-threaded for
            // env mutations (we serialize via the Drop at end
            // of test).
            unsafe {
                match self.0.as_ref() {
                    Some(v) => std::env::set_var("MIGRATE_IN_PROCESS", v),
                    None => std::env::remove_var("MIGRATE_IN_PROCESS"),
                }
            }
        }
    }
    let prev = std::env::var("MIGRATE_IN_PROCESS").ok();
    // SAFETY: see Guard::drop.
    unsafe { std::env::set_var("MIGRATE_IN_PROCESS", "1") };
    let _guard = Guard(prev);

    // Write the marker wasm to a temp file (migrate reads the
    // wasm from disk, per its argv contract).
    let tmp = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("tempfile");
    let bytes = wat::parse_str(MARKER_WAT)?;
    std::fs::write(tmp.path(), &bytes)?;

    // Invoke run_main_from(["migrate", "<tmp>"]) and assert 0.
    let args = vec![
        "migrate".to_string(),
        tmp.path().to_string_lossy().to_string(),
    ];
    let code = edge_libos::cli::run_main_from(args);
    assert_eq!(code, 0, "edge-cli migrate must return 0 on success");
    Ok(())
}

/// Spawn the actual `edge-cli` binary against a tiny wasm and
/// assert exit 0. P2-D3.5: this is the production-shape subprocess
/// test (the migrate subcommand now spawns `edge-cli freeze` and
/// `edge-cli serve` as children). Cheap enough to run in CI on
/// every push.
#[tokio::test]
async fn migration_smoke_subprocess_roundtrip() -> Result<()> {
    use std::process::Command;

    let tmp = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("tempfile");
    let bytes = wat::parse_str(MARKER_WAT)?;
    std::fs::write(tmp.path(), &bytes)?;

    // Resolve the edge-cli binary. cargo test usually sets
    // CARGO_BIN_EXE_edge-cli; if not (e.g. running the test
    // binary directly), fall back to `<test_exe_dir>/../edge-cli`
    // — cargo places workspace bins at `target/<profile>/` while
    // integration-test artifacts live under `target/<profile>/deps/`,
    // so we walk up one level.
    let bin = std::env::var("CARGO_BIN_EXE_edge-cli")
        .ok()
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let dir = exe.parent()?;
            let candidate = dir.join("..").join("edge-cli");
            if candidate.is_file() {
                Some(candidate.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            panic!(
                "cannot locate edge-cli binary: set CARGO_BIN_EXE_edge-cli or \
                 run via `cargo test --profile ci -p edge-libos \
                 --test migration_smoke`"
            )
        });

    let status = Command::new(&bin)
        .arg("migrate")
        .arg(tmp.path())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn {bin}: {e}"))?;
    assert!(
        status.success(),
        "edge-cli migrate exited non-zero: {status}"
    );
    Ok(())
}

/// Shared-memory variant of the migration roundtrip. The fixture
/// declares `(memory 1 1 shared)` so wasmtime exposes the memory
/// as a `SharedMemory`. We write a marker into the shared memory
/// bytes (via `SharedMemory::data` + raw projection), then run the
/// full freeze → encode → decode → apply cycle. The destination
/// kernel must have its memory restored through
/// `apply_snapshot_to_shared_memory` (the Shared arm of
/// `dispatch_memory_apply`), and the marker must be observable in
/// the post-apply shared backing store.
#[test]
fn migration_roundtrip_preserves_shared_memory_state() -> Result<()> {
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio current_thread runtime");
        rt.block_on(f)
    }

    const SHARED_MARKER_WAT: &str = r#"
        (module
          (memory (export "memory") 1 1 shared))
    "#;

    const SHARED_MARKER_BYTES: i64 = 0x4445_5441_5247_494d; // "MIGRATED"

    block_on(async {
        let (engine, linker) = common::engine_and_linker()?;
        let module = common::compile_wat(&engine, SHARED_MARKER_WAT)?;

        // Phase 1: host-A. Instantiate and attach the shared memory.
        let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let shared_mem = instance
            .get_shared_memory(&mut store, "memory")
            .expect("shared memory export must exist");
        store.data_mut().attach_shared_memory(shared_mem);

        // Verify the kernel routed to MemoryKind::Shared.
        {
            let kind = store.data().memory_kind().expect("memory must be attached");
            assert!(
                kind.as_shared_memory().is_some(),
                "kernel must store the SharedMemory, not a regular Memory"
            );
        }

        // Write the marker directly into shared memory bytes.
        // `SharedMemory::data` returns `&[UnsafeCell<u8>]`; project
        // to `&mut [u8]` for the write. Safe here because we are
        // single-threaded inside the freeze CLI's quiescent-point
        // window.
        let shared = store
            .data()
            .memory_kind()
            .unwrap()
            .as_shared_memory()
            .unwrap();
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(shared.data().as_ptr() as *mut u8, shared.data_size())
        };
        bytes[0x1000..0x1008].copy_from_slice(&SHARED_MARKER_BYTES.to_le_bytes());

        // Phase 2: snapshot.
        let snap = try_to_snapshot(store.data(), &store)?;

        // Phase 3+4: encode/decode (simulates cross-host transfer).
        let encoded = encode_snapshot(&snap)?;
        let snap_restored = decode_snapshot(&encoded)?;
        assert_eq!(snap_restored.format_version.0, snap.format_version.0);

        // Phase 5: host-B. Fresh kernel + fresh shared-memory
        // guest. Split-phase apply mirrors the Owned-path test
        // above: kernel-state apply first (no `Store` borrow),
        // then memory apply.
        let mut fresh_store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let fresh_instance = linker.instantiate_async(&mut fresh_store, &module).await?;
        let fresh_shared = fresh_instance
            .get_shared_memory(&mut fresh_store, "memory")
            .expect("shared memory export must exist");
        fresh_store.data_mut().attach_shared_memory(fresh_shared);

        apply_snapshot_kernel_state(&snap_restored, fresh_store.data_mut())?;
        let shared_clone = fresh_store
            .data()
            .memory_kind()
            .expect("memory attached")
            .as_shared_memory()
            .expect("memory is Shared variant")
            .clone();
        apply_snapshot_to_shared_memory(&snap_restored, &shared_clone)?;

        // Read the marker back from the post-apply shared memory.
        let fresh_shared_ref = fresh_store
            .data()
            .memory_kind()
            .unwrap()
            .as_shared_memory()
            .unwrap();
        let restored_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                fresh_shared_ref.data().as_ptr() as *const u8,
                fresh_shared_ref.data_size(),
            )
        };
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&restored_bytes[0x1000..0x1008]);
        let restored_marker = i64::from_le_bytes(buf);
        assert_eq!(
            restored_marker, SHARED_MARKER_BYTES,
            "marker must roundtrip through the Shared-memory encode/decode/apply path"
        );
        Ok::<(), anyhow::Error>(())
    })
}
