//! `edge-cli migrate <wasm> [--] [args...]` — P3 final-bundle.
//!
//! Demonstrates the v1 freeze → serve migration flow (ADR 0003):
//!   1. Run the guest with the supplied argv. At a quiescent point
//!      (here: just after `_start` returns), `try_to_snapshot` the
//!      live kernel into a `KernelSnapshot`.
//!   2. Encode the snapshot to postcard (`encode_snapshot`).
//!   3. Decode the snapshot (`decode_snapshot`) — simulates the
//!      cross-host byte transfer.
//!   4. Apply the snapshot to a fresh kernel + store
//!      (`apply_snapshot`).
//!
//! v1 runs the entire flow **in-process** rather than spawning a
//! child `edge-cli freeze` + `edge-cli serve` subprocess pair.
//! Two reasons:
//!   * `freeze` / `serve` are stubs that return
//!     `CliError::Args("not yet implemented")` — the real bodies
//!     land in D3.5, after which the subprocess variant follows.
//!   * The migration contract under test is the snapshot
//!     encode/decode/apply roundtrip. The wire-format is what
//!     travels between hosts; the subprocess shell is incidental.
//!
//! When the D3.5 freeze/serve bodies land, this subcommand can
//! gain an `--in-process` flag (default) and a `--subprocess`
//! flag that swaps to `Command::new(current_exe)().args(...)`.
//! The in-process path remains the testable contract; the
//! subprocess path is the production shape.

use crate::cli::error::{CliError, CliResult};
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::snapshot::{decode_snapshot, encode_snapshot, try_to_snapshot};

/// `edge-cli migrate <wasm> [--] [args...]` — v1 in-process migration.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    // Parse <wasm> + optional positional guest args after `--`.
    let (wasm_path, guest_args) = parse_migrate_args(args)?;

    let engine = build_engine()?;
    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker)?;

    // Phase 1: run the guest. We use a single kernel + store for
    // both freeze and serve in v1 (in-process). The byte-buffer
    // encode/decode step is what simulates the cross-host
    // transfer; the kernel+store doesn't actually move.
    let module = wasmtime::Module::from_file(&engine, &wasm_path).map_err(CliError::Wasmtime)?;
    let mut store = build_store(
        &engine,
        Kernel::new_without_stdio(guest_args.clone(), vec![]),
    );
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(CliError::Wasmtime)?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    // Drive `_start` so the guest runs. For wasm32-musl guests
    // (CPython, etc.) this is the entry point. For C conformance
    // fixtures the export may be missing — we silently skip the
    // call if so, then snapshot whatever state the kernel has at
    // instantiation time.
    if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
        match start.call_async(&mut store, ()).await {
            Ok(0) => {}
            Ok(rc) => eprintln!(
                "edge-cli migrate: guest _start returned non-zero exit code {rc}; \
                 snapshot will reflect post-exit kernel state"
            ),
            Err(e) => eprintln!(
                "edge-cli migrate: guest _start trapped ({e}); \
                 snapshot will reflect trap-time kernel state"
            ),
        }
    }

    // Phase 2: snapshot the live kernel.
    let snap = try_to_snapshot(store.data(), &store).map_err(CliError::Snapshot)?;

    // Phase 3: encode (simulates wire transfer to host-B).
    let bytes = encode_snapshot(&snap).map_err(CliError::Snapshot)?;

    // Phase 4: decode (host-B receiving).
    let snap_restored = decode_snapshot(&bytes).map_err(CliError::Snapshot)?;

    // Phase 5: apply to a fresh kernel + store (host-B serving).
    let mut fresh_store = build_store(
        &engine,
        Kernel::new_without_stdio(guest_args.clone(), vec![]),
    );
    let fresh_instance = linker
        .instantiate_async(&mut fresh_store, &module)
        .await
        .map_err(CliError::Wasmtime)?;
    if let Some(mem) = fresh_instance.get_memory(&mut fresh_store, "memory") {
        fresh_store.data_mut().attach_memory(mem);
    }
    // v1 migrate only supports Owned-memory guests. Use the public
    // Owned-path API: kernel-state apply first (no Store borrow),
    // then memory apply. The Shared-memory path lives in
    // `dispatch_memory_apply` (private) and is exercised
    // by `tests/futex_conformance.rs::memory_kind_shared_atomic_wait32_not_equal`.
    crate::snapshot::apply_snapshot_kernel_state(&snap_restored, fresh_store.data_mut())
        .map_err(CliError::Snapshot)?;
    let mem_clone = match fresh_store.data().memory() {
        Ok(m) => *m,
        Err(e) => return Err(CliError::Args(format!("no memory attached: {e}"))),
    };
    crate::snapshot::apply_snapshot_to_memory(&snap_restored, mem_clone, &mut fresh_store)
        .map_err(CliError::Snapshot)?;

    eprintln!(
        "edge-cli migrate: roundtripped {} bytes of snapshot state",
        bytes.len()
    );
    Ok(0)
}

fn parse_migrate_args(args: &[String]) -> CliResult<(std::path::PathBuf, Vec<String>)> {
    let mut it = args.iter();
    let wasm = it.next().ok_or_else(|| {
        CliError::Args("usage: edge-cli migrate <wasm> [--] [args...]".to_string())
    })?;
    let wasm_path = std::path::PathBuf::from(wasm);

    let mut guest_args: Vec<String> = Vec::new();
    let mut seen_dashdash = false;
    for a in it {
        if !seen_dashdash && a == "--" {
            seen_dashdash = true;
            continue;
        }
        guest_args.push(a.clone());
    }
    Ok((wasm_path, guest_args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_migrate_args_basic() {
        let args = vec!["foo.wasm".to_string()];
        let (p, ga) = parse_migrate_args(&args).unwrap();
        assert_eq!(p, std::path::PathBuf::from("foo.wasm"));
        assert!(ga.is_empty());
    }

    #[test]
    fn parse_migrate_args_with_dashdash() {
        let args = vec![
            "foo.wasm".to_string(),
            "--".to_string(),
            "arg1".to_string(),
            "arg2".to_string(),
        ];
        let (p, ga) = parse_migrate_args(&args).unwrap();
        assert_eq!(p, std::path::PathBuf::from("foo.wasm"));
        assert_eq!(ga, vec!["arg1".to_string(), "arg2".to_string()]);
    }

    #[test]
    fn parse_migrate_args_missing_wasm_returns_args_error() {
        let args: Vec<String> = vec![];
        assert!(matches!(parse_migrate_args(&args), Err(CliError::Args(_))));
    }
}
