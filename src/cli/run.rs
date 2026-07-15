//! `edge-cli run <wasm> [--] [args...] [--cpu-budget-ms <ms>]`.
//!
//! P2-D3.3: the body of this module was migrated verbatim from
//! `src/bin/edge_python.rs` (the D0-era one-shot driver). The argv
//! parser is unchanged: positional `wasm` path, optional `--`
//! separator, the rest go to the guest as argv.
//!
//! Behaviors preserved verbatim (these are tested in
//! `tests/edge_python_smoke.rs` and `tests/edge_python_import_smoke.rs`):
//!
//! - Magic-prefix detection: `\0asm` → `Module::new`, anything else
//!   → `unsafe { Module::deserialize }`. CPython ships `.wasm`; we
//!   also tolerate precompiled artifacts.
//! - Attach linear memory AFTER instantiation (per EFAULT design).
//! - Dual-signature `_start`: try `() -> ()` then `() -> i32`. The
//!   musl zig cc path uses the void signature; emscripten uses i32.
//! - Drain stdio AFTER `_start` returns/traps.
//! - Propagate the guest exit code (`Kernel::exit_code` set by
//!   NR_EXIT / NR_EXIT_GROUP).
//!
//! P2 metering (ADR 0003): `--cpu-budget-ms <ms>` is optional on
//! `run` (default: unbounded). When supplied, the Store is
//! configured with `set_fuel(ms_to_fuel(budget))` BEFORE
//! `instantiate_async`; a fuel trap during `_start` is classified
//! by `meter::is_out_of_fuel` and surfaces as `CliError::Metered`
//! (exit code 1). The consumed fuel is reported in the error
//! message so callers can tune the budget.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::cli::error::{CliError, CliResult};
use crate::kernel::Kernel;
use crate::meter::{is_out_of_fuel, ms_to_fuel};
use crate::{add_to_linker, build_engine, build_store};
// `crate::{add_to_linker, build_engine, build_store}` are re-exports
// from `src/lib.rs`. The deleted `edge-python` binary imported them
// from the `edge_libos` crate (because it was a separate binary); now
// that we live inside the crate, `crate::` is the right path.

/// Sentinel budget for `run` when `--cpu-budget-ms` is omitted. ADR
/// 0003 §2: defaults are unbounded on `run` and `bench`; `u64::MAX`
/// is the "no ceiling" sentinel because `ms_to_fuel` saturates at it.
const RUN_DEFAULT_BUDGET_MS: u64 = u64::MAX;

/// Entry point for `edge-cli run`. Args layout:
/// - `args[0]` = wasm path (required).
/// - Optional `--cpu-budget-ms <ms>` (anywhere before `--`).
/// - Optional `--` separator; everything after goes to guest argv.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let (wasm_path, budget_ms, script_args) = parse_run_argv(args)?;
    let exit_code = drive_guest(&wasm_path, budget_ms, &script_args).await?;
    Ok(exit_code)
}

/// Parse `run`'s argv into `(wasm_path, budget_ms, guest_args)`.
/// Split out from `run_main` so the parsing rules are unit-testable
/// without spinning up a wasmtime runtime.
fn parse_run_argv(args: &[String]) -> CliResult<(String, u64, Vec<String>)> {
    if args.is_empty() {
        return Err(CliError::Args(
            "usage: edge-cli run <wasm> [--] [args...] [--cpu-budget-ms <ms>]".to_string(),
        ));
    }
    let mut budget_ms = RUN_DEFAULT_BUDGET_MS;
    let mut rest: Vec<&str> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--cpu-budget-ms" {
            let raw = args.get(i + 1).ok_or_else(|| {
                CliError::Args("run: --cpu-budget-ms requires a number".to_string())
            })?;
            budget_ms = raw.parse::<u64>().map_err(|e: std::num::ParseIntError| {
                CliError::Args(format!("run: --cpu-budget-ms: {e}"))
            })?;
            if budget_ms == 0 {
                return Err(CliError::Args(
                    "run: --cpu-budget-ms 0 is reserved (would trap on first instruction)"
                        .to_string(),
                ));
            }
            i += 2;
            continue;
        }
        rest.push(a);
        i += 1;
    }
    if rest.is_empty() {
        return Err(CliError::Args(
            "usage: edge-cli run <wasm> [--] [args...] [--cpu-budget-ms <ms>]".to_string(),
        ));
    }
    let wasm_path = rest[0].to_string();
    let script_args: Vec<String> = if rest.get(1).copied() == Some("--") {
        rest[2..].iter().map(|s| s.to_string()).collect()
    } else {
        rest[1..].iter().map(|s| s.to_string()).collect()
    };
    Ok((wasm_path, budget_ms, script_args))
}

