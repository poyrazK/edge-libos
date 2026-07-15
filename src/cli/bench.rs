//! `edge-cli bench <snap> <wasm> --iters <n> [--cpu-budget-ms <ms>]`.
//!
//! P2-D3.7: hand-rolled p50/p95/p99/max over
//! `apply_snapshot_kernel_state + apply_snapshot_to_memory` cycles.
//! ZERO new deps — no criterion, no divan (both require `cargo bench`,
//! which `reproduce_dod.sh` does not invoke).
//!
//! Engine construct (~10ms) is hoisted OUT of the per-iter loop so
//! the iter cost is purely the apply path (which dominates
//! cold-start: ~1µs for a fresh wasm vs. ~hundreds-of-µs to seed
//! the kernel state + page-copy the linear memory). ADR 0002 §6
//! calls this the "restore cost" — the gate is `p50 < 5ms`.
//!
//! Two-arg form `<snap> <wasm>` — `KernelSnapshot` does not carry
//! module bytes (`src/snapshot.rs:158-187`), so serve and bench
//! both need the matching wasm path on disk.
//!
//! Snapshot portability caveat: serve/bench trust the wasm path
//! matches freeze's. If a different wasm is given, apply still
//! succeeds (the bytes load), but the guest will mis-execute.
//! Future: embed a module hash in `KernelSnapshot`,
//! `SNAPSHOT_FORMAT_VERSION` bump. Out of scope for D3.7.
//!
//! P2 metering (ADR 0003): `--cpu-budget-ms <ms>` is optional on
//! `bench` (default: unbounded, same as `run`). When supplied,
//! each iter calls `_start` after apply and reports `fuel_consumed`
//! in fuel units alongside the wall-clock restore cost. The
//! restore-cost gate is unchanged; fuel consumption is reported
//! as informational data — no fuel-based gate in this slice.

use std::path::PathBuf;

use wasmtime::{Linker, Store};

use crate::cli::error::{CliError, CliResult};
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::meter::ms_to_fuel;
use crate::snapshot::{apply_snapshot_kernel_state, apply_snapshot_to_memory, read_snapshot_file};

