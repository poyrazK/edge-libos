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

mod common;

use anyhow::Result;
use edge_libos::snapshot::{
    apply_snapshot_kernel_state, apply_snapshot_to_memory, decode_snapshot, encode_snapshot,
    try_to_snapshot,
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
/// Plain `#[test]` (not `#[tokio::test]`) because `run_main_from`
/// builds its own current-thread tokio runtime internally; nesting
/// `block_on` inside a `#[tokio::test]` runtime triggers "Cannot
/// start a runtime from within a runtime".
#[test]
fn migration_in_process_via_edge_cli_migrate() -> Result<()> {
    // Write the marker wasm to a temp file (migrate reads the
    // wasm from disk, per its argv contract).
    let tmp = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("tempfile");
    let bytes = wat::parse_str(MARKER_WAT)?;
    std::fs::write(tmp.path(), &bytes)?;

    // Invoke run_main_from(["migrate", "<tmp>"]) and assert 0.
    let args = vec!["migrate".to_string(), tmp.path().to_string_lossy().to_string()];
    let code = edge_libos::cli::run_main_from(args);
    assert_eq!(code, 0, "edge-cli migrate must return 0 on success");
    Ok(())
}

/// Spawn the actual `edge-cli` binary against a tiny wasm and
/// assert exit 0. Skipped by default — pays for subprocess spawn
/// + Wasmtime compile. Run with `--ignored`.
#[tokio::test]
#[ignore = "subprocess smoke; run with `cargo test --profile ci -- --ignored`"]
async fn migration_smoke_subprocess_roundtrip() -> Result<()> {
    use std::process::Command;

    let tmp = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("tempfile");
    let bytes = wat::parse_str(MARKER_WAT)?;
    std::fs::write(tmp.path(), &bytes)?;

    let exe = std::env::current_exe()?;
    // cargo test sets CARGO_BIN_EXE_<name> for the workspace's
    // binaries; we use that if present, else fall back to the
    // current_exe (which is the test binary, not edge-cli —
    // this fallback exists for documentation only).
    let bin = std::env::var("CARGO_BIN_EXE_edge-cli").unwrap_or_else(|_| {
        exe.parent()
            .unwrap()
            .join("edge-cli")
            .to_string_lossy()
            .to_string()
    });

    let status = Command::new(&bin)
        .arg("migrate")
        .arg(tmp.path())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn {bin}: {e}"))?;
    assert!(status.success(), "edge-cli migrate exited non-zero: {status}");
    Ok(())
}
