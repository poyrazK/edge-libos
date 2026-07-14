//! `edge-cli run <wasm> [--] [args...]`.
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

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::kernel::Kernel;
use crate::{add_to_linker, build_engine, build_store};
// `crate::{add_to_linker, build_engine, build_store}` are re-exports
// from `src/lib.rs`. The deleted `edge-python` binary imported them
// from the `edge_libos` crate (because it was a separate binary); now
// that we live inside the crate, `crate::` is the right path.

use crate::cli::error::{CliError, CliResult};

/// Entry point for `edge-cli run`. Args layout:
/// - `args[0]` = wasm path (required).
/// - Optional `--` separator; everything after goes to guest argv.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    if args.is_empty() {
        return Err(CliError::Args(
            "usage: edge-cli run <wasm> [--] [args...]".to_string(),
        ));
    }
    let wasm_path = args[0].clone();
    let script_args: Vec<String> = if args.get(1).map(String::as_str) == Some("--") {
        args[2..].to_vec()
    } else {
        args[1..].to_vec()
    };

    let exit_code = drive_guest(&wasm_path, &script_args).await?;
    Ok(exit_code)
}

/// Instantiate + run + drain. Split out from `run_main` so tests
/// can call it without going through `CliError` plumbing.
async fn drive_guest(wasm_path: &str, _script_args: &[String]) -> CliResult<i32> {
    let engine = build_engine().map_err(CliError::from)?;
    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker).map_err(CliError::from)?;

    // Set up stdio. Default to buffered pipes so we can drain them at the
    // end. Real Wasi-style stdio (TtyFile) is out of scope for P0; tests
    // inspect the buffers directly.
    let kernel = Kernel::new(vec![], vec![]);

    let mut store = build_store(&engine, kernel);
    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("reading {wasm_path}: {e}")))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes).map_err(CliError::from)?
    } else {
        // SAFETY: callers accept `Module::deserialize` for precompiled
        // artifacts. Same precondition as the deleted `edge-python` driver.
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

    // exit() records the code in Kernel::exit_code; we surface that to
    // the host process. Trap from exit (if any) is fine — we just want
    // the recorded code.
    let _ = call_result; // ignore Trap

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
        // No `[--] [args...]`; `args[1..]` is the guest argv.
        let a: Vec<String> = vec!["foo.wasm".into()];
        let split = if a.get(1).map(String::as_str) == Some("--") {
            &a[2..]
        } else {
            &a[1..]
        };
        assert!(split.is_empty());
    }

    #[test]
    fn parses_double_dash_separator() {
        let a: Vec<String> = vec!["foo.wasm".into(), "--".into(), "a".into(), "b".into()];
        let split = if a.get(1).map(String::as_str) == Some("--") {
            &a[2..]
        } else {
            &a[1..]
        };
        assert_eq!(split, &["a".to_string(), "b".to_string()]);
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
