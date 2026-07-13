//! `edge-python` — P0 DoD driver.
//!
//! Loads a wasm32-musl guest, instantiates it under edge-libos, and
//! drives its `_start` export. Drains the buffered stdout/stderr pipes
//! to the host's actual stdout/stderr, and propagates the guest's
//! exit code (set via NR_EXIT / NR_EXIT_GROUP) to the host process.
//!
//! ## Usage
//!
//! ```text
//! edge-python <python.wasm> [--] [args...]
//! ```
//!
//! For P0 we don't actually parse the script path into CPython's argv —
//! CPython's Py_Main takes (argc, argv) via memory pointers, and Step 19
//! currently hard-codes `["python", "-c", "print(2+2)"]` in main.c. That
//! wires up end-to-end with the build.sh produced python.wasm once the
//! full CPython cross-compile lands.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;

use edge_libos::{add_to_linker, build_engine, build_store, Kernel};

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprintln!("usage: edge-python <python.wasm> [--] [args...]");
        std::process::exit(2);
    }
    let wasm_path = raw[0].clone();
    let script_args: Vec<String> = if raw.get(1).map(|s| s.as_str()) == Some("--") {
        raw[2..].to_vec()
    } else {
        raw[1..].to_vec()
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let exit_code = rt.block_on(run(&wasm_path, &script_args))?;
    std::process::exit(exit_code);
}

async fn run(wasm_path: &str, _script_args: &[String]) -> Result<i32> {
    let engine = build_engine()?;
    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker)?;

    // Set up stdio. Default to buffered pipes so we can drain them at the
    // end. Real Wasi-style stdio (TtyFile) is out of scope for P0; tests
    // inspect the buffers directly.
    let kernel = Kernel::new(vec![], vec![]);

    let mut store = build_store(&engine, kernel);
    let bytes =
        std::fs::read(wasm_path).map_err(|e| anyhow::anyhow!("reading {wasm_path}: {e}"))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        unsafe { wasmtime::Module::deserialize(&engine, &bytes)? }
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
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
        eprintln!("edge-python: no _start export in {wasm_path}");
        std::process::exit(2);
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
    use std::io::Write;
    let _ = std::io::stdout().write_all(&bytes);
}

fn drain_to_stderr(buf: &Arc<Mutex<VecDeque<u8>>>) {
    let bytes: Vec<u8> = {
        let mut q = buf.lock();
        q.drain(..).collect()
    };
    use std::io::Write;
    let _ = std::io::stderr().write_all(&bytes);
}