/// Instantiate + run + drain. Split out from `run_main` so tests
/// can call it without going through `CliError` plumbing.
async fn drive_guest(wasm_path: &str, budget_ms: u64, _script_args: &[String]) -> CliResult<i32> {
    let engine = build_engine().map_err(CliError::from)?;
    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker).map_err(CliError::from)?;

    // Set up stdio. Default to buffered pipes so we can drain them at the
    // end. Real Wasi-style stdio (TtyFile) is out of scope for P0; tests
    // inspect the buffers directly.
    let kernel = Kernel::new(vec![], vec![]);

    let mut store = build_store(&engine, kernel);
    // ADR 0003 §2: per-instance fuel budget. The default (u64::MAX)
    // is the unbounded sentinel; a real value is converted via
    // `ms_to_fuel`. Setting fuel on a Store whose engine has
    // `consume_fuel(true)` is the only way to bound guest CPU.
    let budget_fuel = ms_to_fuel(budget_ms);
    store.set_fuel(budget_fuel).map_err(|e| {
        CliError::Args(format!("run: set_fuel failed (engine fuel disabled?): {e}"))
    })?;
    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("reading {wasm_path}: {e}")))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes).map_err(CliError::from)?
    } else {
        // SAFETY: callers accept `Module::deserialize` for precompiled
        // artifacts. Same precondition as the deleted edge-python driver.
        unsafe { wasmtime::Module::deserialize(&engine, &bytes).map_err(CliError::from)? }
    };
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(CliError::from)?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    // Snapshot the stdout/stderr buffer Arcs BEFORE the guest runs so we
    // can drain them after.
    let stdout_buf = store.data().stdout_buf();
    let stderr_buf = store.data().stderr_buf();

    // Call `_start`. Multiple signatures: () -> void (zig cc / CPython),
    // () -> i32 (emscripten).
    let call_result = if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        start.call_async(&mut store, ()).await
    } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut store, "_start") {
        start.call_async(&mut store, ()).await.map(|_| ())
    } else {
        return Err(CliError::Args(format!(
            "edge-cli run: no _start export in {wasm_path}"
        )));
    };

    // ADR 0003 §6: distinguish a fuel-exhaustion trap from any other
    // trap. `is_out_of_fuel` matches on the wasmtime Display string,
    // which is stable across 45.0.x.
    if let Err(ref e) = call_result {
        if is_out_of_fuel(e) {
            let consumed_remaining = store.get_fuel().ok();
            let used_ms = consumed_remaining
                .map(|remaining| crate::meter::fuel_to_ms(budget_fuel.saturating_sub(remaining)))
                .unwrap_or(0);
            // Drain stdio before bailing so partial output isn't lost.
            if let Some(b) = stdout_buf {
                drain_to_stdout(&b);
            }
            if let Some(b) = stderr_buf {
                drain_to_stderr(&b);
            }
            return Err(CliError::Metered(format!(
                "used {used_ms} ms / budget {budget_ms} ms (trap: out of fuel)"
            )));
        }
    }

    // exit() records the code in Kernel::exit_code; we surface that to
    // the host process. Trap from exit (if any) is fine — we just want
    // the recorded code.
    let _ = call_result; // ignore non-fuel traps

    // Drain stdio.
    if let Some(b) = stdout_buf {
        drain_to_stdout(&b);
    }
    if let Some(b) = stderr_buf {
        drain_to_stderr(&b);
    }

    Ok(store.data().exit_code.unwrap_or(0))
}

fn drain_to_stdout(buf: &Arc<Mutex<VecDeque<u8>>>) {
    let bytes: Vec<u8> = {
        let mut q = buf.lock();
        q.drain(..).collect()
    };
    let _ = std::io::stdout().write_all(&bytes);
}

fn drain_to_stderr(buf: &Arc<Mutex<VecDeque<u8>>>) {
    let bytes: Vec<u8> = {
        let mut q = buf.lock();
        q.drain(..).collect()
    };
    let _ = std::io::stderr().write_all(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wasm_path_only() {
        let (w, b, s) = parse_run_argv(&["foo.wasm".into()]).unwrap();
        assert_eq!(w, "foo.wasm");
        assert_eq!(b, RUN_DEFAULT_BUDGET_MS);
        assert!(s.is_empty());
    }

    #[test]
    fn parses_double_dash_separator() {
        let (w, b, s) =
            parse_run_argv(&["foo.wasm".into(), "--".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(w, "foo.wasm");
        assert_eq!(b, RUN_DEFAULT_BUDGET_MS);
        assert_eq!(s, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parses_cpu_budget_ms() {
        let (w, b, s) = parse_run_argv(&[
            "foo.wasm".into(),
            "--cpu-budget-ms".into(),
            "500".into(),
        ])
        .unwrap();
        assert_eq!(w, "foo.wasm");
        assert_eq!(b, 500);
        assert!(s.is_empty());
    }

    #[test]
    fn rejects_zero_budget() {
        let err =
            parse_run_argv(&["foo.wasm".into(), "--cpu-budget-ms".into(), "0".into()]).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_numeric_budget() {
        let err =
            parse_run_argv(&["foo.wasm".into(), "--cpu-budget-ms".into(), "abc".into()])
                .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_budget_without_value() {
        let err = parse_run_argv(&["foo.wasm".into(), "--cpu-budget-ms".into()]).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn empty_args_is_usage_error() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(run_main(&[])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)));
    }
}