/// `edge-cli bench` exit-code semantics:
///
/// - `0` — all iterations ran, p50 < 5ms.
/// - `1` — p50 >= 5ms gate violated (via `CliError::Bench`).
/// - `2` — argv error (via `CliError::Args`).
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let mut iters: usize = 50;
    let mut budget_ms: u64 = u64::MAX; // ADR 0003 §2: bench default is unbounded.
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--iters" {
            let raw = it.next().ok_or_else(|| {
                CliError::Args("bench: --iters requires a number argument".to_string())
            })?;
            iters = raw.parse().map_err(|e: std::num::ParseIntError| {
                CliError::Args(format!("bench: --iters: {e}"))
            })?;
            if iters == 0 {
                return Err(CliError::Args("bench: --iters must be > 0".to_string()));
            }
        } else if a == "--cpu-budget-ms" {
            let raw = it.next().ok_or_else(|| {
                CliError::Args("bench: --cpu-budget-ms requires a number argument".to_string())
            })?;
            budget_ms = raw.parse().map_err(|e: std::num::ParseIntError| {
                CliError::Args(format!("bench: --cpu-budget-ms: {e}"))
            })?;
            if budget_ms == 0 {
                return Err(CliError::Args(
                    "bench: --cpu-budget-ms 0 is reserved (would trap on first instruction)"
                        .to_string(),
                ));
            }
        } else {
            positional.push(a.clone());
        }
    }
    if positional.len() < 2 {
        return Err(CliError::Args(
            "usage: edge-cli bench <snap> <wasm> [--iters <n>] [--cpu-budget-ms <ms>]".to_string(),
        ));
    }
    let snap_path = PathBuf::from(&positional[0]);
    let wasm_path = PathBuf::from(&positional[1]);

    let snap = read_snapshot_file(&snap_path)?;
    let wasm_bytes = std::fs::read(&wasm_path)
        .map_err(|e| CliError::Args(format!("bench: reading {}: {e}", wasm_path.display())))?;

    let engine = build_engine()?;
    let budget_fuel = ms_to_fuel(budget_ms);

    let mut samples_us: Vec<u64> = Vec::with_capacity(iters);
    let mut samples_fuel: Vec<u64> = Vec::with_capacity(iters);
    for i in 0..iters {
        // We rebuild linker+store+module+instance per iter — the
        // engine is hoisted (it's the expensive piece). `apply_*`
        // does the actual work being measured.
        let mut linker: Linker<Kernel> = Linker::new(&engine);
        add_to_linker(&mut linker)?;
        let kernel = Kernel::new(vec![], vec![]);
        let mut store: Store<Kernel> = build_store(&engine, kernel);
        // Per-iter fuel budget — reset every iter so a runaway
        // request can't poison the next iter's measurement. With
        // budget_ms == u64::MAX, set_fuel is a no-op semantics-wise.
        store
            .set_fuel(budget_fuel)
            .map_err(|e| CliError::Args(format!("bench: set_fuel failed: {e}")))?;
        let module = if wasm_bytes.len() >= 4 && &wasm_bytes[0..4] == b"\0asm" {
            wasmtime::Module::new(&engine, &wasm_bytes)?
        } else {
            // SAFETY: callers accept `Module::deserialize` for
            // precompiled artifacts. Same precondition as freeze/serve.
            unsafe { wasmtime::Module::deserialize(&engine, &wasm_bytes) }?
        };
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let mem = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| CliError::Args("bench: no `memory` export".to_string()))?;
        store.data_mut().attach_memory(mem);

        let t0 = std::time::Instant::now();
        apply_snapshot_kernel_state(&snap, store.data_mut())?;
        apply_snapshot_to_memory(&snap, mem, &mut store)?;
        let elapsed_us = t0.elapsed().as_micros() as u64;
        samples_us.push(elapsed_us);

        // ADR 0003: also call `_start` and record fuel consumed.
        // We do this AFTER the apply so the fuel reading reflects
        // what a real serve-loop request would see — apply is
        // restore, _start is request, and the budget applies to
        // the request. The restore time is unaffected.
        if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = start.call_async(&mut store, ()).await;
        } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
            let _ = start.call_async(&mut store, ()).await;
        } else if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            let _ = start.call_async(&mut store, ()).await;
        }
        let fuel_remaining = store.get_fuel().unwrap_or(budget_fuel);
        let fuel_consumed = budget_fuel.saturating_sub(fuel_remaining);
        samples_fuel.push(fuel_consumed);

        if i % 10 == 0 {
            eprintln!("  iter {i:>3}: {elapsed_us} µs, fuel_consumed={fuel_consumed}");
        }
    }

    samples_us.sort_unstable();
    let pct = |p: f64, samples: &[u64]| -> u64 {
        let idx = ((p / 100.0) * (samples.len() as f64 - 1.0)).round() as usize;
        samples[idx]
    };
    let p50 = pct(50.0, &samples_us);
    let p95 = pct(95.0, &samples_us);
    let p99 = pct(99.0, &samples_us);
    let max = *samples_us.last().expect("non-empty samples");

    samples_fuel.sort_unstable();
    let fp50 = pct(50.0, &samples_fuel);
    let fp95 = pct(95.0, &samples_fuel);
    let fp99 = pct(99.0, &samples_fuel);
    let fmax = *samples_fuel.last().expect("non-empty samples");

    println!(
        "edge-cli bench: {iters} iters over {} ({} pages, {} fds)",
        snap_path.display(),
        snap.pages.len(),
        snap.fds.entries.len()
    );
    println!("  budget:       {budget_ms} ms ({budget_fuel} fuel)");
    println!("  restore cost:");
    println!("    p50:  {p50:>6} µs");
    println!("    p95:  {p95:>6} µs");
    println!("    p99:  {p99:>6} µs");
    println!("    max:  {max:>6} µs");
    println!("  fuel consumed (per-iter, post-apply _start):");
    println!("    p50:  {fp50:>6}");
    println!("    p95:  {fp95:>6}");
    println!("    p99:  {fp99:>6}");
    println!("    max:  {fmax:>6}");

    if p50 >= 5_000 {
        return Err(CliError::Bench(format!(
            "p50 cold-start {p50}µs exceeds 5ms gate"
        )));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn argv_requires_snap_and_wasm() {
        let r = rt();
        let err = r.block_on(run_main(&[])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
        let err = r.block_on(run_main(&["only.snap".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_iters_zero() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "snap.bin".into(),
                "f.wasm".into(),
                "--iters".into(),
                "0".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_numeric_iters() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "snap.bin".into(),
                "f.wasm".into(),
                "--iters".into(),
                "abc".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_zero_cpu_budget() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "snap.bin".into(),
                "f.wasm".into(),
                "--cpu-budget-ms".into(),
                "0".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_numeric_cpu_budget() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "snap.bin".into(),
                "f.wasm".into(),
                "--cpu-budget-ms".into(),
                "abc".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    /// Sort + index sanity check on the percentile helper. We
    /// can't easily exercise the inline closure, but the math is
    /// small enough to verify directly by re-implementing it.
    #[test]
    fn percentile_index_for_small_sample_is_well_defined() {
        let mut samples = [300u64, 100, 200, 400, 500];
        samples.sort_unstable();
        for p in [25.0_f64, 50.0, 75.0, 99.0, 100.0] {
            let idx = ((p / 100.0) * (samples.len() as f64 - 1.0)).round() as usize;
            assert!(idx < samples.len(), "p={p} idx={idx} len={}", samples.len());
        }
    }
}
